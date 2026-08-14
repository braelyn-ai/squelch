//! Pending OAuth sessions: the server-side half of the flow, holding the two
//! values that must never ride in a cookie.
//!
//! - `state`, the CSRF token echoed by Google. Compared constant-time against
//!   what comes back on the callback.
//! - The PKCE verifier. This is what makes an intercepted authorization code
//!   worthless: whoever holds the code still cannot redeem it without the
//!   verifier, and the verifier lives here, in this process's memory, for ten
//!   minutes.
//!
//! THREE KINDS OF SESSION share this table, because they are the same
//! ten-minute hop through Google and differ only in what they ask for and what
//! they do on the way back. A SIGNUP holds an invite reservation and provisions
//! a tenant. A CONSOLE login holds nothing, spends nothing, and asks Google for
//! identity alone; it ends in a pairing code for a tenant that already exists.
//! An APP login is the console login with its one input removed: it does not
//! know which tenant it is for, because it finds that out from the mailbox
//! Google names. The [`SessionKind`] is what the callback branches on, and it is
//! the AUTHORITY: the cookie's copy of the same fact is checked against it,
//! never trusted in its place.
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

/// The only form of a session id allowed to touch the control store: lowercase
/// hex SHA-256.
///
/// The invite reservation has to name the session holding it, and the store's
/// standing rule is that it keeps hashes rather than credentials. Equality is
/// all a reservation ever asks of this value, so the preimage never needs to
/// leave memory.
pub fn fingerprint(sid: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(sid.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            // Writing to a String cannot fail.
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Hard ceiling on live sessions. Both flows are rate limited per client, but
/// the table must be bounded by something that does not depend on the limiter's
/// identity model being right: past this, new sessions are refused with a "try
/// again shortly" rather than the process growing without limit.
pub const MAX_SESSIONS: usize = 4_096;

/// Hard ceiling on live IDENTITY sessions, CARVED OUT OF [`MAX_SESSIONS`]
/// rather than added to it. Both login flows spend it: `GET /console/auth` and
/// `GET /app/auth`.
///
/// THE ASYMMETRY IS THE POINT. Either login opens a session with no credential
/// and no invite behind it, so they are the cheap half of this table to flood; a
/// signup has to present a plausible invite code first. Without a per-kind
/// ceiling, a stranger looping a login hop fills all [`MAX_SESSIONS`] slots and
/// every real signup is refused. With it, login flooding can consume at most
/// this many entries and a signup always has
/// `MAX_SESSIONS - MAX_IDENTITY_SESSIONS` slots it cannot be crowded out of.
///
/// ONE CEILING FOR BOTH, not one each. They are the same errand through the same
/// consent, and two budgets would only mean a flooder gets to spend twice for
/// asking the same question two ways.
///
/// The reverse is deliberately NOT true: a table filled with real signups will
/// refuse a login too. Provisioning a mailbox is the flow that cannot be retried
/// cheaply, so it is the one that gets the whole table when it needs it.
pub const MAX_IDENTITY_SESSIONS: usize = 512;

/// Which flow a pending session belongs to, and therefore what the callback
/// does with it.
///
/// The invite id rides INSIDE the signup variant rather than beside the kind,
/// so "a console login has no invite" is a fact the type carries instead of a
/// sentinel row somebody has to remember to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A signup: this session holds a reservation on `invite_id` and will
    /// provision the label it names.
    Signup { invite_id: i64 },
    /// A console login for a tenant that already exists. Spends nothing, holds
    /// nothing, and asks Google only who is signed in.
    Console,
    /// An app login: the same identity hop with no tenant named going in. The
    /// label on a session of this kind is EMPTY and stays empty, because the
    /// mailbox Google names on the way back is what decides which tenant this
    /// was for.
    App,
}

impl SessionKind {
    /// The invite row this session holds, if it holds one. `None` for either
    /// login, which is exactly what the release path wants: nothing to hand
    /// back.
    pub fn invite_id(self) -> Option<i64> {
        match self {
            SessionKind::Signup { invite_id } => Some(invite_id),
            SessionKind::Console | SessionKind::App => None,
        }
    }

    /// Whether this session is one of the two LOGINS, which share
    /// [`MAX_IDENTITY_SESSIONS`]. Asked as a question about the kind rather than
    /// spelled as a match at each call site, so a fourth kind cannot be added
    /// without deciding which budget it spends.
    pub fn is_identity(self) -> bool {
        matches!(self, SessionKind::Console | SessionKind::App)
    }
}

/// What a pending session is holding while the user is at Google.
///
/// NO `Debug`, and that is deliberate: a derived one would print the verifier
/// and the state into any `tracing` call that ever formats a session.
pub struct PendingSession {
    /// Which flow this is, and the invite it holds if it is a signup.
    pub kind: SessionKind,
    /// CSRF token, echoed by Google as `state`.
    pub state: String,
    /// PKCE verifier. Never leaves this process except to Google, on the
    /// exchange.
    pub pkce_verifier: String,
    /// The validated label: the one being provisioned (signup) or the one being
    /// signed in to (console).
    pub label: String,
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
    sessions: HashMap<String, PendingSession>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a pending session under `sid`.
    ///
    /// Expired entries are purged first, so a deployment that has been running
    /// for a week is bounded by concurrent sessions rather than by total ones.
    ///
    /// TWO CEILINGS, and either login meets both: the whole-table
    /// [`MAX_SESSIONS`], and the [`MAX_IDENTITY_SESSIONS`] share of it.
    pub fn insert(
        &mut self,
        sid: String,
        kind: SessionKind,
        state: String,
        pkce_verifier: String,
        label: String,
        now: Instant,
    ) -> Result<(), InsertError> {
        self.sweep(now);
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(InsertError::Full);
        }
        // THE CARVE-OUT. Counted rather than tracked in a counter: the sweep
        // above is already a walk of the same map, so this costs the insert
        // nothing it was not paying, and a count that cannot drift out of step
        // with the table is worth more here than a field.
        if kind.is_identity() && self.identity_len() >= MAX_IDENTITY_SESSIONS {
            return Err(InsertError::Full);
        }
        self.sessions.insert(
            sid,
            PendingSession {
                kind,
                state,
                pkce_verifier,
                label,
                created: now,
            },
        );
        Ok(())
    }

    /// Remove and return a session, if it exists and is live. ONE-SHOT: a
    /// second call with the same id gets `None`, which is what makes a replayed
    /// callback inert.
    pub fn take(&mut self, sid: &str, now: Instant) -> Option<PendingSession> {
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

    /// Live LOGIN sessions of either kind, which is what
    /// [`MAX_IDENTITY_SESSIONS`] bounds.
    pub fn identity_len(&self) -> usize {
        self.sessions.values().filter(|s| s.kind.is_identity()).count()
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
            SessionKind::Signup { invite_id: 1 },
            "state".into(),
            "verifier".into(),
            "ada".into(),
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

    /// The three kinds live in one table and come back as what they went in as.
    /// Neither login carries an invite, which is what makes the release path on
    /// the callback a no-op rather than a lookup for a row that never existed.
    #[test]
    fn a_login_session_holds_no_invite() {
        let mut s = SessionStore::new();
        let now = Instant::now();
        s.insert(
            "console".into(),
            SessionKind::Console,
            "state".into(),
            "verifier".into(),
            "ada".into(),
            now,
        )
        .unwrap();
        // An app login names no tenant going in: the label is empty until
        // Google says which mailbox this is.
        s.insert(
            "app".into(),
            SessionKind::App,
            "state".into(),
            "verifier".into(),
            String::new(),
            now,
        )
        .unwrap();
        insert(&mut s, "signup", now).unwrap();

        let console = s.take("console", now).expect("live");
        assert_eq!(console.kind, SessionKind::Console);
        assert_eq!(console.kind.invite_id(), None);
        assert_eq!(console.label, "ada");

        let app = s.take("app", now).expect("live");
        assert_eq!(app.kind, SessionKind::App);
        assert_eq!(app.kind.invite_id(), None);
        assert_eq!(app.label, "");

        let signup = s.take("signup", now).expect("live");
        assert_eq!(signup.kind, SessionKind::Signup { invite_id: 1 });
        assert_eq!(signup.kind.invite_id(), Some(1));
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

    /// The fingerprint is stable, distinct per session, and not the id itself:
    /// what the control store holds must not be usable as a session id.
    #[test]
    fn a_session_fingerprint_hides_its_id() {
        let a = fingerprint("a-session-id");
        assert_eq!(a, fingerprint("a-session-id"));
        assert_ne!(a, fingerprint("another-session-id"));
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!a.contains("a-session-id"));
    }

    fn insert_login(
        store: &mut SessionStore,
        kind: SessionKind,
        sid: &str,
        now: Instant,
    ) -> Result<(), InsertError> {
        store.insert(
            sid.to_string(),
            kind,
            "state".into(),
            "verifier".into(),
            "ada".into(),
            now,
        )
    }

    /// The logins' share of the table is CARVED OUT of it: a stranger looping
    /// `GET /console/auth` fills the ceiling and stops, and a signup posted at
    /// that moment still opens.
    ///
    /// ONE BUDGET FOR BOTH LOGINS, which is the half worth asserting: with a
    /// ceiling each, the same flooder gets to spend twice by alternating
    /// `/console/auth` and `/app/auth`.
    #[test]
    fn login_flooding_cannot_spend_signup_capacity() {
        let mut s = SessionStore::new();
        let now = Instant::now();
        for i in 0..MAX_IDENTITY_SESSIONS {
            // Alternating, so the shared ceiling is what stops this and not one
            // kind's own.
            let kind = if i % 2 == 0 { SessionKind::Console } else { SessionKind::App };
            insert_login(&mut s, kind, &format!("login{i}"), now).unwrap();
        }
        assert_eq!(s.identity_len(), MAX_IDENTITY_SESSIONS);
        for kind in [SessionKind::Console, SessionKind::App] {
            assert_eq!(
                insert_login(&mut s, kind, "one-login-too-many", now),
                Err(InsertError::Full),
                "the shared login ceiling holds for {kind:?}"
            );
        }
        // ...and the flow that cannot be retried cheaply is untouched: every
        // remaining slot in the table is still a signup's to take.
        for i in 0..(MAX_SESSIONS - MAX_IDENTITY_SESSIONS) {
            insert(&mut s, &format!("signup{i}"), now).expect("signup capacity survives the flood");
        }
        assert_eq!(s.len(), MAX_SESSIONS);
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
