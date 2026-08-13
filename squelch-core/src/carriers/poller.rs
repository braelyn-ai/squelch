//! The background shipment poller: the one place that decides WHEN a carrier is
//! called, on top of the [`CarrierClient`]s that know HOW.
//!
//! THE CARRIER IS THE UNIT OF PACING, not the shipment. Every quota, every
//! `Retry-After`, every rejected credential belongs to one carrier's account, so
//! backoff, budgets and the `min_interval` floor are all held per carrier and a
//! carrier in trouble goes quiet WITHOUT stalling the other three. Within a
//! carrier calls are strictly sequential and spaced; across carriers they
//! interleave on one task, so a five-second DHL floor never blocks a UPS row.
//!
//! A SKIPPED ROW IS NOT A POLLED ROW — BUT AN ANSWERED ONE IS, HOWEVER IT
//! ANSWERED. `last_polled_at` is the queue's rotation key, so every call the
//! carrier actually answered stamps it: a track, a 404, and a transient failure
//! alike. Only a row DROPPED WITHOUT A CALL — its carrier gated by a 429 or a
//! rejected credential, so nothing was asked about this parcel — keeps its old
//! stamp and stays first in line. The distinction is service, not success: the
//! queue sorts never-polled first, so a row that is asked about and then left
//! unstamped is the permanent head of its carrier's queue, and ONE such row can
//! starve every other parcel behind it. Only [`TrackError::NotFound`] counts
//! AGAINST a shipment (see [`crate::store::Store::record_poll_outcome`]); the
//! rest are the carrier's problem, not the parcel's.
//!
//! A BAD ROW IS NOT A BAD CARRIER. Some transient failures are deterministic per
//! tracking number — a 200 whose body will not parse, a shipment-level warning
//! with no status — so one of them is evidence about that row, not about the
//! API. A single transient answer therefore moves on to the next row; only
//! [`TRANSIENT_TOLERANCE`] of them in a row, with nothing answered in between,
//! is evidence about the carrier and earns the carrier-wide backoff.
//!
//! ALL PACING STATE IS IN MEMORY and resets on restart — cooldowns, the transient
//! backoff ladder, and the DHL/USPS budget windows. That is accepted: the
//! randomized startup delay covers the stampede case, the daily cap is set below
//! the free tier with room to spare, and persisting a rate-limit clock would buy
//! a schema migration for a restart that happens once.

use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as Delta, Utc};
use tokio::sync::{Notify, watch};

use super::{CarrierClient, CarrierRegistry, TrackError, build_registry};
use crate::config::{CarriersConfig, Config};
use crate::error::Result;
use crate::metrics::{CarrierPollOutcome, PollCarrier, SyncMetrics};
use crate::store::Store;
use crate::triage::ShipmentStatus;
use crate::types::{AccountId, Shipment};

/// How often the due set is recomputed. Deliberately much shorter than the
/// per-shipment cadence: this is the resolution at which a row that just BECAME
/// due (an ETA rolled over to today) is noticed, not how often anything is
/// called.
const TICK_INTERVAL: Duration = Duration::from_secs(300);

/// Startup delay window. A restart must not be a synchronized burst — across a
/// fleet of daemons sharing one operator's carrier keys, "everyone polls at
/// boot" is exactly how a daily cap dies at 09:00 — so the first pass lands
/// somewhere in [30s, 120s].
const STARTUP_DELAY_MIN: Duration = Duration::from_secs(30);
const STARTUP_DELAY_SPAN_SECS: u64 = 90;

/// Transient-failure ladder, the same shape [`crate::tracking`] uses.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Consecutive transient answers from one carrier — with no track and no 404 in
/// between — before the CARRIER is judged to be in trouble and cooled down.
///
/// One is not evidence: `Transient` is also what a single unparseable body or a
/// single shipment-level warning produces, deterministically, for one number.
/// Cooling the whole carrier on it hands one poison row a veto over every other
/// parcel. Three in a row with nothing answered between them is a different
/// claim, and cheap to be wrong about: the cost of the extra two calls is two
/// calls, paced by `min_interval` and charged to the budget like any other.
const TRANSIENT_TOLERANCE: u32 = 3;

/// How long after a shipment is first seen a `NotFound` is read as PRE-MANIFEST
/// rather than as evidence the number is fiction.
///
/// A retailer routinely emails a waybill before handing the parcel over: DHL
/// 404s and FedEx answers `TRACKINGNUMBER.NOTFOUND` until the first scan, which
/// on a Friday-evening label is the whole weekend away. At the default cadence
/// the retirement cap is reached in ~30 hours, well inside that window, and
/// retirement is not just "stop polling" — the same counter suppresses ambiguous
/// rows from the client lists — so a live parcel would vanish before its first
/// scan. Three days covers a weekend; past it, a number no carrier will
/// acknowledge is a number nobody shipped, and the cap does its real job.
const NOT_FOUND_GRACE_HOURS: i64 = 72;

/// Floor under an honored `Retry-After`. A carrier that says "one second" while
/// answering 429 is describing its token bucket, not its opinion of us; coming
/// straight back is how a soft limit becomes a blocked key.
const RATE_LIMIT_FLOOR: Duration = Duration::from_secs(900);

/// How long a carrier stays quiet after rejecting our credentials. Nothing but
/// an operator editing config fixes this, so the retry exists only to pick the
/// new key up without a restart.
const AUTH_COOLDOWN: Duration = Duration::from_secs(3600);

/// USPS's published default is 60 calls/hour per consumer key, and unlike DHL's
/// daily cap it is not configurable here: it is not a budget an operator chose,
/// it is the product's own ceiling.
const USPS_HOURLY_CAP: u32 = 60;
const USPS_WINDOW: Duration = Duration::from_secs(3600);
const DHL_WINDOW: Duration = Duration::from_secs(86_400);

/// Jitter applied to a shipment's interval, in basis points of it: ±10%.
/// DETERMINISTIC per shipment id, so a row's cadence is stable across restarts
/// (a jitter redrawn every pass would let one row keep winning the early draw)
/// while a batch of shipments detected in the same mail sweep still fans out.
const JITTER_BPS: i64 = 1_000;

fn next_backoff(current: Option<Duration>) -> Duration {
    match current {
        None => BACKOFF_BASE,
        Some(d) => (d * 2).min(BACKOFF_CAP),
    }
}

/// A one-shot startup delay in [30s, 120s].
///
/// Entropy comes from `RandomState`'s per-instance keys — the same source std
/// uses to make `HashMap` iteration unguessable. This crate has no PRNG
/// dependency and does not want one for a number drawn ONCE whose only job is to
/// desynchronize daemons.
fn startup_delay() -> Duration {
    let n = std::collections::hash_map::RandomState::new().hash_one(0u8);
    STARTUP_DELAY_MIN + Duration::from_secs(n % (STARTUP_DELAY_SPAN_SECS + 1))
}

/// Spread an id into something jitter can slice. A plain `id % n` would put
/// consecutively-detected shipments (which is what a mail sweep produces) into
/// consecutive jitter buckets, i.e. no spread at all.
fn spread(id: i64) -> u64 {
    let mut z = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `base` scaled by this shipment's own factor in [0.9, 1.1].
fn jittered(base: Delta, id: i64) -> Delta {
    let secs = base.num_seconds();
    let span = 2 * JITTER_BPS + 1;
    let bps = 10_000 - JITTER_BPS + (spread(id) % span as u64) as i64;
    Delta::seconds(secs.saturating_mul(bps) / 10_000)
}

/// The tighter cadence applies while the parcel is on a truck: out for delivery,
/// or carrying an ETA of TODAY. Both are the same bet — the next interesting
/// transition is minutes away, not a day.
///
/// "Today" is UTC, matching every other timestamp comparison in the store. The
/// worst case is a few hours of the fast lane on either side of a local
/// midnight, which is the harmless direction to be wrong in.
fn on_the_truck(shipment: &Shipment, now: DateTime<Utc>) -> bool {
    shipment.status == ShipmentStatus::OutForDelivery.as_str()
        || shipment
            .eta
            .is_some_and(|eta| eta.date_naive() == now.date_naive())
}

/// Whether this shipment has waited out its own interval. A never-polled row is
/// always due — it is why `list_pollable_shipments` sorts those first.
fn is_due(shipment: &Shipment, now: DateTime<Utc>, knobs: &Knobs) -> bool {
    let Some(last) = shipment.last_polled_at else {
        return true;
    };
    let base = if on_the_truck(shipment, now) {
        knobs.ofd_interval
    } else {
        knobs.poll_interval
    };
    now - last >= jittered(base, shipment.id)
}

/// The cadence knobs, copied out of [`CarriersConfig`] at construction: config
/// is not reread mid-run, so a pass cannot see half of an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Knobs {
    poll_interval: Delta,
    ofd_interval: Delta,
    /// Stop polling a shipment this long after it was first seen.
    max_age: Delta,
    max_failures: u32,
    /// A `NotFound` on a row younger than this is PRE-MANIFEST: the attempt is
    /// stamped, but it does not count toward retirement. See
    /// [`NOT_FOUND_GRACE_HOURS`].
    not_found_grace: Delta,
    /// DHL calls per 24h. 0 means DHL is budgeted to a standstill — the operator
    /// asked for no DHL traffic without removing the key.
    dhl_daily_cap: u32,
}

impl Knobs {
    fn from_config(cfg: &CarriersConfig) -> Self {
        Self {
            poll_interval: Delta::hours(cfg.poll_interval_hours as i64),
            ofd_interval: Delta::minutes(cfg.ofd_poll_interval_mins as i64),
            max_age: Delta::days(cfg.max_age_days as i64),
            max_failures: cfg.max_failures,
            not_found_grace: Delta::hours(NOT_FOUND_GRACE_HOURS),
            dhl_daily_cap: cfg.dhl.as_ref().map_or(0, |d| d.daily_cap),
        }
    }
}

/// A ROLLING call budget: at most `limit` calls in ANY `window`-long stretch.
///
/// Not a token bucket — carrier quotas are published as "N per day", and
/// spending them evenly is not a requirement, only never exceeding them. But
/// "refill on the boundary" is how a published ceiling gets served twice over: a
/// burst at the end of one window and another at the start of the next both sit
/// inside one rolling window, and the carrier counts rolling. So each spend is
/// remembered as its own instant and frees its slot only when IT ages out the
/// back of the window. At most `limit` instants are live (200 for DHL's day, 60
/// for USPS's hour), which is cheaper than the schema migration a persisted
/// clock would cost.
#[derive(Debug)]
struct Budget {
    limit: u32,
    window: Duration,
    /// The instants of the spends still inside the window, oldest first.
    spent: VecDeque<Instant>,
}

impl Budget {
    fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            spent: VecDeque::new(),
        }
    }

    /// Drop the spends that have fallen out the back of the window. Takes `now`
    /// rather than reading the clock so the roll is testable without sitting
    /// through a window.
    fn expire(&mut self, now: Instant) {
        while self
            .spent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= self.window)
        {
            self.spent.pop_front();
        }
    }

    /// Whether a call fits right now.
    fn available(&mut self, now: Instant) -> bool {
        self.expire(now);
        self.spent.len() < self.limit as usize
    }

    fn spend(&mut self, now: Instant) {
        self.expire(now);
        self.spent.push_back(now);
    }
}

/// One carrier's pacing state. Lives for the poller's lifetime, not the pass's:
/// a cooldown entered at the end of one pass is the whole point of the next one
/// skipping the carrier.
#[derive(Debug)]
struct CarrierState {
    /// Earliest instant the next call to this carrier may start, from
    /// [`CarrierClient::min_interval`].
    next_call: Instant,
    /// Carrier-wide pause after a 429, an auth rejection, or a transient run.
    cooldown_until: Option<Instant>,
    /// Transient ladder, reset by any answered call.
    backoff: Option<Duration>,
    /// Transient answers since the last one the carrier answered properly. The
    /// counter is what separates a poison ROW from a broken CARRIER; see
    /// [`TRANSIENT_TOLERANCE`].
    transient_run: u32,
    /// Quota window, for the carriers that have one.
    budget: Option<Budget>,
    /// Whether the CURRENT auth cooldown already printed its line. Without this
    /// a dead key would narrate itself on every pass forever.
    auth_reported: bool,
}

impl CarrierState {
    fn new(budget: Option<Budget>) -> Self {
        Self {
            next_call: Instant::now(),
            cooldown_until: None,
            backoff: None,
            transient_run: 0,
            budget,
            auth_reported: false,
        }
    }

    /// Whether this carrier is closed for business right now. Clears an expired
    /// cooldown as it goes (and with it the auth notice, so a still-bad key says
    /// so once per hour rather than once per pass).
    fn gated(&mut self, now: Instant) -> bool {
        if let Some(until) = self.cooldown_until {
            if now < until {
                return true;
            }
            self.cooldown_until = None;
            self.auth_reported = false;
        }
        match self.budget.as_mut() {
            Some(budget) => !budget.available(now),
            None => false,
        }
    }

    fn cool_down(&mut self, delay: Duration) {
        self.cooldown_until = Some(Instant::now() + delay);
    }

    /// A call the carrier ANSWERED about a parcel — a track or a 404. Either way
    /// the API is up and serving, which is the only thing the transient ladder
    /// and its run counter were ever measuring.
    fn answered(&mut self) {
        self.backoff = None;
        self.transient_run = 0;
    }

    /// One transient answer. Reports whether the run has reached the point where
    /// the CARRIER, rather than the row, is the likely problem.
    fn transient(&mut self) -> bool {
        self.transient_run += 1;
        self.transient_run >= TRANSIENT_TOLERANCE
    }

    fn spend(&mut self, now: Instant) {
        if let Some(budget) = self.budget.as_mut() {
            budget.spend(now);
        }
    }
}

/// Sleep, unless shutdown arrives first. `true` means "stop": either the watch
/// went true, or its sender is gone (a closed channel is a daemon on its way
/// out, and treating it as anything else spins).
async fn sleep_or_shutdown(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    if delay.is_zero() {
        return false;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
    }
}

/// The carrier-poll task. Construct with [`ShipmentPoller::from_config`], which
/// returns `Ok(None)` when no carrier credentials resolve.
pub struct ShipmentPoller {
    store: Arc<dyn Store>,
    account_id: AccountId,
    registry: CarrierRegistry,
    knobs: Knobs,
    /// Forces an immediate pass. Handed out by [`Self::kick_handle`].
    kick: Arc<Notify>,
    metrics: Arc<SyncMetrics>,
    startup_delay: Duration,
    tick_interval: Duration,
}

impl ShipmentPoller {
    /// Build the poller, or `Ok(None)` when [`build_registry`] resolved no
    /// carrier — CREDENTIALS ARE THE FEATURE FLAG, so an operator who configured
    /// nothing gets no task, no timer and no outbound traffic.
    ///
    /// The `Result` is the shape every other background task here has; today
    /// there is no failure path, because a carrier HTTP client that will not
    /// build is already downgraded to "that carrier is not configured".
    pub fn from_config(
        store: Arc<dyn Store>,
        account_id: AccountId,
        config: &Config,
    ) -> Result<Option<Self>> {
        let registry = build_registry(&config.carriers);
        if registry.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            store,
            account_id,
            registry,
            knobs: Knobs::from_config(&config.carriers),
            kick: Arc::new(Notify::new()),
            metrics: SyncMetrics::new(),
            startup_delay: startup_delay(),
            tick_interval: TICK_INTERVAL,
        }))
    }

    /// Share the daemon's registry so carrier polls land in the same scrape as
    /// everything else. Without this the poller counts into a registry nobody
    /// reads.
    pub fn with_metrics(mut self, metrics: Arc<SyncMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// The kick handle: notifying it makes the poller start a pass immediately
    /// instead of waiting out the tick. It does NOT bypass per-carrier
    /// cooldowns, budgets or `min_interval` — a user mashing refresh must not be
    /// able to spend a daily cap or earn a 429.
    pub fn kick_handle(&self) -> Arc<Notify> {
        self.kick.clone()
    }

    /// Configured carrier slugs, sorted: the startup line, and the answer a
    /// future kick endpoint gives back.
    pub fn enabled_carriers(&self) -> Vec<String> {
        let mut out: Vec<String> = self.registry.keys().cloned().collect();
        out.sort();
        out
    }

    /// TEST HOOK: a poller over an explicit registry (mock carriers on an
    /// ephemeral port), with the startup delay and both cadences at zero so
    /// every non-delivered row is due on the first pass.
    #[doc(hidden)]
    pub fn for_test(
        store: Arc<dyn Store>,
        account_id: AccountId,
        registry: CarrierRegistry,
    ) -> Self {
        Self {
            store,
            account_id,
            registry,
            knobs: Knobs {
                poll_interval: Delta::zero(),
                ofd_interval: Delta::zero(),
                max_age: Delta::days(3650),
                max_failures: 5,
                // No pre-manifest grace: a `NotFound` counts immediately, so a
                // test drives the retirement cap without inventing a row that
                // was first seen three days ago. A test that wants the grace
                // sets it.
                not_found_grace: Delta::zero(),
                // Effectively no ceiling: a test that wants one sets it.
                dhl_daily_cap: u32::MAX,
            },
            kick: Arc::new(Notify::new()),
            metrics: SyncMetrics::new(),
            startup_delay: Duration::ZERO,
            tick_interval: Duration::from_millis(50),
        }
    }

    /// TEST HOOK: how long the loop waits between passes.
    #[doc(hidden)]
    pub fn with_tick_interval(mut self, tick: Duration) -> Self {
        self.tick_interval = tick;
        self
    }

    /// TEST HOOK: the per-shipment permanent-failure cap.
    #[doc(hidden)]
    pub fn with_max_failures(mut self, max_failures: u32) -> Self {
        self.knobs.max_failures = max_failures;
        self
    }

    /// TEST HOOK: the pre-manifest window in which a `NotFound` is stamped but
    /// not counted.
    #[doc(hidden)]
    pub fn with_not_found_grace(mut self, grace: Delta) -> Self {
        self.knobs.not_found_grace = grace;
        self
    }

    /// One state per configured carrier, budgets attached where the carrier has
    /// a quota worth counting.
    fn initial_states(&self) -> HashMap<String, CarrierState> {
        self.registry
            .keys()
            .map(|carrier| {
                let budget = match carrier.as_str() {
                    "dhl" if self.knobs.dhl_daily_cap < u32::MAX => {
                        Some(Budget::new(self.knobs.dhl_daily_cap, DHL_WINDOW))
                    }
                    "usps" => Some(Budget::new(USPS_HOURLY_CAP, USPS_WINDOW)),
                    _ => None,
                };
                (carrier.clone(), CarrierState::new(budget))
            })
            .collect()
    }

    /// Run until shutdown: a randomized startup delay, then a pass per tick, per
    /// kick, or both.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut states = self.initial_states();
        if sleep_or_shutdown(self.startup_delay, &mut shutdown).await {
            return Ok(());
        }

        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            if let Err(e) = self.pass(&mut states, &mut shutdown).await {
                // The store is the only thing that can fail a whole pass, and
                // the error names a table at worst. Nothing was stamped, so the
                // next pass sees the same queue.
                eprintln!("squelch: shipment poll pass failed ({e}); nothing was stamped");
            }
            tokio::select! {
                _ = tokio::time::sleep(self.tick_interval) => {}
                // A kick delivered while a pass was running is held as a permit,
                // so it is honored here rather than lost.
                _ = self.kick.notified() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }

    /// One pass: recompute the due set, then interleave it across carriers until
    /// every queue is empty or every carrier is gated.
    async fn pass(
        &self,
        states: &mut HashMap<String, CarrierState>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let now = Utc::now();
        let rows = self.store.list_pollable_shipments(
            self.account_id,
            now - self.knobs.max_age,
            self.knobs.max_failures,
        )?;

        // The store filtered out delivered, retired, over-age and API-less rows;
        // this filters to carriers we hold a key for and to rows whose own
        // interval has elapsed. Queue order is the store's: never-polled first,
        // then least-recently-polled, so a budget is spent on the stalest rows.
        let mut queues: HashMap<String, VecDeque<Shipment>> = HashMap::new();
        for shipment in rows {
            if !self.registry.contains_key(&shipment.carrier) {
                continue;
            }
            if !is_due(&shipment, now, &self.knobs) {
                continue;
            }
            queues
                .entry(shipment.carrier.clone())
                .or_default()
                .push_back(shipment);
        }
        // Sorted so a tie between two idle carriers resolves the same way every
        // pass instead of by hash order.
        let mut carriers: Vec<String> = queues.keys().cloned().collect();
        carriers.sort();

        while let Some((carrier, at)) = next_up(&carriers, &queues, states) {
            if sleep_or_shutdown(at.saturating_duration_since(Instant::now()), shutdown).await {
                return Ok(());
            }
            let Some(shipment) = queues.get_mut(&carrier).and_then(VecDeque::pop_front) else {
                continue;
            };
            let (Some(client), Some(state)) = (
                self.registry.get(&carrier).cloned(),
                states.get_mut(&carrier),
            ) else {
                continue;
            };
            self.poll_one(&client, state, &shipment).await;
        }
        Ok(())
    }

    /// One shipment against its carrier, plus everything the answer implies for
    /// the row and for the carrier.
    async fn poll_one(
        &self,
        client: &Arc<dyn CarrierClient>,
        state: &mut CarrierState,
        shipment: &Shipment,
    ) {
        let started = Instant::now();
        // The gate already confirmed a slot; take it before the call so a
        // failure still counts against the quota — the carrier metered it.
        state.spend(started);
        let result = client.track(&shipment.tracking_number).await;
        // Spacing is measured from the START of the call: `min_interval` is the
        // carrier's rate limit, not a cooldown to serve after our own latency.
        state.next_call = started + client.min_interval();

        let carrier = client.carrier();
        if let Some(label) = PollCarrier::from_slug(carrier) {
            self.metrics.record_carrier_poll(
                label,
                match &result {
                    Ok(_) => CarrierPollOutcome::Ok,
                    Err(TrackError::NotFound) => CarrierPollOutcome::NotFound,
                    Err(TrackError::RateLimited { .. }) => CarrierPollOutcome::RateLimited,
                    Err(TrackError::Auth) => CarrierPollOutcome::Auth,
                    Err(TrackError::Transient) => CarrierPollOutcome::Transient,
                },
            );
        }

        match result {
            Ok(track) => {
                state.answered();
                // Stamps `last_polled_at`, resets `poll_failures`, and leaves
                // `last_message_id` alone; see the store's own doc comment.
                match self.store.apply_carrier_track(
                    self.account_id,
                    shipment.id,
                    &track,
                    Utc::now(),
                ) {
                    Ok(true) => self.metrics.record_shipment_advanced(),
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("squelch: shipment poll could not record a carrier result: {e}")
                    }
                }
            }
            // The only per-SHIPMENT verdict: this number does not exist. The
            // carrier answered, so the carrier is fine — but the answer only
            // counts against the ROW once the pre-manifest window has passed,
            // because "not found yet" is what every carrier says about a label
            // its shipper has not handed over.
            Err(TrackError::NotFound) => {
                state.answered();
                let age = Utc::now() - shipment.first_seen;
                self.record_attempt(shipment.id, age >= self.knobs.not_found_grace);
            }
            // A transient answer is about THIS ROW until proven otherwise: it is
            // what an unparseable body or a shipment-level warning produces,
            // deterministically, for one number. So the attempt is stamped (the
            // row rotates to the back of the queue instead of holding the head
            // of it forever) without counting as a failure, and the pass moves
            // on to the next row. Only a RUN of them with nothing answered in
            // between is evidence about the carrier, and that earns the ladder.
            Err(TrackError::Transient) => {
                self.record_attempt(shipment.id, false);
                if state.transient() {
                    let delay = next_backoff(state.backoff);
                    state.backoff = Some(delay);
                    state.transient_run = 0;
                    state.cool_down(delay);
                }
            }
            // The rest are CARRIER verdicts about no parcel in particular:
            // nothing was asked about this number, so the row is left unstamped
            // on purpose — it was skipped, not polled, and it should be first in
            // line when the carrier reopens.
            Err(TrackError::RateLimited { retry_after }) => {
                let delay = match retry_after {
                    Some(delay) => delay,
                    None => {
                        let delay = next_backoff(state.backoff);
                        state.backoff = Some(delay);
                        delay
                    }
                };
                state.cool_down(delay.max(RATE_LIMIT_FLOOR));
            }
            Err(TrackError::Auth) => {
                // DROP THE REJECTED CREDENTIAL FIRST. The cached token outlives
                // this cooldown (UPS's is four hours, USPS's eight), so without
                // this the carrier reopens in an hour, replays the exact token
                // it already refused, and prints the same line — for as long as
                // the TTL lasts. Re-minting costs one token call and is the only
                // way an operator's corrected key is ever picked up.
                client.invalidate_auth().await;
                if !state.auth_reported {
                    // The CARRIER ONLY. No url, no key, no tracking number: an
                    // auth error body routinely quotes the credential back, and
                    // this line is the one thing an operator sees.
                    eprintln!(
                        "squelch: {carrier} rejected the configured credentials; pausing {carrier} polls for {} minutes",
                        AUTH_COOLDOWN.as_secs() / 60
                    );
                    state.auth_reported = true;
                }
                state.cool_down(AUTH_COOLDOWN);
            }
        }
    }

    /// Stamp a poll ATTEMPT that produced no track, counting it against the row
    /// only when it was permanent. A store error here is reported and dropped:
    /// the next pass re-polls the row, which is the same thing the failed write
    /// would have arranged.
    fn record_attempt(&self, shipment_id: i64, permanent_failure: bool) {
        if let Err(e) = self.store.record_poll_outcome(
            self.account_id,
            shipment_id,
            Utc::now(),
            permanent_failure,
        ) {
            eprintln!("squelch: shipment poll could not record an attempt: {e}");
        }
    }
}

/// The next carrier to call and when it may start: among carriers with work left
/// and no gate against them, the one that can go soonest. Free function because
/// it needs `&mut` on the states while the queues are borrowed.
fn next_up(
    carriers: &[String],
    queues: &HashMap<String, VecDeque<Shipment>>,
    states: &mut HashMap<String, CarrierState>,
) -> Option<(String, Instant)> {
    let now = Instant::now();
    let mut best: Option<(String, Instant)> = None;
    for carrier in carriers {
        if queues.get(carrier).is_none_or(VecDeque::is_empty) {
            continue;
        }
        let Some(state) = states.get_mut(carrier) else {
            continue;
        };
        if state.gated(now) {
            continue;
        }
        let at = state.next_call.max(now);
        if best.as_ref().is_none_or(|(_, best_at)| at < *best_at) {
            best = Some((carrier.clone(), at));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use crate::triage::CarrierTrack;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A FIXED wall clock for every date-sensitive assertion. `Utc::now()` makes
    /// "is this ETA today?" a test that fails for the two hours before UTC
    /// midnight; noon is the middle of a day in every direction.
    fn noon() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-03-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn shipment(id: i64, status: ShipmentStatus) -> Shipment {
        Shipment {
            id,
            account_id: 1,
            tracking_number: "1Z999AA10123456784".to_string(),
            carrier: "ups".to_string(),
            item_name: String::new(),
            status: status.as_str().to_string(),
            tracking_url: None,
            thread_id: None,
            first_seen: noon(),
            last_update: noon(),
            carrier_status_raw: None,
            eta: None,
            delivered_at: None,
            last_polled_at: None,
            poll_failures: 0,
        }
    }

    fn knobs() -> Knobs {
        Knobs::from_config(&CarriersConfig::default())
    }

    // ---- due-ness --------------------------------------------------------

    #[test]
    fn a_never_polled_shipment_is_always_due() {
        let now = noon();
        let mut s = shipment(1, ShipmentStatus::Shipped);
        assert!(is_due(&s, now, &knobs()));
        // …and stops being due the moment it is stamped.
        s.last_polled_at = Some(now);
        assert!(!is_due(&s, now, &knobs()));
    }

    #[test]
    fn the_baseline_cadence_is_the_configured_interval_give_or_take_the_jitter() {
        let now = noon();
        let knobs = knobs(); // 6h baseline
        let mut s = shipment(1, ShipmentStatus::Shipped);

        // Inside the tightest possible jittered interval (6h - 10%), no row is
        // due; past the loosest (6h + 10%), every row is.
        s.last_polled_at = Some(now - Delta::minutes(5 * 60 + 23));
        assert!(!is_due(&s, now, &knobs));
        s.last_polled_at = Some(now - Delta::minutes(6 * 60 + 37));
        assert!(is_due(&s, now, &knobs));
    }

    /// The fast lane is entered by the status OR by an ETA of today, and it is
    /// the ONLY thing that makes a 90-minute-old poll due again.
    ///
    /// Every timestamp here is built from a FIXED noon rather than derived from
    /// `Utc::now()`: `now + 2h` is only "today" if the run does not start within
    /// two hours of UTC midnight, which made this test fail nightly.
    #[test]
    fn the_fast_lane_covers_out_for_delivery_and_an_eta_today() {
        let now = noon();
        let knobs = knobs(); // 60m out-for-delivery cadence
        let hour_and_a_half = Some(now - Delta::minutes(90));

        let mut plain = shipment(1, ShipmentStatus::Shipped);
        plain.last_polled_at = hour_and_a_half;
        assert!(!is_due(&plain, now, &knobs));

        let mut ofd = shipment(1, ShipmentStatus::OutForDelivery);
        ofd.last_polled_at = hour_and_a_half;
        assert!(on_the_truck(&ofd, now));
        assert!(is_due(&ofd, now, &knobs));

        // Still merely "shipped", but the carrier says it lands today.
        let mut eta_today = shipment(1, ShipmentStatus::Shipped);
        eta_today.last_polled_at = hour_and_a_half;
        eta_today.eta = Some(now + Delta::hours(2));
        assert!(on_the_truck(&eta_today, now));
        assert!(is_due(&eta_today, now, &knobs));

        // An ETA on another day is not the fast lane.
        let mut eta_tomorrow = shipment(1, ShipmentStatus::Shipped);
        eta_tomorrow.last_polled_at = hour_and_a_half;
        eta_tomorrow.eta = Some(now + Delta::days(2));
        assert!(!on_the_truck(&eta_tomorrow, now));
        assert!(!is_due(&eta_tomorrow, now, &knobs));
    }

    #[test]
    fn jitter_is_deterministic_per_id_stays_within_ten_percent_and_actually_spreads() {
        let base = Delta::hours(6);
        for id in [1i64, 2, 7, 4242, i64::MAX] {
            let first = jittered(base, id);
            assert_eq!(first, jittered(base, id), "same row, same interval");
            assert!(
                first >= base * 9 / 10 && first <= base * 11 / 10,
                "id {id} jittered out of the ±10% band: {first}"
            );
        }
        // Consecutive ids — what one mail sweep produces — must not land on one
        // interval, or the fan-out is decorative.
        let spread: std::collections::HashSet<i64> = (1..=20)
            .map(|id| jittered(base, id).num_seconds())
            .collect();
        assert!(
            spread.len() > 10,
            "consecutive ids barely spread: {spread:?}"
        );
    }

    #[test]
    fn a_zero_interval_stays_zero_under_jitter() {
        // The test hook sets both cadences to zero; jitter must not resurrect a
        // wait out of it.
        assert_eq!(jittered(Delta::zero(), 99), Delta::zero());
    }

    // ---- budgets ---------------------------------------------------------

    #[test]
    fn a_budget_spends_down_then_frees_each_slot_as_it_ages_out() {
        let start = Instant::now();
        let window = Duration::from_secs(3600);
        let mut budget = Budget::new(2, window);

        assert!(budget.available(start));
        budget.spend(start);
        assert!(budget.available(start));
        budget.spend(start);
        assert!(
            !budget.available(start),
            "the ceiling holds inside a window"
        );
        // Still spent a moment before those two calls age out.
        assert!(!budget.available(start + window - Duration::from_secs(1)));
        // …and both slots are free a window after they were taken.
        assert!(budget.available(start + window));
        budget.spend(start + window);
        assert!(budget.available(start + window));
    }

    /// The bug a fixed window has: two calls at the end of one window and two at
    /// the start of the next are four calls inside one rolling `window`, which
    /// is what the carrier is actually counting. Each slot must come back a
    /// window after IT was spent, not when a shared boundary rolls.
    #[test]
    fn the_budget_cannot_be_exceeded_across_a_window_boundary() {
        let start = Instant::now();
        let window = Duration::from_secs(3600);
        let mut budget = Budget::new(2, window);

        // Both calls at the tail of the first hour.
        let tail = start + window - Duration::from_secs(60);
        budget.spend(tail);
        budget.spend(tail);

        // A fixed window refilled at `start + window` would allow two more here,
        // i.e. four calls in a two-minute span against a cap of two per hour.
        let head = start + window + Duration::from_secs(60);
        assert!(
            !budget.available(head),
            "the ceiling was doubled across the boundary"
        );
        // The slots come back only once the calls that took them are a full
        // window old.
        assert!(!budget.available(tail + window - Duration::from_secs(1)));
        assert!(budget.available(tail + window));

        // …and it holds for any rolling window, not just the one boundary: after
        // spending both again, no `now` inside the next window admits a call.
        let mut budget = Budget::new(2, window);
        budget.spend(start);
        budget.spend(start + Duration::from_secs(1));
        for offset in [1u64, 60, 1800, 3599] {
            assert!(
                !budget.available(start + Duration::from_secs(offset)),
                "a third call fit {offset}s into the window"
            );
        }
    }

    #[test]
    fn a_zero_cap_stops_the_carrier_dead() {
        let start = Instant::now();
        let mut budget = Budget::new(0, DHL_WINDOW);
        assert!(!budget.available(start));
        assert!(!budget.available(start + DHL_WINDOW));
    }

    /// DHL gets the configured daily cap, USPS its product's hourly ceiling, and
    /// the OAuth pair carriers no budget at all.
    #[test]
    fn budgets_are_attached_per_carrier() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut registry = CarrierRegistry::new();
        let http = reqwest::Client::new();
        registry.insert(
            "dhl".to_string(),
            Arc::new(crate::carriers::dhl::DhlClient::for_test(
                "http://127.0.0.1:1",
                http,
            )) as Arc<dyn CarrierClient>,
        );
        let mut poller = ShipmentPoller::for_test(store, 1, registry);
        poller.knobs.dhl_daily_cap = 3;

        let states = poller.initial_states();
        let budget = states["dhl"].budget.as_ref().expect("dhl is budgeted");
        assert_eq!(budget.limit, 3);
        assert_eq!(budget.window, DHL_WINDOW);
        assert_eq!(poller.enabled_carriers(), vec!["dhl".to_string()]);
    }

    // ---- cooldowns -------------------------------------------------------

    #[test]
    fn a_cooldown_gates_the_carrier_until_it_expires() {
        let mut state = CarrierState::new(None);
        let now = Instant::now();
        assert!(!state.gated(now));

        state.cool_down(Duration::from_secs(60));
        assert!(state.gated(Instant::now()));
        // The gate lifts on its own, and clears the auth notice with it so a
        // still-bad key reports once per cooldown rather than once per pass.
        state.auth_reported = true;
        assert!(!state.gated(now + Duration::from_secs(61)));
        assert!(!state.auth_reported);
    }

    #[test]
    fn an_exhausted_budget_gates_the_carrier_without_a_cooldown() {
        let start = Instant::now();
        let mut state = CarrierState::new(Some(Budget::new(1, USPS_WINDOW)));
        assert!(!state.gated(start));
        state.spend(start);
        assert!(state.gated(start));
        assert!(
            state.cooldown_until.is_none(),
            "a spent budget is not an error condition"
        );
        assert!(!state.gated(start + USPS_WINDOW));
    }

    #[test]
    fn the_transient_ladder_climbs_and_caps() {
        assert_eq!(next_backoff(None), BACKOFF_BASE);
        assert_eq!(next_backoff(Some(BACKOFF_BASE)), BACKOFF_BASE * 2);
        assert_eq!(next_backoff(Some(BACKOFF_CAP)), BACKOFF_CAP);
        assert_eq!(next_backoff(Some(BACKOFF_CAP * 2)), BACKOFF_CAP);
    }

    // ---- what one answer does to the row and to the carrier ---------------

    /// What a track call answers. Spelled out because this crate's `Result`
    /// alias fixes the error half to `CoreError`.
    type Tracked = std::result::Result<CarrierTrack, TrackError>;

    /// A carrier client that answers the same thing every time and counts what
    /// the poller asked of it — including the credential invalidation, which
    /// leaves no other trace.
    struct Mock {
        answer: Tracked,
        calls: AtomicUsize,
        invalidated: AtomicUsize,
    }

    impl Mock {
        fn new(answer: Tracked) -> Arc<Self> {
            Arc::new(Self {
                answer,
                calls: AtomicUsize::new(0),
                invalidated: AtomicUsize::new(0),
            })
        }

        fn invalidated(&self) -> usize {
            self.invalidated.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl CarrierClient for Mock {
        fn carrier(&self) -> &'static str {
            "ups"
        }

        fn min_interval(&self) -> Duration {
            Duration::ZERO
        }

        async fn track(&self, _tracking_number: &str) -> Tracked {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer.clone()
        }

        async fn invalidate_auth(&self) {
            self.invalidated.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// One stored shipment, a poller that will only ever talk to the mock, and
    /// the mock. `first_seen` is explicit because the pre-manifest grace is
    /// measured from it.
    fn harness(
        answer: Tracked,
        first_seen: DateTime<Utc>,
    ) -> (Arc<SqliteStore>, ShipmentPoller, Arc<Mock>, Shipment) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let account_id = store.ensure_account("me@example.com").unwrap();
        store
            .upsert_shipment(
                account_id,
                1,
                &crate::triage::ShipmentInfo {
                    carrier: "ups".to_string(),
                    tracking_number: "1Z999AA10123456784".to_string(),
                    item_name: "Socks".to_string(),
                    status: ShipmentStatus::Shipped,
                    tracking_url: None,
                },
                first_seen,
            )
            .unwrap();
        let mock = Mock::new(answer);
        let mut registry = CarrierRegistry::new();
        registry.insert("ups".to_string(), mock.clone() as Arc<dyn CarrierClient>);
        let poller =
            ShipmentPoller::for_test(store.clone() as Arc<dyn Store>, account_id, registry);
        let row = stored(&store, account_id);
        (store, poller, mock, row)
    }

    fn stored(store: &SqliteStore, account_id: AccountId) -> Shipment {
        store
            .list_shipments(account_id, true, u32::MAX)
            .unwrap()
            .remove(0)
    }

    /// DEFECT: a transient answer used to write NOTHING. The row kept its NULL
    /// `last_polled_at`, which sorts first in the poll queue forever, so one
    /// deterministically-failing number (an unparseable 200 is transient) was
    /// re-polled ahead of every other parcel on that carrier until it aged out.
    #[tokio::test]
    async fn a_transient_attempt_is_stamped_but_not_counted_as_a_failure() {
        let (store, poller, mock, row) = harness(Err(TrackError::Transient), noon());
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);
        poller.poll_one(&client, &mut state, &row).await;

        let after = stored(&store, poller.account_id);
        assert!(
            after.last_polled_at.is_some(),
            "the attempt reached the carrier and must rotate the row"
        );
        assert_eq!(
            after.poll_failures, 0,
            "a transient failure is not the parcel's fault"
        );
    }

    /// The other half of the same defect: one bad row must not shut the carrier.
    /// (`carrier_poll.rs` proves the consequence end to end, through the queue.)
    #[tokio::test]
    async fn a_transient_answer_does_not_cool_the_whole_carrier() {
        let (_store, poller, mock, row) = harness(Err(TrackError::Transient), noon());
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);

        poller.poll_one(&client, &mut state, &row).await;
        assert!(
            !state.gated(Instant::now()),
            "one poison number cooled down the whole carrier"
        );
        assert_eq!(state.transient_run, 1);

        // Any answered call clears the run: a carrier that is serving other rows
        // never climbs toward the cooldown at all.
        state.answered();
        assert_eq!(state.transient_run, 0);
        assert_eq!(state.backoff, None);
    }

    /// …but a carrier that answers nothing else IS the problem, and still earns
    /// the ladder — at [`TRANSIENT_TOLERANCE`] calls, not at one.
    #[tokio::test]
    async fn a_run_of_transients_still_cools_the_carrier_down() {
        let (_store, poller, mock, row) = harness(Err(TrackError::Transient), noon());
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);

        for _ in 1..TRANSIENT_TOLERANCE {
            poller.poll_one(&client, &mut state, &row).await;
            assert!(!state.gated(Instant::now()));
        }
        poller.poll_one(&client, &mut state, &row).await;
        assert!(state.gated(Instant::now()), "a dead carrier must go quiet");
        assert_eq!(state.backoff, Some(BACKOFF_BASE));
        assert_eq!(state.transient_run, 0, "the run restarts after a cooldown");
    }

    /// DEFECT: the auth cooldown left the rejected token in the client's cache,
    /// so the carrier reopened an hour later and replayed the same dead
    /// credential until its full TTL expired.
    #[tokio::test]
    async fn an_auth_rejection_drops_the_cached_token_before_the_cooldown() {
        let (store, poller, mock, row) = harness(Err(TrackError::Auth), noon());
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);
        poller.poll_one(&client, &mut state, &row).await;

        assert_eq!(mock.invalidated(), 1, "the rejected token must be dropped");
        assert!(state.gated(Instant::now()), "and the carrier goes quiet");
        assert_eq!(
            stored(&store, poller.account_id).last_polled_at,
            None,
            "nothing was asked about this parcel, so it keeps its place"
        );
    }

    /// A 429 is the same shape of verdict: no parcel was judged, so the row it
    /// was holding stays unstamped and first in line.
    #[tokio::test]
    async fn a_rate_limited_row_keeps_its_place_in_the_queue() {
        let (store, poller, mock, row) =
            harness(Err(TrackError::RateLimited { retry_after: None }), noon());
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);
        poller.poll_one(&client, &mut state, &row).await;

        let after = stored(&store, poller.account_id);
        assert_eq!(after.last_polled_at, None);
        assert_eq!(after.poll_failures, 0);
        assert_eq!(mock.invalidated(), 0, "a quota is not a credential problem");
    }

    /// PRE-MANIFEST. A carrier 404s a waybill its shipper has not handed over
    /// yet, for hours or a weekend. Counting those against the retirement cap
    /// retires live parcels — five of them is ~30 hours at the default cadence —
    /// so inside the grace window the attempt is stamped and nothing else.
    #[tokio::test]
    async fn a_not_found_inside_the_grace_period_does_not_retire_a_young_row() {
        let (store, poller, mock, row) = harness(Err(TrackError::NotFound), Utc::now());
        let poller = poller.with_not_found_grace(Delta::hours(NOT_FOUND_GRACE_HOURS));
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);
        poller.poll_one(&client, &mut state, &row).await;

        let after = stored(&store, poller.account_id);
        assert!(after.last_polled_at.is_some(), "the attempt still rotates");
        assert_eq!(
            after.poll_failures, 0,
            "a label the carrier has not scanned yet is not a phantom"
        );
    }

    /// Past the window, the cap does its real job again.
    #[tokio::test]
    async fn a_not_found_past_the_grace_period_counts_against_the_row() {
        let old = Utc::now() - Delta::hours(NOT_FOUND_GRACE_HOURS + 1);
        let (store, poller, mock, row) = harness(Err(TrackError::NotFound), old);
        let poller = poller.with_not_found_grace(Delta::hours(NOT_FOUND_GRACE_HOURS));
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);
        poller.poll_one(&client, &mut state, &row).await;

        let after = stored(&store, poller.account_id);
        assert!(after.last_polled_at.is_some());
        assert_eq!(after.poll_failures, 1);
    }

    /// A 404 is an ANSWER: whatever it says about the parcel, it proves the API
    /// is up, so it clears a transient run rather than adding to it.
    #[tokio::test]
    async fn an_answered_call_clears_the_transient_ladder() {
        let (_store, poller, mock, row) = harness(Err(TrackError::NotFound), noon());
        let client: Arc<dyn CarrierClient> = mock.clone();
        let mut state = CarrierState::new(None);
        state.transient_run = TRANSIENT_TOLERANCE - 1;
        state.backoff = Some(BACKOFF_CAP);

        poller.poll_one(&client, &mut state, &row).await;
        assert_eq!(state.transient_run, 0);
        assert_eq!(state.backoff, None);
        assert!(!state.gated(Instant::now()));
    }

    // ---- construction ----------------------------------------------------

    #[test]
    fn from_config_is_none_without_carrier_credentials() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut config = Config::default();
        assert!(
            ShipmentPoller::from_config(store.clone(), 1, &config)
                .expect("no carriers is not an error")
                .is_none(),
            "no credentials must mean no task and no outbound traffic"
        );

        // A cadence knob alone does not conjure a poller.
        config.carriers.poll_interval_hours = 1;
        assert!(
            ShipmentPoller::from_config(store.clone(), 1, &config)
                .expect("knobs alone are not an error")
                .is_none()
        );

        config.carriers.dhl = Some(crate::config::DhlCarrierConfig {
            api_key: Some("key".into()),
            daily_cap: 42,
        });
        let poller = ShipmentPoller::from_config(store, 1, &config)
            .expect("building")
            .expect("a configured carrier builds a poller");
        assert_eq!(poller.enabled_carriers(), vec!["dhl".to_string()]);
        assert_eq!(poller.knobs.dhl_daily_cap, 42);
        assert_eq!(poller.knobs.poll_interval, Delta::hours(1));
        // Drawn once, inside the window, so a fleet restart fans out.
        assert!(poller.startup_delay >= STARTUP_DELAY_MIN);
        assert!(
            poller.startup_delay
                <= STARTUP_DELAY_MIN + Duration::from_secs(STARTUP_DELAY_SPAN_SECS)
        );
    }

    #[test]
    fn the_startup_delay_lands_in_its_window_and_is_not_a_constant() {
        let drawn: std::collections::HashSet<u64> =
            (0..64).map(|_| startup_delay().as_secs()).collect();
        assert!(
            drawn.iter().all(|d| (30..=120).contains(d)),
            "startup delay left its window: {drawn:?}"
        );
        assert!(drawn.len() > 1, "the startup delay never varies");
    }
}
