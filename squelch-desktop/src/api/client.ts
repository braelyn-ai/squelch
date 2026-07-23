// Typed client for the squelch human door (/client/*). One function per route.
// Bearer token + base URL come from the settings held in the store; call
// `configureClient()` once after Connect / on load, before any request.
//
// SECURITY: the token is sent only in the Authorization header. It is never
// logged, never placed in a thrown error message, never persisted here.

import {
  ApiError,
  kindForStatus,
  parseGuardKinds,
} from "./errors";
import type {
  ArchiveBody,
  AttentionUpdate,
  AuditEntry,
  BankingRecord,
  CalendarUpdate,
  CreateRuleBody,
  LabelBody,
  Page,
  Receipt,
  RevealedSealed,
  SealedMeta,
  SearchHit,
  SendBody,
  SenderRule,
  Shipment,
  StoreStats,
  TriageConfig,
  TriageConfigPatch,
  ClientThreadView,
  UnsubResolution,
  UnsubscribeRecord,
  UnsubscribeResult,
  UpdatesParams,
  UsageResponse,
} from "./types";

interface ClientConfig {
  baseUrl: string;
  token: string;
}

let config: ClientConfig | null = null;

/** Set the base URL + bearer token used by all subsequent requests. */
export function configureClient(baseUrl: string, token: string): void {
  // Normalize: strip trailing slash so we can join with `/client/...`.
  config = { baseUrl: baseUrl.replace(/\/+$/, ""), token };
}

export function isConfigured(): boolean {
  return config !== null;
}

function requireConfig(): ClientConfig {
  if (!config) {
    throw new ApiError("network", 0, "client not configured");
  }
  return config;
}

interface RequestOpts {
  method?: "GET" | "POST" | "DELETE";
  query?: Record<string, string | number | undefined>;
  body?: unknown;
  /** When true, do not attempt to JSON-parse the response (204 / no-body). */
  noContent?: boolean;
}

async function request<T>(path: string, opts: RequestOpts = {}): Promise<T> {
  const { baseUrl, token } = requireConfig();
  const url = new URL(baseUrl + path);
  if (opts.query) {
    for (const [k, v] of Object.entries(opts.query)) {
      if (v !== undefined && v !== null && v !== "") {
        url.searchParams.set(k, String(v));
      }
    }
  }

  const headers: Record<string, string> = {
    Authorization: `Bearer ${token}`,
    Accept: "application/json",
  };
  if (opts.body !== undefined) {
    headers["Content-Type"] = "application/json";
  }

  let res: Response;
  try {
    res = await fetch(url.toString(), {
      method: opts.method ?? "GET",
      headers,
      body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
    });
  } catch {
    // fetch throwing means transport failure — never echo the token/url detail.
    throw new ApiError("network", 0, "cannot reach squelch server");
  }

  if (!res.ok) {
    const kind = kindForStatus(res.status);
    let message = `request failed (${res.status})`;
    try {
      const data = (await res.json()) as { error?: string };
      if (data && typeof data.error === "string") message = data.error;
    } catch {
      // non-JSON error body; keep the generic message
    }
    const guardKinds =
      kind === "guard_blocked" ? parseGuardKinds(message) : undefined;
    throw new ApiError(kind, res.status, message, guardKinds);
  }

  if (opts.noContent || res.status === 204) {
    return undefined as T;
  }
  return (await res.json()) as T;
}

// --- reads ------------------------------------------------------------------

export function getUpdates(
  params: UpdatesParams = {},
): Promise<Page<AttentionUpdate>> {
  return request<Page<AttentionUpdate>>("/client/updates", {
    query: {
      since: params.since,
      min_importance: params.min_importance,
      tier: params.tier,
      status: params.status,
      band: params.band,
      limit: params.limit,
      cursor: params.cursor,
    },
  });
}

/**
 * Poke the daemon to poll Gmail immediately instead of waiting out its ~45s
 * interval. Fire-and-forget on the server: it returns as soon as the sync loop
 * is woken, NOT when new mail has landed — so callers should re-pull the read
 * model a beat later (the 10s poller also backstops it). `triggered` is false
 * when the server has no sync loop wired (shouldn't happen for `squelchd serve`).
 */
/**
 * DEV RE-TRIAGE (POST /client/retriage): reset the LLM verdicts on the scoped
 * rows so the stage-1/stage-2/extractor pipeline re-runs. Scope is one message
 * or the trailing-days window (default 7 server-side). Returns rows reset.
 */
export function retriage(
  scope: { messageId: number } | { days: number },
): Promise<{ reset: number }> {
  const body =
    "messageId" in scope
      ? { message_id: scope.messageId }
      : { days: scope.days };
  return request<{ reset: number }>("/client/retriage", {
    method: "POST",
    body,
  });
}

export function refreshMail(): Promise<{ triggered: boolean }> {
  return request<{ triggered: boolean }>("/client/refresh", { method: "POST" });
}

export function getThread(threadId: string): Promise<ClientThreadView> {
  return request<ClientThreadView>(
    `/client/thread/${encodeURIComponent(threadId)}`,
  );
}

export function search(
  q: string,
  opts: {
    limit?: number;
    cursor?: string;
    /** keyword | semantic | hybrid. Server default is hybrid when vectors exist. */
    mode?: "keyword" | "semantic" | "hybrid";
  } = {},
): Promise<Page<SearchHit>> {
  return request<Page<SearchHit>>("/client/search", {
    query: { q, limit: opts.limit, cursor: opts.cursor, mode: opts.mode },
  });
}

export function getStats(): Promise<StoreStats> {
  return request<StoreStats>("/client/stats");
}

/**
 * Stage-2 (triage) usage history for the last `days` (default 30), newest-first,
 * with aggregate totals + the model/provider label. Additive to getStats (whose
 * `stage2` today-rollup is unchanged).
 */
export function getUsage(days?: number): Promise<UsageResponse> {
  return request<UsageResponse>("/client/usage", { query: { days } });
}

/**
 * The Stage-2 triage daily caps + the trailing-14d usage/pricing figures the
 * settings "Triage budget" estimator reads. See TriageConfig for the shape.
 */
export function getTriageConfig(): Promise<TriageConfig> {
  return request<TriageConfig>("/client/triage-config");
}

/**
 * Apply a subset of the caps (the three Stage-2 caps and/or the Stage-1 global
 * cap) and return the fresh config (same shape as GET). Each cap must be an
 * integer 1..=100000 — an out-of-range value yields a 400 (kind "bad_request").
 */
export function setTriageConfig(patch: TriageConfigPatch): Promise<TriageConfig> {
  return request<TriageConfig>("/client/triage-config", {
    method: "POST",
    body: patch,
  });
}

export function getAudit(limit?: number): Promise<AuditEntry[]> {
  return request<AuditEntry[]>("/client/audit", { query: { limit } });
}

/**
 * En-route (and optionally delivered) shipment tracking rows. Pass
 * `includeDelivered: true` to include already-delivered packages; the default
 * (false) returns only what's still in transit — what the Sitrep zone shows.
 */
export function getShipments(
  includeDelivered = false,
): Promise<Shipment[]> {
  return request<Shipment[]>("/client/shipments", {
    query: { include_delivered: String(includeDelivered) },
  });
}

/**
 * Receipts (records of money already paid) received within the last `days`
 * (default 30), newest-first. Sealed mail never produces a receipt, so these are
 * structurally sealed-free.
 */
export function getReceipts(days?: number): Promise<Receipt[]> {
  return request<Receipt[]>("/client/receipts", {
    query: { days },
  });
}

/**
 * Calendar notifications received within the last `hours` (server default 24),
 * newest-received first. These never sit in the attention bands (auto-resolved
 * at ingest) — the Sitrep right rail is their surface.
 */
export function getCalendar(hours?: number): Promise<CalendarUpdate[]> {
  return request<CalendarUpdate[]>("/client/calendar", { query: { hours } });
}

/**
 * Banking notifications — statements (balance-ready) and transaction alerts,
 * newest-received first. Sealed mail never produces a banking row, so these are
 * structurally sealed-free (same guarantee as receipts).
 */
export function getBanking(): Promise<BankingRecord[]> {
  return request<BankingRecord[]>("/client/banking");
}

// --- rules ------------------------------------------------------------------

export function listRules(): Promise<SenderRule[]> {
  return request<SenderRule[]>("/client/rules");
}

export function createRule(
  body: CreateRuleBody,
): Promise<{ rule_id: number }> {
  return request<{ rule_id: number }>("/client/rules", {
    method: "POST",
    body,
  });
}

export function deleteRule(id: number): Promise<void> {
  return request<void>(`/client/rules/${id}`, {
    method: "DELETE",
    noContent: true,
  });
}

// --- unsubscribe ------------------------------------------------------------

/**
 * Ask the server to unsubscribe from the sender of `messageId`. The server always
 * returns the `browser` shape now: an http(s) `url` the caller opens externally
 * for the human to finish (one_click/mailto server-side execution was removed).
 * A 422 (no http(s) unsubscribe link) throws ApiError (status 422); a 404
 * (unknown/sealed message) throws kind "not_found" — same as every sealed route.
 */
export function unsubscribe(messageId: number): Promise<UnsubscribeResult> {
  return request<UnsubscribeResult>("/client/unsubscribe", {
    method: "POST",
    body: { message_id: messageId },
  });
}

/** All unsubscribe records for this account, newest requested_at first. */
export function getUnsubscribes(): Promise<UnsubscribeRecord[]> {
  return request<UnsubscribeRecord[]>("/client/unsubscribes");
}

/**
 * Resolve an unsubscribe record (freezes violation accrual server-side):
 * "blocked" after creating a squelch rule, "dismissed" when the human keeps the
 * sender. 404 (kind "not_found") if no record exists for `sender`.
 */
export function setUnsubResolution(
  sender: string,
  resolution: UnsubResolution,
): Promise<{ sender: string; resolution: string }> {
  return request("/client/unsubscribes/resolution", {
    method: "POST",
    body: { sender, resolution },
  });
}

// --- sealed -----------------------------------------------------------------

export function listSealed(): Promise<SealedMeta[]> {
  return request<SealedMeta[]>("/client/sealed");
}

/**
 * Reveal exactly one sealed body. Audited server-side; response is no-store.
 * The returned body must live in React state only and be cleared on unmount.
 */
export function revealSealed(id: number): Promise<RevealedSealed> {
  return request<RevealedSealed>(`/client/sealed/${id}/reveal`, {
    method: "POST",
  });
}

// --- lifecycle --------------------------------------------------------------

export function setStatus(
  messageId: number,
  status: "new" | "open" | "done",
): Promise<{ status: string; message_id: number }> {
  return request(`/client/updates/${messageId}/status`, {
    method: "POST",
    body: { status },
  });
}

// --- actions (writes; the only mutation surface) ----------------------------

export function actionArchive(
  messageId: number,
): Promise<{ status: string; message_id: number }> {
  const body: ArchiveBody = { message_id: messageId, confirm: true };
  return request("/client/actions/archive", { method: "POST", body });
}

export function actionLabel(
  messageId: number,
  add: string[] = [],
  remove: string[] = [],
): Promise<{ status: string; message_id: number }> {
  const body: LabelBody = {
    message_id: messageId,
    add,
    remove,
    confirm: true,
  };
  return request("/client/actions/label", { method: "POST", body });
}

/**
 * Send. Two-phase by design:
 *  - Phase 1 (review): call WITHOUT `overrideGuard`. A guarded body yields a
 *    422 ApiError with `.guardKinds` for the review pane to surface.
 *  - Phase 2 (fire): call with `overrideGuard: true` only after explicit consent.
 * `confirm: true` is always sent (keystroke = consent per undo-first design).
 */
export function actionSend(input: {
  body: string;
  replyToMessageId?: number;
  to?: string;
  subject?: string;
  overrideGuard?: boolean;
}): Promise<{ status: string }> {
  const body: SendBody = {
    body: input.body,
    reply_to_message_id: input.replyToMessageId,
    to: input.to,
    subject: input.subject,
    confirm: true,
    override_guard: input.overrideGuard ?? false,
  };
  return request("/client/actions/send", { method: "POST", body });
}
