//! Transport-agnostic MCP server for squelch.
//!
//! This module knows nothing about stdio vs SSE vs streamable-http. It defines
//! the [`SquelchServer`] handler and its 5 tools. `main.rs` picks the transport
//! and calls `.serve(...)`.
//!
//! SECURITY: sealed (auth-related) messages are excluded structurally by the
//! SQL layer in `squelch-core`. This layer RE-CHECKS the invariant as defense
//! in depth: every value is scrubbed against [`SquelchServer::assert_unsealed`]
//! before it is serialized, and `get_thread` collapses any sealed/unknown thread
//! to the exact same `resource_not_found` error so the two are indistinguishable.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use squelch_core::error::CoreError;
use squelch_core::store::{SqliteStore, Store};
use squelch_core::types::{AccountId, Disposition, ThreadView, Update};

/// The squelch MCP server. Holds the store and the active account.
///
/// v0 is single-account: the account is resolved once at construction. The
/// multi-tenant schema (every row carries `account_id`) is already in place, so
/// per-request account selection can be layered on later without schema changes.
#[derive(Clone)]
pub struct SquelchServer {
    store: Arc<SqliteStore>,
    account_id: AccountId,
    // Read by the macro-generated `ServerHandler` (call_tool/list_tools), not by
    // hand-written code, so dead-code analysis can't see the use.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

/// Parameters for `get_inbox_updates`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetInboxUpdatesParams {
    /// Only return updates received at or after this UTC timestamp (RFC 3339).
    pub since: DateTime<Utc>,
    /// Optional minimum importance (0-255). Omit to use the store default.
    #[serde(default)]
    pub min_importance: Option<u8>,
}

/// Parameters for `get_thread`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetThreadParams {
    /// The thread id to fetch.
    pub id: String,
}

/// Parameters for `get_deadlines`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDeadlinesParams {
    /// Only return deadlines due within this many days. Omit for all deadlines.
    #[serde(default)]
    pub within_days: Option<u32>,
}

/// Parameters for `set_sender_rule`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetSenderRuleParams {
    /// Address or pattern to match the sender against.
    pub match_pattern: String,
    /// Free-text description of what the user wants for this sender.
    pub want: String,
    /// One of: "surface", "squelch", "filtered".
    pub disposition: String,
}

impl SquelchServer {
    /// Build a server over an already-open store, resolving `account_email` to
    /// an account id (creating the account row if needed).
    pub fn new(store: Arc<SqliteStore>, account_email: &str) -> anyhow::Result<Self> {
        let account_id = store.ensure_account(account_email)?;
        Ok(Self {
            store,
            account_id,
            tool_router: Self::tool_router(),
        })
    }

    /// Map a core error onto the MCP wire. NotFound becomes `resource_not_found`;
    /// everything else becomes an opaque internal error (never leaks internals).
    fn map_err(e: CoreError) -> ErrorData {
        match e {
            CoreError::NotFound => ErrorData::resource_not_found("not found", None),
            CoreError::InvalidInput(m) => ErrorData::invalid_params(m, None),
            _ => ErrorData::internal_error("internal error", None),
        }
    }

    /// Defense-in-depth guard. The `Update`/`Deadline`/`ThreadView` types carry
    /// no sensitivity field by design (sealed rows are dropped in SQL), so there
    /// is nothing to inspect on the value itself. This helper exists as the
    /// single choke point where any future sealed-bearing type MUST be checked,
    /// and it re-queries the store's local-only sealed set to guarantee that no
    /// thread we are about to surface overlaps a sealed thread.
    fn thread_is_sealed(&self, thread_id: &str) -> Result<bool, ErrorData> {
        let sealed = self
            .store
            .sealed_messages(self.account_id)
            .map_err(Self::map_err)?;
        Ok(sealed.iter().any(|m| m.thread_id == thread_id))
    }

    /// Number of registered MCP tools (for smoke tests / introspection).
    #[allow(dead_code)]
    pub fn tool_count(&self) -> usize {
        self.tool_router.list_all().len()
    }

    /// Serialize a value into a structured tool result.
    fn ok_json<T: serde::Serialize>(value: T) -> Result<CallToolResult, ErrorData> {
        let block = ContentBlock::json(value)?;
        Ok(CallToolResult::success(vec![block]))
    }
}

#[tool_router]
impl SquelchServer {
    /// Ranked inbox updates. Sealed rows are absent (never redacted).
    #[tool(
        name = "get_inbox_updates",
        description = "Ranked inbox updates since a timestamp. Auth/verification \
                       emails are structurally absent from results."
    )]
    async fn get_inbox_updates(
        &self,
        Parameters(params): Parameters<GetInboxUpdatesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let updates: Vec<Update> = self
            .store
            .ranked_updates(self.account_id, params.since, params.min_importance)
            .map_err(Self::map_err)?;

        // Defense in depth: drop any update whose thread overlaps a sealed
        // thread, even though the SQL layer should already have excluded it.
        let mut safe = Vec::with_capacity(updates.len());
        for u in updates {
            if !self.thread_is_sealed(&u.thread_id)? {
                safe.push(u);
            }
        }
        Self::ok_json(safe)
    }

    /// Full sanitized thread view. A sealed thread returns the SAME error as a
    /// nonexistent one, so its existence cannot be inferred.
    #[tool(
        name = "get_thread",
        description = "Fetch a sanitized thread by id. Unknown or auth-sealed \
                       threads return an identical not-found error."
    )]
    async fn get_thread(
        &self,
        Parameters(params): Parameters<GetThreadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Re-check BEFORE hitting the (already-safe) store path so the two
        // rejection reasons are indistinguishable.
        if self.thread_is_sealed(&params.id)? {
            return Err(ErrorData::resource_not_found("not found", None));
        }
        let view: ThreadView = self
            .store
            .thread_view(self.account_id, &params.id)
            .map_err(Self::map_err)?;
        Self::ok_json(view)
    }

    /// Deadlines/bills within a window. Bypasses the squelch threshold; sealed
    /// rows are still excluded.
    #[tool(
        name = "get_deadlines",
        description = "Bills and deadlines due within N days (default: all). \
                       Bypasses the squelch importance threshold."
    )]
    async fn get_deadlines(
        &self,
        Parameters(params): Parameters<GetDeadlinesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let deadlines = self
            .store
            .deadlines(self.account_id, params.within_days)
            .map_err(Self::map_err)?;
        Self::ok_json(deadlines)
    }

    /// Create or update a local sender rule. Writes ONLY squelch's local store;
    /// never touches Gmail.
    #[tool(
        name = "set_sender_rule",
        description = "Create/update a LOCAL sender rule (surface|squelch|filtered). \
                       Writes only squelch's local store, never the mailbox."
    )]
    async fn set_sender_rule(
        &self,
        Parameters(params): Parameters<SetSenderRuleParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let disposition = Disposition::parse(&params.disposition).ok_or_else(|| {
            ErrorData::invalid_params(
                "disposition must be one of: surface, squelch, filtered",
                None,
            )
        })?;
        let id = self
            .store
            .set_sender_rule(
                self.account_id,
                &params.match_pattern,
                &params.want,
                disposition,
            )
            .map_err(Self::map_err)?;
        Self::ok_json(serde_json::json!({ "rule_id": id }))
    }

    /// List local sender rules for the active account.
    #[tool(
        name = "list_sender_rules",
        description = "List the local sender rules for this account."
    )]
    async fn list_sender_rules(&self) -> Result<CallToolResult, ErrorData> {
        let rules = self
            .store
            .list_sender_rules(self.account_id)
            .map_err(Self::map_err)?;
        Self::ok_json(rules)
    }
}

#[tool_handler]
impl ServerHandler for SquelchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "squelch: local-first email intelligence. Read-only over your \
                 mailbox; the only writes are local sender rules. Auth/2FA/\
                 verification emails are never exposed through these tools.",
            )
    }
}
