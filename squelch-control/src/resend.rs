//! The Resend client: one outbound call, one email, one invite code.
//!
//! When an operator approves a waitlist row the control plane mints an invite
//! and mails it. This module is the mailing, and it speaks exactly one route of
//! Resend's API (`POST /emails`) with the same outbound discipline every other
//! client in this crate has: redirects refused, one timeout, response bodies
//! capped, and errors whose `Display` names nothing.
//!
//! WHAT MUST NOT LEAK IS THE WHOLE POINT. The plaintext invite code exists in
//! this process from [`crate::invites::mint`] to the request body here and
//! nowhere else, and the recipient is a prospect's address. So:
//!
//! - Nothing in this module logs. Not the code, not the address, not the body,
//!   not the provider's answer. The caller logs the WAITLIST ROW ID and the
//!   error, which is all an operator needs to find the row and press the button
//!   again.
//! - [`ResendError`]'s `Display` carries no address, no code, no API key, and
//!   no provider status. Four outcomes, four sentences, all of them safe on a
//!   page and in a log line.
//! - [`ResendClient`] derives nothing: it holds the API key, and a derived
//!   `Debug` is how that reaches a formatted line.
//!
//! COPY RULES (house style): the product is Passband, the voice is the site's
//! lowercase one, and there are no em dashes in anything a person reads.

use std::time::Duration;

use serde::Serialize;

use crate::pages::escape_html;

/// Ceiling on a response body. Resend answers a send with a small JSON object
/// carrying the message id; anything bigger is not the API we know. The body is
/// read and DISCARDED (the id is of no use to us), but it is read capped rather
/// than left unread so a slow, endless body cannot hold the connection.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// Ceiling on a recipient address. RFC 5321's limit, asserted here because this
/// value arrives from a public form and lands in somebody else's mail queue.
const MAX_RECIPIENT_LEN: usize = 254;

/// What the invite email says in its subject line. Not configurable: it is
/// product copy, and an operator retyping it per deployment is how a phishing
/// filter learns two different Passbands.
const SUBJECT: &str = "Your Passband invite";

#[derive(Debug, thiserror::Error)]
pub enum ResendError {
    /// The provider could not be reached, or answered something unreadable.
    #[error("the email provider could not be reached")]
    Unreachable,
    /// 401/403. A deployment misconfiguration worth shouting about: no invite
    /// reaches anybody until it is fixed.
    #[error("the email provider refused our API key")]
    Unauthorized,
    /// The provider took the request and would not send it: an unverified
    /// sending domain, or an address it will not deliver to. Retrying the same
    /// send will fail the same way.
    #[error("the email provider refused to send that message")]
    Refused,
    /// Any other non-success status, including the provider's own rate limit.
    /// Worth trying again.
    #[error("the email provider could not send the invite")]
    Failed,
}

/// `POST /emails` request body. The field names are Resend's.
#[derive(Serialize)]
struct SendRequest<'a> {
    from: &'a str,
    /// An array even for one recipient: that is the shape the API takes.
    to: [&'a str; 1],
    subject: &'a str,
    /// Both parts are sent. A text part is what a client with no HTML shows,
    /// and it is also what keeps the message out of the folder a lone HTML part
    /// tends to land in.
    text: String,
    html: String,
}

/// The client: one reqwest client, one API key, one sender, one base URL.
///
/// DELIBERATELY DERIVES NOTHING, the way [`crate::bifrost::VirtualKey`] does:
/// `api_key` lives here for the process's lifetime and must not be formattable.
pub struct ResendClient {
    base_url: String,
    /// `Bearer <key>`, precomputed once. As secret as the key it carries.
    auth_header: String,
    /// The `From:` every invite is sent as. Validated by config.
    from: String,
    http: reqwest::Client,
}

impl ResendClient {
    /// `base_url` is a canonical origin (no trailing slash), `api_key` the
    /// Resend sending key, and `from` the verified sender every invite goes out
    /// as. All three are validated by [`crate::config::WaitlistConfig`].
    pub fn new(
        base_url: String,
        api_key: String,
        from: String,
        timeout: Duration,
    ) -> Result<Self, ResendError> {
        let http = reqwest::Client::builder()
            // Redirects refused: the request carries the API KEY and an invite
            // code in its body, and a redirect is how either ends up at a host
            // nobody chose.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| ResendError::Unreachable)?;
        Ok(Self {
            base_url,
            auth_header: format!("Bearer {api_key}"),
            from,
            http,
        })
    }

    /// Mail one invite code to one address.
    ///
    /// `signup_url` is where the code is redeemed; it is rendered as a plain
    /// link and the code is NOT put in it. A code in a query string is a code
    /// in an edge log, a referrer header, and a browser history, which is the
    /// one place this crate promises it never goes.
    ///
    /// Returns `Ok(())` once the provider has ACCEPTED the message. That is not
    /// delivery, and the caller's stamp says exactly that much.
    pub async fn send_invite(
        &self,
        to: &str,
        code: &str,
        signup_url: &str,
    ) -> Result<(), ResendError> {
        // Validated by the route that took it, asserted here because this value
        // came from a public form and is about to become somebody's mail.
        if !is_recipient(to) {
            return Err(ResendError::Refused);
        }
        let resp = self
            .http
            .post(format!("{}/emails", self.base_url))
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
            .json(&SendRequest {
                from: &self.from,
                to: [to],
                subject: SUBJECT,
                text: text_body(code, signup_url),
                html: html_body(code, signup_url),
            })
            .send()
            .await
            .map_err(|_| ResendError::Unreachable)?;

        match resp.status().as_u16() {
            // 202 is what Resend answers today; the other two are what an
            // accepted POST is allowed to be.
            200..=202 => {
                // Drained, capped, and thrown away: the message id is of no use
                // here and keeping it would only give it somewhere to be logged.
                read_capped(resp).await?;
                Ok(())
            }
            401 | 403 => Err(ResendError::Unauthorized),
            // 400 is a malformed request, 422 an unverified sending domain or
            // an address the provider will not take. Both mean the same thing
            // to the operator: sending this again changes nothing.
            400 | 422 => Err(ResendError::Refused),
            _ => Err(ResendError::Failed),
        }
    }
}

/// The shape of a recipient: bounded, printable, and an address. The route
/// above checks the same thing more thoroughly; this is the guard on the value
/// that actually leaves the process.
fn is_recipient(to: &str) -> bool {
    (3..=MAX_RECIPIENT_LEN).contains(&to.len())
        && to.contains('@')
        && to.bytes().all(|b| b.is_ascii_graphic())
}

/// The plain-text part. The code sits on its own line so that every mail client
/// in the world lets someone select it.
fn text_body(code: &str, signup_url: &str) -> String {
    format!(
        "you're in.

here is your Passband invite code:

{code}

paste it at {signup_url} to set up your hosted mailbox.

the code works once and expires in {ttl} days. if it lapses, just ask again and
we will send a fresh one.
",
        ttl = crate::invites::DEFAULT_TTL_DAYS,
    )
}

/// The HTML part. Everything interpolated is escaped, including values this
/// crate produced itself: "this one was checked already" is how the exception
/// becomes the rule.
fn html_body(code: &str, signup_url: &str) -> String {
    let code = escape_html(code);
    let url = escape_html(signup_url);
    format!(
        r#"<div style="font: 16px/1.55 ui-sans-serif, system-ui, -apple-system, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif; color: #1a1a1a;">
<p>you're in.</p>
<p>here is your Passband invite code:</p>
<p style="font: 18px/1.4 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; letter-spacing: 0.04em; background: #f3f1ed; border-radius: 8px; padding: 14px 16px; display: inline-block;">{code}</p>
<p>paste it at <a href="{url}">{url}</a> to set up your hosted mailbox.</p>
<p style="color: #6b6b6b;">the code works once and expires in {ttl} days. if it lapses, just ask again and we will send a fresh one.</p>
</div>
"#,
        ttl = crate::invites::DEFAULT_TTL_DAYS,
    )
}

async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, ResendError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| ResendError::Unreachable)? {
        if out.len() + chunk.len() > MAX_RESPONSE_BODY {
            return Err(ResendError::Unreachable);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        extract::State as AxumState,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use serde_json::{Value, json};

    use super::*;

    const API_KEY: &str = "re_the_sending_key";
    const FROM: &str = "Passband <invites@passband.app>";
    const CODE: &str = "ABCD-EFGH-JKMN-PQRS";
    const SIGNUP_URL: &str = "https://signup.passband.app";

    /// What the mock provider recorded: the Authorization header of every
    /// request and the body of every send.
    #[derive(Default)]
    struct Recorder {
        auths: Vec<String>,
        bodies: Vec<Value>,
        /// When set, the send route answers this instead of accepting.
        response: Option<(u16, String)>,
    }

    type Shared = Arc<Mutex<Recorder>>;

    async fn spawn_provider(rec: Shared) -> String {
        let app = Router::new()
            .route(
                "/emails",
                post(
                    |AxumState(rec): AxumState<Shared>, headers: HeaderMap, body: String| async move {
                        let mut r = rec.lock().unwrap();
                        r.auths.push(
                            headers
                                .get(header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_string(),
                        );
                        r.bodies
                            .push(serde_json::from_str(&body).unwrap_or(Value::Null));
                        if let Some((status, body)) = r.response.clone() {
                            return (StatusCode::from_u16(status).unwrap(), body).into_response();
                        }
                        (
                            StatusCode::OK,
                            axum::Json(json!({ "id": "b1c2-d3e4" })),
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(rec);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn client(base_url: String) -> ResendClient {
        ResendClient::new(
            base_url,
            API_KEY.to_string(),
            FROM.to_string(),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    /// The wire: bearer, sender, one recipient, the product subject, and both
    /// parts carrying the code and a plain link.
    #[tokio::test]
    async fn sends_one_invite_with_the_code_in_both_parts() {
        let rec: Shared = Arc::default();
        let base = spawn_provider(rec.clone()).await;
        client(base)
            .send_invite("ada@example.com", CODE, SIGNUP_URL)
            .await
            .unwrap();

        let r = rec.lock().unwrap();
        assert_eq!(r.auths, vec![format!("Bearer {API_KEY}")]);
        let body = &r.bodies[0];
        assert_eq!(body["from"], FROM);
        assert_eq!(body["to"], json!(["ada@example.com"]));
        assert_eq!(body["subject"], SUBJECT);

        let text = body["text"].as_str().unwrap();
        let html = body["html"].as_str().unwrap();
        for part in [text, html] {
            assert!(part.contains(CODE), "{part}");
            assert!(part.contains(SIGNUP_URL), "{part}");
            assert!(part.contains("works once"), "{part}");
            // The constant, not the number it happens to hold: copy that says
            // "30 days" while the mint says 14 is copy that lies.
            assert!(
                part.contains(&format!("{} days", crate::invites::DEFAULT_TTL_DAYS)),
                "{part}"
            );
            // House copy rule.
            assert!(!part.contains('\u{2014}'), "{part}");
        }
        // The code sits on its own line in the text part, so it can be
        // selected, and NEVER in the link.
        assert!(text.contains(&format!("\n\n{CODE}\n\n")), "{text}");
        assert!(!text.contains(&format!("{SIGNUP_URL}?")), "{text}");
        assert!(!html.contains(&format!("{SIGNUP_URL}?")), "{html}");
    }

    /// Four outcomes, and every one of them says nothing about the key, the
    /// address, or the code.
    #[tokio::test]
    async fn maps_provider_answers_onto_four_outcomes() {
        for (status, want) in [
            (401, ResendError::Unauthorized),
            (403, ResendError::Unauthorized),
            (422, ResendError::Refused),
            (400, ResendError::Refused),
            (429, ResendError::Failed),
            (500, ResendError::Failed),
        ] {
            let rec: Shared = Arc::default();
            rec.lock().unwrap().response = Some((status, "{}".into()));
            let base = spawn_provider(rec.clone()).await;
            let err = client(base)
                .send_invite("ada@example.com", CODE, SIGNUP_URL)
                .await
                .expect_err("a refusal is an error");
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&want),
                "{status}"
            );
            let rendered = err.to_string();
            assert!(!rendered.contains(API_KEY), "{rendered}");
            assert!(!rendered.contains(CODE), "{rendered}");
            assert!(!rendered.contains("ada@example.com"), "{rendered}");
            assert!(!rendered.contains(&status.to_string()), "{rendered}");
        }
    }

    /// A provider that is not there is one answer, and an address that could
    /// never be one is refused before the request is built.
    #[tokio::test]
    async fn refuses_before_it_sends_what_it_should_not() {
        let rec: Shared = Arc::default();
        let base = spawn_provider(rec.clone()).await;
        let c = client(base);
        for bad in ["", "ada", &"a".repeat(MAX_RECIPIENT_LEN + 1), "a b@x.test"] {
            assert!(matches!(
                c.send_invite(bad, CODE, SIGNUP_URL).await,
                Err(ResendError::Refused)
            ));
        }
        assert!(rec.lock().unwrap().bodies.is_empty(), "nothing was sent");

        // Nothing listening: one answer, no detail.
        let dead = client("http://127.0.0.1:1".into());
        assert!(matches!(
            dead.send_invite("ada@example.com", CODE, SIGNUP_URL).await,
            Err(ResendError::Unreachable)
        ));
    }
}
