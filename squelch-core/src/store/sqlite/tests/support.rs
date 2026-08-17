//! Fixtures shared by the store test modules.

use super::super::*;
use crate::embed::{Embedder, message_embed_text};
use crate::triage::{CalendarInfo, CalendarKind, DeadlineHit, ReceiptInfo};
use crate::types::{FieldReasons, SealedKind, Sensitivity, Tier};
use chrono::TimeZone;
use std::sync::Arc;

/// A fresh in-memory store plus the standard test account.
pub(super) fn store() -> (SqliteStore, AccountId) {
    let store = SqliteStore::open_in_memory().unwrap();
    let acct = store.ensure_account("me@example.com").unwrap();
    (store, acct)
}

/// Same, with a semantic embedder attached at open (the vector-search tests).
pub(super) fn store_with_embedder(embedder: Arc<dyn Embedder>) -> (SqliteStore, AccountId) {
    let store = SqliteStore::open_in_memory()
        .unwrap()
        .with_embedder(embedder)
        .unwrap();
    let acct = store.ensure_account("me@example.com").unwrap();
    (store, acct)
}

// ---- the message/triage fixture builder --------------------------------

/// One chainable shape for every store fixture, with a terminal per way a test
/// lands a row: [`upsert`](TriagedBuilder::upsert) writes the message alone,
/// [`seed`](TriagedBuilder::seed) adds a hand-written triage row, and
/// [`ingest`](TriagedBuilder::ingest) runs the real ingest path.
#[derive(Clone)]
pub(super) struct TriagedBuilder {
    msg: NewMessage,
    sensitivity: Sensitivity,
    sealed_kind: Option<SealedKind>,
    importance: u8,
    tier: Tier,
    one_line: String,
    reason: String,
    field_reasons: FieldReasons,
    matched_rule: Option<i64>,
    deadline: Option<DeadlineHit>,
    receipt: Option<ReceiptInfo>,
    calendar: Option<CalendarInfo>,
    /// The loose shipping signal ingest computes — set directly here so the
    /// trigger/queue tests do not have to write shipping prose that happens to
    /// satisfy the detector.
    ship_extract: bool,
    confident: bool,
    /// Applied through the Stage-1 categorizer by `ingest`, after the row lands.
    category: Option<String>,
}

/// A plain inbound message with a neutral triage shape — the base every other
/// fixture chains off.
pub(super) fn triaged(account_id: AccountId, gmail: &str, thread: &str) -> TriagedBuilder {
    TriagedBuilder {
        msg: NewMessage {
            account_id,
            gmail_msg_id: gmail.to_string(),
            thread_id: thread.to_string(),
            from_addr: "alice@example.com".to_string(),
            from_name: Some("Alice".to_string()),
            subject: "Lunch?".to_string(),
            received_at: Utc::now(),
            snippet: "want to grab lunch".to_string(),
            body: "Hey, want to grab lunch tomorrow?".to_string(),
            body_html: None,
            is_sent: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        },
        sensitivity: Sensitivity::Normal,
        sealed_kind: None,
        importance: 0,
        tier: Tier::Noise,
        one_line: String::new(),
        reason: String::new(),
        field_reasons: FieldReasons::default(),
        matched_rule: None,
        deadline: None,
        receipt: None,
        calendar: None,
        ship_extract: false,
        confident: true,
        category: None,
    }
}

// Setters are named for the columns they set (`from`, `is_sent`, `to_addrs`),
// which collides with the from_*/is_*/to_* self conventions.
#[allow(clippy::wrong_self_convention)]
impl TriagedBuilder {
    // -- message fields --
    pub(super) fn from(mut self, addr: &str) -> Self {
        self.msg.from_addr = addr.to_string();
        self
    }
    pub(super) fn from_name(mut self, name: Option<&str>) -> Self {
        self.msg.from_name = name.map(Into::into);
        self
    }
    pub(super) fn subject(mut self, subject: &str) -> Self {
        self.msg.subject = subject.to_string();
        self
    }
    pub(super) fn body(mut self, body: &str) -> Self {
        self.msg.body = body.to_string();
        self
    }
    #[allow(dead_code)] // builder completeness; no current test sets a snippet
    pub(super) fn snippet(mut self, snippet: &str) -> Self {
        self.msg.snippet = snippet.to_string();
        self
    }
    pub(super) fn received_at(mut self, at: DateTime<Utc>) -> Self {
        self.msg.received_at = at;
        self
    }
    pub(super) fn is_sent(mut self, is_sent: bool) -> Self {
        self.msg.is_sent = is_sent;
        self
    }
    /// Sent mail's display recipients, as the ingest path renders them.
    pub(super) fn to_addrs(mut self, to: &str) -> Self {
        self.msg.to_addrs = Some(to.to_string());
        self
    }
    pub(super) fn list_unsubscribe(mut self, header: &str, one_click: bool) -> Self {
        self.msg.list_unsubscribe = Some(header.to_string());
        self.msg.list_unsub_one_click = one_click;
        self
    }
    pub(super) fn auth_pass(mut self, auth_pass: Option<bool>) -> Self {
        self.msg.auth_pass = auth_pass;
        self
    }

    // -- triage fields --
    pub(super) fn importance(mut self, importance: u8) -> Self {
        self.importance = importance;
        self
    }
    pub(super) fn tier(mut self, tier: Tier) -> Self {
        self.tier = tier;
        self
    }
    pub(super) fn one_line(mut self, one_line: &str) -> Self {
        self.one_line = one_line.to_string();
        self
    }
    pub(super) fn reason(mut self, reason: &str) -> Self {
        self.reason = reason.to_string();
        self
    }
    pub(super) fn field_reasons(mut self, field_reasons: FieldReasons) -> Self {
        self.field_reasons = field_reasons;
        self
    }
    /// Sensitivity WITHOUT a sealed_kind — the pre-kind row shape.
    pub(super) fn sensitivity(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }
    /// Sealed, carrying its kind (what a real seal writes).
    pub(super) fn sealed(mut self, kind: SealedKind) -> Self {
        self.sensitivity = Sensitivity::Sealed;
        self.sealed_kind = Some(kind);
        self
    }
    pub(super) fn matched_rule(mut self, rule_id: Option<i64>) -> Self {
        self.matched_rule = rule_id;
        self
    }
    pub(super) fn confident(mut self, confident: bool) -> Self {
        self.confident = confident;
        self
    }
    pub(super) fn deadline(mut self, deadline: DeadlineHit) -> Self {
        self.deadline = Some(deadline);
        self
    }
    pub(super) fn receipt(mut self, receipt: ReceiptInfo) -> Self {
        self.receipt = Some(receipt);
        self
    }
    pub(super) fn calendar(mut self, calendar: CalendarInfo) -> Self {
        self.calendar = Some(calendar);
        self
    }
    /// Carry the loose shipping signal, i.e. queue this row for the shipments
    /// extractor (`ship_extract_model='pending'`).
    pub(super) fn ship_extract(mut self, ship_extract: bool) -> Self {
        self.ship_extract = ship_extract;
        self
    }
    pub(super) fn category(mut self, category: &str) -> Self {
        self.category = Some(category.to_string());
        self
    }

    // -- terminals --
    pub(super) fn msg(&self) -> NewMessage {
        self.msg.clone()
    }

    pub(super) fn build(&self) -> TriagedMessage {
        TriagedMessage {
            message: self.msg(),
            recipients: vec![],
            sensitivity: self.sensitivity,
            sealed_kind: self.sealed_kind,
            importance: self.importance,
            tier: self.tier,
            one_line: self.one_line.clone(),
            reason: self.reason.clone(),
            matched_rule: self.matched_rule,
            field_reasons: self.field_reasons.clone(),
            deadline: self.deadline.clone(),
            shipment: None,
            ship_extract: self.ship_extract,
            receipt: self.receipt.clone(),
            calendar: self.calendar.clone(),
            attachments: vec![],
            confident: self.confident,
        }
    }

    /// Write the message row only (no triage row).
    pub(super) fn upsert(&self, store: &SqliteStore) -> i64 {
        store.upsert_message(&self.msg()).unwrap()
    }

    /// Write the message plus a hand-written triage row (`set_triage`).
    pub(super) fn seed(&self, store: &SqliteStore) -> i64 {
        let id = self.upsert(store);
        store
            .set_triage(
                id,
                self.msg.account_id,
                self.importance,
                self.tier,
                self.sensitivity,
                self.sealed_kind,
                &self.one_line,
                &self.reason,
                self.deadline.as_ref().map(|d| d.due_at),
            )
            .unwrap();
        id
    }

    /// Run the real ingest path, then apply `.category()` — if one was set —
    /// through the Stage-1 categorizer.
    pub(super) fn ingest(&self, store: &SqliteStore) -> i64 {
        let id = store.ingest_message(&self.build()).unwrap();
        if let Some(category) = &self.category {
            apply_category(store, self.msg.account_id, id, category, false);
        }
        id
    }
}

// ---- thin wrappers over the builder ------------------------------------

/// A store-ready TriagedMessage carrying a receipt (noise tier), for the
/// auto-resolve tests.
pub(super) fn receipt_triaged(
    acct: AccountId,
    gmail: &str,
    thread: &str,
    amount: Option<f64>,
) -> TriagedBuilder {
    triaged(acct, gmail, thread)
        .from("no-reply@baywheels.com")
        .from_name(Some("Bay Wheels"))
        .subject("Your Bay Wheels ride receipt")
        .importance(15)
        .one_line("Your Bay Wheels ride receipt")
        .reason("receipt")
        .receipt(ReceiptInfo {
            amount,
            currency: amount.map(|_| "USD".into()),
        })
}

/// A plain, normal, non-receipt inbound (or sent) TriagedMessage with an
/// explicit sender + received_at, for the unsubscribe violation tests.
pub(super) fn inbound_triaged(
    acct: AccountId,
    gmail: &str,
    thread: &str,
    from: &str,
    received_at: DateTime<Utc>,
    is_sent: bool,
) -> TriagedBuilder {
    triaged(acct, gmail, thread)
        .from(from)
        .from_name(None)
        .received_at(received_at)
        .is_sent(is_sent)
        .importance(10)
}

/// Set a triage row's LLM category via the real Stage-1 apply path (the
/// categorizer). `needs_stage2` is irrelevant to extraction but honored.
pub(super) fn apply_category(
    store: &SqliteStore,
    acct: AccountId,
    message_id: i64,
    category: &str,
    needs_stage2: bool,
) {
    store
        .stage1_apply(&Stage1Applied {
            message_id,
            account_id: acct,
            importance: 40,
            tier: Tier::Noise,
            one_line: "categorized".into(),
            reason: "stage-1".into(),
            field_reasons: crate::types::FieldReasons::default(),
            stage1_model_used: "claude-opus-5".into(),
            needs_stage2,
            escalation_reason: needs_stage2.then_some("boundary"),
            deadline: None,
            category: Some(category.into()),
        })
        .unwrap();
}

pub(super) fn triage_extract_status(
    store: &SqliteStore,
    message_id: i64,
) -> (String, Option<String>, Option<String>) {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT status, resolved_at, extractor_model_used FROM triage WHERE message_id=?1",
        params![message_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

// ---- calendar updates --------------------------------------------------

/// Build a store-ready TriagedMessage carrying a calendar update (noise
/// tier), for the auto-resolve/listing tests.
pub(super) fn calendar_triaged(
    acct: AccountId,
    gmail: &str,
    kind: CalendarKind,
    received_at: DateTime<Utc>,
) -> TriagedBuilder {
    triaged(acct, gmail, &format!("t-{gmail}"))
        .from("sam@gmail.com")
        .from_name(Some("Sam Doe"))
        .subject("Invitation: Design review @ Wed Jul 22, 2026 10am")
        .received_at(received_at)
        .importance(15)
        .one_line("Invitation: Design review")
        .reason("calendar")
        .calendar(CalendarInfo {
            kind,
            event_title: Some("Design review".into()),
            starts_at: Some(Utc.with_ymd_and_hms(2026, 7, 22, 10, 0, 0).unwrap()),
            organizer: Some("Sam Doe".into()),
        })
}

// ---- receipt -> open-bill auto-close ----------------------------------

/// Build a store-ready TriagedMessage carrying a BILL (deadline tier) from
/// the given sender, for the receipt->bill auto-close tests.
pub(super) fn bill_triaged(
    acct: AccountId,
    gmail: &str,
    from_addr: &str,
    from_name: Option<&str>,
    amount: Option<f64>,
    received_at: DateTime<Utc>,
    due_at: DateTime<Utc>,
) -> TriagedBuilder {
    triaged(acct, gmail, &format!("t-{gmail}"))
        .from(from_addr)
        .from_name(from_name)
        .subject("Your statement is ready")
        .received_at(received_at)
        .importance(200)
        .tier(Tier::Deadline)
        .one_line("Payment due")
        .reason("bill")
        .deadline(DeadlineHit {
            kind: "payment_due".into(),
            amount,
            currency: amount.map(|_| "USD".into()),
            due_at,
            past_due: false,
            source: "test".into(),
        })
}

/// Build a store-ready receipt TriagedMessage from the given sender.
pub(super) fn receipt_from(
    acct: AccountId,
    gmail: &str,
    from_addr: &str,
    from_name: Option<&str>,
    amount: Option<f64>,
    received_at: DateTime<Utc>,
) -> TriagedBuilder {
    receipt_triaged(acct, gmail, &format!("t-{gmail}"), amount)
        .from(from_addr)
        .from_name(from_name)
        .received_at(received_at)
}

/// Read (status, resolved_at) straight off a triage row.
pub(super) fn triage_status(
    store: &SqliteStore,
    acct: AccountId,
    id: i64,
) -> (String, Option<String>) {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT status, resolved_at FROM triage WHERE account_id=?1 AND message_id=?2",
        params![acct, id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

pub(super) fn auto_close_audits(store: &SqliteStore, acct: AccountId) -> Vec<AuditEntry> {
    store
        .list_audit(acct, 50)
        .unwrap()
        .into_iter()
        .filter(|e| e.action == "bill.auto_close")
        .collect()
}

// --- sitrep seen-ledger --------------------------------------------------

/// Helper: a non-sealed triaged message with a chosen tier/importance.
pub(super) fn ingest_normal(
    store: &SqliteStore,
    acct: AccountId,
    gmail: &str,
    thread: &str,
    tier: Tier,
    importance: u8,
    received: DateTime<Utc>,
) -> i64 {
    triaged(acct, gmail, thread)
        .received_at(received)
        .importance(importance)
        .tier(tier)
        .seed(store)
}

// ---- Stage-1 LLM queue / markers -------------------------------------

/// Build a store-ready TriagedMessage with controllable rule/confidence so
/// the queue-marker logic in `ingest_message` is testable directly.
pub(super) fn triaged_row(
    acct: AccountId,
    gmail: &str,
    thread: &str,
    matched_rule: Option<i64>,
    confident: bool,
    sensitivity: Sensitivity,
) -> TriagedBuilder {
    let b = triaged(acct, gmail, thread)
        .importance(40)
        .one_line("seed")
        .reason("seed")
        .matched_rule(matched_rule)
        .confident(confident);
    if sensitivity == Sensitivity::Sealed {
        b.sealed(SealedKind::Otp)
    } else {
        b.sensitivity(sensitivity)
    }
}

// ---- Stage-2 store methods -------------------------------------------

/// Insert a message + a triage row with model_used NULL (queued) or set
/// (processed), controlling sensitivity so the sealed-exclusion is testable.
pub(super) fn seed_triage_row(
    store: &SqliteStore,
    acct: AccountId,
    gmail_id: &str,
    thread: &str,
    sensitivity: Sensitivity,
) -> i64 {
    triaged(acct, gmail_id, thread)
        .sensitivity(sensitivity)
        .importance(40)
        .one_line("ambiguous")
        .reason("no rule matched")
        .seed(store)
}

// ---- NOTIFICATION EVENTS ---------------------------------------------

/// A worthy `NewEvent` for `message_id`, distinct per id so ordering and
/// snapshotting are visible in assertions.
pub(super) fn new_event(acct: AccountId, message_id: i64) -> NewEvent {
    NewEvent {
        account_id: acct,
        message_id,
        thread_id: format!("t{message_id}"),
        kind: EventKind::Surfaced,
        tier: Tier::Signal,
        importance: 70,
        sender: "alice@example.com".to_string(),
        one_line: format!("line {message_id}"),
        deadline: None,
    }
}

/// Embed a message's subject+body with `embedder` and write its vector, exactly
/// as the sync ingest/backfill path does. CALLER ensures the row is non-sealed
/// (mirrors the structural gate: sealed mail never reaches this).
pub(super) fn embed_and_store(
    store: &SqliteStore,
    embedder: &dyn Embedder,
    acct: AccountId,
    message_id: i64,
    subject: &str,
    body: &str,
) {
    let text = message_embed_text(subject, body, 2000);
    let v = embedder.embed(&text).unwrap();
    store.upsert_message_vector(acct, message_id, &v).unwrap();
}

/// Count vectors present for a given message id (0 or 1). Used to assert a
/// sealed message is structurally absent from the vector space.
pub(super) fn vec_count_for(store: &SqliteStore, message_id: i64) -> i64 {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM message_vecs WHERE message_id = ?1",
        params![message_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// The listing policy for shipment tests that are NOT about the read-side
/// filters: an unreachable failure cap and a disabled staleness window, so
/// nothing is ever hidden and the filters cannot perturb a test asking about
/// something else. The suppression, staleness and clear tests build their own.
pub(super) const KEEP_ALL_SHIPMENTS: crate::config::ShipmentListPolicy =
    crate::config::ShipmentListPolicy {
        suppress_failed_ambiguous_at: u32::MAX,
        stale_after_days: 0,
    };

/// [`KEEP_ALL_SHIPMENTS`] with a specific ambiguous-suppression cap, for the
/// tests that ARE about phantom suppression.
pub(super) fn suppress_at(cap: u32) -> crate::config::ShipmentListPolicy {
    crate::config::ShipmentListPolicy {
        suppress_failed_ambiguous_at: cap,
        ..KEEP_ALL_SHIPMENTS
    }
}

/// [`KEEP_ALL_SHIPMENTS`] with a staleness window, for the tests that ARE about
/// the staleness filter.
pub(super) fn stale_after(days: u32) -> crate::config::ShipmentListPolicy {
    crate::config::ShipmentListPolicy {
        stale_after_days: days,
        ..KEEP_ALL_SHIPMENTS
    }
}
