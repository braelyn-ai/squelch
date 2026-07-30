//! `squelchd` — the squelch daemon / CLI.
//!
//! - `auth`: run the OAuth consent flow and store tokens (keyring or file).
//! - `run`: sync-only loop, no HTTP.
//! - `serve`: sync loop plus one axum server hosting the agent door (`/mcp`) and
//!   the human door (`/client/*`).

use clap::{Args, Parser, Subcommand};
use squelch_core::auth::{AuthFlowOptions, AuthScopes, DEFAULT_HEADLESS_PORT, run_auth_flow};
use squelch_core::config::{Config, CredentialBackend, OAuthClientConfig, Stage2CapSources};
use squelch_core::credentials::{
    CredentialStore, FileCredentialStore, KeyringCredentialStore, load_token_backend,
    store_token_backend,
};
use squelch_core::embed::{Embedder, FastEmbedder};
use squelch_core::store::SqliteStore;
use squelch_core::sync::SyncEngine;
use squelch_core::types::AccountId;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Loopback only, by design: a reverse proxy (`tailscale serve`) fronts this.
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8848";

#[derive(Parser)]
#[command(name = "squelchd", about = "squelch local-first email intelligence daemon")]
struct Cli {
    /// Path to config.toml (defaults to ~/.config/squelch/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Authorize a Gmail account and store tokens in the configured backend
    /// (OS keyring, or a mode-0600 JSON file on headless hosts).
    ///
    /// Plain `auth` mints only the READ credential (gmail.readonly) used by
    /// sync. `--write` mints WRITE (gmail.modify + gmail.send) and re-mints
    /// READ, as two consent flows into two distinct slots so the pair cannot
    /// drift; sync/triage never touch the write one.
    ///
    /// HEADLESS: `--headless [--port N]` prints the consent URL and binds a
    /// FIXED loopback port (default 8847) to forward with
    /// `ssh -L 8847:127.0.0.1:8847 <host>`; both flows reuse that one port.
    Auth(AuthArgs),
    /// Run the sync loop only. No HTTP doors are served.
    Run,
    /// Run the sync loop plus one HTTP server hosting both the agent door
    /// (`/mcp`) and the human door (`/client/*`).
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Address to bind both doors to; defaults to `127.0.0.1:8848`, overridable
    /// via `SQUELCH_BIND`. Keep it on loopback behind a reverse proxy.
    #[arg(long)]
    bind: Option<String>,
}

#[derive(Args)]
struct AuthArgs {
    /// Mint the WRITE credential (gmail.modify + gmail.send) AND re-mint the
    /// read-only one, as two consent flows into two separate slots.
    #[arg(long)]
    write: bool,

    /// Headless mode: do NOT auto-open a browser, and bind the loopback
    /// listener to a FIXED port so it can be SSH-forwarded.
    #[arg(long)]
    headless: bool,

    /// Fixed loopback port for --headless (default 8847). Ignored otherwise.
    #[arg(long, default_value_t = DEFAULT_HEADLESS_PORT)]
    port: u16,
}

fn other_err(msg: String) -> squelch_core::CoreError {
    squelch_core::CoreError::Other(anyhow::anyhow!(msg))
}

fn build_runtime() -> Result<tokio::runtime::Runtime, squelch_core::CoreError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| other_err(format!("tokio runtime: {e}")))
}

/// Load config plus the Stage-2 cap sources, which `serve` reports as "default"
/// vs "config" on `/client/triage-config`.
fn load_config(cli: &Cli) -> (Config, Stage2CapSources) {
    match &cli.config {
        Some(path) => Config::load_from_with_cap_sources(path),
        None => Config::load_with_cap_sources(),
    }
}

/// The READ-bound credential store for the sync engine, per configured backend.
fn make_credential_store(
    backend: CredentialBackend,
    account_id: AccountId,
    email: String,
    creds_path: PathBuf,
    client: OAuthClientConfig,
) -> Arc<dyn CredentialStore> {
    match backend {
        CredentialBackend::Keyring => {
            Arc::new(KeyringCredentialStore::new(account_id, email, client))
        }
        CredentialBackend::File => {
            Arc::new(FileCredentialStore::new(account_id, email, creds_path, client))
        }
    }
}

/// Build the semantic-recall embedder. `None` on failure — search then degrades
/// to keyword-only.
fn build_embedder(config: &Config) -> Option<Arc<dyn Embedder>> {
    match FastEmbedder::new(&config.embed.settings()) {
        Ok(e) => Some(Arc::new(e) as Arc<dyn Embedder>),
        Err(e) => {
            eprintln!(
                "squelch: embedder unavailable ({e}); semantic recall disabled \
                 (keyword search + triage unaffected)"
            );
            None
        }
    }
}

/// Mirror the loaded `.env` into config.toml so other binaries and non-repo CWDs
/// resolve the same account/paths. Env-only secrets (`SQUELCH_API_TOKEN`) are
/// never written. Best-effort: failure warns, never fatal.
fn mirror_env_to_config(env_path: &std::path::Path) {
    let Some(config_path) = Config::default_path() else {
        return;
    };
    let pairs: Vec<(String, String)> = match dotenvy::from_path_iter(env_path) {
        Ok(iter) => iter.flatten().collect(),
        Err(e) => {
            eprintln!("squelchd: could not re-read {}: {e}", env_path.display());
            return;
        }
    };
    match squelch_core::config::mirror_env_pairs_to_config(&pairs, &config_path) {
        Ok(true) => eprintln!("squelchd: mirrored .env settings into {}", config_path.display()),
        Ok(false) => {}
        Err(e) => eprintln!(
            "squelchd: could not mirror .env into {}: {e}",
            config_path.display()
        ),
    }
}

fn main() -> ExitCode {
    // Dev convenience: pick up a `.env` before config reads the environment.
    // Never overrides already-exported vars; prod boxes use systemd env instead.
    if let Ok(path) = dotenvy::dotenv() {
        eprintln!("squelchd: loaded env from {}", path.display());
        mirror_env_to_config(&path);
    }

    let cli = Cli::parse();
    let (config, cap_sources) = load_config(&cli);

    let result = match &cli.command {
        Command::Auth(args) => cmd_auth(&config, args),
        Command::Run => run_daemon(config),
        Command::Serve(args) => cmd_serve(config, cap_sources, args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Uppercase tag for the progress banners.
fn scope_word(scopes: AuthScopes) -> &'static str {
    match scopes {
        AuthScopes::Read => "READ",
        AuthScopes::Write => "WRITE",
    }
}

/// Which credentials one `auth` invocation mints, in order. `--write` re-mints
/// READ too so the pair cannot drift, but it stays two scope sets, two flows,
/// two slots — the two-door split is never collapsed into one merged token.
fn auth_plan(args: &AuthArgs) -> Vec<AuthScopes> {
    if args.write {
        vec![AuthScopes::Write, AuthScopes::Read]
    } else {
        vec![AuthScopes::Read]
    }
}

/// Run the OAuth consent flow(s) and persist tokens for the configured account.
fn cmd_auth(config: &Config, args: &AuthArgs) -> Result<(), squelch_core::CoreError> {
    let client = config.oauth_client()?;
    let email = config.require_account_email()?;
    let backend = config.credential_backend;
    let creds_path = config.resolve_credentials_path();

    let plan = auth_plan(args);
    let total = plan.len();
    if total > 1 {
        eprintln!(
            "squelchd: --write renews BOTH credentials so they cannot drift apart; \
             {total} separate Google consent screens are coming, one per credential."
        );
        if args.headless {
            eprintln!(
                "squelchd: both flows reuse loopback port {}, so a single SSH tunnel covers both.",
                args.port
            );
        }
    }

    for (i, scopes) in plan.iter().copied().enumerate() {
        let step = i + 1;
        eprintln!(
            "\nsquelchd: minting {} credential ({step}/{total})...",
            scope_word(scopes)
        );
        // Strictly sequential: each flow's loopback listener is dropped before
        // the next binds, so headless can reuse the one fixed port.
        if let Err(e) = mint_credential(&client, &email, backend, &creds_path, scopes, args) {
            if step > 1 {
                eprintln!(
                    "squelchd: the credential(s) minted earlier in this run ARE stored; \
                     re-run `squelchd auth` to finish the {} credential.",
                    scope_word(scopes)
                );
            }
            return Err(e);
        }
    }
    Ok(())
}

/// One consent flow, persisted into that kind's own slot; scopes from different
/// kinds never share a slot.
fn mint_credential(
    client: &OAuthClientConfig,
    email: &str,
    backend: CredentialBackend,
    creds_path: &std::path::Path,
    scopes: AuthScopes,
    args: &AuthArgs,
) -> Result<(), squelch_core::CoreError> {
    let kind = scopes.kind();
    println!(
        "Authorizing Gmail account: {email} [{}] via {:?} backend",
        scopes.label(),
        backend
    );

    let opts = AuthFlowOptions {
        scopes,
        headless: args.headless,
        port: args.port,
    };
    let token = run_auth_flow(client, &opts)?;
    store_token_backend(backend, creds_path, email, kind, &token)?;

    // Confirm persistence without ever printing the token material.
    let _ = load_token_backend(backend, creds_path, email, kind)?;
    match backend {
        CredentialBackend::Keyring => {
            println!("\nStored {kind:?} credentials for {email} in the OS keyring (service \"squelch\").");
        }
        CredentialBackend::File => {
            println!(
                "\nStored {kind:?} credentials for {email} in {} (mode 0600).",
                creds_path.display()
            );
        }
    }
    if token.refresh_token.is_some() {
        println!("A refresh token was captured; squelch can renew access automatically.");
    } else {
        println!(
            "WARNING: no refresh token was returned. You may need to revoke prior access at \
             https://myaccount.google.com/permissions and re-run `squelchd auth`."
        );
    }
    Ok(())
}

/// Sync-only loop with graceful Ctrl-C shutdown.
fn run_daemon(config: Config) -> Result<(), squelch_core::CoreError> {
    // Fail fast on config problems before spinning up the runtime.
    let email = config.require_account_email()?;
    let client = config.oauth_client()?;

    let mut store = SqliteStore::open(&config.db_path)?;
    let account_id = store.ensure_account(&email)?;

    // Attach the embedder to both the store (query-side) and the engine
    // (write-side); `None` keeps everything working without vector recall.
    let embedder = build_embedder(&config);
    if let Some(e) = &embedder {
        store = store.with_embedder(e.clone())?;
    }
    let store = Arc::new(store);

    let creds = make_credential_store(
        config.credential_backend,
        account_id,
        email.clone(),
        config.resolve_credentials_path(),
        client,
    );

    let runtime = build_runtime()?;
    runtime.block_on(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nsquelch: shutdown requested; finishing in-flight work...");
                let _ = shutdown_tx.send(true);
            }
        });

        let mut engine = SyncEngine::new(store, creds, account_id, email, config);
        if let Some(e) = embedder {
            engine = engine.with_embedder(e);
        }
        engine.run(shutdown_rx).await
    })?;

    eprintln!("squelch: sync stopped.");
    Ok(())
}

/// Resolve the `serve` bind address: `--bind` > `SQUELCH_BIND` > loopback
/// default. Parsed eagerly so a bad value fails before we open anything.
fn resolve_bind(args: &ServeArgs) -> Result<SocketAddr, squelch_core::CoreError> {
    let raw = args
        .bind
        .clone()
        .or_else(|| std::env::var("SQUELCH_BIND").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
    raw.parse()
        .map_err(|e| other_err(format!("invalid bind address `{raw}`: {e}")))
}

/// The router hosting both doors: `/mcp` (agent door, read-only, sealed-absent)
/// and `/client/*` (human door, bearer-authed, the only write capability). They
/// share the store; the agent door never sees the write credential.
fn build_serve_router(
    store: Arc<SqliteStore>,
    account_email: &str,
    api_state: squelch_api::ApiState,
    mcp_cancel: CancellationToken,
) -> anyhow::Result<axum::Router> {
    let mcp_service = squelch_mcp::streamable_http_service(store, account_email, mcp_cancel)?;
    let app = squelch_api::router(api_state).nest_service(squelch_mcp::MCP_PATH, mcp_service);
    Ok(app)
}

/// One runtime hosting the sync loop (READ credential only), both HTTP doors,
/// the auth-mail shredder, and background embedder init. Ctrl-C cancels MCP
/// sessions, stops axum, then flushes sync.
fn cmd_serve(
    config: Config,
    cap_sources: Stage2CapSources,
    args: &ServeArgs,
) -> Result<(), squelch_core::CoreError> {
    // Fail fast on config/address problems before opening the store or runtime.
    let bind = resolve_bind(args)?;
    let email = config.require_account_email()?;
    let client = config.oauth_client()?;

    let store = SqliteStore::open(&config.db_path)?;
    let account_id = store.ensure_account(&email)?;
    let store = Arc::new(store);

    let backend = config.credential_backend;
    let creds_path = config.resolve_credentials_path();

    // Manual-refresh signal: `POST /client/refresh` fires it, the poll loop
    // wakes early.
    let refresh = Arc::new(tokio::sync::Notify::new());

    // Wake channel: the store pokes it on every real `append_event`, and each
    // open `GET /client/events` stream re-reads the table past its own cursor.
    // The payload is only a hint, so capacity just bounds how far a slow reader
    // falls behind before taking the harmless Lagged path.
    let (event_tx, _) = tokio::sync::broadcast::channel::<i64>(256);
    store.attach_event_notifier(event_tx.clone())?;

    // The APNs pusher is the second reader of that log, with its own persisted
    // cursor. Absence of `SQUELCH_RELAY_URL` is the whole feature flag. A relay
    // that IS configured but whose client won't build is a misconfiguration, not
    // "the feature is off" — say so loudly and keep serving mail.
    let pusher = match squelch_core::push::Pusher::from_config(
        store.clone() as Arc<dyn squelch_core::store::Store>,
        account_id,
        &config,
    ) {
        Ok(pusher) => pusher,
        Err(e) => {
            eprintln!(
                "squelchd: APNs pusher NOT started: a relay is configured but its HTTP client could not be built: {e}"
            );
            None
        }
    };
    // Subscribe BEFORE the sender is handed to the human door, so nothing
    // appended during startup is missed.
    let pusher_wake = event_tx.subscribe();

    // The human door refuses to build without SQUELCH_API_TOKEN. The WRITE-bound
    // credential store attached here is what enables the action endpoints; the
    // sync engine below gets a separate READ-bound store and never sees this one.
    let api_state = squelch_api::ApiState::from_env(store.clone(), &email)
        .map_err(|e| other_err(format!("{e}")))?
        .with_write_credentials(backend, email.clone(), creds_path.clone(), client.clone())
        .with_refresh(refresh.clone())
        .with_event_notifier(event_tx)
        .with_stage2_prices(config.stage2.price_in_per_mtok, config.stage2.price_out_per_mtok)
        .with_stage2_model(
            config.stage2.model.clone(),
            config.stage2.stage2_provider.map(|p| p.as_str().to_string()),
        )
        .with_stage2_caps(
            config.stage2.thread_daily_cap,
            config.stage2.sender_daily_cap,
            config.stage2.global_daily_cap,
            cap_sources,
        )
        .with_stage1_config(
            config.stage1.model.clone(),
            config.stage1.price_in_per_mtok,
            config.stage1.price_out_per_mtok,
            config.stage1.global_daily_cap,
        );

    let runtime = build_runtime()?;
    runtime.block_on(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mcp_cancel = CancellationToken::new();

        // The human door needs the same shutdown signal: SSE streams are
        // infinite and `with_graceful_shutdown` waits for open connections, so
        // without this one resident client holds the daemon open forever.
        let api_state = api_state.with_shutdown(shutdown_rx.clone());

        // POSTs opaque pings to the blind relay. Shares the sync loop's shutdown
        // watch and is awaited on the way out so it stops gracefully.
        let pusher_handle = match pusher {
            Some(pusher) => {
                eprintln!("squelchd: APNs pusher enabled (relay configured)");
                let shutdown_rx = shutdown_rx.clone();
                Some(tokio::spawn(
                    async move { pusher.run(pusher_wake, shutdown_rx).await },
                ))
            }
            None => {
                // The normal case — iOS push is opt-in — so a detail, not a warning.
                eprintln!(
                    "squelchd: APNs pusher disabled (no SQUELCH_RELAY_URL / [pusher] relay_url)"
                );
                None
            }
        };

        // No embedder override: the loop resolves it from the shared store each
        // tick, so it picks up the background-attached one.
        let sync_handle = {
            let store = store.clone();
            let email = email.clone();
            let config = config.clone();
            let refresh = refresh.clone();
            let creds =
                make_credential_store(backend, account_id, email.clone(), creds_path, client);
            tokio::spawn(async move {
                SyncEngine::new(store, creds, account_id, email, config)
                    .with_refresh(refresh)
                    .run(shutdown_rx)
                    .await
            })
        };

        // Auth-mail retention runs here because this process owns the write
        // credential (sync is bound to gmail.readonly by hard invariant). No-op
        // unless the shredder is enabled AND a write credential exists.
        {
            let shred_state = api_state.clone();
            tokio::spawn(async move {
                // Stagger past the startup sync burst; retention is in days.
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                loop {
                    match squelch_api::run_shred_pass(&shred_state).await {
                        Ok(0) => {}
                        Ok(n) => eprintln!("squelchd: shredder trashed {n} old auth message(s)"),
                        // Never fatal; the error is redacted by construction.
                        Err(_) => eprintln!("squelchd: shredder pass failed"),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            });
        }

        // Bind BEFORE building the embedder: its first-run model download must
        // not leave the doors unreachable.
        let app = build_serve_router(store.clone(), &email, api_state, mcp_cancel.clone())
            .map_err(squelch_core::CoreError::Other)?;
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| other_err(format!("bind {bind}: {e}")))?;
        let bound = listener.local_addr().unwrap_or(bind);
        // Single startup line. No tokens or message content are ever logged.
        eprintln!(
            "squelchd: serving agent door http://{bound}/mcp and human door http://{bound}/client/*"
        );

        // Build off the async workers and attach to the shared store when ready;
        // search is keyword-only until then.
        {
            let store = store.clone();
            let config = config.clone();
            eprintln!(
                "squelchd: initializing semantic-recall embedder in the background \
                 (first run downloads the model; the server is already serving, \
                 search is keyword-only until the embedder is ready)"
            );
            tokio::spawn(async move {
                let built = tokio::task::spawn_blocking(move || build_embedder(&config)).await;
                match built {
                    Ok(Some(embedder)) => match store.attach_embedder(embedder) {
                        Ok(_) => eprintln!(
                            "squelchd: embedder ready — semantic + hybrid search now enabled"
                        ),
                        Err(e) => eprintln!(
                            "squelchd: embedder attach failed ({e}); search stays keyword-only"
                        ),
                    },
                    Ok(None) => { /* build_embedder already logged the reason */ }
                    Err(e) => eprintln!(
                        "squelchd: embedder init task join error ({e}); search stays keyword-only"
                    ),
                }
            });
        }

        // Graceful shutdown: stop accepting, cancel MCP sessions, signal sync.
        let shutdown_signal = {
            let mcp_cancel = mcp_cancel.clone();
            let shutdown_tx = shutdown_tx.clone();
            async move {
                let _ = tokio::signal::ctrl_c().await;
                eprintln!("squelchd: shutdown requested; stopping doors and flushing sync...");
                mcp_cancel.cancel();
                let _ = shutdown_tx.send(true);
            }
        };

        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal)
            .await;

        // Tell sync to stop even if the server exited for another reason, then
        // wait for it to flush.
        let _ = shutdown_tx.send(true);
        match sync_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("squelchd: sync ended with error: {e}"),
            Err(e) => eprintln!("squelchd: sync task join error: {e}"),
        }
        if let Some(handle) = pusher_handle {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("squelchd: APNs pusher ended with error: {e}"),
                Err(e) => eprintln!("squelchd: APNs pusher task join error: {e}"),
            }
        }

        serve_result.map_err(|e| other_err(format!("http serve: {e}")))
    })?;

    eprintln!("squelchd: stopped.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `auth` parses: flags default off, and `--write`/`--headless --port` stick.
    #[test]
    fn auth_subcommand_parses() {
        let cli = Cli::parse_from(["squelchd", "auth"]);
        match cli.command {
            Command::Auth(args) => {
                assert!(!args.write);
                assert!(!args.headless);
                assert_eq!(args.port, DEFAULT_HEADLESS_PORT);
            }
            _ => panic!("expected auth subcommand"),
        }

        let cli = Cli::parse_from(["squelchd", "auth", "--write", "--headless", "--port", "9100"]);
        match cli.command {
            Command::Auth(args) => {
                assert!(args.write);
                assert!(args.headless);
                assert_eq!(args.port, 9100);
            }
            _ => panic!("expected auth subcommand"),
        }
    }

    /// Plain `auth` mints READ only; `--write` mints WRITE then READ. The
    /// two-door split must survive as separate kinds, separate slots, and
    /// non-overlapping scopes.
    #[test]
    fn auth_plan_write_mints_both_credentials() {
        use squelch_core::credentials::CredentialKind;

        let read_only = auth_plan(&AuthArgs {
            write: false,
            headless: false,
            port: DEFAULT_HEADLESS_PORT,
        });
        assert_eq!(read_only, vec![AuthScopes::Read]);

        let both = auth_plan(&AuthArgs {
            write: true,
            headless: true,
            port: DEFAULT_HEADLESS_PORT,
        });
        assert_eq!(both, vec![AuthScopes::Write, AuthScopes::Read]);
        assert_eq!(both[0].kind(), CredentialKind::Write);
        assert_eq!(both[1].kind(), CredentialKind::Read);
        assert_ne!(
            CredentialKind::Write.slot_key("you@x.com"),
            CredentialKind::Read.slot_key("you@x.com"),
            "the two credentials must land in separate storage slots"
        );

        let (write_scopes, read_scopes) = (both[0].scopes(), both[1].scopes());
        assert!(
            write_scopes.iter().all(|s| !read_scopes.contains(s)),
            "write and read scope sets must not be merged into one consent"
        );
    }

    /// `serve` parses, with and without an explicit `--bind`.
    #[test]
    fn serve_subcommand_parses() {
        let cli = Cli::parse_from(["squelchd", "serve"]);
        match cli.command {
            Command::Serve(args) => assert!(args.bind.is_none()),
            _ => panic!("expected serve subcommand"),
        }

        let cli = Cli::parse_from(["squelchd", "serve", "--bind", "0.0.0.0:9000"]);
        match cli.command {
            Command::Serve(args) => assert_eq!(args.bind.as_deref(), Some("0.0.0.0:9000")),
            _ => panic!("expected serve subcommand"),
        }
    }

    /// Bind resolution: flag > env > loopback default, and a bad value errors.
    #[test]
    fn resolve_bind_precedence_and_default() {
        let args = ServeArgs {
            bind: Some("127.0.0.1:9999".to_string()),
        };
        assert_eq!(
            resolve_bind(&args).unwrap(),
            "127.0.0.1:9999".parse::<SocketAddr>().unwrap()
        );

        let args = ServeArgs { bind: None };
        // Guard against a stray env var in the test process.
        unsafe {
            std::env::remove_var("SQUELCH_BIND");
        }
        assert_eq!(
            resolve_bind(&args).unwrap(),
            DEFAULT_BIND_ADDR.parse::<SocketAddr>().unwrap()
        );

        let args = ServeArgs {
            bind: Some("not-an-addr".to_string()),
        };
        assert!(resolve_bind(&args).is_err());
    }

    /// Both doors are mounted: `/client/stats` is bearer-gated (401, not 404)
    /// and `/mcp` exists.
    #[tokio::test]
    async fn router_mounts_both_doors() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let store = Arc::new(SqliteStore::open_in_memory().expect("in-memory store"));
        let account_id = store.ensure_account("me@localhost").expect("account");
        let api_state = squelch_api::ApiState::new(store.clone(), account_id, "test-token")
            .expect("api state");
        let cancel = CancellationToken::new();
        let app = build_serve_router(store, "me@localhost", api_state, cancel)
            .expect("router builds");

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "/client/stats must be mounted (auth-gated, not missing)"
        );

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "/client/stats must answer 200 with a valid bearer"
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "/mcp must be mounted"
        );
    }
}
