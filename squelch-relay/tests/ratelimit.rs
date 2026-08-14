//! Which identity the two limiters meter, through the real router.
//!
//! The router is driven as a `Service` rather than over a socket, so the peer
//! address is a value the test picks: that is the only way to prove the peer
//! decides the bucket when no proxy is trusted, and yields to the header when
//! one is.

use std::net::SocketAddr;

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

use squelch_relay::Cidr;

mod common;
use common::config;

/// A well-formed open token, so the pixel route does real work.
const OPEN: &str = "aaaaAAAA1111-_bb";

/// The anonymous relay: `/v1/push` is served without a bearer, so a request
/// reaches the limiter with no credential to arrange. The body is deliberately
/// invalid — the limiter charges before the handler ever looks at it, and a 400
/// costs no APNs round trip.
///
/// The peer allowlist is left at the default, which holds every private peer the
/// tests below connect from and no public one.
fn relay(trusted_proxy_hops: usize) -> Router {
    relay_from(trusted_proxy_hops, None)
}

fn relay_from(trusted_proxy_hops: usize, trusted_proxy_cidrs: Option<Vec<Cidr>>) -> Router {
    let mut cfg = config(None, None);
    cfg.trusted_proxy_hops = trusted_proxy_hops;
    cfg.trusted_proxy_cidrs = trusted_proxy_cidrs;
    squelch_relay::router(squelch_relay::RelayState::new(cfg).unwrap())
}

async fn send(app: &Router, req: Request<Body>) -> StatusCode {
    app.clone().oneshot(req).await.unwrap().status()
}

fn with_peer(mut req: Request<Body>, peer: &str) -> Request<Body> {
    let peer: SocketAddr = format!("{peer}:40000").parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

fn request(uri: &str, peer: &str, xff: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(uri);
    if let Some(v) = xff {
        b = b.header("x-forwarded-for", v);
    }
    let req = if uri == "/v1/push" {
        b.method("POST")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    };
    with_peer(req, peer)
}

async fn push(app: &Router, peer: &str, xff: Option<&str>) -> StatusCode {
    send(app, request("/v1/push", peer, xff)).await
}

async fn pixel(app: &Router, peer: &str, xff: Option<&str>) -> StatusCode {
    send(app, request(&format!("/t/{OPEN}"), peer, xff)).await
}

const BUDGET: usize = squelch_relay::ratelimit::REQUESTS_PER_MINUTE as usize;
const PIXEL_BUDGET: usize = squelch_relay::ratelimit::PIXEL_REQUESTS_PER_MINUTE as usize;

/// Spend one identity's push bucket. The bucket refills while the loop runs, so
/// this asserts a 429 appears within twice the budget rather than at an index.
async fn exhaust(app: &Router, peer: &str, xff: Option<&str>) {
    for i in 0..BUDGET * 2 {
        match push(app, peer, xff).await {
            StatusCode::BAD_REQUEST => {}
            StatusCode::TOO_MANY_REQUESTS => return,
            other => panic!("request {i}: unexpected status {other}"),
        }
    }
    panic!("no 429 within twice the per-minute budget");
}

/// The same for the pixel route, which carries the larger budget.
async fn exhaust_pixel(app: &Router, peer: &str, xff: Option<&str>) {
    for i in 0..PIXEL_BUDGET * 2 {
        match pixel(app, peer, xff).await {
            StatusCode::OK => {}
            StatusCode::TOO_MANY_REQUESTS => return,
            other => panic!("request {i}: unexpected status {other}"),
        }
    }
    panic!("no 429 within twice the per-minute pixel budget");
}

/// Does this identity still hold an exhausted bucket? A refilled token can slip
/// through between calls, but a FRESH bucket has the whole budget, so a handful
/// of back-to-back requests separates the two without depending on timing.
async fn is_limited(app: &Router, peer: &str, xff: Option<&str>) -> bool {
    for _ in 0..5 {
        if push(app, peer, xff).await == StatusCode::TOO_MANY_REQUESTS {
            return true;
        }
    }
    false
}

/// The same question on the pixel route.
async fn pixel_is_limited(app: &Router, peer: &str, xff: Option<&str>) -> bool {
    for _ in 0..5 {
        if pixel(app, peer, xff).await == StatusCode::TOO_MANY_REQUESTS {
            return true;
        }
    }
    false
}

/// With no trusted hops configured, the peer decides and the header is not read
/// at all.
#[tokio::test]
async fn hops_zero_keys_on_the_peer_address() {
    let app = relay(0);
    exhaust(&app, "198.51.100.1", None).await;

    // Same peer, and no `X-Forwarded-For` value buys a way out.
    assert!(is_limited(&app, "198.51.100.1", None).await);
    assert!(is_limited(&app, "198.51.100.1", Some("203.0.113.9")).await);
    assert!(is_limited(&app, "198.51.100.1", Some("203.0.113.10")).await);

    // A different peer is a different bucket.
    assert_eq!(
        push(&app, "198.51.100.2", None).await,
        StatusCode::BAD_REQUEST
    );
}

/// `hops` is a claim about traffic arriving THROUGH the operator's proxies. A
/// caller that reaches the listener some other way — the operator widened
/// `SQUELCH_RELAY_BIND` so a proxy on another host could connect — writes a
/// fresh `X-Forwarded-For` per request, and on the hop count alone that is a
/// fresh bucket per request: no limiter at all.
#[tokio::test]
async fn a_peer_outside_the_allowlist_cannot_key_on_the_forwarded_header() {
    let app = relay(1);
    let direct = "203.0.113.7";
    exhaust(&app, direct, Some("198.51.100.1")).await;

    // A different forged client every request, which is the whole attack.
    for n in 2..10 {
        let forged = format!("198.51.100.{n}");
        assert!(
            is_limited(&app, direct, Some(&forged)).await,
            "`{forged}` minted a fresh bucket"
        );
    }
    // Naming one of the operator's own proxies buys nothing either.
    assert!(is_limited(&app, direct, Some("10.0.0.1")).await);
    // The fallback is still the peer, not one global bucket: an untrusted peer
    // is metered, not merged.
    assert_eq!(
        push(&app, "203.0.113.8", Some("198.51.100.1")).await,
        StatusCode::BAD_REQUEST
    );
}

/// The pixel bucket is the bigger prize — it is what fills the open buffer — and
/// it is gated identically.
#[tokio::test]
async fn the_pixel_limiter_is_gated_on_the_peer_too() {
    let app = relay(1);
    let direct = "203.0.113.7";
    exhaust_pixel(&app, direct, Some("198.51.100.1")).await;

    for n in 2..10 {
        let forged = format!("198.51.100.{n}");
        assert!(
            pixel_is_limited(&app, direct, Some(&forged)).await,
            "`{forged}` minted a fresh pixel bucket"
        );
    }
}

/// An operator whose proxy is not on a private network says so, and then that
/// peer — and only that peer — is believed.
#[tokio::test]
async fn an_explicit_allowlist_replaces_the_default_in_both_directions() {
    let app = relay_from(1, Some(vec![Cidr::parse("203.0.113.7").unwrap()]));
    exhaust(&app, "203.0.113.7", Some("198.51.100.1")).await;
    assert!(is_limited(&app, "203.0.113.7", Some("198.51.100.1")).await);
    // A different client through that proxy is untouched: the header is being
    // read.
    assert_eq!(
        push(&app, "203.0.113.7", Some("198.51.100.2")).await,
        StatusCode::BAD_REQUEST
    );

    // Loopback is on the DEFAULT list, and this deployment did not name it.
    exhaust(&app, "127.0.0.1", Some("198.51.100.3")).await;
    assert!(is_limited(&app, "127.0.0.1", Some("198.51.100.4")).await);
}

/// The hop count is still the outer gate: a peer that could be a proxy does not
/// make the header readable where the operator declared no proxy at all.
#[tokio::test]
async fn hops_zero_ignores_the_header_from_a_trusted_peer() {
    let app = relay(0);
    exhaust(&app, "10.0.0.1", Some("203.0.113.9")).await;
    assert!(is_limited(&app, "10.0.0.1", Some("203.0.113.10")).await);
    assert!(is_limited(&app, "10.0.0.1", None).await);
    assert_eq!(
        push(&app, "10.0.0.2", Some("203.0.113.9")).await,
        StatusCode::BAD_REQUEST
    );
}

/// With one proxy trusted, the entry that proxy appended is the client — and it
/// is the whole key, so the peer (the proxy, always the same address) stops
/// mattering. The peers here are private, which the default allowlist holds.
#[tokio::test]
async fn hops_one_keys_on_the_last_forwarded_entry() {
    let app = relay(1);
    exhaust(&app, "10.0.0.1", Some("203.0.113.9")).await;

    assert!(is_limited(&app, "10.0.0.1", Some("203.0.113.9")).await);
    // Same client through a different proxy instance.
    assert!(is_limited(&app, "10.0.0.2", Some("203.0.113.9")).await);
    // A different client behind the same proxy is untouched: this is the whole
    // point — one abuser no longer starves everyone.
    assert_eq!(
        push(&app, "10.0.0.1", Some("203.0.113.10")).await,
        StatusCode::BAD_REQUEST
    );
}

/// The header is caller-supplied up to the trusted boundary, so prepending to it
/// must not produce a fresh bucket.
#[tokio::test]
async fn stuffed_left_hand_entries_cannot_mint_a_new_identity() {
    let app = relay(1);
    exhaust(&app, "10.0.0.1", Some("203.0.113.9")).await;

    for stuffed in [
        "1.1.1.1, 203.0.113.9",
        "2.2.2.2, 3.3.3.3, 203.0.113.9",
        "not-an-ip, 203.0.113.9",
    ] {
        assert!(
            is_limited(&app, "10.0.0.1", Some(stuffed)).await,
            "`{stuffed}` minted a fresh bucket"
        );
    }
}

/// A header too short to reach the trusted position is a header written by the
/// caller: fall back to the peer, not to what they wrote.
#[tokio::test]
async fn missing_or_short_forwarded_falls_back_to_the_peer() {
    let app = relay(1);
    exhaust(&app, "198.51.100.1", None).await;
    assert!(is_limited(&app, "198.51.100.1", None).await);
    assert_eq!(
        push(&app, "198.51.100.2", None).await,
        StatusCode::BAD_REQUEST
    );

    // Two hops claimed, one entry present: the only entry is the caller's.
    let app = relay(2);
    exhaust(&app, "198.51.100.1", Some("203.0.113.9")).await;
    assert!(is_limited(&app, "198.51.100.1", Some("8.8.8.8")).await);
    assert_eq!(
        push(&app, "198.51.100.2", Some("203.0.113.9")).await,
        StatusCode::BAD_REQUEST
    );
}

/// Pixel traffic must never be able to spend the daemon's push budget, whichever
/// identity both are keyed on.
#[tokio::test]
async fn the_two_limiters_stay_independent() {
    let app = relay(1);
    exhaust(&app, "10.0.0.1", Some("203.0.113.9")).await;
    assert!(is_limited(&app, "10.0.0.1", Some("203.0.113.9")).await);

    // Same client, other route: its own bucket, untouched.
    assert_eq!(
        pixel(&app, "10.0.0.1", Some("203.0.113.9")).await,
        StatusCode::OK
    );

    // And the reverse: spending the pixel bucket leaves the push bucket of a
    // different client alone.
    let app = relay(1);
    for _ in 0..squelch_relay::ratelimit::PIXEL_REQUESTS_PER_MINUTE as usize + 1 {
        pixel(&app, "10.0.0.1", Some("203.0.113.9")).await;
    }
    assert_eq!(
        push(&app, "10.0.0.1", Some("203.0.113.9")).await,
        StatusCode::BAD_REQUEST
    );
}
