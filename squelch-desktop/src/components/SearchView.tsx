// Search side view. Input debounced to GET /client/search?q=; results list with
// j/k selection; Enter opens the selected hit's thread (replaces this side view
// with the thread drill-in). The input auto-focuses so `/` lands ready to type.

import { useEffect, useMemo, useRef, useState } from "react";
import { api, ApiError } from "../api";
import type { SearchHit } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { dateTime } from "../lib/format";

export function SearchView({ initialQuery }: { initialQuery: string }) {
  const openSide = useStore((s) => s.openSide);
  const [q, setQ] = useState(initialQuery);
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [idx, setIdx] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Debounced query.
  useEffect(() => {
    const term = q.trim();
    if (!term) {
      setHits([]);
      setError(null);
      setLoading(false);
      return;
    }
    let alive = true;
    setLoading(true);
    const h = window.setTimeout(() => {
      api
        .search(term, { limit: 50 })
        .then((page) => {
          if (!alive) return;
          setHits(page.items);
          setIdx(0);
          setError(null);
        })
        .catch((e) => {
          if (alive)
            setError(e instanceof ApiError ? e.message : "search failed");
        })
        .finally(() => {
          if (alive) setLoading(false);
        });
    }, 220);
    return () => {
      alive = false;
      window.clearTimeout(h);
    };
  }, [q]);

  const openHit = (hit: SearchHit | undefined) => {
    if (hit) openSide({ kind: "thread", threadId: hit.thread_id });
  };

  const bindings = useMemo(
    () => [
      {
        key: "ArrowDown",
        description: "next hit",
        allowInInput: true,
        handler: () => setIdx((i) => Math.min(hits.length - 1, i + 1)),
      },
      {
        key: "ArrowUp",
        description: "prev hit",
        allowInInput: true,
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
      {
        key: "Enter",
        description: "open thread",
        allowInInput: true,
        handler: () => openHit(hits[idx]),
      },
      // j/k also work when focus is not in the input.
      {
        key: "j",
        description: "next hit",
        handler: () => setIdx((i) => Math.min(hits.length - 1, i + 1)),
      },
      {
        key: "k",
        description: "prev hit",
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [hits, idx],
  );
  useKeys("modal", bindings, [bindings]);

  return (
    <div>
      <div className="search-bar">
        <input
          ref={inputRef}
          value={q}
          placeholder="search mail…"
          onChange={(e) => setQ(e.target.value)}
        />
      </div>

      {loading && <div className="side-loading">searching…</div>}
      {error && <div className="side-error">{error}</div>}
      {!loading && !error && q.trim() && hits.length === 0 && (
        <div className="side-empty">no matches.</div>
      )}

      {hits.map((h, i) => (
        <div
          key={h.id}
          className={`hit num${i === idx ? " sel" : ""}`}
          onClick={() => setIdx(i)}
          onDoubleClick={() => openHit(h)}
        >
          <div className="hit-head">
            <span className="hit-from">{h.from_name ?? h.from_addr}</span>
            <span className="hit-when">{dateTime(h.received_at)}</span>
          </div>
          <div className="hit-subject">{h.subject}</div>
          <div className="hit-snip">{h.snippet}</div>
        </div>
      ))}
    </div>
  );
}
