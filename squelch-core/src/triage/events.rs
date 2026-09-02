//! The notification-emission decision: is THIS triage verdict worth waking the
//! user for, and what does the durable `events` row look like?
//!
//! Pure and store-free — the sync engine owns the call sites, the store owns the
//! append — so the whole policy is unit-testable without a database. Kind
//! precedence is `urgent` > `deadline` > `surfaced`; sealed mail is never
//! notified, since even a contentless ping on a lock screen would undo the seal
//! (see docs/SECURITY.md).
//!
//! ELIGIBILITY IS NOT DECIDED HERE. Whether a message may EVER notify is
//! answered once, at ingest, by the one caller that knows which sync path it is
//! on, and stamped on the row as `triage.notify_eligible_at`; this module reads
//! that stamp and asks only the two questions left: is the verdict worth a buzz,
//! and did we get to it in time (docs/NOTIFY.md §11.3).

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
    /// `from_addr`, as the client lists render it (matches
    /// [`crate::types::Update::sender`]).
    pub sender: &'a str,
    pub one_line: &'a str,
    /// `triage.notify_eligible_at`: WHEN WE FIRST SAW this message, stamped once
    /// at ingest, or `None` for a message that may never notify (backfill, a
    /// sent copy, or mail already stale the first time we laid eyes on it).
    ///
    /// Deliberately NOT `received_at`, which this replaced. That one is the
    /// sender's `Date:` header, so it answered "how old does the sender claim
    /// this is" at a site that meant to ask "how long have we been sitting on
    /// it" — and the two diverge exactly when a verdict is late, which is the
    /// case the notification mattered in. See [`NotifyConfig::rescue_window_secs`].
    pub notify_eligible_at: Option<DateTime<Utc>>,
    pub sensitivity: Sensitivity,
    /// `true` for the user's own outbox.
    pub is_sent: bool,
    /// The disposition of the sender rule that decided this row, when one fired.
    pub rule: Option<Disposition>,
    pub tier: Tier,
    pub importance: u8,
    pub deadline: Option<&'a DeadlineHit>,
}

/// How far AHEAD of our clock a message may be dated and still count as fresh:
/// tolerance for a wrong sender clock only, never a licence to notify.
const MAX_FUTURE_SKEW_SECS: i64 = 3600;

/// Why an emission was refused, when it was. TWO refusals, kept apart because
/// they mean opposite things to whoever asked: `NotWorthy` is the system working
/// (this mail does not deserve a buzz), while `Expired` is a notification the
/// user WOULD have wanted and did not get because we were too slow — the drop
/// that ran at 24.7% with no counter and no log line to say so (docs/NOTIFY.md
/// §2a). Only the caller can record it, so only the caller may be told, and it
/// must not have to re-derive the window to work it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Never a candidate (sealed, sent, squelched, never eligible) or simply
    /// below the line. Silent, and correctly so.
    NotWorthy,
    /// Eligible AND worthy, but past [`NotifyConfig::rescue_window_secs`] by the
    /// time this site reached it. Counted, never silent.
    Expired,
}

/// Whether `received_at` is inside the configured freshness window — THE
/// FIRST-SIGHT TEST (see [`NotifyConfig::freshness_window_secs`]). Bounded on
/// BOTH sides because `received_at` is SENDER-CONTROLLED (ingest prefers the
/// RFC822 `Date:` header): with no ceiling, future-dated mail stays "fresh"
/// forever and the backlog grind — which walks the queues `received_at DESC` —
/// storms.
///
/// ASKED ONCE, AT INGEST, by [`crate::sync::SyncEngine`], whose answer becomes
/// `triage.notify_eligible_at`. It is deliberately no longer asked at the
/// emission sites: there it was measuring the wrong clock (see
/// [`EventContext::notify_eligible_at`]).
pub fn is_fresh(received_at: DateTime<Utc>, cfg: &NotifyConfig, now: DateTime<Utc>) -> bool {
    let floor = now - ChronoDuration::seconds(cfg.freshness_window_secs as i64);
    let ceiling = now + ChronoDuration::seconds(MAX_FUTURE_SKEW_SECS);
    received_at >= floor && received_at <= ceiling
}

/// THE decision. `Ok(kind)` when this verdict earns a notification, otherwise
/// the [`Refusal`] saying which kind of no it is. Pure — no store, no clock
/// beyond the injected `now`.
///
/// THE WORTHINESS QUESTION IS ANSWERED BEFORE THE WINDOW ONE, and the order is
/// load-bearing: `Expired` is reserved for mail that would have buzzed, so that
/// the counter hanging off it measures missed notifications rather than
/// every below-the-line row that happened to age out of a queue.
pub fn worthy_kind(
    ctx: &EventContext<'_>,
    cfg: &NotifyConfig,
    now: DateTime<Utc>,
) -> std::result::Result<EventKind, Refusal> {
    // SEAL INVARIANT, defense in depth: sealed rows never get here anyway.
    if ctx.sensitivity != Sensitivity::Normal {
        return Err(Refusal::NotWorthy);
    }
    // The user's own outbox never notifies the user.
    if ctx.is_sent {
        return Err(Refusal::NotWorthy);
    }
    // A squelch/filtered rule is a standing "not from this sender".
    if matches!(
        ctx.rule,
        Some(Disposition::Squelch) | Some(Disposition::Filtered)
    ) {
        return Err(Refusal::NotWorthy);
    }
    // NEVER ELIGIBLE: no stamp means backfill, a sent copy, or mail that was
    // already stale the first time we saw it. Structural, and unrescuable.
    let Some(eligible_at) = ctx.notify_eligible_at else {
        return Err(Refusal::NotWorthy);
    };

    // Precedence: urgent > deadline > surfaced.
    let kind = if matches!(ctx.tier, Tier::PastDue | Tier::Deadline) {
        EventKind::Urgent
    } else if ctx.deadline.is_some() {
        EventKind::Deadline
    } else if ctx.importance >= cfg.min_importance {
        EventKind::Surfaced
    } else {
        return Err(Refusal::NotWorthy);
    };

    // THE RESCUE CEILING, measured from the moment WE saw the message. A verdict
    // that took a slow model call or waited behind a backlog still lands; one
    // that took hours does not, because past that horizon a buzz is news about
    // mail the user has already read.
    if now - eligible_at > ChronoDuration::seconds(cfg.rescue_window_secs as i64) {
        return Err(Refusal::Expired);
    }
    Ok(kind)
}

/// [`worthy_kind`] plus the denormalized snapshot the `events` row stores — what
/// the sync engine hands to [`crate::store::Store::append_event`]. The
/// [`Refusal`] is passed straight through, so the emission site can count an
/// expiry without asking the question twice.
pub fn event_for(
    ctx: &EventContext<'_>,
    cfg: &NotifyConfig,
    now: DateTime<Utc>,
) -> std::result::Result<NewEvent, Refusal> {
    let kind = worthy_kind(ctx, cfg, now)?;
    Ok(NewEvent {
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

/// Gather the [`EventContext`] for an INGEST-path verdict, resolving
/// `matched_rule`'s id against `rules` (the batch's rule list).
///
/// CALLER OBLIGATION: only a CONFIDENT seed may emit — a guess is not grounds
/// for waking anyone. Non-confident rows wait for Stage-1/Stage-2 to refine them
/// and emit from those sites instead, carrying the SAME eligibility stamp — one
/// answer to "may this message ever notify", written here and read there.
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
        // PROVISIONAL. Off the row the engine is about to commit, which is where
        // the stamp was just computed — the one site that knows which sync path
        // this is — but the computed value is not always the value the STORE
        // holds, and the store's is the one that decides.
        //
        // They diverge on exactly one real path: `catch_up()` re-ingests a row
        // that already exists, `ingest_message` writes `notify_eligible_at` on
        // FIRST INSERT ONLY and preserves the stored one on conflict, so a
        // backfilled row (stamp NULL, silent forever) whose `Date:` happens to be
        // inside the freshness window at catch-up time computes a `Some` here
        // that the database will never accept. Emitting off it would notify for
        // month-old mail.
        //
        // So the engine's ingest emission site OVERWRITES this field with a
        // `triage_seed_verdict` re-read of the committed row before calling
        // `emit_event` (see `SyncEngine::ingest_one`, and `emit_seed_event` doing
        // the same thing for the same reason). A future caller that emits from
        // this context WITHOUT that re-read reintroduces the bug.
        notify_eligible_at: triaged.notify_eligible_at,
        sensitivity: triaged.sensitivity,
        is_sent: triaged.message.is_sent,
        rule,
        tier: triaged.tier,
        importance: triaged.importance,
        deadline: triaged.deadline.as_ref(),
    }
}

/// Gather the [`EventContext`] for a Stage-1 row whose model call produced NO
/// verdict, from the heuristic seed the row still carries. `None` means this row
/// must not notify from its seed.
///
/// THE GATE IS THE SEED'S OWN CONFIDENCE, read back rather than guessed at:
/// ingest stores `needs_stage2 = !confident`, so a row still bound for Stage-2
/// has another verdict coming and must wait for it. A CONFIDENT seed on a row
/// that failed Stage-1 permanently is the final word — nothing else will look at
/// it, and `UNIQUE(message_id)` means a notification skipped now is skipped
/// forever — so it notifies, exactly as the seed does on a daemon with no API
/// key at all. That equivalence is the point: "no model to wait for" covers a
/// model that refused as surely as a model that was never configured.
pub fn seed_context<'a>(
    row: &'a crate::store::Stage1Queued,
    seed: &'a crate::store::SeedVerdict,
    deadline: Option<&'a DeadlineHit>,
    rule: Option<Disposition>,
) -> Option<EventContext<'a>> {
    if seed.needs_stage2 {
        return None;
    }
    Some(EventContext {
        account_id: row.account_id,
        message_id: row.message_id,
        thread_id: &row.thread_id,
        sender: &row.from_addr,
        one_line: &seed.one_line,
        // From the SEED, not from `row`: the seed is read back at emission
        // time while the queued row was read at the top of the pass, and a
        // stamp is written once and never rewritten, so the two agree — but
        // the fresher read is the honest one to quote.
        notify_eligible_at: seed.notify_eligible_at,
        sensitivity: row.sensitivity,
        // The Stage-1 queue selects `m.is_sent = 0`.
        is_sent: false,
        rule,
        tier: seed.tier,
        importance: seed.importance,
        deadline,
    })
}

/// The sender's rule as the list stands NOW, for the REFINE sites (Stage-1 /
/// Stage-2 apply). They cannot read it off the triage row: a rule the user adds
/// AFTER ingest — the reactive squelch, the common case — leaves no mark on rows
/// already queued, so without this lookup a mid-grind pass would keep pushing
/// that sender's still-fresh mail.
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

    /// A worthy baseline: eligible as of NOW, normal, unruled, signal-tier,
    /// above threshold.
    fn ctx<'a>(now: DateTime<Utc>) -> EventContext<'a> {
        EventContext {
            account_id: 1,
            message_id: 7,
            thread_id: "t1",
            sender: "a@b.com",
            one_line: "hi",
            notify_eligible_at: Some(now),
            sensitivity: Sensitivity::Normal,
            is_sent: false,
            rule: None,
            tier: Tier::Signal,
            importance: 70,
            deadline: None,
        }
    }

    /// The emission table: every axis of the decision, worthy and unworthy, and
    /// which KIND of unworthy — the two refusals are what the caller counts on.
    #[test]
    fn worthy_kind_table() {
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let d = hit(now);

        // ---- worthy ----------------------------------------------------
        // Score at/above the threshold -> surfaced.
        assert_eq!(worthy_kind(&ctx(now), &cfg, now), Ok(EventKind::Surfaced));
        let mut c = ctx(now);
        c.importance = cfg.min_importance;
        assert_eq!(
            worthy_kind(&c, &cfg, now),
            Ok(EventKind::Surfaced),
            "boundary is inclusive"
        );

        // Urgent tiers bypass the threshold entirely.
        for tier in [Tier::PastDue, Tier::Deadline] {
            let mut c = ctx(now);
            c.tier = tier;
            c.importance = 0;
            assert_eq!(
                worthy_kind(&c, &cfg, now),
                Ok(EventKind::Urgent),
                "{tier:?}"
            );
        }

        // A detected deadline on a non-urgent tier bypasses the threshold too.
        let mut c = ctx(now);
        c.tier = Tier::Noise;
        c.importance = 0;
        c.deadline = Some(&d);
        assert_eq!(worthy_kind(&c, &cfg, now), Ok(EventKind::Deadline));

        // PRECEDENCE: urgent beats deadline beats surfaced.
        let mut c = ctx(now);
        c.tier = Tier::Deadline;
        c.deadline = Some(&d);
        c.importance = 100;
        assert_eq!(worthy_kind(&c, &cfg, now), Ok(EventKind::Urgent));

        // ---- unworthy --------------------------------------------------
        // Below the threshold with nothing else going for it.
        let mut c = ctx(now);
        c.tier = Tier::Noise;
        c.importance = cfg.min_importance - 1;
        assert_eq!(worthy_kind(&c, &cfg, now), Err(Refusal::NotWorthy));

        // Sent mail.
        let mut c = ctx(now);
        c.is_sent = true;
        assert_eq!(worthy_kind(&c, &cfg, now), Err(Refusal::NotWorthy));

        // Squelched / filtered senders are silent even when urgent.
        for disp in [Disposition::Squelch, Disposition::Filtered] {
            let mut c = ctx(now);
            c.rule = Some(disp);
            c.tier = Tier::PastDue;
            c.deadline = Some(&d);
            assert_eq!(
                worthy_kind(&c, &cfg, now),
                Err(Refusal::NotWorthy),
                "{disp:?}"
            );
        }
        // A SURFACE rule is the opposite instruction and must not block.
        let mut c = ctx(now);
        c.rule = Some(Disposition::Surface);
        assert_eq!(worthy_kind(&c, &cfg, now), Ok(EventKind::Surfaced));

        // NEVER ELIGIBLE: no stamp, with a maximally alarming verdict. Backfill,
        // a sent copy and mail already stale at first sight all land here, and
        // none of them is a missed notification — so it is NotWorthy, not
        // Expired, and nothing counts it.
        let mut c = ctx(now);
        c.notify_eligible_at = None;
        c.tier = Tier::PastDue;
        c.importance = 100;
        c.deadline = Some(&d);
        assert_eq!(
            worthy_kind(&c, &cfg, now),
            Err(Refusal::NotWorthy),
            "an unstamped row can never notify"
        );

        // THE RESCUE CEILING, both sides of it. Inside the hour a late verdict
        // still buzzes: this is the 24.7% the old `Date:`-based guard ate.
        let mut c = ctx(now);
        c.notify_eligible_at = Some(now - ChronoDuration::seconds(3599));
        assert_eq!(
            worthy_kind(&c, &cfg, now),
            Ok(EventKind::Surfaced),
            "an hour-old rescue still lands"
        );
        let mut c = ctx(now);
        c.notify_eligible_at = Some(now - ChronoDuration::seconds(3601));
        assert_eq!(
            worthy_kind(&c, &cfg, now),
            Err(Refusal::Expired),
            "past the ceiling the drop is COUNTED, not silent"
        );
        // Exactly at the ceiling is still inside it.
        let mut c = ctx(now);
        c.notify_eligible_at = Some(now - ChronoDuration::seconds(cfg.rescue_window_secs as i64));
        assert_eq!(
            worthy_kind(&c, &cfg, now),
            Ok(EventKind::Surfaced),
            "the edge is inclusive"
        );

        // AND EXPIRY IS THE LAST QUESTION ASKED. A row that was never going to
        // buzz is NotWorthy however long it sat, or the counter hanging off
        // `Expired` would measure queue depth instead of missed notifications.
        let mut c = ctx(now);
        c.notify_eligible_at = Some(now - ChronoDuration::hours(9));
        c.tier = Tier::Noise;
        c.importance = cfg.min_importance - 1;
        assert_eq!(
            worthy_kind(&c, &cfg, now),
            Err(Refusal::NotWorthy),
            "below the line is not a missed notification"
        );
        // Same for a structural gate: a squelched sender's aged-out row is not
        // a rescue anybody wanted.
        let mut c = ctx(now);
        c.notify_eligible_at = Some(now - ChronoDuration::hours(9));
        c.rule = Some(Disposition::Squelch);
        assert_eq!(worthy_kind(&c, &cfg, now), Err(Refusal::NotWorthy));
    }

    /// THE FIRST-SIGHT TEST, now asked exactly once (at ingest) rather than at
    /// every emission site. Both bounds are load-bearing and neither is
    /// reachable through `worthy_kind` any more, so they are asserted here:
    /// `received_at` is the SENDER'S `Date:` header, so an old one must not
    /// stamp and a future-dated one must not buy permanent freshness.
    #[test]
    fn is_fresh_bounds_a_sender_controlled_date_on_both_sides() {
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let win = cfg.freshness_window_secs as i64;

        assert!(is_fresh(now, &cfg, now));
        assert!(
            is_fresh(now - ChronoDuration::seconds(win), &cfg, now),
            "the floor is inclusive"
        );
        assert!(!is_fresh(now - ChronoDuration::seconds(win + 1), &cfg, now));

        // A modestly wrong sender clock is tolerated; a lying one is not.
        assert!(is_fresh(
            now + ChronoDuration::seconds(MAX_FUTURE_SKEW_SECS),
            &cfg,
            now
        ));
        assert!(!is_fresh(
            now + ChronoDuration::seconds(MAX_FUTURE_SKEW_SECS + 1),
            &cfg,
            now
        ));
        assert!(
            !is_fresh(now + ChronoDuration::days(365 * 4), &cfg, now),
            "mail dated 2030 is not fresh, it is forged"
        );
    }

    /// The Stage-1 fallback notifies from the seed, but only when the seed is
    /// the LAST word. Ingest stopped emitting on the promise that a model verdict
    /// was coming; a refusal or a permanent failure is that promise breaking, and
    /// one event per message ever means a notification skipped here is skipped
    /// for good.
    #[test]
    fn a_confident_seed_notifies_when_the_model_never_answered() {
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let row = crate::store::Stage1Queued {
            message_id: 7,
            account_id: 1,
            thread_id: "t1".into(),
            from_addr: "alerts@monitoring.example".into(),
            subject: "checkout api is down".into(),
            body: String::new(),
            received_at: now,
            is_known_contact: false,
            sender_corrected: false,
            sensitivity: Sensitivity::Normal,
            retriage_at: None,
            notify_eligible_at: Some(now),
        };
        let confident = crate::store::SeedVerdict {
            tier: Tier::Signal,
            importance: 75,
            one_line: "incident opened".into(),
            needs_stage2: false,
            deadline: None,
            notify_eligible_at: Some(now),
        };

        let ctx = seed_context(&row, &confident, None, None).expect("a confident seed is final");
        assert_eq!(ctx.one_line, "incident opened", "the seed's own words");
        assert_eq!(
            worthy_kind(&ctx, &cfg, now),
            Ok(EventKind::Surfaced),
            "and it notifies"
        );

        // A row still bound for Stage-2 has another verdict coming: that pass
        // owns the notification, and emitting twice is emitting the weaker one
        // first.
        let unsure = crate::store::SeedVerdict {
            needs_stage2: true,
            ..confident.clone()
        };
        assert!(
            seed_context(&row, &unsure, None, None).is_none(),
            "a seed with an escalation still pending must stay quiet"
        );

        // A rule the owner added since is still honored here.
        let ctx = seed_context(&row, &confident, None, Some(Disposition::Squelch)).unwrap();
        assert_eq!(
            worthy_kind(&ctx, &cfg, now),
            Err(Refusal::NotWorthy),
            "squelched, so silent"
        );

        // AND THE STAMP COMES OFF THE SEED, so a row the Stage-1 pass reached
        // hours late is refused as `Expired` from this site too — the fallback
        // path is exactly where a slow model call lands.
        let stale = crate::store::SeedVerdict {
            notify_eligible_at: Some(now - ChronoDuration::hours(2)),
            ..confident.clone()
        };
        let ctx = seed_context(&row, &stale, None, None).unwrap();
        assert_eq!(worthy_kind(&ctx, &cfg, now), Err(Refusal::Expired));
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
        assert_eq!(
            current_rule("boss@corp.example", &rules),
            Some(Disposition::Surface)
        );
        assert_eq!(current_rule("stranger@nowhere.example", &rules), None);
        // And the squelch it returns is what silences the verdict.
        let now = Utc::now();
        let cfg = NotifyConfig::default();
        let mut c = ctx(now);
        c.rule = current_rule("alerts@monitoring.example", &rules);
        assert_eq!(worthy_kind(&c, &cfg, now), Err(Refusal::NotWorthy));
    }

    /// A FILTERED rule suppresses on its DISPOSITION alone, never on
    /// `want_text` — which the store maps to `None` when empty, so an emit site
    /// inferring the rule from `rule_want_text` presence would let a row with an
    /// empty want_text push.
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
        assert_eq!(
            worthy_kind(&c, &cfg, now),
            Err(Refusal::NotWorthy),
            "filtered stays silent, want_text or not"
        );
    }

    /// SEAL INVARIANT: a sealed message can NEVER produce an event, whatever its
    /// tier, deadline, or score. No path constructs those values for sealed
    /// mail; this proves the decision would refuse them even if one did.
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
            assert_eq!(
                worthy_kind(&c, &cfg, now),
                Err(Refusal::NotWorthy),
                "sealed {tier:?}/{importance}"
            );
            assert!(event_for(&c, &cfg, now).is_err());
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
        assert_eq!(worthy_kind(&c, &cfg, now), Err(Refusal::NotWorthy));
        let mut c = ctx(now);
        c.importance = 90;
        assert_eq!(worthy_kind(&c, &cfg, now), Ok(EventKind::Surfaced));
    }
}
