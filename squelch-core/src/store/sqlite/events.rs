//! The `events` notification log and the push `devices` registry it feeds.

use super::*;

// ---- `events` row mapping (shared by events_after / event_by_id) -----------

/// One `events` row, columns in SELECT order, into an [`Event`]. An unparseable
/// `kind`/`tier` falls back to the least-alarming value rather than erroring:
/// refusing to serve a stored row would stall a client's cursor at it forever.
fn map_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get(0)?,
        message_id: r.get(1)?,
        thread_id: r.get(2)?,
        kind: EventKind::parse(&r.get::<_, String>(3)?).unwrap_or(EventKind::Surfaced),
        tier: Tier::parse(&r.get::<_, String>(4)?).unwrap_or(Tier::Noise),
        importance: r.get::<_, i64>(5)?.clamp(0, 255) as u8,
        sender: r.get(6)?,
        one_line: r.get(7)?,
        deadline: r.get(8)?,
        created_at: dt(r, 9)?,
    })
}

// ---- `devices` row mapping -------------------------------------------------

/// One `devices` row, columns in SELECT order, into a [`Device`].
fn map_device(r: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: r.get(0)?,
        account_id: r.get(1)?,
        token: r.get(2)?,
        platform: r.get(3)?,
        tag: r.get(4)?,
        created_at: dt(r, 5)?,
        last_registered_at: dt(r, 6)?,
    })
}

impl SqliteStore {
    pub(super) fn append_event(&self, ev: &NewEvent) -> Result<Option<i64>> {
        let inserted = {
            let conn = self.lock()?;
            // INSERT OR IGNORE on UNIQUE(message_id): one event per message ever,
            // so a re-ingest or a second refined verdict stays silent (0 rows).
            let n = conn.execute(
                "INSERT OR IGNORE INTO events(account_id, message_id, thread_id, kind, tier,
                     importance, sender, one_line, deadline, created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    ev.account_id,
                    ev.message_id,
                    ev.thread_id,
                    ev.kind.as_str(),
                    ev.tier.as_str(),
                    ev.importance as i64,
                    ev.sender,
                    ev.one_line,
                    ev.deadline,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            if n == 0 {
                return Ok(None);
            }
            conn.last_insert_rowid()
        };
        // Poke the broadcast AFTER dropping the connection lock. The payload is
        // only a hint, so a send error (nobody listening) is not a failure.
        if let Some(tx) = self.event_notifier() {
            let _ = tx.send(inserted);
        }
        Ok(Some(inserted))
    }

    pub(super) fn events_after(
        &self,
        account_id: AccountId,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, message_id, thread_id, kind, tier, importance, sender, one_line,
                    deadline, created_at
             FROM events
             WHERE account_id = ?1 AND id > ?2
             ORDER BY id ASC
             LIMIT ?3",
        )?;
        let out = stmt
            .query_map(params![account_id, after_id, limit as i64], map_event)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn event_by_id(&self, account_id: AccountId, id: i64) -> Result<Option<Event>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, message_id, thread_id, kind, tier, importance, sender, one_line,
                        deadline, created_at
                 FROM events
                 WHERE account_id = ?1 AND id = ?2",
                params![account_id, id],
                map_event,
            )
            .optional()?;
        Ok(row)
    }

    pub(super) fn latest_event_id(&self, account_id: AccountId) -> Result<i64> {
        let conn = self.lock()?;
        // COALESCE, so an account with no events reports the 0 cursor.
        let id: i64 = conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM events WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub(super) fn upsert_device(
        &self,
        account_id: AccountId,
        token: &str,
        platform: &str,
        tag: Option<&str>,
    ) -> Result<Device> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        // UPSERT on UNIQUE(token); `created_at` is deliberately NOT touched on
        // conflict — first sight is a fact worth keeping.
        //
        // The conflict update's account-scoped WHERE is the security property:
        // adopting `excluded.account_id` would let a re-registration silently
        // repoint another account's device. A cross-account collision therefore
        // changes 0 rows, and that is reported rather than swallowed — returning
        // the other account's row would be worse than the rebind.
        let changed = conn.execute(
            "INSERT INTO devices(account_id, token, platform, tag, created_at, last_registered_at)
             VALUES(?1,?2,?3,?4,?5,?5)
             ON CONFLICT(token) DO UPDATE SET
                 platform=excluded.platform,
                 tag=excluded.tag,
                 last_registered_at=excluded.last_registered_at
             WHERE devices.account_id = excluded.account_id",
            params![account_id, token, platform, tag, now],
        )?;
        if changed == 0 {
            // States the RULE, never the token: this string reaches the human
            // door as a 400 body.
            return Err(CoreError::InvalidInput(
                "device token is already registered to another account".to_string(),
            ));
        }
        let device = conn.query_row(
            "SELECT id, account_id, token, platform, tag, created_at, last_registered_at
             FROM devices WHERE token = ?1",
            params![token],
            map_device,
        )?;
        Ok(device)
    }

    pub(super) fn list_devices(&self, account_id: AccountId) -> Result<Vec<Device>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, token, platform, tag, created_at, last_registered_at
             FROM devices WHERE account_id = ?1 ORDER BY id ASC",
        )?;
        let out = stmt
            .query_map(params![account_id], map_device)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn delete_device_by_token(
        &self,
        account_id: AccountId,
        token: &str,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM devices WHERE account_id = ?1 AND token = ?2",
            params![account_id, token],
        )?;
        Ok(n > 0)
    }
}
