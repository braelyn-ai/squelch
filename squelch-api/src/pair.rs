//! `POST /client/pair` — trading a pairing code for a device token.
//!
//! THE ONLY `/client/*` ROUTE SERVED WITHOUT A BEARER, and it has to be: it is
//! how a device that has no credential gets its first one. `squelchd pair`
//! prints a short code, the user carries it to the new device, and the claim
//! here mints a named, revocable token for it.
//!
//! Everything about this file follows from being unauthenticated:
//!
//! - EVERY failure is a bare 401 with no body. A wrong code, an expired one, one
//!   already claimed, one burned by too many misses, a malformed request, an
//!   empty device name, and a store that could not answer are one answer. There
//!   is nothing here to probe with, which is what makes an 8-character code
//!   defensible;
//! - the code and the issued token appear in NOTHING but the request and the
//!   response: no log line, no audit row (the store audits the token by id and
//!   label), no error string. The response body is the one and only time the
//!   plaintext token exists on the wire, and the store cannot reproduce it;
//! - the account is not a parameter. The code IS the account claim, and the
//!   store resolves it from the row it matched;
//! - there is NO CORS layer on this router, unlike the `/client/*` tree. A
//!   pairing code is claimable by anything that can reach the daemon, so the
//!   browser's default refusal to let a random page POST here cross-origin is a
//!   property worth keeping, not a papercut to smooth over. Asserted in the
//!   tests, not just asserted here;
//! - the store work is bounded ([`PAIR_CONCURRENCY`]), because this is the
//!   second route that reaches the daemon-wide store mutex with no credential in
//!   front of it.
//!
//! A miss is charged against the live code (the store burns it after a handful),
//! so this route is never retried automatically by any client we ship.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};

use crate::state::ApiState;

/// Ceiling on a claim body. A code is 8 characters and a device name is capped
/// at 100 by the store, so this is orders of magnitude of headroom and still
/// small enough that an unauthenticated caller cannot make the daemon buffer
/// anything interesting. An oversized body is refused as a rejection, which this
/// module turns into the same 401 as everything else.
const MAX_BODY_BYTES: usize = 4096;

/// Claims allowed to be in the store at once, the same bound and the same reason
/// as [`crate::tracking::PIXEL_CONCURRENCY`]: this is the second unauthenticated
/// route onto the store mutex that `/client/*`, `/mcp` and the sync engine all
/// wait on, so a flood is capped here rather than allowed to serialize the whole
/// daemon.
///
/// The pixel BAILS OUT when its slots are full, because it can answer with its
/// one image without touching the store. This route cannot: an early refusal
/// would be a response that differs from the uniform 401 depending on how busy
/// the daemon is, and that difference is a signal. So a claim WAITS for its slot
/// instead, and a flood buys nothing but a queue.
pub(crate) const PAIR_CONCURRENCY: usize = 4;

/// The claim. Field names are the wire contract Passband encodes.
#[derive(Deserialize)]
struct PairRequest {
    /// Display or normalized form; the store folds case, dashes and whitespace
    /// before hashing, so `xxxx-xxxx` and `XXXXXXXX` are one credential.
    code: String,
    /// The label this device carries in `squelchd token list`, so the operator
    /// can tell which row to revoke later.
    device_name: String,
}

/// The mint. `token` is the ONLY copy that will ever exist: the daemon keeps a
/// hash and cannot reissue it.
#[derive(Serialize)]
struct PairResponse {
    token: String,
    device_id: i64,
    name: String,
}

/// The one answer to every way a claim can fail. No body, no headers, nothing
/// that separates "wrong" from "expired" from "the store is down".
fn refused() -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

/// The pairing router, merged OUTSIDE the bearer layer in [`crate::router`].
/// One route, and nothing else is ever added to it — the same discipline as
/// [`crate::tracking::pixel_router`], so the auth boundary stays a line you can
/// see in `lib.rs`.
pub fn pair_router(state: ApiState) -> Router {
    Router::new()
        .route("/client/pair", post(claim))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// `POST /client/pair` — burn a pairing code, mint a device token.
///
/// The body is extracted as a `Result` rather than by the plain `Json`
/// extractor: axum's own rejection is a 400 that says what was wrong with the
/// JSON, and on this route "what was wrong" is exactly the thing that must not
/// be answerable.
async fn claim(
    State(state): State<ApiState>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return refused();
    };

    // Held across the store call and dropped with it; see [`PAIR_CONCURRENCY`]
    // for why this waits rather than refusing. `Err` is a closed semaphore,
    // which nothing here closes, and it fails closed like everything else.
    let Ok(_permit) = state.pair_slots().acquire().await else {
        return refused();
    };

    // The store owns the whole decision: match, expiry, one-shot burn, the
    // failed-attempt penalty, the name check, and the mint, all in one
    // transaction. Nothing is pre-validated here, because a cheap local reject
    // (a wrong-length code, say) would be a free guess that never cost the
    // attacker an attempt against the live code.
    let store = state.store.clone();
    let issued = tokio::task::spawn_blocking(move || {
        store.claim_pairing_code(&body.code, &body.device_name)
    })
    .await;

    match issued {
        Ok(Ok(issued)) => Json(PairResponse {
            token: issued.token,
            device_id: issued.id,
            name: issued.name,
        })
        .into_response(),
        // A store error and a wrong code are the same 401. Fail-closed: there is
        // no path from here that runs a handler or reports a cause.
        _ => refused(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, header};
    use chrono::Duration;
    use http_body_util::BodyExt as _;
    use squelch_core::store::SqliteStore;
    use squelch_core::store::sqlite::device_tokens::PAIRING_TTL_SECS;
    use squelch_core::types::AccountId;
    use tower::ServiceExt as _;

    use super::*;

    /// The full router, so these tests exercise the real mounting: `/client/pair`
    /// outside the bearer and everything else inside it.
    fn fixture() -> (Arc<SqliteStore>, AccountId, Router) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        // No master token: pairing is the only way in, which is the posture this
        // route exists for.
        let state = ApiState::new(store.clone(), acct, "");
        (store, acct, crate::router(state))
    }

    fn claim_request(code: &str, device_name: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/client/pair")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "code": code, "device_name": device_name }).to_string(),
            ))
            .unwrap()
    }

    fn ttl() -> Duration {
        Duration::seconds(PAIRING_TTL_SECS)
    }

    /// A claimed code yields a token that the bearer-gated tree accepts, which is
    /// the whole loop: no credential -> pairing -> credential.
    #[tokio::test]
    async fn a_claim_mints_a_working_token() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        let resp = app
            .clone()
            .oneshot(claim_request(&minted.code, "  Braelyn's MacBook  "))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();

        let token = body["token"].as_str().unwrap().to_string();
        assert!(token.starts_with("sqd_"), "the wire carries the plaintext");
        assert!(body["device_id"].as_i64().unwrap() > 0);
        assert_eq!(body["name"], "Braelyn's MacBook", "the name is trimmed");

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "the paired token opens the human door"
        );
    }

    /// The dashes are cosmetic and the case is not significant, because a user
    /// retypes what they read off a screen.
    #[tokio::test]
    async fn the_displayed_form_and_the_normalized_form_are_one_credential() {
        for transform in [
            |c: &str| c.to_string(),
            |c: &str| c.replace('-', ""),
            |c: &str| c.to_lowercase(),
            |c: &str| format!(" {} ", c.replace('-', " ")),
        ] {
            let (store, acct, app) = fixture();
            let minted = store.mint_pairing_code(acct, ttl()).unwrap();
            let resp = app
                .oneshot(claim_request(&transform(&minted.code), "phone"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    /// Every refusal is byte-identical: same status, empty body. The codes below
    /// fail for four different reasons and the wire cannot tell them apart.
    #[tokio::test]
    async fn every_failure_is_the_same_bare_401() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        // Already claimed: one-shot, so the second attempt is a stranger's.
        let resp = app
            .clone()
            .oneshot(claim_request(&minted.code, "first"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let expired = store
            .mint_pairing_code(acct, Duration::seconds(-1))
            .unwrap();

        for (name, req) in [
            ("replayed code", claim_request(&minted.code, "second")),
            ("wrong code", claim_request("AAAA-AAAA", "phone")),
            ("expired code", claim_request(&expired.code, "phone")),
            ("empty device name", claim_request(&expired.code, "   ")),
            ("malformed shape", claim_request("", "")),
            (
                "not json at all",
                Request::builder()
                    .method("POST")
                    .uri("/client/pair")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            ),
            (
                "no content type",
                Request::builder()
                    .method("POST")
                    .uri("/client/pair")
                    .body(Body::from("{}"))
                    .unwrap(),
            ),
        ] {
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{name}");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert!(body.is_empty(), "{name} leaked a body");
        }
    }

    /// Guessing costs the user's code, not nothing: after the store's attempt
    /// budget the live code is gone, and the RIGHT code then fails exactly like a
    /// wrong one.
    #[tokio::test]
    async fn a_burned_code_stops_working() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        for _ in 0..5 {
            let resp = app
                .clone()
                .oneshot(claim_request("ZZZZ-ZZZZ", "phone"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        let resp = app
            .oneshot(claim_request(&minted.code, "phone"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "the burned code is as dead as a wrong one"
        );
    }

    /// The concurrency bound must not turn into an oracle. A burst of claims of
    /// the SAME code, more of them at once than there are slots, still answers
    /// exactly the way a serial burst does: one OK (the claim is one-shot) and
    /// bare, empty 401s for the losers. Nothing reports "busy".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_burst_of_claims_queues_instead_of_answering_differently() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..(PAIR_CONCURRENCY * 4) {
            let app = app.clone();
            let code = minted.code.clone();
            set.spawn(async move {
                let resp = app.oneshot(claim_request(&code, "phone")).await.unwrap();
                let status = resp.status();
                let body = resp.into_body().collect().await.unwrap().to_bytes();
                (status, body.len())
            });
        }

        let mut ok = 0;
        while let Some(joined) = set.join_next().await {
            let (status, body_len) = joined.unwrap();
            match status {
                StatusCode::OK => ok += 1,
                StatusCode::UNAUTHORIZED => assert_eq!(body_len, 0, "a refusal leaked a body"),
                other => panic!("a claim under load answered {other}"),
            }
        }
        assert_eq!(ok, 1, "one code, one device");
    }

    /// NO CORS on this router, unlike the `/client/*` tree it sits beside. A page
    /// on the open web can POST here (nothing stops that), but the browser must
    /// refuse to let it READ the response, which is where the minted token would
    /// be. Proven against the gated sibling, where the CORS layer does answer.
    #[tokio::test]
    async fn the_pairing_route_answers_with_no_cors_headers() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        let mut req = claim_request(&minted.code, "phone");
        req.headers_mut().insert(
            header::ORIGIN,
            header::HeaderValue::from_static("https://evil.example"),
        );
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "the claim itself still works");
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "a token handed back cross-origin is a token a web page can steal"
        );

        // Same Origin, a route inside the bearer-gated tree: there the layer is
        // wired deliberately, so this is a difference in mounting, not an accident
        // of the test.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_some(),
            "the /client/* tree is the surface CORS is meant to answer for"
        );
    }

    /// The route is mounted OUTSIDE the bearer: reaching it without a token must
    /// be a decision about the code, never a 401 from the auth layer. Proven by
    /// the negative — a gated sibling 401s with no header while `/client/pair`
    /// gets far enough to answer on the code's merits.
    #[tokio::test]
    async fn the_route_is_reachable_without_a_bearer() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "gated sibling");

        let resp = app
            .oneshot(claim_request(&minted.code, "phone"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "pair needs no bearer");
    }
}
