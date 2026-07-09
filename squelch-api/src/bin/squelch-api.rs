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
//! - `SQUELCH_DB_PATH` (optional): SQLite path. Defaults to the XDG data dir.
//!   Legacy `SQUELCH_DB` is still accepted (deprecated).
//! - `SQUELCH_ACCOUNT_EMAIL` (optional): account email. Defaults to
//!   `me@localhost`. Legacy `SQUELCH_ACCOUNT` is still accepted (deprecated).
//! - `SQUELCH_API_HTTP` (optional): bind address. Defaults to 127.0.0.1:8849.

use std::net::SocketAddr;
use std::sync::Arc;

use squelch_api::{ApiState, router};
use squelch_core::config::Config;
use squelch_core::store::SqliteStore;

/// Loopback default. A reverse proxy fronts this; never widen it silently.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8849";

/// SQLite path via core config's single source of truth: canonical
/// `SQUELCH_DB_PATH` > legacy `SQUELCH_DB` (deprecated) > shared XDG default.
fn db_path() -> std::path::PathBuf {
    squelch_core::config::resolve_db_path()
}

/// Account email: canonical `SQUELCH_ACCOUNT_EMAIL` > legacy `SQUELCH_ACCOUNT`
/// (deprecated) > default.
fn account_email() -> String {
    squelch_core::config::resolve_account_email("me@localhost")
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
    // Wire the Stage-2 per-MTok prices so /client/stats reports cost against the
    // configured model's pricing.
    state = state.with_stage2_prices(
        cfg.stage2.price_in_per_mtok,
        cfg.stage2.price_out_per_mtok,
    );
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
