//! `/client/groups` — send groups: named audiences the user addresses as one,
//! and the history of what has already gone to them.
//!
//! HUMAN DOOR ONLY. There is no `/mcp` counterpart and there will not be one:
//! who the user talks to as a bloc is not something the agent door was handed,
//! and the history read is a SENT-MAIL listing, which the agent door has no
//! surface for at all (see `handlers::get_sent`).
//!
//! Writes here are AUDITED (`group.create` / `group.update` / `group.delete`).
//! A group is the address book behind an irreversible action, so a membership
//! that changed without a trace is a send to someone the user cannot account
//! for.

use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use squelch_core::store::sqlite::groups::{MAX_GROUP_MEMBERS, NewGroupMember};
use squelch_core::types::GroupMode;

use crate::error::ApiError;
use crate::handlers::{audit_action, store_call};
use crate::state::ApiState;

/// Suggestion-menu sized, and capped so no caller can turn autocomplete into a
/// dump of every audience the user keeps.
const DEFAULT_SEARCH_LIMIT: u32 = 8;
const MAX_SEARCH_LIMIT: u32 = 25;

/// History page size. A group's timeline is read a screen at a time, like every
/// other listing on this door.
const DEFAULT_HISTORY_LIMIT: u32 = 50;
const MAX_HISTORY_LIMIT: u32 = 200;

#[derive(Debug, Deserialize)]
pub struct GroupsQuery {
    /// Autocomplete fragment. ABSENT means "list them all" — the Groups page —
    /// while a PRESENT-BUT-EMPTY `q` means the composer asked before anything
    /// was typed, and gets nothing rather than everything.
    q: Option<String>,
    limit: Option<u32>,
}

/// `GET /client/groups` — every group (no `q`), or the ones matching a typed
/// fragment. Membership rides along only on the single-group read; a listing
/// that carried every member of every group would be a contacts dump.
pub async fn list_groups(
    State(state): State<ApiState>,
    Query(params): Query<GroupsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let groups = match params.q {
        Some(q) => {
            let limit = params
                .limit
                .unwrap_or(DEFAULT_SEARCH_LIMIT)
                .min(MAX_SEARCH_LIMIT);
            store_call(&state, move |store, account_id| {
                store.search_send_groups(account_id, &q, limit)
            })
            .await?
        }
        None => {
            store_call(&state, |store, account_id| {
                store.list_send_groups(account_id)
            })
            .await?
        }
    };
    Ok(Json(groups))
}

/// `GET /client/groups/{id}` — one group WITH its membership.
pub async fn get_group(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let group = store_call(&state, move |store, account_id| {
        store.get_send_group(account_id, id)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(group))
}

#[derive(Debug, Deserialize)]
pub struct MemberBody {
    addr: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GroupBody {
    name: String,
    /// How this audience is addressed. Absent defaults to `to` — the VISIBLE
    /// shape, which is the safe default to fall back on: a client that meant
    /// `bcc` and lost the field would otherwise disclose the whole list, and
    /// that is not a mistake a default should be able to make. Parsing is
    /// infallible (see [`GroupMode`]), so an unrecognized value lands here too.
    #[serde(default)]
    mode: Option<GroupMode>,
    #[serde(default)]
    note: Option<String>,
    /// The membership, WHOLESALE. The editor sends the list it is showing; a
    /// client-computed diff would be one dropped request away from silently
    /// keeping someone the user removed.
    #[serde(default)]
    members: Vec<MemberBody>,
}

impl GroupBody {
    fn members(&self) -> Vec<NewGroupMember> {
        self.members
            .iter()
            .map(|m| NewGroupMember {
                addr: m.addr.clone(),
                display_name: m.display_name.clone(),
            })
            .collect()
    }
}

/// The audit detail for a group write. Records the SHAPE — how it is addressed
/// and how many mailboxes it holds — and never the addresses themselves: the
/// audit log is read back on a surface that has no business re-listing an
/// audience, and the membership is one `GET` away for anyone entitled to it.
fn shape(mode: GroupMode, members: usize) -> String {
    format!("{}:{}", mode.as_str(), members)
}

/// `POST /client/groups` — create a group and its membership in one transaction.
pub async fn create_group(
    State(state): State<ApiState>,
    Json(body): Json<GroupBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mode = body.mode.unwrap_or(GroupMode::To);
    let members = body.members();
    let name = body.name.clone();
    let note = body.note.clone().unwrap_or_default();
    let detail = shape(mode, members.len());

    let id = match store_call(&state, move |store, account_id| {
        store.create_send_group(account_id, &name, mode, &note, &members)
    })
    .await
    {
        Ok(id) => id,
        Err(e) => {
            audit_action(&state, "group.create", None, "rejected").await;
            return Err(e);
        }
    };
    audit_action(&state, "group.create", Some(id.to_string()), &detail).await;

    let group = store_call(&state, move |store, account_id| {
        store.get_send_group(account_id, id)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(group))
}

/// `PUT /client/groups/{id}` — rename, re-mode, or re-populate.
pub async fn update_group(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(body): Json<GroupBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mode = body.mode.unwrap_or(GroupMode::To);
    let members = body.members();
    let name = body.name.clone();
    let note = body.note.clone().unwrap_or_default();
    let detail = shape(mode, members.len());
    let target = Some(id.to_string());

    let found = match store_call(&state, move |store, account_id| {
        store.update_send_group(account_id, id, &name, mode, &note, &members)
    })
    .await
    {
        Ok(found) => found,
        Err(e) => {
            audit_action(&state, "group.update", target, "rejected").await;
            return Err(e);
        }
    };
    if !found {
        return Err(ApiError::not_found());
    }
    audit_action(&state, "group.update", target, &detail).await;

    let group = store_call(&state, move |store, account_id| {
        store.get_send_group(account_id, id)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(group))
}

/// `DELETE /client/groups/{id}`.
///
/// The group's `group_sends` rows OUTLIVE it by design (see the schema): what
/// was already sent is a fact, and deleting the audience is a statement about
/// who gets addressed next.
pub async fn delete_group(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let deleted = store_call(&state, move |store, account_id| {
        store.delete_send_group(account_id, id)
    })
    .await?;
    if !deleted {
        return Err(ApiError::not_found());
    }
    audit_action(&state, "group.delete", Some(id.to_string()), "deleted").await;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    limit: Option<u32>,
    /// Plain offset rather than the opaque cursor the mail listings use: this
    /// page merges two sources in Rust, so an offset is the honest unit and
    /// there is no keyset to encode.
    offset: Option<u32>,
}

/// The history envelope. Not [`crate::handlers::Page`]'s `next_cursor` shape:
/// see [`HistoryQuery::offset`].
#[derive(Debug, Serialize)]
struct HistoryPage {
    items: Vec<squelch_core::types::GroupHistoryEntry>,
    /// The offset to ask for next, present only when this page came back full.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<u32>,
}

/// `GET /client/groups/{id}/history` — what has been sent to this group.
///
/// The 404 comes from resolving the group FIRST. Without it an id belonging to
/// another account would fall through to a history query that legitimately
/// matched nothing, and "not yours" would answer 200 with an empty list — an
/// oracle for which group ids exist.
pub async fn group_history(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Query(q): Query<HistoryQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let offset = q.offset.unwrap_or(0);

    store_call(&state, move |store, account_id| {
        store.get_send_group(account_id, id)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;

    let items = store_call(&state, move |store, account_id| {
        store.group_history(account_id, id, limit, offset)
    })
    .await?;
    let next_offset = (items.len() as u32 == limit).then_some(offset + limit);
    Ok(Json(HistoryPage { items, next_offset }))
}

/// `GET /client/groups/limits` — what the client must enforce before it lets a
/// user build an audience it cannot send to.
///
/// Served rather than hardcoded in the client for the reason every other
/// capability probe on this door exists: the daemon and the app ship on
/// separate release trains, and a limit baked into the app is a limit that
/// disagrees with the daemon the moment either moves.
pub async fn group_limits() -> impl IntoResponse {
    Json(serde_json::json!({ "max_members": MAX_GROUP_MEMBERS }))
}

/// Route table, mounted into the bearer-authed `/client` tree.
///
/// `/limits` is declared BEFORE `/{id}`: axum matches static segments ahead of
/// dynamic ones regardless of order, but writing them in this order keeps the
/// two from reading like a conflict.
pub fn routes() -> axum::Router<ApiState> {
    use axum::routing::{delete, get, post, put};
    axum::Router::new()
        .route("/client/groups", get(list_groups))
        .route("/client/groups", post(create_group))
        .route("/client/groups/limits", get(group_limits))
        .route("/client/groups/{id}", get(get_group))
        .route("/client/groups/{id}", put(update_group))
        .route("/client/groups/{id}", delete(delete_group))
        .route("/client/groups/{id}/history", get(group_history))
}
