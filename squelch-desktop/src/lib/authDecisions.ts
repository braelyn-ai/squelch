// "Needs a decision" state for the Auth page — which sign-in alerts and
// password resets the human has already looked at and ruled on.
//
// DEVICE-LOCAL BY DESIGN. There is no server field for this: /client/sealed is
// read-only metadata, and squelch cannot mark Gmail read either (the sync
// credential is gmail.readonly by hard invariant). So the decision lives in
// localStorage, exactly like the arrival seen-set in state/useAuthArrival —
// which is already the app's model for "read state is a local artifact".
//
// What that costs you, stated plainly: decisions do not follow you to another
// machine, and clearing site data brings every card back. The alternative is a
// schema + endpoints for what is currently a UI affordance; if these decisions
// ever need to be a RECORD (audited, cross-device), that is the moment to
// promote them server-side rather than to quietly grow this file.
//
// Note the asymmetry in what the two answers mean. "That was me" is a
// dismissal — it resolves the card and nothing else happens. "Not me" is NOT a
// dismissal: it is the start of an investigation, so the caller opens the
// message so the human can actually read what happened and act on it. We
// record the verdict either way, because "I flagged this one" is worth
// remembering even though we cannot act on it for you.

/** Kinds that ask the human a question rather than handing them a code. */
export const DECISION_KINDS = new Set(["login_alert", "password_reset", "magic_link"]);

/** True when this auth message is one the human should rule on. */
export function needsDecision(kind: string | null | undefined): boolean {
  return kind != null && DECISION_KINDS.has(kind);
}

/** What the human said about a flagged message. */
export type AuthVerdict = "mine" | "not-mine";

const KEY = "squelch.auth-decisions";
/** Cap the stored map so it cannot grow without bound. */
const CAP = 300;

type Stored = Record<string, AuthVerdict>;

function read(): Stored {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return parsed as Stored;
  } catch {
    return {};
  }
}

let cache: Stored | null = null;
const listeners = new Set<() => void>();

function all(): Stored {
  if (cache === null) cache = read();
  return cache;
}

/** The recorded verdict for a message, or null while it is still open. */
export function getDecision(id: number): AuthVerdict | null {
  return all()[String(id)] ?? null;
}

/** Record a verdict and notify subscribers. */
export function setDecision(id: number, verdict: AuthVerdict): void {
  const next = { ...all(), [String(id)]: verdict };
  // Keys are message ids, so numeric order is arrival order: dropping the
  // lowest keys evicts the oldest decisions first.
  const keys = Object.keys(next).sort((a, b) => Number(a) - Number(b));
  if (keys.length > CAP) {
    for (const k of keys.slice(0, keys.length - CAP)) delete next[k];
  }
  cache = next;
  try {
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {
    // Storage unavailable — the in-memory value still holds for this session.
  }
  for (const l of listeners) l();
}

/** Subscribe to decision changes (useSyncExternalStore-shaped). */
export function subscribeDecisions(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** Snapshot identity for useSyncExternalStore — changes only on a write. */
export function decisionsSnapshot(): Stored {
  return all();
}
