//! `squelchd` — the squelch daemon / CLI.
//!
//! - `auth`: run the OAuth consent flow and store tokens (keyring or file).
//! - `run`: sync-only loop, no HTTP.
//! - `serve`: sync loop plus one axum server hosting the agent door (`/mcp`) and
//!   the human door (`/client/*`).

use clap::{Args, Parser, Subcommand};
use squelch_core::auth::{
    AuthFlowOptions, AuthScopes, ConsentBind, CredentialTransfer, DEFAULT_HEADLESS_PORT,
    TransferCredential, decode_transfer, encode_transfer, run_auth_flow, run_broker_flow,
    run_export_flow,
};
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
    /// `--broker <URL>` needs no port and no tunnel at all: consent lands on a
    /// relay that parks the code for this daemon to collect.
    ///
    /// HEADLESS WITHOUT A TUNNEL OR A RELAY: run `squelchd auth --export` on a
    /// machine that has a browser and pipe the blob it prints into
    /// `squelchd auth --import` on the daemon's host. Both machines must use the
    /// same OAuth client_id/client_secret.
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

    /// Fixed port for --headless, and for --export --expose-consent-listener
    /// (default 8847). Ignored otherwise.
    #[arg(long, default_value_t = DEFAULT_HEADLESS_PORT)]
    port: u16,

    /// Run consent through a hosted consent relay instead of a loopback
    /// listener.
    ///
    /// For hosts where no browser can reach 127.0.0.1 (docker, a NAS, a VPS):
    /// this prints a link to open on any device, and the broker parks Google's
    /// one-time code until this daemon collects it. The broker never sees a
    /// token and cannot mint one: the PKCE verifier stays here. Replaces
    /// --headless and --port, so it conflicts with both.
    #[arg(long, value_name = "URL", env = "SQUELCH_BROKER_URL",
          conflicts_with_all = ["headless", "port"])]
    broker: Option<String>,

    /// Run consent HERE and print a credential blob on stdout; store nothing.
    ///
    /// For a daemon no browser can reach at all: run this on a laptop, then feed
    /// the one line it prints to `squelchd auth --import` on the daemon's host.
    /// Google delivers a code only to loopback on the machine running the
    /// browser, so the TOKEN moves instead of the code. Needs no account_email
    /// configured: it reports on stderr which mailbox Google named. All prose
    /// goes to stderr, so `squelchd auth --export > cred.txt` is a clean file.
    /// With --write the blob carries BOTH credentials. Replaces the loopback and
    /// broker transports, so it conflicts with both.
    #[arg(long, conflicts_with_all = ["headless", "broker", "import"])]
    export: bool,

    /// Store a credential blob read from STDIN, minted by `--export` elsewhere.
    ///
    /// Never argv: the blob is a live refresh token, and argv is visible in `ps`
    /// and lands in shell history. The blob's mailbox must match this daemon's
    /// configured account_email. What it carries decides what is stored, so this
    /// conflicts with --write.
    #[arg(long, conflicts_with_all = ["headless", "broker", "write", "export"])]
    import: bool,

    /// With --export: bind the consent listener to every interface instead of
    /// loopback, on --port.
    ///
    /// For running the export INSIDE a container, where `docker run -p
    /// 8847:8847` cannot reach a listener on the container's own 127.0.0.1.
    /// EXPOSURE: for the length of one consent, anything that can route to this
    /// host on that port can connect to the listener. What it could deliver is
    /// an authorization code, which is useless without the PKCE verifier held in
    /// this process and is checked against a per-run `state` first. Still a real
    /// change in reach, which is why it is opt-in.
    // `conflicts_with` is spelled out alongside `requires` because clap DROPS a
    // requirement that conflicts with a flag already on the line: with
    // `requires = "export"` alone, `--import --expose-consent-listener` parses.
    #[arg(long, requires = "export", conflicts_with = "import")]
    expose_consent_listener: bool,
}

/// Ceiling on a pasted credential blob. A real one is about a kilobyte; this is
/// slack, and it exists so a redirected file or a wedged pipe cannot be read
/// into memory without bound.
const MAX_BLOB_BYTES: usize = 64 * 1024;

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
    if args.export {
        return cmd_auth_export(config, args);
    }
    if args.import {
        return cmd_auth_import(config);
    }

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
        if args.broker.is_some() {
            eprintln!(
                "squelchd: each flow prints its OWN consent link; open them one at a time, \
                 as they appear."
            );
        } else if args.headless {
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

    let token = match args.broker.as_deref() {
        // The broker flow needs no listener, so no port and no tunnel; the
        // scope set, the slot it lands in, and the exchange are unchanged.
        // Both arms are handed `email`: the exchange refuses to return a token
        // for any other mailbox, so a consent finished on the wrong Google
        // account fails here instead of being stored under this one.
        Some(broker) => run_broker_flow(client, email, broker, scopes)?,
        None => {
            let opts = AuthFlowOptions {
                scopes,
                headless: args.headless,
                port: args.port,
            };
            run_auth_flow(client, email, &opts)?
        }
    };
    store_and_announce(backend, creds_path, email, kind, &token)?;
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

/// Persist one token into its kind's slot and say where it landed, reading it
/// back first so "stored" means stored. Never prints the token material.
fn store_and_announce(
    backend: CredentialBackend,
    creds_path: &std::path::Path,
    email: &str,
    kind: squelch_core::credentials::CredentialKind,
    token: &squelch_core::credentials::StoredToken,
) -> Result<(), squelch_core::CoreError> {
    store_token_backend(backend, creds_path, email, kind, token)?;
    let _ = load_token_backend(backend, creds_path, email, kind)?;
    match backend {
        CredentialBackend::Keyring => {
            println!(
                "\nStored {kind:?} credentials for {email} in the OS keyring (service \"squelch\")."
            );
        }
        CredentialBackend::File => {
            println!(
                "\nStored {kind:?} credentials for {email} in {} (mode 0600).",
                creds_path.display()
            );
        }
    }
    Ok(())
}

/// `--export`: run consent on THIS machine and print a transfer blob, storing
/// nothing.
///
/// No `account_email` is required or read: the exporting machine is whatever
/// laptop has a browser, not the daemon's host, and the mailbox is whatever
/// Google names on the consent screen. It is reported on stderr so the operator
/// can see they approved as the account they meant to.
fn cmd_auth_export(config: &Config, args: &AuthArgs) -> Result<(), squelch_core::CoreError> {
    let client = config.oauth_client()?;
    let bind = if args.expose_consent_listener {
        ConsentBind::AllInterfaces { port: args.port }
    } else {
        ConsentBind::Loopback
    };

    let plan = auth_plan(args);
    let total = plan.len();
    if total > 1 {
        eprintln!(
            "squelchd: --write exports BOTH credentials in one blob; {total} separate Google \
             consent screens are coming, one per credential. Approve them as the SAME account."
        );
    }

    let mut account: Option<String> = None;
    let mut credentials: Vec<TransferCredential> = Vec::new();
    for (i, scopes) in plan.iter().copied().enumerate() {
        eprintln!(
            "\nsquelchd: authorizing {} credential ({}/{total})...",
            scope_word(scopes),
            i + 1
        );
        let (mailbox, token) = run_export_flow(&client, scopes, bind)?;

        // A blob names ONE mailbox, and the importer stores every entry under
        // it. Two consents finished as different Google accounts would file a
        // stranger's token in this account's other slot.
        if let Some(first) = &account
            && !first.trim().eq_ignore_ascii_case(mailbox.trim())
        {
            return Err(squelch_core::CoreError::Credential(format!(
                "the first consent authorized {first} but this one authorized {mailbox}; a \
                 credential blob carries one account, so nothing was exported. Re-run \
                 `squelchd auth --export --write` and approve both screens as the same account."
            )));
        }
        // A blob without a refresh token buys one hour and then fails in a way
        // nobody connects back to this paste, so it is refused where the fix is
        // still obvious rather than on the daemon.
        if token.refresh_token.is_none() {
            return Err(squelch_core::CoreError::Credential(format!(
                "Google returned no refresh token for the {} credential, so this blob would stop \
                 working within the hour and nothing was exported. Revoke squelch at \
                 https://myaccount.google.com/permissions and re-run `squelchd auth --export`.",
                scope_word(scopes)
            )));
        }
        account = Some(mailbox);
        credentials.push(TransferCredential {
            kind: scopes.kind(),
            token,
        });
    }

    let account = account.ok_or_else(|| other_err("no credential was authorized".to_string()))?;
    let blob = encode_transfer(&CredentialTransfer::new(account.clone(), credentials))?;

    // The ONE thing on stdout, so `--export > cred.txt` is a file that imports
    // exactly as pasted.
    println!("{blob}");

    eprintln!("\nsquelchd: exported {total} credential(s) for {account}.");
    eprintln!(
        "squelchd: that line is a LIVE refresh token in plaintext. Import it, then delete it \
         from the file, the clipboard, and anywhere you pasted it."
    );
    eprintln!("squelchd: import it on the daemon's host with, for example:");
    eprintln!("    docker exec -i <container> squelchd auth --import < cred.txt");
    eprintln!(
        "squelchd: the importing host must use the SAME SQUELCH_CLIENT_ID and \
         SQUELCH_CLIENT_SECRET, since a refresh token only works for the client that minted it."
    );
    Ok(())
}

/// `--import`: store a blob minted by `--export` on another machine.
fn cmd_auth_import(config: &Config) -> Result<(), squelch_core::CoreError> {
    let email = config.require_account_email()?;
    let backend = config.credential_backend;
    let creds_path = config.resolve_credentials_path();

    let transfer = decode_transfer(&read_transfer_blob()?)?;
    check_transfer_account(&email, &transfer.account)?;
    // Every entry is judged before any is written: a blob half-stored leaves the
    // two slots disagreeing about which consent they came from.
    check_transfer_usable(&transfer)?;

    println!(
        "Importing {} credential(s) for {email} into the {backend:?} backend.",
        transfer.credentials.len()
    );
    for cred in &transfer.credentials {
        store_and_announce(backend, &creds_path, &email, cred.kind, &cred.token)?;
    }
    println!("\nsquelch can renew access automatically; the exported blob is no longer needed.");
    Ok(())
}

/// Read the blob from STDIN.
///
/// Never argv: the blob is a live refresh token, and argv is world-readable in
/// `ps` and lands in shell history. An interactive run gets a prompt rather than
/// a process that looks hung.
fn read_transfer_blob() -> Result<String, squelch_core::CoreError> {
    use std::io::{IsTerminal, Read};

    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprintln!("squelchd: paste the line from `squelchd auth --export`, then press enter:");
    }
    let mut buf = String::new();
    // One byte past the ceiling: reading it is how an overrun shows up.
    stdin
        .lock()
        .take(MAX_BLOB_BYTES as u64 + 1)
        .read_to_string(&mut buf)
        .map_err(|e| other_err(format!("reading the credential blob from stdin: {e}")))?;
    if buf.len() > MAX_BLOB_BYTES {
        return Err(other_err(format!(
            "more than {MAX_BLOB_BYTES} bytes arrived on stdin, which no credential blob is; \
             nothing was stored"
        )));
    }
    let blob = buf.trim().to_string();
    if blob.is_empty() {
        return Err(other_err(
            "nothing arrived on stdin, so there was no credential blob to import. Pipe one in: \
             `squelchd auth --import < cred.txt`."
                .to_string(),
        ));
    }
    Ok(blob)
}

/// Fail unless the blob was minted for the mailbox this daemon is configured
/// for.
///
/// Same rule as the consent-time check in squelch-core, for the same reason:
/// storing a token under the wrong address makes the sync loop ingest a
/// stranger's mail and serve it as this account's own. Trimmed and
/// case-insensitive, because Gmail addresses are ASCII and both values are
/// hand-typed somewhere.
fn check_transfer_account(configured: &str, blob: &str) -> Result<(), squelch_core::CoreError> {
    if configured.trim().eq_ignore_ascii_case(blob.trim()) {
        return Ok(());
    }
    Err(squelch_core::CoreError::Credential(format!(
        "this blob was exported for {blob}, but this daemon is configured for {configured}; \
         nothing was stored. Export again while signed in as {configured}, or fix account_email \
         in the config."
    )))
}

/// Fail unless every entry carries a refresh token.
///
/// An access token alone works for about an hour and then fails on a refresh
/// nobody will connect back to a paste from yesterday, so it is refused now.
fn check_transfer_usable(transfer: &CredentialTransfer) -> Result<(), squelch_core::CoreError> {
    for cred in &transfer.credentials {
        if cred.token.refresh_token.is_none() {
            return Err(squelch_core::CoreError::Credential(format!(
                "the {:?} credential in this blob has no refresh token, so it would stop working \
                 within the hour; nothing was stored. Re-run `squelchd auth --export` on the \
                 machine with the browser.",
                cred.kind
            )));
        }
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

/// Say, at startup, whether outbound mail can be tracked and how opens are
/// expected to get back — the two switches are independent and BOTH failure
/// modes are silent.
///
/// `[tracking] base_url` decides where the pixel POINTS, and so which door the
/// recipient's mail client knocks on. `[pusher] relay_url` decides whether this
/// daemon DRAINS a relay. Point the pixel at a relay while leaving `relay_url`
/// unset and every layer still reports success: the pixel serves, the relay
/// buffers the row, and this daemon simply never asks for it. From Passband
/// that is indistinguishable from nobody ever opening the mail.
fn report_tracking_posture(config: &Config) {
    let configured = config
        .tracking
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let Some(base) = configured else {
        eprintln!(
            "squelchd: read tracking disabled (no SQUELCH_TRACK_URL / [tracking] base_url); sends go out untracked"
        );
        return;
    };

    // The human door drops a scheme-less base URL, because it would ride into
    // outbound HTML as a RELATIVE url and resolve against the recipient's mail
    // client. Silent there; loud here.
    let lower = base.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        eprintln!(
            "squelchd: read tracking DISABLED: base_url `{base}` has no http(s) scheme, so it was rejected; sends go out untracked"
        );
        return;
    }

    eprintln!("squelchd: read tracking enabled, pixel at {base}/t/{{token}}");
    if config
        .pusher
        .relay_url
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty)
    {
        eprintln!(
            "squelchd:   no relay configured, so opens are recorded ONLY if that URL reaches THIS daemon's /t/ route. \
             If it points at a relay, opens will buffer there and never be collected."
        );
    }
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
    let event_tx = squelch_api::attach_event_channel(&store)?;

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

    // The opens poller runs the same relay in the other direction: it collects
    // read-tracking pixel hits the relay saw and appends the `opened` events the
    // pusher then delivers. Gated on the SAME `SQUELCH_RELAY_URL`; a daemon
    // whose pixel is reached directly needs none of this.
    let opens_poller = match squelch_core::tracking::OpensPoller::from_config(
        store.clone() as Arc<dyn squelch_core::store::Store>,
        account_id,
        &config,
    ) {
        Ok(poller) => poller,
        Err(e) => {
            eprintln!(
                "squelchd: opens poller NOT started: a relay is configured but its HTTP client could not be built: {e}"
            );
            None
        }
    };

    // Read tracking has two independent switches and no runtime error path: a
    // send the client asked to track just goes out untracked, and an open the
    // daemon never collects looks exactly like nobody opening the mail. Neither
    // is visible from Passband, so the posture is stated here or nowhere.
    report_tracking_posture(&config);

    // The human door refuses to build without SQUELCH_API_TOKEN. The shared
    // config->state wiring also attaches the WRITE-bound credential store that
    // enables the action endpoints; the sync engine below gets a separate
    // READ-bound store and never sees this one.
    let api_state = squelch_api::ApiState::from_config(store.clone(), &email, &config, cap_sources)
        .map_err(|e| other_err(format!("{e}")))?
        .with_refresh(refresh.clone())
        .with_event_notifier(event_tx);

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

        // Drains observed opens off the same relay. Shares the shutdown watch
        // and is awaited on the way out, like the pusher.
        let opens_handle = match opens_poller {
            Some(poller) => {
                eprintln!("squelchd: opens poller enabled (relay configured)");
                let shutdown_rx = shutdown_rx.clone();
                Some(tokio::spawn(async move { poller.run(shutdown_rx).await }))
            }
            None => {
                eprintln!(
                    "squelchd: opens poller disabled (no SQUELCH_RELAY_URL / [pusher] relay_url)"
                );
                None
            }
        };

        // READ-bound store, shared by the sync loop and the contacts harvest.
        let sync_creds = make_credential_store(backend, account_id, email.clone(), creds_path, client);

        // No embedder override: the loop resolves it from the shared store each
        // tick, so it picks up the background-attached one.
        let sync_handle = {
            let store = store.clone();
            let email = email.clone();
            let config = config.clone();
            let refresh = refresh.clone();
            let creds = sync_creds.clone();
            tokio::spawn(async move {
                SyncEngine::new(store, creds, account_id, email, config)
                    .with_refresh(refresh)
                    .run(shutdown_rx)
                    .await
            })
        };

        // One-time Sent-history sweep seeding recipient autocomplete (headers
        // only, read credential). Staggered past the startup sync burst; a
        // failure just retries on the next daemon start — the done flag is only
        // set on completion.
        {
            let store = store.clone();
            let email = email.clone();
            let config = config.clone();
            let creds = sync_creds.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(180)).await;
                let engine = SyncEngine::new(store, creds, account_id, email, config);
                if let Err(e) = engine.harvest_sent_contacts().await {
                    eprintln!(
                        "squelchd: sent-contacts harvest incomplete (retries next start): {e}"
                    );
                }
            });
        }

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
        if let Some(handle) = opens_handle {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("squelchd: opens poller ended with error: {e}"),
                Err(e) => eprintln!("squelchd: opens poller task join error: {e}"),
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
    use squelch_core::credentials::{CredentialKind, StoredToken};

    /// Every flag off, for tests that vary one at a time.
    fn bare_auth_args() -> AuthArgs {
        AuthArgs {
            write: false,
            headless: false,
            port: DEFAULT_HEADLESS_PORT,
            broker: None,
            export: false,
            import: false,
            expose_consent_listener: false,
        }
    }

    /// `auth` parses: flags default off, and `--write`/`--headless --port` stick.
    #[test]
    fn auth_subcommand_parses() {
        let cli = Cli::parse_from(["squelchd", "auth"]);
        match cli.command {
            Command::Auth(args) => {
                assert!(!args.write);
                assert!(!args.headless);
                assert_eq!(args.port, DEFAULT_HEADLESS_PORT);
                // `SQUELCH_BROKER_URL` is the other way this gets set, so the
                // default only holds when the environment is quiet.
                if std::env::var_os("SQUELCH_BROKER_URL").is_none() {
                    assert!(args.broker.is_none());
                }
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
        let read_only = auth_plan(&AuthArgs {
            write: false,
            ..bare_auth_args()
        });
        assert_eq!(read_only, vec![AuthScopes::Read]);

        // The plan is the scope sets, not the transport: a broker run mints the
        // same two credentials into the same two slots, and an export packs the
        // same two into one blob.
        let both = auth_plan(&AuthArgs {
            write: true,
            broker: Some("https://auth.passband.email".to_string()),
            ..bare_auth_args()
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

    /// `--broker` REPLACES the loopback listener rather than decorating it, so
    /// asking for both is a usage error rather than a silent pick.
    #[test]
    fn broker_flag_parses_and_excludes_the_loopback_flags() {
        let cli = Cli::parse_from([
            "squelchd",
            "auth",
            "--broker",
            "https://auth.passband.email",
        ]);
        match cli.command {
            Command::Auth(args) => {
                assert_eq!(args.broker.as_deref(), Some("https://auth.passband.email"));
                assert!(!args.headless);
            }
            _ => panic!("expected auth subcommand"),
        }

        for argv in [
            vec![
                "squelchd",
                "auth",
                "--broker",
                "https://auth.passband.email",
                "--headless",
            ],
            vec![
                "squelchd",
                "auth",
                "--broker",
                "https://auth.passband.email",
                "--port",
                "9100",
            ],
        ] {
            assert!(Cli::try_parse_from(&argv).is_err(), "{argv:?} was accepted");
        }
    }

    /// `--export` and `--import` are transports of their own: each replaces the
    /// loopback listener and the broker rather than decorating them, and asking
    /// for two at once is a usage error rather than a silent pick.
    #[test]
    fn export_and_import_parse_and_exclude_the_other_transports() {
        let cli = Cli::parse_from(["squelchd", "auth", "--export", "--write"]);
        match cli.command {
            Command::Auth(args) => {
                assert!(args.export);
                assert!(args.write, "one blob can carry both credentials");
                assert!(!args.import);
                assert!(!args.expose_consent_listener, "exposure is opt-in");
            }
            _ => panic!("expected auth subcommand"),
        }

        // The exposed listener needs a FIXED port, since that is what a
        // published container port maps to.
        let cli = Cli::parse_from([
            "squelchd",
            "auth",
            "--export",
            "--expose-consent-listener",
            "--port",
            "9100",
        ]);
        match cli.command {
            Command::Auth(args) => {
                assert!(args.expose_consent_listener);
                assert_eq!(args.port, 9100);
            }
            _ => panic!("expected auth subcommand"),
        }

        let cli = Cli::parse_from(["squelchd", "auth", "--import"]);
        match cli.command {
            Command::Auth(args) => {
                assert!(args.import);
                assert!(!args.export);
            }
            _ => panic!("expected auth subcommand"),
        }

        // `--broker` also reads SQUELCH_BROKER_URL, which counts as present for
        // conflict purposes, so the broker pairs only mean anything with the
        // environment quiet.
        let broker_env_quiet = std::env::var_os("SQUELCH_BROKER_URL").is_none();
        let mut bad: Vec<Vec<&str>> = vec![
            vec!["squelchd", "auth", "--export", "--import"],
            vec!["squelchd", "auth", "--export", "--headless"],
            vec!["squelchd", "auth", "--import", "--headless"],
            // The blob decides what it carries, so --write has nothing to say.
            vec!["squelchd", "auth", "--import", "--write"],
            // Exposure is a property of the export listener and nothing else.
            vec!["squelchd", "auth", "--expose-consent-listener"],
            vec!["squelchd", "auth", "--import", "--expose-consent-listener"],
        ];
        if broker_env_quiet {
            bad.push(vec![
                "squelchd",
                "auth",
                "--export",
                "--broker",
                "https://auth.passband.email",
            ]);
            bad.push(vec![
                "squelchd",
                "auth",
                "--import",
                "--broker",
                "https://auth.passband.email",
            ]);
        }
        for argv in bad {
            assert!(Cli::try_parse_from(&argv).is_err(), "{argv:?} was accepted");
        }
    }

    /// A blob minted for another mailbox must be refused by name: storing it
    /// would make the sync loop ingest a stranger's mail as this account's.
    #[test]
    fn an_imported_blob_for_another_account_is_refused_and_names_both() {
        let err = check_transfer_account("me@gmail.com", "someone.else@gmail.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("someone.else@gmail.com"), "{err}");
        assert!(err.contains("me@gmail.com"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");

        // Google echoes its own casing and both values are hand-typed, so
        // neither case nor stray whitespace is a different mailbox.
        assert!(check_transfer_account("me@gmail.com", "me@gmail.com").is_ok());
        assert!(check_transfer_account("Me@Gmail.COM", " me@gmail.com\n").is_ok());
        // A subaddress or another domain IS a different mailbox.
        assert!(check_transfer_account("me@gmail.com", "me+x@gmail.com").is_err());
        assert!(check_transfer_account("me@gmail.com", "me@example.com").is_err());
    }

    /// An access-token-only entry works for an hour and then fails in a way
    /// nobody connects back to the paste, so import refuses it up front.
    #[test]
    fn an_imported_blob_without_a_refresh_token_is_refused() {
        let entry = |refresh: Option<&str>| TransferCredential {
            kind: CredentialKind::Write,
            token: StoredToken {
                access_token: "access".to_string(),
                refresh_token: refresh.map(|r| r.to_string()),
                expires_at: None,
            },
        };

        let usable =
            CredentialTransfer::new("me@gmail.com".to_string(), vec![entry(Some("refresh"))]);
        assert!(check_transfer_usable(&usable).is_ok());

        let doomed = CredentialTransfer::new("me@gmail.com".to_string(), vec![entry(None)]);
        let err = check_transfer_usable(&doomed).unwrap_err().to_string();
        assert!(err.contains("no refresh token"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");

        // Judged before ANY entry is written, so one bad credential cannot
        // leave the two slots disagreeing about which consent they came from.
        let mixed = CredentialTransfer::new(
            "me@gmail.com".to_string(),
            vec![entry(Some("refresh")), entry(None)],
        );
        assert!(check_transfer_usable(&mixed).is_err());
    }

    /// The daemon must read exactly what the exporter wrote, prefix included.
    #[test]
    fn a_blob_round_trips_from_export_to_import() {
        let minted = CredentialTransfer::new(
            "me@gmail.com".to_string(),
            vec![TransferCredential {
                kind: CredentialKind::Read,
                token: StoredToken {
                    access_token: "access".to_string(),
                    refresh_token: Some("refresh".to_string()),
                    expires_at: None,
                },
            }],
        );
        let blob = encode_transfer(&minted).unwrap();
        // What a terminal paste actually delivers.
        let landed = decode_transfer(&format!("{blob}\n")).unwrap();
        assert!(check_transfer_account("me@gmail.com", &landed.account).is_ok());
        assert!(check_transfer_usable(&landed).is_ok());
        assert_eq!(landed.credentials[0].kind, CredentialKind::Read);
        assert_eq!(
            landed.credentials[0].token.refresh_token.as_deref(),
            Some("refresh")
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
