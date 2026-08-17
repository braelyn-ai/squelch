//! Per-client-IP token buckets, and the `X-Forwarded-For` policy that decides
//! which client a request is metered against.
//!
//! Shared by `squelch-relay` and `squelch-broker`. Both are public-facing
//! services whose rate limiter is a real defense rather than a courtesy, and
//! both previously carried byte-identical copies of this engine — exactly the
//! shape a limiter fix gets applied to once and forgotten in the other. The
//! middleware stays in each crate (the state types and the route budgets
//! differ); the bucket engine and the header policy live here, tested once.
//!
//! A leaf crate rather than a module of `squelch-core`: these two binaries run
//! on shared public infrastructure and are blind by design — no store, no mail,
//! no credentials. Reaching this engine must not link the mail core into them.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use http::HeaderMap;

/// Buckets idle longer than this are dropped on the next prune — two windows of
/// slack, so a burst-then-pause client is not credited a full bucket early.
const IDLE_TTL: Duration = Duration::from_secs(120);

/// Prune only once the map is big enough to matter, so the common path stays a
/// single hash lookup.
const PRUNE_AT: usize = 1024;

/// A prune is a full-map scan, so it runs on a clock, not per request: a client
/// walking an IPv6 /64 must not make every request pay for one.
const PRUNE_EVERY: Duration = Duration::from_secs(10);

/// Hard ceiling on tracked buckets. A client rotating addresses faster than
/// [`IDLE_TTL`] leaves nothing to reclaim, so the TTL alone does not bound the
/// map — this does.
const MAX_BUCKETS: usize = 65_536;

/// Evict in a batch down to this size rather than one bucket per insert, so the
/// O(n) scan amortizes to O(1) per request.
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
    /// `None` until the map first grows past [`PRUNE_AT`].
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

    /// Charge one request against `ip`. Returns false when the bucket is empty.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.maintain(ip, now);
        let capacity = self.capacity;
        let refill = self.refill_per_sec;
        let b = self.buckets.entry(ip).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        // `saturating_duration_since` cannot panic on a non-monotonic clock; the
        // worst case is no refill for this request.
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
            self.prune(now);
            self.last_prune = Some(now);
        }
        // Only an insert can breach the ceiling; an existing bucket is free.
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(&ip) {
            self.evict_oldest_to(EVICT_TO);
        }
    }

    /// Drop buckets untouched for [`IDLE_TTL`]; an idle bucket has refilled to
    /// capacity anyway, so forgetting it costs the limiter nothing.
    fn prune(&mut self, now: Instant) {
        self.buckets
            .retain(|_, b| now.saturating_duration_since(b.last) < IDLE_TTL);
    }

    /// Evict least-recently-seen buckets until at most `target` remain.
    /// Forgetting a bucket only ever FORGIVES a client, so over-eviction on
    /// `last` ties is safe; failing to bound the map would not be.
    fn evict_oldest_to(&mut self, target: usize) {
        if self.buckets.len() <= target {
            return;
        }
        let excess = self.buckets.len() - target;
        let mut times: Vec<Instant> = self.buckets.values().map(|b| b.last).collect();
        // O(n): the `excess`-th oldest timestamp without sorting the vector.
        let (_, cutoff, _) = times.select_nth_unstable(excess - 1);
        let cutoff = *cutoff;
        self.buckets.retain(|_, b| b.last > cutoff);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

/// The client address `hops` proxies deep in `X-Forwarded-For`, counting from
/// the right. Only the operator's own infrastructure appends to the right, so
/// everything to the LEFT of that position is caller-controlled and is never
/// read: a caller stuffing entries there shifts nothing.
///
/// Every failure — no header, fewer than `hops` entries, an entry that is not
/// an address — falls back to `peer`, which is the shared bucket. That is
/// closed: the worst case is metering that is too coarse, never an attacker
/// choosing their own key. `hops == 0` is that fallback unconditionally.
///
/// Callers that also gate on WHO may send the header must check that before
/// calling; a hop count only describes the shape of a trusted chain, not its
/// authenticity.
pub fn forwarded_for_ip(headers: &HeaderMap, peer: IpAddr, hops: usize) -> IpAddr {
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
/// also write `ip:port` and the bracketed IPv6 form; an entry in any other
/// shape (`unknown`, an obfuscated node name) is not something to meter.
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
    use http::HeaderValue;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

    /// One `X-Forwarded-For` per line, in the order a proxy chain would append
    /// them.
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

    fn keyed(h: &HeaderMap, peer: IpAddr, hops: usize) -> IpAddr {
        forwarded_for_ip(h, peer, hops)
    }

    /// With no trusted hops configured, the header is not read at all, however
    /// well-formed it looks.
    #[test]
    fn hops_zero_meters_the_peer() {
        assert_eq!(keyed(&headers(&[]), PEER, 0), PEER);
        assert_eq!(keyed(&headers(&["203.0.113.9"]), PEER, 0), PEER);
        assert_eq!(
            keyed(&headers(&["203.0.113.9, 198.51.100.7"]), PEER, 0),
            PEER
        );
    }

    #[test]
    fn hops_one_meters_the_rightmost_entry() {
        assert_eq!(
            keyed(&headers(&["203.0.113.9"]), PEER, 1),
            addr("203.0.113.9")
        );
        assert_eq!(
            keyed(&headers(&["198.51.100.7, 203.0.113.9"]), PEER, 1),
            addr("203.0.113.9")
        );
        // A chain may arrive split across several header lines rather than one
        // comma list; order across them is the same order.
        assert_eq!(
            keyed(&headers(&["198.51.100.7", "203.0.113.9"]), PEER, 1),
            addr("203.0.113.9")
        );
    }

    /// The whole point: what the caller writes is to the LEFT of what the proxy
    /// appends, so no amount of it changes the key.
    #[test]
    fn stuffing_entries_on_the_left_mints_no_identity() {
        let real = keyed(&headers(&["203.0.113.9"]), PEER, 1);
        for stuffed in [
            "1.1.1.1, 203.0.113.9",
            "2.2.2.2, 3.3.3.3, 203.0.113.9",
            "not-an-ip, 203.0.113.9",
            " , 203.0.113.9",
        ] {
            assert_eq!(keyed(&headers(&[stuffed]), PEER, 1), real, "{stuffed}");
        }
        // Nor across separate lines.
        assert_eq!(
            keyed(&headers(&["4.4.4.4", "5.5.5.5", "203.0.113.9"]), PEER, 1),
            real
        );
    }

    #[test]
    fn two_hops_skip_the_inner_proxy() {
        let h = headers(&["198.51.100.7, 203.0.113.9, 10.1.2.3"]);
        assert_eq!(keyed(&h, PEER, 2), addr("203.0.113.9"));
        assert_eq!(keyed(&h, PEER, 3), addr("198.51.100.7"));
    }

    /// Anything the header cannot answer is the shared bucket, never a
    /// caller-chosen value.
    #[test]
    fn fails_closed_to_the_peer() {
        // Missing entirely.
        assert_eq!(keyed(&headers(&[]), PEER, 1), PEER);
        // Present but empty, or only separators.
        assert_eq!(keyed(&headers(&[" "]), PEER, 1), PEER);
        assert_eq!(keyed(&headers(&[" , "]), PEER, 1), PEER);
        // Fewer entries than the operator claims hops: the entry that would be
        // read is one the caller wrote.
        assert_eq!(keyed(&headers(&["203.0.113.9"]), PEER, 2), PEER);
        assert_eq!(
            keyed(&headers(&["198.51.100.7, 203.0.113.9"]), PEER, 3),
            PEER
        );
        // The trusted position holds something that is not an address.
        assert_eq!(keyed(&headers(&["203.0.113.9, unknown"]), PEER, 1), PEER);
        // Not even valid UTF-8, so the value never becomes a `&str`.
        let mut h = HeaderMap::new();
        h.append(
            "x-forwarded-for",
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert_eq!(keyed(&h, PEER, 1), PEER);
    }

    /// Proxies that write a port, and the bracketed IPv6 form, still identify a
    /// client — dropping to the shared bucket for those would be a silent
    /// mis-metering on an otherwise correct deployment.
    #[test]
    fn accepts_the_port_and_bracketed_spellings() {
        assert_eq!(
            keyed(&headers(&["203.0.113.9:4711"]), PEER, 1),
            addr("203.0.113.9")
        );
        assert_eq!(
            keyed(&headers(&["[2001:db8::1]:4711"]), PEER, 1),
            addr("2001:db8::1")
        );
        assert_eq!(
            keyed(&headers(&["[2001:db8::1]"]), PEER, 1),
            addr("2001:db8::1")
        );
        assert_eq!(
            keyed(&headers(&["2001:db8::1"]), PEER, 1),
            addr("2001:db8::1")
        );
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

    /// The map stays bounded against a client that never reuses an address —
    /// exactly the case the idle TTL cannot reclaim.
    #[test]
    fn caps_the_bucket_count_against_address_rotation() {
        let mut l = RateLimiter::per_minute(10.0);
        let t = Instant::now();
        // Every address distinct, every bucket touched "now": nothing is ever
        // idle enough to prune.
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
        // Everything is older than the TTL, but only the first check past the
        // interval may prune.
        let later = t + IDLE_TTL + Duration::from_secs(1);
        l.check(ip(1), later);
        assert_eq!(l.len(), 1);
        for n in 0..PRUNE_AT as u32 {
            l.check(IpAddr::V4(Ipv4Addr::from(n)), later);
        }
        // Still inside PRUNE_EVERY, so no second sweep despite being over
        // PRUNE_AT again.
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
