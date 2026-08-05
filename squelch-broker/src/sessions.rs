//! The session table: everything the broker knows, and it knows it for ten
//! minutes.
//!
//! A session is the consent URL a daemon built, the SHA-256 of a claim token
//! whose pre-image the broker never sees, and at most one parked outcome. It
//! lives IN MEMORY ONLY: an auth code is useful for seconds, so durability
//! would only mean keeping other people's codes longer than anyone needs them.
//! A restart drops pending consents, and the recovery is re-running
//! `squelchd auth`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Registration to expiry. Long enough for a human to walk to another device
/// and read a consent screen, short enough that a parked code is stale by the
/// time anyone could come looking for it.
pub const SESSION_TTL: Duration = Duration::from_secs(600);

/// Hard ceiling on live sessions. Registration is unauthenticated by design, so
/// without this the only thing between a stranger and this process's memory is
/// the rate limiter.
pub const MAX_SESSIONS: usize = 4096;

/// Ceiling on live sessions held by one client. A real daemon holds one, or two
/// while `squelchd auth --write` runs the read and write flows back to back;
/// this leaves room for a lab or a NAT and still means it takes 256 distinct
/// clients to fill [`MAX_SESSIONS`], rather than one.
///
/// Enforced only where a client can be identified: see
/// [`crate::Config::max_sessions_per_client`].
pub const MAX_SESSIONS_PER_CLIENT: usize = 16;

/// Who built the consent URL and who will claim the code. Only `SelfHost`
/// exists today; the hosted signup flow (`docs/HOSTED.md`) adds a kind whose
/// URL belongs to the confidential web client and whose claim path is the
/// control plane rather than a polling daemon. The store, the TTL, the
/// validation, and the callback machinery are shared: only those two ends
/// differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    SelfHost,
}

impl SessionKind {
    /// The log/wire spelling. Never a secret: it names a flow, not a session.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfHost => "self_host",
        }
    }
}

/// What the callback left behind. `Pending` until Google answers, then exactly
/// one of the other two for the rest of the session's life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parked {
    Pending,
    Code(String),
    Denied(String),
}

/// Why a registration was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// The session id is already live. Never an overwrite: that would let a
    /// stranger who guessed an id retarget a consent already in flight.
    Duplicate,
    /// The table is at [`MAX_SESSIONS`].
    Full,
    /// This client already holds its share of the table. Separate from `Full`
    /// because it is the client's own doing and says so: without it, one host
    /// holding every slot answers 503 to everybody else's first consent.
    ClientFull,
}

/// A registration, as the store takes it. A struct rather than a row of
/// positional arguments, because two of them are opaque strings and swapping
/// them would compile.
pub struct NewSession {
    pub session_id: String,
    pub kind: SessionKind,
    pub claim_token_hash: [u8; 32],
    pub auth_url: String,
    /// The client this registration came from, for the per-client cap. Behind a
    /// proxy with no trusted-hops config this is the proxy for everyone, which
    /// is why the cap is off in that case. NEVER LOGGED and never rendered.
    pub owner: IpAddr,
}

/// The result of a callback trying to park an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    Parked,
    /// Something is already parked here. First write wins.
    AlreadyUsed,
    Unknown,
}

/// The result of a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Pending,
    /// The parked code, and the session is gone with it.
    Complete(String),
    /// The user refused consent; the session is gone.
    Denied(String),
    /// The presented token did not hash to the registered value. The session
    /// survives: a wrong guess must not delete the real daemon's consent.
    Forbidden,
    Unknown,
}

struct Session {
    kind: SessionKind,
    claim_token_hash: [u8; 32],
    auth_url: String,
    owner: IpAddr,
    parked: Parked,
    created_at: Instant,
}

impl Session {
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created_at) >= SESSION_TTL
    }
}

/// The in-memory session table.
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a session. Sweeps first, so an expired id is reusable and both
    /// caps count only live sessions.
    ///
    /// `per_client` is the ceiling on sessions already held by this
    /// registration's owner, or `None` where clients cannot be told apart (see
    /// [`crate::Config::max_sessions_per_client`]).
    ///
    /// The sweep is a full-map scan on every insert. That is affordable
    /// precisely because the map is capped at [`MAX_SESSIONS`] and this is the
    /// rate-limited path; the alternative, a sweep on a clock, would let a
    /// burst of expired sessions hold the cap against live ones. The per-client
    /// count rides along on the same scan.
    pub fn register(
        &self,
        new: NewSession,
        per_client: Option<usize>,
        now: Instant,
    ) -> Result<(), RegisterError> {
        let mut sessions = self.lock();
        sessions.retain(|_, s| !s.expired(now));
        if sessions.contains_key(&new.session_id) {
            return Err(RegisterError::Duplicate);
        }
        // Before the global cap: one host filling the table must hear that it
        // is the one at fault, and must not be able to spend the last slot.
        if let Some(limit) = per_client {
            let held = sessions.values().filter(|s| s.owner == new.owner).count();
            if held >= limit {
                return Err(RegisterError::ClientFull);
            }
        }
        if sessions.len() >= MAX_SESSIONS {
            return Err(RegisterError::Full);
        }
        sessions.insert(
            new.session_id,
            Session {
                kind: new.kind,
                claim_token_hash: new.claim_token_hash,
                auth_url: new.auth_url,
                owner: new.owner,
                parked: Parked::Pending,
                created_at: now,
            },
        );
        Ok(())
    }

    /// The consent URL to forward a human to, or `None` when the session is
    /// unknown or expired. An expired hit is purged on the way out.
    pub fn auth_url(&self, session_id: &str, now: Instant) -> Option<String> {
        let mut sessions = self.lock();
        match sessions.get(session_id) {
            Some(s) if !s.expired(now) => Some(s.auth_url.clone()),
            Some(_) => {
                sessions.remove(session_id);
                None
            }
            None => None,
        }
    }

    /// This session's kind. The read side of the hosted seam: the callback and
    /// the claim path branch on it once a second kind exists.
    pub fn kind(&self, session_id: &str, now: Instant) -> Option<SessionKind> {
        let sessions = self.lock();
        sessions
            .get(session_id)
            .filter(|s| !s.expired(now))
            .map(|s| s.kind)
    }

    /// Park a callback outcome. FIRST WRITE WINS: a second callback for the
    /// same session changes nothing, so a replayed redirect cannot swap the
    /// code a daemon is about to claim for one an attacker holds.
    pub fn park(&self, session_id: &str, outcome: Parked, now: Instant) -> ParkOutcome {
        let mut sessions = self.lock();
        let Some(s) = sessions.get_mut(session_id) else {
            return ParkOutcome::Unknown;
        };
        if s.expired(now) {
            sessions.remove(session_id);
            return ParkOutcome::Unknown;
        }
        if !matches!(s.parked, Parked::Pending) {
            return ParkOutcome::AlreadyUsed;
        }
        s.parked = outcome;
        ParkOutcome::Parked
    }

    /// Claim a session's outcome against the hash of the presented token.
    ///
    /// ONE-TIME: reading the parked value and deleting the session happen under
    /// a single lock acquisition, so two daemons polling concurrently cannot
    /// both be handed the same code.
    pub fn claim(&self, session_id: &str, presented: &[u8; 32], now: Instant) -> ClaimOutcome {
        let mut sessions = self.lock();
        let Some(s) = sessions.get(session_id) else {
            return ClaimOutcome::Unknown;
        };
        if s.expired(now) {
            sessions.remove(session_id);
            return ClaimOutcome::Unknown;
        }
        if !ct_eq(&s.claim_token_hash, presented) {
            return ClaimOutcome::Forbidden;
        }
        if matches!(s.parked, Parked::Pending) {
            return ClaimOutcome::Pending;
        }
        match sessions.remove(session_id).map(|s| s.parked) {
            Some(Parked::Code(code)) => ClaimOutcome::Complete(code),
            Some(Parked::Denied(error)) => ClaimOutcome::Denied(error),
            // Unreachable: `Pending` returned above, under this same lock.
            _ => ClaimOutcome::Unknown,
        }
    }

    /// Drop every expired session. Lazy purging on access already bounds what
    /// anyone can observe; this bounds what an idle process holds.
    pub fn sweep(&self, now: Instant) -> usize {
        let mut sessions = self.lock();
        let before = sessions.len();
        sessions.retain(|_, s| !s.expired(now));
        before - sessions.len()
    }

    /// Live sessions, expired ones included until they are swept. A count is
    /// the only thing about this table that is safe to log.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Poisoning is RECOVERED, not propagated: every operation here is a whole
    /// map update under one lock, so there is no half-written invariant to
    /// inherit, and `.expect()` would brick every future consent while
    /// `/healthz` kept answering 200.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Branch-minimal comparison of two SHA-256 digests: every byte is examined
/// whatever the inputs are, so the compare cannot leak the registered hash a
/// prefix at a time. Lengths are equal by construction ([u8; 32]), which is
/// exactly why the hash, not the token, is what gets compared.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;

    const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth?x=1";

    fn hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    /// A registration from one client, with the token hash the tests own.
    fn new_session(id: &str, owner: IpAddr) -> NewSession {
        NewSession {
            session_id: id.to_string(),
            kind: SessionKind::SelfHost,
            claim_token_hash: hash(1),
            auth_url: AUTH_URL.to_string(),
            owner,
        }
    }

    fn store_with(id: &str, now: Instant) -> SessionStore {
        let s = SessionStore::new();
        s.register(new_session(id, ip(1)), None, now).unwrap();
        s
    }

    #[test]
    fn registers_and_serves_the_auth_url() {
        let t = Instant::now();
        let s = store_with("a", t);
        assert_eq!(s.auth_url("a", t).as_deref(), Some(AUTH_URL));
        assert_eq!(s.kind("a", t), Some(SessionKind::SelfHost));
        assert_eq!(s.auth_url("nope", t), None);
        assert_eq!(s.len(), 1);
    }

    /// A duplicate id is a 409, never an overwrite: overwriting would let
    /// anyone who guessed a live id retarget a consent already in flight.
    #[test]
    fn a_duplicate_id_is_refused_without_touching_the_live_session() {
        let t = Instant::now();
        let s = store_with("a", t);
        assert_eq!(
            s.register(
                NewSession {
                    claim_token_hash: hash(2),
                    auth_url: "https://accounts.google.com/o/oauth2/v2/auth?evil=1".into(),
                    ..new_session("a", ip(2))
                },
                None,
                t,
            ),
            Err(RegisterError::Duplicate)
        );
        assert_eq!(s.auth_url("a", t).as_deref(), Some(AUTH_URL));
        // And the original claim token still owns it.
        assert_eq!(s.claim("a", &hash(2), t), ClaimOutcome::Forbidden);
        assert_eq!(s.claim("a", &hash(1), t), ClaimOutcome::Pending);
    }

    #[test]
    fn expires_after_the_ttl() {
        let t = Instant::now();
        let s = store_with("a", t);
        let late = t + SESSION_TTL;

        assert_eq!(s.auth_url("a", late), None);
        assert_eq!(s.len(), 0, "an expired hit purges on access");

        let s = store_with("a", t);
        assert_eq!(s.claim("a", &hash(1), late), ClaimOutcome::Unknown);
        assert_eq!(s.len(), 0);

        let s = store_with("a", t);
        assert_eq!(
            s.park("a", Parked::Code("c".into()), late),
            ParkOutcome::Unknown
        );
        assert_eq!(s.len(), 0);

        // Still alive one instant before the deadline.
        let s = store_with("a", t);
        assert!(
            s.auth_url("a", t + SESSION_TTL - Duration::from_millis(1))
                .is_some()
        );
    }

    /// The whole point of the service: the code goes out exactly once.
    #[test]
    fn a_claim_is_one_time() {
        let t = Instant::now();
        let s = store_with("a", t);
        assert_eq!(s.claim("a", &hash(1), t), ClaimOutcome::Pending);

        assert_eq!(
            s.park("a", Parked::Code("the-code".into()), t),
            ParkOutcome::Parked
        );
        assert_eq!(
            s.claim("a", &hash(1), t),
            ClaimOutcome::Complete("the-code".into())
        );
        assert_eq!(s.claim("a", &hash(1), t), ClaimOutcome::Unknown);
        assert_eq!(s.len(), 0);
    }

    /// A denial is terminal too: the daemon learns why and the session goes.
    #[test]
    fn a_denial_is_claimed_once_and_deletes_the_session() {
        let t = Instant::now();
        let s = store_with("a", t);
        assert_eq!(
            s.park("a", Parked::Denied("access_denied".into()), t),
            ParkOutcome::Parked
        );
        assert_eq!(
            s.claim("a", &hash(1), t),
            ClaimOutcome::Denied("access_denied".into())
        );
        assert_eq!(s.claim("a", &hash(1), t), ClaimOutcome::Unknown);
    }

    /// A replayed redirect must not swap the code a daemon is about to claim.
    #[test]
    fn the_first_callback_write_wins() {
        let t = Instant::now();
        let s = store_with("a", t);
        assert_eq!(
            s.park("a", Parked::Code("first".into()), t),
            ParkOutcome::Parked
        );
        assert_eq!(
            s.park("a", Parked::Code("second".into()), t),
            ParkOutcome::AlreadyUsed
        );
        assert_eq!(
            s.park("a", Parked::Denied("access_denied".into()), t),
            ParkOutcome::AlreadyUsed
        );
        assert_eq!(
            s.claim("a", &hash(1), t),
            ClaimOutcome::Complete("first".into())
        );
    }

    /// A wrong token is a 403 that leaves the session alone; guessing must not
    /// be a way to destroy someone else's consent.
    #[test]
    fn a_wrong_claim_token_neither_reads_nor_deletes() {
        let t = Instant::now();
        let s = store_with("a", t);
        s.park("a", Parked::Code("the-code".into()), t);

        assert_eq!(s.claim("a", &hash(9), t), ClaimOutcome::Forbidden);
        assert_eq!(s.len(), 1);
        assert_eq!(
            s.claim("a", &hash(1), t),
            ClaimOutcome::Complete("the-code".into())
        );
    }

    #[test]
    fn caps_the_table_and_reclaims_it_by_expiry() {
        let t = Instant::now();
        let s = SessionStore::new();
        for i in 0..MAX_SESSIONS {
            s.register(new_session(&format!("s{i}"), ip(1)), None, t)
                .unwrap();
        }
        assert_eq!(
            s.register(new_session("one-too-many", ip(1)), None, t),
            Err(RegisterError::Full)
        );
        // The sweep on insert is what reopens the table, so a full one recovers
        // on its own a TTL later without any operator action.
        assert!(
            s.register(new_session("later", ip(1)), None, t + SESSION_TTL)
                .is_ok()
        );
        assert_eq!(s.len(), 1);
    }

    /// One host must not be able to hold the whole table against everyone
    /// else's first consent.
    #[test]
    fn caps_live_sessions_per_client() {
        let t = Instant::now();
        let s = SessionStore::new();
        let cap = Some(3);

        for i in 0..3 {
            s.register(new_session(&format!("a{i}"), ip(1)), cap, t)
                .unwrap();
        }
        assert_eq!(
            s.register(new_session("a3", ip(1)), cap, t),
            Err(RegisterError::ClientFull)
        );
        // Another client is untouched by the first one's exhaustion.
        assert!(s.register(new_session("b0", ip(2)), cap, t).is_ok());

        // A claimed session frees a slot immediately, and an expired one frees
        // it on the next registration's sweep.
        assert_eq!(
            s.claim("a0", &hash(1), t),
            ClaimOutcome::Pending,
            "pending must not release the slot"
        );
        assert_eq!(
            s.register(new_session("a3", ip(1)), cap, t),
            Err(RegisterError::ClientFull)
        );
        assert!(
            s.register(new_session("a3", ip(1)), cap, t + SESSION_TTL)
                .is_ok()
        );

        // `None` is the deployment that cannot tell clients apart: no cap.
        let s = SessionStore::new();
        for i in 0..MAX_SESSIONS_PER_CLIENT + 5 {
            s.register(new_session(&format!("c{i}"), ip(1)), None, t)
                .unwrap();
        }
    }

    /// The per-client refusal must land before the global one, so a host that
    /// has already had its share cannot take the last slot from a stranger.
    #[test]
    fn a_client_at_its_ceiling_cannot_spend_the_last_global_slot() {
        let t = Instant::now();
        let s = SessionStore::new();
        let cap = Some(2);
        s.register(new_session("a0", ip(1)), cap, t).unwrap();
        s.register(new_session("a1", ip(1)), cap, t).unwrap();
        for i in 0..MAX_SESSIONS - 2 {
            s.register(new_session(&format!("z{i}"), ip(9)), None, t)
                .unwrap();
        }
        assert_eq!(
            s.register(new_session("a2", ip(1)), cap, t),
            Err(RegisterError::ClientFull)
        );
    }

    #[test]
    fn sweeps_expired_sessions_without_touching_live_ones() {
        let t = Instant::now();
        let s = store_with("old", t);
        let later = t + SESSION_TTL - Duration::from_secs(1);
        s.register(new_session("new", ip(1)), None, later).unwrap();

        assert_eq!(s.sweep(t), 0);
        assert_eq!(s.sweep(t + SESSION_TTL), 1);
        assert_eq!(s.len(), 1);
        assert!(s.auth_url("new", t + SESSION_TTL).is_some());
        assert!(!s.is_empty());
    }

    /// Correctness at every byte position is as much of the constant-time
    /// property as a test can observe: a compare that short-circuits anywhere
    /// would still have to be wrong somewhere to be caught, and the loop's
    /// shape (no early return, `|=` over all 32 bytes) is what carries it.
    #[test]
    fn the_hash_compare_examines_every_byte() {
        let a = [7u8; 32];
        assert!(ct_eq(&a, &[7u8; 32]));
        for i in 0..32 {
            let mut b = a;
            b[i] ^= 0x01;
            assert!(!ct_eq(&a, &b), "byte {i} was not compared");
        }
        assert!(!ct_eq(&[0u8; 32], &[0xffu8; 32]));
    }
}
