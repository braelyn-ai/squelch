//! Optional bearer auth for `POST /v1/push`. The token is OPTIONAL — a
//! deployment may serve open-but-rate-limited — and that choice is made once at
//! router construction: this layer is mounted only when a token is configured.
//!
//! `Authorization: Bearer <token>` is compared in CONSTANT TIME, so a timing
//! side channel cannot leak it prefix-by-prefix. Any failure is a bare 401; the
//! token is never logged or echoed.
//!
//! The compare and the header parse are shared with the human door in
//! [`squelch_httpauth`] — one copy, one test suite, one place a fix lands. That
//! crate's only dependency is `subtle`, so sharing them does not link the mail
//! core into this blind, publicly-hosted binary.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};
use squelch_httpauth::{ct_eq, parse_bearer};

use crate::state::RelayState;

/// Middleware: require a valid bearer token or return 401. Only mounted when
/// `SQUELCH_RELAY_AUTH_TOKEN` is configured.
pub async fn require_bearer(
    State(state): State<RelayState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = state.auth_token() else {
        // Unreachable in practice: the layer is not mounted without a token.
        // Fail CLOSED anyway rather than inventing an open path.
        return Err(StatusCode::UNAUTHORIZED);
    };

    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer);

    match presented {
        Some(tok) if ct_eq(tok.as_bytes(), expected.as_bytes()) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
        middleware,
        routing::get,
    };
    use tower::ServiceExt;

    use crate::config::{Config, Environment};

    use super::*;

    const KEY: &str = include_str!("../tests/fixture_test_key.p8");
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// One bare route behind the layer: this proves the WIRING (401 vs pass
    /// through) and the fail-closed no-token branch, not the compare — that is
    /// tested once in `squelch_httpauth`.
    fn app(auth_token: Option<String>) -> Router {
        let state = RelayState::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            apns_key_pem: KEY.to_string(),
            apns_key_id: "KEYID12345".into(),
            apns_team_id: "TEAMID6789".into(),
            apns_topics: vec!["dev.squelch.ios".into()],
            apns_env: Environment::Production,
            auth_token,
            apns_url_override: None,
        })
        .unwrap();
        Router::new()
            .route("/gated", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_bearer,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn gates_on_the_bearer_token() {
        for (name, header_value) in [
            ("no header", None),
            ("wrong token", Some("Bearer nope")),
            (
                "wrong scheme",
                Some("Basic 0123456789abcdef0123456789abcdef"),
            ),
            ("empty token", Some("Bearer ")),
        ] {
            let mut req = Request::builder().uri("/gated");
            if let Some(h) = header_value {
                req = req.header(header::AUTHORIZATION, h);
            }
            let resp = app(Some(TOKEN.into()))
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{name}");
        }

        let req = Request::builder()
            .uri("/gated")
            .header(header::AUTHORIZATION, format!("bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let resp = app(Some(TOKEN.into())).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Mounted without a configured token, the layer fails CLOSED.
    #[tokio::test]
    async fn no_configured_token_fails_closed() {
        let req = Request::builder()
            .uri("/gated")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let resp = app(None).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
