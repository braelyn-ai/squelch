// ARRIVAL DETECTION for the 2FA "present, don't read" flow.
//
// The app already polls sealed metadata (useSitrep, 10s). This hook watches that
// list and fires a one-shot flow the first time a genuinely-new auth message
// appears: a countdown ring on the auth rail, and — for code kinds (otp /
// login_code / verification) — an auto-reveal (server-side audited) + client-
// side code extraction that pops a modal. Password resets / sign-in alerts get
// the ring but no modal.
//
// A message counts as a fresh arrival when it is NOT in the persisted seen-set
// AND its received time is within ~2 minutes (so history never fires). On the
// very first run we silently seed ALL current ids into the set so the backlog
// stays quiet. The seen-set + ring expiry ARE the read state — Gmail read-
// marking is impossible under gmail.readonly and isn't wanted.

import { useEffect, useRef } from "react";
import { api } from "../api";
import type { SealedMeta } from "../api";
import { useStore } from "./store";
import { extractCode, isCodeKind } from "../lib/authCode";

const SEEN_KEY = "squelch.auth-seen";
const SEEN_CAP = 200;
/** A message older than this on first sight is treated as history (no flow). */
const FRESH_WINDOW_MS = 2 * 60_000;

/** Load the persisted seen-set (ids we've already processed). */
function loadSeen(): Set<number> {
  try {
    const raw = localStorage.getItem(SEEN_KEY);
    if (!raw) return new Set();
    const arr = JSON.parse(raw) as unknown;
    if (!Array.isArray(arr)) return new Set();
    return new Set(arr.filter((n): n is number => typeof n === "number"));
  } catch {
    return new Set();
  }
}

/** Persist the seen-set, capped to the most-recent SEEN_CAP ids (numeric order). */
function saveSeen(seen: Set<number>): void {
  try {
    const capped = [...seen].sort((a, b) => a - b).slice(-SEEN_CAP);
    localStorage.setItem(SEEN_KEY, JSON.stringify(capped));
  } catch {
    // Non-fatal: worst case a flow re-fires after a reload. Never throw here.
  }
}

/** Whole ms since an ISO stamp; large number if missing/invalid (=> treat old). */
function ageMs(iso: string | null | undefined): number {
  if (!iso) return Number.MAX_SAFE_INTEGER;
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return Number.MAX_SAFE_INTEGER;
  return Date.now() - t;
}

export function useAuthArrival(): void {
  const sealed = useStore((s) => s.sitrep.sealed);
  const pushAuthRing = useStore((s) => s.pushAuthRing);
  const pushAuthCode = useStore((s) => s.pushAuthCode);

  // In-memory mirror of the persisted seen-set + a first-run guard. Refs so the
  // effect can read/write without re-subscribing on every poll.
  const seenRef = useRef<Set<number> | null>(null);
  const seededRef = useRef(false);

  useEffect(() => {
    if (seenRef.current === null) seenRef.current = loadSeen();
    const seen = seenRef.current;

    // First run of this session: seed the entire current backlog silently so we
    // only ever fire for messages that arrive AFTER the app is watching.
    if (!seededRef.current) {
      seededRef.current = true;
      let changed = false;
      for (const m of sealed) {
        if (!seen.has(m.id)) {
          seen.add(m.id);
          changed = true;
        }
      }
      if (changed) saveSeen(seen);
      return;
    }

    const arrivals: SealedMeta[] = [];
    for (const m of sealed) {
      if (seen.has(m.id)) continue;
      seen.add(m.id); // mark immediately so a re-poll never double-fires
      // Only genuinely-fresh messages fire the flow; late-arriving history stays
      // quiet but is still recorded as seen above.
      if (ageMs(m.received_at) <= FRESH_WINDOW_MS) arrivals.push(m);
    }

    if (arrivals.length === 0) return;
    saveSeen(seen);

    // Ring for every arrival; oldest-first so the queue ends up newest-first.
    const ordered = [...arrivals].sort(
      (a, b) => ageMs(b.received_at) - ageMs(a.received_at),
    );
    for (const m of ordered) {
      pushAuthRing(m.id);
      if (isCodeKind(m.kind)) void revealAndQueue(m, pushAuthCode);
    }
    // Rings auto-expire via the Sidebar animation lifecycle; no timers here.
  }, [sealed, pushAuthRing, pushAuthCode]);
}

/** Auto-reveal a code message (audited) and enqueue the extracted code. */
async function revealAndQueue(
  m: SealedMeta,
  pushAuthCode: (entry: { meta: SealedMeta; code: string | null }) => void,
): Promise<void> {
  try {
    const revealed = await api.revealSealed(m.id);
    pushAuthCode({ meta: m, code: extractCode(revealed.body) });
  } catch {
    // Reveal failed (network / write-guard / already-consumed): still show the
    // modal so the human can jump to Auth. No code, no body retained.
    pushAuthCode({ meta: m, code: null });
  }
}
