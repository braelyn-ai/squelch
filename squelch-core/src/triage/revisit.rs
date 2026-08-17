//! Scheduled re-evaluation: the time dimension triage was missing.
//!
//! Every verdict before this was a pure function of `(message, now_at_ingest)`,
//! which cannot express the most ordinary fact about a mailbox: **relevance
//! expires.** A restaurant confirming Thursday's reservation belongs in the
//! standing band on Tuesday and belongs nowhere at all on Friday, and no amount
//! of better classification at ingest can know that, because on Tuesday the
//! verdict was RIGHT. Nothing was misfiled; the world moved.
//!
//! So the classifier now says when to look again. Alongside the verdict it emits
//! zero or more `{at, why}` pairs — a list, not one date, because one message
//! can have several turning points (a flight is worth more at check-in and worth
//! nothing after landing). A pass picks up the due ones and re-runs triage with
//! the prior verdict and the elapsed time in hand.
//!
//! Two things keep this from becoming a spend loop. The model's dates are
//! [`plan`]ned rather than trusted: parsed, bounded, deduplicated, and capped
//! per message. And obligations get an automatic revisit whether the model
//! remembered to ask for one or not, because the failure mode being fixed here
//! is precisely a model not thinking about tomorrow.

use crate::triage::DeadlineHit;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Who asked for this re-evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisitSource {
    /// The classifier named this date itself.
    Model,
    /// Automatic, from a verdict that carries a deadline. Belt to the model's
    /// braces: an obligation whose date has passed is the single most common
    /// way a row goes stale, and it must not depend on the model volunteering.
    Deadline,
    /// Automatic, from a row sitting in the standing band untouched. After long
    /// enough with no action, a row is either wrong or done; both deserve
    /// another look.
    FyeStale,
}

impl RevisitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Deadline => "deadline",
            Self::FyeStale => "fye_stale",
        }
    }
}

/// One planned re-evaluation, ready to store.
#[derive(Debug, Clone, PartialEq)]
pub struct RevisitRequest {
    pub at: DateTime<Utc>,
    /// Short label for WHY, shown back to the model on re-evaluation. For
    /// [`RevisitSource::Model`] this is model-authored text derived from
    /// untrusted email, so it is neutralized before it renders in a prompt.
    pub why: String,
    pub source: RevisitSource,
}

/// One `{at, why}` entry as the model emits it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisitOut {
    /// RFC3339 timestamp.
    pub at: String,
    pub why: String,
}

/// Planning bounds. Every one of these exists to stop a model-authored date from
/// becoming an unbounded scheduler.
#[derive(Debug, Clone, Copy)]
pub struct RevisitConfig {
    /// Most revisits stored per message per pass.
    pub max_per_message: usize,
    /// A revisit closer than this is pointless churn: the row was just
    /// classified. Nearer dates are pushed out to it.
    pub min_lead: Duration,
    /// A revisit further out than this is dropped. A model that wants to look
    /// again in ten years is not scheduling, it is hallucinating.
    pub max_horizon: Duration,
    /// How long after a deadline passes to re-evaluate automatically.
    pub deadline_grace: Duration,
    /// Two revisits closer together than this are the same revisit.
    pub dedupe_window: Duration,
    /// Hard cap on total re-evaluations for one message, across all passes. A
    /// model that answers every revisit with "check again tomorrow" terminates.
    pub max_per_message_lifetime: u32,
    /// `why` is truncated to this many chars before storage.
    pub max_why_chars: usize,
}

impl Default for RevisitConfig {
    fn default() -> Self {
        Self {
            max_per_message: 4,
            min_lead: Duration::hours(1),
            max_horizon: Duration::days(400),
            deadline_grace: Duration::days(1),
            dedupe_window: Duration::hours(12),
            max_per_message_lifetime: 6,
            max_why_chars: 120,
        }
    }
}

/// Turn the model's requested dates plus the settled verdict into the revisits
/// to actually store, in ascending date order.
///
/// Model dates are bounded and deduplicated, never taken at face value; a
/// deadline adds its own automatic entry unless the model already asked for one
/// around the same time.
pub fn plan(
    model_revisits: &[RevisitOut],
    deadline: Option<&DeadlineHit>,
    cfg: &RevisitConfig,
    now: DateTime<Utc>,
) -> Vec<RevisitRequest> {
    let mut out: Vec<RevisitRequest> = Vec::new();

    for r in model_revisits {
        let Ok(parsed) = DateTime::parse_from_rfc3339(r.at.trim()) else {
            continue; // unparseable date: drop it, never guess one
        };
        let at = parsed.with_timezone(&Utc);
        if at > now + cfg.max_horizon {
            continue;
        }
        // A date already past (or too close) still means "look again", so it is
        // pulled forward to the floor rather than dropped: the model noticed
        // something expires, and being late to notice is not a reason to skip.
        let at = at.max(now + cfg.min_lead);
        let why = truncate_why(&r.why, cfg.max_why_chars);
        push_deduped(
            &mut out,
            RevisitRequest {
                at,
                why,
                source: RevisitSource::Model,
            },
            cfg,
        );
    }

    // The automatic obligation revisit, added LAST so a model-authored entry
    // near the same moment wins the dedupe and keeps its more specific `why`.
    if let Some(hit) = deadline {
        let at = (hit.due_at + cfg.deadline_grace).max(now + cfg.min_lead);
        if at <= now + cfg.max_horizon {
            push_deduped(
                &mut out,
                RevisitRequest {
                    at,
                    why: "the stated due date has passed".to_string(),
                    source: RevisitSource::Deadline,
                },
                cfg,
            );
        }
    }

    out.sort_by_key(|r| r.at);
    out.truncate(cfg.max_per_message);
    out
}

/// Append unless an existing entry sits within the dedupe window.
fn push_deduped(out: &mut Vec<RevisitRequest>, req: RevisitRequest, cfg: &RevisitConfig) {
    let clash = out
        .iter()
        .any(|e| (e.at - req.at).abs() < cfg.dedupe_window);
    if !clash {
        out.push(req);
    }
}

/// Bound `why` by CHARACTERS, not bytes, so a multi-byte grapheme cannot be
/// split into invalid UTF-8 on the way to the database.
fn truncate_why(raw: &str, max: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    trimmed.chars().take(max).collect()
}

/// The `revisit` property for both stages' output schemas: written once so the
/// two stages cannot drift apart on the shape they accept.
pub fn schema_property() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["at", "why"],
            "properties": {
                "at": { "type": "string" },
                "why": { "type": "string" }
            }
        }
    })
}

/// The `revisit` section of both stages' system prompts. Shared verbatim so the
/// two stages ask for the same thing in the same words.
pub const PROMPT: &str = "\
REVISIT: relevance expires, and you are the only one who can see when. Along \
with the verdict, list zero or more dates at which this email should be \
re-evaluated, as objects {\"at\": <RFC3339 UTC timestamp>, \"why\": <short \
reason>}. At each date the email is scored again with your verdict and the \
elapsed time in hand.

Ask for a revisit whenever this email's importance depends on a moment passing:
- an event, reservation, appointment, or booking, just after it happens (a \
confirmed dinner reservation matters until the dinner, and is dead weight the \
next morning)
- a bill or payment, just after its due date
- a trip or delivery, at the point it starts and again once it is over
- an offer, sale, or RSVP, just after it closes
- a waiting-on-a-reply thread, once enough time has passed that silence itself \
is the news

Use the email's OWN dates: if it names a date, revisit just after that date, \
not on it. Do not invent a date the email does not support. Return an empty \
list for mail whose value does not change with time, which is most mail: a \
receipt, a newsletter, a statement, a personal note. Prefer at most two or \
three entries; there is no credit for filling the list.";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap()
    }

    fn cfg() -> RevisitConfig {
        RevisitConfig::default()
    }

    fn model(at: &str, why: &str) -> RevisitOut {
        RevisitOut {
            at: at.into(),
            why: why.into(),
        }
    }

    fn hit(due_at: DateTime<Utc>) -> DeadlineHit {
        DeadlineHit {
            kind: "invoice".into(),
            amount: None,
            currency: None,
            due_at,
            past_due: false,
            source: "test".into(),
        }
    }

    /// The motivating case: a reservation confirmation, revisited the morning
    /// after the meal.
    #[test]
    fn a_reservation_schedules_the_morning_after() {
        let plan = plan(
            &[model(
                "2026-07-11T04:00:00Z",
                "dinner reservation has passed",
            )],
            None,
            &cfg(),
            now(),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].source, RevisitSource::Model);
        assert_eq!(plan[0].why, "dinner reservation has passed");
        assert_eq!(
            plan[0].at,
            Utc.with_ymd_and_hms(2026, 7, 11, 4, 0, 0).unwrap()
        );
    }

    /// Most mail does not expire, and the empty list has to stay cheap.
    #[test]
    fn mail_that_does_not_expire_schedules_nothing() {
        assert!(plan(&[], None, &cfg(), now()).is_empty());
    }

    /// An obligation gets its automatic revisit even when the model forgot to
    /// ask, which is the whole reason the automatic arm exists.
    #[test]
    fn a_deadline_schedules_itself_without_being_asked() {
        let due = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let plan = plan(&[], Some(&hit(due)), &cfg(), now());
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].source, RevisitSource::Deadline);
        assert_eq!(plan[0].at, due + Duration::days(1));
    }

    /// ...but it does not double up when the model already asked for one at
    /// about the same time; the model's more specific reason survives.
    #[test]
    fn the_automatic_deadline_revisit_defers_to_a_model_entry_nearby() {
        let due = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let plan = plan(
            &[model("2026-08-02T06:00:00Z", "invoice due date has passed")],
            Some(&hit(due)),
            &cfg(),
            now(),
        );
        assert_eq!(plan.len(), 1, "one revisit, not two: {plan:?}");
        assert_eq!(plan[0].source, RevisitSource::Model);
        assert_eq!(plan[0].why, "invoice due date has passed");
    }

    /// A model-authored date is data, not a schedule: garbage is dropped rather
    /// than guessed at.
    #[test]
    fn an_unparseable_date_is_dropped_not_guessed() {
        let plan = plan(
            &[
                model("next tuesday", "soon"),
                model("", "empty"),
                model("2026-07-20T00:00:00Z", "real"),
            ],
            None,
            &cfg(),
            now(),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].why, "real");
    }

    /// A date in the past still means "this expires" — the model just noticed
    /// late — so it is pulled to the floor instead of thrown away.
    #[test]
    fn a_past_date_is_pulled_forward_rather_than_dropped() {
        let plan = plan(
            &[model("2020-01-01T00:00:00Z", "long over")],
            None,
            &cfg(),
            now(),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].at, now() + cfg().min_lead);
    }

    /// A wild horizon is not a schedule.
    #[test]
    fn an_absurd_horizon_is_dropped() {
        let plan = plan(
            &[model("2400-01-01T00:00:00Z", "someday")],
            None,
            &cfg(),
            now(),
        );
        assert!(plan.is_empty());
    }

    /// Near-identical dates collapse, so a model listing the same turning point
    /// three ways cannot triple the spend.
    #[test]
    fn near_identical_dates_collapse() {
        let plan = plan(
            &[
                model("2026-07-20T00:00:00Z", "a"),
                model("2026-07-20T03:00:00Z", "b"),
                model("2026-07-20T06:00:00Z", "c"),
            ],
            None,
            &cfg(),
            now(),
        );
        assert_eq!(plan.len(), 1, "{plan:?}");
    }

    #[test]
    fn the_list_is_capped_and_ordered() {
        let entries: Vec<RevisitOut> = (1..=10)
            .map(|d| model(&format!("2026-08-{d:02}T00:00:00Z"), "x"))
            .collect();
        let plan = plan(&entries, None, &cfg(), now());
        assert_eq!(plan.len(), cfg().max_per_message);
        assert!(
            plan.windows(2).all(|w| w[0].at <= w[1].at),
            "ascending: {plan:?}"
        );
    }

    /// A multi-byte `why` must not be sliced into invalid UTF-8.
    #[test]
    fn a_long_multibyte_why_is_truncated_by_characters() {
        let why = "é".repeat(400);
        let plan = plan(&[model("2026-07-20T00:00:00Z", &why)], None, &cfg(), now());
        assert_eq!(plan[0].why.chars().count(), cfg().max_why_chars);
    }
}
