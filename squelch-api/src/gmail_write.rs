//! Gmail WRITE operations — the ONLY write capability in squelch. Lives here,
//! not in squelch-core, so the sync/triage core stays provably write-free.
//! The write token is fetched PER REQUEST from a [`CredentialStore`] bound to
//! [`CredentialKind::Write`], dropped when the call returns, and never reachable
//! from a read path. Tokens, auth headers and bodies are NEVER logged.

use base64::Engine as _;
use serde_json::{Value, json};

use squelch_core::credentials::CredentialStore;
use squelch_core::store::ActionMessageRef;
// The Gmail endpoint, the INBOX label and the Gmail-API response shapes are
// defined once in core sync; the write path reuses them rather than re-deriving
// its own copy. Archiving == removing `LABEL_INBOX`.
use squelch_core::sync::{GMAIL_API_BASE, GmailMessage, LABEL_INBOX};
use squelch_core::types::AccountId;

/// An error from a write operation: deliberately coarse and free of any
/// token/body content.
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
            WriteError::Api { status, message } => {
                write!(f, "gmail api status {status}: {message}")
            }
            WriteError::Transport(m) => write!(f, "{m}"),
            WriteError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure request shaping (unit-testable, no network).
// ---------------------------------------------------------------------------

/// The modify JSON body. Empty arrays are still sent (harmless no-ops) so the
/// shape is uniform.
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
    /// Carbon copies, comma-joined. `Some` only for reply-all; the header is
    /// omitted entirely when this is `None` or empty, so an ordinary reply is
    /// byte-identical to what it has always been.
    pub cc: Option<String>,
    pub subject: String,
    pub body: String,
    /// The original message's `Message-ID` header, if known (for a reply).
    pub in_reply_to: Option<String>,
    /// The accumulated `References` chain, if known (for a reply).
    pub references: Option<String>,
    /// HTML alternative rendered SERVER-SIDE from `body` (see `markdown.rs`) —
    /// never caller-supplied, so the guard's scan of `body` covers it. Some =
    /// multipart/alternative with `body` as the text/plain part.
    pub body_html: Option<String>,
    /// Read-tracking pixel URL. Some also forces multipart/alternative: the
    /// `<img>` rides in the HTML part ONLY, so a plain-text reader fetches
    /// nothing and reads nothing back to us.
    pub pixel_url: Option<String>,
}

/// Escape text for interpolation into HTML markup or a double-quoted attribute
/// value. `&` first, or it would double-escape the entities emitted after it.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Plain text as minimal HTML: blank lines split paragraphs, single newlines
/// inside one become `<br>`. Used only when no `body_html` was rendered —
/// something has to carry the pixel.
fn paragraphs_html(body: &str) -> String {
    let normalized = body.replace("\r\n", "\n");
    let mut out = String::new();
    for para in normalized.split("\n\n") {
        let para = para.trim_matches('\n');
        if para.trim().is_empty() {
            continue;
        }
        let lines: Vec<String> = para.lines().map(escape_html).collect();
        out.push_str(&format!("<p>{}</p>\n", lines.join("<br>")));
    }
    out
}

/// The HTML alternative, or `None` when the message stays text/plain. Present
/// when there is rendered HTML to send, a pixel to carry, or both.
fn html_alternative(parts: &ReplyParts) -> Option<String> {
    if parts.body_html.is_none() && parts.pixel_url.is_none() {
        return None;
    }
    let inner = match parts.body_html.as_deref() {
        Some(html) => html.to_string(),
        None => paragraphs_html(&parts.body),
    };
    let pixel = match parts.pixel_url.as_deref() {
        Some(url) => format!(
            "<img src=\"{}\" width=\"1\" height=\"1\" alt=\"\">\n",
            escape_html(url)
        ),
        None => String::new(),
    };
    Some(format!("<html><body>\n{inner}{pixel}</body></html>\n"))
}

/// A multipart boundary none of the parts contain. Deterministic: a fixed
/// distinctive prefix, and if a part actually contains it (someone typed it),
/// bump the suffix until none does — content can imitate any single boundary,
/// but not every member of an unbounded family.
fn multipart_boundary(parts: &[&str]) -> String {
    let mut n = 0u32;
    loop {
        let boundary = format!("=_passband_alt_{n}");
        if parts.iter().all(|p| !p.contains(&boundary)) {
            return boundary;
        }
        n += 1;
    }
}

/// Build a minimal RFC822 message from [`ReplyParts`], guarded against CRLF
/// header injection. Returns raw bytes ready for base64url encoding.
///
/// With `body_html` or `pixel_url` set the message is multipart/alternative:
/// text/plain first (the raw source, exactly as typed), text/html second. Body
/// content stays structurally inert — the only string that delimits parts is
/// the boundary, and [`multipart_boundary`] guarantees neither part contains it.
pub fn build_reply_rfc822(parts: &ReplyParts) -> Result<Vec<u8>, WriteError> {
    // No field that becomes a header line may contain CR or LF. The body may
    // (it lives after the blank line).
    for (name, val) in [
        ("To", parts.to.as_str()),
        ("Cc", parts.cc.as_deref().unwrap_or("")),
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
    // An empty Cc is no Cc: writing a bare `Cc: ` header would announce a
    // reply-all that copied nobody.
    if let Some(cc) = parts.cc.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("Cc: {cc}\r\n"));
    }
    out.push_str(&format!("Subject: {}\r\n", parts.subject));
    if let Some(irt) = parts.in_reply_to.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("In-Reply-To: {irt}\r\n"));
    }
    if let Some(refs) = parts.references.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("References: {refs}\r\n"));
    }
    // Bodies: normalize bare LFs to CRLF for RFC822 line endings.
    let text = parts.body.replace("\r\n", "\n").replace('\n', "\r\n");
    match html_alternative(parts).as_deref() {
        None => {
            out.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
            out.push_str("MIME-Version: 1.0\r\n");
            out.push_str("\r\n");
            out.push_str(&text);
        }
        Some(html) => {
            let html = html.replace("\r\n", "\n").replace('\n', "\r\n");
            let boundary = multipart_boundary(&[&text, &html]);
            out.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n"
            ));
            out.push_str("MIME-Version: 1.0\r\n");
            out.push_str("\r\n");
            out.push_str(&format!(
                "--{boundary}\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\n{text}\r\n"
            ));
            out.push_str(&format!(
                "--{boundary}\r\nContent-Type: text/html; charset=\"UTF-8\"\r\n\r\n{html}\r\n"
            ));
            out.push_str(&format!("--{boundary}--\r\n"));
        }
    }
    Ok(out.into_bytes())
}

/// The `messages.send` JSON body: `raw` base64url-encoded WITHOUT padding as
/// Gmail expects, `threadId` set only when threading a reply.
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

/// The `References` chain for a reply: the parent `Message-ID` appended to any
/// pre-existing references, or `None` when there is nothing to chain.
pub fn build_references(
    parent_message_id: Option<&str>,
    parent_references: Option<&str>,
) -> Option<String> {
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

/// Gmail metadata headers we read (with the WRITE token) to thread a reply and
/// to derive who it goes to.
///
/// The address fields hold the RAW header values Gmail returned — display names,
/// groups, folding and all. NOTHING here reaches the wire: every recipient is
/// re-derived as a bare address by [`derive_reply_recipients`], which is what
/// keeps an attacker-authored display name from becoming a header.
#[derive(Debug, Default, Clone)]
pub struct ParentHeaders {
    pub message_id: Option<String>,
    pub references: Option<String>,
    pub reply_to: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
}

/// The recipients a reply should carry: bare addresses, comma-joined, in the
/// order the parent listed them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplyRecipients {
    pub to: String,
    /// Empty for a plain reply, and for a reply-all with nobody left to copy.
    pub cc: String,
}

/// Shape an address must have to be emitted into a comma-joined header list:
/// EXACTLY one `@` with a non-empty local part and domain on either side of it
/// — mail-parser hands back `@corp.test` for a quoted local part it stripped,
/// and `a@`/`a@b@c` for other recoveries, none of which Gmail will accept, and
/// one such fragment in a Cc 502s every reply-all on the thread forever.
/// Beyond the shape: `,` would forge a recipient, `<>";\` and `:` are
/// address/group syntax, and whitespace/controls are header structure.
fn addr_is_emittable(addr: &str) -> bool {
    let mut parts = addr.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !addr.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, ',' | '<' | '>' | '"' | ';' | ':' | '\\')
        })
}

/// One address-list header VALUE -> the bare addresses in it, in order.
///
/// The value is parsed STRUCTURALLY (never split on commas: `"Doe, Jane"
/// <j@x>` is one recipient, and a display name is attacker-authored text), by
/// handing mail-parser a synthetic one-header message. CR/LF are flattened to
/// spaces first so a crafted value cannot open a second header inside that
/// synthetic message; header folding, which is what those bytes legitimately
/// mean here, survives unchanged. Anything that comes back still carrying a
/// character it must not is DROPPED rather than repaired — a mangled recipient
/// is worse than a missing one.
fn parse_addr_list(value: &str) -> Vec<String> {
    if value.trim().is_empty() {
        return Vec::new();
    }
    let flattened: String = value
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    // A FOLDED header's continuation is part of this ONE value, so an attacker
    // can hide `Bcc: evil@y` in it: after flattening it reads as top-level text
    // `bob@x Bcc: evil@y`, and mail-parser then DROPS bob and recovers evil —
    // silently swapping the audience. Cut the value at the first embedded
    // header-name token so only the honest prefix survives (fewer recipients,
    // never a forged one, and the drop is visible in the review pane).
    let sanitized = truncate_at_injected_header(&flattened);
    let synthetic = format!("To: {sanitized}\r\n\r\n");
    let Some(msg) = mail_parser::MessageParser::default().parse(synthetic.as_bytes()) else {
        return Vec::new();
    };
    let Some(addrs) = msg.to() else {
        return Vec::new();
    };
    addrs
        .iter()
        .filter_map(|a| a.address())
        .map(str::trim)
        .filter(|a| addr_is_emittable(a))
        .map(str::to_string)
        .collect()
}

/// Cut a flattened header value at the first `keyword:` token sitting at the
/// top level — outside quotes and outside `<...>`. In a real address-list value
/// a bare `word:` only ever appears as smuggled header structure (or group
/// syntax, whose members we do not support anyway), so everything from there on
/// is dropped rather than trusted.
fn truncate_at_injected_header(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut in_quote = false;
    let mut angle_depth = 0i32;
    let mut token_start: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => {
                in_quote = !in_quote;
                token_start = None;
            }
            b'<' if !in_quote => {
                angle_depth += 1;
                token_start = None;
            }
            b'>' if !in_quote => {
                angle_depth = (angle_depth - 1).max(0);
                token_start = None;
            }
            b':' if !in_quote && angle_depth == 0 => {
                // A run of letters/hyphens ending at this colon is a header
                // name — cut the value before it began.
                if let Some(start) = token_start {
                    return &value[..start];
                }
            }
            _ if in_quote || angle_depth > 0 => {}
            b if b.is_ascii_alphabetic() || b == b'-' => {
                if token_start.is_none() {
                    token_start = Some(i);
                }
            }
            _ => token_start = None,
        }
    }
    value
}

/// Case-insensitive membership over a list of already-lowercased addresses.
fn contains_addr(seen: &[String], addr: &str) -> bool {
    let lower = addr.to_ascii_lowercase();
    seen.contains(&lower)
}

/// Who a reply to `headers` should address.
///
/// - `to` is the parent's `Reply-To` when it yields addresses, else its `From`.
/// - `reply_all` = false stops there: no `cc`.
/// - `reply_all` = true adds `cc` = the parent's `To` + `Cc`, minus `own_addr`,
///   minus anyone already in `to`, deduplicated case-insensitively, original
///   order preserved.
/// - REPLYING-ALL TO YOUR OWN MESSAGE (the parent's `to` reduces to only
///   `own_addr`): the parent's `To` becomes the reply's `To` and its `Cc`
///   stays the `Cc`, minus self — addressing the room, not the mirror. This is
///   what every other mail client does with reply-all on own sent mail.
/// - Headers too broken to yield anything return an EMPTY `to`; the callers
///   (send path and preview route) fall back to the STORE's `from_addr` — a
///   reply must never compose recipient-less, and a note-to-self is a real
///   thing to want.
///
/// Output is bare addresses only. Display names are dropped, not escaped, so
/// there is no quoting rule left to get wrong.
///
/// `own_addr` is the ONE canonical account address: Gmail aliases and
/// plus-addressed variants are not known here, so a reply-all can still copy
/// the user's own alias. Self-exclusion is best-effort, not complete.
pub fn derive_reply_recipients(
    headers: &ParentHeaders,
    own_addr: &str,
    reply_all: bool,
) -> ReplyRecipients {
    let own = own_addr.trim().to_ascii_lowercase();
    let from = parse_addr_list(headers.from.as_deref().unwrap_or(""));
    let reply_to = parse_addr_list(headers.reply_to.as_deref().unwrap_or(""));
    let mut to: Vec<String> = if reply_to.is_empty() {
        from.clone()
    } else {
        reply_to
    };

    if !reply_all {
        return ReplyRecipients {
            to: to.join(", "),
            cc: String::new(),
        };
    }

    let header_to = parse_addr_list(headers.to.as_deref().unwrap_or(""));
    let header_cc = parse_addr_list(headers.cc.as_deref().unwrap_or(""));

    // Own sent message: `to` so far is only the user. Promote the parent's To
    // into the reply's To (minus self) so the reply addresses the room.
    let is_own_echo =
        !own.is_empty() && !to.is_empty() && to.iter().all(|a| a.to_ascii_lowercase() == own);
    if is_own_echo {
        let promoted: Vec<String> = header_to
            .iter()
            .filter(|a| a.to_ascii_lowercase() != own)
            .cloned()
            .collect();
        if !promoted.is_empty() {
            to = promoted;
        }
    }

    // Seed the seen-set with the account itself and everyone already in `to`, so
    // the user never copies themselves and no one is addressed twice.
    let mut seen: Vec<String> = to.iter().map(|a| a.to_ascii_lowercase()).collect();
    if !own.is_empty() {
        seen.push(own.clone());
    }
    let mut cc: Vec<String> = Vec::new();
    for addr in header_to.into_iter().chain(header_cc) {
        if !contains_addr(&seen, &addr) {
            seen.push(addr.to_ascii_lowercase());
            cc.push(addr);
        }
    }

    ReplyRecipients {
        to: to.join(", "),
        cc: cc.join(", "),
    }
}

/// `cc` minus every address already in `to`. Used when an explicit `to`
/// overrides the derived one: the derived Cc was computed against a recipient
/// the caller then replaced, so it can now duplicate them.
pub fn cc_excluding(cc: &str, to: &str) -> String {
    let exclude: Vec<String> = parse_addr_list(to)
        .iter()
        .map(|a| a.to_ascii_lowercase())
        .collect();
    parse_addr_list(cc)
        .into_iter()
        .filter(|a| !contains_addr(&exclude, a))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `messages.send` echoed back about the message it created. Parsed
/// leniently — both fields `None` on a `{}` response — because the send has
/// ALREADY succeeded by the time this is built: a missing id costs us the local
/// echo, never the request.
#[derive(Debug, Default, Clone)]
pub struct SentRef {
    pub id: Option<String>,
    pub thread_id: Option<String>,
}

/// One `format=raw` message as Gmail returns it: the base64url RFC822 body plus
/// the ids/date it carries alongside. Decoding is the caller's job (core owns
/// `decode_raw_b64url`).
#[derive(Debug, Clone)]
pub struct FetchedRaw {
    pub raw_b64: String,
    pub thread_id: Option<String>,
    /// Milliseconds since epoch as a decimal string (Gmail's `internalDate`).
    pub internal_date: Option<String>,
}

/// Executes Gmail write ops with a WRITE-bound credential store. The token is
/// fetched per call and never retained.
pub struct GmailWriteClient {
    creds: std::sync::Arc<dyn CredentialStore>,
    account_id: AccountId,
    http: reqwest::Client,
    /// API base URL. Overridable in tests only; production uses Gmail's.
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

    /// Fetch a fresh write access token; an absent/failed credential is
    /// `MissingCredential`, which the handler maps to a 403.
    async fn write_token(&self) -> Result<String, WriteError> {
        match self.creds.token(self.account_id).await {
            Ok(t) => Ok(t.access_token),
            Err(e) => Err(WriteError::MissingCredential(format!(
                "no write credential available: {e}"
            ))),
        }
    }

    /// POST a JSON body with the write bearer token. Never logs the token or
    /// the body.
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
            // Never echo the upstream body (it may carry request context).
            Err(WriteError::Api {
                status: status.as_u16(),
                message: "request rejected".into(),
            })
        }
    }

    /// GET a JSON body with the write bearer token (gmail.modify grants read).
    /// Never logs the token or the response.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, WriteError> {
        let token = self.write_token().await?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| WriteError::Transport(format!("gmail request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            // Never echo the upstream body (it may carry request context).
            return Err(WriteError::Api {
                status: status.as_u16(),
                message: "request rejected".into(),
            });
        }
        resp.json::<T>()
            .await
            .map_err(|e| WriteError::Transport(format!("gmail json decode: {e}")))
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
        self.modify(gmail_msg_id, &[], &[LABEL_INBOX.to_string()])
            .await
    }

    /// Move a message to Gmail's Trash — `/trash`, NEVER `messages.delete`.
    /// Trashed mail is recoverable for 30 days, and permanent delete requires
    /// the full `https://mail.google.com/` scope, which squelch never requests:
    /// the blast radius is capped by the OAuth scope, not by our restraint.
    pub async fn trash(&self, gmail_msg_id: &str) -> Result<(), WriteError> {
        let url = format!("{}/messages/{gmail_msg_id}/trash", self.base);
        self.post_json(&url, &serde_json::json!({}))
            .await
            .map(|_| ())
    }

    /// Read the parent's threading AND recipient headers with the WRITE token
    /// (gmail.modify grants read too). Metadata only — it never fetches a body.
    ///
    /// ONE fetch serves both uses: a reply-all send threads and derives its
    /// audience from the same response, so asking for the address headers costs
    /// no extra round trip even when nobody wants them.
    pub async fn parent_headers(&self, gmail_msg_id: &str) -> Result<ParentHeaders, WriteError> {
        let url = format!(
            "{}/messages/{gmail_msg_id}\
             ?format=metadata&metadataHeaders=Message-ID&metadataHeaders=References\
             &metadataHeaders=Reply-To&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Cc",
            self.base
        );
        let meta: GmailMessage = self.get_json(&url).await?;
        let mut out = ParentHeaders::default();
        // A message may legitimately carry more than one Cc (or To) header; RFC
        // 5322 says a parser should treat them as one list, so append rather
        // than let the last one win and silently shrink the audience.
        fn append(slot: &mut Option<String>, value: String) {
            match slot {
                Some(existing) => {
                    existing.push_str(", ");
                    existing.push_str(&value);
                }
                None => *slot = Some(value),
            }
        }
        if let Some(p) = meta.payload {
            for h in p.headers {
                if h.name.eq_ignore_ascii_case("message-id") {
                    out.message_id = Some(h.value);
                } else if h.name.eq_ignore_ascii_case("references") {
                    out.references = Some(h.value);
                } else if h.name.eq_ignore_ascii_case("reply-to") {
                    append(&mut out.reply_to, h.value);
                } else if h.name.eq_ignore_ascii_case("from") {
                    append(&mut out.from, h.value);
                } else if h.name.eq_ignore_ascii_case("to") {
                    append(&mut out.to, h.value);
                } else if h.name.eq_ignore_ascii_case("cc") {
                    append(&mut out.cc, h.value);
                }
            }
        }
        Ok(out)
    }

    /// `messages.send`: send a raw RFC822 message, optionally threaded. Returns
    /// what Gmail echoed about the created message (see [`SentRef`]).
    pub async fn send(&self, raw: &[u8], thread_id: Option<&str>) -> Result<SentRef, WriteError> {
        let url = format!("{}/messages/send", self.base);
        let body = send_body(raw, thread_id);
        let v = self.post_json(&url, &body).await?;
        Ok(SentRef {
            id: v["id"].as_str().map(str::to_string),
            thread_id: v["threadId"].as_str().map(str::to_string),
        })
    }

    /// Read one message `format=raw` with the WRITE token. Used to echo a
    /// just-sent message into the local store; the read credential never sees
    /// Sent mail between polls.
    pub async fn fetch_raw(&self, gmail_msg_id: &str) -> Result<FetchedRaw, WriteError> {
        let url = format!("{}/messages/{gmail_msg_id}?format=raw", self.base);
        let msg: GmailMessage = self.get_json(&url).await?;
        Ok(FetchedRaw {
            raw_b64: msg.raw.unwrap_or_default(),
            thread_id: msg.thread_id,
            internal_date: msg.internal_date,
        })
    }
}

/// The default reply recipient: the original sender's address.
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
        assert_eq!(
            build_references(Some("<b@x>"), None),
            Some("<b@x>".to_string())
        );
        assert_eq!(
            build_references(None, Some("<a@x>")),
            Some("<a@x>".to_string())
        );
        assert_eq!(build_references(None, None), None);
    }

    // ---- reply-recipient derivation ---------------------------------------

    /// [`ParentHeaders`] carrying only the address fields a derivation reads.
    fn hdrs(reply_to: &str, from: &str, to: &str, cc: &str) -> ParentHeaders {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
        ParentHeaders {
            message_id: None,
            references: None,
            reply_to: some(reply_to),
            from: some(from),
            to: some(to),
            cc: some(cc),
        }
    }

    #[test]
    fn plain_reply_answers_the_sender_and_copies_nobody() {
        let h = hdrs(
            "",
            "Alice <alice@example.com>",
            "me@example.com, bob@example.com",
            "carol@example.com",
        );
        let r = derive_reply_recipients(&h, "me@example.com", false);
        // Bare address only — the display name never survives.
        assert_eq!(r.to, "alice@example.com");
        assert_eq!(r.cc, "");
    }

    #[test]
    fn reply_to_header_beats_from() {
        let h = hdrs(
            "List <list@example.com>",
            "Alice <alice@example.com>",
            "me@example.com",
            "",
        );
        assert_eq!(
            derive_reply_recipients(&h, "me@example.com", false).to,
            "list@example.com"
        );
        // ...and reply-all addresses the Reply-To too, with From nowhere in it.
        let all = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(all.to, "list@example.com");
        assert_eq!(all.cc, "");
    }

    #[test]
    fn a_reply_to_with_no_usable_address_falls_back_to_from() {
        let h = hdrs("undisclosed-recipients:;", "alice@example.com", "", "");
        assert_eq!(
            derive_reply_recipients(&h, "me@example.com", false).to,
            "alice@example.com"
        );
    }

    #[test]
    fn reply_all_copies_the_rest_and_never_the_account() {
        let h = hdrs(
            "",
            "Alice <alice@example.com>",
            "Me <ME@Example.com>, Bob <bob@example.com>",
            "Carol <carol@example.com>",
        );
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "alice@example.com");
        // Self excluded case-INSENSITIVELY; To before Cc, original order kept.
        assert_eq!(r.cc, "bob@example.com, carol@example.com");
    }

    #[test]
    fn reply_all_never_copies_someone_already_in_to() {
        let h = hdrs(
            "",
            "alice@example.com",
            "me@example.com, ALICE@example.com",
            "bob@example.com, Bob <BOB@Example.com>",
        );
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "alice@example.com");
        // Alice is the recipient, not a copy; Bob appears once despite the case.
        assert_eq!(r.cc, "bob@example.com");
    }

    #[test]
    fn a_display_name_containing_a_comma_is_one_recipient() {
        // Splitting the value on commas would read this as three people and
        // address "Jane" and "Bob" to garbage. It is two.
        let h = hdrs(
            "",
            "\"Doe, Jane\" <jane@example.com>",
            "\"Smith, Bob\" <bob@example.com>, me@example.com",
            "",
        );
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "jane@example.com");
        assert_eq!(r.cc, "bob@example.com");
    }

    #[test]
    fn a_crafted_quoted_display_name_cannot_smuggle_a_header_or_a_recipient() {
        // The smuggle sits INSIDE the quotes, so it stays a display name (which
        // is dropped) and the real address parses out clean — no CRLF, no
        // forged Bcc, no extra recipient.
        let h = hdrs(
            "",
            "\"evil\r\nBcc: attacker@evil.test\" <alice@example.com>",
            "\"x\nTo: attacker2@evil.test\" <bob@example.com>, me@example.com",
            "",
        );
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "alice@example.com");
        assert_eq!(r.cc, "bob@example.com");
        for field in [&r.to, &r.cc] {
            assert!(!field.contains('\r') && !field.contains('\n'));
            assert!(!field.to_lowercase().contains("bcc"));
            assert!(!field.contains("evil.test"));
        }
    }

    #[test]
    fn an_unquoted_folded_bcc_smuggle_cannot_append_a_recipient() {
        // The audit's live attack: one legally-folded Cc header whose
        // continuation line reads as a second header. mail-parser RECOVERS the
        // attacker's address from the flattened text, with the smuggled label
        // soaked into its display name — the colon filter is what kills it.
        for cc in [
            "Bob <bob@corp.test>\r\n Bcc: attacker@evil.test",
            "victim@corp.test, bob@corp.test\r\n Bcc: attacker@evil.test",
            "victim@corp.test, bob@corp.test,\r\n Bcc: attacker@evil.test",
        ] {
            let h = hdrs("", "alice@example.com", "", cc);
            let r = derive_reply_recipients(&h, "me@example.com", true);
            assert!(
                !r.to.contains("evil.test") && !r.cc.contains("evil.test"),
                "attacker must never be derived from {cc:?}, got cc = {:?}",
                r.cc
            );
        }
    }

    #[test]
    fn malformed_address_shapes_are_dropped() {
        // mail-parser strips a quoted local part down to `@domain`; Gmail
        // rejects that shape, and one bad fragment in a Cc would 502 every
        // reply-all on the thread forever. Exactly one @, non-empty on both
        // sides, or it does not ship.
        let h = hdrs(
            "",
            "alice@example.com",
            "\"John Doe\"@corp.test, a@, @x.test, a@b@c.test, bob@corp.test",
            "",
        );
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.cc, "bob@corp.test");
    }

    #[test]
    fn reply_all_to_your_own_message_addresses_the_room_not_the_mirror() {
        // The parent is the user's own sent copy: From = me, To = the room.
        // The room is promoted into To; Cc stays Cc; the user appears nowhere.
        let h = hdrs(
            "",
            "me@example.com",
            "alice@example.com, bob@example.com",
            "carol@example.com",
        );
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "alice@example.com, bob@example.com");
        assert_eq!(r.cc, "carol@example.com");
    }

    #[test]
    fn reply_all_to_a_true_note_to_self_still_addresses_yourself() {
        let h = hdrs("", "me@example.com", "me@example.com", "");
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(
            r.to, "me@example.com",
            "nothing to promote; the mirror is the room"
        );
        assert_eq!(r.cc, "");
    }

    #[test]
    fn an_address_carrying_a_control_character_is_dropped_not_repaired() {
        let h = hdrs("", "\"a\" <al\ric\ne@example.com>", "bob@example.com", "");
        let r = derive_reply_recipients(&h, "me@example.com", true);
        // The mangled From yields nothing addressable, so nothing is invented
        // for it; Bob is still a valid copy.
        assert!(!r.to.contains('\r') && !r.to.contains('\n'));
        assert!(!r.cc.contains('\r') && !r.cc.contains('\n'));
        assert_eq!(r.cc, "bob@example.com");
    }

    #[test]
    fn a_note_to_self_still_addresses_self() {
        // Nobody to reply-all TO but the user; the reply must still go somewhere.
        let h = hdrs("", "me@example.com", "me@example.com", "");
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "me@example.com");
        assert_eq!(r.cc, "");
    }

    #[test]
    fn headers_with_nothing_addressable_derive_nothing() {
        let h = hdrs("", "", "", "");
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "");
        assert_eq!(r.cc, "");
    }

    #[test]
    fn an_explicit_to_takes_its_copies_off_the_cc() {
        // The caller retyped the recipient; the derived Cc must not address them
        // a second time, whatever the case.
        assert_eq!(
            cc_excluding(
                "bob@example.com, carol@example.com",
                "Bob <BOB@example.com>"
            ),
            "carol@example.com"
        );
        assert_eq!(cc_excluding("", "bob@example.com"), "");
        assert_eq!(
            cc_excluding("carol@example.com", "dave@example.com"),
            "carol@example.com"
        );
    }

    #[test]
    fn reply_rfc822_writes_cc_when_there_is_one() {
        let mut parts = bare_parts("hi");
        parts.cc = Some("bob@example.com, carol@example.com".into());
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        assert!(s.contains("To: alice@example.com\r\nCc: bob@example.com, carol@example.com\r\n"));
    }

    #[test]
    fn reply_rfc822_omits_an_absent_or_blank_cc() {
        for cc in [None, Some(String::new()), Some("   ".to_string())] {
            let mut parts = bare_parts("hi");
            parts.cc = cc.clone();
            let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
            assert!(!s.contains("Cc:"), "{cc:?} must not write a Cc header");
        }
    }

    #[test]
    fn reply_rfc822_rejects_a_cc_carrying_crlf() {
        // The derivation already drops these; this is the second belt, and it
        // refuses rather than truncates — same rule the To header has.
        for cc in [
            "bob@example.com\r\nBcc: evil@x.com",
            "bob@example.com\nBcc: evil@x.com",
        ] {
            let mut parts = bare_parts("hi");
            parts.cc = Some(cc.into());
            let err = build_reply_rfc822(&parts).unwrap_err();
            match err {
                WriteError::Invalid(m) => assert!(m.starts_with("Cc header"), "{m}"),
                other => panic!("expected Invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn reply_rfc822_has_threading_headers() {
        let parts = ReplyParts {
            to: "alice@example.com".into(),
            cc: None,
            subject: "Re: Hi".into(),
            body: "hello\nthere".into(),
            in_reply_to: Some("<parent@x>".into()),
            references: Some("<root@x> <parent@x>".into()),
            body_html: None,
            pixel_url: None,
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
            cc: None,
            subject: "hi".into(),
            body: "x".into(),
            in_reply_to: None,
            references: None,
            body_html: None,
            pixel_url: None,
        };
        assert!(matches!(
            build_reply_rfc822(&parts),
            Err(WriteError::Invalid(_))
        ));
    }

    #[test]
    fn reply_rfc822_multipart_carries_both_parts() {
        let parts = ReplyParts {
            to: "alice@example.com".into(),
            cc: None,
            subject: "hi".into(),
            body: "**bold** text".into(),
            in_reply_to: None,
            references: None,
            body_html: Some("<div><strong>bold</strong> text</div>".into()),
            pixel_url: None,
        };
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        assert!(
            s.contains("Content-Type: multipart/alternative; boundary=\"=_passband_alt_0\"\r\n")
        );
        // Plain part FIRST and verbatim — the raw markdown source, stars and all.
        let plain = s.find("Content-Type: text/plain").unwrap();
        let html = s.find("Content-Type: text/html").unwrap();
        assert!(plain < html);
        assert!(s.contains("\r\n\r\n**bold** text\r\n"));
        assert!(s.contains("<strong>bold</strong>"));
        assert!(s.ends_with("--=_passband_alt_0--\r\n"));
    }

    /// A ReplyParts with everything optional off; tests set what they exercise.
    fn bare_parts(body: &str) -> ReplyParts {
        ReplyParts {
            to: "alice@example.com".into(),
            cc: None,
            subject: "hi".into(),
            body: body.into(),
            in_reply_to: None,
            references: None,
            body_html: None,
            pixel_url: None,
        }
    }

    /// Split a built message into its top headers and `(part_headers,
    /// part_body)` pairs, using the boundary the message itself declares.
    fn parse_multipart(s: &str) -> (String, Vec<(String, String)>) {
        let (headers, body) = s.split_once("\r\n\r\n").unwrap();
        let ct = headers
            .lines()
            .find(|l| l.starts_with("Content-Type: multipart/alternative"))
            .expect("multipart content-type");
        let boundary = ct
            .split("boundary=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let body = body
            .strip_suffix(&format!("--{boundary}--\r\n"))
            .expect("closing delimiter");
        let parts = body
            .split(&format!("--{boundary}\r\n"))
            .skip(1)
            .map(|chunk| {
                let (ph, pb) = chunk.split_once("\r\n\r\n").unwrap();
                (ph.to_string(), pb.strip_suffix("\r\n").unwrap().to_string())
            })
            .collect();
        (headers.to_string(), parts)
    }

    #[test]
    fn no_html_and_no_pixel_stays_text_plain() {
        let s = String::from_utf8(build_reply_rfc822(&bare_parts("x")).unwrap()).unwrap();
        assert!(s.contains("Content-Type: text/plain; charset=\"UTF-8\"\r\n"));
        assert!(!s.contains("multipart/alternative"));
        assert!(!s.contains("<html>"));
    }

    #[test]
    fn pixel_alone_forces_multipart_and_rides_the_html_part_only() {
        let mut parts = bare_parts("hello\nthere");
        parts.pixel_url = Some("https://p.passband.app/o/abc123.gif".into());
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        let (_headers, mime) = parse_multipart(&s);
        assert_eq!(mime.len(), 2);
        let (plain_h, plain_b) = &mime[0];
        let (html_h, html_b) = &mime[1];
        assert!(plain_h.contains("Content-Type: text/plain; charset=\"UTF-8\""));
        assert!(html_h.contains("Content-Type: text/html; charset=\"UTF-8\""));
        // The plain part is byte-for-byte what the text/plain-only build emits.
        assert_eq!(plain_b, "hello\r\nthere");
        assert!(!plain_b.contains("img"));
        assert!(html_b.contains(
            "<img src=\"https://p.passband.app/o/abc123.gif\" width=\"1\" height=\"1\" alt=\"\">"
        ));
        // The pixel is the last thing in the document, inside <body>.
        let img = html_b.find("<img ").unwrap();
        assert!(img < html_b.find("</body>").unwrap());
        assert!(html_b.starts_with("<html><body>"));
    }

    #[test]
    fn no_pixel_url_means_no_img() {
        let mut parts = bare_parts("x");
        parts.body_html = Some("<div>x</div>".into());
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        assert!(!s.contains("<img"));
    }

    #[test]
    fn pixel_rides_beside_rendered_html_without_replacing_it() {
        let mut parts = bare_parts("**bold**");
        parts.body_html = Some("<div><strong>bold</strong></div>".into());
        parts.pixel_url = Some("https://p.passband.app/o/z.gif".into());
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        let (_h, mime) = parse_multipart(&s);
        let html_b = &mime[1].1;
        assert!(html_b.contains("<strong>bold</strong>"));
        assert!(html_b.contains("<img src=\"https://p.passband.app/o/z.gif\""));
        // The raw markdown source is untouched in the plain part.
        assert_eq!(mime[0].1, "**bold**");
    }

    #[test]
    fn generated_html_escapes_and_renders_paragraphs() {
        let mut parts = bare_parts("a <b> & \"c\"\nsecond line\n\nnew para");
        parts.pixel_url = Some("https://p.passband.app/o/z.gif".into());
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        let (_h, mime) = parse_multipart(&s);
        let html_b = &mime[1].1;
        assert!(html_b.contains("<p>a &lt;b&gt; &amp; &quot;c&quot;<br>second line</p>"));
        assert!(html_b.contains("<p>new para</p>"));
        assert!(!html_b.contains("<b>"));
        // The plain part keeps the characters as typed.
        assert!(mime[0].1.contains("a <b> & \"c\""));
    }

    #[test]
    fn pixel_url_is_attribute_escaped() {
        let mut parts = bare_parts("x");
        parts.pixel_url = Some("https://p.x/o/a?b=1&c=2\"><script>alert(1)</script>".into());
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        assert!(!s.contains("<script>"));
        assert!(s.contains("a?b=1&amp;c=2&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn boundary_never_appears_inside_a_part() {
        let mut parts = bare_parts("look: =_passband_alt_0 and =_passband_alt_1");
        parts.pixel_url = Some("https://p.passband.app/o/z.gif".into());
        let s = String::from_utf8(build_reply_rfc822(&parts).unwrap()).unwrap();
        assert!(s.contains("boundary=\"=_passband_alt_2\""));
        let (_h, mime) = parse_multipart(&s);
        assert_eq!(mime.len(), 2);
        for (_ph, pb) in &mime {
            assert!(!pb.contains("=_passband_alt_2"));
        }
    }

    #[test]
    fn multipart_boundary_dodges_content_collision() {
        // A body that types out boundary 0 forces boundary 1.
        assert_eq!(
            multipart_boundary(&["look: =_passband_alt_0", "<p>x</p>"]),
            "=_passband_alt_1"
        );
        assert_eq!(multipart_boundary(&["plain", "html"]), "=_passband_alt_0");
    }

    #[test]
    fn reply_rfc822_multipart_still_rejects_header_injection() {
        let parts = ReplyParts {
            to: "a@b.com".into(),
            cc: None,
            subject: "hi\r\nBcc: evil@x.com".into(),
            body: "x".into(),
            in_reply_to: None,
            references: None,
            body_html: Some("<p>x</p>".into()),
            pixel_url: None,
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
            cc: None,
            subject: "hi".into(),
            body: "x".into(),
            in_reply_to: None,
            references: None,
            body_html: None,
            pixel_url: None,
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
        assert!(
            req.contains("authorization: Bearer WRITE-TOKEN")
                || req.contains("Authorization: Bearer WRITE-TOKEN")
        );
        assert!(req.contains("\"removeLabelIds\":[\"INBOX\"]"));
        assert!(req.contains("\"addLabelIds\":[]"));
    }

    #[tokio::test]
    async fn send_posts_raw_and_threadid_and_returns_ids() {
        let (base, handle) = mock_once(200, "{\"id\":\"sent-1\",\"threadId\":\"thread-9\"}").await;
        let c = client(base);
        let raw = b"To: a@b.com\r\nSubject: hi\r\n\r\nbody";
        let sent = c.send(raw, Some("thread-9")).await.unwrap();
        assert_eq!(sent.id.as_deref(), Some("sent-1"));
        assert_eq!(sent.thread_id.as_deref(), Some("thread-9"));
        let req = handle.await.unwrap();
        assert!(req.contains("/messages/send"), "send path");
        assert!(req.contains("\"threadId\":\"thread-9\""));
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        assert!(req.contains(&expected), "raw base64url payload present");
    }

    #[tokio::test]
    async fn send_tolerates_an_idless_response() {
        // The send SUCCEEDED; an echo-less body must not become an error.
        let (base, handle) = mock_once(200, "{}").await;
        let c = client(base);
        let sent = c.send(b"To: a@b.com\r\n\r\nx", None).await.unwrap();
        handle.await.unwrap();
        assert!(sent.id.is_none());
        assert!(sent.thread_id.is_none());
    }

    #[tokio::test]
    async fn fetch_raw_gets_format_raw_with_the_write_token() {
        let (base, handle) = mock_once(
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"t9\",\"internalDate\":\"1783591200000\",\
             \"raw\":\"VG86IGFAYi5jb20\"}",
        )
        .await;
        let c = client(base);
        let got = c.fetch_raw("sent-1").await.unwrap();
        let req = handle.await.unwrap();
        assert!(req.starts_with("GET "), "raw fetch is a GET");
        assert!(req.contains("/messages/sent-1?format=raw"));
        assert!(
            req.contains("authorization: Bearer WRITE-TOKEN")
                || req.contains("Authorization: Bearer WRITE-TOKEN")
        );
        assert_eq!(got.raw_b64, "VG86IGFAYi5jb20");
        assert_eq!(got.thread_id.as_deref(), Some("t9"));
        assert_eq!(got.internal_date.as_deref(), Some("1783591200000"));
    }

    #[tokio::test]
    async fn parent_headers_asks_for_the_recipient_headers_and_joins_repeats() {
        let body = serde_json::json!({
            "id": "gmail-parent",
            "payload": { "headers": [
                { "name": "Message-ID", "value": "<parent@x>" },
                { "name": "From", "value": "Alice <alice@example.com>" },
                { "name": "Reply-To", "value": "list@example.com" },
                { "name": "To", "value": "me@example.com" },
                // Two Cc headers are one list, not the last one winning.
                { "name": "Cc", "value": "bob@example.com" },
                { "name": "Cc", "value": "carol@example.com" },
            ]}
        })
        .to_string();
        let (base, handle) = mock_once(200, Box::leak(body.into_boxed_str())).await;
        let c = client(base);
        let h = c.parent_headers("gmail-parent").await.unwrap();
        let req = handle.await.unwrap();
        assert!(req.starts_with("GET ") && req.contains("format=metadata"));
        for name in ["Message-ID", "References", "Reply-To", "From", "To", "Cc"] {
            assert!(
                req.contains(&format!("metadataHeaders={name}")),
                "asks for {name}"
            );
        }
        assert_eq!(h.message_id.as_deref(), Some("<parent@x>"));
        assert_eq!(h.reply_to.as_deref(), Some("list@example.com"));
        assert_eq!(h.from.as_deref(), Some("Alice <alice@example.com>"));
        assert_eq!(h.to.as_deref(), Some("me@example.com"));
        assert_eq!(h.cc.as_deref(), Some("bob@example.com, carol@example.com"));
        // ...and both Ccs survive into the derivation.
        let r = derive_reply_recipients(&h, "me@example.com", true);
        assert_eq!(r.to, "list@example.com");
        assert_eq!(r.cc, "bob@example.com, carol@example.com");
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
