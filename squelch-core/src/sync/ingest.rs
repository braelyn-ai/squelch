//! The ingest pipeline: raw RFC822 bytes -> parsed -> flattened text ->
//! seal-first triage -> a [`TriagedMessage`] ready for an atomic store write.
//!
//! SECURITY (ordering is an invariant):
//!   1. parse + flatten to text (HTML crudely stripped),
//!   2. seal detection FIRST ([`triage::seal`]) — sealed mail is classified
//!      `sensitivity='sealed'`, importance 0, and NEVER runs Stage-1 or reaches
//!      any LLM,
//!   3. only for non-sealed mail: `is_known_contact` + sender rules -> Stage-1.
//!
//! This module is deliberately free of any network/IMAP types so it can be unit
//! tested against fixture RFC822 bytes with no connection.

use crate::config::Stage1Config;
use crate::store::TriagedMessage;
use crate::triage::seal::{self, SealInput};
use crate::triage::stage1_with_config;
use crate::types::{AccountId, NewMessage, Sensitivity, Tier};
use chrono::{DateTime, Utc};
use mail_parser::{Address, MessageParser};

/// The raw identity/metadata the transport supplies alongside the RFC822 body.
/// The Gmail REST engine fills `gmail_msg_id` from the native `message.id` and
/// `gmail_thread_id` from the native `message.threadId`; when both are absent
/// (e.g. a synthetic metadata-only Sent ingest) the pipeline falls back to a
/// header-derived thread key (see [`fallback_thread_id`]).
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
}

/// Crudely flatten HTML to text: drop tags, decode a handful of common
/// entities, collapse whitespace. mail-parser already gives us a text part when
/// one exists; this is the HTML-only fallback so we never feed raw markup to
/// triage.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let chars = html.chars();
    for c in chars {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if in_tag => {}
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
    // Collapse runs of whitespace.
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

/// Derive a stable thread key from headers when X-GM-THRID is unavailable.
/// Uses the root References id, else In-Reply-To, else this message's own
/// Message-ID. This keeps a reply chain grouped without Gmail's THRID.
pub fn fallback_thread_id(msg: &mail_parser::Message) -> Option<String> {
    // References: first id is the thread root.
    if let Some(first) = msg.references().as_text_list().and_then(|l| l.iter().next()) {
        return Some(first.to_string());
    }
    if let Some(irt) = msg.in_reply_to().as_text_list().and_then(|l| l.iter().next()) {
        return Some(irt.to_string());
    }
    msg.message_id().map(|s| s.to_string())
}

/// A tiny FNV-1a hash, hex-encoded. Used as a last-resort stable id when even a
/// Message-ID is missing (so two distinct such messages don't collide on "").
pub fn stable_hash(input: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in input.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
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

    // Extract fields with graceful fallbacks for malformed mail.
    let (from_addr, from_name, subject, received_at, thread_id, msg_id_hdr, text) = match &parsed {
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
            let text = if m.text_body_count() > 0 {
                m.body_text(0).map(|c| c.into_owned()).unwrap_or_default()
            } else if m.html_body_count() > 0 {
                html_to_text(&m.body_html(0).map(|c| c.into_owned()).unwrap_or_default())
            } else {
                String::new()
            };
            let thr = fetched
                .gmail_thread_id
                .clone()
                .or_else(|| fallback_thread_id(m));
            (fa, fname, subject, received, thr, m.message_id().map(|s| s.to_string()), text)
        }
        None => (
            String::new(),
            None,
            String::new(),
            fetched.internal_date.unwrap_or(now),
            fetched.gmail_thread_id.clone(),
            None,
            String::new(),
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

    // A compact snippet for list views; body text drives triage.
    let snippet: String = text.chars().take(200).collect();

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
        is_sent: fetched.is_sent,
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
            sensitivity: Sensitivity::Sealed,
            sealed_kind: Some(kind),
            importance: 0,
            tier: Tier::Noise,
            one_line: String::new(),
            reason: format!("sealed at ingest ({})", kind.as_str()),
            matched_rule: None,
            deadline: None,
            confident: true,
        };
    }

    // ---- Non-sealed: derive known-contact, load rules already provided --
    let is_known = known_contact_lookup(&from_addr);
    // Sender rules are matched inside stage1; the caller supplies them via cfg's
    // sibling argument. We accept them through the wrapper below.
    let result = stage1_with_config(&message, is_known, &[], cfg, now);

    TriagedMessage {
        message,
        sensitivity: Sensitivity::Normal,
        sealed_kind: None,
        importance: result.importance,
        tier: result.tier,
        one_line: result.one_line,
        reason: result.reason,
        matched_rule: result.matched_rule,
        deadline: result.deadline,
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
    if triaged.sensitivity == Sensitivity::Sealed || rules.is_empty() {
        return triaged;
    }
    let is_known = triaged.matched_rule.is_none()
        && triaged.reason.contains("known contact");
    let result = stage1_with_config(&triaged.message, is_known, rules, cfg, now);
    triaged.importance = result.importance;
    triaged.tier = result.tier;
    triaged.one_line = result.one_line;
    triaged.reason = result.reason;
    triaged.matched_rule = result.matched_rule;
    triaged.deadline = result.deadline;
    triaged.confident = result.confident;
    triaged
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
