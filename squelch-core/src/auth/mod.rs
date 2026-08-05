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
//!
//! Both flows end in the same GUARDED exchange: a code becomes a stored token
//! only after Google confirms the grant covers the scopes that were asked for
//! and names the configured account as the mailbox behind it. Neither a loopback
//! listener nor a broker can tell whose Google session finished the screen.
//!
//! [`run_export_flow`] is the loopback flow run on a machine that has NO daemon
//! config: it discovers the mailbox instead of verifying one, and hands back a
//! token for [`transfer`] to carry to a headless host. See `docs/BROKER.md`.
//!
//! A blob arriving at the other end goes through [`verify_transfer_credential`],
//! which asks Google the same two questions the exchange asks. Nothing in a blob
//! is signed, so nothing a blob SAYS about itself is evidence.

mod transfer;
pub use transfer::{
    CredentialTransfer, TRANSFER_PREFIX, TRANSFER_VERSION, TransferCredential, decode_transfer,
    encode_transfer,
};

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

/// The one Gmail call that names the mailbox behind an access token. Every scope
/// set this module mints (`gmail.readonly`, `gmail.modify`) permits it.
const GMAIL_PROFILE_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/profile";

/// Budget for the token exchange and the profile check that follows it. Both are
/// one small round trip to Google. Shared with the refresh in `credentials`, so
/// that every path to a stored token is bounded by the same number.
pub(crate) const EXCHANGE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Ceiling on an untrusted string echoed into the terminal: a broker's error
/// detail, or the account a pasted credential blob claims for itself. Neither
/// comes from anywhere anybody vetted.
const MAX_UNTRUSTED_DETAIL: usize = 200;

/// How long a one-shot consent listener waits for the browser redirect before
/// giving up. Matches the broker's session TTL: the whole budget is spent on one
/// human reading one consent screen.
const CONSENT_WAIT_TIMEOUT: Duration = Duration::from_secs(600);

/// Per-connection read budget on that listener. The redirect is one small GET
/// that is already on the wire when the connection opens, so anything slower is
/// not the browser we are waiting for.
const CONSENT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the accept loop wakes to re-check its deadline.
const CONSENT_ACCEPT_POLL: Duration = Duration::from_millis(200);

/// Ceiling on any response body this module reads. Every answer in the broker
/// contract and Gmail's profile answer are a few hundred bytes of JSON, so this
/// is slack rather than a budget. It exists because `squelchd auth` runs on the
/// NAS and Pi hosts the broker exists for, where an unbounded body streamed by a
/// hostile or wedged host is the whole machine.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

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

/// The scope set a slot REQUIRES. The inverse of [`AuthScopes::kind`], and the
/// reason a token can be held against the slot a credential blob claims for it.
impl From<CredentialKind> for AuthScopes {
    fn from(kind: CredentialKind) -> Self {
        match kind {
            CredentialKind::Read => AuthScopes::Read,
            CredentialKind::Write => AuthScopes::Write,
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

/// Redeem an authorization code and prove the resulting token is the one that
/// was asked for. The verifier proves this process is the one that asked for the
/// code, which is exactly why a parked code is inert to anyone else who sees it.
///
/// The two checks after the exchange are the reason this is the ONLY path to a
/// stored token. A code says nothing about who approved it: whichever Google
/// session happened to be signed in on the browser is the one that consented,
/// and neither a loopback listener nor a broker can see that. So the grant is
/// held against what was requested, and the token is held against the configured
/// account, before anything is persisted.
///
/// `expected_account` is `None` only on the export flow, which runs on a machine
/// with no daemon config and therefore no account to compare against. The
/// mailbox is DISCOVERED either way and returned alongside the token: `None`
/// skips the comparison, it never skips asking Google who this is.
fn exchange_code(
    oauth: &GoogleClient,
    expected_account: Option<&str>,
    scopes: AuthScopes,
    code: String,
    pkce_verifier: PkceCodeVerifier,
) -> Result<(String, StoredToken)> {
    let http = guarded_http(EXCHANGE_HTTP_TIMEOUT)?;

    let token = oauth
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request(&http)
        .map_err(map_oauth_err("token exchange failed"))?;

    let granted: Option<Vec<String>> = token
        .scopes()
        .map(|granted| granted.iter().map(|s| s.to_string()).collect());
    check_scope_grant(scopes, granted.as_deref())?;

    let mailbox = fetch_profile_email(&http, GMAIL_PROFILE_URL, token.access_token().secret())?;
    if let Some(expected) = expected_account {
        check_account_match(expected, &mailbox)?;
    }

    Ok((
        mailbox,
        StoredToken::from_response(
            token.access_token().secret().to_string(),
            token.refresh_token().map(|r| r.secret().to_string()),
            token.expires_in(),
        ),
    ))
}

/// The client for a guarded exchange and for a transfer verification. Shared so
/// that both paths to a stored token carry the same two properties: redirects
/// are REFUSED (an open redirect on the token endpoint is SSRF, and the request
/// carries the client secret), and one round trip has a bounded life.
fn guarded_http(timeout: Duration) -> Result<reqwest::blocking::Client> {
    oauth2::reqwest::blocking::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(map_oauth_err("building http client"))
}

/// Prove one credential out of a pasted blob before ANY of it is stored: that
/// this host's OAuth client minted it, that Google says it opens the configured
/// mailbox, and that its grant covers what the slot it claims actually needs.
///
/// A blob is base64url JSON with no signature over it, so its `account` and
/// `kind` fields are the paster's claims and nothing more. Every other path to a
/// stored token runs the exchange, which holds the token against the configured
/// account precisely because "the exchange response names a token, never an
/// account"; import has to ask the same question of the same authority. Without
/// this, a hand-edited blob files an attacker's refresh token under the user's
/// address (the sync loop then ingests a stranger's mailbox and serves it to
/// `/mcp` as the user's own, and mail composed in the client sends from it), and
/// a hand-edited `kind` files a modify+send token in the Read slot, which is the
/// whole two-door split gone.
///
/// The refresh is what proves the client: a refresh token only works for the
/// `client_id`/`client_secret` that minted it, so a blob from a different OAuth
/// client fails HERE, at paste time, instead of as a mystery `invalid_client` on
/// the first sync an hour later.
///
/// Costs one refresh and one profile call per credential. Callers run the cheap
/// local checks first.
pub fn verify_transfer_credential(
    client: &OAuthClientConfig,
    expected_account: &str,
    kind: CredentialKind,
    token: &StoredToken,
) -> Result<()> {
    verify_transfer_credential_at(
        client,
        expected_account,
        kind,
        token,
        GOOGLE_TOKEN_URL,
        GMAIL_PROFILE_URL,
        EXCHANGE_HTTP_TIMEOUT,
    )
}

/// The verification against explicit endpoints and budget. The two URLs are
/// parameters only so every refusal above can be exercised end to end against a
/// scripted socket; the daemon reaches this through the pinned pair.
pub(crate) fn verify_transfer_credential_at(
    client: &OAuthClientConfig,
    expected_account: &str,
    kind: CredentialKind,
    token: &StoredToken,
    token_url: &str,
    profile_url: &str,
    timeout: Duration,
) -> Result<()> {
    let refresh = token.refresh_token.as_deref().ok_or_else(|| {
        CoreError::Credential(format!(
            "the {kind:?} credential in this blob has no refresh token, so nothing about it can \
             be verified and it would stop working within the hour; nothing was stored."
        ))
    })?;

    let refreshed =
        crate::credentials::refresh_stored_token_detailed_at(client, refresh, token_url, timeout)
            .map_err(|e| transfer_client_error(kind, &e))?;
    let http = guarded_http(timeout)?;
    let mailbox = fetch_profile_email(&http, profile_url, &refreshed.token.access_token)?;

    judge_transfer_credential(
        expected_account,
        kind,
        &mailbox,
        refreshed.granted_scopes.as_deref(),
    )
}

/// What Google's answers mean for one blob entry. Pure, and separate from the
/// two calls that collect them, so every refusal is exercised without a network.
fn judge_transfer_credential(
    expected_account: &str,
    kind: CredentialKind,
    mailbox: &str,
    granted: Option<&[String]>,
) -> Result<()> {
    check_account_match(expected_account, mailbox).map_err(|_| {
        CoreError::Credential(format!(
            "Google says the {kind:?} credential in this blob opens {}, but this daemon is \
             configured for {expected_account}; nothing was stored. The account named inside a \
             blob is not evidence, which is why Google was asked. Export again while signed in \
             as {expected_account}, or fix account_email in the config.",
            printable(mailbox)
        ))
    })?;

    // An absent `scope` means "same as the request" on an exchange (RFC 6749
    // §5.1), and there IS no request to compare against on a refresh. Which slot
    // this token belongs in would be the blob's word alone, so it fails closed.
    let Some(granted) = granted else {
        return Err(CoreError::Credential(format!(
            "Google named no scopes when refreshing the {kind:?} credential in this blob, so \
             there is no proof it belongs in the {kind:?} slot rather than the other one; \
             nothing was stored. Authorize this host directly with `squelchd auth` instead."
        )));
    };
    check_scope_grant(AuthScopes::from(kind), Some(granted)).map_err(|e| {
        CoreError::Credential(format!(
            "the {kind:?} credential in this blob does not carry the scopes the {kind:?} slot \
             needs, so storing it there would put a token with the wrong reach behind that \
             door: {e}"
        ))
    })
}

/// The message for a refresh that failed during verification. `invalid_client`
/// is the ONE outcome worth naming: it means the blob was minted by a different
/// OAuth client, which is the single most common way an import goes wrong.
fn transfer_client_error(kind: CredentialKind, err: &CoreError) -> CoreError {
    let text = err.to_string();
    if text.contains("invalid_client") || text.contains("unauthorized_client") {
        return CoreError::Credential(format!(
            "Google refused the {kind:?} credential in this blob for THIS host's OAuth client, \
             so it was minted by a different one; nothing was stored. Export again on a machine \
             using the same SQUELCH_CLIENT_ID and SQUELCH_CLIENT_SECRET as this host."
        ));
    }
    CoreError::Credential(format!(
        "the {kind:?} credential in this blob could not be checked with Google, so nothing was \
         stored: {text}"
    ))
}

/// Fail unless the grant covers every scope that was requested.
///
/// A partial consent is a hard error rather than a warning because it is
/// invisible afterwards: a Write credential whose grant dropped `gmail.send`
/// stores, loads, and refreshes exactly like a working one, and only fails at
/// the first send, long after the consent screen is gone.
///
/// An ABSENT `scope` on the response means "identical to the request" (RFC 6749
/// §5.1). That is taken at face value because the response comes off Google's
/// pinned token endpoint over TLS with redirects refused, not off anything a
/// third party can shape.
fn check_scope_grant(requested: AuthScopes, granted: Option<&[String]>) -> Result<()> {
    let Some(granted) = granted else {
        return Ok(());
    };
    let missing: Vec<String> = requested
        .scopes()
        .into_iter()
        .filter(|want| !granted.iter().any(|got| got == want))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(CoreError::Credential(format!(
        "the consent screen granted [{}], which does not cover [{}]; nothing was stored. \
         Re-run `squelchd auth` and leave every permission on the Google screen checked.",
        granted.join(", "),
        missing.join(", ")
    )))
}

/// Fail unless Google says the token opens the mailbox this daemon is configured
/// for.
///
/// Without this, a consent finished on a DIFFERENT Google account still stores a
/// token under `account_email`, and the sync loop then ingests a stranger's mail
/// into the user's database and serves it to their own agent surfaces as their
/// own. Case-insensitive and trimmed: Gmail addresses are ASCII and the config
/// value is hand-typed.
fn check_account_match(configured: &str, granted: &str) -> Result<()> {
    if configured.trim().eq_ignore_ascii_case(granted.trim()) {
        return Ok(());
    }
    Err(CoreError::Credential(format!(
        "this consent authorized {granted}, but squelch is configured for {configured}; \
         nothing was stored. Approve the screen while signed in as {configured} (or fix \
         `account_email` in the config) and re-run `squelchd auth`."
    )))
}

/// The one field of `users.getProfile` this module reads.
#[derive(Deserialize)]
struct GmailProfile {
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

/// Ask Gmail whose mailbox a fresh access token opens. This is the only source
/// of truth for that: the exchange response names a token, never an account.
fn fetch_profile_email(
    http: &reqwest::blocking::Client,
    profile_url: &str,
    access_token: &str,
) -> Result<String> {
    // The token rides in the header and nowhere else; reqwest's own errors carry
    // the URL, not the headers, so a failure here cannot print it.
    let resp = http
        .get(profile_url)
        .bearer_auth(access_token)
        .send()
        .map_err(|e| {
            CoreError::Credential(format!(
                "could not ask Google which account this consent authorized: {e}; \
                 nothing was stored"
            ))
        })?;

    let status = resp.status().as_u16();
    let body = read_capped(resp, "Gmail's profile response")?;
    if status != 200 {
        return Err(CoreError::Credential(format!(
            "Google answered {status} when asked which account this consent authorized, so the \
             token could not be matched to the configured mailbox; nothing was stored. Re-run \
             `squelchd auth`."
        )));
    }

    let profile: GmailProfile = serde_json::from_slice(&body).map_err(|e| {
        CoreError::Credential(format!(
            "Google sent an unreadable profile response: {:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    profile
        .email_address
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| {
            CoreError::Credential(
                "Google's profile response named no account, so this token could not be matched \
                 to the configured mailbox; nothing was stored."
                    .to_string(),
            )
        })
}

/// Read a response body with a hard ceiling on it. A declared `Content-Length`
/// is refused before a byte is read, and the read itself goes through `take` so
/// that a chunked body which declares nothing is bounded too.
fn read_capped(resp: reqwest::blocking::Response, what: &str) -> Result<Vec<u8>> {
    let too_big = || {
        CoreError::Credential(format!(
            "{what} is larger than {MAX_RESPONSE_BODY} bytes, which nothing in this contract \
             is; refusing to read it"
        ))
    };
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BODY as u64)
    {
        return Err(too_big());
    }
    let mut body = Vec::new();
    // One byte past the ceiling: reading it is how an undeclared overrun shows up.
    resp.take(MAX_RESPONSE_BODY as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| CoreError::Credential(format!("reading {what}: {e}")))?;
    if body.len() > MAX_RESPONSE_BODY {
        return Err(too_big());
    }
    Ok(body)
}

/// Run the interactive read-only flow with the defaults: ephemeral port, browser
/// auto-open.
pub fn run_installed_app_flow(
    client: &OAuthClientConfig,
    account_email: &str,
) -> Result<StoredToken> {
    run_auth_flow(client, account_email, &AuthFlowOptions::default())
}

/// Run the interactive authorization flow and return the token. BLOCKS the calling
/// thread until the browser redirect arrives.
///
/// `account_email` is the account the caller intends to store this under, and
/// the exchange refuses to hand back a token for any other mailbox.
pub fn run_auth_flow(
    client: &OAuthClientConfig,
    account_email: &str,
    opts: &AuthFlowOptions,
) -> Result<StoredToken> {
    // Headless -> a FIXED port so it can be SSH-forwarded; otherwise ephemeral.
    let bind_addr = if opts.headless {
        format!("127.0.0.1:{}", opts.port)
    } else {
        "127.0.0.1:0".to_string()
    };
    let (listener, redirect_uri) = bind_consent_listener(&bind_addr)?;
    let port = listener
        .local_addr()
        .map_err(|e| CoreError::Credential(format!("reading listener addr: {e}")))?
        .port();

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

    // Loopback: only something already on this machine can connect, so a
    // mismatched `state` is worth ending the flow over.
    let code = wait_for_code(
        &listener,
        csrf_token.secret(),
        StrangerPolicy::Fatal,
        CONSENT_WAIT_TIMEOUT,
    )?;

    let (_mailbox, token) = exchange_code(
        &oauth,
        Some(account_email),
        opts.scopes,
        code,
        pkce_verifier,
    )?;
    Ok(token)
}

/// Where the export flow's one-shot consent listener binds.
///
/// The `redirect_uri` is loopback either way: Google registers no other target
/// for a Desktop client, and the browser that follows it resolves `127.0.0.1`
/// on its own machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentBind {
    /// `127.0.0.1` on an ephemeral port. Nothing off this machine can connect.
    Loopback,
    /// `0.0.0.0:port` — the opt-in for running the export inside a container.
    ///
    /// Under `docker run -p 8847:8847` a listener on `127.0.0.1` is unreachable,
    /// because that loopback is the CONTAINER's; the published port only reaches
    /// a process bound to every interface.
    ///
    /// EXPOSURE, CREDENTIAL: for the length of one consent, anything that can
    /// route to this host's `port` can connect to the listener. What it could
    /// deliver there is an authorization code, which is inert without the PKCE
    /// verifier held in this process and is checked against a per-flow `state`
    /// before it is redeemed. That is a real reduction, not zero, which is why
    /// this is opt-in and never the default.
    ///
    /// EXPOSURE, AVAILABILITY: those strangers can also connect and say nothing,
    /// or say the wrong thing. So on this bind a mismatched `state` is answered
    /// 400 and WAITED OUT rather than treated as fatal (the real redirect still
    /// wins on state equality, so no check is weakened), every connection has a
    /// read timeout, and the wait as a whole has a deadline. The worst a
    /// stranger can do is make the operator run the export again.
    AllInterfaces { port: u16 },
}

impl ConsentBind {
    /// The `bind` address this choice means.
    fn bind_addr(self) -> String {
        match self {
            ConsentBind::Loopback => "127.0.0.1:0".to_string(),
            ConsentBind::AllInterfaces { port } => format!("0.0.0.0:{port}"),
        }
    }

    /// What a request carrying the wrong `state` means on this bind. Off
    /// loopback it is a stranger who must not be able to end the flow; on
    /// loopback it is a local process doing something odd, and the operator is
    /// better served by hearing about it.
    fn stranger_policy(self) -> StrangerPolicy {
        match self {
            ConsentBind::Loopback => StrangerPolicy::Fatal,
            ConsentBind::AllInterfaces { .. } => StrangerPolicy::KeepWaiting,
        }
    }
}

/// What the consent listener does with a request that is not the redirect it is
/// waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrangerPolicy {
    /// End the flow. Only whatever shares this machine can reach the listener.
    Fatal,
    /// Answer 400 and keep waiting, because anything routable can reach it.
    KeepWaiting,
}

/// Bind the one-shot consent listener and derive the `redirect_uri` for it.
///
/// The URI is ALWAYS `http://127.0.0.1:<port>` whatever the bind address is:
/// that is the only shape Google accepts for a Desktop client, and it names the
/// browser's own loopback rather than this process's.
fn bind_consent_listener(bind_addr: &str) -> Result<(TcpListener, String)> {
    let listener = bind_reusable(bind_addr).map_err(|e| {
        CoreError::Credential(format!("binding loopback listener on {bind_addr}: {e}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| CoreError::Credential(format!("reading listener addr: {e}")))?
        .port();
    Ok((listener, format!("http://127.0.0.1:{port}")))
}

/// `TcpListener::bind` with SO_REUSEADDR, which std cannot set before binding.
///
/// The fixed-port flows (`auth --write --headless`, `--export --write
/// --expose-consent-listener`) bind the same port for each of two consents in
/// one run, and the first consent leaves its accepted connection in TIME_WAIT:
/// this listener answers the redirect and closes first. On Linux — the platform
/// those flows exist for — a plain bind then refuses the second flow's port for
/// a minute with "Address already in use". SO_REUSEADDR only permits binding
/// over such remnants; stealing a port with a LIVE listener stays refused (that
/// would be SO_REUSEPORT), so nothing about who can receive the redirect
/// changes.
///
/// TODO(windows): that refusal holds on Unix only. Windows SO_REUSEADDR allows
/// binding over a LIVE listener unless it set SO_EXCLUSIVEADDRUSE, so on that
/// platform this flag is the hijack it refuses elsewhere. The crate compiles
/// for Windows even though no flow ships there; before one does, gate this
/// `set_reuse_address` behind `#[cfg(unix)]` (TIME_WAIT does not block a rebind
/// on Windows, so the flag buys nothing there anyway).
fn bind_reusable(bind_addr: &str) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Socket, Type};

    let addr: std::net::SocketAddr = bind_addr
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(16)?;
    Ok(socket.into())
}

/// Run consent on a machine that has a browser but NO daemon config, and return
/// the mailbox Google named along with the token. Nothing is stored: the caller
/// packs the token into a [`CredentialTransfer`] for a headless host.
///
/// Same client, same PKCE, same guarded exchange as [`run_auth_flow`].
///
/// `expected_account` is the exporting machine's OWN configured account when it
/// has one, and `None` only when it genuinely does not. The export machine is
/// usually a laptop with no daemon config, which is why this flow can discover
/// the mailbox instead of verifying one. But when the config IS there, skipping
/// the check would mint and PRINT a live refresh token for whichever Google
/// session happened to be signed in, and printing it is the part that cannot be
/// taken back. So the check runs whenever the answer is available.
///
/// Every line this prints goes to STDERR, because stdout is the blob.
pub fn run_export_flow(
    client: &OAuthClientConfig,
    expected_account: Option<&str>,
    scopes: AuthScopes,
    bind: ConsentBind,
) -> Result<(String, StoredToken)> {
    let (listener, redirect_uri) = bind_consent_listener(&bind.bind_addr())?;
    let oauth = google_client(client, &redirect_uri)?;
    let (auth_url, csrf_token, pkce_verifier) =
        authorize_url(&oauth, scopes, CsrfToken::new_random());

    eprintln!(
        "\nOpen this URL in your browser to authorize squelch ({}):\n",
        scopes.label()
    );
    eprintln!("{auth_url}\n");
    if webbrowser::open(auth_url.as_str()).is_err() {
        eprintln!("(could not auto-open a browser; copy the URL above manually)");
    }
    if matches!(bind, ConsentBind::AllInterfaces { .. }) {
        // The BOUND port, not the requested one: `--port 0` binds an ephemeral
        // port, and the operator is about to publish this number.
        let port = listener
            .local_addr()
            .map_err(|e| CoreError::Credential(format!("reading listener addr: {e}")))?
            .port();
        eprintln!(
            "The consent listener is bound to every interface on port {port} until this one \
             consent finishes, so open the URL from a browser that can reach it."
        );
    }
    eprintln!("Waiting for the OAuth redirect on {redirect_uri} ...");

    let code = wait_for_code(
        &listener,
        csrf_token.secret(),
        bind.stranger_policy(),
        CONSENT_WAIT_TIMEOUT,
    )?;
    exchange_code(&oauth, expected_account, scopes, code, pkce_verifier)
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
    account_email: &str,
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
    let (_mailbox, token) =
        exchange_code(&oauth, Some(account_email), scopes, code, pkce_verifier)?;
    Ok(token)
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
    // this side does not need: the poll deadline is the contract's TTL. A body
    // over the cap ends the flow whatever the status says, because a host that
    // answers a registration with megabytes is not the contract's broker.
    let body = read_capped(resp, "the consent broker's registration response")?;
    let detail = broker_detail(&String::from_utf8_lossy(&body));
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
                // Not a setback to wait out: a dropped connection is weather, an
                // oversized body is the other end not speaking the contract, and
                // polling it again only streams the same flood.
                let payload = read_capped(resp, "the consent broker's claim response")?;
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

/// An untrusted string, made safe to print. Use it on anything that reaches a
/// terminal from a broker the user merely named or from a credential blob
/// somebody pasted.
///
/// ASCII graphic plus space is the entire alphabet of a contract-conformant
/// message, and it is also the only alphabet that cannot rearrange the trusted
/// literal text such a detail is spliced into: control characters would be
/// escape sequences rather than text (an `ESC` clears the screen, a `BEL`
/// retitles the window), and bidi/format characters (`U+202E` and friends) would
/// let the source reverse the rendered line and compose advice nobody wrote out
/// of our own words. Length is theirs to choose unless it is bounded here too.
pub fn printable(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(MAX_UNTRUSTED_DETAIL)
        .collect()
}

/// Wait on the consent listener for the browser redirect, validate the CSRF
/// `state`, and return the authorization `code`.
///
/// NOTHING here may block without an end: on `AllInterfaces` the listener is
/// reachable by anything that can route to the host, so one stranger who
/// connects and says nothing would otherwise wedge the export forever, and one
/// who says the wrong thing would end it. Hence three bounds — an overall
/// `budget`, a per-connection read timeout, and `policy` for what a wrong
/// `state` costs. A request whose `state` MATCHES is still authoritative in
/// every case, so none of this weakens the CSRF check.
fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    policy: StrangerPolicy,
    budget: Duration,
) -> Result<String> {
    let deadline = Instant::now() + budget;
    // Non-blocking accept is what makes the deadline reachable at all: a
    // blocking one parks until somebody connects, which may be never.
    listener
        .set_nonblocking(true)
        .map_err(|e| CoreError::Credential(format!("arming the consent listener: {e}")))?;

    // Loop so stray pokes (favicon probes, port scanners) don't consume the
    // one-shot listener.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CoreError::Credential(format!(
                "no consent arrived within {} minutes, so the authorization was abandoned and \
                 nothing was stored; re-run the command to try again.",
                budget.as_secs().div_ceil(60)
            )));
        }

        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(CONSENT_ACCEPT_POLL.min(remaining));
                continue;
            }
            Err(e) => return Err(CoreError::Credential(format!("accept failed: {e}"))),
        };
        // A socket accepted from a non-blocking listener inherits that flag on
        // some platforms, and the read below is meant to block up to its budget.
        let timeouts = stream
            .set_nonblocking(false)
            .and_then(|()| stream.set_read_timeout(Some(CONSENT_READ_TIMEOUT.min(remaining))))
            .and_then(|()| stream.set_write_timeout(Some(CONSENT_READ_TIMEOUT.min(remaining))));
        if timeouts.is_err() {
            continue;
        }

        let mut buf = [0u8; 4096];
        // A connection that stalls or dies is somebody else's problem, never the
        // end of the flow: drop it and keep waiting for the browser.
        let Ok(n) = stream.read(&mut buf) else {
            continue;
        };
        let request = String::from_utf8_lossy(&buf[..n]);

        // First line: "GET /?code=...&state=... HTTP/1.1"
        let path = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");

        let (code, state) = parse_redirect_query(path);
        let ours = state
            .as_deref()
            .is_some_and(|s| constant_time_eq(s, expected_state));

        // No OAuth params at all (favicon, empty probe): answer and keep waiting.
        if code.is_none() && state.is_none() {
            write_http(&mut stream, "204 No Content", "");
            continue;
        }

        if ours {
            // State equality identifies THIS flow, so whatever arrived is the
            // answer to it: the code, or Google reporting there will not be one.
            if let Some(code_val) = code.as_deref().filter(|c| !c.is_empty()) {
                let ok =
                    "squelch is authorized. You can close this tab and return to the terminal.";
                write_http(&mut stream, "200 OK", ok);
                return Ok(code_val.to_string());
            }
            let body = "the consent screen returned no authorization code; aborting";
            write_http(&mut stream, "400 Bad Request", body);
            return Err(CoreError::Credential(body.to_string()));
        }

        let body = "state mismatch (possible CSRF)";
        write_http(&mut stream, "400 Bad Request", body);
        match policy {
            StrangerPolicy::Fatal => return Err(CoreError::Credential(body.to_string())),
            // The real redirect can still arrive and still wins on state.
            StrangerPolicy::KeepWaiting => continue,
        }
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
    use crate::config::{GMAIL_MODIFY_SCOPE, GMAIL_SEND_SCOPE};

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

    /// The export listener is loopback unless the operator opts out, and the
    /// redirect Google is handed stays loopback either way: it names the
    /// BROWSER's `127.0.0.1`, not this process's bind address.
    #[test]
    fn the_export_listener_is_loopback_unless_asked_otherwise() {
        assert_eq!(ConsentBind::Loopback.bind_addr(), "127.0.0.1:0");
        assert_eq!(
            ConsentBind::AllInterfaces { port: 8847 }.bind_addr(),
            "0.0.0.0:8847"
        );

        let (listener, redirect_uri) = bind_consent_listener("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(redirect_uri, format!("http://127.0.0.1:{port}"));
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

    /// Bidi and format characters never reach a terminal: they rearrange the
    /// trusted literal text this detail is spliced into rather than adding to
    /// it, which is a way to compose advice nobody wrote.
    #[test]
    fn printable_drops_bidi_and_format_characters() {
        // RLO would render the rest of the line right-to-left.
        assert_eq!(printable("safe\u{202e}gnorts"), "safegnorts");
        for sneaky in [
            "\u{202a}", "\u{202b}", "\u{202c}", "\u{202d}", "\u{202e}", "\u{200e}", "\u{200f}",
            "\u{2066}", "\u{2067}", "\u{2068}", "\u{2069}", "\u{200b}", "\u{feff}", "\u{00a0}",
        ] {
            let out = printable(&format!("a{sneaky}b"));
            assert_eq!(out, "ab", "{sneaky:?} survived as {out:?}");
        }
        // Everything a contract-conformant message is made of still survives.
        assert_eq!(
            printable("auth_url must use https (got `ftp://x`)"),
            "auth_url must use https (got `ftp://x`)"
        );
    }

    // --- the guarded exchange ----------------------------------------------
    //
    // The exchange itself talks to Google; the two judgements that decide
    // whether its token may be stored are pure and asserted here.

    #[test]
    fn a_token_for_another_account_is_refused_and_names_both() {
        let err = check_account_match("me@gmail.com", "someone.else@gmail.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("someone.else@gmail.com"), "{err}");
        assert!(err.contains("me@gmail.com"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");
    }

    /// Google echoes the address in its own casing and the config value is
    /// hand-typed, so neither case nor stray whitespace is a different mailbox.
    #[test]
    fn account_match_ignores_case_and_surrounding_space() {
        assert!(check_account_match("me@gmail.com", "me@gmail.com").is_ok());
        assert!(check_account_match("Me@Gmail.COM", "me@gmail.com").is_ok());
        assert!(check_account_match("  me@gmail.com\n", "me@gmail.com ").is_ok());
        // A subaddress or a different domain IS a different mailbox.
        assert!(check_account_match("me@gmail.com", "me+x@gmail.com").is_err());
        assert!(check_account_match("me@gmail.com", "me@example.com").is_err());
        assert!(check_account_match("me@gmail.com", "").is_err());
    }

    /// A half-granted consent is invisible afterwards: the credential stores and
    /// refreshes like any other and only fails at the first send.
    #[test]
    fn a_partial_grant_is_refused_and_names_what_is_missing() {
        let granted = vec![GMAIL_MODIFY_SCOPE.to_string()];
        let err = check_scope_grant(AuthScopes::Write, Some(&granted))
            .unwrap_err()
            .to_string();
        assert!(err.contains(GMAIL_SEND_SCOPE), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");

        // Read is not a subset of nothing, and a Write grant is not a Read one.
        assert!(check_scope_grant(AuthScopes::Read, Some(&[])).is_err());
        assert!(check_scope_grant(AuthScopes::Read, Some(&granted)).is_err());
    }

    #[test]
    fn a_grant_that_covers_the_request_passes_even_with_extras() {
        let read = vec![GMAIL_READONLY_SCOPE.to_string()];
        assert!(check_scope_grant(AuthScopes::Read, Some(&read)).is_ok());

        // Google adds `openid`/`email`/`profile` of its own accord; coverage is
        // one-directional.
        let write: Vec<String> = WRITE_SCOPES
            .iter()
            .map(|s| s.to_string())
            .chain(["openid".to_string(), "email".to_string()])
            .collect();
        assert!(check_scope_grant(AuthScopes::Write, Some(&write)).is_ok());

        // RFC 6749 §5.1: an absent `scope` means the grant matches the request.
        assert!(check_scope_grant(AuthScopes::Write, None).is_ok());
    }

    /// The cap has to hold for a body that DECLARES its size and for one that
    /// declares nothing, because the other end picks which.
    #[test]
    fn an_oversized_response_body_is_refused_declared_or_not() {
        for declared in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut scratch = [0u8; 1024];
                let _ = stream.read(&mut scratch);
                let flood = vec![b'x'; MAX_RESPONSE_BODY * 2];
                if declared {
                    let head =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", flood.len());
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&flood);
                } else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    );
                    let _ = stream.write_all(format!("{:x}\r\n", flood.len()).as_bytes());
                    let _ = stream.write_all(&flood);
                    let _ = stream.write_all(b"\r\n0\r\n\r\n");
                }
                let _ = stream.flush();
            });

            let http = broker_http().unwrap();
            let resp = http
                .post(format!("http://127.0.0.1:{port}/v1/claim"))
                .send()
                .unwrap();
            let err = read_capped(resp, "the test body").unwrap_err().to_string();
            assert!(
                err.contains("refusing to read it"),
                "declared={declared}: {err}"
            );
            // The flood is never fully written once we hang up; that is the point.
            let _ = server.join();
        }
    }

    #[test]
    fn a_response_body_under_the_cap_is_read_whole() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut scratch = [0u8; 1024];
            let _ = stream.read(&mut scratch);
            let body = r#"{"status":"pending"}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.flush();
        });

        let http = broker_http().unwrap();
        let resp = http
            .post(format!("http://127.0.0.1:{port}/v1/claim"))
            .send()
            .unwrap();
        let body = read_capped(resp, "the test body").unwrap();
        assert!(matches!(
            interpret_claim(200, &body).unwrap(),
            ClaimStep::Pending(None)
        ));
        let _ = server.join();
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
            MAX_UNTRUSTED_DETAIL
        );
    }

    // --- the transfer verification -----------------------------------------
    //
    // The two network calls are the same ones the exchange makes; what they
    // MEAN for a pasted blob is decided here, and asserted without Google.

    fn read_grant() -> Vec<String> {
        vec![GMAIL_READONLY_SCOPE.to_string()]
    }

    fn write_grant() -> Vec<String> {
        WRITE_SCOPES.iter().map(|s| s.to_string()).collect()
    }

    /// The blob's `account` field is a claim by whoever pasted it. Google's
    /// answer is the fact, and a disagreement refuses the whole import.
    #[test]
    fn a_blob_google_disagrees_with_is_refused_and_names_both() {
        let err = judge_transfer_credential(
            "victim@gmail.com",
            CredentialKind::Read,
            "attacker@gmail.com",
            Some(&read_grant()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("attacker@gmail.com"), "{err}");
        assert!(err.contains("victim@gmail.com"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");
        assert!(err.contains("not evidence"), "{err}");

        // The mailbox Google names is echoed, so it is disarmed first.
        let err = judge_transfer_credential(
            "victim@gmail.com",
            CredentialKind::Read,
            "a\u{1b}[2Jb@gmail.com",
            Some(&read_grant()),
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains('\u{1b}'), "escape survived: {err}");
    }

    /// A hand-edited `kind` decides which slot a token lands in. The Read slot
    /// holding a modify+send token is the two-door split gone, so the grant has
    /// to cover the slot rather than the other way round.
    #[test]
    fn a_write_token_cannot_be_filed_in_the_read_slot() {
        let err = judge_transfer_credential(
            "me@gmail.com",
            CredentialKind::Read,
            "me@gmail.com",
            Some(&write_grant()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("wrong reach"), "{err}");
        assert!(err.contains(GMAIL_READONLY_SCOPE), "{err}");

        // And the converse: a read token in the Write slot cannot send.
        let err = judge_transfer_credential(
            "me@gmail.com",
            CredentialKind::Write,
            "me@gmail.com",
            Some(&read_grant()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(GMAIL_SEND_SCOPE), "{err}");

        // Each grant in its own slot is what a real export produces.
        assert!(
            judge_transfer_credential(
                "me@gmail.com",
                CredentialKind::Read,
                "ME@gmail.com",
                Some(&read_grant())
            )
            .is_ok()
        );
        assert!(
            judge_transfer_credential(
                "me@gmail.com",
                CredentialKind::Write,
                "me@gmail.com",
                Some(&write_grant())
            )
            .is_ok()
        );
    }

    /// On a refresh there is no request for an absent `scope` to mean "same as",
    /// so silence proves nothing and the blob's own `kind` would be the only
    /// word on which slot this belongs in.
    #[test]
    fn a_refresh_that_names_no_scopes_is_refused_rather_than_assumed() {
        let err =
            judge_transfer_credential("me@gmail.com", CredentialKind::Write, "me@gmail.com", None)
                .unwrap_err()
                .to_string();
        assert!(err.contains("no scopes"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");
    }

    /// A refresh token only works for the client that minted it, so a blob from
    /// another OAuth client fails at paste time with the fix named, instead of
    /// as a mystery `invalid_client` on the first sync an hour later.
    #[test]
    fn a_blob_from_another_oauth_client_says_so() {
        let err = transfer_client_error(
            CredentialKind::Read,
            &CoreError::Credential(
                "refresh failed: Server returned error response: invalid_client".to_string(),
            ),
        )
        .to_string();
        assert!(err.contains("SQUELCH_CLIENT_ID"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");

        // Anything else keeps the underlying reason, which is what a network
        // failure needs.
        let err = transfer_client_error(
            CredentialKind::Read,
            &CoreError::Credential("refresh failed: connection reset by peer".to_string()),
        )
        .to_string();
        assert!(err.contains("connection reset by peer"), "{err}");
    }

    /// Every slot maps to the scope set it requires, both ways. If this ever
    /// crossed, verification would hold a token against the wrong bar.
    #[test]
    fn credential_kinds_and_scope_sets_map_both_ways() {
        for kind in [CredentialKind::Read, CredentialKind::Write] {
            assert_eq!(AuthScopes::from(kind).kind(), kind);
        }
        assert_eq!(AuthScopes::from(CredentialKind::Read), AuthScopes::Read);
        assert_eq!(AuthScopes::from(CredentialKind::Write), AuthScopes::Write);
    }

    // --- the verification against a scripted Google -------------------------
    //
    // The `_at` seams exist for exactly these: the same refusals the daemon
    // runs, end to end, with the two pinned URLs pointed at a socket this test
    // scripts. Nothing here weakens the pinning — the public wrappers are the
    // only other callers, and they pass the Google constants.

    /// One scripted answer from a fake Google: raw bytes on one accepted
    /// connection, after an optional pause.
    struct ScriptedAnswer {
        delay: Duration,
        raw: String,
    }

    /// A JSON answer. The Content-Type matters: the oauth2 crate refuses to
    /// parse a token response without it.
    fn json_answer(status: &str, body: &str) -> ScriptedAnswer {
        ScriptedAnswer {
            delay: Duration::ZERO,
            raw: format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        }
    }

    /// A token-endpoint success, with or without the `scope` field: a refresh
    /// that names no scopes is one of the refusals under test.
    fn token_answer(scope: Option<&str>) -> ScriptedAnswer {
        let scope_field = scope
            .map(|s| format!(r#","scope":"{s}""#))
            .unwrap_or_default();
        json_answer(
            "200 OK",
            &format!(
                r#"{{"access_token":"fresh-access","token_type":"Bearer","expires_in":3600,"refresh_token":"fresh-refresh"{scope_field}}}"#
            ),
        )
    }

    /// Serve the answers in order, one connection each, on an ephemeral
    /// loopback port. Every script says `Connection: close` so reqwest cannot
    /// pool its way past the sequencing. The thread is left to finish on its
    /// own, so a caller that times out early does not sit through a scripted
    /// delay.
    fn scripted_google(answers: Vec<ScriptedAnswer>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        );
        std::thread::spawn(move || {
            for answer in answers {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut scratch = [0u8; 4096];
                let _ = stream.read(&mut scratch);
                if !answer.delay.is_zero() {
                    std::thread::sleep(answer.delay);
                }
                let _ = stream.write_all(answer.raw.as_bytes());
                let _ = stream.flush();
            }
        });
        base
    }

    /// The token a paste carries. The refresh token is the secret whose absence
    /// from every error message these tests also assert.
    fn pasted_token() -> StoredToken {
        StoredToken {
            access_token: "stale-access".to_string(),
            refresh_token: Some("pasted-refresh-secret".to_string()),
            expires_at: None,
        }
    }

    /// Run the real verification against the scripted host.
    fn verify_at(base: &str, expected: &str, kind: CredentialKind) -> Result<()> {
        verify_transfer_credential_at(
            &test_client(),
            expected,
            kind,
            &pasted_token(),
            &format!("{base}/token"),
            &format!("{base}/profile"),
            Duration::from_secs(5),
        )
    }

    fn verify_err(base: &str, expected: &str, kind: CredentialKind) -> String {
        match verify_at(base, expected, kind) {
            Err(e) => e.to_string(),
            Ok(()) => panic!("the verification should have refused"),
        }
    }

    /// A token endpoint that accepts the connection and then says nothing must
    /// cost the timeout, not forever: this refresh runs inside the sync loop's
    /// blocking path and inside `auth --import`.
    #[test]
    fn a_hung_token_endpoint_cannot_wedge_a_refresh() {
        let base = scripted_google(vec![ScriptedAnswer {
            delay: Duration::from_secs(1),
            raw: String::new(),
        }]);
        let started = Instant::now();
        let err = crate::credentials::refresh_stored_token_detailed_at(
            &test_client(),
            "pasted-refresh-secret",
            &format!("{base}/token"),
            Duration::from_millis(200),
        )
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "the refresh outlived its budget: {:?}",
            started.elapsed()
        );
        assert!(err.contains("refresh failed"), "{err}");
        assert!(!err.contains("pasted-refresh-secret"), "{err}");
    }

    /// `invalid_client` off the wire, not a hand-built string: the blob was
    /// minted by a different OAuth client, and the refusal names the fix.
    #[test]
    fn a_blob_from_another_oauth_client_is_refused_at_the_wire() {
        let base = scripted_google(vec![json_answer(
            "401 Unauthorized",
            r#"{"error":"invalid_client"}"#,
        )]);
        let err = verify_err(&base, "me@gmail.com", CredentialKind::Read);
        assert!(err.contains("SQUELCH_CLIENT_ID"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");
        assert!(!err.contains("pasted-refresh-secret"), "{err}");
    }

    /// A refresh that succeeds proves the client, not the mailbox: when the
    /// profile names a stranger, the refusal must land before anything stores.
    #[test]
    fn a_verified_blob_that_opens_a_strangers_mailbox_is_refused() {
        let base = scripted_google(vec![
            token_answer(Some(GMAIL_READONLY_SCOPE)),
            json_answer("200 OK", r#"{"emailAddress":"attacker@gmail.com"}"#),
        ]);
        let err = verify_err(&base, "victim@gmail.com", CredentialKind::Read);
        assert!(err.contains("attacker@gmail.com"), "{err}");
        assert!(err.contains("victim@gmail.com"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");
    }

    /// The wire shape of the fail-closed rule: a refresh whose answer names no
    /// `scope` field at all leaves the slot unprovable.
    #[test]
    fn a_refresh_naming_no_scopes_is_refused_at_the_wire() {
        let base = scripted_google(vec![
            token_answer(None),
            json_answer("200 OK", r#"{"emailAddress":"me@gmail.com"}"#),
        ]);
        let err = verify_err(&base, "me@gmail.com", CredentialKind::Read);
        assert!(err.contains("no scopes"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");
    }

    /// A profile answer that declares more than the cap is refused before a
    /// byte of it is read.
    #[test]
    fn an_oversized_profile_answer_is_refused() {
        let base = scripted_google(vec![
            token_answer(Some(GMAIL_READONLY_SCOPE)),
            ScriptedAnswer {
                delay: Duration::ZERO,
                raw: format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_RESPONSE_BODY * 2
                ),
            },
        ]);
        let err = verify_err(&base, "me@gmail.com", CredentialKind::Read);
        assert!(err.contains("refusing to read it"), "{err}");
    }

    /// A token endpoint that answers with something other than the contract is
    /// an error, and the one secret in play stays out of it.
    #[test]
    fn garbage_from_the_token_endpoint_names_no_secrets() {
        let base = scripted_google(vec![ScriptedAnswer {
            delay: Duration::ZERO,
            raw: "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 14\r\nConnection: close\r\n\r\n<html>no</html>".to_string(),
        }]);
        let err = verify_err(&base, "me@gmail.com", CredentialKind::Read);
        assert!(err.contains("nothing was stored"), "{err}");
        assert!(!err.contains("pasted-refresh-secret"), "{err}");
    }

    /// The happy path through the same seam, so the refusals above are known to
    /// be refusals and not a broken harness.
    #[test]
    fn the_verification_passes_a_matching_credential() {
        let base = scripted_google(vec![
            token_answer(Some(GMAIL_READONLY_SCOPE)),
            json_answer("200 OK", r#"{"emailAddress":"me@gmail.com"}"#),
        ]);
        assert!(verify_at(&base, "me@gmail.com", CredentialKind::Read).is_ok());
    }

    /// Google unions grants per Cloud project: once write is granted anywhere,
    /// a Read credential's refresh legitimately reports the union. The scope
    /// check is a subset floor for exactly this reason, and an exact match here
    /// would refuse the Read entry of every `--export --write` blob.
    #[test]
    fn a_read_slot_credential_carrying_the_union_grant_still_passes() {
        let union = format!("{GMAIL_READONLY_SCOPE} {}", WRITE_SCOPES.join(" "));
        let base = scripted_google(vec![
            token_answer(Some(&union)),
            json_answer("200 OK", r#"{"emailAddress":"me@gmail.com"}"#),
        ]);
        assert!(verify_at(&base, "me@gmail.com", CredentialKind::Read).is_ok());
    }

    // --- the consent listener ----------------------------------------------

    /// One request to the listener, from a client that behaves like a browser.
    fn get(port: u16, path: &str) {
        use std::net::TcpStream;
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ = s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes());
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
    }

    /// Off loopback, anything routable can connect. A wrong-`state` request must
    /// not end the flow, and the real redirect still wins on state equality.
    #[test]
    fn an_exposed_listener_waits_out_a_stranger() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            get(port, "/?code=stranger-code&state=not-our-state");
            get(port, "/favicon.ico");
            get(port, "/?code=the-real-code&state=our-state");
        });

        let code = wait_for_code(
            &listener,
            "our-state",
            StrangerPolicy::KeepWaiting,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(code, "the-real-code", "a stranger must not win the flow");
        let _ = client.join();
    }

    /// On loopback the only thing that can connect already shares the machine,
    /// and the operator is better served by hearing about it.
    #[test]
    fn a_loopback_listener_treats_a_state_mismatch_as_fatal() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || get(port, "/?code=c&state=wrong"));

        let err = wait_for_code(
            &listener,
            "our-state",
            StrangerPolicy::Fatal,
            Duration::from_secs(10),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("state mismatch"), "{err}");
        let _ = client.join();
    }

    /// A stranger who connects and then says nothing must not wedge the export:
    /// the read is bounded, and so is the wait as a whole.
    #[test]
    fn a_silent_connection_cannot_wedge_the_listener() {
        use std::net::TcpStream;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let squatter = std::thread::spawn(move || {
            let _held = TcpStream::connect(("127.0.0.1", port)).unwrap();
            // Hold the connection open, saying nothing, until the wait is over.
            let _ = done_rx.recv_timeout(Duration::from_secs(30));
        });

        let started = Instant::now();
        let err = wait_for_code(
            &listener,
            "our-state",
            StrangerPolicy::KeepWaiting,
            Duration::from_millis(400),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no consent arrived"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait outlived its budget: {:?}",
            started.elapsed()
        );
        let _ = done_tx.send(());
        let _ = squatter.join();
    }

    /// `auth --write --headless` binds the SAME fixed port for each of two
    /// consents, and the first consent's connection is still in TIME_WAIT when
    /// the second binds: this listener answers the redirect and closes first.
    /// On Linux a plain bind refuses that port for a minute, which is the
    /// second consent failing with "Address already in use" — SO_REUSEADDR on
    /// the listener is what this asserts.
    #[test]
    fn a_fixed_port_survives_back_to_back_consents() {
        use std::net::TcpStream;

        let (listener, _) = bind_consent_listener("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let _ = s.write_all(
                b"GET /?code=the-code&state=our-state HTTP/1.1\r\nHost: x\r\n\r\n",
            );
            // A browser reads the answer, which is what leaves the SERVER side
            // of this connection in TIME_WAIT once the listener closes first.
            let mut sink = Vec::new();
            let _ = s.read_to_end(&mut sink);
        });
        let code = wait_for_code(
            &listener,
            "our-state",
            StrangerPolicy::Fatal,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(code, "the-code");
        let _ = browser.join();
        drop(listener);

        // The same port again, immediately, exactly as the second consent does.
        let rebound = bind_consent_listener(&format!("127.0.0.1:{port}"));
        assert!(
            rebound.is_ok(),
            "the second consent could not bind: {}",
            rebound.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    /// An empty deadline is the flow giving up, not a panic or a busy loop.
    #[test]
    fn the_consent_wait_gives_up_when_nobody_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let err = wait_for_code(
            &listener,
            "our-state",
            StrangerPolicy::Fatal,
            Duration::from_millis(200),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no consent arrived"), "{err}");
        assert!(err.contains("nothing was stored"), "{err}");
    }
}
