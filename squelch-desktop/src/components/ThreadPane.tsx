// Thread drill-in. Fetches GET /client/thread/{id} (ClientThreadView: sanitized
// text + optional server-sanitized `html`), renders messages chronologically.
// j/k moves the highlighted message and scrolls it into view; i toggles remote
// content for the highlighted message. Bodies are already server-sanitized;
// html renders in a hard-sandboxed iframe (see EmailFrame), plain text falls
// back to the existing selectable-text view. Nothing is persisted beyond this
// mounted panel — the per-message remote-allow set is in-memory and dies on
// unmount (thread close). Esc (owned by SideViews) closes.

import { useEffect, useMemo, useRef, useState } from "react";
import { api, ApiError } from "../api";
import type { ClientThreadView } from "../api";
import { useKeys } from "../keys";
import { dateTime } from "../lib/format";
import { EmailFrame, hasRemoteRefs } from "./EmailFrame";

export function ThreadPane({ threadId }: { threadId: string }) {
  const [thread, setThread] = useState<ClientThreadView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [idx, setIdx] = useState(0);
  // Message ids for which the user has opted into remote content. In-memory
  // only; resets whenever a new thread loads (or the panel unmounts).
  const [remoteOk, setRemoteOk] = useState<Set<number>>(() => new Set());
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    setRemoteOk(new Set()); // reset remote-content opt-in per thread.
    api
      .getThread(threadId)
      .then((t) => {
        if (!alive) return;
        setThread(t);
        setIdx(Math.max(0, t.messages.length - 1)); // land on newest
      })
      .catch((e) => {
        if (alive)
          setError(e instanceof ApiError ? e.message : "thread load failed");
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [threadId]);

  const messages = thread?.messages ?? [];
  const count = messages.length;

  const allowRemoteFor = (id: number) =>
    setRemoteOk((prev) => {
      if (prev.has(id)) return prev;
      const next = new Set(prev);
      next.add(id);
      return next;
    });

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next message",
        handler: () => setIdx((i) => Math.min(count - 1, i + 1)),
      },
      {
        key: "k",
        description: "prev message",
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
      {
        key: "i",
        description: "load remote images (selected message)",
        handler: () => {
          const m = messages[idx];
          // Only meaningful when the selected message has remote refs.
          if (m?.html && hasRemoteRefs(m.html)) allowRemoteFor(m.id);
        },
      },
    ],
    [count, idx, messages],
  );
  useKeys("modal", bindings, [bindings]);

  // Scroll the highlighted message into view.
  useEffect(() => {
    const el = containerRef.current?.querySelector<HTMLElement>(
      `[data-mi="${idx}"]`,
    );
    el?.scrollIntoView({ block: "nearest" });
  }, [idx]);

  if (loading) return <div className="side-loading">loading thread…</div>;
  if (error) return <div className="side-error">{error}</div>;
  if (!thread) return <div className="side-empty">no thread.</div>;

  return (
    <div ref={containerRef}>
      {messages.length === 0 && (
        <div className="side-empty">no messages in this thread.</div>
      )}
      {messages.map((m, i) => (
        <div
          key={m.id}
          data-mi={i}
          className={`msg${i === idx ? " sel" : ""}`}
          onClick={() => setIdx(i)}
        >
          <div className="msg-head num">
            <span className="msg-from">{m.from_name ?? m.from_addr}</span>
            <span className="msg-when">{dateTime(m.received_at)}</span>
          </div>
          {m.html ? (
            <EmailFrame
              html={m.html}
              selected={i === idx}
              remoteAllowed={remoteOk.has(m.id)}
              onAllowRemote={() => allowRemoteFor(m.id)}
            />
          ) : (
            <div className="msg-body">{m.content}</div>
          )}
        </div>
      ))}
    </div>
  );
}
