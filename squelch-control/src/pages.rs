//! Every HTML byte this service serves.
//!
//! NO JAVASCRIPT, no external assets, no fonts to fetch, and a
//! `default-src 'none'` CSP that states it to the browser rather than only to
//! the tests. A signup page that pulls in a third party is a signup page a
//! third party can watch, and the one thing a user does here is decide whether
//! to hand over access to their mail.
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

/// The shell every page shares.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
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
:root {{ color-scheme: light dark; }}
body {{ margin: 0; padding: 3rem 1.25rem; background: #fbfaf8; color: #1a1a1a;
  font: 1rem/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }}
main {{ max-width: 34rem; margin: 0 auto; }}
h1 {{ font-size: 1.35rem; font-weight: 600; letter-spacing: -0.01em; margin: 0 0 1rem; }}
h2 {{ font-size: 1.05rem; font-weight: 600; margin: 1.75rem 0 0.6rem; }}
p {{ margin: 0 0 1rem; }}
ul, ol {{ margin: 0 0 1.25rem; padding-left: 1.2rem; }}
li {{ margin: 0 0 0.5rem; }}
label {{ display: block; font-weight: 600; margin: 0 0 0.35rem; }}
input[type=text] {{ width: 100%; box-sizing: border-box; padding: 0.6rem 0.7rem; margin: 0 0 1.25rem;
  border: 1px solid #cdc7bd; border-radius: 6px; background: #fff; color: inherit; font: inherit; }}
button {{ padding: 0.65rem 1.15rem; border: 0; border-radius: 6px; background: #1a1a1a; color: #fbfaf8;
  font: inherit; font-weight: 500; cursor: pointer; }}
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
@media (prefers-color-scheme: dark) {{
  body {{ background: #141414; color: #e8e6e3; }}
  .muted, .suffix {{ color: #9a9a9a; }}
  code, .code {{ background: #262626; }}
  input[type=text] {{ background: #1f1f1f; border-color: #3a3a3a; }}
  button, a.button {{ background: #e8e6e3; color: #141414; }}
  .stop {{ border-left-color: #f2b8b5; }}
}}
</style>
</head>
<body><main>{body}</main></body>
</html>
"#
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // These pages carry a pairing code and name a mailbox. A shared
            // cache must not keep them, and the click through to Google must
            // not carry this URL as a referer.
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
/// It states the Google grant in plain words BEFORE the button, because the
/// consent screen that follows is Google's and says it in Google's vocabulary.
/// `error` re-renders the form with what went wrong; `label` and `invite` are
/// echoed back so a person does not retype the half that was fine. The invite
/// code is echoed only because the browser already has it, and it is escaped
/// like everything else.
pub fn signup_form(base_domain: &str, label: &str, invite: &str, error: Option<&str>) -> Response {
    let error_html = error
        .map(|e| format!(r#"<p class="stop"><strong>{}</strong></p>"#, escape_html(e)))
        .unwrap_or_default();
    page(
        StatusCode::OK,
        "Set up your Passband mailbox",
        &format!(
            r#"<h1>Set up your Passband mailbox</h1>
<p>Passband runs a mailbox daemon for you. It reads your Gmail, sorts what
matters from what does not, and serves the result to the Passband app, where you
archive, label, and reply.</p>
{error_html}
<form method="post" action="/signup">
<label for="invite">Invite code</label>
<input type="text" id="invite" name="invite" value="{invite}" placeholder="XXXX-XXXX"
  autocomplete="off" autocapitalize="off" spellcheck="false" required>
<label for="label">Choose your address</label>
<input type="text" id="label" name="label" value="{label}" placeholder="yourname"
  autocomplete="off" autocapitalize="off" spellcheck="false" required>
<p class="muted">Your mailbox will live at <span class="suffix">https://</span>yourname<span class="suffix">.{domain}</span>.
Lowercase letters, numbers, and hyphens, 3 to 30 characters.</p>
<button type="submit">Continue to Google</button>
</form>
<h2>What Google will ask you to approve</h2>
<p>Three permissions, on one screen. Passband needs all three, so leave every box
checked. If one is missing we will send you back rather than set up a mailbox
that half works.</p>
<ul>
<li><strong>Read your Gmail</strong> (<code>gmail.readonly</code>): every message,
every attachment, and your mail settings. This is what the triage runs on.</li>
<li><strong>Change your Gmail</strong> (<code>gmail.modify</code>): archiving and
labeling, so the app can act on a message instead of only showing it to you.
Permanent deletion is not included and Passband never asks for it.</li>
<li><strong>Send mail as you</strong> (<code>gmail.send</code>): composing and
replying from the app. Nothing is ever sent that you did not write and send
yourself.</li>
</ul>
<p class="muted">Your mail is read by a daemon that runs only for you, in its own
process, with its own database. Signing up means we hold your Google refresh
token, encrypted, so that daemon can keep syncing while you are away. If you
would rather nobody held it, run Passband yourself: the self-hosted daemon talks
to Google directly and our servers are never in the path.</p>"#,
            error_html = error_html,
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
<li>Download Passband from <code>passband.app</code> and open it.</li>
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
            minutes = minutes,
        ),
    )
}

/// The console login's own problem page: the same shell as [`problem`] with NO
/// link out.
///
/// Two reasons it is not [`problem`]. The link there goes to the signup form,
/// which is the wrong place to send somebody who already has a mailbox and was
/// trying to sign in to it. And the only honest link back would be built from
/// the label this request named, which on a uniform refusal is a label that may
/// not exist: rendering it would answer, in an `href`, the question the refusal
/// exists to leave unanswered.
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
        let html = body_of(signup_form("passband.email", "", "", None)).await;
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

    /// Both fields are echoed back into the form, so both are escape paths.
    #[tokio::test]
    async fn the_form_escapes_what_it_echoes() {
        let html = body_of(signup_form(
            "passband.email",
            r#""><script>alert(1)</script>"#,
            r#""onfocus="alert(2)"#,
            Some("<b>nope</b>"),
        ))
        .await;
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains(r#"onfocus="alert(2)"#));
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

    /// House rule: no em dashes in anything a person reads.
    #[tokio::test]
    async fn no_em_dashes_in_user_facing_copy() {
        for html in [
            body_of(signup_form(
                "passband.email",
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
        ] {
            assert!(!html.contains('\u{2014}'), "{html}");
        }
    }

    #[tokio::test]
    async fn every_page_carries_the_security_headers() {
        for r in [
            signup_form("passband.email", "", "", None),
            success("https://ada.passband.email", "ABCD-EFGH", 10),
            problem(StatusCode::BAD_REQUEST, "Nope", "Try again."),
            console_problem(StatusCode::BAD_REQUEST, "Nope", "Try again."),
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
