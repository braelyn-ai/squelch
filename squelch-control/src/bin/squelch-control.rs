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
use squelch_control::config::{self, OUTBOUND_TIMEOUT};
use squelch_control::warden::HttpWarden;
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
}

#[derive(Subcommand)]
enum InviteCommand {
    /// Mint new codes. Each is printed once and stored only as a hash.
    Issue {
        /// How many to mint.
        #[arg(long, default_value_t = 1)]
        count: usize,
    },
    /// List invite codes by id and status. Never prints codes or hashes.
    List,
    /// Revoke an UNUSED code by id.
    Revoke { id: i64 },
}

/// Ceiling on one `invite issue` run. A typo in `--count` should not write ten
/// thousand rows and scroll the real codes off the operator's terminal.
const MAX_ISSUE_COUNT: usize = 100;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SQUELCH_CONTROL_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Serve => serve(),
        Command::Invite { command } => invite(command),
        Command::Tenants => tenants(),
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
        InviteCommand::Issue { count } => {
            if count == 0 || count > MAX_ISSUE_COUNT {
                anyhow::bail!("--count must be between 1 and {MAX_ISSUE_COUNT}");
            }
            for _ in 0..count {
                let minted = invites::mint()?;
                let id = store.insert_invite(&minted.code_hash)?;
                // THE PLAINTEXT, ALONE ON STDOUT.
                println!("{}", minted.code);
                eprintln!("squelch-control: issued invite {id}");
            }
            eprintln!(
                "squelch-control: that is the ONLY time those codes are shown. Only a hash is \
                 stored, so a lost code is re-issued, never recovered. Each one works once."
            );
            Ok(())
        }
        InviteCommand::List => {
            let rows = store.list_invites()?;
            if rows.is_empty() {
                eprintln!("squelch-control: no invite codes have been issued.");
                return Ok(());
            }
            println!("{:>5}  {:<20}  {:<20}  USED BY", "ID", "CREATED", "USED");
            for r in rows {
                println!(
                    "{:>5}  {:<20}  {:<20}  {}",
                    r.id,
                    stamp(r.created_at),
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
