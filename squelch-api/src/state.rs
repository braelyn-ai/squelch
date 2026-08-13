//! Shared handler state for the human door.

use std::sync::Arc;

use squelch_core::config::{Config, CredentialBackend, OAuthClientConfig, Stage2CapSources};
use squelch_core::credentials::{
    CredentialKind, CredentialStore, FileCredentialStore, KeyringCredentialStore,
};
use squelch_core::store::SqliteStore;
use squelch_core::triage::rule_infer::RuleInferClient;
use squelch_core::types::AccountId;

/// State threaded through every `/client/*` handler and the auth middleware.
/// Cheap to clone: `Arc`s plus small copies.
#[derive(Clone)]
pub struct ApiState {
    pub(crate) store: Arc<SqliteStore>,
    pub(crate) account_id: AccountId,
    /// The MASTER token from `SQUELCH_API_TOKEN`, checked before any issued
    /// device token and first-class forever: it is the self-host operator's way
    /// in when the store holds no usable credential, including the store whose
    /// last device token was just revoked.
    ///
    /// `None` when the variable is unset or blank. That is NOT a refusal to
    /// serve: the door comes up and every request 401s until
    /// `squelchd token issue` or a pairing claim mints a device token. An
    /// exported-but-empty variable must not become a credential the door would
    /// accept, and must not keep the door shut either.
    pub(crate) master_token: Option<Arc<str>>,
    /// The WRITE-bound credential store, present only when write credentials are
    /// configured. The ONLY handle to the write token in the process; handlers
    /// load it per-request and never retain it. `None` => actions return 403.
    pub(crate) write_creds: Option<Arc<dyn CredentialStore>>,
    /// Gmail API base override for the write client. Tests only; production
    /// leaves it `None` so live traffic always hits the real API.
    pub(crate) write_api_base: Option<String>,
    /// Per-MTok input price (USD) behind `est_cost_usd_today` in
    /// `/client/stats`. Wire the operator's value in with
    /// [`ApiState::with_stage2_prices`].
    pub(crate) stage2_price_in_per_mtok: f64,
    /// Per-MTok output price (USD) for `est_cost_usd_today`.
    pub(crate) stage2_price_out_per_mtok: f64,
    /// The configured Stage-2 model id, surfaced as a label on `/client/usage`.
    pub(crate) stage2_model: Arc<str>,
    /// The configured Stage-2 provider label, surfaced on `/client/usage`.
    /// `None` when not explicitly configured.
    pub(crate) stage2_provider: Option<Arc<str>>,
    /// The CONFIG/ENV-layer Stage-2 daily caps (already env-folded), reported by
    /// `/client/triage-config` when no runtime override row exists.
    pub(crate) stage2_thread_daily_cap: u32,
    pub(crate) stage2_sender_daily_cap: u32,
    pub(crate) stage2_global_daily_cap: u32,
    /// Whether each config/env-layer cap came from the built-in default or from
    /// config/env, so triage-config can report "default" vs "config" (the
    /// "override" case is decided at read time from `app_settings`).
    pub(crate) stage2_cap_sources: Stage2CapSources,
    /// The configured Stage-1 (small) model id, surfaced on
    /// `/client/triage-config` and `/client/usage`.
    pub(crate) stage1_model: Arc<str>,
    /// Per-MTok Stage-1 prices for the triage-config estimator + usage cost.
    pub(crate) stage1_price_in_per_mtok: f64,
    pub(crate) stage1_price_out_per_mtok: f64,
    /// The CONFIG/ENV-layer Stage-1 GLOBAL daily cap (Stage-1's only scope),
    /// reported by `/client/triage-config` when no runtime override row exists.
    pub(crate) stage1_global_daily_cap: u32,
    /// Manual-refresh signal shared with the Gmail sync loop, fired by
    /// `POST /client/refresh`. `None` when no sync loop is wired in, which the
    /// endpoint reports as `triggered: false` rather than faking a poke.
    pub(crate) refresh: Option<Arc<tokio::sync::Notify>>,
    /// Wake channel for `GET /client/events`, the SAME sender attached to the
    /// store. THE PAYLOAD IS ONLY A HINT — the `events` table is the truth and
    /// every connection re-reads past its own cursor — so a missed or lagged
    /// message costs latency, never an event. `None` => the live path never
    /// fires; replay still works.
    ///
    /// [`SqliteStore::attach_event_notifier`]: squelch_core::store::SqliteStore::attach_event_notifier
    pub(crate) event_notifier: Option<tokio::sync::broadcast::Sender<i64>>,
    /// The daemon's shutdown signal. An SSE stream is infinite and axum's
    /// graceful shutdown waits for open connections, so without this one
    /// resident client would hold squelchd open forever.
    pub(crate) shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    /// PUBLIC base URL that reaches this daemon's `/t/:token` pixel route, from
    /// `[tracking] base_url` / `SQUELCH_TRACK_URL`. THE FEATURE FLAG for
    /// minting: `None` means no send is ever tracked, whatever the client asks
    /// for, and `/client/tracking-config` reports `configured: false`.
    pub(crate) tracking_base_url: Option<Arc<str>>,
    /// Slots for in-flight tracking-pixel writes. The pixel is one of the two
    /// unauthenticated routes that touch the store, so its share of the store
    /// mutex is capped rather than left to whoever floods it.
    pub(crate) pixel_slots: Arc<tokio::sync::Semaphore>,
    /// Slots for in-flight pairing claims — the OTHER unauthenticated route onto
    /// the same store mutex, capped for the same reason. Bounded by waiting
    /// rather than by refusing: a claim that failed fast because the daemon was
    /// busy would be a response that differs from the uniform 401.
    pub(crate) pair_slots: Arc<tokio::sync::Semaphore>,
    /// Slots for in-flight DEVICE-token verifications. The master-token compare
    /// is process-local and unbounded, but any `sqd_`-shaped guess from a caller
    /// with no credential reaches the store, so that branch queues here.
    pub(crate) device_auth_slots: Arc<tokio::sync::Semaphore>,
    /// The control plane's origin, from `[console] sso_url` /
    /// `SQUELCH_CONSOLE_SSO_URL`, behind the console's "Continue with Google"
    /// link.
    ///
    /// THE FEATURE FLAG for console SSO, and the whole self-host/hosted split in
    /// one field: hosted tenants are provisioned with it, a self-host never sets
    /// it, and without it the login page simply does not render the button. It is
    /// a LINK TARGET and nothing more: no trust flows from here, because whatever
    /// comes back is a pairing code the store adjudicates on its own terms.
    pub(crate) console_sso_url: Option<Arc<str>>,
    /// THE ESCAPE HATCH, from `[console] allow_insecure_cookie` /
    /// `SQUELCH_CONSOLE_ALLOW_INSECURE_COOKIE`: serve the console session cookie
    /// without `Secure` on an origin that is not loopback.
    ///
    /// OFF BY DEFAULT AND MEANT TO STAY THERE. The cookie IS a device token, so
    /// turning this on puts a live credential on the wire in the clear for
    /// anything on the path to take. It exists for the self-host running the
    /// console over plain http on a LAN, who would otherwise have a console that
    /// simply does not work (a browser will not store a `Secure` cookie from
    /// `http://`). When it is on, the login page says so where the user can see
    /// it. See [`crate::console`].
    ///
    /// NOT YET CONSULTED (issue #46): squelchd never plumbs the config knob in,
    /// and [`crate::console`]'s `Site::secure()` decides from the request
    /// authority alone — so today the hatch parses but does nothing. Drop the
    /// `dead_code` allow when wiring it.
    #[allow(dead_code)]
    pub(crate) console_allow_insecure_cookie: bool,
    /// Per-client buckets over the two console routes that reach the store with
    /// no credential in front of them. See
    /// [`crate::console::CONSOLE_SIGNIN_REQUESTS_PER_MINUTE`].
    pub(crate) console_signin_limiter: Arc<std::sync::Mutex<ConsoleRateLimiter>>,
    /// The cheap-model handle behind rule-disposition inference: when a rule
    /// write omits `disposition`, the owner's `want` sentence is classified with
    /// this. `None` (no API key configured, and every test harness) means
    /// inference resolves to `filtered`, which is the safe default. HUMAN DOOR
    /// ONLY, like every other rule write.
    pub(crate) rule_infer: Option<Arc<RuleInferClient>>,
    /// The carrier poller's kick handle plus the carriers it was built over,
    /// behind `POST /client/shipments/poll`. `None` when no poller runs — which
    /// is the resting state of every daemon with no carrier credentials — and
    /// the endpoint reports that as `kicked: false` rather than an error.
    pub(crate) shipment_poll: Option<ShipmentPoll>,
}

/// What the human door needs to force a carrier pass: the poller's
/// [`Notify`](tokio::sync::Notify) and the carrier slugs it was built over, so
/// the response can say WHICH carriers a kick will reach. Cheap to clone, like
/// every other field on [`ApiState`].
#[derive(Clone)]
pub(crate) struct ShipmentPoll {
    pub(crate) kick: Arc<tokio::sync::Notify>,
    /// Enabled carrier slugs, sorted and deduplicated at build time.
    pub(crate) carriers: Arc<[String]>,
}

/// A fixed-rate token bucket per client address, for the console's
/// unauthenticated sign-in paths.
///
/// A SIBLING of `squelch_control::ratelimit`'s, not a copy of convenience: the
/// arithmetic and the pruning are the same because they were right there, and
/// what differs is what it hangs off. In-memory, per-process abuse dampening
/// rather than a quota system: a restart forgives everyone.
///
/// WHO A "CLIENT" IS depends on how the router was served. The peer address is
/// read from axum's `ConnectInfo` when the listener attached one; when it did
/// not, every request meters under one shared key. That is coarse, never a
/// bypass: the worst case is one bucket for the whole process, and no caller can
/// choose their own key (no forwarded header is read here, because one that is
/// believed is a way to mint unlimited fresh identities).
pub(crate) struct ConsoleRateLimiter {
    buckets: std::collections::HashMap<std::net::IpAddr, (f64, std::time::Instant)>,
    capacity: f64,
    refill_per_sec: f64,
    last_prune: Option<std::time::Instant>,
}

/// Buckets idle longer than this are dropped on the next prune. An idle bucket
/// has refilled to capacity anyway, so forgetting it costs nothing.
const BUCKET_IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// A prune is a full-map scan, so it runs on a clock rather than per request.
const BUCKET_PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// Hard ceiling on tracked buckets: a client rotating addresses faster than
/// [`BUCKET_IDLE_TTL`] leaves nothing to reclaim, so the TTL alone does not
/// bound the map. Forgetting a bucket only ever FORGIVES a client, so evicting
/// is safe; an unbounded map would not be.
const MAX_BUCKETS: usize = 16_384;

impl ConsoleRateLimiter {
    /// `per_minute` sustained requests, with a burst allowance of the same size.
    pub(crate) fn per_minute(per_minute: f64) -> Self {
        Self {
            buckets: std::collections::HashMap::new(),
            capacity: per_minute,
            refill_per_sec: per_minute / 60.0,
            last_prune: None,
        }
    }

    /// Charge one request against `ip`. `false` means the bucket is empty.
    pub(crate) fn check(&mut self, ip: std::net::IpAddr, now: std::time::Instant) -> bool {
        self.maintain(now);
        let capacity = self.capacity;
        let refill = self.refill_per_sec;
        let bucket = self.buckets.entry(ip).or_insert((capacity, now));
        // `saturating_duration_since` cannot panic on a non-monotonic clock; the
        // worst case is no refill for this request.
        let elapsed = now.saturating_duration_since(bucket.1).as_secs_f64();
        bucket.0 = (bucket.0 + elapsed * refill).min(capacity);
        bucket.1 = now;
        if bucket.0 >= 1.0 {
            bucket.0 -= 1.0;
            true
        } else {
            false
        }
    }

    /// Everything expensive lives here so `check`'s common path stays one hash
    /// lookup.
    fn maintain(&mut self, now: std::time::Instant) {
        let due = self
            .last_prune
            .is_none_or(|t| now.saturating_duration_since(t) >= BUCKET_PRUNE_EVERY);
        if due {
            self.buckets
                .retain(|_, (_, last)| now.saturating_duration_since(*last) < BUCKET_IDLE_TTL);
            self.last_prune = Some(now);
        }
        if self.buckets.len() >= MAX_BUCKETS {
            // Nothing left to reclaim on the clock, so drop the lot rather than
            // grow: every client is forgiven, which is the safe direction.
            self.buckets.clear();
        }
    }
}

/// Capacity of the `GET /client/events` wake channel. THE PAYLOAD IS ONLY A HINT
/// — every stream re-reads the `events` table past its own cursor — so this only
/// bounds how far a slow reader falls behind before taking the harmless Lagged
/// path.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Build the event wake channel and attach it to the store, returning the sender
/// for [`ApiState::with_event_notifier`] and for any other reader that wants to
/// `subscribe` (the APNs pusher does). One helper so every binary uses the same
/// capacity and the same single attach point.
///
/// Subscribe BEFORE handing the sender to the human door, so nothing appended
/// during startup is missed.
pub fn attach_event_channel(
    store: &SqliteStore,
) -> Result<tokio::sync::broadcast::Sender<i64>, squelch_core::CoreError> {
    let (tx, _) = tokio::sync::broadcast::channel::<i64>(EVENT_CHANNEL_CAPACITY);
    store.attach_event_notifier(tx.clone())?;
    Ok(tx)
}

/// Why [`ApiState`] could not be constructed.
///
/// A MISSING BEARER IS NOT IN HERE ANY MORE. The door used to refuse to build
/// without `SQUELCH_API_TOKEN`; it now serves and answers 401 until a credential
/// exists, because pairing is how the first credential arrives and pairing needs
/// a door to talk to. What is left is resolving the account, which is still a
/// store operation and still fallible.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Core(#[from] squelch_core::CoreError),
}

impl ApiState {
    /// Build state from an explicit store + account + master token.
    ///
    /// INFALLIBLE, and deliberately so: there is no configuration this can be
    /// handed that should stop the human door coming up. A blank master token is
    /// simply no master token (see [`ApiState::master_token`]).
    pub fn new(
        store: Arc<SqliteStore>,
        account_id: AccountId,
        master_token: impl Into<String>,
    ) -> Self {
        let master_token = master_token.into();
        // Default prices so /client/stats always has a cost basis; the bin
        // overrides them from the loaded config.
        let s2 = squelch_core::config::Stage2Config::default();
        Self {
            store,
            account_id,
            master_token: Some(master_token.trim())
                .filter(|t| !t.is_empty())
                .map(Arc::from),
            write_creds: None,
            write_api_base: None,
            stage2_price_in_per_mtok: s2.price_in_per_mtok,
            stage2_price_out_per_mtok: s2.price_out_per_mtok,
            stage2_model: Arc::from(s2.model.as_str()),
            stage2_provider: None,
            stage2_thread_daily_cap: s2.thread_daily_cap,
            stage2_sender_daily_cap: s2.sender_daily_cap,
            stage2_global_daily_cap: s2.global_daily_cap,
            stage2_cap_sources: Stage2CapSources::default(),
            stage1_model: {
                let s1 = squelch_core::config::Stage1Config::default();
                Arc::from(s1.model.as_str())
            },
            stage1_price_in_per_mtok: squelch_core::config::Stage1Config::default()
                .price_in_per_mtok,
            stage1_price_out_per_mtok: squelch_core::config::Stage1Config::default()
                .price_out_per_mtok,
            stage1_global_daily_cap: squelch_core::config::Stage1Config::default().global_daily_cap,
            refresh: None,
            event_notifier: None,
            shutdown: None,
            tracking_base_url: None,
            pixel_slots: Arc::new(tokio::sync::Semaphore::new(crate::tracking::PIXEL_CONCURRENCY)),
            pair_slots: Arc::new(tokio::sync::Semaphore::new(crate::pair::PAIR_CONCURRENCY)),
            device_auth_slots: Arc::new(tokio::sync::Semaphore::new(
                crate::auth::DEVICE_AUTH_CONCURRENCY,
            )),
            console_sso_url: None,
            console_allow_insecure_cookie: false,
            console_signin_limiter: Arc::new(std::sync::Mutex::new(ConsoleRateLimiter::per_minute(
                crate::console::CONSOLE_SIGNIN_REQUESTS_PER_MINUTE,
            ))),
            rule_infer: None,
            shipment_poll: None,
        }
    }

    /// The master token, if one is configured. `None` means the door accepts
    /// ONLY issued device tokens, which is the hosted posture and the resting
    /// state of a self-host that has finished pairing.
    pub(crate) fn master_token(&self) -> Option<&str> {
        self.master_token.as_deref()
    }

    /// Share the sync loop's manual-refresh [`Notify`](tokio::sync::Notify).
    /// Wire the SAME handle you pass to [`SyncEngine::with_refresh`]; without it
    /// `POST /client/refresh` is a no-op (`triggered: false`).
    ///
    /// [`SyncEngine::with_refresh`]: squelch_core::sync::SyncEngine::with_refresh
    pub fn with_refresh(mut self, refresh: Arc<tokio::sync::Notify>) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// Share the carrier poller's kick [`Notify`](tokio::sync::Notify) and the
    /// carriers it was built over, enabling `POST /client/shipments/poll`. Wire
    /// the SAME handle the poller handed out
    /// ([`ShipmentPoller::kick_handle`]), with
    /// [`ShipmentPoller::enabled_carriers`] as `carriers`.
    ///
    /// NOT CALLING THIS IS A SUPPORTED STATE, not a misconfiguration: carrier
    /// polling is BYOK, so a daemon with no carrier credentials runs no poller
    /// and the endpoint answers `kicked: false` with an empty carrier list.
    ///
    /// The list is sorted and deduplicated here rather than trusted, so the
    /// response shape does not depend on how the caller assembled it.
    ///
    /// [`ShipmentPoller::kick_handle`]: squelch_core::carriers::poller::ShipmentPoller::kick_handle
    /// [`ShipmentPoller::enabled_carriers`]: squelch_core::carriers::poller::ShipmentPoller::enabled_carriers
    pub fn with_shipment_poll_kick(
        mut self,
        kick: Arc<tokio::sync::Notify>,
        carriers: Vec<String>,
    ) -> Self {
        let mut carriers = carriers;
        carriers.sort();
        carriers.dedup();
        self.shipment_poll = Some(ShipmentPoll {
            kick,
            carriers: carriers.into(),
        });
        self
    }

    /// Share the event-notification broadcast so `GET /client/events` wakes
    /// instead of polling. Wire the SAME sender you pass to
    /// [`SqliteStore::attach_event_notifier`]; without it the SSE endpoint
    /// replays and holds the connection, it just never goes live.
    ///
    /// [`SqliteStore::attach_event_notifier`]: squelch_core::store::SqliteStore::attach_event_notifier
    pub fn with_event_notifier(mut self, tx: tokio::sync::broadcast::Sender<i64>) -> Self {
        self.event_notifier = Some(tx);
        self
    }

    /// Share the process shutdown signal so open SSE streams end when the daemon
    /// stops. REQUIRED wherever `with_graceful_shutdown` is used: a never-ending
    /// stream otherwise keeps the server from finishing its drain.
    pub fn with_shutdown(mut self, shutdown: tokio::sync::watch::Receiver<bool>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// The event-notification broadcast, if one was wired in.
    pub(crate) fn event_notifier(&self) -> Option<&tokio::sync::broadcast::Sender<i64>> {
        self.event_notifier.as_ref()
    }

    /// Set the public base URL the read-tracking pixel is minted against. A
    /// blank value is treated as unset: an exported-but-empty setting must not
    /// put `/t/<token>` into outbound mail. A value without an http(s) scheme is
    /// unset too — it would ride into the HTML as a RELATIVE url and resolve
    /// against whatever the recipient's mail client happens to be, so a typo
    /// must disable tracking rather than mint a pixel that points anywhere.
    pub fn with_tracking_base_url(mut self, base_url: Option<String>) -> Self {
        self.tracking_base_url = base_url
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .filter(|u| {
                let lower = u.to_ascii_lowercase();
                lower.starts_with("http://") || lower.starts_with("https://")
            })
            .map(|u| Arc::from(u.as_str()));
        self
    }

    /// The tracking base URL, already trimmed of a trailing slash. `None` =>
    /// tracking is not configured and no send may mint a token.
    pub(crate) fn tracking_base_url(&self) -> Option<&str> {
        self.tracking_base_url.as_deref()
    }

    /// Set the control plane's origin behind the console's Google sign-in link,
    /// from `SQUELCH_CONSOLE_SSO_URL`.
    ///
    /// Treated exactly like [`ApiState::with_tracking_base_url`]: blank is unset
    /// (an exported-but-empty variable must not become configuration), a trailing
    /// slash is trimmed, and a value with no http(s) scheme is unset, because it
    /// would ride into the page as a RELATIVE href and resolve against the tenant
    /// origin rather than the control plane.
    pub fn with_console_sso_url(mut self, url: Option<String>) -> Self {
        self.console_sso_url = url
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .filter(|u| {
                let lower = u.to_ascii_lowercase();
                lower.starts_with("https://") || lower.starts_with("http://")
            })
            .map(|u| Arc::from(u.as_str()));
        self
    }

    /// The control plane's origin, if console SSO is configured. `None` => the
    /// console renders the pasted-code form only, which is the self-host posture.
    pub(crate) fn console_sso_url(&self) -> Option<&str> {
        self.console_sso_url.as_deref()
    }

    /// Slots for concurrent tracking-pixel writes; see
    /// [`crate::tracking::PIXEL_CONCURRENCY`].
    pub(crate) fn pixel_slots(&self) -> &tokio::sync::Semaphore {
        &self.pixel_slots
    }

    /// Slots for concurrent pairing claims; see [`crate::pair::PAIR_CONCURRENCY`].
    pub(crate) fn pair_slots(&self) -> &tokio::sync::Semaphore {
        &self.pair_slots
    }

    /// Slots for concurrent device-token verifications; see
    /// [`crate::auth::DEVICE_AUTH_CONCURRENCY`].
    pub(crate) fn device_auth_slots(&self) -> &tokio::sync::Semaphore {
        &self.device_auth_slots
    }

    /// Attach the cheap-model handle that infers a rule's disposition from the
    /// owner's plain-English `want`. `None` disables inference gracefully:
    /// rule writes that omit `disposition` are stored `filtered`, which is what
    /// the daemon does when no API key is configured.
    ///
    /// The classifier only ever sees the OWNER'S SENTENCE. See
    /// [`squelch_core::triage::rule_infer`].
    pub fn with_rule_inference(mut self, client: Option<RuleInferClient>) -> Self {
        self.rule_infer = client.map(Arc::new);
        self
    }

    /// The rule-disposition classifier, if one is configured.
    pub(crate) fn rule_infer(&self) -> Option<&RuleInferClient> {
        self.rule_infer.as_deref()
    }

    /// Set the Stage-2 model + provider labels surfaced on `/client/usage`, so
    /// the usage page shows what model produced the spend.
    pub fn with_stage2_model(
        mut self,
        model: impl Into<String>,
        provider: Option<String>,
    ) -> Self {
        self.stage2_model = Arc::from(model.into().as_str());
        self.stage2_provider = provider.map(|p| Arc::from(p.as_str()));
        self
    }

    /// Override the Stage-2 per-MTok prices behind `est_cost_usd_today`, so
    /// switching model reflects in the surfaced cost.
    pub fn with_stage2_prices(mut self, price_in_per_mtok: f64, price_out_per_mtok: f64) -> Self {
        self.stage2_price_in_per_mtok = price_in_per_mtok;
        self.stage2_price_out_per_mtok = price_out_per_mtok;
        self
    }

    /// Set the CONFIG/ENV-layer Stage-2 daily caps + their sources, which
    /// `/client/triage-config` reports until a runtime override row is written.
    pub fn with_stage2_caps(
        mut self,
        thread_daily_cap: u32,
        sender_daily_cap: u32,
        global_daily_cap: u32,
        sources: Stage2CapSources,
    ) -> Self {
        self.stage2_thread_daily_cap = thread_daily_cap;
        self.stage2_sender_daily_cap = sender_daily_cap;
        self.stage2_global_daily_cap = global_daily_cap;
        self.stage2_cap_sources = sources;
        self
    }

    /// Set the Stage-1 model + per-MTok prices + config/env GLOBAL daily cap
    /// surfaced on `/client/triage-config` and `/client/usage`. The cap SOURCE
    /// arrives separately, via [`ApiState::with_stage2_caps`].
    pub fn with_stage1_config(
        mut self,
        model: impl Into<String>,
        price_in_per_mtok: f64,
        price_out_per_mtok: f64,
        global_daily_cap: u32,
    ) -> Self {
        self.stage1_model = Arc::from(model.into().as_str());
        self.stage1_price_in_per_mtok = price_in_per_mtok;
        self.stage1_price_out_per_mtok = price_out_per_mtok;
        self.stage1_global_daily_cap = global_daily_cap;
        self
    }

    /// TEST HOOK: attach a write-credential store AND a mock Gmail API base so
    /// action handlers run end-to-end without live Gmail. Never used in production.
    #[doc(hidden)]
    pub fn with_write_test_harness(
        mut self,
        write_creds: Arc<dyn CredentialStore>,
        api_base: String,
    ) -> Self {
        self.write_creds = Some(write_creds);
        self.write_api_base = Some(api_base);
        self
    }

    /// Attach a WRITE-bound credential store, enabling the action endpoints.
    /// A distinct opt-in step so write capability is never wired in implicitly;
    /// without it the state has no path to any write token and actions 403.
    pub fn with_write_store(mut self, write_creds: Arc<dyn CredentialStore>) -> Self {
        self.write_creds = Some(write_creds);
        self
    }

    /// The ONLY place squelch-api constructs a write credential. Bound to
    /// [`CredentialKind::Write`], so it can never yield the read token.
    pub fn with_write_credentials(
        self,
        backend: CredentialBackend,
        account_email: String,
        credentials_path: std::path::PathBuf,
        client: OAuthClientConfig,
    ) -> Self {
        let account_id = self.account_id;
        let creds: Arc<dyn CredentialStore> = match backend {
            CredentialBackend::Keyring => Arc::new(KeyringCredentialStore::new_with_kind(
                account_id,
                account_email,
                CredentialKind::Write,
                client,
            )),
            CredentialBackend::File => Arc::new(FileCredentialStore::new_with_kind(
                account_id,
                account_email,
                CredentialKind::Write,
                credentials_path,
                client,
            )),
        };
        self.with_write_store(creds)
    }

    /// The WRITE-bound credential store, if action capability is enabled.
    pub(crate) fn write_creds(&self) -> Option<&Arc<dyn CredentialStore>> {
        self.write_creds.as_ref()
    }

    /// The Gmail API base override (tests only); `None` in production.
    pub(crate) fn write_api_base(&self) -> Option<&str> {
        self.write_api_base.as_deref()
    }

    /// Build state, resolving the account email to an id and reading the master
    /// token from `SQUELCH_API_TOKEN`.
    ///
    /// An unset variable is a supported configuration, not an error: the door
    /// serves, every request 401s, and the operator mints the first credential
    /// with `squelchd token issue` or `squelchd pair`. Announcing that here
    /// rather than leaving the operator to discover it as a wall of 401s.
    pub fn from_env(store: Arc<SqliteStore>, account_email: &str) -> Result<Self, StateError> {
        let token = std::env::var("SQUELCH_API_TOKEN").unwrap_or_default();
        if token.trim().is_empty() {
            eprintln!(
                "squelch-api: SQUELCH_API_TOKEN is unset, so the human door accepts only issued \
                 device tokens; run `squelchd pair` (or `squelchd token issue`) to mint one."
            );
        }
        let account_id = store.ensure_account(account_email)?;
        // Console SSO is env-only and hosted-only: the warden sets it when it
        // provisions a tenant, and a self-host leaves it unset, which is what
        // keeps the Google button off a page that has no control plane behind it.
        Ok(Self::new(store, account_id, token)
            .with_console_sso_url(std::env::var("SQUELCH_CONSOLE_SSO_URL").ok()))
    }

    /// THE one way a binary turns a loaded [`Config`] into serving state:
    /// [`ApiState::from_env`] plus every config-derived knob (Stage-2 prices,
    /// model/provider labels, daily caps + their sources, the Stage-1 block) and
    /// the write credential. Both bins call this, so a knob added here reaches
    /// both instead of silently misconfiguring the one that was not updated.
    ///
    /// Action endpoints are enabled ONLY when OAuth client credentials are
    /// configured; the store built here is bound to [`CredentialKind::Write`]
    /// and the sync/triage read path never touches it. Without it, actions 403.
    ///
    /// Process-level plumbing stays with the caller: the manual-refresh
    /// [`Notify`](tokio::sync::Notify), the event notifier, and the shutdown
    /// watch are all shared with other tasks the caller owns.
    pub fn from_config(
        store: Arc<SqliteStore>,
        account_email: &str,
        cfg: &Config,
        cap_sources: Stage2CapSources,
    ) -> Result<Self, StateError> {
        let state = Self::from_env(store, account_email)?
            .with_stage2_prices(cfg.stage2.price_in_per_mtok, cfg.stage2.price_out_per_mtok)
            .with_stage2_model(
                cfg.stage2.model.clone(),
                cfg.stage2.stage2_provider.map(|p| p.as_str().to_string()),
            )
            .with_stage2_caps(
                cfg.stage2.thread_daily_cap,
                cfg.stage2.sender_daily_cap,
                cfg.stage2.global_daily_cap,
                cap_sources,
            )
            .with_stage1_config(
                cfg.stage1.model.clone(),
                cfg.stage1.price_in_per_mtok,
                cfg.stage1.price_out_per_mtok,
                cfg.stage1.global_daily_cap,
            )
            .with_tracking_base_url(cfg.tracking.base_url.clone())
            // Rule-disposition inference rides the SAME key/provider the triage
            // stages resolve and the Stage-1 model; no key => `None` => rules
            // that omit a disposition are stored filtered.
            .with_rule_inference(RuleInferClient::from_config(cfg));

        Ok(match cfg.oauth_client() {
            Ok(client) => state.with_write_credentials(
                cfg.credential_backend,
                account_email.to_string(),
                cfg.resolve_credentials_path(),
                client,
            ),
            Err(_) => {
                eprintln!(
                    "squelch-api: no OAuth client configured; action endpoints will return 403 \
                     (set client_id/client_secret and run `squelchd auth --write`)"
                );
                state
            }
        })
    }

    /// The active account id (exposed for tests / embedders).
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }
}
