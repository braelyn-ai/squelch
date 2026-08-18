//! The warden binary. One image, two jobs, chosen by the first argument.
//!
//! With no arguments it SERVES: validate config from the environment, refuse to
//! start on any bad value, connect to the cluster with the pod's own
//! ServiceAccount, and answer the control plane's calls. That is what the
//! Deployment in `deploy/hosted/20-warden.yaml` runs.
//!
//! `roll` reads the whole fleet instead and puts ONE tenant that has fallen
//! behind today's render back onto it; then the process exits with a code that
//! says whether the fleet converged, and how far off it is. One per run, so the
//! gap between ticks is a real daemon serving real mail before the next mailbox
//! is touched - see [`squelch_warden::Warden::roll`]. That is what the CronJob
//! in `deploy/hosted/90-warden-roller.yaml` runs, on the same image, the same
//! ServiceAccount and the same environment as the serving pod - one ConfigMap
//! (`deploy/hosted/15-warden-config.yaml`) behind both pods' `envFrom`, because
//! both paths render tenants from these values and two renders that disagree
//! take turns rewriting the same Deployments forever.
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
  squelch-warden roll              read the fleet, converge one tenant onto today's render
  squelch-warden roll --dry-run    report what a roll would change, and change nothing

`roll` rolls AT MOST ONE tenant per run: the next tick takes the next one, so a
fleet with N tenants behind needs N runs. It exits 0 converged (nothing to do
counts), 1 not converged (it halted, it never started, a tenant is down or
cannot be rendered, or a dry run found work), 2 nothing left it can fix on its
own (foreign drift, an unreadable label), 3 the fleet is behind and the run is
working through it.
";

// `roll`'s exit status is the CronJob's entire interface to a run: the pod's
// log says what happened, and this says what to do about it.

/// The fleet is on today's render and every tenant is serving it. A run with
/// nothing to do is this; a run that would have had something to do is not.
const EXIT_CONVERGED: u8 = 0;
/// The fleet is NOT converged, in any of the five ways that can be true: the
/// roll halted on the tenant it took, it stopped on a casualty of an earlier
/// run, it never started at all (a refused config, an API server it could not
/// reach), it found a tenant with no workload and no cancellation on record, or
/// it found one whose sealed credential is gone and which therefore cannot be
/// rendered at all. A dry run that would roll anything is here too - it has just
/// reported that the fleet is behind, and reporting that as converged is the
/// one answer that would make `--dry-run` worse than useless.
///
/// All of them want the same thing, which is a person reading the log before
/// anything else is applied, so all of them are one code.
const EXIT_NOT_CONVERGED: u8 = 1;
/// Everything this run could converge did, and at least one tenant is left that
/// this timer will NEVER converge, however many times it runs: another field
/// manager owns part of its Deployment (see [`squelch_warden::Warden::roll`]
/// for why a timer must not repair one), or its identity Secret carries a label
/// that does not validate, so no run can even address it. Both want a person,
/// and neither wants one tonight.
const EXIT_SKIPPED: u8 = 2;
/// The fleet is not on today's render, this run left it no worse, and nobody
/// should do anything. Working exactly as designed: see
/// [`squelch_warden::Warden::roll`] for why one tenant per run is the safety
/// model, and expect one of these per remaining tenant after every image bump.
///
/// Ordinarily that means it rolled a tenant and more are queued behind it. It
/// also covers the two ways a run spends its one attempt without converging
/// anything - the tenant it took turned out to be somebody else's to fix
/// (`recreate_refused`), or its account was cancelled mid-flight - because in
/// both the queue behind it is real, no person is wanted, and the next tick
/// takes the next tenant. What it never covers is a fleet with nothing left to
/// do: [`squelch_warden::Rolled::remaining`] is zero then, and this code is not
/// reachable.
///
/// A foreign skip or an unreadable label raised during a run like this is NOT
/// lost, only deferred: those tenants are still there on the tick that drains
/// the queue, and that run exits [`EXIT_SKIPPED`]. Pacing outranks a skip on
/// purpose - both say "not tonight", and only one of them is finished.
///
/// Its own code rather than folded into either neighbour, because it is the one
/// outcome that is both "the fleet is not on today's render" and "no human
/// should do anything". Reporting it as 0 would make `roll` unable to say when
/// a bump has finished landing; reporting it as 1 would spend the alarm that
/// [`EXIT_NOT_CONVERGED`] is for on the normal case.
///
/// The Job is still marked Failed - Kubernetes knows zero and non-zero and
/// nothing else - so a fleet mid-roll leaves failed Jobs in its history by
/// design. `deploy/hosted/PRODUCTION.md` says what that looks like and how an
/// alert should tell it from the real thing.
const EXIT_PROGRESSING: u8 = 3;
/// The argument list was none of the three this binary accepts (`EX_USAGE`).
/// Deliberately outside the 0-3 range: a mistyped CronJob argument must not
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
/// constants.
///
/// Ranked by what a person has to do and how soon, most urgent first: something
/// is wrong (1), the machine is mid-job and will carry on (3), the machine is
/// done and something is left that it cannot do (2), nothing to do (0). A fleet
/// that is not on today's render outranks a skip, for the reason it always did:
/// it is the more urgent of the two facts, and [`EXIT_PROGRESSING`] inherits
/// that rank for the same reason.
///
/// `dry_run` is an input to the verdict and not a detail of the report, because
/// the same summary means opposite things in the two modes. A real run that
/// rolled a tenant converged it; a dry run that would have rolled three tenants
/// converged nothing and has just said so. Without this, the run the CronJob
/// comments recommend before a pin bump - the one whose entire job is to warn -
/// would exit exactly like a fleet that needs nothing. A dry run never sets
/// [`squelch_warden::Rolled::remaining`], so it can never be PROGRESSING: it
/// made no progress, which is the whole idea.
fn verdict(rolled: &Rolled, dry_run: bool) -> u8 {
    let would_roll = dry_run && !rolled.rolled.is_empty();
    if rolled.halted_on.is_some()
        || !rolled.stranded.is_empty()
        || !rolled.unrenderable.is_empty()
        || would_roll
    {
        EXIT_NOT_CONVERGED
    } else if rolled.remaining > 0 {
        EXIT_PROGRESSING
    } else if !rolled.skipped_foreign.is_empty() || rolled.unreadable > 0 {
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
    // The pacing, said out loud. A run that converged a tenant and left nine is
    // doing its job, and the number is the only thing that says how many more
    // ticks the bump has left in it.
    if rolled.remaining > 0 {
        // A halted run had already marked these and then stopped before taking
        // any of them, so the ordinary promise does not hold: the next tick
        // re-reads the fleet and meets whatever stopped this one. Saying "the
        // next at the next tick" there would be the summary telling an operator
        // to wait for something that is not going to happen.
        let tail = if rolled.halted_on.is_some() {
            "and this run stopped before it took any of them"
        } else {
            "the next at the next tick"
        };
        out.push_str(&format!(
            "  still behind, one per run: {} more, {tail}\n",
            rolled.remaining
        ));
    }
    if !rolled.skipped_foreign.is_empty() {
        out.push_str(&format!(
            "  needs a person (another field manager owns part of the Deployment): {}\n",
            rolled.skipped_foreign.join(", ")
        ));
    }
    // A COUNT and never the names: the name is the string that failed
    // validation, and this text goes to a terminal.
    if rolled.unreadable > 0 {
        out.push_str(&format!(
            "  needs a person (identity Secrets whose label does not validate, so no run can \
             ever address them): {}\n",
            rolled.unreadable
        ));
    }
    if !rolled.skipped_inactive.is_empty() {
        out.push_str(&format!(
            "  no workload to converge (pending or cancelled): {}\n",
            rolled.skipped_inactive.join(", ")
        ));
    }
    if !rolled.stranded.is_empty() {
        out.push_str(&format!(
            "  DOWN (no workload, and nothing recorded a cancellation; a job that did not \
             finish): {}\n",
            rolled.stranded.join(", ")
        ));
    }
    // Named, and named as permanent: this is the one per-tenant read failure a
    // later run does not retry its way out of, and the remedy is a person
    // putting the Secret back rather than anything the roller can do.
    if !rolled.unrenderable.is_empty() {
        out.push_str(&format!(
            "  needs a person (a workload whose sealed credential Secret is gone, so no run \
             can render it): {}\n",
            rolled.unrenderable.join(", ")
        ));
    }
    // The casualty line replaces the halt line rather than joining it: they name
    // the same tenant, and the two stops want different things done about them.
    // A halt is "this tenant did not come back"; a casualty is "this tenant
    // already has what you were about to hand everybody else".
    if let Some(label) = &rolled.casualty {
        out.push_str(&format!(
            "  HALTED before applying anything: {label} carries today's render and is not \
             serving it\n"
        ));
    } else if let Some(label) = &rolled.halted_on {
        out.push_str(&format!(
            "  HALTED on {label}; nothing else was touched, and it is back on the queue\n"
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
/// One ConfigMap keeps the two environments identical; this keeps the code that
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
    Ok(ExitCode::from(verdict(&rolled, dry_run)))
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
    // A warning and not a refusal, because the operator has a second way out:
    // a `ready_timeout` wide enough to sit through a cold model download. What
    // is not survivable is doing neither by accident, and the failure looks like
    // a healthy tenant timing out on its own signup.
    if warden.config().http_readiness && warden.config().model_pvc.is_none() {
        tracing::warn!(
            ready_timeout_secs = warden.config().ready_timeout.as_secs(),
            "SQUELCH_WARDEN_HTTP_READINESS is on with no SQUELCH_WARDEN_MODEL_PVC: /healthz waits out the first-run embedding-model download, so a new tenant is NotReady for the whole of it - longer than SQUELCH_WARDEN_READY_TIMEOUT_SECS, which fails the signup and makes the next fleet roll read that tenant as a casualty"
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
        assert_eq!(verdict(&Rolled::default(), false), EXIT_CONVERGED);
        assert_eq!(
            verdict(
                &Rolled {
                    checked: 3,
                    rolled: vec!["alice".into()],
                    current: 2,
                    skipped_inactive: vec!["bob".into()],
                    ..Rolled::default()
                },
                false
            ),
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
        assert_eq!(verdict(&skipped, false), EXIT_SKIPPED);

        let halted = Rolled {
            halted_on: Some("bob".into()),
            ..skipped
        };
        assert_eq!(verdict(&halted, false), EXIT_NOT_CONVERGED);
    }

    /// A dry run's whole job is to warn before a pin bump. One that would roll
    /// the entire fleet must not exit like a fleet with nothing to do, because
    /// the CronJob comments tell an operator to run it and read the code.
    #[test]
    fn a_dry_run_with_work_to_do_is_not_a_converged_fleet() {
        let would_roll = Rolled {
            checked: 2,
            rolled: vec!["alice".into()],
            current: 1,
            ..Rolled::default()
        };
        assert_eq!(verdict(&would_roll, true), EXIT_NOT_CONVERGED);
        // The same summary from a real run is the opposite verdict: those
        // tenants were rolled, and the fleet is on today's render.
        assert_eq!(verdict(&would_roll, false), EXIT_CONVERGED);

        // A dry run over a converged fleet still exits 0, which is the answer
        // that makes the flag worth running.
        let nothing_to_do = Rolled {
            checked: 2,
            current: 2,
            ..Rolled::default()
        };
        assert_eq!(verdict(&nothing_to_do, true), EXIT_CONVERGED);
    }

    /// A tenant with no workload behind a live Service is DOWN, and the roll
    /// deliberately does not repair it. Exiting green on that would leave a
    /// dark mailbox reported by nothing at all, while a cosmetic foreign skip
    /// raised a code.
    #[test]
    fn a_stranded_tenant_outranks_a_foreign_skip() {
        let stranded = Rolled {
            checked: 2,
            current: 1,
            stranded: vec!["alice".into()],
            skipped_foreign: vec!["bob".into()],
            ..Rolled::default()
        };
        assert_eq!(verdict(&stranded, false), EXIT_NOT_CONVERGED);

        // A cancelled account and a pending signup are not that: nothing is
        // down, and nothing is expected to be running.
        let inactive = Rolled {
            checked: 2,
            current: 1,
            skipped_inactive: vec!["alice".into()],
            ..Rolled::default()
        };
        assert_eq!(verdict(&inactive, false), EXIT_CONVERGED);
    }

    /// A workload whose sealed credential is gone is a mailbox no run will ever
    /// put on today's render. The roller deliberately does NOT halt on it - one
    /// broken tenant must not park the fleet behind it - so the exit code is the
    /// only thing left that can say a person is needed.
    #[test]
    fn an_unrenderable_tenant_cannot_exit_converged() {
        let unrenderable = Rolled {
            checked: 2,
            current: 1,
            unrenderable: vec!["bob".into()],
            ..Rolled::default()
        };
        assert_eq!(verdict(&unrenderable, false), EXIT_NOT_CONVERGED);
        assert_eq!(verdict(&unrenderable, true), EXIT_NOT_CONVERGED);

        // Including on the tick that rolled somebody else, which is the run
        // this actually happens on: the walk goes past bob and converges carol.
        let rolled_around_it = Rolled {
            rolled: vec!["carol".into()],
            ..unrenderable.clone()
        };
        assert_eq!(verdict(&rolled_around_it, false), EXIT_NOT_CONVERGED);
    }

    /// A run that stopped in the READ pass names what it had already queued.
    /// The tenants are not rolled and not lost, and the summary says both.
    #[test]
    fn a_halt_still_reports_the_queue_it_had_marked() {
        let halted = Rolled {
            checked: 3,
            remaining: 2,
            halted_on: Some("carol".into()),
            casualty: Some("carol".into()),
            ..Rolled::default()
        };
        assert_eq!(verdict(&halted, false), EXIT_NOT_CONVERGED);

        let out = summarize(&halted, false);
        assert!(out.contains("2 more"), "{out}");
        // Not the ordinary promise: the next tick meets the same casualty.
        assert!(!out.contains("the next at the next tick"), "{out}");
        assert!(out.contains("HALTED before applying anything"), "{out}");
    }

    /// A run that rolled a tenant and has more queued is PROGRESSING: not
    /// converged, and not a problem either. Getting this wrong in either
    /// direction is what the code exists to prevent - green would make `roll`
    /// unable to say when a bump has landed, and [`EXIT_NOT_CONVERGED`] would
    /// spend the alarm on every normal tick of every normal image bump.
    #[test]
    fn a_paced_run_with_more_to_do_is_neither_converged_nor_a_problem() {
        let mid_roll = Rolled {
            checked: 3,
            rolled: vec!["alice".into()],
            remaining: 2,
            ..Rolled::default()
        };
        assert_eq!(verdict(&mid_roll, false), EXIT_PROGRESSING);

        // The last tick of that same bump: nothing left behind it.
        let finished = Rolled {
            checked: 3,
            rolled: vec!["carol".into()],
            current: 2,
            ..Rolled::default()
        };
        assert_eq!(verdict(&finished, false), EXIT_CONVERGED);

        // Something being wrong still outranks the pacing. A run that rolled a
        // tenant, has more queued, and found a mailbox down wants a person now.
        let mid_roll_with_a_casualty = Rolled {
            stranded: vec!["erin".into()],
            ..mid_roll.clone()
        };
        assert_eq!(
            verdict(&mid_roll_with_a_casualty, false),
            EXIT_NOT_CONVERGED
        );

        // And the pacing outranks a skip, the same way not-converged does: the
        // foreign tenant is not going anywhere, and the run is still working.
        let mid_roll_with_a_skip = Rolled {
            skipped_foreign: vec!["bob".into()],
            ..mid_roll.clone()
        };
        assert_eq!(verdict(&mid_roll_with_a_skip, false), EXIT_PROGRESSING);
    }

    /// A tenant this warden can see and can never address is a fleet that is
    /// not on today's render, permanently. The failure mode being fixed is
    /// SILENCE, so the one thing it may not do is exit green.
    #[test]
    fn an_unreadable_label_cannot_exit_converged() {
        let unreadable = Rolled {
            checked: 1,
            current: 1,
            unreadable: 1,
            ..Rolled::default()
        };
        assert_eq!(verdict(&unreadable, false), EXIT_SKIPPED);

        // Including on a dry run, which is where an operator would go looking
        // for exactly this before a bump.
        assert_eq!(verdict(&unreadable, true), EXIT_SKIPPED);
    }

    #[test]
    fn the_summary_names_every_tenant_a_person_has_to_act_on() {
        let out = summarize(
            &Rolled {
                checked: 5,
                rolled: vec!["alice".into()],
                remaining: 2,
                current: 1,
                skipped_foreign: vec!["bob".into()],
                skipped_inactive: vec!["carol".into()],
                stranded: vec!["erin".into()],
                unrenderable: vec!["frank".into()],
                unreadable: 3,
                halted_on: Some("dave".into()),
                casualty: None,
            },
            false,
        );
        for expected in [
            "alice",
            "bob",
            "carol",
            "dave",
            "erin",
            "frank",
            "5 checked",
            "2 more",
            "HALTED",
        ] {
            assert!(out.contains(expected), "{expected} missing from:\n{out}");
        }
        assert!(out.contains("DOWN"), "{out}");
        assert!(out.contains("does not validate"), "{out}");
        assert!(out.contains("sealed credential Secret is gone"), "{out}");
        assert!(!out.contains("dry run"));
    }

    /// The two stops read differently on purpose: one tenant did not come back,
    /// versus one tenant already has the render nobody else should be given.
    #[test]
    fn a_casualty_says_that_nothing_was_applied() {
        let out = summarize(
            &Rolled {
                checked: 1,
                halted_on: Some("alice".into()),
                casualty: Some("alice".into()),
                ..Rolled::default()
            },
            false,
        );
        assert!(out.contains("HALTED before applying anything"), "{out}");
        assert!(out.contains("alice"), "{out}");
        // One line about the stop, not two about the same tenant.
        assert_eq!(out.matches("HALTED").count(), 1, "{out}");
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
