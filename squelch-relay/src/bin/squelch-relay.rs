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
    let trusted_proxy_hops = config.trusted_proxy_hops;
    let default_allowlist = config.trusted_proxy_cidrs.is_none();
    let trusted_peers = config
        .proxy_allowlist()
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(", ");

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
    // Which identity the limiters meter is not visible from any response, so an
    // operator has no other way to tell a per-client relay from one where every
    // caller shares a bucket. Header VALUES are never logged, only the mode.
    if trusted_proxy_hops == 0 {
        tracing::info!(
            "SQUELCH_RELAY_TRUSTED_PROXY_HOPS is unset: rate limits key on the TCP peer address, so behind a TLS proxy every client shares ONE bucket; set it to the number of proxies in front of this listener (1 for a single one) to meter real clients"
        );
    } else {
        tracing::info!(
            hops = trusted_proxy_hops,
            peers = %trusted_peers,
            "rate limits key on X-Forwarded-For, counting from the right, but ONLY for a request whose TCP peer is one of those addresses; every other peer is metered by its own address, as is a request whose header is shorter than the hop count. THIS LISTENER MUST NOT BE REACHABLE EXCEPT THROUGH THE DECLARED PROXIES: anything that can open a connection to it from a trusted address chooses its own rate-limit identity"
        );
        if default_allowlist {
            tracing::info!(
                "SQUELCH_RELAY_TRUSTED_PROXY_CIDRS is unset, so the trusted peers are the default loopback plus RFC1918/RFC4193 private ranges (the sidecar or private-network proxy case); set it if your proxy reaches this listener from a public address"
            );
        }
    }
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
