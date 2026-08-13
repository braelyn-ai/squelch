//! Specialist extractions — shipments, receipts, banking, calendar and
//! marketing — plus the receipt-to-open-bill auto-close.

use super::*;

/// The `SELECT id` every `(account_id, message_id)`-keyed upsert below needs to
/// report the row it just wrote — UPSERT gives no usable `last_insert_rowid` on
/// the conflict path.
///
/// `table` is interpolated, so it MUST stay a hardcoded literal at every call
/// site; only the two key values are ever caller-supplied, and both are bound.
fn select_row_id(
    conn: &Connection,
    table: &str,
    account_id: AccountId,
    message_id: i64,
) -> Result<i64> {
    let id: i64 = conn.query_row(
        &format!("SELECT id FROM {table} WHERE account_id=?1 AND message_id=?2"),
        params![account_id, message_id],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Upsert a shipment keyed by `(account_id, tracking_number)` in the caller's
/// transaction. A repeat applies the no-regress status state machine
/// ([`crate::triage::ShipmentStatus::merge`]) — a delivered shipment is never
/// walked back — and adopts a more informative `item_name` (never over an
/// LLM-sourced one; see `item_name_source`) or a carrier more specific than
/// "unknown". `last_update`/`last_message_id` advance only when the merge
/// accepts the incoming status, so a stale duplicate never becomes the row's
/// click target.
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
pub(super) fn upsert_shipment_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    s: &crate::triage::ShipmentInfo,
    seen_at: DateTime<Utc>,
) -> Result<i64> {
    use crate::triage::ShipmentStatus;

    let ts = seen_at.to_rfc3339();

    // Read any existing row so the merge runs in Rust rather than a SQL CASE.
    let existing: Option<(i64, String, String, String, String)> = conn
        .query_row(
            "SELECT id, status, item_name, item_name_source, carrier FROM shipments
             WHERE account_id=?1 AND tracking_number=?2",
            params![account_id, s.tracking_number],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;

    match existing {
        None => {
            conn.execute(
                "INSERT INTO shipments(account_id, tracking_number, carrier, item_name,
                     status, tracking_url, last_message_id, first_seen, last_update)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                params![
                    account_id,
                    s.tracking_number,
                    s.carrier,
                    s.item_name,
                    s.status.as_str(),
                    s.tracking_url,
                    message_id,
                    ts,
                ],
            )?;
            let id: i64 = conn.query_row(
                "SELECT id FROM shipments WHERE account_id=?1 AND tracking_number=?2",
                params![account_id, s.tracking_number],
                |r| r.get(0),
            )?;
            Ok(id)
        }
        Some((id, cur_status_s, cur_item, cur_item_source, cur_carrier)) => {
            let cur_status =
                ShipmentStatus::parse(&cur_status_s).unwrap_or(ShipmentStatus::Shipped);
            let merged = ShipmentStatus::merge(cur_status, s.status);

            // Prefer a more informative item name — but a regex-extracted name
            // NEVER replaces an LLM-extracted one (the LLM path is
            // `shipping_apply`, which stamps item_name_source='llm'). This path
            // never changes the source column: a kept llm name stays 'llm', a
            // regex adoption stays 'regex'.
            let item_name = if cur_item_source != "llm"
                && !s.item_name.is_empty()
                && (cur_item.is_empty() || s.item_name.len() > cur_item.len())
            {
                s.item_name.clone()
            } else {
                cur_item
            };
            // Prefer a concrete carrier over a prior "unknown".
            let (carrier, tracking_url) = if cur_carrier == "unknown" && s.carrier != "unknown" {
                (s.carrier.clone(), s.tracking_url.clone())
            } else {
                (cur_carrier, None) // tracking_url handled below (keep existing)
            };

            // The message pointer and clock advance only when the merge ACCEPTS
            // the incoming status: a stale out-of-order email (a late "shipped"
            // after delivered) must not become the row's click target or bump
            // last_update. Its better item name / carrier is still welcome.
            let accepted = merged == s.status;

            // When we kept the existing carrier, don't clobber a good tracking_url
            // with NULL — only update the url when we switched carrier.
            if carrier == s.carrier && s.carrier != "unknown" {
                conn.execute(
                    "UPDATE shipments SET status=?1, item_name=?2, carrier=?3,
                         tracking_url=?4,
                         last_message_id = CASE WHEN ?5 THEN ?6 ELSE last_message_id END,
                         last_update     = CASE WHEN ?5 THEN ?7 ELSE last_update END
                     WHERE id=?8",
                    params![
                        merged.as_str(),
                        item_name,
                        carrier,
                        s.tracking_url,
                        accepted,
                        message_id,
                        ts,
                        id,
                    ],
                )?;
            } else {
                let _ = tracking_url; // existing url retained
                conn.execute(
                    "UPDATE shipments SET status=?1, item_name=?2,
                         last_message_id = CASE WHEN ?3 THEN ?4 ELSE last_message_id END,
                         last_update     = CASE WHEN ?3 THEN ?5 ELSE last_update END
                     WHERE id=?6",
                    params![merged.as_str(), item_name, accepted, message_id, ts, id],
                )?;
            }
            Ok(id)
        }
    }
}

/// Upsert a receipt keyed by `(account_id, message_id)` in the caller's
/// transaction; a re-ingest overwrites in place.
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
pub(super) fn upsert_receipt_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    from_addr: &str,
    from_name: Option<&str>,
    r: &crate::triage::ReceiptInfo,
    received_at: DateTime<Utc>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO receipts(account_id, message_id, from_addr, from_name,
             amount, currency, received_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             from_addr=excluded.from_addr, from_name=excluded.from_name,
             amount=excluded.amount, currency=excluded.currency,
             received_at=excluded.received_at",
        params![
            account_id,
            message_id,
            from_addr,
            from_name,
            r.amount,
            r.currency,
            received_at.to_rfc3339(),
        ],
    )?;
    select_row_id(conn, "receipts", account_id, message_id)
}

/// Upsert a banking row keyed by `(account_id, message_id)` in the caller's
/// transaction; a re-run overwrites in place.
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
fn upsert_banking_conn(conn: &Connection, applied: &BankingApplied) -> Result<i64> {
    conn.execute(
        "INSERT INTO banking(account_id, message_id, kind, institution, amount,
             currency, account_hint, received_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             kind=excluded.kind, institution=excluded.institution,
             amount=excluded.amount, currency=excluded.currency,
             account_hint=excluded.account_hint, received_at=excluded.received_at",
        params![
            applied.account_id,
            applied.message_id,
            applied.kind,
            applied.institution,
            applied.amount,
            applied.currency,
            applied.account_hint,
            applied.received_at.to_rfc3339(),
        ],
    )?;
    select_row_id(conn, "banking", applied.account_id, applied.message_id)
}

/// Upsert a calendar update keyed by `(account_id, message_id)` in the caller's
/// transaction; a re-ingest overwrites in place.
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
pub(super) fn upsert_calendar_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    c: &crate::triage::CalendarInfo,
    received_at: DateTime<Utc>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO calendar_updates(account_id, message_id, kind, event_title,
             starts_at, organizer, received_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             kind=excluded.kind, event_title=excluded.event_title,
             starts_at=excluded.starts_at, organizer=excluded.organizer,
             received_at=excluded.received_at",
        params![
            account_id,
            message_id,
            c.kind.as_str(),
            c.event_title,
            c.starts_at.map(|d| d.to_rfc3339()),
            c.organizer,
            received_at.to_rfc3339(),
        ],
    )?;
    select_row_id(conn, "calendar_updates", account_id, message_id)
}

/// RECEIPT -> OPEN-BILL AUTO-CLOSE: resolve the one OPEN bill (a `deadlines` row
/// whose triage status != 'done') a just-ingested receipt plausibly settles,
/// inside the caller's ingest transaction so both land atomically.
///
/// Matching is the pure logic in [`crate::triage::receipt_match`], biased to
/// precision because a false auto-close hides an unpaid bill: merchant identity
/// by registrable domain or normalized display name; amounts must agree within
/// cents when both parse, and a parsed bill against an unparsed receipt refuses;
/// recency windows anchored on the two `received_at`s. At most ONE bill closes
/// per receipt, the EARLIEST-due match — recurring bills leave identical open
/// months, and closing both would hide an unpaid one.
///
/// The close appends an `audit_log` row (actor="ingest",
/// action="bill.auto_close") so the human door can answer "where did my bill
/// go?". Idempotent: a re-ingest finds the bill already 'done'.
pub(super) fn auto_close_bill_for_receipt_conn(
    conn: &Connection,
    account_id: AccountId,
    receipt_message_id: i64,
    from_addr: &str,
    from_name: Option<&str>,
    r: &crate::triage::ReceiptInfo,
    received_at: DateTime<Utc>,
) -> Result<Option<i64>> {
    use crate::triage::receipt_match;

    // Candidate OPEN bills: every deadline whose triage row is not yet done, the
    // message join supplying biller identity + recency anchor. The open set is
    // small, so the pure rules filter it in Rust.
    let mut stmt = conn.prepare(
        "SELECT d.message_id, d.amount, d.currency, d.due_at,
                m.from_addr, m.from_name, m.received_at
         FROM deadlines d
         JOIN triage t ON t.message_id = d.message_id
         JOIN messages m ON m.id = d.message_id
         WHERE d.account_id = ?1
           AND t.status != 'done'
           AND t.sensitivity != 'sealed'
           AND d.message_id != ?2",
    )?;
    let rows = stmt.query_map(params![account_id, receipt_message_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<f64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    // Best match = the EARLIEST-due bill that passes every rule.
    let mut best: Option<(i64, DateTime<Utc>, Option<f64>)> = None;
    for row in rows {
        let (bill_id, bill_amount, bill_currency, due_s, bill_addr, bill_name, bill_recv_s) = row?;

        // Currency sanity (v0 is USD-only, but never compare across currencies).
        if let (Some(rc), Some(bc)) = (r.currency.as_deref(), bill_currency.as_deref())
            && rc != bc
        {
            continue;
        }
        // Merchant identity is mandatory.
        if !receipt_match::merchant_matches(from_addr, from_name, &bill_addr, bill_name.as_deref())
        {
            continue;
        }
        // Amount rule picks the recency window (or refuses outright).
        let Some(window_days) = receipt_match::amounts_permit_close(r.amount, bill_amount) else {
            continue;
        };
        // Recency: the bill must PRECEDE the receipt (a payment follows its
        // bill), within the rule's window.
        let bill_recv = parse_dt(&bill_recv_s)?;
        let age = received_at - bill_recv;
        if age < chrono::Duration::zero() || age > chrono::Duration::days(window_days) {
            continue;
        }

        let due_at = parse_dt(&due_s)?;
        if best
            .as_ref()
            .is_none_or(|(_, best_due, _)| due_at < *best_due)
        {
            best = Some((bill_id, due_at, bill_amount));
        }
    }
    let Some((bill_id, _, bill_amount)) = best else {
        return Ok(None);
    };

    // 'done' stamps resolved_at, sealed is excluded, and the status guard makes
    // a re-run a no-op.
    let n = conn.execute(
        "UPDATE triage
         SET status = 'done', resolved_at = ?1
         WHERE account_id = ?2 AND message_id = ?3
           AND sensitivity != 'sealed' AND status != 'done'",
        params![Utc::now().to_rfc3339(), account_id, bill_id],
    )?;
    if n == 0 {
        return Ok(None); // raced/no-op — nothing closed, nothing to audit
    }

    // Record WHY in the audit log so the resolution is always explainable.
    let fmt_amt = |a: Option<f64>| a.map_or("unparsed".to_string(), |v| format!("${v:.2}"));
    conn.execute(
        "INSERT INTO audit_log(account_id, ts, actor, action, target, detail)
         VALUES(?1,?2,'ingest','bill.auto_close',?3,?4)",
        params![
            account_id,
            Utc::now().to_rfc3339(),
            bill_id.to_string(),
            format!(
                "receipt message {} from {} ({}) matched open bill (bill {})",
                receipt_message_id,
                from_addr,
                fmt_amt(r.amount),
                fmt_amt(bill_amount),
            ),
        ],
    )?;
    Ok(Some(bill_id))
}

impl SqliteStore {
    pub(super) fn upsert_shipment(
        &self,
        account_id: AccountId,
        message_id: i64,
        shipment: &crate::triage::ShipmentInfo,
        seen_at: DateTime<Utc>,
    ) -> Result<i64> {
        let conn = self.lock()?;
        upsert_shipment_conn(&conn, account_id, message_id, shipment, seen_at)
    }

    pub(super) fn list_shipments(
        &self,
        account_id: AccountId,
        include_delivered: bool,
    ) -> Result<Vec<crate::types::Shipment>> {
        let conn = self.lock()?;
        // No sealed rows: detection never runs on sealed mail, and sealing an
        // already-extracted message deletes its shipment row (correct_triage).
        // LEFT JOIN: a NULL last_message_id (row written by an older daemon)
        // leaves the shipment standing, just with nowhere to jump to.
        let mut sql = String::from(
            "SELECT s.id, s.account_id, s.tracking_number, s.carrier, s.item_name, s.status,
                    s.tracking_url, s.first_seen, s.last_update, m.thread_id
             FROM shipments s
             LEFT JOIN messages m ON m.id = s.last_message_id AND m.account_id = s.account_id
             WHERE s.account_id=?1",
        );
        if !include_delivered {
            sql.push_str(" AND s.status != 'delivered'");
        }
        sql.push_str(" ORDER BY s.last_update DESC");
        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params![account_id], |r| {
                Ok(crate::types::Shipment {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    tracking_number: r.get(2)?,
                    carrier: r.get(3)?,
                    item_name: r.get(4)?,
                    status: r.get(5)?,
                    tracking_url: r.get(6)?,
                    first_seen: dt(r, 7)?,
                    last_update: dt(r, 8)?,
                    thread_id: r.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn upsert_receipt(
        &self,
        account_id: AccountId,
        message_id: i64,
        from_addr: &str,
        from_name: Option<&str>,
        receipt: &crate::triage::ReceiptInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64> {
        let conn = self.lock()?;
        upsert_receipt_conn(
            &conn,
            account_id,
            message_id,
            from_addr,
            from_name,
            receipt,
            received_at,
        )
    }

    pub(super) fn list_receipts(&self, account_id: AccountId, days: u32) -> Result<Vec<Receipt>> {
        let conn = self.lock()?;
        // No sealed filter needed: detection never runs on sealed mail, so the
        // table holds no sealed rows by construction.
        let since = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT rc.id, rc.account_id, rc.message_id, m.thread_id, rc.from_addr,
                    rc.from_name, rc.amount, rc.currency, rc.received_at
             FROM receipts rc
             JOIN messages m ON m.id = rc.message_id
             WHERE rc.account_id=?1 AND rc.received_at >= ?2
             ORDER BY rc.received_at DESC",
        )?;
        let out = stmt
            .query_map(params![account_id, since], |r| {
                Ok(Receipt {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    message_id: r.get(2)?,
                    thread_id: r.get(3)?,
                    from_addr: r.get(4)?,
                    from_name: r.get(5)?,
                    amount: r.get(6)?,
                    currency: r.get(7)?,
                    received_at: dt(r, 8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn banking_apply(&self, applied: &BankingApplied) -> Result<i64> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let id = upsert_banking_conn(&tx, applied)?;
        // Stamp the extractor marker (leaving the extract queue) and, for a
        // RECORD (statement/alert), resolve the row to 'done' so it leaves the
        // attention bands. The sensitivity='normal' guard keeps a sealed row from
        // ever being mutated here.
        let now_s = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE triage SET
                 extractor_model_used = ?3,
                 status = CASE WHEN ?4 = 1 THEN 'done' ELSE status END,
                 resolved_at = CASE WHEN ?4 = 1 THEN ?5 ELSE resolved_at END
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.extractor_model_used,
                applied.auto_resolve as i64,
                now_s,
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub(super) fn marketing_apply(&self, applied: &MarketingApplied) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO marketing(account_id, message_id, brand, offer, discount, code,
                                   expires_at, received_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(account_id, message_id) DO UPDATE SET
                 brand=excluded.brand,
                 offer=excluded.offer,
                 discount=excluded.discount,
                 code=excluded.code,
                 expires_at=excluded.expires_at",
            params![
                applied.account_id,
                applied.message_id,
                applied.brand,
                applied.offer,
                applied.discount,
                applied.code,
                applied.expires_at,
                applied.received_at.to_rfc3339(),
            ],
        )?;
        // Stamp the extractor marker so the row leaves the queue. NO status
        // change: marketing does not auto-resolve. The sensitivity='normal' guard
        // keeps a sealed row from ever being mutated here.
        tx.execute(
            "UPDATE triage SET extractor_model_used = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.extractor_model_used
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn shipping_apply(&self, applied: &ShippingApplied) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        if let Some(name) = applied.item_name.as_deref().filter(|n| !n.is_empty()) {
            // Find the shipment row this message belongs to. The tracking
            // number re-detected from the SAME message is the row's dedupe key,
            // so it wins; a message the detector cannot re-key falls back to
            // the row it last touched. Neither matching drops the name
            // SILENTLY — an item name with no shipment row is an orphan.
            let read = |sql: &str, key: &dyn rusqlite::ToSql| {
                tx.query_row(sql, params![applied.account_id, key], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .optional()
            };
            let mut row: Option<(i64, String, String)> = None;
            if let Some(tn) = applied.tracking_number.as_deref() {
                row = read(
                    "SELECT id, item_name, item_name_source FROM shipments
                     WHERE account_id=?1 AND tracking_number=?2",
                    &tn,
                )?;
            }
            if row.is_none() {
                row = read(
                    "SELECT id, item_name, item_name_source FROM shipments
                     WHERE account_id=?1 AND last_message_id=?2",
                    &applied.message_id,
                )?;
            }
            if let Some((id, cur_item, cur_source)) = row {
                // PROVENANCE MERGE: an llm name replaces a regex name outright;
                // an existing llm name only yields to a MORE INFORMATIVE llm
                // name (same longer-wins rule as the regex path uses within
                // its own source).
                let write =
                    cur_source != "llm" || cur_item.is_empty() || name.len() > cur_item.len();
                if write {
                    tx.execute(
                        "UPDATE shipments SET item_name=?1, item_name_source='llm' WHERE id=?2",
                        params![name, id],
                    )?;
                }
            }
        }
        // Stamp the extractor marker so the row leaves the queue. NO status
        // change: shipping does not auto-resolve. The sensitivity='normal' guard
        // keeps a sealed row from ever being mutated here.
        tx.execute(
            "UPDATE triage SET extractor_model_used = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.extractor_model_used
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn marketing_offers(
        &self,
        account_id: AccountId,
        days: u32,
        limit: u32,
    ) -> Result<Vec<MarketingOffer>> {
        let conn = self.lock()?;
        let since = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT k.message_id, m.thread_id, m.from_addr, m.subject, k.brand, k.offer,
                    k.discount, k.code, k.expires_at, k.received_at
             FROM marketing k
             JOIN messages m ON m.id = k.message_id AND m.account_id = k.account_id
             WHERE k.account_id = ?1 AND k.received_at >= ?2
             ORDER BY k.received_at DESC
             LIMIT ?3",
        )?;
        let out = stmt
            .query_map(params![account_id, since, limit], |r| {
                Ok(MarketingOffer {
                    message_id: r.get(0)?,
                    thread_id: r.get(1)?,
                    sender: r.get(2)?,
                    subject: r.get(3)?,
                    brand: r.get(4)?,
                    offer: r.get(5)?,
                    discount: r.get(6)?,
                    code: r.get(7)?,
                    expires_at: r.get(8)?,
                    received_at: dt(r, 9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn list_banking(&self, account_id: AccountId) -> Result<Vec<Banking>> {
        let conn = self.lock()?;
        // No sealed filter needed: extraction never runs on sealed mail, so the
        // table holds no sealed rows by construction.
        let mut stmt = conn.prepare(
            "SELECT b.id, b.message_id, m.thread_id, m.from_addr, b.kind, b.institution,
                    b.amount, b.currency, b.account_hint, b.received_at
             FROM banking b
             JOIN messages m ON m.id = b.message_id
             WHERE b.account_id=?1
             ORDER BY b.received_at DESC",
        )?;
        let out = stmt
            .query_map(params![account_id], |r| {
                Ok(Banking {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    thread_id: r.get(2)?,
                    from_addr: r.get(3)?,
                    kind: r.get(4)?,
                    institution: r.get(5)?,
                    amount: r.get(6)?,
                    currency: r.get(7)?,
                    account_hint: r.get(8)?,
                    received_at: dt(r, 9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn upsert_calendar_update(
        &self,
        account_id: AccountId,
        message_id: i64,
        calendar: &crate::triage::CalendarInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64> {
        let conn = self.lock()?;
        upsert_calendar_conn(&conn, account_id, message_id, calendar, received_at)
    }

    pub(super) fn list_calendar_updates(
        &self,
        account_id: AccountId,
        hours: u32,
    ) -> Result<Vec<CalendarUpdate>> {
        let conn = self.lock()?;
        // The window is on received_at, NOT the event's starts_at. No sealed
        // filter needed: detection never runs on sealed mail.
        let since = (Utc::now() - chrono::Duration::hours(hours as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.message_id, m.thread_id, c.kind, c.event_title, c.starts_at,
                    c.organizer, c.received_at
             FROM calendar_updates c
             JOIN messages m ON m.id = c.message_id
             WHERE c.account_id=?1 AND c.received_at >= ?2
             ORDER BY c.received_at DESC",
        )?;
        let out = stmt
            .query_map(params![account_id, since], |r| {
                Ok(CalendarUpdate {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    thread_id: r.get(2)?,
                    kind: r.get(3)?,
                    event_title: r.get(4)?,
                    starts_at: dt_opt(r, 5)?,
                    organizer: r.get(6)?,
                    received_at: dt(r, 7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }
}
