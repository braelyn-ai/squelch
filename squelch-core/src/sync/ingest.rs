//! The ingest pipeline: raw RFC822 bytes -> parsed -> flattened text ->
//! seal-first triage -> a [`TriagedMessage`] ready for an atomic store write.
//!
//! ORDERING IS A SECURITY INVARIANT: parse/flatten, then seal detection FIRST
//! (sealed mail is `sensitivity='sealed'`, importance 0, and never runs Stage-1 or
//! reaches any LLM), and only then, for non-sealed mail, contacts/rules -> Stage-1.
//! Network-free by design so it is testable against fixture bytes.

use crate::config::Stage1Config;
use crate::store::{SqliteStore, Store, TriagedMessage};
use crate::sync::html::sanitize_email_html;
use crate::triage::calendar;
use crate::triage::receipt;
use crate::triage::seal::{self, SealInput};
use crate::triage::shipment;
use crate::triage::stage1_with_config;
use crate::types::{AccountId, AttachmentInfo, FieldReasons, NewMessage, Sensitivity, Tier};
use chrono::{DateTime, Utc};
use mail_parser::{Address, MessageParser, MimeHeaders};

/// The raw identity/metadata the transport supplies alongside the RFC822 body.
/// When the native Gmail ids are absent the pipeline falls back to a header-derived
/// thread key (see [`fallback_thread_id`]).
#[derive(Debug, Clone)]
pub struct RawFetched {
    pub account_id: AccountId,
    /// Stable per-account message id (Gmail `message.id`, or a Message-ID hash
    /// fallback when absent).
    pub gmail_msg_id: String,
    /// Gmail `message.threadId` when available; otherwise a header-derived key.
    pub gmail_thread_id: Option<String>,
    /// Full RFC822 bytes (from `format=raw`), base64url-decoded.
    pub raw: Vec<u8>,
    /// Date fallback if the message lacks a parseable Date header.
    pub internal_date: Option<DateTime<Utc>>,
    /// Whether this came from the Sent mailbox (seeds the contacts table).
    pub is_sent: bool,
    /// The account's own email, compared lower-cased so the user's own address can
    /// NEVER become a contact (contacts come from Sent mail's To/Cc, not From). May
    /// be empty when unknown — then only From is excluded, which on Sent mail is
    /// the account anyway.
    pub account_addr: String,
}

/// Crudely flatten HTML to text: drop tags, decode common entities, collapse
/// whitespace. The HTML-only fallback, so raw markup never reaches triage.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();
    // Inside a <style>/<script> block: that TEXT is code, not prose, and must not
    // land in the body fed to the triage models.
    let mut skip_block: Option<&'static str> = None;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                in_tag = false;
                let name = tag_buf.trim().to_ascii_lowercase();
                let first = name.split_whitespace().next().unwrap_or("");
                match skip_block {
                    None => {
                        if first == "style" {
                            skip_block = Some("style");
                        } else if first == "script" {
                            skip_block = Some("script");
                        }
                    }
                    Some(k) => {
                        if first == format!("/{k}") {
                            skip_block = None;
                        }
                    }
                }
                out.push(' ');
            }
            _ if in_tag => tag_buf.push(c),
            _ if skip_block.is_some() => {}
            _ => out.push(c),
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_addr(addr: &Address) -> (String, Option<String>) {
    match addr.first() {
        Some(a) => (
            a.address().unwrap_or_default().to_string(),
            a.name().map(|n| n.to_string()),
        ),
        None => (String::new(), None),
    }
}

/// Collect every non-empty email address from an [`Address`] header (flat or
/// grouped lists). Used to derive contacts from Sent mail's To/Cc recipients.
fn collect_addrs(addr: &Address, out: &mut Vec<String>) {
    for a in addr.iter() {
        if let Some(email) = a.address()
            && !email.is_empty()
        {
            out.push(email.to_string());
        }
    }
}

/// Heuristic: does `addr` look like a machine/robot address rather than a person?
/// Used ONLY to filter recipient contact seeding, so mailto-unsubscribe traffic
/// never becomes a "person I know" and pollutes triage. Combines local-part
/// prefixes, domain first-label hints, and token-like locals (hex/UUID blobs).
/// Ordinary addresses — including a dotted local like `rentbikes.net` — must pass.
pub fn is_robot_address(addr: &str) -> bool {
    let addr = addr.trim().to_ascii_lowercase();
    let (local, domain) = match addr.split_once('@') {
        Some((l, d)) if !l.is_empty() && !d.is_empty() => (l, d),
        // No parseable local@domain — not our concern here; let it through.
        _ => return false,
    };

    // Domain first-label hints (e.g. leave.mcmap.chase.com, unsub.beehiiv.com).
    let first_label = domain.split('.').next().unwrap_or("");
    const DOMAIN_ROBOT_LABELS: &[&str] =
        &["unsub", "unsubscribe", "leave", "bounce", "optout", "opt-out"];
    if DOMAIN_ROBOT_LABELS.contains(&first_label) {
        return true;
    }

    // Segment on the plus-address boundary so a prefix on ANY +-segment
    // (e.g. "unsubscribe-mc.us22_...") is caught.
    const LOCAL_ROBOT_PREFIXES: &[&str] = &[
        "unsubscribe",
        "unsub",
        "leave-",
        "optout",
        "opt-out",
        "bounce",
        "noreply",
        "no-reply",
        "donotreply",
        "do-not-reply",
        "list-",
    ];
    let plus_segments: Vec<&str> = local.split('+').collect();
    for seg in &plus_segments {
        for p in LOCAL_ROBOT_PREFIXES {
            if seg.starts_with(p) {
                return true;
            }
        }
        // "*.optout" style suffix on a segment (e.g. dxirq3pb.560xwm.9t9eb.optout).
        if seg.ends_with(".optout") || seg.ends_with("-optout") {
            return true;
        }
    }

    // Token-like locals: opaque machine blobs no human would choose. First,
    // multiple +-separated UUID-ish segments (beehiiv style).
    let uuidish_segments = plus_segments.iter().filter(|s| looks_uuidish(s)).count();
    if uuidish_segments >= 1 && plus_segments.len() >= 2 {
        return true;
    }
    for seg in &plus_segments {
        if looks_uuidish(seg) || is_hex_blob(seg) {
            return true;
        }
    }

    // Break the local on the usual separators and inspect each run, so a real
    // dotted name (rentbikes.net) never trips it — its runs are short and
    // vowel-rich.
    for run in local.split(['.', '-', '_', '=']) {
        if is_token_blob(run) {
            return true;
        }
    }

    false
}

/// A long random-looking machine token: >=25 alphanumeric chars, at least one
/// digit, vowel ratio below 20%. Human words carry far more vowels even when long.
fn is_token_blob(s: &str) -> bool {
    if s.len() < 25 || !s.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    let has_digit = s.bytes().any(|b| b.is_ascii_digit());
    if !has_digit {
        return false;
    }
    let vowels = s.bytes().filter(|b| b"aeiou".contains(b)).count();
    vowels * 5 < s.len()
}

/// A UUID shape: 8-4-4-4-12 hex groups separated by hyphens.
fn looks_uuidish(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    groups
        .iter()
        .zip(lens.iter())
        .all(|(g, &n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A long opaque hex/token blob: >=20 chars drawn only from `[0-9a-f-]`. Real
/// names contain letters outside a-f and stay under the threshold, so they pass.
fn is_hex_blob(s: &str) -> bool {
    if s.len() < 20 {
        return false;
    }
    s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Capture unsubscribe intent: the raw trimmed `List-Unsubscribe` value, plus
/// whether RFC 8058 one-click is advertised (`List-Unsubscribe-Post` containing
/// `List-Unsubscribe=One-Click`). One-click is forced `false` without a
/// `List-Unsubscribe` header. Only raw material — parsing is the endpoint's job.
pub fn extract_unsub_headers(msg: &mail_parser::Message) -> (Option<String>, bool) {
    let list_unsub = msg
        .header_raw("List-Unsubscribe")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let one_click = list_unsub.is_some()
        && msg
            .header_raw("List-Unsubscribe-Post")
            .map(|v| v.to_ascii_lowercase().contains("list-unsubscribe=one-click"))
            .unwrap_or(false);
    (list_unsub, one_click)
}

/// Derive a stable thread key from headers when the Gmail thread id is absent:
/// the root References id, else In-Reply-To, else this message's own Message-ID.
pub fn fallback_thread_id(msg: &mail_parser::Message) -> Option<String> {
    // The first References id is the thread root.
    if let Some(first) = msg.references().as_text_list().and_then(|l| l.iter().next()) {
        return Some(first.to_string());
    }
    if let Some(irt) = msg.in_reply_to().as_text_list().and_then(|l| l.iter().next()) {
        return Some(irt.to_string());
    }
    msg.message_id().map(|s| s.to_string())
}

/// A tiny FNV-1a hash, hex-encoded: the last-resort stable id when even a
/// Message-ID is missing, so two such messages don't collide on "".
pub fn stable_hash(input: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in input.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// INGEST CAPS. Bytes are stored only up to these limits; a part over the
/// per-attachment cap, or one that would push the message's stored total over the
/// per-message cap, keeps its metadata but drops its bytes (`data: None`).
const ATTACHMENT_CAP_BYTES: usize = 10 * 1024 * 1024;
const MESSAGE_ATTACHMENT_TOTAL_CAP_BYTES: usize = 25 * 1024 * 1024;

/// Extract every attachment part from a parsed message — REAL attachments
/// (`Content-Disposition: attachment`) AND cid-inline parts
/// (`Content-Disposition: inline` with a `Content-ID`, how templates embed
/// logos). mail-parser's `attachments()` iterator already yields exactly this set
/// (everything that is not a displayed text/html body part).
///
/// Caps are applied here (see the consts above): the metadata of an over-cap
/// part is always kept (`size_bytes` is its real decoded size), but its bytes are
/// dropped (`data: None`) so the store lands a NULL blob. Filename falls back to
/// `attachment-<n>` (1-based part order) when the part declares none; the mime is
/// the part's declared type, falling back to `application/octet-stream`.
///
/// This runs for BOTH sealed and non-sealed mail — storage is fine either way;
/// the byte-serving endpoint is what guards sealed parents.
pub fn extract_attachments(m: &mail_parser::Message) -> Vec<AttachmentInfo> {
    let mut out = Vec::new();
    let mut stored_total: usize = 0;
    for (i, part) in m.attachments().enumerate() {
        // Multipart containers carry no bytes of their own; mail-parser should
        // not list them as attachments, but skip defensively.
        if part.is_multipart() {
            continue;
        }
        let bytes = part.contents();
        let size = bytes.len();
        let filename = part
            .attachment_name()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("attachment-{}", i + 1));
        let mime = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        // Keep bytes only when this part is within the per-attachment cap AND
        // keeping it would not push the message's stored total over its cap.
        let within_per = size <= ATTACHMENT_CAP_BYTES;
        let within_total =
            stored_total.saturating_add(size) <= MESSAGE_ATTACHMENT_TOTAL_CAP_BYTES;
        let data = if within_per && within_total {
            stored_total += size;
            Some(bytes.to_vec())
        } else {
            None
        };
        out.push(AttachmentInfo {
            filename,
            mime,
            size_bytes: size as i64,
            data,
        });
    }
    out
}

/// Turn raw fetched bytes into a fully-triaged, store-ready message.
///
/// `known_contact_lookup` is called ONLY for non-sealed mail, with the parsed
/// from-address, so the caller can consult the contacts table. Sealed mail
/// short-circuits before this is ever invoked.
pub fn ingest(
    fetched: &RawFetched,
    cfg: &Stage1Config,
    now: DateTime<Utc>,
    mut known_contact_lookup: impl FnMut(&str) -> bool,
) -> TriagedMessage {
    let parsed = MessageParser::default().parse(&fetched.raw);

    // Recipient addresses (To + Cc) — only meaningful for Sent mail, where they
    // become contacts. Collected here while the parse is in hand.
    let mut recipients: Vec<String> = Vec::new();
    if fetched.is_sent && let Some(m) = &parsed {
        if let Some(to) = m.to() {
            collect_addrs(to, &mut recipients);
        }
        if let Some(cc) = m.cc() {
            collect_addrs(cc, &mut recipients);
        }
    }

    // Extract fields with graceful fallbacks for malformed mail.
    #[allow(clippy::type_complexity)]
    let (
        from_addr,
        from_name,
        subject,
        received_at,
        thread_id,
        msg_id_hdr,
        text,
        body_html,
        list_unsubscribe,
        list_unsub_one_click,
    ) = match &parsed {
            Some(m) => {
                let (fa, fname) = m.from().map(first_addr).unwrap_or_default();
                let subject = m.subject().unwrap_or("").to_string();
                let received = m
                    .date()
                    .and_then(|d| DateTime::parse_from_rfc3339(&d.to_rfc3339()).ok())
                    .map(|d| d.with_timezone(&Utc))
                    .or(fetched.internal_date)
                    .unwrap_or(now);
                // Prefer a plain-text body; fall back to HTML flattened to text.
                // This flattened `text` is the UNCHANGED path that feeds triage,
                // FTS, and the agent door — it must not be affected by the HTML
                // work below.
                let text = if m.text_body_count() > 0 {
                    m.body_text(0).map(|c| c.into_owned()).unwrap_or_default()
                } else if m.html_body_count() > 0 {
                    html_to_text(&m.body_html(0).map(|c| c.into_owned()).unwrap_or_default())
                } else {
                    String::new()
                };
                // Separately: capture the RENDERED HTML body (when present) and
                // sanitize it server-side for the human door. `None` for
                // plain-text-only mail (leaves body_html NULL). This never feeds
                // triage/FTS/MCP — only GET /client/thread/{id}.
                //
                // IMPORTANT: mail-parser's `body_html(0)` SYNTHESIZES HTML from a
                // text/plain part when no real HTML alternative exists (it wraps
                // the text in <p> tags), and `html_body_count()` counts that
                // synthetic entry. We must NOT store that — a genuinely
                // plain-text-only email has to leave body_html NULL. So we check
                // the actual part type (`is_text_html`) and only capture a REAL
                // `text/html` MIME part.
                let body_html = m
                    .html_part(0)
                    .filter(|p| p.is_text_html())
                    .and_then(|_| m.body_html(0))
                    .map(|c| sanitize_email_html(&c))
                    .filter(|s| !s.trim().is_empty());
                let thr = fetched
                    .gmail_thread_id
                    .clone()
                    .or_else(|| fallback_thread_id(m));
                let (list_unsub, one_click) = extract_unsub_headers(m);
                (
                    fa,
                    fname,
                    subject,
                    received,
                    thr,
                    m.message_id().map(|s| s.to_string()),
                    text,
                    body_html,
                    list_unsub,
                    one_click,
                )
            }
            None => (
                String::new(),
                None,
                String::new(),
                fetched.internal_date.unwrap_or(now),
                fetched.gmail_thread_id.clone(),
                None,
                String::new(),
                None,
                None,
                false,
            ),
        };

    // A gmail_msg_id is required to key the row. Fall back to Message-ID, then a
    // hash of the raw bytes so nothing collides on an empty string.
    let gmail_msg_id = if !fetched.gmail_msg_id.is_empty() {
        fetched.gmail_msg_id.clone()
    } else if let Some(mid) = &msg_id_hdr {
        stable_hash(mid)
    } else {
        stable_hash(&String::from_utf8_lossy(&fetched.raw))
    };

    let thread_id = thread_id.unwrap_or_else(|| gmail_msg_id.clone());

    // Attachments (real + cid-inline), capped. Extracted once here and moved into
    // whichever TriagedMessage return path fires. Present for sealed mail too —
    // storage is fine; the byte-serving endpoint guards sealed parents.
    let attachments = parsed
        .as_ref()
        .map(extract_attachments)
        .unwrap_or_default();

    // A compact snippet for list views; body text drives triage.
    let snippet: String = text.chars().take(200).collect();

    // Finalize the contact recipients (Sent mail only): drop the account's OWN
    // address and the From address (on Sent mail From == the account), case-fold
    // and dedup. This is the explicit guard that the user's own address can
    // never become a contact.
    let self_addr = fetched.account_addr.trim().to_ascii_lowercase();
    let from_lc = from_addr.trim().to_ascii_lowercase();
    let mut seen: Vec<String> = Vec::new();
    recipients.retain(|r| {
        let lc = r.trim().to_ascii_lowercase();
        if lc.is_empty() || lc == self_addr || lc == from_lc || seen.contains(&lc) {
            return false;
        }
        // Drop machine/robot recipients (Gmail mailto-unsubscribe traffic goes to
        // real addresses that must never become "people I know" contacts).
        if is_robot_address(&lc) {
            return false;
        }
        seen.push(lc);
        true
    });

    let message = NewMessage {
        account_id: fetched.account_id,
        gmail_msg_id,
        thread_id,
        from_addr: from_addr.clone(),
        from_name,
        subject: subject.clone(),
        received_at,
        snippet,
        body: text.clone(),
        body_html,
        is_sent: fetched.is_sent,
        list_unsubscribe,
        list_unsub_one_click,
    };

    // ---- SEAL DETECTION FIRST (security invariant) ----------------------
    let seal_kind = seal::detect_sealed(&SealInput {
        from_addr: &from_addr,
        subject: &subject,
        body: &text,
    });
    if let Some(kind) = seal_kind {
        // Sealed: importance 0, no Stage-1, no deadline, never confident enough
        // to matter — it will never be surfaced or sent to an LLM.
        return TriagedMessage {
            message,
            recipients,
            sensitivity: Sensitivity::Sealed,
            sealed_kind: Some(kind),
            importance: 0,
            tier: Tier::Noise,
            one_line: String::new(),
            reason: format!("sealed at ingest ({})", kind.as_str()),
            field_reasons: FieldReasons::default(),
            matched_rule: None,
            deadline: None,
            shipment: None,
            receipt: None,
            calendar: None,
            attachments,
            confident: true,
        };
    }

    // ---- Sent mail: seed contacts, but DO NOT run Stage-1 triage ----------
    // The user's own outbox must never pollute the ranked inbox. We write a
    // neutral tier=noise/importance=0 row (belt: ranked_updates/search also
    // exclude is_sent=1) and skip the LLM path entirely. Recipients still seed
    // the contacts table via `ingest_message`.
    if fetched.is_sent {
        return TriagedMessage {
            message,
            recipients,
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
            importance: 0,
            tier: Tier::Noise,
            one_line: String::new(),
            reason: "sent mail (contacts seeded; not triaged)".to_string(),
            field_reasons: FieldReasons::default(),
            matched_rule: None,
            deadline: None,
            shipment: None,
            receipt: None,
            calendar: None,
            attachments,
            confident: true,
        };
    }

    // ---- Non-sealed: derive known-contact, load rules already provided --
    let is_known = known_contact_lookup(&from_addr);
    // Sender rules are matched inside stage1; the caller supplies them via cfg's
    // sibling argument. We accept them through the wrapper below.
    let result = stage1_with_config(&message, is_known, &[], cfg, now);

    // SHIPMENT DETECTION runs INDEPENDENTLY of the triage tier: a "your order
    // shipped" email is noise-tier for the ranked inbox but still feeds the
    // package tracker. Only ever runs here, on the NON-SEALED path — a sealed OTP
    // short-circuited above and never reaches this line.
    let shipment = shipment::detect_shipment(&from_addr, &subject, &text);

    // RECEIPT DETECTION runs INDEPENDENTLY of the triage tier AND of shipment
    // detection: a receipt (record of money already paid) is noise-tier for the
    // ranked inbox but feeds the Receipts category, and an order-confirmation with
    // a total AND tracking is BOTH a receipt and a shipment. Only ever runs here,
    // on the NON-SEALED path — a sealed OTP short-circuited above and never reaches
    // this line, so a receipt can never carry sealed data. When a receipt is
    // present, the store's ingest write force-resolves this message's triage row
    // (status='done') so it never surfaces as inbox clutter.
    let receipt = receipt::detect_receipt(&from_addr, &subject, &text);

    // CALENDAR DETECTION runs INDEPENDENTLY of the triage tier, exactly like
    // receipts: an invite/cancellation/RSVP is a record of scheduling state
    // that feeds the Calendar category (and is auto-resolved to 'done' by the
    // store's ingest write). Only ever runs here, on the NON-SEALED path — a
    // sealed OTP short-circuited above and never reaches this line, so a
    // calendar update can never carry sealed data.
    let calendar = calendar::detect_calendar(
        &from_addr,
        message.from_name.as_deref(),
        &subject,
        &text,
        received_at,
    );

    TriagedMessage {
        message,
        recipients,
        sensitivity: Sensitivity::Normal,
        sealed_kind: None,
        importance: result.importance,
        tier: result.tier,
        one_line: result.one_line,
        reason: result.reason,
        field_reasons: result.field_reasons,
        matched_rule: result.matched_rule,
        deadline: result.deadline,
        shipment,
        receipt,
        calendar,
        attachments,
        confident: result.confident,
    }
}

/// Full ingest with sender rules. Kept separate so the common test path
/// ([`ingest`]) needs no rules argument, while the sync engine passes the
/// account's rule list.
pub fn ingest_with_rules(
    fetched: &RawFetched,
    cfg: &Stage1Config,
    now: DateTime<Utc>,
    rules: &[crate::types::SenderRule],
    known_contact_lookup: impl FnMut(&str) -> bool,
) -> TriagedMessage {
    // Reuse the seal-first path from `ingest`, then, if it came back normal,
    // re-run Stage-1 WITH rules. This keeps the seal invariant in exactly one
    // place while still honoring user rules.
    let mut triaged = ingest(fetched, cfg, now, known_contact_lookup);
    // Sealed and Sent mail never run Stage-1 (Sent is neutral tier-noise), so
    // they must not run the rules re-pass either.
    if triaged.sensitivity == Sensitivity::Sealed || fetched.is_sent || rules.is_empty() {
        return triaged;
    }
    let is_known = triaged.matched_rule.is_none()
        && triaged.reason.contains("known contact");
    let result = stage1_with_config(&triaged.message, is_known, rules, cfg, now);
    triaged.importance = result.importance;
    triaged.tier = result.tier;
    triaged.one_line = result.one_line;
    triaged.reason = result.reason;
    triaged.field_reasons = result.field_reasons;
    triaged.matched_rule = result.matched_rule;
    triaged.deadline = result.deadline;
    triaged.confident = result.confident;
    triaged
}

/// Ingest ONE message the account just sent, from its raw RFC822 bytes.
///
/// Pure bytes-in: the caller (squelch-api's send path, which alone holds write
/// credentials) does the Gmail fetch and hands the decoded bytes over, so core
/// stays Gmail-WRITE-free. Runs the same seal-first pipeline as the sync engine
/// with `is_sent: true`, so no LLM is ever called and the row lands neutral
/// (tier=noise, importance=0); the attention/search queries filter `is_sent=0`,
/// so an echoed message creates no attention noise. Idempotent: the store's
/// `UNIQUE(account_id, gmail_msg_id)` upsert makes a re-ingest of the same Gmail
/// id a no-op update, returning the existing local id.
///
/// A SEALED OUTBOUND COPY IS NOT WRITTEN: `Ok(None)`, nothing committed. Seal
/// detection runs BEFORE the `is_sent` branch (that ordering is the security
/// invariant), so a reply quoting an OTP trips it. Committing that row would put a
/// sealed message in the thread, and `thread_guard_and_subject` 404s any thread
/// holding one, so echoing the reply would HIDE the counterparty's mail the user
/// was reading a second ago. Skipping degrades to "your reply appears on the next
/// backfill", which is exactly the pre-echo status quo.
pub fn ingest_sent(
    store: &SqliteStore,
    account_id: AccountId,
    account_addr: &str,
    gmail_msg_id: &str,
    gmail_thread_id: Option<String>,
    raw: Vec<u8>,
    internal_date: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> crate::error::Result<Option<i64>> {
    let fetched = RawFetched {
        account_id,
        gmail_msg_id: gmail_msg_id.to_string(),
        gmail_thread_id,
        raw,
        internal_date,
        is_sent: true,
        account_addr: account_addr.to_string(),
    };
    // Both arguments are unreachable on the sent path: Stage-1 (which cfg tunes)
    // and the contact lookup only run for non-sealed RECEIVED mail.
    let triaged = ingest(&fetched, &Stage1Config::default(), now, |_| false);
    if triaged.sensitivity == Sensitivity::Sealed {
        return Ok(None);
    }
    store.ingest_message(&triaged).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(account_id: AccountId, msgid: &str, bytes: &str, is_sent: bool) -> RawFetched {
        RawFetched {
            account_id,
            gmail_msg_id: msgid.to_string(),
            gmail_thread_id: Some(format!("thr-{msgid}")),
            raw: bytes.as_bytes().to_vec(),
            internal_date: Some(Utc::now()),
            is_sent,
            account_addr: "me@example.com".to_string(),
        }
    }

    #[test]
    fn html_flatten_strips_tags_and_entities() {
        let t = html_to_text("<p>Hello&nbsp;<b>world</b> &amp; <i>friends</i></p>");
        assert_eq!(t, "Hello world & friends");
    }

    #[test]
    fn sealed_otp_lands_sealed_with_importance_zero() {
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Enter this code to continue.\r\n";
        let f = raw(1, "g-otp", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(t.sealed_kind.is_some());
        assert_eq!(t.importance, 0);
        assert!(t.deadline.is_none());
    }

    #[test]
    fn dated_bill_lands_deadline_tier() {
        let eml = "From: Acme <invoices@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #4402 from Acme\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your invoice total is $1,299.00. Payment due by August 15, 2026.\r\n";
        let f = raw(1, "g-bill", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Normal);
        assert_eq!(t.tier, Tier::Deadline);
        let d = t.deadline.expect("deadline extracted");
        assert_eq!(d.amount, Some(1299.00));
        assert!(!d.past_due);
    }

    #[test]
    fn ebay_return_refund_is_not_a_past_due_bill() {
        // The real inbox case (today = 2026-07-09): an eBay RETURN REFUND arriving
        // "by July 13th" was mis-triaged as a past-due bill "104 weeks after due
        // date". It must now produce NO bill tier, NO past_due, and no deadline row.
        let eml = "From: eBay <ebay@ebay.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your return refund is on its way\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your eBay return refund of $23.99 will be issued by July 13th.\r\n";
        let f = raw(1, "g-ebay", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| false);
        assert_ne!(t.tier, Tier::PastDue, "refund must not be past-due");
        assert_ne!(t.tier, Tier::Deadline, "refund must not be a deadline");
        assert!(t.deadline.is_none(), "no deadline row for a refund");
    }

    #[test]
    fn yearless_genuine_bill_resolves_to_receipt_year_future() {
        // A genuine bill "due July 13" (no year) received 2026-07-09 resolves to
        // 2026-07-13 (this year, future) => Deadline tier, NOT past_due.
        let eml = "From: Acme <billing@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #900\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Amount due $50.00. Payment due July 13th.\r\n";
        let f = raw(1, "g-yearless", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| true);
        assert_eq!(t.tier, Tier::Deadline);
        let d = t.deadline.expect("deadline extracted");
        assert!(!d.past_due, "future year-less date is not past-due");
        assert_eq!(
            d.due_at,
            DateTime::parse_from_rfc3339("2026-07-13T23:59:59Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn yearless_recently_passed_bill_is_past_due_by_days() {
        // "due July 1" received 2026-07-09 => 2026-07-01 (past by days, within the
        // 14-day grace) => PastDue, legitimately, not "weeks/years" late.
        let eml = "From: Acme <billing@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #901\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Amount due $50.00. Payment due July 1.\r\n";
        let f = raw(1, "g-recent", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| true);
        let d = t.deadline.expect("deadline extracted");
        assert!(d.past_due);
        // Due date is 2026-07-01, ~8 days before receipt — days, not weeks.
        let days = (now - d.due_at).num_days();
        assert!((7..=9).contains(&days), "past by days, got {days}");
    }

    #[test]
    fn html_only_body_is_flattened_before_triage() {
        let eml = "From: News <news@substack.com>\r\n\
                   Subject: The Weekly Roundup\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   \r\n\
                   <html><body><p>Great stuff. <a href=\"x\">Unsubscribe</a> | Manage preferences</p></body></html>\r\n";
        let f = raw(1, "g-news", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Normal);
        assert_eq!(t.tier, Tier::Noise);
        assert!(t.message.body.contains("Unsubscribe"));
        assert!(!t.message.body.contains('<'));
    }

    #[test]
    fn known_contact_lookup_is_consulted_for_normal_mail() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   Subject: dinner plans\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   friday at 7?\r\n";
        let f = raw(1, "g-alice", eml, false);
        let mut asked = Vec::new();
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |addr| {
            asked.push(addr.to_string());
            true
        });
        assert_eq!(t.tier, Tier::Signal);
        assert!(asked.iter().any(|a| a == "alice@friends.com"));
    }

    #[test]
    fn sent_mail_derives_contacts_from_recipients_not_self() {
        // From is the account (self); To/Cc are the real contacts.
        let eml = "From: Me <me@example.com>\r\n\
                   To: Alice <alice@friends.com>\r\n\
                   Cc: bob@friends.com\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   see you friday\r\n";
        let f = raw(1, "g-sent", eml, /* is_sent */ true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        // recipients hold alice + bob, never self.
        assert!(t.recipients.iter().any(|r| r == "alice@friends.com"));
        assert!(t.recipients.iter().any(|r| r == "bob@friends.com"));
        assert!(!t.recipients.iter().any(|r| r == "me@example.com"));
        // Sent mail is not triaged: neutral noise / importance 0.
        assert_eq!(t.tier, Tier::Noise);
        assert_eq!(t.importance, 0);
    }

    #[test]
    fn sent_mail_never_seeds_self_even_when_to_is_self() {
        let eml = "From: me@example.com\r\n\
                   To: me@example.com\r\n\
                   Subject: note to self\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   reminder\r\n";
        let f = raw(1, "g-self", eml, true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.recipients.is_empty(), "self address must never be a contact");
    }

    #[test]
    fn received_mail_seeds_no_contacts() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: hi\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   hello\r\n";
        let f = raw(1, "g-recv", eml, /* is_sent */ false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.recipients.is_empty());
    }

    #[test]
    fn robot_addresses_are_filtered() {
        // Every one of these is a live example that MUST be filtered.
        let robots = [
            "leave-HXZRUFGHTN2UJLNONA7FQQ27HY.110064@leave.mcmap.chase.com",
            "9b284cf8-cebd-451c-95f9-cb939dc4682d+dac84f53-1111-2222-3333-444455556666+xyz@unsub.beehiiv.com",
            "unsubscribe-mc.us22_89497e127e8f1447718905808.aef79637c2-4c8c73a87a@unsubscribe.mailchimpapp.net",
            "dxirq3pb.560xwm.9t9eb.optout@e2ma.net",
            "unsubscribe@gf.d.sender-sib.com",
            "unsubscribe@unsub.spmta.com",
            "d6f58aa9b599316889f7d3cc20bf13bc@hous.craigslist.org",
            "1axcsnai4asp830zv6mplv6pvulamp169hk3nf-bboynton97=gmail.com@bf02.na2.hubspotemail.net",
            "097a2550-566e-11e6-83f0-002590e879ee@unsub.r.groupon.com",
        ];
        for r in robots {
            assert!(is_robot_address(r), "should be filtered as robot: {r}");
        }
    }

    #[test]
    fn real_people_survive() {
        // Real people that MUST pass through as contacts.
        let people = [
            "ellie@elliehuxtable.com",
            "bam@bamteamre.com",
            "cameron@tcpre.com",
            "rentbikes.net@gmail.com",
        ];
        for p in people {
            assert!(!is_robot_address(p), "should NOT be filtered: {p}");
        }
    }

    #[test]
    fn sent_mail_drops_robot_recipients_keeps_people() {
        let eml = "From: Me <me@example.com>\r\n\
                   To: Alice <alice@friends.com>, unsubscribe@unsub.spmta.com\r\n\
                   Cc: d6f58aa9b599316889f7d3cc20bf13bc@hous.craigslist.org\r\n\
                   Subject: mixed\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   body\r\n";
        let f = raw(1, "g-mixed", eml, true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.recipients.iter().any(|r| r == "alice@friends.com"));
        assert!(!t.recipients.iter().any(|r| r.contains("unsub")));
        assert!(!t.recipients.iter().any(|r| r.contains("craigslist")));
    }

    #[test]
    fn shipping_email_produces_a_shipment() {
        let eml = "From: UPS <ship-confirm@ups.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your order of Wireless Headphones has shipped\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your UPS package is on its way. Tracking number 1Z999AA10123456784.\r\n";
        let f = raw(1, "g-ship", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let s = t.shipment.expect("shipment detected");
        assert_eq!(s.carrier, "ups");
        assert_eq!(s.tracking_number, "1Z999AA10123456784");
        assert_eq!(s.item_name, "Wireless Headphones");
        // Shipping mail is noise-tier for the ranked inbox.
        assert_eq!(t.tier, Tier::Noise);
    }

    #[test]
    fn bay_wheels_receipt_produces_a_receipt_row_with_amount() {
        // The live bug: a Bay Wheels ride receipt landed in Newsletters. It must
        // now classify as a receipt (with its total) and drop from inbox clutter.
        let eml = "From: Bay Wheels <no-reply@baywheels.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your Bay Wheels ride receipt\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Thanks for riding! Receipt for your ride. Total: $3.49.\r\n";
        let f = raw(1, "g-baywheels", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let r = t.receipt.expect("receipt detected");
        assert_eq!(r.amount, Some(3.49));
        // Receipts are noise-tier for the ranked inbox.
        assert_eq!(t.tier, Tier::Noise);
    }

    #[test]
    fn order_confirmation_receipt_extracts_total() {
        let eml = "From: Shop <orders@shop.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Order confirmation #12345\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Thank you for your order. Order total $3.49.\r\n";
        let f = raw(1, "g-order", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.receipt.expect("receipt").amount, Some(3.49));
    }

    #[test]
    fn refund_is_not_a_receipt_at_ingest() {
        let eml = "From: eBay <ebay@ebay.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your refund receipt\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your refund of $23.99 has been issued to your card.\r\n";
        let f = raw(1, "g-refund", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.receipt.is_none(), "a refund must not be a receipt");
    }

    #[test]
    fn sealed_otp_never_produces_a_receipt() {
        // A sealed OTP short-circuits before receipt detection ever runs.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Thank you for your payment.\r\n";
        let f = raw(1, "g-otp-rcpt", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(t.receipt.is_none(), "sealed mail must never yield a receipt");
    }

    #[test]
    fn google_invite_produces_a_calendar_update_at_ingest() {
        // A Google Calendar invite: classified as a calendar update (with title
        // + start extracted) so the ingest write auto-resolves it out of the
        // attention bands and into the Calendar category.
        let eml = "From: Sam Doe <sam@gmail.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invitation: Design review @ Wed Jul 22, 2026 10am - 11am (PDT) (me@example.com)\r\n\
                   Date: Mon, 20 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Sam Doe has invited you. View on Google Calendar.\r\n";
        let f = raw(1, "g-cal-inv", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let c = t.calendar.expect("calendar update detected");
        assert_eq!(c.kind, crate::triage::CalendarKind::Invite);
        assert_eq!(c.event_title.as_deref(), Some("Design review"));
        assert!(c.starts_at.is_some());
    }

    #[test]
    fn newsletter_mentioning_calendar_is_not_a_calendar_update_at_ingest() {
        let eml = "From: Shop News <news@shop.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your July events calendar is here!\r\n\
                   Date: Mon, 20 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Check our calendar of sales events. Unsubscribe here.\r\n";
        let f = raw(1, "g-cal-news", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.calendar.is_none(), "topical calendar prose must not classify");
    }

    #[test]
    fn sealed_otp_never_produces_a_calendar_update() {
        // A sealed OTP short-circuits before calendar detection ever runs, even
        // with an invitation-shaped subject.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 20 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Invitation: security review @ Wed Jul 22, 2026 10am.\r\n";
        let f = raw(1, "g-otp-cal", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(t.calendar.is_none(), "sealed mail must never yield a calendar update");
    }

    #[test]
    fn sealed_otp_never_produces_a_shipment() {
        // A sealed OTP short-circuits before shipment detection ever runs.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920123456. Enter this code to continue.\r\n";
        let f = raw(1, "g-otp2", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(t.shipment.is_none(), "sealed mail must never yield a shipment");
    }

    #[test]
    fn list_unsubscribe_headers_land_on_the_message() {
        // A newsletter advertising both a mailto and an https one-click endpoint,
        // with RFC 8058 List-Unsubscribe-Post. Both fields must be captured.
        let eml = "From: News <news@substack.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: The Weekly Roundup\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   List-Unsubscribe: <mailto:unsub@substack.com?subject=bye>, <https://substack.com/u/9?x=1>\r\n\
                   List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
                   \r\n\
                   Great stuff this week.\r\n";
        let f = raw(1, "g-unsub", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let lu = t.message.list_unsubscribe.expect("list-unsubscribe captured");
        assert!(lu.contains("mailto:unsub@substack.com"));
        assert!(lu.contains("https://substack.com/u/9"));
        assert!(t.message.list_unsub_one_click, "RFC 8058 one-click detected");
    }

    #[test]
    fn list_unsubscribe_without_post_is_not_one_click() {
        let eml = "From: News <news@substack.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Roundup\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   List-Unsubscribe: <https://substack.com/u/9>\r\n\
                   \r\n\
                   body\r\n";
        let f = raw(1, "g-unsub2", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.message.list_unsubscribe.is_some());
        assert!(!t.message.list_unsub_one_click, "no List-Unsubscribe-Post => not one-click");
    }

    #[test]
    fn no_unsub_header_leaves_fields_empty() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   friday?\r\n";
        let f = raw(1, "g-plain-unsub", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.message.list_unsubscribe.is_none());
        assert!(!t.message.list_unsub_one_click);
    }

    #[test]
    fn attachments_extracted_with_caps_and_cid_inline() {
        // A real multipart/mixed with: a text body (NOT an attachment), a pdf, a
        // png, a cid-inline png (no filename), and an oversized octet-stream part
        // that must exceed the 10 MB per-attachment cap.
        let big = "A".repeat(11 * 1024 * 1024);
        let eml = format!(
            "From: Sender <s@example.com>\r\n\
             To: me@example.com\r\n\
             Subject: Files attached\r\n\
             Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\
             \r\n\
             --BOUND\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Here are the files.\r\n\
             --BOUND\r\n\
             Content-Type: application/pdf; name=\"doc.pdf\"\r\n\
             Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             SGVsbG8=\r\n\
             --BOUND\r\n\
             Content-Type: image/png; name=\"pic.png\"\r\n\
             Content-Disposition: attachment; filename=\"pic.png\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             d29ybGQ=\r\n\
             --BOUND\r\n\
             Content-Type: image/png\r\n\
             Content-ID: <logo@squelch>\r\n\
             Content-Disposition: inline\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             aW5saW5l\r\n\
             --BOUND\r\n\
             Content-Type: application/octet-stream; name=\"big.bin\"\r\n\
             Content-Disposition: attachment; filename=\"big.bin\"\r\n\
             \r\n\
             {big}\r\n\
             --BOUND--\r\n"
        );
        let f = raw(1, "g-att", &eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let a = &t.attachments;
        assert_eq!(
            a.len(),
            4,
            "pdf + png + cid-inline + big (the text body is not an attachment)"
        );

        let pdf = a.iter().find(|x| x.filename == "doc.pdf").expect("pdf");
        assert_eq!(pdf.mime, "application/pdf");
        assert_eq!(pdf.size_bytes, 5);
        assert_eq!(pdf.data.as_deref(), Some(&b"Hello"[..]));

        let png = a.iter().find(|x| x.filename == "pic.png").expect("png");
        assert_eq!(png.mime, "image/png");
        assert_eq!(png.data.as_deref(), Some(&b"world"[..]));

        // The cid-inline part declares no filename -> attachment-<n> fallback, and
        // its bytes are kept (inline images are how templates embed logos).
        let inline = a
            .iter()
            .find(|x| x.filename.starts_with("attachment-"))
            .expect("cid inline part included");
        assert_eq!(inline.mime, "image/png");
        assert_eq!(inline.data.as_deref(), Some(&b"inline"[..]));

        // The oversized part keeps its metadata (real size) but drops its bytes.
        let big_att = a.iter().find(|x| x.filename == "big.bin").expect("big");
        assert_eq!(big_att.size_bytes, (11 * 1024 * 1024) as i64);
        assert!(big_att.data.is_none(), "over-cap attachment stores no bytes");
    }

    #[test]
    fn plain_mail_has_no_attachments() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   friday?\r\n";
        let f = raw(1, "g-plain-att", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.attachments.is_empty());
    }

    #[test]
    fn sealed_mail_still_extracts_attachments_for_storage() {
        // Attachments are STORED for sealed mail like the body; serving is guarded
        // downstream. So extraction must still happen on the sealed path.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                   \r\n\
                   --B\r\nContent-Type: text/plain\r\n\r\n\
                   Your one-time passcode is 483920. Enter this code to continue.\r\n\
                   --B\r\nContent-Type: application/pdf\r\n\
                   Content-Disposition: attachment; filename=\"statement.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
                   --B--\r\n";
        let f = raw(1, "g-sealed-att", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert_eq!(t.attachments.len(), 1, "sealed mail's attachment is still extracted for storage");
        assert_eq!(t.attachments[0].filename, "statement.pdf");
    }

    #[test]
    fn ingest_sent_commits_a_neutral_row_and_is_idempotent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Me <me@example.com>\r\n\
                   To: Alice <alice@friends.com>\r\n\
                   Subject: Re: Lunch?\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   yes, noon works\r\n";
        let now = Utc::now();
        let id = ingest_sent(
            &store,
            acct,
            "me@example.com",
            "sent-1",
            Some("thread-77".to_string()),
            eml.as_bytes().to_vec(),
            Some(now),
            now,
        )
        .unwrap()
        .expect("a normal outbound copy is committed");

        // Visible in the thread, neutral, and never a source of attention noise.
        let view = store.thread_view_with_html(acct, "thread-77").unwrap();
        assert_eq!(view.messages.len(), 1);
        assert!(view.messages[0].content.contains("noon works"));
        let updates = store
            .attention_updates(acct, now - chrono::Duration::days(1), None, None, None)
            .unwrap();
        assert!(updates.is_empty(), "sent mail never enters the attention bands");
        // Recipients still seed contacts.
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());

        // Re-ingesting the same Gmail id upserts onto the same local row.
        let again = ingest_sent(
            &store,
            acct,
            "me@example.com",
            "sent-1",
            Some("thread-77".to_string()),
            eml.as_bytes().to_vec(),
            Some(now),
            now,
        )
        .unwrap();
        assert_eq!(again, Some(id), "idempotent on the UNIQUE(account, gmail id) upsert");
        assert_eq!(
            store.thread_view_with_html(acct, "thread-77").unwrap().messages.len(),
            1
        );
    }

    #[test]
    fn ingest_sent_skips_a_sealed_outbound_copy_and_commits_nothing() {
        // A reply quoting an OTP trips seal detection (which runs before the
        // `is_sent` branch, by design). Committing that row would make
        // `thread_guard_and_subject` 404 the thread the user is reading, so nothing
        // is written at all.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let parent = store
            .upsert_message(&NewMessage {
                account_id: acct,
                gmail_msg_id: "g-parent".to_string(),
                thread_id: "thread-88".to_string(),
                from_addr: "noreply@bank.com".to_string(),
                from_name: None,
                subject: "Your verification code".to_string(),
                received_at: Utc::now(),
                snippet: String::new(),
                body: "hello".to_string(),
                body_html: None,
                is_sent: false,
                list_unsubscribe: None,
                list_unsub_one_click: false,
            })
            .unwrap();
        store
            .set_triage(
                parent,
                acct,
                40,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();

        let eml = "From: Me <me@example.com>\r\n\
                   To: Support <support@bank.com>\r\n\
                   Subject: Re: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   I never asked for this. Your one-time passcode is 483920.\r\n";
        let now = Utc::now();
        let echoed = ingest_sent(
            &store,
            acct,
            "me@example.com",
            "sent-sealed",
            Some("thread-88".to_string()),
            eml.as_bytes().to_vec(),
            Some(now),
            now,
        )
        .unwrap();
        assert!(echoed.is_none(), "a sealed outbound copy is not echoed");

        // Nothing committed: no sealed row, and the thread still opens with the
        // parent alone rather than 404ing on a sealed member.
        assert!(store.sealed_messages(acct).unwrap().is_empty());
        let view = store.thread_view_with_html(acct, "thread-88").unwrap();
        assert_eq!(view.messages.len(), 1, "only the parent; the echo was skipped");
        // Contacts are not seeded either — the whole write was skipped.
        assert!(!store.is_known_contact(acct, "support@bank.com").unwrap());
    }

    #[test]
    fn missing_msgid_falls_back_to_hash() {
        let eml = "From: x@y.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let mut f = raw(1, "", eml, false);
        f.gmail_thread_id = None;
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(!t.message.gmail_msg_id.is_empty());
        // thread_id falls back to the derived id, never empty.
        assert!(!t.message.thread_id.is_empty());
    }
}
