//! `squelchd` — the squelch daemon / CLI.
//!
//! Subcommands:
//! - `auth`: run the interactive installed-app OAuth flow and store tokens in
//!   the OS keyring. Requires your own Google Cloud "Desktop app" OAuth client
//!   (see [`squelch_core::config`] docs).
//! - `run`: long-running sync/serve loop. STUB — implemented by another agent.

use clap::{Parser, Subcommand};
use squelch_core::auth::run_installed_app_flow;
use squelch_core::config::Config;
use squelch_core::credentials::{KeyringCredentialStore, load_token, store_token};
use squelch_core::store::SqliteStore;
use squelch_core::sync::SyncEngine;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

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
    /// Authorize a Gmail account (read-only) and store tokens in the OS keyring.
    Auth,
    /// Run the daemon (sync + serve). Currently a stub.
    Run,
}

fn load_config(cli: &Cli) -> Config {
    match &cli.config {
        Some(path) => Config::load_from(path),
        None => Config::load(),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = load_config(&cli);

    let result = match cli.command {
        Command::Auth => cmd_auth(&config),
        Command::Run => run_daemon(config),
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
fn cmd_auth(config: &Config) -> Result<(), squelch_core::CoreError> {
    let client = config.oauth_client()?;
    let email = config.require_account_email()?;

    println!("Authorizing Gmail account: {email}");
    let token = run_installed_app_flow(&client)?;
    store_token(&email, &token)?;

    // Confirm persistence without ever printing the token material.
    let _ = load_token(&email)?;
    let has_refresh = token.refresh_token.is_some();
    println!("\nStored credentials for {email} in the OS keyring (service \"squelch\").");
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
    let store = SqliteStore::open(&config.db_path)?;
    let account_id = store.ensure_account(&email)?;

    let store = Arc::new(store);
    let creds = Arc::new(KeyringCredentialStore::new(
        account_id,
        email.clone(),
        client,
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| squelch_core::CoreError::Other(anyhow::anyhow!("tokio runtime: {e}")))?;

    runtime.block_on(async move {
        // Broadcast shutdown to the sync loop on Ctrl-C.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nsquelch: shutdown requested; finishing in-flight work...");
                let _ = shutdown_tx.send(true);
            }
        });

        let engine = SyncEngine::new(store, creds, account_id, email, config);
        engine.run(shutdown_rx).await
    })?;

    eprintln!("squelch: sync stopped.");
    Ok(())
}
