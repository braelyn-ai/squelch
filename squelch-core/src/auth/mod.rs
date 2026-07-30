//! Installed-app (Desktop) OAuth 2.0 for Gmail: PKCE consent URL -> loopback
//! redirect on `127.0.0.1` -> code exchange, with `state` verified as a CSRF token.
//! TWO-DOOR: one token per call, never both scope sets — [`AuthScopes::Read`] mints
//! [`GMAIL_READONLY_SCOPE`], [`AuthScopes::Write`] mints [`WRITE_SCOPES`] for
//! human-door actions. The code, tokens, and client secret are never logged.

use crate::config::{GMAIL_READONLY_SCOPE, OAuthClientConfig, WRITE_SCOPES};
use crate::credentials::{CredentialKind, StoredToken};
use crate::error::{CoreError, Result};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};
use std::io::{Read, Write};
use std::net::TcpListener;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Fixed loopback port used in `--headless` mode so it can be SSH-forwarded.
pub const DEFAULT_HEADLESS_PORT: u16 = 8847;

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

    let oauth = BasicClient::new(ClientId::new(client.client_id.clone()))
        .set_client_secret(ClientSecret::new(client.client_secret.clone()))
        .set_auth_uri(
            AuthUrl::new(GOOGLE_AUTH_URL.to_string()).map_err(map_oauth_err("bad auth url"))?,
        )
        .set_token_uri(
            TokenUrl::new(GOOGLE_TOKEN_URL.to_string()).map_err(map_oauth_err("bad token url"))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.clone()).map_err(map_oauth_err("bad redirect url"))?,
        );

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let mut req = oauth.authorize_url(CsrfToken::new_random);
    for scope in opts.scopes.scopes() {
        req = req.add_scope(Scope::new(scope));
    }
    let (auth_url, csrf_token) = req
        // Ask for a refresh token and force consent so we reliably get one.
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(pkce_challenge)
        .url();

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
}
