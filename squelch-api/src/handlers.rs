//! `/client/*` handlers for the human door.
//!
//! Handlers are thin: parse/validate query + path params, call one or two
//! `Store` methods (via `spawn_blocking`, since the store is sync), and serialize
//! core types straight to JSON. Sealed handling lives in the store; the reveal
//! handler is the one place that intentionally surfaces a sealed body, and it
//! audits every call first.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use squelch_core::store::{ActionMessageRef, NewAuditEntry, Store};
use squelch_core::types::{Disposition, Tier};

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

/// The pagination cursor is an opaque token that round-trips a row offset. It is
/// intentionally minimal (`off:<n>`, then base64url) so a client treats it as
/// opaque but we can decode it. Not security-sensitive; just a scroll position.
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

/// A tiny dependency-free base64url codec, kept local so squelch-api needs no
/// extra crate just to opaque-ify an integer offset.
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

/// Resolve `(limit, offset)` from optional `limit`/`cursor` query params.
/// A present `cursor` wins over any absent offset; `limit` is clamped to
/// `[1, MAX_LIMIT]`. Returns 400 on a malformed cursor.
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

/// Run a synchronous store closure off the async runtime. Panics inside the
/// closure surface as a 500 (opaque).
async fn blocking<T, F>(f: F) -> Result<T, ApiError>
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
    let since = q
        .since
        .unwrap_or_else(|| Utc::now() - chrono::Duration::days(DEFAULT_UPDATES_WINDOW_DAYS));
    let min_importance = q.min_importance;

    let store = state.store.clone();
    let account_id = state.account_id;
    let items = blocking(move || {
        // ranked_updates already excludes sealed rows in SQL. Tier filtering and
        // pagination are applied here over the ranked slice.
        let mut all = store.ranked_updates(account_id, since, min_importance)?;
        if let Some(t) = tier_filter {
            all.retain(|u| u.tier == t);
        }
        let page = all
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect::<Vec<_>>();
        Ok(page)
    })
    .await?;

    let next = next_cursor(items.len(), limit, offset);
    Ok(Json(Page {
        items,
        next_cursor: next,
    }))
}

// --- GET /client/thread/{thread_id} -----------------------------------------

pub async fn get_thread(
    State(state): State<ApiState>,
    Path(thread_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    // thread_view returns NotFound for sealed OR nonexistent threads, keeping
    // the two indistinguishable exactly as on the MCP surface.
    let view = blocking(move || store.thread_view(account_id, &thread_id)).await?;
    Ok(Json(view))
}

// --- GET /client/search -----------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    q: String,
    limit: Option<u32>,
    cursor: Option<String>,
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

    let store = state.store.clone();
    let account_id = state.account_id;
    // Store::search excludes sealed rows in SQL.
    let items = blocking(move || store.search(account_id, &term, limit, offset)).await?;

    let next = next_cursor(items.len(), limit, offset);
    Ok(Json(Page {
        items,
        next_cursor: next,
    }))
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
    let id = blocking(move || {
        store.set_sender_rule(account_id, &body.match_pattern, &body.want, disposition)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(json!({ "rule_id": id }))))
}

pub async fn delete_rule(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let deleted = blocking(move || store.delete_sender_rule(account_id, id)).await?;
    if deleted {
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
}

pub async fn reveal_sealed(
    State(state): State<ApiState>,
    Path(message_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;

    let sealed = blocking(move || {
        // Audit BEFORE returning the body. The audit row records only the
        // message id (no body/content) so the log itself never leaks secrets.
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
    let stats = blocking(move || store.stats(account_id)).await?;
    Ok(Json(stats))
}

// --- ACTIONS: the ONLY write capability in the system -----------------------
//
// GATES (all non-negotiable, enforced here):
//   (a) every action body MUST carry `"confirm": true`, else 400.
//   (b) `send` runs the outbound secret guard; matches => 422 unless
//       `"override_guard": true`.
//   (c) EVERY action — attempted AND completed, success or failure — appends an
//       audit row (actor="client-api").
//
// If no write credential is configured the action returns 403 with a hint to
// run `squelchd auth --write`. Sync/triage/MCP never load the write token.

/// Actor written into the audit log for all write actions.
const ACTION_ACTOR: &str = "client-api";

/// The `confirm` contract message returned on a missing/false confirm.
const CONFIRM_HINT: &str =
    "this action requires an explicit \"confirm\": true in the request body";

/// Append an audit row for an action, best-effort. Audit failures must not mask
/// the action's own outcome, so a failed insert is swallowed (it cannot leak
/// anything and the action result is what the caller cares about).
async fn audit_action(
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

/// Map a [`WriteError`] to an [`ApiError`]. A missing credential is 403 (with a
/// hint); everything else is an opaque-ish 502/400 that never echoes secrets.
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

    // (b) OUTBOUND GUARD: scan the body for secret-looking patterns. Report only
    // REDACTED kinds, never the matched text. Overridable with override_guard.
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

    // Resolve the reply parent (if any) for recipient/subject/threading.
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

    // Recipient: explicit `to`, else the parent sender. Required.
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

    // Threading headers: fetched from Gmail using the WRITE token (gmail.modify
    // grants read), never the read credential.
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
            audit_action(&state, "send", target, "ok").await;
            Ok(Json(json!({ "status": "sent" })))
        }
        Err(e) => {
            audit_action(&state, "send", target, "failed:gmail").await;
            Err(write_error(&e))
        }
    }
}
