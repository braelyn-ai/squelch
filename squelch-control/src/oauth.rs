//! The confidential-client half of Google OAuth: build a consent URL, redeem
//! the authorization code, and ask Google which mailbox the resulting token
//! opens.
//!
//! TWO FLOWS RUN THROUGH HERE and they ask Google for different things, which is
//! why every entry point takes a [`Flow`]:
//!
//! - [`Flow::Signup`] is a GRANT. It asks for all three Gmail scopes and
//!   `access_type=offline`, because what it is for is a refresh token a daemon
//!   will use for years.
//! - [`Flow::Console`] is a LOGIN. It asks for identity alone (`openid email`),
//!   online, and the only thing it ever produces is a verified mailbox address:
//!   see [`verify_identity`], which returns a `String` and no token at all.
//!   Nothing about signing in to a console needs access to mail, so nothing here
//!   asks for it.
//!
//! THE ASYNC TWIN of what `squelch-core::auth` does for `squelchd auth`. The
//! shapes are core's on purpose (the `oauth2` crate for the exchange,
//! [`StoredToken::from_response`] for the result, core's scope constant for the
//! request) because the token this produces is opened and used by core on the
//! other side of the age envelope. What is NOT shared is the transport: core's
//! flow is blocking because it runs at a CLI before any runtime exists, and a
//! web service cannot block a runtime thread on a network round trip.
//!
//! TWO GUARDS ride on every request here, exactly as they do in core:
//! - Redirects are REFUSED. These requests carry the client secret and a
//!   bearer token; an open redirect on the token endpoint is SSRF.
//! - Every round trip is bounded by a stated budget rather than by whatever
//!   reqwest's default happens to be in some future version.
//!
//! PRIVACY: the authorization code, the verifier, the client secret, and both
//! tokens never reach a log line or an error string in this module.

use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use squelch_core::config::{GMAIL_READONLY_SCOPE, WRITE_SCOPES};
use squelch_core::credentials::StoredToken;

/// Ceiling on any response body read here. Google's token and profile answers
/// are a few hundred bytes of JSON, so this is slack rather than a budget. It
/// exists because an unbounded body from a hostile or wedged endpoint is the
/// whole process.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// The Google client with both endpoints set, spelled out once so the consent
/// URL and the exchange share one constructor.
type GoogleClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// What went wrong on the way to a token. Every variant is deliberately terse:
/// these strings reach a log line and (in generic form) a page.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("oauth client configuration: {0}")]
    Config(String),
    #[error("building the http client")]
    Http,
    /// The exchange itself failed. The underlying error is NOT included: it can
    /// echo the request, which carries the client secret and the code.
    #[error("the authorization code could not be exchanged")]
    Exchange,
    /// Google granted less than was asked for. A partial consent is fatal, not
    /// a warning: a daemon that cannot read the mailbox is not a daemon, and one
    /// that cannot archive or send is a tenant whose app 403s on every button.
    #[error("the Google consent did not include every permission this flow asks for")]
    Scope,
    /// The profile call failed or answered something that is not a mailbox.
    #[error("Google did not say which mailbox this grant is for")]
    Profile,
    /// Google returned no refresh token, so the daemon would stop working
    /// within the hour with nothing to recover from.
    #[error("Google returned no refresh token for this grant")]
    NoRefreshToken,
}

/// The endpoints and client credentials one flow needs. Borrowed from
/// [`crate::Config`] so this module has no opinion about where they came from,
/// which is also what lets tests point it at a mock Google.
pub struct GoogleEndpoints<'a> {
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub redirect_uri: &'a str,
    pub auth_url: &'a str,
    pub token_url: &'a str,
    /// Gmail's profile endpoint, which names the mailbox behind a Gmail grant.
    /// [`Flow::Signup`] only: it needs `gmail.readonly`, which is precisely what
    /// a console login does not have.
    pub profile_url: &'a str,
    /// OpenID Connect's userinfo endpoint. [`Flow::Console`] only: it is what an
    /// `openid email` token can read, and reading it is the whole errand.
    pub userinfo_url: &'a str,
    pub timeout: Duration,
}

/// Which flow a consent is being built for. It decides the scope set, the
/// consent parameters, and which endpoint names the mailbox afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Hosted signup: a grant that provisions a daemon.
    Signup,
    /// A console login for a tenant that already exists.
    Console,
}

/// A consent URL plus the two secrets that must be remembered until the user
/// comes back. NO `Debug`: the verifier is the thing that makes an intercepted
/// code redeemable.
pub struct Consent {
    pub url: String,
    pub state: String,
    pub pkce_verifier: String,
}

/// What a completed exchange yields.
pub struct ExchangedGrant {
    /// The token as the daemon's file backend stores it. Sealed immediately by
    /// the caller; never persisted here.
    pub token: StoredToken,
    /// The mailbox Google says this grant opens, as Google spells it.
    pub account_email: String,
}

/// A URL that will not parse names the FIELD, never the value: the client
/// secret is not in any of these, but a redirect URI typo is a deploy problem
/// and the field name is what fixes it.
fn bad_url(what: &'static str) -> impl Fn(url::ParseError) -> OAuthError {
    move |err| OAuthError::Config(format!("{what}: {err}"))
}

fn client(e: &GoogleEndpoints<'_>) -> Result<GoogleClient, OAuthError> {
    Ok(
        BasicClient::new(ClientId::new(e.client_id.to_string()))
            .set_client_secret(ClientSecret::new(e.client_secret.to_string()))
            .set_auth_uri(AuthUrl::new(e.auth_url.to_string()).map_err(bad_url("auth url"))?)
            .set_token_uri(TokenUrl::new(e.token_url.to_string()).map_err(bad_url("token url"))?)
            .set_redirect_uri(
                RedirectUrl::new(e.redirect_uri.to_string()).map_err(bad_url("redirect uri"))?,
            ),
    )
}

/// The http client both calls share.
fn http(timeout: Duration) -> Result<reqwest::Client, OAuthError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| OAuthError::Http)
}

/// OpenID Connect's two identity scopes, which is the entire ask of a console
/// login. `openid` is what makes this an authentication request; `email` is the
/// one claim the flow acts on.
const OPENID_SCOPE: &str = "openid";
const EMAIL_SCOPE: &str = "email";

/// The canonical spelling Google ECHOES for [`EMAIL_SCOPE`] on the token
/// response. The short name goes out on the request and the long one comes
/// back, so a check that only knew one of them would refuse every real console
/// login.
const EMAIL_SCOPE_URL: &str = "https://www.googleapis.com/auth/userinfo.email";

/// What a flow asks Google for.
///
/// SIGNUP asks for all three Gmail scopes in ONE consent, deliberately. Hosted
/// Passband ships the actions in the app, and the daemon's write path loads the
/// [`squelch_core::credentials::CredentialKind::Write`] slot, which cannot be
/// filled by a grant that was never asked for. A second consent later would mean
/// a tenant whose Archive and Send buttons 403 until they go find it, so the
/// honest thing is to ask once, on the screen that explains why. Both halves
/// come from squelch-core's constants rather than being spelled out here: these
/// strings are also what the daemon checks a token against, and two copies of a
/// scope URL is one typo away from a mailbox nobody can open.
///
/// CONSOLE asks for identity and nothing else. Signing in to a console is not a
/// reason to hold a key to somebody's mail, and a login that requested Gmail
/// scopes would be asking a person to re-approve, on every sign in, the thing
/// they already approved once at signup.
fn requested_scopes(flow: Flow) -> Vec<&'static str> {
    match flow {
        Flow::Signup => {
            let mut scopes = Vec::with_capacity(1 + WRITE_SCOPES.len());
            scopes.push(GMAIL_READONLY_SCOPE);
            scopes.extend_from_slice(WRITE_SCOPES);
            scopes
        }
        Flow::Console => vec![OPENID_SCOPE, EMAIL_SCOPE],
    }
}

/// What a grant must COVER for a flow to be usable. One entry per requirement;
/// any one of an entry's spellings satisfies it.
///
/// The alternatives exist for exactly one reason: Google accepts `email` on the
/// request and reports [`EMAIL_SCOPE_URL`] on the response. Everything else is a
/// single spelling, and the Gmail set is unchanged.
fn required_scopes(flow: Flow) -> Vec<Vec<&'static str>> {
    match flow {
        // Every Gmail scope, spelled one way, exactly as before.
        Flow::Signup => requested_scopes(flow).into_iter().map(|s| vec![s]).collect(),
        Flow::Console => vec![vec![OPENID_SCOPE], vec![EMAIL_SCOPE, EMAIL_SCOPE_URL]],
    }
}

/// Build the consent URL for one hop through Google.
///
/// The extra parameters differ by flow, and both choices are load bearing:
///
/// - SIGNUP sends `access_type=offline` + `prompt=consent`, which are what make
///   Google return a refresh token. That is the entire point of the exchange: an
///   access token alone dies in an hour.
/// - CONSOLE sends NEITHER. Online access means Google issues no refresh token,
///   so there is no long-lived credential for this service to be careful with on
///   a path that only needs to learn a name. `prompt=select_account` instead, so
///   somebody signed in to two Google accounts picks the one that owns the
///   mailbox rather than being silently refused for being the wrong person.
pub fn consent_url(
    e: &GoogleEndpoints<'_>,
    state: String,
    flow: Flow,
) -> Result<Consent, OAuthError> {
    let oauth = client(e)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut request = oauth
        .authorize_url(|| CsrfToken::new(state))
        .add_scopes(
            requested_scopes(flow)
                .into_iter()
                .map(|s| Scope::new(s.to_string())),
        )
        .set_pkce_challenge(challenge);
    request = match flow {
        Flow::Signup => request
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent"),
        Flow::Console => request.add_extra_param("prompt", "select_account"),
    };
    let (url, csrf) = request.url();
    Ok(Consent {
        url: url.to_string(),
        state: csrf.secret().to_string(),
        pkce_verifier: verifier.secret().to_string(),
    })
}

/// Redeem an authorization code and learn whose mailbox it opens.
///
/// The two checks after the exchange are why this is the only path to a sealed
/// credential. A code says nothing about who approved it: whichever Google
/// session was signed in on that browser is the one that consented, and neither
/// a cookie nor a `state` can see that. So the grant is held against what was
/// requested, and the mailbox is DISCOVERED from Google rather than taken from
/// anything the browser sent.
pub async fn exchange_code(
    e: &GoogleEndpoints<'_>,
    code: String,
    pkce_verifier: String,
) -> Result<ExchangedGrant, OAuthError> {
    let oauth = client(e)?;
    let http = http(e.timeout)?;

    let response = oauth
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier))
        .request_async(&http)
        .await
        // Deliberately dropped: oauth2's error `Display` can carry the request
        // and the provider's raw body, which is how a client secret or a code
        // ends up in somebody's log aggregator.
        .map_err(|_| OAuthError::Exchange)?;

    let granted: Option<Vec<String>> = response
        .scopes()
        .map(|s| s.iter().map(|s| s.to_string()).collect());
    check_scope_grant(granted.as_deref(), Flow::Signup)?;

    let access_token = response.access_token().secret().to_string();
    let refresh_token = response.refresh_token().map(|r| r.secret().to_string());
    // A grant with no refresh token provisions a daemon that dies within the
    // hour, long after anyone would connect the failure back to this signup.
    // Refused here, before a tenant exists.
    if refresh_token.is_none() {
        return Err(OAuthError::NoRefreshToken);
    }

    // Gmail's profile endpoint, which does not carry `email_verified`: the
    // mailbox is named by the grant this user just approved.
    let account_email =
        fetch_profile_email(&http, e.profile_url, &access_token, Verification::NotClaimed).await?;

    Ok(ExchangedGrant {
        token: StoredToken::from_response(access_token, refresh_token, response.expires_in()),
        account_email,
    })
}

/// Redeem a console login's authorization code and answer with ONE THING: the
/// mailbox Google says is signed in.
///
/// THE RETURN TYPE IS THE SECURITY PROPERTY. A console login is an
/// authentication, so no token from it is ever wanted by anything downstream;
/// making that structural (a `String` comes out, and the token dies with this
/// function's stack frame) means there is no credential for a later caller to
/// store, seal, log, or hand to a warden by mistake. The refresh token that
/// [`exchange_code`] insists on is not merely optional here: `access_type` is
/// left at Google's online default so one is never issued at all.
pub async fn verify_identity(
    e: &GoogleEndpoints<'_>,
    code: String,
    pkce_verifier: String,
) -> Result<String, OAuthError> {
    let oauth = client(e)?;
    let http = http(e.timeout)?;

    let response = oauth
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier))
        .request_async(&http)
        .await
        .map_err(|_| OAuthError::Exchange)?;

    let granted: Option<Vec<String>> = response
        .scopes()
        .map(|s| s.iter().map(|s| s.to_string()).collect());
    check_scope_grant(granted.as_deref(), Flow::Console)?;

    // The userinfo endpoint, not Gmail's profile: an `openid email` token opens
    // exactly this and nothing else, which is the point of asking for so little.
    // `Required`, because the address it answers with is the ONLY evidence this
    // flow has about who is signing in.
    let access_token = response.access_token().secret().to_string();
    fetch_profile_email(
        &http,
        e.userinfo_url,
        &access_token,
        Verification::Required,
    )
    .await
}

/// Hold the granted scopes against what the flow asked for: every requirement in
/// [`required_scopes`] must be covered by some spelling.
///
/// A SUBSET FLOOR, not an exact match, and that is not laziness: Google unions
/// grants across a Cloud project, so a user who has previously granted more to
/// another client of the same project gets a token that reports more than we
/// asked for. What must never happen is LESS, which is the case this catches:
/// Google's screen lets a user uncheck individual boxes, and a token missing
/// `gmail.send` provisions a tenant whose Compose button fails forever with no
/// way back to the consent screen.
///
/// An absent `scope` field means "same as the request" per RFC 6749 section 5.1,
/// so it passes.
fn check_scope_grant(granted: Option<&[String]>, flow: Flow) -> Result<(), OAuthError> {
    match granted {
        None => Ok(()),
        Some(scopes) => {
            let all_granted = required_scopes(flow)
                .iter()
                .all(|spellings| scopes.iter().any(|got| spellings.contains(&got.as_str())));
            if all_granted {
                Ok(())
            } else {
                Err(OAuthError::Scope)
            }
        }
    }
}

/// What either endpoint that names a mailbox answers with. ONE struct for both
/// because the two spell the same fact differently: Gmail's profile calls it
/// `emailAddress`, OpenID Connect calls it `email` and adds whether it has been
/// verified.
#[derive(serde::Deserialize)]
struct ProfileAnswer {
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
    email: Option<String>,
    /// OIDC only. An `email` claim marked unverified is a name the provider
    /// itself will not vouch for, and this flow's whole job is deciding whether
    /// somebody is who they say they are.
    ///
    /// TWO SPELLINGS, because OpenID Connect's own errata allow both and Google
    /// has shipped each: the JSON boolean `true` and the string `"true"`. A
    /// check that knew only the boolean would read a string as "not present"
    /// and, under [`Verification::Required`], refuse every real console login.
    email_verified: Option<VerifiedClaim>,
}

/// `email_verified` as a provider may spell it.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum VerifiedClaim {
    Bool(bool),
    Text(String),
}

impl VerifiedClaim {
    /// Whether this claim says verified. Anything that is not a `true` in one of
    /// the two spellings is not one.
    fn is_true(&self) -> bool {
        match self {
            VerifiedClaim::Bool(b) => *b,
            VerifiedClaim::Text(s) => s.trim().eq_ignore_ascii_case("true"),
        }
    }
}

/// Whether the endpoint being read is one that VOUCHES for the address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verification {
    /// OIDC userinfo, read by [`Flow::Console`]. The `email_verified` claim must
    /// be present and true. ABSENT IS NOT VERIFIED: this is the whole evidence a
    /// console login has that the person is who they say they are, and treating
    /// a missing claim as a yes means a provider (or anything that can answer as
    /// one) hands over a mailbox by leaving a field out.
    Required,
    /// Gmail's profile endpoint, read by [`Flow::Signup`], which does not carry
    /// the claim at all: the mailbox is named by the grant the user just
    /// approved, so there is nothing for a missing field to weaken. An explicit
    /// `false` is still refused, because a provider that volunteers one is
    /// saying something worth hearing.
    NotClaimed,
}

/// Ask Google which mailbox an access token belongs to.
///
/// `profile_url` decides which endpoint that is, and the caller picks it by
/// flow: Gmail's profile for a signup's Gmail grant, OIDC userinfo for a console
/// login's identity token.
async fn fetch_profile_email(
    http: &reqwest::Client,
    profile_url: &str,
    access_token: &str,
    verification: Verification,
) -> Result<String, OAuthError> {
    let resp = http
        .get(profile_url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| OAuthError::Profile)?;
    if !resp.status().is_success() {
        return Err(OAuthError::Profile);
    }
    let body = read_capped(resp).await?;
    email_from_answer(&body, verification)
}

/// The judgement half of [`fetch_profile_email`], with no transport in it: parse
/// the answer, hold it to [`Verification`], and hold what is left to the shape a
/// mailbox has.
fn email_from_answer(body: &[u8], verification: Verification) -> Result<String, OAuthError> {
    let profile: ProfileAnswer = serde_json::from_slice(body).map_err(|_| OAuthError::Profile)?;
    let verified = profile.email_verified.as_ref().map(VerifiedClaim::is_true);
    match (verification, verified) {
        // ABSENT IS NOT VERIFIED on the endpoint whose whole job is vouching.
        (Verification::Required, Some(true)) => {}
        (Verification::Required, _) => return Err(OAuthError::Profile),
        (Verification::NotClaimed, Some(false)) => return Err(OAuthError::Profile),
        (Verification::NotClaimed, _) => {}
    }
    let email = profile
        .email_address
        .or(profile.email)
        .unwrap_or_default();
    let email = email.trim();
    // Shape, not validity: this string becomes a database key, an env-file
    // value in a tenant's pod, and a line on a page. Anything with a control
    // character, a newline, or no `@` is not a mailbox.
    if email.is_empty()
        || email.len() > 320
        || !email.contains('@')
        || email.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(OAuthError::Profile);
    }
    Ok(email.to_string())
}

/// Read a response body up to [`MAX_RESPONSE_BODY`], streaming so that an
/// oversized body is abandoned rather than buffered.
async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, OAuthError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| OAuthError::Profile)? {
        if out.len() + chunk.len() > MAX_RESPONSE_BODY {
            return Err(OAuthError::Profile);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints() -> GoogleEndpoints<'static> {
        GoogleEndpoints {
            client_id: "client-id",
            client_secret: "client-secret",
            redirect_uri: "https://signup.passband.email/oauth/callback",
            auth_url: crate::config::GOOGLE_AUTH_URL,
            token_url: crate::config::GOOGLE_TOKEN_URL,
            profile_url: crate::config::GMAIL_PROFILE_URL,
            userinfo_url: crate::config::GOOGLE_USERINFO_URL,
            timeout: Duration::from_secs(5),
        }
    }

    /// The consent URL is the one thing a user sees before Google does, so
    /// every parameter on it is asserted: all three scopes, the PKCE method, the
    /// state we chose, and the refresh-token parameters.
    #[test]
    fn builds_a_consent_url_asking_for_all_three_scopes_with_pkce() {
        let c = consent_url(&endpoints(), "the-state".into(), Flow::Signup).unwrap();
        let url = url::Url::parse(&c.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(url.host_str(), Some("accounts.google.com"));
        // Space-delimited, as OAuth spells a scope set.
        let scope = q.get("scope").map(String::as_str).unwrap_or_default();
        let asked: Vec<&str> = scope.split(' ').collect();
        assert_eq!(asked, requested_scopes(Flow::Signup), "{scope}");
        assert!(scope.contains("gmail.readonly"), "{scope}");
        assert!(scope.contains("gmail.modify"), "{scope}");
        assert!(scope.contains("gmail.send"), "{scope}");
        assert_eq!(q.get("state").map(String::as_str), Some("the-state"));
        assert_eq!(c.state, "the-state");
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(q.get("access_type").map(String::as_str), Some("offline"));
        assert_eq!(q.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("https://signup.passband.email/oauth/callback")
        );
        // The verifier is kept, and the URL carries only its S256 challenge.
        assert!(!c.pkce_verifier.is_empty());
        assert!(!c.url.contains(&c.pkce_verifier));
    }

    #[test]
    fn each_consent_gets_its_own_verifier() {
        let a = consent_url(&endpoints(), "a".into(), Flow::Signup).unwrap();
        let b = consent_url(&endpoints(), "b".into(), Flow::Signup).unwrap();
        assert_ne!(a.pkce_verifier, b.pkce_verifier);
    }

    /// A console login is an AUTHENTICATION, and its consent URL has to say so:
    /// identity scopes only, no Gmail scope of any kind, and no `access_type`,
    /// so Google issues no refresh token for a flow that would have nowhere to
    /// put one.
    #[test]
    fn a_console_consent_asks_only_who_is_signed_in() {
        let c = consent_url(&endpoints(), "the-state".into(), Flow::Console).unwrap();
        let url = url::Url::parse(&c.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        let scope = q.get("scope").map(String::as_str).unwrap_or_default();
        assert_eq!(scope.split(' ').collect::<Vec<_>>(), vec!["openid", "email"]);
        assert!(!scope.contains("gmail"), "{scope}");
        assert_eq!(q.get("access_type"), None, "{scope}");
        assert_eq!(q.get("prompt").map(String::as_str), Some("select_account"));
        assert_eq!(q.get("state").map(String::as_str), Some("the-state"));
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert!(!c.url.contains(&c.pkce_verifier));
    }

    fn granted(scopes: &[&str]) -> Vec<String> {
        scopes.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_partial_consent_is_fatal() {
        let signup_scopes = requested_scopes(Flow::Signup);
        // No `scope` field means "exactly what was requested" (RFC 6749 §5.1).
        assert!(check_scope_grant(None, Flow::Signup).is_ok());
        assert!(check_scope_grant(Some(&granted(&signup_scopes)), Flow::Signup).is_ok());
        // The union case: more than we asked for still covers what we asked for.
        let mut unioned = granted(&signup_scopes);
        unioned.push(EMAIL_SCOPE_URL.to_string());
        assert!(check_scope_grant(Some(&unioned), Flow::Signup).is_ok());

        // Every way to grant LESS, including each single box a user can uncheck
        // on Google's screen. Read alone used to pass and now must not: it
        // provisions a tenant that cannot archive, label, or send.
        assert!(check_scope_grant(Some(&[]), Flow::Signup).is_err());
        assert!(check_scope_grant(Some(&granted(&["openid"])), Flow::Signup).is_err());
        for dropped in &signup_scopes {
            let partial: Vec<String> = signup_scopes
                .iter()
                .filter(|s| *s != dropped)
                .map(|s| s.to_string())
                .collect();
            assert!(
                check_scope_grant(Some(&partial), Flow::Signup).is_err(),
                "a consent missing {dropped} must be refused"
            );
        }
    }

    /// The console flow's only evidence is the userinfo answer, so an address it
    /// does not vouch for is not an identity. ABSENT IS NOT VERIFIED: a missing
    /// claim used to pass, which meant anything answering as the userinfo
    /// endpoint could hand over a mailbox by leaving one field out.
    #[test]
    fn an_unvouched_userinfo_address_is_refused() {
        let ok = |body: &str| email_from_answer(body.as_bytes(), Verification::Required);

        // Both spellings of PRESENT AND TRUE, which is what Google actually
        // ships: the JSON boolean and the string.
        assert_eq!(
            ok(r#"{"email":"ada@example.com","email_verified":true}"#).unwrap(),
            "ada@example.com"
        );
        assert_eq!(
            ok(r#"{"email":"ada@example.com","email_verified":"true"}"#).unwrap(),
            "ada@example.com"
        );
        assert_eq!(
            ok(r#"{"email":"ada@example.com","email_verified":"TRUE"}"#).unwrap(),
            "ada@example.com"
        );

        for refused in [
            // Absent. The case this test exists for.
            r#"{"email":"ada@example.com"}"#,
            r#"{"email":"ada@example.com","email_verified":false}"#,
            r#"{"email":"ada@example.com","email_verified":"false"}"#,
            r#"{"email":"ada@example.com","email_verified":null}"#,
            r#"{"email":"ada@example.com","email_verified":"yes"}"#,
            r#"{"email":"ada@example.com","email_verified":1}"#,
        ] {
            assert!(ok(refused).is_err(), "{refused}");
        }
    }

    /// Gmail's profile endpoint does not carry the claim at all, and a signup's
    /// mailbox is named by the grant the user just approved, so a missing claim
    /// there is not a refusal. A volunteered `false` still is.
    #[test]
    fn the_gmail_profile_endpoint_is_read_without_a_verified_claim() {
        let read = |body: &str| email_from_answer(body.as_bytes(), Verification::NotClaimed);
        assert_eq!(
            read(r#"{"emailAddress":"ada@example.com"}"#).unwrap(),
            "ada@example.com"
        );
        assert!(read(r#"{"emailAddress":"ada@example.com","email_verified":false}"#).is_err());
        assert!(read(r#"{"emailAddress":"ada@example.com","email_verified":"false"}"#).is_err());
    }

    /// Whatever the verification says, what comes back has to be shaped like a
    /// mailbox: this string becomes a database key and a line on a page.
    #[test]
    fn an_answer_that_is_not_a_mailbox_is_refused() {
        for body in [
            r#"{"email":"","email_verified":true}"#,
            r#"{"email":"not-an-address","email_verified":true}"#,
            r#"{"email":"ada lovelace@example.com","email_verified":true}"#,
            r#"{"email":"ada\nlovelace@example.com","email_verified":true}"#,
            r#"{"email_verified":true}"#,
            r#"not json"#,
        ] {
            assert!(
                email_from_answer(body.as_bytes(), Verification::Required).is_err(),
                "{body}"
            );
        }
    }

    /// Google takes `email` on the request and reports the userinfo URL on the
    /// answer. Both spellings satisfy the identity requirement, because a check
    /// that knew only one of them would refuse every real console login.
    #[test]
    fn a_console_grant_is_accepted_in_either_spelling_google_uses() {
        for reported in [
            granted(&["openid", EMAIL_SCOPE]),
            granted(&["openid", EMAIL_SCOPE_URL]),
            granted(&["openid", EMAIL_SCOPE_URL, "profile"]),
        ] {
            assert!(
                check_scope_grant(Some(&reported), Flow::Console).is_ok(),
                "{reported:?}"
            );
        }
        assert!(check_scope_grant(None, Flow::Console).is_ok());

        // Identity is the whole ask, so anything short of it is refused, and a
        // pile of Gmail scopes with no `openid` is not an authentication.
        for short in [
            granted(&[]),
            granted(&["openid"]),
            granted(&[EMAIL_SCOPE_URL]),
            granted(&requested_scopes(Flow::Signup)),
        ] {
            assert!(
                check_scope_grant(Some(&short), Flow::Console).is_err(),
                "{short:?}"
            );
        }
    }
}
