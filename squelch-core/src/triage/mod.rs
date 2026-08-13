//! Triage pipeline. The [`seal`] detector runs first at ingest, so sealed auth
//! mail never reaches [`stage1`] — the deterministic, LLM-free classifier here
//! that gives every non-sealed message a [`Tier`], importance, and one-liner.
//! Its rungs are first-match-wins and bills are checked first by design.
//! `confident == false` is the only thing that queues a row for Stage-2.

pub mod calendar;
pub mod deadline;
pub mod events;
pub mod extract;
pub mod llm;
pub mod receipt;
pub mod receipt_match;
pub mod rule_infer;
pub mod rules;
pub mod seal;
pub mod shipment;
pub mod stage1_llm;
pub mod stage2;
pub(crate) mod text;

pub use calendar::{CalendarInfo, CalendarKind, detect_calendar};
pub use deadline::DeadlineHit;
pub use receipt::{ReceiptInfo, detect_receipt};
pub use shipment::{CarrierTrack, ShipmentInfo, ShipmentStatus, detect_shipment};

use crate::config::Stage1Config;
use crate::error::CoreError;
use crate::store::{Stage1Queued, Stage2Queued};
use crate::types::{
    Disposition, FieldReasons, NewMessage, SealedKind, SenderRule, Sensitivity, Tier,
};
use chrono::{DateTime, Utc};

/// Result of the Stage-1 rules engine for a single (non-sealed) message.
/// `confident == false` enqueues the row for the Stage-2 LLM pass.
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
    /// Per-property justifications for this row's importance / deadline / tier,
    /// derived from the rung actually taken. Served HUMAN-DOOR ONLY (see
    /// [`crate::types::FieldReasons`]).
    pub field_reasons: FieldReasons,
}

/// Triage outcome for the store/upsert path. Prefer [`Stage1Result`].
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

/// Run the Stage-1 rules engine over a stored-message view with the default
/// [`Stage1Config`]. Called by the sync engine in the same transaction that
/// stores the (already non-sealed) message.
pub fn stage1(msg: &NewMessage, is_known_contact: bool, rules: &[SenderRule]) -> Stage1Result {
    stage1_with_config(
        msg,
        is_known_contact,
        rules,
        &Stage1Config::default(),
        Utc::now(),
    )
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

    // Resolved up front because rung 1 (bills, evaluated first) needs it for
    // bill-trust; rung 2 below still owns the rule's disposition tiering.
    let matched_rule = rules::match_sender_rule(from, rules);

    // ---- Rung 1: BILL / PAYMENT (highest recall) ------------------------
    if let Some(hit) = deadline::detect_bill(subject, body, now) {
        // Sender trust gates the scream: a KNOWN CONTACT, or an explicit SURFACE
        // rule (the user vouching for their own vendor), keeps full confidence
        // and the natural PastDue/Deadline tier. A bill from an UNKNOWN, unruled
        // sender must NEVER land CONFIDENT PastDue — that is the scam vector (a
        // stranger screaming "past due" for $2.4M): cap at Deadline, mark
        // not-confident, moderate importance.
        let surface_ruled = matched_rule.is_some_and(|r| r.disposition == Disposition::Surface);
        let trusted = is_known_contact || surface_ruled;

        // Sanity dampeners never raise a tier, only shave confidence. An explicit
        // SURFACE rule bypasses them (the user knows this vendor sends big, urgent
        // invoices); mere known-contact trust does not.
        let absurd_amount = hit
            .amount
            .is_some_and(|a| a > cfg.bill_absurd_amount_threshold);
        let scammy = deadline::has_scammy_phrasing(subject, body);
        let dampened = (absurd_amount || scammy) && !surface_ruled;

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
            reason.push_str(
                "; bill-like from unknown sender (capped at Deadline, deferring to Stage-2)",
            );
        }
        if dampened && absurd_amount {
            reason.push_str("; absurd amount dampener");
        }
        if dampened && scammy {
            reason.push_str("; screamy/scam phrasing dampener");
        }

        let importance_reason = if trusted {
            let src = if surface_ruled {
                "surface rule"
            } else {
                "known contact"
            };
            format!("trusted bill via {src} -> importance {importance}")
        } else {
            format!("bill from unknown, unruled sender -> capped importance {importance}")
        };
        let deadline_reason = format!(
            "bill signal '{}' matched in {} (due {}, past_due={})",
            hit.kind,
            hit.source,
            hit.due_at.format("%Y-%m-%d"),
            hit.past_due
        );
        // Say "capped" only when a cap actually BIT: a future-dated bill's natural
        // tier is already deadline, so nothing was capped.
        let tier_reason = if tier == Tier::PastDue {
            "overdue bill from trusted sender -> past_due".to_string()
        } else if !trusted {
            if hit.past_due {
                "overdue claim from untrusted (unknown, unruled) sender capped at deadline, \
                 never past_due"
                    .to_string()
            } else {
                "future-dated bill from untrusted (unknown, unruled) sender -> deadline".to_string()
            }
        } else if dampened {
            if hit.past_due {
                "trusted overdue bill dampened (absurd amount / scam phrasing) -> capped at deadline"
                    .to_string()
            } else {
                "trusted bill dampened (absurd amount / scam phrasing) -> deadline".to_string()
            }
        } else {
            "future-dated bill -> deadline".to_string()
        };

        return Stage1Result {
            tier,
            importance,
            one_line,
            reason,
            deadline: Some(hit),
            matched_rule: None,
            confident,
            field_reasons: FieldReasons {
                importance: Some(importance_reason),
                deadline: Some(deadline_reason),
                tier: Some(tier_reason),
            },
        };
    }

    // ---- Rung 2: SENDER RULES (user's own local rules) ------------------
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
                field_reasons: FieldReasons {
                    importance: Some(format!(
                        "sender rule #{} (squelch) -> noise importance {}",
                        rule.id, cfg.rule_squelch_importance
                    )),
                    deadline: None,
                    tier: Some(format!("squelch rule #{} -> noise", rule.id)),
                },
            },
            Disposition::Surface => Stage1Result {
                tier: Tier::Signal,
                importance: cfg.rule_surface_importance,
                one_line: short_subject(subject),
                reason: format!("matched surface rule #{} ({})", rule.id, rule.match_pattern),
                deadline: None,
                matched_rule: Some(rule.id),
                confident: true,
                field_reasons: FieldReasons {
                    importance: Some(format!(
                        "sender rule #{} (surface) -> signal importance {}",
                        rule.id, cfg.rule_surface_importance
                    )),
                    deadline: None,
                    tier: Some(format!("surface rule #{} -> signal", rule.id)),
                },
            },
            // Filtered: `want_text` is the account owner's own standing
            // instruction for this sender, written in whatever polarity they
            // like — mail they DO want ("only school closures") or mail they do
            // NOT care about ("i dont care about the approval emails"). Either
            // way it is a natural-language predicate we cannot evaluate without
            // the LLM. Park in Noise, NOT confident, so Stage-2 picks it up.
            //
            // Stage-2 injects the verbatim `want_text` into its TRUSTED CONTEXT
            // block (owner- or agent-authored through an audited door; never
            // email body content) and decides surface-vs-squelch against it; see
            // [`stage2::build_user_message`] and `stage2::apply_result`.
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
                field_reasons: FieldReasons {
                    importance: Some(format!(
                        "filtered rule #{} -> provisional noise importance {} pending Stage-2 want_text eval",
                        rule.id, cfg.rule_filtered_importance
                    )),
                    deadline: None,
                    tier: Some(format!(
                        "filtered rule #{} -> noise (deferred to Stage-2)",
                        rule.id
                    )),
                },
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
            field_reasons: FieldReasons {
                importance: Some(format!(
                    "known contact (appears in your Sent mail) -> signal importance {}",
                    cfg.known_contact_importance
                )),
                deadline: None,
                tier: Some("known contact -> signal".to_string()),
            },
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
            field_reasons: FieldReasons {
                importance: Some(format!(
                    "automated alert (build/outage/incident language) -> signal importance {}",
                    cfg.alert_importance
                )),
                deadline: None,
                tier: Some("automated alert -> signal".to_string()),
            },
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
        return noise(
            cfg,
            subject,
            "cold-outbound / sales language from unknown sender",
        );
    }

    // ---- Rung 6: FALL-THROUGH (the ambiguous middle) --------------------
    Stage1Result {
        tier: Tier::Noise,
        importance: cfg.fallthrough_importance,
        one_line: short_subject(subject),
        reason: "no Stage-1 rule matched — ambiguous, deferring to Stage-2".to_string(),
        deadline: None,
        matched_rule: None,
        confident: false,
        field_reasons: FieldReasons {
            importance: Some(format!(
                "unknown sender fell through to ambiguous band -> provisional importance {} pending Stage-2",
                cfg.fallthrough_importance
            )),
            deadline: None,
            tier: Some("ambiguous fall-through -> noise (deferred to Stage-2)".to_string()),
        },
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
        field_reasons: FieldReasons {
            importance: Some(format!(
                "{reason} -> noise importance {}",
                cfg.noise_importance
            )),
            deadline: None,
            tier: Some(format!("{reason} -> noise")),
        },
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

/// The first sender rule matching `from_addr`, plus its disposition. Prefer
/// [`rules::match_sender_rule`].
pub fn apply_sender_rules<'a>(
    from_addr: &str,
    rules: &'a [SenderRule],
) -> Option<(&'a SenderRule, Disposition)> {
    rules::match_sender_rule(from_addr, rules).map(|r| (r, r.disposition))
}

/// Sealed rows must never reach the Stage-2 LLM path — a real release-mode check
/// behind the queue predicate's SQL exclusion. Call it on every queued row right
/// before building the API request; the error is redacted to the message id.
/// See docs/SECURITY.md.
pub fn stage2_sealed_guard(row: &Stage2Queued) -> crate::error::Result<()> {
    if matches!(row.sensitivity, Sensitivity::Sealed) {
        return Err(CoreError::InvalidInput(format!(
            "stage-2 sealed guard: message {} is sealed and must never reach the LLM",
            row.message_id
        )));
    }
    Ok(())
}

/// Sealed rows must never reach the Stage-1 LLM path — a real release-mode check
/// behind the queue predicate's `sensitivity='normal'` exclusion. Call it on every
/// queued row right before building the API request. See docs/SECURITY.md.
pub fn stage1_sealed_guard(row: &Stage1Queued) -> crate::error::Result<()> {
    if matches!(row.sensitivity, Sensitivity::Sealed) {
        return Err(CoreError::InvalidInput(format!(
            "stage-1 sealed guard: message {} is sealed and must never reach the LLM",
            row.message_id
        )));
    }
    Ok(())
}

/// Sealed-guard entry for callers that only have a [`Sensitivity`]: a real
/// release-mode check that errors on sealed input. See docs/SECURITY.md.
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
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
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
        // A confident PastDue requires a trusted (known-contact) sender.
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
        // Known sender => confident; unknown senders defer.
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
        // A squelch rule must not suppress a bill; the known sender keeps PastDue.
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

    // ---- Sender-trust-gated bills ---------------------------------------

    #[test]
    fn pge_style_past_due_from_known_sender_still_screams() {
        // A legit past-due from a known biller must land CONFIDENT PastDue.
        let m = msg(
            "customerservice@pge.com",
            "Your PG&E bill is past due",
            "Your account is past due. Amount due $214.77. Please pay to avoid \
             service interruption.",
        );
        let r = run(&m, true, &[]);
        assert_eq!(r.tier, Tier::PastDue, "known-sender past-due must scream");
        assert!(
            r.confident,
            "known-sender past-due must be final, not Stage-2"
        );
        assert_eq!(r.importance, 95);
        let d = r.deadline.unwrap();
        assert!(d.past_due);
        assert_eq!(d.amount, Some(214.77));
    }

    #[test]
    fn scam_past_due_from_unknown_sender_is_not_confident_past_due() {
        // A scam "past due" from an unknown sender must be Deadline at most and
        // NOT confident, however large the parsed amount.
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
        // Large amount + scam phrasing + a Surface rule => CONFIDENT PastDue with
        // BOTH dampeners bypassed: the user vouched for this vendor.
        let m = msg(
            "info@sfmail.corpnet.com",
            "You're Past Due — Penalties Are Adding Up",
            "Your balance of $2,470,000 is past due. Penalties are adding up. \
             Pay now to avoid further action!!!",
        );
        let rules = vec![rule(11, "*@sfmail.corpnet.com", Disposition::Surface, "")];
        let r = run(&m, false, &rules);
        assert_eq!(
            r.tier,
            Tier::PastDue,
            "surface-ruled sender screams past-due"
        );
        assert!(r.confident, "surface-ruled bill is final, not Stage-2");
        assert_eq!(r.importance, 95, "full bill importance for trusted sender");
        // Dampeners are BYPASSED: their notes must NOT appear.
        assert!(
            !r.reason.contains("absurd amount"),
            "absurd dampener bypassed"
        );
        assert!(
            !r.reason.contains("screamy/scam"),
            "scammy dampener bypassed"
        );
        assert!(r.reason.contains("surface rule"), "names the trust source");
        // The bill rung wins over rung-2 rule tiering, so no matched_rule id here.
        assert_eq!(r.matched_rule, None);
    }

    #[test]
    fn corpnet_past_due_without_rule_is_still_dampened_regression() {
        // Same email, no Surface rule, unknown sender: capped at Deadline, not
        // confident, both dampeners fire.
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
        // A Squelch rule confers no bill-trust, so an unknown-sender past-due stays
        // capped and deferred; rung 1 still wins over the squelch rule.
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
        // Clean past-due from an unknown sender WITH a Surface rule: confident
        // PastDue, and the reason names the trust source.
        let m = msg(
            "billing@myvendor.example",
            "Invoice past due",
            "Your invoice for $120.00 is past due.",
        );
        let rules = vec![rule(
            13,
            "billing@myvendor.example",
            Disposition::Surface,
            "",
        )];
        let r = run(&m, false, &rules);
        assert_eq!(r.tier, Tier::PastDue);
        assert!(r.confident);
        assert_eq!(r.importance, 95);
        assert!(r.reason.contains("surface rule #13"));
    }

    #[test]
    fn absurd_amount_dampens_even_known_sender() {
        // An absurd amount caps and defers even a trusted known-sender bill.
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
        // Filtered rule: the want_text can't be evaluated without the LLM.
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
        // Exercise the 3-arg public entry point (default config + now()).
        let m = msg("alice@friends.com", "hi", "hello");
        let _ = stage1(&m, true, &[]);
    }

    // ---- Per-property field_reasons (branch-accurate) -------------------

    #[test]
    fn field_reasons_for_known_contact_branch() {
        let m = msg("alice@friends.com", "dinner", "friday?");
        let r = run(&m, true, &[]);
        let fr = &r.field_reasons;
        let imp = fr.importance.as_deref().unwrap();
        assert!(imp.contains("known contact"), "importance reason: {imp}");
        assert!(imp.contains("70"), "states the actual importance: {imp}");
        assert_eq!(fr.tier.as_deref(), Some("known contact -> signal"));
        // A known-contact row carries no deadline, so no deadline reason.
        assert!(fr.deadline.is_none());
    }

    #[test]
    fn field_reasons_for_sender_rule_branch() {
        let m = msg("promo@shop.com", "50% off", "buy");
        let rules = vec![rule(4, "*@shop.com", Disposition::Squelch, "")];
        let r = run(&m, false, &rules);
        let fr = &r.field_reasons;
        let imp = fr.importance.as_deref().unwrap();
        assert!(imp.contains("sender rule #4"), "names the rule id: {imp}");
        assert!(imp.contains("squelch"), "names the disposition: {imp}");
        assert_eq!(fr.tier.as_deref(), Some("squelch rule #4 -> noise"));
        assert!(fr.deadline.is_none());
    }

    #[test]
    fn field_reasons_for_deadline_hit_branch() {
        // Known-sender past-due bill: all three reasons describe the STORED values.
        let m = msg(
            "billing@utilityco.com",
            "PAST DUE: Your electric bill",
            "Amount due $84.20. This payment is overdue.",
        );
        let r = run(&m, true, &[]);
        assert_eq!(r.tier, Tier::PastDue);
        let fr = &r.field_reasons;
        let dl = fr.deadline.as_deref().expect("deadline reason present");
        assert!(
            dl.contains("past_due=true"),
            "deadline reason states past-due: {dl}"
        );
        assert!(
            dl.contains("bill signal"),
            "deadline reason names the matched signal: {dl}"
        );
        let tier = fr.tier.as_deref().unwrap();
        assert!(
            tier.contains("past_due"),
            "tier reason states the stored tier: {tier}"
        );
        let imp = fr.importance.as_deref().unwrap();
        assert!(imp.contains("trusted bill"), "importance reason: {imp}");
    }

    #[test]
    fn field_reasons_for_untrusted_bill_states_the_cap() {
        // The tier reason must describe the WINNING value (capped at deadline),
        // never the discarded past_due.
        let m = msg(
            "billing@unknown-vendor.example",
            "Invoice past due",
            "Your invoice for $120.00 is past due.",
        );
        let r = run(&m, false, &[]);
        assert_eq!(r.tier, Tier::Deadline);
        let tier = r.field_reasons.tier.as_deref().unwrap();
        assert!(tier.contains("capped at deadline"), "tier reason: {tier}");
        assert!(
            !tier.contains("-> past_due"),
            "must not derive a discarded past_due: {tier}"
        );
    }

    #[test]
    fn field_reasons_for_ambiguous_fallthrough_branch() {
        let m = msg("random@nowhere.org", "hey", "reaching out about something");
        let r = run(&m, false, &[]);
        assert!(!r.confident);
        let fr = &r.field_reasons;
        let imp = fr.importance.as_deref().unwrap();
        assert!(imp.contains("fell through"), "importance reason: {imp}");
        assert!(imp.contains("Stage-2"), "notes the deferral: {imp}");
        assert!(fr.tier.as_deref().unwrap().contains("ambiguous"));
        assert!(fr.deadline.is_none());
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

    fn stage1_queued_row(sensitivity: Sensitivity) -> Stage1Queued {
        Stage1Queued {
            message_id: 7,
            account_id: 1,
            thread_id: "t".into(),
            from_addr: "x@y.com".into(),
            subject: "s".into(),
            body: "b".into(),
            received_at: Utc::now(),
            is_known_contact: false,
            sensitivity,
        }
    }

    #[test]
    fn stage1_sealed_guard_rejects_sealed_allows_normal() {
        let sealed = stage1_queued_row(Sensitivity::Sealed);
        assert!(matches!(
            stage1_sealed_guard(&sealed).unwrap_err(),
            CoreError::InvalidInput(_)
        ));
        let normal = stage1_queued_row(Sensitivity::Normal);
        assert!(stage1_sealed_guard(&normal).is_ok());
    }

    #[test]
    fn llm_triage_guard_errors_on_sealed_in_release_semantics() {
        // Must be a REAL error return regardless of build profile.
        assert!(stage2_llm_triage(Sensitivity::Sealed).is_err());
        assert!(stage2_llm_triage(Sensitivity::Normal).is_ok());
    }
}
