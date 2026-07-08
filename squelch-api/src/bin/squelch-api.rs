//! Thin dev binary for the human door.
//!
//! Opens the store, builds [`squelch_api::ApiState`] from the environment
//! (refusing to start without `SQUELCH_API_TOKEN`), and serves the `/client/*`
//! router on a loopback address by default. On a headless Linux box a reverse
//! proxy (e.g. `tailscale serve`) is expected to front this listener; we never
//! default to a non-loopback interface.
//!
//! Env:
//! - `SQUELCH_API_TOKEN` (required): bearer token for every `/client/*` route.
//! - `SQUELCH_DB` (optional): SQLite path. Defaults to the XDG data dir.
//! - `SQUELCH_ACCOUNT` (optional): account email. Defaults to `me@localhost`.
//! - `SQUELCH_API_HTTP` (optional): bind address. Defaults to 127.0.0.1:8849.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use squelch_api::{ApiState, router};
use squelch_core::config::Config;
use squelch_core::store::SqliteStore;

/// Loopback default. A reverse proxy fronts this; never widen it silently.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8849";

fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("SQUELCH_DB") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        let dir = PathBuf::from(home).join(".local/share/squelch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("squelch.db");
    }
    PathBuf::from("squelch.db")
}

fn account_email() -> String {
    std::env::var("SQUELCH_ACCOUNT").unwrap_or_else(|_| "me@localhost".to_string())
}

fn bind_addr() -> anyhow::Result<SocketAddr> {
    let s = std::env::var("SQUELCH_API_HTTP").unwrap_or_else(|_| DEFAULT_HTTP_ADDR.to_string());
    s.parse()
        .map_err(|e| anyhow::anyhow!("invalid SQUELCH_API_HTTP `{s}`: {e}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let store = Arc::new(SqliteStore::open(db_path())?);
    let email = account_email();
    // Refuses to build (and thus serve) without SQUELCH_API_TOKEN.
    let mut state = ApiState::from_env(store, &email)?;

    // Enable action endpoints ONLY when OAuth client credentials are configured.
    // The write credential store is bound to CredentialKind::Write inside
    // `with_write_credentials`; the sync/triage read path never touches it. If
    // the OAuth client isn't configured, actions return 403 with a hint.
    let cfg = Config::load();
    match cfg.oauth_client() {
        Ok(client) => {
            state = state.with_write_credentials(
                cfg.credential_backend,
                email.clone(),
                cfg.resolve_credentials_path(),
                client,
            );
        }
        Err(_) => {
            eprintln!(
                "squelch-api: no OAuth client configured; action endpoints will return 403 \
                 (set client_id/client_secret and run `squelchd auth --write`)"
            );
        }
    }

    let app = router(state);

    let addr = bind_addr()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr().unwrap_or(addr);
    // Single startup line. No token or message content is ever logged.
    eprintln!("squelch-api: serving human door on http://{bound}/client/*");

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("squelch-api: shutting down");
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
