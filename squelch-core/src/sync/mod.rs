//! The Gmail sync engine (REST API, polling).
//!
//! Responsibilities:
//! - Talk to the Gmail REST API at `https://gmail.googleapis.com/gmail/v1/users/me/...`
//!   over HTTPS (reqwest + rustls) using a Bearer access token from the existing
//!   [`CredentialStore`]. The read-only `gmail.readonly` OAuth scope is honored
//!   by the REST API (unlike IMAP XOAUTH2, which rejects it) — this is the whole
//!   reason the transport is REST and not IMAP.
//! - On first run, backfill the last `backfill_days` of INBOX (`format=raw` ->
//!   RFC822 bytes) plus SENT headers (`format=metadata`) to seed contacts, then
//!   record the account's `historyId`.
//! - Then poll `history.list` every `poll_secs` for `messageAdded` events on
//!   INBOX, fetch each new message `format=raw`, and ingest, advancing the
//!   `historyId` cursor. A 404 (expired historyId) triggers a fresh catch-up.
//!
//! SECURITY INVARIANTS honored here:
//! - The OAuth scope is fixed read-only upstream; we only ever *read* mail.
//! - Every fetched message goes through
//!   [`crate::sync::ingest::ingest_with_rules`] which runs seal detection FIRST,
//!   so sealed mail is classified and stored `sensitivity='sealed'` in the same
//!   transaction with importance 0 and never reaches Stage-2 or any LLM.
//! - Tokens / `Authorization` headers / message bodies are NEVER logged. Only
//!   counts and redacted context.

pub mod html;
pub mod ingest;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::config::{Config, Stage2Provider};
use crate::credentials::CredentialStore;
use crate::error::{CoreError, Result};
use crate::store::{Store, SyncState};
use crate::sync::ingest::{RawFetched, ingest_with_rules};
use crate::triage::stage2::{self, ClassifyOutcome, RowContext};
use crate::triage::stage2_sealed_guard;
use crate::types::{AccountId, SenderRule, Sensitivity};

/// Gmail REST base for the authenticated user. Fixed; not user-tunable.
const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// The INBOX and SENT label ids (Gmail system labels).
const LABEL_INBOX: &str = "INBOX";
const LABEL_SENT: &str = "SENT";

/// The single `sync_state` row key for the REST engine's historyId cursor.
const HISTORY_KEY: &str = "history";

/// The `wake_budget.thread_id` sentinel for the global-per-account-per-day
/// Stage-2 budget. No real Gmail thread id can collide (Gmail thread ids are
/// hex, never this literal). Keeping global counting in the same table avoids a
/// schema addition.
const GLOBAL_BUDGET_KEY: &str = "__global__";

/// Prefix for the per-SENDER-per-day Stage-2 budget key stored in the same
/// `wake_budget` table (`thread_id = "sender:<addr>"`). No real Gmail thread id
/// starts with `sender:` (they are hex), so this never collides with a real
/// per-thread row or the `__global__` sentinel. Mirrors the `__global__`
/// pattern; avoids a schema addition. (Schema applies fresh; dev dbs get reset.)
const SENDER_BUDGET_PREFIX: &str = "sender:";

/// Model id stamped on a row skipped for being older than `stage2_max_age_days`:
/// it is marked processed WITHOUT a model call so it neither consumes budget nor
/// sits queued forever, keeping its Stage-1 values.
const STALE_SKIP_MODEL: &str = "stale-skip";

/// Reconnect / retry backoff bounds for the outer driver loop.
const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// Decode a base64url (Gmail `format=raw`) payload into RFC822 bytes.
///
/// Gmail returns the raw message web-safe base64url encoded, usually WITHOUT
/// padding. We accept both padded and unpadded input. Errors are surfaced (not
/// logged with content) so a single bad message doesn't poison the batch.
pub fn decode_raw_b64url(s: &str) -> Result<Vec<u8>> {
    // Try no-pad first (Gmail's usual shape), then the padded variant.
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s.trim()))
        .map_err(|e| CoreError::InvalidInput(format!("base64url decode failed: {e}")))
}

/// Decide, given the persisted historyId cursor, whether the incremental poll
/// can proceed (Some(history_id)) or a fresh backfill-style catch-up is required
/// (None). Pure so the 404-fallback path is unit-testable without a network.
///
/// `expired` reflects an HTTP 404 from `history.list` (Gmail drops history
/// older than ~a week). `cursor` is the stored historyId (0 / absent means we
/// never established one, i.e. first run).
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

/// Advance a historyId cursor: take the max of the current cursor and every
/// `historyId` observed in a `history.list` page, never moving backwards. Pure
/// and network-free so cursor arithmetic is unit-testable.
pub fn advance_history_cursor(current: u64, observed: impl IntoIterator<Item = u64>) -> u64 {
    observed.into_iter().fold(current, u64::max)
}

// ---- Gmail REST response shapes (only the fields we consume) ---------------

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    thread_id: Option<String>,
    /// base64url of the full RFC822 message (present with `format=raw`).
    #[serde(default)]
    raw: Option<String>,
    /// Milliseconds since epoch as a decimal string (Gmail's `internalDate`).
    #[serde(default)]
    internal_date: Option<String>,
}

/// A single Gmail metadata header. Test-only now that SENT backfill fetches full
/// bodies (`format=raw`) rather than headers — the contacts-seeding tests build
/// these to exercise the header-parsing ingest path via
/// [`synthesize_rfc822_headers`].
#[cfg(test)]
struct MessageHeader {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileResp {
    history_id: String,
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

/// Everything the sync loop needs, resolved once at startup.
pub struct SyncEngine<S: Store, C: CredentialStore> {
    store: Arc<S>,
    creds: Arc<C>,
    account_id: AccountId,
    /// The account's own email; passed to ingest so the user's own address is
    /// excluded from the Sent-derived contacts table.
    account_email: String,
    config: Config,
    http: reqwest::Client,
    /// The resolved Stage-2 API key and its provider, if present at startup.
    /// When `None`, Stage-2 is DISABLED gracefully (rows stay queued, one stderr
    /// notice, sync continues). The key is never logged.
    stage2_key: Option<(String, Stage2Provider)>,
    /// On-box embedder OVERRIDE for v1 semantic recall. Usually `None`: the
    /// effective embedder is resolved via [`SyncEngine::embedder`], which falls
    /// back to the STORE's embedder. Reading it from the store each time means a
    /// LATE-attached embedder (e.g. `squelchd serve` attaches it in the background
    /// after binding the port) is picked up automatically — until it is ready,
    /// ingest simply skips the vector write and the startup/periodic backfill pass
    /// fills the gap once the embedder appears. CPU work runs under
    /// `spawn_blocking` so the poll loop never stalls on it.
    embedder: Option<Arc<dyn crate::embed::Embedder>>,
}

impl<S: Store + 'static, C: CredentialStore + 'static> SyncEngine<S, C> {
    pub fn new(
        store: Arc<S>,
        creds: Arc<C>,
        account_id: AccountId,
        account_email: String,
        config: Config,
    ) -> Self {
        // rustls-only client; no native-tls. Timeouts keep a hung connection
        // from wedging the poll loop.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client build");
        // Resolve the Stage-2 API key + provider once. Absence => graceful
        // disable with a single stderr notice (no key material logged).
        let stage2_key = config.stage2.resolve_key_and_provider();
        if stage2_key.is_none() {
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
            stage2_key,
            embedder: None,
        }
    }

    /// Attach an on-box [`Embedder`](crate::embed::Embedder) OVERRIDE so ingest
    /// and the startup backfill populate the semantic-recall vector index. Usually
    /// unnecessary — the engine falls back to the STORE's embedder (see
    /// [`SyncEngine::embedder`]) — but kept for callers (`run`) that build the
    /// embedder eagerly and want it used even if the store's copy differs. Absence
    /// keeps sync fully functional (no vectors written). Chainable with [`new`].
    pub fn with_embedder(mut self, embedder: Arc<dyn crate::embed::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Resolve the EFFECTIVE embedder for this tick: the explicit override if set,
    /// else whatever is currently attached to the store. Reading the store each
    /// time means a background-attached embedder (`squelchd serve`) is picked up
    /// automatically once it becomes ready — before that, this returns `None` and
    /// ingest simply skips the vector write (the backfill pass fills it in later).
    fn embedder(&self) -> Option<Arc<dyn crate::embed::Embedder>> {
        self.embedder.clone().or_else(|| self.store.embedder())
    }

    /// Perform an authenticated GET returning parsed JSON. On a 401 we re-request
    /// the token once (the [`CredentialStore`] auto-refreshes) and retry exactly
    /// once. A 404 is surfaced as [`CoreError::NotFound`] so callers can branch
    /// (used for the expired-historyId fallback). The `Authorization` header and
    /// response body are NEVER logged.
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let resp = self.send_get(url).await?;
        match resp.status() {
            s if s.is_success() => resp
                .json::<T>()
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("gmail json decode: {e}"))),
            StatusCode::NOT_FOUND => Err(CoreError::NotFound),
            s => Err(CoreError::Other(anyhow::anyhow!(
                "gmail api status {}",
                s.as_u16()
            ))),
        }
    }

    /// Send a GET with a Bearer token, retrying once on 401 with a fresh token.
    async fn send_get(&self, url: &str) -> Result<reqwest::Response> {
        let token = self.creds.token(self.account_id).await?;
        let resp = self.bearer_get(url, &token.access_token).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            // Redacted: no token/header content, just the fact of a retry.
            eprintln!("squelch: gmail 401; refreshing token and retrying once");
            let token = self.creds.token(self.account_id).await?;
            return self.bearer_get(url, &token.access_token).await;
        }
        Ok(resp)
    }

    async fn bearer_get(&self, url: &str, access_token: &str) -> Result<reqwest::Response> {
        self.http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("gmail request: {e}")))
    }

    /// One full lifecycle: backfill if needed (establishing the historyId), then
    /// poll until an error bubbles up (caller retries with backoff) or shutdown.
    async fn run_once(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        eprintln!("squelch: gmail REST sync starting for <redacted account>");

        // First run (no history cursor) => full backfill + seed contacts.
        let cursor = self.load_history_cursor()?;
        if cursor.is_none() {
            self.backfill().await?;
            // Stage-2 pass over the backfill batch's ambiguous rows.
            self.stage2_pass().await;
        }

        // VECTOR BACKFILL: embed any NON-SEALED messages still missing a vector.
        // Covers pre-existing rows (schema/model added after a prior sync) and
        // ingest-time embed failures. Sealed rows are structurally excluded by
        // `messages_missing_vectors` (it selects sensitivity='normal' only), so
        // this can never embed sealed content. No-op when no embedder is attached.
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
        let n = self.fetch_raw_and_ingest(&inbox_ids, /* is_sent */ false).await?;
        eprintln!("squelch: backfilled {n} INBOX messages");

        // SENT bodies (format=raw). Fetching full bodies (not just headers) is
        // what makes v1 semantic recall cover WHAT THE USER WROTE ("did I say
        // I'd send X"). The is_sent ingest path is otherwise unchanged: contacts
        // are still derived from To/Cc recipients, the row is stored neutral
        // (tier=noise, importance=0) and stays out of triage/updates/search via
        // the existing is_sent exclusions. The extra win over the old
        // headers-only path is that the sent BODY lands in `messages.body`, so
        // it is embeddable for recall.
        let sent_ids = self.list_message_ids(LABEL_SENT, Some(&q)).await?;
        let seeded = self
            .fetch_raw_and_ingest(&sent_ids, /* is_sent */ true)
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
            self.poll_once().await?;
            // Stage-2 LLM triage pass after each ingest batch. Never crashes the
            // sync loop (all failures handled internally).
            self.stage2_pass().await;

            // VECTOR BACKFILL each tick. Cheap no-op when no embedder is attached
            // or the queue is empty. This is what makes a BACKGROUND-attached
            // embedder (`squelchd serve` attaches it after binding the port) catch
            // up WITHOUT a daemon restart: rows ingested while the embedder was not
            // yet ready are picked up on the next tick once it becomes available.
            self.backfill_missing_vectors().await;

            // Sleep the poll interval, waking early on shutdown.
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
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

    /// Walk `history.list` from `start_history_id`, ingesting newly added INBOX
    /// messages and advancing the persisted cursor. Propagates
    /// [`CoreError::NotFound`] on an expired historyId so the caller can fall
    /// back to a catch-up.
    async fn history_walk(&self, start_history_id: u64) -> Result<()> {
        let mut cursor = start_history_id;
        let mut page_token: Option<String> = None;
        let mut new_ids: Vec<String> = Vec::new();

        loop {
            let mut url = format!(
                "{GMAIL_API_BASE}/history?startHistoryId={start_history_id}\
                 &historyTypes=messageAdded&labelId={LABEL_INBOX}"
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

        if !new_ids.is_empty() {
            let n = self.fetch_raw_and_ingest(&new_ids, false).await?;
            eprintln!("squelch: ingested {n} new INBOX messages");
        }
        self.store_history_cursor(cursor)?;
        Ok(())
    }

    /// Fresh catch-up: re-run the backfill-window INBOX fetch (dedup makes it
    /// idempotent) and re-establish the historyId. Used on first run's poll and
    /// on an expired-history 404.
    async fn catch_up(&self) -> Result<()> {
        let q = format!("newer_than:{}d", self.config.sync.backfill_days);
        let ids = self.list_message_ids(LABEL_INBOX, Some(&q)).await?;
        let n = self.fetch_raw_and_ingest(&ids, false).await?;
        if n > 0 {
            eprintln!("squelch: catch-up ingested {n} INBOX messages");
        }
        let history_id = self.fetch_profile_history_id().await?;
        self.store_history_cursor(history_id)?;
        Ok(())
    }

    // ---- Gmail REST calls --------------------------------------------------

    /// List all message ids under `label`, optionally narrowed by a Gmail search
    /// `q`. Paginates fully.
    async fn list_message_ids(&self, label: &str, q: Option<&str>) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{GMAIL_API_BASE}/messages?labelIds={label}");
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
    /// the (unchanged) ingest pipeline. Sequential — rate limits are a non-issue
    /// at this volume. Returns the count ingested.
    async fn fetch_raw_and_ingest(&self, ids: &[String], is_sent: bool) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let rules = self.store.list_sender_rules(self.account_id)?;
        let now = Utc::now();
        let mut count = 0usize;

        for id in ids {
            let url = format!("{GMAIL_API_BASE}/messages/{id}?format=raw");
            let msg: RawMessage = self.get_json(&url).await?;
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
                gmail_msg_id: if msg.id.is_empty() { id.clone() } else { msg.id.clone() },
                gmail_thread_id: msg.thread_id.clone(),
                raw,
                internal_date: parse_internal_date(msg.internal_date.as_deref()),
                is_sent,
                account_addr: self.account_email.clone(),
            };
            if let Some((id, text)) = self.ingest_one(&fetched, &rules, now)? {
                self.embed_and_store(id, text).await;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Run one fetched message through the unchanged seal-first ingest pipeline
    /// and commit it atomically. Returns `Some((message_id, embed_text))` for a
    /// NORMAL-sensitivity message so the caller can embed it for semantic recall,
    /// or `None` for a SEALED message — the STRUCTURAL GATE that keeps sealed
    /// content out of the vector space (there is nothing to embed, not a filtered
    /// embedding). `embed_text` is the same subject+body flattening used at query
    /// time, truncated per config.
    fn ingest_one(
        &self,
        fetched: &RawFetched,
        rules: &[SenderRule],
        now: DateTime<Utc>,
    ) -> Result<Option<(i64, String)>> {
        let triaged = ingest_with_rules(
            fetched,
            &self.config.stage1,
            now,
            rules,
            |addr| self.store.is_known_contact(self.account_id, addr).unwrap_or(false),
        );
        let id = self.store.ingest_message(&triaged)?;
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

    /// Embed `text` off the async runtime (CPU work) and write the vector for
    /// `message_id`. No-op when no embedder is attached. A failure logs a
    /// REDACTED one-liner (id + error kind only, never body) and never
    /// propagates — the vector can be recovered by the backfill pass, so an embed
    /// failure must never block or crash ingest.
    async fn embed_and_store(&self, message_id: i64, text: String) {
        let Some(embedder) = self.embedder() else {
            return;
        };
        let account_id = self.account_id;
        let store = self.store.clone();
        // spawn_blocking: ONNX inference is CPU-bound; keep it off the poll loop.
        let result = tokio::task::spawn_blocking(move || {
            let vec = embedder.embed(&text)?;
            store.upsert_message_vector(account_id, message_id, &vec)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("squelch: embed failed for message {message_id} (recoverable via backfill): {e}"),
            Err(e) => eprintln!("squelch: embed task join error for message {message_id}: {e}"),
        }
    }

    /// STARTUP VECTOR BACKFILL: embed every NON-SEALED message that still lacks a
    /// vector, in throttled batches, so semantic recall covers pre-existing rows
    /// and any ingest-time embed failures.
    ///
    /// SECURITY: [`Store::messages_missing_vectors`] selects ONLY
    /// `sensitivity='normal'` rows, so sealed content is structurally absent from
    /// this pass — there is nothing sealed to embed. Each batch's embedding is CPU
    /// work run under `spawn_blocking` so the async runtime never stalls; a failure
    /// logs a redacted one-liner and moves on (the row stays missing and is retried
    /// next startup). No-op when no embedder is attached.
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
            // Flatten each message the SAME way ingest/query does, then embed the
            // whole batch in one blocking ONNX pass and write the vectors.
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

    /// Run one Stage-2 LLM triage pass over the queued (non-confident) rows.
    ///
    /// Fetch up to `batch_per_cycle` rows (`model_used IS NULL AND
    /// sensitivity='normal'`), process sequentially (no rate limiter needed at
    /// this volume). For each row: enforce the sealed guard, check the
    /// per-thread and global daily budgets, increment BEFORE the call (so retry
    /// storms can't exceed the cap), classify, and apply the result. Budget
    /// exhaustion skips the row (it stays queued) and logs at most one notice
    /// per cycle. Any per-row failure is logged (redacted) and never crashes the
    /// sync loop — the whole pass returns `Ok(())` regardless.
    ///
    /// No-op when Stage-2 is disabled (no API key).
    async fn stage2_pass(&self) {
        let Some((api_key, provider)) = self.stage2_key.as_ref() else {
            return; // disabled; notice already emitted at startup
        };
        let api_key = api_key.as_str();
        let provider = *provider;
        let cfg = &self.config.stage2;

        let queued = match self.store.stage2_queue(self.account_id, cfg.batch_per_cycle) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("squelch: stage-2 queue read failed ({e}); skipping pass");
                return;
            }
        };
        if queued.is_empty() {
            return;
        }

        // UTC date key for the budget rows; one value for the whole pass.
        let now = Utc::now();
        let day = now.format("%Y-%m-%d").to_string();
        // The staleness cutoff: rows received before this are stale-skipped.
        let stale_cutoff = now - ChronoDuration::days(cfg.max_age_days as i64);
        // "at most once per cycle" per-thread / per-sender exhaustion notices.
        // (The global notice fires at most once because hitting the cap breaks
        // the loop.)
        let mut warned_thread = false;
        let mut warned_sender = false;
        let mut processed = 0usize;
        let mut stale_skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        for row in &queued {
            // SEALED GUARD (defense in depth): the queue excludes sealed rows in
            // SQL, but re-check before every classify call.
            if let Err(e) = stage2_sealed_guard(row) {
                eprintln!("squelch: stage-2 sealed guard tripped ({e}); skipping row");
                continue;
            }

            // SKIP-STALE: a queued row older than max_age_days is marked processed
            // WITHOUT a model call (model_used='stale-skip'), keeping Stage-1
            // values. It neither consumes budget nor sits queued forever.
            if row.received_at < stale_cutoff {
                let _ = self.store.stage2_mark_processed(
                    self.account_id,
                    row.message_id,
                    STALE_SKIP_MODEL,
                );
                stale_skipped += 1;
                continue;
            }

            // GLOBAL budget check (per-account-per-day).
            match self
                .store
                .stage2_budget_used(self.account_id, GLOBAL_BUDGET_KEY, &day)
            {
                Ok(used) if used >= cfg.global_daily_cap => {
                    eprintln!(
                        "squelch: stage-2 global daily budget exhausted ({used}/{}); \
                         remaining rows stay queued",
                        cfg.global_daily_cap
                    );
                    break; // global cap blocks every remaining row this cycle
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 global budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // PER-THREAD budget check.
            match self
                .store
                .stage2_budget_used(self.account_id, &row.thread_id, &day)
            {
                Ok(used) if used >= cfg.thread_daily_cap => {
                    if !warned_thread {
                        eprintln!(
                            "squelch: stage-2 per-thread daily budget exhausted for at least \
                             one thread ({}/{}); those rows stay queued",
                            used, cfg.thread_daily_cap
                        );
                        warned_thread = true;
                    }
                    continue; // this thread is capped; try the next row
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 thread budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // PER-SENDER budget check (per-account-per-day, keyed by from_addr).
            // Stops one chatty sender fanning many DIFFERENT threads from burning
            // the budget. Same sentinel-row pattern as __global__.
            let sender_key = format!("{SENDER_BUDGET_PREFIX}{}", row.from_addr);
            match self
                .store
                .stage2_budget_used(self.account_id, &sender_key, &day)
            {
                Ok(used) if used >= cfg.sender_daily_cap => {
                    if !warned_sender {
                        eprintln!(
                            "squelch: stage-2 per-sender daily budget exhausted for at least \
                             one sender ({}/{}); those rows stay queued",
                            used, cfg.sender_daily_cap
                        );
                        warned_sender = true;
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
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, &sender_key, &day)
            {
                eprintln!("squelch: stage-2 sender budget increment failed ({e}); skipping row");
                continue;
            }

            // Classify.
            let ctx = RowContext::from_queued(row, cfg.max_body_chars);
            let outcome = stage2::classify(&self.http, api_key, cfg, provider, &ctx).await;

            match outcome {
                Ok(ClassifyOutcome::Ok(out, usage)) => {
                    if let Some(u) = usage {
                        in_tok += u.input_tokens;
                        out_tok += u.output_tokens;
                        // USAGE LEDGER: record this successful call's token usage
                        // per account per day. Best-effort — a ledger write
                        // failure must not affect triage.
                        if let Err(e) = self.store.stage2_bump_usage(
                            self.account_id,
                            &day,
                            u.input_tokens,
                            u.output_tokens,
                        ) {
                            eprintln!("squelch: stage-2 usage ledger bump failed ({e})");
                        }
                    }
                    let applied = stage2::apply_result(row, &out, &cfg.model, Utc::now());
                    if let Err(e) = self.store.stage2_apply(&applied) {
                        eprintln!("squelch: stage-2 apply failed ({e}); row stays queued");
                    } else {
                        processed += 1;
                    }
                }
                Ok(ClassifyOutcome::Refused) => {
                    // Keep Stage-1 values; mark processed so it doesn't loop.
                    // Redacted: no body/subject logged.
                    eprintln!("squelch: stage-2 refusal (redacted); keeping stage-1 values");
                    let _ = self.store.stage2_mark_processed(
                        self.account_id,
                        row.message_id,
                        &cfg.model,
                    );
                }
                Ok(ClassifyOutcome::Failed(kind)) => {
                    // Permanent failure (400/401/truncation/parse): mark the row
                    // failed (processed) so it does not loop forever. `kind` is
                    // already redacted (status/error-type only).
                    eprintln!("squelch: stage-2 permanent failure ({kind}); marking row failed");
                    let _ = self.store.stage2_mark_processed(
                        self.account_id,
                        row.message_id,
                        &cfg.model,
                    );
                }
                Err(e) => {
                    // Retryable class exhausted / transport error. Leave the row
                    // queued for a future cycle. `e` is redacted.
                    eprintln!("squelch: stage-2 {e}; row stays queued");
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

    /// `users.getProfile` -> the account's current historyId.
    async fn fetch_profile_history_id(&self) -> Result<u64> {
        let url = format!("{GMAIL_API_BASE}/profile");
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
        // Documentation anchor: non-confident rows are left with model_used NULL;
        // the Stage-2 queue predicate is `model_used IS NULL AND sensitivity='normal'`.
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
                Ok(()) => return Ok(()),
                Err(e) => {
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                    // Redacted: error strings from this crate never carry secrets.
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

/// Minimal percent-encoding for a Gmail `q` value (space -> `%20`, and the few
/// reserved characters a search query can contain). Enough for `newer_than:Nd`
/// and simple queries; we don't build arbitrary user queries here.
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

/// Gmail `internalDate` is milliseconds-since-epoch as a decimal string.
fn parse_internal_date(s: Option<&str>) -> Option<DateTime<Utc>> {
    let ms: i64 = s?.trim().parse().ok()?;
    DateTime::from_timestamp_millis(ms)
}

/// Rebuild a header-only RFC822 blob from Gmail metadata headers so the existing
/// mail-parser-based ingest path runs unchanged. A trailing blank line ends the
/// header section (empty body).
///
/// Test-only since SENT backfill switched to `format=raw` (full bodies for
/// recall); retained because the contacts-seeding tests exercise the header
/// parsing path through it.
#[cfg(test)]
fn synthesize_rfc822_headers(headers: &[MessageHeader]) -> String {
    let mut out = String::new();
    for h in headers {
        // Skip anything with embedded CR/LF defensively (header injection guard);
        // Gmail values are single-line but we never trust upstream blindly.
        if h.value.contains('\r') || h.value.contains('\n') {
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
    use crate::types::Tier;

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
        assert_eq!(history_poll_decision(None, false), HistoryDecision::FullCatchUp);
        assert_eq!(history_poll_decision(Some(0), false), HistoryDecision::FullCatchUp);
    }

    // ---- header synthesis for metadata-only sent seeding ------------------

    #[test]
    fn synthesize_headers_seeds_recipients_not_self() {
        // From is the account itself; contacts come from To/Cc recipients.
        let headers = vec![
            MessageHeader { name: "From".into(), value: "me@example.com".into() },
            MessageHeader { name: "To".into(), value: "alice@friends.com".into() },
            MessageHeader { name: "Cc".into(), value: "bob@friends.com".into() },
            MessageHeader { name: "Subject".into(), value: "re: lunch".into() },
            MessageHeader { name: "Date".into(), value: "Mon, 7 Jul 2026 10:00:00 +0000".into() },
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
        // Updated for bug #3: a CONFIDENT PastDue now requires a TRUSTED sender.
        // We first seed the biller as a known contact (via a prior sent-path
        // message), proving a legit past-due from a known biller still screams.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // Seed billing@utilityco.com as a known contact by having the user send
        // TO the biller (contacts are derived from Sent-mail recipients).
        let seed = "From: me@example.com\r\n\
                    To: Utility <billing@utilityco.com>\r\n\
                    Subject: account setup\r\n\
                    Date: Mon, 7 Jul 2026 09:00:00 +0000\r\n\
                    \r\n\
                    hello\r\n";
        let sf = fixture(acct, "g-seed", seed, /* is_sent */ true);
        ingest_into(&store, acct, &sf, Utc::now());
        assert!(store.is_known_contact(acct, "billing@utilityco.com").unwrap());

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
        // past-due bill surfaces. Assert it landed the top scream tier for a
        // KNOWN sender.
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
        assert!(updates.is_empty(), "sent mail must never surface in ranked_updates");

        // And it must not appear in search results either.
        let hits = store.search(acct, "lunch", 10, 0).unwrap();
        assert!(hits.is_empty(), "sent mail must not appear in search");
    }

    // ---- HTML body: ingest sanitize + human-door serving ------------------

    #[test]
    fn html_email_stores_sanitized_html_served_by_client_thread_view() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // An HTML email carrying dangerous markup (script, onerror, javascript:
        // href, form) plus benign table/img/style content.
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

        // The flattened text path is unaffected and still feeds triage/FTS.
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

        // A historyId larger than u32::MAX to prove the widened field holds it.
        let big = (u32::MAX as u64) + 123_456;
        store
            .set_sync_state(
                acct,
                HISTORY_KEY,
                &SyncState { uidvalidity: 0, last_uid: big },
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
}
