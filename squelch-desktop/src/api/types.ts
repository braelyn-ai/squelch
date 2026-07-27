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

/**
 * Per-property triage justifications. Each value is a short (<= ~200 char)
 * human-readable reason for THAT property's value. An absent key means no reason
 * was recorded for that property; the whole object is `null` for rows that
 * predate the feature. Surfaced as hover tooltips at each value's display site.
 */
export interface FieldReasons {
  importance?: string;
  deadline?: string;
  tier?: string;
}

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
  /** Per-property triage reasons; null on rows predating the feature. */
  /** Per-property triage reasons. The key is ABSENT (undefined) on rows without
   *  recorded reasons — the server omits it rather than emitting null (the
   *  agent-door byte-absence guarantee depends on skip_serializing_if). Always
   *  access via `u.field_reasons?.<key>`. */
  field_reasons?: FieldReasons | null;
  /** True when the message carries stored attachments (paperclip glyph).
   *  ABSENT on rows served by a pre-attachment daemon — treat as false. */
  has_attachments?: boolean;
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
 * core::types::Attachment — one attachment on a HUMAN-door ClientMessage. Never
 * present on the /mcp (agent-door) SanitizedMessage. `size` is bytes.
 * `downloadable` is false when the raw bytes were NOT stored (the attachment was
 * over the ingest cap): the metadata still exists but GET /client/attachments/{id}
 * would 410, so the UI shows the card dimmed with no actions. Inline images
 * referenced by `cid:` in the body ARE stored as attachments (that's how mail
 * templates embed logos), so this list can include the message's own inline art.
 */
export interface Attachment {
  id: number;
  filename: string;
  mime: string;
  size: number; // bytes
  downloadable: boolean;
}

/**
 * core::types::ClientMessage — the HUMAN-door message shape. Adds `html`: a
 * server-side-sanitized (ammonia) HTML string, or null for plain-text-only
 * mail (client then falls back to `content`). Remote content is blocked by the
 * client CSP, never at ingest. `attachments` is ALWAYS present ([] when none).
 */
export interface ClientMessage {
  id: number;
  from_addr: string;
  from_name: string | null;
  received_at: string;
  content: string;
  html: string | null;
  attachments: Attachment[];
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
  /** Opens the email directly in the thread viewer (same click as attention rows). */
  thread_id: string;
  from_addr: string;
  from_name: string | null;
  amount: number | null;
  currency: string | null;
  received_at: string; // RFC3339
}

/** What a calendar notification IS: a fresh invite, a changed event, a
 *  cancellation, or someone's RSVP to an event of yours. */
export type CalendarKind = "invite" | "update" | "cancellation" | "response";

/**
 * GET /client/calendar — a calendar-notification record (Google/Outlook invite
 * mail etc.), auto-resolved out of the attention bands at ingest; the Sitrep
 * right rail is its only surface. `event_title`/`starts_at`/`organizer` are
 * best-effort extractions and may be null.
 */
export interface CalendarUpdate {
  id: number;
  message_id: number;
  kind: CalendarKind;
  event_title: string | null;
  starts_at: string | null; // RFC3339
  organizer: string | null;
  received_at: string; // RFC3339
}

/** What a banking notification IS: a periodic statement (a balance to review)
 *  or a per-transaction alert (a charge/debit notice). */
export type BankingKind = "statement" | "transaction_alert" | "autopay";

/**
 * GET /client/banking — a banking-notification record (statement-ready / balance
 * mail, or a transaction alert). Human door only: sealed mail can NEVER produce a
 * banking row (structural, like receipts). `institution` is a clean display name
 * ("Chase") or null; `amount` is the statement TOTAL balance for a statement, or
 * the transaction amount for an alert (null when nothing parsed → render "—");
 * `account_hint` is a masked tail like "…1234", never a full account number.
 */
export interface BankingRecord {
  id: number;
  message_id: number;
  /** Opens the email directly in the thread viewer (same click as attention rows). */
  thread_id: string;
  /** Sender address — display fallback when no institution was extracted. */
  from_addr?: string;
  kind: BankingKind;
  institution: string | null;
  amount: number | null;
  currency: string | null;
  account_hint: string | null;
  received_at: string; // RFC3339, newest first
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

// --- unsubscribe ------------------------------------------------------------

/** How an unsubscribe request is carried out. `browser` is the only method the
 *  server ever returns or records now (the client opens the link; the server
 *  never delivers anything). Pre-revision dev DBs may hold legacy
 *  one_click/mailto ledger rows, which GET /client/unsubscribes would surface
 *  verbatim — tolerate them when reading. */
export type UnsubscribeMethod = "browser" | "one_click" | "mailto";

/** Terminal state a human put an unsubscribe record into ("Block"/"Keep"). */
export type UnsubResolution = "blocked" | "dismissed";

/**
 * POST /client/unsubscribe result. The server always returns the `browser`
 * shape now (one_click/mailto execution was removed): `sender` is the lowercased
 * address the request targeted, and `url` is the http(s) url the client must
 * open in the system browser for the human to finish the unsubscribe there.
 * A 422 (no http(s) unsubscribe link) surfaces as an ApiError, never this shape;
 * a 404 (unknown/sealed message) likewise.
 */
export interface UnsubscribeResult {
  method: "browser";
  sender: string;
  url: string;
}

/**
 * GET /client/unsubscribes row — one per sender the human asked to unsubscribe
 * from, newest `requested_at` first. `violation_count`/`last_violation_at` count
 * inbound mail that landed >72h AFTER the request while still unresolved (the
 * "they're ignoring you" signal). `resolution` is null until the human blocks or
 * dismisses; a non-null resolution freezes violation accrual server-side.
 */
export interface UnsubscribeRecord {
  sender: string;
  requested_at: string; // RFC3339
  method: UnsubscribeMethod;
  violation_count: number;
  last_violation_at: string | null; // RFC3339
  resolution: UnsubResolution | null;
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
  /**
   * Resolved sender/subject when `target` parses as a message id that exists for
   * the account (`target_sender` = from_name or from_addr, `target_subject` = raw
   * subject). Human-door data: a sealed message's sender/subject IS included here
   * by design (the Auth tab already shows them to the human) — sealed CONTENT is
   * never present. Null when the target isn't a resolvable message id.
   */
  target_sender: string | null;
  target_subject: string | null;
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
 * One usage ledger category (GET /client/usage → `categories` map VALUES). The
 * triage pipeline reports its stages as separate categories, keyed on the wire by
 * id ("stage1" runs on every email, "stage2" on escalations). Each entry carries
 * ONLY its own daily rows, aggregate totals, and the model that produced the
 * spend — the category id is the map KEY (never a field), and there is no
 * server-supplied label or provider here (the client derives the label from the
 * key). `provider` is tolerated as optional for forward-compat but the server
 * does not emit it per-entry.
 */
export interface UsageCategory {
  rows: UsageRow[];
  totals: UsageTotals;
  model: string;
  provider?: string | null;
}

/**
 * GET /client/usage response — triage usage history + totals + the model label
 * that produced the spend. `provider` is null when not explicitly configured
 * server-side; `model` is always present (the configured model id). The top-level
 * `rows`/`totals`/`model`/`provider` describe the primary (Stage-2) category and
 * remain for older servers + the Settings account read; `categories`, when
 * present, is the full per-stage breakdown the Usage view renders generically —
 * an OBJECT MAP keyed by category id ({"stage1": {...}, "stage2": {...}}), not an
 * array.
 */
export interface UsageResponse {
  rows: UsageRow[];
  totals: UsageTotals;
  provider: string | null;
  model: string;
  categories?: Record<string, UsageCategory>;
}

/**
 * Where each cap value came from: the built-in "default", the server's
 * "config" file, or a live "app override" written through POST /client/triage-config.
 */
export type TriageConfigSource = "default" | "config" | "override";

/** Per-cap provenance (GET/POST /client/triage-config → `sources`). */
export interface TriageConfigSources {
  thread_daily_cap: TriageConfigSource;
  sender_daily_cap: TriageConfigSource;
  global_daily_cap: TriageConfigSource;
}

/**
 * Stage-1 triage (the small LLM that runs on EVERY email) config + trailing-14d
 * usage/pricing. `global_daily_cap` is its own daily ceiling (independent of the
 * Stage-2 caps), with the same default/config/override provenance in `source`.
 * `avg_tokens_*` are null until the Stage-1 ledger has at least one call.
 */
export interface TriageStage1 {
  model: string; // e.g. "claude-haiku-4-5-20251001"
  global_daily_cap: number;
  source: TriageConfigSource;
  avg_calls_per_day: number;
  avg_tokens_in_per_call: number | null;
  avg_tokens_out_per_call: number | null;
  price_in_per_mtok: number;
  price_out_per_mtok: number;
}

/**
 * GET/POST /client/triage-config — the Stage-2 triage daily caps plus the
 * trailing-14d usage/pricing figures the settings estimator reads. `avg_tokens_*`
 * are null until the usage ledger has at least one Stage-2 call. `stage1` carries
 * the Stage-1 (every-email small-LLM) model/cap/usage; `stage2_model` is the
 * capable model that Stage-2 escalations run on.
 */
export interface TriageConfig {
  thread_daily_cap: number;
  sender_daily_cap: number;
  global_daily_cap: number;
  sources: TriageConfigSources;
  avg_inbound_per_day: number;
  avg_stage2_calls_per_day: number;
  avg_tokens_in_per_call: number | null;
  avg_tokens_out_per_call: number | null;
  price_in_per_mtok: number;
  price_out_per_mtok: number;
  stage1: TriageStage1;
  stage2_model: string;
}

/**
 * POST /client/triage-config body — any subset of the three Stage-2 caps plus the
 * optional Stage-1 global daily cap (same 1..=100000 validation / 400 semantics).
 */
export interface TriageConfigPatch {
  thread_daily_cap?: number;
  sender_daily_cap?: number;
  global_daily_cap?: number;
  stage1_global_daily_cap?: number;
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
  /** Server-sanitized (ammonia) HTML when the mail had one — render in the
   *  hard-sandboxed EmailFrame, exactly like normal mail. Sensitive like
   *  `body`: React state only, never persist. */
  html: string | null;
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

/**
 * GET /client/triage-debug/{message_id} — the FULL triage state of one message
 * (dev inspector). Read-only; 404 for unknown/sealed.
 */
export interface TriageDebug {
  message_id: number;
  subject: string;
  importance: number;
  tier: string;
  category: string | null;
  one_line: string;
  reason: string;
  field_reasons: FieldReasons | null;
  deadline: string | null;
  matched_rule_id: number | null;
  status: string;
  surfaced_at: string | null;
  resolved_at: string | null;
  stage1_model_used: string | null;
  model_used: string | null;
  needs_stage2: boolean;
  extractor_model_used: string | null;
  created_at: string;
}

// --- auth-mail shredder (retention) -----------------------------------------

/**
 * types::ShredStats (GET/POST /client/shredder). The retention policy for auth
 * mail plus the real counts from the server's `shred_log` ledger.
 *
 * `enabled` and `write_ready` are SEPARATE on purpose: the pass can only run
 * when both hold, and the UI has to be able to say "off" and "on, but no write
 * credential" differently rather than showing a switch that quietly does
 * nothing. Shredding means Gmail's Trash (recoverable for 30 days), never a
 * permanent delete.
 */
export interface ShredStats {
  /** Whether automatic shredding is turned on. Server default is false. */
  enabled: boolean;
  /** Retention window in days; auth mail older than this is eligible. */
  after_days: number;
  /** Messages trashed in the trailing 30 days — the headline figure. */
  shredded_recent: number;
  /** Messages trashed since the ledger began. */
  shredded_total: number;
  /** ISO time of the most recent shred, or null if nothing ever has been. */
  last_shredded_at: string | null;
  /** How many messages are eligible right now under the current policy. */
  pending: number;
  /** False when no write credential is configured (the pass cannot run). */
  write_ready: boolean;
}

/** POST /client/shredder body — any subset of the policy. */
export interface ShredderPatch {
  enabled?: boolean;
  after_days?: number;
}

// --- triage feedback (human corrections) ------------------------------------

/**
 * types::TriageFeedback (GET/POST /client/triage-feedback). One recorded
 * "the pipeline said X, the human said Y" pair.
 *
 * `original` is the JSON snapshot of the whole triage verdict at correction
 * time, including which model produced it — that is what makes the row usable
 * for refining the triage agent rather than just an audit line.
 */
export interface TriageFeedback {
  id: number;
  message_id: number;
  corrected_at: string;
  /** Which axis was overruled: "tier" | "category" | "sensitivity". */
  dimension: string;
  /** What triage had; null when that axis was never set. */
  from_value: string | null;
  /** What the human said it should be. */
  to_value: string;
  original: Record<string, unknown> | null;
  sender: string;
  subject: string;
  note: string | null;
}

// --- marketing (extracted promotions) ---------------------------------------

/**
 * store::MarketingOffer (GET /client/marketing). One promotion pulled out of a
 * marketing-categorized email by the specialist extractor.
 *
 * There is deliberately NO url/link field. Asking a model to emit a URL derived
 * from untrusted email content, which the client then renders as clickable, is a
 * prompt-injection lever — a hostile email could steer it and squelch would be
 * the one presenting the result. Open the email; its real links are extracted
 * from the sanitized html and re-guarded to http(s) (see EmailFrame).
 */
export interface MarketingOffer {
  message_id: number;
  thread_id: string;
  sender: string;
  subject: string;
  /** Clean brand/publication display name, or null. */
  brand: string | null;
  /** One line on what is actually being offered, or null. */
  offer: string | null;
  /** Headline saving as stated ("30% off"), or null. Never computed. */
  discount: string | null;
  /** Promo code, shape-validated server-side. Never a sentence or a URL. */
  code: string | null;
  /** YYYY-MM-DD, only when plausible relative to arrival. */
  expires_at: string | null;
  received_at: string;
}
