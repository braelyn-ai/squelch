//! Transport-agnostic MCP server for squelch.
//!
//! This module knows nothing about stdio vs SSE vs streamable-http. It defines
//! the [`SquelchServer`] handler and its 7 tools. `main.rs` picks the transport
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
use squelch_core::store::{NewAuditEntry, SqliteStore, Store};
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

/// Parameters for `search_mail`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchMailParams {
    /// Free-text query. Matched with hybrid keyword + semantic recall.
    pub query: String,
    /// Max number of summaries to return (1-50). Defaults to 10.
    #[serde(default)]
    pub k: Option<u8>,
}

/// One `search_mail` result: a SUMMARY ONLY (no body). To read the full thread,
/// pass `thread_id` to `get_thread`.
#[derive(Debug, serde::Serialize)]
pub struct SearchMailHit {
    /// Sender address (with display name when known).
    pub sender: String,
    /// A one-line summary — the message subject (never the body).
    pub one_line: String,
    pub received_at: DateTime<Utc>,
    /// The id to pass to `get_thread` to read the full thread.
    pub thread_id: String,
    /// Rank position (1 = most relevant) from the fused hybrid search.
    pub relevance: u32,
}

/// Parameters for `get_deadlines`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDeadlinesParams {
    /// Only return deadlines due within this many days. Omit for all deadlines.
    #[serde(default)]
    pub within_days: Option<u32>,
}

/// Parameters for `get_shipments`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetShipmentsParams {
    /// Include delivered shipments too. Omit/false => en-route packages only.
    #[serde(default)]
    pub include_delivered: Option<bool>,
}

/// One `get_shipments` result: a tracked package. No message body / no sealed
/// content — shipments are extracted from non-sealed shipping mail only.
#[derive(Debug, serde::Serialize)]
pub struct ShipmentHit {
    pub item_name: String,
    pub carrier: String,
    pub status: String,
    pub tracking_number: String,
    pub tracking_url: Option<String>,
    pub last_update: DateTime<Utc>,
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

/// Truncate `s` to at most `max` characters (not bytes — safe on UTF-8),
/// appending a single ellipsis when it was cut. Used to keep audit `detail`
/// bounded and readable for the human review UI.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut it = s.char_indices();
    match it.nth(max) {
        Some((idx, _)) => {
            let mut out = s[..idx].to_string();
            out.push('…');
            out
        }
        None => s.to_string(),
    }
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
        description = "Ranked inbox updates since a timestamp. Each result's \
                       `thread_id` is the id to pass to get_thread to read the \
                       full thread. Auth/verification emails are structurally \
                       absent from results."
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

        // SEEN-LEDGER: the agent door also stamps. Once the serialization set is
        // fixed, mark those rows surfaced (surfaced_at=now if NULL, new->open) so
        // the ledger answers "did ANYONE see this" across both doors. Sealed rows
        // can't be here (SQL + defense-in-depth), and mark_surfaced re-guards
        // sensitivity, so nothing sealed is ever stamped. The RESPONSE SHAPE IS
        // UNCHANGED — the agent doesn't bucket, so we serialize `Update` as before
        // and stamp as a side effect.
        let ids: Vec<i64> = safe.iter().map(|u| u.id).collect();
        self.store
            .mark_surfaced(self.account_id, &ids)
            .map_err(Self::map_err)?;

        Self::ok_json(safe)
    }

    /// Full sanitized thread view. A sealed thread returns the SAME error as a
    /// nonexistent one, so its existence cannot be inferred.
    ///
    /// FORGIVENESS: `id` may be a thread id (the `thread_id` field on every
    /// `get_inbox_updates` / `search_mail` result) OR a single message id. When
    /// the thread lookup misses, `id` is retried as a message id and, on a hit,
    /// that message's thread is returned. The SEALED indistinguishable-404 holds
    /// through BOTH paths: a sealed message id resolves to `None` in SQL, so it
    /// yields the same not-found error as a nonexistent id and never leaks that a
    /// sealed message (or its thread) exists.
    #[tool(
        name = "get_thread",
        description = "Fetch a sanitized thread. `id` is EITHER a thread id (the \
                       `thread_id` field returned by get_inbox_updates and \
                       search_mail) OR a single message id — a message id resolves \
                       to its thread. Unknown or auth-sealed ids return an \
                       identical not-found error."
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

        // PATH 1: treat `id` as a thread id.
        match self.store.thread_view(self.account_id, &params.id) {
            Ok(view) => Self::ok_json(view),
            Err(CoreError::NotFound) => {
                // PATH 2 (forgiveness): retry `id` as a MESSAGE id -> its thread.
                // `thread_id_for_message` excludes sealed rows in SQL, so a sealed
                // (or nonexistent) message id yields None -> the identical 404.
                let message_id: i64 = match params.id.parse() {
                    Ok(n) => n,
                    // Not numeric => can't be a message id; keep the same 404.
                    Err(_) => return Err(ErrorData::resource_not_found("not found", None)),
                };
                let thread_id = self
                    .store
                    .thread_id_for_message(self.account_id, message_id)
                    .map_err(Self::map_err)?;
                let Some(thread_id) = thread_id else {
                    return Err(ErrorData::resource_not_found("not found", None));
                };
                // Re-run the FULL sealed guard on the resolved thread: the
                // message itself was unsealed, but a sibling in its thread may be
                // sealed, which seals the whole thread (indistinguishable 404).
                if self.thread_is_sealed(&thread_id)? {
                    return Err(ErrorData::resource_not_found("not found", None));
                }
                let view: ThreadView = self
                    .store
                    .thread_view(self.account_id, &thread_id)
                    .map_err(Self::map_err)?;
                Self::ok_json(view)
            }
            Err(e) => Err(Self::map_err(e)),
        }
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

    /// Packages currently in transit (and, optionally, delivered ones). Extracted
    /// from non-sealed shipping/delivery mail; sealed content can never appear
    /// here (shipments are never built from sealed mail).
    #[tool(
        name = "get_shipments",
        description = "Tracked packages/shipments. Returns en-route packages by \
                       default (item_name, carrier, status, tracking_number, \
                       tracking_url, last_update); pass include_delivered=true to \
                       also include delivered ones. Extracted from shipping mail; \
                       auth/verification emails are never represented."
    )]
    async fn get_shipments(
        &self,
        Parameters(params): Parameters<GetShipmentsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let include_delivered = params.include_delivered.unwrap_or(false);
        // The shipments table holds no sealed rows by construction (detection
        // never runs on sealed mail), so — unlike thread/update surfaces — there
        // is no sealed row to filter here.
        let shipments = self
            .store
            .list_shipments(self.account_id, include_delivered)
            .map_err(Self::map_err)?;
        let out: Vec<ShipmentHit> = shipments
            .into_iter()
            .map(|s| ShipmentHit {
                item_name: s.item_name,
                carrier: s.carrier,
                status: s.status,
                tracking_number: s.tracking_number,
                tracking_url: s.tracking_url,
                last_update: s.last_update,
            })
            .collect();
        Self::ok_json(out)
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

        // AUDIT (agent door): this is the highest-value entry in the ledger — a
        // prompt-injected agent tampering with rules is the known blast radius of
        // this tool, and it must never write untraced. detail carries the
        // disposition + the `want` text truncated to ~120 chars so the human
        // review UI reads cleanly without unbounded free text.
        let detail = format!(
            "{}: {}",
            disposition.as_str(),
            truncate_chars(&params.want, 120)
        );
        let audit = NewAuditEntry {
            actor: "agent".to_string(),
            action: "rule.set".to_string(),
            target: Some(params.match_pattern.clone()),
            detail: Some(detail),
        };

        // FAIL-CLOSED: the audit row is committed in the SAME transaction as the
        // rule write. If the audit insert fails, the rule write is rolled back and
        // the tool returns an error — stricter than the human door's best-effort
        // action audit, because this is a WRITE by an untrusted-adjacent actor.
        let id = self
            .store
            .set_sender_rule_audited(
                self.account_id,
                &params.match_pattern,
                &params.want,
                disposition,
                &audit,
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

    /// Hybrid keyword + semantic search over the mailbox. Returns SUMMARIES ONLY
    /// (sender, subject one-line, received_at, thread_id, relevance) — never
    /// bodies. `get_thread` remains the escalation to read full content: pass a
    /// result's `thread_id` to it.
    ///
    /// SEALED: auth/verification mail is never embedded and is excluded in SQL by
    /// both the keyword and semantic legs, so it can never appear here. A
    /// defense-in-depth re-check drops any hit whose thread overlaps a sealed
    /// thread before serialization, mirroring `get_inbox_updates`.
    #[tool(
        name = "search_mail",
        description = "Search the mailbox (hybrid keyword + semantic recall). \
                       Returns SUMMARIES ONLY (sender, one-line subject, \
                       received_at, thread_id, relevance) — never message bodies. \
                       To read a result, pass its `thread_id` to get_thread. \
                       Auth/verification emails are structurally absent."
    )]
    async fn search_mail(
        &self,
        Parameters(params): Parameters<SearchMailParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = params.query.trim();
        if query.is_empty() {
            return Err(ErrorData::invalid_params("query must not be empty", None));
        }
        // Default 10, clamp to 1..=50 (u8 default `10` when omitted).
        let k = params.k.unwrap_or(10).clamp(1, 50) as usize;

        // hybrid_search excludes sealed rows in BOTH the keyword and vector legs
        // (and never embedded sealed mail in the first place). Degrades to
        // keyword-only when no embedder is attached.
        let hits = self
            .store
            .hybrid_search(self.account_id, query, k)
            .map_err(Self::map_err)?;

        // Defense in depth: drop any hit whose thread overlaps a sealed thread,
        // exactly like get_inbox_updates. Relevance is the fused rank (1-based)
        // over the SURVIVING set so the client sees a dense 1..N ordering.
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            if self.thread_is_sealed(&hit.thread_id)? {
                continue;
            }
            let sender = match &hit.from_name {
                Some(name) if !name.trim().is_empty() => {
                    format!("{} <{}>", name.trim(), hit.from_addr)
                }
                _ => hit.from_addr.clone(),
            };
            out.push(SearchMailHit {
                sender,
                // one_line is the SUBJECT — a summary, never the body.
                one_line: hit.subject,
                received_at: hit.received_at,
                thread_id: hit.thread_id,
                relevance: (out.len() as u32) + 1,
            });
        }
        Self::ok_json(out)
    }
}

#[tool_handler]
impl ServerHandler for SquelchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "squelch: local-first email intelligence. Read-only over your \
                 mailbox; the only writes are local sender rules. Use search_mail \
                 to find mail (summaries only) and get_thread to read a thread — \
                 pass a result's thread_id (get_thread also accepts a message id). \
                 get_deadlines lists bills due; get_shipments lists packages in \
                 transit. Auth/2FA/verification emails are never exposed through \
                 these tools.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::wrapper::Parameters;
    use squelch_core::store::Store;
    use squelch_core::types::{AttentionStatus, SealedKind, Sensitivity, Tier};

    /// A read through the AGENT DOOR (`get_inbox_updates`) stamps the seen-ledger
    /// exactly like the human door: surfaced_at set, new->open. The response shape
    /// is unchanged (still an `Update` set) — this asserts the side effect.
    #[tokio::test]
    async fn mcp_fetch_stamps_the_ledger() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();

        // One normal message + one sealed OTP.
        let mut normal = squelch_core::types::NewMessage {
            account_id: acct,
            gmail_msg_id: "g1".into(),
            thread_id: "t1".into(),
            from_addr: "alice@example.com".into(),
            from_name: None,
            subject: "hi".into(),
            received_at: Utc::now(),
            snippet: "".into(),
            body: "".into(),
            body_html: None,
            is_sent: false,
        };
        let nid = store.upsert_message(&normal).unwrap();
        store
            .set_triage(nid, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        normal.gmail_msg_id = "g2".into();
        normal.thread_id = "t2".into();
        normal.subject = "code".into();
        let sid = store.upsert_message(&normal).unwrap();
        store
            .set_triage(
                sid,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);
        let _ = server
            .get_inbox_updates(Parameters(GetInboxUpdatesParams {
                since,
                min_importance: None,
            }))
            .await
            .unwrap();

        // The normal row is now surfaced+open; the sealed row is untouched.
        let rows = store
            .attention_updates(acct, since, None, None, None)
            .unwrap();
        assert_eq!(rows.len(), 1, "sealed never surfaces");
        assert_eq!(rows[0].update.id, nid);
        assert_eq!(rows[0].status, AttentionStatus::Open);
        assert!(rows[0].surfaced_at.is_some());

        // Sealed row: still status='new', surfaced_at NULL (never stamped).
        let stats = store.stats(acct).unwrap();
        assert_eq!(stats.sealed, 1);
    }

    /// The AGENT DOOR write (`set_sender_rule`) appends an audit row: actor
    /// "agent", action "rule.set", target = the match_pattern, detail carrying the
    /// disposition + truncated want text. This is the highest-value ledger entry.
    #[tokio::test]
    async fn set_sender_rule_writes_audit_row() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();

        let long_want = "x".repeat(200);
        let res = server
            .set_sender_rule(Parameters(SetSenderRuleParams {
                match_pattern: "*@spam.com".into(),
                want: long_want,
                disposition: "squelch".into(),
            }))
            .await
            .unwrap();
        assert!(!res.is_error.unwrap_or(false));

        // The rule landed...
        let rules = store.list_sender_rules(acct).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_pattern, "*@spam.com");

        // ...and so did exactly one audit row with the expected shape.
        let audit = store.list_audit(acct, 10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].actor, "agent");
        assert_eq!(audit[0].action, "rule.set");
        assert_eq!(audit[0].target.as_deref(), Some("*@spam.com"));
        let detail = audit[0].detail.as_deref().unwrap();
        assert!(detail.starts_with("squelch: "), "detail: {detail}");
        // want was truncated (200 chars -> ~120 + ellipsis), so far under the raw.
        assert!(detail.chars().count() <= 132, "detail too long: {detail}");
        assert!(detail.ends_with('…'), "truncation marker missing: {detail}");
    }

    /// Seed one non-sealed message with a triage row, returning its local id.
    fn seed_msg(
        store: &SqliteStore,
        acct: AccountId,
        gmail: &str,
        thread: &str,
        subject: &str,
        sensitivity: Sensitivity,
        kind: Option<SealedKind>,
    ) -> i64 {
        let msg = squelch_core::types::NewMessage {
            account_id: acct,
            gmail_msg_id: gmail.into(),
            thread_id: thread.into(),
            from_addr: "alice@example.com".into(),
            from_name: Some("Alice".into()),
            subject: subject.into(),
            received_at: Utc::now(),
            snippet: subject.into(),
            body: subject.into(),
            body_html: None,
            is_sent: false,
        };
        let id = store.upsert_message(&msg).unwrap();
        store
            .set_triage(id, acct, 80, Tier::Signal, sensitivity, kind, "", "", None)
            .unwrap();
        id
    }

    /// search_mail returns SUMMARIES ONLY, excludes sealed mail, and its
    /// thread_id round-trips to get_thread.
    #[tokio::test]
    async fn search_mail_returns_summaries_and_excludes_sealed() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        seed_msg(&store, acct, "g1", "t1", "quarterly invoice from acme", Sensitivity::Normal, None);
        // A sealed OTP that also matches the query token — must never surface.
        seed_msg(
            &store,
            acct,
            "g2",
            "t2",
            "your acme verification code",
            Sensitivity::Sealed,
            Some(SealedKind::Otp),
        );

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let res = server
            .search_mail(Parameters(SearchMailParams {
                query: "acme".into(),
                k: None,
            }))
            .await
            .unwrap();

        // Pull the JSON payload back out and assert on it.
        let text = res.content[0].as_text().unwrap().text.as_str();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        let hits = value.as_array().unwrap();
        assert_eq!(hits.len(), 1, "sealed hit must be absent");
        let hit = &hits[0];
        assert_eq!(hit["thread_id"], "t1");
        assert_eq!(hit["relevance"], 1);
        assert!(hit["sender"].as_str().unwrap().contains("alice@example.com"));
        // SUMMARY ONLY: the one_line is the subject; there is no `body` field.
        assert_eq!(hit["one_line"], "quarterly invoice from acme");
        assert!(hit.get("body").is_none(), "search_mail must never emit a body");
    }

    /// get_thread forgiveness: a MESSAGE id resolves to its thread; a sealed
    /// message id returns the SAME not-found as a nonexistent id (no leak).
    #[tokio::test]
    async fn get_thread_resolves_message_id_and_seals_indistinguishably() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let mid = seed_msg(&store, acct, "g1", "t1", "hello there", Sensitivity::Normal, None);
        let sealed_mid =
            seed_msg(&store, acct, "g2", "t2", "code 123", Sensitivity::Sealed, Some(SealedKind::Otp));

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();

        // Thread id works (path 1).
        assert!(server
            .get_thread(Parameters(GetThreadParams { id: "t1".into() }))
            .await
            .is_ok());

        // Message id resolves to its thread (path 2, forgiveness).
        let by_msg = server
            .get_thread(Parameters(GetThreadParams { id: mid.to_string() }))
            .await
            .unwrap();
        let text = by_msg.content[0].as_text().unwrap().text.as_str();
        let view: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(view["thread_id"], "t1");

        // A SEALED message id and a nonexistent id both 404 identically.
        let sealed_err = server
            .get_thread(Parameters(GetThreadParams { id: sealed_mid.to_string() }))
            .await
            .unwrap_err();
        let missing_err = server
            .get_thread(Parameters(GetThreadParams { id: "999999".into() }))
            .await
            .unwrap_err();
        assert_eq!(sealed_err.code, missing_err.code);
        assert_eq!(sealed_err.message, missing_err.message);
        // And the sealed THREAD id itself is also an identical 404.
        let sealed_thread_err = server
            .get_thread(Parameters(GetThreadParams { id: "t2".into() }))
            .await
            .unwrap_err();
        assert_eq!(sealed_thread_err.code, missing_err.code);
    }

    /// get_shipments returns en-route packages by default and includes delivered
    /// ones only when asked. Shipments are structurally sealed-free (never built
    /// from sealed mail), so there is no sealed row to exclude here.
    #[tokio::test]
    async fn get_shipments_en_route_by_default_and_delivered_with_flag() {
        use squelch_core::triage::{ShipmentInfo, ShipmentStatus};
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let mid = seed_msg(&store, acct, "g1", "t1", "shipped", Sensitivity::Normal, None);
        store
            .upsert_shipment(
                acct,
                mid,
                &ShipmentInfo {
                    carrier: "ups".into(),
                    tracking_number: "1Z999AA10123456784".into(),
                    item_name: "Headphones".into(),
                    status: ShipmentStatus::Shipped,
                    tracking_url: Some("https://www.ups.com/track?tracknum=1Z".into()),
                },
                Utc::now(),
            )
            .unwrap();
        store
            .upsert_shipment(
                acct,
                mid,
                &ShipmentInfo {
                    carrier: "usps".into(),
                    tracking_number: "9400111899223817428490".into(),
                    item_name: "Book".into(),
                    status: ShipmentStatus::Delivered,
                    tracking_url: None,
                },
                Utc::now(),
            )
            .unwrap();

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();

        // Default: en-route only.
        let res = server
            .get_shipments(Parameters(GetShipmentsParams { include_delivered: None }))
            .await
            .unwrap();
        let text = res.content[0].as_text().unwrap().text.as_str();
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        let hits = v.as_array().unwrap();
        assert_eq!(hits.len(), 1, "delivered excluded by default");
        assert_eq!(hits[0]["status"], "shipped");
        assert_eq!(hits[0]["tracking_number"], "1Z999AA10123456784");
        // SUMMARY-ONLY shape: no body key.
        assert!(hits[0].get("body").is_none());

        // With the flag: both.
        let res = server
            .get_shipments(Parameters(GetShipmentsParams {
                include_delivered: Some(true),
            }))
            .await
            .unwrap();
        let text = res.content[0].as_text().unwrap().text.as_str();
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    /// FAIL-CLOSED: an invalid disposition never reaches the store, so no rule and
    /// no audit row is written — the tool errors out clean.
    #[tokio::test]
    async fn set_sender_rule_bad_disposition_writes_nothing() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();

        let err = server
            .set_sender_rule(Parameters(SetSenderRuleParams {
                match_pattern: "*@spam.com".into(),
                want: "nope".into(),
                disposition: "bogus".into(),
            }))
            .await;
        assert!(err.is_err());
        assert_eq!(store.list_sender_rules(acct).unwrap().len(), 0);
        assert_eq!(store.list_audit(acct, 10).unwrap().len(), 0);
    }
}
