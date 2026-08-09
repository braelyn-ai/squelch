//! A per-client-IP token bucket over the control-plane surface: in-memory,
//! per-process abuse dampening, not a quota system. A restart forgives
//! everyone.
//!
//! A SIBLING of `squelch-control::ratelimit` and `squelch-broker::ratelimit`,
//! not a copy of convenience: the bucket arithmetic, the pruning and the
//! `X-Forwarded-For` reading are the same because they were right there, and
//! the state type they hang off is different, which is the same reason the
//! middleware lives in each crate. If a fourth service appears, this is the
//! moment to lift it into a leaf crate beside `squelch-httpauth`.
//!
//! ONE bucket set rather than one per route, which is where this differs from
//! its siblings. Those are public surfaces whose routes cost different things;
//! this one has a single legitimate caller holding a single credential, so the
//! interesting distinction is not which route was asked for, it is how fast
//! somebody who does not hold the token is asking. The limiter runs OUTSIDE the
//! bearer gate for that reason: a flood should be cheap to refuse, and a
//! constant-time compare per request is not the cheapest thing available.
//!
//! The client IP is the TCP peer by default, so behind the ingress controller
//! the whole cluster shares one bucket and "per-IP" means "per ingress".
//! `X-Forwarded-For` is NOT trusted on its own: caller-supplied, it would mint
//! unlimited fresh identities. `SQUELCH_WARDEN_TRUSTED_PROXY_HOPS` is how an
//! operator states how much of that header their own infrastructure wrote.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::WardenState;

/// Sustained requests per minute, per client, over the whole `/v1` surface.
///
/// A signup is five calls and a human waits for it, so this is roughly twenty
/// concurrent signups a minute from one address: far above what the control
/// plane does and far below what a credential-guessing loop wants.
pub const CONTROL_REQUESTS_PER_MINUTE: f64 = 120.0;

/// Buckets idle longer than this are dropped on the next prune. An idle bucket
/// has refilled to capacity anyway, so forgetting it costs nothing.
const IDLE_TTL: Duration = Duration::from_secs(120);

/// Prune only once the map is big enough to matter, so the common path stays a
/// single hash lookup.
const PRUNE_AT: usize = 1024;

/// A prune is a full-map scan, so it runs on a clock rather than per request: a
/// client walking an IPv6 /64 must not make every request pay for one.
const PRUNE_EVERY: Duration = Duration::from_secs(10);

/// Hard ceiling on tracked buckets. A client rotating addresses faster than
/// [`IDLE_TTL`] leaves nothing to reclaim, so the TTL alone does not bound the
/// map.
const MAX_BUCKETS: usize = 65_536;

/// Evict in a batch rather than one bucket per insert, so the O(n) scan
/// amortizes to O(1) per request.
const EVICT_TO: usize = MAX_BUCKETS / 2;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A fixed-rate token bucket keyed by client IP.
pub struct RateLimiter {
    buckets: HashMap<IpAddr, Bucket>,
    capacity: f64,
    refill_per_sec: f64,
    last_prune: Option<Instant>,
}

impl RateLimiter {
    /// `per_minute` sustained requests, with a burst allowance of the same size.
    pub fn per_minute(per_minute: f64) -> Self {
        Self {
            buckets: HashMap::new(),
            capacity: per_minute,
            refill_per_sec: per_minute / 60.0,
            last_prune: None,
        }
    }

    /// Charge one request against `ip`. False means the bucket is empty.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.maintain(ip, now);
        let capacity = self.capacity;
        let refill = self.refill_per_sec;
        let b = self.buckets.entry(ip).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        // `saturating_duration_since` cannot panic on a non-monotonic clock;
        // the worst case is no refill for this request.
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * refill).min(capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Bound the map in size and scan cost. Everything expensive lives here so
    /// `check`'s common path stays one hash lookup.
    fn maintain(&mut self, ip: IpAddr, now: Instant) {
        if self.buckets.len() < PRUNE_AT {
            return;
        }
        let due = self
            .last_prune
            .is_none_or(|t| now.saturating_duration_since(t) >= PRUNE_EVERY);
        if due {
            self.buckets
                .retain(|_, b| now.saturating_duration_since(b.last) < IDLE_TTL);
            self.last_prune = Some(now);
        }
        // Only an insert can breach the ceiling; an existing bucket is free.
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(&ip) {
            self.evict_oldest_to(EVICT_TO);
        }
    }

    /// Evict least-recently-seen buckets until at most `target` remain.
    /// Forgetting a bucket only ever FORGIVES a client, so over-eviction on a
    /// tie is safe; failing to bound the map would not be.
    fn evict_oldest_to(&mut self, target: usize) {
        if self.buckets.len() <= target {
            return;
        }
        let excess = self.buckets.len() - target;
        let mut times: Vec<Instant> = self.buckets.values().map(|b| b.last).collect();
        let (_, cutoff, _) = times.select_nth_unstable(excess - 1);
        let cutoff = *cutoff;
        self.buckets.retain(|_, b| b.last > cutoff);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

/// Middleware: 429 once a client outruns [`CONTROL_REQUESTS_PER_MINUTE`].
pub async fn limit_control(
    State(state): State<WardenState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // `ConnectInfo` is absent when the router is driven directly as a `Service`
    // (tower `oneshot`); those callers share the unspecified address.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |ConnectInfo(a)| a.ip());
    let ip = client_ip(req.headers(), peer, state.trusted_proxy_hops());
    if state.check_rate(ip) {
        Ok(next.run(req).await)
    } else {
        // PRIVACY: no address, no path. That a client was throttled is all a
        // log line needs.
        tracing::warn!("rate limited");
        Err(StatusCode::TOO_MANY_REQUESTS)
    }
}

/// Which identity a request is metered under.
///
/// `hops` is the operator's assertion of how many proxies sit in front of this
/// listener. Each appends the address it saw to `X-Forwarded-For`, so the
/// rightmost `hops` entries are the only ones written by infrastructure the
/// operator controls, and the client is the `hops`-th from the right.
/// Everything left of it arrived from the caller and is never read.
///
/// Every failure falls back to `peer`, the shared bucket. That is closed: the
/// worst case is metering that is too coarse, never an attacker choosing their
/// own key.
pub(crate) fn client_ip(headers: &HeaderMap, peer: IpAddr, hops: usize) -> IpAddr {
    if hops == 0 {
        return peer;
    }
    // Only the last `hops` entries can matter, so the header is never collected
    // whole: a caller controls its length.
    let mut tail: VecDeque<&str> = VecDeque::with_capacity(hops);
    let entries = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|e| !e.is_empty());
    for e in entries {
        if tail.len() == hops {
            tail.pop_front();
        }
        tail.push_back(e);
    }
    if tail.len() < hops {
        return peer;
    }
    tail.front().and_then(|e| parse_entry(e)).unwrap_or(peer)
}

/// Parse one `X-Forwarded-For` entry. A bare address is the norm, but proxies
/// also write `ip:port` and the bracketed IPv6 form; anything else is not
/// something to meter.
fn parse_entry(entry: &str) -> Option<IpAddr> {
    entry
        .parse::<IpAddr>()
        .ok()
        .or_else(|| entry.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .or_else(|| entry.strip_prefix('[')?.strip_suffix(']')?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn spends_and_refills_a_bucket() {
        let mut l = RateLimiter::per_minute(60.0);
        let t0 = Instant::now();
        for _ in 0..60 {
            assert!(l.check(ip(1), t0));
        }
        assert!(!l.check(ip(1), t0), "the bucket is empty");
        // One token per second at 60/minute.
        assert!(l.check(ip(1), t0 + Duration::from_secs(1)));
    }

    #[test]
    fn meters_clients_separately() {
        let mut l = RateLimiter::per_minute(1.0);
        let t0 = Instant::now();
        assert!(l.check(ip(1), t0));
        assert!(!l.check(ip(1), t0));
        assert!(l.check(ip(2), t0), "another client has its own bucket");
    }

    #[test]
    fn bounds_the_bucket_map() {
        let mut l = RateLimiter::per_minute(600.0);
        let t0 = Instant::now();
        for i in 0..(PRUNE_AT + 100) {
            let addr = IpAddr::V4(Ipv4Addr::from((i as u32).to_be_bytes()));
            l.check(addr, t0);
        }
        // Everything is fresh, so nothing is pruned yet...
        assert!(l.len() > PRUNE_AT);
        // ...and after the idle window, the next request collects them.
        let later = t0 + IDLE_TTL + PRUNE_EVERY;
        l.check(ip(9), later);
        assert!(l.len() <= 1, "idle buckets are dropped: {}", l.len());
    }

    /// With no trusted hops, a caller-supplied header cannot mint identities.
    #[test]
    fn ignores_forwarded_for_until_the_operator_says_otherwise() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        assert_eq!(client_ip(&h, ip(1), 0), ip(1));
        assert_eq!(
            client_ip(&h, ip(1), 1),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
    }

    /// With one trusted hop, entries the caller stuffed to the LEFT shift
    /// nothing: the client is still the rightmost entry.
    #[test]
    fn reads_only_what_our_own_proxies_wrote() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            "9.9.9.9, 8.8.8.8, 1.2.3.4".parse().unwrap(),
        );
        assert_eq!(
            client_ip(&h, ip(1), 1),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_ip(&h, ip(1), 2),
            "8.8.8.8".parse::<IpAddr>().unwrap()
        );
        // More hops asserted than entries present: fall back to the peer rather
        // than reading something the caller wrote.
        assert_eq!(client_ip(&h, ip(1), 4), ip(1));
    }

    #[test]
    fn parses_the_forwarded_forms_proxies_actually_write() {
        assert_eq!(parse_entry("1.2.3.4"), Some("1.2.3.4".parse().unwrap()));
        assert_eq!(
            parse_entry("1.2.3.4:5678"),
            Some("1.2.3.4".parse().unwrap())
        );
        assert_eq!(parse_entry("[::1]"), Some("::1".parse().unwrap()));
        assert_eq!(parse_entry("[::1]:80"), Some("::1".parse().unwrap()));
        assert_eq!(parse_entry("unknown"), None);
        assert_eq!(parse_entry(""), None);
    }
}
