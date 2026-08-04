//! The broker binary: validate config from the environment, refuse to start on
//! any bad value, serve the router, sweep expired sessions on a clock. The bind
//! default is loopback — a TLS proxy is expected in front, and we never widen to
//! a public interface silently.
//!
//! Env table: `README.md`. Wire contract: `docs/BROKER.md`.

use std::net::SocketAddr;
use std::time::Duration;

use squelch_broker::{BrokerState, Config, SESSION_TTL, router};
use tracing_subscriber::EnvFilter;

/// How often the background sweep runs. Lazy purging already bounds what anyone
/// can observe, so this only bounds what an idle process holds: a tenth of the
/// TTL is far more often than it needs to be and still costs nothing.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Ctrl-C, or the SIGTERM a container runtime stops us with.
///
/// SIGTERM is not optional here: this process is PID 1 in its image, and PID 1
/// has no default disposition for a signal it does not handle. Without this the
/// platform's stop would be ignored and every redeploy would wait out the grace
/// period for a SIGKILL.
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
            // Losing the handler costs a slower stop, never correctness: there
            // is no state to flush.
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SQUELCH_BROKER_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().map_err(|e| anyhow::anyhow!("squelch-broker: {e}"))?;
    let bind = config.bind;
    let public_url = config.public_url.clone();
    let insecure = config.is_insecure();

    let state = BrokerState::new(config);
    let app = router(state.clone());

    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_EVERY);
        loop {
            ticker.tick().await;
            let swept = sweeper.sweep_expired();
            if swept > 0 {
                // PRIVACY: a count. Never which sessions went.
                tracing::debug!(swept, "expired sessions swept");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let bound = listener.local_addr().unwrap_or(bind);
    // Single startup line. The public URL is public by definition; nothing else
    // about a session ever appears in a log.
    tracing::info!(
        %bound,
        %public_url,
        ttl_secs = SESSION_TTL.as_secs(),
        "squelch-broker: serving"
    );
    if insecure {
        tracing::warn!(
            "SQUELCH_BROKER_PUBLIC_URL is plain http; Google would deliver authorization codes to /callback in the clear. Local development only"
        );
    }

    let shutdown = async {
        shutdown_signal().await;
        // Pending consents die with the process, by design: the recovery is
        // re-running `squelchd auth`.
        tracing::info!("squelch-broker: shutting down");
    };
    // Connect info is required by the per-IP rate limiters.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}
