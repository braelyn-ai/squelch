//! The warden binary. One image, two jobs, chosen by the first argument.
//!
//! With no arguments it SERVES: validate config from the environment, refuse to
//! start on any bad value, connect to the cluster with the pod's own
//! ServiceAccount, and answer the control plane's calls. That is what the
//! Deployment in `deploy/hosted/20-warden.yaml` runs.
//!
//! `roll` walks the whole fleet instead and puts every tenant that has fallen
//! behind today's render back onto it, one at a time; then the process exits
//! with a code that says whether the fleet converged. That is what the CronJob
//! in `deploy/hosted/90-warden-roller.yaml` runs, on the same image, the same
//! ServiceAccount and the same environment as the serving pod - the same
//! environment because both paths render tenants from it, and two renders that
//! disagree would take turns rewriting the same Deployments forever.
//!
//! It is a library call and not an HTTP route on purpose. A converging pass over
//! every tenant is the most powerful thing this service can do, and it stays
//! inside the cluster: no credential leaves the box to trigger it, nothing
//! outside k3s can ask for it, and the bearer token buys no part of it.
//!
//! The serving path binds all interfaces because it is a pod: a ClusterIP
//! Service in front, a NetworkPolicy around it, and its own Ingress at
//! `warden.<base domain>` with TLS terminated there. Loopback inside a pod would
//! just make the Service answer nothing.
//!
//! Env table: `README.md`. Runbook: `deploy/hosted/SETUP.md`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use squelch_warden::{Cluster, Config, KubeCluster, Rolled, Warden, WardenState, router};
use tracing_subscriber::EnvFilter;

/// What this process was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Serve,
    Roll { dry_run: bool },
}

/// The whole grammar, which is also the whole help text: an unknown argument
/// prints this and stops, and there is no `--help` flag because it would print
/// the same three lines.
const USAGE: &str = "\
usage:
  squelch-warden                   serve the control plane's API (what the Deployment runs)
  squelch-warden roll              converge every tenant onto today's render, one at a time
  squelch-warden roll --dry-run    report what a roll would change, and change nothing

`roll` exits 0 converged (nothing to do counts), 1 halted on a tenant that did
not converge, 2 converged with tenants skipped for foreign drift.
";

// `roll`'s exit status is the CronJob's entire interface to a run: the pod's
// log says what happened, and this says what to do about it.

/// The fleet is on today's render. A run with nothing to do is this.
const EXIT_CONVERGED: u8 = 0;
/// The fleet is NOT converged: the roll halted on a tenant that did not come
/// back, or it never started at all (a refused config, an API server it could
/// not reach). Both want the same thing - a person reading the log before
/// anything else is applied - so both are one code.
const EXIT_HALTED: u8 = 1;
/// Everything this run could converge did, and at least one tenant was left
/// alone because another field manager owns part of its Deployment. Nothing is
/// broken, and nothing will fix itself either; see
/// [`squelch_warden::Warden::roll`] for why a timer must not repair one.
const EXIT_SKIPPED: u8 = 2;
/// The argument list was none of the three this binary accepts (`EX_USAGE`).
/// Deliberately outside the 0-2 range: a mistyped CronJob argument must not
/// read as a verdict on the fleet.
const EXIT_USAGE: u8 = 64;

/// Parse the argument list. `None` is a usage error.
///
/// Matched by hand rather than with a parser crate: the grammar is three lines
/// and a dependency here would be a dependency in the pod that holds an API
/// token. Unknown arguments are REFUSED rather than ignored - falling through to
/// `serve` would answer a mistyped `roll` by starting a second warden that
/// serves nobody and never converges anything, which is the one failure a
/// silently-ignored argument could produce here.
fn command(args: &[String]) -> Option<Command> {
    match args {
        [] => Some(Command::Serve),
        [verb] if verb == "roll" => Some(Command::Roll { dry_run: false }),
        [verb, flag] if verb == "roll" && flag == "--dry-run" => {
            Some(Command::Roll { dry_run: true })
        }
        _ => None,
    }
}

/// What the run's outcome means to whoever scheduled it. See the exit-code
/// constants; a halt outranks a skip, because a fleet that stopped converging
/// is the more urgent of the two facts.
fn verdict(rolled: &Rolled) -> u8 {
    if rolled.halted_on.is_some() {
        EXIT_HALTED
    } else if !rolled.skipped_foreign.is_empty() {
        EXIT_SKIPPED
    } else {
        EXIT_CONVERGED
    }
}

/// The run in a few lines, for a person reading `kubectl logs`.
///
/// Counts and LABELS, which are public subdomains. The structured per-tenant
/// lines are in the log above this; what this adds is the shape of the whole
/// run in one place, and which tenants a person now has to do something about.
fn summarize(rolled: &Rolled, dry_run: bool) -> String {
    let (head, verb) = if dry_run {
        ("fleet roll (dry run)", "would roll")
    } else {
        ("fleet roll", "rolled")
    };
    let mut out = format!(
        "{head}: {} checked, {} {verb}, {} already current\n",
        rolled.checked,
        rolled.rolled.len(),
        rolled.current,
    );
    if !rolled.rolled.is_empty() {
        out.push_str(&format!("  {verb}: {}\n", rolled.rolled.join(", ")));
    }
    if !rolled.skipped_foreign.is_empty() {
        out.push_str(&format!(
            "  needs a person (another field manager owns part of the Deployment): {}\n",
            rolled.skipped_foreign.join(", ")
        ));
    }
    if !rolled.skipped_inactive.is_empty() {
        out.push_str(&format!(
            "  no workload to converge (pending or stopped): {}\n",
            rolled.skipped_inactive.join(", ")
        ));
    }
    if let Some(label) = &rolled.halted_on {
        out.push_str(&format!(
            "  HALTED on {label}; the tenants after it were not touched\n"
        ));
    }
    out
}

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

/// Read the environment and connect, for whichever job this process is doing.
///
/// ONE construction for both, and that is the point rather than tidiness: a
/// tenant's Deployment is rendered from this config, so a roller built from a
/// different one would apply a different render than the warden that serves
/// signups, and the two would flip every tenant back and forth between them.
/// The manifests keep the environments identical; this keeps the code that
/// reads them identical.
async fn connect() -> anyhow::Result<Arc<Warden>> {
    let config = Config::from_env().map_err(|e| anyhow::anyhow!("squelch-warden: {e}"))?;
    // Fail at startup rather than on the first signup: a warden that cannot
    // reach the API server is a warden that can do nothing at all, and the
    // operator should hear about that while they are still looking at a
    // terminal.
    let cluster = KubeCluster::connect(config.namespace.clone())
        .await
        .map_err(|e| anyhow::anyhow!("squelch-warden: cannot reach the Kubernetes API ({e})"))?;
    Ok(Arc::new(Warden::new(
        Arc::new(config),
        Arc::new(cluster) as Arc<dyn Cluster>,
    )))
}

/// Walk the fleet once and leave. Nothing is served, nothing is scheduled, and
/// the answer is an exit code: this process exists for the length of one pass.
async fn roll(dry_run: bool) -> anyhow::Result<ExitCode> {
    let warden = connect().await?;
    // The error carries a machine reason and nothing else, like every other
    // answer this service gives; the detail is already in the log above it.
    let rolled = warden
        .roll(dry_run)
        .await
        .map_err(|e| anyhow::anyhow!("squelch-warden roll: {e}"))?;
    eprint!("{}", summarize(&rolled, dry_run));
    Ok(ExitCode::from(verdict(&rolled)))
}

/// Serve until the kubelet stops us.
async fn serve() -> anyhow::Result<()> {
    let warden = connect().await?;
    let bind = warden.config().bind;
    let pending_ttl = warden.config().pending_ttl;
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
        base_domain = %warden.config().base_domain,
        namespace = %warden.config().namespace,
        user_namespaces = warden.config().user_namespaces,
        "squelch-warden: serving"
    );
    if !warden.config().user_namespaces {
        tracing::warn!(
            "SQUELCH_WARDEN_USER_NAMESPACES is off: tenant pods will share the node's user namespace, so uid isolation is the only boundary left between them"
        );
    }
    if warden.config().llm_base_url.is_none() {
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

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Before the subscriber, so a typo costs a usage message rather than a log
    // line about a warden that is not starting.
    let Some(command) = command(&args) else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(EXIT_USAGE));
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SQUELCH_WARDEN_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match command {
        Command::Serve => {
            serve().await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Roll { dry_run } => roll(dry_run).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_serves_and_roll_rolls() {
        assert_eq!(command(&argv(&[])), Some(Command::Serve));
        assert_eq!(
            command(&argv(&["roll"])),
            Some(Command::Roll { dry_run: false })
        );
        assert_eq!(
            command(&argv(&["roll", "--dry-run"])),
            Some(Command::Roll { dry_run: true })
        );
    }

    /// Each of these asks for something this binary does not do, and answering
    /// any of them by serving would start a warden nobody asked for. The
    /// refusal is what makes the CronJob's argument list a contract.
    #[test]
    fn anything_else_is_a_usage_error() {
        for args in [
            vec!["--dry-run"],
            vec!["Roll"],
            vec!["roll", "--dryrun"],
            vec!["roll", "--dry-run", "extra"],
            vec!["roll", "alice"],
            vec!["serve"],
            vec!["--help"],
            vec![""],
        ] {
            assert_eq!(command(&argv(&args)), None, "{args:?} must not be accepted");
        }
    }

    #[test]
    fn a_converged_fleet_including_an_empty_one_exits_zero() {
        assert_eq!(verdict(&Rolled::default()), EXIT_CONVERGED);
        assert_eq!(
            verdict(&Rolled {
                checked: 3,
                rolled: vec!["alice".into()],
                current: 2,
                skipped_inactive: vec!["bob".into()],
                ..Rolled::default()
            }),
            EXIT_CONVERGED
        );
    }

    #[test]
    fn a_skip_is_not_a_failure_and_a_halt_outranks_both() {
        let skipped = Rolled {
            checked: 2,
            current: 1,
            skipped_foreign: vec!["alice".into()],
            ..Rolled::default()
        };
        assert_eq!(verdict(&skipped), EXIT_SKIPPED);

        let halted = Rolled {
            halted_on: Some("bob".into()),
            ..skipped
        };
        assert_eq!(verdict(&halted), EXIT_HALTED);
    }

    #[test]
    fn the_summary_names_every_tenant_a_person_has_to_act_on() {
        let out = summarize(
            &Rolled {
                checked: 4,
                rolled: vec!["alice".into()],
                current: 1,
                skipped_foreign: vec!["bob".into()],
                skipped_inactive: vec!["carol".into()],
                halted_on: Some("dave".into()),
            },
            false,
        );
        for expected in ["alice", "bob", "carol", "dave", "4 checked", "HALTED"] {
            assert!(out.contains(expected), "{expected} missing from:\n{out}");
        }
        assert!(!out.contains("dry run"));
    }

    #[test]
    fn a_dry_run_says_so_and_says_it_in_the_conditional() {
        let out = summarize(
            &Rolled {
                checked: 1,
                rolled: vec!["alice".into()],
                ..Rolled::default()
            },
            true,
        );
        assert!(out.contains("dry run"), "{out}");
        assert!(out.contains("would roll"), "{out}");
    }
}
