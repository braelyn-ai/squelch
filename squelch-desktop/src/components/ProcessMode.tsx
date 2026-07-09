// PROCESS MODE — the `p` triage deck (UX-DIRECTIONS alternative A, "survives as
// process-mode"). OWNED BY: view-agent-2 (actions).
//
// Card-by-card walk of the NEW + STILL OPEN bands with the same verbs as the
// list (r reply, e archive, d done, t tune, Space skip). archive/done resolve
// the item, drop it from the queue, and advance; the counter ticks down to an
// empty-queue "cleared" state. Reads the live bands from the store so items the
// user resolves elsewhere fall out too.

import { useMemo, useState, useEffect } from "react";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import type { AttentionUpdate } from "../api";
import { useActions } from "../actions/useActions";

export function ProcessMode({ onClose }: { onClose: () => void }) {
  const sitrep = useStore((s) => s.sitrep);
  const act = useActions();

  useKeyContext("modal");

  // Snapshot the queue on entry (new + open, in band order). We keep a set of
  // ids we've already passed/handled so the live-band reconciliation below can
  // present remaining work while items resolve underneath us.
  const [queue] = useState<AttentionUpdate[]>(() => [
    ...sitrep.new,
    ...sitrep.open,
  ]);
  const [handled, setHandled] = useState<Set<number>>(() => new Set());
  const [idx, setIdx] = useState(0);

  // An item is "still pending" if it hasn't been handled here AND still exists
  // in a live band (someone may resolve it elsewhere).
  const liveIds = useMemo(() => {
    const s = new Set<number>();
    for (const u of [...sitrep.new, ...sitrep.open]) s.add(u.id);
    return s;
  }, [sitrep]);

  const pending = useMemo(
    () => queue.filter((u) => !handled.has(u.id) && liveIds.has(u.id)),
    [queue, handled, liveIds],
  );

  // Clamp the cursor if the pending list shrank underneath us.
  useEffect(() => {
    if (idx > pending.length - 1) setIdx(Math.max(0, pending.length - 1));
  }, [pending.length, idx]);

  const total = queue.length;
  const done = total - pending.length;
  const current = pending[idx] ?? null;

  function markHandled(id: number) {
    setHandled((prev) => {
      const next = new Set(prev);
      next.add(id);
      return next;
    });
    // Cursor stays put; the handled item drops out and the next slides in.
  }

  function skip() {
    if (pending.length === 0) return;
    setIdx((i) => (i + 1) % pending.length);
  }

  const bindings = useMemo(
    () => [
      { key: "Escape", description: "exit process mode", handler: () => onClose() },
      { key: "q", description: "exit", handler: () => onClose() },
      { key: "Space", description: "skip", handler: () => skip() },
      { key: "j", description: "next", handler: () => skip() },
      {
        key: "k",
        description: "prev",
        handler: () =>
          setIdx((i) => (pending.length === 0 ? 0 : (i - 1 + pending.length) % pending.length)),
      },
      {
        key: "r",
        description: "reply",
        handler: () => {
          if (current) act.reply(current);
        },
      },
      {
        key: "e",
        description: "archive",
        handler: () => {
          if (current) {
            act.archive(current);
            markHandled(current.id);
          }
        },
      },
      {
        key: "d",
        description: "done",
        handler: () => {
          if (current) {
            act.done(current);
            markHandled(current.id);
          }
        },
      },
      {
        key: "t",
        description: "tune sender",
        handler: () => {
          if (current) act.tune(current.sender);
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [current, pending.length, act],
  );
  useKeys("modal", bindings, [bindings]);

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        background: "var(--overlay)",
        display: "grid",
        placeItems: "center",
        zIndex: 90,
      }}
    >
      <div style={{ width: 640, maxWidth: "92vw" }}>
        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            color: "var(--fg-dim)",
            fontSize: 12,
            marginBottom: 8,
          }}
        >
          <span>process mode · <kbd>space</kbd> skip · <kbd>esc</kbd> exit</span>
          <span className="num">
            {done} / {total} cleared · {pending.length} left
          </span>
        </div>

        {current ? (
          <Card u={current} />
        ) : (
          <EmptyState total={total} onClose={onClose} />
        )}
      </div>
    </div>
  );
}

function Card({ u }: { u: AttentionUpdate }) {
  return (
    <div
      style={{
        background: "var(--bg-raised)",
        border: "1px solid var(--border)",
        borderRadius: 6,
        padding: 20,
      }}
    >
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "baseline",
          marginBottom: 10,
        }}
      >
        <span style={{ color: "var(--fg)", fontSize: 14 }}>{u.sender}</span>
        <span className="num" style={{ color: "var(--fg-dim)", fontSize: 12 }}>
          importance {u.importance} · {u.tier}
        </span>
      </div>
      <div style={{ color: "var(--fg)", fontSize: 14, marginBottom: 8 }}>
        {u.one_line}
      </div>
      <div style={{ color: "var(--fg-dim)", fontSize: 12, marginBottom: 16 }}>
        {u.reason}
      </div>
      <div style={{ display: "flex", gap: 10, color: "var(--fg-faint)", fontSize: 12 }}>
        <span><kbd>r</kbd> reply</span>
        <span><kbd>e</kbd> archive</span>
        <span><kbd>d</kbd> done</span>
        <span><kbd>t</kbd> tune</span>
        <span><kbd>space</kbd> skip</span>
      </div>
    </div>
  );
}

function EmptyState({ total, onClose }: { total: number; onClose: () => void }) {
  return (
    <div
      style={{
        background: "var(--bg-raised)",
        border: "1px solid var(--border)",
        borderRadius: 6,
        padding: 32,
        textAlign: "center",
      }}
    >
      <div style={{ color: "var(--accent)", fontSize: 15, marginBottom: 6 }}>
        {total === 0 ? "nothing to process" : "queue cleared"}
      </div>
      <div style={{ color: "var(--fg-dim)", fontSize: 12, marginBottom: 16 }}>
        {total === 0
          ? "no new or still-open items right now."
          : `worked through ${total} item${total === 1 ? "" : "s"}.`}
      </div>
      <button onClick={() => onClose()}>esc · back to sitrep</button>
    </div>
  );
}
