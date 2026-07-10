//! Triage pipeline.
//!
//! Stage 1 has two sub-stages that run at INGEST, in this order:
//!   1. [`seal`] detector (implemented elsewhere) — seals auth mail before
//!      anything else can see it. Sealed mail never reaches [`stage1`].
//!   2. [`stage1`] — the rules engine in this module. A deterministic,
//!      LLM-free classifier that assigns every non-sealed message a [`Tier`],
//!      an importance score, and a one-liner. Where it is *not confident*, it
//!      marks the message for a later Stage-2 LLM pass.
//!
//! Stage 2: LLM ranking (stub in v0; ALWAYS skipped for sealed content). The
//! only messages that should reach Stage-2 are the ones [`stage1`] returns with
//! `confident == false`.
//!
//! ## The Stage-1 ladder (priority order — first match wins)
//!
//! 1. **BILL / PAYMENT** — highest recall priority. Missing a bill is the
//!    user's #1 fear, so bills are checked first and bypass every threshold by
//!    landing in [`Tier::PastDue`] / [`Tier::Deadline`]. `confident = true`.
//! 2. **SENDER RULES** — the user's own local rules (glob on the from-address):
//!    - `Squelch` -> [`Tier::Noise`], confident.
//!    - `Surface` -> [`Tier::Signal`], confident.
//!    - `Filtered` -> [`Tier::Noise`], **not confident** — the rule's
//!      `want_text` is a natural-language filter we can't evaluate without the
//!      LLM, so it queues for Stage-2 (see TODO below).
//! 3. **KNOWN CONTACT** — sender appears in the user's Sent mail -> Signal,
//!    confident.
//! 4. **ALERT** — ops/monitoring language from an automated sender -> Signal,
//!    confident.
//! 5. **NOISE** — unsubscribe/list shapes, receipts, cold-sales -> Noise,
//!    confident.
//! 6. **FALL-THROUGH** — unknown sender, no pattern -> the ambiguous middle:
//!    Noise-ish importance but **not confident**, so Stage-2 gets a look.

pub mod deadline;
pub mod rules;
pub mod seal;
pub mod shipment;
pub mod stage2;

pub use deadline::DeadlineHit;
pub use shipment::{ShipmentInfo, ShipmentStatus, detect_shipment};

use crate::config::Stage1Config;
use crate::error::CoreError;
use crate::store::Stage2Queued;
use crate::types::{Disposition, NewMessage, SealedKind, SenderRule, Sensitivity, Tier};
use chrono::{DateTime, Utc};

/// Result of the Stage-1 rules engine for a single (non-sealed) message.
///
/// `confident == false` is the signal to the sync engine that this message
/// should be enqueued for the Stage-2 LLM pass. Everything else is final.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage1Result {
    pub tier: Tier,
    pub importance: u8,
    pub one_line: String,
    pub reason: String,
    pub deadline: Option<DeadlineHit>,
    /// Local id of the sender rule that fired, if any.
    pub matched_rule: Option<i64>,
    /// `true` => final; `false` => queue for Stage-2 LLM triage.
    pub confident: bool,
}

/// Legacy triage outcome kept for the store/upsert path. Prefer [`Stage1Result`].
#[derive(Debug, Clone)]
pub struct TriageResult {
    pub importance: u8,
    pub tier: Tier,
    pub sensitivity: Sensitivity,
    pub sealed_kind: Option<SealedKind>,
    pub one_line: String,
    pub reason: String,
    pub matched_rule: Option<i64>,
}

/// Run the Stage-1 rules engine over a stored-message view.
///
/// This is the entry point the sync engine calls, in the same transaction that
/// stores the (already non-sealed) message. The signature is deliberately free
/// of IMAP/store types: it takes the core [`NewMessage`] view, a
/// `is_known_contact` flag the caller derives from Sent-mail contacts, and the
/// account's [`SenderRule`]s.
///
/// Uses the default [`Stage1Config`]. Call [`stage1_with_config`] to override.
pub fn stage1(msg: &NewMessage, is_known_contact: bool, rules: &[SenderRule]) -> Stage1Result {
    stage1_with_config(msg, is_known_contact, rules, &Stage1Config::default(), Utc::now())
}

/// [`stage1`] with an explicit config and `now` (injected for deterministic
/// tests and for past-due math).
pub fn stage1_with_config(
    msg: &NewMessage,
    is_known_contact: bool,
    rules: &[SenderRule],
    cfg: &Stage1Config,
    now: DateTime<Utc>,
) -> Stage1Result {
    let subject = msg.subject.as_str();
    let body = msg.body.as_str();
    let from = msg.from_addr.as_str();

    // Resolve the matched sender rule ONCE, up front. Rung 2 still owns the
    // rule's disposition-driven tiering below; here we only need it to decide
    // bill-trust in rung 1 (bills are evaluated first). Matching is pure — this
    // reorder changes no behavior for the non-bill rungs.
    let matched_rule = rules::match_sender_rule(from, rules);

    // ---- Rung 1: BILL / PAYMENT (highest recall) ------------------------
    if let Some(hit) = deadline::detect_bill(subject, body, now) {
        // Sender trust gates the scream. A bill from a TRUSTED sender keeps full
        // confidence and its natural PastDue/Deadline tier. Trust has two
        // sources:
        //   * a KNOWN CONTACT (the sender appears in the user's Sent mail), or
        //   * an explicit SURFACE sender rule — the user ruled this sender, which
        //     is a direct statement of trust in their own vendor.
        // A bill from an UNKNOWN, unruled sender must NOT be allowed to land
        // CONFIDENT PastDue — that is exactly the scam vector in bug #3 (a
        // stranger screaming "past due" for $2.4M). We cap it at Deadline, mark
        // it not-confident (queue for Stage-2), and use a moderate importance.
        let surface_ruled = matched_rule
            .is_some_and(|r| r.disposition == Disposition::Surface);
        let trusted = is_known_contact || surface_ruled;

        // Sanity dampeners. These NEVER raise a tier; they only shave confidence.
        // A SURFACE-ruled sender is an EXPLICIT user trust statement: the user
        // knows their vendor sends big, urgent invoices, so the absurd-amount and
        // scammy-phrasing dampeners are bypassed for them. Known-contact trust
        // does NOT bypass the dampeners (a known biller with a $2.4M "past due" is
        // still suspicious); only the explicit rule does.
        let absurd_amount = hit
            .amount
            .is_some_and(|a| a > cfg.bill_absurd_amount_threshold);
        let scammy = deadline::has_scammy_phrasing(subject, body);
        let dampened = (absurd_amount || scammy) && !surface_ruled;

        // Tier: trusted senders keep their natural tier. Untrusted senders are
        // capped at Deadline (never PastDue). Dampeners also cap at Deadline.
        let natural_tier = if hit.past_due {
            Tier::PastDue
        } else {
            Tier::Deadline
        };
        let tier = if trusted && !dampened {
            natural_tier
        } else {
            Tier::Deadline
        };

        // Confidence: trusted + no dampener => final. Anything else => Stage-2.
        let confident = trusted && !dampened;

        let importance = if trusted {
            cfg.bill_importance
        } else {
            cfg.bill_unknown_sender_importance
        };

        let amount_str = match (hit.amount, hit.currency.as_deref()) {
            (Some(a), Some(c)) => format!(" {a:.2} {c}"),
            _ => String::new(),
        };
        let one_line = if tier == Tier::PastDue {
            format!("Past-due bill{amount_str}: {}", short_subject(subject))
        } else {
            format!("Bill due{amount_str}: {}", short_subject(subject))
        };

        let mut reason = format!(
            "bill/payment signal (kind={}, source={}, past_due={})",
            hit.kind, hit.source, hit.past_due
        );
        // Name the trust source that applied (or that it was absent).
        if surface_ruled {
            if let Some(r) = matched_rule {
                reason.push_str(&format!(
                    "; trusted via surface rule #{} ({})",
                    r.id, r.match_pattern
                ));
            }
        } else if is_known_contact {
            reason.push_str("; trusted via known contact");
        } else {
            reason.push_str("; bill-like from unknown sender (capped at Deadline, deferring to Stage-2)");
        }
        if dampened && absurd_amount {
            reason.push_str("; absurd amount dampener");
        }
        if dampened && scammy {
            reason.push_str("; screamy/scam phrasing dampener");
        }

        return Stage1Result {
            tier,
            importance,
            one_line,
            reason,
            deadline: Some(hit),
            matched_rule: None,
            confident,
        };
    }

    // ---- Rung 2: SENDER RULES (user's own local rules) ------------------
    // Reuse the rule resolved up front for rung 1's bill-trust decision.
    if let Some(rule) = matched_rule {
        return match rule.disposition {
            Disposition::Squelch => Stage1Result {
                tier: Tier::Noise,
                importance: cfg.rule_squelch_importance,
                one_line: short_subject(subject),
                reason: format!("matched squelch rule #{} ({})", rule.id, rule.match_pattern),
                deadline: None,
                matched_rule: Some(rule.id),
                confident: true,
            },
            Disposition::Surface => Stage1Result {
                tier: Tier::Signal,
                importance: cfg.rule_surface_importance,
                one_line: short_subject(subject),
                reason: format!("matched surface rule #{} ({})", rule.id, rule.match_pattern),
                deadline: None,
                matched_rule: Some(rule.id),
                confident: true,
            },
            // Filtered: the rule's `want_text` is a natural-language predicate we
            // cannot evaluate without the LLM. Park it in Noise but mark it
            // NOT confident so Stage-2 picks it up.
            //
            // TODO(stage2): when the Stage-2 pass runs for a Filtered-rule match,
            // inject `rule.want_text` as a TRUSTED user instruction (it is the
            // user's own rule, not attacker-controlled email content) and let the
            // LLM decide surface-vs-squelch against it.
            Disposition::Filtered => Stage1Result {
                tier: Tier::Noise,
                importance: cfg.rule_filtered_importance,
                one_line: short_subject(subject),
                reason: format!(
                    "matched filtered rule #{} ({}) — deferring to Stage-2 for want_text eval",
                    rule.id, rule.match_pattern
                ),
                deadline: None,
                matched_rule: Some(rule.id),
                confident: false,
            },
        };
    }

    // ---- Rung 3: KNOWN CONTACT ------------------------------------------
    if is_known_contact {
        return Stage1Result {
            tier: Tier::Signal,
            importance: cfg.known_contact_importance,
            one_line: short_subject(subject),
            reason: "known contact (appears in your Sent mail)".to_string(),
            deadline: None,
            matched_rule: None,
            confident: true,
        };
    }

    // ---- Rung 4: ALERT (automated ops/monitoring) -----------------------
    if rules::is_automated_sender(from) && rules::is_alert(subject, body) {
        return Stage1Result {
            tier: Tier::Signal,
            importance: cfg.alert_importance,
            one_line: format!("Alert: {}", short_subject(subject)),
            reason: "automated alert (build/outage/incident language)".to_string(),
            deadline: None,
            matched_rule: None,
            confident: true,
        };
    }

    // ---- Rung 5: NOISE (newsletter / receipt / cold sales) --------------
    if rules::is_unsubscribe_shaped(subject, body) {
        return noise(cfg, subject, "bulk/list mail (unsubscribe footer)");
    }
    if rules::is_receipt(subject, body) {
        return noise(cfg, subject, "order confirmation / receipt");
    }
    // Cold sales: only for UNKNOWN senders (known contacts already handled).
    if rules::is_sales(subject, body) {
        return noise(cfg, subject, "cold-outbound / sales language from unknown sender");
    }

    // ---- Rung 6: FALL-THROUGH (the ambiguous middle) --------------------
    // Unknown sender, nothing matched. Not confident: Stage-2 gets a look.
    Stage1Result {
        tier: Tier::Noise,
        importance: cfg.fallthrough_importance,
        one_line: short_subject(subject),
        reason: "no Stage-1 rule matched — ambiguous, deferring to Stage-2".to_string(),
        deadline: None,
        matched_rule: None,
        confident: false,
    }
}

fn noise(cfg: &Stage1Config, subject: &str, reason: &str) -> Stage1Result {
    Stage1Result {
        tier: Tier::Noise,
        importance: cfg.noise_importance,
        one_line: short_subject(subject),
        reason: reason.to_string(),
        deadline: None,
        matched_rule: None,
        confident: true,
    }
}

/// Trim/normalize a subject into a compact one-liner.
fn short_subject(subject: &str) -> String {
    let s = subject.trim();
    let s = if s.is_empty() { "(no subject)" } else { s };
    const MAX: usize = 120;
    if s.chars().count() > MAX {
        let truncated: String = s.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

/// Back-compat glob-based sender-rule match. Prefer
/// [`rules::match_sender_rule`]. Returns the first matching rule + disposition.
pub fn apply_sender_rules<'a>(
    from_addr: &str,
    rules: &'a [SenderRule],
) -> Option<(&'a SenderRule, Disposition)> {
    rules::match_sender_rule(from_addr, rules).map(|r| (r, r.disposition))
}

/// The SEALED GUARD for Stage-2 (defense in depth).
///
/// The Stage-2 queue predicate already excludes sealed rows in SQL, but that is
/// one layer. This is the second: a REAL runtime check (compiled into release,
/// unlike the old `debug_assert!`) that refuses to let any sealed row cross into
/// the LLM path. Returns `Err(CoreError::InvalidInput)` if a sealed row is ever
/// handed to Stage-2 — a bug in the caller, surfaced loudly rather than
/// silently leaking auth content to the model.
///
/// Call this on every queued row immediately before building the API request.
pub fn stage2_sealed_guard(row: &Stage2Queued) -> crate::error::Result<()> {
    if matches!(row.sensitivity, Sensitivity::Sealed) {
        // Redacted: no subject/body/sender — just the invariant and the id.
        return Err(CoreError::InvalidInput(format!(
            "stage-2 sealed guard: message {} is sealed and must never reach the LLM",
            row.message_id
        )));
    }
    Ok(())
}

/// Back-compat guard entry kept for callers that only have a [`Sensitivity`].
/// Replaces the old `debug_assert!`-only stub with a REAL release-mode check:
/// returns an error for sealed input instead of compiling the assertion out.
///
/// The per-row orchestration now lives in [`stage2`] (prompt build, `classify`,
/// `apply_result`) and is driven by the sync loop; this remains the minimal
/// invariant gate.
pub fn stage2_llm_triage(sensitivity: Sensitivity) -> crate::error::Result<()> {
    if matches!(sensitivity, Sensitivity::Sealed) {
        return Err(CoreError::InvalidInput(
            "sealed content must never reach the LLM stage".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap()
    }

    fn msg(from: &str, subject: &str, body: &str) -> NewMessage {
        NewMessage {
            account_id: 1,
            gmail_msg_id: "g1".into(),
            thread_id: "t1".into(),
            from_addr: from.into(),
            from_name: None,
            subject: subject.into(),
            received_at: now(),
            snippet: String::new(),
            body: body.into(),
            body_html: None,
            is_sent: false,
        }
    }

    fn rule(id: i64, pattern: &str, disp: Disposition, want: &str) -> SenderRule {
        SenderRule {
            id,
            account_id: 1,
            match_pattern: pattern.into(),
            want_text: want.into(),
            disposition: disp,
            updated_at: now(),
        }
    }

    fn run(m: &NewMessage, known: bool, rules: &[SenderRule]) -> Stage1Result {
        stage1_with_config(m, known, rules, &Stage1Config::default(), now())
    }

    // ---- Rung 1: bills --------------------------------------------------

    #[test]
    fn past_due_bill() {
        // Updated for bug #3: a confident PastDue now requires a TRUSTED sender
        // (known contact). Same email from a known biller still screams.
        let m = msg(
            "billing@utilityco.com",
            "PAST DUE: Your electric bill",
            "Amount due $84.20. This payment is overdue.",
        );
        let r = run(&m, true, &[]);
        assert_eq!(r.tier, Tier::PastDue);
        assert!(r.confident);
        let d = r.deadline.expect("deadline populated");
        assert!(d.past_due);
        assert_eq!(d.amount, Some(84.20));
        assert_eq!(d.currency.as_deref(), Some("USD"));
        assert_eq!(r.importance, 95);
    }

    #[test]
    fn dated_future_bill_is_deadline() {
        // Known sender => confident. (Bug #3: unknown senders now defer.)
        let m = msg(
            "invoices@acme.com",
            "Invoice #4402 from Acme",
            "Your invoice total is $1,299.00. Payment due by August 15, 2026.",
        );
        let r = run(&m, true, &[]);
        assert_eq!(r.tier, Tier::Deadline);
        assert!(r.confident);
        let d = r.deadline.unwrap();
        assert!(!d.past_due);
        assert_eq!(d.amount, Some(1299.00));
    }

    #[test]
    fn bill_wins_over_sender_rule() {
        // Even a squelch rule must not suppress a bill. Known sender so it keeps
        // its full PastDue tier (bug #3 caps only UNKNOWN senders).
        let m = msg(
            "billing@acme.com",
            "Your invoice is ready",
            "Amount due: $10.00, due date January 1, 2026",
        );
        let rules = vec![rule(9, "*@acme.com", Disposition::Squelch, "")];
        let r = run(&m, true, &rules);
        assert!(matches!(r.tier, Tier::PastDue));
        assert_eq!(r.matched_rule, None);
    }

    // ---- Bug #3: sender-trust-gated bills -------------------------------

    #[test]
    fn pge_style_past_due_from_known_sender_still_screams() {
        // The user's real fear: a legit past-due from a known biller MUST still
        // land CONFIDENT PastDue at the top of the scream tier.
        let m = msg(
            "customerservice@pge.com",
            "Your PG&E bill is past due",
            "Your account is past due. Amount due $214.77. Please pay to avoid \
             service interruption.",
        );
        let r = run(&m, true, &[]);
        assert_eq!(r.tier, Tier::PastDue, "known-sender past-due must scream");
        assert!(r.confident, "known-sender past-due must be final, not Stage-2");
        assert_eq!(r.importance, 95);
        let d = r.deadline.unwrap();
        assert!(d.past_due);
        assert_eq!(d.amount, Some(214.77));
    }

    #[test]
    fn scam_past_due_from_unknown_sender_is_not_confident_past_due() {
        // Bug #3 (confirmed against real mail): a scam "past due" from an unknown
        // sender parsed $2,470,000 and landed CONFIDENT PastDue at the top of
        // the scream tier. It must now be Deadline AT MOST, NOT confident.
        let m = msg(
            "info@sfmail.corpnet.com",
            "You're Past Due — Penalties Are Adding Up",
            "Your balance of $2,470,000 is past due. Penalties are adding up. \
             Pay now to avoid further action!!!",
        );
        let r = run(&m, false, &[]);
        assert_ne!(r.tier, Tier::PastDue, "scam must NOT be PastDue");
        assert_eq!(r.tier, Tier::Deadline, "capped at Deadline at most");
        assert!(!r.confident, "unknown-sender bill must defer to Stage-2");
        assert_eq!(r.importance, 55, "moderate importance from config");
        assert!(r.reason.contains("unknown sender"));
        // Both dampeners should have fired.
        assert!(r.reason.contains("absurd amount"));
        assert!(r.reason.contains("screamy/scam"));
    }

    #[test]
    fn bill_like_from_unknown_sender_defers_even_without_scam_signals() {
        // A plain, un-screamy invoice from a stranger: still capped at Deadline
        // and deferred to Stage-2 (never confident PastDue).
        let m = msg(
            "billing@unknown-vendor.example",
            "Invoice past due",
            "Your invoice for $120.00 is past due.",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Deadline);
        assert!(!r.confident);
        assert_eq!(r.importance, 55);
    }

    #[test]
    fn corpnet_past_due_with_surface_rule_is_confident_pastdue_dampeners_bypassed() {
        // A Surface rule is an explicit user statement of trust in their vendor.
        // The real corpnet-style fixture: large amount + "penalties are adding up"
        // + a Surface rule => CONFIDENT PastDue, BOTH dampeners bypassed. The user
        // ruled the sender; they know this vendor sends big, urgent invoices.
        let m = msg(
            "info@sfmail.corpnet.com",
            "You're Past Due — Penalties Are Adding Up",
            "Your balance of $2,470,000 is past due. Penalties are adding up. \
             Pay now to avoid further action!!!",
        );
        let rules = vec![rule(11, "*@sfmail.corpnet.com", Disposition::Surface, "")];
        let r = run(&m, false, &rules);
        assert_eq!(r.tier, Tier::PastDue, "surface-ruled sender screams past-due");
        assert!(r.confident, "surface-ruled bill is final, not Stage-2");
        assert_eq!(r.importance, 95, "full bill importance for trusted sender");
        // Dampeners are BYPASSED: their notes must NOT appear.
        assert!(!r.reason.contains("absurd amount"), "absurd dampener bypassed");
        assert!(!r.reason.contains("screamy/scam"), "scammy dampener bypassed");
        assert!(r.reason.contains("surface rule"), "names the trust source");
        // The bill rung wins over rung-2 rule tiering, so no matched_rule id here.
        assert_eq!(r.matched_rule, None);
    }

    #[test]
    fn corpnet_past_due_without_rule_is_still_dampened_regression() {
        // Same email, no Surface rule, unknown sender: the bug #3 protections
        // still hold — capped at Deadline, not confident, both dampeners fire.
        let m = msg(
            "info@sfmail.corpnet.com",
            "You're Past Due — Penalties Are Adding Up",
            "Your balance of $2,470,000 is past due. Penalties are adding up. \
             Pay now to avoid further action!!!",
        );
        let r = run(&m, false, &[]);
        assert_ne!(r.tier, Tier::PastDue);
        assert_eq!(r.tier, Tier::Deadline);
        assert!(!r.confident, "unknown-sender bill defers to Stage-2");
        assert_eq!(r.importance, 55);
        assert!(r.reason.contains("unknown sender"));
        assert!(r.reason.contains("absurd amount"));
        assert!(r.reason.contains("screamy/scam"));
    }

    #[test]
    fn squelch_ruled_sender_bill_unchanged() {
        // A Squelch rule does NOT confer bill-trust: the sender is not explicitly
        // trusted, so an unknown-sender past-due stays capped at Deadline and
        // deferred (the squelch rule still can't suppress a bill — rung 1 wins).
        let m = msg(
            "info@sfmail.corpnet.com",
            "You're Past Due — Penalties Are Adding Up",
            "Your balance of $2,470,000 is past due. Penalties are adding up!!!",
        );
        let rules = vec![rule(12, "*@sfmail.corpnet.com", Disposition::Squelch, "")];
        let r = run(&m, false, &rules);
        assert_eq!(r.tier, Tier::Deadline, "squelch confers no bill-trust");
        assert!(!r.confident);
        assert_eq!(r.matched_rule, None, "bill rung wins over the squelch rule");
    }

    #[test]
    fn surface_rule_trust_on_clean_past_due_names_source() {
        // A clean past-due (no dampeners) from an unknown sender WITH a Surface
        // rule: confident PastDue, reason names the surface-rule trust source.
        let m = msg(
            "billing@myvendor.example",
            "Invoice past due",
            "Your invoice for $120.00 is past due.",
        );
        let rules = vec![rule(13, "billing@myvendor.example", Disposition::Surface, "")];
        let r = run(&m, false, &rules);
        assert_eq!(r.tier, Tier::PastDue);
        assert!(r.confident);
        assert_eq!(r.importance, 95);
        assert!(r.reason.contains("surface rule #13"));
    }

    #[test]
    fn absurd_amount_dampens_even_known_sender() {
        // Sanity dampener applies regardless of sender: an absurd amount caps a
        // known-sender bill at Deadline and defers it, even though it's "trusted".
        let m = msg(
            "billing@utilityco.com",
            "PAST DUE",
            "Your balance of $2,470,000.00 is past due.",
        );
        let r = run(&m, true, &[]);
        assert_eq!(r.tier, Tier::Deadline, "absurd amount caps tier");
        assert!(!r.confident);
        assert!(r.reason.contains("absurd amount"));
    }

    // ---- Rung 2: sender rules ------------------------------------------

    #[test]
    fn surface_rule() {
        let m = msg("boss@work.com", "sync notes", "here you go");
        let rules = vec![rule(3, "boss@*", Disposition::Surface, "")];
        let r = run(&m, false, &rules);
        assert_eq!(r.tier, Tier::Signal);
        assert!(r.confident);
        assert_eq!(r.matched_rule, Some(3));
    }

    #[test]
    fn squelch_rule() {
        let m = msg("promo@shop.com", "50% off everything", "buy now");
        let rules = vec![rule(4, "*@shop.com", Disposition::Squelch, "")];
        let r = run(&m, false, &rules);
        assert_eq!(r.tier, Tier::Noise);
        assert!(r.confident);
        assert_eq!(r.matched_rule, Some(4));
    }

    #[test]
    fn minga_style_filtered_newsletter_defers_to_stage2() {
        // Filtered rule: user wants only "school closure" items from this
        // newsletter, which we can't evaluate without the LLM.
        let m = msg(
            "newsletter@minga.school",
            "This week at Minga: bake sale, spirit week, and more",
            "Lots of happenings. Unsubscribe here.",
        );
        let rules = vec![rule(
            7,
            "*@minga.school",
            Disposition::Filtered,
            "only tell me about school closures or emergencies",
        )];
        let r = run(&m, false, &rules);
        assert_eq!(r.tier, Tier::Noise);
        assert!(!r.confident, "filtered rule must defer to Stage-2");
        assert_eq!(r.matched_rule, Some(7));
        assert!(r.reason.contains('7'));
    }

    // ---- Rung 3: known contact -----------------------------------------

    #[test]
    fn known_contact_is_signal() {
        let m = msg("alice@friends.com", "dinner plans", "friday at 7?");
        let r = run(&m, true, &[]);
        assert_eq!(r.tier, Tier::Signal);
        assert!(r.confident);
        assert_eq!(r.importance, 70);
        assert_eq!(r.matched_rule, None);
    }

    // ---- Rung 4: alerts -------------------------------------------------

    #[test]
    fn ci_failure_from_automated_sender_is_signal() {
        let m = msg(
            "ci@buildbot.example.com",
            "Build failed on main",
            "The build failed. See logs.",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Signal);
        assert!(r.confident);
        assert_eq!(r.importance, 75);
        assert!(r.one_line.starts_with("Alert:"));
    }

    #[test]
    fn outage_alert() {
        let m = msg(
            "alerts@datadog.com",
            "Incident: api is down",
            "We detected an outage in production.",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Signal);
        assert!(r.confident);
    }

    #[test]
    fn alert_language_from_a_human_is_not_auto_alert() {
        // Same words, but a real person — falls through, not confident.
        let m = msg(
            "coworker@work.com",
            "the build failed again ugh",
            "can you look?",
        );
        let r = run(&m, false, &[]);
        // Not an automated sender => not the alert rung. Unknown human => falls
        // through to the ambiguous middle.
        assert_eq!(r.tier, Tier::Noise);
        assert!(!r.confident);
    }

    // ---- Rung 5: noise --------------------------------------------------

    #[test]
    fn newsletter_noise() {
        let m = msg(
            "news@substack.com",
            "The Weekly Roundup",
            "Great stuff this week. Unsubscribe | Manage your preferences",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Noise);
        assert!(r.confident);
        assert_eq!(r.importance, 15);
    }

    #[test]
    fn receipt_noise() {
        let m = msg(
            "orders@shop.com",
            "Your order #12345 has shipped",
            "Tracking number: 1Z999. Thanks for your purchase.",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Noise);
        assert!(r.confident);
    }

    #[test]
    fn cold_sales_noise() {
        let m = msg(
            "sdr@vendor.io",
            "Quick question about your stack",
            "Would love to book a call to show you a demo of our pricing.",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Noise);
        assert!(r.confident);
    }

    // ---- Rung 6: fall-through ------------------------------------------

    #[test]
    fn ambiguous_unknown_sender_defers_to_stage2() {
        let m = msg(
            "random@nowhere.org",
            "hey",
            "wanted to reach out about something",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Noise);
        assert!(!r.confident, "ambiguous mail must defer to Stage-2");
        assert_eq!(r.importance, 40);
    }

    #[test]
    fn empty_subject_gets_placeholder() {
        let m = msg("x@y.com", "", "body");
        let r = run(&m, false, &[]);
        assert_eq!(r.one_line, "(no subject)");
    }

    #[test]
    fn default_public_signature_compiles() {
        // Exercise the 3-arg public entry point (uses default config + now()).
        let m = msg("alice@friends.com", "hi", "hello");
        let _ = stage1(&m, true, &[]);
    }

    // ---- Stage-2 sealed guard (real, release-mode) ----------------------

    fn queued_row(sensitivity: Sensitivity) -> Stage2Queued {
        Stage2Queued {
            message_id: 7,
            account_id: 1,
            thread_id: "t".into(),
            from_addr: "x@y.com".into(),
            subject: "s".into(),
            body: "b".into(),
            received_at: Utc::now(),
            is_known_contact: false,
            rule_want_text: None,
            sensitivity,
        }
    }

    #[test]
    fn sealed_guard_rejects_sealed_row() {
        let row = queued_row(Sensitivity::Sealed);
        let err = stage2_sealed_guard(&row).unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)));
    }

    #[test]
    fn sealed_guard_allows_normal_row() {
        let row = queued_row(Sensitivity::Normal);
        assert!(stage2_sealed_guard(&row).is_ok());
    }

    #[test]
    fn llm_triage_guard_errors_on_sealed_in_release_semantics() {
        // The old stub used debug_assert! (compiled out in release). This must be
        // a REAL error return regardless of build profile.
        assert!(stage2_llm_triage(Sensitivity::Sealed).is_err());
        assert!(stage2_llm_triage(Sensitivity::Normal).is_ok());
    }
}
