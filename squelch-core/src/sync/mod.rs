//! Gmail sync: REST + polling. REST, not IMAP — the read-only `gmail.readonly`
//! scope works over REST and IMAP XOAUTH2 rejects it. First run backfills
//! `backfill_days` of INBOX+SENT and records the `historyId`; after that
//! `history.list` polls `messageAdded`, a 404 falling back to a full catch-up.
//! Tokens, headers and bodies are NEVER logged; ingest seals first (SECURITY.md).

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
use crate::triage::events;
use crate::triage::extract::{self, banking, marketing};
use crate::triage::stage1_llm::{self, HEURISTIC_ONLY};
use crate::triage::stage2::{self, ClassifyOutcome, RowContext};
use crate::triage::{stage1_sealed_guard, stage2_sealed_guard};
use crate::types::{AccountId, SenderRule, Sensitivity};

/// Gmail REST base for the authenticated user. Fixed; not user-tunable.
const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// The INBOX and SENT label ids (Gmail system labels).
const LABEL_INBOX: &str = "INBOX";
const LABEL_SENT: &str = "SENT";

/// The single `sync_state` row key for the REST engine's historyId cursor.
const HISTORY_KEY: &str = "history";

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
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '.' })
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

/// A single Gmail metadata header. Test-only: the contacts-seeding tests build
/// these to exercise header parsing via [`synthesize_rfc822_headers`].
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

/// Which daily cap a budget-exhausted notice is about; each is rate-limited to
/// once per UTC day (see [`SyncEngine::warn_days`]).
#[derive(Debug, Clone, Copy)]
enum CapKind {
    Thread,
    Sender,
    Global,
    Stage1Global,
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
    /// Stage-2 API key + provider, resolved once at startup. `None` disables
    /// Stage-2 gracefully: rows stay queued, one stderr notice, sync continues.
    /// The key is never logged.
    stage2_key: Option<(String, Stage2Provider)>,
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
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client build");
        // Absence => graceful disable, one notice, no key material logged.
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
            refresh: Arc::new(tokio::sync::Notify::new()),
            warn_days: std::sync::Mutex::new(WarnDays::default()),
        }
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
            // Redacted: the fact of a retry, never token/header content.
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
            self.poll_once().await?;
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
            let n = self
                .fetch_raw_and_ingest(&new_ids, false, IngestOrigin::Incremental)
                .await?;
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
        // A catch-up may carry genuinely new mail, so it is allowed to notify.
        // What keeps the whole-window re-scan from storming is the freshness
        // window in `triage::events` plus the one-event-per-message key.
        let n = self
            .fetch_raw_and_ingest(&ids, false, IngestOrigin::Incremental)
            .await?;
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
        let triaged = ingest_with_rules(
            fetched,
            &self.config.stage1,
            now,
            rules,
            |addr| self.store.is_known_contact(self.account_id, addr).unwrap_or(false),
        );
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
            Ok(Err(e)) => eprintln!("squelch: embed failed for message {message_id} (recoverable via backfill): {e}"),
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

    /// Run one Stage-1 LLM refine pass over rows still carrying their ingest
    /// heuristic seed (`stage1_model_used IS NULL`): sealed guard, GLOBAL
    /// Stage-1 budget with increment-before-call so retries can't exceed it,
    /// classify, apply — which stamps `stage1_model_used` and sets
    /// `needs_stage2`. On refusal or permanent error the row keeps its seed
    /// values stamped `heuristic-only` and the seed's own `needs_stage2` decides
    /// escalation. Budget exhaustion defers rows without loss; no failure
    /// crashes the sync loop. No-op when the LLM is disabled (no API key).
    async fn stage1_pass(&self) {
        let Some((api_key, provider)) = self.stage2_key.as_ref() else {
            return; // disabled; notice already emitted at startup
        };
        let api_key = api_key.as_str();
        let provider = *provider;
        let cfg = &self.config.stage1;

        // Re-read the cap at the START of the pass so a client change via
        // POST /client/triage-config applies within a cycle, no restart.
        // Precedence: override > config/env > default.
        let caps = self
            .store
            .stage2_cap_overrides(self.account_id)
            .unwrap_or_default();
        let global_daily_cap = caps
            .stage1_global_daily_cap
            .unwrap_or(cfg.global_daily_cap);

        let queued = match self.store.stage1_queue(self.account_id, cfg.batch_per_cycle) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("squelch: stage-1 queue read failed ({e}); skipping pass");
                return;
            }
        };
        if queued.is_empty() {
            return;
        }

        let now = Utc::now();
        let day = now.format("%Y-%m-%d").to_string();
        // Deliberately the Stage-2 max-age, so both stages age out together.
        let stale_cutoff = now - ChronoDuration::days(self.config.stage2.max_age_days as i64);
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
                continue;
            }

            // GLOBAL budget check (Stage-1's ONLY scope). Once hit, every
            // remaining row this cycle stays queued, unstamped.
            match self
                .store
                .stage2_budget_used(self.account_id, STAGE1_GLOBAL_BUDGET_KEY, &day)
            {
                Ok(used) if used >= global_daily_cap => {
                    if self.warn_once_per_day(CapKind::Stage1Global, &day) {
                        eprintln!(
                            "squelch: stage-1 global daily budget exhausted \
                             ({used}/{global_daily_cap}); remaining rows stay queued"
                        );
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-1 global budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // Increment BEFORE the call so the attempt counts even on error/retry.
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, STAGE1_GLOBAL_BUDGET_KEY, &day)
            {
                eprintln!("squelch: stage-1 global budget increment failed ({e}); skipping row");
                continue;
            }

            let outcome = stage1_llm::classify(&self.http, api_key, cfg, provider, row).await;
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
                        ) {
                            eprintln!("squelch: stage-1 usage ledger bump failed ({e})");
                        }
                    }
                    let applied = stage1_llm::apply_result(row, &out, &cfg.model, Utc::now());
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
        let Some((api_key, provider)) = self.stage2_key.as_ref() else {
            return; // disabled; notice already emitted at startup
        };
        let api_key = api_key.as_str();
        let provider = *provider;
        // Extractors run on the STAGE-1 (small) model and share its config + cap.
        let cfg = &self.config.stage1;

        let categories = extract::extractable_categories();
        if categories.is_empty() {
            return;
        }

        // Extract calls count against the SAME daily counter as Stage-1, runtime
        // override included.
        let caps = self
            .store
            .stage2_cap_overrides(self.account_id)
            .unwrap_or_default();
        let global_daily_cap = caps.stage1_global_daily_cap.unwrap_or(cfg.global_daily_cap);

        let queued = match self
            .store
            .extract_queue(self.account_id, &categories, cfg.batch_per_cycle)
        {
            Ok(q) => q,
            Err(e) => {
                eprintln!("squelch: extract queue read failed ({e}); skipping pass");
                return;
            }
        };
        if queued.is_empty() {
            return;
        }

        let now = Utc::now();
        let day = now.format("%Y-%m-%d").to_string();
        let stale_cutoff = now - ChronoDuration::days(self.config.stage2.max_age_days as i64);
        let mut extracted = 0usize;
        let mut skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        for row in &queued {
            // SEALED GUARD: the queue already excludes sealed rows in SQL (they
            // carry a NULL category); re-check anyway (docs/SECURITY.md).
            if let Err(e) = extract::extract_sealed_guard(row) {
                eprintln!("squelch: extract sealed guard tripped ({e}); skipping row");
                continue;
            }

            // SKIP-STALE: mark extracted WITHOUT a model call, so an old row
            // neither spends budget nor sits queued forever.
            if row.received_at < stale_cutoff {
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
            if !banking::CATEGORIES.contains(&row.category.as_str()) {
                let _ = self.store.extract_mark_processed(
                    self.account_id,
                    row.message_id,
                    "skip-no-extractor",
                );
                skipped += 1;
                continue;
            }

            // SHARED Stage-1 global budget. Once hit, every remaining row this
            // cycle stays queued, unstamped.
            match self
                .store
                .stage2_budget_used(self.account_id, STAGE1_GLOBAL_BUDGET_KEY, &day)
            {
                Ok(used) if used >= global_daily_cap => {
                    if self.warn_once_per_day(CapKind::Stage1Global, &day) {
                        eprintln!(
                            "squelch: stage-1 global daily budget exhausted \
                             ({used}/{global_daily_cap}); extract rows stay queued"
                        );
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: extract budget read failed ({e}); skipping row");
                    continue;
                }
            }
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, STAGE1_GLOBAL_BUDGET_KEY, &day)
            {
                eprintln!("squelch: extract budget increment failed ({e}); skipping row");
                continue;
            }

            // ROUTE BY CATEGORY: each specialist owns its own prompt, schema and
            // ledger line, so the row's category decides which one runs.
            if marketing::CATEGORIES.contains(&row.category.as_str()) {
                match marketing::classify(&self.http, api_key, cfg, provider, row).await {
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
                continue;
            }

            let outcome = banking::classify(&self.http, api_key, cfg, provider, row).await;
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
                        eprintln!("squelch: banking apply failed ({e}); row marked apply-failed");
                        let _ = self.store.extract_mark_processed(
                            self.account_id,
                            row.message_id,
                            "apply-failed",
                        );
                    } else {
                        extracted += 1;
                    }
                }
                Ok(banking::ExtractOutcome::Refused) | Ok(banking::ExtractOutcome::Failed(_)) => {
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
        let Some((api_key, provider)) = self.stage2_key.as_ref() else {
            return; // disabled; notice already emitted at startup
        };
        let api_key = api_key.as_str();
        let provider = *provider;
        let cfg = &self.config.stage2;

        // Re-read the three caps at the START of the pass so a client change via
        // POST /client/triage-config applies within a cycle, no restart.
        // Precedence: override > config/env > default.
        let caps = self
            .store
            .stage2_cap_overrides(self.account_id)
            .unwrap_or_default();
        let thread_daily_cap = caps.thread_daily_cap.unwrap_or(cfg.thread_daily_cap);
        let sender_daily_cap = caps.sender_daily_cap.unwrap_or(cfg.sender_daily_cap);
        let global_daily_cap = caps.global_daily_cap.unwrap_or(cfg.global_daily_cap);

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
        let stale_cutoff = now - ChronoDuration::days(cfg.max_age_days as i64);
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
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, &sender_key, &day)
            {
                eprintln!("squelch: stage-2 sender budget increment failed ({e}); skipping row");
                continue;
            }

            let ctx = RowContext::from_queued(row, cfg.max_body_chars);
            let outcome = stage2::classify(&self.http, api_key, cfg, provider, &ctx).await;

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
                        ) {
                            eprintln!("squelch: stage-2 usage ledger bump failed ({e})");
                        }
                    }
                    let applied = stage2::apply_result(row, &out, &cfg.model, Utc::now());
                    match self.store.stage2_apply(&applied) {
                        Err(e) => {
                            eprintln!("squelch: stage-2 apply failed ({e}); row stays queued");
                        }
                        // TOCTOU: sealed by hand mid-pass, so no verdict landed
                        // and there is nothing to notify for.
                        Ok(false) => {}
                        Ok(true) => {
                            processed += 1;
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
                Ok(()) => return Ok(()),
                Err(e) => {
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

/// Gmail `internalDate` is milliseconds-since-epoch as a decimal string.
fn parse_internal_date(s: Option<&str>) -> Option<DateTime<Utc>> {
    let ms: i64 = s?.trim().parse().ok()?;
    DateTime::from_timestamp_millis(ms)
}

/// Rebuild a header-only RFC822 blob from Gmail metadata headers so the
/// mail-parser ingest path runs over it unchanged; the trailing blank line ends
/// the header section (empty body). Test-only, for the contacts-seeding tests.
#[cfg(test)]
fn synthesize_rfc822_headers(headers: &[MessageHeader]) -> String {
    let mut out = String::new();
    for h in headers {
        // HEADER INJECTION GUARD: Gmail values are single-line, but upstream is
        // never trusted blindly.
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
    use crate::types::{Disposition, Tier, TriageAxis};

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
        assert_eq!(ev, None, "old mail is silent no matter what the verdict says");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());

        // Sanity: the guard stopped it, not a mis-triage.
        let updates = store.ranked_updates(acct, old - ChronoDuration::days(1), None).unwrap();
        let bill = updates.iter().find(|u| u.id == mid).expect("bill surfaced in the client");
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
        assert_eq!(store.sealed_messages(acct).unwrap().len(), 1, "it WAS sealed");
    }

    #[test]
    fn squelched_sender_and_noise_are_both_silent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        // A SQUELCH rule outranks the heuristic that would otherwise have
        // surfaced this sender (proved above).
        store
            .set_sender_rule(acct, "*@monitoring.example", "not urgent", Disposition::Squelch)
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
        assert!(row.received_at > now + ChronoDuration::days(1000), "the Date: header won");

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
            .set_sender_rule(acct, "*@monitoring.example", "not urgent", Disposition::Squelch)
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
        assert!(!a.contains("attacker") && !a.contains("evil"), "address must not leak: {a}");
        let hex = &a["sender#".len()..];
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "hex only: {hex}");
        // Deterministic (correlatable across a day) and injective per sender.
        assert_eq!(a, redact_sender("attacker@evil.example"));
        assert_ne!(a, redact_sender("someone@else.example"));
    }

    #[test]
    fn sanitize_ascii_strips_control_and_caps_length() {
        // Newlines (log-forging), ANSI escapes, and RTL-override become '.'.
        let clean = sanitize_ascii("abc\n\x1b[31mDEF\u{202e}", 64);
        assert!(!clean.contains('\n') && !clean.contains('\u{1b}') && !clean.contains('\u{202e}'));
        assert!(clean.starts_with("abc."), "printable kept, control replaced: {clean}");
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
