// Client-side token tally for the BYOK assistant. The assistant's calls go
// straight from the user's machine to their provider with their own key — the
// squelch server never sees them, so this usage is tracked locally (in
// localStorage) and feeds the Usage page's "Assistant" slot. Entirely separate
// from server-side Stage-2 (triage) usage.

const KEY = "squelch.assistant.usage";

export interface AssistantUsage {
  /** Number of completed asks (not per-turn API calls). */
  asks: number;
  inputTokens: number;
  outputTokens: number;
  /** Last model used, for display. */
  lastModel: string | null;
  /** RFC3339 of the most recent ask. */
  lastAt: string | null;
}

const EMPTY: AssistantUsage = {
  asks: 0,
  inputTokens: 0,
  outputTokens: 0,
  lastModel: null,
  lastAt: null,
};

export function getAssistantUsage(): AssistantUsage {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return { ...EMPTY };
    const parsed = JSON.parse(raw) as Partial<AssistantUsage>;
    return {
      asks: parsed.asks ?? 0,
      inputTokens: parsed.inputTokens ?? 0,
      outputTokens: parsed.outputTokens ?? 0,
      lastModel: parsed.lastModel ?? null,
      lastAt: parsed.lastAt ?? null,
    };
  } catch {
    return { ...EMPTY };
  }
}

/** Fold one completed ask (summed across its tool-loop turns) into the tally. */
export function recordAssistantUsage(
  model: string,
  inputTokens: number,
  outputTokens: number,
  at: string,
): void {
  const cur = getAssistantUsage();
  const next: AssistantUsage = {
    asks: cur.asks + 1,
    inputTokens: cur.inputTokens + inputTokens,
    outputTokens: cur.outputTokens + outputTokens,
    lastModel: model,
    lastAt: at,
  };
  try {
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {
    // storage full/unavailable — usage display is best-effort, not load-bearing.
  }
}

export function clearAssistantUsage(): void {
  try {
    localStorage.removeItem(KEY);
  } catch {
    // nothing to clear.
  }
}
