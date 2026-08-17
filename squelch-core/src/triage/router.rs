//! The escalation router: what decides that a row deserves a second, harder
//! look. It is a PURE function of the model's verdict, the row's facts, and what
//! the deterministic detectors saw — never of the model's opinion about itself.
//!
//! The rule this module exists to enforce: **the classifier does not route
//! itself.** A model asked "are you sure?" answers from the same place it
//! produced the verdict, so it is confidently wrong in exactly the cases that
//! need a second look — the novel, the ambiguous, the adversarial. Self-report
//! survives here as ONE arm ([`EscalationReason::ModelUnsure`]) and it is
//! evaluated LAST, which makes it strictly additive: it can buy an escalation
//! nothing else caught, and it can never talk the router out of one.
//!
//! The deterministic engine in [`super`] is the other half of this. It no longer
//! decides anything on its own (see the pass in [`crate::sync`]), but it still
//! runs, and here it is a WITNESS: when the regexes and the model materially
//! disagree, that disagreement is free, honest evidence that nobody had to
//! self-assess to produce. A buried bill and a surfaced scam are both caught by
//! disagreement, and neither depends on the model volunteering doubt.

use crate::triage::deadline;
use crate::types::Tier;
use chrono::{DateTime, Utc};

/// Why a row escalated. Ordered by stakes, highest first: [`should_escalate`]
/// returns the FIRST arm that fires, so the recorded reason names the worst
/// thing that could be wrong with the row rather than the first thing noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationReason {
    /// The deterministic bill detector fired and the model filed the row with no
    /// obligation at all. Burying a real bill is the worst mistake this product
    /// can make, so a disagreement in this direction always escalates.
    BuriedBill,
    /// The model put an UNKNOWN, unvouched sender in the standing band. This is
    /// the stranger-screaming-PAST-DUE vector; the ingest-time engine has capped
    /// it since day one, and the model path must not be the way around that cap.
    UnverifiedUrgency,
    /// Scam-shaped phrasing, and the model surfaced the row anyway.
    ScamShape,
    /// The model claims routine mail reports a real problem (fraud flag, bounced
    /// payment, lost package). That claim lifts a row out of its rail and into
    /// the attention bands, which is too big a move to take on one cheap look.
    Exception,
    /// A bill that needs paying. Money with an action attached is worth the
    /// second call on its own.
    Invoice,
    /// The account owner has corrected triage for this sender before. Their
    /// correction is the strongest evidence in the system that this sender is
    /// one we get wrong.
    SenderCorrected,
    /// Importance landed within a hair of the surface threshold. A point either
    /// way flips whether the user ever sees the row, so boundary rows are where
    /// a scoring error costs the most and confidence is worth the least.
    Boundary,
    /// The model volunteered that it was unsure. Evaluated last, on purpose:
    /// this arm may only ADD escalations.
    ModelUnsure,
}

impl EscalationReason {
    /// A short stable slug, stored on the row and emitted in metrics so the
    /// escalation mix is inspectable without re-deriving it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuriedBill => "buried_bill",
            Self::UnverifiedUrgency => "unverified_urgency",
            Self::ScamShape => "scam_shape",
            Self::Exception => "exception",
            Self::Invoice => "invoice",
            Self::SenderCorrected => "sender_corrected",
            Self::Boundary => "boundary",
            Self::ModelUnsure => "model_unsure",
        }
    }

    /// A human-readable clause for the row's stored reason text, phrased as what
    /// is being CHECKED rather than what is wrong: an escalation is a second
    /// look, not a verdict, and the copy should not prejudge the answer.
    pub fn detail(self) -> &'static str {
        match self {
            Self::BuriedBill => {
                "the bill detector found a payment obligation this verdict does not account for"
            }
            Self::UnverifiedUrgency => {
                "an unknown sender was placed in the standing band on their own say-so"
            }
            Self::ScamShape => "scam-shaped phrasing on a row that was surfaced anyway",
            Self::Exception => "a routine-genre row claiming a real problem",
            Self::Invoice => "a bill that needs paying",
            Self::SenderCorrected => "the account owner has corrected this sender before",
            Self::Boundary => "the score landed on the line between surfaced and buried",
            Self::ModelUnsure => "the first pass reported low confidence",
        }
    }
}

/// Everything [`should_escalate`] is allowed to look at. Deliberately concrete:
/// no store handle, no clock of its own, nothing that would make the routing
/// decision untestable or order-dependent.
pub struct EscalationInput<'a> {
    /// The STORED importance, after the known-contact floor and clamps.
    pub importance: u8,
    /// The STORED tier, after [`super::stage2::derive_deadline_and_tier`] and
    /// the record clamp — the tier the user would actually see.
    pub tier: Tier,
    /// The normalized routing category.
    pub category: &'a str,
    /// The model's `exception` flag.
    pub exception: bool,
    /// The model's `confident` flag, INVERTED. The only self-report here.
    pub model_says_unsure: bool,
    /// Whether the verdict carries a deadline.
    pub has_deadline: bool,
    pub subject: &'a str,
    pub body: &'a str,
    /// Sent-derived contact: someone the account owner has written to.
    pub is_known_contact: bool,
    /// The account owner has previously corrected a triage verdict for this
    /// sender address.
    pub sender_corrected: bool,
}

/// Router tunables. Separate from the stage configs because these describe the
/// ROUTING policy, not either stage's own budget.
#[derive(Debug, Clone, Copy)]
pub struct RouterConfig {
    /// The importance at or above which a row surfaces — the same number
    /// [`crate::config::NotifyConfig::min_importance`] uses, threaded in so
    /// "the line" means one thing in this codebase.
    pub surface_threshold: u8,
    /// How close to `surface_threshold` counts as a boundary row.
    pub boundary_margin: u8,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            surface_threshold: 50,
            boundary_margin: 10,
        }
    }
}

/// Decide whether a Stage-1 verdict needs the escalated pass. `None` means the
/// verdict stands.
///
/// Arms are evaluated in [`EscalationReason`] order and the first hit wins, so
/// the reason recorded on the row is the highest-stakes thing in play. `now` is
/// injected for the bill detector's past-due math.
pub fn should_escalate(
    inp: &EscalationInput<'_>,
    cfg: &RouterConfig,
    now: DateTime<Utc>,
) -> Option<EscalationReason> {
    let standing = matches!(inp.tier, Tier::PastDue | Tier::Deadline);
    let surfaced = standing || inp.importance >= cfg.surface_threshold;

    // 1. DISAGREEMENT, in the direction that costs the most. The detector is
    //    tuned for recall and fires on plenty of mail that is not really a bill,
    //    which is exactly why it is a witness and not a decider — but a verdict
    //    that files a detector-flagged payment obligation as having no
    //    obligation at all is a disagreement worth a real look.
    if !inp.has_deadline && !standing && deadline::detect_bill(inp.subject, inp.body, now).is_some()
    {
        return Some(EscalationReason::BuriedBill);
    }

    // 2. The scam vector, closed on the model path the same way the ingest-time
    //    engine closes it on the heuristic path.
    if standing && !inp.is_known_contact {
        return Some(EscalationReason::UnverifiedUrgency);
    }

    // 3. Disagreement in the other direction: the detectors smell a scam and the
    //    verdict put it in front of the user anyway.
    if surfaced && deadline::has_scammy_phrasing(inp.subject, inp.body) {
        return Some(EscalationReason::ScamShape);
    }

    if inp.exception {
        return Some(EscalationReason::Exception);
    }

    if inp.category == "invoice" {
        return Some(EscalationReason::Invoice);
    }

    if inp.sender_corrected {
        return Some(EscalationReason::SenderCorrected);
    }

    // 7. Boundary rows. Saturating on both sides so a threshold near 0 or 100
    //    cannot wrap the window around.
    let lo = cfg.surface_threshold.saturating_sub(cfg.boundary_margin);
    let hi = cfg.surface_threshold.saturating_add(cfg.boundary_margin);
    if inp.importance >= lo && inp.importance <= hi {
        return Some(EscalationReason::Boundary);
    }

    // 8. LAST: the model's own doubt. Reaching this line means nothing
    //    structural fired, so this arm only ever adds.
    if inp.model_says_unsure {
        return Some(EscalationReason::ModelUnsure);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap()
    }

    /// A row nothing should fire on: settled noise, well clear of the line.
    fn calm<'a>() -> EscalationInput<'a> {
        EscalationInput {
            importance: 10,
            tier: Tier::Noise,
            category: "marketing",
            exception: false,
            model_says_unsure: false,
            has_deadline: false,
            subject: "Weekend picks",
            body: "Here are some things happening this weekend.",
            is_known_contact: false,
            sender_corrected: false,
        }
    }

    fn cfg() -> RouterConfig {
        RouterConfig::default()
    }

    #[test]
    fn a_settled_verdict_does_not_escalate() {
        assert_eq!(should_escalate(&calm(), &cfg(), now()), None);
    }

    /// The headline case: the detector sees a bill, the model files it as
    /// nothing. Burying a real bill is the worst outcome, so this wins over
    /// every other arm.
    #[test]
    fn a_bill_the_model_did_not_see_escalates_first() {
        let mut inp = calm();
        inp.subject = "Invoice 4471 - amount due $240.00 by August 30";
        inp.body = "Your invoice is attached. Payment due August 30, 2026.";
        assert_eq!(
            should_escalate(&inp, &cfg(), now()),
            Some(EscalationReason::BuriedBill)
        );
    }

    /// The same row, once the model HAS accounted for the obligation, is not a
    /// disagreement any more and must not burn a second call.
    #[test]
    fn a_bill_the_model_did_see_is_not_a_disagreement() {
        let mut inp = calm();
        inp.subject = "Invoice 4471 - amount due $240.00 by August 30";
        inp.body = "Your invoice is attached. Payment due August 30, 2026.";
        inp.has_deadline = true;
        inp.is_known_contact = true;
        inp.tier = Tier::Deadline;
        // Escalates as Invoice-category money if categorized so, but NOT as a
        // buried bill: with `general` here, nothing structural remains.
        assert_ne!(
            should_escalate(&inp, &cfg(), now()),
            Some(EscalationReason::BuriedBill)
        );
    }

    /// A stranger cannot talk their way into the standing band on one cheap look.
    #[test]
    fn an_unknown_sender_in_the_standing_band_escalates() {
        let mut inp = calm();
        inp.tier = Tier::Deadline;
        inp.has_deadline = true;
        inp.importance = 90;
        assert_eq!(
            should_escalate(&inp, &cfg(), now()),
            Some(EscalationReason::UnverifiedUrgency)
        );
    }

    /// ...and a known contact in the same band does not, since the trust that
    /// gates it is present.
    #[test]
    fn a_known_contact_in_the_standing_band_does_not_escalate_for_urgency() {
        let mut inp = calm();
        inp.tier = Tier::Deadline;
        inp.has_deadline = true;
        inp.importance = 90;
        inp.is_known_contact = true;
        assert_ne!(
            should_escalate(&inp, &cfg(), now()),
            Some(EscalationReason::UnverifiedUrgency)
        );
    }

    #[test]
    fn an_exception_claim_escalates() {
        let mut inp = calm();
        inp.exception = true;
        assert_eq!(
            should_escalate(&inp, &cfg(), now()),
            Some(EscalationReason::Exception)
        );
    }

    #[test]
    fn a_corrected_sender_escalates() {
        let mut inp = calm();
        inp.sender_corrected = true;
        assert_eq!(
            should_escalate(&inp, &cfg(), now()),
            Some(EscalationReason::SenderCorrected)
        );
    }

    /// Either side of the line, within the margin.
    #[test]
    fn scores_on_the_line_escalate_from_both_sides() {
        for importance in [40u8, 50, 60] {
            let mut inp = calm();
            inp.importance = importance;
            assert_eq!(
                should_escalate(&inp, &cfg(), now()),
                Some(EscalationReason::Boundary),
                "importance {importance} sits on the line"
            );
        }
        // Clear of the margin on both sides.
        for importance in [10u8, 39, 61, 95] {
            let mut inp = calm();
            inp.importance = importance;
            // 95 is above the surface line, so only assert the boundary arm.
            assert_ne!(
                should_escalate(&inp, &cfg(), now()),
                Some(EscalationReason::Boundary),
                "importance {importance} is clear of the line"
            );
        }
    }

    /// THE POINT OF THE MODULE: self-report is additive only. It buys an
    /// escalation when nothing else fires...
    #[test]
    fn model_doubt_alone_still_escalates() {
        let mut inp = calm();
        inp.model_says_unsure = true;
        assert_eq!(
            should_escalate(&inp, &cfg(), now()),
            Some(EscalationReason::ModelUnsure)
        );
    }

    /// One structural arm's setup: a label and the mutation that trips it.
    type Arm = (&'static str, Box<dyn Fn(&mut EscalationInput<'static>)>);

    /// ...and confidence CANNOT buy its way out of one. This is the regression
    /// guard for the whole redesign: every structural arm must still fire on a
    /// row the model swears it is sure about.
    #[test]
    fn a_confident_model_cannot_suppress_a_structural_escalation() {
        let structural: Vec<Arm> = vec![
            (
                "buried bill",
                Box::new(|i: &mut EscalationInput<'_>| {
                    i.subject = "Invoice 4471 - amount due $240.00 by August 30";
                    i.body = "Payment due August 30, 2026.";
                }),
            ),
            (
                "unverified urgency",
                Box::new(|i: &mut EscalationInput<'_>| {
                    i.tier = Tier::Deadline;
                    i.has_deadline = true;
                }),
            ),
            (
                "exception",
                Box::new(|i: &mut EscalationInput<'_>| i.exception = true),
            ),
            (
                "invoice",
                Box::new(|i: &mut EscalationInput<'_>| i.category = "invoice"),
            ),
            (
                "corrected sender",
                Box::new(|i: &mut EscalationInput<'_>| i.sender_corrected = true),
            ),
            (
                "boundary",
                Box::new(|i: &mut EscalationInput<'_>| i.importance = 50),
            ),
        ];
        for (label, mutate) in structural {
            let mut inp = calm();
            mutate(&mut inp);
            // The model is maximally sure of itself.
            inp.model_says_unsure = false;
            let verdict = should_escalate(&inp, &cfg(), now());
            assert!(
                verdict.is_some() && verdict != Some(EscalationReason::ModelUnsure),
                "{label}: a confident model must not suppress a structural escalation"
            );
        }
    }
}
