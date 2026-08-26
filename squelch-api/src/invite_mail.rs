//! The invite mail: a DRAFT the user edits, and the one block they cannot.
//!
//! THIS FILE IS PRODUCT COPY, and it is reviewed as copy rather than as code —
//! but only as a STARTING POINT now. What goes out is whatever the person
//! pressing send left in the composer, over their own name, from their own
//! mailbox. What is written here is the draft they open on.
//!
//! THE VOICE IS THE USER'S, NOT THE PRODUCT'S, which is why it can be handed
//! over: the mail is sent by their Gmail and reads as something a person typed,
//! so letting them actually type it is the honest end of that idea rather than
//! a departure from it.
//!
//! THE ONE THING THEY DO NOT EDIT IS `{{invite}}`. It expands, per recipient,
//! into the link, that friend's own code, and the true expiry. A single marker
//! rather than four, deliberately: everything inside it is a FACT THE DAEMON
//! GUARANTEES, and prose the user can reword sits outside. Nobody has to be
//! trusted to keep a link and a code and an expiry in step by hand, and nobody
//! has to look at four sets of braces to write two sentences.
//!
//! MARKDOWN IN, exactly like the composer (`body_format: "markdown"`), and the
//! HTML half is rendered by [`crate::markdown`] from the same source the
//! outbound guard scanned. That is not a convenience: it means an invite cannot
//! carry markup the composer would have refused, and there is no second
//! renderer here to drift from the first.
//!
//! THE NUMBER IS REAL OR IT IS ABSENT. When the draft says "about 15% of my
//! emails",
//! 15 came from this user's own mailbox via [`crate::sharing::share_stat`],
//! which refuses to answer at all without enough history, enough mail, and an
//! answer worth saying. There is no default and no house average.
//!
//! HOUSE COPY RULES: the product is Passband, the voice is lowercase, and there
//! are no em dashes in anything a person reads.

use crate::gmail_write::ReplyParts;

/// The marker the invite block replaces. Doubled braces because a single brace
/// is a character people type; this pair is not.
pub const INVITE_MARKER: &str = "{{invite}}";

/// Where the footer link points. The one piece of branding on a mail that is
/// otherwise entirely personal, and the only URL in here that is not the
/// deployment's own.
const SITE_URL: &str = "https://passband.app";

/// The subject the draft opens on. First person, because of who it is from.
pub const DEFAULT_SUBJECT: &str = "I invited you to Passband";

/// The body the draft opens on, in markdown.
///
/// `open_percent` is this mailbox's real open rate, or `None` when there is no
/// honest one — in which case the opening makes the same claim in words rather
/// than in digits, which is the only truthful way to say it without a
/// measurement behind it.
pub fn default_body(open_percent: Option<u32>) -> String {
    let opening = match open_percent {
        Some(percent) => format!(
            "I have been using Passband for my email. turns out I only need to manually open \
             about {percent}% of my emails and agents sort the rest. I thought you might find \
             it useful too."
        ),
        None => "I have been using Passband for my email. agents sort it so I only open what \
                 actually needs me. I thought you might find it useful too."
            .to_string(),
    };
    format!(
        "{opening}

I had an invite spare:

{INVITE_MARKER}

--
sent with [Passband]({SITE_URL})
"
    )
}

/// What one recipient's invite block expands to.
pub struct InviteBlock<'a> {
    /// The invite code, `XXXX-XXXX-XXXX-XXXX`.
    pub code: &'a str,
    /// Where it is redeemed, from the control plane rather than hardcoded.
    pub signup_url: &'a str,
    /// Whole days until it lapses, from the control plane's own expiry, so the
    /// mail cannot promise a window the code does not have.
    pub expires_in_days: i64,
}

/// The one-click link: the signup page with the code already in the field.
///
/// The same trade the mailed invite makes on the control plane's side (see
/// `squelch_control::resend`): the code is in a query string, so it is in the
/// recipient's history and in any proxy's log until it is redeemed or lapses,
/// and single use is what bounds that. The bare code sits below it for the
/// client that mangles the link, the forwarded copy, and the person finishing
/// on another device.
fn invite_link(block: &InviteBlock<'_>) -> String {
    format!(
        "{}/?invite={}",
        block.signup_url.trim_end_matches('/'),
        percent_encode(block.code)
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

/// The block itself, in markdown.
///
/// The code sits in a fenced span on its own line so that every mail client
/// lets somebody select it, and so the markdown renderer cannot take a dash in
/// it for anything else. The link above it is the fast path, not the only one.
fn render_block(block: &InviteBlock<'_>) -> String {
    let expiry = if block.expires_in_days == 1 {
        "one use, and it expires tomorrow.".to_string()
    } else {
        format!("one use, {} days.", block.expires_in_days)
    };
    format!(
        "[Set up your mailbox]({link})

or go to {signup} and paste this in:

`{code}`

{expiry}",
        link = invite_link(block),
        signup = block.signup_url,
        code = block.code,
    )
}

/// Put one recipient's invite into the user's draft.
///
/// Every occurrence, not just the first: somebody who pasted the marker twice
/// meant it twice, and a half-substituted body would mail a friend the literal
/// characters `{{invite}}`.
pub fn fill(body: &str, block: &InviteBlock<'_>) -> String {
    body.replace(INVITE_MARKER, &render_block(block))
}

/// Whether a draft still carries the marker.
///
/// Checked before ANY code is minted. An invite mail with no invite in it is a
/// mail nobody can act on, and a code minted for one is quota the user spent on
/// nothing.
pub fn has_marker(body: &str) -> bool {
    body.contains(INVITE_MARKER)
}

/// The whole message, ready for [`crate::gmail_write::build_reply_rfc822`].
///
/// A COLD SEND: no `In-Reply-To`, no `References`, no thread. It is a new
/// conversation with somebody, and threading it onto anything would be wrong.
///
/// `body` is the FILLED markdown; the HTML half is rendered from it by the same
/// module the composer's sends go through.
pub fn compose(to: &str, subject: &str, body: &str) -> ReplyParts {
    ReplyParts {
        to: to.to_string(),
        cc: None,
        // ONE invite, ONE recipient. An invite is addressed to a person by name;
        // a blind copy list on it would mail a stranger something that names
        // them and hides who else got it.
        bcc: None,
        subject: subject.to_string(),
        body: body.to_string(),
        in_reply_to: None,
        references: None,
        body_html: Some(crate::markdown::render_email_html(body)),
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

    fn block() -> InviteBlock<'static> {
        InviteBlock {
            code: CODE,
            signup_url: SIGNUP,
            expires_in_days: 30,
        }
    }

    /// The draft the composer opens on: prose, one marker, and a footer that
    /// links to the site.
    #[test]
    fn the_draft_carries_one_marker_and_a_linked_footer() {
        let body = default_body(Some(15));
        assert!(has_marker(&body), "{body}");
        assert_eq!(body.matches(INVITE_MARKER).count(), 1, "{body}");
        assert!(body.contains("about 15% of my emails"), "{body}");
        assert!(
            body.contains(&format!("sent with [Passband]({SITE_URL})")),
            "the footer links to the site: {body}"
        );
        // House copy rule.
        assert!(!body.contains('\u{2014}'), "{body}");
        // No code, no link, no expiry in the DRAFT: those are facts, and they
        // arrive per recipient.
        assert!(!body.contains("one use"), "{body}");
    }

    /// The number appears only when there is one, and the no-number draft makes
    /// no numeric claim at all rather than a softer one.
    #[test]
    fn the_stat_is_present_or_the_sentence_is_different() {
        let with = default_body(Some(15));
        assert!(with.contains("about 15% of my emails"), "{with}");

        let without = default_body(None);
        assert!(!without.contains('%'), "{without}");
        assert!(without.contains("agents sort it"), "{without}");
        assert!(!without.contains("percent"), "{without}");
    }

    /// Filling puts the link, the code and the true expiry where the marker was
    /// and leaves everything the user wrote alone.
    #[test]
    fn filling_replaces_the_marker_and_nothing_else() {
        let draft = "my own words\n\n{{invite}}\n\nand my own sign-off";
        let filled = fill(draft, &block());
        assert!(filled.starts_with("my own words"), "{filled}");
        assert!(filled.ends_with("and my own sign-off"), "{filled}");
        assert!(!filled.contains(INVITE_MARKER), "{filled}");
        assert!(filled.contains(CODE), "{filled}");
        assert!(
            filled.contains(&format!("{SIGNUP}/?invite={CODE}")),
            "{filled}"
        );
        assert!(filled.contains("one use, 30 days."), "{filled}");
    }

    /// Every occurrence: a half-substituted body would mail somebody the
    /// literal braces.
    #[test]
    fn filling_replaces_every_marker() {
        let filled = fill("{{invite}} and again {{invite}}", &block());
        assert!(!filled.contains(INVITE_MARKER), "{filled}");
        // Counted on the block's own first line, not on the code: the code
        // appears TWICE inside one block (in the link and on its own), so
        // counting it would pass for the wrong reason.
        assert_eq!(filled.matches("Set up your mailbox").count(), 2, "{filled}");
    }

    /// The expiry is the control plane's, not a constant compiled in here, and
    /// one day is not "1 days".
    #[test]
    fn the_expiry_is_whatever_it_was_told() {
        let mut b = block();
        b.expires_in_days = 14;
        assert!(fill(INVITE_MARKER, &b).contains("one use, 14 days."));
        b.expires_in_days = 1;
        let last = fill(INVITE_MARKER, &b);
        assert!(!last.contains("1 days"), "{last}");
        assert!(last.contains("expires tomorrow"), "{last}");
    }

    /// A draft with the marker torn out is refused before anything is minted;
    /// this is the predicate that does it.
    #[test]
    fn a_draft_without_the_marker_is_recognisable() {
        assert!(has_marker(&default_body(None)));
        assert!(!has_marker("just some words with no invite in them"));
        // A single brace is a character people type, and is not the marker.
        assert!(!has_marker("{invite}"));
    }

    /// The HTML half comes from the composer's own renderer, which is what
    /// stops an invite carrying markup a composed mail could not.
    #[test]
    fn composes_a_new_conversation_with_no_pixel() {
        let filled = fill(&default_body(Some(15)), &block());
        let parts = compose("friend@example.com", DEFAULT_SUBJECT, &filled);
        assert_eq!(parts.to, "friend@example.com");
        assert_eq!(parts.subject, DEFAULT_SUBJECT);
        assert!(parts.in_reply_to.is_none());
        assert!(parts.references.is_none());
        assert!(parts.cc.is_none());
        assert!(
            parts.pixel_url.is_none(),
            "an invite must never carry a tracking pixel"
        );
        let html = parts.body_html.expect("an html alternative");
        assert!(html.contains(CODE), "{html}");
        assert!(html.contains(SITE_URL), "the footer link survives: {html}");
    }

    /// Typed HTML is characters, not markup — the composer's contract, inherited
    /// here for free by going through the same renderer.
    #[test]
    fn a_users_draft_cannot_carry_markup_into_the_html() {
        let parts = compose(
            "friend@example.com",
            DEFAULT_SUBJECT,
            "<script>alert(1)</script> & <b>hi</b>",
        );
        let html = parts.body_html.unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<b>hi</b>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }
}
