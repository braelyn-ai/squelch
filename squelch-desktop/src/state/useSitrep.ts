// Polling hook that keeps the sitrep read model fresh: fetches the three bands
// + stats + sealed metadata every 10s and on window focus. Writes results into
// the store; view agents just read store.sitrep.
//
// Each band is fetched with its own server-side `band` filter so the buckets
// match the server's definitions exactly (standing/new/open). Sealed is
// metadata-only (never bodies here).

import { useEffect } from "react";
import { api, ApiError } from "../api";
import { useStore } from "./store";

const POLL_MS = 10_000;
const PAGE_LIMIT = 200;

export interface SitrepController {
  refresh: () => Promise<void>;
}

// Module-level in-flight guard shared by the interval poller AND the manual
// refresh button, so a user poke never races an overlapping scheduled pull.
let inFlight = false;

/**
 * Fetch the sitrep read model once and write it into the store. Standalone (not
 * a hook) so the poller and the manual refresh button share EXACTLY one code
 * path. No-ops if a pull is already in flight or the door isn't connected.
 */
export async function pullSitrep(): Promise<void> {
  if (inFlight) return;
  if (useStore.getState().connStatus !== "connected") return;
  const st0 = useStore.getState();
  inFlight = true;
  try {
    const [standing, fresh, open, stats, sealed] = await Promise.all([
      api.getUpdates({ band: "standing", limit: PAGE_LIMIT }),
      api.getUpdates({ band: "new", limit: PAGE_LIMIT }),
      api.getUpdates({ band: "open", limit: PAGE_LIMIT }),
      api.getStats(),
      api.listSealed(),
    ]);
    st0.setSitrep({
      standing: standing.items,
      new: fresh.items,
      open: open.items,
      stats,
      sealed,
    });
    st0.setRefreshError(null);
    st0.markRefreshed();

    // Keep a valid selection: if nothing selected, land on the first row.
    const st = useStore.getState();
    if (st.selectedId === null) {
      const ids = st.orderedIds();
      if (ids.length > 0) st.select(ids[0]);
    }
  } catch (e) {
    // Keep the kind so the UI can say "daemon unreachable" vs "token rejected"
    // instead of one undifferentiated failure.
    const err =
      e instanceof ApiError
        ? { message: e.message, kind: e.kind }
        : { message: "refresh failed", kind: "unknown" as const };
    useStore.getState().setRefreshError(err);
  } finally {
    inFlight = false;
  }
}

/**
 * MANUAL refresh: poke the daemon to poll Gmail NOW, then re-pull the read model
 * so freshly-ingested mail shows without waiting out the ~45s server poll or the
 * 10s client poll. The server poke is fire-and-forget, so we pull once right
 * after the rows are likely landed and once more a beat later to catch a slower
 * Gmail round trip. Safe to call repeatedly; the in-flight guard coalesces.
 */
export async function triggerMailRefresh(): Promise<void> {
  if (useStore.getState().connStatus !== "connected") return;
  try {
    await api.refreshMail();
  } catch {
    // Poke failed (network/door) — still try to pull; pullSitrep surfaces its
    // own error. Never echo the token/url.
  }
  await new Promise((r) => setTimeout(r, 400));
  await pullSitrep();
  await new Promise((r) => setTimeout(r, 1600));
  await pullSitrep();
}

export function useSitrep(): SitrepController {
  const connStatus = useStore((s) => s.connStatus);

  // Interval + focus polling, only while connected.
  useEffect(() => {
    if (connStatus !== "connected") return;
    void pullSitrep();
    const iv = window.setInterval(() => void pullSitrep(), POLL_MS);
    const onFocus = () => void pullSitrep();
    window.addEventListener("focus", onFocus);
    return () => {
      window.clearInterval(iv);
      window.removeEventListener("focus", onFocus);
    };
  }, [connStatus]);

  return { refresh: pullSitrep };
}
