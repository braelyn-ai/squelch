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

/// The `delivered_at` binding for a status the email path just accepted: the
/// seen-at stamp once it lands Delivered, else NULL. Every write pairs it with
/// `COALESCE(delivered_at, ?)`, so a NULL is a no-op and an existing stamp
/// (from an earlier email or a carrier poll) is never overwritten.
fn delivered_ts(status: crate::triage::ShipmentStatus, ts: &str) -> Option<String> {
    (status == crate::triage::ShipmentStatus::Delivered).then(|| ts.to_string())
}

/// `app_settings` key recording that the one-shot shipment re-detect
/// ([`SqliteStore::shipments_redetect_cleanup`]) has run for an account. Written
/// inside the same transaction as the pass's deletions, so "the flag is set" and
/// "the deletions happened" are one fact.
const SHIPMENTS_REDETECT_FLAG: &str = "shipments_redetect_v1";

/// The MERCHANT NAMESPACE an order reference lives in: the registrable domain of
/// the sender that supplied it, lowercased, or `""` when the address yields none.
///
/// An order reference is unique only WITHIN the shop that issued it — "Order
/// #1042" from two retailers is two purchases — so `order_ref` alone is not an
/// identity and every lookup pairs it with this. Reuses
/// [`receipt_match::registrable_domain`](crate::triage::receipt_match::registrable_domain),
/// the codebase's one definition of merchant identity, rather than adding a
/// second domain parser that would drift from it.
pub(super) fn merchant_key(from_addr: &str) -> String {
    crate::triage::receipt_match::registrable_domain(from_addr).unwrap_or_default()
}

/// The merchant namespace for one message: [`merchant_key`] over its sender.
/// `""` when the message is gone or its address has no derivable domain — a
/// namespace of its own, matching only other unattributable rows.
fn message_merchant(conn: &Connection, account_id: AccountId, message_id: i64) -> Result<String> {
    let from_addr: Option<String> = conn
        .query_row(
            "SELECT from_addr FROM messages WHERE account_id = ?1 AND id = ?2",
            params![account_id, message_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(from_addr.as_deref().map(merchant_key).unwrap_or_default())
}

/// Upsert a shipment keyed by `(account_id, tracking_number)` in the caller's
/// transaction. A repeat applies the no-regress status state machine
/// ([`crate::triage::ShipmentStatus::merge`]) — a delivered shipment is never
/// walked back — and adopts a more informative `item_name` (never over one the
/// EXTRACTOR wrote; see `item_name_source`) or a carrier more specific than
/// "unknown". `last_update`/`last_message_id` advance only when the merge
/// accepts the incoming status, so a stale duplicate never becomes the row's
/// click target.
///
/// PROVENANCE. `created_by_message_id` is written ONCE, on the INSERT, and never
/// updated: it is the only column that answers "which mail minted this row",
/// which the phantom reaping keys on. `item_name_msg` follows the name — it
/// moves to `message_id` exactly when this message's name is adopted — so
/// sealing a message can scrub the text it contributed wherever that landed.
/// `item_name_source` records the MECHANISM instead, and this path never writes
/// it: an insert takes the column's 'regex' default, and an update keeps
/// whatever is there.
///
/// AN ACCEPTED UPDATE UN-RETIRES THE ROW: `poll_failures` goes back to 0
/// alongside `last_message_id`. A carrier answering "no such number" retires a
/// shipment permanently — it leaves the poll queue AND, for an ambiguous number
/// shape, the client lists — and until this, a successful poll was the only
/// thing that could ever bring it back, which a retired row can no longer
/// produce. Mail is the other witness: another message that the no-regress state
/// machine BELIEVES is fresh evidence this parcel is real, so the count of
/// consecutive carrier rejections starts over. It is gated on `accepted` rather
/// than written on every upsert so a stale duplicate — the late "shipped" after
/// a delivery, which is exactly the mail the merge just rejected — cannot
/// resurrect a genuine phantom. A re-ingest of the SAME message also resets,
/// which costs at most one more retirement cycle for a number that deserved it.
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
                     status, tracking_url, last_message_id, first_seen, last_update,
                     delivered_at, created_by_message_id, item_name_msg)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8,?9,?7,?10)",
                params![
                    account_id,
                    s.tracking_number,
                    s.carrier,
                    s.item_name,
                    s.status.as_str(),
                    s.tracking_url,
                    message_id,
                    ts,
                    delivered_ts(s.status, &ts),
                    (!s.item_name.is_empty()).then_some(message_id),
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

            // ITEM NAME, by PROVENANCE. This is the REGEX path, so it never
            // touches a name the extractor wrote — longer-wins exists to pick
            // between two regex guesses, and against a model's "Anker USB-C
            // charger" it would keep subject junk purely for being longer. It
            // also never CHANGES `item_name_source`: a kept llm name stays 'llm',
            // an adoption here stays 'regex'.
            //
            // HEALING: a stored name that TODAY's stricter strip refuses (the
            // live "package now with its carrier!" rows, written before the
            // filler patterns existed) is worth less than nothing — the client's
            // own "Package via <carrier>" fallback beats it. So it yields to a
            // shorter real name, and an EMPTY extraction is allowed to clear it,
            // which is the only way those rows ever get better.
            let llm_owned = cur_item_source == "llm";
            let cur_is_junk = crate::triage::shipment::is_junk_item_name(&cur_item);
            let adopt_name = !llm_owned
                && if s.item_name.is_empty() {
                    cur_is_junk
                } else {
                    cur_item.is_empty() || cur_is_junk || s.item_name.len() > cur_item.len()
                };
            let item_name = if adopt_name {
                s.item_name.clone()
            } else {
                cur_item
            };
            // The name's PROVENANCE follows the name: this message's only when
            // this message's name won, and NULL when the adoption was a heal to
            // empty — nobody donated the absence of a name.
            let item_name_msg = (!item_name.is_empty()).then_some(message_id);
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
                         last_update     = CASE WHEN ?5 THEN ?7 ELSE last_update END,
                         poll_failures   = CASE WHEN ?5 THEN 0 ELSE poll_failures END,
                         delivered_at    = COALESCE(delivered_at, ?9),
                         item_name_msg   = CASE WHEN ?10 THEN ?11 ELSE item_name_msg END
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
                        delivered_ts(merged, &ts),
                        adopt_name,
                        item_name_msg,
                    ],
                )?;
            } else {
                let _ = tracking_url; // existing url retained
                conn.execute(
                    "UPDATE shipments SET status=?1, item_name=?2,
                         last_message_id = CASE WHEN ?3 THEN ?4 ELSE last_message_id END,
                         last_update     = CASE WHEN ?3 THEN ?5 ELSE last_update END,
                         poll_failures   = CASE WHEN ?3 THEN 0 ELSE poll_failures END,
                         delivered_at    = COALESCE(delivered_at, ?7),
                         item_name_msg   = CASE WHEN ?8 THEN ?9 ELSE item_name_msg END
                     WHERE id=?6",
                    params![
                        merged.as_str(),
                        item_name,
                        accepted,
                        message_id,
                        ts,
                        id,
                        delivered_ts(merged, &ts),
                        adopt_name,
                        item_name_msg,
                    ],
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

/// The `(id, tracking_number)` of every shipment row THIS message CREATED — the
/// ONLY rows a shipments-extractor apply is allowed to delete. Read before any
/// write, so the upsert below can never delete the row it just wrote.
///
/// Keyed on `created_by_message_id`, NOT `last_message_id`. The latter MOVES to
/// whichever mail most recently advanced the row, so a second email covering two
/// packages would make the first email's weeks-old package look like this
/// message's to reap — order reference, ETA, delivery stamp, poll state and all.
/// Provenance is immutable; the pointer is not.
fn shipments_fed_by(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, tracking_number FROM shipments
         WHERE account_id = ?1 AND created_by_message_id = ?2",
    )?;
    let out = stmt
        .query_map(params![account_id, message_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}

/// Delete the AMBIGUOUS-shaped rows in `fed`, sparing `keep` (the number the
/// model just confirmed). Shape-gated on purpose: a model false negative must
/// never destroy a real `1Z…` / `TBA…` / IMpb package, and only the bare
/// digit-runs can be phantoms of a retailer's item or order id in the first
/// place (see
/// [`is_ambiguous_tracking_shape`](crate::triage::is_ambiguous_tracking_shape)).
///
/// CALLED ONLY ON POSITIVE EVIDENCE that a fed row is a phantom: the model
/// returned a DIFFERENT tracking number for this mail, or said it is not a
/// shipment at all. An extraction that simply names no number is silence, not a
/// verdict — see [`SqliteStore::shipments_extract_apply`].
///
/// CARRIER EVIDENCE OVERRULES BOTH. A row a carrier has answered about
/// (`carrier_status_raw`) or been asked about (`last_polled_at`) is a real
/// package whatever a model or a regex now thinks of its shape, so the delete
/// refuses it in SQL rather than trusting every caller to remember.
fn delete_fed_phantoms(
    conn: &Connection,
    account_id: AccountId,
    fed: &[(i64, String)],
    keep: Option<&str>,
) -> Result<()> {
    for (id, number) in fed {
        if keep == Some(number.as_str()) {
            continue;
        }
        if crate::triage::is_ambiguous_tracking_shape(number) {
            conn.execute(
                "DELETE FROM shipments
                 WHERE account_id = ?1 AND id = ?2
                   AND carrier_status_raw IS NULL AND last_polled_at IS NULL",
                params![account_id, id],
            )?;
        }
    }
    Ok(())
}

/// A shipment row's current `item_name`.
fn shipment_item_name(conn: &Connection, shipment_id: i64) -> Result<String> {
    let name: String = conn.query_row(
        "SELECT item_name FROM shipments WHERE id = ?1",
        params![shipment_id],
        |r| r.get(0),
    )?;
    Ok(name)
}

/// Write an item name the EXTRACTOR produced, with both halves of its
/// provenance: `item_name_msg` (which message) and `item_name_source='llm'`
/// (which mechanism). Every caller is on the extractor's apply path, so the
/// source is a constant rather than an argument — a `'regex'` name is only ever
/// written by [`upsert_shipment_conn`], which does not go through here.
///
/// `name_msg` is NOT always the message being applied: a promoted staged order
/// donates a name the ORDER CONFIRMATION wrote, and sealing that confirmation
/// has to scrub it from wherever it was donated. A wrong id here silently makes
/// sealing a no-op for that text, so the provenance is a REQUIRED argument
/// rather than something a caller can forget to update.
fn set_shipment_item_name(
    conn: &Connection,
    shipment_id: i64,
    name: &str,
    name_msg: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE shipments SET item_name = ?2, item_name_msg = ?3,
             item_name_source = 'llm'
         WHERE id = ?1",
        params![shipment_id, name, name_msg],
    )?;
    Ok(())
}

/// The extractor's name write onto a row that may already carry one, applying
/// the PROVENANCE rule: an llm name replaces a regex name outright (a model that
/// read the body beats a phrase lifted out of a subject), while an existing llm
/// name only yields to a MORE INFORMATIVE llm name — the same longer-wins tie
/// break the regex path uses within its own source. Returns whether it wrote.
fn adopt_llm_item_name(
    conn: &Connection,
    shipment_id: i64,
    name: &str,
    name_msg: Option<i64>,
) -> Result<bool> {
    let (cur_name, cur_source): (String, String) = conn.query_row(
        "SELECT item_name, item_name_source FROM shipments WHERE id = ?1",
        params![shipment_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if cur_source == "llm" && !cur_name.is_empty() && name.len() <= cur_name.len() {
        return Ok(false);
    }
    set_shipment_item_name(conn, shipment_id, name, name_msg)?;
    Ok(true)
}

/// The shipments carrying `(merchant, order_ref)`, capped at TWO — the caller
/// only needs to tell none from one from several.
///
/// Multiplicity is legitimate: an order that splits into two boxes puts the same
/// reference on both. It is also exactly when an item name must not be donated —
/// the earlier `query_row` silently took whichever row SQLite handed back first,
/// so half the time the name landed on the wrong package.
fn shipments_by_order_ref(
    conn: &Connection,
    account_id: AccountId,
    merchant: &str,
    order_ref: &str,
) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, item_name FROM shipments
         WHERE account_id = ?1 AND order_merchant = ?2 AND order_ref = ?3
         LIMIT 2",
    )?;
    let rows = stmt
        .query_map(params![account_id, merchant, order_ref], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The projection every shipment read shares, in [`shipment_row`]'s order.
/// LEFT JOIN: a NULL `last_message_id` (a row written by an older daemon, or one
/// only a carrier poll has touched) leaves the shipment standing, just with
/// nowhere to jump to.
const SHIPMENT_COLUMNS: &str = "s.id, s.account_id, s.tracking_number, s.carrier,
            s.item_name, s.status, s.tracking_url, s.first_seen, s.last_update,
            m.thread_id, s.carrier_status_raw, s.eta, s.delivered_at, s.last_polled_at,
            s.poll_failures";

/// The tables [`SHIPMENT_COLUMNS`] is read from, split out so the listing can
/// append its own column (`cleared_at`, which the wire type deliberately does
/// not carry) without a second copy of the projection.
const SHIPMENT_FROM: &str = "FROM shipments s
     LEFT JOIN messages m ON m.id = s.last_message_id AND m.account_id = s.account_id";

fn shipment_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::types::Shipment> {
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
        carrier_status_raw: r.get(10)?,
        eta: dt_opt(r, 11)?,
        delivered_at: dt_opt(r, 12)?,
        last_polled_at: dt_opt(r, 13)?,
        poll_failures: r.get(14)?,
    })
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

    /// Apply one SHIPMENTS-EXTRACTOR verdict in ONE transaction — the identity
    /// merge rules. See [`Store::shipments_extract_apply`](crate::store::Store::shipments_extract_apply)
    /// for the contract; the reasoning for each branch is inline below.
    pub(super) fn shipments_extract_apply(
        &self,
        a: &crate::triage::extract::shipments::ShipmentsApplied,
    ) -> Result<bool> {
        use crate::triage::{ShipmentInfo, ShipmentStatus, shipment::tracking_url};

        let mut conn = self.lock()?;
        let tx = conn.transaction()?;

        // MARKER FIRST, guarded on sensitivity='normal'. Matching zero rows means
        // the message was SEALED between the queue read and here (or never
        // existed): commit the nothing we have done and write no derived row —
        // sealed mail must leave no trace in the shipments zone.
        let n = tx.execute(
            "UPDATE triage SET ship_extract_model = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![a.message_id, a.account_id, a.extractor_model_used],
        )?;
        if n == 0 {
            tx.commit()?;
            return Ok(false);
        }

        // The blast radius of this apply: rows THIS message CREATED. Every delete
        // below is bounded to it, so one mail's verdict can never reach a package
        // another mail established.
        let fed = shipments_fed_by(&tx, a.account_id, a.message_id)?;
        // The namespace this mail's order reference lives in — its sender's
        // registrable domain. Read once; every order_ref read and write pairs it.
        let merchant = message_merchant(&tx, a.account_id, a.message_id)?;

        // NEGATIVE VERDICT: the model read the mail and says it is not an inbound
        // package. That IS positive evidence about the rows it minted, so retire
        // its phantoms; keep everything else.
        if !a.is_shipment {
            delete_fed_phantoms(&tx, a.account_id, &fed, None)?;
            tx.commit()?;
            return Ok(false);
        }

        let wrote = if let Some(tn) = a.tracking_number.as_deref() {
            // IDENTITY: a carrier tracking number. Any OTHER ambiguous row this
            // message created was the regex detector reading an order id as a
            // number — the model named which number this mail is really about.
            delete_fed_phantoms(&tx, a.account_id, &fed, Some(tn))?;
            let info = ShipmentInfo {
                carrier: a.carrier.clone(),
                tracking_number: tn.to_string(),
                item_name: a.item_name.clone().unwrap_or_default(),
                // A shipping mail that states no status is in transit, matching
                // the regex detector's own default.
                status: a.status.unwrap_or(ShipmentStatus::Shipped),
                tracking_url: tracking_url(&a.carrier, tn),
            };
            let ship_id =
                upsert_shipment_conn(&tx, a.account_id, a.message_id, &info, a.received_at)?;

            // EXTRACTOR BEATS THE REGEX NAME, by provenance rather than by
            // length: the upsert's longer-name-wins rule exists to pick between
            // two REGEX guesses, and it otherwise keeps junk like "package now
            // with its carrier!" over the model's "Anker USB-C charger" purely
            // for being longer. Against ANOTHER llm name, longer still wins.
            if let Some(name) = a.item_name.as_deref() {
                adopt_llm_item_name(&tx, ship_id, name, Some(a.message_id))?;
            }

            if let Some(oref) = a.order_ref.as_deref() {
                tx.execute(
                    "UPDATE shipments SET order_ref = ?2, order_merchant = ?3 WHERE id = ?1",
                    params![ship_id, oref, merchant],
                )?;
                // PROMOTION: the order confirmation that arrived days earlier
                // staged a row under this reference. Donate its name if we have
                // none WORTH KEEPING — a stored name today's strip refuses is no
                // better than none — then delete it: the purchase now has a real
                // identity.
                // The staged row's own name PROVENANCE rides along: the donated
                // text belongs to the mail that wrote it, not to this ship
                // notice, and sealing that mail must still scrub it.
                let staged: Option<(i64, String, Option<i64>)> = tx
                    .query_row(
                        "SELECT id, item_name, COALESCE(item_name_msg, last_message_id)
                         FROM shipment_orders
                         WHERE account_id = ?1 AND order_merchant = ?2 AND order_ref = ?3",
                        params![a.account_id, merchant, oref],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if let Some((staged_id, staged_name, staged_name_msg)) = staged {
                    let cur = shipment_item_name(&tx, ship_id)?;
                    if (cur.trim().is_empty() || crate::triage::shipment::is_junk_item_name(&cur))
                        && !staged_name.trim().is_empty()
                    {
                        set_shipment_item_name(&tx, ship_id, &staged_name, staged_name_msg)?;
                    }
                    tx.execute(
                        "DELETE FROM shipment_orders WHERE id = ?1",
                        params![staged_id],
                    )?;
                }
            }
            true
        } else if let Some(oref) = a.order_ref.as_deref() {
            // IDENTITY: an order reference and NO tracking number. NOTHING IS
            // DELETED HERE — see the rule on `shipments_extract_apply`: the model
            // naming no number is silence, and the row this message minted may be
            // a real package whose number the extractor merely echoed into the
            // order field.
            match shipments_by_order_ref(&tx, a.account_id, &merchant, oref)?.as_slice() {
                // The shipment already landed under this reference: the only
                // thing an order mail can still add is the item's name.
                [(ship_id, cur_name)] => match a.item_name.as_deref() {
                    Some(name) if cur_name.trim().is_empty() => {
                        set_shipment_item_name(&tx, *ship_id, name, Some(a.message_id))?;
                        true
                    }
                    _ => false,
                },
                // SEVERAL packages carry this reference — the order shipped in
                // more than one box. Naming one of them would be a coin flip, and
                // the purchase is already tracked, so this mail writes nothing at
                // all: not a name, not a fresh staging row.
                [_, _, ..] => false,
                // No tracking number anywhere yet: STAGE the purchase, keyed by
                // the retailer's reference IN ITS MERCHANT'S NAMESPACE, until a
                // ship notice promotes it.
                [] => {
                    let ts = a.received_at.to_rfc3339();
                    let name = a.item_name.clone().unwrap_or_default();
                    tx.execute(
                        "INSERT INTO shipment_orders(account_id, order_ref, order_merchant,
                             item_name, thread_id, last_message_id, item_name_msg,
                             first_seen, last_update)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                         ON CONFLICT(account_id, order_merchant, order_ref) DO UPDATE SET
                             item_name = CASE WHEN excluded.item_name != ''
                                              THEN excluded.item_name ELSE item_name END,
                             item_name_msg = CASE WHEN excluded.item_name != ''
                                              THEN excluded.item_name_msg ELSE item_name_msg END,
                             thread_id = excluded.thread_id,
                             last_message_id = excluded.last_message_id,
                             last_update = excluded.last_update",
                        params![
                            a.account_id,
                            oref,
                            merchant,
                            name,
                            a.thread_id,
                            a.message_id,
                            (!name.is_empty()).then_some(a.message_id),
                            ts,
                        ],
                    )?;
                    true
                }
            }
        } else {
            // NO IDENTITY AT ALL — a shipping mail naming neither a number nor an
            // order. Conservative by construction, and DELETING NOTHING for the
            // same reason as the branch above: adopt a name onto the thread's ONE
            // shipment if it has none, and otherwise leave the world alone.
            let survivors: Vec<(i64, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT s.id, s.item_name FROM shipments s
                     JOIN messages m
                       ON m.id = s.last_message_id AND m.account_id = s.account_id
                     WHERE s.account_id = ?1 AND m.thread_id = ?2",
                )?;
                stmt.query_map(params![a.account_id, a.thread_id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };
            // Two packages in one thread and no identity to tell them apart: the
            // name would be a coin flip, so nothing is written. Status,
            // `last_message_id` and `last_update` are never touched here either —
            // a mail with no identity has no claim on a row's lifecycle.
            match (survivors.as_slice(), a.item_name.as_deref()) {
                ([(ship_id, cur_name)], Some(name)) if cur_name.trim().is_empty() => {
                    set_shipment_item_name(&tx, *ship_id, name, Some(a.message_id))?;
                    true
                }
                _ => false,
            }
        };

        tx.commit()?;
        Ok(wrote)
    }

    pub(super) fn list_shipments(
        &self,
        account_id: AccountId,
        include_delivered: bool,
        policy: crate::config::ShipmentListPolicy,
    ) -> Result<Vec<crate::types::Shipment>> {
        let conn = self.lock()?;
        // No sealed rows: detection never runs on sealed mail, and sealing an
        // already-extracted message deletes its shipment row (correct_triage).
        // `cleared_at` rides along as an extra column: the read-side policy below
        // needs it, and the wire type deliberately does not carry it.
        let mut sql = format!(
            "SELECT {SHIPMENT_COLUMNS}, s.cleared_at {SHIPMENT_FROM} WHERE s.account_id=?1"
        );
        if !include_delivered {
            sql.push_str(" AND s.status != 'delivered'");
        }
        sql.push_str(" ORDER BY s.last_update DESC");
        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params![account_id], |r| {
                Ok((shipment_row(r)?, dt_opt(r, 15)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // EVERY HIDE IN HERE IS READ-SIDE, deliberately not a stored "hidden"
        // flag: the rows stay live so a later repair pass can still fix them, so
        // the poller keeps polling them (see `list_pollable_shipments`, which
        // filters on NONE of this), and so each hide reverses itself the moment
        // the package does something. One filter, one place.
        let stale_before = (policy.stale_after_days > 0)
            .then(|| Utc::now() - chrono::Duration::days(policy.stale_after_days as i64));
        Ok(out
            .into_iter()
            .filter(|(s, cleared_at)| {
                // 1. PHANTOM. A carrier that has rejected an ambiguous bare
                //    digit-run `suppress_failed_ambiguous_at` times running is
                //    telling us it was never a tracking number — but only the
                //    ambiguous SHAPES can be phantoms, so a 1Z…/TBA…/IMpb row is
                //    never hidden however badly it polls. One successful poll
                //    zeroes the counter and the row is back. NOTE: a cap of 0
                //    hides every ambiguous row, since `poll_failures` is never
                //    negative; callers pass the carrier poller's retirement cap.
                let phantom = s.poll_failures >= policy.suppress_failed_ambiguous_at
                    && crate::triage::is_ambiguous_tracking_shape(&s.tracking_number);
                // 2. STALE. `last_update` advances ONLY on a user-visible change
                //    (status, eta, or the carrier's raw string), so this is
                //    exactly "nothing has happened to this package in N days".
                //    An update pulls it back inside the window on its own.
                let stale = stale_before.is_some_and(|cutoff| s.last_update < cutoff);
                // 3. CLEARED. The comparison IS the revival: hide only while the
                //    row has not moved since the user cleared it. Nothing ever
                //    resets `cleared_at`, and nothing needs to.
                let cleared = cleared_at.is_some_and(|at| s.last_update <= at);
                !(phantom || stale || cleared)
            })
            .map(|(s, _)| s)
            .collect())
    }

    /// Stamp the user's "stop showing me this" on one shipment. Unconditional so
    /// a re-clear RESTAMPS (a row revived by an update and cleared again must
    /// hide against the LATER stamp), which also makes it idempotent. `false`
    /// means no such row for this account.
    pub(super) fn clear_shipment(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE shipments SET cleared_at = ?3 WHERE account_id = ?1 AND id = ?2",
            params![account_id, shipment_id, at.to_rfc3339()],
        )?;
        Ok(n > 0)
    }

    /// One-shot repair: re-run the (tightened) detector over each shipment row's
    /// FEEDER MESSAGE and delete the row when that message no longer yields that
    /// tracking number — the phantom rows a looser detector minted from eBay item
    /// ids and marketing digit-runs. Returns the number of rows deleted, and 0
    /// once the pass has already run for this account.
    ///
    /// ATOMIC WITH ITS OWN DONE-FLAG. The deletions and the `app_settings` flag
    /// that records them commit in ONE transaction, so the pass can never
    /// complete unrecorded — with the flag written by the caller afterwards, a
    /// crash or an unwritable settings row meant the whole thing ran AGAIN next
    /// start, and by then the extractor had written rows the regex cannot
    /// reproduce. The store owns both halves for that reason; callers just call.
    ///
    /// ONLY REGEX PHANTOMS ARE IN SCOPE. The keep test is "does the regex
    /// detector still yield this number", which extractor-written rows fail BY
    /// CONSTRUCTION — the model found what the regex could not. So a row with
    /// carrier evidence (`carrier_status_raw` / `last_polled_at`) or extractor
    /// evidence (`order_ref`) is never judged at all.
    ///
    /// A row whose `last_message_id` is NULL is LEFT ALONE too: there is no
    /// evidence to re-judge it on (an older daemon wrote it, or only a carrier
    /// poll has touched it), and deleting on absent evidence drops live packages.
    ///
    /// SECURITY: the detector never runs on sealed mail, so the join skips any
    /// feeder whose triage row is not `sensitivity='normal'` — those rows keep
    /// the structural guarantee they already have (sealing deletes the shipment).
    pub(super) fn shipments_redetect_cleanup(&self, account_id: AccountId) -> Result<u64> {
        let mut conn = self.lock()?;
        let done: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE account_id = ?1 AND key = ?2",
                params![account_id, SHIPMENTS_REDETECT_FLAG],
                |r| r.get(0),
            )
            .optional()?;
        if done.as_deref() == Some("done") {
            return Ok(0);
        }

        let tx = conn.transaction()?;
        let rows: Vec<(i64, String, String, String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT s.id, s.tracking_number, m.from_addr, m.subject, m.body
                 FROM shipments s
                 JOIN messages m
                   ON m.id = s.last_message_id AND m.account_id = s.account_id
                 LEFT JOIN triage t ON t.message_id = m.id AND t.account_id = m.account_id
                 WHERE s.account_id = ?1
                   AND COALESCE(t.sensitivity, 'normal') = 'normal'
                   AND s.carrier_status_raw IS NULL
                   AND s.last_polled_at IS NULL
                   AND s.order_ref IS NULL",
            )?;
            stmt.query_map(params![account_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut deleted = 0u64;
        for (id, tracking_number, from_addr, subject, body) in rows {
            // Keep ONLY when the same message still yields the SAME number: a
            // different number means the row was minted from a candidate the
            // tightened gates now reject, and the surviving number gets its own
            // row from the next ingest of that mail.
            let still_detected = crate::triage::detect_shipment(&from_addr, &subject, &body)
                .is_some_and(|s| s.tracking_number == tracking_number);
            if !still_detected {
                deleted += tx.execute(
                    "DELETE FROM shipments WHERE account_id=?1 AND id=?2",
                    params![account_id, id],
                )? as u64;
            }
        }

        tx.execute(
            "INSERT INTO app_settings(account_id, key, value)
             VALUES(?1, ?2, 'done')
             ON CONFLICT(account_id, key) DO UPDATE SET value = excluded.value",
            params![account_id, SHIPMENTS_REDETECT_FLAG],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    pub(super) fn list_pollable_shipments(
        &self,
        account_id: AccountId,
        min_first_seen: DateTime<Utc>,
        max_failures: u32,
    ) -> Result<Vec<crate::types::Shipment>> {
        let conn = self.lock()?;
        // Amazon and "unknown" are excluded by the carrier list: neither has an
        // API to poll. Ordered never-polled first, then least-recently-polled,
        // so a caller taking a prefix spreads its budget evenly.
        //
        // NO `cleared_at` AND NO STALENESS PREDICATE HERE, ON PURPOSE. A row the
        // user cleared, and a row the listing hides as stale, are both still
        // polled: the poll is what produces the `last_update` that brings them
        // back, so filtering them out here would make hiding permanent. Only
        // `list_shipments` filters. Do not "optimize" this.
        let mut stmt = conn.prepare(&format!(
            "SELECT {SHIPMENT_COLUMNS} {SHIPMENT_FROM}
             WHERE s.account_id=?1
               AND s.status != 'delivered'
               AND s.carrier IN ('ups','usps','fedex','dhl')
               AND s.first_seen >= ?2
               AND s.poll_failures < ?3
             ORDER BY s.last_polled_at IS NOT NULL, s.last_polled_at, s.id"
        ))?;
        let out = stmt
            .query_map(
                params![account_id, min_first_seen.to_rfc3339(), max_failures],
                shipment_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn apply_carrier_track(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        track: &crate::triage::CarrierTrack,
        polled_at: DateTime<Utc>,
    ) -> Result<bool> {
        use crate::triage::ShipmentStatus;
        let conn = self.lock()?;

        // Read the row so reconciliation and the visible-change test run in Rust
        // rather than a SQL CASE.
        let existing: Option<(String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT status, carrier_status_raw, eta FROM shipments
                 WHERE account_id=?1 AND id=?2",
                params![account_id, shipment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((cur_status_s, cur_raw, cur_eta)) = existing else {
            return Ok(false);
        };
        let cur_status = ShipmentStatus::parse(&cur_status_s).unwrap_or(ShipmentStatus::Shipped);
        // A status the carrier's vocabulary could not map leaves the
        // email-inferred one alone; the raw string still lands.
        let status = match track.status {
            Some(carrier) => ShipmentStatus::reconcile_carrier(cur_status, carrier),
            None => cur_status,
        };

        let eta = track.eta.map(|t| t.to_rfc3339());
        let status_changed = status != cur_status;
        // `last_update` is the Sitrep sort key, so it advances ONLY on a change
        // the user can see — a poll that confirms what the row already says must
        // not churn the card order.
        let visible_change = status_changed
            || cur_raw.as_deref() != Some(track.carrier_status_raw.as_str())
            || cur_eta != eta;
        // The carrier's own delivery timestamp when it gave one, else the poll
        // clock; COALESCE keeps whatever an earlier email or poll recorded.
        let delivered_at = (status == ShipmentStatus::Delivered)
            .then(|| track.delivered_at.unwrap_or(polled_at).to_rfc3339());

        // NEVER touches `last_message_id`: no message backs a poll, so the row's
        // click target stays the last accepted email. The sealing delete in
        // feedback.rs is keyed on `last_message_id`, so a poll-advanced row that
        // dropped its pointer would survive the seal of the mail that fed it.
        conn.execute(
            "UPDATE shipments
                SET status             = ?3,
                    carrier_status_raw = ?4,
                    eta                = ?5,
                    delivered_at       = COALESCE(delivered_at, ?6),
                    last_polled_at     = ?7,
                    poll_failures      = 0,
                    last_update        = CASE WHEN ?8 THEN ?7 ELSE last_update END
              WHERE account_id=?1 AND id=?2",
            params![
                account_id,
                shipment_id,
                status.as_str(),
                track.carrier_status_raw,
                eta,
                delivered_at,
                polled_at.to_rfc3339(),
                visible_change,
            ],
        )?;
        Ok(status_changed)
    }

    pub(super) fn record_poll_outcome(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        polled_at: DateTime<Utc>,
        permanent_failure: bool,
    ) -> Result<()> {
        let conn = self.lock()?;
        // The ATTEMPT is always stamped, so a shipment the carrier keeps
        // rejecting still rotates through the poll queue. Only a PERMANENT
        // failure counts toward the retirement cap — a transient network or
        // rate-limit error must not retire a live shipment — and a successful
        // poll ([`SqliteStore::apply_carrier_track`]) resets the counter.
        conn.execute(
            "UPDATE shipments
                SET last_polled_at = ?3,
                    poll_failures  = poll_failures + CASE WHEN ?4 THEN 1 ELSE 0 END
              WHERE account_id=?1 AND id=?2",
            params![
                account_id,
                shipment_id,
                polled_at.to_rfc3339(),
                permanent_failure,
            ],
        )?;
        Ok(())
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
