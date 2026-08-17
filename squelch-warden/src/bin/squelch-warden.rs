//! The warden binary: validate config from the environment, refuse to start on
//! any bad value, connect to the cluster with the pod's own ServiceAccount, and
//! serve the router.
//!
//! It binds all interfaces because it is a pod: a ClusterIP Service in front, a
//! NetworkPolicy around it, and its own Ingress at `warden.<base domain>` with
//! TLS terminated there. Loopback inside a pod would just make the Service
//! answer nothing.
//!
//! Env table: `README.md`. Runbook: `deploy/hosted/SETUP.md`.

use std::sync::Arc;
use std::time::Duration;

use squelch_warden::{Cluster, Config, KubeCluster, Warden, WardenState, router};
use tracing_subscriber::EnvFilter;

/// How often the pending sweep runs: four times per TTL, so an abandoned signup
/// is collected within a quarter of one, bounded so a short TTL does not spin
/// and a long one still gets a daily pass.
fn sweep_interval(pending_ttl: Duration) -> Duration {
    (pending_ttl / 4).clamp(Duration::from_secs(60), Duration::from_secs(60 * 60))
}

/// Ctrl-C, or the SIGTERM the kubelet stops us with.
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
            // warden holds no state of its own, and a provision in flight is
            // either finished at the API server or was never applied.
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
            EnvFilter::try_from_env("SQUELCH_WARDEN_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().map_err(|e| anyhow::anyhow!("squelch-warden: {e}"))?;
    let bind = config.bind;
    let base_domain = config.base_domain.clone();
    let namespace = config.namespace.clone();
    let user_namespaces = config.user_namespaces;
    let llm_gateway = config.llm_base_url.is_some();

    // Fail at startup rather than on the first signup: a warden that cannot
    // reach the API server is a warden that can do nothing at all, and the
    // operator should hear about that while they are still looking at a
    // terminal.
    let cluster = KubeCluster::connect(namespace.clone())
        .await
        .map_err(|e| anyhow::anyhow!("squelch-warden: cannot reach the Kubernetes API ({e})"))?;

    let pending_ttl = config.pending_ttl;
    let warden = Arc::new(Warden::new(
        Arc::new(config),
        Arc::new(cluster) as Arc<dyn Cluster>,
    ));
    let app = router(WardenState::new(warden.clone()));

    // The one background job this service has. A signup that reached phase one
    // and never came back parks an identity Secret nothing will ever open and
    // holds a public subdomain against everyone else, and the control plane
    // cannot see it to ask. Detached rather than joined: the process's job is
    // to serve, and a sweep that fails is a sweep that runs again shortly.
    let sweeper = warden.clone();
    let interval = sweep_interval(pending_ttl);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; skip it so a restart loop cannot
        // turn into a sweep loop.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match sweeper.sweep_pending().await {
                Ok(0) => {}
                Ok(collected) => tracing::info!(collected, "swept abandoned pending tenants"),
                // Already logged with its machine reason inside the sweep.
                Err(_) => {}
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let bound = listener.local_addr().unwrap_or(bind);
    // One startup line. The token is not in it, and never is.
    tracing::info!(
        %bound,
        %base_domain,
        %namespace,
        user_namespaces,
        "squelch-warden: serving"
    );
    if !user_namespaces {
        tracing::warn!(
            "SQUELCH_WARDEN_USER_NAMESPACES is off: tenant pods will share the node's user namespace, so uid isolation is the only boundary left between them"
        );
    }
    if !llm_gateway {
        tracing::warn!(
            "no LLM gateway configured (SQUELCH_WARDEN_LLM_BASE_URL unset): every tenant runs heuristic-only triage, and llm-key installs will be refused"
        );
    }

    // ConnectInfo is what feeds the per-IP rate limiter its peer address; a bare
    // `serve(listener, app)` never inserts it and every client would fold into
    // the 0.0.0.0 fallback bucket, turning the limiter into a global one.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        shutdown_signal().await;
        tracing::info!("squelch-warden: shutting down");
    })
    .await?;
    Ok(())
}
