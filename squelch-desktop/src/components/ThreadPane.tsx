// Thread drill-in. Fetches GET /client/thread/{id} (sanitized messages only),
// renders them chronologically, j/k moves the highlighted message and scrolls it
// into view. Bodies are already server-sanitized; still selectable-text only,
// never persisted beyond this mounted panel. Esc (owned by SideViews) closes.

import { useEffect, useMemo, useRef, useState } from "react";
import { api, ApiError } from "../api";
import type { ThreadView } from "../api";
import { useKeys } from "../keys";
import { dateTime } from "../lib/format";

export function ThreadPane({ threadId }: { threadId: string }) {
  const [thread, setThread] = useState<ThreadView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [idx, setIdx] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
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

  const count = thread?.messages.length ?? 0;

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
    ],
    [count],
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
      {thread.messages.length === 0 && (
        <div className="side-empty">no messages in this thread.</div>
      )}
      {thread.messages.map((m, i) => (
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
          <div className="msg-body">{m.content}</div>
        </div>
      ))}
    </div>
  );
}
