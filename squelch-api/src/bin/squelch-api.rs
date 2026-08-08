//! Thin dev binary for the human door: opens the store, builds
//! [`squelch_api::ApiState`] from the environment, and serves `/client/*` on
//! loopback — never a non-loopback interface, a reverse proxy is expected to
//! front it. Also reads `SQUELCH_DB_PATH`, `SQUELCH_ACCOUNT_EMAIL` and
//! `SQUELCH_API_HTTP`.
//!
//! `SQUELCH_API_TOKEN` is optional here, as it is in `squelchd serve`: without
//! it the door still comes up and accepts issued device tokens only. This binary
//! mints none — that is `squelchd token issue` / `squelchd pair`, which write to
//! the same store.

use std::net::SocketAddr;
use std::sync::Arc;

use squelch_api::{ApiState, attach_event_channel, router};
use squelch_core::config::Config;
use squelch_core::store::SqliteStore;

/// Loopback default. A reverse proxy fronts this; never widen it silently.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8849";

/// Canonical `SQUELCH_DB_PATH` > legacy `SQUELCH_DB` > shared XDG default.
fn db_path() -> std::path::PathBuf {
    squelch_core::config::resolve_db_path()
}

/// Canonical `SQUELCH_ACCOUNT_EMAIL` > legacy `SQUELCH_ACCOUNT` > default.
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
    let (cfg, cap_sources) = Config::load_with_cap_sources();
    // The shared config->state wiring (prices, model labels, caps, Stage-1, and
    // the write credential).
    let state = ApiState::from_config(store.clone(), &email, &cfg, cap_sources)?;

    // SSE plumbing. Nothing here appends events, but the shutdown signal is NOT
    // optional: an open `/client/events` stream would keep
    // `with_graceful_shutdown` waiting forever on Ctrl-C.
    let event_tx = attach_event_channel(&store)?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = state.with_event_notifier(event_tx).with_shutdown(shutdown_rx);

    let app = router(state);

    let addr = bind_addr()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr().unwrap_or(addr);
    // Single startup line. No token or message content is ever logged.
    eprintln!("squelch-api: serving human door on http://{bound}/client/*");

    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("squelch-api: shutting down");
        // Ends open SSE streams so the drain below can actually finish.
        let _ = shutdown_tx.send(true);
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
