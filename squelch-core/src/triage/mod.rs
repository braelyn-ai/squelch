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

pub use deadline::DeadlineHit;

use crate::config::Stage1Config;
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

    // ---- Rung 1: BILL / PAYMENT (highest recall) ------------------------
    if let Some(hit) = deadline::detect_bill(subject, body, now) {
        let tier = if hit.past_due {
            Tier::PastDue
        } else {
            Tier::Deadline
        };
        let amount_str = match (hit.amount, hit.currency.as_deref()) {
            (Some(a), Some(c)) => format!(" {a:.2} {c}"),
            _ => String::new(),
        };
        let one_line = if hit.past_due {
            format!("Past-due bill{amount_str}: {}", short_subject(subject))
        } else {
            format!("Bill due{amount_str}: {}", short_subject(subject))
        };
        let reason = format!(
            "bill/payment signal (kind={}, source={}, past_due={})",
            hit.kind, hit.source, hit.past_due
        );
        return Stage1Result {
            tier,
            importance: cfg.bill_importance,
            one_line,
            reason,
            deadline: Some(hit),
            matched_rule: None,
            confident: true,
        };
    }

    // ---- Rung 2: SENDER RULES (user's own local rules) ------------------
    if let Some(rule) = rules::match_sender_rule(from, rules) {
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

/// Stage-2 LLM triage. STUB in v0 — never called for sealed content.
///
/// The `debug_assert` documents the invariant: callers must not route sealed
/// messages here.
pub fn stage2_llm_triage(sensitivity: Sensitivity) -> Option<TriageResult> {
    debug_assert!(
        !matches!(sensitivity, Sensitivity::Sealed),
        "sealed content must never reach the LLM stage"
    );
    // TODO: call the model, fill importance/tier/one_line/reason. Only messages
    // with `Stage1Result.confident == false` should ever get here.
    None
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
        let m = msg(
            "billing@utilityco.com",
            "PAST DUE: Your electric bill",
            "Amount due $84.20. This payment is overdue.",
        );
        let r = run(&m, false, &[]);
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
        let m = msg(
            "invoices@acme.com",
            "Invoice #4402 from Acme",
            "Your invoice total is $1,299.00. Payment due by August 15, 2026.",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Deadline);
        assert!(r.confident);
        let d = r.deadline.unwrap();
        assert!(!d.past_due);
        assert_eq!(d.amount, Some(1299.00));
    }

    #[test]
    fn bill_wins_over_sender_rule() {
        // Even a squelch rule must not suppress a bill.
        let m = msg(
            "billing@acme.com",
            "Your invoice is ready",
            "Amount due: $10.00, due date January 1, 2026",
        );
        let rules = vec![rule(9, "*@acme.com", Disposition::Squelch, "")];
        let r = run(&m, false, &rules);
        assert!(matches!(r.tier, Tier::PastDue));
        assert_eq!(r.matched_rule, None);
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
}
