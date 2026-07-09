// Sealed reveal panel. Fetches ONE sealed body via api.revealSealed on mount
// (audited + no-store server-side), holds it in local React state ONLY, and
// clears that state on unmount. Never written to localStorage/disk. Esc closes.
//
// SECURITY: the body is never logged and never lifted into the global store.
// Unmount nulls the state; the panel is rendered only while a reveal is active.

import { useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api";
import type { RevealedSealed, SealedMeta } from "../api";
import { useKeys, useKeyContext } from "../keys";
import { dateTime } from "../lib/format";

export interface RevealPanelProps {
  meta: SealedMeta;
  onClose: () => void;
}

export function RevealPanel({ meta, onClose }: RevealPanelProps) {
  const [revealed, setRevealed] = useState<RevealedSealed | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useKeyContext("modal");
  const bindings = useMemo(
    () => [{ key: "Escape", description: "close reveal", handler: () => onClose() }],
    [onClose],
  );
  useKeys("modal", bindings, [bindings]);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    api
      .revealSealed(meta.id)
      .then((r) => {
        if (alive) setRevealed(r);
      })
      .catch((e) => {
        if (alive)
          setError(e instanceof ApiError ? e.message : "reveal failed");
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    // Clear the sensitive body on unmount.
    return () => {
      alive = false;
      setRevealed(null);
    };
  }, [meta.id]);

  return (
    <div className="reveal-panel" onClick={onClose}>
      <div className="reveal-card num" onClick={(e) => e.stopPropagation()}>
        <div className="banner">
          <span>🔒 sensitive · one-time reveal · not stored</span>
          <span>
            <kbd>Esc</kbd> close
          </span>
        </div>
        <div className="subject">{meta.subject}</div>
        <div className="from">
          {(revealed?.from_name ? `${revealed.from_name} · ` : "") + meta.sender}
          {" · "}
          {dateTime(meta.received_at)}
          {meta.kind ? ` · ${meta.kind}` : ""}
        </div>

        {loading && <div className="side-loading">revealing…</div>}
        {error && <div className="side-error">{error}</div>}
        {revealed && <div className="body">{revealed.body}</div>}

        <div className="foot">
          <span>cleared from memory when you close this.</span>
          <span>audited server-side</span>
        </div>
      </div>
    </div>
  );
}
