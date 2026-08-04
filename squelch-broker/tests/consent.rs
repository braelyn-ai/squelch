//! The consent flow end to end, driven through the router: register, show the
//! link, park the callback, claim once.
//!
//! The properties under test are the ones the whole service exists for. The
//! code goes out exactly once and to exactly one holder of the claim token; a
//! consent URL that is not Google's own authorization endpoint pointed back at
//! this broker is refused; and nothing but the daemon ever sees the code.

use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use squelch_broker::{BrokerState, Config, SESSION_TTL, router};
use tower::ServiceExt;

const PUBLIC_URL: &str = "https://auth.passband.email";
const CALLBACK: &str = "https://auth.passband.email/callback";
const CLAIM_TOKEN: &str = "sZmJ2QpVv1nUoGkYb3xLd8fRtWcE7hAy0iNqP4jTuM6";
const CODE: &str = "4/0AeanS0b-DEADBEEF_the-authorization-code";

fn state() -> BrokerState {
    BrokerState::new(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_url: PUBLIC_URL.into(),
        callback_url: CALLBACK.into(),
    })
}

fn app() -> axum::Router {
    router(state())
}

/// A well-formed session id: 43 base64url characters, which is 32 bytes.
fn sid(fill: char) -> String {
    std::iter::repeat_n(fill, 43).collect()
}

/// What the daemon registers: the hash, never the token.
fn hash_hex(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn consent_url(session_id: &str) -> String {
    format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id=1234.apps.googleusercontent.com\
         &redirect_uri={CALLBACK}&response_type=code&state={session_id}\
         &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256\
         &scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fgmail.modify&access_type=offline"
    )
}

fn registration(session_id: &str) -> Value {
    json!({
        "session_id": session_id,
        "claim_token_hash": hash_hex(CLAIM_TOKEN),
        "auth_url": consent_url(session_id),
    })
}

fn post(uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// `Instant` cannot be moved backwards portably (it is monotonic since boot, so
/// subtracting on a freshly booted machine underflows). Aging a session is
/// therefore a sweep against a future clock, which applies the same expiry
/// predicate the routes apply lazily on access.
fn age_past_the_ttl(state: &BrokerState) {
    assert_eq!(state.sessions().sweep(Instant::now() + SESSION_TTL), 1);
}

#[tokio::test]
async fn the_whole_consent_flow_hands_the_code_over_exactly_once() {
    let app = app();
    let id = sid('a');

    let resp = app
        .clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["expires_in"], SESSION_TTL.as_secs());

    // The interstitial forwards to the consent URL the daemon built, escaped
    // for an href and not otherwise altered.
    let resp = app
        .clone()
        .oneshot(get(&format!("/link?s={id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let html = body_text(resp).await;
    assert!(
        html.contains(&consent_url(&id).replace('&', "&amp;")),
        "{html}"
    );
    assert!(html.contains("cryptographically incapable of minting them"));

    // Polling before consent completes says so, without spending the session.
    let claim = json!({ "session_id": id, "claim_token": CLAIM_TOKEN });
    let resp = app
        .clone()
        .oneshot(post("/v1/claim", &claim))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await, json!({ "status": "pending" }));

    // Google's redirect parks the code. The page never shows it.
    let resp = app
        .clone()
        .oneshot(get(&format!("/callback?code={CODE}&state={id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("Authorized"));
    assert!(!html.contains(CODE), "the callback page rendered the code");

    let resp = app
        .clone()
        .oneshot(post("/v1/claim", &claim))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        json!({ "status": "complete", "code": CODE })
    );

    // One claim, and the session is gone with the code.
    let resp = app
        .clone()
        .oneshot(post("/v1/claim", &claim))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await, json!({ "status": "unknown" }));

    // And the link is spent too.
    let resp = app.oneshot(get(&format!("/link?s={id}"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A wrong token gets nothing and destroys nothing: guessing must not be a way
/// to delete someone else's pending consent.
#[tokio::test]
async fn a_wrong_claim_token_is_forbidden_and_the_session_survives() {
    let app = app();
    let id = sid('b');
    app.clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();
    app.clone()
        .oneshot(get(&format!("/callback?code={CODE}&state={id}")))
        .await
        .unwrap();

    for wrong in ["not-the-claim-token", CLAIM_TOKEN.trim_end_matches('6'), ""] {
        let resp = app
            .clone()
            .oneshot(post(
                "/v1/claim",
                &json!({ "session_id": id, "claim_token": wrong }),
            ))
            .await
            .unwrap();
        // An empty token is a malformed request; a wrong one is a refusal.
        // Neither may return the code, and neither may spend the session.
        let expected = if wrong.is_empty() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(resp.status(), expected, "{wrong:?}");
        let body = body_json(resp).await;
        assert!(body.get("code").is_none(), "{body}");
    }

    let resp = app
        .oneshot(post(
            "/v1/claim",
            &json!({ "session_id": id, "claim_token": CLAIM_TOKEN }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["code"], CODE);
}

/// A refusal is an outcome the daemon needs, not a failure to swallow.
#[tokio::test]
async fn a_declined_consent_is_parked_and_claimed_as_denied() {
    let app = app();
    let id = sid('c');
    app.clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(get(&format!("/callback?error=access_denied&state={id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_text(resp).await.contains("Consent declined"));

    let claim = json!({ "session_id": id, "claim_token": CLAIM_TOKEN });
    let resp = app
        .clone()
        .oneshot(post("/v1/claim", &claim))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        json!({ "status": "denied", "error": "access_denied" })
    );

    // Denial is terminal: the session went with it.
    let resp = app.oneshot(post("/v1/claim", &claim)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// First write wins, so a replayed redirect cannot swap the code a daemon is
/// about to claim for one an attacker holds.
#[tokio::test]
async fn a_replayed_callback_changes_nothing() {
    let app = app();
    let id = sid('d');
    app.clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();
    app.clone()
        .oneshot(get(&format!("/callback?code={CODE}&state={id}")))
        .await
        .unwrap();

    for query in [
        format!("code=an-attackers-code&state={id}"),
        format!("error=access_denied&state={id}"),
    ] {
        let resp = app
            .clone()
            .oneshot(get(&format!("/callback?{query}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert!(body_text(resp).await.contains("already been used"));
    }

    let resp = app
        .oneshot(post(
            "/v1/claim",
            &json!({ "session_id": id, "claim_token": CLAIM_TOKEN }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["code"], CODE);
}

/// Overwriting a live session would let anyone who guessed an id retarget a
/// consent already in flight.
#[tokio::test]
async fn a_duplicate_session_id_is_a_conflict() {
    let app = app();
    let id = sid('e');
    let resp = app
        .clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let mut second = registration(&id);
    second["claim_token_hash"] = json!(hash_hex("an-attackers-claim-token"));
    let resp = app
        .clone()
        .oneshot(post("/v1/sessions", &second))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // The original registration still owns the session.
    let resp = app
        .oneshot(post(
            "/v1/claim",
            &json!({ "session_id": id, "claim_token": CLAIM_TOKEN }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "pending");
}

/// Registration is the door an open redirect would come through.
#[tokio::test]
async fn a_consent_url_that_is_not_googles_is_refused() {
    let app = app();
    let id = sid('f');

    let bad_urls = [
        // The attacker's own host, in every dress.
        "https://evil.com/o/oauth2/v2/auth".to_string(),
        "https://accounts.google.com.evil.com/o/oauth2/v2/auth".to_string(),
        "https://accounts.google.com@evil.com/o/oauth2/v2/auth".to_string(),
        "http://accounts.google.com/o/oauth2/v2/auth".to_string(),
        "https://accounts.google.com/o/oauth2/v2/auth/../../../evil".to_string(),
        // Google's endpoint, pointed somewhere else.
        consent_url(&id).replace(CALLBACK, "https://evil.com/callback"),
        // Google's endpoint, bound to a different session.
        consent_url(&id).replace(&id, &sid('9')),
        // PKCE removed or downgraded, which would make the parked code usable
        // by whoever holds it.
        consent_url(&id).replace("&code_challenge_method=S256", ""),
        consent_url(&id).replace("code_challenge_method=S256", "code_challenge_method=plain"),
        consent_url(&id).replace("response_type=code", "response_type=token"),
        "not a url at all".to_string(),
    ];

    for auth_url in bad_urls {
        let mut body = registration(&id);
        body["auth_url"] = json!(auth_url);
        let resp = app
            .clone()
            .oneshot(post("/v1/sessions", &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{auth_url}");
        // The refusal names the constraint, never the value.
        let error = body_json(resp).await["error"].as_str().unwrap().to_string();
        assert!(!error.contains("evil.com"), "{error}");
    }

    // None of that registered anything, so the id is still free.
    let resp = app
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn malformed_registrations_and_claims_are_refused() {
    let app = app();
    let id = sid('g');

    let short_id = &id[..42];
    for body in [
        json!({}),
        json!({ "session_id": id, "claim_token_hash": hash_hex(CLAIM_TOKEN) }),
        json!({ "session_id": short_id, "claim_token_hash": hash_hex(CLAIM_TOKEN), "auth_url": consent_url(short_id) }),
        json!({ "session_id": id, "claim_token_hash": "not-hex", "auth_url": consent_url(&id) }),
        json!({ "session_id": id, "claim_token_hash": hash_hex(CLAIM_TOKEN).to_uppercase(), "auth_url": consent_url(&id) }),
    ] {
        let resp = app
            .clone()
            .oneshot(post("/v1/sessions", &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    }

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    for body in [
        json!({}),
        json!({ "session_id": id }),
        json!({ "session_id": short_id, "claim_token": CLAIM_TOKEN }),
        json!({ "session_id": id, "claim_token": "x".repeat(513) }),
    ] {
        let resp = app.clone().oneshot(post("/v1/claim", &body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    }
}

/// Unknown, malformed, and expired all render the same page: telling them apart
/// would answer "does this session exist?" for anyone who asks.
#[tokio::test]
async fn an_expired_session_is_gone_from_both_doors() {
    let state = state();
    let app = router(state.clone());
    let id = sid('h');

    app.clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();
    age_past_the_ttl(&state);

    let resp = app
        .clone()
        .oneshot(get(&format!("/link?s={id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let html = body_text(resp).await;
    assert!(html.contains("expired"));
    assert!(html.contains("squelchd auth"));

    let resp = app
        .clone()
        .oneshot(post(
            "/v1/claim",
            &json!({ "session_id": id, "claim_token": CLAIM_TOKEN }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await, json!({ "status": "unknown" }));

    // A late callback has nowhere to park.
    let resp = app
        .oneshot(get(&format!("/callback?code={CODE}&state={id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_and_malformed_links_render_the_same_expiry_page() {
    let app = app();
    let mut seen = Vec::new();
    for query in [
        String::new(),
        "?".to_string(),
        "?s=".to_string(),
        format!("?s={}", sid('z')),
        "?s=not-a-session-id".to_string(),
        "?s=../../etc/passwd".to_string(),
        format!("?s={}%3Cscript%3Ealert(1)%3C%2Fscript%3E", sid('z')),
    ] {
        let resp = app
            .clone()
            .oneshot(get(&format!("/link{query}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{query}");
        let html = body_text(resp).await;
        // Nothing the caller supplied is ever reflected into the page.
        assert!(!html.contains("script"), "{query}");
        assert!(!html.contains("passwd"), "{query}");
        seen.push(html);
    }
    assert!(
        seen.windows(2).all(|w| w[0] == w[1]),
        "the expiry page differed between unknown, malformed, and missing ids"
    );
}

/// A callback carrying nothing usable parks nothing.
#[tokio::test]
async fn a_callback_without_a_usable_code_parks_nothing() {
    let app = app();
    let id = sid('i');
    app.clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();

    for query in [
        format!("state={id}"),
        format!("state={id}&code="),
        format!("state={id}&code=has%20a%20space"),
        format!("state={id}&code=has%0Aa%20newline"),
    ] {
        let resp = app
            .clone()
            .oneshot(get(&format!("/callback?{query}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{query}");
    }

    // The session is untouched, so the real redirect still works.
    let resp = app
        .clone()
        .oneshot(get(&format!("/callback?code={CODE}&state={id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .oneshot(post(
            "/v1/claim",
            &json!({ "session_id": id, "claim_token": CLAIM_TOKEN }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["code"], CODE);
}

/// An error string reaches a daemon's terminal, and Google is not the only
/// party who can put one in the URL.
#[tokio::test]
async fn a_callback_error_is_flattened_to_an_oauth_error_code() {
    let app = app();
    let id = sid('j');
    app.clone()
        .oneshot(post("/v1/sessions", &registration(&id)))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(get(&format!(
            "/callback?error=%3Cscript%3Ealert(1)%3C%2Fscript%3E&state={id}"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(!html.contains("alert(1)"), "{html}");

    let resp = app
        .oneshot(post(
            "/v1/claim",
            &json!({ "session_id": id, "claim_token": CLAIM_TOKEN }),
        ))
        .await
        .unwrap();
    assert_eq!(
        body_json(resp).await,
        json!({ "status": "denied", "error": "invalid_request" })
    );
}

#[tokio::test]
async fn healthz_answers_without_a_session() {
    let resp = app().oneshot(get("/healthz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_text(resp).await, "ok");
}
