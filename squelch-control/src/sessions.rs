//! Pending signup sessions: the server-side half of the flow, holding the two
//! values that must never ride in a cookie.
//!
//! - `state`, the CSRF token echoed by Google. Compared constant-time against
//!   what comes back on the callback.
//! - The PKCE verifier. This is what makes an intercepted authorization code
//!   worthless: whoever holds the code still cannot redeem it without the
//!   verifier, and the verifier lives here, in this process's memory, for ten
//!   minutes.
//!
//! In memory rather than in the store, deliberately. A restart drops pending
//! consents, and that is the correct behaviour: the recovery is to start the
//! signup again, and a verifier persisted to disk is a verifier that can be
//! read off disk.
//!
//! ONE-SHOT: [`SessionStore::take`] removes the session. A callback that is
//! replayed, or a code that is delivered twice, finds nothing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a session lives. The whole budget is one human reading one Google
/// consent screen. Matches [`crate::cookie::COOKIE_TTL_SECS`].
pub const SESSION_TTL: Duration = Duration::from_secs(10 * 60);

/// Hard ceiling on live sessions. Signup is rate limited per client, but the
/// table must be bounded by something that does not depend on the limiter's
/// identity model being right: past this, new signups are refused with a "try
/// again shortly" rather than the process growing without limit.
pub const MAX_SESSIONS: usize = 4_096;

/// What a pending signup is holding while the user is at Google.
///
/// NO `Debug`, and that is deliberate: a derived one would print the verifier
/// and the state into any `tracing` call that ever formats a session.
pub struct PendingSignup {
    /// CSRF token, echoed by Google as `state`.
    pub state: String,
    /// PKCE verifier. Never leaves this process except to Google, on the
    /// exchange.
    pub pkce_verifier: String,
    /// The validated label this signup provisions.
    pub label: String,
    /// The invite row it will spend.
    pub invite_id: i64,
    created: Instant,
}

/// Why a session could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertError {
    /// The table is at [`MAX_SESSIONS`].
    Full,
}

#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<String, PendingSignup>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a pending signup under `sid`.
    ///
    /// Expired entries are purged first, so a deployment that has been running
    /// for a week is bounded by concurrent signups rather than by total ones.
    pub fn insert(
        &mut self,
        sid: String,
        state: String,
        pkce_verifier: String,
        label: String,
        invite_id: i64,
        now: Instant,
    ) -> Result<(), InsertError> {
        self.sweep(now);
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(InsertError::Full);
        }
        self.sessions.insert(
            sid,
            PendingSignup {
                state,
                pkce_verifier,
                label,
                invite_id,
                created: now,
            },
        );
        Ok(())
    }

    /// Remove and return a session, if it exists and is live. ONE-SHOT: a
    /// second call with the same id gets `None`, which is what makes a replayed
    /// callback inert.
    pub fn take(&mut self, sid: &str, now: Instant) -> Option<PendingSignup> {
        let session = self.sessions.remove(sid)?;
        // Expired is the same answer as absent. The entry is gone either way:
        // an expired session that stayed would be a slot an attacker could
        // keep probing.
        (now.saturating_duration_since(session.created) <= SESSION_TTL).then_some(session)
    }

    /// Drop every expired session. Called on insert and by the background
    /// sweep, so an idle process does not hold a table of dead entries.
    pub fn sweep(&mut self, now: Instant) -> usize {
        let before = self.sessions.len();
        self.sessions
            .retain(|_, s| now.saturating_duration_since(s.created) <= SESSION_TTL);
        before - self.sessions.len()
    }

    /// Live sessions. A COUNT is the only thing about this table that may be
    /// logged.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(store: &mut SessionStore, sid: &str, now: Instant) -> Result<(), InsertError> {
        store.insert(
            sid.to_string(),
            "state".into(),
            "verifier".into(),
            "ada".into(),
            1,
            now,
        )
    }

    #[test]
    fn a_session_is_taken_exactly_once() {
        let mut s = SessionStore::new();
        let now = Instant::now();
        insert(&mut s, "sid", now).unwrap();
        assert_eq!(s.len(), 1);

        let taken = s.take("sid", now).expect("live");
        assert_eq!(taken.label, "ada");
        assert_eq!(taken.pkce_verifier, "verifier");
        assert!(s.take("sid", now).is_none(), "replay finds nothing");
        assert!(s.is_empty());
    }

    #[test]
    fn an_expired_session_is_gone_and_unreplayable() {
        let mut s = SessionStore::new();
        let now = Instant::now();
        insert(&mut s, "sid", now).unwrap();
        let later = now + SESSION_TTL + Duration::from_secs(1);
        assert!(s.take("sid", later).is_none());
        assert!(s.is_empty(), "the entry is removed, not merely refused");
    }

    #[test]
    fn expired_sessions_are_swept() {
        let mut s = SessionStore::new();
        let now = Instant::now();
        insert(&mut s, "a", now).unwrap();
        insert(&mut s, "b", now + Duration::from_secs(60)).unwrap();
        let later = now + SESSION_TTL + Duration::from_secs(1);
        assert_eq!(s.sweep(later), 1);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn the_table_is_bounded() {
        let mut s = SessionStore::new();
        let now = Instant::now();
        for i in 0..MAX_SESSIONS {
            insert(&mut s, &format!("sid{i}"), now).unwrap();
        }
        assert_eq!(insert(&mut s, "one-too-many", now), Err(InsertError::Full));
        // ...and the ceiling forgives once the sessions age out.
        let later = now + SESSION_TTL + Duration::from_secs(1);
        insert(&mut s, "later", later).unwrap();
        assert_eq!(s.len(), 1);
    }
}
