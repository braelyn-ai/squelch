//! Recipient autocomplete over the Sent-derived contacts table, plus the
//! Sent-history harvest's merge. Search is HUMAN-DOOR ONLY (`/client/contacts`);
//! the agent door never learns the table exists.

use super::*;

impl SqliteStore {
    /// HUMAN-DOOR ONLY: rank contacts for a typed fragment. Prefix matches (on
    /// the address or the display name) sort above substring matches, then the
    /// user's OWN address, then by how often and how recently the user has
    /// written to them.
    ///
    /// Self is ranked WITHIN the tier the fragment earned, never above it: for
    /// your own address you are the answer and not the person you write to
    /// most, but a fragment that prefix-matches somebody else and merely
    /// appears somewhere inside your address still belongs to them.
    pub fn search_contacts(
        &self,
        account_id: AccountId,
        q: &str,
        limit: u32,
    ) -> Result<Vec<ContactEntry>> {
        let q = q.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        // LIKE metacharacters in the fragment are literal text to the user.
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let contains = format!("%{escaped}%");
        let prefix = format!("{escaped}%");

        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT addr, display_name, sent_count, last_sent_at FROM contacts
             WHERE account_id = ?1
               AND (addr LIKE ?2 ESCAPE '\\' OR display_name LIKE ?2 ESCAPE '\\')
             ORDER BY (addr LIKE ?3 ESCAPE '\\' OR display_name LIKE ?3 ESCAPE '\\') DESC,
                      (lower(addr) = (SELECT lower(trim(email)) FROM accounts
                                      WHERE id = ?1)) DESC,
                      sent_count DESC,
                      COALESCE(last_sent_at, '') DESC,
                      addr ASC
             LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![account_id, contains, prefix, limit], |r| {
                Ok(ContactEntry {
                    addr: r.get(0)?,
                    display_name: r.get(1)?,
                    sent_count: r.get(2)?,
                    last_sent_at: dt_opt(r, 3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Merge the Sent-history harvest's aggregate. MAX/COALESCE semantics, not
    /// increments: the harvest overlaps mail the ingest path already counted,
    /// and a re-run after an interrupted pass must be idempotent.
    pub fn merge_harvested_contacts(
        &self,
        account_id: AccountId,
        batch: &[ContactEntry],
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        for c in batch {
            if c.addr.trim().is_empty() {
                continue;
            }
            let last_sent = c.last_sent_at.map(|d| d.to_rfc3339());
            tx.execute(
                "INSERT INTO contacts(account_id, addr, sent_count, first_seen,
                                      last_sent_at, display_name)
                 VALUES(?1,?2,?3,COALESCE(?4,''),?4,?5)
                 ON CONFLICT(account_id, addr) DO UPDATE SET
                     sent_count = MAX(sent_count, excluded.sent_count),
                     last_sent_at = NULLIF(
                         MAX(COALESCE(last_sent_at,''), COALESCE(excluded.last_sent_at,'')), ''),
                     display_name = COALESCE(excluded.display_name, display_name)",
                params![account_id, c.addr, c.sent_count, last_sent, c.display_name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn entry(addr: &str, name: Option<&str>, count: i64, day: u32) -> ContactEntry {
        ContactEntry {
            addr: addr.into(),
            display_name: name.map(Into::into),
            sent_count: count,
            last_sent_at: Some(Utc.with_ymd_and_hms(2026, 7, day, 12, 0, 0).unwrap()),
        }
    }

    fn store_with(entries: &[ContactEntry]) -> (SqliteStore, AccountId) {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        store.merge_harvested_contacts(acct, entries).unwrap();
        (store, acct)
    }

    #[test]
    fn prefix_beats_substring_then_count() {
        let (store, acct) = store_with(&[
            entry("zalice@x.com", None, 50, 1),
            entry("alice@x.com", Some("Alice"), 2, 1),
            entry("albert@x.com", None, 9, 1),
        ]);
        let hits = store.search_contacts(acct, "al", 10).unwrap();
        let addrs: Vec<_> = hits.iter().map(|h| h.addr.as_str()).collect();
        // Both prefix matches first (count-ordered), the substring match last.
        assert_eq!(addrs, ["albert@x.com", "alice@x.com", "zalice@x.com"]);
    }

    #[test]
    fn name_matches_too() {
        let (store, acct) = store_with(&[entry("a.j@x.com", Some("Alice Johnson"), 1, 1)]);
        let hits = store.search_contacts(acct, "johns", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display_name.as_deref(), Some("Alice Johnson"));
    }

    #[test]
    fn like_metacharacters_are_literal() {
        let (store, acct) = store_with(&[entry("percent@x.com", None, 1, 1)]);
        assert!(store.search_contacts(acct, "%", 10).unwrap().is_empty());
        assert!(store.search_contacts(acct, "_", 10).unwrap().is_empty());
    }

    #[test]
    fn merge_is_idempotent_and_takes_max() {
        let (store, acct) = store_with(&[entry("bob@x.com", None, 3, 10)]);
        // Re-run with a lower count and older date: nothing regresses. A name
        // arriving later fills in.
        store
            .merge_harvested_contacts(acct, &[entry("bob@x.com", Some("Bob"), 1, 2)])
            .unwrap();
        let hits = store.search_contacts(acct, "bob", 10).unwrap();
        assert_eq!(hits[0].sent_count, 3);
        assert_eq!(hits[0].display_name.as_deref(), Some("Bob"));
        assert_eq!(
            hits[0].last_sent_at.unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap()
        );
    }

    #[test]
    fn your_own_address_is_a_contact_you_never_had_to_earn() {
        // Nothing in the mail path seeds it — both the Sent seed and the harvest
        // strip self — so `ensure_account` is the only thing standing between
        // the user and being a stranger to their own inbox.
        let (store, acct) = store_with(&[]);
        assert!(store.is_known_contact(acct, "me@example.com").unwrap());
        assert!(
            store.is_known_contact(acct, "ME@Example.COM").unwrap(),
            "the header spells it however it likes"
        );
        let hits = store.search_contacts(acct, "me@ex", 10).unwrap();
        assert_eq!(hits[0].addr, "me@example.com");
        assert_eq!(hits[0].display_name.as_deref(), Some("Me"));
    }

    #[test]
    fn typing_me_offers_yourself_first() {
        // The display name earns its keep here: "me" is how a person reaches for
        // their own address, and it outranks the contact they write to most.
        let (store, acct) = store_with(&[entry("mel@x.com", Some("Mel"), 99, 1)]);
        let hits = store.search_contacts(acct, "me", 10).unwrap();
        assert_eq!(hits[0].addr, "me@example.com");
        assert_eq!(hits[1].addr, "mel@x.com");
    }

    #[test]
    fn self_ranks_inside_its_tier_not_above_it() {
        // "exa" prefix-matches this contact and only appears INSIDE the user's
        // own address. The fragment picked its owner; self does not jump it.
        let (store, acct) = store_with(&[entry("example@x.com", None, 1, 1)]);
        let addrs: Vec<_> = store
            .search_contacts(acct, "exa", 10)
            .unwrap()
            .into_iter()
            .map(|h| h.addr)
            .collect();
        assert_eq!(addrs, ["example@x.com", "me@example.com"]);
    }

    #[test]
    fn reseeding_self_never_stomps_the_row() {
        // `ensure_account` runs on every daemon start. Suppose the row has moved
        // since (a name the user set, a count real mail bumped): a restart must
        // leave it exactly where it is, and must not double it.
        let (store, acct) = store_with(&[]);
        store
            .merge_harvested_contacts(acct, &[entry("me@example.com", Some("Braelyn"), 7, 11)])
            .unwrap();
        assert_eq!(store.ensure_account("me@example.com").unwrap(), acct);
        let hits = store.search_contacts(acct, "me@example.com", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sent_count, 7);
        assert_eq!(hits[0].display_name.as_deref(), Some("Braelyn"));
    }

    #[test]
    fn empty_query_returns_nothing() {
        let (store, acct) = store_with(&[entry("bob@x.com", None, 1, 1)]);
        assert!(store.search_contacts(acct, "  ", 10).unwrap().is_empty());
    }
}
