// Polling hook that keeps the sitrep read model fresh: fetches the three bands
// + stats + sealed metadata every 10s and on window focus. Writes results into
// the store; view agents just read store.sitrep.
//
// Each band is fetched with its own server-side `band` filter so the buckets
// match the server's definitions exactly (standing/new/open). Sealed is
// metadata-only (never bodies here).

import { useEffect, useRef, useCallback } from "react";
import { api, ApiError } from "../api";
import { useStore } from "./store";

const POLL_MS = 10_000;
const PAGE_LIMIT = 200;

export interface SitrepController {
  refresh: () => Promise<void>;
}

export function useSitrep(): SitrepController {
  const connStatus = useStore((s) => s.connStatus);
  const setSitrep = useStore((s) => s.setSitrep);
  const setRefreshError = useStore((s) => s.setRefreshError);
  const markRefreshed = useStore((s) => s.markRefreshed);
  const select = useStore((s) => s.select);

  const inFlight = useRef(false);

  const refresh = useCallback(async () => {
    if (inFlight.current) return;
    if (useStore.getState().connStatus !== "connected") return;
    inFlight.current = true;
    try {
      const [standing, fresh, open, stats, sealed] = await Promise.all([
        api.getUpdates({ band: "standing", limit: PAGE_LIMIT }),
        api.getUpdates({ band: "new", limit: PAGE_LIMIT }),
        api.getUpdates({ band: "open", limit: PAGE_LIMIT }),
        api.getStats(),
        api.listSealed(),
      ]);
      setSitrep({
        standing: standing.items,
        new: fresh.items,
        open: open.items,
        stats,
        sealed,
      });
      setRefreshError(null);
      markRefreshed();

      // Keep a valid selection: if nothing selected, land on the first row.
      const st = useStore.getState();
      if (st.selectedId === null) {
        const ids = st.orderedIds();
        if (ids.length > 0) select(ids[0]);
      }
    } catch (e) {
      const msg =
        e instanceof ApiError ? e.message : "refresh failed";
      setRefreshError(msg);
    } finally {
      inFlight.current = false;
    }
  }, [setSitrep, setRefreshError, markRefreshed, select]);

  // Interval + focus polling, only while connected.
  useEffect(() => {
    if (connStatus !== "connected") return;
    void refresh();
    const iv = window.setInterval(() => void refresh(), POLL_MS);
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);
    return () => {
      window.clearInterval(iv);
      window.removeEventListener("focus", onFocus);
    };
  }, [connStatus, refresh]);

  return { refresh };
}
