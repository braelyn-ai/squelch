//! Per-client-IP token buckets over the routes that do work: in-memory,
//! per-process abuse dampening, not a quota system — a restart forgives
//! everyone.
//!
//! The client IP is the TCP peer address by default, so behind the expected TLS
//! proxy the whole deployment shares one bucket. `X-Forwarded-For` is NOT
//! trusted on its own: caller-supplied, it would mint unlimited fresh
//! identities. `SQUELCH_RELAY_TRUSTED_PROXY_HOPS` is how an operator states how
//! much of that header their own infrastructure wrote — see [`client_ip`] —
//! and nothing left of that stated boundary is ever read. That claim is only
//! honoured for a peer inside `SQUELCH_RELAY_TRUSTED_PROXY_CIDRS`: a hop count
//! describes traffic arriving through the operator's proxies, and anything that
//! reaches the listener some other way is metered by its own address.
//!
//! The bucket engine and the header walk are [`squelch_ratelimit`], shared with
//! the broker; the CIDR gate, the route budgets and the middleware are here.
//!
//! Both routes carry their own buckets regardless. The pixel is metered
//! SEPARATELY because mail clients fetching pixels are the whole internet: on
//! one limiter they would spend the daemon's push budget, and under the default
//! shared-bucket keying that is one flood away.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use squelch_ratelimit::forwarded_for_ip;

pub use squelch_ratelimit::RateLimiter;

use crate::config::Cidr;
use crate::state::RelayState;

/// Sustained rate, in requests per minute, per client IP.
pub const REQUESTS_PER_MINUTE: f64 = 120.0;

/// The same, for `GET /t/{token}`. Higher because every recipient's mail client
/// arrives here — one send to a large list is a burst of legitimate fetches —
/// and cheaper to serve: one bounded insert, no APNs round trip. It stays high
/// even when keying is per-real-client: a mail provider that proxies images
/// (Gmail is the common one) fetches every one of its users' pixels from a
/// handful of its own addresses, so "one client" here is routinely a whole
/// provider, and a dropped fetch is an open lost for good — mail clients do not
/// retry an image.
pub const PIXEL_REQUESTS_PER_MINUTE: f64 = 600.0;

/// Middleware: 429 once a client IP outruns [`REQUESTS_PER_MINUTE`].
pub async fn limit(
    State(state): State<crate::state::RelayState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    gate(&state, RelayState::check_rate, "push", req, next).await
}

/// Middleware: the same for `GET /t/{token}`, against its own buckets.
pub async fn limit_pixel(
    State(state): State<crate::state::RelayState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    gate(&state, RelayState::check_pixel_rate, "pixel", req, next).await
}

/// Which identity a request is metered under.
///
/// `hops` is the operator's assertion of how many proxies sit in front of this
/// listener (`SQUELCH_RELAY_TRUSTED_PROXY_HOPS`). Each one appends the address
/// it saw to `X-Forwarded-For`, so the rightmost `hops` entries are the only
/// ones written by infrastructure the operator controls, and the client is the
/// `hops`-th from the right. Everything left of it arrived from the caller and
/// is never read: a caller stuffing entries there shifts nothing.
///
/// The claim binds to `trusted`, the peers that could BE those proxies. A hop
/// count says nothing about a connection that did not come through them, and a
/// listener bound to a routable interface takes connections from anyone: read
/// the header on peer alone and every direct caller writes their own key, one
/// fresh identity per request, which is both limiters gone.
///
/// Every failure — an untrusted peer, no header, fewer than `hops` entries, an
/// entry that is not an address — falls back to `peer`, which is the shared
/// bucket. That is closed: the worst case is metering that is too coarse, never
/// an attacker choosing their own key. `hops == 0` is that fallback
/// unconditionally.
pub(crate) fn client_ip(
    headers: &HeaderMap,
    peer: IpAddr,
    hops: usize,
    trusted: &[Cidr],
) -> IpAddr {
    if !trusted.iter().any(|c| c.contains(peer)) {
        return peer;
    }
    forwarded_for_ip(headers, peer, hops)
}

async fn gate(
    state: &RelayState,
    charge: fn(&RelayState, IpAddr) -> bool,
    route: &'static str,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // `ConnectInfo` is absent when the router is driven directly as a `Service`
    // (tower `oneshot`); those callers share the unspecified address. Production
    // always serves with connect info attached.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |ConnectInfo(a)| a.ip());
    let cfg = state.config();
    let ip = client_ip(
        req.headers(),
        peer,
        cfg.trusted_proxy_hops,
        cfg.proxy_allowlist(),
    );
    if charge(state, ip) {
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
    use crate::config::DEFAULT_TRUSTED_PROXY_CIDRS;
    use axum::http::HeaderValue;

    /// The header walk itself is covered in `squelch_ratelimit`; these cases are
    /// about WHO the relay lets speak for a client.
    fn keyed(headers: &HeaderMap, peer: IpAddr, hops: usize) -> IpAddr {
        client_ip(headers, peer, hops, DEFAULT_TRUSTED_PROXY_CIDRS)
    }

    fn headers(lines: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for l in lines {
            h.append("x-forwarded-for", HeaderValue::from_str(l).unwrap());
        }
        h
    }

    fn addr(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The operator declared a proxy; this caller is not it. A listener on a
    /// routable interface takes connections from anyone, and a fresh header per
    /// request is a fresh bucket per request — both limiters gone.
    #[test]
    fn a_peer_outside_the_allowlist_never_reads_the_header() {
        let direct = addr("203.0.113.7");
        for spoofed in [
            "198.51.100.7",
            "1.2.3.4",
            "10.0.0.1",
            "127.0.0.1",
            "2001:db8::dead",
        ] {
            assert_eq!(
                keyed(&headers(&[spoofed]), direct, 1),
                direct,
                "`{spoofed}` chose its own key"
            );
        }
        // Nor by claiming the whole chain.
        assert_eq!(
            keyed(&headers(&["1.1.1.1, 2.2.2.2, 3.3.3.3"]), direct, 2),
            direct
        );
    }

    /// The same request through the proxy the operator actually runs.
    #[test]
    fn a_peer_inside_the_allowlist_reads_the_header() {
        for proxy in ["127.0.0.1", "10.0.0.5", "172.16.0.5", "192.168.1.5"] {
            assert_eq!(
                keyed(&headers(&["203.0.113.9"]), addr(proxy), 1),
                addr("203.0.113.9"),
                "{proxy}"
            );
        }
        // An explicit list replaces the default in both directions.
        let only = [Cidr::parse("203.0.113.7").unwrap()];
        let h = headers(&["198.51.100.7"]);
        assert_eq!(
            client_ip(&h, addr("203.0.113.7"), 1, &only),
            addr("198.51.100.7")
        );
        assert_eq!(
            client_ip(&h, addr("127.0.0.1"), 1, &only),
            addr("127.0.0.1")
        );
    }

    /// Hops is still the outer gate: a trusted peer changes nothing when the
    /// operator has declared no proxy at all.
    #[test]
    fn hops_zero_ignores_the_header_even_from_a_trusted_peer() {
        for peer in ["127.0.0.1", "10.0.0.5", "192.168.1.5"] {
            assert_eq!(keyed(&headers(&["203.0.113.9"]), addr(peer), 0), addr(peer));
        }
    }

    /// A dual-stack listener reports a v4 connection as `::ffff:a.b.c.d`, which
    /// must not quietly cost the deployment its per-client metering.
    #[test]
    fn an_ipv4_mapped_peer_still_matches_a_v4_entry() {
        assert_eq!(
            keyed(&headers(&["203.0.113.9"]), addr("::ffff:10.0.0.5"), 1),
            addr("203.0.113.9")
        );
        let mapped_public = addr("::ffff:203.0.113.7");
        assert_eq!(
            keyed(&headers(&["203.0.113.9"]), mapped_public, 1),
            mapped_public
        );
    }
}
