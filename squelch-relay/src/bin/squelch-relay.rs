//! The relay binary: validate config from the environment, refuse to start on
//! any bad value, serve the router. The bind default is loopback — a TLS proxy
//! is expected in front, and we never widen to a public interface silently.
//!
//! Env table: `README.md`.

use std::net::SocketAddr;

use squelch_relay::{Config, RelayState, router};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SQUELCH_RELAY_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().map_err(|e| anyhow::anyhow!("squelch-relay: {e}"))?;
    let bind = config.bind;
    let authed = config.auth_token.is_some();
    let topics = config.apns_topics.len();
    let environment = config.apns_env.as_str();
    let overridden = config.apns_url_override.is_some();
    let ephemeral_buffer = config.db_path.is_none();

    let state = RelayState::new(config)?;
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let bound = listener.local_addr().unwrap_or(bind);
    // Single startup line. No key material, bearer token, or topic value.
    tracing::info!(
        %bound,
        authed,
        topics,
        environment,
        "squelch-relay: serving"
    );
    if !authed {
        tracing::warn!(
            "SQUELCH_RELAY_ALLOW_ANONYMOUS is set; POST /v1/push is served WITHOUT authentication"
        );
    }
    if overridden {
        tracing::warn!(
            "SQUELCH_RELAY_APNS_URL_OVERRIDE is set; pushes are NOT going to Apple (test-only)"
        );
    }
    if ephemeral_buffer {
        tracing::warn!(
            "SQUELCH_RELAY_DB_PATH is unset; the open buffer is in memory and any open the daemon has not drained is LOST on restart"
        );
    }

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("squelch-relay: shutting down");
    };
    // Connect info is required by the per-IP rate limiter.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}
