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
use squelch_core::CoreError;
use squelch_core::store::{
    ActionMessageRef, Draft, NewAuditEntry, SearchFilter, SitrepBand, SqliteStore, Store,
};
use squelch_core::sync::{decode_raw_b64url, parse_internal_date};
use squelch_core::triage::llm::Usage;
use squelch_core::triage::rule_infer;
use squelch_core::types::{AccountId, AttentionStatus, Disposition, ShredStats, Tier, TriageAxis};
use std::collections::HashMap;
use std::time::Duration;

use crate::error::ApiError;
use crate::gmail_write::{
    GmailWriteClient, ReplyParts, SentRef, WriteError, build_references, build_reply_rfc822,
    cc_excluding, derive_reply_recipients, reply_subject,
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

/// How long the post-send echo may spend on its read-back (see [`echo_sent`]).
const ECHO_FETCH_TIMEOUT_SECS: u64 = 5;

// --- pagination cursor ------------------------------------------------------

/// An opaque token round-tripping a row offset (`off:<n>`, base64url). Not
/// security-sensitive; it is a scroll position. Unpadded base64url, the same
/// alphabet and padding discipline the Gmail write path uses.
mod cursor {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    pub fn encode_offset(offset: u32) -> String {
        URL_SAFE_NO_PAD.encode(format!("off:{offset}"))
    }

    pub fn decode_offset(s: &str) -> Option<u32> {
        let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        text.strip_prefix("off:")?.parse().ok()
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

/// Run a store call for the ACTIVE ACCOUNT off the async runtime: the preamble
/// every handler shares (clone the `Arc`, copy the account id, hop to a blocking
/// thread). The account id is threaded in rather than captured, so no call site
/// can accidentally query a different account.
pub(crate) async fn store_call<T, F>(state: &ApiState, f: F) -> Result<T, ApiError>
where
    F: FnOnce(&SqliteStore, AccountId) -> Result<T, CoreError> + Send + 'static,
    T: Send + 'static,
{
    let store = state.store.clone();
    let account_id = state.account_id;
    blocking(move || f(&store, account_id)).await
}

/// [`store_call`] for the common read shape: whatever the store returns IS the
/// JSON response body.
pub(crate) async fn query<T, F>(state: &ApiState, f: F) -> Result<Json<T>, ApiError>
where
    F: FnOnce(&SqliteStore, AccountId) -> Result<T, CoreError> + Send + 'static,
    T: Send + 'static,
{
    Ok(Json(store_call(state, f).await?))
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
    /// READ WITHOUT SURFACING. Default false, so every existing client keeps the
    /// stamping fetch it has always had. `true` is for a reader acting on the
    /// user's behalf (the embedded agent answering "anything important today?"):
    /// it sees the whole page, but shows the human only the few rows it decides
    /// are worth raising. Stamping all of them would report rows as seen that
    /// nobody ever saw, collapsing New into Open and emptying the band the user
    /// reads. Whatever the agent does surface is stamped by the client's own
    /// (non-peek) fetch of those rows.
    #[serde(default)]
    peek: bool,
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
    let peek = q.peek;

    let items = store_call(&state, move |store, account_id| {
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
        // `peek` skips the stamp entirely: same rows, no ledger write, because
        // returning a row to an agent is not the same event as showing it to
        // the user. This is the ONLY thing peek changes.
        if !peek {
            let ids: Vec<i64> = page.iter().map(|u| u.update.id).collect();
            store.mark_surfaced(account_id, &ids)?;
        }

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

    let updated = store_call(&state, move |store, account_id| {
        store.set_attention_status(account_id, message_id, status)
    })
    .await?;
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

    Ok(Json(
        json!({ "status": status.as_str(), "message_id": message_id }),
    ))
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

    let msg_id = body.message_id;
    let reset = store_call(&state, move |store, account_id| {
        store.retriage_reset(account_id, msg_id, days)
    })
    .await?;

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
    let dbg = store_call(&state, move |store, account_id| {
        store.triage_debug(account_id, message_id)
    })
    .await?
    .ok_or(ApiError::not_found())?;
    Ok(Json(dbg))
}

// --- GET /client/thread/{thread_id} -----------------------------------------

/// One message of the served thread: the store's [`ClientMessage`] verbatim,
/// flattened, plus the read-time known-sender bit. Flattened rather than nested
/// so every field a client already decodes keeps its place on the wire.
#[derive(Debug, Serialize)]
struct ThreadMessageView {
    #[serde(flatten)]
    message: squelch_core::types::ClientMessage,
    /// `true` when this message's `from_addr` is a Sent-derived contact AND the
    /// mail passed email authentication at ingest. The contact half is the SAME
    /// `Store::is_known_contact` signal triage trusts, not a parallel notion.
    /// The reader uses it to decide whether to strip tracking pixels: somebody
    /// the user writes to is allowed to learn the mail was opened.
    ///
    /// The auth half is what stops a stranger from spoofing `From:` into the
    /// bypass — a From header alone is free to write. It comes from the stored
    /// `auth_pass`, so it needs a re-sync to change; the contact half is
    /// computed per request, so a newly-written-to sender takes effect on the
    /// next open with no re-ingest.
    sender_known: bool,
}

/// `ClientThreadView` on the wire, message-for-message, with the extra bit.
#[derive(Debug, Serialize)]
struct ThreadResponse {
    thread_id: String,
    subject: String,
    messages: Vec<ThreadMessageView>,
}

pub async fn get_thread(
    State(state): State<ApiState>,
    Path(thread_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // Sealed and nonexistent threads are the SAME NotFound. Each message carries
    // its server-sanitized `html`, null for plain-text-only mail.
    query(&state, move |store, account_id| {
        let view = store.thread_view_with_html(account_id, &thread_id)?;
        // Memoized per distinct sender: a thread is usually two addresses
        // repeated, and each miss is a round trip to `contacts`. NOCASE is the
        // column's own collation, so the key is lowercased to match.
        let mut seen: HashMap<String, bool> = HashMap::new();
        let mut messages = Vec::with_capacity(view.messages.len());
        for message in view.messages {
            let known = match seen.get(&message.from_addr.to_ascii_lowercase()) {
                Some(known) => *known,
                None => {
                    let known = store.is_known_contact(account_id, &message.from_addr)?;
                    seen.insert(message.from_addr.to_ascii_lowercase(), known);
                    known
                }
            };
            // AND'd with the ingest-time email-authentication verdict, and
            // gated HERE rather than inside `is_known_contact`: that call also
            // feeds triage's known-contact importance floor at ingest, which
            // must not move. A NULL `auth_pass` — mail never evaluated, or
            // ingested before the column existed — reads as NO bypass, the safe
            // default for a permissive feature, so nothing is backfilled.
            let sender_known = known && message.auth_pass == Some(true);
            messages.push(ThreadMessageView {
                message,
                sender_known,
            });
        }
        Ok(ThreadResponse {
            thread_id: view.thread_id,
            subject: view.subject,
            messages,
        })
    })
    .await
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
        .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '/' && *c != '\\' && *c != '"')
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
    // `None` => unknown id OR sealed parent (indistinguishable): 404.
    let found = store_call(&state, move |store, account_id| {
        store.attachment_bytes(account_id, attachment_id)
    })
    .await?;
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
    /// The raw query, operators included (`invoice from:jane after:2026-01-01`).
    /// Parsed exactly once, here, then threaded into the store as
    /// `(text, filter)`.
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

/// The recall window semantic/hybrid rank before the page is cut out of it.
///
/// With no filter it is exactly the page. With one, the filter is applied
/// POST-HOC to hydrated hits (KNN and RRF cannot carry a `from:`/date
/// predicate), so we over-fetch to leave room for the rows it drops.
/// APPROXIMATION: a heavily-filtered query can still under-fill a page — the
/// matching mail may sit below the window entirely.
///
/// `saturating_add` because the offset comes off the wire (a crafted cursor
/// must not overflow), and BOTH branches cap: past `MAX_RECALL_K` the fused
/// ranking is noise anyway, and an uncapped `k` would hand sqlite-vec a
/// KNN the size of the cursor.
fn recall_k(limit: u32, offset: u32, filter: &SearchFilter) -> usize {
    const MAX_RECALL_K: usize = 600;
    let page = (limit as usize).saturating_add(offset as usize);
    if filter.is_empty() {
        page.min(MAX_RECALL_K)
    } else {
        page.saturating_mul(3).min(MAX_RECALL_K)
    }
}

pub async fn search(
    State(state): State<ApiState>,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if query.q.trim().is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    // ONE parse for every leg below: operators come out as a `SearchFilter`,
    // what is left is the text to rank on.
    let (term, filter) = squelch_core::store::parse_search_query(query.q.trim());
    let term = term.trim().to_string();
    if term.is_empty() && filter.is_empty() {
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
    // than erroring — but the response reports the kind actually run. A query
    // that is nothing BUT operators has no text to rank, so every mode collapses
    // to the keyword leg's filter-only listing.
    let effective = if term.is_empty() {
        SearchMode::Keyword
    } else {
        match mode {
            SearchMode::Semantic | SearchMode::Hybrid if !have_vectors => SearchMode::Keyword,
            other => other,
        }
    };

    let k = recall_k(limit, offset, &filter);

    // Keyword paginates and filters in SQL; semantic/hybrid rank a top-k window,
    // filter the hydrated hits, and offset the fused slice. EVERY leg excludes
    // sealed rows in SQL. The bool is the recall legs' WINDOW FULL signal (see
    // below); the keyword leg paginates exactly, so it never needs one.
    let (items, window_full) = store_call(&state, move |store, account_id| match effective {
        SearchMode::Keyword => store
            .search_filtered(account_id, &term, &filter, limit, offset)
            .map(|hits| (hits, false)),
        SearchMode::Semantic => {
            let (mut hits, window_full) =
                store.semantic_search_hits(account_id, &term, &filter, k)?;
            let page: Vec<_> = hits
                .drain(..)
                .skip(offset as usize)
                .take(limit as usize)
                .collect();
            Ok((page, window_full))
        }
        SearchMode::Hybrid => {
            let (mut hits, window_full) = store.hybrid_search(account_id, &term, &filter, k)?;
            let page: Vec<_> = hits
                .drain(..)
                .skip(offset as usize)
                .take(limit as usize)
                .collect();
            Ok((page, window_full))
        }
    })
    .await?;

    // A full page always advances by `limit`. A SHORT page normally means the
    // results are exhausted — except on a filtered recall leg, where the filter
    // may have thinned a FULL candidate window: there the walk continues,
    // advancing by what was actually served (the offset indexes the filtered
    // sequence, and ranking is deterministic). A short page that served
    // NOTHING ends the walk even window-full: the same offset would rebuild
    // the same window and hand the client the same empty page forever.
    let next = match next_cursor(items.len(), limit, offset) {
        Some(cursor) => Some(cursor),
        None if window_full && !items.is_empty() => {
            Some(cursor::encode_offset(offset + items.len() as u32))
        }
        None => None,
    };
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
    // The shipments table holds no sealed rows by construction: detection never
    // runs on sealed mail, so there is no sealed filtering to apply.
    query(&state, move |store, account_id| {
        store.list_shipments(account_id, q.include_delivered)
    })
    .await
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
    let days = q.days.unwrap_or(DEFAULT_RECEIPTS_DAYS);
    // Newest-first. Structurally sealed-free, like shipments.
    query(&state, move |store, account_id| {
        store.list_receipts(account_id, days)
    })
    .await
}

// --- GET /client/banking -----------------------------------------------------

/// SERIALIZED SHAPE IS A WIRE CONTRACT — the desktop Banking zone decodes
/// exactly [`squelch_core::types::Banking`], newest-received first, with every
/// extracted field nullable and `account_hint` only ever a masked last-4 tail.
/// Sealed mail can never produce a banking row (structural, like receipts).
pub async fn get_banking(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    query(&state, |store, account_id| store.list_banking(account_id)).await
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
    let days = q
        .days
        .unwrap_or(DEFAULT_MARKETING_DAYS)
        .clamp(1, MAX_MARKETING_DAYS);
    query(&state, move |store, account_id| {
        store.marketing_offers(account_id, days, MARKETING_LIMIT)
    })
    .await
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
    let hours = q
        .hours
        .unwrap_or(DEFAULT_CALENDAR_HOURS)
        .clamp(MIN_CALENDAR_HOURS, MAX_CALENDAR_HOURS);
    query(&state, move |store, account_id| {
        store.list_calendar_updates(account_id, hours)
    })
    .await
}

// --- GET/POST/DELETE /client/rules ------------------------------------------

pub async fn list_rules(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    query(&state, |store, account_id| {
        store.list_sender_rules(account_id)
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct CreateRuleBody {
    match_pattern: String,
    want: String,
    /// What the rule DOES. Optional: absent (or the literal `"auto"`) means the
    /// daemon infers it from `want`, the plain-English sentence the composer
    /// asks for, so the client never has to make the user pick a taxonomy.
    /// An explicit value is honored verbatim.
    #[serde(default)]
    disposition: Option<String>,
    /// The message the user was acting FROM (the block-sender flows), so a
    /// squelch rule can resolve it: blocking the sender IS that email's
    /// disposition. Optional — the rule editor has no source message.
    #[serde(default)]
    source_message_id: Option<i64>,
    /// INTENT TO MASS-RESOLVE. `true` only from the block-sender flow, where the
    /// user's click means "and clear what they already sent"; every other path
    /// (the rule editor's save, an undo recreating a deleted rule) omits it.
    ///
    /// Separate from the disposition on purpose: "this rule squelches" and "sweep
    /// this sender out of the inbox right now" are different claims, and undoing
    /// a rule delete must be able to make the first without making the second.
    #[serde(default)]
    sweep: bool,
}

/// The literal `disposition` value that asks for inference explicitly, for a
/// client that would rather send a value than omit the key. EXACT MATCH: the real
/// dispositions parse exactly, so this one does too.
const DISPOSITION_AUTO: &str = "auto";

const DISPOSITION_HINT: &str = "disposition must be surface, squelch, or filtered (or omitted, or \"auto\", \
     to infer it from want)";

/// A rule write's disposition, plus WHO decided it. The flag is
/// security-relevant: an inferred verdict is a guess about one sentence, so it
/// may set what the rule does but must not trigger the destructive side effect
/// an explicit squelch does. See [`create_rule`].
struct ResolvedDisposition {
    disposition: Disposition,
    /// `true` when the REQUEST BODY named this disposition; `false` when a model
    /// inferred it from `want`.
    explicit: bool,
    /// What the inference call spent, when one ran. Billed by the caller into
    /// [`RULE_INFER_LEDGER_CATEGORY`]; `None` when no call happened.
    usage: Option<Usage>,
}

/// The usage-ledger category for rule-disposition inference. Its own line, not
/// folded into `stage1`: this is interactive, user-triggered spend, and a call
/// that lands in no category is spend no cap and no report can see.
const RULE_INFER_LEDGER_CATEGORY: &str = "rule_infer";

/// Bill one inference call. Best-effort by design: the rule is already written
/// when this runs, and a ledger write that fails must not fail the save. Runs
/// INSIDE the caller's existing store hop, so it costs no extra blocking task.
fn bill_rule_inference(store: &SqliteStore, account_id: AccountId, usage: Option<Usage>) {
    let Some(u) = usage else { return };
    let day = Utc::now().format("%Y-%m-%d").to_string();
    if let Err(e) = store.extract_bump_usage(
        account_id,
        &day,
        RULE_INFER_LEDGER_CATEGORY,
        u.input_tokens,
        u.output_tokens,
    ) {
        eprintln!("squelch: rule inference usage ledger bump failed ({e})");
    }
}

/// Resolve the disposition for a rule write: an explicit value parses as before,
/// and an absent/`auto` one is inferred from the owner's `want` sentence.
///
/// The classifier is fed the OWNER'S SENTENCE ONLY — never a subject, body, or
/// any other message content — so a hostile email can never author a rule about
/// its own sender. See [`squelch_core::triage::rule_infer`].
///
/// Inference NEVER fails a save: no LLM configured, an API error, or a refusal
/// all resolve to `filtered`, which is the safe answer (a filtered rule defers to
/// Stage-2, which reads the verbatim want text per email).
///
/// INFERENCE IS OPT-IN, EXACTLY: only an ABSENT key or the exact literal `"auto"`
/// asks for it. Every other present value must parse as a real disposition or the
/// write is a 400 — a typo, a stray empty string, or a case variant is a client
/// bug, and quietly inferring instead would hand a model a decision the client
/// believed it had made itself.
async fn resolve_disposition(
    state: &ApiState,
    body: &CreateRuleBody,
) -> Result<ResolvedDisposition, ApiError> {
    let named = match body.disposition.as_deref() {
        None => None,
        Some(DISPOSITION_AUTO) => None,
        Some(raw) => Some(raw),
    };

    if let Some(raw) = named {
        let disposition =
            Disposition::parse(raw).ok_or_else(|| ApiError::bad_request(DISPOSITION_HINT))?;
        return Ok(ResolvedDisposition {
            disposition,
            explicit: true,
            usage: None,
        });
    }

    // Inference needs something to read. An empty want with no disposition is
    // not a rule anyone could act on, so it is a 400 rather than a silent
    // filtered rule the store would reject anyway.
    if body.want.trim().is_empty() {
        return Err(ApiError::bad_request(
            "want must not be empty when disposition is omitted; say in plain words what you \
             do, or do not, want from this sender",
        ));
    }

    let (disposition, usage) = match state.rule_infer() {
        Some(llm) => rule_infer::infer_disposition(&body.want, llm).await,
        // No model configured: filtered is the safe standing answer.
        None => (Disposition::Filtered, None),
    };
    Ok(ResolvedDisposition {
        disposition,
        explicit: false,
        usage,
    })
}

/// SIDE-EFFECT GUARD (security-relevant): may this rule write mark the sender's
/// existing mail resolved?
///
/// TWO CONDITIONS, BOTH REQUIRED.
///
/// 1. An EXPLICIT squelch. An INFERRED squelch is one cheap model's read of one
///    sentence, and the side effect is destructive at scale (every open row from
///    that sender goes to done); a misclassification must never mass-resolve
///    mail. The inferred rule still takes effect on FUTURE mail, which is
///    recoverable by editing the rule.
/// 2. `sweep`, the client's separate statement that this write means "and clear
///    what they already sent". Only the block-sender flow sets it. A rule editor
///    save, or an undo recreating a rule the user just deleted, restores the
///    squelch WITHOUT re-sweeping an inbox the user may have since gone through.
fn may_resolve_sender_mail(resolved: &ResolvedDisposition, sweep: bool) -> bool {
    sweep && resolved.explicit && resolved.disposition == Disposition::Squelch
}

pub async fn create_rule(
    State(state): State<ApiState>,
    Json(body): Json<CreateRuleBody>,
) -> Result<impl IntoResponse, ApiError> {
    if body.match_pattern.trim().is_empty() {
        return Err(ApiError::bad_request("match_pattern must not be empty"));
    }
    let resolved = resolve_disposition(&state, &body).await?;
    let disposition = resolved.disposition;

    let pattern = body.match_pattern.trim().to_string();
    let source_message_id = body.source_message_id;
    let sweep = body.sweep;
    let usage = resolved.usage;
    let id = store_call(&state, move |store, account_id| {
        let id = store.set_sender_rule(account_id, &body.match_pattern, &body.want, disposition)?;
        bill_rule_inference(store, account_id, usage);
        Ok(id)
    })
    .await?;
    // Best-effort audit: target is the match_pattern, detail the new rule id and
    // the RESOLVED disposition — an inferred verdict has to be as traceable as a
    // named one, since nothing in the request body records it.
    audit_action(
        &state,
        "rule.create",
        Some(pattern.clone()),
        &format!("{id} disposition={}", disposition.as_str()),
    )
    .await;
    // RESOLUTION: a squelch rule marks that sender's open mail done — the user
    // just said "never again" about them, and leaving the mail they already sent
    // in the bands makes the rule look like it did nothing. Squelch only: a
    // surface/filtered rule is tuning, not a verdict.
    //
    // EXPLICIT squelch AND `sweep` — see [`may_resolve_sender_mail`]: an inferred
    // squelch never mass-resolves mail, and neither does a save that did not ask
    // to sweep (the editor, an undo).
    //
    // Driven by the PATTERN, not by source_message_id, so the block prompt in
    // the action layer — which has no message on screen to name — resolves too.
    // Wildcards are left alone: `*@acme.com` is a domain verdict, and resolving
    // every address under a domain from one click is a bigger claim than the
    // click made.
    if may_resolve_sender_mail(&resolved, sweep) {
        if !pattern.contains('*') {
            resolve_sender_done(&state, &pattern).await;
        } else if let Some(mid) = source_message_id {
            resolve_done(&state, mid).await;
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "rule_id": id, "disposition": disposition.as_str() })),
    ))
}

pub async fn update_rule(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(body): Json<CreateRuleBody>,
) -> Result<impl IntoResponse, ApiError> {
    if body.match_pattern.trim().is_empty() {
        return Err(ApiError::bad_request("match_pattern must not be empty"));
    }
    // An edit re-infers from the SUBMITTED want when the client omits the
    // disposition: the sentence is what the rule means, so rewriting it can
    // legitimately change what the rule does.
    let resolved = resolve_disposition(&state, &body).await?;
    let disposition = resolved.disposition;

    let pattern = body.match_pattern.clone();
    let usage = resolved.usage;
    let updated = store_call(&state, move |store, account_id| {
        let updated = store.update_sender_rule(
            account_id,
            id,
            &body.match_pattern,
            &body.want,
            disposition,
        )?;
        // Billed even when the id was bogus: the model call happened and was
        // paid for regardless of what the store did with the row.
        bill_rule_inference(store, account_id, usage);
        Ok(updated)
    })
    .await?;
    if updated {
        // Only a real edit is recorded; a 404 changed nothing, so no row.
        audit_action(
            &state,
            "rule.update",
            Some(pattern),
            &format!("{id} disposition={}", disposition.as_str()),
        )
        .await;
        Ok(Json(
            json!({ "rule_id": id, "disposition": disposition.as_str() }),
        ))
    } else {
        Err(ApiError::not_found())
    }
}

pub async fn delete_rule(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let deleted = store_call(&state, move |store, account_id| {
        store.delete_sender_rule(account_id, id)
    })
    .await?;
    if deleted {
        // target is the rule id — the pattern is gone post-delete.
        audit_action(&state, "rule.delete", Some(id.to_string()), "ok").await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

// --- GET/PUT/DELETE /client/drafts ------------------------------------------
//
// Drafts are LOCAL and human-door only: nothing here is synced to Gmail Drafts
// and no draft ever leaves the machine. Hence NO AUDIT ROWS anywhere in this
// section — the audit log is the ledger of reveals and Gmail writes, and a
// composition that never left is neither.

/// One draft on the wire. `to_addr` is spelled `to` here: the field name the
/// composer sends and the send endpoint uses.
#[derive(Debug, Serialize)]
struct DraftView {
    id: i64,
    reply_to_message_id: Option<i64>,
    to: String,
    subject: String,
    body: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<Draft> for DraftView {
    fn from(d: Draft) -> Self {
        Self {
            id: d.id,
            reply_to_message_id: d.reply_to_message_id,
            to: d.to_addr,
            subject: d.subject,
            body: d.body,
            created_at: d.created_at,
            updated_at: d.updated_at,
        }
    }
}

/// The `Cache-Control: no-store` header pair, one spelling for every response
/// that must not be retained anywhere on the path: a revealed sealed body, and a
/// draft (an unsent composition is the user's own thinking, not something to leave
/// in a proxy or a disk cache).
fn no_store() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    headers
}

pub async fn list_drafts(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let drafts = store_call(&state, |store, account_id| store.list_drafts(account_id)).await?;
    let items: Vec<DraftView> = drafts.into_iter().map(DraftView::from).collect();
    Ok((no_store(), Json(items)))
}

// --- GET /client/contacts ---------------------------------------------------
//
// Recipient autocomplete over the Sent-derived contacts table. HUMAN DOOR ONLY:
// who the user writes to is exactly the kind of graph the agent door must never
// see, so no `/mcp` surface touches this table.

#[derive(Debug, Deserialize)]
pub struct ContactsQuery {
    q: String,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ContactView {
    addr: String,
    display_name: Option<String>,
    sent_count: i64,
    last_sent_at: Option<String>,
}

pub async fn get_contacts(
    State(state): State<ApiState>,
    Query(params): Query<ContactsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let q = params.q.trim().to_string();
    if q.is_empty() {
        return Ok((no_store(), Json(Vec::<ContactView>::new())));
    }
    // Suggestion-menu sized by default; capped so no caller turns this into a
    // contacts dump endpoint.
    let limit = params.limit.unwrap_or(8).min(25);
    let hits = store_call(&state, move |store, account_id| {
        store.search_contacts(account_id, &q, limit)
    })
    .await?;
    let items: Vec<ContactView> = hits
        .into_iter()
        .map(|c| ContactView {
            addr: c.addr,
            display_name: c.display_name,
            sent_count: c.sent_count,
            last_sent_at: c.last_sent_at.map(|d| d.to_rfc3339()),
        })
        .collect();
    Ok((no_store(), Json(items)))
}

#[derive(Debug, Deserialize)]
pub struct DraftBody {
    /// The message being replied to; absent or null addresses the account's
    /// single new-message draft.
    #[serde(default)]
    reply_to_message_id: Option<i64>,
    /// Every text field is optional — a half-composed draft is the normal case,
    /// so a missing (or null) field stores "" rather than 400ing.
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// `PUT /client/drafts` — save the draft for one key (reply target, or the
/// new-message slot). Idempotent by key: a second PUT edits the same row.
pub async fn put_draft(
    State(state): State<ApiState>,
    Json(body): Json<DraftBody>,
) -> Result<impl IntoResponse, ApiError> {
    // No draft is ever SAVED against sealed mail: the parent is resolved through
    // the same lookup `send` uses, where sealed and unknown are one 404. A
    // POST-HOC seal (hand correction, or a re-ingest that trips detection) is the
    // store's job — both seal paths delete the drafts keyed to that message, and
    // `list_drafts` filters a sealed parent as a belt.
    if let Some(parent) = body.reply_to_message_id {
        resolve_target(&state, parent).await?;
    }
    let reply_to = body.reply_to_message_id;
    let to = body.to.unwrap_or_default();
    let subject = body.subject.unwrap_or_default();
    let text = body.body.unwrap_or_default();

    let draft = store_call(&state, move |store, account_id| {
        store.upsert_draft(account_id, reply_to, &to, &subject, &text, Utc::now())
    })
    .await?;
    Ok((no_store(), Json(DraftView::from(draft))))
}

/// `DELETE /client/drafts/{id}` — discard one draft. Another account's id and an
/// unknown id are the same 404.
pub async fn delete_draft(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let deleted = store_call(&state, move |store, account_id| {
        store.delete_draft(account_id, id)
    })
    .await?;
    if deleted {
        Ok(Json(json!({ "status": "deleted", "id": id })))
    } else {
        Err(ApiError::not_found())
    }
}

// --- POST /client/markdown/preview -------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MarkdownPreviewBody {
    pub source: String,
}

/// The composer/signature preview: the SAME render the send path uses for the
/// HTML half of multipart/alternative, so what the preview shows is what a
/// send of this source would carry. Stateless — renders and returns, stores
/// and audits nothing, because nothing happened.
pub async fn markdown_preview(
    Json(body): Json<MarkdownPreviewBody>,
) -> Result<impl IntoResponse, ApiError> {
    Ok((
        no_store(),
        Json(json!({ "html": crate::markdown::render_email_html(&body.source) })),
    ))
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

pub async fn list_sealed(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let sealed = store_call(&state, |store, account_id| {
        store.sealed_messages(account_id)
    })
    .await?;
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
    let sealed = store_call(&state, move |store, account_id| {
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
    Ok((no_store(), Json(payload)))
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
    query(&state, move |store, account_id| {
        store.list_audit(account_id, limit)
    })
    .await
}

// --- GET /client/stats -------------------------------------------------------

pub async fn get_stats(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    // UTC day key for today's Stage-2 usage row.
    let day = Utc::now().format("%Y-%m-%d").to_string();
    // Band counts run under the same default window as the lists they head.
    let bands_since = Utc::now() - chrono::Duration::days(DEFAULT_UPDATES_WINDOW_DAYS);
    let (stats, usage) = store_call(&state, move |store, account_id| {
        let stats = store.stats(account_id, bands_since)?;
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
    let days = q
        .days
        .unwrap_or(DEFAULT_USAGE_DAYS)
        .clamp(1, MAX_USAGE_DAYS);
    // EVERY category in the ledger, not a hand-written list: extractors each
    // write their own, so naming them here means every extractor added later
    // spends invisibly. `extract_banking` did exactly that for ten days.
    let by_category = store_call(&state, move |store, account_id| {
        store.list_usage_by_category(account_id, days)
    })
    .await?;

    let s2_rows: Vec<squelch_core::store::Stage2UsageDay> = by_category
        .iter()
        .find(|(c, _)| c == "stage2")
        .map(|(_, rows)| rows.clone())
        .unwrap_or_default();

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
        let est_cost_usd =
            (in_tok as f64 / 1_000_000.0) * price_in + (out_tok as f64 / 1_000_000.0) * price_out;
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

    // Stage-2 only, for the flat top-level fields older clients read. Every
    // category, Stage-2 included, is rolled up again below for `categories`.
    let (s2_out, s2_totals) = rollup(
        s2_rows,
        state.stage2_price_in_per_mtok,
        state.stage2_price_out_per_mtok,
    );

    // One entry per ledger category. Only Stage-2 runs the capable model and its
    // prices; EVERYTHING ELSE — Stage-1 and every extractor — runs the stage-1
    // small model and shares its config, so it costs at stage-1 rates. See
    // `extract_pass`: "Extractors run on the STAGE-1 (small) model and share its
    // config + cap."
    //
    // The two STAGES are always present, empty or not: they are permanent parts
    // of the pipeline, and a reader who spent nothing on escalations today is
    // owed "Stage 2: nothing" rather than a section that silently vanishes.
    // Extractors are the opposite — they come and go with what the ledger has
    // actually seen, which is what makes a new one report with no edit here.
    let mut listed: Vec<(String, Vec<squelch_core::store::Stage2UsageDay>)> = by_category.clone();
    for stage in ["stage1", "stage2"] {
        if !listed.iter().any(|(c, _)| c == stage) {
            listed.push((stage.to_string(), Vec::new()));
        }
    }
    listed.sort_by(|a, b| a.0.cmp(&b.0));

    let mut categories = serde_json::Map::new();
    for (name, rows) in &listed {
        let is_stage2 = name == "stage2";
        let (price_in, price_out) = if is_stage2 {
            (
                state.stage2_price_in_per_mtok,
                state.stage2_price_out_per_mtok,
            )
        } else {
            (
                state.stage1_price_in_per_mtok,
                state.stage1_price_out_per_mtok,
            )
        };
        let model = if is_stage2 {
            state.stage2_model.as_ref()
        } else {
            state.stage1_model.as_ref()
        };
        let (out_rows, totals) = rollup(rows.clone(), price_in, price_out);
        categories.insert(
            name.clone(),
            json!({ "model": model, "rows": out_rows, "totals": totals }),
        );
    }

    Ok(Json(json!({
        // Top-level fields are Stage-2 (backward-compatible shape).
        "rows": s2_out,
        "totals": s2_totals,
        "provider": state.stage2_provider.as_deref(),
        "model": state.stage2_model.as_ref(),
        "categories": categories,
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
    let now = Utc::now();
    let since = now - chrono::Duration::days(TRIAGE_CONFIG_TRAILING_DAYS);
    let since_day = since.format("%Y-%m-%d").to_string();

    let (overrides, inbound, usage, s1_usage) = store_call(state, move |store, account_id| {
        let overrides = store.stage2_cap_overrides(account_id)?;
        let inbound = store.count_inbound_since(account_id, since)?;
        let usage = store.stage2_usage_since(account_id, &since_day)?;
        let s1_usage = store.stage1_usage_since(account_id, &since_day)?;
        Ok((overrides, inbound, usage, s1_usage))
    })
    .await?;

    // Effective cap = override if present, else the config/env-layer value.
    let thread = overrides
        .thread_daily_cap
        .unwrap_or(state.stage2_thread_daily_cap);
    let sender = overrides
        .sender_daily_cap
        .unwrap_or(state.stage2_sender_daily_cap);
    let global = overrides
        .global_daily_cap
        .unwrap_or(state.stage2_global_daily_cap);
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
    let bad = || ApiError::bad_request(format!("each cap must be an integer in {min}..={max}"));
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
        updates.push((
            squelch_core::config::APP_SETTING_THREAD_DAILY_CAP,
            validate(v)?,
        ));
    }
    if let Some(v) = &body.sender_daily_cap {
        updates.push((
            squelch_core::config::APP_SETTING_SENDER_DAILY_CAP,
            validate(v)?,
        ));
    }
    if let Some(v) = &body.global_daily_cap {
        updates.push((
            squelch_core::config::APP_SETTING_GLOBAL_DAILY_CAP,
            validate(v)?,
        ));
    }
    if let Some(v) = &body.stage1_global_daily_cap {
        updates.push((
            squelch_core::config::APP_SETTING_STAGE1_GLOBAL_DAILY_CAP,
            validate(v)?,
        ));
    }

    if !updates.is_empty() {
        let to_write = updates.clone();
        store_call(&state, move |store, account_id| {
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
const CONFIRM_HINT: &str = "this action requires an explicit \"confirm\": true in the request body";

/// Append an audit row, best-effort: a failed insert is swallowed so it cannot
/// mask the action's own outcome.
pub(crate) async fn audit_action(
    state: &ApiState,
    action: &'static str,
    target: Option<String>,
    outcome: &str,
) {
    let entry = NewAuditEntry {
        actor: ACTION_ACTOR.to_string(),
        action: action.to_string(),
        target,
        detail: Some(outcome.to_string()),
    };
    let _ = store_call(state, move |store, account_id| {
        store.append_audit(account_id, &entry)
    })
    .await;
}

/// RESOLUTION: mark a triage row `done` after a successful action. Best-effort,
/// so bookkeeping cannot mask the action's success; sealed rows are guarded in
/// the store, so this can never touch sealed mail.
async fn resolve_done(state: &ApiState, message_id: i64) {
    let _ = store_call(state, move |store, account_id| {
        store.set_attention_status(account_id, message_id, AttentionStatus::Done)
    })
    .await;
}

/// RESOLUTION, SENDER-WIDE: the same bookkeeping for an action that is a verdict
/// on a SENDER rather than on one message — unsubscribing, and a squelch rule.
///
/// Resolving only the thread the reader happened to have open leaves the rest of
/// that sender's mail in the bands, which is indistinguishable from the action
/// having failed: unsubscribe from a newsletter with nine emails in the window
/// and eight of them stay. Best-effort like `resolve_done`, for the same reason.
async fn resolve_sender_done(state: &ApiState, sender: &str) {
    let sender = sender.to_string();
    let _ = store_call(state, move |store, account_id| {
        store.resolve_sender(account_id, &sender)
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
async fn resolve_target(state: &ApiState, message_id: i64) -> Result<ActionMessageRef, ApiError> {
    store_call(state, move |store, account_id| {
        store.action_message_ref(account_id, message_id)
    })
    .await
}

/// Whether a successful action resolves the target's attention row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnSuccess {
    /// RESOLUTION: mark the target row done — the action handled the message.
    ResolveDone,
    /// Leave the attention row where it is (labeling is not handling).
    LeaveOpen,
}

/// The ladder every message-scoped action walks: confirm gate -> the caller's own
/// rejection -> write client -> non-sealed target -> the Gmail call.
///
/// AUDIT STRINGS ARE A CONTRACT. Every exit appends a row under `action` with the
/// detail clients assert on — "rejected:confirm", the caller's `reject` detail,
/// "rejected:no_write_credential", "failed:target", "failed:gmail", "ok" — in that
/// order, so no action can quietly stop auditing an early return. `reject` is the
/// action's own pre-flight failure as (audit detail, response error), checked
/// immediately after the confirm gate.
async fn guarded_action<F, Fut>(
    state: &ApiState,
    action: &'static str,
    message_id: i64,
    confirm: bool,
    reject: Option<(&'static str, ApiError)>,
    on_success: OnSuccess,
    call: F,
) -> Result<(), ApiError>
where
    F: FnOnce(GmailWriteClient, ActionMessageRef) -> Fut,
    Fut: std::future::Future<Output = Result<(), WriteError>>,
{
    let target = Some(message_id.to_string());

    if !confirm {
        audit_action(state, action, target, "rejected:confirm").await;
        return Err(ApiError::bad_request(CONFIRM_HINT));
    }
    if let Some((detail, err)) = reject {
        audit_action(state, action, target, detail).await;
        return Err(err);
    }

    let client = match write_client(state) {
        Ok(c) => c,
        Err(e) => {
            audit_action(state, action, target, "rejected:no_write_credential").await;
            return Err(e);
        }
    };

    let msg = match resolve_target(state, message_id).await {
        Ok(m) => m,
        Err(e) => {
            audit_action(state, action, target, "failed:target").await;
            return Err(e);
        }
    };

    match call(client, msg).await {
        Ok(()) => {
            if on_success == OnSuccess::ResolveDone {
                resolve_done(state, message_id).await;
            }
            audit_action(state, action, target, "ok").await;
            Ok(())
        }
        Err(e) => {
            audit_action(state, action, target, "failed:gmail").await;
            Err(write_error(&e))
        }
    }
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
    guarded_action(
        &state,
        "archive",
        body.message_id,
        body.confirm,
        None,
        // A successful archive auto-resolves the target row.
        OnSuccess::ResolveDone,
        |client, msg| async move { client.archive(&msg.gmail_msg_id).await },
    )
    .await?;
    Ok(Json(
        json!({ "status": "archived", "message_id": body.message_id }),
    ))
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
    let message_id = body.message_id;
    // Checked after the confirm gate, exactly where the hand-written ladder had it.
    let empty = (body.add.is_empty() && body.remove.is_empty()).then(|| {
        (
            "rejected:empty",
            ApiError::bad_request("label requires a non-empty add or remove list"),
        )
    });

    guarded_action(
        &state,
        "label",
        message_id,
        body.confirm,
        empty,
        // Labeling a message is not handling it; the attention row stands.
        OnSuccess::LeaveOpen,
        |client, msg| async move {
            client
                .modify(&msg.gmail_msg_id, &body.add, &body.remove)
                .await
        },
    )
    .await?;
    Ok(Json(
        json!({ "status": "labeled", "message_id": message_id }),
    ))
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
    /// Reply to EVERYONE on the parent: `to` from its Reply-To/From, `cc` from
    /// the rest of its To/Cc minus this account. Requires
    /// `reply_to_message_id` — there is no audience to widen without a parent —
    /// and absent means today's single-recipient reply, so every existing caller
    /// is unchanged. See [`crate::gmail_write::derive_reply_recipients`].
    #[serde(default)]
    reply_all: bool,
    /// Explicit subject (overrides the reply-derived subject).
    #[serde(default)]
    subject: Option<String>,
    body: String,
    /// `"markdown"` = `body` is markdown source: it goes out verbatim as the
    /// text/plain part with a server-rendered HTML alternative beside it. The
    /// guard scans `body` either way — the HTML is derived from nothing else.
    /// Absent/other = today's single-part plain send, byte-identical.
    #[serde(default)]
    body_format: Option<String>,
    #[serde(default)]
    confirm: bool,
    /// Override the outbound secret guard (still audited).
    #[serde(default)]
    override_guard: bool,
    /// The local draft this send consumes, discarded once the mail is away. Only
    /// a successful send clears it — every rejection leaves the composition
    /// recoverable.
    #[serde(default)]
    draft_id: Option<i64>,
    /// Ask for a read-tracking pixel on this send. Absent is `false`, so every
    /// existing caller stays untracked. Honored ONLY when `[tracking] base_url`
    /// is configured; without it the send goes out untracked rather than failing
    /// — the client learns tracking is unavailable from
    /// `GET /client/tracking-config`, not from a refused send.
    #[serde(default)]
    include_tracker: Option<bool>,
}

/// ECHO the just-sent message into the local store so the thread view shows the
/// reply without waiting for the next Sent poll: fetch it back `format=raw` on
/// the write token, decode, ingest with `is_sent: true`.
///
/// STRICTLY BEST-EFFORT — the send has already succeeded, so every failure path
/// returns `None` after auditing; none of them may fail the request. Logs carry
/// IDS ONLY, never message content. Returns the local message id on success.
///
/// AUDIT (contract, docs/SECURITY.md §5): action `send.echo`, detail
/// `skipped:no_id` | `skipped:sealed` | `failed:fetch` | `failed:ingest` |
/// `ok:<local id>`.
async fn echo_sent(
    state: &ApiState,
    client: &GmailWriteClient,
    target: Option<String>,
    sent: &SentRef,
) -> Option<i64> {
    let gmail_msg_id = match sent.id.as_deref() {
        Some(id) => id.to_string(),
        // Gmail echoed no id: nothing to fetch back.
        None => {
            audit_action(state, "send.echo", target, "skipped:no_id").await;
            return None;
        }
    };

    // TIMEOUT BUDGET: the Swift client's POST timeout is 15s and `action_send` has
    // already spent two serial Gmail calls (parent headers, then the send) before
    // getting here. The echo is bookkeeping, so it may not spend what is left —
    // past 5s it gives up and the reply lands on the next backfill instead.
    let fetched = match tokio::time::timeout(
        Duration::from_secs(ECHO_FETCH_TIMEOUT_SECS),
        client.fetch_raw(&gmail_msg_id),
    )
    .await
    {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => {
            eprintln!("squelch-api: echo of sent message {gmail_msg_id} failed to fetch: {e}");
            audit_action(state, "send.echo", target, "failed:fetch").await;
            return None;
        }
        Err(_elapsed) => {
            eprintln!("squelch-api: echo of sent message {gmail_msg_id} timed out");
            audit_action(state, "send.echo", target, "failed:fetch").await;
            return None;
        }
    };
    let raw = match decode_raw_b64url(&fetched.raw_b64) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("squelch-api: echo of sent message {gmail_msg_id} failed to decode: {e}");
            audit_action(state, "send.echo", target, "failed:fetch").await;
            return None;
        }
    };
    // A `format=raw` read that carried no `raw` field decodes to zero bytes, which
    // would ingest as a message with no headers and no body. Nothing to echo.
    if raw.is_empty() {
        eprintln!("squelch-api: echo of sent message {gmail_msg_id} returned no bytes");
        audit_action(state, "send.echo", target, "failed:fetch").await;
        return None;
    }

    let thread_id = fetched.thread_id.clone().or_else(|| sent.thread_id.clone());
    let internal_date = parse_internal_date(fetched.internal_date.as_deref());
    // Kept back for the log line; the closure takes ownership of the id itself.
    let log_id = gmail_msg_id.clone();
    let ingested = store_call(state, move |store, account_id| {
        let account_addr = store.account_email(account_id)?;
        squelch_core::sync::ingest::ingest_sent(
            store,
            account_id,
            &account_addr,
            &gmail_msg_id,
            thread_id,
            raw,
            internal_date,
            Utc::now(),
        )
    })
    .await;

    match ingested {
        Ok(Some(local_id)) => {
            audit_action(state, "send.echo", target, &format!("ok:{local_id}")).await;
            Some(local_id)
        }
        // The outbound copy tripped seal detection, so core committed nothing: a
        // sealed row in the thread would 404 the whole thread the user is reading.
        // The reply shows up on the next backfill instead.
        Ok(None) => {
            audit_action(state, "send.echo", target, "skipped:sealed").await;
            None
        }
        Err(_) => {
            eprintln!("squelch-api: echo of sent message {log_id} failed to ingest");
            audit_action(state, "send.echo", target, "failed:ingest").await;
            None
        }
    }
}

/// Discard the draft a successful send consumed. Best-effort and IDS ONLY in the
/// log line: the mail has already left, so a failed cleanup must not surface as
/// a send failure. A `false` result is a stale id, not an error.
async fn discard_sent_draft(state: &ApiState, draft_id: i64) {
    if store_call(state, move |store, account_id| {
        store.delete_draft(account_id, draft_id)
    })
    .await
    .is_err()
    {
        eprintln!("squelch-api: draft {draft_id} outlived its send; discard failed");
    }
}

/// Mint and persist this send's read-tracking token, returning
/// `(token, pixel_url)`.
///
/// `None` covers every reason a send goes out untracked: the client did not ask,
/// no `[tracking] base_url` is configured, or the tracker row would not persist.
/// THE ROW COMES FIRST BY DESIGN — a pixel whose token is not in
/// `send_trackers` records nothing, so it must never reach the wire.
///
/// AUDIT: action `send.tracker`, detail `minted` | `failed:entropy` |
/// `failed:store`. The token itself is never audited and never logged.
async fn mint_tracker(
    state: &ApiState,
    requested: bool,
    target: Option<String>,
) -> Option<(String, String)> {
    if !requested {
        return None;
    }
    let base = state.tracking_base_url()?;
    let token = match squelch_core::tracking::mint_token() {
        Ok(t) => t,
        Err(_) => {
            audit_action(state, "send.tracker", target, "failed:entropy").await;
            return None;
        }
    };
    let created_at = Utc::now().timestamp();
    let stored = token.clone();
    if store_call(state, move |store, account_id| {
        store.insert_send_tracker(account_id, &stored, None, created_at)
    })
    .await
    .is_err()
    {
        audit_action(state, "send.tracker", target, "failed:store").await;
        return None;
    }
    let pixel_url = format!("{base}/t/{token}");
    audit_action(state, "send.tracker", target, "minted").await;
    Some((token, pixel_url))
}

/// Drop a tracker whose send never reached the wire.
///
/// The mint has to come first (see [`mint_tracker`]), so a send that fails after
/// it leaves a row nothing will ever link to a message: its opens could never be
/// read back, and it would sit in `send_trackers` for the life of the database.
/// BEST-EFFORT — the token is already dead either way.
///
/// AUDIT: action `send.tracker`, detail `discarded` | `failed:discard`.
async fn discard_tracker(state: &ApiState, token: &str, target: Option<String>) {
    let token = token.to_string();
    let detail = match store_call(state, move |store, account_id| {
        store.delete_send_tracker(account_id, &token)
    })
    .await
    {
        Ok(_) => "discarded",
        Err(_) => "failed:discard",
    };
    audit_action(state, "send.tracker", target, detail).await;
}

/// Point a send's tracker at the echoed local copy, which is what makes
/// `GET /client/messages/{id}/opens` able to find it. BEST-EFFORT: the mail is
/// away and the pixel already works, so a failure only leaves opens recorded
/// against a tracker no message names yet.
async fn link_tracker(state: &ApiState, token: &str, message_id: i64) {
    let token = token.to_string();
    if store_call(state, move |store, account_id| {
        store.set_send_tracker_message(account_id, &token, message_id)
    })
    .await
    .is_err()
    {
        eprintln!("squelch-api: sent message {message_id} was not linked to its tracker");
    }
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
    // Reply-all has nothing to widen without a parent to read the audience off.
    // Checked here, with the other body validations, so it costs no Gmail call.
    if body.reply_all && body.reply_to_message_id.is_none() {
        audit_action(&state, "send", target, "rejected:reply_all_no_parent").await;
        return Err(ApiError::bad_request(
            "reply_all requires `reply_to_message_id`",
        ));
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

    // The parent's headers come from Gmail on the WRITE token (gmail.modify
    // grants read), never the read credential. ONE fetch: it carries both the
    // threading ids and the address lists reply-all derives from.
    let parent_hdrs = match &parent {
        Some(p) => match client.parent_headers(&p.gmail_msg_id).await {
            Ok(h) => Some(h),
            // Threading is best-effort — an unthreaded reply still reaches the
            // right person. REPLY-ALL IS NOT: the audience IS the request, and
            // quietly falling back to reply-to-sender would send to fewer people
            // than the user reviewed. Fail loudly instead.
            Err(_) if body.reply_all => {
                audit_action(&state, "send", target, "failed:gmail").await;
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "could not read the original message's recipients; \
                     reply-all was not sent",
                ));
            }
            Err(_) => None,
        },
        None => None,
    };
    let (in_reply_to, references) = match &parent_hdrs {
        Some(h) => {
            let refs = build_references(h.message_id.as_deref(), h.references.as_deref());
            (h.message_id.clone(), refs)
        }
        None => (None, None),
    };

    // The reply's derived audience, bare addresses only (display names
    // dropped). Derived for EVERY reply with readable headers, not just
    // reply-all: a plain reply must honor the parent's Reply-To, and the
    // preview route runs this same derivation — "what was previewed is what is
    // sent" only holds if the send derives too.
    let derived = match &parent_hdrs {
        Some(h) => {
            // Whose address to leave off the copy list. Unreadable (which the
            // account row makes near-impossible) only costs self-exclusion: the
            // user is copied on their own reply rather than the send failing.
            let own = store_call(&state, |store, account_id| store.account_email(account_id))
                .await
                .unwrap_or_default();
            Some(derive_reply_recipients(h, &own, body.reply_all))
        }
        None => None,
    };

    // PRECEDENCE: an explicit `to` wins, then the derivation, then the stored
    // From. A derived `cc` rides along on EVERY branch — dropping it because
    // the caller retyped the `to` would send to fewer people than they asked
    // for — always minus whoever the chosen `to` already covers.
    let derived_cc = derived.as_ref().map(|d| d.cc.clone()).unwrap_or_default();
    let to = match body.to.clone().filter(|s| !s.trim().is_empty()) {
        Some(t) => t,
        None => match (&derived, &parent) {
            // Derivation that yielded nothing addressable falls through to the
            // stored From rather than composing a recipient-less reply.
            (Some(d), _) if !d.to.trim().is_empty() => d.to.clone(),
            (_, Some(p)) => p.from_addr.clone(),
            (_, None) => {
                audit_action(&state, "send", target, "rejected:no_recipient").await;
                return Err(ApiError::bad_request(
                    "send requires `to` (or `reply_to_message_id` to derive it)",
                ));
            }
        },
    };
    let cc = cc_excluding(&derived_cc, &to);
    // For the audit line, counted before `cc` moves into the MIME parts.
    let copied = cc.split(',').filter(|s| !s.trim().is_empty()).count();

    let subject = body
        .subject
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| match &parent {
            Some(p) => reply_subject(&p.subject),
            None => String::new(),
        });

    // Last thing before composing, so every rejection above costs no token.
    let tracker = mint_tracker(
        &state,
        body.include_tracker.unwrap_or(false),
        target.clone(),
    )
    .await;

    let parts = ReplyParts {
        to,
        cc: Some(cc).filter(|s| !s.trim().is_empty()),
        subject,
        body: body.body.clone(),
        in_reply_to,
        references,
        body_html: match body.body_format.as_deref() {
            Some("markdown") => Some(crate::markdown::render_email_html(&body.body)),
            _ => None,
        },
        pixel_url: tracker.as_ref().map(|(_, url)| url.clone()),
    };
    let raw = match build_reply_rfc822(&parts) {
        Ok(r) => r,
        Err(e) => {
            if let Some((token, _)) = &tracker {
                discard_tracker(&state, token, target.clone()).await;
            }
            audit_action(&state, "send", target, "rejected:compose").await;
            return Err(write_error(&e));
        }
    };

    match client.send(&raw, thread_id.as_deref()).await {
        Ok(sent) => {
            // A cold send has no target row to resolve.
            if let Some(id) = body.reply_to_message_id {
                resolve_done(&state, id).await;
            }
            if let Some(draft_id) = body.draft_id {
                discard_sent_draft(&state, draft_id).await;
            }
            // A reply-all is the one send that reaches N people; the ledger
            // says so, with the recipient count, instead of a plain "ok".
            let outcome = if body.reply_all {
                format!("ok:reply_all:{}", copied + 1)
            } else {
                "ok".to_string()
            };
            audit_action(&state, "send", target.clone(), &outcome).await;
            // The mail is away; everything below is bookkeeping that cannot fail
            // the request.
            let echo_message_id = echo_sent(&state, &client, target, &sent).await;
            if let (Some((token, _)), Some(message_id)) = (&tracker, echo_message_id) {
                link_tracker(&state, token, message_id).await;
            }
            // A cold send falls back to the thread Gmail just created, so the
            // client can open it.
            let thread = thread_id.or(sent.thread_id);
            Ok(Json(json!({
                "status": "sent",
                "echo_message_id": echo_message_id,
                "thread_id": thread,
            })))
        }
        Err(e) => {
            if let Some((token, _)) = &tracker {
                discard_tracker(&state, token, target.clone()).await;
            }
            audit_action(&state, "send", target, "failed:gmail").await;
            Err(write_error(&e))
        }
    }
}

// --- GET /client/messages/{id}/reply_recipients ------------------------------

#[derive(Debug, Deserialize)]
pub struct ReplyRecipientsQuery {
    /// `true` = show what reply-all would address. Absent is a plain reply.
    #[serde(default)]
    all: bool,
}

/// What a reply would be addressed to. `cc` is omitted when there is none, so a
/// plain reply's answer has exactly one field.
#[derive(Debug, Serialize)]
struct ReplyRecipientsView {
    to: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    cc: String,
}

/// PREVIEW, not a send: the audience the send path would compose, so the client
/// can show the user who they are about to write to BEFORE the mail leaves.
/// Derived by the same function the send uses, from the same one metadata fetch,
/// so what is previewed is what is sent.
///
/// Sealed and unknown ids are the same 404 every message-target route returns.
/// A missing write credential is the usual 403 and an upstream failure a 502 —
/// both are answers the client can show, not a 500.
pub async fn reply_recipients(
    State(state): State<ApiState>,
    Path(message_id): Path<i64>,
    Query(q): Query<ReplyRecipientsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let msg = resolve_target(&state, message_id).await?;
    let client = write_client(&state)?;
    let own = store_call(&state, |store, account_id| store.account_email(account_id)).await?;
    let headers = client
        .parent_headers(&msg.gmail_msg_id)
        .await
        .map_err(|e| write_error(&e))?;
    let derived = derive_reply_recipients(&headers, &own, q.all);
    // Headers too broken to yield an address fall back to the stored From, the
    // same recipient a plain reply would have used all along.
    let to = if derived.to.trim().is_empty() {
        msg.from_addr.clone()
    } else {
        derived.to
    };
    Ok(Json(ReplyRecipientsView { to, cc: derived.cc }))
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
    let sender = sender.to_string();
    store_call(state, move |store, account_id| {
        store.upsert_unsubscribe(
            account_id,
            &sender,
            method,
            Some(source_message_id),
            Utc::now(),
        )
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
    let fields = store_call(&state, move |store, account_id| {
        store.message_unsub_fields(account_id, message_id)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;

    let sender = fields.from_addr.trim().to_ascii_lowercase();
    // HEADER FIRST, body second. `List-Unsubscribe` is the sender NAMING its own
    // endpoint; a footer link is us reading the mail and picking one. Plenty of
    // senders ship no header at all, and telling those readers "there is no
    // unsubscribe link" while one sits visible at the bottom of the message is
    // simply wrong.
    let plan = match crate::unsubscribe::classify_unsubscribe(fields.list_unsubscribe.as_deref()) {
        crate::unsubscribe::UnsubPlan::None => {
            crate::unsubscribe::classify_unsubscribe_body(fields.body_html.as_deref())
        }
        header => header,
    };

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
            // RESOLUTION: unsubscribing IS the disposition of this SENDER —
            // asking them to stop and leaving their mail open would demand a
            // second act, once per thread they already sent.
            resolve_sender_done(&state, &sender).await;
            Ok(Json(
                json!({ "method": "browser", "sender": sender, "url": url }),
            ))
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

    let message_id = body.message_id;
    let to = to_value.clone();
    let n = note.clone();
    let recorded = store_call(&state, move |store, account_id| {
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
    query(&state, move |store, account_id| {
        store.list_triage_feedback(account_id, limit)
    })
    .await
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
    store_call(state, |store, account_id| {
        let enabled = store.get_app_setting(account_id, SHRED_ENABLED_KEY)?;
        let days = store.get_app_setting(account_id, SHRED_AFTER_DAYS_KEY)?;
        Ok(parse_shred_policy(enabled, days))
    })
    .await
}

/// Assemble the Auth page's shredder panel state.
async fn shred_stats(state: &ApiState) -> Result<ShredStats, ApiError> {
    let now = Utc::now();
    let recent_since = now - chrono::Duration::days(SHRED_RECENT_DAYS);

    let (enabled, after_days, pending, shredded_recent, shredded_total, last_shredded_at) =
        store_call(state, move |store, account_id| {
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
    let candidates = store_call(state, move |store, account_id| {
        store.shred_candidates(account_id, cutoff, SHRED_BATCH)
    })
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
        let c = candidate.clone();
        let recorded = store_call(state, move |store, account_id| {
            store.record_shred(account_id, &c, Utc::now())
        })
        .await;
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
        let v = days.to_string();
        store_call(&state, move |store, account_id| {
            store.set_app_setting(account_id, SHRED_AFTER_DAYS_KEY, &v)
        })
        .await?;
    }
    if let Some(enabled) = body.enabled {
        let v = if enabled { "1" } else { "0" }.to_string();
        store_call(&state, move |store, account_id| {
            store.set_app_setting(account_id, SHRED_ENABLED_KEY, &v)
        })
        .await?;
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
    // Newest requested_at first.
    query(&state, |store, account_id| {
        store.list_unsubscribes(account_id)
    })
    .await
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

    let s = sender.clone();
    let r = resolution.clone();
    let updated = store_call(&state, move |store, account_id| {
        store.set_unsubscribe_resolution(account_id, &s, &r)
    })
    .await?;
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

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn offset_tokens_are_unpadded_base64url() {
        // Frozen shape: unpadded base64url of `off:<n>`. Changing the codec
        // would invalidate every cursor a client is holding.
        assert_eq!(cursor::encode_offset(50), "b2ZmOjUw");
        assert_eq!(cursor::encode_offset(2), "b2ZmOjI");
        for off in [0u32, 1, 2, 50, 500, 12345, u32::MAX] {
            assert_eq!(
                cursor::decode_offset(&cursor::encode_offset(off)),
                Some(off)
            );
        }
    }

    #[test]
    fn malformed_tokens_decode_to_none() {
        // Bad alphabet, padding (we emit none), a dangling 1-char chunk, and
        // valid base64 that is not an offset token.
        for bad in ["@@notbase64@@", "b2ZmOjIy=", "b2ZmOjIyM", "", "b", "aGk"] {
            assert_eq!(cursor::decode_offset(bad), None, "{bad:?}");
        }
    }
}
