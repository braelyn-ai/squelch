//! Per-client-IP token bucket over `POST /v1/push`.
//!
//! In-memory and per-process — this is abuse dampening, not a quota system. A
//! restart forgives everyone, and a horizontally scaled deployment limits per
//! instance. Both are acceptable: the relay is stateless by design and the
//! design doc puts real abuse controls at distribution time.
//!
//! CONSTRAINT: the client IP is the TCP peer address. Behind the expected TLS
//! proxy every request peers from the proxy, so the whole deployment shares one
//! bucket. `X-Forwarded-For` is deliberately NOT trusted — it is caller-supplied
//! and would hand any client an unlimited supply of fresh identities.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};

/// Sustained rate, in requests per minute, per client IP.
pub const REQUESTS_PER_MINUTE: f64 = 120.0;

/// Buckets idle longer than this are dropped on the next prune. Two windows of
/// slack so a burst-then-pause client is not credited a full bucket early.
const IDLE_TTL: Duration = Duration::from_secs(120);

/// Prune only once the map is big enough to matter, so the common path stays a
/// single hash lookup.
const PRUNE_AT: usize = 1024;

/// A prune is a full-map scan, so it runs on a clock, not per request: a client
/// walking an IPv6 /64 must not be able to make every request pay for one.
const PRUNE_EVERY: Duration = Duration::from_secs(10);

/// Hard ceiling on tracked buckets. A client rotating source addresses faster
/// than [`IDLE_TTL`] has nothing to reclaim, so the TTL alone does not bound the
/// map — this does. ~64k buckets is a few MB and a sub-millisecond scan.
const MAX_BUCKETS: usize = 65_536;

/// When the ceiling is hit, evict down to this fraction of it rather than
/// evicting one bucket per insert: the O(n) scan then amortizes to O(1) per
/// request instead of running on every single one.
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
    /// When the last full-map maintenance ran. `None` until the map first grows
    /// past [`PRUNE_AT`].
    last_prune: Option<Instant>,
}

impl RateLimiter {
    /// A limiter allowing `per_minute` sustained requests, with a burst
    /// allowance of the same size.
    pub fn per_minute(per_minute: f64) -> Self {
        Self {
            buckets: HashMap::new(),
            capacity: per_minute,
            refill_per_sec: per_minute / 60.0,
            last_prune: None,
        }
    }

    /// Charge one request against `ip`. Returns false when the bucket is empty.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.maintain(ip, now);
        let capacity = self.capacity;
        let refill = self.refill_per_sec;
        let b = self.buckets.entry(ip).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        // `saturating_duration_since` keeps a non-monotonic surprise from
        // panicking; the worst case is simply no refill for this request.
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

    /// Keep the map bounded in both size and scan cost. Everything expensive
    /// lives here so `check`'s common path stays one hash lookup.
    fn maintain(&mut self, ip: IpAddr, now: Instant) {
        if self.buckets.len() < PRUNE_AT {
            return;
        }
        let due = self
            .last_prune
            .is_none_or(|t| now.saturating_duration_since(t) >= PRUNE_EVERY);
        if due {
            self.prune(now);
            self.last_prune = Some(now);
        }
        // Only an insert can breach the ceiling; an existing bucket is free.
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(&ip) {
            self.evict_oldest_to(EVICT_TO);
        }
    }

    /// Drop buckets untouched for [`IDLE_TTL`]. An idle bucket has refilled to
    /// capacity anyway, so forgetting it costs the limiter nothing.
    fn prune(&mut self, now: Instant) {
        self.buckets
            .retain(|_, b| now.saturating_duration_since(b.last) < IDLE_TTL);
    }

    /// Evict least-recently-seen buckets until at most `target` remain.
    ///
    /// Forgetting a bucket only ever FORGIVES a client, so over-eviction (ties
    /// on `last`) is safe; being unable to bound the map would not be.
    fn evict_oldest_to(&mut self, target: usize) {
        if self.buckets.len() <= target {
            return;
        }
        let excess = self.buckets.len() - target;
        let mut times: Vec<Instant> = self.buckets.values().map(|b| b.last).collect();
        // `select_nth_unstable` is O(n): the cutoff is the `excess`-th oldest
        // timestamp, without sorting the whole vector.
        let (_, cutoff, _) = times.select_nth_unstable(excess - 1);
        let cutoff = *cutoff;
        self.buckets.retain(|_, b| b.last > cutoff);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

/// Middleware: 429 once a client IP outruns [`REQUESTS_PER_MINUTE`].
pub async fn limit(
    State(state): State<crate::state::RelayState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // `ConnectInfo` is absent when the router is driven directly as a `Service`
    // (tower `oneshot`) rather than served over TCP. Those callers all share the
    // unspecified address; production always serves with connect info attached.
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |ConnectInfo(a)| a.ip());
    if state.check_rate(ip) {
        Ok(next.run(req).await)
    } else {
        // No client detail is logged beyond the fact that a limit was hit.
        tracing::warn!("push rate limit exceeded");
        Err(StatusCode::TOO_MANY_REQUESTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn allows_a_full_burst_then_denies() {
        let mut l = RateLimiter::per_minute(3.0);
        let t = Instant::now();
        assert!(l.check(ip(1), t));
        assert!(l.check(ip(1), t));
        assert!(l.check(ip(1), t));
        assert!(!l.check(ip(1), t));
    }

    #[test]
    fn buckets_are_per_ip() {
        let mut l = RateLimiter::per_minute(1.0);
        let t = Instant::now();
        assert!(l.check(ip(1), t));
        assert!(!l.check(ip(1), t));
        assert!(l.check(ip(2), t));
    }

    #[test]
    fn refills_over_time() {
        let mut l = RateLimiter::per_minute(60.0);
        let t = Instant::now();
        for _ in 0..60 {
            assert!(l.check(ip(1), t));
        }
        assert!(!l.check(ip(1), t));
        // 60/min == 1/sec.
        assert!(l.check(ip(1), t + Duration::from_secs(1)));
    }

    /// The map must be bounded even against a client that never reuses an
    /// address, which is exactly the case the idle TTL cannot reclaim.
    #[test]
    fn caps_the_bucket_count_against_address_rotation() {
        let mut l = RateLimiter::per_minute(10.0);
        let t = Instant::now();
        // Every address distinct and every bucket touched "now", so nothing is
        // ever idle enough to prune.
        for n in 0..(MAX_BUCKETS as u32 + 5_000) {
            l.check(IpAddr::V4(Ipv4Addr::from(n)), t);
            assert!(l.len() <= MAX_BUCKETS, "map exceeded the ceiling");
        }
        assert!(l.len() <= MAX_BUCKETS);
    }

    /// A full-map scan must not run on every request once the map is large.
    #[test]
    fn prune_runs_on_a_clock_not_per_request() {
        let mut l = RateLimiter::per_minute(1000.0);
        let t = Instant::now();
        for n in 0..PRUNE_AT as u32 {
            l.check(IpAddr::V4(Ipv4Addr::from(n)), t);
        }
        // Everything is now older than the TTL, but only the first check past
        // the interval may prune; the next one within the interval must not.
        let later = t + IDLE_TTL + Duration::from_secs(1);
        l.check(ip(1), later);
        assert_eq!(l.len(), 1);
        for n in 0..PRUNE_AT as u32 {
            l.check(IpAddr::V4(Ipv4Addr::from(n)), later);
        }
        // Still inside PRUNE_EVERY, so the idle sweep has not run again even
        // though the map is back over PRUNE_AT.
        assert_eq!(l.len(), PRUNE_AT + 1);
        assert!(l.check(ip(2), later + PRUNE_EVERY));
    }

    #[test]
    fn prunes_idle_buckets() {
        let mut l = RateLimiter::per_minute(10.0);
        let t = Instant::now();
        for n in 0..PRUNE_AT {
            l.check(IpAddr::V4(Ipv4Addr::from(n as u32)), t);
        }
        assert_eq!(l.len(), PRUNE_AT);
        // The next check trips the prune; everything is older than the TTL.
        l.check(ip(1), t + IDLE_TTL + Duration::from_secs(1));
        assert_eq!(l.len(), 1);
    }
}
