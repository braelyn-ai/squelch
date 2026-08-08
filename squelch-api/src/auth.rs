//! Bearer auth for the human door: every `/client/*` route except the pairing
//! claim goes through [`require_bearer`].
//!
//! TWO CREDENTIALS, IN THIS ORDER, and the order is the contract:
//!
//! 1. `SQUELCH_API_TOKEN`, the self-host MASTER token, compared in constant time
//!    so a timing side channel cannot leak it prefix-by-prefix. It is checked
//!    first, it is never deprecated, and it works with an empty `device_tokens`
//!    table — it is the way back in after revoking the last device.
//! 2. An issued PER-DEVICE token, verified by hashing what was presented and
//!    looking the hash up. Named, revocable, and effective the instant it is
//!    revoked because nothing here caches (see
//!    [`SqliteStore::verify_device_token`](squelch_core::store::SqliteStore::verify_device_token)).
//!
//! FAIL-CLOSED EVERYWHERE. A missing header, an unparseable one, an unknown or
//! revoked token, and a store that could not answer all end the request; none of
//! them reaches a handler. The response is a bare status with no body, and
//! nothing about the presented token is logged or echoed.
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

/// Device-token verifications allowed to be in the store at once, matching
/// [`crate::tracking::PIXEL_CONCURRENCY`] and [`crate::pair::PAIR_CONCURRENCY`].
///
/// The master-token compare above it is process-local and stays unbounded, but
/// ANY caller can put an `sqd_`-shaped guess into the branch below it, and each
/// guess takes the store mutex the whole daemon shares. Waiting for a slot is
/// invisible from outside: the answer is the same 401 either way.
pub(crate) const DEVICE_AUTH_CONCURRENCY: usize = 4;

/// Whether a presented token authenticates, master first then device tokens.
///
/// `Err` is reserved for the store failing to answer, which is an operator
/// problem rather than a verdict on the credential; it is deliberately NOT
/// folded into "no", so a broken store cannot look like a wrong token in the
/// logs the operator reads next.
async fn authenticates(state: &ApiState, presented: String) -> Result<bool, StatusCode> {
    // The master token is a process-local string compare: no store round trip,
    // so the operator's way in survives a store that is misbehaving.
    if let Some(master) = state.master_token()
        && ct_eq(presented.as_bytes(), master.as_bytes())
    {
        return Ok(true);
    }

    // Past this point an unauthenticated guess reaches the store, so the branch
    // is bounded; see [`DEVICE_AUTH_CONCURRENCY`]. The permit covers the lookup
    // and is dropped with this call, well before the request runs a handler.
    let _permit = state
        .device_auth_slots()
        .acquire()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // One point lookup per request, on the blocking pool because it takes the
    // store mutex. Uncached BY DESIGN: `squelchd token revoke` has to bite on
    // the very next request, and there is no invalidation scheme that is cheaper
    // to get right than a hash lookup on a UNIQUE index.
    let store = state.store.clone();
    let verified = tokio::task::spawn_blocking(move || store.verify_device_token(&presented))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(verified.is_some())
}

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
        .and_then(parse_bearer)
        // Owned so the verification can cross the blocking pool; the borrow of
        // `req` ends here, before `next.run` takes it.
        .map(str::to_string)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if authenticates(&state, presented).await? {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
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
    use squelch_core::types::AccountId;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "test-secret-token";

    /// One bare route behind the layer: this proves the WIRING (401 vs pass
    /// through), not the compare — that is tested once in `squelch_httpauth`.
    fn app_with(state: ApiState) -> Router {
        Router::new()
            .route("/gated", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_bearer,
            ))
            .with_state(state)
    }

    /// A store + account + state over them, with `master` as the configured
    /// `SQUELCH_API_TOKEN` equivalent (`""` for "no master token configured").
    fn fixture(master: &str) -> (Arc<SqliteStore>, AccountId, ApiState) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let state = ApiState::new(store.clone(), acct, master);
        (store, acct, state)
    }

    fn app() -> Router {
        app_with(fixture(TOKEN).2)
    }

    async fn status(app: Router, header_value: Option<&str>) -> StatusCode {
        let mut req = Request::builder().uri("/gated");
        if let Some(h) = header_value {
            req = req.header(header::AUTHORIZATION, h);
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn gates_on_the_bearer_token() {
        for (name, header_value) in [
            ("no header", None),
            ("wrong token", Some("Bearer nope")),
            ("wrong scheme", Some("Basic test-secret-token")),
            ("empty token", Some("Bearer ")),
            // Shaped like an issued token but never issued: the device-token
            // branch must refuse it exactly as flatly as anything else.
            ("unissued device token", Some("Bearer sqd_not-a-real-token")),
        ] {
            assert_eq!(
                status(app(), header_value).await,
                StatusCode::UNAUTHORIZED,
                "{name}"
            );
        }

        assert_eq!(
            status(app(), Some(&format!("bearer {TOKEN}"))).await,
            StatusCode::OK
        );
    }

    /// The whole point of the slice: a token minted into the store authenticates
    /// without ever being configured anywhere, and stops the moment it is
    /// revoked, with no restart and no cache to wait out.
    #[tokio::test]
    async fn device_tokens_pass_until_revoked() {
        let (store, acct, state) = fixture(TOKEN);
        let issued = store.issue_device_token(acct, "laptop").unwrap();
        let header_value = format!("Bearer {}", issued.token);

        assert_eq!(
            status(app_with(state.clone()), Some(&header_value)).await,
            StatusCode::OK,
            "an issued device token authenticates"
        );
        // The master token is unaffected by a device token existing.
        assert_eq!(
            status(app_with(state.clone()), Some(&format!("Bearer {TOKEN}"))).await,
            StatusCode::OK
        );

        assert!(store.revoke_device_token(acct, issued.id).unwrap());
        assert_eq!(
            status(app_with(state), Some(&header_value)).await,
            StatusCode::UNAUTHORIZED,
            "revocation bites on the next request"
        );
    }

    /// A door with no `SQUELCH_API_TOKEN` still SERVES; it just has nothing to
    /// accept yet. This is the hosted posture, and the state a self-host is in
    /// between first boot and its first pairing.
    #[tokio::test]
    async fn serves_without_a_master_token() {
        let (store, acct, state) = fixture("");

        for (name, header_value) in [
            ("no header", None),
            ("any token at all", Some("Bearer anything")),
            // The blank master must not be matchable by a blank-ish bearer.
            ("whitespace bearer", Some("Bearer    ")),
        ] {
            assert_eq!(
                status(app_with(state.clone()), header_value).await,
                StatusCode::UNAUTHORIZED,
                "{name}"
            );
        }

        let issued = store.issue_device_token(acct, "phone").unwrap();
        assert_eq!(
            status(app_with(state), Some(&format!("Bearer {}", issued.token))).await,
            StatusCode::OK,
            "a device token is the way in when no master token is configured"
        );
    }

    /// A whitespace-only `SQUELCH_API_TOKEN` is the same as no token: an
    /// exported-but-empty variable must never become a credential.
    #[tokio::test]
    async fn blank_master_token_is_no_master_token() {
        let (_, _, state) = fixture("   ");
        assert!(state.master_token().is_none());
        assert_eq!(
            status(app_with(state), Some("Bearer    ")).await,
            StatusCode::UNAUTHORIZED
        );
    }
}
