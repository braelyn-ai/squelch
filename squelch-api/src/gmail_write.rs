//! Gmail WRITE operations — the ONLY write capability in squelch.
//!
//! This module lives in squelch-api (the human door), NOT in squelch-core's
//! sync engine, so the surfacer/sync/triage core stays provably write-free. It
//! mirrors the read engine's reqwest+rustls patterns (see
//! `squelch_core::sync`) but every request carries the WRITE credential
//! ([`CredentialKind::Write`]).
//!
//! CREDENTIAL DISCIPLINE:
//! - The write token is fetched PER REQUEST from a [`CredentialStore`] bound to
//!   [`CredentialKind::Write`] and dropped when the call returns. It is never
//!   held long-lived and is never reachable from any read/sync path.
//! - `Authorization` headers, tokens, and message bodies are NEVER logged.
//!
//! The request-shaping functions ([`modify_request`], [`build_reply_rfc822`],
//! [`send_request_body`]) are pure so their Gmail API shapes are unit-testable
//! without a network.

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use squelch_core::credentials::CredentialStore;
use squelch_core::store::ActionMessageRef;
use squelch_core::types::AccountId;

/// Gmail REST base for the authenticated user. Same host the read engine uses.
const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// The Gmail system label for the inbox. Archiving == removing this label.
pub const LABEL_INBOX: &str = "INBOX";

/// An error from a write operation. Kept deliberately coarse and free of any
/// token/body content; callers map it to an HTTP status.
#[derive(Debug)]
pub enum WriteError {
    /// No write credential is configured/stored (run `squelchd auth --write`).
    MissingCredential(String),
    /// The Gmail API returned a non-success status.
    Api { status: u16, message: String },
    /// A local/transport failure (network, serialization, credential store).
    Transport(String),
    /// The caller passed invalid input (e.g. empty send body).
    Invalid(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::MissingCredential(m) => write!(f, "{m}"),
            WriteError::Api { status, message } => write!(f, "gmail api status {status}: {message}"),
            WriteError::Transport(m) => write!(f, "{m}"),
            WriteError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure request shaping (unit-testable, no network).
// ---------------------------------------------------------------------------

/// The modify JSON body (`addLabelIds`/`removeLabelIds`). Empty arrays are still
/// sent (harmless no-ops) so the shape is uniform. Pure and base-independent.
pub fn modify_body(add: &[String], remove: &[String]) -> Value {
    json!({
        "addLabelIds": add,
        "removeLabelIds": remove,
    })
}

/// The `(path, json_body)` for a `messages.modify` call, relative to the API
/// base. `add`/`remove` are label ids.
pub fn modify_request(gmail_msg_id: &str, add: &[String], remove: &[String]) -> (String, Value) {
    let url = format!("{GMAIL_API_BASE}/messages/{gmail_msg_id}/modify");
    (url, modify_body(add, remove))
}

/// The archive request: remove `INBOX`, add nothing.
pub fn archive_request(gmail_msg_id: &str) -> (String, Value) {
    modify_request(gmail_msg_id, &[], &[LABEL_INBOX.to_string()])
}

/// Inputs for composing a reply/new message.
#[derive(Debug, Clone)]
pub struct ReplyParts {
    /// Recipient. For a reply this defaults to the original sender.
    pub to: String,
    pub subject: String,
    pub body: String,
    /// The original message's `Message-ID` header, if known (for a reply).
    pub in_reply_to: Option<String>,
    /// The accumulated `References` chain, if known (for a reply).
    pub references: Option<String>,
}

/// Build a minimal RFC822 message from [`ReplyParts`]. Header values are guarded
/// against CRLF injection (any header line containing a bare CR/LF is rejected
/// component-by-component). Returns the raw bytes ready for base64url encoding.
pub fn build_reply_rfc822(parts: &ReplyParts) -> Result<Vec<u8>, WriteError> {
    // Header-injection guard: no field that becomes a header line may contain a
    // CR or LF. The body is allowed newlines (it lives after the blank line).
    for (name, val) in [
        ("To", parts.to.as_str()),
        ("Subject", parts.subject.as_str()),
        ("In-Reply-To", parts.in_reply_to.as_deref().unwrap_or("")),
        ("References", parts.references.as_deref().unwrap_or("")),
    ] {
        if val.contains('\r') || val.contains('\n') {
            return Err(WriteError::Invalid(format!(
                "{name} header must not contain CR/LF"
            )));
        }
    }
    if parts.to.trim().is_empty() {
        return Err(WriteError::Invalid("reply has no recipient".into()));
    }

    let mut out = String::new();
    out.push_str(&format!("To: {}\r\n", parts.to));
    out.push_str(&format!("Subject: {}\r\n", parts.subject));
    if let Some(irt) = parts.in_reply_to.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("In-Reply-To: {irt}\r\n"));
    }
    if let Some(refs) = parts.references.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("References: {refs}\r\n"));
    }
    out.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("\r\n");
    // Body: normalize bare LFs to CRLF for RFC822 line endings.
    out.push_str(&parts.body.replace("\r\n", "\n").replace('\n', "\r\n"));
    Ok(out.into_bytes())
}

/// The `messages.send` JSON body. `raw` is RFC822 bytes, base64url-encoded (no
/// padding) as Gmail expects. `threadId` is set only when threading a reply.
/// Pure and base-independent.
pub fn send_body(raw: &[u8], thread_id: Option<&str>) -> Value {
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let mut body = json!({ "raw": encoded });
    if let Some(tid) = thread_id {
        body["threadId"] = Value::String(tid.to_string());
    }
    body
}

/// The `(url, json_body)` for a `messages.send` call.
pub fn send_request_body(raw: &[u8], thread_id: Option<&str>) -> (String, Value) {
    let url = format!("{GMAIL_API_BASE}/messages/send");
    (url, send_body(raw, thread_id))
}

/// Compute the reply subject: prepend `Re: ` unless it is already present.
pub fn reply_subject(original: &str) -> String {
    let trimmed = original.trim();
    if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("re:") {
        trimmed.to_string()
    } else {
        format!("Re: {trimmed}")
    }
}

/// Build the `References` chain for a reply: append the parent `Message-ID` to
/// any pre-existing references. Returns `None` when there is nothing to chain.
pub fn build_references(parent_message_id: Option<&str>, parent_references: Option<&str>) -> Option<String> {
    match (parent_references, parent_message_id) {
        (Some(refs), Some(mid)) => Some(format!("{refs} {mid}")),
        (Some(refs), None) => Some(refs.to_string()),
        (None, Some(mid)) => Some(mid.to_string()),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Network executor. Holds a Write-bound credential store; nothing else.
// ---------------------------------------------------------------------------

/// Gmail metadata headers we read (with the WRITE token) to thread a reply.
#[derive(Debug, Default, Clone)]
pub struct ParentHeaders {
    pub message_id: Option<String>,
    pub references: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageMetadata {
    #[serde(default)]
    payload: Option<MetaPayload>,
}

#[derive(Debug, Deserialize)]
struct MetaPayload {
    #[serde(default)]
    headers: Vec<MetaHeader>,
}

#[derive(Debug, Deserialize)]
struct MetaHeader {
    name: String,
    value: String,
}

/// Executes Gmail write ops with a WRITE-bound credential store. The token is
/// fetched per call from `creds` and never retained.
pub struct GmailWriteClient {
    creds: std::sync::Arc<dyn CredentialStore>,
    account_id: AccountId,
    http: reqwest::Client,
    /// API base URL. Defaults to Gmail's; overridable in tests to point at a
    /// local mock server (production always uses the real base).
    base: String,
}

impl GmailWriteClient {
    pub fn new(creds: std::sync::Arc<dyn CredentialStore>, account_id: AccountId) -> Self {
        Self::with_base(creds, account_id, GMAIL_API_BASE.to_string())
    }

    /// Construct with an explicit API base (tests). Production uses [`Self::new`].
    pub fn with_base(
        creds: std::sync::Arc<dyn CredentialStore>,
        account_id: AccountId,
        base: String,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client build");
        Self {
            creds,
            account_id,
            http,
            base,
        }
    }

    /// Fetch a fresh write access token. Absent/failed credential => a
    /// `MissingCredential` error (which the handler maps to 403 with a hint).
    async fn write_token(&self) -> Result<String, WriteError> {
        match self.creds.token(self.account_id).await {
            Ok(t) => Ok(t.access_token),
            Err(e) => Err(WriteError::MissingCredential(format!(
                "no write credential available: {e}"
            ))),
        }
    }

    /// POST a JSON body to `url` with the write bearer token. On success returns
    /// the parsed JSON response. Never logs the token or the body.
    async fn post_json(&self, url: &str, body: &Value) -> Result<Value, WriteError> {
        let token = self.write_token().await?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(&token)
            .json(body)
            .send()
            .await
            .map_err(|e| WriteError::Transport(format!("gmail request failed: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            resp.json::<Value>()
                .await
                .map_err(|e| WriteError::Transport(format!("gmail json decode: {e}")))
        } else {
            // Do not echo the response body (may contain request context); report
            // only the status code.
            Err(WriteError::Api {
                status: status.as_u16(),
                message: "request rejected".into(),
            })
        }
    }

    /// `messages.modify`: add/remove labels on a Gmail message.
    pub async fn modify(
        &self,
        gmail_msg_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<(), WriteError> {
        let url = format!("{}/messages/{gmail_msg_id}/modify", self.base);
        let body = modify_body(add, remove);
        self.post_json(&url, &body).await.map(|_| ())
    }

    /// Archive: remove the INBOX label.
    pub async fn archive(&self, gmail_msg_id: &str) -> Result<(), WriteError> {
        self.modify(gmail_msg_id, &[], &[LABEL_INBOX.to_string()]).await
    }

    /// Read the parent message's threading headers (Message-ID, References) using
    /// the WRITE token (gmail.modify grants read too). Used only to thread a
    /// reply correctly; never exposes a body.
    pub async fn parent_headers(&self, gmail_msg_id: &str) -> Result<ParentHeaders, WriteError> {
        let url = format!(
            "{}/messages/{gmail_msg_id}\
             ?format=metadata&metadataHeaders=Message-ID&metadataHeaders=References",
            self.base
        );
        let token = self.write_token().await?;
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| WriteError::Transport(format!("gmail request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(WriteError::Api {
                status: status.as_u16(),
                message: "request rejected".into(),
            });
        }
        let meta: MessageMetadata = resp
            .json()
            .await
            .map_err(|e| WriteError::Transport(format!("gmail json decode: {e}")))?;
        let mut out = ParentHeaders::default();
        if let Some(p) = meta.payload {
            for h in p.headers {
                if h.name.eq_ignore_ascii_case("message-id") {
                    out.message_id = Some(h.value);
                } else if h.name.eq_ignore_ascii_case("references") {
                    out.references = Some(h.value);
                }
            }
        }
        Ok(out)
    }

    /// `messages.send`: send a raw RFC822 message, optionally threaded.
    pub async fn send(&self, raw: &[u8], thread_id: Option<&str>) -> Result<(), WriteError> {
        let url = format!("{}/messages/send", self.base);
        let body = send_body(raw, thread_id);
        self.post_json(&url, &body).await.map(|_| ())
    }
}

/// Resolve the default reply recipient for an action target: the original
/// sender's address.
pub fn default_recipient(msg: &ActionMessageRef) -> String {
    msg.from_addr.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_removes_inbox_only() {
        let (url, body) = archive_request("abc123");
        assert!(url.ends_with("/messages/abc123/modify"));
        assert_eq!(body["removeLabelIds"], json!(["INBOX"]));
        assert_eq!(body["addLabelIds"], json!([] as [String; 0]));
    }

    #[test]
    fn modify_add_and_remove_shape() {
        let (url, body) = modify_request(
            "m1",
            &["Label_1".to_string(), "STARRED".to_string()],
            &["UNREAD".to_string()],
        );
        assert!(url.ends_with("/messages/m1/modify"));
        assert_eq!(body["addLabelIds"], json!(["Label_1", "STARRED"]));
        assert_eq!(body["removeLabelIds"], json!(["UNREAD"]));
    }

    #[test]
    fn send_body_is_base64url_nopad_and_threaded() {
        let raw = b"To: a@b.com\r\nSubject: hi\r\n\r\nbody";
        let (url, body) = send_request_body(raw, Some("thread-42"));
        assert!(url.ends_with("/messages/send"));
        assert_eq!(body["threadId"], "thread-42");
        let enc = body["raw"].as_str().unwrap();
        // No padding, web-safe.
        assert!(!enc.contains('='));
        assert!(!enc.contains('+') && !enc.contains('/'));
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(enc)
            .unwrap();
        assert_eq!(decoded, raw);
    }

    #[test]
    fn send_body_without_thread_omits_threadid() {
        let (_url, body) = send_request_body(b"x", None);
        assert!(body.get("threadId").is_none());
    }

    #[test]
    fn reply_subject_prefixes_once() {
        assert_eq!(reply_subject("Lunch?"), "Re: Lunch?");
        assert_eq!(reply_subject("Re: Lunch?"), "Re: Lunch?");
        assert_eq!(reply_subject("RE: Lunch?"), "RE: Lunch?");
        assert_eq!(reply_subject("  spaced  "), "Re: spaced");
    }

    #[test]
    fn references_chain_appends_parent() {
        assert_eq!(
            build_references(Some("<b@x>"), Some("<a@x> <ab@x>")),
            Some("<a@x> <ab@x> <b@x>".to_string())
        );
        assert_eq!(build_references(Some("<b@x>"), None), Some("<b@x>".to_string()));
        assert_eq!(build_references(None, Some("<a@x>")), Some("<a@x>".to_string()));
        assert_eq!(build_references(None, None), None);
    }

    #[test]
    fn reply_rfc822_has_threading_headers() {
        let parts = ReplyParts {
            to: "alice@example.com".into(),
            subject: "Re: Hi".into(),
            body: "hello\nthere".into(),
            in_reply_to: Some("<parent@x>".into()),
            references: Some("<root@x> <parent@x>".into()),
        };
        let raw = build_reply_rfc822(&parts).unwrap();
        let s = String::from_utf8(raw).unwrap();
        assert!(s.contains("To: alice@example.com\r\n"));
        assert!(s.contains("Subject: Re: Hi\r\n"));
        assert!(s.contains("In-Reply-To: <parent@x>\r\n"));
        assert!(s.contains("References: <root@x> <parent@x>\r\n"));
        // Blank line separates headers from body; body CRLF-normalized.
        assert!(s.contains("\r\n\r\nhello\r\nthere"));
    }

    #[test]
    fn reply_rfc822_rejects_header_injection() {
        let parts = ReplyParts {
            to: "a@b.com\r\nBcc: evil@x.com".into(),
            subject: "hi".into(),
            body: "x".into(),
            in_reply_to: None,
            references: None,
        };
        assert!(matches!(
            build_reply_rfc822(&parts),
            Err(WriteError::Invalid(_))
        ));
    }

    #[test]
    fn reply_rfc822_rejects_empty_recipient() {
        let parts = ReplyParts {
            to: "   ".into(),
            subject: "hi".into(),
            body: "x".into(),
            in_reply_to: None,
            references: None,
        };
        assert!(matches!(
            build_reply_rfc822(&parts),
            Err(WriteError::Invalid(_))
        ));
    }

    // ---- network executor against a one-shot mock server ------------------

    use async_trait::async_trait;
    use squelch_core::credentials::OAuthToken;
    use squelch_core::error::Result as CoreResult;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A credential store that hands out a fixed write token (no keyring/file).
    struct StubCreds;
    #[async_trait]
    impl CredentialStore for StubCreds {
        async fn token(&self, _account: AccountId) -> CoreResult<OAuthToken> {
            Ok(OAuthToken {
                access_token: "WRITE-TOKEN".into(),
                refresh_token: None,
                expires_at: None,
            })
        }
    }

    /// Spawn a one-shot HTTP/1.1 server that captures the first request's raw
    /// bytes and replies with `status`/`resp_body`. Returns (base_url, join).
    async fn mock_once(
        status: u16,
        resp_body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            req
        });
        (format!("http://{addr}"), handle)
    }

    fn client(base: String) -> GmailWriteClient {
        GmailWriteClient::with_base(Arc::new(StubCreds), 1_i64, base)
    }

    #[tokio::test]
    async fn archive_sends_modify_removing_inbox() {
        let (base, handle) = mock_once(200, "{}").await;
        let c = client(base);
        c.archive("gmail-123").await.unwrap();
        let req = handle.await.unwrap();
        assert!(req.starts_with("POST "), "must be a POST");
        assert!(req.contains("/messages/gmail-123/modify"), "modify path");
        assert!(req.contains("authorization: Bearer WRITE-TOKEN") || req.contains("Authorization: Bearer WRITE-TOKEN"));
        // Body carries removeLabelIds:["INBOX"] and empty addLabelIds.
        assert!(req.contains("\"removeLabelIds\":[\"INBOX\"]"));
        assert!(req.contains("\"addLabelIds\":[]"));
    }

    #[tokio::test]
    async fn send_posts_raw_and_threadid() {
        let (base, handle) = mock_once(200, "{\"id\":\"x\"}").await;
        let c = client(base);
        let raw = b"To: a@b.com\r\nSubject: hi\r\n\r\nbody";
        c.send(raw, Some("thread-9")).await.unwrap();
        let req = handle.await.unwrap();
        assert!(req.contains("/messages/send"), "send path");
        assert!(req.contains("\"threadId\":\"thread-9\""));
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        assert!(req.contains(&expected), "raw base64url payload present");
    }

    #[tokio::test]
    async fn api_error_status_is_surfaced_without_body() {
        let (base, handle) = mock_once(403, "{\"error\":\"insufficientPermissions\"}").await;
        let c = client(base);
        let err = c.archive("g1").await.unwrap_err();
        handle.await.unwrap();
        match err {
            WriteError::Api { status, message } => {
                assert_eq!(status, 403);
                // The upstream body is NOT echoed.
                assert!(!message.contains("insufficientPermissions"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
