//! Every HTML byte this service serves.
//!
//! NO JAVASCRIPT, no external assets, no fonts to fetch, and a
//! `default-src 'none'` CSP that states it to the browser rather than only to
//! the tests. A signup page that pulls in a third party is a signup page a
//! third party can watch, and the one thing a user does here is decide whether
//! to hand over access to their mail.
//!
//! THE LOGO IS SUBJECT TO THAT RULE RATHER THAN AN EXCEPTION TO IT. These pages
//! carry the Passband masthead so that arriving from the landing page does not
//! feel like leaving the product, and the mark reaches them as an inline `<svg>`
//! element ([`MARK`]) because that is markup rather than a fetch: `img-src` is
//! still `'none'` and the CSP did not have to move an inch to let a logo in. The
//! wordmark's serif is the platform's, for the same reason there is no webfont
//! anywhere here.
//!
//! Everything interpolated goes through [`escape_html`], including strings that
//! were validated elsewhere: "this one was checked already" is how the
//! exception becomes the rule.
//!
//! COPY RULES (house style): the product is Passband, the daemon is squelchd,
//! and there are no em dashes in anything a person reads.

use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};

use crate::store::WaitlistRow;

/// The Content-Security-Policy every page carries. The single allowance is the
/// inline `<style>`; `frame-ancestors 'none'` (with the older `X-Frame-Options`
/// beside it) keeps a signup decision from being framed inside someone else's
/// page. `form-action` names accounts.google.com as well as 'self' because
/// Chrome enforces form-action against the REDIRECT TARGET of a form
/// submission (a long-standing, spec-contested behavior): POST /signup answers
/// 303 to Google consent, and under `form-action 'self'` Chrome silently eats
/// that navigation - the button "does nothing" while the server logs a
/// perfectly healthy signup. Found live on the first real signup, 2026-08-11.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' https://accounts.google.com; frame-ancestors 'none'; base-uri 'none'";

/// Escape text for HTML.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode a query-parameter VALUE. Only the unreserved set survives,
/// so a label or a code can never break out of the parameter it sits in.
pub fn percent_encode(s: &str) -> String {
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

/// The `passband://pair` deep link the native client claims.
///
/// Built HERE from this deployment's own tenant URL and the validated pairing
/// code, never echoed from the warden's answer: an `href` assembled from a
/// remote service's response is an open redirect with our domain in front of it.
pub fn deep_link(tenant_url: &str, pair_code: &str) -> String {
    format!(
        "passband://pair?url={}&code={}",
        percent_encode(tenant_url),
        percent_encode(pair_code)
    )
}

/// Where the client is downloaded from.
///
/// THIS IS WHERE THE DOWNLOAD LIVES NOW. The landing page used to lead with it
/// and leads with the waitlist instead: an app is not worth holding before
/// there is a mailbox behind it to point at, so the download waits until the
/// end of this flow, which is the first moment somebody actually needs it. The
/// pages that name it are the two that finish a flow, [`success`] and
/// [`app_signed_in`], and on both of them it is the literal next step.
///
/// HARDCODED, NOT CONFIGURED, for the reason [`crate::resend::MARK_URL`] is:
/// there is one place the Mac client is published and it is not the operator's
/// origin. A self-hoster's own host serves no zips, so a variable pointing this
/// at their site would only ever produce a 404 with their domain in front of
/// it. `/download/latest` is the site's own redirect to whatever the appcast
/// says shipped last (`passband-site/server.ts`), so this URL does not move
/// when a release does.
const DOWNLOAD_URL: &str = "https://passband.app/download/latest";

/// Where a finished console login sends the browser: the tenant's own console,
/// carrying the pairing code it will claim.
///
/// BUILT HERE FROM TWO THINGS THIS DEPLOYMENT OWNS, and that is the whole reason
/// the console-login route takes no return URL: `tenant_url` comes from this
/// deployment's configured base domain and a label this crate validated, and the
/// code is the warden's answer, shape-checked and then percent-encoded. There is
/// no caller-supplied string anywhere in the result, so there is no open
/// redirect to find. A `?next=` parameter would have been one line and a
/// standing invitation.
pub fn console_callback_url(tenant_url: &str, pair_code: &str) -> String {
    format!(
        "{tenant_url}/console/callback?code={}",
        percent_encode(pair_code)
    )
}

/// The Passband mark, inline, in one colour.
///
/// INLINE AND MONOCHROME BECAUSE OF THE CSP AT THE TOP OF THIS FILE. `img-src`
/// is `'none'`, so there is no `<img src>` to point at `mark.svg` and no data
/// URI either; an inline `<svg>` element is markup rather than a fetch, so it is
/// the one way a logo reaches these pages without opening a hole for a third
/// party to be watching from. `currentColor` on both the fill and the stroke
/// makes the whole thing take its colour from CSS, which is what lets one copy
/// serve the light and dark grounds.
///
/// GENERATED, NOT DRAWN. The bars are `brand/svg/mark-mono.svg`'s verbatim, and
/// the curve is that file's path run through Douglas-Peucker at a tolerance of
/// one canvas unit: 350 points become 39, 4.5 kB becomes 0.3 kB, and at masthead
/// size (one unit is 0.025 px there) the two are the same picture. `brand/`
/// remains the source; if the geometry ever moves, this is regenerated from it
/// rather than edited.
///
/// THE SIMPLIFICATION KEEPS THE RULE `brand/generate.ts` FAILS ITS BUILD OVER:
/// no bar breaches the line, because the curve is the filter admitting the bars
/// and not a decoration laid over them. Checked against every bar across its
/// full width, the original clears by 0.59 units and this by 0.32; a cheaper
/// simplification that looked identical at 26 px put three bars through the
/// right shoulder, which is why the tolerance is where it is.
///
/// `aria-hidden` because the wordmark beside it already says Passband, and a
/// screen reader should not say it twice.
const MARK: &str = concat!(
    r#"<svg viewBox="0 221 1024 557" aria-hidden="true" fill="currentColor"><g>"#,
    r#"<rect x="129.8" y="697.3" width="44.4" height="74.7" rx="22.2"/>"#,
    r#"<rect x="189.8" y="600.7" width="44.4" height="171.3" rx="22.2"/>"#,
    r#"<rect x="249.8" y="550.1" width="44.4" height="221.9" rx="22.2"/>"#,
    r#"<rect x="309.8" y="420.4" width="44.4" height="351.6" rx="22.2"/>"#,
    r#"<rect x="369.8" y="297.4" width="44.4" height="474.6" rx="22.2"/>"#,
    r#"<rect x="429.8" y="370.0" width="44.4" height="402.0" rx="22.2"/>"#,
    r#"<rect x="489.8" y="253.1" width="44.4" height="518.9" rx="22.2"/>"#,
    r#"<rect x="549.8" y="329.5" width="44.4" height="442.5" rx="22.2"/>"#,
    r#"<rect x="609.8" y="356.3" width="44.4" height="415.7" rx="22.2"/>"#,
    r#"<rect x="669.8" y="362.2" width="44.4" height="409.8" rx="22.2"/>"#,
    r#"<rect x="729.8" y="555.9" width="44.4" height="216.1" rx="22.2"/>"#,
    r#"<rect x="789.8" y="619.1" width="44.4" height="152.9" rx="22.2"/>"#,
    r#"<rect x="849.8" y="699.3" width="44.4" height="72.7" rx="22.2"/>"#,
    r#"</g><path fill="none" stroke="currentColor" stroke-width="12" "#,
    r#"stroke-linecap="round" stroke-linejoin="round" d="#,
    r#"M-12 768L12 765L33 760L54 752L69 745L87 733L102 721L117 707L135 686"#,
    r#"L159 653L189 602L279 420L309 364L339 316L369 279L384 265L399 254"#,
    r#"L414 244L429 238L462 229L501 227L552 228L588 235L606 242L621 251"#,
    r#"L636 262L651 275L681 311L711 357L744 418L834 600L861 646L888 685"#,
    r#"L918 718L936 733L954 744L972 753L993 760L1035 768"/></svg>"#,
);

/// The shell every page shares.
///
/// THE MASTHEAD IS PART OF THE SHELL, so every page carries it: the mark, then
/// the name in the brand's serif, top left. It is NOT A LINK, deliberately. The
/// obvious `href` is the marketing site, and the page this sits on hardest is
/// the one where somebody is deciding whether to hand over their mail; a way out
/// in the top left corner of that decision buys continuity with the landing page
/// at the cost of a door out of the flow, and the flow is the thing. Being
/// link-free also keeps it free of configuration, which is what lets the shell
/// carry it rather than every call site having to pass an origin down.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    render(status, title, body, "")
}

/// The same shell on the OPERATOR'S surface: the landing page's dark ground in
/// both palettes, its mark, its type, and none of its selling. See the
/// `body.console` block in the stylesheet for what that means and why the
/// colours are restated there rather than inherited from the dark media query.
///
/// The three pages behind the admin token, and nothing else. A signup page is
/// read once by somebody deciding whether to hand over their mail; this is read
/// every day by one person who already decided.
fn console(status: StatusCode, title: &str, body: &str) -> Response {
    render(status, title, body, r#" class="console""#)
}

fn render(status: StatusCode, title: &str, body: &str, body_class: &str) -> Response {
    let title = escape_html(title);
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex, nofollow">
<title>{title}</title>
<style>
/* THE BRAND ACCENT, stated once. `--brand` is the mark's ink ramp on a light
   ground and its lit ramp on a dark one (brand/README.md's palette), and it is
   the only colour on these pages that is the product's rather than the
   document's: the mark, the links, and the focus ring. The button stays neutral
   because the landing page has no primary button in this blue and inventing one
   here would match nothing. */
:root {{ color-scheme: light dark; --brand: #1f7099; }}
body {{ margin: 0; padding: 2.25rem 1.25rem 3rem; background: #fbfaf8; color: #1a1a1a;
  font: 1rem/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }}
main {{ max-width: 34rem; margin: 0 auto; }}
/* The masthead, aligned to the reading column rather than to the viewport, so
   it sits over the content instead of drifting off into the margin on a wide
   window. On a phone the two are the same corner. */
.brand {{ display: flex; align-items: center; gap: 0.55rem; margin: 0 0 2.25rem; }}
.brand svg {{ height: 1.5rem; width: auto; flex: none; color: var(--brand); }}
/* The brand's ONE serif moment, rationed the way the site and the Swift
   client's Typo ration it: Newsreader for the wordmark and nowhere else, since
   a display serif spread further becomes wallpaper. The webfont itself cannot
   be fetched under this page's CSP, so it is named first for the reader who
   happens to have it installed and the stack falls through to the platform's
   own serif for everybody else. */
.wordmark {{ font-family: "Newsreader", ui-serif, Georgia, "Times New Roman", serif;
  font-size: 1.32rem; font-weight: 500; letter-spacing: -0.005em; }}
/* The admin dashboard is the one page with a table, and the reading width the
   rest of the site is set to folds it into three cramped columns. */
main:has(table) {{ max-width: 44rem; }}
h1 {{ font-size: 1.6rem; font-weight: 600; letter-spacing: -0.02em; margin: 0 0 0.75rem; }}
h2 {{ font-size: 1.05rem; font-weight: 600; margin: 1.75rem 0 0.6rem; }}
p {{ margin: 0 0 1rem; }}
ul, ol {{ margin: 0 0 1.25rem; padding-left: 1.2rem; }}
li {{ margin: 0 0 0.5rem; }}
a {{ color: var(--brand); text-underline-offset: 0.15em; }}
label {{ display: block; font-weight: 600; margin: 0 0 0.35rem; }}
input[type=text], input[type=password], input[type=email] {{ width: 100%; box-sizing: border-box; padding: 0.6rem 0.7rem;
  margin: 0 0 1.25rem;
  border: 1px solid #cdc7bd; border-radius: 6px; background: #fff; color: inherit; font: inherit; }}
button {{ padding: 0.65rem 1.15rem; border: 0; border-radius: 6px; background: #1a1a1a; color: #fbfaf8;
  font: inherit; font-weight: 500; cursor: pointer; }}
/* One ring for every interactive thing, drawn OUTSIDE the control so it never
   changes the layout it lands on. Keyboard-only (`:focus-visible`), so clicking
   a button does not leave it haloed. */
:is(input, button, a, summary):focus-visible {{ outline: 2px solid var(--brand); outline-offset: 2px; }}
/* THE FORM IS THE PAGE. The signup form asks for two things and everything else
   on the page is context for them, so the fields sit on their own ground and
   the prose does not. */
.card {{ background: #fff; border: 1px solid #e6e1d9; border-radius: 10px;
  padding: 1.35rem 1.35rem 1.1rem; margin: 0 0 1.5rem; }}
.card > :last-child {{ margin-bottom: 0; }}
/* A field's hint belongs TO the input, not to the gap under it: the input's own
   bottom margin is dropped so the two read as one block. */
.field {{ margin: 0 0 1.25rem; }}
.field input {{ margin-bottom: 0.4rem; }}
.field .hint {{ margin: 0; }}
.hint {{ color: #6b6b6b; font-size: 0.9rem; }}
/* The second route off this page: a border rather than a second filled button,
   because there is exactly one primary action here and it is not this one. */
.alt {{ border: 1px solid #e6e1d9; border-radius: 10px; padding: 0.9rem 1.1rem;
  margin: 0 0 1.75rem; font-size: 0.95rem; }}
/* Collapsed by default and it stays that way on a submit, because a form post
   that fails re-renders a fresh page: whoever opened this read it already. */
details {{ border-top: 1px solid #e6e1d9; padding: 1rem 0 0; margin: 0 0 1.5rem; }}
/* The line under the button explains what pressing it does, so it sits close to
   it, but not touching: the button has no margin of its own. */
button + .hint {{ margin-top: 0.9rem; }}
summary {{ cursor: pointer; font-weight: 600; }}
details > :not(summary) {{ margin-top: 0.9rem; }}
table {{ width: 100%; border-collapse: collapse; margin: 0 0 1.5rem; font-size: 0.95rem; }}
th {{ font-size: 0.78rem; font-weight: 600; text-transform: uppercase; letter-spacing: 0.05em;
  color: #6b6b6b; }}
th, td {{ text-align: left; vertical-align: top; padding: 0.6rem 0.75rem 0.6rem 0;
  border-bottom: 1px solid #e6e1d9; }}
td form {{ display: inline; }}
/* The second button on a row: same shape, less pull. Re-sending is the rarer
   errand and must not read as the thing to click. */
button.quiet {{ background: none; color: inherit; border: 1px solid #cdc7bd;
  padding: 0.35rem 0.6rem; font-size: 0.85rem; font-weight: 400; }}
.muted {{ color: #6b6b6b; font-size: 0.9rem; }}
.suffix {{ color: #6b6b6b; }}
.stop {{ border-left: 3px solid #b3261e; padding: 0.1rem 0 0.1rem 0.85rem; }}
.code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 1.5rem;
  letter-spacing: 0.08em; background: #efece7; padding: 0.4rem 0.7rem; border-radius: 6px;
  display: inline-block; user-select: all; }}
code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em;
  background: #efece7; padding: 0.1em 0.35em; border-radius: 3px; word-break: break-all; }}
a.button {{ display: inline-block; margin: 0.25rem 0 1.25rem; padding: 0.65rem 1.15rem;
  background: #1a1a1a; color: #fbfaf8; text-decoration: none; border-radius: 6px; font-weight: 500; }}
/* THE DARK GROUND IS THE LANDING PAGE'S GROUND, value for value: #0f0f10 under
   #f5f5f7, cards and inputs as the white washes it builds them from, and the
   mark's lit ramp for the accent. Almost everybody who arrives here arrived
   from that page, which has no light mode at all, so this is the transition
   they actually see. The light ground stays the warm paper it was: there is no
   landing page in light mode to match it to. */
@media (prefers-color-scheme: dark) {{
  :root {{ --brand: #7cc8eb; }}
  body {{ background: #0f0f10; color: #f5f5f7; }}
  .muted, .suffix, .hint, th {{ color: #a0a0a7; }}
  code, .code {{ background: rgba(255, 255, 255, 0.07); }}
  input[type=text], input[type=password], input[type=email] {{ background: rgba(255, 255, 255, 0.04);
    border-color: rgba(255, 255, 255, 0.14); }}
  button, a.button {{ background: #f5f5f7; color: #0f0f10; }}
  button.quiet {{ background: none; color: #f5f5f7; border-color: rgba(255, 255, 255, 0.14); }}
  th, td {{ border-bottom-color: rgba(255, 255, 255, 0.11); }}
  .stop {{ border-left-color: #f2b8b5; }}
  .card {{ background: rgba(255, 255, 255, 0.055); border-color: rgba(255, 255, 255, 0.11); }}
  .alt, details {{ border-color: rgba(255, 255, 255, 0.11); }}
}}
/* THE OPERATOR'S SURFACE.
   The landing page's ground, its mark and its type, and none of its selling: no
   hero, no accent hardware, no motion. This is a page somebody reads in columns
   and presses in a hurry, so it takes the dark field from that page and leaves
   behind everything built to persuade.
   ALWAYS DARK, in both palettes, exactly as the landing page is: nobody has a
   light-mode Passband to match it to. That is why every colour the dark media
   query above sets is RESTATED here instead of being inherited from it, which
   would leave this page reading as warm paper for anybody whose OS is light. */
/* THE ACCENT COMES WITH THE GROUND. `--brand` is the mark's ink ramp on light
   and its lit ramp on dark, chosen by the media query above; this surface is
   dark in BOTH, so it must state the dark one or a light-mode operator gets the
   ink ramp on a near-black field. */
body.console {{ --brand: #7cc8eb; background: #0f0f10; color: #f5f5f7; }}
/* Wider ONLY when there is a board to lay out, the same way the base sheet
   above widens for a table. The login door is one field, and a credential
   stretched across fifty-four rems reads as a page that lost its layout. */
body.console main:has(table) {{ max-width: 54rem; }}
body.console .muted, body.console .hint, body.console th {{ color: #8a8a90; }}
body.console code, body.console .code {{ background: rgba(255, 255, 255, 0.07); }}
body.console :is(input[type=text], input[type=password], input[type=email]) {{
  background: rgba(255, 255, 255, 0.04); border-color: rgba(255, 255, 255, 0.14); }}
body.console button {{ background: #f5f5f7; color: #0f0f10; }}
body.console button.quiet {{ background: none; color: #f5f5f7;
  border-color: rgba(255, 255, 255, 0.2); }}
body.console button.quiet:hover {{ border-color: rgba(255, 255, 255, 0.45); }}
body.console :is(th, td) {{ border-bottom-color: rgba(255, 255, 255, 0.09); }}
body.console .stop {{ border-left-color: #f2b8b5; }}
/* The board says its own size, so the operator does not count rows to learn it. */
body.console h1 {{ margin-bottom: 0; }}
body.console .tally {{ color: #8a8a90; font-size: 0.9rem; margin: 0.3rem 0 2.5rem; }}
/* Section headings as CONSOLE LABELS rather than document headings. This page
   is three lists, not three chapters, and a heading that competes with the
   addresses under it is a heading in the way. */
body.console h2 {{ font-size: 0.72rem; font-weight: 600; text-transform: uppercase;
  letter-spacing: 0.12em; color: #8a8a90; margin: 2.75rem 0 0.6rem; }}
body.console h2 + .hint {{ margin: -0.2rem 0 1rem; }}
/* THE COLUMNS ARE THE WHOLE POINT. The address is what gets read, so it takes
   the width and the ink. The date and the button are looked at only after a row
   has been picked, so they shrink to their contents and stay out of the scan:
   `width: 1%` with nowrap is how a table is told to give a column exactly what
   it needs and not one pixel more. It is also what stops a date breaking across
   two lines in the middle of `2026-08-14`, which is what the narrow reading
   column used to do to every row on the board. */
body.console :is(td, th) {{ padding: 0.6rem 1.1rem 0.6rem 0; }}
body.console td.who {{ color: #f5f5f7; }}
body.console td.when {{ width: 1%; white-space: nowrap; color: #8a8a90;
  font-size: 0.85rem; font-variant-numeric: tabular-nums; }}
body.console :is(td.act, th:last-child) {{ width: 1%; white-space: nowrap;
  text-align: right; padding-right: 0; }}
/* THE ONE ROW THAT IS WRONG: approved, and nobody was told. Every other button
   in a row is bordered rather than filled, so this is the only filled one among
   them and the eye lands on it. (The direct-invite form at the foot of the page
   is filled too, and should be: it is a form's submit, not one of forty
   identical row actions, and it competes with nothing above it.) */
body.console :is(.flag, .done) {{ display: inline-block; margin-right: 0.55rem;
  padding: 0.12rem 0.55rem; border-radius: 999px; border: 1px solid;
  font-size: 0.72rem; font-weight: 600; letter-spacing: 0.06em;
  text-transform: uppercase; }}
body.console .flag {{ color: #f2b8b5; border-color: rgba(242, 184, 181, 0.35); }}
/* The other end of the funnel, and the reason this column exists: the invite
   was spent, so there is a mailbox. Quieter than the red one, because a board
   should draw the eye to what is broken before what worked. */
body.console .done {{ color: var(--brand); border-color: rgba(124, 200, 235, 0.3); }}
/* The direct invite: field and button on one row, at the foot of the board.
   It used to sit BETWEEN the two lists, which split the one thing this page is
   for (reading down a list of people) into two halves either side of a form. */
body.console .invite {{ display: flex; gap: 0.5rem; max-width: 34rem; }}
body.console .invite input {{ margin: 0; }}
body.console .invite button {{ flex: none; }}
/* The way out, at the foot of the board and quiet: it is the rarest press on
   this page, and the session is long enough now that it needed to exist at all. */
body.console .signout {{ margin: 2.75rem 0 0.6rem; }}
/* A label the screen reader gets and the layout does not. The heading above the
   row and the placeholder in it are enough on screen; neither is a label. */
body.console .sr {{ position: absolute; width: 1px; height: 1px; overflow: hidden;
  clip-path: inset(50%); white-space: nowrap; }}
</style>
</head>
<body{body_class}><main><header class="brand">{mark}<span class="wordmark">Passband</span></header>
{body}</main></body>
</html>
"#,
        title = title,
        mark = MARK,
        body = body,
        body_class = body_class,
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // These pages carry a pairing code, name a mailbox, and on the
            // admin side list addresses that belong to people who are not
            // customers. A shared cache must not keep them, and the click
            // through to Google must not carry this URL as a referer.
            (header::CACHE_CONTROL, "no-store, no-cache"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        html,
    )
        .into_response()
}

/// The signup form.
///
/// TWO FIELDS AND ONE BUTTON ARE THE PAGE, and everything else is arranged
/// around that. The form sits on its own card so the thing to do is the thing
/// that looks like it; each input carries its own hint, tied to it with
/// `aria-describedby` rather than left as loose prose a screen reader reads as
/// an unrelated paragraph.
///
/// THE GRANT IS STILL STATED BEFORE THE BUTTON, in one line naming all three
/// permissions, because the consent screen that follows is Google's and says it
/// in Google's vocabulary. What moved is the LONG version: three paragraphs of
/// scope detail above the fold pushed the form off a phone screen and read as
/// the page's subject, which it is not. It is a `<details>` now, shut by
/// default, open to anybody who wants it, and made of the same markup as before
/// (native disclosure, no JavaScript, nothing to fetch).
///
/// THE COPY IS DELIBERATELY SHORT, and the cuts were length rather than
/// substance. Every disclosure this page ever made it still makes: all three
/// scope names, what each one is for, that a partial grant is refused, and that
/// we hold the refresh token with self-hosting as the way out. What went is the
/// prose around them, because a wall of text in front of a two-field form is
/// read by nobody and so discloses nothing. Anything added here should be a
/// clause, not a paragraph.
///
/// `waitlist_url` is THE OTHER WAY OFF THIS PAGE: somebody who has no invite
/// code cannot get one here, and until this link existed their only move was to
/// guess at the field or close the tab. It is rendered UNCONDITIONALLY when the
/// waitlist is configured, never only on a refusal, so it answers nothing about
/// whether the code that was just typed exists. `None` (a deployment with no
/// waitlist configured) renders no link rather than a dead one.
///
/// `error` re-renders the form with what went wrong; `label` and `invite` are
/// echoed back so a person does not retype the half that was fine. The invite
/// code is echoed only because the browser already has it, and it is escaped
/// like everything else.
pub fn signup_form(
    base_domain: &str,
    waitlist_url: Option<&str>,
    label: &str,
    invite: &str,
    error: Option<&str>,
) -> Response {
    let error_html = stop_note(error);
    // The link out, or nothing at all. Built from the configured origin, which
    // this crate owns; it is escaped anyway, because the rule is that nothing is
    // interpolated raw.
    let waitlist_html = waitlist_url
        .map(|url| {
            format!(
                r#"<p class="alt">No invite code? <a href="{url}">Join the waitlist</a>.</p>"#,
                url = escape_html(url),
            )
        })
        .unwrap_or_default();
    page(
        StatusCode::OK,
        "Set up your Passband mailbox",
        &format!(
            r#"<h1>Set up your mailbox</h1>
<p>Passband triages your Gmail and serves it to the app.</p>
{error_html}
<form class="card" method="post" action="/signup">
<div class="field">
<label for="invite">Invite code</label>
<input type="text" id="invite" name="invite" value="{invite}" placeholder="XXXX-XXXX-XXXX-XXXX"
  aria-describedby="invite-hint" autocomplete="off" autocapitalize="off" spellcheck="false"
  autofocus required>
<p class="hint" id="invite-hint">Case and dashes do not matter.</p>
</div>
<div class="field">
<label for="label">Choose your address</label>
<input type="text" id="label" name="label" value="{label}" placeholder="yourname"
  aria-describedby="label-hint" autocomplete="off" autocapitalize="off" spellcheck="false" required>
<p class="hint" id="label-hint">Lives at <span class="suffix">https://</span>yourname<span class="suffix">.{domain}</span>.
Lowercase, numbers, hyphens, 3 to 30 characters.</p>
</div>
<button type="submit">Continue to Google</button>
<p class="hint">Next, Google asks to read, change, and send your Gmail. Passband
needs all three.</p>
</form>
{waitlist_html}
<details>
<summary>What Google will ask you to approve</summary>
<ul>
<li><strong>Read</strong> (<code>gmail.readonly</code>): what the triage runs on.</li>
<li><strong>Change</strong> (<code>gmail.modify</code>): archiving and labeling. Never deletion.</li>
<li><strong>Send</strong> (<code>gmail.send</code>): replying from the app.</li>
</ul>
<p>Leave every box checked. A partial grant is sent back.</p>
</details>
<p class="muted">We hold your Google refresh token, encrypted, so your daemon can
sync while you are away. Self-host if you would rather we did not.</p>"#,
            error_html = error_html,
            waitlist_html = waitlist_html,
            invite = escape_html(invite),
            label = escape_html(label),
            domain = escape_html(base_domain),
        ),
    )
}

/// The end of the flow: the tenant exists, the daemon is running, and the user
/// needs to get the app connected to it.
///
/// The code and the URL are `user-select: all` text rather than a copy button,
/// because a copy button is JavaScript and this page has none. The deep link is
/// the fast path; typing the code into the app is the path that always works.
pub fn success(tenant_url: &str, pair_code: &str, minutes: i64) -> Response {
    let link = deep_link(tenant_url, pair_code);
    page(
        StatusCode::OK,
        "Your mailbox is ready",
        &format!(
            r#"<h1>Your mailbox is ready</h1>
<p>Your daemon is running at <code>{url}</code> and is syncing your mail now.
One more step: connect the app.</p>
<ol>
<li><a href="{download}">Download Passband</a> and open it.</li>
<li>Press <strong>Pair</strong>.</li>
<li>Enter the code below, or open the link on the same device.</li>
</ol>
<p><span class="code">{code}</span></p>
<p><a class="button" href="{link}">Open Passband and pair</a></p>
<p class="muted">The code is good for {minutes} minutes and works once. If it
expires before you get to it, that is fine: open Passband, point it at
<code>{url}</code>, and ask for a new code.</p>
<p class="muted">Keep the code to yourself while it is live. It is what lets a
device in.</p>"#,
            url = escape_html(tenant_url),
            code = escape_html(pair_code),
            link = escape_html(&link),
            download = DOWNLOAD_URL,
            minutes = minutes,
        ),
    )
}

/// Where an app login ends: signed in, and one press from a connected app.
///
/// THE DEEP LINK IS THE PAGE. Everything else on it exists for the cases the
/// link cannot cover: a browser that will not hand a custom scheme to the OS, a
/// sign in finished on a phone for a Mac, an app that is not installed yet. So
/// the code and the server are on it as `user-select: all` text too, exactly as
/// they are on [`success`], and for the same reason: the link is the fast path
/// and typing is the path that always works.
///
/// NO AUTOMATIC REDIRECT, and that is deliberate rather than unfinished. This
/// page carries a live pairing code, and a redirect that fires on load would
/// hand it to whatever claims `passband://` the moment the browser renders,
/// before the person reading has agreed to anything. A press is one act more and
/// it is the act that says which device this code is for.
///
/// The mailbox is named on it because this is the one screen that can say it: an
/// app login never asked who the person was, so "signed in as" is the only
/// confirmation they get that Google picked the account they meant.
pub fn app_signed_in(
    account_email: &str,
    tenant_url: &str,
    pair_code: &str,
    minutes: i64,
) -> Response {
    let link = deep_link(tenant_url, pair_code);
    page(
        StatusCode::OK,
        "Signed in",
        &format!(
            r#"<h1>Signed in as {email}</h1>
<p>Your mailbox is at <code>{url}</code>. One press connects Passband to it.</p>
<p><a class="button" href="{link}">Open Passband</a></p>
<h2>If that button does nothing</h2>
<p>Passband may not be installed on this device, or your browser may not open
app links. <a href="{download}">Download Passband</a> if you need it, then open
it, choose hosted, and enter these:</p>
<p><code>{url}</code></p>
<p><span class="code">{code}</span></p>
<p class="muted">The code is good for {minutes} minutes and works once. If it
expires before you get to it, come back here and sign in again.</p>
<p class="muted">Keep the code to yourself while it is live. It is what lets a
device in.</p>"#,
            email = escape_html(account_email),
            url = escape_html(tenant_url),
            code = escape_html(pair_code),
            link = escape_html(&link),
            download = DOWNLOAD_URL,
            minutes = minutes,
        ),
    )
}

/// The page BOTH logins get when they are refused: the same shell as [`problem`]
/// with NO link out.
///
/// Two reasons it is not [`problem`]. The link there goes to the signup form,
/// which is the wrong place to send somebody who already has a mailbox and was
/// trying to sign in to it. And the only honest link back would be built from
/// the label this request named, which on a uniform refusal is a label that may
/// not exist: rendering it would answer, in an `href`, the question the refusal
/// exists to leave unanswered. An app login has it worse still: it never named a
/// label at all, so there is not even a wrong link to render.
pub fn console_problem(status: StatusCode, heading: &str, detail: &str) -> Response {
    page(
        status,
        heading,
        &format!(
            r#"<h1>{heading}</h1>
<p>{detail}</p>"#,
            heading = escape_html(heading),
            detail = escape_html(detail),
        ),
    )
}

/// The console's problem page WITH the one link that is always right on it:
/// back to the console this sign in was for.
///
/// Used where the label is one this service MINTED into a session of its own
/// (an expired console login, a replayed callback, a cookie that does not match
/// its session), so the `href` echoes nothing a stranger chose and answers
/// nothing about which addresses exist: it is the address the person already
/// typed, handed back. [`console_problem`] stays link-free for the refusals that
/// turn on identity, where the only link available would be built from a label
/// the refusal exists to say nothing about.
///
/// NEVER the signup form. Somebody signing in to a mailbox they already own is
/// not somebody to send off to make a second one.
pub fn console_problem_with_link(
    status: StatusCode,
    heading: &str,
    detail: &str,
    console_url: &str,
) -> Response {
    page(
        status,
        heading,
        &format!(
            r#"<h1>{heading}</h1>
<p>{detail}</p>
<p><a class="button" href="{url}">Back to your console</a></p>"#,
            heading = escape_html(heading),
            detail = escape_html(detail),
            url = escape_html(console_url),
        ),
    )
}

/// The admin door: one field, and nothing that describes what is behind it.
///
/// Anybody can ask for `/admin`, so this page says as little as a page can. It
/// names no waitlist, no counts, and no operator.
///
/// `error` is present only on a refusal, so it decides the status too: a
/// message rendered here means the request was turned away, and a 200 with a
/// "not accepted" sentence in it is a lie to everything that reads status
/// codes.
pub fn admin_login(error: Option<&str>) -> Response {
    let status = if error.is_some() {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::OK
    };
    admin_door(status, &stop_note(error))
}

/// The door after a DELIBERATE sign out, which is not a refusal and must not be
/// dressed as one.
///
/// A 200 and a quiet note, where [`admin_login`] would answer 401 with a red
/// banner. The rule that function states still holds and this is the other half
/// of it: a 200 carrying "not accepted" is a lie to everything that reads status
/// codes, and so is a 401 for a request that did exactly what was asked.
pub fn admin_signed_out() -> Response {
    admin_door(StatusCode::OK, r#"<p class="hint">Signed out.</p>"#)
}

/// The form both doors show, with whatever note belongs above it.
fn admin_door(status: StatusCode, note: &str) -> Response {
    console(
        status,
        "Passband admin",
        &format!(
            r#"<h1>Passband admin</h1>
{note}
<form method="post" action="/admin/login">
<label for="token">Admin token</label>
<input type="password" id="token" name="token" autocomplete="current-password"
  autocapitalize="off" spellcheck="false" required>
<button type="submit">Sign in</button>
</form>"#
        ),
    )
}

/// The CSRF refusal, naming the origin this deployment answers to and the one
/// the request actually stated.
///
/// A 403 with no words is the right shape for a page on some other origin that
/// pressed these buttons, and the wrong shape for the operator, who meets the
/// same refusal when an extension, a proxy, or a sandboxed frame rewrites those
/// headers under an address bar that reads correctly. Both are told the same
/// thing, because the attacker's half of it is a request they wrote themselves.
///
/// `report` arrives already filtered by [`crate::admin`] and is escaped here
/// too: the rule is that nothing is interpolated raw.
pub fn admin_cross_origin(expected: &str, report: &str) -> Response {
    console(
        StatusCode::FORBIDDEN,
        "Passband admin",
        &format!(
            r#"<h1>That request did not come from this site</h1>
<p>This page only accepts form posts made by a page loaded from
<code>{expected}</code>, and this one stated something else. Nothing was
changed.</p>
<p class="muted">What arrived: <code>{report}</code></p>
<p class="muted">A browser on this origin sends its own address here. Something
between the form and this service rewrote it, which is usually an extension, a
proxy, or the page running inside a sandboxed frame. Opening
<code>{expected}/admin</code> in an ordinary tab is the fix.</p>"#,
            expected = escape_html(expected.trim_end_matches('/')),
            report = escape_html(report),
        ),
    )
}

/// The dashboard: who is waiting, who has been approved, and the two buttons.
///
/// ALWAYS 200, even carrying an `error`. The list under it is correct either
/// way, and the banner says what the button did NOT do (a row already approved,
/// an invite already spent); that is a finished page describing a real state,
/// not a failed request.
///
/// THE CODE IS NOT HERE, and cannot be. Only its hash was kept, so the one
/// remedy for a lost invite is a fresh one, which is what the send buttons are.
pub fn admin_page(
    pending: &[WaitlistRow],
    approved: &[WaitlistRow],
    error: Option<&str>,
) -> Response {
    let waiting: String = pending
        .iter()
        .map(|r| {
            format!(
                r#"<tr><td class="who">{email}</td><td class="when">{joined}</td><td class="act">{action}</td></tr>
"#,
                email = escape_html(&r.email),
                joined = day(r.created_at),
                // "Approve", not "Approve and email invite". The long label was
                // the only disclosure that pressing it sends mail, and it paid
                // for that by repeating three words down every row of the board
                // in the loudest thing on the page. The sentence above the table
                // says it once, and the button is a word.
                action = action_form("/admin/approve", r.id, "Approve", true),
            )
        })
        .collect();

    let history: String = approved
        .iter()
        .map(|r| {
            // Two shapes for one row. A stamped `notified_at` is the quiet
            // case (the mail went out; the button is there for the person who
            // lost it), and a missing one is the loud case: this row is
            // approved and nobody was told.
            // THREE STATES, AND THE LAST ONE IS THE POINT OF THE COLUMN.
            //
            // Redemption was always in the store (`invite_codes.used_at`, joined
            // on in `list_waitlist`) and the re-send handler already refused a
            // spent code with INVITE_SPENT. What was missing was SAYING so: the
            // only way to learn somebody had signed up was to press a button and
            // read the refusal. Now the board says it, and offers NO button on
            // that row, because the one it used to offer could only ever fail.
            //
            // No button is the honest answer rather than a tidy one. A spent
            // code means a mailbox exists, and minting a second for the same
            // person is a tenant nobody approved, which is what the handler's
            // refusal is there to stop.
            let outcome = if let Some(at) = r.accepted_at {
                let label = r
                    .accepted_label
                    .as_deref()
                    .map(|l| format!(" <code>{}</code>", escape_html(l)))
                    .unwrap_or_default();
                format!(
                    r#"<span class="done">signed up</span><span class="muted">{}</span>{label}"#,
                    day(at),
                )
            } else if let Some(at) = r.notified_at {
                format!(
                    r#"<span class="muted">Invited {}</span> {}"#,
                    day(at),
                    action_form("/admin/send", r.id, "Re-send", true),
                )
            } else {
                // A PILL, not the `.stop` rule. `.stop` is a left border meant
                // for a whole paragraph, and inline beside a button it drew a
                // stray red tick that ran into the label next to it.
                format!(
                    r#"<span class="flag">email not sent</span>{}"#,
                    action_form("/admin/send", r.id, "Send invite", false),
                )
            };
            let approved_on = r
                .approved_at
                .map(|at| format!("<br>approved {}", day(at)))
                .unwrap_or_default();
            format!(
                r#"<tr><td class="who">{email}</td><td class="when">joined {joined}{approved_on}</td><td class="act">{outcome}</td></tr>
"#,
                email = escape_html(&r.email),
                joined = day(r.created_at),
            )
        })
        .collect();

    console(
        StatusCode::OK,
        "Waitlist",
        &format!(
            r#"<h1>Waitlist</h1>
<p class="tally">{waiting_count} waiting, {approved_count} approved recently,
{accepted_count} of those signed up</p>
{error_html}
<h2>Waiting</h2>
<p class="hint">Approving mints one invite code and emails it. The code works
once and expires in {ttl} days.</p>
{waiting_table}
<h2>Approved recently</h2>
{history_table}
<h2>Invite someone directly</h2>
<form class="invite" method="post" action="/admin/invite">
<label class="sr" for="email">Email address</label>
<input type="email" id="email" name="email" placeholder="them@example.com"
  autocomplete="off" autocapitalize="off" spellcheck="false" required>
<button type="submit">Mint and email an invite</button>
</form>
<p class="hint">They do not have to be on the list. The address lands under
Approved above and is tracked from there like everybody else. Nothing here can
read a code back out, so a lost invite is replaced rather than resent, and a
code that was already spent is not replaced at all.</p>
<form class="signout" method="post" action="/admin/logout">
<button type="submit" class="quiet">Sign out</button>
</form>
<p class="hint">This session lasts {session_days} days. Signing out ends it on
this browser; rotating the admin token ends it everywhere.</p>"#,
            error_html = stop_note(error),
            waiting_count = pending.len(),
            approved_count = approved.len(),
            accepted_count = approved.iter().filter(|r| r.accepted_at.is_some()).count(),
            session_days = crate::cookie::ADMIN_COOKIE_TTL_SECS / (24 * 60 * 60),
            waiting_table = table(
                r#"<th>Email</th><th>Joined</th><th></th>"#,
                &waiting,
                "Nobody is waiting.",
            ),
            history_table = table(
                r#"<th>Email</th><th>Dates</th><th>Outcome</th>"#,
                &history,
                "Nobody has been approved yet.",
            ),
            ttl = crate::invites::DEFAULT_TTL_DAYS,
        ),
    )
}

/// One row's button, as its own form. No JavaScript on this page, so a button
/// that acts is a form that posts, and the row it acts on rides in a hidden
/// field.
fn action_form(action: &str, id: i64, label: &str, quiet: bool) -> String {
    let class = if quiet { r#" class="quiet""# } else { "" };
    format!(
        r#"<form method="post" action="{action}"><input type="hidden" name="id" value="{id}"><button type="submit"{class}>{label}</button></form>"#,
        action = escape_html(action),
        label = escape_html(label),
    )
}

/// A table, or a sentence saying there is nothing to put in one.
fn table(head: &str, rows: &str, empty: &str) -> String {
    if rows.is_empty() {
        format!(r#"<p class="muted">{}</p>"#, escape_html(empty))
    } else {
        format!("<table><tr>{head}</tr>\n{rows}</table>")
    }
}

/// The error banner every page above renders the same way.
///
/// `role="alert"` because these pages have no JavaScript and a refusal is a
/// whole new document: without it, a screen reader lands the user back at the
/// top of a page that looks identical to the one they just submitted, with the
/// one sentence that changed sitting silently in the middle of it.
fn stop_note(error: Option<&str>) -> String {
    error
        .map(|e| {
            format!(
                r#"<p class="stop" role="alert"><strong>{}</strong></p>"#,
                escape_html(e)
            )
        })
        .unwrap_or_default()
}

/// A date as the operator reads it. Escaped like everything else: the rule is
/// that nothing is interpolated raw, and an exception for "this one is only
/// ever digits" is how the rule stops being one.
fn day(ts: DateTime<Utc>) -> String {
    escape_html(&ts.format("%Y-%m-%d").to_string())
}

/// Something went wrong after the user left for Google. `status` is the HTTP
/// status; `detail` is a sentence a person can act on and never a machine
/// reason.
pub fn problem(status: StatusCode, heading: &str, detail: &str) -> Response {
    page(
        status,
        heading,
        &format!(
            r#"<h1>{heading}</h1>
<p>{detail}</p>
<p><a class="button" href="/">Start again</a></p>"#,
            heading = escape_html(heading),
            detail = escape_html(detail),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(r: Response) -> String {
        String::from_utf8(to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    /// The rendered page WITHOUT the shell's stylesheet: what a reader can
    /// actually see.
    ///
    /// USE THIS FOR ANY "MUST NOT APPEAR" ASSERTION. Every page here renders
    /// through the one shell in [`render`], inline stylesheet and comments and
    /// all, so a negative match against the whole response is really a match
    /// against whatever those comments happen to say this week. That has broken
    /// three separate assertions already: a comment about the signup form, one
    /// about the admin board's `.invite` row, and one about re-sending being the
    /// rarer errand. None of the three was ever about the stylesheet.
    async fn document_of(r: Response) -> String {
        let html = body_of(r).await;
        html.split_once("</style>")
            .expect("every page carries the shell's stylesheet")
            .1
            .to_string()
    }

    #[test]
    fn escapes_the_characters_that_end_a_page() {
        assert_eq!(
            escape_html(r#"<script>alert("x&y")</script>"#),
            "&lt;script&gt;alert(&quot;x&amp;y&quot;)&lt;/script&gt;"
        );
        assert_eq!(escape_html("it's"), "it&#39;s");
    }

    #[test]
    fn percent_encodes_everything_outside_the_unreserved_set() {
        assert_eq!(percent_encode("ada-lovelace_1.0~"), "ada-lovelace_1.0~");
        assert_eq!(percent_encode("https://a.b"), "https%3A%2F%2Fa.b");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode("a b"), "a%20b");
    }

    /// The link shape the Swift client parses (`passband://pair?url=…&code=…`).
    #[test]
    fn builds_the_deep_link_the_app_expects() {
        assert_eq!(
            deep_link("https://ada.passband.email", "ABCD-EFGH"),
            "passband://pair?url=https%3A%2F%2Fada.passband.email&code=ABCD-EFGH"
        );
    }

    /// The form is the only place the three grants are explained in the
    /// product's own words before Google states them in Google's, so all three
    /// are named and each says what it is for.
    #[tokio::test]
    async fn the_form_states_all_three_grants_and_the_base_domain() {
        let html = body_of(signup_form("passband.email", None, "", "", None)).await;
        assert!(html.contains("gmail.readonly"));
        assert!(html.contains("gmail.modify"));
        assert!(html.contains("gmail.send"));
        // ...and the reason for each, not just the scope name.
        assert!(html.contains("triage"), "{html}");
        assert!(html.contains("labeling"), "{html}");
        assert!(html.contains("replying from the app"), "{html}");
        assert!(html.contains(".passband.email"));
        assert!(html.contains(r#"action="/signup""#));
        // No script anywhere, and nothing to fetch.
        assert!(!html.contains("<script"));
        assert!(!html.contains("http://"));
    }

    /// The scope detail may be folded away, but all three permissions are named
    /// in the open, above the button, where somebody deciding whether to press
    /// it will read them. The `<details>` block is the long version and is shut
    /// by default; the summary line is not.
    #[tokio::test]
    async fn the_form_names_all_three_permissions_outside_the_disclosure() {
        let html = body_of(signup_form("passband.email", None, "", "", None)).await;
        let (before_details, details) = html.split_once("<details>").expect("{html}");
        assert!(
            before_details.contains("read, change, and send your Gmail"),
            "{html}"
        );
        assert!(before_details.contains("needs all three"), "{html}");
        // Shut by default: `open` is what would render it expanded.
        assert!(!details.contains("<details open"), "{html}");
        assert!(!html.contains("<details open"), "{html}");
        // A disclosure is markup, not script. This page still has none.
        assert!(!html.contains("<script"), "{html}");
    }

    /// The invite field shows the shape of a code this deployment actually
    /// mints. It used to show the eight-symbol shape that predates
    /// [`crate::invites::CODE_LEN`], which is a placeholder no live code has
    /// looked like since.
    #[tokio::test]
    async fn the_invite_placeholder_is_the_shape_a_minted_code_has() {
        let html = body_of(signup_form("passband.email", None, "", "", None)).await;
        let minted = crate::invites::mint().unwrap().code;
        let shape: String = minted
            .chars()
            .map(|c| if c == '-' { '-' } else { 'X' })
            .collect();
        assert!(
            html.contains(&format!(r#"placeholder="{shape}""#)),
            "{html}"
        );
    }

    /// The way off this page for somebody who has no code, and the reason it is
    /// not conditional on anything the user typed: rendering it only on an
    /// invite refusal would make its presence an answer about the code space.
    #[tokio::test]
    async fn the_form_offers_the_waitlist_whether_or_not_anything_was_refused() {
        let url = "https://passband.app/waitlist";
        for error in [None, Some("That invite code is not usable.")] {
            let html = body_of(signup_form("passband.email", Some(url), "", "", error)).await;
            assert!(html.contains(&format!(r#"href="{url}""#)), "{html}");
            assert!(html.contains("No invite code?"), "{html}");
        }
        // A deployment with no waitlist configured links nowhere at all rather
        // than to a page that does not exist.
        let bare = body_of(signup_form("passband.email", None, "", "", None)).await;
        assert!(!bare.contains("waitlist"), "{bare}");
        assert!(!bare.contains("No invite code"), "{bare}");
    }

    /// Both fields are echoed back into the form, so both are escape paths.
    #[tokio::test]
    async fn the_form_escapes_what_it_echoes() {
        let html = body_of(signup_form(
            "passband.email",
            Some(r#"https://evil.test/" onmouseover="alert(3)"#),
            r#""><script>alert(1)</script>"#,
            r#""onfocus="alert(2)"#,
            Some("<b>nope</b>"),
        ))
        .await;
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains(r#"onfocus="alert(2)"#));
        assert!(!html.contains(r#"onmouseover="alert(3)"#));
        assert!(!html.contains("<b>nope</b>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[tokio::test]
    async fn the_success_page_carries_the_code_the_url_and_the_link() {
        let html = body_of(success("https://ada.passband.email", "ABCD-EFGH", 10)).await;
        assert!(html.contains("ABCD-EFGH"));
        assert!(html.contains("https://ada.passband.email"));
        assert!(
            html.contains(
                "passband://pair?url=https%3A%2F%2Fada.passband.email&amp;code=ABCD-EFGH"
            )
        );
        assert!(html.contains("passband.app"));
        assert!(!html.contains("<script"));
    }

    /// THE DOWNLOAD LIVES AT THE END OF THE FLOW, not on the landing page: that
    /// page leads with the waitlist now, so these two are the only screens that
    /// hand anybody the client. Both need it, and each for its own reason: one
    /// has just built a mailbox for somebody who has no app, and the other is
    /// the page a person lands on precisely when the deep link did nothing,
    /// which is most often because the app is not installed.
    ///
    /// A REAL `href`, not the bare hostname the success page used to print. It
    /// was `<code>passband.app</code>` in the middle of a numbered step, which
    /// asks somebody mid-setup to retype a domain, and now it is the one thing
    /// on that step that can be pressed.
    #[tokio::test]
    async fn the_pages_that_finish_a_flow_hand_over_the_client() {
        for html in [
            body_of(success("https://ada.passband.email", "ABCD-EFGH", 10)).await,
            body_of(app_signed_in(
                "ada@example.com",
                "https://ada.passband.email",
                "ABCD-EFGH",
                10,
            ))
            .await,
        ] {
            assert!(
                html.contains(&format!(r#"<a href="{DOWNLOAD_URL}">Download Passband</a>"#)),
                "{html}"
            );
        }
        // And nowhere else. The signup form is somebody deciding whether to
        // hand over their mail, and an app they cannot yet point at anything is
        // not what that page is for.
        let form = body_of(signup_form("passband.email", None, "", "", None)).await;
        assert!(!form.contains(DOWNLOAD_URL), "{form}");
    }

    /// The console redirect is assembled from a validated tenant URL and a
    /// validated code, and the code is encoded on the way in: nothing in it can
    /// end the parameter it sits in or start a second one.
    #[test]
    fn builds_the_console_callback_from_its_own_parts() {
        assert_eq!(
            console_callback_url("https://ada.passband.email", "ABCD-EFGH"),
            "https://ada.passband.email/console/callback?code=ABCD-EFGH"
        );
        assert_eq!(
            console_callback_url("https://ada.passband.email", "A&next=https://evil.test"),
            "https://ada.passband.email/console/callback?code=A%26next%3Dhttps%3A%2F%2Fevil.test"
        );
    }

    /// The console refusal links nowhere: the only link it could offer would be
    /// built from a label the refusal is deliberately saying nothing about.
    #[tokio::test]
    async fn the_console_refusal_offers_no_way_onward() {
        let html = body_of(console_problem(
            StatusCode::BAD_REQUEST,
            "Nope",
            "Try again.",
        ))
        .await;
        assert!(!html.contains("<a "), "{html}");
        assert!(!html.contains("href"), "{html}");
    }

    /// A waitlist row as the store hands one over.
    fn row(id: i64, email: &str, notified: bool) -> WaitlistRow {
        // 2026-01-01T00:00:00Z.
        let at = DateTime::from_timestamp(1_767_225_600, 0).unwrap();
        WaitlistRow {
            id,
            email: email.to_string(),
            created_at: at,
            status: crate::store::WAITLIST_APPROVED.to_string(),
            approved_at: Some(at),
            invite_id: Some(7),
            notified_at: notified.then_some(at),
            accepted_at: None,
            accepted_label: None,
        }
    }

    /// The same row after they spent the code: a mailbox exists.
    fn accepted_row(id: i64, email: &str, label: &str) -> WaitlistRow {
        let at = DateTime::from_timestamp(1_767_225_600, 0).unwrap();
        WaitlistRow {
            accepted_at: Some(at),
            accepted_label: Some(label.to_string()),
            ..row(id, email, true)
        }
    }

    /// The dashboard is the one page that renders a string a stranger typed
    /// into a public form, so it is the one page where escaping is the whole
    /// defense. `<`, `>`, and `"` all survive the address shape check.
    #[tokio::test]
    async fn the_dashboard_escapes_an_address_that_is_an_attack() {
        let hostile = r#""><script>alert(1)</script>@evil.test"#;
        let html = body_of(admin_page(
            &[row(1, hostile, false)],
            &[row(2, hostile, true)],
            Some("<b>nope</b>"),
        ))
        .await;
        assert!(!html.contains("<script>alert(1)"), "{html}");
        assert!(!html.contains("<b>nope</b>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&quot;&gt;"), "{html}");
    }

    /// The direct-invite form is ON the dashboard, and it is styled: the shared
    /// stylesheet named two input types by hand, so an `email` input rendered as
    /// an unstyled browser default next to the ones that are not.
    #[tokio::test]
    async fn the_dashboard_carries_a_styled_form_for_inviting_directly() {
        let page = body_of(admin_page(&[], &[], None)).await;
        assert!(page.contains(r#"action="/admin/invite""#), "{page}");
        assert!(page.contains(r#"name="email""#), "{page}");
        assert!(page.contains("Mint and email an invite"), "{page}");
        assert!(page.contains("input[type=email]"), "{page}");
        // Both palettes, or it is an unstyled box in one of them, plus the
        // console surface, which restates every colour rather than inheriting
        // the dark media query (see the `body.console` block).
        assert_eq!(page.matches("input[type=email]").count(), 3, "{page}");
        assert!(!page.contains("<script"), "{page}");
    }

    /// Which button a row gets is the whole state machine the operator sees: a
    /// stamped row is quiet, an unstamped one is loud and asks to be pressed.
    #[tokio::test]
    async fn an_unsent_invite_says_so_and_offers_the_button() {
        let sent = body_of(admin_page(&[], &[row(1, "ada@example.com", true)], None)).await;
        assert!(sent.contains("Invited 2026-01-01"), "{sent}");
        // Quiet: this row is fine, and the button is only there for somebody
        // who lost the mail. Matched WITH its label, because the board carries
        // a quiet sign-out button of its own at the foot.
        assert!(
            sent.contains(r#"<button type="submit" class="quiet">Re-send</button>"#),
            "{sent}"
        );
        assert!(!sent.contains("email not sent"), "{sent}");

        let failed = body_of(admin_page(&[], &[row(1, "ada@example.com", false)], None)).await;
        assert!(
            failed.contains(r#"<span class="flag">email not sent</span>"#),
            "{failed}"
        );
        // Loud, and the only row action on the board that is: this one is asking
        // to be pressed. No `class="quiet"` on THIS button, which is what the
        // label in the match is for.
        assert!(
            failed.contains(r#"<button type="submit">Send invite</button>"#),
            "{failed}"
        );

        let waiting = body_of(admin_page(&[row(1, "ada@example.com", false)], &[], None)).await;
        assert!(waiting.contains(">Approve</button>"), "{waiting}");
        assert!(waiting.contains(r#"name="id" value="1""#), "{waiting}");
        assert!(waiting.contains(r#"action="/admin/approve""#), "{waiting}");
        // No JavaScript on this page either: a button that acts is a form.
        assert!(!waiting.contains("<script"), "{waiting}");
    }

    /// THE FAR END OF THE FUNNEL, which the board could not show at all before
    /// the redemption join: an invite that was spent means a mailbox, and the
    /// row says which one.
    ///
    /// NO BUTTON ON THAT ROW. `admin::send` refuses a spent code (INVITE_SPENT)
    /// because a second code for somebody who already has a mailbox is a tenant
    /// nobody approved, so offering the press was offering a guaranteed
    /// refusal, and pressing it was the only way to discover they had signed up.
    #[tokio::test]
    async fn a_spent_invite_says_which_mailbox_it_became_and_offers_no_button() {
        let html = document_of(admin_page(
            &[],
            &[accepted_row(1, "ada@example.com", "ada")],
            None,
        ))
        .await;
        assert!(html.contains(r#"<span class="done">signed up</span>"#), "{html}");
        assert!(html.contains("<code>ada</code>"), "{html}");
        assert!(html.contains("1 of those signed up"), "{html}");
        // The whole point: no press, and nothing that could mint a second code.
        assert!(!html.contains("Re-send"), "{html}");
        assert!(!html.contains("/admin/send"), "{html}");

        // An outstanding invite is unchanged: still offered, still quiet.
        let waiting_on_them =
            document_of(admin_page(&[], &[row(1, "ada@example.com", true)], None)).await;
        assert!(waiting_on_them.contains("Re-send"), "{waiting_on_them}");
        assert!(
            !waiting_on_them.contains("signed up</span>"),
            "{waiting_on_them}"
        );
        assert!(
            waiting_on_them.contains("0 of those signed up"),
            "{waiting_on_them}"
        );
    }

    /// A DELIBERATE SIGN OUT IS NOT A REFUSAL. The session runs for a month
    /// now, so ending one on a borrowed machine had to become a press rather
    /// than a trip to Railway to rotate the token; and the page that press lands
    /// on answers 200, because the request did exactly what was asked. The
    /// expired-session door is the 401, and still says so.
    #[tokio::test]
    async fn signing_out_is_a_200_and_being_thrown_out_is_a_401() {
        let out = admin_signed_out();
        assert_eq!(out.status(), StatusCode::OK);
        let html = document_of(out).await;
        assert!(html.contains("Signed out."), "{html}");
        // The door, not a dead end: the way back in is on it.
        assert!(html.contains(r#"action="/admin/login""#), "{html}");
        // Not dressed as a refusal.
        assert!(!html.contains(r#"class="stop""#), "{html}");

        assert_eq!(
            admin_login(Some("Your admin session has ended.")).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(admin_login(None).status(), StatusCode::OK);
    }

    /// The exit is ON the board, and it is a form, because a link cannot carry
    /// the same-origin POST every other admin action on this page is held to.
    #[tokio::test]
    async fn the_board_carries_the_way_out() {
        let html = document_of(admin_page(&[], &[], None)).await;
        assert!(html.contains(r#"action="/admin/logout""#), "{html}");
        assert!(html.contains("Sign out"), "{html}");
        // The page states its own lifetime rather than leaving the operator to
        // discover it by being signed out.
        assert!(html.contains("This session lasts 30 days"), "{html}");
    }

    /// The door names nothing that is behind it, and a refusal is a 401 rather
    /// than a 200 with bad news in it.
    #[tokio::test]
    async fn the_admin_door_says_nothing_and_refuses_with_a_status() {
        let r = admin_login(None);
        assert_eq!(r.status(), StatusCode::OK);
        let html = body_of(r).await;
        assert!(html.contains(r#"type="password""#), "{html}");
        assert!(html.contains(r#"action="/admin/login""#), "{html}");
        assert!(!html.to_lowercase().contains("waitlist"), "{html}");

        let refused = admin_login(Some("That token was not accepted."));
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    }

    /// House rule: no em dashes in anything a person reads.
    #[tokio::test]
    async fn no_em_dashes_in_user_facing_copy() {
        for html in [
            body_of(signup_form(
                "passband.email",
                Some("https://passband.app/waitlist"),
                "ada",
                "ABCD-EFGH",
                Some("no"),
            ))
            .await,
            body_of(success("https://ada.passband.email", "ABCD-EFGH", 10)).await,
            body_of(problem(StatusCode::BAD_REQUEST, "Nope", "Try again.")).await,
            body_of(console_problem(
                StatusCode::BAD_REQUEST,
                "Nope",
                "Try again.",
            ))
            .await,
            body_of(admin_login(Some("no"))).await,
            body_of(admin_page(
                &[row(1, "ada@example.com", false)],
                &[row(2, "bob@example.com", true)],
                Some("no"),
            ))
            .await,
            body_of(admin_page(&[], &[], None)).await,
        ] {
            assert!(!html.contains('\u{2014}'), "{html}");
        }
    }

    /// The masthead is part of the shell, so it is on every page, and it is
    /// MARKUP rather than anything the browser has to go and get: no `src`, no
    /// `url()`, no scheme anywhere in it. That is the whole reason the logo
    /// could be added without touching a CSP that says `default-src 'none'`.
    #[tokio::test]
    async fn every_page_wears_the_mark_and_fetches_nothing_to_do_it() {
        for html in [
            body_of(signup_form("passband.email", None, "", "", None)).await,
            body_of(success("https://ada.passband.email", "ABCD-EFGH", 10)).await,
            body_of(app_signed_in(
                "ada@example.com",
                "https://ada.passband.email",
                "ABCD-EFGH",
                10,
            ))
            .await,
            body_of(problem(StatusCode::BAD_REQUEST, "Nope", "Try again.")).await,
            body_of(console_problem(
                StatusCode::BAD_REQUEST,
                "Nope",
                "Try again.",
            ))
            .await,
            body_of(admin_login(None)).await,
            body_of(admin_page(&[row(1, "ada@example.com", false)], &[], None)).await,
        ] {
            assert!(html.contains(r#"<header class="brand">"#), "{html}");
            assert!(
                html.contains(r#"<span class="wordmark">Passband</span>"#),
                "{html}"
            );
            assert!(html.contains("<svg"), "{html}");
            // Inline, not fetched: an `<img src>` or a `url()` here would be a
            // request `default-src 'none'` refuses, i.e. a broken logo.
            assert!(!html.contains("<img"), "{html}");
            assert!(!html.contains("url("), "{html}");
            assert!(!html.contains("http://"), "{html}");
        }
    }

    /// The masthead does not link out. The one honest `href` would be the
    /// marketing site, and the page it sits on hardest is the one where somebody
    /// is deciding whether to hand over their mail; a door out of that decision
    /// is not what the top left corner is for. The console refusal leans on
    /// this: it asserts it offers NO way onward, and a linked masthead would
    /// have made that false everywhere at once.
    #[tokio::test]
    async fn the_masthead_is_not_a_way_out_of_the_flow() {
        let html = body_of(signup_form("passband.email", None, "", "", None)).await;
        let (masthead, _) = html.split_once("</header>").expect("{html}");
        assert!(!masthead.contains("<a "), "{masthead}");
        assert!(!masthead.contains("href"), "{masthead}");
    }

    #[tokio::test]
    async fn every_page_carries_the_security_headers() {
        for r in [
            signup_form("passband.email", None, "", "", None),
            success("https://ada.passband.email", "ABCD-EFGH", 10),
            problem(StatusCode::BAD_REQUEST, "Nope", "Try again."),
            console_problem(StatusCode::BAD_REQUEST, "Nope", "Try again."),
            admin_login(None),
            admin_page(&[row(1, "ada@example.com", false)], &[], None),
        ] {
            let h = r.headers().clone();
            assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
            assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
            assert_eq!(h[header::CACHE_CONTROL], "no-store, no-cache");
            assert!(
                h[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("default-src 'none'")
            );
        }
    }
}
