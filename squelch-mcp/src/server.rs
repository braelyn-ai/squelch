//! Transport-agnostic MCP server: the [`SquelchServer`] handler and its tools.
//!
//! Sealed (auth-related) mail is excluded structurally in SQL by `squelch-core`;
//! this layer re-checks as defense in depth, and `get_thread` collapses a sealed
//! and an unknown thread into one indistinguishable `resource_not_found`.
//! See docs/SECURITY.md.

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
use squelch_core::config::ShipmentListPolicy;
use squelch_core::error::CoreError;
use squelch_core::store::{NewAuditEntry, SearchSort, SqliteStore, Store};
use squelch_core::types::{AccountId, Disposition, SenderRule, ThreadView, Update};

/// The squelch MCP server. Single-account: the account is resolved once at
/// construction, though every row already carries `account_id`.
#[derive(Clone)]
pub struct SquelchServer {
    store: Arc<SqliteStore>,
    account_id: AccountId,
    /// The operator's `[carriers]` LISTING policy, carried so `get_shipments`
    /// hides exactly what `GET /client/shipments` hides. Defaults to the config
    /// default; wire the real one with
    /// [`SquelchServer::with_shipment_policy`].
    ///
    /// This field exists because the agent door once hardcoded the BUILT-IN
    /// retirement cap and ignored the operator's, so an operator who set
    /// `max_failures = 1` kept seeing retired phantoms through their agent for
    /// four more failures, and one who set `10` had live packages hidden from it
    /// at five. Two doors, one view.
    shipment_policy: ShipmentListPolicy,
    // Read only by the macro-generated `ServerHandler`, so dead-code analysis
    // can't see the use.
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
    /// Result order. "recent" (the default) ranks by relevance with a tilt
    /// toward mail that arrived recently. "best_match" turns that tilt off and
    /// ranks on relevance alone — use it when the words matter more than the
    /// date, such as an old thread you can quote but cannot place.
    #[serde(default)]
    pub sort: Option<String>,
}

/// One `search_mail` result: a SUMMARY ONLY, never a body.
#[derive(Debug, serde::Serialize)]
pub struct SearchMailHit {
    /// Sender address (with display name when known).
    pub sender: String,
    /// A one-line summary — the message subject (never the body).
    pub one_line: String,
    pub received_at: DateTime<Utc>,
    /// The id to pass to `get_thread` to read the full thread.
    pub thread_id: String,
    /// Rank position (1 = best) in the fused hybrid search, under whichever
    /// `sort` ran. NOT "most textually similar" under the default sort, which
    /// blends recency in.
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

/// One `get_shipments` result. Shipments are only ever built from non-sealed mail.
#[derive(Debug, serde::Serialize)]
pub struct ShipmentHit {
    pub item_name: String,
    pub carrier: String,
    pub status: String,
    pub tracking_number: String,
    pub tracking_url: Option<String>,
    pub last_update: DateTime<Utc>,
    /// Carrier-estimated delivery; `None` when the carrier gives none (and
    /// always `None` on a daemon that polls no carrier).
    pub eta: Option<DateTime<Utc>>,
    /// The carrier's own latest status string, verbatim; `None` until the first
    /// poll. Carried because our five-rung ladder loses detail the agent can
    /// usefully relay ("Delivered to neighbor", "Held at customs").
    pub carrier_status_raw: Option<String>,
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

/// The account owner's standing instruction for a sender, delivered ALONGSIDE
/// that sender's mail (issue #21).
///
/// This is a sender rule's `want_text`: the owner's own words, written through
/// the client or through `set_sender_rule`, which is audited fail-closed. It is
/// never text out of an email body. Stage-2 has read it since sender rules
/// shipped — it is how a `filtered` rule decides surface-vs-squelch — but a rule
/// that only steers triage is invisible to the agent doing the talking, so
/// "tell me the total on the statement, not the minimum payment" had no way to
/// reach the sentence the user actually reads.
///
/// DELIBERATELY NOT THE DISPOSITION. surface/squelch/filtered is a VERDICT, and
/// the pipeline has already applied it: by the time the agent sees a row, the
/// verdict is its `tier` and `importance`. Handing over the verdict too invites
/// the agent to apply it a second time, and there is one case where doing so is
/// actively wrong — a bill OUTRANKS a squelch rule on purpose (Rung 1 runs
/// before Rung 2 in `triage::stage1`), so an agent re-reading "squelch" off a
/// past-due notice would bury exactly the mail the ladder went out of its way to
/// raise. What travels is the instruction, not the judgment.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StandingInstruction {
    /// The rule's sender pattern, so the agent can attribute what it is
    /// following ("per your rule for `*@chase.com`") instead of asserting an
    /// unsourced preference at the user.
    pub match_pattern: String,
    /// The owner's instruction, VERBATIM — the same bytes Stage-2 reads. Either
    /// polarity: it may name what they want from this sender ("only the
    /// statement total") or what they do not care about ("skip the promos").
    pub want: String,
}

/// One `get_inbox_updates` result: the ranked update, plus the sender's standing
/// instruction when they have one.
///
/// A WRAPPER RATHER THAN A FIELD ON [`Update`]: `Update` is the human door's row
/// type as well, and the client reads rule text from its own rules list already.
/// The flatten leaves every existing key exactly where it was, so this is an
/// additive change to the agent door's wire shape.
#[derive(Debug, serde::Serialize)]
pub struct InboxUpdate {
    #[serde(flatten)]
    pub update: Update,
    /// Absent (not null) when this sender has no rule, or has one that carries
    /// no instruction text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standing_instruction: Option<StandingInstruction>,
}

/// A `get_thread` result: the sanitized thread, plus the standing instructions
/// covering the people in it.
///
/// A LIST, because a thread can have several correspondents and a rule can match
/// any of them. Deduplicated by pattern, so one rule matching four messages
/// reads once rather than four times, and absent from the JSON entirely when
/// nobody in the thread is ruled.
#[derive(Debug, serde::Serialize)]
pub struct ThreadWithInstructions {
    #[serde(flatten)]
    pub view: ThreadView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub standing_instructions: Vec<StandingInstruction>,
}

impl SquelchServer {
    /// Build a server over an already-open store, resolving `account_email` to
    /// an account id (creating the account row if needed).
    pub fn new(store: Arc<SqliteStore>, account_email: &str) -> anyhow::Result<Self> {
        let account_id = store.ensure_account(account_email)?;
        Ok(Self {
            store,
            account_id,
            shipment_policy: ShipmentListPolicy::default(),
            tool_router: Self::tool_router(),
        })
    }

    /// Carry the operator's `[carriers]` listing policy into `get_shipments`.
    ///
    /// A BUILDER RATHER THAN AN ARGUMENT to [`SquelchServer::new`] deliberately,
    /// matching how `ApiState` takes its optional wiring: the default is a
    /// working server, and the daemon adds the configured value on the way past.
    /// Pass the SAME value you give `ApiState::with_shipment_policy` — the whole
    /// point is that the two doors agree on which packages exist.
    pub fn with_shipment_policy(mut self, policy: ShipmentListPolicy) -> Self {
        self.shipment_policy = policy;
        self
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

    /// Defense-in-depth guard, and the single choke point for it: re-queries the
    /// store's local-only sealed set to guarantee no thread we are about to
    /// surface overlaps a sealed thread.
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

    /// The account's sender rules, read in the order `squelch-core` itself reads
    /// them (`list_sender_rules`, newest-updated first). Same list, same order,
    /// so the rule this door names for an address is the rule Stage-1 would pick
    /// for it — [`squelch_core::triage::rules::match_sender_rule`] takes the
    /// FIRST match, which makes the order part of the answer whenever two
    /// patterns overlap.
    fn sender_rules(&self) -> Result<Vec<SenderRule>, ErrorData> {
        self.store
            .list_sender_rules(self.account_id)
            .map_err(Self::map_err)
    }

    /// The standing instruction for one sender, resolved BY ADDRESS against the
    /// rules as they stand NOW.
    ///
    /// NOT A JOIN ON `triage.matched_rule_id`, which is the obvious
    /// implementation and the wrong one, for three separate reasons:
    ///
    /// 1. THE MOTIVATING EXAMPLE WOULD MISS. `matched_rule_id` records the rule
    ///    that DECIDED the row, and Rung 1 (bill/payment) is evaluated BEFORE
    ///    sender rules and returns `matched_rule: None` — it reads the rule only
    ///    to decide whether to trust the sender's "past due". A credit-card
    ///    statement is a bill, so "tell me the total on cc statements" is
    ///    precisely the mail whose triage row carries no rule id at all.
    /// 2. A RULE WRITTEN AFTER THE MAIL LANDED leaves no mark on rows already
    ///    triaged — the same gap that made
    ///    [`squelch_core::triage::events::current_rule`] necessary for the
    ///    refine sites, and the common shape here: the user tells the agent what
    ///    they want *because* of the mail they were just shown.
    /// 3. The id answers "what decided this row". The question this door is
    ///    asking is "what did the owner ask for about this sender", which is a
    ///    property of the address, not of one verdict.
    ///
    /// IN RUST, NOT IN SQL. `match_sender_rule`'s globs are hand-rolled (`*`
    /// only, case-insensitive, nothing else metacharacter) exactly so a
    /// user-authored pattern cannot inject one. SQLite's `GLOB` is
    /// case-SENSITIVE and gives `?` and `[` meaning; `LIKE` reads `%` and `_`.
    /// Either re-expression would be a second spelling of a predicate that
    /// already exists in Rust, free to drift from the one triage actually ran.
    ///
    /// A BLANK `want_text` IS NO INSTRUCTION. Only a `filtered` rule is required
    /// to carry text (`validate_sender_rule`); a bare surface/squelch rule is a
    /// verdict with nothing to say about how to present anything, and an empty
    /// string on the wire reads as an instruction the agent has to interpret.
    /// Gated on `trim`, sent unmodified: `want_text` is stored verbatim and
    /// every other reader of it gets the owner's bytes.
    fn instruction_for(from_addr: &str, rules: &[SenderRule]) -> Option<StandingInstruction> {
        let rule = squelch_core::triage::rules::match_sender_rule(from_addr, rules)?;
        if rule.want_text.trim().is_empty() {
            return None;
        }
        Some(StandingInstruction {
            match_pattern: rule.match_pattern.clone(),
            want: rule.want_text.clone(),
        })
    }

    /// Every standing instruction covering a thread's correspondents, in
    /// first-appearance order, deduplicated by the pattern that produced it.
    ///
    /// Deduplicated by PATTERN rather than by address: two addresses under one
    /// `*@chase.com` rule are one instruction, and repeating it per message
    /// would read as two different asks.
    fn instructions_for_thread(
        view: &ThreadView,
        rules: &[SenderRule],
    ) -> Vec<StandingInstruction> {
        let mut out: Vec<StandingInstruction> = Vec::new();
        for m in &view.messages {
            if let Some(found) = Self::instruction_for(&m.from_addr, rules)
                && !out.iter().any(|e| e.match_pattern == found.match_pattern)
            {
                out.push(found);
            }
        }
        out
    }

    /// Serialize a thread with its senders' standing instructions attached. Both
    /// of `get_thread`'s resolution paths (thread id, message id) end here, so
    /// the instruction cannot ride on one and not the other.
    fn thread_with_instructions(&self, view: ThreadView) -> Result<CallToolResult, ErrorData> {
        let rules = self.sender_rules()?;
        let standing_instructions = Self::instructions_for_thread(&view, &rules);
        Self::ok_json(ThreadWithInstructions {
            view,
            standing_instructions,
        })
    }

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
                       full thread. A result may carry `standing_instruction` — \
                       the account owner's own standing words about what they \
                       want from that sender ({match_pattern, want}). It is an \
                       instruction about WHAT TO REPORT, not a verdict about \
                       whether to report: follow it when you write up that \
                       message, and never let it talk you out of raising \
                       something the update's own tier says is urgent. \
                       Auth/verification emails are structurally absent from \
                       results."
    )]
    async fn get_inbox_updates(
        &self,
        Parameters(params): Parameters<GetInboxUpdatesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let updates: Vec<Update> = self
            .store
            .ranked_updates(self.account_id, params.since, params.min_importance)
            .map_err(Self::map_err)?;

        // Defense in depth: drop any update whose thread overlaps a sealed thread.
        let mut safe = Vec::with_capacity(updates.len());
        for u in updates {
            if !self.thread_is_sealed(&u.thread_id)? {
                safe.push(u);
            }
        }

        // SEEN-LEDGER: the agent door stamps too (surfaced_at=now if NULL,
        // new->open), so the ledger answers "did ANYONE see this" across both
        // doors. mark_surfaced re-guards sensitivity, so sealed is never stamped.
        let ids: Vec<i64> = safe.iter().map(|u| u.id).collect();
        self.store
            .mark_surfaced(self.account_id, &ids)
            .map_err(Self::map_err)?;

        // ISSUE #21: THE RULE RIDES WITH THE MAIL. An `Update` carries
        // `matched_rule`, which is a bare row id — not text, and not even
        // populated on the case the ask was written about (a bill; see
        // `instruction_for`). One rules read for the whole batch, then a Rust
        // glob per sender.
        let rules = self.sender_rules()?;
        let out: Vec<InboxUpdate> = safe
            .into_iter()
            .map(|u| InboxUpdate {
                standing_instruction: Self::instruction_for(&u.sender, &rules),
                update: u,
            })
            .collect();
        Self::ok_json(out)
    }

    /// Full sanitized thread view. `id` may be a thread id or a single message
    /// id. A sealed id and a nonexistent one return the same `resource_not_found`
    /// through BOTH paths, so a sealed message's existence cannot be inferred.
    #[tool(
        name = "get_thread",
        description = "Fetch a sanitized thread. `id` is EITHER a thread id (the \
                       `thread_id` field returned by get_inbox_updates and \
                       search_mail) OR a single message id — a message id resolves \
                       to its thread. `standing_instructions` carries the account \
                       owner's own standing words about the people in the thread \
                       ({match_pattern, want}); follow them when you report what \
                       the thread says. Unknown or auth-sealed ids return an \
                       identical not-found error."
    )]
    async fn get_thread(
        &self,
        Parameters(params): Parameters<GetThreadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Re-check before the store path so the two rejections are indistinguishable.
        if self.thread_is_sealed(&params.id)? {
            return Err(ErrorData::resource_not_found("not found", None));
        }

        // PATH 1: treat `id` as a thread id.
        match self.store.thread_view(self.account_id, &params.id) {
            Ok(view) => self.thread_with_instructions(view),
            Err(CoreError::NotFound) => {
                // PATH 2: retry `id` as a MESSAGE id. `thread_id_for_message`
                // excludes sealed rows in SQL, so a sealed or nonexistent message
                // id both yield None -> the identical 404.
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
                // Re-guard the resolved thread: an unsealed message may have a
                // sealed sibling, which seals the whole thread.
                if self.thread_is_sealed(&thread_id)? {
                    return Err(ErrorData::resource_not_found("not found", None));
                }
                let view: ThreadView = self
                    .store
                    .thread_view(self.account_id, &thread_id)
                    .map_err(Self::map_err)?;
                self.thread_with_instructions(view)
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

    /// Packages in transit (and, optionally, delivered ones). Never built from
    /// sealed mail, so sealed content can't appear here.
    #[tool(
        name = "get_shipments",
        description = "Tracked packages/shipments. Returns en-route packages by \
                       default (item_name, carrier, status, tracking_number, \
                       tracking_url, last_update, eta, carrier_status_raw); pass \
                       include_delivered=true to also include delivered ones. \
                       eta and carrier_status_raw come from the carrier's own API \
                       and are null until the package has been polled. Extracted \
                       from shipping mail; auth/verification emails are never \
                       represented."
    )]
    async fn get_shipments(
        &self,
        Parameters(params): Parameters<GetShipmentsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let include_delivered = params.include_delivered.unwrap_or(false);
        // No sealed row to filter: detection never runs on sealed mail. The
        // OPERATOR'S listing policy does the rest — phantom digit-runs the
        // carrier keeps rejecting, rows nothing has happened to for
        // `stale_after_days`, and rows the user cleared — and it is the SAME
        // value the human door holds, so an agent and its user see the same
        // packages. Every hide is read-side: the rows keep being polled and come
        // back on their own.
        let shipments = self
            .store
            .list_shipments(self.account_id, include_delivered, self.shipment_policy)
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
                eta: s.eta,
                carrier_status_raw: s.carrier_status_raw,
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
            squelch_core::text::truncate_ellipsis(&params.want, 120)
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
    /// RECENCY IS PART OF THE RANK, and the agent can turn it off. The default
    /// order tilts toward mail that landed recently, so `relevance: 1` means
    /// "best answer", not "most textually similar". An agent hunting an OLD
    /// thread passes `sort: "best_match"` rather than reading down the list.
    ///
    /// SEALED: auth/verification mail is never embedded and is excluded in SQL by
    /// both the keyword and semantic legs, so it can never appear here. A
    /// defense-in-depth re-check drops any hit whose thread overlaps a sealed
    /// thread before serialization, mirroring `get_inbox_updates`.
    #[tool(
        name = "search_mail",
        description = "Search the mailbox (hybrid keyword + semantic recall). \
                       Ranked with a tilt toward recent mail by default; pass \
                       sort=\"best_match\" to rank on relevance alone when \
                       hunting an older thread. Returns SUMMARIES ONLY (sender, \
                       one-line subject, received_at, thread_id, relevance) — \
                       never message bodies. To read a result, pass its \
                       `thread_id` to get_thread. Auth/verification emails are \
                       structurally absent."
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

        // An unreadable sort is the agent's mistake to see, not one to paper
        // over: silently serving `recent` for a `sort` the model invented would
        // teach it that the argument works.
        let sort = match params.sort.as_deref() {
            Some(s) => SearchSort::parse(s).ok_or_else(|| {
                ErrorData::invalid_params("sort must be one of: recent, best_match", None)
            })?,
            None => SearchSort::default(),
        };

        // hybrid_search excludes sealed rows in BOTH the keyword and vector legs
        // (and never embedded sealed mail in the first place). Degrades to
        // keyword-only when no embedder is attached. No operator filter and no
        // window-fullness on this door: the agent asks for top-k, not pages.
        //
        // OFF THE RUNTIME, the way `squelch-api`'s `blocking` runs its store
        // calls: the vector leg runs an ONNX embed of the query, which is tens
        // of milliseconds on a loaded session and ~200 ms when it has to reload
        // one first. Inline, that is a tokio worker parked on CPU work. The
        // per-hit seal check below stays inline: it is a point lookup, and it
        // was never the thing holding a worker.
        let store = self.store.clone();
        let account_id = self.account_id;
        let query = query.to_string();
        let (hits, _window_full) = tokio::task::spawn_blocking(move || {
            // `partial: false` — an agent sends settled words, so nothing is
            // matched as a prefix. The as-you-type widening belongs to a human
            // still typing (the panel's `partial=1`), and applying it here
            // would rank `passwordless` beside a query for `password`.
            //
            // `want_windows: false` — this door builds its result from the
            // SUBJECT and never reads the snippet, and windowing is one extra
            // FTS query per hydrated hit, up to `k`. The legs come back too and
            // are dropped: `hybrid_search_legs` is the shape that lets a caller
            // say what it does not need.
            store
                .hybrid_search_legs(
                    account_id,
                    &query,
                    &Default::default(),
                    sort,
                    false,
                    false,
                    k,
                )
                .map(|(hits, full)| (hits.into_iter().map(|h| h.hit).collect::<Vec<_>>(), full))
        })
        .await
        .map_err(|_| ErrorData::internal_error("internal error", None))?
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "squelch: local-first email intelligence. Read-only over your \
                 mailbox; the only writes are local sender rules. Use search_mail \
                 to find mail (summaries only) and get_thread to read a thread — \
                 pass a result's thread_id (get_thread also accepts a message id). \
                 get_deadlines lists bills due; get_shipments lists packages in \
                 transit. When mail arrives from a sender the account owner has \
                 written a rule for, get_inbox_updates and get_thread deliver \
                 that rule's instruction text with it — obey it when you report \
                 that mail. Auth/2FA/verification emails are never exposed through \
                 these tools.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::wrapper::Parameters;
    use squelch_core::store::{SpamScope, Store};
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
            is_spam: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        };
        let nid = store.upsert_message(&normal).unwrap();
        store
            .set_triage(
                nid,
                acct,
                80,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
            .attention_updates(acct, since, None, None, None, false, SpamScope::Exclude)
            .unwrap();
        assert_eq!(rows.len(), 1, "sealed never surfaces");
        assert_eq!(rows[0].update.id, nid);
        assert_eq!(rows[0].status, AttentionStatus::Open);
        assert!(rows[0].surfaced_at.is_some());

        // Sealed row: still status='new', surfaced_at NULL (never stamped).
        let stats = store
            .stats(acct, chrono::Utc::now() - chrono::Duration::days(30))
            .unwrap();
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
            is_spam: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        };
        let id = store.upsert_message(&msg).unwrap();
        store
            .set_triage(id, acct, 80, Tier::Signal, sensitivity, kind, "", "", None)
            .unwrap();
        id
    }

    /// [`seed_msg`] with an explicit body and arrival time, for the tests that
    /// are about RANKING rather than about what a message contains.
    fn seed_dated(
        store: &SqliteStore,
        acct: AccountId,
        gmail: &str,
        thread: &str,
        subject: &str,
        body: &str,
        received_at: DateTime<Utc>,
    ) -> i64 {
        let msg = squelch_core::types::NewMessage {
            account_id: acct,
            gmail_msg_id: gmail.into(),
            thread_id: thread.into(),
            from_addr: "alice@example.com".into(),
            from_name: Some("Alice".into()),
            subject: subject.into(),
            received_at,
            snippet: subject.into(),
            body: body.into(),
            body_html: None,
            is_sent: false,
            is_spam: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        };
        let id = store.upsert_message(&msg).unwrap();
        store
            .set_triage(
                id,
                acct,
                80,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        id
    }

    /// search_mail returns SUMMARIES ONLY, excludes sealed mail, and its
    /// thread_id round-trips to get_thread.
    #[tokio::test]
    async fn search_mail_returns_summaries_and_excludes_sealed() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        seed_msg(
            &store,
            acct,
            "g1",
            "t1",
            "quarterly invoice from acme",
            Sensitivity::Normal,
            None,
        );
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
                sort: None,
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
        assert!(
            hit["sender"]
                .as_str()
                .unwrap()
                .contains("alice@example.com")
        );
        // SUMMARY ONLY: the one_line is the subject; there is no `body` field.
        assert_eq!(hit["one_line"], "quarterly invoice from acme");
        assert!(
            hit.get("body").is_none(),
            "search_mail must never emit a body"
        );
    }

    /// The agent can turn the recency tilt off, and cannot invent a third order.
    #[tokio::test]
    async fn search_mail_takes_a_sort_and_refuses_an_invented_one() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();

        // A stronger match from long ago against a weaker one from today, so
        // the two orders genuinely disagree about which comes first. Seeded
        // through `upsert_message` rather than by UPDATE: `messages_fts` is a
        // plain fts5 table written by the upsert, so a raw UPDATE would leave
        // the index holding the old text.
        seed_dated(
            &store,
            acct,
            "g-old",
            "t-old",
            "contract",
            "contract",
            Utc::now() - chrono::Duration::days(500),
        );
        seed_dated(
            &store,
            acct,
            "g-new",
            "t-new",
            "weekly digest",
            "A stray mention of a contract sits far down this roundup of newsletter \
             items, among gardening tips, local events, recipes, and a reader letter \
             about compost.",
            Utc::now(),
        );

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let top = |res: CallToolResult| -> String {
            let text = res.content[0].as_text().unwrap().text.clone();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            value.as_array().unwrap()[0]["thread_id"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let recent = server
            .search_mail(Parameters(SearchMailParams {
                query: "contract".into(),
                k: None,
                sort: None,
            }))
            .await
            .unwrap();
        assert_eq!(top(recent), "t-new", "the default tilts toward recent mail");

        let best = server
            .search_mail(Parameters(SearchMailParams {
                query: "contract".into(),
                k: None,
                sort: Some("best_match".into()),
            }))
            .await
            .unwrap();
        assert_eq!(top(best), "t-old", "best_match ranks on relevance alone");

        // An invented value is an error the model can see and correct, never a
        // silent fallback that teaches it the argument works.
        let bad = server
            .search_mail(Parameters(SearchMailParams {
                query: "contract".into(),
                k: None,
                sort: Some("newest".into()),
            }))
            .await;
        assert!(bad.is_err(), "an unknown sort must be invalid_params");
    }

    /// get_thread forgiveness: a MESSAGE id resolves to its thread; a sealed
    /// message id returns the SAME not-found as a nonexistent id (no leak).
    #[tokio::test]
    async fn get_thread_resolves_message_id_and_seals_indistinguishably() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let mid = seed_msg(
            &store,
            acct,
            "g1",
            "t1",
            "hello there",
            Sensitivity::Normal,
            None,
        );
        let sealed_mid = seed_msg(
            &store,
            acct,
            "g2",
            "t2",
            "code 123",
            Sensitivity::Sealed,
            Some(SealedKind::Otp),
        );

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();

        // Thread id works (path 1).
        assert!(
            server
                .get_thread(Parameters(GetThreadParams { id: "t1".into() }))
                .await
                .is_ok()
        );

        // Message id resolves to its thread (path 2, forgiveness).
        let by_msg = server
            .get_thread(Parameters(GetThreadParams {
                id: mid.to_string(),
            }))
            .await
            .unwrap();
        let text = by_msg.content[0].as_text().unwrap().text.as_str();
        let view: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(view["thread_id"], "t1");

        // A SEALED message id and a nonexistent id both 404 identically.
        let sealed_err = server
            .get_thread(Parameters(GetThreadParams {
                id: sealed_mid.to_string(),
            }))
            .await
            .unwrap_err();
        let missing_err = server
            .get_thread(Parameters(GetThreadParams {
                id: "999999".into(),
            }))
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
    /// ones only when asked, and carries the carrier's ETA + verbatim status for
    /// a polled row. Shipments are structurally sealed-free (never built from
    /// sealed mail), so there is no sealed row to exclude here.
    #[tokio::test]
    async fn get_shipments_en_route_by_default_and_delivered_with_flag() {
        use squelch_core::triage::{CarrierTrack, ShipmentInfo, ShipmentStatus};
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let mid = seed_msg(
            &store,
            acct,
            "g1",
            "t1",
            "shipped",
            Sensitivity::Normal,
            None,
        );
        let eta = Utc::now() + chrono::Duration::hours(6);
        let ups = store
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
            .apply_carrier_track(
                acct,
                ups,
                &CarrierTrack {
                    status: None,
                    carrier_status_raw: "Held at customs".into(),
                    eta: Some(eta),
                    delivered_at: None,
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
            .get_shipments(Parameters(GetShipmentsParams {
                include_delivered: None,
            }))
            .await
            .unwrap();
        let text = res.content[0].as_text().unwrap().text.as_str();
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        let hits = v.as_array().unwrap();
        assert_eq!(hits.len(), 1, "delivered excluded by default");
        assert_eq!(hits[0]["status"], "shipped");
        assert_eq!(hits[0]["tracking_number"], "1Z999AA10123456784");
        // The carrier's own words survive a status it does not map onto our
        // ladder ("Held at customs" left the row `shipped`), and the ETA rides
        // out as a timestamp the agent can parse back.
        assert_eq!(hits[0]["carrier_status_raw"], "Held at customs");
        assert_eq!(
            hits[0]["eta"]
                .as_str()
                .unwrap()
                .parse::<DateTime<Utc>>()
                .unwrap(),
            eta
        );
        // SUMMARY-ONLY shape: no body key. The agent door stays minimal — no
        // thread_id, no ids, nothing to pivot into a message with.
        assert!(hits[0].get("body").is_none());
        assert!(hits[0].get("thread_id").is_none());

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

    /// Read the tracking numbers `get_shipments` returned, for the policy test.
    async fn agent_door_numbers(server: &SquelchServer) -> Vec<String> {
        let res = server
            .get_shipments(Parameters(GetShipmentsParams {
                include_delivered: Some(true),
            }))
            .await
            .unwrap();
        let text = res.content[0].as_text().unwrap().text.as_str();
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .map(|h| h["tracking_number"].as_str().unwrap().to_string())
            .collect()
    }

    /// DEFECT (P1): the agent door hardcoded the BUILT-IN retirement cap and
    /// ignored the operator's `[carriers] max_failures`, so with `max_failures=1`
    /// an agent kept reporting phantoms four failures after the human door had
    /// retired them, and with `10` it lost live packages the human door still
    /// showed. Now it carries the policy, and both doors are asserted to agree
    /// given the same one.
    #[tokio::test]
    async fn get_shipments_honors_a_non_default_policy_and_matches_the_human_door() {
        use squelch_core::triage::{ShipmentInfo, ShipmentStatus};
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let mid = seed_msg(
            &store,
            acct,
            "g1",
            "t1",
            "shipped",
            Sensitivity::Normal,
            None,
        );

        // An AMBIGUOUS bare digit-run with ONE permanent rejection against it.
        let ambiguous = store
            .upsert_shipment(
                acct,
                mid,
                &ShipmentInfo {
                    carrier: "fedex".into(),
                    tracking_number: "123456789012".into(),
                    item_name: "Maybe a package".into(),
                    status: ShipmentStatus::Shipped,
                    tracking_url: None,
                },
                Utc::now(),
            )
            .unwrap();
        store
            .record_poll_outcome(acct, ambiguous, Utc::now(), true)
            .unwrap();

        // A TIGHT policy (retire at 1) must hide it on the agent door too. The
        // default cap of 5 is what the old code used, and under it this row is
        // still visible — so a stale hardcode fails this assertion.
        let tight = ShipmentListPolicy {
            suppress_failed_ambiguous_at: 1,
            stale_after_days: 0,
        };
        let server = SquelchServer::new(store.clone(), "me@localhost")
            .unwrap()
            .with_shipment_policy(tight);
        assert!(
            agent_door_numbers(&server).await.is_empty(),
            "the operator's max_failures=1 must retire the phantom on the agent door"
        );

        // TWO DOORS, ONE VIEW: same policy, same rows, whichever door asks.
        let human = store.list_shipments(acct, true, tight).unwrap();
        assert!(
            human.is_empty(),
            "the human door hides it under the same policy"
        );

        // And a LOOSE policy keeps it, on both doors.
        let loose = ShipmentListPolicy {
            suppress_failed_ambiguous_at: 10,
            stale_after_days: 0,
        };
        let server = SquelchServer::new(store.clone(), "me@localhost")
            .unwrap()
            .with_shipment_policy(loose);
        assert_eq!(agent_door_numbers(&server).await, vec!["123456789012"]);
        assert_eq!(
            store
                .list_shipments(acct, true, loose)
                .unwrap()
                .into_iter()
                .map(|s| s.tracking_number)
                .collect::<Vec<_>>(),
            vec!["123456789012"],
            "and the human door agrees under that one too"
        );
    }

    /// The staleness half of the same contract: an agent must not report a
    /// package nothing has happened to for longer than the operator's window.
    #[tokio::test]
    async fn get_shipments_hides_a_stale_package_from_the_agent_too() {
        use squelch_core::triage::{ShipmentInfo, ShipmentStatus};
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        let mid = seed_msg(
            &store,
            acct,
            "g1",
            "t1",
            "shipped",
            Sensitivity::Normal,
            None,
        );
        let ship = |number: &str| ShipmentInfo {
            carrier: "ups".into(),
            tracking_number: number.into(),
            item_name: "Headphones".into(),
            status: ShipmentStatus::Shipped,
            tracking_url: None,
        };
        store
            .upsert_shipment(
                acct,
                mid,
                &ship("1Z999AA10123456784"),
                Utc::now() - chrono::Duration::days(8),
            )
            .unwrap();
        store
            .upsert_shipment(
                acct,
                mid,
                &ship("1Z999AA10123456785"),
                Utc::now() - chrono::Duration::days(6),
            )
            .unwrap();

        let policy = ShipmentListPolicy {
            suppress_failed_ambiguous_at: u32::MAX,
            stale_after_days: 7,
        };
        let server = SquelchServer::new(store.clone(), "me@localhost")
            .unwrap()
            .with_shipment_policy(policy);
        assert_eq!(
            agent_door_numbers(&server).await,
            vec!["1Z999AA10123456785"],
            "8 days silent is hidden, 6 days silent is not"
        );
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

    // ---- issue #21: the rule rides with the mail --------------------------

    /// [`seed_msg`] with a chosen SENDER and tier, for the standing-instruction
    /// tests — which are entirely about which address a rule's pattern matches,
    /// and so cannot use the shared `alice@example.com`.
    ///
    /// Triage is written the way `set_triage` writes it: `matched_rule_id` NULL.
    /// That is not a shortcut, it is the case under test — see
    /// `a_bill_carries_its_rule_though_no_rule_decided_it`.
    fn seed_from(
        store: &SqliteStore,
        acct: AccountId,
        gmail: &str,
        thread: &str,
        subject: &str,
        from_addr: &str,
        tier: Tier,
    ) -> i64 {
        let msg = squelch_core::types::NewMessage {
            account_id: acct,
            gmail_msg_id: gmail.into(),
            thread_id: thread.into(),
            from_addr: from_addr.into(),
            from_name: None,
            subject: subject.into(),
            received_at: Utc::now(),
            snippet: subject.into(),
            body: subject.into(),
            body_html: None,
            is_sent: false,
            is_spam: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        };
        let id = store.upsert_message(&msg).unwrap();
        store
            .set_triage(
                id,
                acct,
                80,
                tier,
                Sensitivity::Normal,
                None,
                subject,
                "",
                None,
            )
            .unwrap();
        id
    }

    /// Decode a `get_inbox_updates` payload into its JSON array.
    fn updates_json(res: &CallToolResult) -> Vec<serde_json::Value> {
        let text = res.content[0].as_text().unwrap().text.as_str();
        serde_json::from_str::<serde_json::Value>(text)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    async fn fetch_updates(server: &SquelchServer) -> Vec<serde_json::Value> {
        let res = server
            .get_inbox_updates(Parameters(GetInboxUpdatesParams {
                since: Utc::now() - chrono::Duration::days(1),
                min_importance: None,
            }))
            .await
            .unwrap();
        updates_json(&res)
    }

    /// ISSUE #21, the whole of it: a sender the owner has written a rule for
    /// delivers that rule's WORDS beside their mail, so the agent reporting the
    /// message knows what to say about it. The existing keys are untouched (the
    /// wrapper flattens), and the DISPOSITION is deliberately not on the wire.
    #[tokio::test]
    async fn standing_instruction_rides_with_the_mail() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        store
            .set_sender_rule(
                acct,
                "*@chase.com",
                "the statement total, not the minimum payment",
                Disposition::Filtered,
            )
            .unwrap();
        seed_from(
            &store,
            acct,
            "g1",
            "t1",
            "Your statement is ready",
            "statements@chase.com",
            Tier::Deadline,
        );

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let rows = fetch_updates(&server).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        let instruction = &row["standing_instruction"];
        assert_eq!(
            instruction["want"],
            "the statement total, not the minimum payment"
        );
        assert_eq!(instruction["match_pattern"], "*@chase.com");
        // The VERDICT stays off the agent door: it is already spent as `tier`.
        assert!(
            instruction.get("disposition").is_none(),
            "a verdict must not travel with the instruction: {instruction}"
        );

        // Additive: every key an agent already reads is still where it was.
        assert_eq!(row["thread_id"], "t1");
        assert_eq!(row["tier"], "deadline");
        assert_eq!(row["one_line"], "Your statement is ready");
    }

    /// THE REASON THIS RESOLVES BY ADDRESS AND NOT BY `matched_rule_id`.
    ///
    /// Rung 1 of Stage-1 (bill/payment) runs BEFORE sender rules and returns
    /// `matched_rule: None` — it reads a rule only to decide whether to trust
    /// the sender's "past due" (`triage::mod`'s own tests pin this: "bill rung
    /// wins over the squelch rule"). A credit-card statement is a bill, so the
    /// issue's motivating example is precisely the mail whose triage row carries
    /// NO rule id. A `LEFT JOIN sender_rules ON sr.id = t.matched_rule_id` would
    /// deliver nothing here.
    ///
    /// The same assertion covers the other half: a rule written AFTER the mail
    /// was triaged — the common shape, since the user says what they want
    /// because of a message they were just shown — never marks the existing row
    /// either.
    #[tokio::test]
    async fn a_bill_carries_its_rule_though_no_rule_decided_it() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        // Mail first, rule second, and the triage row keeps matched_rule_id NULL
        // throughout — exactly what Rung 1 leaves behind.
        seed_from(
            &store,
            acct,
            "g1",
            "t1",
            "Your statement is ready",
            "statements@chase.com",
            Tier::Deadline,
        );
        store
            .set_sender_rule(
                acct,
                "*@chase.com",
                "the statement total, not the minimum payment",
                Disposition::Filtered,
            )
            .unwrap();

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let rows = fetch_updates(&server).await;
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0]["matched_rule"].is_null(),
            "the fixture is only meaningful while no rule decided the row: {}",
            rows[0]
        );
        assert_eq!(
            rows[0]["standing_instruction"]["want"], "the statement total, not the minimum payment",
            "an id join would have delivered nothing here"
        );
    }

    /// A BARE VERDICT SAYS NOTHING. Only a `filtered` rule is required to carry
    /// want text; a surface/squelch rule with none is a decision about where the
    /// mail goes, not an instruction about how to describe it, and an empty
    /// string on the wire is an instruction the agent has to interpret. The key
    /// is absent, not null, not "".
    #[tokio::test]
    async fn a_rule_with_no_words_delivers_no_instruction() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        store
            .set_sender_rule(acct, "*@chase.com", "   ", Disposition::Squelch)
            .unwrap();
        seed_from(
            &store,
            acct,
            "g1",
            "t1",
            "statement",
            "statements@chase.com",
            Tier::Noise,
        );
        // ...and an entirely unruled sender, which must read the same way.
        seed_from(
            &store,
            acct,
            "g2",
            "t2",
            "lunch?",
            "bob@example.com",
            Tier::Signal,
        );

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        for row in fetch_updates(&server).await {
            assert!(
                row.get("standing_instruction").is_none(),
                "absent, never null or empty: {row}"
            );
        }
    }

    /// get_thread carries the instructions for the people in the thread, ONCE
    /// each: one rule matching three messages is one ask, not three. Both of
    /// get_thread's resolution paths are covered — the thread id here, the
    /// message id below — because the instruction must not ride on one and not
    /// the other.
    #[tokio::test]
    async fn get_thread_carries_each_instruction_once() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        store
            .set_sender_rule(
                acct,
                "*@chase.com",
                "the statement total, not the minimum payment",
                Disposition::Filtered,
            )
            .unwrap();
        // Two DIFFERENT addresses under the one pattern, plus an unruled third
        // party on the same thread.
        let seed =
            |g, subject, from| seed_from(&store, acct, g, "t1", subject, from, Tier::Deadline);
        seed("g1", "statement", "statements@chase.com");
        seed("g2", "re: statement", "alerts@chase.com");
        let mid = seed("g3", "re: statement", "bob@example.com");

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let read = |id: String| {
            let server = server.clone();
            async move {
                let res = server
                    .get_thread(Parameters(GetThreadParams { id }))
                    .await
                    .unwrap();
                let text = res.content[0].as_text().unwrap().text.clone();
                serde_json::from_str::<serde_json::Value>(&text).unwrap()
            }
        };

        let by_thread = read("t1".to_string()).await;
        let list = by_thread["standing_instructions"].as_array().unwrap();
        assert_eq!(list.len(), 1, "one rule, one ask: {by_thread}");
        assert_eq!(
            list[0]["want"],
            "the statement total, not the minimum payment"
        );
        assert_eq!(list[0]["match_pattern"], "*@chase.com");
        // Flatten kept the thread itself intact.
        assert_eq!(by_thread["thread_id"], "t1");
        assert_eq!(by_thread["messages"].as_array().unwrap().len(), 3);

        // PATH 2: the same thread reached by a MESSAGE id — and by the message
        // of the UNRULED sender, so a per-message shortcut would answer empty.
        let by_message = read(mid.to_string()).await;
        assert_eq!(
            by_message["standing_instructions"], by_thread["standing_instructions"],
            "both get_thread paths deliver the same instructions"
        );
    }

    /// A thread nobody is ruled for carries no key at all, rather than an empty
    /// list the agent has to reason about.
    #[tokio::test]
    async fn an_unruled_thread_carries_no_instruction_key() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        seed_from(
            &store,
            acct,
            "g1",
            "t1",
            "lunch?",
            "bob@example.com",
            Tier::Signal,
        );

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let res = server
            .get_thread(Parameters(GetThreadParams { id: "t1".into() }))
            .await
            .unwrap();
        let text = res.content[0].as_text().unwrap().text.clone();
        let view: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            view.get("standing_instructions").is_none(),
            "absent, never []: {view}"
        );
        assert_eq!(view["thread_id"], "t1");
    }

    /// The pattern is matched with the SAME glob triage runs — case-insensitive,
    /// `*` the only metacharacter — not with a SQL `GLOB`/`LIKE` re-spelling.
    /// `?` is a literal here and case does not matter; under SQLite's `GLOB` the
    /// first is a wildcard and the second decides the answer.
    #[tokio::test]
    async fn the_glob_is_the_rust_one() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@localhost").unwrap();
        store
            .set_sender_rule(acct, "*@CHASE.com", "totals only", Disposition::Filtered)
            .unwrap();
        store
            .set_sender_rule(acct, "a?b@example.com", "never", Disposition::Filtered)
            .unwrap();
        seed_from(
            &store,
            acct,
            "g1",
            "t1",
            "statement",
            "Statements@chase.com",
            Tier::Deadline,
        );
        seed_from(
            &store,
            acct,
            "g2",
            "t2",
            "hi",
            "axb@example.com",
            Tier::Signal,
        );

        let server = SquelchServer::new(store.clone(), "me@localhost").unwrap();
        let rows = fetch_updates(&server).await;
        let by_thread = |t: &str| rows.iter().find(|r| r["thread_id"] == t).unwrap().clone();
        assert_eq!(
            by_thread("t1")["standing_instruction"]["want"],
            "totals only",
            "the glob folds case on both sides"
        );
        assert!(
            by_thread("t2").get("standing_instruction").is_none(),
            "`?` is a literal, not a single-character wildcard"
        );
    }
}
