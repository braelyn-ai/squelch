//! Installed-app (Desktop) OAuth 2.0 for Gmail: PKCE consent URL -> loopback
//! redirect on `127.0.0.1` -> code exchange, with `state` verified as a CSRF token.
//! TWO-DOOR: one token per call, never both scope sets — [`AuthScopes::Read`] mints
//! [`GMAIL_READONLY_SCOPE`], [`AuthScopes::Write`] mints [`WRITE_SCOPES`] for
//! human-door actions. The code, tokens, and client secret are never logged.
//!
//! [`run_broker_flow`] is the same flow with the redirect landing on a consent
//! broker (`docs/BROKER.md`) instead of a loopback listener, for hosts where no
//! browser can reach `127.0.0.1`. The two differ ONLY in where the code comes
//! from: same client, same PKCE, same exchange, and the verifier that makes a
//! code redeemable never leaves this process.

use crate::config::{GMAIL_READONLY_SCOPE, OAuthClientConfig, WRITE_SCOPES};
use crate::credentials::{CredentialKind, StoredToken};
use crate::error::{CoreError, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};
use url::Url;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Fixed loopback port used in `--headless` mode so it can be SSH-forwarded.
pub const DEFAULT_HEADLESS_PORT: u16 = 8847;

/// Entropy behind a broker session id and behind a claim token. 32 bytes is
/// exactly 43 unpadded base64url characters, which is the length the broker
/// validates: neither side ever decodes the id, so the length IS the shape.
const BROKER_ID_BYTES: usize = 32;

/// How often the daemon asks the broker whether consent landed. The whole budget
/// is spent waiting on one human reading one consent screen, so this is polite
/// rather than tight.
const BROKER_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long the daemon polls before giving up. Tracks the broker's session TTL
/// (`docs/BROKER.md`: ten minutes) — past it the session is gone and the only
/// recovery is a fresh `squelchd auth`.
const BROKER_POLL_TIMEOUT: Duration = Duration::from_secs(600);

/// Per-request budget for the two broker calls. Both are a small JSON round trip
/// to a service that, by design, has nothing slow to do.
const BROKER_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const BROKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on a broker-supplied string echoed into the terminal. The broker is a
/// host the user named, not a host anyone vetted.
const MAX_BROKER_DETAIL: usize = 200;

/// Which scope set (and therefore which credential kind) to authorize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScopes {
    /// `gmail.readonly` — the Read credential.
    Read,
    /// `gmail.modify` + `gmail.send` — the Write credential.
    Write,
}

impl AuthScopes {
    /// The OAuth scope strings for this set.
    pub fn scopes(self) -> Vec<String> {
        match self {
            AuthScopes::Read => vec![GMAIL_READONLY_SCOPE.to_string()],
            AuthScopes::Write => WRITE_SCOPES.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The credential kind this scope set maps to.
    pub fn kind(self) -> CredentialKind {
        match self {
            AuthScopes::Read => CredentialKind::Read,
            AuthScopes::Write => CredentialKind::Write,
        }
    }

    /// Human-readable label for prompts.
    pub fn label(self) -> &'static str {
        match self {
            AuthScopes::Read => "read-only Gmail (gmail.readonly)",
            AuthScopes::Write => "Gmail modify + send (gmail.modify, gmail.send)",
        }
    }
}

/// Options controlling the interactive flow.
#[derive(Debug, Clone)]
pub struct AuthFlowOptions {
    /// Which scope set to request.
    pub scopes: AuthScopes,
    /// Headless: don't auto-open a browser; bind a fixed, SSH-forwardable port.
    pub headless: bool,
    /// Fixed loopback port for headless mode; ignored (ephemeral) otherwise.
    pub port: u16,
}

impl Default for AuthFlowOptions {
    fn default() -> Self {
        Self {
            scopes: AuthScopes::Read,
            headless: false,
            port: DEFAULT_HEADLESS_PORT,
        }
    }
}

fn map_oauth_err<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> CoreError + '_ {
    move |e| CoreError::Credential(format!("{ctx}: {e}"))
}

/// The Google OAuth client with both endpoints set. Spelling the typestate out
/// once is what lets the loopback and broker flows share one constructor, one
/// consent-URL builder, and one exchange.
type GoogleClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// Build the Google client for one `redirect_uri`. Google checks the exchange's
/// `redirect_uri` against the one the consent URL carried, so the client is
/// built once per flow and used for both halves — the loopback and broker flows
/// differ only in the string that lands here.
fn google_client(client: &OAuthClientConfig, redirect_uri: &str) -> Result<GoogleClient> {
    Ok(BasicClient::new(ClientId::new(client.client_id.clone()))
        .set_client_secret(ClientSecret::new(client.client_secret.clone()))
        .set_auth_uri(
            AuthUrl::new(GOOGLE_AUTH_URL.to_string()).map_err(map_oauth_err("bad auth url"))?,
        )
        .set_token_uri(
            TokenUrl::new(GOOGLE_TOKEN_URL.to_string()).map_err(map_oauth_err("bad token url"))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_string())
                .map_err(map_oauth_err("bad redirect url"))?,
        ))
}

/// Build the consent URL: the scope set, a fresh PKCE challenge, and the two
/// extra params that make Google return a refresh token.
///
/// `state` is the caller's to choose — a random CSRF token on the loopback flow,
/// the session id on the broker flow, where `docs/BROKER.md` pins
/// `state == session_id` because that is how a callback finds its session.
fn authorize_url(
    oauth: &GoogleClient,
    scopes: AuthScopes,
    state: CsrfToken,
) -> (Url, CsrfToken, PkceCodeVerifier) {
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let mut req = oauth.authorize_url(move || state);
    for scope in scopes.scopes() {
        req = req.add_scope(Scope::new(scope));
    }
    let (url, csrf_token) = req
        // Ask for a refresh token and force consent so we reliably get one.
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(pkce_challenge)
        .url();
    (url, csrf_token, pkce_verifier)
}

/// Redeem an authorization code. The verifier proves this process is the one
/// that asked for the code, which is exactly why a parked code is inert to
/// anyone else who sees it.
fn exchange_code(
    oauth: &GoogleClient,
    code: String,
    pkce_verifier: PkceCodeVerifier,
) -> Result<StoredToken> {
    let http = oauth2::reqwest::blocking::ClientBuilder::new()
        // Never follow redirects: guards against SSRF on the token endpoint.
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .build()
        .map_err(map_oauth_err("building http client"))?;

    let token = oauth
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request(&http)
        .map_err(map_oauth_err("token exchange failed"))?;

    Ok(StoredToken::from_response(
        token.access_token().secret().to_string(),
        token.refresh_token().map(|r| r.secret().to_string()),
        token.expires_in(),
    ))
}

/// Run the interactive read-only flow with the defaults: ephemeral port, browser
/// auto-open.
pub fn run_installed_app_flow(client: &OAuthClientConfig) -> Result<StoredToken> {
    run_auth_flow(client, &AuthFlowOptions::default())
}

/// Run the interactive authorization flow and return the token. BLOCKS the calling
/// thread until the browser redirect arrives.
pub fn run_auth_flow(client: &OAuthClientConfig, opts: &AuthFlowOptions) -> Result<StoredToken> {
    // Headless -> a FIXED port so it can be SSH-forwarded; otherwise ephemeral.
    let bind_addr = if opts.headless {
        format!("127.0.0.1:{}", opts.port)
    } else {
        "127.0.0.1:0".to_string()
    };
    let listener = TcpListener::bind(&bind_addr).map_err(|e| {
        CoreError::Credential(format!("binding loopback listener on {bind_addr}: {e}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| CoreError::Credential(format!("reading listener addr: {e}")))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let oauth = google_client(client, &redirect_uri)?;
    let (auth_url, csrf_token, pkce_verifier) =
        authorize_url(&oauth, opts.scopes, CsrfToken::new_random());

    if opts.headless {
        println!(
            "\n=== squelch headless authorization ({}) ===",
            opts.scopes.label()
        );
        println!(
            "\nThis box is headless. Forward the loopback port to your laptop, e.g.:\n\
             \n    ssh -L {port}:127.0.0.1:{port} <this-host>\n\
             \nthen open this URL in your LOCAL browser:\n"
        );
        println!("{auth_url}\n");
        println!("Waiting for the OAuth redirect on {redirect_uri} (via your tunnel) ...");
    } else {
        println!(
            "\nOpen this URL in your browser to authorize squelch ({}):\n",
            opts.scopes.label()
        );
        println!("{auth_url}\n");
        if webbrowser::open(auth_url.as_str()).is_err() {
            println!("(could not auto-open a browser; copy the URL above manually)");
        }
        println!("Waiting for the OAuth redirect on {redirect_uri} ...");
    }

    let code = wait_for_code(&listener, csrf_token.secret())?;

    exchange_code(&oauth, code, pkce_verifier)
}

/// Run the consent flow through a broker (`docs/BROKER.md`) instead of a
/// loopback listener, and return the token. BLOCKS until the user finishes
/// consent on whatever device they opened the link on, or the session expires.
///
/// The broker never becomes able to mint anything. It sees a session id, the
/// SHA-256 of a claim token, and a consent URL; it parks a code that is inert
/// without the PKCE verifier built here, which goes to Google and nowhere else.
pub fn run_broker_flow(
    client: &OAuthClientConfig,
    broker_url: &str,
    scopes: AuthScopes,
) -> Result<StoredToken> {
    let base = broker_base(broker_url)?;
    let redirect_uri = format!("{base}/callback");

    // Two independent secrets: the session id ADDRESSES the session and is
    // public by design (it is in the printed link and in `state`), while the
    // claim token is what proves we are the daemon that opened it. Deriving one
    // from the other would publish the second.
    let session_id = broker_random_id()?;
    let claim_token = broker_random_id()?;

    let oauth = google_client(client, &redirect_uri)?;
    // `state` is the session id by contract, not a CSRF token of our choosing:
    // it is how the broker's callback finds the session Google is answering.
    // It is still 32 unguessable bytes minted for this one flow, so it carries
    // everything a random `CsrfToken` would have.
    let (auth_url, _state, pkce_verifier) =
        authorize_url(&oauth, scopes, CsrfToken::new(session_id.clone()));

    let http = broker_http()?;
    register_session(&http, &base, &session_id, &claim_token, auth_url.as_str())?;

    let minutes = BROKER_POLL_TIMEOUT.as_secs() / 60;
    println!(
        "\n=== squelch broker authorization ({}) ===",
        scopes.label()
    );
    println!("\nOpen this link on any device with a browser and approve the consent screen:\n");
    println!("{base}/link?s={session_id}\n");
    println!("The link is good for {minutes} minutes and works once.");
    println!("Waiting for consent (the code is collected here, never by the broker) ...");

    let code = poll_for_code(&http, &base, &session_id, &claim_token)?;
    exchange_code(&oauth, code, pkce_verifier)
}

/// Canonicalize `--broker` the same way the broker canonicalizes its own public
/// URL: parser-normalized, no trailing slash. The `redirect_uri` in the consent
/// URL is compared against that value BYTE FOR BYTE, so a host that differs only
/// in case or an explicit `:443` has to normalize to the same string here or
/// every registration is a 400.
///
/// Plain HTTP is refused off-loopback: the claim token travels in a POST body to
/// this origin, and an observer who lifts it can redeem the parked code before
/// the daemon does. On loopback "the network" is this machine, and dev runs of
/// the broker need it.
fn broker_base(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let url = Url::parse(raw).map_err(|e| {
        CoreError::Credential(format!(
            "--broker must be an absolute URL like https://auth.passband.email (`{raw}`: {e})"
        ))
    })?;

    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => {
            return Err(CoreError::Credential(format!(
                "--broker URL has no host (`{raw}`)"
            )));
        }
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(CoreError::Credential(format!(
                "--broker must use https off loopback (`{raw}`): the claim token would cross \
                 the network in the clear"
            )));
        }
        other => {
            return Err(CoreError::Credential(format!(
                "--broker must be an http(s) URL (`{raw}` uses `{other}`)"
            )));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CoreError::Credential(
            "--broker URL must not carry userinfo".to_string(),
        ));
    }
    // The broker serves `/link`, `/callback`, and `/v1/*` at its root and
    // refuses to boot on a public URL with a path, so a prefix here would only
    // build addresses nobody serves.
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(CoreError::Credential(format!(
            "--broker must be a bare origin with no path, query, or fragment (`{raw}`)"
        )));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// The daemon's HTTP client for the broker. REDIRECTS ARE REFUSED: the claim
/// body carries the claim token, and a `307` would walk that POST to a host of
/// the broker's choosing.
fn broker_http() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(BROKER_HTTP_TIMEOUT)
        .connect_timeout(BROKER_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(map_oauth_err("building broker http client"))
}

/// 32 bytes of OS entropy as unpadded base64url — the shape `docs/BROKER.md`
/// specifies for a session id and for a claim token. `Err` only when the OS
/// refuses entropy, which must fail the flow rather than fall back to something
/// guessable.
fn broker_random_id() -> Result<String> {
    let mut bytes = [0u8; BROKER_ID_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|_| CoreError::Credential("no OS entropy for a broker session".to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Lowercase hex SHA-256: the one spelling the broker's `claim_token_hash`
/// accepts.
fn hex_sha256(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Register the session. Only the HASH of the claim token goes over the wire:
/// the token itself is presented once, when the code is collected.
fn register_session(
    http: &reqwest::blocking::Client,
    base: &str,
    session_id: &str,
    claim_token: &str,
    auth_url: &str,
) -> Result<()> {
    let resp = http
        .post(format!("{base}/v1/sessions"))
        .json(&serde_json::json!({
            "session_id": session_id,
            "claim_token_hash": hex_sha256(claim_token),
            "auth_url": auth_url,
        }))
        .send()
        .map_err(|e| {
            CoreError::Credential(format!("could not reach the consent broker at {base}: {e}"))
        })?;

    let status = resp.status().as_u16();
    // Read only for the refusal reason. A 201 body carries `expires_in`, which
    // this side does not need: the poll deadline is the contract's TTL.
    let detail = resp.text().ok().and_then(|b| broker_detail(&b));
    let because = detail.map(|d| format!(" ({d})")).unwrap_or_default();

    match status {
        201 => Ok(()),
        // Session ids are 32 random bytes, so this is not a collision: the id is
        // live at the broker already and a fresh run gets a fresh one.
        409 => Err(CoreError::Credential(
            "the consent broker already has a session under this id; re-run `squelchd auth` to \
             retry with a new one"
                .to_string(),
        )),
        // Every field this side sends is generated here, so a refusal means the
        // two ends disagree about the shape, not that the user typed something.
        400 => Err(CoreError::Credential(format!(
            "the consent broker refused the registration{because}; this daemon and {base} \
             disagree about the v1 contract (docs/BROKER.md), so update whichever is older"
        ))),
        429 => Err(CoreError::Credential(format!(
            "the consent broker is rate limiting this host{because}; wait a minute and re-run \
             `squelchd auth`"
        ))),
        503 => Err(CoreError::Credential(format!(
            "the consent broker has no room for another session{because}; wait a minute and \
             re-run `squelchd auth`"
        ))),
        other => Err(CoreError::Credential(format!(
            "the consent broker answered {other} to the registration{because}"
        ))),
    }
}

/// One resolved poll of `/v1/claim`.
///
/// No `Debug`: `Complete` holds the authorization code, and a derived one is how
/// a code ends up in the first error someone formats with `{:?}`.
enum ClaimStep {
    /// Consent has not landed yet, or the broker briefly would not answer —
    /// which reads the same from here. The refusing status is carried so that a
    /// flow which runs out of time against a broken broker can say which.
    Pending(Option<u16>),
    Complete(String),
}

/// The `/v1/claim` body. No `Debug`, for the reason above.
#[derive(Deserialize)]
struct ClaimResponse {
    status: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

/// Poll until the broker has an outcome or the session's life runs out.
///
/// A dropped connection mid-flight is NOT fatal: the consent screen is already
/// in front of the user, registration proved the host answers, and throwing the
/// session away over one refused packet costs them the whole screen. The
/// deadline, not a single failure, ends the flow.
fn poll_for_code(
    http: &reqwest::blocking::Client,
    base: &str,
    session_id: &str,
    claim_token: &str,
) -> Result<String> {
    let claim_url = format!("{base}/v1/claim");
    let body = serde_json::json!({ "session_id": session_id, "claim_token": claim_token });
    let deadline = Instant::now() + BROKER_POLL_TIMEOUT;
    let mut setback: Option<String> = None;

    while Instant::now() + BROKER_POLL_INTERVAL < deadline {
        match http.post(&claim_url).json(&body).send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let payload = resp.bytes().unwrap_or_default();
                match interpret_claim(status, &payload)? {
                    ClaimStep::Complete(code) => return Ok(code),
                    ClaimStep::Pending(refusal) => {
                        setback = refusal.map(|status| format!("HTTP {status}"))
                    }
                }
            }
            // Kept, not printed: if this is why the flow runs out of time, the
            // timeout is the place to say so.
            Err(e) => setback = Some(e.to_string()),
        }
        std::thread::sleep(BROKER_POLL_INTERVAL);
    }

    let trailer = setback
        .map(|e| format!(" (the last poll failed with: {e})"))
        .unwrap_or_default();
    let minutes = BROKER_POLL_TIMEOUT.as_secs() / 60;
    Err(CoreError::Credential(format!(
        "no consent arrived within {minutes} minutes, so the link has expired{trailer}; \
         re-run `squelchd auth`"
    )))
}

/// Interpret one claim response. Pure, and separate from the loop, so every
/// branch of the contract is exercised without a broker on the wire.
fn interpret_claim(status: u16, body: &[u8]) -> Result<ClaimStep> {
    match status {
        200 => {}
        // The token we presented does not hash to the one registered under this
        // session id. Against an honest broker holding OUR session that cannot
        // happen, so it is a bug on one side or somebody else answering.
        403 => {
            return Err(CoreError::Credential(
                "the consent broker rejected this daemon's claim token; the session is not ours. \
                 Re-run `squelchd auth`."
                    .to_string(),
            ));
        }
        404 => {
            return Err(CoreError::Credential(
                "the consent link expired or was already used; re-run `squelchd auth`".to_string(),
            ));
        }
        // Throttled, or a proxy having a bad minute. Indistinguishable from an
        // unfinished consent from here, and abandoning a session the user is
        // halfway through costs them the whole screen — the deadline, not one
        // bad answer, ends the flow.
        429 | 500..=599 => return Ok(ClaimStep::Pending(Some(status))),
        other => {
            return Err(CoreError::Credential(format!(
                "the consent broker answered {other} to a claim"
            )));
        }
    }

    // serde's own `Display` embeds the offending value, which for this body
    // could be the authorization code. Only the error class and position.
    let resp: ClaimResponse = serde_json::from_slice(body).map_err(|e| {
        CoreError::Credential(format!(
            "the consent broker sent an unreadable claim response: {:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;

    match resp.status.as_deref() {
        Some("pending") => Ok(ClaimStep::Pending(None)),
        Some("complete") => resp
            .code
            .filter(|c| !c.is_empty())
            .map(ClaimStep::Complete)
            .ok_or_else(|| {
                CoreError::Credential(
                    "the consent broker reported a completed consent with no code".to_string(),
                )
            }),
        Some("denied") => {
            let why = resp.error.as_deref().unwrap_or("access_denied");
            Err(CoreError::Credential(format!(
                "consent was refused at the Google screen ({}); nothing was authorized. Re-run \
                 `squelchd auth` to try again.",
                printable(why)
            )))
        }
        // `unknown` arrives as the 404 above; anything else is contract skew.
        _ => Err(CoreError::Credential(
            "the consent broker sent a claim status this daemon does not understand; the two \
             disagree about the v1 contract (docs/BROKER.md)"
                .to_string(),
        )),
    }
}

/// The `error` string out of a broker response, if it carried one.
fn broker_detail(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = printable(value.get("error")?.as_str()?);
    (!message.trim().is_empty()).then_some(message)
}

/// A broker-supplied string, made safe to print. It lands in a terminal, and the
/// broker is a host the user named rather than one anyone vetted: control
/// characters would be an escape sequence, not text, and the length is theirs to
/// choose unless it is bounded here.
fn printable(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_BROKER_DETAIL)
        .collect()
}

/// Block on the loopback listener for a single redirect request, validate the
/// CSRF `state`, and return the authorization `code`.
fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    // Loop so stray pokes (favicon probes) don't consume the one-shot listener.
    loop {
        let (mut stream, _) = listener
            .accept()
            .map_err(|e| CoreError::Credential(format!("accept failed: {e}")))?;

        let mut buf = [0u8; 4096];
        let n = stream
            .read(&mut buf)
            .map_err(|e| CoreError::Credential(format!("reading request: {e}")))?;
        let request = String::from_utf8_lossy(&buf[..n]);

        // First line: "GET /?code=...&state=... HTTP/1.1"
        let path = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");

        let (code, state) = parse_redirect_query(path);

        // No OAuth params at all (favicon, empty probe): answer and keep waiting.
        if code.is_none() && state.is_none() {
            write_http(&mut stream, "204 No Content", "");
            continue;
        }

        let (status, body): (&str, &str) = match (code.as_deref(), state.as_deref()) {
            (Some(code_val), Some(state_val))
                if constant_time_eq(state_val, expected_state) && !code_val.is_empty() =>
            {
                let ok = "squelch is authorized. You can close this tab and return to the terminal.";
                write_http(&mut stream, "200 OK", ok);
                return Ok(code_val.to_string());
            }
            (_, Some(state_val)) if !constant_time_eq(state_val, expected_state) => {
                ("400 Bad Request", "state mismatch (possible CSRF); aborting")
            }
            _ => ("400 Bad Request", "missing authorization code"),
        };
        write_http(&mut stream, status, body);
        return Err(CoreError::Credential(body.to_string()));
    }
}

fn write_http(stream: &mut impl Write, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Extract `code` and `state` from a redirect path like `/?code=..&state=..`.
fn parse_redirect_query(path: &str) -> (Option<String>, Option<String>) {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let v = url_decode(v);
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                _ => {}
            }
        }
    }
    (code, state)
}

/// Minimal percent-decoder (also '+' -> space); enough for OAuth redirect params.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Length-checked, branch-minimal string comparison for the CSRF token.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_and_state() {
        let (code, state) = parse_redirect_query("/?code=abc123&state=xyz&scope=foo");
        assert_eq!(code.as_deref(), Some("abc123"));
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a%2Fb+c"), "a/b c");
        assert_eq!(url_decode("plain"), "plain");
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("token", "token"));
        assert!(!constant_time_eq("token", "toke"));
        assert!(!constant_time_eq("token", "tokel"));
    }

    // --- broker flow -------------------------------------------------------
    //
    // Everything below is offline. The one piece that must talk to Google (the
    // exchange) is shared with the loopback flow and is not mocked here.

    const BROKER: &str = "https://auth.passband.email";

    fn test_client() -> OAuthClientConfig {
        OAuthClientConfig {
            client_id: "1234.apps.googleusercontent.com".to_string(),
            client_secret: "not-a-real-secret".to_string(),
        }
    }

    /// The broker validates the id by LENGTH, so the encoding has to land on 43
    /// characters exactly and stay inside the base64url alphabet.
    #[test]
    fn broker_ids_are_43_base64url_chars_and_never_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let id = broker_random_id().expect("OS entropy");
            assert_eq!(
                id.len(),
                43,
                "32 bytes is exactly 43 unpadded base64url chars"
            );
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "id must survive a query string unescaped: {id}"
            );
            assert!(seen.insert(id), "minted the same id twice");
        }
    }

    #[test]
    fn hashes_the_claim_token_as_lowercase_hex() {
        assert_eq!(
            hex_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hex_sha256(&broker_random_id().unwrap()).len(), 64);
    }

    /// The broker compares `redirect_uri` against its own canonical public URL
    /// byte for byte, so this side has to normalize the same way it does.
    #[test]
    fn broker_base_canonicalizes_the_origin() {
        assert_eq!(broker_base(BROKER).unwrap(), BROKER);
        assert_eq!(broker_base("https://auth.passband.email/").unwrap(), BROKER);
        assert_eq!(
            broker_base("  https://auth.passband.email  ").unwrap(),
            BROKER
        );
        assert_eq!(broker_base("https://AUTH.Passband.Email").unwrap(), BROKER);
        assert_eq!(
            broker_base("https://auth.passband.email:443").unwrap(),
            BROKER
        );
        // A dev broker on loopback keeps its port.
        assert_eq!(
            broker_base("http://127.0.0.1:8851/").unwrap(),
            "http://127.0.0.1:8851"
        );
        assert_eq!(
            broker_base("http://localhost:8851").unwrap(),
            "http://localhost:8851"
        );
    }

    #[test]
    fn broker_base_refuses_anything_that_would_leak_or_misaddress() {
        // Plaintext off loopback would hand an observer the claim token.
        assert!(broker_base("http://auth.passband.email").is_err());
        assert!(broker_base("ftp://auth.passband.email").is_err());
        assert!(broker_base("auth.passband.email").is_err());
        assert!(broker_base("https://user:pw@auth.passband.email").is_err());
        // The broker serves its routes at the root; a prefix addresses nothing.
        assert!(broker_base("https://auth.passband.email/broker").is_err());
        assert!(broker_base("https://auth.passband.email/?x=1").is_err());
        assert!(broker_base("https://auth.passband.email/#f").is_err());
        assert!(broker_base("").is_err());
    }

    /// The consent URL is the one field the broker validates strictly: every
    /// check in `squelch-broker::validate` is asserted here from this side.
    #[test]
    fn the_broker_consent_url_matches_what_the_broker_validates() {
        let session_id = broker_random_id().unwrap();
        let redirect_uri = format!("{BROKER}/callback");
        let oauth = google_client(&test_client(), &redirect_uri).unwrap();
        let (url, _state, _verifier) = authorize_url(
            &oauth,
            AuthScopes::Write,
            CsrfToken::new(session_id.clone()),
        );

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(url.path(), "/o/oauth2/v2/auth");
        assert_eq!(url.fragment(), None);

        let params: std::collections::HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        // state IS the session id: the callback finds its session by it.
        assert_eq!(params.get("state"), Some(&session_id));
        assert_eq!(params.get("redirect_uri"), Some(&redirect_uri));
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256"),
            "`plain` would make the parked code redeemable by whoever holds it"
        );
        assert!(!params.get("code_challenge").unwrap().is_empty());
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("1234.apps.googleusercontent.com")
        );
        // The refresh token depends on both of these.
        assert_eq!(
            params.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(params.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(WRITE_SCOPES.join(" ").as_str())
        );
        // The client secret has no business in a URL a user will read.
        assert!(!url.as_str().contains("not-a-real-secret"));
    }

    /// The loopback flow keeps a random `state`: it is a CSRF token there, not
    /// an address, and sharing the builder must not have changed that.
    #[test]
    fn the_loopback_consent_url_still_carries_a_random_state() {
        let oauth = google_client(&test_client(), "http://127.0.0.1:8847").unwrap();
        let mut states = std::collections::HashSet::new();
        for _ in 0..8 {
            let (url, csrf, _) = authorize_url(&oauth, AuthScopes::Read, CsrfToken::new_random());
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            assert_eq!(
                &state,
                csrf.secret(),
                "the returned token addresses the URL"
            );
            assert!(states.insert(state), "state repeated across flows");
        }
    }

    fn claim(status: u16, body: &str) -> Result<ClaimStep> {
        interpret_claim(status, body.as_bytes())
    }

    /// The message a refused claim ends the flow with. `ClaimStep` has no
    /// `Debug` by design, which is also why `unwrap_err` is unavailable here.
    fn claim_err(status: u16, body: &str) -> String {
        match claim(status, body) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("{status} {body} should have ended the flow"),
        }
    }

    #[test]
    fn a_pending_claim_keeps_polling() {
        assert!(matches!(
            claim(200, r#"{"status":"pending"}"#).unwrap(),
            ClaimStep::Pending(None)
        ));
        // Throttled or momentarily gone reads the same from here, and the
        // status rides along so a timeout can name it.
        for status in [429, 502, 503, 504] {
            assert!(
                matches!(claim(status, "").unwrap(), ClaimStep::Pending(Some(s)) if s == status),
                "{status} should be waited out, not fatal"
            );
        }
    }

    #[test]
    fn a_complete_claim_yields_the_code() {
        let step = claim(200, r#"{"status":"complete","code":"4/0Aeanabc-123"}"#).unwrap();
        match step {
            ClaimStep::Complete(code) => assert_eq!(code, "4/0Aeanabc-123"),
            ClaimStep::Pending(_) => panic!("a complete claim must yield the code"),
        }
        // A `complete` with nothing to exchange is a broken broker, not a code.
        assert!(claim(200, r#"{"status":"complete"}"#).is_err());
        assert!(claim(200, r#"{"status":"complete","code":""}"#).is_err());
    }

    #[test]
    fn a_denied_claim_says_the_user_refused() {
        let msg = claim_err(200, r#"{"status":"denied","error":"access_denied"}"#);
        assert!(msg.contains("refused"), "{msg}");
        assert!(msg.contains("access_denied"), "{msg}");
        assert!(msg.contains("nothing was authorized"), "{msg}");

        // The error string comes from a host nobody vetted: it reaches a
        // terminal as text or not at all.
        let msg = claim_err(200, "{\"status\":\"denied\",\"error\":\"a\\u001b[2J\\nb\"}");
        assert!(!msg.contains('\u{1b}'), "escape survived: {msg}");
        assert!(!msg.contains('\n'), "newline survived: {msg}");
        // Disarmed, not dropped: what is left is text.
        assert!(msg.contains("a[2Jb"), "{msg}");
    }

    #[test]
    fn a_forbidden_claim_is_a_hard_error() {
        let msg = claim_err(403, r#"{"error":"claim token mismatch"}"#);
        assert!(msg.contains("claim token"), "{msg}");
        assert!(msg.contains("not ours"), "{msg}");
    }

    #[test]
    fn an_unknown_session_tells_the_user_to_re_run() {
        let msg = claim_err(404, r#"{"status":"unknown"}"#);
        assert!(msg.contains("expired or was already used"), "{msg}");
        assert!(msg.contains("squelchd auth"), "{msg}");
    }

    #[test]
    fn an_off_contract_claim_response_is_refused_rather_than_guessed() {
        assert!(claim(200, r#"{"status":"maybe"}"#).is_err());
        assert!(claim(200, "{}").is_err());
        assert!(claim(200, "not json").is_err());
        // A 4xx that is not one of the contract's own is not a wait.
        assert!(claim(418, "").is_err());
        assert!(claim(401, "").is_err());
    }

    /// Nothing parsed out of a claim body may reach an error message. This is
    /// the one place a code could be echoed by accident.
    #[test]
    fn no_claim_error_ever_echoes_the_code() {
        let code = "4/0Aeansecretcode";
        for body in [
            format!(r#"{{"status":"maybe","code":"{code}"}}"#),
            format!(r#"{{"status":"complete","code":{{"nested":"{code}"}}}}"#),
            format!(r#"{{"code":"{code}""#),
        ] {
            let msg = claim_err(200, &body);
            assert!(!msg.contains(code), "the code leaked into: {msg}");
        }
    }

    #[test]
    fn broker_detail_extracts_a_printable_reason_or_nothing() {
        assert_eq!(
            broker_detail(r#"{"error":"auth_url must use https"}"#).as_deref(),
            Some("auth_url must use https")
        );
        assert_eq!(broker_detail(r#"{"status":"pending"}"#), None);
        assert_eq!(broker_detail("<html>502</html>"), None);
        assert_eq!(broker_detail(r#"{"error":"   "}"#), None);
        assert_eq!(
            broker_detail(&format!(r#"{{"error":"{}"}}"#, "x".repeat(5000)))
                .unwrap()
                .len(),
            MAX_BROKER_DETAIL
        );
    }
}
