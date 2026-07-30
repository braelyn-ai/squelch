//! `/client/*` handlers for the human door.
//!
//! Handlers are thin: validate params, call the store via `spawn_blocking`,
//! serialize core types to JSON. Sealed handling lives in the store;
//! [`reveal_sealed`] is the one place a sealed body is surfaced, and it audits
//! before returning.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use squelch_core::store::{ActionMessageRef, NewAuditEntry, SitrepBand, Store};
use squelch_core::types::{AttentionStatus, Disposition, ShredStats, Tier, TriageAxis};

use crate::error::ApiError;
use crate::gmail_write::{
    GmailWriteClient, ReplyParts, WriteError, build_references, build_reply_rfc822, reply_subject,
};
use crate::guard;
use crate::state::ApiState;

/// Default page size when `limit` is omitted, and the hard ceiling we clamp to.
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
/// Default `since` window for `/client/updates` when the caller omits it.
const DEFAULT_UPDATES_WINDOW_DAYS: i64 = 30;

/// Actor label written into the audit log for human-door reveals.
const AUDIT_ACTOR: &str = "human";

// --- pagination cursor ------------------------------------------------------

/// An opaque token round-tripping a row offset (`off:<n>`, base64url). Not
/// security-sensitive; it is a scroll position.
mod cursor {
    use super::base64_lite::{decode, encode};

    pub fn encode_offset(offset: u32) -> String {
        encode(format!("off:{offset}").as_bytes())
    }

    pub fn decode_offset(s: &str) -> Option<u32> {
        let bytes = decode(s)?;
        let text = String::from_utf8(bytes).ok()?;
        text.strip_prefix("off:")?.parse().ok()
    }
}

/// A dependency-free base64url codec, so opaque-ifying an integer offset needs
/// no extra crate.
mod base64_lite {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 63) as usize] as char);
            }
        }
        out
    }

    pub fn decode(input: &str) -> Option<Vec<u8>> {
        fn val(c: u8) -> Option<u32> {
            match c {
                b'A'..=b'Z' => Some((c - b'A') as u32),
                b'a'..=b'z' => Some((c - b'a' + 26) as u32),
                b'0'..=b'9' => Some((c - b'0' + 52) as u32),
                b'-' => Some(62),
                b'_' => Some(63),
                _ => None,
            }
        }
        let bytes = input.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        for chunk in bytes.chunks(4) {
            if chunk.len() < 2 {
                return None;
            }
            let mut n = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                n |= val(c)? << (18 - 6 * i);
            }
            out.push((n >> 16) as u8);
            if chunk.len() > 2 {
                out.push((n >> 8) as u8);
            }
            if chunk.len() > 3 {
                out.push(n as u8);
            }
        }
        Some(out)
    }
}

/// Resolve `(limit, offset)` from the `limit`/`cursor` query params. `limit` is
/// clamped to `[1, MAX_LIMIT]`; a malformed cursor is a 400.
fn paginate(limit: Option<u32>, cursor: Option<&str>) -> Result<(u32, u32), ApiError> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = match cursor {
        Some(c) => cursor::decode_offset(c).ok_or_else(|| ApiError::bad_request("bad cursor"))?,
        None => 0,
    };
    Ok((limit, offset))
}

/// Build the `next_cursor` for a page: `Some` only if the page came back full
/// (so there may be more), pointing at the next offset.
fn next_cursor(returned: usize, limit: u32, offset: u32) -> Option<String> {
    (returned as u32 == limit).then(|| cursor::encode_offset(offset + limit))
}

/// Envelope for paginated list endpoints.
#[derive(Debug, Serialize)]
struct Page<T> {
    items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Run a synchronous store closure off the async runtime. A panic inside it
/// surfaces as an opaque 500.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, squelch_core::CoreError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .map_err(ApiError::from)
}

// --- GET /client/updates ----------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UpdatesQuery {
    since: Option<DateTime<Utc>>,
    min_importance: Option<u8>,
    tier: Option<String>,
    /// Attention-lifecycle filter: new|open|done.
    status: Option<String>,
    /// Server-side sitrep bucket: standing|new|open.
    band: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

pub async fn get_updates(
    State(state): State<ApiState>,
    Query(q): Query<UpdatesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let (limit, offset) = paginate(q.limit, q.cursor.as_deref())?;
    let tier_filter = match q.tier.as_deref() {
        None => None,
        Some(s) => Some(Tier::parse(s).ok_or_else(|| {
            ApiError::bad_request("tier must be one of: past_due, deadline, signal, noise")
        })?),
    };
    let status_filter = match q.status.as_deref() {
        None => None,
        Some(s) => Some(
            AttentionStatus::parse(s)
                .ok_or_else(|| ApiError::bad_request("status must be one of: new, open, done"))?,
        ),
    };
    let band = match q.band.as_deref() {
        None => None,
        Some(s) => Some(
            SitrepBand::parse(s)
                .ok_or_else(|| ApiError::bad_request("band must be one of: standing, new, open"))?,
        ),
    };
    let since = q
        .since
        .unwrap_or_else(|| Utc::now() - chrono::Duration::days(DEFAULT_UPDATES_WINDOW_DAYS));
    let min_importance = q.min_importance;

    let store = state.store.clone();
    let account_id = state.account_id;
    let items = blocking(move || {
        // attention_updates excludes sealed rows in SQL. status/band filter
        // server-side; tier and pagination apply over the ranked slice here.
        let mut all =
            store.attention_updates(account_id, since, min_importance, status_filter, band)?;
        if let Some(t) = tier_filter {
            all.retain(|u| u.update.tier == t);
        }
        let page = all
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect::<Vec<_>>();

        // SEEN-LEDGER: the response carries the PRE-stamp surfaced_at, then this
        // exact set is stamped (surfaced_at=now if NULL, new->open). Sealed rows
        // cannot be in `page`, and mark_surfaced re-guards sensitivity anyway.
        let ids: Vec<i64> = page.iter().map(|u| u.update.id).collect();
        store.mark_surfaced(account_id, &ids)?;

        Ok(page)
    })
    .await?;

    let next = next_cursor(items.len(), limit, offset);
    Ok(Json(Page {
        items,
        next_cursor: next,
    }))
}

// --- POST /client/updates/{message_id}/status -------------------------------

#[derive(Debug, Deserialize)]
pub struct StatusBody {
    /// "done" to dismiss, "open" to reopen. ("new" is accepted for symmetry.)
    status: String,
}

pub async fn set_update_status(
    State(state): State<ApiState>,
    Path(message_id): Path<i64>,
    Json(body): Json<StatusBody>,
) -> Result<impl IntoResponse, ApiError> {
    let status = AttentionStatus::parse(&body.status)
        .ok_or_else(|| ApiError::bad_request("status must be one of: new, open, done"))?;

    let store = state.store.clone();
    let account_id = state.account_id;
    let updated =
        blocking(move || store.set_attention_status(account_id, message_id, status)).await?;
    if !updated {
        // Missing OR sealed => NotFound, keeping the two indistinguishable.
        return Err(ApiError::not_found());
    }

    // Audited; no body content is recorded.
    audit_action(
        &state,
        "set_status",
        Some(message_id.to_string()),
        status.as_str(),
    )
    .await;

    Ok(Json(json!({ "status": status.as_str(), "message_id": message_id })))
}

// --- POST /client/refresh ---------------------------------------------------

/// Poke the Gmail sync loop to poll NOW. Fire-and-forget: it does not block on
/// the round trip, and `triggered: false` means no sync loop is wired to this
/// door. A READ-path trigger — no write scope, no mutation, nothing to audit.
pub async fn refresh_now(State(state): State<ApiState>) -> impl IntoResponse {
    let triggered = match &state.refresh {
        Some(notify) => {
            notify.notify_one();
            true
        }
        None => false,
    };
    Json(json!({ "triggered": triggered }))
}

// --- POST /client/retriage (developer tool) ----------------------------------

#[derive(Debug, Deserialize)]
pub struct RetriageBody {
    /// Re-triage just this message. When absent, the trailing-`days` window.
    #[serde(default)]
    message_id: Option<i64>,
    /// Trailing window in days (default 7, clamped 1..=90). Ignored when
    /// `message_id` is set.
    #[serde(default)]
    days: Option<u32>,
}

/// DEV RE-TRIAGE: reset LLM markers on the scoped rows so the pipeline re-runs.
/// Rule-decided and sealed rows are never touched (store-level guard), and a
/// sealed or unknown `message_id` resets 0 rows — indistinguishable by design.
pub async fn retriage(
    State(state): State<ApiState>,
    Json(body): Json<RetriageBody>,
) -> Result<impl IntoResponse, ApiError> {
    let days = body.days.unwrap_or(7).clamp(1, 90);
    let target = body.message_id.map(|id| id.to_string());

    let store = state.store.clone();
    let account_id = state.account_id;
    let msg_id = body.message_id;
    let reset = blocking(move || store.retriage_reset(account_id, msg_id, days)).await?;

    audit_action(
        &state,
        "retriage",
        target,
        &format!("days={days},reset={reset}"),
    )
    .await;

    // Wake the sync loop so the LLM passes pick the rows up now, not next cycle.
    if let Some(notify) = &state.refresh {
        notify.notify_one();
    }
    Ok(Json(json!({ "reset": reset })))
}

// --- GET /client/triage-debug/{message_id} (developer tool) ------------------

/// DEV INSPECTOR: the full triage state of one non-sealed message. Read-only;
/// sealed and unknown are an indistinguishable 404, and no body content is carried.
pub async fn triage_debug(
    State(state): State<ApiState>,
    Path(message_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let dbg = blocking(move || store.triage_debug(account_id, message_id))
        .await?
        .ok_or(ApiError::not_found())?;
    Ok(Json(dbg))
}

// --- GET /client/thread/{thread_id} -----------------------------------------

pub async fn get_thread(
    State(state): State<ApiState>,
    Path(thread_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    // Sealed and nonexistent threads are the SAME NotFound. Each message carries
    // its server-sanitized `html`, null for plain-text-only mail.
    let view = blocking(move || store.thread_view_with_html(account_id, &thread_id)).await?;
    Ok(Json(view))
}

// --- GET /client/attachments/{id} -------------------------------------------

/// Render-safety whitelist for the served `Content-Type`: the stored mime is
/// echoed ONLY for `application/pdf` and non-SVG `image/*`. Everything else
/// serves as `application/octet-stream`, so a hostile attachment can never
/// render inline from our origin. This header discipline IS the security story
/// for the byte endpoint.
fn safe_content_type(mime: &str) -> String {
    // Compare on the bare type, lowercased, ignoring `; charset=...` params.
    let base = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base == "application/pdf" {
        return base;
    }
    if let Some(sub) = base.strip_prefix("image/")
        && !sub.is_empty()
        // ANY svg/xml-ish image subtype is scriptable: never echo it renderable.
        && !sub.contains("svg")
        && !sub.contains("xml")
    {
        return base;
    }
    "application/octet-stream".to_string()
}

/// Sanitize a filename for `Content-Disposition`: strip path separators,
/// control chars, quotes and non-ASCII, falling back to "attachment".
fn sanitize_attachment_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| {
            c.is_ascii() && !c.is_ascii_control() && *c != '/' && *c != '\\' && *c != '"'
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Serve one attachment's raw bytes. The parent-message sealed guard lives in
/// [`Store::attachment_bytes`]; this handler adds the header discipline (see
/// [`safe_content_type`]) and the over-cap 410.
pub async fn get_attachment(
    State(state): State<ApiState>,
    Path(attachment_id): Path<i64>,
) -> Result<Response, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    // `None` => unknown id OR sealed parent (indistinguishable): 404.
    let found = blocking(move || store.attachment_bytes(account_id, attachment_id)).await?;
    let (filename, mime, data) = found.ok_or_else(ApiError::not_found)?;
    // Metadata exists but the bytes were never stored (over the ingest cap): 410.
    let bytes =
        data.ok_or_else(|| ApiError::new(StatusCode::GONE, "attachment bytes not stored"))?;

    let ctype = safe_content_type(&mime);
    let disposition = format!(
        "attachment; filename=\"{}\"",
        sanitize_attachment_filename(&filename)
    );

    // Built by hand, not via `Vec<u8>`'s IntoResponse, so we own the
    // Content-Type exactly.
    let mut resp = Response::new(Body::from(bytes));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&ctype)
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| header::HeaderValue::from_static("attachment")),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, max-age=3600"),
    );
    Ok(resp)
}

// --- GET /client/search -----------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    q: String,
    limit: Option<u32>,
    cursor: Option<String>,
    /// Retrieval mode: keyword|semantic|hybrid. Omitted => hybrid when a vector
    /// index is available (an embedder is attached), else keyword.
    mode: Option<String>,
}

/// The three retrieval modes for `/client/search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Keyword,
    Semantic,
    Hybrid,
}

impl SearchMode {
    fn parse(s: &str) -> Option<SearchMode> {
        match s {
            "keyword" => Some(SearchMode::Keyword),
            "semantic" => Some(SearchMode::Semantic),
            "hybrid" => Some(SearchMode::Hybrid),
            _ => None,
        }
    }

    /// The `match_kind` label echoed on the response.
    fn as_str(self) -> &'static str {
        match self {
            SearchMode::Keyword => "keyword",
            SearchMode::Semantic => "semantic",
            SearchMode::Hybrid => "hybrid",
        }
    }
}

/// Search response envelope: a page of hits plus the mode actually run.
#[derive(Debug, Serialize)]
struct SearchPage<T> {
    items: Vec<T>,
    match_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

pub async fn search(
    State(state): State<ApiState>,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let term = query.q.trim().to_string();
    if term.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let (limit, offset) = paginate(query.limit, query.cursor.as_deref())?;

    // An attached embedder means vectors are being written; that gates
    // semantic/hybrid and drives the default mode.
    let have_vectors = state.store.embedder().is_some();

    let mode = match query.mode.as_deref() {
        Some(s) => SearchMode::parse(s).ok_or_else(|| {
            ApiError::bad_request("mode must be one of: keyword, semantic, hybrid")
        })?,
        None => {
            if have_vectors {
                SearchMode::Hybrid
            } else {
                SearchMode::Keyword
            }
        }
    };

    // Semantic/hybrid asked for without a vector index degrade to keyword rather
    // than erroring — but the response reports the kind actually run.
    let effective = match mode {
        SearchMode::Semantic | SearchMode::Hybrid if !have_vectors => SearchMode::Keyword,
        other => other,
    };

    let store = state.store.clone();
    let account_id = state.account_id;
    // Keyword paginates in SQL; semantic/hybrid rank a top-k window and offset
    // the fused slice. EVERY leg excludes sealed rows in SQL.
    let items = blocking(move || match effective {
        SearchMode::Keyword => store.search(account_id, &term, limit, offset),
        SearchMode::Semantic => {
            let k = (limit + offset) as usize;
            let mut hits = store.semantic_search_hits(account_id, &term, k)?;
            let dropped: Vec<_> = hits.drain(..).skip(offset as usize).take(limit as usize).collect();
            Ok(dropped)
        }
        SearchMode::Hybrid => {
            let k = (limit + offset) as usize;
            let mut hits = store.hybrid_search(account_id, &term, k)?;
            let dropped: Vec<_> = hits.drain(..).skip(offset as usize).take(limit as usize).collect();
            Ok(dropped)
        }
    })
    .await?;

    let next = next_cursor(items.len(), limit, offset);
    Ok(Json(SearchPage {
        items,
        match_kind: effective.as_str(),
        next_cursor: next,
    }))
}

// --- GET /client/shipments --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ShipmentsQuery {
    /// Include delivered shipments too. Default false => en-route only.
    #[serde(default)]
    include_delivered: bool,
}

pub async fn get_shipments(
    State(state): State<ApiState>,
    Query(q): Query<ShipmentsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    // The shipments table holds no sealed rows by construction: detection never
    // runs on sealed mail, so there is no sealed filtering to apply.
    let items = blocking(move || store.list_shipments(account_id, q.include_delivered)).await?;
    Ok(Json(items))
}

// --- GET /client/receipts ---------------------------------------------------

/// Default look-back window for the receipts list.
const DEFAULT_RECEIPTS_DAYS: u32 = 30;

#[derive(Debug, Deserialize)]
pub struct ReceiptsQuery {
    /// Look-back window in days. Default 30.
    days: Option<u32>,
}

pub async fn get_receipts(
    State(state): State<ApiState>,
    Query(q): Query<ReceiptsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let days = q.days.unwrap_or(DEFAULT_RECEIPTS_DAYS);
    // Newest-first. Structurally sealed-free, like shipments.
    let items = blocking(move || store.list_receipts(account_id, days)).await?;
    Ok(Json(items))
}

// --- GET /client/banking -----------------------------------------------------

/// SERIALIZED SHAPE IS A WIRE CONTRACT — the desktop Banking zone decodes
/// exactly [`squelch_core::types::Banking`], newest-received first, with every
/// extracted field nullable and `account_hint` only ever a masked last-4 tail.
/// Sealed mail can never produce a banking row (structural, like receipts).
pub async fn get_banking(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let items = blocking(move || store.list_banking(account_id)).await?;
    Ok(Json(items))
}

// --- GET /client/marketing ---------------------------------------------------

/// Look-back window and clamp bounds. Promos age out fast; a fortnight is
/// already generous for "what is on offer".
const DEFAULT_MARKETING_DAYS: u32 = 14;
const MAX_MARKETING_DAYS: u32 = 90;
const MARKETING_LIMIT: u32 = 200;

#[derive(Debug, Deserialize)]
pub struct MarketingQuery {
    #[serde(default)]
    days: Option<u32>,
}

/// Extracted promotions, newest first. Structurally sealed-free: only an
/// extractor writes this table, and sealed mail never reaches one.
pub async fn get_marketing(
    State(state): State<ApiState>,
    Query(q): Query<MarketingQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let days = q.days.unwrap_or(DEFAULT_MARKETING_DAYS).clamp(1, MAX_MARKETING_DAYS);
    let store = state.store.clone();
    let account_id = state.account_id;
    let items = blocking(move || store.marketing_offers(account_id, days, MARKETING_LIMIT)).await?;
    Ok(Json(items))
}

// --- GET /client/calendar ----------------------------------------------------

/// Look-back window in hours of MAIL ARRIVAL, not event start, plus its clamp
/// bounds.
const DEFAULT_CALENDAR_HOURS: u32 = 24;
const MIN_CALENDAR_HOURS: u32 = 1;
const MAX_CALENDAR_HOURS: u32 = 168; // one week

#[derive(Debug, Deserialize)]
pub struct CalendarQuery {
    /// Window in hours over `received_at`. Out-of-range values clamp, not 400.
    hours: Option<u32>,
}

/// SERIALIZED SHAPE IS A WIRE CONTRACT — the desktop sidebar decodes exactly
/// [`squelch_core::types::CalendarUpdate`], newest-received first, with every
/// extracted field nullable and `thread_id` the joined message's thread. Sealed
/// mail can never produce a calendar row (structural, like receipts/banking).
pub async fn get_calendar(
    State(state): State<ApiState>,
    Query(q): Query<CalendarQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let hours = q
        .hours
        .unwrap_or(DEFAULT_CALENDAR_HOURS)
        .clamp(MIN_CALENDAR_HOURS, MAX_CALENDAR_HOURS);
    let items = blocking(move || store.list_calendar_updates(account_id, hours)).await?;
    Ok(Json(items))
}

// --- GET/POST/DELETE /client/rules ------------------------------------------

pub async fn list_rules(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let rules = blocking(move || store.list_sender_rules(account_id)).await?;
    Ok(Json(rules))
}

#[derive(Debug, Deserialize)]
pub struct CreateRuleBody {
    match_pattern: String,
    want: String,
    disposition: String,
}

pub async fn create_rule(
    State(state): State<ApiState>,
    Json(body): Json<CreateRuleBody>,
) -> Result<impl IntoResponse, ApiError> {
    if body.match_pattern.trim().is_empty() {
        return Err(ApiError::bad_request("match_pattern must not be empty"));
    }
    let disposition = Disposition::parse(&body.disposition)
        .ok_or_else(|| ApiError::bad_request("disposition must be surface, squelch, or filtered"))?;

    let store = state.store.clone();
    let account_id = state.account_id;
    let pattern = body.match_pattern.clone();
    let id = blocking(move || {
        store.set_sender_rule(account_id, &body.match_pattern, &body.want, disposition)
    })
    .await?;
    // Best-effort audit: target is the match_pattern, detail the new rule id.
    audit_action(&state, "rule.create", Some(pattern), &id.to_string()).await;
    Ok((StatusCode::CREATED, Json(json!({ "rule_id": id }))))
}

pub async fn update_rule(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(body): Json<CreateRuleBody>,
) -> Result<impl IntoResponse, ApiError> {
    if body.match_pattern.trim().is_empty() {
        return Err(ApiError::bad_request("match_pattern must not be empty"));
    }
    let disposition = Disposition::parse(&body.disposition)
        .ok_or_else(|| ApiError::bad_request("disposition must be surface, squelch, or filtered"))?;

    let store = state.store.clone();
    let account_id = state.account_id;
    let pattern = body.match_pattern.clone();
    let updated = blocking(move || {
        store.update_sender_rule(account_id, id, &body.match_pattern, &body.want, disposition)
    })
    .await?;
    if updated {
        // Only a real edit is recorded; a 404 changed nothing, so no row.
        audit_action(&state, "rule.update", Some(pattern), &id.to_string()).await;
        Ok(Json(json!({ "rule_id": id })))
    } else {
        Err(ApiError::not_found())
    }
}

pub async fn delete_rule(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let deleted = blocking(move || store.delete_sender_rule(account_id, id)).await?;
    if deleted {
        // target is the rule id — the pattern is gone post-delete.
        audit_action(&state, "rule.delete", Some(id.to_string()), "ok").await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

// --- GET /client/sealed ------------------------------------------------------

/// Sealed METADATA only. No bodies here, ever.
#[derive(Debug, Serialize)]
struct SealedMeta {
    id: i64,
    thread_id: String,
    sender: String,
    subject: String,
    kind: Option<String>,
    received_at: DateTime<Utc>,
}

pub async fn list_sealed(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let sealed = blocking(move || store.sealed_messages(account_id)).await?;
    let items: Vec<SealedMeta> = sealed
        .into_iter()
        .map(|m| SealedMeta {
            id: m.id,
            thread_id: m.thread_id,
            sender: m.from_addr,
            subject: m.subject,
            kind: m.sealed_kind,
            received_at: m.received_at,
        })
        .collect();
    Ok(Json(items))
}

// --- POST /client/sealed/{message_id}/reveal --------------------------------

/// The revealed sealed body. Marked `Cache-Control: no-store` on the response.
#[derive(Debug, Serialize)]
struct RevealedSealed {
    id: i64,
    thread_id: String,
    sender: String,
    from_name: Option<String>,
    subject: String,
    kind: Option<String>,
    received_at: DateTime<Utc>,
    body: String,
    /// Server-sanitized HTML when the mail had one, through the same single
    /// audited reveal door and `no-store` like the text body.
    html: Option<String>,
}

pub async fn reveal_sealed(
    State(state): State<ApiState>,
    Path(message_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;

    let sealed = blocking(move || {
        // Audit BEFORE returning the body, recording only the message id so the
        // log itself never leaks a secret.
        store.append_audit(
            account_id,
            &NewAuditEntry {
                actor: AUDIT_ACTOR.to_string(),
                action: "reveal_sealed".to_string(),
                target: Some(message_id.to_string()),
                detail: None,
            },
        )?;
        store.sealed_body(account_id, message_id)
    })
    .await?;

    let payload = RevealedSealed {
        id: sealed.id,
        thread_id: sealed.thread_id,
        sender: sealed.from_addr,
        from_name: sealed.from_name,
        subject: sealed.subject,
        kind: sealed.sealed_kind,
        received_at: sealed.received_at,
        body: sealed.body,
        html: sealed.body_html,
    };

    // Never cache a sealed body anywhere along the path.
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok((headers, Json(payload)))
}

// --- GET /client/audit -------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    limit: Option<u32>,
}

pub async fn get_audit(
    State(state): State<ApiState>,
    Query(q): Query<AuditQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let store = state.store.clone();
    let account_id = state.account_id;
    let rows = blocking(move || store.list_audit(account_id, limit)).await?;
    Ok(Json(rows))
}

// --- GET /client/stats -------------------------------------------------------

pub async fn get_stats(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    // UTC day key for today's Stage-2 usage row.
    let day = Utc::now().format("%Y-%m-%d").to_string();
    let (stats, usage) = blocking(move || {
        let stats = store.stats(account_id)?;
        let usage = store.stage2_usage_today(account_id, &day)?;
        Ok((stats, usage))
    })
    .await?;

    // tokens/1e6 * per-MTok price, over input+output. Switching the model means
    // updating stage2_price_*_per_mtok or this drifts.
    let est_cost_usd_today = (usage.input_tokens as f64 / 1_000_000.0)
        * state.stage2_price_in_per_mtok
        + (usage.output_tokens as f64 / 1_000_000.0) * state.stage2_price_out_per_mtok;

    let mut body = serde_json::to_value(&stats)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    body["stage2"] = json!({
        "calls_today": usage.calls,
        "input_tokens_today": usage.input_tokens,
        "output_tokens_today": usage.output_tokens,
        "est_cost_usd_today": est_cost_usd_today,
    });
    Ok(Json(body))
}

// --- GET /client/usage -------------------------------------------------------

/// Default look-back window (rows) for the usage history.
const DEFAULT_USAGE_DAYS: u32 = 30;
const MAX_USAGE_DAYS: u32 = 365;

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// How many recent days of Stage-2 usage to return. Default 30.
    days: Option<u32>,
}

/// One day's row in the usage history response.
#[derive(Debug, Serialize)]
struct UsageRow {
    day: String,
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
}

/// Totals across the returned window, costed from the same per-MTok prices
/// `get_stats` uses.
#[derive(Debug, Serialize)]
struct UsageTotals {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    est_cost_usd: f64,
}

/// GET /client/usage — triage usage history + totals + model label. Additive to
/// `/client/stats`, which still carries today's `stage2` rollup.
pub async fn get_usage(
    State(state): State<ApiState>,
    Query(q): Query<UsageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let days = q.days.unwrap_or(DEFAULT_USAGE_DAYS).clamp(1, MAX_USAGE_DAYS);
    let store = state.store.clone();
    let account_id = state.account_id;
    let (s2_rows, s1_rows) = blocking(move || {
        let s2 = store.list_usage(account_id, days)?;
        let s1 = store.list_usage_stage1(account_id, days)?;
        Ok((s2, s1))
    })
    .await?;

    // Stage-1 and Stage-2 are SEPARATE categories, each costed from its own
    // per-MTok prices; the client renders whatever categories arrive.
    let rollup = |rows: Vec<squelch_core::store::Stage2UsageDay>,
                  price_in: f64,
                  price_out: f64|
     -> (Vec<UsageRow>, UsageTotals) {
        let (mut in_tok, mut out_tok, mut calls) = (0u64, 0u64, 0u64);
        let out_rows: Vec<UsageRow> = rows
            .into_iter()
            .map(|r| {
                calls += r.calls;
                in_tok += r.input_tokens;
                out_tok += r.output_tokens;
                UsageRow {
                    day: r.day,
                    calls: r.calls,
                    input_tokens: r.input_tokens,
                    output_tokens: r.output_tokens,
                }
            })
            .collect();
        let est_cost_usd = (in_tok as f64 / 1_000_000.0) * price_in
            + (out_tok as f64 / 1_000_000.0) * price_out;
        (
            out_rows,
            UsageTotals {
                calls,
                input_tokens: in_tok,
                output_tokens: out_tok,
                est_cost_usd,
            },
        )
    };

    let (s2_out, s2_totals) = rollup(
        s2_rows,
        state.stage2_price_in_per_mtok,
        state.stage2_price_out_per_mtok,
    );
    let (s1_out, s1_totals) = rollup(
        s1_rows,
        state.stage1_price_in_per_mtok,
        state.stage1_price_out_per_mtok,
    );

    Ok(Json(json!({
        // Top-level fields are Stage-2 (backward-compatible shape).
        "rows": s2_out,
        "totals": s2_totals,
        "provider": state.stage2_provider.as_deref(),
        "model": state.stage2_model.as_ref(),
        "categories": {
            "stage1": {
                "model": state.stage1_model.as_ref(),
                "rows": s1_out,
                "totals": s1_totals,
            },
            "stage2": {
                "model": state.stage2_model.as_ref(),
                "rows": s2_out,
                "totals": s2_totals,
            },
        },
    })))
}

// --- GET/POST /client/triage-config -----------------------------------------
//
// LLM-triage daily budget caps, configurable at runtime without a restart. POST
// persists an app_settings OVERRIDE that the Stage-2 pass re-reads at the start
// of each cycle. Precedence: override > config/env > default.

/// Trailing window (days) for the volume/usage averages on the endpoint.
const TRIAGE_CONFIG_TRAILING_DAYS: i64 = 14;

/// One cap's wire "source": an override row wins, else the config/env layer.
fn cap_source_str(
    has_override: bool,
    config_source: squelch_core::config::CapSource,
) -> &'static str {
    if has_override {
        "override"
    } else {
        config_source.as_str()
    }
}

/// The `/client/triage-config` response body: effective caps, per-cap sources,
/// trailing-14d averages, prices. Shared by GET and POST, which returns this
/// fresh shape after persisting.
async fn triage_config_body(state: &ApiState) -> Result<serde_json::Value, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let now = Utc::now();
    let since = now - chrono::Duration::days(TRIAGE_CONFIG_TRAILING_DAYS);
    let since_day = since.format("%Y-%m-%d").to_string();

    let (overrides, inbound, usage, s1_usage) = blocking(move || {
        let overrides = store.stage2_cap_overrides(account_id)?;
        let inbound = store.count_inbound_since(account_id, since)?;
        let usage = store.stage2_usage_since(account_id, &since_day)?;
        let s1_usage = store.stage1_usage_since(account_id, &since_day)?;
        Ok((overrides, inbound, usage, s1_usage))
    })
    .await?;

    // Effective cap = override if present, else the config/env-layer value.
    let thread = overrides.thread_daily_cap.unwrap_or(state.stage2_thread_daily_cap);
    let sender = overrides.sender_daily_cap.unwrap_or(state.stage2_sender_daily_cap);
    let global = overrides.global_daily_cap.unwrap_or(state.stage2_global_daily_cap);
    let stage1_global = overrides
        .stage1_global_daily_cap
        .unwrap_or(state.stage1_global_daily_cap);

    let src = &state.stage2_cap_sources;
    let days = TRIAGE_CONFIG_TRAILING_DAYS as f64;
    let avg_inbound_per_day = inbound as f64 / days;
    let avg_stage2_calls_per_day = usage.calls as f64 / days;
    // Tokens/call are per-CALL means (null when the ledger has no calls yet).
    let per_call = |u: &squelch_core::store::Stage2Usage| {
        if u.calls == 0 {
            (serde_json::Value::Null, serde_json::Value::Null)
        } else {
            (
                json!(u.input_tokens as f64 / u.calls as f64),
                json!(u.output_tokens as f64 / u.calls as f64),
            )
        }
    };
    let (avg_tokens_in_per_call, avg_tokens_out_per_call) = per_call(&usage);
    let (s1_tokens_in_per_call, s1_tokens_out_per_call) = per_call(&s1_usage);

    Ok(json!({
        "thread_daily_cap": thread,
        "sender_daily_cap": sender,
        "global_daily_cap": global,
        "sources": {
            "thread_daily_cap": cap_source_str(overrides.thread_daily_cap.is_some(), src.thread_daily_cap),
            "sender_daily_cap": cap_source_str(overrides.sender_daily_cap.is_some(), src.sender_daily_cap),
            "global_daily_cap": cap_source_str(overrides.global_daily_cap.is_some(), src.global_daily_cap),
        },
        "avg_inbound_per_day": avg_inbound_per_day,
        "avg_stage2_calls_per_day": avg_stage2_calls_per_day,
        "avg_tokens_in_per_call": avg_tokens_in_per_call,
        "avg_tokens_out_per_call": avg_tokens_out_per_call,
        "price_in_per_mtok": state.stage2_price_in_per_mtok,
        "price_out_per_mtok": state.stage2_price_out_per_mtok,
        "stage2_model": state.stage2_model.as_ref(),
        // Stage-1 runs on every non-rule email and its only cap is GLOBAL.
        "stage1": {
            "model": state.stage1_model.as_ref(),
            "global_daily_cap": stage1_global,
            "source": cap_source_str(
                overrides.stage1_global_daily_cap.is_some(),
                src.stage1_global_daily_cap,
            ),
            "avg_calls_per_day": s1_usage.calls as f64 / days,
            "avg_tokens_in_per_call": s1_tokens_in_per_call,
            "avg_tokens_out_per_call": s1_tokens_out_per_call,
            "price_in_per_mtok": state.stage1_price_in_per_mtok,
            "price_out_per_mtok": state.stage1_price_out_per_mtok,
        },
    }))
}

pub async fn get_triage_config(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(triage_config_body(&state).await?))
}

/// POST body: any subset of the caps. Fields are raw JSON values on purpose, so
/// a non-integer fails our own validation with the contractual 400 rather than
/// the extractor's 422.
#[derive(Debug, Deserialize)]
pub struct TriageConfigBody {
    #[serde(default)]
    thread_daily_cap: Option<serde_json::Value>,
    #[serde(default)]
    sender_daily_cap: Option<serde_json::Value>,
    #[serde(default)]
    global_daily_cap: Option<serde_json::Value>,
    /// The Stage-1 GLOBAL daily cap; same validation and 400 as the others.
    #[serde(default)]
    stage1_global_daily_cap: Option<serde_json::Value>,
}

pub async fn set_triage_config(
    State(state): State<ApiState>,
    Json(body): Json<TriageConfigBody>,
) -> Result<impl IntoResponse, ApiError> {
    // ALL-OR-NOTHING: every provided cap is validated before anything is
    // written, so a bad value persists nothing.
    let min = squelch_core::config::STAGE2_CAP_MIN as i64;
    let max = squelch_core::config::STAGE2_CAP_MAX as i64;
    let bad = || {
        ApiError::bad_request(format!("each cap must be an integer in {min}..={max}"))
    };
    let validate = |v: &serde_json::Value| -> Result<u32, ApiError> {
        // `as_i64` is `Some` only for a JSON integer (5.5 / "5" => None).
        let n = v.as_i64().ok_or_else(bad)?;
        if (min..=max).contains(&n) {
            Ok(n as u32)
        } else {
            Err(bad())
        }
    };

    let mut updates: Vec<(&'static str, u32)> = Vec::new();
    if let Some(v) = &body.thread_daily_cap {
        updates.push((squelch_core::config::APP_SETTING_THREAD_DAILY_CAP, validate(v)?));
    }
    if let Some(v) = &body.sender_daily_cap {
        updates.push((squelch_core::config::APP_SETTING_SENDER_DAILY_CAP, validate(v)?));
    }
    if let Some(v) = &body.global_daily_cap {
        updates.push((squelch_core::config::APP_SETTING_GLOBAL_DAILY_CAP, validate(v)?));
    }
    if let Some(v) = &body.stage1_global_daily_cap {
        updates.push((
            squelch_core::config::APP_SETTING_STAGE1_GLOBAL_DAILY_CAP,
            validate(v)?,
        ));
    }

    if !updates.is_empty() {
        let store = state.store.clone();
        let account_id = state.account_id;
        let to_write = updates.clone();
        blocking(move || {
            for (key, value) in &to_write {
                store.set_app_setting(account_id, key, &value.to_string())?;
            }
            Ok(())
        })
        .await?;

        // detail names the caps set, e.g. "thread=5,global=300".
        let detail = updates
            .iter()
            .map(|(key, value)| {
                let name = match *key {
                    k if k == squelch_core::config::APP_SETTING_THREAD_DAILY_CAP => "thread",
                    k if k == squelch_core::config::APP_SETTING_SENDER_DAILY_CAP => "sender",
                    k if k == squelch_core::config::APP_SETTING_STAGE1_GLOBAL_DAILY_CAP => {
                        "stage1_global"
                    }
                    _ => "global",
                };
                format!("{name}={value}")
            })
            .collect::<Vec<_>>()
            .join(",");
        audit_action(&state, "triage_config", None, &detail).await;
    }

    // Return the fresh GET shape (effective values after applying).
    Ok(Json(triage_config_body(&state).await?))
}

// --- ACTIONS: the ONLY write capability in the system -----------------------
//
// Non-negotiable gates: every body MUST carry `"confirm": true` (else 400);
// `send` runs the outbound secret guard (422 unless `"override_guard": true`);
// EVERY action, attempted or completed, appends an audit row. Without a write
// credential the action is a 403 — sync/triage/MCP never load the write token.

/// Actor written into the audit log for all write actions.
const ACTION_ACTOR: &str = "client-api";

/// The `confirm` contract message returned on a missing/false confirm.
const CONFIRM_HINT: &str =
    "this action requires an explicit \"confirm\": true in the request body";

/// Append an audit row, best-effort: a failed insert is swallowed so it cannot
/// mask the action's own outcome.
pub(crate) async fn audit_action(
    state: &ApiState,
    action: &'static str,
    target: Option<String>,
    outcome: &str,
) {
    let store = state.store.clone();
    let account_id = state.account_id;
    let entry = NewAuditEntry {
        actor: ACTION_ACTOR.to_string(),
        action: action.to_string(),
        target,
        detail: Some(outcome.to_string()),
    };
    let _ = tokio::task::spawn_blocking(move || store.append_audit(account_id, &entry)).await;
}

/// RESOLUTION: mark a triage row `done` after a successful action. Best-effort,
/// so bookkeeping cannot mask the action's success; sealed rows are guarded in
/// the store, so this can never touch sealed mail.
async fn resolve_done(state: &ApiState, message_id: i64) {
    let store = state.store.clone();
    let account_id = state.account_id;
    let _ = tokio::task::spawn_blocking(move || {
        store.set_attention_status(account_id, message_id, AttentionStatus::Done)
    })
    .await;
}

/// Resolve the WRITE-bound gmail client, or 403 with a hint if none configured.
fn write_client(state: &ApiState) -> Result<GmailWriteClient, ApiError> {
    match state.write_creds() {
        Some(creds) => Ok(match state.write_api_base() {
            Some(base) => {
                GmailWriteClient::with_base(creds.clone(), state.account_id, base.to_string())
            }
            None => GmailWriteClient::new(creds.clone(), state.account_id),
        }),
        None => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "write credential not configured; run `squelchd auth --write`",
        )),
    }
}

/// Map a [`WriteError`] to an [`ApiError`], never echoing upstream detail.
fn write_error(e: &WriteError) -> ApiError {
    match e {
        WriteError::MissingCredential(_) => ApiError::new(
            StatusCode::FORBIDDEN,
            "write credential not configured; run `squelchd auth --write`",
        ),
        WriteError::Invalid(m) => ApiError::bad_request(m.clone()),
        WriteError::Api { .. } | WriteError::Transport(_) => {
            ApiError::new(StatusCode::BAD_GATEWAY, "gmail request failed")
        }
    }
}

/// Look up the (non-sealed) action target for a local message id.
async fn resolve_target(
    state: &ApiState,
    message_id: i64,
) -> Result<ActionMessageRef, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    tokio::task::spawn_blocking(move || store.action_message_ref(account_id, message_id))
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .map_err(ApiError::from)
}

// --- POST /client/actions/archive -------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ArchiveBody {
    message_id: i64,
    #[serde(default)]
    confirm: bool,
}

pub async fn action_archive(
    State(state): State<ApiState>,
    Json(body): Json<ArchiveBody>,
) -> Result<impl IntoResponse, ApiError> {
    let target = Some(body.message_id.to_string());

    if !body.confirm {
        audit_action(&state, "archive", target, "rejected:confirm").await;
        return Err(ApiError::bad_request(CONFIRM_HINT));
    }

    let client = match write_client(&state) {
        Ok(c) => c,
        Err(e) => {
            audit_action(&state, "archive", target, "rejected:no_write_credential").await;
            return Err(e);
        }
    };

    let msg = match resolve_target(&state, body.message_id).await {
        Ok(m) => m,
        Err(e) => {
            audit_action(&state, "archive", target, "failed:target").await;
            return Err(e);
        }
    };

    match client.archive(&msg.gmail_msg_id).await {
        Ok(()) => {
            // RESOLUTION: a successful archive auto-resolves the target row.
            resolve_done(&state, body.message_id).await;
            audit_action(&state, "archive", target, "ok").await;
            Ok(Json(json!({ "status": "archived", "message_id": body.message_id })))
        }
        Err(e) => {
            audit_action(&state, "archive", target, "failed:gmail").await;
            Err(write_error(&e))
        }
    }
}

// --- POST /client/actions/label ---------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LabelBody {
    message_id: i64,
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
    #[serde(default)]
    confirm: bool,
}

pub async fn action_label(
    State(state): State<ApiState>,
    Json(body): Json<LabelBody>,
) -> Result<impl IntoResponse, ApiError> {
    let target = Some(body.message_id.to_string());

    if !body.confirm {
        audit_action(&state, "label", target, "rejected:confirm").await;
        return Err(ApiError::bad_request(CONFIRM_HINT));
    }
    if body.add.is_empty() && body.remove.is_empty() {
        audit_action(&state, "label", target, "rejected:empty").await;
        return Err(ApiError::bad_request("label requires a non-empty add or remove list"));
    }

    let client = match write_client(&state) {
        Ok(c) => c,
        Err(e) => {
            audit_action(&state, "label", target, "rejected:no_write_credential").await;
            return Err(e);
        }
    };

    let msg = match resolve_target(&state, body.message_id).await {
        Ok(m) => m,
        Err(e) => {
            audit_action(&state, "label", target, "failed:target").await;
            return Err(e);
        }
    };

    match client.modify(&msg.gmail_msg_id, &body.add, &body.remove).await {
        Ok(()) => {
            audit_action(&state, "label", target, "ok").await;
            Ok(Json(json!({ "status": "labeled", "message_id": body.message_id })))
        }
        Err(e) => {
            audit_action(&state, "label", target, "failed:gmail").await;
            Err(write_error(&e))
        }
    }
}

// --- POST /client/actions/send ----------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SendBody {
    /// Reply to a stored message (thread + recipient + subject derived from it).
    #[serde(default)]
    reply_to_message_id: Option<i64>,
    /// Explicit recipient (overrides the reply default).
    #[serde(default)]
    to: Option<String>,
    /// Explicit subject (overrides the reply-derived subject).
    #[serde(default)]
    subject: Option<String>,
    body: String,
    #[serde(default)]
    confirm: bool,
    /// Override the outbound secret guard (still audited).
    #[serde(default)]
    override_guard: bool,
}

pub async fn action_send(
    State(state): State<ApiState>,
    Json(body): Json<SendBody>,
) -> Result<impl IntoResponse, ApiError> {
    let target = body.reply_to_message_id.map(|id| id.to_string());

    if !body.confirm {
        audit_action(&state, "send", target, "rejected:confirm").await;
        return Err(ApiError::bad_request(CONFIRM_HINT));
    }
    if body.body.trim().is_empty() {
        audit_action(&state, "send", target, "rejected:empty_body").await;
        return Err(ApiError::bad_request("send requires a non-empty body"));
    }

    // OUTBOUND GUARD: report only REDACTED kinds, never the matched text.
    let matches = guard::scan_kinds(&body.body);
    if !matches.is_empty() && !body.override_guard {
        audit_action(&state, "send", target, "blocked:guard").await;
        return Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!(
                "outbound guard blocked send; matched (redacted) kinds: {}. \
                 resend with \"override_guard\": true to send anyway",
                matches.join(", ")
            ),
        });
    }
    if !matches.is_empty() {
        // Overridden: record that the guard was bypassed (kinds only).
        audit_action(
            &state,
            "send",
            target.clone(),
            &format!("guard_override:{}", matches.join(",")),
        )
        .await;
    }

    let client = match write_client(&state) {
        Ok(c) => c,
        Err(e) => {
            audit_action(&state, "send", target, "rejected:no_write_credential").await;
            return Err(e);
        }
    };

    let (parent, thread_id) = match body.reply_to_message_id {
        Some(id) => match resolve_target(&state, id).await {
            Ok(m) => {
                let tid = m.thread_id.clone();
                (Some(m), Some(tid))
            }
            Err(e) => {
                audit_action(&state, "send", target, "failed:target").await;
                return Err(e);
            }
        },
        None => (None, None),
    };

    let to = match body.to.clone().filter(|s| !s.trim().is_empty()) {
        Some(t) => t,
        None => match &parent {
            Some(p) => p.from_addr.clone(),
            None => {
                audit_action(&state, "send", target, "rejected:no_recipient").await;
                return Err(ApiError::bad_request(
                    "send requires `to` (or `reply_to_message_id` to derive it)",
                ));
            }
        },
    };

    let subject = body
        .subject
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| match &parent {
            Some(p) => reply_subject(&p.subject),
            None => String::new(),
        });

    // Threading headers come from Gmail on the WRITE token (gmail.modify grants
    // read), never the read credential.
    let (in_reply_to, references) = match &parent {
        Some(p) => match client.parent_headers(&p.gmail_msg_id).await {
            Ok(h) => {
                let refs = build_references(h.message_id.as_deref(), h.references.as_deref());
                (h.message_id, refs)
            }
            // Non-fatal: send without threading headers rather than fail.
            Err(_) => (None, None),
        },
        None => (None, None),
    };

    let parts = ReplyParts {
        to,
        subject,
        body: body.body.clone(),
        in_reply_to,
        references,
    };
    let raw = match build_reply_rfc822(&parts) {
        Ok(r) => r,
        Err(e) => {
            audit_action(&state, "send", target, "rejected:compose").await;
            return Err(write_error(&e));
        }
    };

    match client.send(&raw, thread_id.as_deref()).await {
        Ok(()) => {
            // A cold send has no target row to resolve.
            if let Some(id) = body.reply_to_message_id {
                resolve_done(&state, id).await;
            }
            audit_action(&state, "send", target, "ok").await;
            Ok(Json(json!({ "status": "sent" })))
        }
        Err(e) => {
            audit_action(&state, "send", target, "failed:gmail").await;
            Err(write_error(&e))
        }
    }
}

// --- UNSUBSCRIBE: human-door-only, no agent-door exposure --------------------
//
// POST /client/unsubscribe hands the CLIENT the first http(s) List-Unsubscribe
// URL to confirm and open; the server NEVER makes the request itself. Sealed and
// unknown messages are an indistinguishable 404; no http(s) link is a 422.

/// Upsert the unsubscribe ledger row, resetting the violation clock: a fresh
/// request restarts the 72h grace.
async fn record_unsub(
    state: &ApiState,
    sender: &str,
    method: &'static str,
    source_message_id: i64,
) -> Result<(), ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let sender = sender.to_string();
    blocking(move || {
        store.upsert_unsubscribe(account_id, &sender, method, Some(source_message_id), Utc::now())
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct UnsubscribeBody {
    message_id: i64,
}

pub async fn unsubscribe(
    State(state): State<ApiState>,
    Json(body): Json<UnsubscribeBody>,
) -> Result<impl IntoResponse, ApiError> {
    let message_id = body.message_id;
    let target = Some(message_id.to_string());

    // `None` => missing OR sealed, indistinguishable, both 404.
    let store = state.store.clone();
    let account_id = state.account_id;
    let fields = blocking(move || store.message_unsub_fields(account_id, message_id))
        .await?
        .ok_or_else(ApiError::not_found)?;

    let sender = fields.from_addr.trim().to_ascii_lowercase();
    let plan = crate::unsubscribe::classify_unsubscribe(fields.list_unsubscribe.as_deref());

    match plan {
        crate::unsubscribe::UnsubPlan::None => {
            // No http(s) unsubscribe URL to hand the client.
            Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "message has no http(s) unsubscribe link",
            ))
        }
        crate::unsubscribe::UnsubPlan::Browser { url } => {
            // No outbound request is made here. The audit records the sender
            // only, never the URL.
            record_unsub(&state, &sender, "browser", message_id).await?;
            audit_action(&state, "unsubscribe", target, &format!("browser:{sender}")).await;
            Ok(Json(json!({ "method": "browser", "sender": sender, "url": url })))
        }
    }
}

// --- TRIAGE FEEDBACK (human corrections) ------------------------------------
//
// Applying the fix and recording it are ONE call on purpose: a correction the
// human sees take effect but that never reaches the dataset makes triage look
// better than it is. `to_value` is validated against TriageAxis::allowed, so a
// typo can never write a label the pipeline would not itself produce.

/// How many corrections GET returns by default / at most.
const FEEDBACK_DEFAULT_LIMIT: u32 = 200;
const FEEDBACK_MAX_LIMIT: u32 = 2000;
/// Ceiling on the optional free-text note.
const FEEDBACK_NOTE_MAX: usize = 500;

#[derive(Debug, Deserialize)]
pub struct TriageFeedbackBody {
    message_id: i64,
    /// Which axis was wrong: "tier" | "category".
    dimension: String,
    /// The value it should have had.
    to_value: String,
    #[serde(default)]
    note: Option<String>,
}

pub async fn post_triage_feedback(
    State(state): State<ApiState>,
    Json(body): Json<TriageFeedbackBody>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(axis) = TriageAxis::parse(body.dimension.trim()) else {
        return Err(ApiError::bad_request(
            "dimension must be one of: tier, category, sensitivity",
        ));
    };
    let to_value = body.to_value.trim().to_ascii_lowercase();
    if !axis.allowed().contains(&to_value.as_str()) {
        return Err(ApiError::bad_request(format!(
            "{} must be one of: {}",
            axis.as_str(),
            axis.allowed().join(", ")
        )));
    }
    let note = body
        .note
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if let Some(n) = &note
        && n.chars().count() > FEEDBACK_NOTE_MAX
    {
        return Err(ApiError::bad_request("note is too long"));
    }

    let store = state.store.clone();
    let account_id = state.account_id;
    let message_id = body.message_id;
    let to = to_value.clone();
    let n = note.clone();
    let recorded = blocking(move || {
        store.correct_triage(account_id, message_id, axis, &to, n.as_deref(), Utc::now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;

    audit_action(
        &state,
        "triage_correction",
        Some(message_id.to_string()),
        &format!(
            "{}:{}->{}",
            axis.as_str(),
            recorded.from_value.as_deref().unwrap_or("none"),
            to_value
        ),
    )
    .await;
    Ok(Json(recorded))
}

#[derive(Debug, Deserialize)]
pub struct FeedbackQuery {
    #[serde(default)]
    limit: Option<u32>,
}

/// The refinement dataset: where triage actually goes wrong.
pub async fn get_triage_feedback(
    State(state): State<ApiState>,
    Query(q): Query<FeedbackQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = q
        .limit
        .unwrap_or(FEEDBACK_DEFAULT_LIMIT)
        .clamp(1, FEEDBACK_MAX_LIMIT);
    let store = state.store.clone();
    let account_id = state.account_id;
    let items = blocking(move || store.list_triage_feedback(account_id, limit)).await?;
    Ok(Json(items))
}

// --- AUTH-MAIL SHREDDER (retention) -----------------------------------------
//
// Moves auth mail older than the policy window to Gmail's TRASH and records it
// in `shred_log`. Because this deletes a human's mail on a timer, the safety
// posture is non-negotiable: OFF by default; trash only (recoverable for 30
// days, and `gmail.modify` cannot permanently delete); a no-op without the
// opt-in write credential; bounded per pass, oldest first; and the ledger row
// lands only AFTER Gmail confirms, with an audit row for every shred.

const SHRED_ENABLED_KEY: &str = "shred_enabled";
const SHRED_AFTER_DAYS_KEY: &str = "shred_after_days";
/// Default retention window. Matches the Auth page's copy.
const SHRED_DEFAULT_DAYS: i64 = 30;
/// Floor on the window. A shorter one would start trashing codes while they are
/// plausibly still in use, which is a footgun we simply do not offer.
const SHRED_MIN_DAYS: i64 = 7;
const SHRED_MAX_DAYS: i64 = 365;
/// Messages trashed per pass. Bounds the API burst on a first run; the next
/// pass picks up where this one left off (candidates come back oldest-first).
const SHRED_BATCH: u32 = 50;
/// Window for the headline "shredded recently" figure.
const SHRED_RECENT_DAYS: i64 = 30;

/// Interpret the raw `app_settings` values into a policy. Unset or malformed
/// falls back to the SAFE default (off / 30d) rather than erroring: a corrupted
/// knob must never start deleting mail, nor take the Auth page down.
fn parse_shred_policy(enabled: Option<String>, days: Option<String>) -> (bool, i64) {
    let enabled = enabled
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let after_days = days
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|d| (SHRED_MIN_DAYS..=SHRED_MAX_DAYS).contains(d))
        .unwrap_or(SHRED_DEFAULT_DAYS);
    (enabled, after_days)
}

/// Read the account's retention policy off the store.
async fn shred_policy(state: &ApiState) -> Result<(bool, i64), ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    blocking(move || {
        let enabled = store.get_app_setting(account_id, SHRED_ENABLED_KEY)?;
        let days = store.get_app_setting(account_id, SHRED_AFTER_DAYS_KEY)?;
        Ok(parse_shred_policy(enabled, days))
    })
    .await
}

/// Assemble the Auth page's shredder panel state.
async fn shred_stats(state: &ApiState) -> Result<ShredStats, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let now = Utc::now();
    let recent_since = now - chrono::Duration::days(SHRED_RECENT_DAYS);

    let (enabled, after_days, pending, shredded_recent, shredded_total, last_shredded_at) =
        blocking(move || {
            let enabled = store.get_app_setting(account_id, SHRED_ENABLED_KEY)?;
            let days = store.get_app_setting(account_id, SHRED_AFTER_DAYS_KEY)?;
            let (enabled, after_days) = parse_shred_policy(enabled, days);
            let cutoff = now - chrono::Duration::days(after_days);
            let pending = store.shred_pending_count(account_id, cutoff)?;
            let (recent, total, last) = store.shred_counts(account_id, recent_since)?;
            Ok((enabled, after_days, pending, recent, total, last))
        })
        .await?;

    Ok(ShredStats {
        enabled,
        after_days,
        shredded_recent,
        shredded_total,
        last_shredded_at,
        pending,
        // Separate from `enabled` so the UI can tell "off" from "on but unable".
        write_ready: state.write_creds().is_some(),
    })
}

/// Run one bounded retention pass, returning how many messages were trashed.
/// Disabled or credential-less is `Ok(0)`, not an error, so a timer can fire it
/// unconditionally. A per-message Gmail failure is audited and skipped — it
/// neither aborts the pass nor writes a ledger row.
pub async fn run_shred_pass(state: &ApiState) -> Result<u32, ApiError> {
    let (enabled, after_days) = shred_policy(state).await?;
    if !enabled {
        return Ok(0);
    }
    let Ok(client) = write_client(state) else {
        // No write credential: nothing to do, and nothing to complain about.
        return Ok(0);
    };

    let cutoff = Utc::now() - chrono::Duration::days(after_days);
    let store = state.store.clone();
    let account_id = state.account_id;
    let candidates = blocking(move || store.shred_candidates(account_id, cutoff, SHRED_BATCH))
        .await?;

    let mut shredded = 0u32;
    for candidate in candidates {
        if client.trash(&candidate.gmail_msg_id).await.is_err() {
            // Skipped with no ledger row, so a later pass retries it naturally.
            audit_action(
                state,
                "shred_failed",
                Some(candidate.message_id.to_string()),
                &candidate.sender,
            )
            .await;
            continue;
        }
        // Ledger AFTER Gmail confirms, so the count can never overstate.
        let store = state.store.clone();
        let c = candidate.clone();
        let recorded =
            blocking(move || store.record_shred(account_id, &c, Utc::now())).await;
        if recorded.is_err() {
            // The mail IS trashed and only the bookkeeping failed: say so in the
            // audit trail rather than silently under-counting.
            audit_action(
                state,
                "shred_unrecorded",
                Some(candidate.message_id.to_string()),
                &candidate.sender,
            )
            .await;
            continue;
        }
        audit_action(
            state,
            "shred",
            Some(candidate.message_id.to_string()),
            &format!("trash:{}", candidate.sender),
        )
        .await;
        shredded += 1;
    }
    Ok(shredded)
}

pub async fn get_shredder(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(shred_stats(&state).await?))
}

#[derive(Debug, Deserialize)]
pub struct ShredderBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    after_days: Option<i64>,
}

pub async fn set_shredder(
    State(state): State<ApiState>,
    Json(body): Json<ShredderBody>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(days) = body.after_days {
        if !(SHRED_MIN_DAYS..=SHRED_MAX_DAYS).contains(&days) {
            return Err(ApiError::bad_request(format!(
                "after_days must be between {SHRED_MIN_DAYS} and {SHRED_MAX_DAYS}"
            )));
        }
        let store = state.store.clone();
        let account_id = state.account_id;
        let v = days.to_string();
        blocking(move || store.set_app_setting(account_id, SHRED_AFTER_DAYS_KEY, &v)).await?;
    }
    if let Some(enabled) = body.enabled {
        let store = state.store.clone();
        let account_id = state.account_id;
        let v = if enabled { "1" } else { "0" }.to_string();
        blocking(move || store.set_app_setting(account_id, SHRED_ENABLED_KEY, &v)).await?;
        // Turning automatic deletion on or off is a policy change worth a row.
        audit_action(
            &state,
            "shred_policy",
            None,
            if enabled { "enabled" } else { "disabled" },
        )
        .await;
    }
    Ok(Json(shred_stats(&state).await?))
}

/// POST /client/shredder/run — run a pass now, without waiting for the timer.
pub async fn run_shredder(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let shredded = run_shred_pass(&state).await?;
    let stats = shred_stats(&state).await?;
    Ok(Json(json!({ "shredded": shredded, "stats": stats })))
}

// --- GET /client/unsubscribes -----------------------------------------------

pub async fn list_unsubscribes(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    // Newest requested_at first.
    let items = blocking(move || store.list_unsubscribes(account_id)).await?;
    Ok(Json(items))
}

// --- POST /client/unsubscribes/resolution -----------------------------------

#[derive(Debug, Deserialize)]
pub struct ResolutionBody {
    sender: String,
    resolution: String,
}

pub async fn unsubscribe_resolution(
    State(state): State<ApiState>,
    Json(body): Json<ResolutionBody>,
) -> Result<impl IntoResponse, ApiError> {
    let resolution = match body.resolution.as_str() {
        "blocked" | "dismissed" => body.resolution.clone(),
        _ => {
            return Err(ApiError::bad_request(
                "resolution must be one of: blocked, dismissed",
            ));
        }
    };
    let sender = body.sender.trim().to_ascii_lowercase();
    if sender.is_empty() {
        return Err(ApiError::bad_request("sender must not be empty"));
    }

    let store = state.store.clone();
    let account_id = state.account_id;
    let s = sender.clone();
    let r = resolution.clone();
    let updated = blocking(move || store.set_unsubscribe_resolution(account_id, &s, &r)).await?;
    if !updated {
        return Err(ApiError::not_found());
    }

    audit_action(
        &state,
        "unsub_resolution",
        Some(sender.clone()),
        &format!("{sender}:{resolution}"),
    )
    .await;
    Ok(Json(json!({ "sender": sender, "resolution": resolution })))
}

#[cfg(test)]
mod shred_policy_tests {
    use super::*;

    #[test]
    fn unset_policy_is_off_at_the_default_window() {
        // An account that never opted in never deletes mail.
        assert_eq!(parse_shred_policy(None, None), (false, SHRED_DEFAULT_DAYS));
    }

    #[test]
    fn enabled_accepts_the_stored_form_and_a_friendly_one() {
        assert!(parse_shred_policy(Some("1".into()), None).0);
        assert!(parse_shred_policy(Some("true".into()), None).0);
        assert!(parse_shred_policy(Some("TRUE".into()), None).0);
        assert!(!parse_shred_policy(Some("0".into()), None).0);
        // Anything unrecognized reads as OFF, never as on.
        assert!(!parse_shred_policy(Some("yes".into()), None).0);
        assert!(!parse_shred_policy(Some(String::new()), None).0);
    }

    #[test]
    fn window_is_clamped_to_the_supported_range() {
        assert_eq!(parse_shred_policy(None, Some("90".into())).1, 90);
        assert_eq!(parse_shred_policy(None, Some(" 45 ".into())).1, 45);
        // Out of range and garbage fall back to the default rather than
        // deleting on an aggressive window.
        assert_eq!(
            parse_shred_policy(None, Some("1".into())).1,
            SHRED_DEFAULT_DAYS
        );
        assert_eq!(
            parse_shred_policy(None, Some("100000".into())).1,
            SHRED_DEFAULT_DAYS
        );
        assert_eq!(
            parse_shred_policy(None, Some("-30".into())).1,
            SHRED_DEFAULT_DAYS
        );
        assert_eq!(
            parse_shred_policy(None, Some("soon".into())).1,
            SHRED_DEFAULT_DAYS
        );
    }

    #[test]
    fn a_corrupt_knob_never_enables_deletion() {
        for raw in ["", "  ", "off", "null", "2", "-1", "on"] {
            assert!(
                !parse_shred_policy(Some(raw.into()), Some("30".into())).0,
                "{raw:?} must not read as enabled"
            );
        }
    }
}
