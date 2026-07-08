//! Triage pipeline.
//!
//! Stage 1: [`seal`] detector (implemented) — seals auth mail before anything
//!          else can see it.
//! Rule stage: apply local sender rules (stub).
//! Stage 2: LLM ranking (stub in v0; ALWAYS skipped for sealed content).

pub mod seal;

use crate::types::{Disposition, SealedKind, SenderRule, Sensitivity, Tier};

/// Outcome of triaging a single message.
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

/// Rule-stage outcome for a message given the account's sender rules.
///
/// STUB: v0 does a naive substring match of `match_pattern` against the sender
/// address. A real impl would support globs/regex and richer matching. Returns
/// the first matching rule's disposition + id, if any.
pub fn apply_sender_rules<'a>(
    from_addr: &str,
    rules: &'a [SenderRule],
) -> Option<(&'a SenderRule, Disposition)> {
    let from_lower = from_addr.to_ascii_lowercase();
    for rule in rules {
        // TODO: proper glob/regex matching. v0: strip a leading '*' and
        // substring-match the remainder.
        let needle = rule.match_pattern.trim_start_matches('*').to_ascii_lowercase();
        if !needle.is_empty() && from_lower.contains(&needle) {
            return Some((rule, rule.disposition));
        }
    }
    None
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
    // TODO: call the model, fill importance/tier/one_line/reason.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rule(pattern: &str, disp: Disposition) -> SenderRule {
        SenderRule {
            id: 1,
            account_id: 1,
            match_pattern: pattern.to_string(),
            want_text: String::new(),
            disposition: disp,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn rule_stage_substring_match() {
        let rules = vec![rule("*@newsletter.com", Disposition::Squelch)];
        let hit = apply_sender_rules("promo@newsletter.com", &rules);
        assert!(matches!(hit, Some((_, Disposition::Squelch))));
        assert!(apply_sender_rules("alice@example.com", &rules).is_none());
    }
}
