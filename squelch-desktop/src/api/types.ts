// TypeScript mirrors of the squelch-core / squelch-api JSON contracts served
// under /client/*. Field names and enum string values match the Rust serde
// output EXACTLY (serde rename_all = "snake_case"). Do not "improve" these.

export type Tier = "past_due" | "deadline" | "signal" | "noise";
export type AttentionStatus = "new" | "open" | "done";
export type Disposition = "surface" | "squelch" | "filtered";
/** Server-side sitrep bucket (query param `band` on /client/updates). */
export type Band = "standing" | "new" | "open";
export type SealedKind =
  | "otp"
  | "password_reset"
  | "magic_link"
  | "login_alert"
  | "verification";

/** core::types::Update — the ranked update. */
export interface Update {
  id: number;
  thread_id: string;
  tier: Tier;
  importance: number;
  sender: string;
  one_line: string;
  reason: string;
  deadline: string | null; // RFC3339
  matched_rule: number | null;
}

/**
 * core::types::AttentionUpdate. NOTE: Rust `#[serde(flatten)]`s `Update` into
 * this struct, so on the wire it is ONE flat object (Update fields + the three
 * lifecycle fields), not `{ update: {...} }`. `surfaced_at == null` => NEW.
 */
export interface AttentionUpdate extends Update {
  status: AttentionStatus;
  surfaced_at: string | null;
  resolved_at: string | null;
}

/**
 * core::types::SanitizedMessage — the AGENT-door (/mcp) shape. TEXT ONLY; this
 * never carries html. Retained for parity but the desktop client uses
 * ClientThreadView (below) for the thread drill-in.
 */
export interface SanitizedMessage {
  id: number;
  from_addr: string;
  from_name: string | null;
  received_at: string;
  content: string;
}

/** core::types::ThreadView — the MCP shape (no html). */
export interface ThreadView {
  thread_id: string;
  subject: string;
  messages: SanitizedMessage[];
}

/**
 * core::types::ClientMessage — the HUMAN-door message shape. Adds `html`: a
 * server-side-sanitized (ammonia) HTML string, or null for plain-text-only
 * mail (client then falls back to `content`). Remote content is blocked by the
 * client CSP, never at ingest.
 */
export interface ClientMessage {
  id: number;
  from_addr: string;
  from_name: string | null;
  received_at: string;
  content: string;
  html: string | null;
}

/** core::types::ClientThreadView (GET /client/thread/{id}). */
export interface ClientThreadView {
  thread_id: string;
  subject: string;
  messages: ClientMessage[];
}

/** Carriers a shipment can be tracked through (serde snake_case / lowercase). */
export type Carrier =
  | "ups"
  | "usps"
  | "fedex"
  | "dhl"
  | "amazon"
  | "unknown";

/** Shipment lifecycle status (serde rename_all = "snake_case"). */
export type ShipmentStatus =
  | "ordered"
  | "shipped"
  | "out_for_delivery"
  | "delivered"
  | "exception";

/** core::types::Shipment (GET /client/shipments) — snake_case on the wire. */
export interface Shipment {
  id: number;
  account_id: number;
  tracking_number: string;
  carrier: Carrier;
  item_name: string;
  status: ShipmentStatus;
  tracking_url: string | null;
  first_seen: string; // RFC3339
  last_update: string; // RFC3339
}

/**
 * core::types::Receipt (GET /client/receipts) — snake_case on the wire. A record
 * of money already paid. `amount`/`currency` are best-effort: a receipt with no
 * parseable total still exists (amount === null → render "—").
 */
export interface Receipt {
  id: number;
  account_id: number;
  message_id: number;
  from_addr: string;
  from_name: string | null;
  amount: number | null;
  currency: string | null;
  received_at: string; // RFC3339
}

/** core::types::SenderRule (GET /client/rules) */
export interface SenderRule {
  id: number;
  account_id: number;
  match_pattern: string;
  want_text: string;
  disposition: Disposition;
  updated_at: string;
}

/** Body for POST /client/rules */
export interface CreateRuleBody {
  match_pattern: string;
  want: string;
  disposition: Disposition;
}

/** core::types::SearchHit (GET /client/search) */
export interface SearchHit {
  id: number;
  thread_id: string;
  from_addr: string;
  from_name: string | null;
  subject: string;
  received_at: string;
  snippet: string;
}

/** core::types::AuditEntry (GET /client/audit) */
export interface AuditEntry {
  id: number;
  account_id: number;
  ts: string;
  actor: string;
  action: string;
  target: string | null;
  detail: string | null;
}

/** core::types::BandCounts */
export interface BandCounts {
  standing: number;
  new: number;
  open: number;
}

/**
 * core::types::Stage2Stats — the stage-2 (LLM triage) cost/usage rollup. Optional
 * on the wire: older servers omit it entirely, so every field is treated as
 * best-effort by the read side. `est_cost_usd_today` is the running estimate the
 * Sitrep status strip surfaces ("triage: $0.02 today").
 */
export interface Stage2Stats {
  est_cost_usd_today?: number;
}

/** core::types::StoreStats (GET /client/stats) */
export interface StoreStats {
  tier_counts: Record<string, number>;
  total: number;
  sealed: number;
  last_history_id: number | null;
  bands: BandCounts;
  last_surfaced_at: string | null;
  /** Stage-2 cost/usage rollup; absent on older servers. */
  stage2?: Stage2Stats | null;
}

/**
 * One day's Stage-2 (triage) usage row (GET /client/usage → `rows[]`). Newest-
 * first on the wire; sparse (only days that actually spent tokens appear).
 */
export interface UsageRow {
  day: string; // "YYYY-MM-DD"
  calls: number;
  input_tokens: number;
  output_tokens: number;
}

/** Aggregate totals over the returned usage window (GET /client/usage). */
export interface UsageTotals {
  calls: number;
  input_tokens: number;
  output_tokens: number;
  /** Estimated USD cost, computed server-side from the config per-MTok prices. */
  est_cost_usd: number;
}

/**
 * GET /client/usage response — Stage-2 triage usage history + totals + the model
 * label that produced the spend. `provider` is null when not explicitly
 * configured server-side; `model` is always present (the configured model id).
 */
export interface UsageResponse {
  rows: UsageRow[];
  totals: UsageTotals;
  provider: string | null;
  model: string;
}

/** handlers::SealedMeta (GET /client/sealed) — metadata ONLY, never bodies. */
export interface SealedMeta {
  id: number;
  thread_id: string;
  sender: string;
  subject: string;
  kind: string | null;
  received_at: string;
}

/**
 * handlers::RevealedSealed (POST /client/sealed/{id}/reveal). The `body` field
 * is a sensitive one-time reveal: hold in React state only, never persist.
 */
export interface RevealedSealed {
  id: number;
  thread_id: string;
  sender: string;
  from_name: string | null;
  subject: string;
  kind: string | null;
  received_at: string;
  body: string;
}

/** Generic paginated list envelope (handlers::Page<T>). */
export interface Page<T> {
  items: T[];
  next_cursor?: string;
}

// --- action bodies / results ------------------------------------------------

export interface ArchiveBody {
  message_id: number;
  confirm: boolean;
}

export interface LabelBody {
  message_id: number;
  add?: string[];
  remove?: string[];
  confirm: boolean;
}

export interface SendBody {
  reply_to_message_id?: number;
  to?: string;
  subject?: string;
  body: string;
  confirm: boolean;
  override_guard?: boolean;
}

// --- query params -----------------------------------------------------------

export interface UpdatesParams {
  since?: string;
  min_importance?: number;
  tier?: Tier;
  status?: AttentionStatus;
  band?: Band;
  limit?: number;
  cursor?: string;
}
