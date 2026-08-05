//! Strict validation of everything a stranger registers.
//!
//! The consent URL is the dangerous field: the broker renders it as a link on
//! its own domain, so anything less than an exact match on scheme, host, path,
//! and the OAuth parameters ships an open redirect wearing `passband.email`.
//! Every check here is an equality, never a "contains" or a suffix test, and
//! the parsing is done by a real URL parser because that is the only thing that
//! agrees with what a browser will do with the string.
//!
//! PRIVACY: no error message here ever echoes the value it rejected. These
//! strings go back on the wire and into a caller's log.

use std::collections::HashMap;

use url::Url;

/// A session id is 32 random bytes as unpadded base64url, which is exactly 43
/// characters. The length check IS the "decodes to 32 bytes" check, so the id
/// is never decoded: it is an opaque map key and a `state` value, never
/// material, and a decoder would only add a way to disagree with the daemon
/// about what the id was.
pub const SESSION_ID_LEN: usize = 43;

/// A SHA-256 as lowercase hex.
pub const CLAIM_TOKEN_HASH_LEN: usize = 64;

/// Consent URLs run long once scopes are listed, but not unbounded: this one
/// gets held in memory for the whole TTL and rendered into a page.
pub const MAX_AUTH_URL: usize = 4096;

/// The only host a registered consent URL may point at.
const GOOGLE_HOST: &str = "accounts.google.com";

/// The only path.
const GOOGLE_AUTH_PATH: &str = "/o/oauth2/v2/auth";

/// The scopes `squelchd` requests: `squelchd auth` asks for the first,
/// `squelchd auth --write` for the other two, and nothing asks for anything
/// else.
///
/// SOURCE OF TRUTH is squelch-core (`GMAIL_READONLY_SCOPE` and `WRITE_SCOPES`
/// in `squelch-core/src/config.rs`); they are restated here rather than
/// imported because the broker ships as its own image and must not pull the
/// daemon's crate (rusqlite, keyring, fastembed, an embedding model) in to
/// learn three strings. Self-hosters build their consent URL from those same
/// constants, so an allowlist costs no legitimate client anything.
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
pub const GMAIL_SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";

/// Every scope a registration may ask for. A consent URL requesting anything
/// outside this set is refused: the broker renders the request as a link on its
/// own domain, so without this a stranger can register
/// `https://mail.google.com/` (full mailbox control, deletion included) and we
/// will serve the page that asks a human to grant it.
pub const ALLOWED_SCOPES: &[&str] = &[GMAIL_READONLY_SCOPE, GMAIL_MODIFY_SCOPE, GMAIL_SEND_SCOPE];

/// Why a consent URL was refused. Each message is safe to hand back to the
/// caller: it names the constraint, never the value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthUrlError {
    #[error("auth_url is longer than {MAX_AUTH_URL} bytes")]
    TooLong,
    #[error("auth_url is not a valid absolute URL")]
    Unparseable,
    #[error("auth_url must use https")]
    Scheme,
    #[error("auth_url must not carry userinfo")]
    Userinfo,
    #[error("auth_url host must be exactly {GOOGLE_HOST}")]
    Host,
    #[error("auth_url must not specify a port")]
    Port,
    #[error("auth_url path must be exactly {GOOGLE_AUTH_PATH}")]
    Path,
    #[error("auth_url must not carry a fragment")]
    Fragment,
    #[error("auth_url repeats the `{0}` query parameter")]
    DuplicateParam(&'static str),
    #[error("auth_url `redirect_uri` must be exactly this broker's callback URL")]
    RedirectUri,
    #[error("auth_url `state` must be the session_id")]
    State,
    #[error("auth_url `response_type` must be `code`")]
    ResponseType,
    #[error("auth_url is missing a `code_challenge`")]
    CodeChallenge,
    #[error("auth_url `code_challenge_method` must be `S256`")]
    CodeChallengeMethod,
    #[error("auth_url is missing a `client_id`")]
    ClientId,
    #[error("auth_url is missing a `scope`")]
    Scope,
    #[error(
        "auth_url `scope` must be a subset of the scopes squelchd requests (gmail.readonly, gmail.modify, gmail.send)"
    )]
    ScopeNotAllowed,
}

/// The query parameters this broker has an opinion about. Anything else Google
/// accepts (`access_type`, `prompt`, `login_hint`) rides along untouched: the
/// daemon owns the rest of its own consent request.
const CHECKED_PARAMS: &[&str] = &[
    "redirect_uri",
    "state",
    "response_type",
    "code_challenge",
    "code_challenge_method",
    "client_id",
    "scope",
];

/// True when `s` is a well-formed session id.
pub fn is_session_id(s: &str) -> bool {
    s.len() == SESSION_ID_LEN && s.bytes().all(is_base64url)
}

/// Decode a registered `claim_token_hash`. Lowercase hex only, exactly 32
/// bytes: the contract says one spelling, and accepting a second would mean two
/// strings that name the same session's secret.
pub fn claim_token_hash(raw: &str) -> Option<[u8; 32]> {
    if raw.len() != CLAIM_TOKEN_HASH_LEN {
        return None;
    }
    let bytes = raw.as_bytes();
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = nibble(bytes[2 * i])?;
        let lo = nibble(bytes[2 * i + 1])?;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

/// Validate a registered consent URL against this broker's callback and the
/// session it belongs to.
pub fn auth_url(raw: &str, session_id: &str, callback_url: &str) -> Result<(), AuthUrlError> {
    if raw.len() > MAX_AUTH_URL {
        return Err(AuthUrlError::TooLong);
    }
    let url = Url::parse(raw).map_err(|_| AuthUrlError::Unparseable)?;

    if url.scheme() != "https" {
        return Err(AuthUrlError::Scheme);
    }
    // `https://accounts.google.com@evil.com/...` has host `evil.com`, so the
    // host check below already refuses it. Userinfo is refused on its own
    // anyway: it has no place in a consent URL, and reading a link whose
    // apparent host is not its real host is the whole trick.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AuthUrlError::Userinfo);
    }
    // Equality, not a suffix test: `accounts.google.com.evil.com` ends with
    // nothing this compares against.
    if url.host_str() != Some(GOOGLE_HOST) {
        return Err(AuthUrlError::Host);
    }
    // The parser reports `None` for a scheme's default port, so this refuses
    // only a genuinely redirected one.
    if url.port().is_some() {
        return Err(AuthUrlError::Port);
    }
    if url.path() != GOOGLE_AUTH_PATH {
        return Err(AuthUrlError::Path);
    }
    // A fragment never reaches Google's server, which is exactly why it is the
    // place to hide text in a link we publish.
    if url.fragment().is_some() {
        return Err(AuthUrlError::Fragment);
    }

    // A repeated parameter is an ambiguity: Google picks one, we would check
    // the other. Refuse rather than pick.
    let mut params: HashMap<String, String> = HashMap::new();
    for (k, v) in url.query_pairs() {
        if let Some(checked) = CHECKED_PARAMS.iter().find(|c| ***c == *k)
            && params.insert(k.to_string(), v.to_string()).is_some()
        {
            return Err(AuthUrlError::DuplicateParam(checked));
        }
    }
    let param = |name: &str| params.get(name).map(String::as_str).unwrap_or_default();

    if param("redirect_uri") != callback_url {
        return Err(AuthUrlError::RedirectUri);
    }
    if param("state") != session_id {
        return Err(AuthUrlError::State);
    }
    if param("response_type") != "code" {
        return Err(AuthUrlError::ResponseType);
    }
    // Present is all the broker can check: the challenge is a hash of a
    // verifier it must never see. Its absence, though, would mean a parked code
    // that anyone holding it could redeem, which is the invariant this whole
    // service rests on.
    if param("code_challenge").is_empty() {
        return Err(AuthUrlError::CodeChallenge);
    }
    // `plain` would make the challenge the verifier, which is the same failure
    // wearing a method name.
    if param("code_challenge_method") != "S256" {
        return Err(AuthUrlError::CodeChallengeMethod);
    }
    if param("client_id").is_empty() {
        return Err(AuthUrlError::ClientId);
    }
    // The scope is what the human is actually being asked to grant, and the
    // interstitial spells it out on our domain. A URL with no scope authorizes
    // nothing; a URL with a scope squelchd never asks for is a stranger's
    // consent request wearing our page.
    let scope = param("scope");
    let mut requested = scope.split_whitespace().peekable();
    if requested.peek().is_none() {
        return Err(AuthUrlError::Scope);
    }
    if !requested.all(|s| ALLOWED_SCOPES.contains(&s)) {
        return Err(AuthUrlError::ScopeNotAllowed);
    }
    Ok(())
}

/// The scopes a registered consent URL asks for, in the order it lists them.
///
/// Re-parsed from the URL rather than stored beside it, so the page cannot
/// describe one grant while the link requests another. Registration already
/// refused everything outside [`ALLOWED_SCOPES`], but the caller escapes what
/// it renders anyway: "this one was checked elsewhere" is how the exception
/// becomes the rule.
pub fn requested_scopes(auth_url: &str) -> Vec<String> {
    let Ok(url) = Url::parse(auth_url) else {
        return Vec::new();
    };
    url.query_pairs()
        .find(|(k, _)| k == "scope")
        .map(|(_, v)| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn is_base64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALLBACK: &str = "https://auth.passband.email/callback";
    const SID: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEF-";

    fn url_with(params: &[(&str, &str)]) -> String {
        let mut u = Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
        {
            let mut q = u.query_pairs_mut();
            for (k, v) in params {
                q.append_pair(k, v);
            }
        }
        u.to_string()
    }

    fn good_params() -> Vec<(&'static str, &'static str)> {
        vec![
            ("client_id", "1234.apps.googleusercontent.com"),
            ("redirect_uri", CALLBACK),
            ("response_type", "code"),
            ("state", SID),
            (
                "code_challenge",
                "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            ),
            ("code_challenge_method", "S256"),
            ("scope", "https://www.googleapis.com/auth/gmail.modify"),
            ("access_type", "offline"),
        ]
    }

    fn check(params: &[(&str, &str)]) -> Result<(), AuthUrlError> {
        auth_url(&url_with(params), SID, CALLBACK)
    }

    fn without(name: &str) -> Vec<(&'static str, &'static str)> {
        good_params()
            .into_iter()
            .filter(|(k, _)| *k != name)
            .collect()
    }

    fn replacing(name: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut p = without(name);
        p.push((name, value));
        p
    }

    #[test]
    fn accepts_a_real_consent_url() {
        assert_eq!(SID.len(), SESSION_ID_LEN);
        assert!(is_session_id(SID));
        check(&good_params()).unwrap();
    }

    /// Every one of these is a working open redirect if the host check is
    /// anything but an equality against the parsed host.
    #[test]
    fn refuses_every_shape_of_impostor_host() {
        for (name, raw, want) in [
            (
                "lookalike suffix",
                "https://accounts.google.com.evil.com/o/oauth2/v2/auth",
                AuthUrlError::Host,
            ),
            (
                "userinfo trick",
                "https://accounts.google.com@evil.com/o/oauth2/v2/auth",
                AuthUrlError::Userinfo,
            ),
            (
                "userinfo with a password",
                "https://accounts.google.com:x@evil.com/o/oauth2/v2/auth",
                AuthUrlError::Userinfo,
            ),
            (
                "userinfo on the real host",
                "https://user@accounts.google.com/o/oauth2/v2/auth",
                AuthUrlError::Userinfo,
            ),
            (
                "subdomain",
                "https://evil.accounts.google.com/o/oauth2/v2/auth",
                AuthUrlError::Host,
            ),
            (
                "bare evil host",
                "https://evil.com/o/oauth2/v2/auth",
                AuthUrlError::Host,
            ),
            (
                "trailing dot",
                "https://accounts.google.com./o/oauth2/v2/auth",
                AuthUrlError::Host,
            ),
            // An `@` smuggled past the userinfo split as `%40` is not a host
            // the parser will build at all, so this one never reaches the host
            // check.
            (
                "encoded at-sign",
                "https://accounts.google.com%40evil.com/o/oauth2/v2/auth",
                AuthUrlError::Unparseable,
            ),
            (
                "redirected port",
                "https://accounts.google.com:8443/o/oauth2/v2/auth",
                AuthUrlError::Port,
            ),
            (
                "plaintext",
                "http://accounts.google.com/o/oauth2/v2/auth",
                AuthUrlError::Scheme,
            ),
            (
                "javascript",
                "javascript:alert(1)//accounts.google.com/o/oauth2/v2/auth",
                AuthUrlError::Scheme,
            ),
            ("relative", "/o/oauth2/v2/auth", AuthUrlError::Unparseable),
        ] {
            // The query is well-formed in every case: only the origin is not.
            let full = format!(
                "{raw}?client_id=c&redirect_uri={CALLBACK}&response_type=code&state={SID}&code_challenge=x&code_challenge_method=S256"
            );
            assert_eq!(auth_url(&full, SID, CALLBACK), Err(want), "{name}");
        }
    }

    #[test]
    fn refuses_a_path_that_is_not_the_authorization_endpoint() {
        for path in [
            "/",
            "/o/oauth2/v2/auth/extra",
            "/o/oauth2/auth",
            "/o/oauth2/v2/%2e%2e/auth",
            "/o/oauth2/v2/AUTH",
        ] {
            let full = format!(
                "https://accounts.google.com{path}?client_id=c&redirect_uri={CALLBACK}&response_type=code&state={SID}&code_challenge=x&code_challenge_method=S256"
            );
            assert_eq!(
                auth_url(&full, SID, CALLBACK),
                Err(AuthUrlError::Path),
                "{path}"
            );
        }
        // Segment normalization is the parser's job, and a normalized path that
        // lands on the endpoint IS the endpoint.
        let normalized = format!(
            "https://accounts.google.com/o/oauth2/v3/../v2/auth?client_id=c&redirect_uri={CALLBACK}&response_type=code&state={SID}&code_challenge=x&code_challenge_method=S256&scope={GMAIL_READONLY_SCOPE}"
        );
        assert_eq!(auth_url(&normalized, SID, CALLBACK), Ok(()));
    }

    /// The redirect_uri is what makes the parked code reachable at all: a
    /// near-miss sends Google's code to someone else's listener.
    #[test]
    fn refuses_a_redirect_uri_that_is_not_this_callback() {
        for bad in [
            "https://auth.passband.email/callback/",
            "https://auth.passband.email/Callback",
            "https://auth.passband.email/callback?x=1",
            "https://auth.passband.email/callback#x",
            "https://auth.passband.email.evil.com/callback",
            "https://evil.com/callback",
            "http://auth.passband.email/callback",
            "https://auth.passband.email",
            "",
        ] {
            let mut params: Vec<(&str, &str)> = without("redirect_uri");
            params.push(("redirect_uri", bad));
            assert_eq!(check(&params), Err(AuthUrlError::RedirectUri), "{bad:?}");
        }
        assert_eq!(
            check(&without("redirect_uri")),
            Err(AuthUrlError::RedirectUri)
        );
    }

    #[test]
    fn requires_the_state_to_be_the_session_id() {
        assert_eq!(
            check(&replacing(
                "state",
                "0123456789abcdefghijklmnopqrstuvwxyzABCDEFX"
            )),
            Err(AuthUrlError::State)
        );
        assert_eq!(check(&without("state")), Err(AuthUrlError::State));
        // A prefix is not a match.
        let short = &SID[..SESSION_ID_LEN - 1];
        let mut p: Vec<(&str, &str)> = without("state");
        p.push(("state", short));
        assert_eq!(check(&p), Err(AuthUrlError::State));
    }

    #[test]
    fn requires_the_pkce_and_code_flow_parameters() {
        assert_eq!(
            check(&replacing("response_type", "token")),
            Err(AuthUrlError::ResponseType)
        );
        assert_eq!(
            check(&without("response_type")),
            Err(AuthUrlError::ResponseType)
        );
        assert_eq!(
            check(&without("code_challenge")),
            Err(AuthUrlError::CodeChallenge)
        );
        assert_eq!(
            check(&replacing("code_challenge", "")),
            Err(AuthUrlError::CodeChallenge)
        );
        // `plain` makes the challenge the verifier: the parked code would be
        // redeemable by whoever holds it.
        assert_eq!(
            check(&replacing("code_challenge_method", "plain")),
            Err(AuthUrlError::CodeChallengeMethod)
        );
        assert_eq!(
            check(&replacing("code_challenge_method", "s256")),
            Err(AuthUrlError::CodeChallengeMethod)
        );
        assert_eq!(
            check(&without("code_challenge_method")),
            Err(AuthUrlError::CodeChallengeMethod)
        );
        assert_eq!(check(&without("client_id")), Err(AuthUrlError::ClientId));
        assert_eq!(
            check(&replacing("client_id", "")),
            Err(AuthUrlError::ClientId)
        );
    }

    /// Google picks one of a repeated parameter and we would check the other.
    #[test]
    fn refuses_a_repeated_checked_parameter() {
        let mut p = good_params();
        p.push(("redirect_uri", "https://evil.com/callback"));
        assert_eq!(check(&p), Err(AuthUrlError::DuplicateParam("redirect_uri")));

        let mut p = good_params();
        p.push(("state", "something-else"));
        assert_eq!(check(&p), Err(AuthUrlError::DuplicateParam("state")));

        // The scope is checked, so it may not repeat either: Google would union
        // one reading of the pair and we would have rendered the other.
        let mut p = good_params();
        p.push(("scope", GMAIL_SEND_SCOPE));
        assert_eq!(check(&p), Err(AuthUrlError::DuplicateParam("scope")));

        // Parameters the broker has no opinion about may repeat: they are the
        // daemon's business and Google's.
        let mut p = good_params();
        p.push(("login_hint", "someone@example.com"));
        assert_eq!(check(&p), Ok(()));
    }

    /// Without this, a stranger registers a consent URL asking for anything
    /// Google will grant and the broker serves the page that asks for it.
    #[test]
    fn refuses_a_scope_squelchd_never_requests() {
        for bad in [
            // Full mailbox control, permanent deletion included.
            "https://mail.google.com/",
            "https://www.googleapis.com/auth/drive",
            "https://www.googleapis.com/auth/cloud-platform",
            "https://www.googleapis.com/auth/gmail.settings.basic",
            // A prefix of an allowed scope is not an allowed scope.
            "https://www.googleapis.com/auth/gmail",
            "https://www.googleapis.com/auth/gmail.readonly.evil",
            // Nor is one that merely contains it.
            "openid",
            // One good scope does not carry a bad one in with it.
            "https://www.googleapis.com/auth/gmail.readonly https://mail.google.com/",
        ] {
            assert_eq!(
                check(&replacing("scope", bad)),
                Err(AuthUrlError::ScopeNotAllowed),
                "{bad:?}"
            );
        }

        // Missing or empty authorizes nothing and renders as nothing.
        assert_eq!(check(&without("scope")), Err(AuthUrlError::Scope));
        assert_eq!(check(&replacing("scope", "")), Err(AuthUrlError::Scope));
        assert_eq!(check(&replacing("scope", "   ")), Err(AuthUrlError::Scope));
    }

    /// Exactly what `squelchd auth` and `squelchd auth --write` send.
    #[test]
    fn accepts_the_scope_sets_squelchd_asks_for() {
        for good in [
            GMAIL_READONLY_SCOPE,
            GMAIL_MODIFY_SCOPE,
            GMAIL_SEND_SCOPE,
            "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/gmail.send",
            // A `+`-encoded separator decodes to the same set.
            "https://www.googleapis.com/auth/gmail.send  https://www.googleapis.com/auth/gmail.modify",
        ] {
            assert_eq!(check(&replacing("scope", good)), Ok(()), "{good:?}");
        }
    }

    #[test]
    fn reads_back_the_scopes_the_page_will_render() {
        let url = url_with(&good_params());
        assert_eq!(requested_scopes(&url), vec![GMAIL_MODIFY_SCOPE.to_string()]);

        let two = url_with(&replacing(
            "scope",
            "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/gmail.send",
        ));
        assert_eq!(
            requested_scopes(&two),
            vec![GMAIL_MODIFY_SCOPE.to_string(), GMAIL_SEND_SCOPE.to_string()]
        );

        // Nothing to render is an empty list, never a panic.
        assert!(requested_scopes(&url_with(&without("scope"))).is_empty());
        assert!(requested_scopes("not a url").is_empty());
    }

    #[test]
    fn refuses_a_fragment_and_an_oversized_url() {
        let with_fragment = format!("{}#anything", url_with(&good_params()));
        assert_eq!(
            auth_url(&with_fragment, SID, CALLBACK),
            Err(AuthUrlError::Fragment)
        );

        let padding = "x".repeat(MAX_AUTH_URL);
        let mut p = good_params();
        p.push(("login_hint", &padding));
        assert_eq!(check(&p), Err(AuthUrlError::TooLong));
    }

    #[test]
    fn session_ids_are_exactly_thirty_two_base64url_bytes() {
        assert!(is_session_id(&"a".repeat(SESSION_ID_LEN)));
        assert!(is_session_id(&"-".repeat(SESSION_ID_LEN)));
        assert!(is_session_id(&"_".repeat(SESSION_ID_LEN)));

        assert!(!is_session_id(""));
        assert!(!is_session_id(&"a".repeat(SESSION_ID_LEN - 1)));
        assert!(!is_session_id(&"a".repeat(SESSION_ID_LEN + 1)));
        // Standard base64, padding, and anything that could reshape a URL or a
        // page are all out of the alphabet.
        for bad in ["+", "/", "=", ".", "%", " ", "<", "\n", "'"] {
            let s = format!("{}{bad}", "a".repeat(SESSION_ID_LEN - 1));
            assert!(!is_session_id(&s), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn claim_token_hashes_are_lowercase_hex() {
        let hex = "0123456789abcdef".repeat(4);
        let decoded = claim_token_hash(&hex).unwrap();
        assert_eq!(decoded[0], 0x01);
        assert_eq!(decoded[1], 0x23);
        assert_eq!(decoded[31], 0xef);
        assert_eq!(claim_token_hash(&"0".repeat(64)), Some([0u8; 32]));
        assert_eq!(claim_token_hash(&"f".repeat(64)), Some([0xffu8; 32]));

        // One spelling only: two would be two strings naming one secret.
        assert_eq!(claim_token_hash(&"A".repeat(64)), None);
        assert_eq!(claim_token_hash(&"0".repeat(63)), None);
        assert_eq!(claim_token_hash(&"0".repeat(65)), None);
        assert_eq!(claim_token_hash(""), None);
        assert_eq!(claim_token_hash(&"g".repeat(64)), None);
        assert_eq!(claim_token_hash(&"0 ".repeat(32)), None);
    }
}
