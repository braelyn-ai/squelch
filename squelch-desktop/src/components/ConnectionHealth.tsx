// Connection-health surfaces for the daemon link. Two states, deliberately
// distinct so a dead daemon can never masquerade as inbox zero:
//
//   DaemonDownPane — the read model has NEVER loaded this session and refreshes
//     are failing. Replaces the routed main view entirely: there is no data to
//     show, so showing empty bands would lie. Offers retry + Settings.
//
//   ConnectionBanner — we HAVE synced data but the poller is now failing.
//     Keeps the (stale) data on screen and pins a banner saying how old it is.
//
// A 401 is called out separately from a transport failure: "token rejected"
// sends you to Settings, "unreachable" sends you to the daemon. Never echoes
// the server URL or token (see api/client.ts security note).

import { useState } from "react";
import { CloudOff, KeyRound, RefreshCw, Settings } from "lucide-react";
import { useStore, pullSitrep } from "../state";
import type { RefreshError } from "../state";
import { relAge } from "../lib/format";

/** True when the failure is the token, not the transport. */
function isAuthFailure(err: RefreshError): boolean {
  return err.kind === "unauthorized" || err.kind === "forbidden";
}

function RetryButton() {
  const [busy, setBusy] = useState(false);
  return (
    <button
      type="button"
      className="conn-retry"
      disabled={busy}
      onClick={() => {
        setBusy(true);
        void pullSitrep().finally(() => setBusy(false));
      }}
    >
      <RefreshCw size={13} className={busy ? "spin" : undefined} />
      retry now
    </button>
  );
}

/**
 * Fullscreen replacement for the routed view when nothing has ever loaded.
 * Rendered by Main in place of RouteBody (Settings stays reachable via the
 * sidebar so the token/URL can be fixed without a working daemon).
 */
export function DaemonDownPane() {
  const refreshError = useStore((s) => s.refreshError);
  const setView = useStore((s) => s.setView);
  if (!refreshError) return null;

  const auth = isAuthFailure(refreshError);

  return (
    <div className="daemon-down">
      <div className="daemon-down-card">
        <span className="down-glyph">
          {auth ? <KeyRound size={22} /> : <CloudOff size={22} />}
        </span>
        <h1>{auth ? "token rejected" : "can't reach the squelch daemon"}</h1>
        <p className="sub">
          {auth
            ? "The server refused the saved token. Update it in Settings."
            : "The server URL didn't answer. Is squelchd running? Retrying every 10 seconds."}
        </p>
        <div className="down-actions">
          {!auth && <RetryButton />}
          <button
            type="button"
            className="conn-settings"
            onClick={() => setView("settings")}
          >
            <Settings size={13} />
            open settings
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Pinned degraded-mode banner: data on screen is real but stale because the
 * poller is failing. Mounted above the routed view so it shows on every
 * surface. Renders nothing while healthy or while DaemonDownPane owns the
 * failure (i.e. before any successful sync).
 */
export function ConnectionBanner() {
  const refreshError = useStore((s) => s.refreshError);
  const lastRefresh = useStore((s) => s.lastRefresh);
  if (!refreshError || lastRefresh === null) return null;

  const auth = isAuthFailure(refreshError);
  const age = relAge(new Date(lastRefresh).toISOString());
  const staleNote = age && age !== "now" ? ` — showing mail from ${age} ago` : "";

  return (
    <div className="conn-banner" role="alert">
      {auth ? <KeyRound size={14} /> : <CloudOff size={14} />}
      <span className="conn-banner-text">
        {auth
          ? `token rejected${staleNote}`
          : `connection to daemon lost${staleNote}`}
      </span>
      {auth ? (
        <button
          type="button"
          className="conn-settings"
          onClick={() => useStore.getState().setView("settings")}
        >
          open settings
        </button>
      ) : (
        <RetryButton />
      )}
    </div>
  );
}
