//! Bearer-token auth for the human door: every `/client/*` route goes through
//! [`require_bearer`]. The expected token is non-empty by construction (the
//! state refuses to build otherwise), comparison is CONSTANT TIME so a timing
//! side channel cannot leak it prefix-by-prefix, and a missing/bad header is a
//! bare 401 that never echoes or logs the token.
//!
//! The compare and the header parse are shared with the relay in
//! [`squelch_httpauth`] — one copy, one test suite, one place a fix lands.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};
use squelch_httpauth::{ct_eq, parse_bearer};

use crate::state::ApiState;

/// Middleware: require a valid bearer token or return 401.
pub async fn require_bearer(
    State(state): State<ApiState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer);

    match presented {
        Some(tok) if ct_eq(tok.as_bytes(), state.token.as_bytes()) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
        middleware,
        routing::get,
    };
    use squelch_core::store::SqliteStore;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "test-secret-token";

    /// One bare route behind the layer: this proves the WIRING (401 vs pass
    /// through), not the compare — that is tested once in `squelch_httpauth`.
    fn app() -> Router {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let state = ApiState::new(store, acct, TOKEN).unwrap();
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
            ("wrong scheme", Some("Basic test-secret-token")),
            ("empty token", Some("Bearer ")),
        ] {
            let mut req = Request::builder().uri("/gated");
            if let Some(h) = header_value {
                req = req.header(header::AUTHORIZATION, h);
            }
            let resp = app()
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
        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
