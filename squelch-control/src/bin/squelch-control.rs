//! The control-plane binary: `serve`, and the invite-code operator commands.
//!
//! The invite subcommands talk to the STORE directly, exactly as `squelchd
//! token` does: they are operator commands run at a shell, and requiring a
//! running control plane to mint the code that lets someone in would be a
//! circle. They also deliberately need NONE of the serving config, so an
//! operator can issue codes on a box that has no OAuth client and no warden
//! bearer in its environment.
//!
//! NOTHING HERE PRINTS SECRET MATERIAL EXCEPT THE ONE LINE THAT IS SUPPOSED TO:
//! `invite issue` prints each code once, on stdout, alone, so
//! `squelch-control invite issue --count 5 > codes.txt` is a file of codes and
//! nothing else. Everything a human reads goes to stderr. `invite list` prints
//! neither the codes nor their hashes.
//!
//! Env table: `README.md`. Wire contract and deployment: `docs/HOSTED.md`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use squelch_control::bifrost::BifrostClient;
use squelch_control::config::{self, BifrostConfig, OUTBOUND_TIMEOUT};
use squelch_control::warden::{HttpWarden, Warden as _};
use squelch_control::{Config, ControlState, ControlStore, invites, router};
use tracing_subscriber::EnvFilter;

/// How often expired signup sessions are swept. Taking a session already
/// removes expired entries, so this only bounds what an idle process holds.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(
    name = "squelch-control",
    about = "Passband hosted signup control plane",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the signup site.
    Serve,
    /// Mint, list, and revoke invite codes.
    Invite {
        #[command(subcommand)]
        command: InviteCommand,
    },
    /// List provisioned tenants.
    Tenants,
    /// Mint, install, and revoke tenants' LLM virtual keys.
    Llm {
        #[command(subcommand)]
        command: LlmCommand,
    },
}

/// The LLM-key operator commands. Unlike `invite`, these need the Bifrost trio
/// and the warden pair in the environment as well as the store: minting is a
/// governance call and installing is a warden call. They still do NOT need the
/// OAuth client or the cookie key.
///
/// The key VALUES follow the same rule as everywhere else: each exists between
/// the mint and the warden PUT and is never printed, stored, or logged. What
/// these commands print are IDS.
#[derive(Subcommand)]
enum LlmCommand {
    /// Mint BOTH virtual keys for a tenant — triage and assistant — and
    /// install them via one warden call. A tenant that already has them gets
    /// NEW keys; the old ones stay live in Bifrost until revoked there, and
    /// their ids are printed as a reminder.
    Mint { label: String },
    /// Revoke a tenant's recorded virtual keys — both of them — in Bifrost
    /// and forget them.
    Revoke { label: String },
}

#[derive(Subcommand)]
enum InviteCommand {
    /// Mint new codes. Each is printed once and stored only as a hash.
    Issue {
        /// How many to mint.
        #[arg(long, default_value_t = 1)]
        count: usize,
        /// How many days each code stays usable.
        #[arg(long, default_value_t = invites::DEFAULT_TTL_DAYS)]
        ttl: i64,
    },
    /// List invite codes by id and status. Never prints codes or hashes.
    List,
    /// Revoke an UNUSED code by id.
    Revoke { id: i64 },
}

/// Ceiling on one `invite issue` run. A typo in `--count` should not write ten
/// thousand rows and scroll the real codes off the operator's terminal.
const MAX_ISSUE_COUNT: usize = 100;

/// Ceiling on `--ttl`. A code good for years is a code that outlives the
/// campaign it was minted for and the person who was sent it.
const MAX_TTL_DAYS: i64 = 365;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SQUELCH_CONTROL_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Serve => serve(),
        Command::Invite { command } => invite(command),
        Command::Tenants => tenants(),
        Command::Llm { command } => llm(command),
    }
}

fn open_store() -> anyhow::Result<ControlStore> {
    let path = config::db_path_from_env();
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(ControlStore::open(&path)?)
}

fn invite(command: InviteCommand) -> anyhow::Result<()> {
    let store = open_store()?;
    match command {
        InviteCommand::Issue { count, ttl } => {
            if count == 0 || count > MAX_ISSUE_COUNT {
                anyhow::bail!("--count must be between 1 and {MAX_ISSUE_COUNT}");
            }
            if !(1..=MAX_TTL_DAYS).contains(&ttl) {
                anyhow::bail!("--ttl must be between 1 and {MAX_TTL_DAYS} days");
            }
            let expires_at = chrono::Utc::now() + chrono::Duration::days(ttl);
            for _ in 0..count {
                let minted = invites::mint()?;
                let id = store.insert_invite(&minted.code_hash, expires_at)?;
                // THE PLAINTEXT, ALONE ON STDOUT.
                println!("{}", minted.code);
                eprintln!("squelch-control: issued invite {id}");
            }
            eprintln!(
                "squelch-control: that is the ONLY time those codes are shown. Only a hash is \
                 stored, so a lost code is re-issued, never recovered. Each one works once, and \
                 expires {}.",
                stamp(expires_at)
            );
            Ok(())
        }
        InviteCommand::List => {
            let rows = store.list_invites()?;
            if rows.is_empty() {
                eprintln!("squelch-control: no invite codes have been issued.");
                return Ok(());
            }
            println!(
                "{:>5}  {:<20}  {:<20}  {:<20}  USED BY",
                "ID", "CREATED", "EXPIRES", "USED"
            );
            for r in rows {
                println!(
                    "{:>5}  {:<20}  {:<20}  {:<20}  {}",
                    r.id,
                    stamp(r.created_at),
                    r.expires_at.map(stamp).unwrap_or_else(|| "-".to_string()),
                    r.used_at.map(stamp).unwrap_or_else(|| "-".to_string()),
                    r.used_by_label.unwrap_or_else(|| "-".to_string()),
                );
            }
            Ok(())
        }
        InviteCommand::Revoke { id } => {
            if store.revoke_invite(id)? {
                eprintln!("squelch-control: invite {id} revoked.");
            } else {
                // Two cases, one message, and here that is honesty rather than
                // secrecy: an operator at a shell can list the codes.
                eprintln!(
                    "squelch-control: nothing to revoke for invite {id}. It has already been \
                     used, or there is no such invite."
                );
            }
            Ok(())
        }
    }
}

fn tenants() -> anyhow::Result<()> {
    let rows = open_store()?.list_tenants()?;
    if rows.is_empty() {
        eprintln!("squelch-control: no tenants have been provisioned.");
        return Ok(());
    }
    println!(
        "{:<32}  {:<10}  {:<20}  ACCOUNT",
        "LABEL", "STATUS", "CREATED"
    );
    for t in rows {
        println!(
            "{:<32}  {:<10}  {:<20}  {}",
            t.label,
            t.status,
            stamp(t.created_at),
            t.account_email
        );
    }
    Ok(())
}

/// An RFC3339 stamp in UTC, seconds resolution: precise enough to correlate
/// with a log line, short enough to sit in a column.
fn stamp(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn llm(command: LlmCommand) -> anyhow::Result<()> {
    let Some(llm) =
        BifrostConfig::from_env().map_err(|e| anyhow::anyhow!("squelch-control: {e}"))?
    else {
        anyhow::bail!(
            "the LLM gateway is not configured: set SQUELCH_CONTROL_BIFROST_URL and \
             SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN, the gateway admin's username:password \
             (and optionally SQUELCH_CONTROL_LLM_BUDGET_USD, SQUELCH_CONTROL_LLM_MODELS, \
             SQUELCH_CONTROL_ASSISTANT_BUDGET_USD, and SQUELCH_CONTROL_ASSISTANT_MODELS)"
        );
    };
    let (warden_url, warden_token) =
        config::warden_from_env().map_err(|e| anyhow::anyhow!("squelch-control: {e}"))?;

    let store = open_store()?;
    let warden = HttpWarden::new(warden_url, warden_token, OUTBOUND_TIMEOUT)?;
    let bifrost = BifrostClient::new(
        llm.url.clone(),
        llm.admin_token.clone(),
        llm.models.clone(),
        OUTBOUND_TIMEOUT,
    )?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        match command {
            LlmCommand::Mint { label } => llm_mint(&store, &bifrost, &warden, &llm, &label).await,
            LlmCommand::Revoke { label } => llm_revoke(&store, &bifrost, &label).await,
        }
    })
}

/// Mint both -> record both -> install both, the same order signup uses, but
/// FAIL-LOUD: this is the command an operator runs to fix a keyless or
/// mis-keyed tenant, so a half-finished rotation must end in an error naming
/// what to do, not a shrug.
async fn llm_mint(
    store: &ControlStore,
    bifrost: &BifrostClient,
    warden: &HttpWarden,
    llm: &BifrostConfig,
    label: &str,
) -> anyhow::Result<()> {
    if !store.label_exists(label)? {
        anyhow::bail!("no tenant `{label}` in the control store");
    }
    let old = store.tenant_vk(label)?;
    let old_assistant = store.tenant_assistant_vk(label)?;

    // Each id is recorded BEFORE the next step is attempted: from the moment a
    // key exists in Bifrost, whatever happens next, the store must name it so
    // a later `llm revoke` or `llm mint` can find it. A failed install or a
    // failed second mint does not un-mint the first.
    let vk = bifrost.mint_virtual_key(label, llm.budget_usd).await?;
    if !store.set_tenant_vk(label, &vk.id)? {
        anyhow::bail!(
            "tenant `{label}` vanished from the store mid-mint; revoke virtual key {} in Bifrost by hand",
            vk.id
        );
    }
    let assistant = match bifrost
        .mint_assistant_key(label, &llm.assistant_models, llm.assistant_budget_usd)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            if let Some(old) = &old {
                // The store was repointed at the new triage id above, but the
                // cluster still runs on the OLD key. The id must not scroll
                // away, or the operator has nothing to revoke by.
                eprintln!(
                    "squelch-control: the previous triage virtual key {old} is still installed \
                     and live in Bifrost; the store now tracks only the new one."
                );
            }
            anyhow::bail!(
                "minted triage virtual key {} but the assistant mint failed: {e}. The triage id \
                 is recorded and its key is NOT yet installed; run `llm mint {label}` again to \
                 finish the rotation, or `llm revoke {label}` to back out",
                vk.id
            );
        }
    };
    if !store.set_tenant_assistant_vk(label, &assistant.id)? {
        anyhow::bail!(
            "tenant `{label}` vanished from the store mid-mint; revoke virtual keys {} and {} in Bifrost by hand",
            vk.id,
            assistant.id
        );
    }
    if let Err(e) = warden
        .put_llm_key(label, Some(&vk.value), Some(&assistant.value))
        .await
    {
        for (kind, old) in [("triage", &old), ("assistant", &old_assistant)] {
            if let Some(old) = old {
                // The rotation half-failed: the cluster still runs on the OLD
                // keys, which this store no longer tracks. The ids must not
                // scroll away.
                eprintln!(
                    "squelch-control: the previous {kind} virtual key {old} is still installed \
                     and live in Bifrost; the store now tracks only the new one."
                );
            }
        }
        anyhow::bail!(
            "minted virtual keys {} and {} but the warden did not take them: {e}. The ids are \
             recorded; run `llm mint {label}` again to replace them, or `llm revoke {label}` to \
             back out",
            vk.id,
            assistant.id
        );
    }

    eprintln!(
        "squelch-control: virtual keys {} (triage) and {} (assistant) minted and installed for {label}.",
        vk.id, assistant.id
    );
    for (kind, old, new) in [
        ("triage", old, &vk.id),
        ("assistant", old_assistant, &assistant.id),
    ] {
        if let Some(old) = old.filter(|old| old != new) {
            eprintln!(
                "squelch-control: the PREVIOUS {kind} virtual key {old} is still live in Bifrost; \
                 revoke it there. This store now tracks only the new key."
            );
        }
    }
    Ok(())
}

/// Revoke BOTH recorded keys. Each revoke is tolerant of already-gone (the
/// client maps 404 to Ok), and a failure on one does not stop the other: the
/// pointer for whichever revoke did land is cleared, so the retry has only the
/// stragglers left to chase.
async fn llm_revoke(
    store: &ControlStore,
    bifrost: &BifrostClient,
    label: &str,
) -> anyhow::Result<()> {
    if !store.label_exists(label)? {
        anyhow::bail!("no tenant `{label}` in the control store");
    }
    let triage = store.tenant_vk(label)?;
    let assistant = store.tenant_assistant_vk(label)?;
    if triage.is_none() && assistant.is_none() {
        eprintln!("squelch-control: no virtual keys are recorded for {label}; nothing to revoke.");
        return Ok(());
    }

    let mut failed = Vec::new();
    if let Some(id) = triage {
        match bifrost.revoke_virtual_key(&id).await {
            Ok(()) => {
                // Cleared only AFTER Bifrost confirms: a revoke that failed
                // must leave the pointer in place for the retry.
                store.clear_tenant_vk(label)?;
                eprintln!(
                    "squelch-control: triage virtual key {id} revoked and forgotten for {label}."
                );
            }
            Err(e) => failed.push(format!("triage key {id}: {e}")),
        }
    }
    if let Some(id) = assistant {
        match bifrost.revoke_virtual_key(&id).await {
            Ok(()) => {
                store.clear_tenant_assistant_vk(label)?;
                eprintln!(
                    "squelch-control: assistant virtual key {id} revoked and forgotten for {label}."
                );
            }
            Err(e) => failed.push(format!("assistant key {id}: {e}")),
        }
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "not everything was revoked ({}); the ids stay recorded, run `llm revoke {label}` again",
            failed.join("; ")
        );
    }
    Ok(())
}

fn serve() -> anyhow::Result<()> {
    let config = Config::from_env().map_err(|e| anyhow::anyhow!("squelch-control: {e}"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve_async(config))
}

async fn serve_async(config: Config) -> anyhow::Result<()> {
    let bind = config.bind;
    let public_url = config.public_url.clone();
    let base_domain = config.base_domain.clone();
    let insecure = config.is_insecure();

    let store = open_store()?;
    let warden = Arc::new(HttpWarden::new(
        config.warden_url.clone(),
        config.warden_token.clone(),
        OUTBOUND_TIMEOUT,
    )?);
    // The Bifrost client is derived inside `ControlState::new` from
    // `config.bifrost`: signup mints per-tenant LLM keys exactly when the
    // trio is configured, and half-states were refused at config time.
    let state = ControlState::new(config, store, warden)?;
    let app = router(state.clone());

    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_EVERY);
        loop {
            ticker.tick().await;
            let swept = sweeper.sweep_sessions();
            if swept > 0 {
                // PRIVACY: a count. Never which sessions went.
                tracing::debug!(swept, "expired signup sessions swept");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let bound = listener.local_addr().unwrap_or(bind);
    // One startup line. The public URL and the base domain are public by
    // definition; nothing else about the configuration is logged.
    tracing::info!(%bound, %public_url, %base_domain, "squelch-control: serving");
    if insecure {
        tracing::warn!(
            "SQUELCH_CONTROL_PUBLIC_URL is plain http, so the signup cookie cannot be Secure and \
             Google would deliver authorization codes in the clear. Local development only"
        );
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// Ctrl-C, or the SIGTERM a container runtime stops us with.
///
/// SIGTERM is not optional: this process is PID 1 in its image, and PID 1 has
/// no default disposition for a signal it does not handle. Without this every
/// redeploy would wait out the grace period for a SIGKILL.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            // Losing the handler costs a slower stop, never correctness:
            // pending signups die with the process either way, by design.
            Err(e) => {
                tracing::warn!(error = %e, "no SIGTERM handler; stopping on ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("squelch-control: shutting down");
}
