//! Calendar-update detection: pure classification of invites, updates,
//! cancellations, and RSVP responses, plus a best-effort title, start time, and
//! organizer ([`CalendarInfo`]). A calendar update is a RECORD — noise-tier and
//! auto-resolved at ingest, where "resolved" means inside squelch's attention
//! ledger only (sync never writes to Gmail). Never run for sealed mail (see
//! docs/SECURITY.md).
//!
//! INVARIANT — detection is STRUCTURAL, never topical: a calendar-system subject
//! prefix, with the prefixes humans also use ("Invitation:", "Accepted:", …)
//! additionally demanding corroboration (an "@ <date>" clause, a calendar-
//! notifier sender, or invite machinery in the body), or the full relayed-RSVP
//! subject template. A newsletter that merely mentions "calendar" must not match.

use crate::triage::text::rx;
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use regex::Regex;
use std::sync::OnceLock;

/// What kind of calendar update this message is. [`as_str`] is a WIRE CONTRACT:
/// it serializes into `calendar_updates.kind` AND the /client/calendar shape —
/// "invite" | "update" | "cancellation" | "response".
///
/// [`as_str`]: CalendarKind::as_str
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarKind {
    /// A new invitation ("Invitation: …").
    Invite,
    /// A change to an existing event ("Updated invitation: …", "Updated: …").
    Update,
    /// The event was canceled ("Canceled event: …", "Canceled: …").
    Cancellation,
    /// An attendee's RSVP ("Accepted: …", "Declined: …", "Tentatively
    /// accepted: …", "New time proposed: …").
    Response,
}

impl CalendarKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CalendarKind::Invite => "invite",
            CalendarKind::Update => "update",
            CalendarKind::Cancellation => "cancellation",
            CalendarKind::Response => "response",
        }
    }

    pub fn parse(s: &str) -> Option<CalendarKind> {
        match s {
            "invite" => Some(CalendarKind::Invite),
            "update" => Some(CalendarKind::Update),
            "cancellation" => Some(CalendarKind::Cancellation),
            "response" => Some(CalendarKind::Response),
            _ => None,
        }
    }
}

/// A detected calendar update: the `calendar_updates` table shape minus the DB
/// identity/timestamp columns. Every extracted field is best-effort — the
/// structural subject shape alone makes it a calendar update.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarInfo {
    pub kind: CalendarKind,
    /// The subject with the kind prefix and any trailing "@ <date> …" clause
    /// stripped; `None` when that leaves nothing.
    pub event_title: Option<String>,
    /// Best-effort start parsed from an "@ Wed Jul 22, 2026 10am …" clause.
    /// LIMITATION (v0): the wall-clock time is stored AS UTC (subject timezone
    /// abbreviations are ambiguous and unparsed) and date-only clauses land at
    /// midnight — fine for a label, never for scheduling math.
    pub starts_at: Option<DateTime<Utc>>,
    /// Best-effort organizer: an "Organizer: …" body line wins, else the
    /// sender's name/address — except for RSVP [`responses`], whose sender is
    /// the ATTENDEE, so it stays `None` rather than mislabeling the responder.
    ///
    /// [`responses`]: CalendarKind::Response
    pub organizer: Option<String>,
}

/// A subject-prefix rule: the regex (anchored at the subject start), the kind
/// it classifies, and whether the prefix is STRONG (calendar-system-only
/// phrasing, structural on its own) or needs corroboration.
struct PrefixRule {
    re: Regex,
    kind: CalendarKind,
    strong: bool,
}

struct CalendarDetector {
    /// Ordered prefix rules, more-specific shapes FIRST so "Updated invitation:"
    /// never falls through to "Invitation:" — first match wins.
    prefixes: Vec<PrefixRule>,
    /// Corroborating calendar machinery for WEAK prefixes: sender shapes and
    /// body phrases only a real invite carries. Topical words like a bare
    /// "calendar" in prose deliberately do NOT appear here.
    corroborate_sender: Vec<Regex>,
    corroborate_text: Vec<Regex>,
    /// Relayed-RSVP subject TEMPLATE: "<Name> accepted your <title> invitation
    /// (Aug 5–10, 2026)". Such RSVPs carry no calendar-service sender and no
    /// prefix, so the full structured form is itself the structural signal.
    /// Prose never matches: detection is subject-shape only, and the template
    /// requires a non-empty title between "your" and "invitation".
    rsvp_template: Regex,
    /// "Organizer: …" body line (Google/Outlook both emit one).
    organizer_line: Regex,
    /// Date-ish leadin (weekday/month/recurrence word) for the clause after
    /// " @ ", so the title/date split never truncates a title containing "@".
    dateish_leadin: Regex,
    /// Month-day(-year) fragment inside the date clause.
    month_day: Regex,
    /// Clock time inside the date clause ("10am", "9:30pm").
    clock: Regex,
    /// Standalone 4-digit year trailing a date-RANGE clause ("Aug 5–10, 2026"),
    /// where the range tail puts it out of `month_day`'s inline year group.
    year: Regex,
}

fn detector() -> &'static CalendarDetector {
    static D: OnceLock<CalendarDetector> = OnceLock::new();
    D.get_or_init(|| {
        let strong = |p: &str, kind| PrefixRule { re: rx(p), kind, strong: true };
        let weak = |p: &str, kind| PrefixRule { re: rx(p), kind, strong: false };
        CalendarDetector {
            prefixes: vec![
                // STRONG: only calendar systems produce these.
                strong(r"^updated invitation:\s*", CalendarKind::Update),
                strong(r"^cancell?ed event:\s*", CalendarKind::Cancellation),
                strong(r"^tentatively accepted:\s*", CalendarKind::Response),
                strong(r"^new time proposed:\s*", CalendarKind::Response),
                strong(r"^meeting cancell?ed:\s*", CalendarKind::Cancellation),
                // WEAK: humans/marketers use these too ("Invitation: VIP
                // sale") — require corroboration.
                weak(r"^invitation:\s*", CalendarKind::Invite),
                weak(r"^updated:\s*", CalendarKind::Update),
                weak(r"^cancell?ed:\s*", CalendarKind::Cancellation),
                weak(r"^accepted:\s*", CalendarKind::Response),
                weak(r"^declined:\s*", CalendarKind::Response),
                weak(r"^tentative:\s*", CalendarKind::Response),
            ],
            corroborate_sender: vec![
                rx(r"calendar-notification@"),
                rx(r"@calendar\."),
                rx(r"calendar-server\."),
            ],
            corroborate_text: vec![
                // Invite machinery, not topical prose.
                rx(r"\btext/calendar\b"),
                rx(r"\binvite\.ics\b"),
                rx(r"\.ics\b"),
                rx(r"\bview on google calendar\b"),
                rx(r"\bgoogle calendar\b"),
                rx(r"\boutlook calendar\b"),
                rx(r"\badd to calendar\b"),
                rx(r"\bmicrosoft teams meeting\b"),
                rx(r"(?m)^\s*when:\s"),
                rx(r"(?m)^\s*organi[sz]er:?\s"),
                rx(r"\brsvp\b"),
                rx(r"\bmeeting request\b"),
                rx(r"\bhas (accepted|declined|tentatively accepted) this (meeting|event|invitation)\b"),
            ],
            rsvp_template: rx(
                r"^(?P<who>[^:@\r\n]{1,60}?)\s+(?:has\s+)?(?:accepted|declined|tentatively accepted)\s+your\s+(?P<title>.+?)\s+invitation\b[.!\s]*(?:\((?P<dates>[^)]+)\))?\s*$",
            ),
            organizer_line: rx(r"(?m)^\s*organi[sz]er:?\s*(\S[^\r\n]*)"),
            dateish_leadin: rx(
                r"^(?:mon|tue|wed|thu|fri|sat|sun|jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec|daily|weekly|monthly|annually|every)[a-z]*\b",
            ),
            month_day: rx(
                r"\b(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(\d{1,2})(?:st|nd|rd|th)?(?:,\s*(\d{4}))?",
            ),
            clock: rx(r"\b(\d{1,2})(?::(\d{2}))?\s*(am|pm)\b"),
            year: rx(r"\b(20\d{2})\b"),
        }
    })
}

/// Does anything corroborate a WEAK subject prefix? The "@ <date>" subject
/// clause is checked separately by the caller, via the title/clause split.
fn has_corroboration(from_addr: &str, body: &str) -> bool {
    let d = detector();
    d.corroborate_sender.iter().any(|re| re.is_match(from_addr))
        || d.corroborate_text.iter().any(|re| re.is_match(body))
}

/// Split the post-prefix subject into `(title, date_clause)` on the " @ <date>"
/// convention, at the FIRST " @ " whose suffix starts date-ish — so a title
/// containing a literal "@" ("Coffee @ Blue Bottle") is not truncated. No
/// date-ish suffix means the whole remainder is the title.
fn split_title_clause(rest: &str) -> (String, Option<&str>) {
    let d = detector();
    for (i, _) in rest.match_indices('@') {
        // A separator "@" stands alone: at the start or after whitespace, and
        // followed by whitespace (never an email's "user@host").
        let standalone = (i == 0
            || rest[..i].ends_with(|c: char| c.is_whitespace()))
            && rest[i + 1..].starts_with(|c: char| c.is_whitespace());
        if !standalone {
            continue;
        }
        let suffix = rest[i + 1..].trim_start();
        if d.dateish_leadin.is_match(suffix) {
            return (rest[..i].trim().to_string(), Some(suffix));
        }
    }
    (rest.trim().to_string(), None)
}

/// Resolve a year-less (month, day) to the nearest occurrence relative to
/// receipt: same year, unless that lands more than 14 days before `received_at`,
/// in which case roll forward a year.
fn resolve_yearless(month: u32, day: u32, received_at: DateTime<Utc>) -> Option<NaiveDate> {
    let recv = received_at.date_naive();
    let same_year = NaiveDate::from_ymd_opt(recv.year(), month, day)?;
    if (recv - same_year).num_days() > 14 {
        NaiveDate::from_ymd_opt(recv.year() + 1, month, day)
    } else {
        Some(same_year)
    }
}

/// Best-effort start time from an "@ …" date clause: the FIRST month-day (a
/// year-less one anchors to `received_at`) and the FIRST clock time after it.
/// See [`CalendarInfo::starts_at`] for the v0 timezone limitation.
fn parse_starts_at(clause: &str, received_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let d = detector();
    let cap = d.month_day.captures(clause)?;
    let month = match cap.get(1)?.as_str().to_lowercase().as_str() {
        "jan" => 1, "feb" => 2, "mar" => 3, "apr" => 4, "may" => 5, "jun" => 6,
        "jul" => 7, "aug" => 8, "sep" => 9, "oct" => 10, "nov" => 11, "dec" => 12,
        _ => return None,
    };
    let day: u32 = cap.get(2)?.as_str().parse().ok()?;
    let after = &clause[cap.get(0)?.end()..];
    let date = match cap.get(3) {
        Some(y) => NaiveDate::from_ymd_opt(y.as_str().parse().ok()?, month, day)?,
        // A range clause ("Aug 5–10, 2026") separates the year from the first
        // month-day: try a standalone trailing year before resolving year-less.
        None => match d.year.captures(after) {
            Some(y) => NaiveDate::from_ymd_opt(y.get(1)?.as_str().parse().ok()?, month, day)?,
            None => resolve_yearless(month, day, received_at)?,
        },
    };

    // First clock time AFTER the date fragment (the start of "10am – 11am").
    let (h, m) = match d.clock.captures(after) {
        Some(t) => {
            let mut h: u32 = t.get(1)?.as_str().parse().ok()?;
            let m: u32 = t.get(2).map_or(0, |mm| mm.as_str().parse().unwrap_or(0));
            let pm = t.get(3)?.as_str().eq_ignore_ascii_case("pm");
            if h == 12 { h = 0; }
            if pm { h += 12; }
            (h, m)
        }
        None => (0, 0), // date-only clause: midnight
    };
    let ndt = date.and_hms_opt(h, m, 0)?;
    Some(Utc.from_utc_datetime(&ndt))
}

/// Detect a calendar update from a message's surfaces. `None` when the subject
/// carries no calendar-system prefix shape, or a weak prefix has no
/// corroboration (so "Invitation: VIP sale!" never matches).
///
/// SECURITY: callers MUST NOT invoke this for sealed mail (docs/SECURITY.md).
pub fn detect_calendar(
    from_addr: &str,
    from_name: Option<&str>,
    subject: &str,
    body: &str,
    received_at: DateTime<Utc>,
) -> Option<CalendarInfo> {
    let d = detector();
    let subject = subject.trim();

    // 1. Structural subject shape, in order: PREFIX rules (first match wins,
    //    WEAK ones demanding corroboration), then the relayed-RSVP TEMPLATE
    //    (whose `who` is the RESPONDER). Anything else is not a calendar update.
    let (kind, title, clause, responder) = if let Some((rule, rest)) =
        d.prefixes.iter().find_map(|rule| {
            rule.re
                .find(subject)
                .filter(|m| m.start() == 0)
                .map(|m| (rule, &subject[m.end()..]))
        }) {
        // The "@ <date>" clause is also the strongest corroboration a subject
        // can carry, so the split has to happen before the weak-prefix check.
        let (title, clause) = split_title_clause(rest);
        if !rule.strong && clause.is_none() && !has_corroboration(from_addr, body) {
            return None;
        }
        (rule.kind, title, clause, None)
    } else if let Some(cap) = d.rsvp_template.captures(subject) {
        let title = cap.name("title").map_or(String::new(), |m| m.as_str().trim().to_string());
        let clause = cap.name("dates").map(|m| m.as_str());
        let who = cap
            .name("who")
            .map(|m| m.as_str().trim().to_string())
            .filter(|w| !w.is_empty());
        (CalendarKind::Response, title, clause, who)
    } else {
        return None;
    };

    // 2. Best-effort extraction. Empty titles become None, never "".
    let event_title = (!title.is_empty()).then_some(title);
    let starts_at = clause.and_then(|c| parse_starts_at(c, received_at));
    let organizer = d
        .organizer_line
        .captures(body)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .or_else(|| match kind {
            // A relayed RSVP is ABOUT the responder, so carry their name (else
            // the from-address). A bare prefix RSVP has no such name and its
            // sender is the attendee, not the organizer: stays None.
            CalendarKind::Response => responder.or_else(|| {
                d.rsvp_template
                    .is_match(subject)
                    .then(|| from_addr.to_string())
            }),
            _ => from_name
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(String::from)
                .or_else(|| Some(from_addr.to_string())),
        });

    Some(CalendarInfo {
        kind,
        event_title,
        starts_at,
        organizer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, dy: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, dy, 12, 0, 0).unwrap()
    }

    fn ts(y: i32, mo: u32, dy: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, dy, h, mi, 0).unwrap()
    }

    // ---- positives: Google Calendar subject shapes ------------------------

    #[test]
    fn google_invitation_with_date_clause() {
        // The canonical Google invite: prefix + "@ <date>" clause. The clause
        // itself corroborates — no body machinery needed.
        let c = detect_calendar(
            "sam@gmail.com",
            Some("Sam Doe"),
            "Invitation: Design review @ Wed Jul 22, 2026 10am - 11am (PDT) (bboynton97@gmail.com)",
            "",
            at(2026, 7, 20),
        )
        .expect("google invite");
        assert_eq!(c.kind, CalendarKind::Invite);
        assert_eq!(c.event_title.as_deref(), Some("Design review"));
        assert_eq!(c.starts_at, Some(ts(2026, 7, 22, 10, 0)));
        assert_eq!(c.organizer.as_deref(), Some("Sam Doe"));
    }

    #[test]
    fn google_updated_invitation_is_update() {
        let c = detect_calendar(
            "sam@gmail.com",
            Some("Sam Doe"),
            "Updated invitation: Standup @ Weekly from 9:30am to 9:45am (PDT)",
            "",
            at(2026, 7, 20),
        )
        .expect("updated invitation");
        assert_eq!(c.kind, CalendarKind::Update);
        assert_eq!(c.event_title.as_deref(), Some("Standup"));
        // Recurrence clause carries no month-day: starts_at stays None.
        assert_eq!(c.starts_at, None);
    }

    #[test]
    fn google_canceled_event_is_cancellation() {
        let c = detect_calendar(
            "calendar-notification@google.com",
            Some("Google Calendar"),
            "Canceled event: 1:1 Braelyn/Sam @ Thu Jul 23, 2026 2pm - 2:30pm (PDT)",
            "This event has been canceled.",
            at(2026, 7, 20),
        )
        .expect("cancellation");
        assert_eq!(c.kind, CalendarKind::Cancellation);
        assert_eq!(c.event_title.as_deref(), Some("1:1 Braelyn/Sam"));
        assert_eq!(c.starts_at, Some(ts(2026, 7, 23, 14, 0)));
    }

    #[test]
    fn accepted_rsvp_with_body_machinery_is_response() {
        // Outlook-style RSVP: weak "Accepted:" prefix, corroborated by the
        // body. The sender is the ATTENDEE, so organizer stays None.
        let c = detect_calendar(
            "colleague@corp.com",
            Some("Colleague"),
            "Accepted: Budget review",
            "Colleague has accepted this meeting.",
            at(2026, 7, 20),
        )
        .expect("rsvp response");
        assert_eq!(c.kind, CalendarKind::Response);
        assert_eq!(c.event_title.as_deref(), Some("Budget review"));
        assert_eq!(c.organizer, None, "RSVP sender is the attendee, not organizer");
    }

    #[test]
    fn declined_and_tentative_shapes_are_responses() {
        let c = detect_calendar(
            "a@corp.com",
            None,
            "Declined: Sprint planning @ Mon Jul 27, 2026 1pm - 2pm (PDT)",
            "",
            at(2026, 7, 20),
        )
        .expect("declined");
        assert_eq!(c.kind, CalendarKind::Response);

        // "Tentatively accepted:" is a STRONG shape — no corroboration needed.
        let c = detect_calendar(
            "a@corp.com",
            None,
            "Tentatively accepted: Offsite",
            "",
            at(2026, 7, 20),
        )
        .expect("tentative");
        assert_eq!(c.kind, CalendarKind::Response);
        assert_eq!(c.event_title.as_deref(), Some("Offsite"));
    }

    #[test]
    fn outlook_updated_with_ics_mention_is_update() {
        let c = detect_calendar(
            "organizer@corp.com",
            Some("Org Anizer"),
            "Updated: Quarterly all-hands",
            "When: Tuesday, July 28, 2026 3:00 PM\nWhere: Main hall\nattachment: invite.ics",
            at(2026, 7, 20),
        )
        .expect("outlook update");
        assert_eq!(c.kind, CalendarKind::Update);
        assert_eq!(c.event_title.as_deref(), Some("Quarterly all-hands"));
        assert_eq!(c.organizer.as_deref(), Some("Org Anizer"));
    }

    // ---- relayed-RSVP template (personal-address senders) ------------------

    #[test]
    fn relayed_rsvp_from_personal_address_is_response() {
        // An RSVP relayed from a PERSONAL address: no calendar-service sender
        // and no prefix, so the structured subject template has to carry it.
        let c = detect_calendar(
            "ellie@elliehuxtable.com",
            Some("Ellie Huxtable"),
            "Ellie Huxtable accepted your Indiana trip invitation (Aug 5\u{2013}10, 2026)",
            "",
            at(2026, 7, 20),
        )
        .expect("relayed RSVP");
        assert_eq!(c.kind, CalendarKind::Response);
        assert_eq!(c.event_title.as_deref(), Some("Indiana trip"));
        assert_eq!(c.starts_at, Some(ts(2026, 8, 5, 0, 0)), "range start, en dash");
        assert_eq!(c.organizer.as_deref(), Some("Ellie Huxtable"));
    }

    #[test]
    fn relayed_rsvp_declined_variant_and_hyphen_range() {
        let c = detect_calendar(
            "sam@sam.dev",
            None,
            "Sam Chen declined your Q3 offsite invitation (Sep 14-16, 2026)",
            "",
            at(2026, 7, 20),
        )
        .expect("declined variant");
        assert_eq!(c.kind, CalendarKind::Response);
        assert_eq!(c.event_title.as_deref(), Some("Q3 offsite"));
        assert_eq!(c.starts_at, Some(ts(2026, 9, 14, 0, 0)), "hyphen range start");
        assert_eq!(c.organizer.as_deref(), Some("Sam Chen"));
    }

    #[test]
    fn relayed_rsvp_single_date_and_no_date_both_work() {
        let c = detect_calendar(
            "e@e.com",
            None,
            "Ellie Huxtable tentatively accepted your dinner invitation (Aug 5, 2026)",
            "",
            at(2026, 7, 20),
        )
        .expect("single date");
        assert_eq!(c.starts_at, Some(ts(2026, 8, 5, 0, 0)));

        let c = detect_calendar(
            "e@e.com",
            None,
            "Ellie Huxtable accepted your housewarming invitation",
            "",
            at(2026, 7, 20),
        )
        .expect("dateless template still matches");
        assert_eq!(c.event_title.as_deref(), Some("housewarming"));
        assert_eq!(c.starts_at, None);
    }

    #[test]
    fn body_only_accepted_your_invitation_does_not_match() {
        // A human WRITING "I accepted your invitation" in a body (or a plain
        // subject) is prose, not the structured template.
        let c = detect_calendar(
            "friend@gmail.com",
            Some("Friend"),
            "Re: Indiana trip",
            "I accepted your invitation, see you there!",
            at(2026, 7, 20),
        );
        assert!(c.is_none(), "body prose must not match: {c:?}");
        // Subject "I accepted your invitation" has no title between "your" and
        // "invitation" — the template requires one.
        let c = detect_calendar(
            "friend@gmail.com",
            Some("Friend"),
            "I accepted your invitation",
            "",
            at(2026, 7, 20),
        );
        assert!(c.is_none(), "titleless template must not match: {c:?}");
    }

    // ---- extraction details ----------------------------------------------

    #[test]
    fn title_with_literal_at_sign_is_not_truncated() {
        // "Coffee @ Blue Bottle" — the first " @ " suffix is not date-ish, so
        // the split walks past it to the real date clause.
        let c = detect_calendar(
            "sam@gmail.com",
            None,
            "Invitation: Coffee @ Blue Bottle @ Fri Jul 24, 2026 8am - 9am (PDT)",
            "",
            at(2026, 7, 20),
        )
        .expect("invite");
        assert_eq!(c.event_title.as_deref(), Some("Coffee @ Blue Bottle"));
        assert_eq!(c.starts_at, Some(ts(2026, 7, 24, 8, 0)));
    }

    #[test]
    fn yearless_date_anchors_to_received_year() {
        let c = detect_calendar(
            "sam@gmail.com",
            None,
            "Invitation: Dinner @ Tue Jul 28 6pm (PDT)",
            "",
            at(2026, 7, 20),
        )
        .expect("invite");
        assert_eq!(c.starts_at, Some(ts(2026, 7, 28, 18, 0)));
        // December receipt of a January date rolls forward a year.
        let c = detect_calendar(
            "sam@gmail.com",
            None,
            "Invitation: Kickoff @ Mon Jan 4 9am",
            "",
            at(2026, 12, 28),
        )
        .expect("invite");
        assert_eq!(c.starts_at, Some(ts(2027, 1, 4, 9, 0)));
    }

    #[test]
    fn date_only_clause_lands_at_midnight_and_organizer_body_line_wins() {
        let c = detect_calendar(
            "calendar-notification@google.com",
            Some("Google Calendar"),
            "Invitation: Company holiday @ Fri Dec 25, 2026",
            "Organizer: Alice Chen <alice@corp.com>",
            at(2026, 7, 20),
        )
        .expect("invite");
        assert_eq!(c.starts_at, Some(ts(2026, 12, 25, 0, 0)));
        assert_eq!(c.organizer.as_deref(), Some("Alice Chen <alice@corp.com>"));
    }

    #[test]
    fn noon_and_midnight_clock_edges_parse() {
        let c = detect_calendar(
            "s@g.com", None,
            "Invitation: Lunch @ Wed Jul 22, 2026 12pm - 1pm",
            "", at(2026, 7, 20),
        )
        .expect("invite");
        assert_eq!(c.starts_at, Some(ts(2026, 7, 22, 12, 0)), "12pm is noon");
        let c = detect_calendar(
            "s@g.com", None,
            "Invitation: Midnight run @ Wed Jul 22, 2026 12am",
            "", at(2026, 7, 20),
        )
        .expect("invite");
        assert_eq!(c.starts_at, Some(ts(2026, 7, 22, 0, 0)), "12am is midnight");
    }

    #[test]
    fn empty_title_after_stripping_is_none() {
        let c = detect_calendar(
            "s@g.com", None,
            "Invitation: @ Wed Jul 22, 2026 10am",
            "", at(2026, 7, 20),
        )
        .expect("still a calendar update");
        assert_eq!(c.event_title, None);
    }

    // ---- negatives: structure required, topics rejected -------------------

    #[test]
    fn newsletter_mentioning_calendar_is_not_a_calendar_update() {
        // Topical "calendar" prose with no structural subject shape.
        let c = detect_calendar(
            "news@shop.com",
            Some("Shop News"),
            "Your July events calendar is here!",
            "Check our calendar of sales events. Add to calendar! Unsubscribe.",
            at(2026, 7, 20),
        );
        assert!(c.is_none(), "topical calendar mention must not match: {c:?}");
    }

    #[test]
    fn marketing_invitation_without_corroboration_is_rejected() {
        // Weak "Invitation:" prefix, no date clause, no calendar machinery.
        let c = detect_calendar(
            "promo@retail.com",
            Some("Retail Co"),
            "Invitation: VIP sale this weekend",
            "Exclusive deals for our best customers. Shop now. Unsubscribe.",
            at(2026, 7, 20),
        );
        assert!(c.is_none(), "marketing invitation must not match: {c:?}");
    }

    #[test]
    fn plain_mail_from_calendarish_sender_is_not_matched() {
        // Calendar-notifier sender but NO structural subject shape: the sender
        // alone never classifies.
        let c = detect_calendar(
            "calendar-notification@google.com",
            Some("Google Calendar"),
            "Reminder settings have changed",
            "You can change your notification settings at any time.",
            at(2026, 7, 20),
        );
        assert!(c.is_none(), "sender alone must not classify: {c:?}");
    }

    #[test]
    fn accepted_application_without_calendar_machinery_is_rejected() {
        // Weak "Accepted:" prefix on a non-calendar subject with no
        // corroboration — must not match.
        let c = detect_calendar(
            "admissions@school.edu",
            Some("Admissions"),
            "Accepted: your application status update",
            "Congratulations! Your application has been accepted.",
            at(2026, 7, 20),
        );
        assert!(c.is_none(), "non-calendar Accepted: must not match: {c:?}");
    }

    #[test]
    fn otp_shaped_email_is_not_a_calendar_update() {
        // Ingest never calls this for sealed mail; it must not match anyway.
        let c = detect_calendar(
            "noreply@bank.com",
            None,
            "Your verification code",
            "Your one-time passcode is 483920.",
            at(2026, 7, 20),
        );
        assert!(c.is_none());
    }
}
