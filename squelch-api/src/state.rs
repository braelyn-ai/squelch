//! Shared handler state for the human door.

use std::sync::Arc;

use squelch_core::config::{CredentialBackend, OAuthClientConfig, Stage2CapSources};
use squelch_core::credentials::{
    CredentialKind, CredentialStore, FileCredentialStore, KeyringCredentialStore,
};
use squelch_core::store::SqliteStore;
use squelch_core::types::AccountId;

/// State threaded through every `/client/*` handler and the auth middleware.
///
/// Cheap to clone (it is `Arc`s + small copies), matching how squelch-mcp clones
/// its server per session. Holds the store, the active account, and the bearer
/// token the auth layer compares against.
#[derive(Clone)]
pub struct ApiState {
    pub(crate) store: Arc<SqliteStore>,
    pub(crate) account_id: AccountId,
    /// The static shared secret. Guaranteed non-empty by construction: both
    /// constructors reject an empty/unset token, so the auth layer never has to
    /// decide whether to "serve open".
    pub(crate) token: Arc<str>,
    /// The WRITE-bound credential store, present only when write credentials are
    /// configured. This is the ONLY handle to the write token in the process;
    /// action handlers load the token from it per-request and never retain it.
    /// `None` => action endpoints return 403 (run `squelchd auth --write`).
    pub(crate) write_creds: Option<Arc<dyn CredentialStore>>,
    /// Gmail API base URL override for the write client. `None` uses the real
    /// Gmail base. Set only in tests (point at a local mock server); production
    /// never sets it, so live traffic always hits the real API.
    pub(crate) write_api_base: Option<String>,
    /// Per-MTok input price (USD) used to compute `est_cost_usd_today` in
    /// `/client/stats`. Defaults to the Stage2Config default (claude-haiku-4-5);
    /// wire the operator's config value in with [`ApiState::with_stage2_prices`].
    pub(crate) stage2_price_in_per_mtok: f64,
    /// Per-MTok output price (USD) for `est_cost_usd_today`. See
    /// [`ApiState::stage2_price_in_per_mtok`].
    pub(crate) stage2_price_out_per_mtok: f64,
    /// The configured Stage-2 model id (e.g. `claude-haiku-4-5`), surfaced as a
    /// label on `/client/usage`. Defaults to the Stage2Config default.
    pub(crate) stage2_model: Arc<str>,
    /// The configured Stage-2 provider label (e.g. `anthropic`/`openai`), if
    /// known, surfaced on `/client/usage`. `None` when not explicitly configured.
    pub(crate) stage2_provider: Option<Arc<str>>,
    /// The CONFIG/ENV-layer Stage-2 daily caps (already env-folded), reported by
    /// `/client/triage-config` when no runtime override row exists. Default to
    /// the Stage2Config defaults; wire the operator's config in with
    /// [`ApiState::with_stage2_caps`].
    pub(crate) stage2_thread_daily_cap: u32,
    pub(crate) stage2_sender_daily_cap: u32,
    pub(crate) stage2_global_daily_cap: u32,
    /// Whether each config/env-layer cap came from the built-in default or from
    /// config/env, so the triage-config endpoint can report "default" vs
    /// "config" (the "override" case is decided at read time from `app_settings`).
    pub(crate) stage2_cap_sources: Stage2CapSources,
    /// The configured Stage-1 model id (small model), surfaced on
    /// `/client/triage-config` and `/client/usage`.
    pub(crate) stage1_model: Arc<str>,
    /// Per-MTok Stage-1 prices for the triage-config estimator + usage cost.
    pub(crate) stage1_price_in_per_mtok: f64,
    pub(crate) stage1_price_out_per_mtok: f64,
    /// The CONFIG/ENV-layer Stage-1 GLOBAL daily cap (Stage-1's only scope),
    /// reported by `/client/triage-config` when no runtime override row exists.
    pub(crate) stage1_global_daily_cap: u32,
    /// Manual-refresh signal shared with the Gmail sync loop. `POST /client/refresh`
    /// fires it to wake the poll loop for an immediate Gmail poll. `None` when no
    /// sync loop is wired in (standalone `squelch-api` bin / tests): the endpoint
    /// then reports `triggered: false` rather than pretending to have poked one.
    pub(crate) refresh: Option<Arc<tokio::sync::Notify>>,
    /// Wake channel for `GET /client/events`. The SAME sender is attached to the
    /// store ([`SqliteStore::attach_event_notifier`]), which pokes it on every
    /// real `append_event`. THE PAYLOAD IS ONLY A HINT — the `events` table is
    /// the source of truth and every SSE connection re-reads past its own cursor
    /// — so a missed or lagged message costs latency, never an event. `None`
    /// (standalone bin / tests) just means the live path never fires; replay
    /// still works.
    ///
    /// [`SqliteStore::attach_event_notifier`]: squelch_core::store::SqliteStore::attach_event_notifier
    pub(crate) event_notifier: Option<tokio::sync::broadcast::Sender<i64>>,
    /// The daemon's shutdown signal. An SSE stream is infinite by construction,
    /// and axum's graceful shutdown waits for open connections — so without this
    /// one resident client would hold squelchd open forever. Every stream selects
    /// on it and ends cleanly when it flips. `None` => nothing to wait on.
    pub(crate) shutdown: Option<tokio::sync::watch::Receiver<bool>>,
}

/// Why [`ApiState`] could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// `SQUELCH_API_TOKEN` was unset or empty. We refuse to serve rather than
    /// serve the human door open.
    #[error(
        "SQUELCH_API_TOKEN is unset or empty; squelch-api refuses to serve without a bearer token"
    )]
    MissingToken,

    #[error(transparent)]
    Core(#[from] squelch_core::CoreError),
}

impl ApiState {
    /// Build state from an explicit store + account + token. The token must be
    /// non-empty or this returns [`StateError::MissingToken`].
    pub fn new(
        store: Arc<SqliteStore>,
        account_id: AccountId,
        token: impl Into<String>,
    ) -> Result<Self, StateError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(StateError::MissingToken);
        }
        // Prices default to the Stage2Config defaults so /client/stats always has
        // a sane cost basis; the bin overrides them from the loaded config.
        let s2 = squelch_core::config::Stage2Config::default();
        Ok(Self {
            store,
            account_id,
            token: Arc::from(token.as_str()),
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
        })
    }

    /// Share the sync loop's manual-refresh [`Notify`](tokio::sync::Notify) so
    /// `POST /client/refresh` can wake it for an immediate Gmail poll. Wire the
    /// SAME handle here that you pass to [`SyncEngine::with_refresh`]. Without it
    /// the refresh endpoint is a no-op (`triggered: false`).
    ///
    /// [`SyncEngine::with_refresh`]: squelch_core::sync::SyncEngine::with_refresh
    pub fn with_refresh(mut self, refresh: Arc<tokio::sync::Notify>) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// Share the event-notification broadcast so `GET /client/events` wakes on a
    /// new event instead of polling. Wire the SAME sender here that you pass to
    /// [`SqliteStore::attach_event_notifier`]; without it the SSE endpoint still
    /// replays and still holds the connection open, it just never goes live.
    ///
    /// [`SqliteStore::attach_event_notifier`]: squelch_core::store::SqliteStore::attach_event_notifier
    pub fn with_event_notifier(mut self, tx: tokio::sync::broadcast::Sender<i64>) -> Self {
        self.event_notifier = Some(tx);
        self
    }

    /// Share the process shutdown signal so open SSE streams end when the daemon
    /// stops. REQUIRED anywhere `axum::serve(..).with_graceful_shutdown(..)` is
    /// used: a never-ending stream would otherwise keep the server from ever
    /// finishing its drain. Wire the same [`watch`](tokio::sync::watch) channel
    /// the sync loop gets.
    pub fn with_shutdown(mut self, shutdown: tokio::sync::watch::Receiver<bool>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// The event-notification broadcast, if one was wired in.
    pub(crate) fn event_notifier(&self) -> Option<&tokio::sync::broadcast::Sender<i64>> {
        self.event_notifier.as_ref()
    }

    /// Set the Stage-2 model + provider labels surfaced on `/client/usage`. Wire
    /// this from the loaded [`squelch_core::config::Stage2Config`] alongside the
    /// prices so the usage page shows what model produced the spend.
    pub fn with_stage2_model(
        mut self,
        model: impl Into<String>,
        provider: Option<String>,
    ) -> Self {
        self.stage2_model = Arc::from(model.into().as_str());
        self.stage2_provider = provider.map(|p| Arc::from(p.as_str()));
        self
    }

    /// Override the Stage-2 per-MTok prices used for `est_cost_usd_today`. Wire
    /// this from the loaded [`squelch_core::config::Stage2Config`] so switching
    /// `model` (and thus its config prices) reflects in the surfaced cost.
    pub fn with_stage2_prices(mut self, price_in_per_mtok: f64, price_out_per_mtok: f64) -> Self {
        self.stage2_price_in_per_mtok = price_in_per_mtok;
        self.stage2_price_out_per_mtok = price_out_per_mtok;
        self
    }

    /// Set the CONFIG/ENV-layer Stage-2 daily caps + their sources surfaced on
    /// `/client/triage-config`. Wire these from
    /// [`squelch_core::config::Config::load_with_cap_sources`] so the endpoint
    /// reports the operator's config values (and "default" vs "config") until a
    /// runtime override row is written.
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
    /// surfaced on `/client/triage-config` and `/client/usage`. Wire this from the
    /// loaded [`squelch_core::config::Stage1Config`] (the daily-cap SOURCE is
    /// threaded via [`ApiState::with_stage2_caps`]'s `Stage2CapSources`).
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

    /// TEST HOOK: attach a raw write-credential store AND a mock Gmail API base
    /// so action handlers can be exercised end-to-end without live Gmail. Never
    /// called in production (the base override defaults to `None`).
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
    /// Without this the state has no path to any write token and action
    /// endpoints return 403. Kept a distinct opt-in step so the write capability
    /// is never wired in implicitly.
    pub fn with_write_store(mut self, write_creds: Arc<dyn CredentialStore>) -> Self {
        self.write_creds = Some(write_creds);
        self
    }

    /// Build and attach a WRITE-bound credential store for the given backend.
    /// This is the ONLY place squelch-api constructs a write credential; it is
    /// bound to [`CredentialKind::Write`] so it can never yield the read token.
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

    /// Build state resolving the account email to an id (creating the row if
    /// needed) and reading the bearer token from `SQUELCH_API_TOKEN`. Refuses to
    /// build if the token is unset/empty.
    pub fn from_env(store: Arc<SqliteStore>, account_email: &str) -> Result<Self, StateError> {
        let token = std::env::var("SQUELCH_API_TOKEN").unwrap_or_default();
        if token.trim().is_empty() {
            return Err(StateError::MissingToken);
        }
        let account_id = store.ensure_account(account_email)?;
        Self::new(store, account_id, token)
    }

    /// The active account id (exposed for tests / embedders).
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }
}
