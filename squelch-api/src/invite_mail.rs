//! The invite mail: what a friend actually receives.
//!
//! THIS FILE IS PRODUCT COPY, and it is reviewed as copy rather than as code.
//! Anything changed here changes what goes out over a user's own name, from
//! their own mailbox, to somebody who did not ask for it. That is the whole
//! reason the rules below are rules.
//!
//! THE VOICE IS THE USER'S, NOT THE PRODUCT'S. The mail is sent by the user's
//! Gmail, so it is written in first person and it says as little marketing as
//! it can get away with. Every sentence in it is one a person could plausibly
//! have typed. That is not decoration: a marketing email arriving from a
//! friend's real address is worse than no email, because it spends the friend's
//! credibility rather than ours.
//!
//! THE NUMBER IS REAL OR IT IS ABSENT. When the copy says "I only need to
//! manually open about 15% of it", 15 was computed from this user's own mailbox
//! by [`crate::sharing::share_stat`], which refuses to answer at all unless
//! there is enough history behind it, enough mail behind it, and the answer is
//! worth saying. There is no default, no rounded-up guess, and no house
//! average: with no number the mail uses copy that makes no numeric claim.
//! See `share_stat` for the one bias that survives every guard.
//!
//! HOUSE COPY RULES: the product is Passband, the voice is lowercase, and there
//! are no em dashes in anything a person reads.
//!
//! WHAT IS NEVER IN HERE: the share token, anything about the sender's mailbox
//! beyond the one rounded number, and any other recipient. One mail, one
//! friend, and nobody on it can tell who else was invited.

use crate::gmail_write::ReplyParts;

/// The subject line. First person, because of who it is from.
const SUBJECT: &str = "I invited you to Passband";

/// The opening, when this mailbox produced a number worth quoting. `{percent}`
/// is a whole number of percent, already rounded and already bounds-checked.
fn opening_with_stat(percent: u32) -> String {
    format!(
        "I have been using Passband for my email. turns out I only need to manually open about \
         {percent}% of it, and agents filter out the rest. thought you might find it useful too."
    )
}

/// The opening when there is no honest number to give: a new mailbox, a quiet
/// one, or one whose owner opens most of their mail anyway.
///
/// It makes the same claim in words rather than in digits, which is the only
/// honest way to say it without a measurement behind it.
const OPENING_WITHOUT_STAT: &str = "I have been using Passband for my email. agents filter out the noise so I only open what \
     actually needs me. thought you might find it useful too.";

/// The line above the code.
const HANDOFF: &str = "I had an invite spare:";

/// The expiry line. A DAY IS NOT DAYS: the copy is a sentence somebody could
/// have typed, and "one use, 1 days" is a sentence nobody has ever typed.
fn expiry_line(days: i64) -> String {
    if days == 1 {
        "one use, and it expires tomorrow.".to_string()
    } else {
        format!("one use, {days} days.")
    }
}

/// The footer. The one piece of branding on a mail that is otherwise entirely
/// personal, and deliberately the quietest thing in it.
const FOOTER: &str = "sent with Passband";

/// What the mail is made of. Assembled by the caller so that everything
/// variable is visible in one place and nothing in this module reads the world.
pub struct InviteCopy<'a> {
    /// The invite code, `XXXX-XXXX-XXXX-XXXX`.
    pub code: &'a str,
    /// Where it is redeemed, from the control plane rather than hardcoded.
    pub signup_url: &'a str,
    /// Whole days until the code lapses, from the control plane's own expiry,
    /// so the mail cannot promise a window the code does not have.
    pub expires_in_days: i64,
    /// This mailbox's open rate as a whole percent, or `None` when there is no
    /// honest one. See the module header.
    pub open_percent: Option<u32>,
    /// The user's own line, if they wrote one. Their words, above ours.
    pub note: Option<&'a str>,
}

/// The one-click link: the signup page with the code already in the field.
///
/// The same trade the mailed invite makes on the control plane's side (see
/// `squelch_control::resend`): the code is in a query string, so it is in the
/// recipient's history and in any proxy's log until it is redeemed or lapses,
/// and single use is what bounds that. The bare code sits below it for the
/// client that mangles the link, the forwarded copy, and the person finishing
/// on another device.
fn invite_link(copy: &InviteCopy<'_>) -> String {
    format!(
        "{}/?invite={}",
        copy.signup_url.trim_end_matches('/'),
        percent_encode(copy.code)
    )
}

/// Percent-encode everything that is not unreserved. A code is only ever
/// Crockford base32 and dashes, which is exactly why this is here: "only ever,
/// today" is the assumption that breaks quietly later.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The plain-text part, which is also what the guard scans and what the user
/// is shown as a preview before they send.
///
/// The code sits ALONE ON A LINE so that every mail client in the world lets
/// somebody select it. The link above it is the fast path, not the only one.
pub fn text_body(copy: &InviteCopy<'_>) -> String {
    let opening = match copy.open_percent {
        Some(percent) => opening_with_stat(percent),
        None => OPENING_WITHOUT_STAT.to_string(),
    };
    let note = match copy.note.map(str::trim).filter(|n| !n.is_empty()) {
        // The user's own words go FIRST and stand alone. Nothing is added to
        // them, nothing is corrected in them, and the product's copy starts
        // underneath.
        Some(note) => format!("{note}\n\n"),
        None => String::new(),
    };
    format!(
        "{note}{opening}

{HANDOFF}

{link}

or go to {signup} and paste this in:

{code}

{expiry}

--
{FOOTER}
",
        link = invite_link(copy),
        signup = copy.signup_url,
        code = copy.code,
        expiry = expiry_line(copy.expires_in_days),
    )
}

/// The HTML part.
///
/// Everything interpolated is escaped, including the values this daemon
/// produced itself: "this one was checked already" is how the exception becomes
/// the rule. The note is the one genuinely user-authored string, and it is
/// escaped like everything else and rendered as plain lines rather than as
/// markup, because a mail going out under somebody's name is not a place to
/// start interpreting formatting.
///
/// NO REMOTE IMAGES AND NO MARK. The control plane's own invite mail carries a
/// wordmark, because it comes from Passband. This one comes from a person, and
/// a logo in it is exactly the thing that turns a personal note into an ad.
pub fn html_body(copy: &InviteCopy<'_>) -> String {
    let opening = escape_html(&match copy.open_percent {
        Some(percent) => opening_with_stat(percent),
        None => OPENING_WITHOUT_STAT.to_string(),
    });
    let note = match copy.note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => format!("<p>{}</p>\n", escape_html(note).replace('\n', "<br>\n")),
        None => String::new(),
    };
    let link = escape_html(&invite_link(copy));
    let code = escape_html(copy.code);
    let signup = escape_html(copy.signup_url);
    format!(
        r#"<div style="font: 16px/1.55 ui-sans-serif, system-ui, -apple-system, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif; color: #1a1a1a;">
{note}<p>{opening}</p>
<p>{HANDOFF}</p>
<p><a href="{link}" style="display: inline-block; background: #1a1a1a; color: #fbfaf8; text-decoration: none; padding: 12px 22px; border-radius: 8px; font-weight: 500;">Set up your mailbox</a></p>
<p style="color: #6b6b6b;">or go to <a href="{signup}">{signup}</a> and paste this in:</p>
<p style="font: 18px/1.4 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; letter-spacing: 0.04em; background: #f3f1ed; border-radius: 8px; padding: 14px 16px; display: inline-block;">{code}</p>
<p style="color: #6b6b6b;">{expiry}</p>
<p style="color: #9a9a9a; font-size: 13px; margin-top: 28px;">{FOOTER}</p>
</div>
"#,
        expiry = escape_html(&expiry_line(copy.expires_in_days)),
    )
}

/// Escape text for interpolation into HTML markup. `&` first, or it would
/// double-escape the entities emitted after it.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The whole message, ready for [`crate::gmail_write::build_reply_rfc822`].
///
/// A COLD SEND: no `In-Reply-To`, no `References`, no thread. It is a new
/// conversation with somebody, and threading it onto anything would be wrong.
pub fn compose(to: &str, copy: &InviteCopy<'_>) -> ReplyParts {
    ReplyParts {
        to: to.to_string(),
        cc: None,
        subject: SUBJECT.to_string(),
        body: text_body(copy),
        in_reply_to: None,
        references: None,
        body_html: Some(html_body(copy)),
        // NEVER. A read receipt on an invite would report a stranger's open of
        // a mail they did not ask for, back to somebody they have not agreed to
        // be tracked by. Tracking is per-send and opt-in everywhere else in
        // this daemon; here it is simply not offered.
        pixel_url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "ABCD-EFGH-JKMN-PQRS";
    const SIGNUP: &str = "https://signup.passband.app";

    fn copy(open_percent: Option<u32>, note: Option<&'static str>) -> InviteCopy<'static> {
        InviteCopy {
            code: CODE,
            signup_url: SIGNUP,
            expires_in_days: 30,
            open_percent,
            note,
        }
    }

    /// Both parts carry the code, the link, and the true expiry, and the code
    /// is selectable on its own line in the text part.
    #[test]
    fn both_parts_carry_the_code_and_where_to_spend_it() {
        let c = copy(Some(15), None);
        let text = text_body(&c);
        let html = html_body(&c);
        for part in [&text, &html] {
            assert!(part.contains(CODE), "{part}");
            assert!(part.contains(SIGNUP), "{part}");
            assert!(part.contains("one use, 30 days"), "{part}");
            // House copy rule.
            assert!(!part.contains('\u{2014}'), "{part}");
        }
        assert!(text.contains(&format!("\n\n{CODE}\n\n")), "{text}");
        assert!(text.contains(&format!("{SIGNUP}/?invite={CODE}")), "{text}");
    }

    /// The expiry is the control plane's, not a constant compiled in here: a
    /// mail that says 30 days while the code lasts 14 is a mail that lies.
    #[test]
    fn the_expiry_is_whatever_it_was_told() {
        let mut c = copy(None, None);
        c.expires_in_days = 14;
        assert!(text_body(&c).contains("one use, 14 days"));
        assert!(html_body(&c).contains("one use, 14 days"));
    }

    /// One day is not "1 days". The copy has to read like a sentence somebody
    /// typed, in every branch, including the one nobody looks at.
    #[test]
    fn the_last_day_reads_like_a_sentence() {
        let mut c = copy(None, None);
        c.expires_in_days = 1;
        for part in [text_body(&c), html_body(&c)] {
            assert!(!part.contains("1 days"), "{part}");
            assert!(part.contains("expires tomorrow"), "{part}");
        }
    }

    /// The number appears only when there is one, and the no-number copy makes
    /// no numeric claim at all rather than a softer one.
    #[test]
    fn the_stat_is_present_or_the_sentence_is_different() {
        let with = text_body(&copy(Some(15), None));
        assert!(with.contains("about 15% of it"), "{with}");

        let without = text_body(&copy(None, None));
        assert!(!without.contains('%'), "{without}");
        assert!(without.contains("filter out the noise"), "{without}");
        // And no digits smuggled in as words either.
        assert!(!without.contains("percent"), "{without}");
    }

    /// The user's line goes first, in their words, and nothing is added to it.
    #[test]
    fn a_personal_note_leads_and_is_left_alone() {
        let text = text_body(&copy(Some(15), Some("thought of you on this one")));
        assert!(text.starts_with("thought of you on this one\n\n"), "{text}");
        // Blank and whitespace-only notes are the same as no note.
        let none = text_body(&copy(Some(15), Some("   \n ")));
        assert!(none.starts_with("I have been using Passband"), "{none}");
    }

    /// The one genuinely user-authored string cannot become markup. It is going
    /// into a mail sent from somebody's real address, so a note that closes the
    /// tag it is inside of is not an option.
    #[test]
    fn a_note_cannot_carry_markup_into_the_html() {
        let html = html_body(&copy(None, Some("<script>alert(1)</script> & <b>hi</b>")));
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<b>hi</b>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
    }

    /// A cold send, and never a tracked one.
    #[test]
    fn composes_a_new_conversation_with_no_pixel() {
        let parts = compose("friend@example.com", &copy(Some(15), None));
        assert_eq!(parts.to, "friend@example.com");
        assert_eq!(parts.subject, SUBJECT);
        assert!(parts.in_reply_to.is_none());
        assert!(parts.references.is_none());
        assert!(parts.cc.is_none());
        assert!(
            parts.pixel_url.is_none(),
            "an invite must never carry a tracking pixel"
        );
        assert!(parts.body_html.is_some());
    }
}
