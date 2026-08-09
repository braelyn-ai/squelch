//! The confidential-client half of Google OAuth: build a consent URL, redeem
//! the authorization code, and ask Google which mailbox the resulting token
//! opens.
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
use squelch_core::config::GMAIL_READONLY_SCOPE;
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
    /// a warning: a daemon that cannot read the mailbox is not a daemon.
    #[error("the Google consent did not include read access to Gmail")]
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
    pub profile_url: &'a str,
    pub timeout: Duration,
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

/// Build the consent URL for one signup.
///
/// `gmail.readonly` ONLY. The hosted MVP never asks for write or send at
/// signup: the daemon syncs and triages with the read credential, and the
/// action scopes are a separate, later, opt-in grant. Asking for them here
/// would put "send mail as you" on the first screen a user ever sees.
///
/// `access_type=offline` + `prompt=consent` are what make Google return a
/// refresh token, which is the entire point of the exchange: an access token
/// alone dies in an hour.
pub fn consent_url(e: &GoogleEndpoints<'_>, state: String) -> Result<Consent, OAuthError> {
    let oauth = client(e)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf) = oauth
        .authorize_url(|| CsrfToken::new(state))
        .add_scope(Scope::new(GMAIL_READONLY_SCOPE.to_string()))
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(challenge)
        .url();
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
    check_scope_grant(granted.as_deref())?;

    let access_token = response.access_token().secret().to_string();
    let refresh_token = response.refresh_token().map(|r| r.secret().to_string());
    // A grant with no refresh token provisions a daemon that dies within the
    // hour, long after anyone would connect the failure back to this signup.
    // Refused here, before a tenant exists.
    if refresh_token.is_none() {
        return Err(OAuthError::NoRefreshToken);
    }

    let account_email = fetch_profile_email(&http, e.profile_url, &access_token).await?;

    Ok(ExchangedGrant {
        token: StoredToken::from_response(access_token, refresh_token, response.expires_in()),
        account_email,
    })
}

/// Hold the granted scopes against what signup asked for.
///
/// A SUBSET FLOOR, not an exact match, and that is not laziness: Google unions
/// grants across a Cloud project, so a user who has previously granted more to
/// another client of the same project gets a token that reports more than
/// `gmail.readonly`. What must never happen is LESS, which is the case this
/// catches. An absent `scope` field means "same as the request" per RFC 6749
/// section 5.1, so it passes.
fn check_scope_grant(granted: Option<&[String]>) -> Result<(), OAuthError> {
    match granted {
        None => Ok(()),
        Some(scopes) if scopes.iter().any(|s| s == GMAIL_READONLY_SCOPE) => Ok(()),
        Some(_) => Err(OAuthError::Scope),
    }
}

#[derive(serde::Deserialize)]
struct GmailProfile {
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

/// Ask Gmail which mailbox an access token opens. The one call every scope set
/// this service mints permits.
async fn fetch_profile_email(
    http: &reqwest::Client,
    profile_url: &str,
    access_token: &str,
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
    let profile: GmailProfile = serde_json::from_slice(&body).map_err(|_| OAuthError::Profile)?;
    let email = profile.email_address.unwrap_or_default();
    let email = email.trim();
    // Shape, not validity: this string becomes a database key, an env-file
    // value on the VPS, and a line on a page. Anything with a control
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
            timeout: Duration::from_secs(5),
        }
    }

    /// The consent URL is the one thing a user sees before Google does, so
    /// every parameter on it is asserted: the scope (readonly ONLY), the PKCE
    /// method, the state we chose, and the refresh-token parameters.
    #[test]
    fn builds_a_consent_url_asking_for_readonly_with_pkce() {
        let c = consent_url(&endpoints(), "the-state".into()).unwrap();
        let url = url::Url::parse(&c.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(q.get("scope").map(String::as_str), Some(GMAIL_READONLY_SCOPE));
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
        // Write scopes are never requested at signup.
        assert!(!c.url.contains("gmail.modify"));
        assert!(!c.url.contains("gmail.send"));
    }

    #[test]
    fn each_consent_gets_its_own_verifier() {
        let a = consent_url(&endpoints(), "a".into()).unwrap();
        let b = consent_url(&endpoints(), "b".into()).unwrap();
        assert_ne!(a.pkce_verifier, b.pkce_verifier);
    }

    #[test]
    fn a_partial_consent_is_fatal() {
        assert!(check_scope_grant(None).is_ok());
        assert!(check_scope_grant(Some(&[GMAIL_READONLY_SCOPE.to_string()])).is_ok());
        // The union case: more than we asked for still covers what we asked for.
        assert!(
            check_scope_grant(Some(&[
                GMAIL_READONLY_SCOPE.to_string(),
                "https://www.googleapis.com/auth/gmail.send".to_string(),
            ]))
            .is_ok()
        );
        // Less is not.
        assert!(check_scope_grant(Some(&[])).is_err());
        assert!(check_scope_grant(Some(&["openid".to_string()])).is_err());
    }
}
