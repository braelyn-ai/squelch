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
  ClientThreadView,
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

export function getThread(threadId: string): Promise<ClientThreadView> {
  return request<ClientThreadView>(
    `/client/thread/${encodeURIComponent(threadId)}`,
  );
}

export function search(
  q: string,
  opts: { limit?: number; cursor?: string } = {},
): Promise<Page<SearchHit>> {
  return request<Page<SearchHit>>("/client/search", {
    query: { q, limit: opts.limit, cursor: opts.cursor },
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
