// DEV-MODE re-triage button (masthead, top right). Renders nothing unless the
// developerMode pref is on. Fires POST /client/retriage for the trailing 7
// days, toasts the reset count, and disables while in flight. The sync loop is
// woken server-side, so fresh verdicts land within seconds on the next poll.

import { useState } from "react";
import { RotateCw } from "lucide-react";
import { api, ApiError } from "../api";
import { useStore } from "../state";
import { usePref } from "../lib/prefs";

const RETRIAGE_DAYS = 7;

export function RetriageButton({ onSky = false }: { onSky?: boolean }) {
  const dev = usePref("developerMode");
  const pushToast = useStore((s) => s.pushToast);
  const [busy, setBusy] = useState(false);

  if (!dev) return null;

  const run = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const { reset } = await api.retriage({ days: RETRIAGE_DAYS });
      pushToast(
        reset > 0
          ? `re-triaging ${reset} email${reset === 1 ? "" : "s"} (last ${RETRIAGE_DAYS}d)…`
          : "nothing to re-triage in the window",
        "info",
      );
    } catch (e) {
      pushToast(e instanceof ApiError ? e.message : "re-triage failed", "error");
    } finally {
      setBusy(false);
    }
  };

  return (
    <button
      type="button"
      className={`retriage-btn${onSky ? " on-sky" : ""}`}
      onClick={() => void run()}
      disabled={busy}
      title={`dev: reset LLM verdicts for the last ${RETRIAGE_DAYS} days and re-run triage (rule-decided and sealed mail untouched)`}
    >
      <RotateCw size={12} className={busy ? "spin" : undefined} /> re-triage 7d
    </button>
  );
}
