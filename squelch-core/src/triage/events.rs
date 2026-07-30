//! The notification-emission decision: is THIS triage verdict worth waking the
//! user for, and if so, what does the durable `events` row look like?
//!
//! Deliberately PURE and store-free. The sync engine owns the call sites (it is
//! the only layer that knows which sync path it is on), the store owns the
//! append; this module owns nothing but the judgment, so the whole policy is
//! unit-testable without a database or a Gmail account.
//!
//! ## The rules
//!
//! A verdict is worthy iff ALL of:
//!   * sensitivity is `Normal` — sealed mail is NEVER notified. It would be at
//!     most a contentless ping, and an OTP surfacing on a lock screen would undo
//!     the entire seal design, so v1 skips it outright. This check is redundant
//!     (sealed rows are noise-tier/importance-0 and never see Stage-1) and is
//!     here anyway: a seal invariant should hold for two independent reasons.
//!   * it is not the user's own Sent mail.
//!   * no `Squelch`/`Filtered` sender rule decided it — a rule is the user
//!     telling us in advance that this sender does not get to interrupt them.
//!     (`Filtered` is included on the conservative side: its `want_text` is
//!     evaluated by Stage-2 for the CLIENT surfaces, but a sender the user
//!     bothered to filter should not push to their phone.)
//!   * AND it is urgent-tier, or carries a deadline, or scores at/above
//!     `notify.min_importance`.
//!   * AND the mail is FRESH (see [`is_fresh`]).
//!
//! The kind precedence is `urgent` > `deadline` > `surfaced`, matching the order
//! of the three disjuncts above.

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::config::NotifyConfig;
use crate::store::{NewEvent, TriagedMessage};
use crate::triage::DeadlineHit;
use crate::types::{AccountId, Disposition, EventKind, SenderRule, Sensitivity, Tier};

/// Every fact the emission decision needs about one triaged message, gathered at
/// whichever verdict site produced it (ingest heuristic, Stage-1 apply, Stage-2
/// apply). Borrowed, so no site pays for a clone to ask the question.
#[derive(Debug, Clone, Copy)]
pub struct EventContext<'a> {
    pub account_id: AccountId,
    pub message_id: i64,
    pub thread_id: &'a str,
    /// The sender as the client lists render it — `from_addr`, matching
    /// [`crate::types::Update::sender`].
    pub sender: &'a str,
    pub one_line: &'a str,
    pub received_at: DateTime<Utc>,
    pub sensitivity: Sensitivity,
    /// `true` for the user's own outbox.
    pub is_sent: bool,
    /// The disposition of the sender rule that decided this row, when one fired.
    pub rule: Option<Disposition>,
    pub tier: Tier,
    pub importance: u8,
    pub deadline: Option<&'a DeadlineHit>,
}

/// How far AHEAD of our clock a message may be dated and still count as fresh.
/// Only a tolerance for a wrong sender clock or a mislabeled timezone — never a
/// licence to notify. One hour is roomy enough for the broken-MUA cases and far
/// too narrow to guess a future sync's processing time within.
const MAX_FUTURE_SKEW_SECS: i64 = 3600;

/// Whether `received_at` is inside the configured freshness window relative to
/// `now`. THE STORM GUARD — see [`NotifyConfig::freshness_window_secs`].
///
/// BOUNDED ON BOTH SIDES, because `received_at` is SENDER-CONTROLLED: ingest
/// prefers the RFC822 `Date:` header over Gmail's `internalDate`, and dating
/// mail in the future is a standing spam trick for winning inbox sort order.
/// An unbounded upper edge would mean months-old mail dated 2030 counts as
/// "fresh" forever — so the backlog grind on a fresh install (Stage-1/Stage-2
/// walk the queues `received_at DESC`, i.e. future-dated rows FIRST) and
/// `catch_up()`'s whole-window re-scan would push it, defeating the "never on
/// initial backfill" guarantee this window exists to provide.
pub fn is_fresh(received_at: DateTime<Utc>, cfg: &NotifyConfig, now: DateTime<Utc>) -> bool {
    let floor = now - ChronoDuration::seconds(cfg.freshness_window_secs as i64);
    let ceiling = now + ChronoDuration::seconds(MAX_FUTURE_SKEW_SECS);
    received_at >= floor && received_at <= ceiling
}

/// THE decision. `Some(kind)` when this verdict earns a notification, `None`
/// otherwise. Pure — no store, no clock beyond the injected `now`.
pub fn worthy_kind(
    ctx: &EventContext<'_>,
    cfg: &NotifyConfig,
    now: DateTime<Utc>,
) -> Option<EventKind> {
    // SEAL INVARIANT (belt and braces — see the module header).
    if ctx.sensitivity != Sensitivity::Normal {
        return None;
    }
    // The user's own outbox never notifies the user.
    if ctx.is_sent {
        return None;
    }
    // A squelch/filtered rule is a standing "not from this sender".
    if matches!(ctx.rule, Some(Disposition::Squelch) | Some(Disposition::Filtered)) {
        return None;
    }
    // STORM GUARD: old mail is silent no matter how good the verdict is.
    if !is_fresh(ctx.received_at, cfg, now) {
        return None;
    }

    // Precedence: urgent > deadline > surfaced.
    if matches!(ctx.tier, Tier::PastDue | Tier::Deadline) {
        return Some(EventKind::Urgent);
    }
    if ctx.deadline.is_some() {
        return Some(EventKind::Deadline);
    }
    if ctx.importance >= cfg.min_importance {
        return Some(EventKind::Surfaced);
    }
    None
}

/// [`worthy_kind`] plus the denormalized snapshot the `events` row stores. This
/// is what the sync engine hands to [`crate::store::Store::append_event`].
pub fn event_for(
    ctx: &EventContext<'_>,
    cfg: &NotifyConfig,
    now: DateTime<Utc>,
) -> Option<NewEvent> {
    let kind = worthy_kind(ctx, cfg, now)?;
    Some(NewEvent {
        account_id: ctx.account_id,
        message_id: ctx.message_id,
        thread_id: ctx.thread_id.to_string(),
        kind,
        tier: ctx.tier,
        importance: ctx.importance,
        sender: ctx.sender.to_string(),
        one_line: ctx.one_line.to_string(),
        deadline: ctx.deadline.map(|d| d.due_at.to_rfc3339()),
    })
}

/// Gather the [`EventContext`] for an INGEST-path verdict (the heuristic seed,
/// in the same tick that wrote the message row).
///
/// `rules` is the account's rule list as loaded for the ingest batch; it is what
/// resolves `matched_rule`'s id back to a disposition.
///
/// NOTE the caller's other obligation: only a CONFIDENT seed may emit. A
/// non-confident seed is a guess, and guessing is not grounds for waking anyone
/// — those rows wait for Stage-1/Stage-2 to refine them and emit from those call
/// sites instead (under the same freshness window). Confidence is not folded in
/// here because it is a property of the INGEST path only: a refined verdict is
/// final whatever the seed thought.
pub fn ingest_context<'a>(
    triaged: &'a TriagedMessage,
    message_id: i64,
    rules: &[SenderRule],
) -> EventContext<'a> {
    let rule = triaged
        .matched_rule
        .and_then(|id| rules.iter().find(|r| r.id == id))
        .map(|r| r.disposition);
    EventContext {
        account_id: triaged.message.account_id,
        message_id,
        thread_id: &triaged.message.thread_id,
        sender: &triaged.message.from_addr,
        one_line: &triaged.one_line,
        received_at: triaged.message.received_at,
        sensitivity: triaged.sensitivity,
        is_sent: triaged.message.is_sent,
        rule,
        tier: triaged.tier,
        importance: triaged.importance,
        deadline: triaged.deadline.as_ref(),
    }
}

/// The rule disposition for a REFINE-site context (Stage-1 / Stage-2 apply):
/// the sender's rule as the list stands NOW, not as it stood at ingest.
///
/// Refine sites cannot read the rule off the triage row. A rule that existed at
/// ingest stamps `stage1_model_used='rule'` and keeps the row out of the queues
/// entirely; a rule the user adds AFTERWARDS — the reactive squelch, which is
/// the common case — leaves no mark on rows already queued. Without this
/// lookup, squelching a sender would not stop the pass that is mid-grind from
/// pushing their still-fresh mail.
pub fn current_rule(from_addr: &str, rules: &[SenderRule]) -> Option<Disposition> {
    crate::triage::rules::match_sender_rule(from_addr, rules).map(|r| r.disposition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::triage::deadline::DeadlineHit;

    fn hit(now: DateTime<Utc>) -> DeadlineHit {
        DeadlineHit {
            kind: "bill".to_string(),
            amount: Some(42.0),
            currency: Some("USD".to_string()),
            due_at: now + ChronoDuration::days(3),
            past_due: false,
            source: "subject".to_string(),
        }
    }

    /// A worthy baseline: fresh, normal, unruled, signal-tier, above threshold.
    fn ctx<'a>(now: DateTime<Utc>) -> EventContext<'a> {
        EventContext {
            account_id: 1,
            message_id: 7,
            thread_id: "t1",
            sender: "a@b.com",
            one_line: "hi",
            received_at: now,
            sensitivity: Sensitivity::Normal,
            is_sent: false,
            rule: None,
            tier: Tier::Signal,
            importance: 70,
            deadline: None,
        }
    }

    /// The emission table: every axis of the decision, worthy and unworthy.
    #[test]
    fn worthy_kind_table() {
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let d = hit(now);

        // ---- worthy ----------------------------------------------------
        // Score at/above the threshold -> surfaced.
        assert_eq!(worthy_kind(&ctx(now), &cfg, now), Some(EventKind::Surfaced));
        let mut c = ctx(now);
        c.importance = cfg.min_importance;
        assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Surfaced), "boundary is inclusive");

        // Urgent tiers bypass the threshold entirely.
        for tier in [Tier::PastDue, Tier::Deadline] {
            let mut c = ctx(now);
            c.tier = tier;
            c.importance = 0;
            assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Urgent), "{tier:?}");
        }

        // A detected deadline on a non-urgent tier bypasses the threshold too.
        let mut c = ctx(now);
        c.tier = Tier::Noise;
        c.importance = 0;
        c.deadline = Some(&d);
        assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Deadline));

        // PRECEDENCE: urgent beats deadline beats surfaced.
        let mut c = ctx(now);
        c.tier = Tier::Deadline;
        c.deadline = Some(&d);
        c.importance = 100;
        assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Urgent));

        // ---- unworthy --------------------------------------------------
        // Below the threshold with nothing else going for it.
        let mut c = ctx(now);
        c.tier = Tier::Noise;
        c.importance = cfg.min_importance - 1;
        assert_eq!(worthy_kind(&c, &cfg, now), None);

        // Sent mail.
        let mut c = ctx(now);
        c.is_sent = true;
        assert_eq!(worthy_kind(&c, &cfg, now), None);

        // Squelched / filtered senders are silent even when urgent.
        for disp in [Disposition::Squelch, Disposition::Filtered] {
            let mut c = ctx(now);
            c.rule = Some(disp);
            c.tier = Tier::PastDue;
            c.deadline = Some(&d);
            assert_eq!(worthy_kind(&c, &cfg, now), None, "{disp:?}");
        }
        // A SURFACE rule is the opposite instruction and must not block.
        let mut c = ctx(now);
        c.rule = Some(Disposition::Surface);
        assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Surfaced));

        // STALE: one second past the window, with a maximally alarming verdict.
        let mut c = ctx(now);
        c.received_at = now - ChronoDuration::seconds(cfg.freshness_window_secs as i64 + 1);
        c.tier = Tier::PastDue;
        c.importance = 100;
        c.deadline = Some(&d);
        assert_eq!(worthy_kind(&c, &cfg, now), None, "old mail is silent");
        // Exactly at the edge is still fresh.
        let mut c = ctx(now);
        c.received_at = now - ChronoDuration::seconds(cfg.freshness_window_secs as i64);
        assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Surfaced));

        // FUTURE-DATED: the `Date:` header is sender-controlled, and dating mail
        // in the future is a standard spam trick. It must not buy freshness —
        // otherwise months-old mail dated 2030 notifies forever, and the backlog
        // grind (which walks `received_at DESC`, so future dates come FIRST)
        // storms on a fresh install.
        for ahead in [ChronoDuration::days(365 * 4), ChronoDuration::hours(2)] {
            let mut c = ctx(now);
            c.received_at = now + ahead;
            c.tier = Tier::PastDue;
            c.importance = 100;
            c.deadline = Some(&d);
            assert_eq!(worthy_kind(&c, &cfg, now), None, "future-dated by {ahead}");
        }
        // A modestly wrong sender clock is still tolerated, both edges.
        let mut c = ctx(now);
        c.received_at = now + ChronoDuration::seconds(MAX_FUTURE_SKEW_SECS);
        assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Surfaced), "skew edge is inclusive");
        let mut c = ctx(now);
        c.received_at = now + ChronoDuration::seconds(MAX_FUTURE_SKEW_SECS + 1);
        assert_eq!(worthy_kind(&c, &cfg, now), None, "one second past the skew allowance");
    }

    /// The refine sites read the rule list LIVE, so a rule added after a message
    /// was queued still silences it.
    #[test]
    fn current_rule_matches_the_live_list_by_glob() {
        let rules = vec![
            SenderRule {
                id: 1,
                account_id: 1,
                match_pattern: "*@monitoring.example".to_string(),
                want_text: "not urgent".to_string(),
                disposition: Disposition::Squelch,
                updated_at: Utc::now(),
            },
            SenderRule {
                id: 2,
                account_id: 1,
                match_pattern: "boss@corp.example".to_string(),
                want_text: "always".to_string(),
                disposition: Disposition::Surface,
                updated_at: Utc::now(),
            },
        ];
        assert_eq!(
            current_rule("alerts@monitoring.example", &rules),
            Some(Disposition::Squelch)
        );
        assert_eq!(current_rule("boss@corp.example", &rules), Some(Disposition::Surface));
        assert_eq!(current_rule("stranger@nowhere.example", &rules), None);
        // And the squelch it returns is what silences the verdict.
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let mut c = ctx(now);
        c.rule = current_rule("alerts@monitoring.example", &rules);
        assert_eq!(worthy_kind(&c, &cfg, now), None);
    }

    /// A FILTERED rule suppresses on its DISPOSITION alone — `want_text` plays
    /// no part. The distinction matters because the store maps an empty
    /// `want_text` to `None` on the Stage-2 queue row: an emit site inferring
    /// the rule from `rule_want_text` presence (as the Stage-2 site once did)
    /// would let an empty-want_text Filtered rule push. Rule creation now
    /// rejects that shape too, but rows written before the validation can still
    /// carry it, so the emit-site suppression must not depend on it.
    #[test]
    fn filtered_rule_with_empty_want_text_still_silences() {
        let rules = vec![SenderRule {
            id: 1,
            account_id: 1,
            match_pattern: "*@vendor.example".to_string(),
            want_text: "   ".to_string(),
            disposition: Disposition::Filtered,
            updated_at: Utc::now(),
        }];
        assert_eq!(
            current_rule("billing@vendor.example", &rules),
            Some(Disposition::Filtered),
            "the match is by pattern + disposition, never by want_text"
        );
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let mut c = ctx(now);
        c.rule = current_rule("billing@vendor.example", &rules);
        c.tier = Tier::PastDue;
        c.importance = 100;
        assert_eq!(worthy_kind(&c, &cfg, now), None, "filtered stays silent, want_text or not");
    }

    /// SEAL INVARIANT: a sealed message can NEVER produce an event — not with a
    /// past-due tier, not with a deadline, not at importance 100. No path
    /// constructs those values for sealed mail; this proves the decision would
    /// refuse them even if one did.
    #[test]
    fn sealed_can_never_produce_an_event() {
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let d = hit(now);
        for (tier, importance, deadline) in [
            (Tier::Noise, 0u8, None),
            (Tier::PastDue, 100, Some(&d)),
            (Tier::Deadline, 100, Some(&d)),
            (Tier::Signal, 100, None),
        ] {
            let mut c = ctx(now);
            c.sensitivity = Sensitivity::Sealed;
            c.tier = tier;
            c.importance = importance;
            c.deadline = deadline;
            assert_eq!(worthy_kind(&c, &cfg, now), None, "sealed {tier:?}/{importance}");
            assert!(event_for(&c, &cfg, now).is_none());
        }
    }

    /// The snapshot really is denormalized: the row carries everything a client
    /// needs to render without touching the message.
    #[test]
    fn event_for_snapshots_the_verdict() {
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let d = hit(now);
        let mut c = ctx(now);
        c.tier = Tier::Deadline;
        c.deadline = Some(&d);
        let ev = event_for(&c, &cfg, now).expect("worthy");
        assert_eq!(ev.account_id, 1);
        assert_eq!(ev.message_id, 7);
        assert_eq!(ev.thread_id, "t1");
        assert_eq!(ev.kind, EventKind::Urgent);
        assert_eq!(ev.tier, Tier::Deadline);
        assert_eq!(ev.importance, 70);
        assert_eq!(ev.sender, "a@b.com");
        assert_eq!(ev.one_line, "hi");
        assert_eq!(ev.deadline.as_deref(), Some(d.due_at.to_rfc3339().as_str()));
    }

    /// A raised `min_importance` moves the line; env/config drives it.
    #[test]
    fn min_importance_is_config_driven() {
        let now = Utc::now();
        let cfg = NotifyConfig {
            min_importance: 90,
            ..NotifyConfig::default()
        };
        let c = ctx(now); // importance 70
        assert_eq!(worthy_kind(&c, &cfg, now), None);
        let mut c = ctx(now);
        c.importance = 90;
        assert_eq!(worthy_kind(&c, &cfg, now), Some(EventKind::Surfaced));
    }
}
