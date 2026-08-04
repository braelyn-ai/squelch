//! The open buffer: the only state this relay keeps, and it keeps it briefly.
//!
//! A row is an OPAQUE token, a timestamp, and a user-agent string — never a
//! message, a mailbox, an address, or anything that names one. Only the user's
//! daemon knows which message a token belongs to, and the buffer is a queue, not
//! an archive: rows are deleted the moment the daemon acknowledges them.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use serde::Serialize;

/// Rows kept per token. A pixel can be re-fetched forever — every forward, every
/// proxy refetch, every scanner — so without a cap one token is an unbounded
/// write channel into the buffer.
pub const MAX_PER_TOKEN: usize = 50;

/// Rows past this age are dropped: a daemon that has been offline for a quarter
/// is not going to reconcile opens it can no longer place in a thread.
const RETENTION_SECS: i64 = 90 * 24 * 60 * 60;

/// The purge is a full-table scan and the cutoff moves by seconds, so it runs on
/// a clock rather than per insert.
const PURGE_EVERY: Duration = Duration::from_secs(600);

/// User agents are attacker-controlled and unbounded on the wire.
pub const MAX_USER_AGENT: usize = 512;

/// Rows returned by one drain, so a long-offline daemon reconciles in bounded
/// pages instead of one unbounded body.
pub const DRAIN_LIMIT: usize = 500;

/// AUTOINCREMENT (not a bare rowid) because the cursor is an ack: rows below it
/// are deleted, and a reused id would hide a later open behind an already-acked
/// number.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS opens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token TEXT NOT NULL,
    opened_at INTEGER NOT NULL,
    user_agent TEXT
);
CREATE INDEX IF NOT EXISTS idx_opens_token ON opens(token);
";

/// One buffered open, exactly as it goes back to the daemon.
#[derive(Debug, Clone, Serialize)]
pub struct Open {
    pub id: i64,
    pub token: String,
    pub opened_at: i64,
    pub user_agent: Option<String>,
}

/// The buffer. On-disk when a path is configured, in memory otherwise.
pub struct OpenStore {
    inner: Mutex<Inner>,
}

struct Inner {
    conn: Connection,
    last_purge: Instant,
}

impl OpenStore {
    /// Open the buffer at `path`, or in memory when `None` — in which case a
    /// restart drops whatever the daemon had not yet drained.
    pub fn open(path: Option<&Path>) -> rusqlite::Result<Self> {
        let conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                conn,
                last_purge: Instant::now(),
            }),
        })
    }

    /// Buffer one open. Beyond [`MAX_PER_TOKEN`] the row is dropped silently:
    /// the daemon already has the signal that matters.
    pub fn record(
        &self,
        token: &str,
        opened_at: i64,
        user_agent: Option<&str>,
    ) -> rusqlite::Result<()> {
        let mut g = self.lock();
        // One statement, so the count and the insert cannot interleave with a
        // concurrent open of the same token.
        g.conn.execute(
            "INSERT INTO opens (token, opened_at, user_agent)
             SELECT ?1, ?2, ?3
             WHERE (SELECT count(*) FROM opens WHERE token = ?1) < ?4",
            params![token, opened_at, user_agent, MAX_PER_TOKEN as i64],
        )?;
        g.purge_if_due(opened_at, Instant::now());
        Ok(())
    }

    /// Delete everything at or below `cursor`, then return up to
    /// [`DRAIN_LIMIT`] rows above it.
    ///
    /// The delete is the acknowledgement: a daemon presenting `cursor` is
    /// asserting it has durably stored every open up to that id. Both halves run
    /// in one transaction so a crash mid-drain either acks nothing or acks
    /// exactly what it returns.
    pub fn drain(&self, cursor: i64) -> rusqlite::Result<Vec<Open>> {
        let mut g = self.lock();
        let tx = g.conn.transaction()?;
        tx.execute("DELETE FROM opens WHERE id <= ?1", params![cursor])?;
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT id, token, opened_at, user_agent FROM opens
                 WHERE id > ?1 ORDER BY id LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![cursor, DRAIN_LIMIT as i64], |r| {
                Ok(Open {
                    id: r.get(0)?,
                    token: r.get(1)?,
                    opened_at: r.get(2)?,
                    user_agent: r.get(3)?,
                })
            })?;
            mapped.collect::<rusqlite::Result<Vec<Open>>>()?
        };
        tx.commit()?;
        Ok(rows)
    }

    /// Poisoning is RECOVERED, not propagated: a panic in one handler must not
    /// brick every future open, and each statement is atomic so there is no
    /// half-written invariant to inherit.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Inner {
    fn purge_if_due(&mut self, now_unix: i64, now: Instant) {
        if now.saturating_duration_since(self.last_purge) < PURGE_EVERY {
            return;
        }
        self.last_purge = now;
        // A failed sweep is not a failed open: the row is already buffered.
        if let Err(e) = self.conn.execute(
            "DELETE FROM opens WHERE opened_at < ?1",
            params![now_unix - RETENTION_SECS],
        ) {
            tracing::warn!(error = %e, "open buffer purge failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> OpenStore {
        OpenStore::open(None).unwrap()
    }

    #[test]
    fn records_and_drains_in_id_order() {
        let s = store();
        s.record("aaaa", 100, Some("Mozilla")).unwrap();
        s.record("bbbb", 101, None).unwrap();

        let rows = s.drain(0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].token, "aaaa");
        assert_eq!(rows[0].opened_at, 100);
        assert_eq!(rows[0].user_agent.as_deref(), Some("Mozilla"));
        assert_eq!(rows[1].token, "bbbb");
        assert!(rows[1].user_agent.is_none());
        assert!(rows[0].id < rows[1].id);
    }

    /// The cursor is an ack: presenting it deletes what it covers, and the ids
    /// never come back.
    #[test]
    fn the_cursor_acks_by_deleting() {
        let s = store();
        for i in 0..3 {
            s.record("aaaa", 100 + i, None).unwrap();
        }
        let rows = s.drain(0).unwrap();
        assert_eq!(rows.len(), 3);

        let acked = rows[1].id;
        let rest = s.drain(acked).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, rows[2].id);
        // Re-presenting a stale cursor cannot resurrect acked rows.
        assert_eq!(s.drain(0).unwrap().len(), 1);

        s.drain(rows[2].id).unwrap();
        assert!(s.drain(0).unwrap().is_empty());
    }

    /// Ids keep climbing after a drain, or a new open would land under a cursor
    /// the daemon has already acked and never be seen.
    #[test]
    fn ids_are_never_reused_after_an_ack() {
        let s = store();
        s.record("aaaa", 100, None).unwrap();
        let first = s.drain(0).unwrap()[0].id;
        s.drain(first).unwrap();
        s.record("aaaa", 101, None).unwrap();
        assert!(s.drain(first).unwrap()[0].id > first);
    }

    #[test]
    fn caps_rows_per_token() {
        let s = store();
        for i in 0..MAX_PER_TOKEN as i64 + 20 {
            s.record("aaaa", 100 + i, None).unwrap();
        }
        s.record("bbbb", 500, None).unwrap();
        let rows = s.drain(0).unwrap();
        assert_eq!(
            rows.iter().filter(|r| r.token == "aaaa").count(),
            MAX_PER_TOKEN
        );
        // The cap is per token, not global.
        assert_eq!(rows.iter().filter(|r| r.token == "bbbb").count(), 1);
    }

    #[test]
    fn drain_is_paged() {
        let s = store();
        for i in 0..DRAIN_LIMIT as i64 + 10 {
            s.record(&format!("t{i}"), 100 + i, None).unwrap();
        }
        let page = s.drain(0).unwrap();
        assert_eq!(page.len(), DRAIN_LIMIT);
        let rest = s.drain(page[DRAIN_LIMIT - 1].id).unwrap();
        assert_eq!(rest.len(), 10);
    }

    #[test]
    fn purges_rows_past_the_retention_window() {
        let s = store();
        let now = 1_800_000_000;
        s.record("aaaa", now - RETENTION_SECS - 1, None).unwrap();
        s.record("bbbb", now, None).unwrap();
        {
            let mut g = s.lock();
            g.last_purge = Instant::now() - PURGE_EVERY;
            g.purge_if_due(now, Instant::now());
        }
        let rows = s.drain(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token, "bbbb");
    }

    #[test]
    fn survives_a_reopen_of_the_same_file() {
        let dir = std::env::temp_dir().join(format!("squelch-relay-opens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opens.sqlite3");
        let _ = std::fs::remove_file(&path);

        OpenStore::open(Some(&path))
            .unwrap()
            .record("aaaa", 100, None)
            .unwrap();
        let s = OpenStore::open(Some(&path)).unwrap();
        assert_eq!(s.drain(0).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
