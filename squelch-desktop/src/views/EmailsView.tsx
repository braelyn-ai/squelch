// EMAILS VIEW — the traditional flat inbox. This is the "I just want a normal
// email view" escape hatch: ALL updates (every tier, noise included), sorted
// by order received (newest first), one white card of dense rows — gmail, not
// triage. The abstracted bands live on the Sitrep; this surface hides nothing.
//
// Keyboard-first: j/k traverse rows; Enter drills into a thread; r/e/d
// dispatch through lib/dispatch (the seam to ActionLayer). e/d optimistically
// drop the row here (the action layer only removes it from the sitrep bands;
// this list is its own fetch). Owns the "list" KeyContext.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Inbox } from "lucide-react";
import { api, ApiError } from "../api";
import type { AttentionUpdate } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { SitrepHeader } from "../components/SitrepHeader";
import { UpdateRow } from "../components/UpdateRow";
import { flipTheme } from "../components/ThemeToggle";
import { ShortcutsOverlay } from "../components/ShortcutsOverlay";
import {
  dispatchArchive,
  dispatchDone,
  dispatchReply,
} from "../lib/dispatch";
import { prefetchThread } from "../lib/threadPrefetch";
import "../styles/sitrep.css";

// One generous page — the read model is local, this is cheap.
const FETCH_LIMIT = 500;

/** Epoch ms for "order received". surfaced_at approximates arrival; items the
 *  triage loop hasn't surfaced yet (surfaced_at null) are the newest mail, so
 *  they sort to the top. Ties (and the null bucket) break on id, which is
 *  ingest order. */
function receivedTs(u: AttentionUpdate): number {
  if (!u.surfaced_at) return Number.MAX_SAFE_INTEGER;
  const t = new Date(u.surfaced_at).getTime();
  return Number.isNaN(t) ? 0 : t;
}

export function EmailsView() {
  const sitrep = useStore((s) => s.sitrep);
  const refreshError = useStore((s) => s.refreshError);
  const lastRefresh = useStore((s) => s.lastRefresh);
  const openSide = useStore((s) => s.openSide);
  const openThread = useStore((s) => s.openThread);
  const setView = useStore((s) => s.setView);
  const fireUndo = useStore((s) => s.fireUndo);

  const [items, setItems] = useState<AttentionUpdate[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [idx, setIdx] = useState(0);

  // What last moved `idx`: keyboard (j/k/arrows) should follow-scroll the
  // selection into view; a mouse hover must NOT — hovering a row near the
  // viewport edge would otherwise scroll-jump under the cursor. The follow
  // effect reads this ref and skips mouse-driven changes.
  const moveSrc = useRef<"keyboard" | "mouse">("keyboard");
  // Keyboard move: tag the source, then step the selection.
  const moveByKey = (next: (i: number) => number) => {
    moveSrc.current = "keyboard";
    setIdx(next);
  };
  // STABLE per-row callbacks (UpdateRow is memoized): rebuilding closures for
  // 500 rows on every hover-driven idx change caused a full-list re-render per
  // hovered row. These only change identity when the fetched list does.
  const rowsRef = useRef<AttentionUpdate[]>([]);
  const idIndex = useMemo(() => {
    const m = new Map<number, number>();
    (items ?? []).forEach((u, i) => m.set(u.id, i));
    rowsRef.current = items ?? [];
    return m;
  }, [items]);
  const hoverSelectById = useCallback(
    (id: number) => {
      const i = idIndex.get(id);
      if (i !== undefined) {
        moveSrc.current = "mouse";
        setIdx(i);
      }
    },
    [idIndex],
  );
  const openUpdate = useCallback(
    (u: AttentionUpdate) => openThread(u.thread_id, rowsRef.current),
    [openThread],
  );

  // Keyboard-shortcuts help overlay ('?').
  const [showShortcuts, setShowShortcuts] = useState(false);

  const authCount = sitrep.sealed.length;
  const openAuth = () => setView("auth");

  // Pull the flat inbox; re-pull whenever the 10s sitrep poll ticks so new
  // mail lands at the top (and undone rows come back).
  useEffect(() => {
    let alive = true;
    api
      .getUpdates({ limit: FETCH_LIMIT })
      .then((page) => {
        if (!alive) return;
        // Done/archived mail leaves the inbox (gmail semantics). This also
        // keeps auto-resolved receipts out — they're records on the sitrep
        // rail, not inbox rows.
        setItems(
          page.items
            .filter((u) => u.status !== "done")
            .sort((a, b) => receivedTs(b) - receivedTs(a) || b.id - a.id),
        );
        setError(null);
      })
      .catch((e) => {
        if (alive) setError(e instanceof ApiError ? e.message : "load failed");
      });
    return () => {
      alive = false;
    };
  }, [lastRefresh]);

  const rows = items ?? [];

  // Keep the selection in range as rows arrive/leave.
  useEffect(() => {
    setIdx((i) => Math.max(0, Math.min(i, rows.length - 1)));
  }, [rows.length]);

  // INSTANT-OPEN warmers: prefetch the selected/hovered row's thread (and its
  // images) so Enter/click renders from cache. Debounced so sweeping the mouse
  // down the list doesn't fire a request per row — only rows the cursor (or
  // selection) actually rests on get warmed.
  useEffect(() => {
    const u = rows[idx];
    if (!u) return;
    const t = window.setTimeout(() => prefetchThread(u.thread_id), 120);
    return () => window.clearTimeout(t);
  }, [idx, rows]);
  // Also warm the top few rows when a fresh list lands — the likeliest opens.
  useEffect(() => {
    for (const u of (items ?? []).slice(0, 3)) prefetchThread(u.thread_id);
  }, [items]);

  // Keep the keyboard selection on screen (j/k/arrows in a long list). Skip
  // mouse-driven changes: a hover must not scroll the row it's hovering.
  useEffect(() => {
    if (moveSrc.current !== "keyboard") return;
    document
      .querySelector(".sitrep .row.sel")
      ?.scrollIntoView({ block: "nearest" });
  }, [idx]);

  const selected: AttentionUpdate | undefined = rows[idx];
  const removeRow = (id: number) =>
    setItems((xs) => (xs ? xs.filter((u) => u.id !== id) : xs));

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next",
        handler: () => moveByKey((i) => Math.min(rows.length - 1, i + 1)),
      },
      {
        key: "k",
        description: "prev",
        handler: () => moveByKey((i) => Math.max(0, i - 1)),
      },
      {
        key: "ArrowDown",
        description: "next",
        handler: () => moveByKey((i) => Math.min(rows.length - 1, i + 1)),
      },
      {
        key: "ArrowUp",
        description: "prev",
        handler: () => moveByKey((i) => Math.max(0, i - 1)),
      },
      {
        key: "Enter",
        description: "drill in",
        handler: () => {
          // Hand the current ordered inbox rows to the viewer as its queue so
          // "done + next" (e/d) can advance in place.
          if (selected) openThread(selected.thread_id, rows);
        },
      },
      {
        key: "r",
        description: "reply",
        handler: () => {
          if (selected) dispatchReply(selected);
        },
      },
      {
        key: "e",
        description: "archive",
        handler: () => {
          if (selected) {
            void dispatchArchive(selected);
            removeRow(selected.id);
          }
        },
      },
      {
        key: "d",
        description: "done",
        handler: () => {
          if (selected) {
            void dispatchDone(selected);
            removeRow(selected.id);
          }
        },
      },
      // NOTE: `t` (tune sender) is registered by ActionLayer, which owns the tune
      // overlay.
      {
        key: "a",
        description: "browse all",
        handler: () => openSide({ kind: "browse" }),
      },
      {
        key: "T",
        description: "rules",
        handler: () => setView("rules"),
      },
      {
        key: "A",
        description: "audit log",
        handler: () => setView("audit"),
      },
      {
        key: "g",
        description: "auth messages",
        handler: () => openAuth(),
      },
      {
        key: "/",
        description: "search",
        handler: () => openSide({ kind: "search", query: "" }),
      },
      { key: "u", description: "undo", handler: () => void fireUndo() },
      {
        key: "\\",
        description: "toggle light/dark theme",
        handler: () => flipTheme(),
      },
      {
        key: "?",
        description: "keyboard shortcuts",
        handler: () => setShowShortcuts((v) => !v),
      },
    ],
    // Recompute when the list or selection changes so closures stay fresh.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [rows, idx],
  );
  useKeys("list", bindings, [bindings]);

  return (
    <div className="sitrep">
      <SitrepHeader
        stats={sitrep.stats}
        standingCount={sitrep.standing.length}
        newCount={sitrep.new.length}
        openCount={sitrep.open.length}
        authCount={authCount}
        refreshError={refreshError?.message ?? null}
        onShowShortcuts={() => setShowShortcuts(true)}
        onOpenAuth={openAuth}
        onOpenAudit={() => setView("audit")}
      />

      <section className="band">
        <div className="band-head">
          <span className="glyph">
            <Inbox size={13} />
          </span>
          All mail
          {rows.length > 0 && <span className="count">({rows.length})</span>}
          <span className="sub">— newest first</span>
        </div>
        {error ? (
          <div className="band-empty">{error}</div>
        ) : items === null ? (
          <div className="band-empty">loading mail…</div>
        ) : rows.length === 0 ? (
          <div className="band-empty">No mail.</div>
        ) : (
          rows.map((u, i) => (
            <UpdateRow
              key={u.id}
              update={u}
              selected={i === idx}
              onSelect={hoverSelectById}
              onOpen={openUpdate}
            />
          ))
        )}
      </section>

      {showShortcuts && (
        <ShortcutsOverlay onClose={() => setShowShortcuts(false)} />
      )}
    </div>
  );
}
