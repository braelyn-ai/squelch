// Browse-all (`a`) — the "radio console" survivor. Fetches ALL updates incl.
// below-squelch (no band filter), tier-colored, ranked by importance. A
// client-side squelch knob (min importance) hides the noise below the line
// without re-fetching. j/k selects, Enter opens the thread.

import { useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api";
import type { AttentionUpdate } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { relAge, tierColor, importanceColor } from "../lib/format";

export function BrowseView() {
  const openSide = useStore((s) => s.openSide);
  const [all, setAll] = useState<AttentionUpdate[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [squelch, setSquelch] = useState(0); // client-side min importance
  const [idx, setIdx] = useState(0);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    api
      .getUpdates({ limit: 500 })
      .then((page) => {
        if (!alive) return;
        // Highest importance first — the ranked board.
        const sorted = [...page.items].sort(
          (a, b) => b.importance - a.importance,
        );
        setAll(sorted);
        setError(null);
      })
      .catch((e) => {
        if (alive)
          setError(e instanceof ApiError ? e.message : "load failed");
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  const visible = useMemo(
    () => all.filter((u) => u.importance >= squelch),
    [all, squelch],
  );

  // Keep selection in range as the knob moves.
  useEffect(() => {
    setIdx((i) => Math.min(i, Math.max(0, visible.length - 1)));
  }, [visible.length]);

  const openThread = (u: AttentionUpdate | undefined) => {
    if (u) openSide({ kind: "thread", threadId: u.thread_id });
  };

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next",
        handler: () => setIdx((i) => Math.min(visible.length - 1, i + 1)),
      },
      { key: "k", description: "prev", handler: () => setIdx((i) => Math.max(0, i - 1)) },
      {
        key: "Enter",
        description: "open thread",
        handler: () => openThread(visible[idx]),
      },
      {
        key: "+",
        description: "raise squelch",
        handler: () => setSquelch((s) => Math.min(100, s + 5)),
      },
      {
        key: "=",
        description: "raise squelch",
        handler: () => setSquelch((s) => Math.min(100, s + 5)),
      },
      {
        key: "-",
        description: "lower squelch",
        handler: () => setSquelch((s) => Math.max(0, s - 5)),
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [visible, idx],
  );
  useKeys("modal", bindings, [bindings]);

  if (loading) return <div className="side-loading">loading all mail…</div>;
  if (error) return <div className="side-error">{error}</div>;

  const hidden = all.length - visible.length;

  return (
    <div>
      <div className="browse-toolbar num">
        <span>squelch ≥ {squelch}</span>
        <input
          type="range"
          min={0}
          max={100}
          step={5}
          value={squelch}
          onChange={(e) => setSquelch(Number(e.target.value))}
        />
        <span>{hidden} below line</span>
      </div>

      {visible.length === 0 && (
        <div className="side-empty">nothing above the squelch line.</div>
      )}

      {visible.map((u, i) => (
        <div
          key={u.id}
          className={`row num${i === idx ? " sel" : ""}`}
          style={{ borderLeftColor: i === idx ? tierColor(u.tier) : "transparent" }}
          onClick={() => setIdx(i)}
          onDoubleClick={() => openThread(u)}
        >
          <span
            className="tier-dot"
            style={{ background: tierColor(u.tier) }}
            title={u.tier}
          />
          <span className="imp" style={{ color: importanceColor(u.importance) }}>
            {u.importance}
          </span>
          <span className="sender" title={u.sender}>
            {u.sender}
          </span>
          <span className="one-line" title={u.one_line}>
            {u.one_line}
          </span>
          <span className="meta">
            <span className="age">{relAge(u.surfaced_at)}</span>
          </span>
        </div>
      ))}
    </div>
  );
}
