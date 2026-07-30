//! Thin dev binary for the human door: opens the store, builds
//! [`squelch_api::ApiState`] from the environment (refusing to start without
//! `SQUELCH_API_TOKEN`), and serves `/client/*` on loopback — never a
//! non-loopback interface, a reverse proxy is expected to front it. Also reads
//! `SQUELCH_DB_PATH`, `SQUELCH_ACCOUNT_EMAIL` and `SQUELCH_API_HTTP`.

use std::net::SocketAddr;
use std::sync::Arc;

use squelch_api::{ApiState, router};
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
    // Refuses to build (and thus serve) without SQUELCH_API_TOKEN.
    let mut state = ApiState::from_env(store.clone(), &email)?;

    // Action endpoints are enabled ONLY when OAuth client credentials are
    // configured; the store built below is bound to CredentialKind::Write and
    // the sync/triage read path never touches it. Without it, actions 403.
    let (cfg, cap_sources) = Config::load_with_cap_sources();
    state = state.with_stage2_prices(
        cfg.stage2.price_in_per_mtok,
        cfg.stage2.price_out_per_mtok,
    );
    state = state.with_stage2_model(
        cfg.stage2.model.clone(),
        cfg.stage2.stage2_provider.map(|p| p.as_str().to_string()),
    );
    state = state.with_stage2_caps(
        cfg.stage2.thread_daily_cap,
        cfg.stage2.sender_daily_cap,
        cfg.stage2.global_daily_cap,
        cap_sources,
    );
    state = state.with_stage1_config(
        cfg.stage1.model.clone(),
        cfg.stage1.price_in_per_mtok,
        cfg.stage1.price_out_per_mtok,
        cfg.stage1.global_daily_cap,
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

    // SSE plumbing. Nothing here appends events, but the shutdown signal is NOT
    // optional: an open `/client/events` stream would keep
    // `with_graceful_shutdown` waiting forever on Ctrl-C.
    let (event_tx, _) = tokio::sync::broadcast::channel::<i64>(256);
    store.attach_event_notifier(event_tx.clone())?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    state = state.with_event_notifier(event_tx).with_shutdown(shutdown_rx);

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
