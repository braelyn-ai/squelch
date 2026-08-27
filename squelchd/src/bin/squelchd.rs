//! `squelchd` — the squelch daemon / CLI.
//!
//! - `auth`: run the OAuth consent flow and store tokens (keyring or file).
//! - `run`: sync-only loop, no HTTP.
//! - `serve`: sync loop plus one axum server hosting the agent door (`/mcp`) and
//!   the human door (`/client/*`).
//! - `token`: issue / list / revoke the human door's per-device tokens, plus
//!   `first-paired`, the one machine-readable line the hosted warden reads.
//! - `pair`: mint a short code a new device trades for one of those tokens.

use chrono::{Duration, SecondsFormat};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use squelch_core::auth::{
    AuthFlowOptions, AuthScopes, ConsentBind, CredentialTransfer, DEFAULT_HEADLESS_PORT,
    TransferCredential, decode_transfer, encode_transfer, printable, run_auth_flow,
    run_broker_flow, run_export_flow, verify_transfer_credential,
};
use squelch_core::config::{Config, CredentialBackend, OAuthClientConfig, Stage2CapSources};
use squelch_core::credentials::{
    CredentialStore, FileCredentialStore, KeyringCredentialStore, load_token_backend,
    store_token_backend, store_tokens_backend, write_private,
};
use squelch_core::embed::{Embedder, LazyEmbedder, ReapOutcome};
use squelch_core::metrics::SyncMetrics;
use squelch_core::store::sqlite::device_tokens::PAIRING_TTL_SECS;
use squelch_core::store::{SqliteStore, Store};
use squelch_core::sync::{SyncEngine, wait_for_embedder_gate};
use squelch_core::types::AccountId;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

/// Loopback only, by design: a reverse proxy (`tailscale serve`) fronts this.
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8848";

#[derive(Parser)]
#[command(
    name = "squelchd",
    about = "squelch local-first email intelligence daemon"
)]
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
    /// Issue, list and revoke the per-device tokens the human door accepts.
    ///
    /// One token per device means one revocation per device: losing a phone
    /// costs that phone's row, not a rotation of the shared secret on every
    /// client you own. `SQUELCH_API_TOKEN` keeps working alongside these and is
    /// never deprecated.
    Token(TokenArgs),
    /// Mint a short pairing code (and a deep link) a new device can trade for
    /// its own token.
    ///
    /// The device claims it at `POST /client/pair`, which is the one human-door
    /// route served without a bearer, since the device has none yet. One code is
    /// live per account at a time, it expires in minutes, and it is spent on
    /// first use or after a handful of failed claims.
    Pair(PairArgs),
}

#[derive(Args)]
struct TokenArgs {
    #[command(subcommand)]
    command: TokenCommand,
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Mint a token and print it ONCE. squelchd stores only a hash and cannot
    /// show it again; a lost token is re-issued, never recovered.
    Issue {
        /// The label this token carries in `token list`, so the row to revoke
        /// later is the one you can recognize ("braelyn's iphone").
        #[arg(long)]
        name: String,
    },
    /// List every token this account has been issued, revoked ones included.
    /// Prints no token material, not even hashes.
    List,
    /// Revoke a token by id. Effective on the very next request: the door reads
    /// the row every time and caches nothing.
    Revoke {
        /// The id from `token list`.
        id: i64,
    },
    /// Print when a client device FIRST paired with this mailbox: one RFC3339
    /// timestamp, or the word `none`.
    ///
    /// MACHINE-READABLE, not a report. The hosted warden execs exactly this in a
    /// tenant pod to answer "did anybody ever actually run the app" (the
    /// activation signal), so the contract is one line on stdout and nothing
    /// else, forever.
    ///
    /// Its own subcommand rather than a flag on `token list`, and that is the
    /// point: a listing carries device NAMES, and a caller that only needs a
    /// timestamp must not be able to over-share by accident. One line out means
    /// there is nothing else in it to leak. The console-session exclusion and
    /// the "a revoked device still paired" rule both live in the store, in one
    /// place, rather than being re-derived by whoever reads the output.
    FirstPaired,
}

#[derive(Args)]
struct PairArgs {
    /// The base URL the device should claim against, baked into the deep link.
    /// Defaults to this daemon's loopback address, which is right when the
    /// device is this machine; point it at whatever a phone can actually reach
    /// (a `tailscale serve` hostname, say) when it is not.
    #[arg(long)]
    url: Option<String>,
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
    /// --headless, --port, --export, and --import rather than decorating them.
    // NO `conflicts_with_all` here, and none naming "broker" elsewhere: clap
    // counts an env-sourced value as present for conflicts, so with
    // SQUELCH_BROKER_URL exported every one of those pairs would fail on a flag
    // the operator never typed — including `--import`, which is the sanctioned
    // replacement for the broker (docs/BROKER.md: DO NOT DEPLOY). The conflicts
    // are enforced in `resolve_transport`, which can see WHERE the value came
    // from and only refuses what was actually asked for on the command line.
    #[arg(long, value_name = "URL", env = "SQUELCH_BROKER_URL")]
    broker: Option<String>,

    /// Run consent HERE and print a credential blob on stdout; store nothing.
    ///
    /// For a daemon no browser can reach at all: run this on a laptop, then feed
    /// the one line it prints to `squelchd auth --import` on the daemon's host.
    /// Google delivers a code only to loopback on the machine running the
    /// browser, so the TOKEN moves instead of the code. Needs no account_email
    /// configured, and reports on stderr which mailbox Google named; when one IS
    /// configured, the consent must land on it. Write the blob with
    /// `--out <path>` (mode 0600) rather than a redirect, which takes the
    /// ambient umask. With --write the blob carries BOTH credentials. Replaces
    /// the loopback and broker transports.
    #[arg(long, conflicts_with_all = ["headless", "import"])]
    export: bool,

    /// With --export: write the blob to PATH, created mode 0600, instead of
    /// stdout.
    ///
    /// `--export > cred.txt` creates that file with the ambient umask, which is
    /// 0644 on most hosts, and the file is a live refresh token. This is the
    /// same private write every other credential in squelch gets.
    #[arg(
        long,
        value_name = "PATH",
        requires = "export",
        conflicts_with = "import"
    )]
    out: Option<PathBuf>,

    /// Store a credential blob read from STDIN, minted by `--export` elsewhere.
    ///
    /// Never argv: the blob is a live refresh token, and argv is visible in `ps`
    /// and lands in shell history. The blob's mailbox must match this daemon's
    /// configured account_email. What it carries decides what is stored, so this
    /// conflicts with --write.
    #[arg(long, conflicts_with_all = ["headless", "write", "export"])]
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

/// Where one flag's value came from.
///
/// `--broker` also reads `SQUELCH_BROKER_URL`, and clap counts an env-sourced
/// value as PRESENT for conflict resolution. That is why the broker's conflicts
/// are enforced here instead: an ambient URL in the environment must not refuse
/// a transport the operator asked for by name, least of all `--import`, which is
/// what replaced the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagSource {
    /// Typed on this command line.
    CommandLine,
    /// A default or an environment variable: inherited, not asked for.
    Ambient,
}

/// Which of `auth`'s transport flags were typed rather than inherited.
#[derive(Debug, Clone, Copy)]
struct AuthFlagSources {
    broker: FlagSource,
    port: FlagSource,
}

impl AuthFlagSources {
    /// Read clap's own record of where each value came from.
    fn from_matches(matches: &clap::ArgMatches) -> Self {
        let Some(auth) = matches.subcommand_matches("auth") else {
            return Self {
                broker: FlagSource::Ambient,
                port: FlagSource::Ambient,
            };
        };
        let source = |id: &str| match auth.value_source(id) {
            Some(clap::parser::ValueSource::CommandLine) => FlagSource::CommandLine,
            _ => FlagSource::Ambient,
        };
        Self {
            broker: source("broker"),
            port: source("port"),
        }
    }
}

/// Which consent transport one `auth` run uses. Each REPLACES the others rather
/// than decorating them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transport {
    /// Consent here, print a blob, store nothing.
    Export,
    /// Store a blob minted elsewhere.
    Import,
    /// Consent lands on a relay that parks the code.
    Broker(String),
    /// Consent lands on a listener this process owns.
    Loopback,
}

/// The broker URL this run actually has, and the one notion of "a broker is
/// present" every decision here uses.
///
/// clap reports `SQUELCH_BROKER_URL=` as a VALUE, so an empty variable left in a
/// compose file would otherwise make plain `auth` a `Broker("")` run that dies on
/// a URL parse nobody asked for. Blank is how an environment says nothing. A
/// `--broker ""` that was TYPED is a different statement and still fails in
/// `broker_base`, where the operator can see what they typed.
fn broker_url(args: &AuthArgs, sources: AuthFlagSources) -> Option<&str> {
    let url = args.broker.as_deref()?;
    if sources.broker == FlagSource::CommandLine {
        return Some(url);
    }
    (!url.trim().is_empty()).then_some(url)
}

/// Pick the transport, and refuse only the combinations the operator actually
/// asked for.
///
/// An ambient `SQUELCH_BROKER_URL` is a default, not an instruction: any
/// transport named on the command line wins over it, silently and without an
/// exit 2 about a flag nobody typed. A `--broker` that WAS typed still conflicts
/// with every other transport, which is the check clap can no longer make.
fn resolve_transport(
    args: &AuthArgs,
    sources: AuthFlagSources,
) -> Result<Transport, squelch_core::CoreError> {
    let broker = broker_url(args, sources);
    let typed_broker = broker.is_some() && sources.broker == FlagSource::CommandLine;
    let refuse = |other: &str| {
        Err(other_err(format!(
            "--broker and {other} are two different ways to run consent, and this run asked for \
             both; pick one."
        )))
    };

    if args.export {
        if typed_broker {
            return refuse("--export");
        }
        return Ok(Transport::Export);
    }
    if args.import {
        if typed_broker {
            return refuse("--import");
        }
        return Ok(Transport::Import);
    }
    match broker {
        Some(url) if typed_broker => {
            if args.headless {
                return refuse("--headless");
            }
            if sources.port == FlagSource::CommandLine {
                return refuse("--port");
            }
            Ok(Transport::Broker(url.to_string()))
        }
        // Inherited from the environment: it is the transport only when nothing
        // on the command line names another one.
        Some(url) => {
            if args.headless || sources.port == FlagSource::CommandLine {
                Ok(Transport::Loopback)
            } else {
                Ok(Transport::Broker(url.to_string()))
            }
        }
        None => Ok(Transport::Loopback),
    }
}

/// Whether a `--port` typed on THIS command line will go unbound.
///
/// A fixed port is bound by `--headless`, which is what an SSH tunnel forwards,
/// and by `--export --expose-consent-listener`, which is what a published
/// container port maps to. Every other run takes an ephemeral port, so a typed
/// `--port` there is a tunnel or a `-p` mapping that will never be connected to,
/// and silence about it looks exactly like a port that was honored.
///
/// A typed port beside a TYPED `--broker` never reaches this: `resolve_transport`
/// refuses that pair. Beside an AMBIENT one it resolves to `Loopback`, where the
/// port is still ignored unless the run is headless.
fn typed_port_is_ignored(args: &AuthArgs, transport: &Transport, sources: AuthFlagSources) -> bool {
    if sources.port != FlagSource::CommandLine {
        return false;
    }
    let bound = match transport {
        Transport::Loopback => args.headless,
        Transport::Export => args.expose_consent_listener,
        Transport::Import | Transport::Broker(_) => false,
    };
    !bound
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
        CredentialBackend::File => Arc::new(FileCredentialStore::new(
            account_id, email, creds_path, client,
        )),
    }
}

/// Build the semantic-recall embedder. `None` on failure — search then degrades
/// to keyword-only.
///
/// The concrete [`LazyEmbedder`] rather than `Arc<dyn Embedder>`, because the
/// reaper in [`spawn_embedder_reaper`] needs the unload method and the trait
/// deliberately does not carry it: unloading is a property of THIS embedder, not
/// of embedding. Attach sites coerce.
fn build_embedder(config: &Config, metrics: Option<Arc<SyncMetrics>>) -> Option<Arc<LazyEmbedder>> {
    let built = LazyEmbedder::new(&config.embed.settings()).map(|e| match metrics {
        Some(m) => e.with_metrics(m),
        None => e,
    });
    match built {
        Ok(e) => Some(Arc::new(e)),
        Err(e) => {
            eprintln!(
                "squelch: embedder unavailable ({e}); semantic recall disabled \
                 (keyword search + triage unaffected)"
            );
            None
        }
    }
}

/// How often the reaper looks. A minute is far below any sane idle window and
/// costs one uncontended `try_lock` when there is nothing to do, so there is no
/// reason to make it a knob.
///
/// It is also a FLOOR on `[embed] idle_unload_secs`: nothing is unloaded
/// between ticks, so the effective window is the configured one rounded up to
/// the next minute. The startup line below prints both numbers rather than
/// leaving an operator who set 5 wondering why it took 65.
const EMBEDDER_REAP_INTERVAL_SECS: u64 = 60;

/// How many consecutive contended ticks before the reaper says so. An embed
/// holding the lock is normal and happens all day; a whole hour of them is a
/// session no reaper can ever reach, which is the memory this feature exists to
/// give back sitting there anyway.
const EMBEDDER_CONTENDED_TICKS_BEFORE_WARNING: u32 = 60;

/// Drop the embedding session once it has gone `idle_secs` without a call. That
/// session is 85-90% of a tenant daemon's memory (~250-300 MB against ~25-40 MB
/// for everything else, SQLite included) and a mailbox is idle nearly all the
/// time, so a daemon that holds it forever is a pod that is mostly ONNX arenas
/// waiting for mail. Reloading costs ~200 ms off the cached weights, paid by
/// whichever search or poll tick lands first.
///
/// `idle_secs == 0` means never unload: no task is spawned at all, and the
/// daemon behaves exactly as it did before this existed.
fn spawn_embedder_reaper(
    embedder: Arc<LazyEmbedder>,
    idle_secs: u64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if idle_secs == 0 {
        return;
    }
    let max_idle = std::time::Duration::from_secs(idle_secs);
    let interval = std::time::Duration::from_secs(EMBEDDER_REAP_INTERVAL_SECS);
    // Once, at startup, with the EFFECTIVE numbers: the configured window and
    // the tick that rounds it up are two different values and an operator who
    // reads only the first will misread every unload line that follows.
    eprintln!(
        "squelchd: embedder reaper armed: unloads after {idle_secs} s idle, \
         checked every {EMBEDDER_REAP_INTERVAL_SECS} s (so the session goes at \
         {idle_secs}-{} s past the last use)",
        idle_secs + EMBEDDER_REAP_INTERVAL_SECS
    );
    tokio::spawn(async move {
        let mut contended_run: u32 = 0;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                // An Err is the SENDER being dropped, which is the daemon on its
                // way out. It has to end the task rather than fall through: a
                // dropped sender makes `changed()` return instantly forever, and
                // this loop would spin a core until the process died.
                res = shutdown.changed() => {
                    if res.is_err() {
                        return;
                    }
                }
            }
            if *shutdown.borrow() {
                return;
            }
            // Dropping an ONNX session and trimming glibc's arenas are both
            // millisecond-scale rather than instant, and this runs on the same
            // runtime as the doors.
            let e = embedder.clone();
            let outcome = match tokio::task::spawn_blocking(move || e.reap_if_idle(max_idle)).await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    // A panic inside the reap (a session whose Drop blew up, say)
                    // must not vanish into a Kept: it would repeat every tick
                    // with the gauge pinned at 1 and nothing in the log. Say so
                    // and leave the contention count alone.
                    eprintln!(
                        "squelchd: embedder reap task join error ({e}); trying again next tick"
                    );
                    continue;
                }
            };
            match outcome {
                ReapOutcome::Unloaded => {
                    contended_run = 0;
                    eprintln!(
                        "squelchd: embedder unloaded after {idle_secs} s idle; \
                         reloads on the next embed or search"
                    );
                }
                ReapOutcome::Kept => contended_run = 0,
                ReapOutcome::Contended => {
                    contended_run += 1;
                    // ONCE per run, not per tick: the whole point is a line an
                    // operator can find, and a line every minute forever is a
                    // line nobody reads. The task keeps going either way — a
                    // reaper that gave up would be the leak it was built to
                    // stop.
                    // Every N ticks rather than once: permanent contention is
                    // the case this exists for, and one line at minute 60 has
                    // rotated out of the log by the time anyone looks.
                    if contended_run.is_multiple_of(EMBEDDER_CONTENDED_TICKS_BEFORE_WARNING) {
                        eprintln!(
                            "squelchd: the embedder lock has been held on every \
                             reap for {} minutes; the session is not being \
                             unloaded and its memory is not coming back",
                            EMBEDDER_CONTENDED_TICKS_BEFORE_WARNING as u64
                                * EMBEDDER_REAP_INTERVAL_SECS
                                / 60
                        );
                    }
                }
            }
        }
    });
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
        Ok(true) => eprintln!(
            "squelchd: mirrored .env settings into {}",
            config_path.display()
        ),
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

    // Parsed through `ArgMatches` rather than `Cli::parse()` so `auth` can see
    // which of its flags were typed and which came out of the environment.
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    let flag_sources = AuthFlagSources::from_matches(&matches);
    let (config, cap_sources) = load_config(&cli);

    let result = match &cli.command {
        Command::Auth(args) => cmd_auth(&config, args, flag_sources),
        Command::Run => run_daemon(config),
        Command::Serve(args) => cmd_serve(config, cap_sources, args),
        Command::Token(args) => cmd_token(&config, args),
        Command::Pair(args) => cmd_pair(&config, args),
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
fn cmd_auth(
    config: &Config,
    args: &AuthArgs,
    sources: AuthFlagSources,
) -> Result<(), squelch_core::CoreError> {
    let transport = resolve_transport(args, sources)?;
    if broker_url(args, sources).is_some() && !matches!(transport, Transport::Broker(_)) {
        eprintln!(
            "squelchd: SQUELCH_BROKER_URL is set, but this run asked for another transport on \
             the command line, so the broker is not being used."
        );
    }
    // Said once here, before the transport dispatch, so the export path is
    // covered too and says it exactly once.
    if typed_port_is_ignored(args, &transport, sources) {
        eprintln!(
            "squelchd: --port {} is ignored on this run; a fixed port is bound only by \
             --headless, and by --export --expose-consent-listener.",
            args.port
        );
    }

    let broker = match &transport {
        Transport::Export => return cmd_auth_export(config, args),
        Transport::Import => return cmd_auth_import(config),
        Transport::Broker(url) => Some(url.as_str()),
        Transport::Loopback => None,
    };

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
        if broker.is_some() {
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
        if let Err(e) = mint_credential(&client, &email, backend, &creds_path, scopes, args, broker)
        {
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
    broker: Option<&str>,
) -> Result<(), squelch_core::CoreError> {
    let kind = scopes.kind();
    println!(
        "Authorizing Gmail account: {email} [{}] via {:?} backend",
        scopes.label(),
        backend
    );

    let token = match broker {
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
    announce_stored(backend, creds_path, email, kind);
    Ok(())
}

/// Say where one credential landed. Split out because an import writes every
/// slot before it announces ANY of them: a line saying "stored" that a later
/// failure walks back is worse than no line at all.
fn announce_stored(
    backend: CredentialBackend,
    creds_path: &std::path::Path,
    email: &str,
    kind: squelch_core::credentials::CredentialKind,
) {
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
}

/// Fail unless both consents in one export named the same mailbox.
///
/// A blob names ONE mailbox, and the importer stores every entry under it. Two
/// consents finished as different Google accounts would file a stranger's token
/// in this account's other slot.
///
/// Both values are whatever Google's userinfo named, and this message reaches a
/// terminal, so both are disarmed on the way in: raw, an `ESC` in one clears the
/// screen and a `BEL` retitles the window.
fn check_export_same_account(first: &str, mailbox: &str) -> Result<(), squelch_core::CoreError> {
    if first.trim().eq_ignore_ascii_case(mailbox.trim()) {
        return Ok(());
    }
    let (first, mailbox) = (printable(first), printable(mailbox));
    Err(squelch_core::CoreError::Credential(format!(
        "the first consent authorized {first} but this one authorized {mailbox}; a \
         credential blob carries one account, so nothing was exported. Re-run \
         `squelchd auth --export --write` and approve both screens as the same account."
    )))
}

/// `--export`: run consent on THIS machine and print a transfer blob, storing
/// nothing.
///
/// No `account_email` is REQUIRED: the exporting machine is whatever laptop has
/// a browser, not the daemon's host, and the mailbox is whatever Google names on
/// the consent screen. It is reported on stderr so the operator can see they
/// approved as the account they meant to. When one IS configured here (same
/// binary, same config.toml, possibly the same .env), the check is available and
/// runs: a consent finished under the wrong Google session would otherwise mint
/// and PRINT a live refresh token, and printing it is the part that cannot be
/// taken back.
fn cmd_auth_export(config: &Config, args: &AuthArgs) -> Result<(), squelch_core::CoreError> {
    let client = config.oauth_client()?;
    let expected_account = config
        .account_email
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    if let Some(account) = expected_account {
        eprintln!(
            "squelchd: this machine is configured for {account}, so every consent must be \
             approved as that account."
        );
    }
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
        let (mailbox, token) = run_export_flow(&client, expected_account, scopes, bind)?;

        if let Some(first) = &account {
            check_export_same_account(first, &mailbox)?;
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

    match &args.out {
        // Mode 0600 from the moment it exists, which a shell redirect cannot do:
        // `> cred.txt` takes the ambient umask, and the file is a live token.
        Some(path) => {
            let mut bytes = blob.into_bytes();
            bytes.push(b'\n');
            write_private(path, &bytes)?;
            eprintln!(
                "\nsquelchd: wrote {} credential(s) for {account} to {} (mode 0600).",
                total,
                path.display()
            );
        }
        // The ONE thing on stdout, so a redirect is still a file that imports
        // exactly as pasted.
        None => {
            println!("{blob}");
            eprintln!("\nsquelchd: exported {total} credential(s) for {account}.");
        }
    }

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
///
/// A blob is unsigned JSON, so nothing it SAYS about itself is evidence: the
/// `account` field is the paster's claim, and so is the `kind` that decides
/// which slot each token lands in. Every entry is therefore taken to Google
/// before ANY of them is written, which is the same question the exchange asks
/// on every other path to a stored token.
fn cmd_auth_import(config: &Config) -> Result<(), squelch_core::CoreError> {
    let client = config.oauth_client()?;
    let email = config.require_account_email()?;
    let backend = config.credential_backend;
    let creds_path = config.resolve_credentials_path();

    let transfer = decode_transfer(&read_transfer_blob()?)?;
    // The cheap local checks first: no network is spent on a blob that could not
    // land whatever Google says about it.
    check_transfer_account(&email, &transfer.account)?;
    check_transfer_usable(&transfer)?;

    println!(
        "Checking {} credential(s) with Google before anything is stored...",
        transfer.credentials.len()
    );
    // What gets STORED is what verification minted, not what was pasted. The
    // refresh it already ran is the only part of a blob Google ever named, so
    // keeping the blob's access token and expiry alongside it would persist two
    // attacker-authored fields; an absent `expires_at` in particular reads as
    // "never expires" and would pin a bearer string of the paster's choosing on
    // every Gmail call.
    let mut entries: Vec<(
        squelch_core::credentials::CredentialKind,
        squelch_core::credentials::StoredToken,
    )> = Vec::new();
    for cred in &transfer.credentials {
        let verified = verify_transfer_credential(&client, &email, cred.kind, &cred.token)?;
        println!(
            "  {:?}: Google confirms this token opens {email} with the scopes the {:?} slot needs.",
            cred.kind, cred.kind
        );
        entries.push((cred.kind, verified));
    }

    // One unit: either both slots come from this import or neither does, so the
    // two doors can never end up describing different consents.
    store_tokens_backend(backend, &creds_path, &email, &entries)?;
    for (kind, _) in &entries {
        let _ = load_token_backend(backend, &creds_path, &email, *kind)?;
    }
    for (kind, _) in &entries {
        announce_stored(backend, &creds_path, &email, *kind);
    }
    println!("\nsquelch can renew access automatically; the exported blob is no longer needed.");
    Ok(())
}

/// Read the blob from STDIN.
///
/// Never argv: the blob is a live refresh token, and argv is world-readable in
/// `ps` and lands in shell history.
///
/// A TTY gets ONE LINE, because that is what a paste is and because reading to
/// EOF there would need a Ctrl-D nobody is expecting: the prompt would sit there
/// looking hung after the enter. A pipe still gets read to EOF, which is what
/// `< cred.txt` delivers. Both are capped.
fn read_transfer_blob() -> Result<String, squelch_core::CoreError> {
    use std::io::IsTerminal;

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return read_blob_capped(stdin.lock(), BlobRead::ToEndOfInput);
    }

    eprintln!("squelchd: paste the single line from `squelchd auth --export` and press enter.");
    eprintln!(
        "squelchd: the paste is not echoed. Piping the file in is still safer, since a terminal \
         keeps its own copy of anything it does echo: squelchd auth --import < cred.txt"
    );
    // Echo off for the same reason a password prompt does it, restored when the
    // guard drops.
    let echo = TerminalEcho::suppressed();
    let blob = read_blob_capped(stdin.lock(), BlobRead::OneLine);
    drop(echo);
    // The enter that ended the line was not echoed either.
    eprintln!();
    blob
}

/// How far [`read_blob_capped`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobRead {
    /// A paste at a prompt, which ends at the enter that follows it.
    OneLine,
    /// A pipe or a redirect, which ends at EOF.
    ToEndOfInput,
}

/// The read itself, split from the terminal handling so both shapes are
/// testable. Capped either way, because a redirected file or a wedged pipe is
/// not a blob.
fn read_blob_capped(
    reader: impl std::io::BufRead,
    how: BlobRead,
) -> Result<String, squelch_core::CoreError> {
    use std::io::{BufRead, Read};

    let mut buf = String::new();
    // One byte past the ceiling: reading it is how an overrun shows up.
    let mut capped = reader.take(MAX_BLOB_BYTES as u64 + 1);
    match how {
        BlobRead::OneLine => capped.read_line(&mut buf),
        BlobRead::ToEndOfInput => capped.read_to_string(&mut buf),
    }
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

/// Terminal echo, off for as long as this is alive and back on when it drops.
///
/// The line pasted at the import prompt is a live refresh token. A terminal that
/// echoes it leaves a copy in the scrollback of whatever window it was pasted
/// into, which outlives the paste by however long that window does.
///
/// Restoring is best effort by construction: a run killed by a signal never
/// reaches the drop, and the fix for that terminal is `stty sane`. Failing to
/// suppress at all is not fatal either, because refusing the import over it
/// would be worse than the echo.
struct TerminalEcho {
    #[cfg(unix)]
    restore: Option<libc::termios>,
}

impl TerminalEcho {
    #[cfg(unix)]
    fn suppressed() -> Self {
        // SAFETY: both calls take a pointer to a termios this frame owns, and
        // neither retains it. A non-tty stdin fails tcgetattr, which is handled.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return Self { restore: None };
            }
            let restore = term;
            term.c_lflag &= !libc::ECHO;
            // FLUSH so anything typed ahead of the prompt is dropped rather than
            // echoed, and on restore so a stray second line does not fall
            // through to the shell.
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &term) != 0 {
                return Self { restore: None };
            }
            Self {
                restore: Some(restore),
            }
        }
    }

    #[cfg(not(unix))]
    fn suppressed() -> Self {
        Self {}
    }
}

impl Drop for TerminalEcho {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(term) = self.restore.as_ref() {
            // SAFETY: `term` is the settings read in `suppressed`, unchanged.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, term);
            }
        }
    }
}

/// Fail unless the blob was minted for the mailbox this daemon is configured
/// for.
///
/// Same rule as the consent-time check in squelch-core, for the same reason:
/// storing a token under the wrong address makes the sync loop ingest a
/// stranger's mail and serve it as this account's own. Trimmed and
/// case-insensitive, because Gmail addresses are ASCII and both values are
/// hand-typed somewhere.
///
/// This is a CHEAP PRE-FILTER, not the guarantee. The blob's `account` is a
/// string whoever pasted it chose; only [`verify_transfer_credential`] can say
/// what a token actually opens. It runs first because it costs nothing and
/// catches the honest mistake (the wrong file, the wrong account) before any
/// network call.
///
/// The blob's value is attacker-chosen and this message reaches a terminal, so
/// it is disarmed on the way in: raw, an `ESC` in it clears the screen and a
/// `BEL` retitles the window.
fn check_transfer_account(configured: &str, blob: &str) -> Result<(), squelch_core::CoreError> {
    if configured.trim().eq_ignore_ascii_case(blob.trim()) {
        return Ok(());
    }
    let blob = printable(blob);
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

// ---- per-device human-door tokens -----------------------------------------
//
// `token` and `pair` talk to the STORE directly, exactly as `auth` talks to the
// credential backend directly: they are operator commands run at a shell, and
// requiring a running daemon to mint the credential that lets you reach the
// daemon would be a circle. These are the store's FIRST second-process writers,
// which is why `SqliteStore::set_pragmas` puts every connection in WAL with a
// busy timeout: a live `squelchd serve` on the same file is then not a conflict,
// and each command holds the write lock for one transaction.
//
// NOTHING HERE PRINTS SECRET MATERIAL EXCEPT THE ONE LINE THAT IS SUPPOSED TO.
// `token issue` prints its plaintext once, `pair` prints its code once, and
// `token list` prints neither, not even the stored hashes.

/// The URL scheme Passband registers for pairing deep links.
const PAIR_LINK_SCHEME: &str = "passband://pair";

/// Open the store and resolve the configured account, without a daemon.
fn open_account_store(
    config: &Config,
) -> Result<(SqliteStore, AccountId), squelch_core::CoreError> {
    let email = config.require_account_email()?;
    let store = SqliteStore::open(&config.db_path)?;
    let account_id = store.ensure_account(&email)?;
    Ok((store, account_id))
}

/// Whether the human door has a master token configured. The VALUE is never
/// read out; only its presence, which is the difference between an empty token
/// list meaning "nothing can get in" and meaning "only the master key can".
fn master_token_configured() -> bool {
    std::env::var("SQUELCH_API_TOKEN").is_ok_and(|t| !t.trim().is_empty())
}

/// An RFC3339 stamp in UTC, seconds resolution: precise enough to correlate with
/// a log line, short enough to sit in a column.
fn stamp(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn cmd_token(config: &Config, args: &TokenArgs) -> Result<(), squelch_core::CoreError> {
    let (store, account_id) = open_account_store(config)?;
    match &args.command {
        TokenCommand::Issue { name } => {
            let issued = store.issue_device_token(account_id, name)?;
            eprintln!(
                "squelchd: issued device token {} for {:?}.",
                issued.id, issued.name
            );
            // THE PLAINTEXT, ALONE ON STDOUT, so `squelchd token issue --name x
            // > token.txt` is a file containing a token and nothing else.
            // Everything a human reads goes to stderr, like `auth --export`.
            println!("{}", issued.token);
            eprintln!(
                "squelchd: that is the ONLY time this token is shown. squelchd keeps a hash of it \
                 and cannot print it again; a lost token is re-issued, never recovered. Revoke \
                 this one with `squelchd token revoke {}`.",
                issued.id
            );
            Ok(())
        }
        TokenCommand::List => {
            let tokens = store.list_device_tokens(account_id)?;
            if tokens.is_empty() {
                println!("No device tokens have been issued.");
                if master_token_configured() {
                    eprintln!(
                        "squelchd: the human door accepts SQUELCH_API_TOKEN only. \
                         Run `squelchd pair` to give a device its own revocable token."
                    );
                } else {
                    eprintln!(
                        "squelchd: SQUELCH_API_TOKEN is unset too, so the human door currently \
                         accepts nothing. Run `squelchd pair`, or \
                         `squelchd token issue --name <name>`."
                    );
                }
                return Ok(());
            }
            // Width from the data so a long device name does not wrap the
            // columns after it; the store caps names, so this is bounded.
            let width = tokens
                .iter()
                .map(|t| t.name.chars().count())
                .max()
                .unwrap_or(4)
                .max(4);
            println!(
                "{:>5}  {:<width$}  {:<20}  {:<20}  STATUS",
                "ID", "NAME", "CREATED", "LAST USED"
            );
            for t in &tokens {
                println!(
                    "{:>5}  {:<width$}  {:<20}  {:<20}  {}",
                    t.id,
                    t.name,
                    stamp(t.created_at),
                    t.last_used_at
                        .map(stamp)
                        .unwrap_or_else(|| "never".to_string()),
                    match t.revoked_at {
                        // Revoked rows are LISTED, not hidden: a revocation the
                        // operator cannot see afterwards is not much of a control.
                        Some(at) => format!("revoked {}", stamp(at)),
                        None => "active".to_string(),
                    }
                );
            }
            Ok(())
        }
        TokenCommand::Revoke { id } => {
            if store.revoke_device_token(account_id, *id)? {
                println!("Revoked device token {id}.");
                eprintln!(
                    "squelchd: it stops working on the very next request; the door reads the row \
                     every time and caches nothing."
                );
            } else {
                // Unknown and already-revoked are one answer, which is also what
                // the store returns: revoking twice is not a failure.
                println!("No active device token {id} for this account.");
            }
            Ok(())
        }
        TokenCommand::FirstPaired => {
            // EXACTLY ONE LINE ON STDOUT, and nothing else on it, ever — the
            // `token issue` discipline for the same reason: something machine
            // is reading this. Here that reader is `squelch-warden`, which
            // execs the command in a tenant pod and parses the first non-empty
            // line; a second line, a header, or a friendly note would be a
            // parse failure on a route the tenant cannot see or fix.
            //
            // Seconds precision and a `Z` offset, not the store's raw column:
            // one spelling crosses the wire, so the control plane never has to
            // decide whether two renderings of the same instant are the same
            // stamp. Nanoseconds would say when a device was minted to a
            // resolution nobody asked for.
            //
            // `none` rather than an empty line, because an empty line is what a
            // broken exec also produces.
            let first = store.first_client_pairing_at(account_id)?;
            println!(
                "{}",
                first
                    .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
                    .unwrap_or_else(|| "none".to_string())
            );
            Ok(())
        }
    }
}

/// Percent-encode a value for a query string. Everything outside the unreserved
/// set goes, so a base URL's `://`, its slashes, and any `&` in it land in the
/// deep link as DATA rather than as structure the parser on the other side would
/// read as the end of the parameter.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The base URL the pairing link names, trailing slashes trimmed.
///
/// Refused unless it is http(s): the link tells a device where to POST a live
/// pairing code, and a typo that produced a `file:` or scheme-less value would
/// be dropped by the app with nothing to explain it. Better to fail here, where
/// the operator can see what they typed.
///
/// Without `--url` the link is built from `bind`, which the caller resolves the
/// SAME way `serve` does. Naming the compiled-in default instead would hand an
/// operator on a non-default port a link to a port nothing listens on, and the
/// device would fail with nothing to explain it. A WILDCARD bind keeps its port
/// and prints as loopback: `0.0.0.0` is not an address anything can POST to, and
/// loopback is right whenever the device being paired is this machine.
fn pair_base_url(args: &PairArgs, bind: SocketAddr) -> Result<String, squelch_core::CoreError> {
    let raw = match args.url.as_deref().map(str::trim) {
        Some(u) if !u.is_empty() => u,
        _ => {
            let addr = if bind.ip().is_unspecified() {
                let host = match bind.ip() {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
                };
                SocketAddr::new(host, bind.port())
            } else {
                bind
            };
            // `SocketAddr`'s Display brackets an IPv6 literal, which is what a
            // URL authority needs.
            return Ok(format!("http://{addr}"));
        }
    };
    let base = raw.trim_end_matches('/');
    let lower = base.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(other_err(format!(
            "--url must be an http(s) address the device can reach, for example \
             http://{DEFAULT_BIND_ADDR}; got `{raw}`"
        )));
    }
    Ok(base.to_string())
}

/// Whether a base URL would carry the code and the token it buys over the
/// network in the clear: plain http to somewhere that is not this machine.
///
/// Loopback over http is fine — nothing leaves the box — so only the off-box
/// case is worth shouting about.
fn is_cleartext_offbox(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    // https is safe, and anything else was already refused by `pair_base_url`.
    let Some(rest) = lower.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // An IPv6 literal is bracketed, so the port has to come off after the
    // closing bracket rather than at the first colon.
    let host = match authority.split_once(']') {
        Some((bracketed, _)) => bracketed.trim_start_matches('['),
        None => authority.split(':').next().unwrap_or_default(),
    };
    let loopback = host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    !loopback
}

fn cmd_pair(config: &Config, args: &PairArgs) -> Result<(), squelch_core::CoreError> {
    // Before the store: a bad --url (or a bad SQUELCH_BIND behind the default)
    // must not spend a code the operator then has to wait out or supersede.
    let base = pair_base_url(args, resolve_bind(&ServeArgs { bind: None })?)?;
    let (store, account_id) = open_account_store(config)?;

    let minted = store.mint_pairing_code(account_id, Duration::seconds(PAIRING_TTL_SECS))?;

    println!("Pairing code: {}", minted.code);
    println!(
        "Valid for {} minutes, until {}.",
        PAIRING_TTL_SECS / 60,
        stamp(minted.expires_at)
    );
    println!();
    println!("On the device you are pairing, open this link:");
    println!();
    println!(
        "  {PAIR_LINK_SCHEME}?url={}&code={}",
        percent_encode(&base),
        percent_encode(&minted.code)
    );
    println!();
    println!("Or type the code into Passband, pointed at {base}");

    if is_cleartext_offbox(&base) {
        eprintln!(
            "\nsquelchd: WARNING: {base} is plain http to another machine. The pairing code and \
             the device token it mints would cross the network in the clear, readable by anything \
             on the path, which is enough for a stranger to pair themselves. Put https in front \
             (`tailscale serve` is the short version) and pass that address as --url."
        );
    }

    eprintln!(
        "\nsquelchd: that code is a credential. It is good for one device, it is spent the first \
         time it is claimed, and a handful of wrong guesses burns it. Minting another supersedes \
         this one."
    );
    if !master_token_configured() {
        eprintln!(
            "squelchd: SQUELCH_API_TOKEN is unset, so pairing is currently the only way into the \
             human door on this daemon."
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
    // No metrics registry: this mode serves no scrape endpoint, so the
    // loaded/unloaded gauge would have no reader.
    let embedder = build_embedder(&config, None);
    if let Some(e) = &embedder {
        store = store.with_embedder(e.clone() as Arc<dyn Embedder>)?;
    }
    let store = Arc::new(store);
    // Read off config before the engine takes ownership of it below.
    let idle_unload_secs = config.embed.idle_unload_secs;

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
            spawn_embedder_reaper(e.clone(), idle_unload_secs, shutdown_rx.clone());
            engine = engine.with_embedder(e as Arc<dyn Embedder>);
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
        .or_else(|| {
            std::env::var("SQUELCH_BIND")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
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

/// Resolve `[metrics] bind` (env `SQUELCH_METRICS_BIND`) into a socket address.
/// `None` means the config never asked for a metrics listener. A value that is
/// present but unparseable is FATAL, and eagerly so, for the same reason as
/// [`resolve_bind`]: an operator who asked for a scrape endpoint and silently
/// got none has monitoring that reports nothing and looks fine doing it.
fn resolve_metrics_bind(config: &Config) -> Result<Option<SocketAddr>, squelch_core::CoreError> {
    let Some(raw) = config
        .metrics
        .bind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    raw.parse()
        .map(Some)
        .map_err(|e| other_err(format!("invalid metrics bind address `{raw}`: {e}")))
}

/// Say, at startup, whether anything can scrape or probe this daemon.
///
/// Both routes on that listener are UNAUTHENTICATED — neither a scraper nor a
/// kubelet has a credential to offer — so its bind address is the whole access
/// control, which is why this prints rather than hides. `/metrics` exposes
/// counts and sizes only, with no sender, subject or message id ever becoming a
/// label, and `/healthz` exposes one word out of two.
fn report_metrics_posture(config: &Config) {
    match config
        .metrics
        .bind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(addr) => eprintln!(
            "squelchd: metrics and readiness enabled at http://{addr}/metrics and /healthz (no auth; bind it where only your scraper reaches it)"
        ),
        None => eprintln!(
            "squelchd: metrics disabled (set SQUELCH_METRICS_BIND / [metrics] bind, e.g. 127.0.0.1:9848, to serve GET /metrics and GET /healthz)"
        ),
    }
}

/// Whether the daemon has finished starting, as flags the startup path flips.
///
/// The doors bind BEFORE the daemon is up, deliberately (see [`cmd_serve`]), so
/// "the socket accepts" and "this mailbox is serving" are different questions,
/// and only the second one is worth an orchestrator's attention. Both flags
/// start false, which is what makes a startup that FAILS after the bind report
/// not-ready: a daemon that died on the way up has flipped nothing.
///
/// Two conditions, and each rules out a state a bare TCP accept calls healthy:
///
/// - **The sync engine's task is alive.** It is spawned before the doors bind
///   and it is what makes a mailbox a mailbox. If that task ends or panics,
///   both doors go on accepting connections in front of a store nothing is
///   filling, and every probe that only opens a socket passes forever.
/// - **The background embedder init has settled.** SETTLED, not succeeded: a
///   first-run model download is the long pole of a cold start and is precisely
///   what a probe must wait out, but an embedder that can NEVER build (no
///   egress to the model host, a corrupt cache) leaves a daemon that syncs,
///   serves both doors and searches by keyword. Refusing to call that ready
///   would take a working mailbox down over a degraded search index.
///
/// ## What this does NOT catch, and why it is left that way
///
/// A credential Google has stopped accepting does not make a daemon unready.
/// `SyncEngine::run` retries with backoff on every error and returns only when
/// shutdown is signalled, so a rejected token, an expired grant and a network
/// that is down all leave the task alive and this flag set. The first flag
/// therefore means "the sync task has not died", not "sync is working".
///
/// That is the deliberate answer rather than a gap to close later, because
/// readiness is what a Service routes on. A mailbox whose OAuth grant expired
/// still holds every message it has ever synced, still serves both doors, and
/// is reached through exactly one route — which is also where its owner has to
/// go to re-consent. Answering 503 would pull that pod out of its own Service
/// and take the door away at the moment it is needed, and it would stop the
/// hosted fleet roller dead — that pass reads a tenant carrying today's render
/// and not serving it as a casualty, and refuses to roll anything else at all.
/// A dead credential is a thing to ALERT on, and the metrics door next to this
/// one is where that signal belongs.
///
/// The store is deliberately not a flag. `SqliteStore::open` and its migrations
/// run before the runtime exists and a failure exits the process, so there is
/// no state in which anything is listening and the store is not open.
#[derive(Clone, Default)]
struct Readiness {
    flags: Arc<ReadinessFlags>,
}

struct ReadinessFlags {
    sync_running: AtomicBool,
    /// A watch sender rather than an `AtomicBool` because two things now read
    /// this bit: `/healthz`, and the sync engine, whose first backfill parks
    /// until the embedder init has resolved (see
    /// `SyncEngine::with_embedder_gate`). ONE bit for both, so settling the
    /// probe always releases the backfill, and there is no second flag to forget
    /// in a new arm of the init task. The reverse does not hold: the backfill
    /// also releases ITSELF at `EMBEDDER_GATE_CEILING` (three minutes) with the
    /// probe still 503, and that is the same 180 s as the warden's default ready
    /// timeout, so a wedged init fails the signup at about the instant the
    /// backfill starts without it. The ceiling's log line describes the
    /// daemon, not the tenant's fate.
    embedder_settled: tokio::sync::watch::Sender<bool>,
}

impl Default for ReadinessFlags {
    fn default() -> Self {
        Self {
            sync_running: AtomicBool::new(false),
            embedder_settled: tokio::sync::watch::channel(false).0,
        }
    }
}

/// Clears the sync flag when the sync task ends, HOWEVER it ends: a returned
/// error, a panic and an abort all drop this, and all three leave a daemon that
/// is no longer syncing.
struct SyncRunning(Arc<ReadinessFlags>);

impl Drop for SyncRunning {
    fn drop(&mut self) {
        self.0.sync_running.store(false, Ordering::Relaxed);
    }
}

impl Readiness {
    /// Mark the sync engine running, and hand back the guard that unmarks it.
    ///
    /// Called BEFORE the task is spawned, with the guard moved into it. Setting
    /// the flag from inside the task instead would race a sync loop that fails
    /// on its first tick: its guard would drop before the parent ever set the
    /// flag, and a dead engine would be reported as a running one for the life
    /// of the process.
    fn sync_started(&self) -> SyncRunning {
        self.flags.sync_running.store(true, Ordering::Relaxed);
        SyncRunning(self.flags.clone())
    }

    /// Mark the background embedder init resolved, whichever way it resolved.
    /// Every gate handed out by [`Readiness::embedder_gate`] opens in the same
    /// motion, because it is the same bit.
    fn embedder_settled(&self) {
        // `send_replace`, not `send`: the latter is an error when no receiver is
        // alive, and a daemon whose sync task has already died still has a probe
        // to answer.
        self.flags.embedder_settled.send_replace(true);
    }

    /// A receiver on the embedder-settled bit, for the sync engine's first
    /// backfill to wait on. Already true if the init resolved before the caller
    /// asked, so a late subscriber never waits for something that has happened.
    fn embedder_gate(&self) -> tokio::sync::watch::Receiver<bool> {
        self.flags.embedder_settled.subscribe()
    }

    /// Both conditions, and nothing derived from either: the caller gets one
    /// bit because one bit is all `/healthz` may say.
    fn is_up(&self) -> bool {
        self.flags.sync_running.load(Ordering::Relaxed) && *self.flags.embedder_settled.borrow()
    }
}

/// Everything the metrics door reads. Shared with the daemon proper; the only
/// thing it owns is its concurrency gate.
#[derive(Clone)]
struct MetricsState {
    metrics: Arc<squelch_core::metrics::SyncMetrics>,
    store: Arc<SqliteStore>,
    account_id: AccountId,
    config: Arc<Config>,
    /// ONE store read at a time, and the permit is held across the whole read.
    /// This route carries no credential and every store call queues on the
    /// daemon-wide mutex that both doors and the sync loop wait on, so a scrape
    /// flood is capped here rather than allowed to serialize the daemon — the
    /// same reasoning as `PAIR_CONCURRENCY`, one slot tighter because a scrape
    /// is a periodic housekeeping read, not a user waiting on a screen.
    gate: Arc<tokio::sync::Semaphore>,
}

/// The db-derived half of a scrape, or `None` if the store could not be read.
async fn gather_store_metrics(
    state: &MetricsState,
) -> Option<squelch_core::metrics::StoreSnapshot> {
    let _permit = state.gate.acquire().await.ok()?;
    let store = state.store.clone();
    let config = state.config.clone();
    let account_id = state.account_id;

    let gathered = tokio::task::spawn_blocking(move || {
        // Band counts are not exported (they are a windowed view of the same
        // rows the tier gauges already carry), so this window only satisfies
        // `stats`'s signature.
        let bands_since = chrono::Utc::now() - Duration::days(30);
        let stats = store.stats(account_id, bands_since)?;
        let llm = squelch_core::metrics::usage_from_ledger(
            store.list_usage_by_category(account_id, squelch_core::metrics::LEDGER_ALL_DAYS)?,
        );
        let (db_bytes, wal_bytes) = squelch_core::metrics::db_file_sizes(&config.db_path);
        // A COUNT, never a listing: the exported gauge is unlabelled, so no
        // device name can reach an unauthenticated scrape even in principle.
        let devices_paired = store.count_client_devices(account_id)?;
        Ok::<_, squelch_core::CoreError>(squelch_core::metrics::StoreSnapshot {
            llm_cost_usd: squelch_core::metrics::estimate_cost_usd(&llm, &config),
            stats,
            llm,
            db_bytes,
            wal_bytes,
            devices_paired,
        })
    })
    .await;

    match gathered {
        Ok(Ok(snapshot)) => Some(snapshot),
        // A HALF-WORKING SCRAPE BEATS A 500. The in-process counters are still
        // true and are exactly what an operator needs when the store is the
        // thing that is unhappy; a failed scrape would instead take the whole
        // series down and read as "the daemon is gone".
        Ok(Err(e)) => {
            eprintln!(
                "squelchd: metrics scrape could not read the store ({e}); serving counters only"
            );
            None
        }
        Err(e) => {
            eprintln!("squelchd: metrics scrape task join error ({e}); serving counters only");
            None
        }
    }
}

async fn serve_metrics(
    axum::extract::State(state): axum::extract::State<MetricsState>,
) -> impl axum::response::IntoResponse {
    let db = gather_store_metrics(&state).await;
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        squelch_core::metrics::render(&state.metrics, db.as_ref()),
    )
}

/// `GET /healthz`: `200` once the daemon is up, `503` until then, and nothing
/// else in either direction.
///
/// Two states and one word each, because anything that can open the metrics
/// port can read this: the listener carries no authentication of any kind, so
/// every byte answered here is a byte an unauthenticated caller gets. A phase
/// name, a count or a reason would each describe the tenant behind the port,
/// and none of them tells a probe anything it can act on — a probe has exactly
/// two moves.
async fn serve_healthz(
    axum::extract::State(readiness): axum::extract::State<Readiness>,
) -> impl axum::response::IntoResponse {
    if readiness.is_up() {
        (axum::http::StatusCode::OK, "ok")
    } else {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "starting")
    }
}

/// The metrics door: `GET /metrics`, `GET /healthz`, and nothing else, on a
/// listener of its own. Kept off the main router deliberately — the doors sit
/// behind whatever proxy fronts them, and an unauthenticated scrape route must
/// not inherit that exposure.
///
/// Readiness belongs on THIS listener for the same reason and one more: the
/// doors serve nothing unauthenticated, so a probe against them would have to
/// hold a bearer token, and a token an orchestrator holds is a credential in a
/// pod spec. This listener is already separate, already unauthenticated, and
/// already the one a scraper is pointed at.
///
/// The two routes carry SEPARATE state, which is load bearing rather than tidy.
/// A scrape reads the store behind [`MetricsState`]'s single permit; a
/// readiness route that queued on the same permit would answer late whenever
/// the store is busy, and a late answer is a healthy tenant pulled out of
/// service. `/healthz` holds no store handle at all, so it cannot acquire that
/// dependency by a later edit.
fn build_metrics_router(state: MetricsState, readiness: Readiness) -> axum::Router {
    axum::Router::new()
        .route("/metrics", axum::routing::get(serve_metrics))
        .with_state(state)
        .merge(
            axum::Router::new()
                .route("/healthz", axum::routing::get(serve_healthz))
                .with_state(readiness),
        )
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
    // The agent door gets the HUMAN DOOR'S OWN listing policy, read back off the
    // state rather than resolved from config a second time: the two doors must
    // agree about which packages exist, and the way that broke before was each
    // side deciding for itself.
    let mcp_service = squelch_mcp::streamable_http_service(
        store,
        account_email,
        api_state.shipment_policy(),
        mcp_cancel,
    )?;
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
    let metrics_bind = resolve_metrics_bind(&config)?;
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
    report_metrics_posture(&config);

    // ONE registry for the process: the sync engine writes it, the metrics door
    // reads it. Built here so both sides get the same Arc.
    let sync_metrics = squelch_core::metrics::SyncMetrics::new();

    // Same shape, different question: the startup path below flips these and
    // the metrics door's `/healthz` reads them. See [`Readiness`] for which
    // conditions count as up and why.
    let readiness = Readiness::default();

    // The carrier poller keeps the shipments tracker moving between emails.
    // CARRIER CREDENTIALS ARE ITS ONLY FEATURE FLAG: with none configured
    // `from_config` returns None and no carrier API is ever contacted. Built
    // after the metrics registry so its polls land in the same scrape as
    // everything else.
    let shipment_poller = match squelch_core::carriers::poller::ShipmentPoller::from_config(
        store.clone() as Arc<dyn squelch_core::store::Store>,
        account_id,
        &config,
    ) {
        Ok(poller) => poller.map(|p| p.with_metrics(sync_metrics.clone())),
        Err(e) => {
            eprintln!(
                "squelchd: shipment poller NOT started: carriers are configured but the poller could not be built: {e}"
            );
            None
        }
    };
    // The kick handle + the carriers behind it, for `POST /client/shipments/poll`.
    // Taken here because `run` consumes the poller below; `None` when no poller
    // was built, which the endpoint reports as `kicked: false`.
    let shipment_kick = shipment_poller
        .as_ref()
        .map(|p| (p.kick_handle(), p.enabled_carriers()));

    // The human door SERVES WITH OR WITHOUT SQUELCH_API_TOKEN. Without it the
    // door accepts only the per-device tokens in the store, and a daemon with
    // neither still comes up 401ing everything: pairing is how the first
    // credential arrives, and `POST /client/pair` needs a door to arrive at.
    // The shared config->state wiring also attaches the WRITE-bound credential
    // store that enables the action endpoints; the sync engine below gets a
    // separate READ-bound store and never sees this one.
    let api_state = squelch_api::ApiState::from_config(store.clone(), &email, &config, cap_sources)
        .map_err(|e| other_err(format!("{e}")))?
        .with_refresh(refresh.clone())
        // THE SAME registry the sync engine writes, so `/client/stats` answers
        // "is this mailbox still connected" from live state rather than from a
        // copy taken at boot.
        .with_sync_metrics(sync_metrics.clone())
        .with_event_notifier(event_tx);
    // Carrier polling is BYOK, so the no-poller path is the common one and is
    // spelled out rather than left implicit: the door still serves
    // `/client/shipments/poll`, it just answers `kicked: false`.
    let api_state = match shipment_kick {
        Some((kick, carriers)) => api_state.with_shipment_poll_kick(kick, carriers),
        None => api_state,
    };

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
                Some(tokio::spawn(async move {
                    pusher.run(pusher_wake, shutdown_rx).await
                }))
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

        // Polls the carrier APIs for shipments detected in mail. Same shape as
        // the two above: shares the shutdown watch, awaited on the way out.
        let shipment_handle = match shipment_poller {
            Some(poller) => {
                eprintln!(
                    "squelchd: shipment poller enabled (carriers: {})",
                    poller.enabled_carriers().join(", ")
                );
                let shutdown_rx = shutdown_rx.clone();
                Some(tokio::spawn(async move { poller.run(shutdown_rx).await }))
            }
            None => {
                // The normal case — carrier polling is BYOK — so a detail, not a
                // warning.
                eprintln!(
                    "squelchd: shipment poller disabled (no carrier credentials in [carriers])"
                );
                None
            }
        };

        // READ-bound store, shared by the sync loop and the contacts harvest.
        let sync_creds =
            make_credential_store(backend, account_id, email.clone(), creds_path, client);

        // No embedder override: the loop resolves it from the shared store each
        // tick, so it picks up the background-attached one. The gate is what
        // keeps a brand-new tenant's FIRST backfill from racing that attach:
        // without it the whole 30-day window ingests with no vector while the
        // model is still downloading, and the vector pass then has to embed all
        // of it in batches, which is the memory that OOM-killed tenant daemons.
        // Only the first backfill waits; polling and catch-up never do.
        let sync_handle = {
            let store = store.clone();
            let email = email.clone();
            let config = config.clone();
            let refresh = refresh.clone();
            let creds = sync_creds.clone();
            let sync_metrics = sync_metrics.clone();
            // Taken before the spawn and dropped by the task, so readiness
            // tracks the engine and not the intention to start one.
            let sync_running = readiness.sync_started();
            let embedder_gate = readiness.embedder_gate();
            tokio::spawn(async move {
                let _sync_running = sync_running;
                SyncEngine::new(store, creds, account_id, email, config)
                    .with_refresh(refresh)
                    .with_metrics(sync_metrics)
                    .with_embedder_gate(embedder_gate)
                    .run(shutdown_rx)
                    .await
            })
        };

        // One-time Sent-history sweeps, both headers-only on the read credential:
        // the contacts harvest seeding recipient autocomplete, then the
        // recipients backfill filling `to_addrs` on sent mail that predates the
        // column. Staggered past the startup sync burst and run one after the
        // other so they never contend for the same Gmail quota. Either failing
        // just retries on the next daemon start — each done flag is only set on
        // completion — and neither can block the sync loop, which runs in its own
        // task.
        //
        // The stagger is measured from when that burst can actually START, which
        // is why this waits on the SAME embedder gate the first backfill does: on
        // a cold model cache the backfill now begins minutes into the run, and a
        // sweep that is one metadata GET per Sent message (the heaviest Gmail
        // consumer here) landing on top of a 30-day raw backfill on one
        // credential is how a tenant earns a 429 — which bubbles out of
        // `run_once` and bounces the whole sync lifecycle through backoff.
        {
            let store = store.clone();
            let email = email.clone();
            let config = config.clone();
            let creds = sync_creds.clone();
            let mut embedder_gate = readiness.embedder_gate();
            tokio::spawn(async move {
                wait_for_embedder_gate(&mut embedder_gate).await;
                tokio::time::sleep(std::time::Duration::from_secs(180)).await;
                let engine = SyncEngine::new(store, creds, account_id, email, config);
                if let Err(e) = engine.harvest_sent_contacts().await {
                    eprintln!(
                        "squelchd: sent-contacts harvest incomplete (retries next start): {e}"
                    );
                }
                if let Err(e) = engine.backfill_sent_recipients().await {
                    eprintln!(
                        "squelchd: sent-recipients backfill incomplete (retries next start): {e}"
                    );
                }
            });
        }

        // One-shot shipment re-detect: the tracking-number detector was tightened
        // (eBay item ids and marketing digit-runs used to mint phantom rows), so
        // every existing row is re-judged against its own feeder message ONCE and
        // the ones the detector no longer produces are deleted. Store-only and
        // fast, so it runs inline on a blocking thread rather than on a delay —
        // no network, no Gmail quota.
        //
        // The ONCE is the store's job, not this call site's: it checks and writes
        // its own done-flag in the SAME TRANSACTION as the deletions, so the pass
        // cannot complete without being recorded. A re-run that skipped an
        // unrecorded pass would reap the rows the extractor has since written,
        // which no regex can reproduce. A failure here simply retries next start.
        {
            let store = store.clone();
            tokio::task::spawn_blocking(move || {
                match store.shipments_redetect_cleanup(account_id) {
                    // Count only — a tracking number is never logged.
                    Ok(n) => {
                        eprintln!("squelchd: shipment re-detect removed {n} stale shipment row(s)")
                    }
                    Err(_) => eprintln!("squelchd: shipment re-detect failed (retries next start)"),
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

        // The metrics door, on its OWN listener: a second bind, a one-route
        // router, and the same shutdown watch the doors and sync share. Absent
        // `[metrics] bind`, none of this exists.
        let metrics_handle = match metrics_bind {
            Some(addr) => {
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| other_err(format!("bind metrics {addr}: {e}")))?;
                let bound = listener.local_addr().unwrap_or(addr);
                eprintln!(
                    "squelchd: serving metrics http://{bound}/metrics and readiness http://{bound}/healthz"
                );
                let app = build_metrics_router(
                    MetricsState {
                        metrics: sync_metrics.clone(),
                        store: store.clone(),
                        account_id,
                        config: Arc::new(config.clone()),
                        gate: Arc::new(tokio::sync::Semaphore::new(1)),
                    },
                    readiness.clone(),
                );
                // Its own receiver off the sender, so it does not depend on
                // where in this block the sync engine took the original.
                let mut shutdown_rx = shutdown_tx.subscribe();
                Some(tokio::spawn(async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async move {
                            loop {
                                if *shutdown_rx.borrow() {
                                    return;
                                }
                                if shutdown_rx.changed().await.is_err() {
                                    return;
                                }
                            }
                        })
                        .await
                }))
            }
            None => None,
        };

        // Build off the async workers and attach to the shared store when ready;
        // search is keyword-only until then.
        {
            let store = store.clone();
            let config = config.clone();
            let readiness = readiness.clone();
            let sync_metrics = sync_metrics.clone();
            // Read off config here because the blocking closure below takes it.
            let idle_unload_secs = config.embed.idle_unload_secs;
            // Its own receiver, taken here rather than inside the task, for the
            // same reason the metrics server takes one: it must not depend on
            // where in this block the sync engine took the original.
            let reaper_shutdown = shutdown_tx.subscribe();
            eprintln!(
                "squelchd: initializing semantic-recall embedder in the background \
                 (first run downloads the model; the server is already serving, \
                 search is keyword-only until the embedder is ready)"
            );
            tokio::spawn(async move {
                let built =
                    tokio::task::spawn_blocking(move || build_embedder(&config, Some(sync_metrics)))
                        .await;
                match built {
                    Ok(Some(embedder)) => {
                        match store.attach_embedder(embedder.clone() as Arc<dyn Embedder>) {
                            Ok(_) => eprintln!(
                                "squelchd: embedder ready — semantic + hybrid search now enabled"
                            ),
                            Err(e) => eprintln!(
                                "squelchd: embedder attach failed ({e}); search stays keyword-only"
                            ),
                        }
                        // Spawned even when the attach failed, and especially
                        // then: an embedder nobody can reach still loaded a
                        // session at construction, and this is what puts that
                        // 250 MB down instead of holding it until restart.
                        spawn_embedder_reaper(embedder, idle_unload_secs, reaper_shutdown);
                    }
                    Ok(None) => { /* build_embedder already logged the reason */ }
                    Err(e) => eprintln!(
                        "squelchd: embedder init task join error ({e}); search stays keyword-only"
                    ),
                }
                // Every arm above, including the ones that gave up: this is the
                // last step of startup, and readiness waits on it having
                // HAPPENED rather than on it having worked. See [`Readiness`].
                // The sync engine's first backfill waits on this same call for
                // the same reason: an init that failed is one to stop waiting
                // for, not one to wait out.
                readiness.embedder_settled();
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
        if let Some(handle) = shipment_handle {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("squelchd: shipment poller ended with error: {e}"),
                Err(e) => eprintln!("squelchd: shipment poller task join error: {e}"),
            }
        }
        if let Some(handle) = metrics_handle {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("squelchd: metrics server ended with error: {e}"),
                Err(e) => eprintln!("squelchd: metrics server task join error: {e}"),
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

    const BROKER: &str = "https://auth.passband.email";

    /// Every flag off, for tests that vary one at a time.
    fn bare_auth_args() -> AuthArgs {
        AuthArgs {
            write: false,
            headless: false,
            port: DEFAULT_HEADLESS_PORT,
            broker: None,
            export: false,
            out: None,
            import: false,
            expose_consent_listener: false,
        }
    }

    /// Nothing typed on the command line: the state an `auth` run is in when
    /// only the environment has an opinion.
    fn ambient_sources() -> AuthFlagSources {
        AuthFlagSources {
            broker: FlagSource::Ambient,
            port: FlagSource::Ambient,
        }
    }

    /// Parse an argv the way `main` does, so `value_source` is available and
    /// tests see exactly what the binary sees.
    fn parse_auth(argv: &[&str]) -> (AuthArgs, AuthFlagSources) {
        let matches = Cli::command().try_get_matches_from(argv).expect("parses");
        let sources = AuthFlagSources::from_matches(&matches);
        match Cli::from_arg_matches(&matches).unwrap().command {
            Command::Auth(args) => (args, sources),
            _ => panic!("expected auth subcommand"),
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

        let cli = Cli::parse_from([
            "squelchd",
            "auth",
            "--write",
            "--headless",
            "--port",
            "9100",
        ]);
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
    /// asking for both is a usage error rather than a silent pick. The refusal
    /// is `resolve_transport`'s rather than clap's, because clap cannot tell a
    /// typed `--broker` from an inherited `SQUELCH_BROKER_URL`.
    #[test]
    fn a_typed_broker_excludes_every_other_transport() {
        let (args, sources) = parse_auth(["squelchd", "auth", "--broker", BROKER].as_ref());
        assert_eq!(args.broker.as_deref(), Some(BROKER));
        assert_eq!(sources.broker, FlagSource::CommandLine);
        assert_eq!(
            resolve_transport(&args, sources).unwrap(),
            Transport::Broker(BROKER.to_string())
        );

        for argv in [
            vec!["squelchd", "auth", "--broker", BROKER, "--headless"],
            vec!["squelchd", "auth", "--broker", BROKER, "--port", "9100"],
            vec!["squelchd", "auth", "--broker", BROKER, "--export"],
            vec!["squelchd", "auth", "--broker", BROKER, "--import"],
        ] {
            let (args, sources) = parse_auth(&argv);
            let err = resolve_transport(&args, sources)
                .expect_err(&format!("{argv:?} was accepted"))
                .to_string();
            assert!(err.contains("--broker"), "{argv:?}: {err}");
            assert!(err.contains("pick one"), "{argv:?}: {err}");
        }
    }

    /// An exported `SQUELCH_BROKER_URL` is a default, not an instruction.
    ///
    /// clap counts an env-sourced value as PRESENT for conflicts, so with the
    /// broker's conflicts declared there, an ambient URL made `auth --import`
    /// exit 2 naming a flag the operator never typed. Export and import are the
    /// sanctioned replacement for the broker (docs/BROKER.md: DO NOT DEPLOY), so
    /// an environment variable must not be able to disable them.
    #[test]
    fn an_ambient_broker_url_never_disables_another_transport() {
        let ambient = |extra: &[&str]| {
            let mut args = bare_auth_args();
            args.broker = Some(BROKER.to_string());
            for flag in extra {
                match *flag {
                    "--export" => args.export = true,
                    "--import" => args.import = true,
                    "--headless" => args.headless = true,
                    other => panic!("unhandled {other}"),
                }
            }
            args
        };

        // Alone it IS the transport; that is the point of the variable.
        assert_eq!(
            resolve_transport(&ambient(&[]), ambient_sources()).unwrap(),
            Transport::Broker(BROKER.to_string())
        );
        // Anything named on the command line wins over it, silently.
        assert_eq!(
            resolve_transport(&ambient(&["--import"]), ambient_sources()).unwrap(),
            Transport::Import
        );
        assert_eq!(
            resolve_transport(&ambient(&["--export"]), ambient_sources()).unwrap(),
            Transport::Export
        );
        assert_eq!(
            resolve_transport(&ambient(&["--headless"]), ambient_sources()).unwrap(),
            Transport::Loopback
        );
        let typed_port = AuthFlagSources {
            broker: FlagSource::Ambient,
            port: FlagSource::CommandLine,
        };
        assert_eq!(
            resolve_transport(&ambient(&[]), typed_port).unwrap(),
            Transport::Loopback
        );

        // And with no broker anywhere, the listener is the transport.
        assert_eq!(
            resolve_transport(&bare_auth_args(), ambient_sources()).unwrap(),
            Transport::Loopback
        );
    }

    /// `SQUELCH_BROKER_URL=` is how a leftover compose entry says nothing, and
    /// clap hands it over as a value all the same.
    ///
    /// Taken as a URL it resolves to `Broker("")` and dies on a parse nobody
    /// asked for: plain `squelchd auth` would stop working on any host that ever
    /// had the variable in a file. Blank from the environment is no broker at
    /// all, which is also what the "SQUELCH_BROKER_URL is set" note must decide.
    #[test]
    fn an_empty_ambient_broker_url_is_no_broker_at_all() {
        for blank in ["", "   ", "\t\n"] {
            let ambient = |extra: fn(&mut AuthArgs)| {
                let mut args = bare_auth_args();
                args.broker = Some(blank.to_string());
                extra(&mut args);
                args
            };

            let args = ambient(|_| {});
            assert!(
                broker_url(&args, ambient_sources()).is_none(),
                "{blank:?} must not read as a broker, note included"
            );
            assert_eq!(
                resolve_transport(&args, ambient_sources()).unwrap(),
                Transport::Loopback,
                "{blank:?}"
            );

            // And it refuses nothing, because there is nothing to conflict with.
            for (args, want) in [
                (ambient(|a| a.import = true), Transport::Import),
                (ambient(|a| a.export = true), Transport::Export),
                (ambient(|a| a.headless = true), Transport::Loopback),
            ] {
                assert_eq!(
                    resolve_transport(&args, ambient_sources()).unwrap(),
                    want,
                    "{blank:?}"
                );
            }
        }

        // A `--broker ""` that was TYPED is a different statement: it stays a
        // broker run here and fails later, where the operator typed it.
        let (args, sources) = parse_auth(&["squelchd", "auth", "--broker", ""]);
        assert_eq!(sources.broker, FlagSource::CommandLine);
        assert_eq!(
            resolve_transport(&args, sources).unwrap(),
            Transport::Broker(String::new())
        );
    }

    /// A `--port` the run will never bind is a tunnel or a `-p` mapping that
    /// will never be connected to, and silence about it looks exactly like a
    /// port that was honored.
    #[test]
    fn a_typed_port_nothing_will_bind_is_called_out() {
        let ignored = |argv: &[&str]| {
            let (args, sources) = parse_auth(argv);
            let transport = resolve_transport(&args, sources).expect("resolves");
            typed_port_is_ignored(&args, &transport, sources)
        };

        // Bound: the two runs that fix a port on purpose.
        for argv in [
            vec!["squelchd", "auth", "--headless", "--port", "9100"],
            vec![
                "squelchd",
                "auth",
                "--export",
                "--expose-consent-listener",
                "--port",
                "9100",
            ],
        ] {
            assert!(!ignored(&argv), "{argv:?}");
        }

        // Ignored: an ephemeral port is bound instead, whatever was typed.
        for argv in [
            vec!["squelchd", "auth", "--port", "9100"],
            vec!["squelchd", "auth", "--export", "--port", "9100"],
            vec!["squelchd", "auth", "--import", "--port", "9100"],
        ] {
            assert!(ignored(&argv), "{argv:?}");
        }

        // A port nobody typed is the default, and the default is never a
        // promise the operator made.
        for argv in [
            vec!["squelchd", "auth"],
            vec!["squelchd", "auth", "--headless"],
            vec!["squelchd", "auth", "--export"],
        ] {
            assert!(!ignored(&argv), "{argv:?}");
        }

        // An ambient broker plus a typed port resolves to Loopback, which binds
        // an ephemeral port unless the run is headless — so the warning belongs
        // there too.
        let mut args = bare_auth_args();
        args.broker = Some(BROKER.to_string());
        let typed_port = AuthFlagSources {
            broker: FlagSource::Ambient,
            port: FlagSource::CommandLine,
        };
        let transport = resolve_transport(&args, typed_port).unwrap();
        assert_eq!(transport, Transport::Loopback);
        assert!(typed_port_is_ignored(&args, &transport, typed_port));
        args.headless = true;
        assert!(!typed_port_is_ignored(&args, &transport, typed_port));

        // A typed port beside a TYPED broker never gets this far.
        let argv = ["squelchd", "auth", "--broker", BROKER, "--port", "9100"];
        let (args, sources) = parse_auth(&argv);
        assert!(resolve_transport(&args, sources).is_err());
    }

    /// The parse itself must no longer refuse these pairs, or the runtime check
    /// above never gets a chance to make the distinction.
    #[test]
    fn the_broker_conflicts_are_no_longer_clap_s_to_enforce() {
        for argv in [
            vec!["squelchd", "auth", "--import", "--broker", BROKER],
            vec!["squelchd", "auth", "--export", "--broker", BROKER],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "{argv:?} must reach resolve_transport"
            );
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

        // Every one of these is a conflict between flags that can ONLY come
        // from the command line, so clap can still settle them itself.
        for argv in [
            vec!["squelchd", "auth", "--export", "--import"],
            vec!["squelchd", "auth", "--export", "--headless"],
            vec!["squelchd", "auth", "--import", "--headless"],
            // The blob decides what it carries, so --write has nothing to say.
            vec!["squelchd", "auth", "--import", "--write"],
            // Exposure is a property of the export listener and nothing else.
            vec!["squelchd", "auth", "--expose-consent-listener"],
            vec!["squelchd", "auth", "--import", "--expose-consent-listener"],
            // So is --out.
            vec!["squelchd", "auth", "--out", "cred.txt"],
            vec!["squelchd", "auth", "--import", "--out", "cred.txt"],
        ] {
            assert!(Cli::try_parse_from(&argv).is_err(), "{argv:?} was accepted");
        }
    }

    /// `--export --out PATH` exists so the blob is never written through a shell
    /// redirect, which takes the ambient umask (0644 on most hosts) for a file
    /// that is a live refresh token.
    #[cfg(unix)]
    #[test]
    fn the_exported_blob_lands_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let (args, _) = parse_auth(&["squelchd", "auth", "--export", "--out", "cred.txt"]);
        assert_eq!(args.out.as_deref(), Some(std::path::Path::new("cred.txt")));

        let path = std::env::temp_dir().join(format!("squelch-export-{}.txt", std::process::id()));
        let blob = encode_transfer(&CredentialTransfer::new(
            "me@gmail.com".to_string(),
            vec![TransferCredential {
                kind: CredentialKind::Read,
                token: StoredToken {
                    access_token: "access".to_string(),
                    refresh_token: Some("refresh".to_string()),
                    expires_at: None,
                },
            }],
        ))
        .unwrap();

        // Pre-create it world-readable: --out has to fix the mode, not inherit
        // whatever was there.
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut bytes = blob.clone().into_bytes();
        bytes.push(b'\n');
        write_private(&path, &bytes).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        // And what landed still imports exactly as pasted.
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(decode_transfer(&written).unwrap().account, "me@gmail.com");
        std::fs::remove_file(&path).ok();
    }

    /// A TTY paste ends at the enter that follows it. Reading to EOF there would
    /// need a Ctrl-D nobody expects, which is the prompt hanging.
    #[test]
    fn a_pasted_blob_ends_at_the_newline_and_a_piped_one_at_eof() {
        let blob = "squelch-cred-v1.payload";
        // What a terminal delivers: the line, the enter, and nothing more that
        // this read may wait for.
        assert_eq!(
            read_blob_capped(format!("{blob}\n").as_bytes(), BlobRead::OneLine).unwrap(),
            blob
        );
        // A second line is not part of the paste.
        assert_eq!(
            read_blob_capped(format!("{blob}\nextra\n").as_bytes(), BlobRead::OneLine).unwrap(),
            blob
        );
        // A pipe has no prompt and no trailing newline to rely on.
        assert_eq!(
            read_blob_capped(blob.as_bytes(), BlobRead::ToEndOfInput).unwrap(),
            blob
        );

        // Nothing at all is the mistake worth naming, either way.
        for how in [BlobRead::OneLine, BlobRead::ToEndOfInput] {
            let err = read_blob_capped("   \n".as_bytes(), how)
                .unwrap_err()
                .to_string();
            assert!(err.contains("nothing arrived on stdin"), "{how:?}: {err}");
        }

        // The cap holds for a wedged pipe and for a terminal that somehow
        // delivers one enormous line.
        let flood = "x".repeat(MAX_BLOB_BYTES + 10);
        for how in [BlobRead::OneLine, BlobRead::ToEndOfInput] {
            let err = read_blob_capped(flood.as_bytes(), how)
                .unwrap_err()
                .to_string();
            assert!(err.contains("no credential blob is"), "{how:?}: {err}");
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

    /// The account inside a blob is a string whoever pasted it chose, and this
    /// refusal prints it. Raw, an ESC in it clears the screen and a BEL retitles
    /// the window, so it reaches the terminal as text or not at all.
    #[test]
    fn a_blob_cannot_paint_the_terminal_through_the_refusal() {
        let hostile = "\u{1b}[2J\u{1b}]0;pwned\u{7}victim@gmail.com\u{202e}";
        let err = check_transfer_account("me@gmail.com", hostile)
            .unwrap_err()
            .to_string();
        for sneaky in ['\u{1b}', '\u{7}', '\u{202e}', '\n', '\r'] {
            assert!(!err.contains(sneaky), "{sneaky:?} survived into: {err}");
        }
        // Disarmed, not dropped: what is left is still legible.
        assert!(err.contains("victim@gmail.com"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");

        // And length is theirs to choose unless it is bounded here too.
        let err = check_transfer_account("me@gmail.com", &"a".repeat(100_000))
            .unwrap_err()
            .to_string();
        assert!(
            err.len() < 1_000,
            "unbounded blob text: {} chars",
            err.len()
        );
    }

    /// Two consents in one export must have named the same mailbox, and the
    /// refusal prints both. They come from Google's userinfo by way of whichever
    /// session finished the consent, so they reach the terminal as text or not
    /// at all.
    #[test]
    fn a_mismatched_export_names_both_mailboxes_without_painting_the_terminal() {
        let hostile = "\u{1b}[2J\u{1b}]0;pwned\u{7}stranger@gmail.com\u{202e}";
        for (first, mailbox) in [(hostile, "me@gmail.com"), ("me@gmail.com", hostile)] {
            let err = check_export_same_account(first, mailbox)
                .unwrap_err()
                .to_string();
            for sneaky in ['\u{1b}', '\u{7}', '\u{202e}', '\n', '\r'] {
                assert!(!err.contains(sneaky), "{sneaky:?} survived into: {err}");
            }
            // Disarmed, not dropped: both mailboxes are still legible.
            assert!(err.contains("stranger@gmail.com"), "{err}");
            assert!(err.contains("me@gmail.com"), "{err}");
            assert!(err.contains("nothing was exported"), "{err}");
        }

        // And length is theirs to choose unless it is bounded here too.
        let err = check_export_same_account(&"a".repeat(100_000), "me@gmail.com")
            .unwrap_err()
            .to_string();
        assert!(
            err.len() < 1_000,
            "unbounded mailbox text: {} chars",
            err.len()
        );

        // Google echoes its own casing and the consent screen adds no
        // whitespace worth caring about; anything else IS a second account.
        assert!(check_export_same_account("me@gmail.com", "me@gmail.com").is_ok());
        assert!(check_export_same_account("Me@Gmail.COM", " me@gmail.com\n").is_ok());
        assert!(check_export_same_account("me@gmail.com", "me+x@gmail.com").is_err());
        assert!(check_export_same_account("me@gmail.com", "me@example.com").is_err());
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

    /// The pairing link's `url` parameter carries a whole URL inside a query
    /// value, so every character that means something to a query parser has to
    /// come out the other side as data.
    #[test]
    fn a_base_url_survives_the_deep_link_intact() {
        assert_eq!(
            percent_encode("http://127.0.0.1:8848"),
            "http%3A%2F%2F127.0.0.1%3A8848"
        );
        // An `&` or a `#` in the base would otherwise end the parameter early
        // and hand the rest to whatever came next.
        assert_eq!(percent_encode("a&b=c#d"), "a%26b%3Dc%23d");
        // The code's own alphabet is unreserved, so it rides through unchanged
        // and stays readable in the printed link.
        assert_eq!(percent_encode("ABCD-EFGH"), "ABCD-EFGH");
    }

    /// `--url` decides where a device POSTs a live pairing code, so a value that
    /// could not be one fails HERE, where the operator can see what they typed,
    /// rather than silently in an app that drops the link.
    #[test]
    fn pair_url_defaults_to_loopback_and_refuses_non_http() {
        let bind: SocketAddr = DEFAULT_BIND_ADDR.parse().unwrap();
        let with = |url: Option<&str>| {
            pair_base_url(
                &PairArgs {
                    url: url.map(str::to_string),
                },
                bind,
            )
        };

        let default = format!("http://{DEFAULT_BIND_ADDR}");
        assert_eq!(with(None).unwrap(), default);
        // A blank or whitespace `--url` is an environment saying nothing, the
        // same reading `auth` gives an empty `SQUELCH_BROKER_URL`.
        assert_eq!(with(Some("")).unwrap(), default);
        assert_eq!(with(Some("   ")).unwrap(), default);

        // Trailing slashes go, so the claim path is never `//client/pair`.
        assert_eq!(
            with(Some("https://box.tail.ts.net//")).unwrap(),
            "https://box.tail.ts.net"
        );
        assert_eq!(
            with(Some("HTTPS://Box.example")).unwrap(),
            "HTTPS://Box.example"
        );

        for bad in ["box.tail.ts.net", "file:///etc/passwd", "passband://pair"] {
            assert!(with(Some(bad)).is_err(), "{bad} must be refused");
        }
    }

    /// The default link follows the bind `serve` actually resolves, port and
    /// all: an operator who moved the daemon off 8848 would otherwise print a
    /// link to a port nothing is listening on.
    #[test]
    fn the_default_pair_url_follows_the_resolved_bind() {
        let default_for =
            |bind: &str| pair_base_url(&PairArgs { url: None }, bind.parse().unwrap()).unwrap();

        assert_eq!(default_for("127.0.0.1:9000"), "http://127.0.0.1:9000");
        // A wildcard bind keeps the PORT and prints as loopback: `0.0.0.0` is
        // not somewhere a device can POST.
        assert_eq!(default_for("0.0.0.0:9000"), "http://127.0.0.1:9000");
        assert_eq!(default_for("[::]:9000"), "http://[::1]:9000");
        // A specific non-loopback bind is named as-is; the operator chose it.
        assert_eq!(default_for("192.168.1.9:8848"), "http://192.168.1.9:8848");
        // `--url` still wins over every one of those.
        assert_eq!(
            pair_base_url(
                &PairArgs {
                    url: Some("https://box.tail.ts.net".to_string())
                },
                "0.0.0.0:9000".parse().unwrap()
            )
            .unwrap(),
            "https://box.tail.ts.net"
        );
    }

    /// The pairing code and the token it buys ride this URL. Plain http off the
    /// box means both are readable in transit, which is worth a warning; loopback
    /// and https are not.
    #[test]
    fn cleartext_off_box_pairing_is_the_only_thing_warned_about() {
        for quiet in [
            "http://127.0.0.1:8848",
            "http://localhost:8848",
            "http://[::1]:8848",
            "https://box.tail.ts.net",
            "HTTPS://Box.example",
        ] {
            assert!(!is_cleartext_offbox(quiet), "{quiet} needs no warning");
        }
        for loud in [
            "http://192.168.1.9:8848",
            "http://nas.local:8848",
            "http://[2001:db8::1]:8848",
            "HTTP://Nas.Local",
        ] {
            assert!(is_cleartext_offbox(loud), "{loud} must be warned about");
        }
    }

    /// The token subcommands parse the shapes the README documents.
    #[test]
    fn token_and_pair_subcommands_parse() {
        match Cli::parse_from(["squelchd", "token", "issue", "--name", "my phone"]).command {
            Command::Token(args) => match args.command {
                TokenCommand::Issue { name } => assert_eq!(name, "my phone"),
                _ => panic!("expected issue"),
            },
            _ => panic!("expected token subcommand"),
        }
        match Cli::parse_from(["squelchd", "token", "revoke", "7"]).command {
            Command::Token(args) => match args.command {
                TokenCommand::Revoke { id } => assert_eq!(id, 7),
                _ => panic!("expected revoke"),
            },
            _ => panic!("expected token subcommand"),
        }
        assert!(matches!(
            Cli::parse_from(["squelchd", "token", "list"]).command,
            Command::Token(TokenArgs {
                command: TokenCommand::List
            })
        ));
        // The warden execs this exact spelling in a tenant pod; a rename here
        // is a rename of a wire contract.
        assert!(matches!(
            Cli::parse_from(["squelchd", "token", "first-paired"]).command,
            Command::Token(TokenArgs {
                command: TokenCommand::FirstPaired
            })
        ));
        // A name is not optional: an unlabeled token is one nobody can pick out
        // of `token list` to revoke.
        assert!(Cli::try_parse_from(["squelchd", "token", "issue"]).is_err());

        match Cli::parse_from(["squelchd", "pair"]).command {
            Command::Pair(args) => assert!(args.url.is_none()),
            _ => panic!("expected pair subcommand"),
        }
        match Cli::parse_from(["squelchd", "pair", "--url", "https://box.example"]).command {
            Command::Pair(args) => assert_eq!(args.url.as_deref(), Some("https://box.example")),
            _ => panic!("expected pair subcommand"),
        }
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
        let api_state = squelch_api::ApiState::new(store.clone(), account_id, "test-token");
        let cancel = CancellationToken::new();
        let app =
            build_serve_router(store, "me@localhost", api_state, cancel).expect("router builds");

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
            .oneshot(Request::builder().uri("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::NOT_FOUND, "/mcp must be mounted");
    }

    /// The metrics bind IS the feature flag: absent or blank means no listener,
    /// and a present-but-bad value fails the daemon rather than quietly serving
    /// no metrics.
    #[test]
    fn resolve_metrics_bind_is_the_feature_flag() {
        let mut config = Config::default();
        assert_eq!(resolve_metrics_bind(&config).unwrap(), None);

        config.metrics.bind = Some("   ".to_string());
        assert_eq!(resolve_metrics_bind(&config).unwrap(), None);

        config.metrics.bind = Some(" 127.0.0.1:9848 ".to_string());
        assert_eq!(
            resolve_metrics_bind(&config).unwrap(),
            Some("127.0.0.1:9848".parse::<SocketAddr>().unwrap())
        );

        config.metrics.bind = Some("nine-thousand".to_string());
        assert!(resolve_metrics_bind(&config).is_err());
    }

    /// A metrics door over an empty in-memory store, for the tests that care
    /// about the router rather than about the numbers.
    fn test_metrics_state() -> MetricsState {
        let store = Arc::new(SqliteStore::open_in_memory().expect("in-memory store"));
        let account_id = store.ensure_account("me@localhost").expect("account");
        let config = Config {
            // A path that does not exist: the db-size gauges must report 0 rather
            // than measuring whatever sits at the developer's default db path.
            db_path: std::env::temp_dir().join("squelch-metrics-door-test.db"),
            ..Config::default()
        };
        MetricsState {
            metrics: squelch_core::metrics::SyncMetrics::new(),
            store,
            account_id,
            config: Arc::new(config),
            gate: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// The metrics door serves `/metrics` and `/healthz` and NOTHING else — no
    /// door of the main router leaks onto this unauthenticated listener.
    #[tokio::test]
    async fn metrics_door_serves_only_metrics() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let app = build_metrics_router(test_metrics_state(), Readiness::default());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("squelchd_build_info{version="));
        // The store read succeeded, so the db-derived block is present.
        assert!(text.contains("squelchd_db_size_bytes{file=\"wal\"} 0\n"));
        assert!(text.contains("squelchd_store_messages_sealed 0\n"));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "the metrics listener hosts no other route"
        );
    }

    /// `/healthz` has two states and one word, and it reaches the second one
    /// only when the whole startup path has run.
    ///
    /// The first assertion is the bug this route exists for: a daemon that has
    /// bound its listeners and nothing more is NOT ready, however cleanly it
    /// accepts a connection.
    #[tokio::test]
    async fn healthz_is_up_only_once_startup_finished() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let readiness = Readiness::default();
        let app = build_metrics_router(test_metrics_state(), readiness.clone());
        let probe = |app: axum::Router| async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri("/healthz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let body = axum::body::to_bytes(resp.into_body(), 1 << 10)
                .await
                .unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        };

        // Bound, and nothing else: the state the old TCP-accept probe called
        // Ready two seconds in.
        let (status, body) = probe(app.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.split_whitespace().count(), 1, "one word, no detail");

        // Half started is not started. Each condition alone is still a 503.
        let sync_running = readiness.sync_started();
        assert_eq!(probe(app.clone()).await.0, StatusCode::SERVICE_UNAVAILABLE);

        // The sync engine's gate is this same bit, which is the point of it
        // being one bit: a gate taken before the settle opens with it, and one
        // taken after is already open.
        let gate_before = readiness.embedder_gate();
        assert!(!*gate_before.borrow(), "shut until the init resolves");
        readiness.embedder_settled();
        assert!(*gate_before.borrow(), "settling the probe opens the gate");
        assert!(
            *readiness.embedder_gate().borrow(),
            "and a late one is open"
        );

        let (status, body) = probe(app.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");

        // And a startup that came apart AFTER the bind reports 503 rather than
        // 200, which is the whole point: dropping the guard is what a sync task
        // that returned, panicked or was aborted does.
        drop(sync_running);
        let (status, body) = probe(app).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.split_whitespace().count(), 1);
    }
}
