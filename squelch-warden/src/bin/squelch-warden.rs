//! The warden binary: validate config from the environment, refuse to start on
//! any bad value, make sure the state directory exists, serve the router.
//!
//! The bind default is loopback and stays loopback: Caddy fronts this at
//! `warden.<base domain>` with TLS, and a warden listening on a public
//! interface would be a root-equivalent API in the clear.
//!
//! Env table: `README.md`. Runbook: `deploy/hosted/SETUP.md`.

use std::sync::Arc;

use squelch_warden::host::{CommandRunner, Fs, RealCommandRunner, RealFs};
use squelch_warden::{Config, Warden, WardenState, router};
use tracing_subscriber::EnvFilter;

/// Ctrl-C, or the SIGTERM systemd stops us with.
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
            // Losing the handler costs a slower stop, never correctness: the
            // state file is written atomically and a provision in flight holds
            // no lock anything else needs.
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
            EnvFilter::try_from_env("SQUELCH_WARDEN_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().map_err(|e| anyhow::anyhow!("squelch-warden: {e}"))?;
    let bind = config.bind;
    let base_domain = config.base_domain.clone();
    let port_base = config.port_base;

    let warden = Arc::new(Warden::new(
        Arc::new(config),
        Arc::new(RealFs) as Arc<dyn Fs>,
        Arc::new(RealCommandRunner) as Arc<dyn CommandRunner>,
    ));
    // Fail at startup rather than on the first signup: a state dir the warden
    // cannot create is a warden that cannot remember anything, and the operator
    // should hear about that while they are still looking at the terminal.
    warden
        .ensure_state_dir()
        .map_err(|e| anyhow::anyhow!("squelch-warden: state dir is not usable ({e})"))?;

    let app = router(WardenState::new(warden));

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let bound = listener.local_addr().unwrap_or(bind);
    // One startup line. The token is not in it, and never is.
    tracing::info!(%bound, %base_domain, port_base, "squelch-warden: serving");
    if !bound.ip().is_loopback() {
        tracing::warn!(
            "SQUELCH_WARDEN_BIND is not loopback. This API creates systemd units; put TLS and a proxy in front of it or bind 127.0.0.1"
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            shutdown_signal().await;
            tracing::info!("squelch-warden: shutting down");
        })
        .await?;
    Ok(())
}
