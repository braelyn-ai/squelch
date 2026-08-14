//! Per-client-IP token buckets over every route that does work: in-memory,
//! per-process abuse dampening, not a quota system — a restart forgives
//! everyone. With no authentication anywhere in this service (every stranger's
//! daemon is a legitimate client), this and the session caps are the defense.
//!
//! The client IP is the TCP peer address by default, so behind the expected TLS
//! proxy the whole deployment shares one bucket and "per-IP" means "per
//! deployment". `X-Forwarded-For` is NOT trusted on its own: caller-supplied, it
//! would mint unlimited fresh identities.
//! `SQUELCH_BROKER_TRUSTED_PROXY_HOPS` is how an operator states how much of
//! that header their own infrastructure wrote — see [`client_ip`] — and nothing
//! left of that stated boundary is ever read. Setting it is what makes both the
//! buckets here and the per-client session cap real.
//!
//! The bucket engine and the header walk are [`squelch_ratelimit`], shared with
//! the relay; the route budgets and the middleware are here.
//!
//! Every route carries its OWN buckets, because a 429 costs a different thing
//! on each one:
//!
//! - `/v1/sessions` is tight. A real client registers once per consent and then
//!   polls; sharing a budget with the polling would give registration hundreds
//!   of requests a minute of headroom it has no use for, and registration is
//!   the route that allocates.
//! - `/v1/claim` is generous: a daemon polls it for the whole time a human
//!   takes to read a consent screen.
//! - `/link` is metered like a page, because browsers prefetch and mail clients
//!   scan links, and that traffic must not spend a daemon's claim budget.
//! - `/callback` is the most generous of all. Refusing it destroys a consent the
//!   user has ALREADY granted: Google will not redirect twice, and the recovery
//!   is the whole flow again.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use squelch_ratelimit::forwarded_for_ip;

pub use squelch_ratelimit::RateLimiter;

use crate::state::BrokerState;

/// Sustained rate, in requests per minute, per client IP, for `POST
/// /v1/sessions`. A daemon registers ONCE per `squelchd auth` (twice for
/// `--write`), so this is deliberately far below the polling rate: registration
/// is the only route that allocates, and its legitimate traffic is a trickle.
/// Under the default shared-bucket keying this is a whole deployment's fresh
/// consents per minute, which is still many more than a self-hosted broker
/// sees.
pub const REGISTER_REQUESTS_PER_MINUTE: f64 = 30.0;

/// The same, for `POST /v1/claim`. Generous because a daemon polls every two
/// seconds for the whole ten-minute consent window, and behind a proxy with no
/// trusted-hops config every daemon's polling lands in one bucket.
pub const CLAIM_REQUESTS_PER_MINUTE: f64 = 600.0;

/// The same, for `GET /link`. One human click, but browsers prefetch and mail
/// clients scan links.
pub const PAGE_REQUESTS_PER_MINUTE: f64 = 300.0;

/// The same, for `GET /callback`. The highest number here because it is the one
/// route whose refusal costs a user something they cannot get back: the consent
/// they just granted at Google. It is also the cheapest to serve (one map
/// lookup into a table that is already capped) and it can do nothing at all
/// without a live session id.
pub const CALLBACK_REQUESTS_PER_MINUTE: f64 = 1200.0;

/// The client a request is attributed to, resolved once by [`gate`] and left in
/// the request's extensions for the handler.
///
/// Registration needs the same identity the limiter used, and recomputing it
/// there would be a second place for the two to disagree about who a client is.
/// PRIVACY: an address is a client identifier, so this never reaches a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        // Every route that extracts this sits behind a rate-limit layer, which
        // is what inserts it. The fallback keeps a router built without one
        // from being a panic: it collapses to a single shared identity, which
        // is the same posture as `hops == 0`.
        Ok(parts
            .extensions
            .get::<ClientIp>()
            .copied()
            .unwrap_or(ClientIp(IpAddr::V4(Ipv4Addr::UNSPECIFIED))))
    }
}

/// Middleware: 429 once a client outruns [`REGISTER_REQUESTS_PER_MINUTE`].
pub async fn limit_register(
    State(state): State<BrokerState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    gate(
        &state,
        BrokerState::check_register_rate,
        "register",
        req,
        next,
    )
    .await
}

/// Middleware: the same for `POST /v1/claim`, against its own buckets.
pub async fn limit_claim(
    State(state): State<BrokerState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    gate(&state, BrokerState::check_claim_rate, "claim", req, next).await
}

/// Middleware: the same for `GET /link`.
pub async fn limit_page(
    State(state): State<BrokerState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    gate(&state, BrokerState::check_page_rate, "page", req, next).await
}

/// Middleware: the same for `GET /callback`, against the most generous buckets
/// here — a refusal on this route loses a consent the user already granted.
pub async fn limit_callback(
    State(state): State<BrokerState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    gate(
        &state,
        BrokerState::check_callback_rate,
        "callback",
        req,
        next,
    )
    .await
}

async fn gate(
    state: &BrokerState,
    charge: fn(&BrokerState, IpAddr) -> bool,
    route: &'static str,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // `ConnectInfo` is absent when the router is driven directly as a `Service`
    // (tower `oneshot`); those callers share the unspecified address. Production
    // always serves with connect info attached.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |ConnectInfo(a)| a.ip());
    let ip = forwarded_for_ip(req.headers(), peer, state.config().trusted_proxy_hops);
    if charge(state, ip) {
        // The handler meters its own per-client limits against the same
        // identity, rather than resolving one of its own.
        req.extensions_mut().insert(ClientIp(ip));
        Ok(next.run(req).await)
    } else {
        // No client detail is logged beyond the fact that a limit was hit.
        tracing::warn!(route, "rate limit exceeded");
        Err(StatusCode::TOO_MANY_REQUESTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn the_claim_rate_outlasts_a_full_consent_window() {
        let mut l = RateLimiter::per_minute(CLAIM_REQUESTS_PER_MINUTE);
        let start = Instant::now();
        // One poll every two seconds for ten minutes.
        for i in 0..300u64 {
            let t = start + Duration::from_secs(i * 2);
            assert!(l.check(ip(1), t), "poll {i} was throttled");
        }
    }

    /// Registration and polling are not on one budget: a client registers once
    /// and polls three hundred times, so a shared bucket would hand the route
    /// that allocates hundreds of requests a minute of headroom.
    #[test]
    fn registration_is_metered_far_tighter_than_polling() {
        const { assert!(REGISTER_REQUESTS_PER_MINUTE * 10.0 < CLAIM_REQUESTS_PER_MINUTE) };

        let mut l = RateLimiter::per_minute(REGISTER_REQUESTS_PER_MINUTE);
        let t = Instant::now();
        // Both registrations of a `squelchd auth --write` run, twice over, with
        // room to spare.
        for i in 0..4 {
            assert!(l.check(ip(1), t), "registration {i} was throttled");
        }
        // And a flood is cut off well inside a claim budget.
        let flooded = (0..REGISTER_REQUESTS_PER_MINUTE as u32 + 1).all(|_| l.check(ip(1), t));
        assert!(!flooded, "registration must run out before the claim rate");
    }

    /// Google redirects once. A 429 here is a consent the user granted and will
    /// have to grant again, so this bucket is the largest one.
    #[test]
    fn the_callback_bucket_is_the_most_forgiving() {
        const { assert!(CALLBACK_REQUESTS_PER_MINUTE > PAGE_REQUESTS_PER_MINUTE) };
        const { assert!(CALLBACK_REQUESTS_PER_MINUTE >= CLAIM_REQUESTS_PER_MINUTE) };
    }
}
