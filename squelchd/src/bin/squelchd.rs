//! `squelchd` — the squelch daemon / CLI.
//!
//! Subcommands:
//! - `auth`: run the OAuth consent flow and store tokens in the configured
//!   backend (keyring or file). Requires your own Google Cloud "Desktop app"
//!   OAuth client (see [`squelch_core::config`] docs).
//! - `run`: sync-only loop (back-compat). Drives the Gmail sync engine and
//!   nothing else.
//! - `serve`: the UNIFIED process. One tokio runtime hosts the sync loop AND a
//!   single axum server that mounts BOTH doors — the agent door (MCP Streamable
//!   HTTP at `/mcp`, via [`squelch_mcp`]) and the human door (the authenticated
//!   `/client/*` API, via [`squelch_api`]). Bind from `--bind`/`SQUELCH_BIND`
//!   (default loopback `127.0.0.1:8848`); a reverse proxy such as
//!   `tailscale serve` is expected to front it.

use clap::{Args, Parser, Subcommand};
use squelch_core::auth::{AuthFlowOptions, AuthScopes, DEFAULT_HEADLESS_PORT, run_auth_flow};
use squelch_core::config::{Config, CredentialBackend};
use squelch_core::credentials::{
    FileCredentialStore, KeyringCredentialStore, load_token_backend, store_token_backend,
};
use squelch_core::embed::{Embedder, FastEmbedder};
use squelch_core::store::SqliteStore;
use squelch_core::sync::SyncEngine;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Default bind address for `serve`. Loopback ONLY: a reverse proxy
/// (`tailscale serve`) fronts this listener. We never default to a
/// non-loopback interface, and never silently widen it.
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
    /// Plain `auth` mints the READ credential (gmail.readonly) used by the sync
    /// daemon. `auth --write` mints the separate WRITE credential
    /// (gmail.modify + gmail.send) used only by human-door action endpoints; it
    /// is stored in a distinct slot and never touched by sync/triage.
    ///
    /// HEADLESS: on a box with no browser/keyring, run
    /// `squelchd auth [--write] --headless [--port N]`. It prints the consent
    /// URL and binds a FIXED loopback port (default 8847). Forward it from your
    /// laptop with `ssh -L 8847:127.0.0.1:8847 <host>`, then open the URL in
    /// your local browser to complete consent.
    Auth(AuthArgs),
    /// Run the sync loop ONLY (back-compat). No HTTP doors are served.
    Run,
    /// Run the UNIFIED daemon: the sync loop plus one HTTP server hosting both
    /// the agent door (`/mcp`) and the human door (`/client/*`).
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Address to bind the unified HTTP server to (both doors). Defaults to the
    /// loopback `127.0.0.1:8848`, overridable via `SQUELCH_BIND`. Keep it on
    /// loopback and front it with a reverse proxy (`tailscale serve`).
    #[arg(long)]
    bind: Option<String>,
}

#[derive(Args)]
struct AuthArgs {
    /// Mint the WRITE credential (gmail.modify + gmail.send) instead of the
    /// default read-only credential. Stored in a separate slot.
    #[arg(long)]
    write: bool,

    /// Headless mode: do NOT auto-open a browser, and bind the loopback
    /// listener to a FIXED port so it can be SSH-forwarded from your laptop
    /// (`ssh -L <port>:127.0.0.1:<port> <host>`).
    #[arg(long)]
    headless: bool,

    /// Fixed loopback port for --headless (default 8847). Ignored otherwise.
    #[arg(long, default_value_t = DEFAULT_HEADLESS_PORT)]
    port: u16,
}

fn load_config(cli: &Cli) -> Config {
    match &cli.config {
        Some(path) => Config::load_from(path),
        None => Config::load(),
    }
}

/// Build the on-box semantic-recall embedder from config. Returns `None` (with a
/// single redacted stderr notice) if the model fails to construct — semantic
/// recall then degrades gracefully: sync, triage, and keyword search all keep
/// working, only vector recall is unavailable. First construction downloads the
/// ONNX weights to the configured cache dir (fastembed logs its own progress; the
/// core embedder logs a one-line first-download notice).
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = load_config(&cli);

    let result = match &cli.command {
        Command::Auth(args) => cmd_auth(&config, args),
        Command::Run => run_daemon(config),
        Command::Serve(args) => cmd_serve(config, args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Errors may reference missing credentials etc. — safe to print, we
            // never put tokens or secrets into error strings.
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run the OAuth consent flow and persist tokens for the configured account.
/// The scope set (read vs write) and the storage slot are chosen from the flags;
/// the storage backend (keyring vs file) comes from config.
fn cmd_auth(config: &Config, args: &AuthArgs) -> Result<(), squelch_core::CoreError> {
    let client = config.oauth_client()?;
    let email = config.require_account_email()?;

    let scopes = if args.write {
        AuthScopes::Write
    } else {
        AuthScopes::Read
    };
    let kind = scopes.kind();
    let backend = config.credential_backend;
    let creds_path = config.resolve_credentials_path();

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
    let token = run_auth_flow(&client, &opts)?;
    store_token_backend(backend, &creds_path, &email, kind, &token)?;

    // Confirm persistence without ever printing the token material.
    let _ = load_token_backend(backend, &creds_path, &email, kind)?;
    let has_refresh = token.refresh_token.is_some();
    match backend {
        squelch_core::config::CredentialBackend::Keyring => {
            println!("\nStored {kind:?} credentials for {email} in the OS keyring (service \"squelch\").");
        }
        squelch_core::config::CredentialBackend::File => {
            println!(
                "\nStored {kind:?} credentials for {email} in {} (mode 0600).",
                creds_path.display()
            );
        }
    }
    if has_refresh {
        println!("A refresh token was captured; squelch can renew access automatically.");
    } else {
        println!(
            "WARNING: no refresh token was returned. You may need to revoke prior access at \
             https://myaccount.google.com/permissions and re-run `squelchd auth`."
        );
    }
    Ok(())
}

/// Run the daemon: load config, open the store, build the keyring-backed
/// credential store, and drive the Gmail IMAP sync loop under a tokio runtime
/// with graceful Ctrl-C shutdown.
///
/// v0 resolves exactly one account (`config.account_email`), but `account_id`
/// flows through the whole engine so multi-tenant is a data change, not a
/// rewrite.
fn run_daemon(config: Config) -> Result<(), squelch_core::CoreError> {
    // Resolve the single v0 account and its OAuth client up front so we fail fast
    // with a helpful message before spinning up the async runtime.
    let email = config.require_account_email()?;
    let client = config.oauth_client()?;

    // Open the store and ensure the account row exists; its id threads through
    // the engine (multi-tenant-shaped).
    let mut store = SqliteStore::open(&config.db_path)?;
    let account_id = store.ensure_account(&email)?;

    // On-box semantic recall (v1): build the embedder once, attach it to BOTH the
    // store (query-side: semantic_search/hybrid_search embed the query) and the
    // sync engine (write-side: embed at ingest + startup backfill). `None` keeps
    // everything working without vector recall.
    let embedder = build_embedder(&config);
    if let Some(e) = &embedder {
        store = store.with_embedder(e.clone())?;
    }

    let store = Arc::new(store);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| squelch_core::CoreError::Other(anyhow::anyhow!("tokio runtime: {e}")))?;

    // Sync ALWAYS uses the READ credential. Pick the concrete backend store per
    // config; both are Read-bound. SyncEngine is monomorphic over the store
    // type, so we branch here rather than through a trait object.
    let backend = config.credential_backend;
    let creds_path = config.resolve_credentials_path();
    runtime.block_on(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nsquelch: shutdown requested; finishing in-flight work...");
                let _ = shutdown_tx.send(true);
            }
        });

        match backend {
            CredentialBackend::Keyring => {
                let creds = Arc::new(KeyringCredentialStore::new(
                    account_id,
                    email.clone(),
                    client,
                ));
                let mut engine = SyncEngine::new(store, creds, account_id, email, config);
                if let Some(e) = embedder {
                    engine = engine.with_embedder(e);
                }
                engine.run(shutdown_rx).await
            }
            CredentialBackend::File => {
                let creds = Arc::new(FileCredentialStore::new(
                    account_id,
                    email.clone(),
                    creds_path,
                    client,
                ));
                let mut engine = SyncEngine::new(store, creds, account_id, email, config);
                if let Some(e) = embedder {
                    engine = engine.with_embedder(e);
                }
                engine.run(shutdown_rx).await
            }
        }
    })?;

    eprintln!("squelch: sync stopped.");
    Ok(())
}

/// Resolve the `serve` bind address: `--bind` wins, then `SQUELCH_BIND`, then
/// the loopback default. Parsed to a concrete [`SocketAddr`] so a bad value
/// fails fast before we open the store or bind anything.
fn resolve_bind(args: &ServeArgs) -> Result<SocketAddr, squelch_core::CoreError> {
    let raw = args
        .bind
        .clone()
        .or_else(|| std::env::var("SQUELCH_BIND").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
    raw.parse().map_err(|e| {
        squelch_core::CoreError::Other(anyhow::anyhow!("invalid bind address `{raw}`: {e}"))
    })
}

/// Build the unified axum router that hosts BOTH doors.
///
/// - `/mcp` — the agent door, an MCP Streamable HTTP service built through
///   [`squelch_mcp::streamable_http_service`]. Narrow, sealed-absent, zero write
///   capability. Its `mcp_cancel` token, when cancelled, tears down active MCP
///   sessions.
/// - `/client/*` — the human door, [`squelch_api::router`], which already layers
///   bearer auth onto every route. Hosts the only write/action capability.
///
/// Both doors share the same `Arc<SqliteStore>`; the agent door NEVER sees the
/// write credential (it only reads the store), preserving the two-door split.
fn build_serve_router(
    store: Arc<SqliteStore>,
    account_email: &str,
    api_state: squelch_api::ApiState,
    mcp_cancel: CancellationToken,
) -> anyhow::Result<axum::Router> {
    let mcp_service =
        squelch_mcp::streamable_http_service(store, account_email, mcp_cancel)?;
    // Human-door router carries its own bearer auth + state; merge the agent
    // door's nested service alongside it under a single app.
    let app = squelch_api::router(api_state)
        .nest_service(squelch_mcp::MCP_PATH, mcp_service);
    Ok(app)
}

/// The UNIFIED daemon: one runtime, one process, both doors + the sync loop.
///
/// Concurrency model:
/// - The Gmail [`SyncEngine`] runs on its own spawned task, driven by a `watch`
///   channel. It ALWAYS uses the READ credential (never the write token).
/// - One axum server hosts `/mcp` (agent door) and `/client/*` (human door).
/// - On Ctrl-C we: cancel the MCP token (drops active MCP sessions), tell axum
///   to stop accepting connections, then signal the sync loop to finish
///   in-flight work and flush. We await the sync task before returning.
fn cmd_serve(config: Config, args: &ServeArgs) -> Result<(), squelch_core::CoreError> {
    // Fail fast on config/address problems before opening the store or runtime.
    let bind = resolve_bind(args)?;
    let email = config.require_account_email()?;
    let client = config.oauth_client()?;

    let mut store = SqliteStore::open(&config.db_path)?;
    let account_id = store.ensure_account(&email)?;

    // On-box semantic recall (v1): one embedder, attached to the store (query
    // side, shared with the human door's search) and the sync engine (write side).
    let embedder = build_embedder(&config);
    if let Some(e) = &embedder {
        store = store.with_embedder(e.clone())?;
    }

    let store = Arc::new(store);

    let backend = config.credential_backend;
    let creds_path = config.resolve_credentials_path();

    // Human door refuses to build without SQUELCH_API_TOKEN — surface that now
    // (StateError carries either the missing-token message or a core error).
    // The action endpoints (the ONLY write capability) are enabled by attaching
    // a WRITE-bound credential store here. It is bound to CredentialKind::Write
    // inside `with_write_credentials`; the SyncEngine below is handed a SEPARATE
    // Read-bound store and never sees this one.
    let api_state = squelch_api::ApiState::from_env(store.clone(), &email)
        .map_err(|e| squelch_core::CoreError::Other(anyhow::anyhow!("{e}")))?
        .with_write_credentials(backend, email.clone(), creds_path.clone(), client.clone());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| squelch_core::CoreError::Other(anyhow::anyhow!("tokio runtime: {e}")))?;

    runtime.block_on(async move {
        // Sync shutdown signal (watch) + MCP session cancellation token.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mcp_cancel = CancellationToken::new();

        // Spawn the sync loop. It is monomorphic over the credential store, so
        // we pick the concrete Read-bound backend here. Sync NEVER touches the
        // write credential.
        let sync_handle = {
            let store = store.clone();
            let email = email.clone();
            let config = config.clone();
            let embedder = embedder.clone();
            tokio::spawn(async move {
                match backend {
                    CredentialBackend::Keyring => {
                        let creds = Arc::new(KeyringCredentialStore::new(
                            account_id,
                            email.clone(),
                            client,
                        ));
                        let mut engine = SyncEngine::new(store, creds, account_id, email, config);
                        if let Some(e) = embedder {
                            engine = engine.with_embedder(e);
                        }
                        engine.run(shutdown_rx).await
                    }
                    CredentialBackend::File => {
                        let creds = Arc::new(FileCredentialStore::new(
                            account_id,
                            email.clone(),
                            creds_path,
                            client,
                        ));
                        let mut engine = SyncEngine::new(store, creds, account_id, email, config);
                        if let Some(e) = embedder {
                            engine = engine.with_embedder(e);
                        }
                        engine.run(shutdown_rx).await
                    }
                }
            })
        };

        // Build the combined app and bind before announcing.
        let app = build_serve_router(store.clone(), &email, api_state, mcp_cancel.clone())
            .map_err(squelch_core::CoreError::Other)?;
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| squelch_core::CoreError::Other(anyhow::anyhow!("bind {bind}: {e}")))?;
        let bound = listener.local_addr().unwrap_or(bind);
        // Single startup line. No tokens or message content are ever logged.
        eprintln!(
            "squelchd: serving agent door http://{bound}/mcp and human door http://{bound}/client/*"
        );

        // Graceful shutdown: stop accepting, cancel MCP sessions, then signal
        // sync to flush.
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

        // The server has stopped accepting. Make sure sync is told to stop even
        // if the server exited for another reason, then wait for it to flush.
        let _ = shutdown_tx.send(true);
        match sync_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("squelchd: sync ended with error: {e}"),
            Err(e) => eprintln!("squelchd: sync task join error: {e}"),
        }

        serve_result
            .map_err(|e| squelch_core::CoreError::Other(anyhow::anyhow!("http serve: {e}")))
    })?;

    eprintln!("squelchd: stopped.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Explicit flag wins.
        let args = ServeArgs {
            bind: Some("127.0.0.1:9999".to_string()),
        };
        assert_eq!(
            resolve_bind(&args).unwrap(),
            "127.0.0.1:9999".parse::<SocketAddr>().unwrap()
        );

        // No flag, no env => loopback default.
        let args = ServeArgs { bind: None };
        // Guard against a stray env var in the test process.
        unsafe {
            std::env::remove_var("SQUELCH_BIND");
        }
        assert_eq!(
            resolve_bind(&args).unwrap(),
            DEFAULT_BIND_ADDR.parse::<SocketAddr>().unwrap()
        );

        // Garbage value errors rather than silently falling back.
        let args = ServeArgs {
            bind: Some("not-an-addr".to_string()),
        };
        assert!(resolve_bind(&args).is_err());
    }

    /// The unified router mounts BOTH doors: an unauthenticated `/client/stats`
    /// is rejected by the human door's bearer layer (401/… not 404), and `/mcp`
    /// exists (a bare GET is not 404). This proves composition without needing a
    /// live sync loop.
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

        // Human door present + auth-gated: no bearer => NOT 404.
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

        // With the correct bearer, the human door answers (not 404, not 401).
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

        // Agent door present: a bare GET to /mcp is handled by the MCP service,
        // which is NOT 404 (the path is mounted).
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
