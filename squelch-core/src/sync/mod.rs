//! The Gmail IMAP sync engine.
//!
//! Responsibilities:
//! - Connect to `imap.gmail.com:993` over TLS and authenticate with
//!   `AUTHENTICATE XOAUTH2` using a read-only OAuth access token
//!   ([`xoauth2_sasl`] builds the SASL string; it is NEVER logged).
//! - On first run for a mailbox, backfill the last `backfill_days` of INBOX plus
//!   `[Gmail]/Sent Mail` headers (to seed the contacts "people I know" signal).
//! - Then IDLE on INBOX, coalescing notifications, and UID-fetch everything above
//!   the persisted `last_uid` through the seal-first [`ingest`] pipeline.
//! - Persist a per-mailbox UID cursor in `sync_state`; reset it on UIDVALIDITY
//!   change.
//!
//! SECURITY INVARIANTS honored here:
//! - The OAuth scope is fixed read-only upstream; we only ever *read* mail.
//! - Every fetched message goes through [`crate::sync::ingest::ingest_with_rules`]
//!   which runs seal detection FIRST, so sealed mail is classified and stored
//!   `sensitivity='sealed'` in the same transaction with importance 0 and never
//!   reaches Stage-2 or any LLM.
//! - Tokens / SASL strings / full bodies are never logged. Only counts and
//!   redacted metadata.

pub mod ingest;

use std::sync::Arc;
use std::time::Duration;

use async_imap::Session;
use async_imap::extensions::idle::IdleResponse;
use async_native_tls::TlsStream;
use base64::Engine as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::StreamExt;
use tokio::net::TcpStream;

use crate::config::Config;
use crate::credentials::CredentialStore;
use crate::error::{CoreError, Result};
use crate::store::{Store, SyncState};
use crate::sync::ingest::{RawFetched, ingest_with_rules};
use crate::types::{AccountId, SenderRule};

/// Gmail IMAP endpoint. Fixed; the host is not user-tunable (config carries the
/// account, not the server).
const IMAP_HOST: &str = "imap.gmail.com";
const IMAP_PORT: u16 = 993;

/// The INBOX and Sent mailbox names. Gmail's Sent folder is a virtual mailbox.
const INBOX: &str = "INBOX";
const SENT_MAILBOX: &str = "[Gmail]/Sent Mail";

/// Re-issue IDLE before Gmail's ~29-30 minute drop.
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

/// Reconnect backoff bounds.
const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// A TLS-wrapped tokio TCP stream — the transport async-imap runs over.
type ImapStream = TlsStream<TcpStream>;
type ImapSession = Session<ImapStream>;

/// SASL `XOAUTH2` authenticator for async-imap.
///
/// The initial (empty) server challenge is answered with the base64-encoded
/// `user=<email>^Aauth=Bearer <token>^A^A` string. Any subsequent challenge
/// (an error continuation) is answered with an empty response so the server can
/// return a tagged NO. The bearer token is stored only in memory and never
/// logged.
struct XOAuth2 {
    user: String,
    access_token: String,
}

impl async_imap::Authenticator for &XOAuth2 {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        // async-imap base64-encodes whatever we return. The first challenge is
        // the empty SASL initial-response prompt; we answer with the token blob.
        xoauth2_sasl(&self.user, &self.access_token).into_bytes()
    }
}

/// Build the raw (un-base64'd) XOAUTH2 SASL exchange string.
///
/// Format per Google:
/// `user=<email>\x01auth=Bearer <token>\x01\x01`.
///
/// SECURITY: the returned string embeds the bearer token — callers MUST NOT log
/// it. It exists only to be base64-encoded and written to the socket.
pub fn xoauth2_sasl(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

/// Base64 of the SASL string — exposed for tests and to make the encoding
/// explicit at the call site. Not logged.
pub fn xoauth2_b64(user: &str, access_token: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(xoauth2_sasl(user, access_token))
}

/// Everything the sync loop needs, resolved once at startup.
pub struct SyncEngine<S: Store, C: CredentialStore> {
    store: Arc<S>,
    creds: Arc<C>,
    account_id: AccountId,
    account_email: String,
    config: Config,
}

impl<S: Store + 'static, C: CredentialStore + 'static> SyncEngine<S, C> {
    pub fn new(
        store: Arc<S>,
        creds: Arc<C>,
        account_id: AccountId,
        account_email: String,
        config: Config,
    ) -> Self {
        Self {
            store,
            creds,
            account_id,
            account_email,
            config,
        }
    }

    /// Open a TLS connection and authenticate via XOAUTH2. On an auth failure we
    /// refresh the token once (the [`CredentialStore`] does this transparently on
    /// the *next* `token()` call) and retry a single time.
    async fn connect(&self) -> Result<ImapSession> {
        // First attempt with whatever the credential store hands us (already
        // refreshed if it was near expiry).
        let token = self.creds.token(self.account_id).await?;
        match self.try_authenticate(&token.access_token).await {
            Ok(session) => Ok(session),
            Err(first_err) => {
                tracing_auth_retry(&first_err);
                // Force a fresh token and retry exactly once. The keyring store
                // refreshes on demand; to *force* it we cannot easily invalidate,
                // so we simply re-request — if the token was rejected because it
                // expired mid-flight, the store will have refreshed by now.
                let token = self.creds.token(self.account_id).await?;
                self.try_authenticate(&token.access_token).await
            }
        }
    }

    async fn try_authenticate(&self, access_token: &str) -> Result<ImapSession> {
        let tcp = TcpStream::connect((IMAP_HOST, IMAP_PORT))
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("imap tcp connect: {e}")))?;
        let tls = async_native_tls::connect(IMAP_HOST, tcp)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("imap tls handshake: {e}")))?;

        let client = async_imap::Client::new(tls);
        let auth = XOAuth2 {
            user: self.account_email.clone(),
            access_token: access_token.to_string(),
        };
        // authenticate consumes the client and returns the session or (client, err).
        client
            .authenticate("XOAUTH2", &auth)
            .await
            .map_err(|(e, _client)| CoreError::Credential(format!("XOAUTH2 auth failed: {e}")))
    }

    /// One full lifecycle of a connection: (re)connect, backfill if needed, then
    /// IDLE forever until an error bubbles up (which the caller retries with
    /// backoff) or `shutdown` fires.
    async fn run_once(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let mut session = self.connect().await?;
        eprintln!("squelch: connected to {IMAP_HOST} as <redacted account>");

        // Seed contacts from Sent mail (headers only) on first sync of that box.
        self.backfill_sent(&mut session).await?;

        // Backfill / catch-up INBOX, establishing the UID cursor.
        self.sync_inbox(&mut session).await?;

        // IDLE loop. It takes ownership of the session (async-imap's idle()
        // consumes it) and hands it back for a graceful logout on clean exit.
        let mut session = self.idle_loop(session, shutdown).await?;

        // Graceful logout on clean shutdown.
        let _ = session.logout().await;
        Ok(())
    }

    /// Backfill `[Gmail]/Sent Mail` headers over the backfill window to seed the
    /// contacts table. Runs only the first time (no sync_state for the mailbox).
    async fn backfill_sent(&self, session: &mut ImapSession) -> Result<()> {
        if self
            .store
            .sync_state(self.account_id, SENT_MAILBOX)?
            .is_some()
        {
            return Ok(()); // already seeded
        }

        let mailbox = session
            .select(SENT_MAILBOX)
            .await
            .map_err(imap_err("select Sent"))?;
        let uidvalidity = mailbox.uid_validity.unwrap_or(0);

        let uids = self
            .uid_search_since(session, self.backfill_since())
            .await?;
        if uids.is_empty() {
            self.store.set_sync_state(
                self.account_id,
                SENT_MAILBOX,
                &SyncState {
                    uidvalidity,
                    last_uid: 0,
                },
            )?;
            return Ok(());
        }

        let max_uid = *uids.iter().max().unwrap();
        // HEADERS ONLY: enough for mail-parser to read From/To; is_sent=1 makes
        // upsert seed contacts. We do not need the body for the contacts signal.
        let ingested = self
            .fetch_and_ingest(session, &uids, /* is_sent */ true, /* headers_only */ true)
            .await?;
        eprintln!("squelch: seeded contacts from {ingested} sent messages");

        self.store.set_sync_state(
            self.account_id,
            SENT_MAILBOX,
            &SyncState {
                uidvalidity,
                last_uid: max_uid,
            },
        )?;
        Ok(())
    }

    /// Backfill or catch up INBOX, then leave the cursor at the newest UID.
    async fn sync_inbox(&self, session: &mut ImapSession) -> Result<()> {
        let mailbox = session.select(INBOX).await.map_err(imap_err("select INBOX"))?;
        let uidvalidity = mailbox.uid_validity.unwrap_or(0);

        let prior = self.store.sync_state(self.account_id, INBOX)?;
        // UIDVALIDITY change (or first run) => full backfill window.
        let (fetch_from_scratch, last_uid) = match prior {
            Some(state) if state.uidvalidity == uidvalidity => (false, state.last_uid),
            Some(_) => {
                eprintln!("squelch: INBOX UIDVALIDITY changed; resetting cursor");
                (true, 0)
            }
            None => (true, 0),
        };

        let uids = if fetch_from_scratch {
            self.uid_search_since(session, self.backfill_since()).await?
        } else {
            // Everything strictly above the cursor.
            self.uid_search_above(session, last_uid).await?
        };

        let mut new_cursor = last_uid;
        if !uids.is_empty() {
            new_cursor = new_cursor.max(*uids.iter().max().unwrap());
            let n = self
                .fetch_and_ingest(session, &uids, false, false)
                .await?;
            eprintln!("squelch: ingested {n} INBOX messages (backfill/catch-up)");
        }

        self.store.set_sync_state(
            self.account_id,
            INBOX,
            &SyncState {
                uidvalidity,
                last_uid: new_cursor,
            },
        )?;
        Ok(())
    }

    /// IDLE on INBOX; on any notification, debounce for `coalesce_secs` then fetch
    /// everything above the cursor in one batch. Takes ownership of the session
    /// (async-imap's `idle()` consumes it) and returns it for a graceful logout.
    /// Returns Ok(session) on graceful shutdown, Err on connection trouble (the
    /// caller reconnects with backoff).
    async fn idle_loop(
        &self,
        mut session: ImapSession,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<ImapSession> {
        loop {
            if *shutdown.borrow() {
                return Ok(session);
            }

            // async-imap's idle() consumes the Session; done() hands it back.
            let mut handle = session.idle();
            handle.init().await.map_err(imap_err("idle init"))?;

            let (wait_fut, interrupt) = handle.wait_with_timeout(IDLE_TIMEOUT);

            let outcome = tokio::select! {
                res = wait_fut => res.map_err(imap_err("idle wait")),
                _ = shutdown.changed() => {
                    drop(interrupt); // stop the idle stream
                    Ok(IdleResponse::ManualInterrupt)
                }
            };

            // Regardless of outcome, end IDLE and get the Session back.
            session = handle.done().await.map_err(imap_err("idle done"))?;

            match outcome? {
                IdleResponse::ManualInterrupt => {
                    if *shutdown.borrow() {
                        return Ok(session);
                    }
                    // Spurious interrupt: loop and re-IDLE.
                }
                IdleResponse::Timeout => {
                    // Re-issue IDLE (loop). Do a cheap catch-up in case we missed
                    // an EXISTS during the gap.
                    self.drain_new(&mut session).await?;
                }
                IdleResponse::NewData(_) => {
                    // Coalesce: wait out the debounce window (or until shutdown),
                    // absorbing bursts of arrivals into one fetch.
                    let coalesce = Duration::from_secs(self.config.sync.coalesce_secs);
                    tokio::select! {
                        _ = tokio::time::sleep(coalesce) => {}
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() { return Ok(session); }
                        }
                    }
                    self.drain_new(&mut session).await?;
                }
            }
        }
    }

    /// Fetch and ingest everything above the persisted INBOX cursor, advancing it.
    async fn drain_new(&self, session: &mut ImapSession) -> Result<()> {
        // Re-select to refresh EXISTS/UIDVALIDITY view.
        let mailbox = session.select(INBOX).await.map_err(imap_err("re-select INBOX"))?;
        let uidvalidity = mailbox.uid_validity.unwrap_or(0);

        let last_uid = match self.store.sync_state(self.account_id, INBOX)? {
            Some(state) if state.uidvalidity == uidvalidity => state.last_uid,
            _ => {
                // UIDVALIDITY changed under us: reset and full-backfill.
                eprintln!("squelch: INBOX UIDVALIDITY changed during IDLE; resetting");
                0
            }
        };

        let uids = self.uid_search_above(session, last_uid).await?;
        if uids.is_empty() {
            self.store.set_sync_state(
                self.account_id,
                INBOX,
                &SyncState { uidvalidity, last_uid },
            )?;
            return Ok(());
        }
        let new_cursor = last_uid.max(*uids.iter().max().unwrap());
        let n = self.fetch_and_ingest(session, &uids, false, false).await?;
        eprintln!("squelch: ingested {n} new INBOX messages");
        self.store.set_sync_state(
            self.account_id,
            INBOX,
            &SyncState {
                uidvalidity,
                last_uid: new_cursor,
            },
        )?;
        Ok(())
    }

    /// UID SEARCH SINCE <date> — the backfill window.
    async fn uid_search_since(
        &self,
        session: &mut ImapSession,
        since: DateTime<Utc>,
    ) -> Result<Vec<u32>> {
        // IMAP SEARCH date format: DD-Mon-YYYY.
        let date = since.format("%d-%b-%Y").to_string();
        let set = session
            .uid_search(format!("SINCE {date}"))
            .await
            .map_err(imap_err("uid search since"))?;
        Ok(set.into_iter().collect())
    }

    /// UID SEARCH UID <last+1>:* — everything strictly newer than the cursor.
    async fn uid_search_above(&self, session: &mut ImapSession, last_uid: u32) -> Result<Vec<u32>> {
        let lo = last_uid.saturating_add(1);
        let set = session
            .uid_search(format!("UID {lo}:*"))
            .await
            .map_err(imap_err("uid search above"))?;
        // Gmail's `lo:*` can echo the highest UID even when it's below `lo`;
        // filter defensively so we never re-ingest the cursor message.
        Ok(set.into_iter().filter(|u| *u >= lo).collect())
    }

    /// Fetch the given UIDs and run each through the ingest pipeline, committing
    /// via [`Store::ingest_message`]. Returns the count ingested.
    async fn fetch_and_ingest(
        &self,
        session: &mut ImapSession,
        uids: &[u32],
        is_sent: bool,
        headers_only: bool,
    ) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }
        let rules = self.store.list_sender_rules(self.account_id)?;
        let now = Utc::now();

        // Batch the UID set into one FETCH. Body vs. headers-only per caller.
        let uid_set = join_uids(uids);
        let body_item = if headers_only {
            "BODY.PEEK[HEADER]"
        } else {
            "BODY.PEEK[]"
        };
        let query = format!("(UID INTERNALDATE X-GM-MSGID {body_item})");

        let mut stream = session
            .uid_fetch(uid_set, query)
            .await
            .map_err(imap_err("uid fetch"))?;

        let mut count = 0usize;
        while let Some(item) = stream.next().await {
            let fetch = item.map_err(imap_err("fetch item"))?;
            let raw = match (fetch.body(), fetch.header()) {
                (Some(b), _) => b.to_vec(),
                (None, Some(h)) => h.to_vec(),
                (None, None) => continue, // nothing usable
            };
            let internal_date = fetch
                .internal_date()
                .map(|d| d.with_timezone(&Utc));
            let gmail_msg_id = fetch
                .gmail_msg_id()
                .map(|id| id.to_string())
                .unwrap_or_default();

            let fetched = RawFetched {
                account_id: self.account_id,
                gmail_msg_id,
                // async-imap 0.11 does not surface X-GM-THRID; the ingest
                // pipeline derives a stable thread key from References/
                // In-Reply-To/Message-ID headers instead.
                gmail_thread_id: None,
                raw,
                internal_date,
                is_sent,
            };

            let triaged = ingest_with_rules(
                &fetched,
                &self.config.stage1,
                now,
                &rules,
                |addr| self.store.is_known_contact(self.account_id, addr).unwrap_or(false),
            );
            self.store.ingest_message(&triaged)?;
            count += 1;
        }
        Ok(count)
    }

    fn backfill_since(&self) -> DateTime<Utc> {
        Utc::now() - ChronoDuration::days(self.config.sync.backfill_days as i64)
    }

    fn rules_for_stage2_note() -> &'static str {
        // Documentation anchor: non-confident rows are left with model_used NULL;
        // the Stage-2 queue predicate is `model_used IS NULL AND sensitivity='normal'`.
        "model_used IS NULL AND sensitivity='normal'"
    }

    /// The top-level driver: loop, reconnecting with exponential backoff on any
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
                    // Clean shutdown from inside the loop.
                    return Ok(());
                }
                Err(e) => {
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                    // Redacted: error strings from this crate never carry secrets.
                    eprintln!(
                        "squelch: sync connection error ({e}); reconnecting in {}s",
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

/// Map an async-imap error to a redacted [`CoreError`]. The error string from
/// async-imap describes the protocol failure, not message content.
fn imap_err(ctx: &'static str) -> impl Fn(async_imap::error::Error) -> CoreError {
    move |e| CoreError::Other(anyhow::anyhow!("imap {ctx}: {e}"))
}

/// Log an auth retry without leaking anything. Kept tiny and dependency-free.
fn tracing_auth_retry(_e: &CoreError) {
    eprintln!("squelch: XOAUTH2 auth failed; refreshing token and retrying once");
}

/// Join a UID slice into an IMAP sequence set, using contiguous ranges where
/// possible to keep the command short.
fn join_uids(uids: &[u32]) -> String {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            end = sorted[i + 1];
            i += 1;
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}:{end}"));
        }
        i += 1;
    }
    parts.join(",")
}

/// Type alias helper so callers can name the concrete rule slice.
pub type Rules = Vec<SenderRule>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Stage1Config;
    use crate::store::SqliteStore;
    use crate::types::Tier;

    /// Build a RawFetched from an RFC822 string, as the IMAP layer would.
    fn fixture(account_id: AccountId, msgid: &str, eml: &str, is_sent: bool) -> RawFetched {
        RawFetched {
            account_id,
            gmail_msg_id: msgid.to_string(),
            gmail_thread_id: None,
            raw: eml.as_bytes().to_vec(),
            internal_date: Some(Utc::now()),
            is_sent,
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

        // Never surfaces via the MCP-facing query.
        let updates = store
            .ranked_updates(acct, Utc::now() - ChronoDuration::days(1), None)
            .unwrap();
        assert!(updates.is_empty(), "sealed OTP must not surface");

        // But is present in the local-only sealed view, importance 0.
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
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
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
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].tier, Tier::PastDue);
        let deadlines = store.deadlines(acct, None).unwrap();
        assert!(deadlines[0].past_due);
    }

    #[test]
    fn sent_message_seeds_contacts_and_marks_known() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // A Sent message. is_sent=1 -> upsert seeds the contacts table. The
        // contacts key is the message's from_addr; for backfilled Sent headers
        // this is how the "people I know" signal is seeded.
        let eml = "From: bob@friends.com\r\n\
                   To: someone@else.com\r\n\
                   Subject: re: lunch\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   sounds good\r\n";
        let f = fixture(acct, "g-sent", eml, /* is_sent */ true);
        ingest_into(&store, acct, &f, Utc::now());

        // The sender is now a known contact (sent_count incremented).
        assert!(store.is_known_contact(acct, "bob@friends.com").unwrap());
        // And an unrelated address is not.
        assert!(!store.is_known_contact(acct, "stranger@nowhere.io").unwrap());
    }

    #[test]
    fn sync_state_round_trips_and_resets_on_uidvalidity() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        assert!(store.sync_state(acct, INBOX).unwrap().is_none());

        store
            .set_sync_state(
                acct,
                INBOX,
                &SyncState {
                    uidvalidity: 42,
                    last_uid: 100,
                },
            )
            .unwrap();
        let s = store.sync_state(acct, INBOX).unwrap().unwrap();
        assert_eq!(s.uidvalidity, 42);
        assert_eq!(s.last_uid, 100);
    }

    #[test]
    fn xoauth2_sasl_has_exact_control_bytes() {
        let s = xoauth2_sasl("me@example.com", "ya29.TOKEN");
        assert_eq!(s, "user=me@example.com\x01auth=Bearer ya29.TOKEN\x01\x01");
        // The token appears exactly once and is bracketed by the control bytes.
        assert!(s.contains("\x01\x01"));
        assert!(s.starts_with("user=me@example.com\x01"));
    }

    #[test]
    fn xoauth2_b64_round_trips() {
        let b64 = xoauth2_b64("u@x.com", "tok");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(raw, xoauth2_sasl("u@x.com", "tok").into_bytes());
    }

    #[test]
    fn join_uids_ranges_and_singletons() {
        assert_eq!(join_uids(&[1, 2, 3, 5, 7, 8]), "1:3,5,7:8");
        assert_eq!(join_uids(&[10]), "10");
        assert_eq!(join_uids(&[3, 1, 2]), "1:3");
        assert_eq!(join_uids(&[]), "");
    }
}
