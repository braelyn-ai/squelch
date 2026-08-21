//! The daemon's own health, in Prometheus text format.
//!
//! Hand-rolled exposition rather than a client library: the surface is a dozen
//! families and one GET, and a scrape endpoint is not worth a dependency tree
//! that pulls its own HTTP stack into the daemon.
//!
//! Two halves. [`SyncMetrics`] is the in-process registry the sync engine
//! increments as it works — atomics only, no locks on any hot path, and it is
//! LOST ON RESTART by design. Everything else ([`StoreSnapshot`]) is derived
//! from the store at scrape time, which is what makes the LLM counters survive
//! a restart: they are read from the persisted usage ledger, not from a
//! process-lifetime tally.
//!
//! NOTHING HERE IS PER-MESSAGE. No sender, no subject, no thread id ever
//! becomes a label: an unauthenticated scrape must not be able to learn who
//! writes to this mailbox, and label cardinality is a memory bill in the
//! scraper besides.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use chrono::Utc;

use crate::config::Config;
use crate::store::Stage2UsageDay;
use crate::types::StoreStats;

/// `days` on the ledger readers is a per-category ROW limit, and the LLM
/// families are COUNTERS: a trailing window would drop old day-rows and make
/// them run backwards, which a scraper reads as a counter reset (and then as a
/// full re-ingest of the total). So the ledger is always read whole.
pub const LEDGER_ALL_DAYS: u32 = u32::MAX;

/// How a Gmail call failed, as seen where the status is still typed. Kept
/// coarse on purpose: these four are the ones with DIFFERENT operator
/// responses (re-auth, wait, file a bug, check the network).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmailErrorKind {
    /// 401, or a credential that could not be refreshed at all (`invalid_grant`
    /// means the refresh token is dead and only `squelchd auth` fixes it).
    Auth,
    /// 429, or a 403 whose body names a rate/quota reason.
    Quota,
    /// Any other non-2xx.
    Http,
    /// The request never got a status: DNS, TLS, connect, timeout.
    Network,
}

impl GmailErrorKind {
    fn label(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::Http => "http",
            Self::Network => "network",
        }
    }
}

/// A carrier the shipment poller can call, as a metrics label. A CLOSED set of
/// four, which is the whole cardinality bound on the carrier axis: the label is
/// resolved from this enum, never from the `carrier` string on a shipment row,
/// so nothing a mail body wrote can become a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollCarrier {
    Ups,
    Fedex,
    Usps,
    Dhl,
}

impl PollCarrier {
    /// Every carrier, in exposition order. This array's ORDER IS THE INDEX used
    /// by [`SyncMetrics::record_carrier_poll`], so it must stay in step with the
    /// variant declaration order.
    const ALL: [Self; 4] = [Self::Ups, Self::Fedex, Self::Usps, Self::Dhl];

    /// The registry slug onto the label, or `None` for anything else — a carrier
    /// with no client is a carrier with no polls to count.
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "ups" => Some(Self::Ups),
            "fedex" => Some(Self::Fedex),
            "usps" => Some(Self::Usps),
            "dhl" => Some(Self::Dhl),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ups => "ups",
            Self::Fedex => "fedex",
            Self::Usps => "usps",
            Self::Dhl => "dhl",
        }
    }
}

/// How one carrier-API poll ended. The five are exactly [`crate::carriers`]'s
/// four error classes plus success, because those are the five the poller
/// already treats differently — an operator reading `rate_limited` climbing
/// knows to slow the cadence, and `auth` climbing knows to re-key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierPollOutcome {
    Ok,
    NotFound,
    RateLimited,
    Auth,
    Transient,
}

impl CarrierPollOutcome {
    /// Exposition order; the index into a carrier's outcome row.
    const ALL: [Self; 5] = [
        Self::Ok,
        Self::NotFound,
        Self::RateLimited,
        Self::Auth,
        Self::Transient,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
            Self::Auth => "auth",
            Self::Transient => "transient",
        }
    }
}

/// What one row's trip through the Stage-1 refine pass produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage1Verdict {
    /// A model verdict landed on the row.
    Ok,
    /// Refusal or permanent failure: the ingest heuristic's seed stands.
    Fallback,
    /// Older than the pass's cutoff, so marked processed without a call.
    StaleSkipped,
}

/// What one row's trip through the RE-EVALUATION pass produced.
///
/// Its own axis rather than a share of Stage-1's, even though a revisit is
/// literally a Stage-1 call: `stage1_ok` is the answer to "how much NEW mail did
/// we classify today", and folding re-evaluations into it makes that number
/// unreadable exactly when the staleness sweep is busiest — which is the moment
/// someone would be looking. The spend ledger already separates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisitVerdict {
    /// A re-evaluated verdict landed on the row.
    Ok,
    /// Refusal or permanent failure: the prior verdict stands.
    Fallback,
}

/// What one row's trip through the Stage-2 escalation pass produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage2Verdict {
    Ok,
    Refused,
    /// Permanent failure; the row is marked processed so it cannot loop.
    Failed,
    /// Retryable class exhausted or a transport error; the row stays queued.
    Retryable,
    StaleSkipped,
}

/// The in-process registry. One per daemon, shared (via [`Arc`]) by the sync
/// engine that writes it and the metrics door that reads it.
///
/// Every counter is [`Ordering::Relaxed`]: these are independent tallies, no
/// reader draws a conclusion from the order two of them were bumped in, and a
/// scrape landing between two increments is a scrape, not a race.
#[derive(Debug, Default)]
pub struct SyncMetrics {
    /// Unix seconds of the last successful sync tick. 0 = never — see
    /// [`render`] for why that value is deliberate rather than absent.
    sync_last_success_unix: AtomicI64,
    sync_runs_ok: AtomicU64,
    sync_runs_err: AtomicU64,
    /// A GAUGE kept in an atomic: it is set to 0 on success and stepped on
    /// failure, so it answers "is this broken NOW", not "how often".
    sync_consecutive_failures: AtomicU64,

    gmail_auth: AtomicU64,
    gmail_quota: AtomicU64,
    gmail_http: AtomicU64,
    gmail_network: AtomicU64,

    stage1_ok: AtomicU64,
    stage1_fallback: AtomicU64,
    stage1_stale_skipped: AtomicU64,

    revisit_ok: AtomicU64,
    revisit_fallback: AtomicU64,

    stage2_ok: AtomicU64,
    stage2_refused: AtomicU64,
    stage2_failed: AtomicU64,
    stage2_retryable: AtomicU64,
    stage2_stale_skipped: AtomicU64,

    /// Carrier polls as `[carrier][outcome]`, both axes closed enums — 20
    /// series, fixed forever, no matter how many shipments or tracking numbers
    /// pass through. A TRACKING NUMBER IS NEVER A LABEL: it names a parcel and
    /// its recipient to anyone who can reach the scrape.
    carrier_poll: [[AtomicU64; CarrierPollOutcome::ALL.len()]; PollCarrier::ALL.len()],
    /// Polls that moved a shipment's status, which is the only reason the
    /// feature exists: a poller that answers 200 all day and advances nothing is
    /// broken in a way the poll counter alone cannot show.
    shipments_advanced_by_poll: AtomicU64,
    /// Unix seconds of the last poll a carrier answered. 0 = never, for the same
    /// reason as the sync stamp below.
    carrier_poll_last_success_unix: AtomicI64,
}

impl SyncMetrics {
    /// A fresh registry, already shared: every call site wants a handle, none
    /// wants ownership.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A sync run returned cleanly: one more OK run, the freshness stamp moves,
    /// and the failure streak is broken.
    pub fn record_sync_ok(&self) {
        self.sync_runs_ok.fetch_add(1, Ordering::Relaxed);
        self.sync_consecutive_failures.store(0, Ordering::Relaxed);
        self.stamp_sync_success();
    }

    /// A sync run bubbled an error up to the retry loop.
    pub fn record_sync_error(&self) {
        self.sync_runs_err.fetch_add(1, Ordering::Relaxed);
        self.sync_consecutive_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Move the freshness stamp to now WITHOUT touching the run counters. The
    /// poll loop calls this per successful tick: a healthy daemon stays inside
    /// one `run_once` for weeks, so the run-level stamp alone would look like a
    /// mailbox that stopped syncing at boot.
    pub fn stamp_sync_success(&self) {
        self.sync_last_success_unix
            .store(Utc::now().timestamp(), Ordering::Relaxed);
    }

    pub fn record_gmail_error(&self, kind: GmailErrorKind) {
        let slot = match kind {
            GmailErrorKind::Auth => &self.gmail_auth,
            GmailErrorKind::Quota => &self.gmail_quota,
            GmailErrorKind::Http => &self.gmail_http,
            GmailErrorKind::Network => &self.gmail_network,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_stage1(&self, verdict: Stage1Verdict) {
        let slot = match verdict {
            Stage1Verdict::Ok => &self.stage1_ok,
            Stage1Verdict::Fallback => &self.stage1_fallback,
            Stage1Verdict::StaleSkipped => &self.stage1_stale_skipped,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_revisit(&self, verdict: RevisitVerdict) {
        let slot = match verdict {
            RevisitVerdict::Ok => &self.revisit_ok,
            RevisitVerdict::Fallback => &self.revisit_fallback,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_stage2(&self, verdict: Stage2Verdict) {
        let slot = match verdict {
            Stage2Verdict::Ok => &self.stage2_ok,
            Stage2Verdict::Refused => &self.stage2_refused,
            Stage2Verdict::Failed => &self.stage2_failed,
            Stage2Verdict::Retryable => &self.stage2_retryable,
            Stage2Verdict::StaleSkipped => &self.stage2_stale_skipped,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one carrier-API poll. A successful one ALSO moves the freshness
    /// stamp, so the two can never disagree about whether a carrier has ever
    /// answered.
    pub fn record_carrier_poll(&self, carrier: PollCarrier, outcome: CarrierPollOutcome) {
        // Both indices come from closed enums whose `ALL` arrays size the table,
        // so this cannot be out of bounds.
        self.carrier_poll[carrier as usize][outcome as usize].fetch_add(1, Ordering::Relaxed);
        if outcome == CarrierPollOutcome::Ok {
            self.carrier_poll_last_success_unix
                .store(Utc::now().timestamp(), Ordering::Relaxed);
        }
    }

    /// A poll advanced a shipment's status (`apply_carrier_track` said so).
    pub fn record_shipment_advanced(&self) {
        self.shipments_advanced_by_poll
            .fetch_add(1, Ordering::Relaxed);
    }

    fn get(&self, slot: &AtomicU64) -> f64 {
        slot.load(Ordering::Relaxed) as f64
    }
}

// --- text exposition ---------------------------------------------------------

/// Prometheus metric type, as it appears on the `# TYPE` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
}

impl MetricKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// Builder for the text exposition format (v0.0.4): per family a `# HELP` and a
/// `# TYPE` line, then its samples, then a trailing newline the parser needs on
/// the last line like every other.
#[derive(Debug, Default)]
pub struct Exposition {
    out: String,
}

impl Exposition {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a family. Must precede that family's samples, and a family must not
    /// be reopened — a scraper rejects an exposition whose samples for one name
    /// are interleaved with another's.
    pub fn family(&mut self, name: &str, kind: MetricKind, help: &str) {
        self.out.push_str("# HELP ");
        self.out.push_str(name);
        self.out.push(' ');
        // HELP escapes backslash and newline only; a quote is literal there.
        for c in help.chars() {
            match c {
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                c => self.out.push(c),
            }
        }
        self.out.push_str("\n# TYPE ");
        self.out.push_str(name);
        self.out.push(' ');
        self.out.push_str(kind.as_str());
        self.out.push('\n');
    }

    /// One sample line. `labels` are written in the order given and their values
    /// escaped; an empty slice writes the bare metric name.
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                self.out.push_str(k);
                self.out.push_str("=\"");
                push_label_value(&mut self.out, v);
                self.out.push('"');
            }
            self.out.push('}');
        }
        self.out.push(' ');
        self.out.push_str(&fmt_value(value));
        self.out.push('\n');
    }

    /// The common case: a family with exactly one unlabelled sample.
    pub fn scalar(&mut self, name: &str, kind: MetricKind, help: &str, value: f64) {
        self.family(name, kind, help);
        self.sample(name, &[], value);
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// Escape a label value: backslash, quote and newline are the three characters
/// the format cannot carry raw. Everything reaching here is ours (tier names,
/// ledger categories, a version string), but a category is written by whoever
/// adds the next extractor, so it is escaped rather than trusted.
fn push_label_value(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Format a sample value. Whole numbers print without a decimal point so the
/// exposition reads like the integers these mostly are; everything else gets
/// Rust's shortest round-tripping form. Non-finite values use the format's own
/// spellings rather than Rust's `inf`/`NaN`.
fn fmt_value(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    // 1e15 is where f64 stops representing consecutive integers exactly; past
    // it the default form is the honest one.
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    format!("{v}")
}

// --- store-derived block -----------------------------------------------------

/// One ledger category's totals over all recorded days.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCategoryUsage {
    /// The ledger's own label (`stage1`, `stage2`, `extract_banking`, ...).
    pub category: String,
    pub calls: u64,
    /// The UNCACHED prompt remainder — cache writes/reads are separate below.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

/// Everything the db-derived half of a scrape needs, gathered by the caller in
/// ONE trip to the store so the endpoint holds the store mutex once.
#[derive(Debug, Clone)]
pub struct StoreSnapshot {
    pub stats: StoreStats,
    /// Every category the ledger has ever seen. NEVER a hand-written list: each
    /// extractor writes its own category, so naming them here would silently
    /// stop reporting the next one added.
    pub llm: Vec<LlmCategoryUsage>,
    /// Estimated spend behind `llm`, from [`estimate_cost_usd`].
    pub llm_cost_usd: f64,
    pub db_bytes: u64,
    /// The `-wal` sidecar; 0 when checkpointed away or absent.
    pub wal_bytes: u64,
    /// Unrevoked, non-console device tokens
    /// ([`crate::store::SqliteStore::count_client_devices`]).
    ///
    /// A GAUGE, and NOT the activation signal: it falls again when somebody
    /// revokes a phone, while activation is a fact about the past that nothing
    /// undoes. The stamp is read out of band by `squelchd token first-paired`;
    /// this is here so an operator can see the fleet's paired devices on a
    /// dashboard, and nothing reads it back.
    pub devices_paired: u64,
}

/// Roll the per-day ledger rows up into one total per category.
pub fn usage_from_ledger(rows: Vec<(String, Vec<Stage2UsageDay>)>) -> Vec<LlmCategoryUsage> {
    rows.into_iter()
        .map(|(category, days)| {
            let mut usage = LlmCategoryUsage {
                category,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            };
            for d in days {
                usage.calls += d.calls;
                usage.input_tokens += d.input_tokens;
                usage.output_tokens += d.output_tokens;
                usage.cache_creation_tokens += d.cache_creation_tokens;
                usage.cache_read_tokens += d.cache_read_tokens;
            }
            usage
        })
        .collect()
}

/// Estimated spend for a ledger rollup, costed exactly as `/client/usage` does:
/// only `stage2` runs the capable model and its prices; Stage-1 and EVERY
/// extractor share the stage-1 small model's config, so they cost at stage-1
/// rates. Switching a model without updating its `price_*_per_mtok` makes this
/// drift, there as here.
pub fn estimate_cost_usd(usage: &[LlmCategoryUsage], config: &Config) -> f64 {
    usage
        .iter()
        .map(|u| {
            let (price_in, price_out) = if u.category == "stage2" {
                (
                    config.stage2.price_in_per_mtok,
                    config.stage2.price_out_per_mtok,
                )
            } else {
                (
                    config.stage1.price_in_per_mtok,
                    config.stage1.price_out_per_mtok,
                )
            };
            (u.input_tokens as f64 / 1_000_000.0) * price_in
                + (u.output_tokens as f64 / 1_000_000.0) * price_out
                + (u.cache_creation_tokens as f64 / 1_000_000.0)
                    * price_in
                    * crate::config::CACHE_WRITE_INPUT_MULT
                + (u.cache_read_tokens as f64 / 1_000_000.0)
                    * price_in
                    * crate::config::CACHE_READ_INPUT_MULT
        })
        .sum()
}

/// Size of the sqlite db and its `-wal` sidecar. A file that is absent or
/// unreadable reports 0: a scrape must not fail because sqlite checkpointed the
/// wal away between the metadata call and this one.
pub fn db_file_sizes(db_path: &std::path::Path) -> (u64, u64) {
    let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut wal = db_path.as_os_str().to_os_string();
    wal.push("-wal");
    (size(db_path), size(std::path::Path::new(&wal)))
}

/// The whole exposition: the in-process registry always, plus the db-derived
/// families when the caller managed to read the store.
pub fn render(metrics: &SyncMetrics, db: Option<&StoreSnapshot>) -> String {
    let mut e = Exposition::new();

    // The workspace pins ONE version across every crate, so core's is the
    // daemon's.
    e.family(
        "squelchd_build_info",
        MetricKind::Gauge,
        "Build information for the running daemon; the value is always 1.",
    );
    e.sample(
        "squelchd_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );

    // 0 UNTIL THE FIRST SUCCESS, not absent: `time() - metric` on a daemon that
    // has never synced then reads as ~56 years stale and fires, which is
    // exactly right. An absent series would instead make the alert silently
    // evaluate to nothing for the one tenant most likely to be broken.
    e.scalar(
        "squelchd_sync_last_success_timestamp_seconds",
        MetricKind::Gauge,
        "Unix timestamp of the last successful sync tick; 0 if this daemon has never synced.",
        metrics.sync_last_success_unix.load(Ordering::Relaxed) as f64,
    );

    e.family(
        "squelchd_sync_runs_total",
        MetricKind::Counter,
        "Sync runs by outcome, counted where the run loop retries.",
    );
    e.sample(
        "squelchd_sync_runs_total",
        &[("outcome", "ok")],
        metrics.get(&metrics.sync_runs_ok),
    );
    e.sample(
        "squelchd_sync_runs_total",
        &[("outcome", "error")],
        metrics.get(&metrics.sync_runs_err),
    );

    e.scalar(
        "squelchd_sync_consecutive_failures",
        MetricKind::Gauge,
        "Sync failures since the last success; 0 while sync is healthy.",
        metrics.get(&metrics.sync_consecutive_failures),
    );

    e.family(
        "squelchd_gmail_api_errors_total",
        MetricKind::Counter,
        "Failed Gmail API calls by kind.",
    );
    for (kind, slot) in [
        (GmailErrorKind::Auth, &metrics.gmail_auth),
        (GmailErrorKind::Quota, &metrics.gmail_quota),
        (GmailErrorKind::Http, &metrics.gmail_http),
        (GmailErrorKind::Network, &metrics.gmail_network),
    ] {
        e.sample(
            "squelchd_gmail_api_errors_total",
            &[("kind", kind.label())],
            metrics.get(slot),
        );
    }

    e.family(
        "squelchd_triage_verdicts_total",
        MetricKind::Counter,
        "Triage rows processed by stage and outcome.",
    );
    for (outcome, slot) in [
        ("ok", &metrics.stage1_ok),
        ("fallback", &metrics.stage1_fallback),
        ("stale_skipped", &metrics.stage1_stale_skipped),
    ] {
        e.sample(
            "squelchd_triage_verdicts_total",
            &[("stage", "stage1"), ("outcome", outcome)],
            metrics.get(slot),
        );
    }
    for (outcome, slot) in [
        ("ok", &metrics.revisit_ok),
        ("fallback", &metrics.revisit_fallback),
    ] {
        e.sample(
            "squelchd_triage_verdicts_total",
            &[("stage", "revisit"), ("outcome", outcome)],
            metrics.get(slot),
        );
    }
    for (outcome, slot) in [
        ("ok", &metrics.stage2_ok),
        ("refused", &metrics.stage2_refused),
        ("failed", &metrics.stage2_failed),
        ("retryable", &metrics.stage2_retryable),
        ("stale_skipped", &metrics.stage2_stale_skipped),
    ] {
        e.sample(
            "squelchd_triage_verdicts_total",
            &[("stage", "stage2"), ("outcome", outcome)],
            metrics.get(slot),
        );
    }

    // ALL 20 series are emitted, including carriers this daemon has no
    // credentials for: an absent series is indistinguishable from a scraper
    // problem, and a flat 0 is what makes `rate(...)` on a carrier that just
    // started failing read correctly from its first sample.
    e.family(
        "squelchd_carrier_poll_total",
        MetricKind::Counter,
        "Carrier tracking-API polls by carrier and outcome.",
    );
    for carrier in PollCarrier::ALL {
        for outcome in CarrierPollOutcome::ALL {
            e.sample(
                "squelchd_carrier_poll_total",
                &[("carrier", carrier.label()), ("outcome", outcome.label())],
                metrics.get(&metrics.carrier_poll[carrier as usize][outcome as usize]),
            );
        }
    }

    e.scalar(
        "squelchd_shipments_advanced_by_poll_total",
        MetricKind::Counter,
        "Shipments whose status a carrier poll moved (as opposed to confirmed).",
        metrics.get(&metrics.shipments_advanced_by_poll),
    );

    // 0 until a carrier has ever answered, for the same reason the sync stamp
    // is: `time() - metric` must fire on a poller that has never worked, not
    // evaluate to nothing.
    e.scalar(
        "squelchd_shipment_poll_last_success_timestamp_seconds",
        MetricKind::Gauge,
        "Unix timestamp of the last carrier poll that returned a track; 0 if none ever has.",
        metrics
            .carrier_poll_last_success_unix
            .load(Ordering::Relaxed) as f64,
    );

    // The db-derived families are OPTIONAL on purpose: see the caller. A scrape
    // that carries the atomics and omits these is a degraded scrape; a 500 is
    // no scrape at all.
    if let Some(db) = db {
        e.family(
            "squelchd_store_messages",
            MetricKind::Gauge,
            "Non-sealed triaged messages by tier.",
        );
        for (tier, count) in &db.stats.tier_counts {
            e.sample("squelchd_store_messages", &[("tier", tier)], *count as f64);
        }
        e.scalar(
            "squelchd_store_messages_sealed",
            MetricKind::Gauge,
            "Sealed messages (metadata only; never counted per tier).",
            db.stats.sealed as f64,
        );

        e.family(
            "squelchd_llm_calls_total",
            MetricKind::Counter,
            "LLM calls by ledger category, from the persisted usage ledger.",
        );
        for u in &db.llm {
            e.sample(
                "squelchd_llm_calls_total",
                &[("category", &u.category)],
                u.calls as f64,
            );
        }
        e.family(
            "squelchd_llm_tokens_total",
            MetricKind::Counter,
            "LLM tokens by ledger category and direction, from the persisted usage ledger.",
        );
        for u in &db.llm {
            e.sample(
                "squelchd_llm_tokens_total",
                &[("category", &u.category), ("direction", "input")],
                u.input_tokens as f64,
            );
            e.sample(
                "squelchd_llm_tokens_total",
                &[("category", &u.category), ("direction", "output")],
                u.output_tokens as f64,
            );
            // `input` above is the uncached remainder only; the cache split gets
            // its own directions so the exported totals cover the whole prompt.
            e.sample(
                "squelchd_llm_tokens_total",
                &[("category", &u.category), ("direction", "cache_write")],
                u.cache_creation_tokens as f64,
            );
            e.sample(
                "squelchd_llm_tokens_total",
                &[("category", &u.category), ("direction", "cache_read")],
                u.cache_read_tokens as f64,
            );
        }
        e.scalar(
            "squelchd_llm_cost_usd_total",
            MetricKind::Counter,
            "Estimated LLM spend in USD: ledger tokens at the configured per-MTok prices.",
            db.llm_cost_usd,
        );

        // UNLABELLED, which is what keeps it inside this module's rule: a
        // per-device series would put a device name in a label, and a scrape is
        // unauthenticated. One integer says how many clients this mailbox has
        // without saying anything about any of them.
        e.scalar(
            "squelchd_devices_paired",
            MetricKind::Gauge,
            "Client devices currently holding an unrevoked device token; console sessions excluded.",
            db.devices_paired as f64,
        );

        e.family(
            "squelchd_db_size_bytes",
            MetricKind::Gauge,
            "On-disk size of the sqlite database and its write-ahead log.",
        );
        e.sample(
            "squelchd_db_size_bytes",
            &[("file", "db")],
            db.db_bytes as f64,
        );
        e.sample(
            "squelchd_db_size_bytes",
            &[("file", "wal")],
            db.wal_bytes as f64,
        );
    }

    e.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_values_escape_the_three_special_characters() {
        let mut e = Exposition::new();
        e.family("x_total", MetricKind::Counter, "help");
        e.sample("x_total", &[("k", "a\\b\"c\nd")], 1.0);
        assert!(e.finish().contains(r#"x_total{k="a\\b\"c\nd"} 1"#));
    }

    #[test]
    fn help_escapes_backslash_and_newline_but_not_quotes() {
        let mut e = Exposition::new();
        e.family("x_total", MetricKind::Counter, "a\\b\nc \"quoted\"");
        assert!(
            e.finish()
                .starts_with("# HELP x_total a\\\\b\\nc \"quoted\"\n")
        );
    }

    #[test]
    fn exposition_writes_help_type_samples_and_a_trailing_newline() {
        let mut e = Exposition::new();
        e.scalar("x_gauge", MetricKind::Gauge, "a gauge", 3.0);
        assert_eq!(
            e.finish(),
            "# HELP x_gauge a gauge\n# TYPE x_gauge gauge\nx_gauge 3\n"
        );
    }

    #[test]
    fn values_print_whole_numbers_without_a_decimal_point() {
        assert_eq!(fmt_value(0.0), "0");
        assert_eq!(fmt_value(42.0), "42");
        assert_eq!(fmt_value(1.5), "1.5");
        assert_eq!(fmt_value(f64::INFINITY), "+Inf");
        assert_eq!(fmt_value(f64::NAN), "NaN");
    }

    #[test]
    fn sync_ok_stamps_freshness_and_clears_the_failure_streak() {
        let m = SyncMetrics::new();
        m.record_sync_error();
        m.record_sync_error();
        assert_eq!(m.sync_consecutive_failures.load(Ordering::Relaxed), 2);
        assert_eq!(m.sync_last_success_unix.load(Ordering::Relaxed), 0);

        m.record_sync_ok();
        assert_eq!(m.sync_consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(m.sync_runs_ok.load(Ordering::Relaxed), 1);
        assert_eq!(m.sync_runs_err.load(Ordering::Relaxed), 2);
        assert!(m.sync_last_success_unix.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn verdict_and_gmail_counters_land_on_their_own_labels() {
        let m = SyncMetrics::new();
        m.record_gmail_error(GmailErrorKind::Quota);
        m.record_gmail_error(GmailErrorKind::Quota);
        m.record_gmail_error(GmailErrorKind::Auth);
        m.record_stage1(Stage1Verdict::Fallback);
        m.record_stage2(Stage2Verdict::Retryable);

        let text = render(&m, None);
        assert!(text.contains("squelchd_gmail_api_errors_total{kind=\"quota\"} 2\n"));
        assert!(text.contains("squelchd_gmail_api_errors_total{kind=\"auth\"} 1\n"));
        assert!(text.contains("squelchd_gmail_api_errors_total{kind=\"network\"} 0\n"));
        assert!(
            text.contains(
                "squelchd_triage_verdicts_total{stage=\"stage1\",outcome=\"fallback\"} 1\n"
            )
        );
        assert!(text.contains(
            "squelchd_triage_verdicts_total{stage=\"stage2\",outcome=\"retryable\"} 1\n"
        ));
    }

    /// The carrier axis is closed: every carrier/outcome pair is exported, a
    /// slug that is not a carrier maps to no label at all, and the freshness
    /// stamp moves only for an answered poll.
    #[test]
    fn carrier_poll_counters_export_every_pair_and_stamp_only_on_success() {
        let m = SyncMetrics::new();
        assert_eq!(PollCarrier::from_slug("amazon"), None);
        assert_eq!(PollCarrier::from_slug("UPS"), None);
        assert_eq!(PollCarrier::from_slug("dhl"), Some(PollCarrier::Dhl));

        m.record_carrier_poll(PollCarrier::Dhl, CarrierPollOutcome::RateLimited);
        m.record_carrier_poll(PollCarrier::Dhl, CarrierPollOutcome::RateLimited);
        m.record_carrier_poll(PollCarrier::Ups, CarrierPollOutcome::NotFound);
        assert_eq!(
            m.carrier_poll_last_success_unix.load(Ordering::Relaxed),
            0,
            "a failed poll is not a success"
        );

        m.record_carrier_poll(PollCarrier::Ups, CarrierPollOutcome::Ok);
        m.record_shipment_advanced();

        let text = render(&m, None);
        assert!(
            text.contains(
                "squelchd_carrier_poll_total{carrier=\"dhl\",outcome=\"rate_limited\"} 2\n"
            )
        );
        assert!(
            text.contains("squelchd_carrier_poll_total{carrier=\"ups\",outcome=\"not_found\"} 1\n")
        );
        assert!(text.contains("squelchd_carrier_poll_total{carrier=\"ups\",outcome=\"ok\"} 1\n"));
        // A carrier nobody configured still exports its whole outcome row.
        assert!(
            text.contains("squelchd_carrier_poll_total{carrier=\"fedex\",outcome=\"auth\"} 0\n")
        );
        assert!(
            text.contains(
                "squelchd_carrier_poll_total{carrier=\"usps\",outcome=\"transient\"} 0\n"
            )
        );
        assert!(text.contains("squelchd_shipments_advanced_by_poll_total 1\n"));
        assert!(m.carrier_poll_last_success_unix.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn render_without_a_store_snapshot_omits_only_the_db_families() {
        let m = SyncMetrics::new();
        let text = render(&m, None);
        assert!(text.contains(&format!(
            "squelchd_build_info{{version=\"{}\"}} 1\n",
            env!("CARGO_PKG_VERSION")
        )));
        // Never-synced reads as 0, so `time() - metric` fires rather than going
        // silent.
        assert!(text.contains("squelchd_sync_last_success_timestamp_seconds 0\n"));
        assert!(!text.contains("squelchd_db_size_bytes"));
        assert!(!text.contains("squelchd_store_messages"));
        assert!(!text.contains("squelchd_devices_paired"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn render_with_a_store_snapshot_carries_ledger_categories_verbatim() {
        let m = SyncMetrics::new();
        let stats = StoreStats {
            tier_counts: [("signal".to_string(), 7i64), ("noise".to_string(), 3)]
                .into_iter()
                .collect(),
            total: 10,
            sealed: 2,
            last_history_id: None,
            bands: Default::default(),
            last_surfaced_at: None,
        };
        let db = StoreSnapshot {
            stats,
            llm: usage_from_ledger(vec![(
                "extract_banking".to_string(),
                vec![
                    Stage2UsageDay {
                        day: "2026-08-10".to_string(),
                        calls: 2,
                        input_tokens: 100,
                        output_tokens: 10,
                        cache_creation_tokens: 30,
                        cache_read_tokens: 400,
                    },
                    Stage2UsageDay {
                        day: "2026-08-09".to_string(),
                        calls: 1,
                        input_tokens: 50,
                        output_tokens: 5,
                        cache_creation_tokens: 10,
                        cache_read_tokens: 200,
                    },
                ],
            )]),
            llm_cost_usd: 0.25,
            db_bytes: 4096,
            wal_bytes: 0,
            devices_paired: 2,
        };
        let text = render(&m, Some(&db));
        assert!(text.contains("squelchd_store_messages{tier=\"signal\"} 7\n"));
        assert!(text.contains("squelchd_store_messages_sealed 2\n"));
        assert!(text.contains("squelchd_llm_calls_total{category=\"extract_banking\"} 3\n"));
        assert!(text.contains(
            "squelchd_llm_tokens_total{category=\"extract_banking\",direction=\"input\"} 150\n"
        ));
        assert!(text.contains(
            "squelchd_llm_tokens_total{category=\"extract_banking\",direction=\"cache_write\"} 40\n"
        ));
        assert!(text.contains(
            "squelchd_llm_tokens_total{category=\"extract_banking\",direction=\"cache_read\"} 600\n"
        ));
        assert!(text.contains("squelchd_llm_cost_usd_total 0.25\n"));
        assert!(text.contains("squelchd_db_size_bytes{file=\"wal\"} 0\n"));
        // The device gauge is a bare scalar: no name, no id, nothing that says
        // WHICH devices, because a scrape needs no bearer.
        assert!(text.contains("# TYPE squelchd_devices_paired gauge\n"));
        assert!(text.contains("squelchd_devices_paired 2\n"));
        assert!(!text.contains("squelchd_devices_paired{"));
    }

    #[test]
    fn cost_prices_stage2_apart_from_every_other_category() {
        let config = Config::default();
        let usage = vec![
            LlmCategoryUsage {
                category: "stage2".to_string(),
                calls: 1,
                input_tokens: 1_000_000,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
            LlmCategoryUsage {
                category: "extract_banking".to_string(),
                calls: 1,
                input_tokens: 1_000_000,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        ];
        let expected = config.stage2.price_in_per_mtok + config.stage1.price_in_per_mtok;
        assert!((estimate_cost_usd(&usage, &config) - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn cost_prices_cache_tokens_off_the_input_price() {
        let config = Config::default();
        let cached = vec![LlmCategoryUsage {
            category: "stage2".to_string(),
            calls: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
        }];
        let expected = config.stage2.price_in_per_mtok
            * (crate::config::CACHE_WRITE_INPUT_MULT + crate::config::CACHE_READ_INPUT_MULT);
        assert!((estimate_cost_usd(&cached, &config) - expected).abs() < 1e-9);

        // A cached-read prompt must cost LESS than the same tokens uncached —
        // the whole point of recording the split.
        let raw = vec![LlmCategoryUsage {
            category: "stage2".to_string(),
            calls: 1,
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
        }];
        let read_only = vec![LlmCategoryUsage {
            category: "stage2".to_string(),
            calls: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 1_000_000,
        }];
        assert!(estimate_cost_usd(&read_only, &config) < estimate_cost_usd(&raw, &config));
    }

    #[test]
    fn wal_path_is_the_db_path_plus_the_suffix_and_missing_files_are_zero() {
        let dir = std::env::temp_dir().join(format!("squelch-metrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("squelch.db");
        std::fs::write(&db, b"0123456789").unwrap();
        // No wal alongside it: checkpointed away is the normal steady state.
        assert_eq!(db_file_sizes(&db), (10, 0));

        std::fs::write(dir.join("squelch.db-wal"), b"01234").unwrap();
        assert_eq!(db_file_sizes(&db), (10, 5));
        std::fs::remove_dir_all(&dir).ok();
    }
}
