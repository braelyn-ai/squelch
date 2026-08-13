//! Gmail sync: REST + polling. REST, not IMAP — the read-only `gmail.readonly`
//! scope works over REST and IMAP XOAUTH2 rejects it. First run backfills
//! `backfill_days` of INBOX+SENT and records the `historyId`; after that
//! `history.list` polls `messageAdded` under BOTH labels — INBOX for incoming
//! mail, SENT for whatever the user wrote from another client — a 404 falling
//! back to a full catch-up.
//! Tokens, headers and bodies are NEVER logged; ingest seals first (SECURITY.md).

pub mod html;
pub mod ingest;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::config::{Config, ResolvedLlm, Stage2Provider};
use crate::credentials::CredentialStore;
use crate::error::{CoreError, Result};
use crate::metrics::{GmailErrorKind, Stage1Verdict, Stage2Verdict, SyncMetrics};
use crate::store::{ContactEntry, Stage2CapOverrides, Store, SyncState};
use crate::sync::ingest::{
    RawFetched, collect_mailboxes, format_recipients, ingest_with_rules, is_robot_address,
};
use crate::triage::events;
use crate::triage::extract::{self, CategoryExtractor, RowAction, banking, marketing};
use crate::triage::stage1_llm::{self, HEURISTIC_ONLY};
use crate::triage::stage2::{self, ClassifyOutcome, RowContext};
use crate::triage::{stage1_sealed_guard, stage2_sealed_guard};
use crate::types::{AccountId, SenderRule, Sensitivity};

/// Gmail REST base for the authenticated user. Fixed; not user-tunable. `pub`
/// so squelch-api's write path targets the same host from one definition.
pub const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// The INBOX system label. `pub` because the write path archives by removing
/// exactly this label.
pub const LABEL_INBOX: &str = "INBOX";
/// The SENT system label.
const LABEL_SENT: &str = "SENT";

/// The single `sync_state` row key for the REST engine's historyId cursor.
const HISTORY_KEY: &str = "history";

/// `sync_state` row key for the one-time Sent-contacts harvest's done flag
/// (`last_uid >= 1` = complete; absent/0 = redo on next daemon start).
const SENT_CONTACTS_KEY: &str = "sent_contacts";

/// `sync_state` row key for the one-time sent-RECIPIENTS backfill's done flag,
/// with the same semantics as [`SENT_CONTACTS_KEY`]. Distinct from it because
/// the two sweeps fill different columns and either can complete alone.
const SENT_RECIPIENTS_KEY: &str = "sent_recipients";

/// How many rows the recipients backfill claims from the store per batch. Only a
/// memory bound: the pass loops until the queue is empty.
const SENT_RECIPIENTS_BATCH: u32 = 500;

/// `wake_budget.thread_id` sentinel for the per-account-per-day Stage-2 budget.
/// Gmail thread ids are hex, so no real thread can collide with it.
const GLOBAL_BUDGET_KEY: &str = "__global__";

/// Prefix for the per-SENDER-per-day Stage-2 budget key in the same
/// `wake_budget` table (`thread_id = "sender:<addr>"`). Gmail thread ids are
/// hex, so this collides with neither a real thread nor `__global__`.
const SENDER_BUDGET_PREFIX: &str = "sender:";

/// `wake_budget.thread_id` sentinel for the Stage-1 daily budget. Stage-1 must
/// see every email, so a global cap is its only scope; the key is distinct from
/// the Stage-2 sentinel so the two stages' daily counts never collide.
const STAGE1_GLOBAL_BUDGET_KEY: &str = "__stage1_global__";

/// Which sync path an ingest batch is on. Decides ONE thing: whether a
/// notification-worthy verdict may append an `events` row. Backfill never
/// notifies (a fresh install must not fire a hundred pushes for a month of
/// already-read mail); every incremental path may, kept safe by the freshness
/// window plus the one-event-per-message key even when `catch_up()` re-scans
/// the whole backfill window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestOrigin {
    /// First-run backfill. Structurally silent.
    Backfill,
    /// A history walk or a catch-up re-scan: mail that may be genuinely new.
    Incremental,
}

/// Model id stamped on a row older than `stage2_max_age_days`: marked processed
/// WITHOUT a model call, keeping its Stage-1 values, so it neither spends budget
/// nor sits queued forever.
const STALE_SKIP_MODEL: &str = "stale-skip";

/// Reconnect / retry backoff bounds for the outer driver loop.
const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// Collapse an untrusted header-derived string to printable ASCII before it
/// reaches the log: control chars, ANSI escapes and log-forging newlines become
/// `.`, and the result is capped so a pathological header can't flood the log.
fn sanitize_ascii(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '.'
            }
        })
        .take(max)
        .collect()
}

/// A stable, non-reversible tag (`sender#<12 hex of sha256>`) for a sender
/// address. `from_addr` is untrusted header-derived PII and must never be
/// logged; the tag still correlates repeated notices for the same sender.
fn redact_sender(from_addr: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(from_addr.as_bytes());
    let mut hex = String::with_capacity(12);
    for b in digest.iter().take(6) {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("sender#{hex}")
}

/// Decode a base64url (Gmail `format=raw`) payload into RFC822 bytes. Gmail
/// usually omits padding; both padded and unpadded input are accepted. Errors
/// are surfaced without content so one bad message can't poison the batch.
pub fn decode_raw_b64url(s: &str) -> Result<Vec<u8>> {
    // MEMORY GUARD: bound peak ingest memory ourselves rather than inheriting
    // Gmail's ~50MB limit. b64 length upper-bounds the decoded size.
    const MAX_RAW_BYTES: usize = 64 * 1024 * 1024;
    let t = s.trim();
    if t.len() / 4 * 3 > MAX_RAW_BYTES {
        return Err(CoreError::InvalidInput(
            "raw message exceeds the 64MB ingest bound".to_string(),
        ));
    }
    // Try no-pad first (Gmail's usual shape), then the padded variant.
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(t)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(t))
        .map_err(|e| CoreError::InvalidInput(format!("base64url decode failed: {e}")))
}

/// Decide whether the incremental poll can proceed or a fresh catch-up is
/// required. `expired` reflects an HTTP 404 from `history.list` (Gmail drops
/// history older than ~a week); a 0/absent `cursor` means first run. Pure, so
/// the 404-fallback path is unit-testable without a network.
pub fn history_poll_decision(cursor: Option<u64>, expired: bool) -> HistoryDecision {
    match cursor {
        Some(id) if id > 0 && !expired => HistoryDecision::Incremental(id),
        _ => HistoryDecision::FullCatchUp,
    }
}

/// The outcome of [`history_poll_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryDecision {
    /// Poll `history.list` starting from this historyId.
    Incremental(u64),
    /// historyId is absent or expired: do a fresh backfill-window catch-up.
    FullCatchUp,
}

/// Advance a historyId cursor to the max of itself and every `historyId`
/// observed in a page — never backwards. Pure, so it is unit-testable.
pub fn advance_history_cursor(current: u64, observed: impl IntoIterator<Item = u64>) -> u64 {
    observed.into_iter().fold(current, u64::max)
}

/// Drop from `ids` every id also in `claimed`, preserving order. Used to hand the
/// SENT batch only what the INBOX batch did NOT already ingest: a self-addressed
/// message carries both labels, and the store's message upsert writes
/// `is_sent = excluded.is_sent` on conflict, so re-ingesting it as sent would
/// hide the visible copy. Pure, so the precedence rule is unit-testable.
fn subtract_ids(ids: Vec<String>, claimed: &[String]) -> Vec<String> {
    if claimed.is_empty() {
        return ids;
    }
    let claimed: std::collections::HashSet<&str> = claimed.iter().map(|s| s.as_str()).collect();
    ids.into_iter()
        .filter(|id| !claimed.contains(id.as_str()))
        .collect()
}

// ---- Gmail REST response shapes (only the fields we consume) ---------------
// These model the GMAIL API's own JSON, never squelch's client-facing wire
// contracts. squelch-api's write path deserializes the same Gmail resources, so
// the shared ones are `pub` here and defined exactly once.

#[derive(Debug, Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListMessagesResp {
    #[serde(default)]
    messages: Vec<MessageRef>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// A Gmail `users.messages.get` resource, across every format squelch asks for:
/// `format=raw` fills `raw`, `format=metadata` fills `payload.headers`, and a
/// field the requested format omits simply stays `None`/empty.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    /// base64url of the full RFC822 message (present with `format=raw`).
    #[serde(default)]
    pub raw: Option<String>,
    /// Milliseconds since epoch as a decimal string (Gmail's `internalDate`).
    #[serde(default)]
    pub internal_date: Option<String>,
    /// MIME structure (present with `format=metadata`/`full`); squelch reads
    /// only its headers.
    #[serde(default)]
    pub payload: Option<MessagePayload>,
}

/// The `payload` object of a Gmail message; only `headers` is consumed.
#[derive(Debug, Default, Deserialize)]
pub struct MessagePayload {
    #[serde(default)]
    pub headers: Vec<MessageHeader>,
}

/// A single Gmail `payload.headers[]` entry. Also what the contacts-seeding
/// tests build to exercise header parsing via `synthesize_rfc822_headers`.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileResp {
    history_id: String,
}

/// A Gmail `users.labels.get` resource; only the unread pair is read, and both
/// counters default to 0 because Gmail omits them on a label with nothing in it.
///
/// `id` is required for exactly that reason: with only defaulted fields, ANY
/// 200 body — `{}`, an error envelope, some proxy's interstitial — would decode
/// to a confident `(0, 0)` and overwrite real counts. Requiring the one field
/// every label resource carries turns those bodies into decode errors, which
/// the caller keeps its last known counts through.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelResp {
    /// Never read; its presence is the check.
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    messages_unread: i64,
    #[serde(default)]
    threads_unread: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryListResp {
    #[serde(default)]
    history: Vec<HistoryRecord>,
    #[serde(default)]
    next_page_token: Option<String>,
    /// The newest historyId as of this response.
    #[serde(default)]
    history_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    messages_added: Vec<HistoryMessageAdded>,
}

#[derive(Debug, Deserialize)]
struct HistoryMessageAdded {
    message: MessageRef,
}

/// Parse a decimal string historyId; malformed input yields 0 (treated as
/// "unknown", forcing a full catch-up rather than a panic).
fn parse_history_id(s: &str) -> u64 {
    s.trim().parse::<u64>().unwrap_or(0)
}

/// Which daily cap a budget-exhausted notice is about; each is rate-limited to
/// once per UTC day (see [`SyncEngine::warn_days`]).
#[derive(Debug, Clone, Copy)]
enum CapKind {
    Thread,
    Sender,
    Global,
    Stage1Global,
}

/// The preamble every LLM pass shares: resolved credentials, runtime cap
/// overrides, and the pass clock. `stale_cutoff` is deliberately the Stage-2
/// max-age for every pass, so all stages age rows out together.
struct PassSetup<'a> {
    api_key: &'a str,
    provider: Stage2Provider,
    /// The endpoint every classify call this pass makes posts to — resolved
    /// once at startup, gateway override already folded in.
    url: &'a str,
    caps: Stage2CapOverrides,
    /// UTC date key (`YYYY-MM-DD`) for the budget rows; one value per pass.
    day: String,
    stale_cutoff: DateTime<Utc>,
}

/// Outcome of a check-then-increment budget gate.
enum BudgetGate {
    Proceed,
    /// Cap hit: every remaining row this cycle is blocked.
    Exhausted,
    /// Budget read or increment failed: skip this row, try the next.
    SkipRow,
}

/// The last UTC day (`YYYY-MM-DD`) each cap kind's notice was emitted; re-armed
/// when the day rolls over, so a capped account logs once a day, not per poll.
#[derive(Default)]
struct WarnDays {
    thread: Option<String>,
    sender: Option<String>,
    global: Option<String>,
    stage1_global: Option<String>,
}

/// Everything the sync loop needs, resolved once at startup.
pub struct SyncEngine<S: Store, C: CredentialStore + ?Sized> {
    store: Arc<S>,
    creds: Arc<C>,
    account_id: AccountId,
    /// The account's own email; passed to ingest so the user's own address is
    /// excluded from the Sent-derived contacts table.
    account_email: String,
    config: Config,
    http: reqwest::Client,
    /// LLM key + provider + endpoint URL, resolved once at startup. `None`
    /// disables LLM triage gracefully: rows stay queued, one stderr notice,
    /// sync continues. The key is never logged.
    stage2_llm: Option<ResolvedLlm>,
    /// Embedder OVERRIDE; usually `None`, with [`SyncEngine::embedder`] falling
    /// back to the store's. Resolving per tick is what lets a LATE-attached
    /// embedder be picked up without a restart.
    embedder: Option<Arc<dyn crate::embed::Embedder>>,
    /// Manual-refresh signal: notifying it wakes the sleeping poll loop early.
    /// Coalescing is intentional — several pokes during one in-flight poll
    /// collapse into a single extra tick.
    refresh: Arc<tokio::sync::Notify>,
    /// Per-cap-kind last-warned UTC day. In-memory only; a restart re-arms them,
    /// and one fresh notice on restart is acceptable.
    warn_days: std::sync::Mutex<WarnDays>,
    /// Set while the INBOX unread fetch is failing, so a persistent failure
    /// (revoked scope, Gmail outage) says so ONCE instead of once per poll. The
    /// next success re-arms it.
    unread_warned: AtomicBool,
    /// Scrape-facing counters. Its own registry unless the daemon shares one
    /// via [`SyncEngine::with_metrics`], so an engine built anywhere (tests, the
    /// contacts harvest, `run`) still records rather than branching on absence.
    metrics: Arc<SyncMetrics>,
    /// Gmail REST base for every call this engine makes. Always
    /// [`GMAIL_API_BASE`] in production; the tests point it at a loopback mock so
    /// the walk/cursor rules are asserted on the wire rather than on a struct.
    api_base: String,
}

impl<S: Store + 'static, C: CredentialStore + 'static + ?Sized> SyncEngine<S, C> {
    pub fn new(
        store: Arc<S>,
        creds: Arc<C>,
        account_id: AccountId,
        account_email: String,
        config: Config,
    ) -> Self {
        // Timeouts keep a hung connection from wedging the poll loop.
        // Redirects are REFUSED deliberately: this client carries credentials
        // (Gmail bearer token, LLM x-api-key) and reqwest re-sends custom
        // headers cross-host on a redirect. Gmail's API does not redirect, so
        // this matches the repo's other Google-facing clients.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build");
        // Absence => graceful disable, one notice, no key material logged.
        let stage2_llm = config.stage2.resolve_llm();
        if stage2_llm.is_none() {
            eprintln!(
                "squelch: no Stage-2 API key set (SQUELCH_STAGE2_API_KEY / ANTHROPIC_API_KEY / \
                 OPENAI_API_KEY) — Stage-2 LLM triage disabled (ambiguous rows stay queued; \
                 sync continues)"
            );
        }
        Self {
            store,
            creds,
            account_id,
            account_email,
            config,
            http,
            stage2_llm,
            embedder: None,
            refresh: Arc::new(tokio::sync::Notify::new()),
            warn_days: std::sync::Mutex::new(WarnDays::default()),
            unread_warned: AtomicBool::new(false),
            metrics: SyncMetrics::new(),
            api_base: GMAIL_API_BASE.to_string(),
        }
    }

    /// Share the daemon's [`SyncMetrics`] so what this engine records is what
    /// `/metrics` serves: create ONE at startup and hand a clone to each side.
    /// Without it the engine still counts, into a registry nobody scrapes.
    pub fn with_metrics(mut self, metrics: Arc<SyncMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Point every Gmail call at `base` instead of [`GMAIL_API_BASE`]. Test-only:
    /// a build that could be aimed at an arbitrary host would be a bearer-token
    /// exfiltration primitive.
    #[cfg(test)]
    fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }

    /// Share a manual-refresh [`Notify`](tokio::sync::Notify) so the human door's
    /// `POST /client/refresh` can wake the poll loop between intervals: create
    /// ONE at daemon startup and hand a clone to each side. Without it the engine
    /// still polls on its own interval, just never early.
    pub fn with_refresh(mut self, refresh: Arc<tokio::sync::Notify>) -> Self {
        self.refresh = refresh;
        self
    }

    /// Attach an [`Embedder`](crate::embed::Embedder) OVERRIDE, for callers that
    /// build one eagerly and want it used even if the store's copy differs.
    /// Usually unnecessary — [`SyncEngine::embedder`] falls back to the store's —
    /// and absence keeps sync fully functional, just writing no vectors.
    pub fn with_embedder(mut self, embedder: Arc<dyn crate::embed::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// The EFFECTIVE embedder for this tick: the override, else whatever is
    /// attached to the store RIGHT NOW — so an embedder attached in the
    /// background after startup is picked up without a restart. Until then this
    /// is `None`, ingest skips the vector write, and the backfill pass fills in.
    fn embedder(&self) -> Option<Arc<dyn crate::embed::Embedder>> {
        self.embedder.clone().or_else(|| self.store.embedder())
    }

    /// Authenticated GET returning parsed JSON. A 404 surfaces as
    /// [`CoreError::NotFound`] so callers can branch on it (the
    /// expired-historyId fallback). Header and body are NEVER logged.
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let resp = self.send_get(url).await?;
        match resp.status() {
            s if s.is_success() => resp
                .json::<T>()
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("gmail json decode: {e}"))),
            // NOT an error metric: this is the expired-historyId signal the
            // caller recovers from with a catch-up, and counting a routine
            // self-heal would make the errors family unalertable.
            StatusCode::NOT_FOUND => Err(CoreError::NotFound),
            s => {
                // Classified HERE, the last place the status is typed AND the
                // body is still readable: a 403 is a quota refusal or an
                // authorization one depending only on the reason string Google
                // puts in the body, and once this is an `anyhow` chain nothing
                // downstream can tell them apart. The body is read for that one
                // decision and never logged.
                let body = resp.text().await.unwrap_or_default();
                self.metrics
                    .record_gmail_error(classify_gmail_status(s.as_u16(), &body));
                Err(CoreError::Other(anyhow::anyhow!(
                    "gmail api status {}",
                    s.as_u16()
                )))
            }
        }
    }

    /// Send a GET with a Bearer token, retrying once on 401 with a fresh token.
    async fn send_get(&self, url: &str) -> Result<reqwest::Response> {
        let token = self.token_for_request().await?;
        let resp = self.bearer_get(url, &token.access_token).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            // Redacted: the fact of a retry, never token/header content.
            eprintln!("squelch: gmail 401; refreshing token and retrying once");
            let token = self.token_for_request().await?;
            return self.bearer_get(url, &token.access_token).await;
        }
        Ok(resp)
    }

    /// The access token for one Gmail call. A credential failure never reaches
    /// a status code — the refresh exchange is what failed, `invalid_grant`
    /// included — so it is counted as `auth` on the typed variant here rather
    /// than string-matched out of an error chain later.
    async fn token_for_request(&self) -> Result<crate::credentials::OAuthToken> {
        self.creds.token(self.account_id).await.inspect_err(|e| {
            if matches!(e, CoreError::Credential(_)) {
                self.metrics.record_gmail_error(GmailErrorKind::Auth);
            }
        })
    }

    async fn bearer_get(&self, url: &str, access_token: &str) -> Result<reqwest::Response> {
        self.http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| {
                // No status ever arrived: DNS, TLS, connect or the client's own
                // timeout. Distinct from `http` because the fix is different.
                self.metrics.record_gmail_error(GmailErrorKind::Network);
                CoreError::Other(anyhow::anyhow!("gmail request: {e}"))
            })
    }

    /// One full lifecycle: backfill if needed (establishing the historyId), then
    /// poll until an error bubbles up (caller retries with backoff) or shutdown.
    async fn run_once(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        eprintln!("squelch: gmail REST sync starting for <redacted account>");

        // First run (no history cursor) => full backfill + seed contacts.
        let cursor = self.load_history_cursor()?;
        if cursor.is_none() {
            self.backfill().await?;
            // Stage-1, then Stage-2 over what Stage-1 escalated, then the
            // specialist extractors over each row's FINAL category.
            self.stage1_pass().await;
            self.stage2_pass().await;
            self.extract_pass().await;
        }

        self.backfill_missing_vectors().await;

        self.poll_loop(shutdown).await
    }

    /// First-run backfill: INBOX bodies over the window, then SENT headers to
    /// seed contacts, then persist the account's current historyId.
    async fn backfill(&self) -> Result<()> {
        let since = self.backfill_since();

        // INBOX bodies.
        let q = format!("newer_than:{}d", self.config.sync.backfill_days);
        let inbox_ids = self.list_message_ids(LABEL_INBOX, Some(&q)).await?;
        // Backfill NEVER notifies (see `IngestOrigin`).
        let n = self
            .fetch_raw_and_ingest(&inbox_ids, /* is_sent */ false, IngestOrigin::Backfill)
            .await?;
        eprintln!("squelch: backfilled {n} INBOX messages");

        // SENT bodies, not just headers, so semantic recall covers WHAT THE USER
        // WROTE. Contacts still come from To/Cc; the row is stored neutral
        // (tier=noise, importance=0) and the is_sent exclusions keep it out of
        // triage/updates/search.
        let sent_ids = self.list_message_ids(LABEL_SENT, Some(&q)).await?;
        let seeded = self
            .fetch_raw_and_ingest(&sent_ids, /* is_sent */ true, IngestOrigin::Backfill)
            .await?;
        eprintln!("squelch: backfilled {seeded} SENT messages (bodies for recall + contacts)");

        // Establish the historyId cursor from the profile.
        let history_id = self.fetch_profile_history_id().await?;
        self.store_history_cursor(history_id)?;
        eprintln!("squelch: history cursor established (backfill window from {since})");
        Ok(())
    }

    /// Poll `history.list` every `poll_secs`, ingesting `messageAdded` INBOX
    /// messages and advancing the cursor. A poll batch IS the coalesced batch.
    async fn poll_loop(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let interval = Duration::from_secs(self.config.sync.poll_secs);
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            // BEFORE the walk, not after: the walk is what can return Err and
            // bounce the whole lifecycle, and a mailbox stuck in that retry loop
            // should still have a fresh unread count for the human door.
            self.refresh_inbox_unread().await;
            self.poll_once().await?;
            // THE freshness stamp for a healthy daemon: `run()` only records one
            // on its way out, and a mailbox that polls happily for a month never
            // goes there — staleness alerts would fire on the boot timestamp.
            self.metrics.stamp_sync_success();
            // Both stages refine within the same cycle; neither can crash the
            // loop (all failures are handled internally).
            self.stage1_pass().await;
            self.stage2_pass().await;
            // AFTER both stages, so it sees each row's FINAL category (Stage-2
            // may have overwritten Stage-1's).
            self.extract_pass().await;

            // Per-tick, so an embedder attached after startup catches up on rows
            // ingested before it was ready, no restart needed.
            self.backfill_missing_vectors().await;

            // A refresh poke that arrives mid-poll is not lost: `Notify` stores
            // one permit, so the next `notified()` returns at once and the loop
            // runs one more immediate tick.
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.refresh.notified() => {
                    eprintln!("squelch: manual refresh — polling now");
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }

    /// A single poll tick: consult the cursor, either run the incremental
    /// history walk or (on absent/expired cursor) a fresh catch-up.
    async fn poll_once(&self) -> Result<()> {
        let cursor = self.load_history_cursor()?;
        match history_poll_decision(cursor, false) {
            HistoryDecision::Incremental(start) => {
                match self.history_walk(start).await {
                    Ok(()) => Ok(()),
                    // Expired historyId (404): fall back to a fresh catch-up.
                    Err(CoreError::NotFound) => {
                        eprintln!("squelch: historyId expired; falling back to catch-up");
                        self.catch_up().await
                    }
                    Err(e) => Err(e),
                }
            }
            HistoryDecision::FullCatchUp => self.catch_up().await,
        }
    }

    /// One incremental poll: TWO label-filtered `history.list` walks from the
    /// SAME start cursor, INBOX then SENT, with the cursor committed ONCE at the
    /// end at the max historyId either walk observed.
    ///
    /// The SENT walk is what makes mail the user writes from Gmail web or their
    /// phone land here at all: such a message never carries INBOX, so an
    /// INBOX-only walk is blind to it, and the api's post-send echo covers only
    /// what was sent through the daemon itself. Both walks start from the same
    /// `start_history_id` because one Gmail cursor spans the whole mailbox.
    ///
    /// ORDER IS LOAD-BEARING. A self-addressed message carries both labels and so
    /// appears in both walks, and the store's message upsert writes
    /// `is_sent = excluded.is_sent` on conflict — last writer wins — so a SENT
    /// copy landing second would flip the row to `is_sent = 1` and drop it out of
    /// every band. The INBOX walk therefore runs FIRST and its ids are subtracted
    /// from the SENT batch, making the visible copy authoritative no matter what
    /// the conflict clause does.
    ///
    /// Propagates [`CoreError::NotFound`] on an expired historyId so the caller
    /// can fall back to a catch-up.
    async fn history_walk(&self, start_history_id: u64) -> Result<()> {
        let (inbox_ids, inbox_cursor) = self
            .history_walk_label(start_history_id, LABEL_INBOX)
            .await?;
        if !inbox_ids.is_empty() {
            let n = self
                .fetch_raw_and_ingest(&inbox_ids, false, IngestOrigin::Incremental)
                .await?;
            eprintln!("squelch: ingested {n} new INBOX messages");
        }

        // The SENT half is best-effort AGAINST THE CURSOR: any failure holds the
        // cursor where it is and returns, so the next poll re-walks both labels
        // from the same start rather than stepping over INBOX history that the
        // successful half already covered. Re-walking costs a few fetches and
        // changes nothing: `UNIQUE(account_id, gmail_msg_id)` collapses the
        // message rows and `UNIQUE(message_id)` on `events` collapses the
        // notifications.
        let (sent_ids, sent_cursor) =
            match self.history_walk_label(start_history_id, LABEL_SENT).await {
                Ok(v) => v,
                // An expired historyId is not a SENT-specific failure: the cursor
                // itself is gone and only a catch-up can recover it.
                Err(CoreError::NotFound) => return Err(CoreError::NotFound),
                Err(e) => {
                    eprintln!("squelch: sent history walk failed ({e}); holding the cursor");
                    return Ok(());
                }
            };

        let sent_only = subtract_ids(sent_ids, &inbox_ids);
        if !sent_only.is_empty() {
            // `Incremental`, like the INBOX half: the silence guarantee for the
            // user's own mail is NOT the origin but `events::worthy_kind`, which
            // returns `None` for an `is_sent` row, and the 'n/a' stage markers
            // ingest stamps, which keep it out of both LLM queues.
            match self
                .fetch_raw_and_ingest(&sent_only, true, IngestOrigin::Incremental)
                .await
            {
                Ok(n) => eprintln!("squelch: ingested {n} new SENT messages"),
                Err(e) => {
                    eprintln!("squelch: sent ingest failed ({e}); holding the cursor");
                    return Ok(());
                }
            }
        }

        self.store_history_cursor(advance_history_cursor(
            start_history_id,
            [inbox_cursor, sent_cursor],
        ))?;
        Ok(())
    }

    /// Walk `history.list` from `start_history_id` under ONE label filter,
    /// returning the deduped `messageAdded` ids and the max historyId observed.
    /// Persists nothing: the caller owns the single cursor commit that covers
    /// every label. Propagates [`CoreError::NotFound`] on an expired historyId.
    async fn history_walk_label(
        &self,
        start_history_id: u64,
        label: &str,
    ) -> Result<(Vec<String>, u64)> {
        let mut cursor = start_history_id;
        let mut page_token: Option<String> = None;
        let mut new_ids: Vec<String> = Vec::new();

        loop {
            let mut url = format!(
                "{}/history?startHistoryId={start_history_id}\
                 &historyTypes=messageAdded&labelId={label}",
                self.api_base
            );
            if let Some(tok) = &page_token {
                url.push_str(&format!("&pageToken={tok}"));
            }
            let page: HistoryListResp = self.get_json(&url).await?;

            // Advance the cursor from every observed historyId (records + the
            // page-level newest id).
            let observed = page
                .history
                .iter()
                .filter_map(|r| r.id.as_deref().map(parse_history_id))
                .chain(page.history_id.as_deref().map(parse_history_id));
            cursor = advance_history_cursor(cursor, observed);

            for rec in &page.history {
                for added in &rec.messages_added {
                    new_ids.push(added.message.id.clone());
                }
            }

            match page.next_page_token {
                Some(tok) => page_token = Some(tok),
                None => break,
            }
        }

        // Dedup ids (a message can appear across pages); order is irrelevant —
        // dedup at the store keys on (account_id, gmail_msg_id).
        new_ids.sort_unstable();
        new_ids.dedup();
        Ok((new_ids, cursor))
    }

    /// Fresh catch-up: re-run the backfill-window INBOX fetch, then the SENT
    /// fetch over the same window (dedup makes both idempotent), and
    /// re-establish the historyId. Used on first run's poll and on an
    /// expired-history 404 — exactly the cases where the history walk cannot
    /// account for the gap, so the sent half has to be re-listed too or mail the
    /// user wrote from another client during it is lost for good.
    async fn catch_up(&self) -> Result<()> {
        let q = format!("newer_than:{}d", self.config.sync.backfill_days);
        let ids = self.list_message_ids(LABEL_INBOX, Some(&q)).await?;
        // A catch-up may carry genuinely new mail, so it is allowed to notify.
        // What keeps the whole-window re-scan from storming is the freshness
        // window in `triage::events` plus the one-event-per-message key.
        let n = self
            .fetch_raw_and_ingest(&ids, false, IngestOrigin::Incremental)
            .await?;
        if n > 0 {
            eprintln!("squelch: catch-up ingested {n} INBOX messages");
        }

        // INBOX first, and its ids subtracted, for the same reason the history
        // walk orders itself that way: a self-addressed message is in both
        // listings and its visible copy must win the unique-key race.
        let sent_ids = self.list_message_ids(LABEL_SENT, Some(&q)).await?;
        let sent_only = subtract_ids(sent_ids, &ids);
        let sent_n = self
            .fetch_raw_and_ingest(&sent_only, true, IngestOrigin::Incremental)
            .await?;
        if sent_n > 0 {
            eprintln!("squelch: catch-up ingested {sent_n} SENT messages");
        }

        let history_id = self.fetch_profile_history_id().await?;
        self.store_history_cursor(history_id)?;
        Ok(())
    }

    // ---- Sent-contacts harvest --------------------------------------------

    /// ONE-TIME sweep of the ENTIRE Sent mailbox — no window — seeding the
    /// contacts table from every To/Cc the account has ever written to.
    /// `format=metadata` with only To/Cc requested: headers, never bodies, so
    /// deep history costs one light call per sent message. Merged with MAX
    /// semantics ([`Store::merge_harvested_contacts`]) so overlap with the
    /// ingest path's own seeding and an interrupted re-run cannot double-count.
    ///
    /// Completion is a `sync_state` flag (`mailbox = 'sent_contacts'`); an
    /// interrupted pass leaves it unset and the next daemon start redoes the
    /// sweep from scratch — idempotent, just re-paged.
    pub async fn harvest_sent_contacts(&self) -> Result<()> {
        let done = self
            .store
            .sync_state(self.account_id, SENT_CONTACTS_KEY)?
            .map(|s| s.last_uid >= 1)
            .unwrap_or(false);
        if done {
            return Ok(());
        }

        let ids = self.list_message_ids(LABEL_SENT, None).await?;
        eprintln!(
            "squelch: sent-contacts harvest scanning {} sent messages (headers only)",
            ids.len()
        );

        let self_addr = self.account_email.trim().to_ascii_lowercase();
        // addr -> aggregate. Counting per message occurrence matches the ingest
        // path's per-send bump closely enough for ranking.
        let mut agg: std::collections::HashMap<String, ContactEntry> =
            std::collections::HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            let url = format!(
                "{}/messages/{id}?format=metadata\
                 &metadataHeaders=To&metadataHeaders=Cc",
                self.api_base
            );
            let msg: GmailMessage = self.get_json(&url).await?;
            let sent_at = parse_internal_date(msg.internal_date.as_deref());
            let headers = msg.payload.map(|p| p.headers).unwrap_or_default();
            // Through mail-parser via header synthesis, so grouped lists, quoted
            // display names and RFC2047 encoding parse exactly as ingest does.
            let blob = synthesize_rfc822_headers(&headers);
            let Some(parsed) = mail_parser::MessageParser::default().parse(blob.as_bytes()) else {
                continue;
            };
            for list in [parsed.to(), parsed.cc()].into_iter().flatten() {
                for mailbox in list.iter() {
                    let Some(addr) = mailbox.address() else {
                        continue;
                    };
                    let addr = addr.trim().to_ascii_lowercase();
                    // Same gate as ingest seeding: never the account itself,
                    // never robot/unsubscribe traffic.
                    if addr.is_empty() || addr == self_addr || is_robot_address(&addr) {
                        continue;
                    }
                    let name = mailbox
                        .name()
                        .map(|n| n.trim().to_string())
                        .filter(|n| !n.is_empty());
                    let entry = agg.entry(addr.clone()).or_insert_with(|| ContactEntry {
                        addr,
                        display_name: None,
                        sent_count: 0,
                        last_sent_at: None,
                    });
                    entry.sent_count += 1;
                    if entry.display_name.is_none() {
                        entry.display_name = name;
                    }
                    if sent_at > entry.last_sent_at {
                        entry.last_sent_at = sent_at;
                    }
                }
            }
            if (i + 1) % 1000 == 0 {
                eprintln!(
                    "squelch: sent-contacts harvest {}/{} messages scanned",
                    i + 1,
                    ids.len()
                );
            }
        }

        let batch: Vec<ContactEntry> = agg.into_values().collect();
        self.store
            .merge_harvested_contacts(self.account_id, &batch)?;
        self.store.set_sync_state(
            self.account_id,
            SENT_CONTACTS_KEY,
            &SyncState {
                uidvalidity: 1,
                last_uid: 1,
            },
        )?;
        eprintln!(
            "squelch: sent-contacts harvest complete — {} unique recipients",
            batch.len()
        );
        Ok(())
    }

    // ---- Sent-recipients backfill -----------------------------------------

    /// ONE-TIME sweep filling `messages.to_addrs` on sent rows ingested before
    /// that column existed, so the human door's sent listing can show who each
    /// message went to instead of a blank. Same shape as
    /// [`harvest_sent_contacts`](Self::harvest_sent_contacts): `format=metadata`
    /// with only To/Cc requested (headers, never bodies) on the READ credential,
    /// and a `sync_state` completion flag so it runs once per install.
    ///
    /// BEST-EFFORT AND NON-FATAL by construction: it is spawned beside the sync
    /// loop, never inside it, and any error returns early with the flag UNSET, so
    /// the next daemon start simply redoes whatever is still NULL. A message
    /// Gmail no longer has (404) is not an error — it is written as "" ("looked,
    /// nobody named"), which is what keeps one deleted message from re-queueing
    /// the whole pass forever.
    pub async fn backfill_sent_recipients(&self) -> Result<()> {
        let done = self
            .store
            .sync_state(self.account_id, SENT_RECIPIENTS_KEY)?
            .map(|s| s.last_uid >= 1)
            .unwrap_or(false);
        if done {
            return Ok(());
        }

        let mut filled = 0usize;
        // Per-message failures are SKIPPED, not fatal: one message with a
        // persistent 4xx quirk must not abort the sweep (or re-run the whole
        // pass on every daemon start forever). Skipped rows stay NULL and are
        // filtered out of each batch so the loop still terminates; the done
        // flag is only set after a pass with zero skips, so they retry on the
        // next start.
        let mut skipped: Vec<i64> = Vec::new();
        loop {
            let pending = self
                .store
                .sent_missing_recipients(self.account_id, SENT_RECIPIENTS_BATCH)?;
            let pending: Vec<_> = pending
                .iter()
                .filter(|r| !skipped.contains(&r.message_id))
                .collect();
            if pending.is_empty() {
                break;
            }
            for row in &pending {
                let url = format!(
                    "{}/messages/{}?format=metadata\
                     &metadataHeaders=To&metadataHeaders=Cc",
                    self.api_base, row.gmail_msg_id
                );
                let msg: GmailMessage = match self.get_json(&url).await {
                    Ok(msg) => msg,
                    // Gone upstream: record the absence rather than retrying it
                    // on every daemon start for the life of the install.
                    Err(CoreError::NotFound) => GmailMessage::default(),
                    Err(_) => {
                        skipped.push(row.message_id);
                        continue;
                    }
                };
                let headers = msg.payload.map(|p| p.headers).unwrap_or_default();
                // Through mail-parser via header synthesis, so grouped lists,
                // quoted display names and RFC2047 encoding render exactly as the
                // ingest path renders them.
                let blob = synthesize_rfc822_headers(&headers);
                let mut mailboxes: Vec<(String, Option<String>)> = Vec::new();
                if let Some(parsed) = mail_parser::MessageParser::default().parse(blob.as_bytes()) {
                    for list in [parsed.to(), parsed.cc()].into_iter().flatten() {
                        collect_mailboxes(list, &mut mailboxes);
                    }
                }
                // EVERY row is written, "" included: the store predicate is
                // `to_addrs IS NULL`, so a row left NULL would be handed back by
                // the next batch query and loop forever.
                let to = format_recipients(&mailboxes).unwrap_or_default();
                self.store
                    .set_message_to_addrs(self.account_id, row.message_id, &to)?;
                filled += 1;
            }
        }

        if skipped.is_empty() {
            self.store.set_sync_state(
                self.account_id,
                SENT_RECIPIENTS_KEY,
                &SyncState {
                    uidvalidity: 1,
                    last_uid: 1,
                },
            )?;
            if filled > 0 {
                eprintln!("squelch: sent-recipients backfill complete — {filled} messages filled");
            }
        } else {
            eprintln!(
                "squelch: sent-recipients backfill left {} of {} unfetched; retrying next start",
                skipped.len(),
                skipped.len() + filled
            );
        }
        Ok(())
    }

    // ---- Gmail REST calls --------------------------------------------------

    /// List all message ids under `label`, optionally narrowed by a Gmail search
    /// `q`. Paginates fully.
    async fn list_message_ids(&self, label: &str, q: Option<&str>) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{}/messages?labelIds={label}", self.api_base);
            if let Some(q) = q {
                url.push_str(&format!("&q={}", urlencode(q)));
            }
            if let Some(tok) = &page_token {
                url.push_str(&format!("&pageToken={tok}"));
            }
            let page: ListMessagesResp = self.get_json(&url).await?;
            ids.extend(page.messages.into_iter().map(|m| m.id));
            match page.next_page_token {
                Some(tok) => page_token = Some(tok),
                None => break,
            }
        }
        Ok(ids)
    }

    /// Fetch each id `format=raw`, base64url-decode to RFC822, and run through
    /// the ingest pipeline. Sequential — rate limits are a non-issue at this
    /// volume. Returns the count ingested.
    async fn fetch_raw_and_ingest(
        &self,
        ids: &[String],
        is_sent: bool,
        origin: IngestOrigin,
    ) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let rules = self.store.list_sender_rules(self.account_id)?;
        let now = Utc::now();
        let mut count = 0usize;

        for id in ids {
            let url = format!("{}/messages/{id}?format=raw", self.api_base);
            let msg: GmailMessage = self.get_json(&url).await?;
            let raw_b64 = match &msg.raw {
                Some(r) => r,
                None => continue, // nothing to ingest
            };
            let raw = match decode_raw_b64url(raw_b64) {
                Ok(bytes) => bytes,
                Err(e) => {
                    // Redacted: id + error only, never content.
                    eprintln!("squelch: skipping message (decode error): {e}");
                    continue;
                }
            };
            let fetched = RawFetched {
                account_id: self.account_id,
                gmail_msg_id: if msg.id.is_empty() {
                    id.clone()
                } else {
                    msg.id.clone()
                },
                gmail_thread_id: msg.thread_id.clone(),
                raw,
                internal_date: parse_internal_date(msg.internal_date.as_deref()),
                is_sent,
                account_addr: self.account_email.clone(),
            };
            if let Some((id, text)) = self.ingest_one(&fetched, &rules, now, origin)? {
                self.embed_and_store(id, text).await;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Run one fetched message through the seal-first ingest pipeline and commit
    /// it atomically. Returns `Some((message_id, embed_text))` for a NORMAL row,
    /// `None` for a SEALED one — the structural gate keeping sealed content out
    /// of the vector space (nothing to embed, not a filtered embedding).
    /// `embed_text` is the same flattening used at query time.
    fn ingest_one(
        &self,
        fetched: &RawFetched,
        rules: &[SenderRule],
        now: DateTime<Utc>,
        origin: IngestOrigin,
    ) -> Result<Option<(i64, String)>> {
        let triaged = ingest_with_rules(fetched, &self.config.stage1, now, rules, |addr| {
            self.store
                .is_known_contact(self.account_id, addr)
                .unwrap_or(false)
        });
        // A SEALED OUTBOUND COPY IS NOT COMMITTED — the same rule
        // [`ingest::ingest_sent`] holds the api's send echo to, and now that the
        // sent walk delivers outbound mail on every poll it has to hold here too.
        // `thread_guard_and_subject` 404s any thread containing a sealed message,
        // so storing the user's own reply that quotes an OTP would HIDE the
        // counterparty's mail they were reading a second ago. Seal detection runs
        // BEFORE ingest's `is_sent` branch precisely so this case is catchable.
        if fetched.is_sent && triaged.sensitivity != Sensitivity::Normal {
            return Ok(None);
        }
        let id = self.store.ingest_message(&triaged)?;
        // Notify on a CONFIDENT heuristic seed only; a guess waits for the
        // Stage-1/Stage-2 apply sites. ACCEPTED TRADEOFF: Stage-1 may later
        // DOWNGRADE the seed, but the push has already fired and
        // UNIQUE(message_id) means no second, corrected event.
        if origin == IngestOrigin::Incremental && triaged.confident {
            self.emit_event(&events::ingest_context(&triaged, id, rules), now);
        }
        // STRUCTURAL EXCLUSION: sealed mail is never embedded.
        if triaged.sensitivity != Sensitivity::Normal {
            return Ok(None);
        }
        let text = crate::embed::message_embed_text(
            &triaged.message.subject,
            &triaged.message.body,
            self.config.embed.max_chars,
        );
        Ok(Some((id, text)))
    }

    /// The sender's CURRENT rule disposition, for the refine emission sites —
    /// see [`events::current_rule`] for why a queued row cannot answer this
    /// itself. A store error reads as "no rule"; the surrounding pass has already
    /// logged any real store trouble by then.
    fn current_rule(&self, from_addr: &str) -> Option<crate::types::Disposition> {
        let rules = self.store.list_sender_rules(self.account_id).ok()?;
        events::current_rule(from_addr, &rules)
    }

    /// The single emission point for all three verdict sites (ingest heuristic,
    /// Stage-1 apply, Stage-2 apply); the decision itself lives in
    /// [`events::event_for`], which owns the seal invariant and the freshness
    /// storm guard. BEST-EFFORT: a store error is logged (ids only) and
    /// swallowed — a notification is never worth failing triage over. Store-side
    /// `UNIQUE(message_id)` makes a repeat call a silent no-op, which is what
    /// makes the refine passes and `catch_up()`'s re-scan safe to hook.
    fn emit_event(&self, ctx: &events::EventContext<'_>, now: DateTime<Utc>) {
        let Some(ev) = events::event_for(ctx, &self.config.notify, now) else {
            return;
        };
        match self.store.append_event(&ev) {
            Ok(Some(id)) => eprintln!(
                "squelch: notification event {id} ({}) for message {}",
                ev.kind.as_str(),
                ev.message_id
            ),
            // Already notified once; one event per message, ever.
            Ok(None) => {}
            Err(e) => eprintln!(
                "squelch: append_event failed ({e}); no notification for message {}",
                ev.message_id
            ),
        }
    }

    /// Embed `text` off the async runtime and write the vector for
    /// `message_id`. No-op without an embedder. A failure logs a redacted
    /// one-liner (id + error kind, never body) and never propagates — the
    /// backfill pass recovers the vector, so ingest must not block on it.
    async fn embed_and_store(&self, message_id: i64, text: String) {
        let Some(embedder) = self.embedder() else {
            return;
        };
        let account_id = self.account_id;
        let store = self.store.clone();
        // ONNX inference is CPU-bound; keep it off the poll loop.
        let result = tokio::task::spawn_blocking(move || {
            let vec = embedder.embed(&text)?;
            store.upsert_message_vector(account_id, message_id, &vec)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!(
                "squelch: embed failed for message {message_id} (recoverable via backfill): {e}"
            ),
            Err(e) => eprintln!("squelch: embed task join error for message {message_id}: {e}"),
        }
    }

    /// Embed every message still missing a vector, in throttled batches, so
    /// recall covers pre-existing rows and ingest-time embed failures. Sealed
    /// content is structurally absent — `messages_missing_vectors` selects only
    /// `sensitivity='normal'` (see docs/SECURITY.md). A failed batch logs a
    /// redacted one-liner and is retried on a later pass. No-op with no embedder.
    async fn backfill_missing_vectors(&self) {
        let Some(embedder) = self.embedder() else {
            return;
        };
        let batch = self.config.embed.backfill_batch.max(1);
        let max_chars = self.config.embed.max_chars;
        let account_id = self.account_id;
        let mut total = 0usize;

        loop {
            let missing = match self.store.messages_missing_vectors(account_id, batch) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("squelch: vector backfill query failed ({e}); stopping pass");
                    return;
                }
            };
            if missing.is_empty() {
                break;
            }
            let n = missing.len();
            // Flatten each message the SAME way ingest and query do.
            let store = self.store.clone();
            let embedder = embedder.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                let texts: Vec<String> = missing
                    .iter()
                    .map(|m| crate::embed::message_embed_text(&m.subject, &m.body, max_chars))
                    .collect();
                let vecs = embedder.embed_batch(&texts)?;
                for (m, vec) in missing.iter().zip(vecs.iter()) {
                    store.upsert_message_vector(account_id, m.message_id, vec)?;
                }
                Ok(())
            })
            .await;

            match result {
                Ok(Ok(())) => total += n,
                Ok(Err(e)) => {
                    eprintln!("squelch: vector backfill batch failed ({e}); stopping pass");
                    break;
                }
                Err(e) => {
                    eprintln!("squelch: vector backfill task join error ({e}); stopping pass");
                    break;
                }
            }

            // A short batch means we drained the queue; stop before re-querying.
            if n < batch {
                break;
            }
            // Throttle between batches so a large backfill doesn't peg the CPU or
            // starve the poll loop.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if total > 0 {
            eprintln!("squelch: vector backfill embedded {total} message(s) for semantic recall");
        }
    }

    /// True at most once per UTC `day` per cap `kind`, so a persistently-capped
    /// account logs each notice once a day rather than every poll. Stamps the
    /// day as a side effect; a poisoned lock defaults to warning, never to
    /// silently swallowing the notice.
    fn warn_once_per_day(&self, kind: CapKind, day: &str) -> bool {
        let mut guard = match self.warn_days.lock() {
            Ok(g) => g,
            Err(_) => return true,
        };
        let slot = match kind {
            CapKind::Thread => &mut guard.thread,
            CapKind::Sender => &mut guard.sender,
            CapKind::Global => &mut guard.global,
            CapKind::Stage1Global => &mut guard.stage1_global,
        };
        if slot.as_deref() == Some(day) {
            false
        } else {
            *slot = Some(day.to_string());
            true
        }
    }

    /// Shared pass preamble; `None` when the LLM is disabled (no API key —
    /// the notice was already emitted at startup). Caps are re-read at the
    /// START of every pass so a client change via POST /client/triage-config
    /// applies within a cycle, no restart. Precedence: override > config/env
    /// > default.
    fn pass_setup(&self) -> Option<PassSetup<'_>> {
        let llm = self.stage2_llm.as_ref()?;
        let caps = self
            .store
            .stage2_cap_overrides(self.account_id)
            .unwrap_or_default();
        let now = Utc::now();
        Some(PassSetup {
            api_key: &llm.api_key,
            provider: llm.provider,
            url: &llm.url,
            caps,
            day: now.format("%Y-%m-%d").to_string(),
            stale_cutoff: now - ChronoDuration::days(self.config.stage2.max_age_days as i64),
        })
    }

    /// Unwrap a pass's queue read; a read error logs once and yields an empty
    /// queue, which the caller treats as "nothing to do".
    fn read_queue<T>(res: Result<Vec<T>>, label: &str) -> Vec<T> {
        match res {
            Ok(q) => q,
            Err(e) => {
                eprintln!("squelch: {label} queue read failed ({e}); skipping pass");
                Vec::new()
            }
        }
    }

    /// Check-then-increment the SHARED Stage-1 global daily budget (Stage-1
    /// and the extractors bill the same counter). The increment lands BEFORE
    /// the model call so an attempt counts even on error/retry — a retry storm
    /// can never exceed the cap. Stage-2's three-scope gate stays inline in
    /// [`Self::stage2_pass`]: it checks all three caps before incrementing any
    /// of them, which a per-key check-then-increment helper would break.
    fn gate_stage1_global_budget(
        &self,
        day: &str,
        cap: u32,
        label: &str,
        tail: &str,
    ) -> BudgetGate {
        match self
            .store
            .stage2_budget_used(self.account_id, STAGE1_GLOBAL_BUDGET_KEY, day)
        {
            Ok(used) if used >= cap => {
                if self.warn_once_per_day(CapKind::Stage1Global, day) {
                    eprintln!(
                        "squelch: stage-1 global daily budget exhausted \
                         ({used}/{cap}); {tail} stay queued"
                    );
                }
                return BudgetGate::Exhausted;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("squelch: {label} budget read failed ({e}); skipping row");
                return BudgetGate::SkipRow;
            }
        }
        if let Err(e) =
            self.store
                .stage2_increment_budget(self.account_id, STAGE1_GLOBAL_BUDGET_KEY, day)
        {
            eprintln!("squelch: {label} budget increment failed ({e}); skipping row");
            return BudgetGate::SkipRow;
        }
        BudgetGate::Proceed
    }

    /// Run one Stage-1 LLM refine pass over rows still carrying their ingest
    /// heuristic seed (`stage1_model_used IS NULL`): sealed guard, GLOBAL
    /// Stage-1 budget with increment-before-call so retries can't exceed it,
    /// classify, apply — which stamps `stage1_model_used` and sets
    /// `needs_stage2`. On refusal or permanent error the row keeps its seed
    /// values stamped `heuristic-only` and the seed's own `needs_stage2` decides
    /// escalation. Budget exhaustion defers rows without loss; no failure
    /// crashes the sync loop. No-op when the LLM is disabled (no API key).
    async fn stage1_pass(&self) {
        let Some(PassSetup {
            api_key,
            provider,
            url,
            caps,
            day,
            stale_cutoff,
        }) = self.pass_setup()
        else {
            return;
        };
        let cfg = &self.config.stage1;
        let global_daily_cap = caps.stage1_global_daily_cap.unwrap_or(cfg.global_daily_cap);

        let queued = Self::read_queue(
            self.store
                .stage1_queue(self.account_id, cfg.batch_per_cycle),
            "stage-1",
        );
        if queued.is_empty() {
            return;
        }

        let mut refined = 0usize;
        let mut fallback = 0usize;
        let mut stale_skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        for row in &queued {
            // SEALED GUARD: the queue already excludes sealed rows in SQL;
            // re-check before every classify call (docs/SECURITY.md).
            if let Err(e) = stage1_sealed_guard(row) {
                eprintln!("squelch: stage-1 sealed guard tripped ({e}); skipping row");
                continue;
            }

            // SKIP-STALE: mark processed WITHOUT a model call, keeping the seed.
            if row.received_at < stale_cutoff {
                let _ = self.store.stage1_mark_processed(
                    self.account_id,
                    row.message_id,
                    HEURISTIC_ONLY,
                );
                stale_skipped += 1;
                self.metrics.record_stage1(Stage1Verdict::StaleSkipped);
                continue;
            }

            // GLOBAL budget check (Stage-1's ONLY scope). Once hit, every
            // remaining row this cycle stays queued, unstamped.
            match self.gate_stage1_global_budget(
                &day,
                global_daily_cap,
                "stage-1 global",
                "remaining rows",
            ) {
                BudgetGate::Exhausted => break,
                BudgetGate::SkipRow => continue,
                BudgetGate::Proceed => {}
            }

            let outcome = stage1_llm::classify(&self.http, url, api_key, cfg, provider, row).await;
            match outcome {
                Ok(stage1_llm::ClassifyOutcome::Ok(out, usage)) => {
                    if let Some(u) = usage {
                        in_tok += u.input_tokens;
                        out_tok += u.output_tokens;
                        if let Err(e) = self.store.stage1_bump_usage(
                            self.account_id,
                            &day,
                            u.input_tokens,
                            u.output_tokens,
                            u.cache_creation_input_tokens,
                            u.cache_read_input_tokens,
                        ) {
                            eprintln!("squelch: stage-1 usage ledger bump failed ({e})");
                        }
                    }
                    let applied = stage1_llm::apply_result(
                        row,
                        &out,
                        &cfg.model,
                        cfg.known_contact_importance,
                        Utc::now(),
                    );
                    match self.store.stage1_apply(&applied) {
                        Err(e) => {
                            eprintln!("squelch: stage-1 apply failed ({e}); row stays queued");
                        }
                        // TOCTOU: the row was sealed by hand while this pass held
                        // it, so the guarded UPDATE matched nothing and no verdict
                        // landed. Emitting on a bare Ok would snapshot sender +
                        // one_line for a now-sealed row.
                        Ok(false) => {}
                        Ok(true) => {
                            refined += 1;
                            self.metrics.record_stage1(Stage1Verdict::Ok);
                            // The refined verdict is final, so it emits whatever
                            // the seed thought; the freshness window is what stops
                            // this pass storming a fresh install's backlog.
                            self.emit_event(
                                &events::EventContext {
                                    account_id: self.account_id,
                                    message_id: row.message_id,
                                    thread_id: &row.thread_id,
                                    sender: &row.from_addr,
                                    one_line: &applied.one_line,
                                    received_at: row.received_at,
                                    sensitivity: row.sensitivity,
                                    // The Stage-1 queue selects `m.is_sent = 0`.
                                    is_sent: false,
                                    // The queue only excludes rows a rule decided
                                    // AT INGEST, so read the rule list as it
                                    // stands NOW to catch rules added since.
                                    rule: self.current_rule(&row.from_addr),
                                    tier: applied.tier,
                                    importance: applied.importance,
                                    deadline: applied.deadline.as_ref(),
                                },
                                Utc::now(),
                            );
                        }
                    }
                }
                Ok(stage1_llm::ClassifyOutcome::Refused)
                | Ok(stage1_llm::ClassifyOutcome::Failed(_)) => {
                    // HEURISTIC FALLBACK: keep the seed values and mark processed
                    // so the row cannot loop; the ingest-time needs_stage2 seed
                    // survives and drives escalation.
                    let _ = self.store.stage1_mark_processed(
                        self.account_id,
                        row.message_id,
                        HEURISTIC_ONLY,
                    );
                    fallback += 1;
                    self.metrics.record_stage1(Stage1Verdict::Fallback);
                }
                Err(e) => {
                    // Retryable class exhausted / transport error. Leave the row
                    // queued (stage1_model_used stays NULL) for a future cycle.
                    eprintln!("squelch: stage-1 {e}; row stays queued");
                }
            }
        }

        if refined > 0 || fallback > 0 || stale_skipped > 0 {
            eprintln!(
                "squelch: stage-1 refined {refined} rows (model={}, in_tok={in_tok}, \
                 out_tok={out_tok}); heuristic-fallback {fallback}; stale-skipped {stale_skipped}",
                cfg.model
            );
        }
    }

    /// Run one SPECIALIST-EXTRACTOR pass over rows whose FINAL category has a
    /// registered extractor — hence AFTER both stage passes. Per row: sealed
    /// guard, stale skip, then check + increment the SHARED Stage-1 daily budget
    /// (extractors run on the Stage-1 model and share its cap) before
    /// dispatching. Token usage bills to the extractor's OWN ledger category.
    /// Budget exhaustion defers rows without loss; per-row failures are logged
    /// redacted and never crash the sync loop. No-op when there is no API key.
    async fn extract_pass(&self) {
        let categories = extract::extractable_categories();
        if categories.is_empty() {
            return;
        }
        let Some(PassSetup {
            api_key,
            provider,
            url,
            caps,
            day,
            stale_cutoff,
        }) = self.pass_setup()
        else {
            return;
        };
        // Extractors run on the STAGE-1 (small) model and share its config +
        // cap: extract calls count against the SAME daily counter as Stage-1,
        // runtime override included.
        let cfg = &self.config.stage1;
        let global_daily_cap = caps.stage1_global_daily_cap.unwrap_or(cfg.global_daily_cap);

        let queued = Self::read_queue(
            self.store
                .extract_queue(self.account_id, &categories, cfg.batch_per_cycle),
            "extract",
        );
        if queued.is_empty() {
            return;
        }

        let mut extracted = 0usize;
        let mut skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        for row in &queued {
            // ONE ORDERED DECISION per row — sealed guard (the queue already
            // excludes sealed rows in SQL; re-check anyway, docs/SECURITY.md),
            // then the stale skip, then the extractor lookup. It lives in
            // `route_extract_row` so a new specialist cannot be added behind a
            // guard that does not know about it.
            let extractor = match extract::route_extract_row(row, stale_cutoff) {
                RowAction::Sealed => {
                    // Re-run the guard purely to log its redacted message.
                    if let Err(e) = extract::extract_sealed_guard(row) {
                        eprintln!("squelch: extract sealed guard tripped ({e}); skipping row");
                    }
                    continue;
                }
                // SKIP-STALE: mark extracted WITHOUT a model call, so an old row
                // neither spends budget nor sits queued forever.
                RowAction::Stale => {
                    let _ = self.store.extract_mark_processed(
                        self.account_id,
                        row.message_id,
                        STALE_SKIP_MODEL,
                    );
                    skipped += 1;
                    continue;
                }
                // A row whose category has no handler is marked processed so it
                // cannot loop.
                RowAction::NoExtractor => {
                    let _ = self.store.extract_mark_processed(
                        self.account_id,
                        row.message_id,
                        "skip-no-extractor",
                    );
                    skipped += 1;
                    continue;
                }
                RowAction::Run(extractor) => extractor,
            };

            // SHARED Stage-1 global budget. Once hit, every remaining row this
            // cycle stays queued, unstamped.
            match self.gate_stage1_global_budget(&day, global_daily_cap, "extract", "extract rows")
            {
                BudgetGate::Exhausted => break,
                BudgetGate::SkipRow => continue,
                BudgetGate::Proceed => {}
            }

            // ROUTE BY CATEGORY: each specialist owns its own prompt, schema and
            // ledger line, so the row's category decides which one runs.
            match extractor {
                CategoryExtractor::Marketing => {
                    match marketing::classify(&self.http, url, api_key, cfg, provider, row).await {
                        Ok(marketing::ExtractOutcome::Ok(out, usage)) => {
                            if let Some(u) = usage {
                                in_tok += u.input_tokens;
                                out_tok += u.output_tokens;
                                if let Err(e) = self.store.extract_bump_usage(
                                    self.account_id,
                                    &day,
                                    marketing::LEDGER_CATEGORY,
                                    u.input_tokens,
                                    u.output_tokens,
                                    u.cache_creation_input_tokens,
                                    u.cache_read_input_tokens,
                                ) {
                                    eprintln!("squelch: extract usage ledger bump failed ({e})");
                                }
                            }
                            let applied = marketing::apply_result(row, &out, &cfg.model);
                            if let Err(e) = self.store.marketing_apply(&applied) {
                                // The call is already paid for: mark processed rather
                                // than re-buying it every cycle.
                                eprintln!(
                                    "squelch: marketing apply failed ({e}); row marked apply-failed"
                                );
                                let _ = self.store.extract_mark_processed(
                                    self.account_id,
                                    row.message_id,
                                    "apply-failed",
                                );
                            } else {
                                extracted += 1;
                            }
                        }
                        Ok(marketing::ExtractOutcome::Refused)
                        | Ok(marketing::ExtractOutcome::Failed(_)) => {
                            let _ = self.store.extract_mark_processed(
                                self.account_id,
                                row.message_id,
                                "extract-failed",
                            );
                            skipped += 1;
                        }
                        Err(e) => {
                            eprintln!("squelch: extract {e}; row stays queued");
                        }
                    }
                }
                CategoryExtractor::Banking => {
                    let outcome =
                        banking::classify(&self.http, url, api_key, cfg, provider, row).await;
                    match outcome {
                        Ok(banking::ExtractOutcome::Ok(out, usage)) => {
                            if let Some(u) = usage {
                                in_tok += u.input_tokens;
                                out_tok += u.output_tokens;
                                if let Err(e) = self.store.extract_bump_usage(
                                    self.account_id,
                                    &day,
                                    banking::LEDGER_CATEGORY,
                                    u.input_tokens,
                                    u.output_tokens,
                                    u.cache_creation_input_tokens,
                                    u.cache_read_input_tokens,
                                ) {
                                    eprintln!("squelch: extract usage ledger bump failed ({e})");
                                }
                            }
                            let applied = banking::apply_result(row, &out, &cfg.model);
                            if let Err(e) = self.store.banking_apply(&applied) {
                                // Failure sentinel rather than a re-queue: the call is
                                // already paid for, a store failure is unlikely to heal
                                // on a retry, and leaving the row queued would re-buy a
                                // call every cycle. Only the Banking record is lost — the
                                // email itself is still in the inbox.
                                eprintln!(
                                    "squelch: banking apply failed ({e}); row marked apply-failed"
                                );
                                let _ = self.store.extract_mark_processed(
                                    self.account_id,
                                    row.message_id,
                                    "apply-failed",
                                );
                            } else {
                                extracted += 1;
                            }
                        }
                        Ok(banking::ExtractOutcome::Refused)
                        | Ok(banking::ExtractOutcome::Failed(_)) => {
                            // Mark processed so the row cannot loop; no specialist row is
                            // written, so nothing appears in the Banking zone.
                            let _ = self.store.extract_mark_processed(
                                self.account_id,
                                row.message_id,
                                "extract-failed",
                            );
                            skipped += 1;
                        }
                        Err(e) => {
                            // Retryable class exhausted / transport error: leave the row
                            // queued (extractor_model_used stays NULL) for a later cycle.
                            eprintln!("squelch: extract {e}; row stays queued");
                        }
                    }
                }
            }
        }

        if extracted > 0 || skipped > 0 {
            eprintln!(
                "squelch: extract processed {extracted} rows (model={}, in_tok={in_tok}, \
                 out_tok={out_tok}); skipped {skipped}",
                cfg.model
            );
        }
    }

    /// Run one Stage-2 LLM triage pass over the queued (non-confident) rows:
    /// up to `batch_per_cycle` rows (`model_used IS NULL AND
    /// sensitivity='normal'`), sequentially. Per row — sealed guard, the three
    /// daily budget checks, increment BEFORE the call so retry storms can't
    /// exceed a cap, classify, apply. Budget exhaustion leaves rows queued. Any
    /// per-row failure is logged redacted and never crashes the sync loop.
    /// No-op when Stage-2 is disabled (no API key).
    async fn stage2_pass(&self) {
        let Some(PassSetup {
            api_key,
            provider,
            url,
            caps,
            day,
            stale_cutoff,
        }) = self.pass_setup()
        else {
            return;
        };
        let cfg = &self.config.stage2;
        let thread_daily_cap = caps.thread_daily_cap.unwrap_or(cfg.thread_daily_cap);
        let sender_daily_cap = caps.sender_daily_cap.unwrap_or(cfg.sender_daily_cap);
        let global_daily_cap = caps.global_daily_cap.unwrap_or(cfg.global_daily_cap);

        let queued = Self::read_queue(
            self.store
                .stage2_queue(self.account_id, cfg.batch_per_cycle),
            "stage-2",
        );
        if queued.is_empty() {
            return;
        }

        let mut processed = 0usize;
        let mut stale_skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        for row in &queued {
            // SEALED GUARD: the queue already excludes sealed rows in SQL;
            // re-check before every classify call (docs/SECURITY.md).
            if let Err(e) = stage2_sealed_guard(row) {
                eprintln!("squelch: stage-2 sealed guard tripped ({e}); skipping row");
                continue;
            }

            // SKIP-STALE: mark processed WITHOUT a model call, keeping Stage-1
            // values, so the row neither spends budget nor sits queued forever.
            if row.received_at < stale_cutoff {
                let _ = self.store.stage2_mark_processed(
                    self.account_id,
                    row.message_id,
                    STALE_SKIP_MODEL,
                );
                stale_skipped += 1;
                self.metrics.record_stage2(Stage2Verdict::StaleSkipped);
                continue;
            }

            // GLOBAL budget check: once the account cap is hit, BREAK — every
            // remaining row this cycle is blocked.
            match self
                .store
                .stage2_budget_used(self.account_id, GLOBAL_BUDGET_KEY, &day)
            {
                Ok(used) if used >= global_daily_cap => {
                    if self.warn_once_per_day(CapKind::Global, &day) {
                        eprintln!(
                            "squelch: stage-2 global daily budget exhausted ({used}/{global_daily_cap}); \
                             remaining rows stay queued"
                        );
                    }
                    break; // global cap blocks every remaining row this cycle
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 global budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // PER-THREAD budget check; the notice names the capped thread.
            match self
                .store
                .stage2_budget_used(self.account_id, &row.thread_id, &day)
            {
                Ok(used) if used >= thread_daily_cap => {
                    if self.warn_once_per_day(CapKind::Thread, &day) {
                        // thread_id is Gmail hex, but sanitize defensively in
                        // case a malformed cursor ever supplies otherwise.
                        eprintln!(
                            "squelch: stage-2 per-thread daily budget exhausted for thread {} \
                             ({used}/{thread_daily_cap}); those rows stay queued",
                            sanitize_ascii(&row.thread_id, 64)
                        );
                    }
                    continue; // this thread is capped; try the next row
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 thread budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // PER-SENDER budget check, keyed by from_addr: stops one chatty
            // sender fanning many DIFFERENT threads from burning the budget.
            let sender_key = format!("{SENDER_BUDGET_PREFIX}{}", row.from_addr);
            match self
                .store
                .stage2_budget_used(self.account_id, &sender_key, &day)
            {
                Ok(used) if used >= sender_daily_cap => {
                    if self.warn_once_per_day(CapKind::Sender, &day) {
                        // from_addr is UNTRUSTED header PII: log the
                        // non-reversible tag, never the address.
                        eprintln!(
                            "squelch: stage-2 per-sender daily budget exhausted for sender {} \
                             ({used}/{sender_daily_cap}); those rows stay queued",
                            redact_sender(&row.from_addr)
                        );
                    }
                    continue; // this sender is capped; try the next row
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 sender budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // Increment ALL THREE budgets BEFORE the call so the attempt counts
            // even if it errors or retries.
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, GLOBAL_BUDGET_KEY, &day)
            {
                eprintln!("squelch: stage-2 global budget increment failed ({e}); skipping row");
                continue;
            }
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, &row.thread_id, &day)
            {
                eprintln!("squelch: stage-2 thread budget increment failed ({e}); skipping row");
                continue;
            }
            if let Err(e) = self
                .store
                .stage2_increment_budget(self.account_id, &sender_key, &day)
            {
                eprintln!("squelch: stage-2 sender budget increment failed ({e}); skipping row");
                continue;
            }

            let ctx = RowContext::from_queued(row, cfg.max_body_chars);
            let outcome = stage2::classify(&self.http, url, api_key, cfg, provider, &ctx).await;

            match outcome {
                Ok(ClassifyOutcome::Ok(out, usage)) => {
                    if let Some(u) = usage {
                        in_tok += u.input_tokens;
                        out_tok += u.output_tokens;
                        // USAGE LEDGER, best-effort: a ledger write failure must
                        // not affect triage.
                        if let Err(e) = self.store.stage2_bump_usage(
                            self.account_id,
                            &day,
                            u.input_tokens,
                            u.output_tokens,
                            u.cache_creation_input_tokens,
                            u.cache_read_input_tokens,
                        ) {
                            eprintln!("squelch: stage-2 usage ledger bump failed ({e})");
                        }
                    }
                    let applied = stage2::apply_result(
                        row,
                        &out,
                        &cfg.model,
                        self.config.stage1.known_contact_importance,
                        Utc::now(),
                    );
                    match self.store.stage2_apply(&applied) {
                        Err(e) => {
                            eprintln!("squelch: stage-2 apply failed ({e}); row stays queued");
                        }
                        // TOCTOU: sealed by hand mid-pass, so no verdict landed
                        // and there is nothing to notify for.
                        Ok(false) => {}
                        Ok(true) => {
                            processed += 1;
                            self.metrics.record_stage2(Stage2Verdict::Ok);
                            self.emit_event(
                                &events::EventContext {
                                    account_id: self.account_id,
                                    message_id: row.message_id,
                                    thread_id: &row.thread_id,
                                    sender: &row.from_addr,
                                    one_line: &applied.one_line,
                                    received_at: row.received_at,
                                    sensitivity: row.sensitivity,
                                    // The Stage-2 queue selects `m.is_sent = 0`.
                                    is_sent: false,
                                    // Read NOW, not at ingest: the row only
                                    // records the rule in force when it was
                                    // queued, so a sender squelched since then
                                    // would otherwise still push.
                                    rule: self.current_rule(&row.from_addr),
                                    tier: applied.tier,
                                    importance: applied.importance,
                                    deadline: applied.deadline.as_ref(),
                                },
                                Utc::now(),
                            );
                        }
                    }
                }
                Ok(ClassifyOutcome::Refused) => {
                    // Keep Stage-1 values; mark processed so it doesn't loop.
                    // Redacted: no body/subject logged.
                    eprintln!("squelch: stage-2 refusal (redacted); keeping stage-1 values");
                    self.metrics.record_stage2(Stage2Verdict::Refused);
                    let _ = self.store.stage2_mark_processed(
                        self.account_id,
                        row.message_id,
                        &cfg.model,
                    );
                }
                Ok(ClassifyOutcome::Failed(kind)) => {
                    // Permanent failure (400/401/truncation/parse): mark the row
                    // processed so it cannot loop. `kind` is already redacted.
                    eprintln!("squelch: stage-2 permanent failure ({kind}); marking row failed");
                    self.metrics.record_stage2(Stage2Verdict::Failed);
                    let _ = self.store.stage2_mark_processed(
                        self.account_id,
                        row.message_id,
                        &cfg.model,
                    );
                }
                Err(e) => {
                    // Retryable class exhausted / transport error: leave the row
                    // queued for a later cycle. `e` is redacted.
                    eprintln!("squelch: stage-2 {e}; row stays queued");
                    self.metrics.record_stage2(Stage2Verdict::Retryable);
                }
            }
        }

        if processed > 0 || stale_skipped > 0 {
            eprintln!(
                "squelch: stage-2 processed {processed} rows (model={}, in_tok={in_tok}, \
                 out_tok={out_tok}); stale-skipped {stale_skipped}",
                cfg.model
            );
        }
    }

    /// `users.labels.get(INBOX)` -> Gmail's own unread counts, into the store.
    ///
    /// The ONLY source for these numbers: the read scope cannot see (or write)
    /// read state through any other call, and our tables hold just the backfill
    /// window, so nothing local can answer "how much unread mail is sitting in
    /// Gmail right now".
    ///
    /// Cannot fail a cycle — sync's job is mail, and a stale count is a far
    /// better outcome than a poll loop that gives up over a cosmetic number. On
    /// failure of EITHER half (the fetch or the store write) the previous row
    /// stands, no clearing and no zeroing, and the notice is printed once per
    /// failing streak rather than every poll.
    async fn refresh_inbox_unread(&self) {
        let url = format!("{}/labels/{LABEL_INBOX}", self.api_base);
        // Both halves land in one Result so both go through the one latch:
        // whichever way this fails, it fails every 45s, and a branch that
        // printed on its own would be the branch that spams.
        let outcome = match self.get_json::<LabelResp>(&url).await {
            Ok(label) => self
                .store
                .set_inbox_unread(self.account_id, label.messages_unread, label.threads_unread)
                .map_err(|e| format!("storing the inbox unread counts failed ({e})")),
            Err(e) => Err(format!("inbox unread count fetch failed ({e})")),
        };
        match outcome {
            // Re-armed only by a whole cycle that worked, fetch and write both.
            Ok(()) => self.unread_warned.store(false, Ordering::Relaxed),
            // `swap` is the arm-and-test: only the transition into failure
            // prints. Error strings from this crate carry no secrets.
            Err(why) => {
                if !self.unread_warned.swap(true, Ordering::Relaxed) {
                    eprintln!("squelch: {why}; keeping the last known counts");
                }
            }
        }
    }

    /// `users.getProfile` -> the account's current historyId.
    async fn fetch_profile_history_id(&self) -> Result<u64> {
        let url = format!("{}/profile", self.api_base);
        let profile: ProfileResp = self.get_json(&url).await?;
        Ok(parse_history_id(&profile.history_id))
    }

    // ---- historyId cursor persistence (sync_state, key='history') ----------

    fn load_history_cursor(&self) -> Result<Option<u64>> {
        Ok(self
            .store
            .sync_state(self.account_id, HISTORY_KEY)?
            .map(|s| s.last_uid))
    }

    fn store_history_cursor(&self, history_id: u64) -> Result<()> {
        self.store.set_sync_state(
            self.account_id,
            HISTORY_KEY,
            &SyncState {
                uidvalidity: 0,
                last_uid: history_id,
            },
        )
    }

    fn backfill_since(&self) -> DateTime<Utc> {
        Utc::now() - ChronoDuration::days(self.config.sync.backfill_days as i64)
    }

    fn rules_for_stage2_note() -> &'static str {
        // Documentation anchor for the Stage-2 queue predicate: non-confident
        // rows are the ones left with model_used NULL.
        "model_used IS NULL AND sensitivity='normal'"
    }

    /// The top-level driver: loop, retrying with exponential backoff on any
    /// error, until shutdown is signalled.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let _ = Self::rules_for_stage2_note();
        let mut backoff = BACKOFF_START;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            match self.run_once(&mut shutdown).await {
                Ok(()) => {
                    self.metrics.record_sync_ok();
                    return Ok(());
                }
                Err(e) => {
                    self.metrics.record_sync_error();
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                    // Error strings from this crate never carry secrets.
                    eprintln!(
                        "squelch: sync error ({e}); retrying in {}s",
                        backoff.as_secs()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() { return Ok(()); }
                        }
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                }
            }
        }
    }
}

/// Which [`GmailErrorKind`] a non-2xx Gmail response is.
///
/// 429 is unambiguous. 403 is NOT: Google returns it both for "you are asking
/// too fast" and for "this credential may not do that", separated only by the
/// `reason` in the JSON body. Everything else is `http`, which is the bucket
/// that means "read the logs".
fn classify_gmail_status(status: u16, body: &str) -> GmailErrorKind {
    const QUOTA_REASONS: [&str; 4] = [
        "ratelimitexceeded",
        "userratelimitexceeded",
        "quotaexceeded",
        "dailylimitexceeded",
    ];
    match status {
        401 => GmailErrorKind::Auth,
        429 => GmailErrorKind::Quota,
        // Bounded: an error body is a few hundred bytes, and the reason sits at
        // the front of it. Never logged, only matched.
        403 => {
            let head: String = body.chars().take(2048).collect::<String>().to_lowercase();
            if QUOTA_REASONS.iter().any(|r| head.contains(r)) {
                GmailErrorKind::Quota
            } else {
                // A scope/permission 403 stays in `http`: it is rare, it is not
                // a credential this daemon can refresh its way out of, and the
                // stderr line already names it.
                GmailErrorKind::Http
            }
        }
        _ => GmailErrorKind::Http,
    }
}

/// Minimal percent-encoding for a Gmail `q` value. Enough for `newer_than:Nd`
/// and simple queries; arbitrary user queries are never built here.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Gmail `internalDate` is milliseconds-since-epoch as a decimal string. `pub`
/// so squelch-api's send-echo parses it from one definition.
pub fn parse_internal_date(s: Option<&str>) -> Option<DateTime<Utc>> {
    let ms: i64 = s?.trim().parse().ok()?;
    DateTime::from_timestamp_millis(ms)
}

/// Rebuild a header-only RFC822 blob from Gmail metadata headers so mail-parser
/// runs over it unchanged; the trailing blank line ends the header section
/// (empty body). Used by the Sent-contacts harvest (`format=metadata` carries
/// headers only) and the contacts-seeding tests.
fn synthesize_rfc822_headers(headers: &[MessageHeader]) -> String {
    let mut out = String::new();
    for h in headers {
        // HEADER INJECTION GUARD: Gmail names and values are single-line, but
        // upstream is never trusted blindly — a CR/LF in either would splice a
        // synthetic header into the blob.
        if h.name.contains('\r')
            || h.name.contains('\n')
            || h.value.contains('\r')
            || h.value.contains('\n')
        {
            continue;
        }
        out.push_str(&h.name);
        out.push_str(": ");
        out.push_str(&h.value);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out
}

/// Type alias helper so callers can name the concrete rule slice.
pub type Rules = Vec<SenderRule>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Stage1Config;
    use crate::store::SqliteStore;
    use crate::types::{Disposition, Tier, TriageAxis};

    /// The 403 split is the whole reason this classifier reads the body: Google
    /// spends one status on "too fast" and on "not allowed", and only the reason
    /// string tells them apart.
    #[test]
    fn gmail_status_classification_splits_403_on_the_reason() {
        let quota_body = r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#;
        assert_eq!(
            classify_gmail_status(403, quota_body),
            GmailErrorKind::Quota
        );
        assert_eq!(
            classify_gmail_status(403, r#"{"error":{"errors":[{"reason":"forbidden"}]}}"#),
            GmailErrorKind::Http
        );
        assert_eq!(classify_gmail_status(429, ""), GmailErrorKind::Quota);
        assert_eq!(classify_gmail_status(401, ""), GmailErrorKind::Auth);
        assert_eq!(classify_gmail_status(500, ""), GmailErrorKind::Http);
    }

    /// Build a RawFetched from an RFC822 string, as the transport layer would.
    /// The account's own address is fixed to `me@example.com` in these fixtures.
    fn fixture(account_id: AccountId, msgid: &str, eml: &str, is_sent: bool) -> RawFetched {
        RawFetched {
            account_id,
            gmail_msg_id: msgid.to_string(),
            gmail_thread_id: None,
            raw: eml.as_bytes().to_vec(),
            internal_date: Some(Utc::now()),
            is_sent,
            account_addr: "me@example.com".to_string(),
        }
    }

    /// End-to-end through the real store: ingest_with_rules -> ingest_message.
    fn ingest_into(
        store: &SqliteStore,
        account_id: AccountId,
        f: &RawFetched,
        now: DateTime<Utc>,
    ) -> i64 {
        let rules = store.list_sender_rules(account_id).unwrap();
        let triaged = ingest_with_rules(f, &Stage1Config::default(), now, &rules, |addr| {
            store.is_known_contact(account_id, addr).unwrap_or(false)
        });
        store.ingest_message(&triaged).unwrap()
    }

    // ---- notification events at the ingest call site -----------------------
    //
    // These drive the real pipeline through the real store; only the HTTP fetch
    // above it is out of reach, which is why the helper repeats the engine's two
    // lines of gating instead of calling `ingest_one` directly.

    /// Mirror of the engine's ingest emission site. Returns
    /// `(message_id, emitted_event_id)`.
    fn ingest_and_notify(
        store: &SqliteStore,
        account_id: AccountId,
        f: &RawFetched,
        now: DateTime<Utc>,
        origin: IngestOrigin,
    ) -> (i64, Option<i64>) {
        let cfg = crate::config::NotifyConfig::default();
        let rules = store.list_sender_rules(account_id).unwrap();
        let triaged = ingest_with_rules(f, &Stage1Config::default(), now, &rules, |addr| {
            store.is_known_contact(account_id, addr).unwrap_or(false)
        });
        let id = store.ingest_message(&triaged).unwrap();
        let mut emitted = None;
        if origin == IngestOrigin::Incremental && triaged.confident {
            let ctx = events::ingest_context(&triaged, id, &rules);
            if let Some(ev) = events::event_for(&ctx, &cfg, now) {
                emitted = store.append_event(&ev).unwrap();
            }
        }
        (id, emitted)
    }

    /// An ops-alert EML dated `at`: automated sender + alert language, so it
    /// lands Signal / importance 75 / CONFIDENT.
    fn alert_eml(at: DateTime<Utc>) -> String {
        format!(
            "From: Monitoring <alerts@monitoring.example>\r\n\
             To: me@example.com\r\n\
             Subject: Incident: checkout api is down\r\n\
             Date: {}\r\n\
             \r\n\
             A high-severity incident was opened for the checkout service.\r\n",
            at.to_rfc2822()
        )
    }

    #[test]
    fn fresh_worthy_ingest_emits_exactly_one_event() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eml = alert_eml(now);
        let f = fixture(acct, "g-alert", &eml, false);

        let (mid, ev_id) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental);
        let ev_id = ev_id.expect("a fresh confident alert above the line must notify");

        let ev = store.event_by_id(acct, ev_id).unwrap().expect("event row");
        assert_eq!(ev.message_id, mid);
        assert_eq!(ev.kind, crate::types::EventKind::Surfaced);
        assert_eq!(ev.tier, Tier::Signal);
        assert_eq!(ev.sender, "alerts@monitoring.example");
        assert_eq!(store.latest_event_id(acct).unwrap(), ev_id);

        // RE-INGEST (history overlap / catch-up re-scan) must stay silent.
        let (mid2, again) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental);
        assert_eq!(mid2, mid, "same message row");
        assert_eq!(again, None, "one event per message, ever");
        assert_eq!(store.events_after(acct, 0, 100).unwrap().len(), 1);
    }

    #[test]
    fn backfill_never_emits() {
        // A fresh install backfills a month of already-read mail. Not one push.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eml = alert_eml(now);
        let f = fixture(acct, "g-alert", &eml, false);

        let (_, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Backfill);
        assert_eq!(ev, None, "backfill is structurally silent");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert_eq!(store.latest_event_id(acct).unwrap(), 0);
    }

    #[test]
    fn stale_mail_is_silent_even_at_the_top_tier() {
        // THE STORM GUARD: a past-due bill from a KNOWN biller is the loudest
        // verdict the pipeline can produce, and old mail is silent anyway. This
        // is what makes `catch_up()`'s whole-window re-scan safe.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let old = now - ChronoDuration::days(3);

        // Seed the biller as a known contact so the bill lands CONFIDENT PastDue.
        let seed = format!(
            "From: me@example.com\r\n\
             To: Utility <billing@utilityco.example>\r\n\
             Subject: account setup\r\n\
             Date: {}\r\n\
             \r\n\
             hello\r\n",
            old.to_rfc2822()
        );
        let sf = fixture(acct, "g-seed", &seed, /* is_sent */ true);
        ingest_and_notify(&store, acct, &sf, now, IngestOrigin::Incremental);

        let eml = format!(
            "From: Utility <billing@utilityco.example>\r\n\
             To: me@example.com\r\n\
             Subject: PAST DUE: Your electric bill\r\n\
             Date: {}\r\n\
             \r\n\
             Amount due $84.20. This payment is overdue.\r\n",
            old.to_rfc2822()
        );
        let f = fixture(acct, "g-pastdue", &eml, false);
        let (mid, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental);
        assert_eq!(
            ev, None,
            "old mail is silent no matter what the verdict says"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());

        // Sanity: the guard stopped it, not a mis-triage.
        let updates = store
            .ranked_updates(acct, old - ChronoDuration::days(1), None)
            .unwrap();
        let bill = updates
            .iter()
            .find(|u| u.id == mid)
            .expect("bill surfaced in the client");
        assert_eq!(bill.tier, Tier::PastDue);
    }

    #[test]
    fn sealed_mail_never_emits_an_event() {
        // SEAL INVARIANT end to end: an OTP must never reach a lock screen.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eml = format!(
            "From: Bank <noreply@bank.example>\r\n\
             To: me@example.com\r\n\
             Subject: Your verification code\r\n\
             Date: {}\r\n\
             \r\n\
             Your one-time passcode is 483920. Enter this code to continue.\r\n",
            now.to_rfc2822()
        );
        let f = fixture(acct, "g-otp", &eml, false);
        let (_, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental);
        assert_eq!(ev, None, "sealed mail must never notify");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert_eq!(
            store.sealed_messages(acct).unwrap().len(),
            1,
            "it WAS sealed"
        );
    }

    #[test]
    fn squelched_sender_and_noise_are_both_silent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        // A SQUELCH rule outranks the heuristic that would otherwise have
        // surfaced this sender (proved above).
        store
            .set_sender_rule(
                acct,
                "*@monitoring.example",
                "not urgent",
                Disposition::Squelch,
            )
            .unwrap();
        let eml = alert_eml(now);
        let f = fixture(acct, "g-alert", &eml, false);
        let (_, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental);
        assert_eq!(ev, None, "a squelch-ruled sender is silent");

        // Plain below-the-line noise: fresh, confident, and simply not important.
        let news = format!(
            "From: News <hello@newsletter.example>\r\n\
             To: me@example.com\r\n\
             Subject: This week in widgets\r\n\
             Date: {}\r\n\
             \r\n\
             Lots of widget news. Click here to unsubscribe from these emails.\r\n",
            now.to_rfc2822()
        );
        let nf = fixture(acct, "g-news", &news, false);
        let (_, ev) = ingest_and_notify(&store, acct, &nf, now, IngestOrigin::Incremental);
        assert_eq!(ev, None, "noise below the line is silent");

        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
    }

    /// Mirror of the engine's STAGE-1 apply emission site for a row the pass
    /// ALREADY HOLDS: apply via the real `stage1_apply`, and emit only when the
    /// guarded UPDATE matched, consulting the rule list as it stands NOW.
    fn refine_row_and_notify(
        store: &SqliteStore,
        account_id: AccountId,
        row: &crate::store::Stage1Queued,
        tier: Tier,
        importance: u8,
        now: DateTime<Utc>,
    ) -> Option<i64> {
        let cfg = crate::config::NotifyConfig::default();
        let applied = crate::store::Stage1Applied {
            message_id: row.message_id,
            account_id,
            importance,
            tier,
            one_line: "refined one-liner".into(),
            reason: "stage-1".into(),
            field_reasons: crate::types::FieldReasons::default(),
            stage1_model_used: "claude-haiku-4-5".into(),
            needs_stage2: false,
            deadline: None,
            category: None,
        };
        // TOCTOU gate, as the engine has it: a verdict that did not land
        // (`false` — sealed mid-pass) must not emit.
        if !store.stage1_apply(&applied).unwrap() {
            return None;
        }
        let rules = store.list_sender_rules(account_id).unwrap();
        let ctx = events::EventContext {
            account_id,
            message_id: row.message_id,
            thread_id: &row.thread_id,
            sender: &row.from_addr,
            one_line: "refined one-liner",
            received_at: row.received_at,
            sensitivity: row.sensitivity,
            is_sent: false,
            rule: events::current_rule(&row.from_addr, &rules),
            tier,
            importance,
            deadline: None,
        };
        events::event_for(&ctx, &cfg, now).and_then(|ev| store.append_event(&ev).unwrap())
    }

    /// [`refine_row_and_notify`] when nothing is racing the queue read: fetch
    /// the queued row by id first.
    fn refine_and_notify(
        store: &SqliteStore,
        account_id: AccountId,
        message_id: i64,
        tier: Tier,
        importance: u8,
        now: DateTime<Utc>,
    ) -> Option<i64> {
        let row = store
            .stage1_queue(account_id, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == message_id)
            .expect("the row is queued for the stage-1 refine pass");
        refine_row_and_notify(store, account_id, &row, tier, importance, now)
    }

    #[test]
    fn a_row_sealed_mid_pass_lands_no_verdict_and_emits_nothing() {
        // TOCTOU: the pass SELECTs its queue, the user seals one of the held rows
        // (an OTP they spotted), and only THEN does the pass apply. Emitting on a
        // bare Ok would snapshot sender + one_line for a now-sealed message.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let f = fixture(acct, "g-alert", &alert_eml(now), false);
        let (mid, _) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Backfill);

        // The pass is already holding the queued row...
        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        // ...when the seal lands.
        store
            .correct_triage(acct, mid, TriageAxis::Sensitivity, "sealed", None, now)
            .unwrap()
            .unwrap();

        assert_eq!(
            refine_row_and_notify(&store, acct, &row, Tier::PastDue, 100, now),
            None,
            "a verdict that did not land must not notify"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
    }

    #[test]
    fn future_dated_backlog_mail_stays_silent_through_the_refine_pass() {
        // The `Date:` header is SENDER-CONTROLLED and ingest prefers it over
        // Gmail's internalDate. The refine passes grind the backlog
        // `received_at DESC` — future-dated rows FIRST — so without an upper
        // edge on the freshness window a fresh install storms on mail dated 2030.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let eml = alert_eml(now + ChronoDuration::days(365 * 4));
        let f = fixture(acct, "g-liar", &eml, false);
        let (mid, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Backfill);
        assert_eq!(ev, None, "backfill is silent by origin");

        // Sanity: the lying header really is what the row carries.
        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        assert!(
            row.received_at > now + ChronoDuration::days(1000),
            "the Date: header won"
        );

        assert_eq!(
            refine_and_notify(&store, acct, mid, Tier::PastDue, 100, now),
            None,
            "future-dated mail is outside the freshness window, loud verdict or not"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
    }

    #[test]
    fn squelching_a_sender_silences_rows_already_queued() {
        // THE REACTIVE SQUELCH: the mail is already in the Stage-1 queue when the
        // user squelches the sender, and the 'rule' marker is stamped at INGEST
        // only — so the refine site must read the rule list live or push mail
        // from a sender the user just silenced.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let f = fixture(acct, "g-alert", &alert_eml(now), false);
        let (mid, _) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Backfill);

        store
            .set_sender_rule(
                acct,
                "*@monitoring.example",
                "not urgent",
                Disposition::Squelch,
            )
            .unwrap();
        assert_eq!(
            refine_and_notify(&store, acct, mid, Tier::PastDue, 100, now),
            None,
            "a sender squelched AFTER the row was queued must not push"
        );

        // Control: the same verdict from an unruled sender does notify, so the
        // silence above is the rule and not the harness.
        let free = alert_eml(now).replace("alerts@monitoring.example", "alerts@other.example");
        let ff = fixture(acct, "g-other", &free, false);
        let (mid2, _) = ingest_and_notify(&store, acct, &ff, now, IngestOrigin::Backfill);
        assert!(
            refine_and_notify(&store, acct, mid2, Tier::PastDue, 100, now).is_some(),
            "unruled sender, same verdict: notifies"
        );
        assert_eq!(store.events_after(acct, 0, 100).unwrap().len(), 1);
    }

    // ---- budget-notice log redaction (PII safety) -------------------------

    #[test]
    fn redact_sender_hides_the_address_but_stays_stable() {
        let a = redact_sender("attacker@evil.example");
        assert!(a.starts_with("sender#"), "tagged form: {a}");
        assert_eq!(a.len(), "sender#".len() + 12, "12 hex chars of sha256");
        assert!(
            !a.contains("attacker") && !a.contains("evil"),
            "address must not leak: {a}"
        );
        let hex = &a["sender#".len()..];
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "hex only: {hex}"
        );
        // Deterministic (correlatable across a day) and injective per sender.
        assert_eq!(a, redact_sender("attacker@evil.example"));
        assert_ne!(a, redact_sender("someone@else.example"));
    }

    #[test]
    fn sanitize_ascii_strips_control_and_caps_length() {
        // Newlines (log-forging), ANSI escapes, and RTL-override become '.'.
        let clean = sanitize_ascii("abc\n\x1b[31mDEF\u{202e}", 64);
        assert!(!clean.contains('\n') && !clean.contains('\u{1b}') && !clean.contains('\u{202e}'));
        assert!(
            clean.starts_with("abc."),
            "printable kept, control replaced: {clean}"
        );
        // Pathologically long header can't flood the log.
        assert_eq!(sanitize_ascii(&"a".repeat(200), 10).chars().count(), 10);
    }

    // ---- base64url raw decode ---------------------------------------------

    #[test]
    fn decode_raw_b64url_no_pad_round_trips() {
        let eml = "From: a@b.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(eml);
        let out = decode_raw_b64url(&enc).unwrap();
        assert_eq!(out, eml.as_bytes());
    }

    #[test]
    fn decode_raw_b64url_accepts_padded_and_web_safe() {
        // 4 bytes => 6 base64 chars + '==' padding; values force '-'/'_' web-safe.
        let bytes: Vec<u8> = vec![0xfb, 0xff, 0xbf, 0xf0];
        let padded = base64::engine::general_purpose::URL_SAFE.encode(&bytes);
        assert!(padded.contains('='), "expected padding in this fixture");
        assert!(
            padded.contains('-') || padded.contains('_'),
            "expected web-safe chars in this fixture"
        );
        let out = decode_raw_b64url(&padded).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn decode_raw_b64url_rejects_garbage() {
        assert!(decode_raw_b64url("!!!not base64!!!").is_err());
    }

    // ---- history cursor advance -------------------------------------------

    #[test]
    fn advance_history_cursor_takes_max_never_regresses() {
        assert_eq!(advance_history_cursor(100, [50, 75, 40]), 100);
        assert_eq!(advance_history_cursor(100, [150, 120, 200]), 200);
        assert_eq!(advance_history_cursor(0, std::iter::empty()), 0);
        assert_eq!(advance_history_cursor(10, [10]), 10);
    }

    // ---- 404 / expired-history fallback decision --------------------------

    #[test]
    fn history_decision_incremental_when_cursor_present_and_fresh() {
        assert_eq!(
            history_poll_decision(Some(4242), false),
            HistoryDecision::Incremental(4242)
        );
    }

    #[test]
    fn history_decision_full_catchup_on_expired() {
        assert_eq!(
            history_poll_decision(Some(4242), true),
            HistoryDecision::FullCatchUp
        );
    }

    #[test]
    fn history_decision_full_catchup_when_absent_or_zero() {
        assert_eq!(
            history_poll_decision(None, false),
            HistoryDecision::FullCatchUp
        );
        assert_eq!(
            history_poll_decision(Some(0), false),
            HistoryDecision::FullCatchUp
        );
    }

    // ---- header synthesis for metadata-only sent seeding ------------------

    #[test]
    fn synthesize_headers_seeds_recipients_not_self() {
        // From is the account itself; contacts come from To/Cc recipients.
        let headers = vec![
            MessageHeader {
                name: "From".into(),
                value: "me@example.com".into(),
            },
            MessageHeader {
                name: "To".into(),
                value: "alice@friends.com".into(),
            },
            MessageHeader {
                name: "Cc".into(),
                value: "bob@friends.com".into(),
            },
            MessageHeader {
                name: "Subject".into(),
                value: "re: lunch".into(),
            },
            MessageHeader {
                name: "Date".into(),
                value: "Mon, 7 Jul 2026 10:00:00 +0000".into(),
            },
        ];
        let raw = synthesize_rfc822_headers(&headers);
        assert!(raw.ends_with("\r\n\r\n"));

        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mut f = fixture(acct, "g-sent", &raw, true);
        f.raw = raw.into_bytes();
        ingest_into(&store, acct, &f, Utc::now());
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
        assert!(store.is_known_contact(acct, "bob@friends.com").unwrap());
        // The account's own address must NEVER become a contact.
        assert!(!store.is_known_contact(acct, "me@example.com").unwrap());
    }

    #[test]
    fn synthesize_headers_drops_injected_newlines() {
        let headers = vec![MessageHeader {
            name: "From".into(),
            value: "x@y.com\r\nBcc: evil@z.com".into(),
        }];
        let raw = synthesize_rfc822_headers(&headers);
        assert!(!raw.contains("Bcc"), "CRLF-injected header must be dropped");
    }

    // ---- internalDate parsing ---------------------------------------------

    #[test]
    fn parse_internal_date_millis() {
        // 2026-07-07T10:00:00Z = 1783591200000 ms.
        let dt = parse_internal_date(Some("1783591200000")).unwrap();
        assert_eq!(dt.timestamp(), 1783591200);
        assert!(parse_internal_date(None).is_none());
        assert!(parse_internal_date(Some("garbage")).is_none());
    }

    #[test]
    fn parse_history_id_handles_bad_input() {
        assert_eq!(parse_history_id("12345"), 12345);
        assert_eq!(parse_history_id(""), 0);
        assert_eq!(parse_history_id("not-a-number"), 0);
    }

    // ---- ingest pipeline invariants (unchanged behavior) ------------------

    #[test]
    fn sealed_otp_stored_sealed_with_importance_zero() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Enter this code to continue.\r\n";
        let f = fixture(acct, "g-otp", eml, false);
        ingest_into(&store, acct, &f, Utc::now());

        let updates = store
            .ranked_updates(acct, Utc::now() - ChronoDuration::days(1), None)
            .unwrap();
        assert!(updates.is_empty(), "sealed OTP must not surface");

        let sealed = store.sealed_messages(acct).unwrap();
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].sealed_kind.as_deref(), Some("otp"));
    }

    #[test]
    fn dated_bill_stored_as_deadline_with_deadlines_row() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Acme <invoices@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #4402 from Acme\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your invoice total is $1,299.00. Payment due by August 15, 2026.\r\n";
        let now = DateTime::parse_from_rfc3339("2026-07-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let f = fixture(acct, "g-bill", eml, false);
        ingest_into(&store, acct, &f, now);

        let updates = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].tier, Tier::Deadline);

        let deadlines = store.deadlines(acct, Some(365)).unwrap();
        assert_eq!(deadlines.len(), 1, "a deadlines row must be written");
        assert_eq!(deadlines[0].amount, Some(1299.00));
        assert!(!deadlines[0].past_due);
    }

    #[test]
    fn past_due_bill_lands_past_due_tier() {
        // A CONFIDENT PastDue requires a TRUSTED sender, so seed the biller as a
        // known contact first: a legit past-due from a known biller still screams.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // Contacts are derived from Sent-mail recipients.
        let seed = "From: me@example.com\r\n\
                    To: Utility <billing@utilityco.com>\r\n\
                    Subject: account setup\r\n\
                    Date: Mon, 7 Jul 2026 09:00:00 +0000\r\n\
                    \r\n\
                    hello\r\n";
        let sf = fixture(acct, "g-seed", seed, /* is_sent */ true);
        ingest_into(&store, acct, &sf, Utc::now());
        assert!(
            store
                .is_known_contact(acct, "billing@utilityco.com")
                .unwrap()
        );

        let eml = "From: Utility <billing@utilityco.com>\r\n\
                   Subject: PAST DUE: Your electric bill\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Amount due $84.20. This payment is overdue.\r\n";
        let now = DateTime::parse_from_rfc3339("2026-07-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let f = fixture(acct, "g-pastdue", eml, false);
        ingest_into(&store, acct, &f, now);

        let updates = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        // The seed sent-message is excluded from ranked_updates; only the
        // past-due bill surfaces, at the top tier for a KNOWN sender.
        let bill = updates
            .iter()
            .find(|u| u.one_line.contains("PAST DUE"))
            .expect("past-due bill update present");
        assert_eq!(bill.tier, Tier::PastDue);
        let deadlines = store.deadlines(acct, None).unwrap();
        assert!(deadlines[0].past_due);
    }

    #[test]
    fn sent_message_seeds_recipient_contacts_never_self_and_skips_inbox() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // The user (me@example.com) sends to Alice, cc Bob. From == self.
        let eml = "From: me@example.com\r\n\
                   To: Alice <alice@friends.com>\r\n\
                   Cc: bob@friends.com\r\n\
                   Subject: re: lunch\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   sounds good\r\n";
        let now = Utc::now();
        let f = fixture(acct, "g-sent", eml, /* is_sent */ true);
        ingest_into(&store, acct, &f, now);

        // Recipients become contacts; the account's own address never does.
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
        assert!(store.is_known_contact(acct, "bob@friends.com").unwrap());
        assert!(!store.is_known_contact(acct, "me@example.com").unwrap());
        assert!(!store.is_known_contact(acct, "stranger@nowhere.io").unwrap());

        // Sent mail must NOT pollute the ranked inbox.
        let updates = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        assert!(
            updates.is_empty(),
            "sent mail must never surface in ranked_updates"
        );

        // And it must not appear in search results either.
        let hits = store.search(acct, "lunch", 10, 0).unwrap();
        assert!(hits.is_empty(), "sent mail must not appear in search");
    }

    // ---- HTML body: ingest sanitize + human-door serving ------------------

    #[test]
    fn html_email_stores_sanitized_html_served_by_client_thread_view() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // Dangerous markup (script, onerror, javascript: href, form) alongside
        // benign table/img/style content.
        let eml = "From: News <news@substack.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Weekly\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   \r\n\
                   <html><body><script>steal()</script>\
                   <table><tr><td style=\"color:red\">Hello</td></tr></table>\
                   <img src=\"https://cdn.example.com/x.png\" onerror=\"evil()\">\
                   <a href=\"javascript:evil()\">bad</a>\
                   <form action=\"http://evil\"><input name=\"pw\"></form>\
                   </body></html>\r\n";
        let f = fixture(acct, "g-html", eml, false);
        ingest_into(&store, acct, &f, Utc::now());

        // gmail_thread_id is None in `fixture`, so thread_id falls back to the
        // message id "g-html".
        let view = store
            .thread_view_with_html(acct, "g-html")
            .expect("thread present");
        let msg = &view.messages[0];
        let html = msg.html.as_deref().expect("html stored");

        // Dangerous constructs are gone.
        assert!(!html.to_lowercase().contains("script"));
        assert!(!html.contains("steal"));
        assert!(!html.to_lowercase().contains("onerror"));
        assert!(!html.contains("evil"));
        assert!(!html.to_lowercase().contains("javascript:"));
        assert!(!html.to_lowercase().contains("<form"));
        assert!(!html.to_lowercase().contains("<input"));
        // Benign content survives recognizably.
        assert!(html.contains("<table"));
        assert!(html.contains("style=\"color:red\""));
        assert!(html.contains("https://cdn.example.com/x.png"));

        // The flattened text path still feeds triage/FTS.
        assert!(msg.content.contains("Hello"));
        assert!(!msg.content.contains('<'));
    }

    #[test]
    fn plaintext_email_leaves_html_null() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Alice <alice@friends.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: hi\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   plain text only, no markup\r\n";
        let f = fixture(acct, "g-plain", eml, false);
        ingest_into(&store, acct, &f, Utc::now());

        let view = store.thread_view_with_html(acct, "g-plain").unwrap();
        assert!(
            view.messages[0].html.is_none(),
            "plain-text-only mail must leave html NULL"
        );
        assert!(view.messages[0].content.contains("plain text only"));
    }

    #[test]
    fn sync_state_round_trips_history_id() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        assert!(store.sync_state(acct, HISTORY_KEY).unwrap().is_none());

        // A historyId larger than u32::MAX, to prove the field holds it.
        let big = (u32::MAX as u64) + 123_456;
        store
            .set_sync_state(
                acct,
                HISTORY_KEY,
                &SyncState {
                    uidvalidity: 0,
                    last_uid: big,
                },
            )
            .unwrap();
        let s = store.sync_state(acct, HISTORY_KEY).unwrap().unwrap();
        assert_eq!(s.last_uid, big);
    }

    #[test]
    fn urlencode_escapes_spaces_and_reserved() {
        assert_eq!(urlencode("newer_than:30d"), "newer_than:30d");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("x&y"), "x%26y");
    }

    #[test]
    fn subtract_ids_drops_the_claimed_and_keeps_order() {
        let claimed = vec!["b".to_string(), "d".to_string()];
        let ids = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert_eq!(
            subtract_ids(ids, &claimed),
            vec!["a".to_string(), "c".into()]
        );
        // Nothing claimed: the batch passes through untouched.
        assert_eq!(
            subtract_ids(vec!["a".to_string()], &[]),
            vec!["a".to_string()]
        );
    }

    // ---- the two-walk incremental poll, on the wire ------------------------
    //
    // `history.list` is LABEL-FILTERED, so an INBOX-only poll is structurally
    // blind to mail the user sends from Gmail web or their phone. Everything
    // under test here — walk ORDER, the single cursor commit, a cursor HELD on a
    // sent-side failure — lives in the sequence of HTTP calls and cannot be
    // asserted on a struct, so these drive the real engine against an axum app on
    // an ephemeral loopback port (the shape tests/push_relay.rs already uses).

    use async_trait::async_trait;
    use axum::extract::{Path as AxumPath, Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// One label's scripted `history.list` answer.
    #[derive(Clone, Default)]
    struct LabelHistory {
        /// `messageAdded` records, as (historyId, message ids).
        records: Vec<(u64, Vec<String>)>,
        /// The page-level newest historyId.
        top: u64,
        /// Answer 500 instead: an outage that hits ONE label's walk.
        fail: bool,
    }

    impl LabelHistory {
        fn added(top: u64, records: &[(u64, &str)]) -> Self {
            Self {
                records: records
                    .iter()
                    .map(|(hid, id)| (*hid, vec![id.to_string()]))
                    .collect(),
                top,
                fail: false,
            }
        }
        fn quiet(top: u64) -> Self {
            Self {
                records: Vec::new(),
                top,
                fail: false,
            }
        }
        fn broken() -> Self {
            Self {
                records: Vec::new(),
                top: 0,
                fail: true,
            }
        }
    }

    #[derive(Default)]
    struct GmailState {
        /// labelId -> its `history.list` answer.
        history: HashMap<String, LabelHistory>,
        /// labelIds -> the ids `messages.list` returns (the catch-up path).
        listing: HashMap<String, Vec<String>>,
        /// gmail message id -> RFC822 source.
        bodies: HashMap<String, String>,
        /// labelId -> the verbatim 200 body `labels.get` answers with. Absent
        /// (or `None`) means 500: an unscripted label must not read as a real
        /// zero. Whole bodies rather than a counter pair because the shapes
        /// under test include ones that are not label resources at all.
        labels: HashMap<String, Option<Value>>,
        /// `users.getProfile`'s historyId.
        profile_history_id: u64,
        /// Every call this mock served, in order, as `verb:key`.
        seen: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct MockGmail(Arc<Mutex<GmailState>>);

    impl MockGmail {
        fn history(&self, label: &str, h: LabelHistory) -> &Self {
            self.0.lock().unwrap().history.insert(label.to_string(), h);
            self
        }
        fn listing(&self, label: &str, ids: &[&str]) -> &Self {
            self.0.lock().unwrap().listing.insert(
                label.to_string(),
                ids.iter().map(|s| s.to_string()).collect(),
            );
            self
        }
        fn body(&self, id: &str, eml: String) -> &Self {
            self.0.lock().unwrap().bodies.insert(id.to_string(), eml);
            self
        }
        /// Script `labels.get` for one label: `Some(counts)` answers a
        /// well-formed label resource, `None` answers 500 (the outage the
        /// stored counts must survive).
        fn label(&self, label: &str, unread: Option<(i64, i64)>) -> &Self {
            let body = unread.map(|(messages, threads)| {
                json!({
                    "id": label,
                    "messagesUnread": messages,
                    "threadsUnread": threads,
                })
            });
            self.0
                .lock()
                .unwrap()
                .labels
                .insert(label.to_string(), body);
            self
        }
        /// Script `labels.get` with a verbatim 200 body, for the shapes
        /// [`MockGmail::label`] cannot express: a label Gmail sent without
        /// counters, or a 200 that is not a label resource.
        fn label_body(&self, label: &str, body: Value) -> &Self {
            self.0
                .lock()
                .unwrap()
                .labels
                .insert(label.to_string(), Some(body));
            self
        }
        fn profile(&self, history_id: u64) -> &Self {
            self.0.lock().unwrap().profile_history_id = history_id;
            self
        }
        fn seen(&self) -> Vec<String> {
            self.0.lock().unwrap().seen.clone()
        }
        fn calls(&self, key: &str) -> usize {
            self.seen().iter().filter(|s| *s == key).count()
        }
        /// Index of `key` in the call log; panics if it was never served.
        fn at(&self, key: &str) -> usize {
            self.seen()
                .iter()
                .position(|s| s == key)
                .unwrap_or_else(|| panic!("{key} was never called; saw {:?}", self.seen()))
        }
    }

    async fn mock_history(
        State(g): State<MockGmail>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let label = q.get("labelId").cloned().unwrap_or_default();
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("history:{label}"));
        let h = st.history.get(&label).cloned().unwrap_or_default();
        if h.fail {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let records: Vec<Value> = h
            .records
            .iter()
            .map(|(hid, ids)| {
                json!({
                    "id": hid.to_string(),
                    "messagesAdded": ids
                        .iter()
                        .map(|m| json!({ "message": { "id": m } }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        Json(json!({ "history": records, "historyId": h.top.to_string() })).into_response()
    }

    async fn mock_messages_list(
        State(g): State<MockGmail>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let label = q.get("labelIds").cloned().unwrap_or_default();
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("list:{label}"));
        let ids = st.listing.get(&label).cloned().unwrap_or_default();
        Json(json!({
            "messages": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
        }))
    }

    async fn mock_message_get(
        State(g): State<MockGmail>,
        AxumPath(id): AxumPath<String>,
    ) -> Response {
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("get:{id}"));
        match st.bodies.get(&id) {
            // Both shapes at once: `raw` for the ingest fetches, and the
            // `payload.headers` a `format=metadata` caller reads. Gmail sends one
            // or the other per the requested format; serving both keeps the mock
            // format-agnostic, and each caller reads only its own field.
            Some(eml) => Json(json!({
                "id": id,
                "raw": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(eml),
                "payload": { "headers": mock_headers(eml) },
                "internalDate": Utc::now().timestamp_millis().to_string(),
            }))
            .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    /// The header block of an RFC822 fixture as Gmail's `payload.headers[]`.
    /// Fixtures are single-line headers, so no continuation handling. Values are
    /// trimmed of the CR the fixtures' CRLF endings leave behind — Gmail's own
    /// values are bare, and `synthesize_rfc822_headers` drops anything carrying
    /// one as an injection attempt.
    fn mock_headers(eml: &str) -> Vec<Value> {
        eml.lines()
            .take_while(|l| !l.trim().is_empty())
            .filter_map(|l| l.split_once(": "))
            .map(|(name, value)| json!({ "name": name.trim(), "value": value.trim() }))
            .collect()
    }

    async fn mock_label(State(g): State<MockGmail>, AxumPath(id): AxumPath<String>) -> Response {
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("label:{id}"));
        match st.labels.get(&id).cloned().flatten() {
            Some(body) => Json(body).into_response(),
            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    async fn mock_profile(State(g): State<MockGmail>) -> Json<Value> {
        let st = g.0.lock().unwrap();
        Json(json!({ "historyId": st.profile_history_id.to_string() }))
    }

    /// Bind the mock on an ephemeral loopback port; returns its API base.
    async fn serve_mock(g: MockGmail) -> String {
        let app = Router::new()
            .route("/history", get(mock_history))
            .route("/messages", get(mock_messages_list))
            .route("/messages/{id}", get(mock_message_get))
            .route("/labels/{id}", get(mock_label))
            .route("/profile", get(mock_profile))
            .with_state(g);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// A credential store handing out one fixed token; the mock ignores it.
    struct FixedToken;

    #[async_trait]
    impl CredentialStore for FixedToken {
        async fn token(&self, _account: AccountId) -> Result<crate::credentials::OAuthToken> {
            Ok(crate::credentials::OAuthToken {
                access_token: "test-token".to_string(),
                refresh_token: None,
                expires_at: None,
            })
        }
    }

    fn engine(
        store: Arc<SqliteStore>,
        acct: AccountId,
        base: &str,
    ) -> SyncEngine<SqliteStore, FixedToken> {
        SyncEngine::new(
            store,
            Arc::new(FixedToken),
            acct,
            "me@example.com".to_string(),
            Config::default(),
        )
        .with_api_base(base.to_string())
    }

    /// Open an in-memory store with a live history cursor at `cursor`.
    fn store_at_cursor(cursor: Option<u64>) -> (Arc<SqliteStore>, AccountId) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        if let Some(c) = cursor {
            store
                .set_sync_state(
                    acct,
                    HISTORY_KEY,
                    &SyncState {
                        uidvalidity: 0,
                        last_uid: c,
                    },
                )
                .unwrap();
        }
        (store, acct)
    }

    fn cursor_of(store: &SqliteStore, acct: AccountId) -> Option<u64> {
        store
            .sync_state(acct, HISTORY_KEY)
            .unwrap()
            .map(|s| s.last_uid)
    }

    /// Mail the user WROTE, dated `at` so it is inside the freshness window —
    /// the storm guard must not be what makes these tests quiet.
    fn sent_eml(at: DateTime<Utc>, to: &str, subject: &str) -> String {
        format!(
            "From: me@example.com\r\n\
             To: {to}\r\n\
             Subject: {subject}\r\n\
             Date: {}\r\n\
             \r\n\
             writing this from the phone app\r\n",
            at.to_rfc2822()
        )
    }

    #[tokio::test]
    async fn a_sent_history_record_ingests_neutral_and_silent() {
        // Sent from the phone: SENT only, never INBOX. Before the sent walk this
        // message simply did not exist locally until the next full backfill.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::quiet(100));
        g.history(LABEL_SENT, LabelHistory::added(140, &[(140, "g-phone")]));
        g.body(
            "g-phone",
            sent_eml(now, "alice@friends.com", "lunch thursday"),
        );
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        // The row landed...
        let view = store.thread_view(acct, "g-phone").unwrap();
        assert_eq!(view.messages.len(), 1);
        let mid = view.messages[0].id;

        // ...NEUTRAL, and out of both LLM queues: no triage spend on the user's
        // own writing, ever.
        let row = store.triage_debug(acct, mid).unwrap().expect("triage row");
        assert_eq!(row.tier, "noise");
        assert_eq!(row.importance, 0);
        assert_eq!(row.stage1_model_used.as_deref(), Some("n/a"));
        assert!(!row.needs_stage2);
        assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
        assert!(store.stage2_queue(acct, 10).unwrap().is_empty());

        // THE SILENCE INVARIANT: never a push for mail the user sent themselves.
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert_eq!(store.latest_event_id(acct).unwrap(), 0);

        // And it stays out of the bands and out of search, as sent mail must.
        assert!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .is_empty()
        );
        assert!(store.search(acct, "lunch", 10, 0).unwrap().is_empty());

        // The recipients still seed contacts, which is half the point of having
        // the mail at all.
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
        assert_eq!(cursor_of(&store, acct), Some(140));
    }

    /// A sent row exactly as an install predating `to_addrs` left it: recipients
    /// NULL, triage row present.
    fn old_sent_row(store: &SqliteStore, acct: AccountId, gmail: &str) -> i64 {
        let id = store
            .upsert_message(&crate::types::NewMessage {
                account_id: acct,
                gmail_msg_id: gmail.to_string(),
                thread_id: format!("t-{gmail}"),
                from_addr: "me@example.com".to_string(),
                from_name: Some("Me".to_string()),
                subject: "lunch thursday".to_string(),
                received_at: Utc::now(),
                snippet: String::new(),
                body: "writing this from the phone app".to_string(),
                body_html: None,
                is_sent: true,
                to_addrs: None,
                list_unsubscribe: None,
                list_unsub_one_click: false,
                auth_pass: None,
            })
            .unwrap();
        store
            .set_triage(
                id,
                acct,
                0,
                Tier::Noise,
                crate::types::Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        id
    }

    #[tokio::test]
    async fn the_sent_recipients_backfill_fills_old_rows_exactly_once() {
        // Sent mail ingested before `to_addrs` existed has no recipients to show.
        // The one-shot backfill fetches headers only and fills them, then its
        // sync_state flag keeps it from ever paying for that walk again.
        let (store, acct) = store_at_cursor(Some(100));
        let id = old_sent_row(&store, acct, "g-old");

        let g = MockGmail::default();
        g.body(
            "g-old",
            sent_eml(Utc::now(), "Alice <alice@friends.com>", "lunch thursday"),
        );
        let base = serve_mock(g.clone()).await;
        let eng = engine(store.clone(), acct, &base);

        eng.backfill_sent_recipients().await.unwrap();
        let listed = store.sent_listing(acct, 10, 0).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].to, "Alice <alice@friends.com>");
        assert!(store.sent_missing_recipients(acct, 10).unwrap().is_empty());

        // Second run: the flag is set, so not one more Gmail call.
        let calls = g.calls("get:g-old");
        eng.backfill_sent_recipients().await.unwrap();
        assert_eq!(
            g.calls("get:g-old"),
            calls,
            "the sweep runs once per install"
        );
    }

    #[tokio::test]
    async fn the_sent_recipients_backfill_records_a_message_gmail_no_longer_has() {
        // A 404 is an answer, not a failure: the row is marked "looked, nobody
        // named" so it leaves the queue. Left NULL it would be handed back by the
        // very next batch query, and the pass would never terminate.
        let (store, acct) = store_at_cursor(Some(100));
        old_sent_row(&store, acct, "g-gone");

        let base = serve_mock(MockGmail::default()).await;
        engine(store.clone(), acct, &base)
            .backfill_sent_recipients()
            .await
            .unwrap();

        assert!(store.sent_missing_recipients(acct, 10).unwrap().is_empty());
        assert_eq!(store.sent_listing(acct, 10, 0).unwrap()[0].to, "");
    }

    #[tokio::test]
    async fn a_failed_sent_recipients_backfill_leaves_the_flag_unset() {
        // A non-404 error SKIPS the message (its row stays NULL) and the pass
        // finishes Ok — one message with a persistent 4xx quirk must not abort
        // the sweep for everything behind it. But the done flag is only set on
        // a clean pass, so the next daemon start redoes whatever is still NULL.
        let (store, acct) = store_at_cursor(Some(100));
        old_sent_row(&store, acct, "g-old");

        // No route at all: every fetch is a transport error, not a 404.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let dead = format!("http://{addr}");

        engine(store.clone(), acct, &dead)
            .backfill_sent_recipients()
            .await
            .unwrap();
        assert!(
            store
                .sync_state(acct, SENT_RECIPIENTS_KEY)
                .unwrap()
                .is_none(),
            "an interrupted sweep must retry on the next start"
        );
        assert_eq!(store.sent_missing_recipients(acct, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_sealed_outbound_copy_is_never_committed() {
        // The user replies from their phone to a thread quoting an OTP. Seal
        // detection fires on the reply, and `thread_guard_and_subject` 404s any
        // thread holding a sealed row — so committing it would hide the
        // counterparty's mail the user is reading. Nothing is written at all.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let reply = format!(
            "From: me@example.com\r\n\
             To: Bank <noreply@bank.example>\r\n\
             Subject: Re: Your verification code\r\n\
             Date: {}\r\n\
             \r\n\
             got it, thanks\r\n\
             > Your one-time passcode is 483920. Enter this code to continue.\r\n",
            now.to_rfc2822()
        );

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::quiet(100));
        g.history(
            LABEL_SENT,
            LabelHistory::added(170, &[(170, "g-sealed-out")]),
        );
        g.body("g-sealed-out", reply);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert!(
            store.thread_view(acct, "g-sealed-out").is_err(),
            "no row is committed for a sealed outbound copy"
        );
        assert!(
            store.sealed_messages(acct).unwrap().is_empty(),
            "not committed-then-sealed: not committed at all"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        // The poll is otherwise normal: the cursor still commits.
        assert_eq!(cursor_of(&store, acct), Some(170));
    }

    #[tokio::test]
    async fn a_self_addressed_send_keeps_its_visible_inbox_copy() {
        // Mail to yourself carries BOTH labels, so both filtered walks return it.
        // The message upsert writes `is_sent = excluded.is_sent` on conflict, so a
        // sent copy landing second would hide the message from every band.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let eml = format!(
            "From: me@example.com\r\n\
             To: me@example.com\r\n\
             Subject: note to self about the lease\r\n\
             Date: {}\r\n\
             \r\n\
             remember to countersign\r\n",
            now.to_rfc2822()
        );

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(150, &[(150, "g-self")]));
        g.history(LABEL_SENT, LabelHistory::added(150, &[(150, "g-self")]));
        g.body("g-self", eml);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        // ONE row, and it is the visible one.
        let view = store.thread_view(acct, "g-self").unwrap();
        assert_eq!(view.messages.len(), 1, "one row, not two");
        let visible = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        assert_eq!(
            visible.len(),
            1,
            "the inbox copy must win the unique-key race"
        );
        assert_eq!(visible[0].id, view.messages[0].id);

        // The INBOX walk ran FIRST and claimed the id, so the sent half never even
        // re-fetched it.
        assert!(
            g.at("history:INBOX") < g.at("history:SENT"),
            "{:?}",
            g.seen()
        );
        assert_eq!(g.calls("get:g-self"), 1, "fetched once, not twice");
    }

    #[tokio::test]
    async fn a_failing_sent_walk_keeps_the_inbox_batch_and_holds_the_cursor() {
        // Losing the sent half is survivable; losing INBOX progress is not. The
        // cursor stays put and the next poll re-walks both labels from it.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let eml = format!(
            "From: Alice <alice@friends.com>\r\n\
             To: me@example.com\r\n\
             Subject: the lease\r\n\
             Date: {}\r\n\
             \r\n\
             sending the countersigned copy over\r\n",
            now.to_rfc2822()
        );

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(150, &[(150, "g-in")]));
        g.history(LABEL_SENT, LabelHistory::broken());
        g.body("g-in", eml);
        let base = serve_mock(g.clone()).await;

        // The poll SUCCEEDS: a sent-side outage is not worth a backoff cycle.
        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert_eq!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .len(),
            1,
            "the inbox batch is ingested even though the sent walk failed"
        );
        assert_eq!(
            cursor_of(&store, acct),
            Some(100),
            "a partial poll must not commit the cursor"
        );

        // The re-walk next poll is idempotent: still one row, still one event.
        let events_before = store.events_after(acct, 0, 100).unwrap().len();
        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();
        assert_eq!(store.thread_view(acct, "g-in").unwrap().messages.len(), 1);
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            events_before
        );
        assert_eq!(cursor_of(&store, acct), Some(100));
    }

    #[tokio::test]
    async fn the_cursor_commits_once_at_the_max_across_both_walks() {
        // One commit, at the highest historyId either walk saw — committing per
        // walk would let the second one rewind the first.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(150, &[(150, "g-in")]));
        g.history(LABEL_SENT, LabelHistory::added(220, &[(220, "g-out")]));
        g.body(
            "g-in",
            format!(
                "From: Alice <alice@friends.com>\r\n\
                 To: me@example.com\r\n\
                 Subject: the lease\r\n\
                 Date: {}\r\n\
                 \r\n\
                 countersigned copy attached\r\n",
                now.to_rfc2822()
            ),
        );
        g.body("g-out", sent_eml(now, "alice@friends.com", "re: the lease"));
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert_eq!(cursor_of(&store, acct), Some(220), "max across both walks");
        // Both landed, each on its own side of the visibility line.
        assert_eq!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .len(),
            1,
            "only the received message is in a band"
        );
        assert_eq!(store.thread_view(acct, "g-out").unwrap().messages.len(), 1);
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
    }

    #[tokio::test]
    async fn catch_up_lists_inbox_and_sent_over_the_same_window() {
        // No cursor: the history walk cannot account for the gap, so both labels
        // are re-listed or everything written from another client is lost.
        let (store, acct) = store_at_cursor(None);
        let now = Utc::now();

        let g = MockGmail::default();
        // "g-self" is in BOTH listings, as a self-addressed message is.
        g.listing(LABEL_INBOX, &["g-in", "g-self"]);
        g.listing(LABEL_SENT, &["g-out", "g-self"]);
        g.body(
            "g-in",
            format!(
                "From: Alice <alice@friends.com>\r\n\
                 To: me@example.com\r\n\
                 Subject: the lease\r\n\
                 Date: {}\r\n\
                 \r\n\
                 countersigned copy attached\r\n",
                now.to_rfc2822()
            ),
        );
        g.body("g-out", sent_eml(now, "bob@friends.com", "re: the lease"));
        g.body(
            "g-self",
            format!(
                "From: me@example.com\r\n\
                 To: me@example.com\r\n\
                 Subject: note to self\r\n\
                 Date: {}\r\n\
                 \r\n\
                 countersign this\r\n",
                now.to_rfc2822()
            ),
        );
        g.profile(900);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert!(g.at("list:INBOX") < g.at("list:SENT"), "{:?}", g.seen());
        assert_eq!(store.thread_view(acct, "g-out").unwrap().messages.len(), 1);
        assert!(store.is_known_contact(acct, "bob@friends.com").unwrap());
        // The dual-listed message keeps its visible copy, exactly as in the walk.
        let visible: Vec<String> = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap()
            .into_iter()
            .map(|u| u.thread_id)
            .collect();
        assert!(visible.contains(&"g-in".to_string()));
        assert!(visible.contains(&"g-self".to_string()));
        assert_eq!(g.calls("get:g-self"), 1, "fetched once, not twice");
        assert_eq!(cursor_of(&store, acct), Some(900));
    }

    #[tokio::test]
    async fn a_message_the_api_already_echoed_redelivers_as_a_no_op() {
        // The api echoes a send at send time; the sent walk then delivers the very
        // same Gmail id on the next poll.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let eml = sent_eml(now, "alice@friends.com", "re: the lease");

        let echoed = ingest::ingest_sent(
            &store,
            acct,
            "me@example.com",
            "g-echo",
            None,
            eml.clone().into_bytes(),
            Some(now),
            now,
        )
        .unwrap()
        .expect("the echo commits a row");

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::quiet(100));
        g.history(LABEL_SENT, LabelHistory::added(160, &[(160, "g-echo")]));
        g.body("g-echo", eml);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        let view = store.thread_view(acct, "g-echo").unwrap();
        assert_eq!(view.messages.len(), 1, "one row, not a second copy");
        assert_eq!(view.messages[0].id, echoed, "the same local row");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .is_empty(),
            "re-delivery must not promote the echo into a band"
        );
        let row = store
            .triage_debug(acct, echoed)
            .unwrap()
            .expect("triage row");
        assert_eq!(row.tier, "noise");
        assert_eq!(row.importance, 0);
    }

    #[tokio::test]
    async fn the_inbox_unread_counts_refresh_and_survive_a_failed_fetch() {
        // Gmail owns these numbers: nothing local can be re-derived into them,
        // so a failed fetch must leave the last known pair standing rather than
        // clear it or read as "0 unread".
        let (store, acct) = store_at_cursor(Some(100));

        let g = MockGmail::default();
        g.label(LABEL_INBOX, Some((214, 190)));
        let base = serve_mock(g.clone()).await;
        let engine = engine(store.clone(), acct, &base);

        engine.refresh_inbox_unread().await;
        let got = store.inbox_unread(acct).unwrap().expect("counts stored");
        assert_eq!((got.messages, got.threads), (214, 190));
        assert_eq!(g.calls("label:INBOX"), 1, "one labels.get per refresh");

        // Gmail goes down mid-run: the refresh swallows it (a poll cycle must
        // not die over a cosmetic number) and the stored pair is untouched.
        g.label(LABEL_INBOX, None);
        engine.refresh_inbox_unread().await;
        let kept = store.inbox_unread(acct).unwrap().expect("last counts kept");
        assert_eq!((kept.messages, kept.threads), (214, 190));
        assert_eq!(kept.fetched_at, got.fetched_at, "the stamp did not move");

        // Recovery overwrites with the newer truth.
        g.label(LABEL_INBOX, Some((3, 3)));
        engine.refresh_inbox_unread().await;
        let fresh = store.inbox_unread(acct).unwrap().unwrap();
        assert_eq!((fresh.messages, fresh.threads), (3, 3));
    }

    #[tokio::test]
    async fn a_200_that_is_not_a_label_keeps_the_counts_but_a_counterless_label_zeroes_them() {
        // The two 200s that look alike to a defaulted decoder and must not be
        // treated alike: Gmail dropping the counters (a real zero) versus a
        // body that is not a label at all (no answer, keep the last one).
        let (store, acct) = store_at_cursor(Some(100));

        let g = MockGmail::default();
        g.label(LABEL_INBOX, Some((214, 190)));
        let base = serve_mock(g.clone()).await;
        let engine = engine(store.clone(), acct, &base);
        engine.refresh_inbox_unread().await;
        let seeded = store.inbox_unread(acct).unwrap().expect("counts stored");

        // Not label resources: an empty object and an error envelope served
        // with a 200. Required `id` makes each a decode error, so the seeded
        // pair stands untouched — the stamp included.
        for body in [json!({}), json!({ "error": { "code": 403 } })] {
            g.label_body(LABEL_INBOX, body);
            engine.refresh_inbox_unread().await;
            let kept = store.inbox_unread(acct).unwrap().expect("last counts kept");
            assert_eq!((kept.messages, kept.threads), (214, 190));
            assert_eq!(kept.fetched_at, seeded.fetched_at, "the stamp did not move");
        }

        // A label resource WITHOUT the counters is Gmail's way of saying zero:
        // it omits them on a label with nothing unread, so this one is stored.
        g.label_body(LABEL_INBOX, json!({ "id": LABEL_INBOX, "name": "INBOX" }));
        engine.refresh_inbox_unread().await;
        let zeroed = store
            .inbox_unread(acct)
            .unwrap()
            .expect("zero is an answer");
        assert_eq!((zeroed.messages, zeroed.threads), (0, 0));
    }
}
