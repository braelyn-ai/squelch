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
use crate::sync::html::sanitize_email_html;
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
    /// The account's own email address, used to guard the contacts table so the
    /// user's own address can NEVER become a contact (Sent mail's From header is
    /// the user; contacts must be derived from To/Cc recipients instead).
    /// Lower-cased comparison; may be empty when unknown (then only the From
    /// address is excluded, since on Sent mail From == the account).
    pub account_addr: String,
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

/// Collect every non-empty email address from an [`Address`] header (handles
/// both flat address lists and grouped lists). Used to derive contacts from the
/// To/Cc recipients of Sent mail.
fn collect_addrs(addr: &Address, out: &mut Vec<String>) {
    for a in addr.iter() {
        if let Some(email) = a.address()
            && !email.is_empty()
        {
            out.push(email.to_string());
        }
    }
}

/// Heuristic: does `addr` look like a machine/robot address rather than a real
/// person? Used ONLY to filter recipient contact seeding, so Gmail's
/// mailto-unsubscribe traffic (real emails sent to unsubscribe/leave/optout
/// robots) never becomes a "person I know" contact and pollutes triage.
///
/// Kept deliberately simple — obvious rules over cleverness. It combines:
///   * local-part prefixes/patterns (unsub, leave-, optout, bounce, noreply…),
///   * domain first-label hints (unsub., leave., bounce., optout.…),
///   * token-like locals (long hex/UUID blobs, or multiple +-separated
///     UUID-ish segments) that no human picks as an address.
///
/// A real local part like `rentbikes.net` must pass (short, has vowels, not a
/// hex blob), and so must ordinary `first@domain.com` addresses.
pub fn is_robot_address(addr: &str) -> bool {
    let addr = addr.trim().to_ascii_lowercase();
    let (local, domain) = match addr.split_once('@') {
        Some((l, d)) if !l.is_empty() && !d.is_empty() => (l, d),
        // No parseable local@domain — not our concern here; let it through.
        _ => return false,
    };

    // --- Domain first-label hints (e.g. leave.mcmap.chase.com, unsub.beehiiv.com)
    let first_label = domain.split('.').next().unwrap_or("");
    const DOMAIN_ROBOT_LABELS: &[&str] =
        &["unsub", "unsubscribe", "leave", "bounce", "optout", "opt-out"];
    if DOMAIN_ROBOT_LABELS.contains(&first_label) {
        return true;
    }

    // --- Local-part prefixes/patterns.
    // The + is the plus-address boundary; segment on it so a prefix on ANY
    // +-segment (e.g. "unsubscribe-mc.us22_...") is caught.
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

    // --- Token-like locals: opaque machine blobs no human would choose.
    // Multiple +-separated UUID-ish segments (beehiiv style).
    let uuidish_segments = plus_segments.iter().filter(|s| looks_uuidish(s)).count();
    if uuidish_segments >= 1 && plus_segments.len() >= 2 {
        return true;
    }
    // A single segment that is itself a UUID or a long hex/token blob.
    for seg in &plus_segments {
        if looks_uuidish(seg) || is_hex_blob(seg) {
            return true;
        }
    }

    // A long opaque alnum run (>=25 chars) with a machine-token character mix:
    // digits present AND a low vowel ratio. Break the local on the usual
    // separators and inspect each run so a real dotted name (rentbikes.net)
    // never trips it — its runs are short and vowel-rich.
    for run in local.split(['.', '-', '_', '=']) {
        if is_token_blob(run) {
            return true;
        }
    }

    false
}

/// A long random-looking machine token: >=25 alphanumeric chars, containing at
/// least one digit, with a low vowel ratio (<20%). Human words — even long ones
/// — carry far more vowels; random ids skimp on them and pepper in digits.
fn is_token_blob(s: &str) -> bool {
    if s.len() < 25 || !s.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    let has_digit = s.bytes().any(|b| b.is_ascii_digit());
    if !has_digit {
        return false;
    }
    let vowels = s.bytes().filter(|b| b"aeiou".contains(b)).count();
    // Low vowel density is the tell of a random token.
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

/// A long opaque hex/token blob: >=20 chars drawn only from [0-9a-f-] (and not a
/// human-readable dotted name). Real names like `rentbikes.net` contain letters
/// outside a-f and stay well under the threshold, so they pass.
fn is_hex_blob(s: &str) -> bool {
    if s.len() < 20 {
        return false;
    }
    // Only hex digits and hyphens — a machine token, not a word.
    s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
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
    let (from_addr, from_name, subject, received_at, thread_id, msg_id_hdr, text, body_html) =
        match &parsed {
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
                (
                    fa,
                    fname,
                    subject,
                    received,
                    thr,
                    m.message_id().map(|s| s.to_string()),
                    text,
                    body_html,
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
            matched_rule: None,
            deadline: None,
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
        recipients,
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
