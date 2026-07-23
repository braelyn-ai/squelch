// AUDIT LOG view ('A'). Human review of what the AI agent (over the /mcp door)
// and this app (the /client door) have done. GET /client/audit, newest first.
// Read-only scrollback with j/k selection: the selected row expands its full
// target + detail; everything else truncates.
//
// READABLE ENTRIES: action slugs are mapped to verb phrases and, when the server
// resolved the target to a message, we render "{sender} · {subject}" (text-only —
// mail-derived, so never as HTML) in place of the raw numeric target. Sealed
// sender/subject appearing here is a deliberate accepted decision (the Auth tab
// already shows the human those); sealed CONTENT never reaches this surface.
//
// UNDO: rows whose action has a safe inverse AND whose detail marks success get a
// small undo button — archive (restore INBOX), set_status→done (reopen), and
// rule.create (delete the created rule, but only when the audit row carries its
// id). The undo itself lands as a NEW audit row, which is correct and desired.
//
// Follows the SidePanel conditional-mount contract: registers j/k into the
// existing "modal" context via useKeys — it must NOT push a second context
// (RoutedView/SidePanel already pushed "modal"; Esc is owned there).

import { useCallback, useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api";
import type { AuditEntry } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { relAge } from "../lib/format";

const INBOX = "INBOX";

// Actors we treat as "the agent" (visually distinct, accent border). The
// server-side agent door is still landing, so tolerate a few likely spellings.
const AGENT_ACTORS = /^(agent|mcp|assistant|ai)/i;
// The app's own door.
const APP_ACTORS = /^(client-api|client|app|user)$/i;

interface ActorChip {
  label: string;
  cls: string; // extra class -> CSS accent for agent vs app vs unknown
}

function actorChip(actor: string): ActorChip {
  if (AGENT_ACTORS.test(actor)) return { label: "Agent", cls: "agent" };
  if (APP_ACTORS.test(actor)) return { label: "You", cls: "app" };
  // Unknown actor: show it verbatim rather than mislabeling.
  return { label: actor || "?", cls: "other" };
}

// Map raw action slugs to a readable verb phrase. Covers both the dotted server
// slugs (rule.create) and their underscore variants (create_rule) so a slug
// rename on either side degrades gracefully. set_status is detail-driven (below).
const ACTION_VERBS: Record<string, string> = {
  archive: "archived",
  label: "relabeled a message",
  send: "sent a reply",
  reveal_sealed: "revealed auth message",
  reveal: "revealed auth message",
  unsubscribe: "opened unsubscribe",
  unsub_resolution: "resolved unsubscribe prompt",
  "rule.create": "created a sender rule",
  create_rule: "created a sender rule",
  "rule.update": "updated a sender rule",
  update_rule: "updated a sender rule",
  "rule.delete": "deleted a sender rule",
  delete_rule: "deleted a sender rule",
};

function actionVerb(e: AuditEntry): string {
  // set_status carries the new status in `detail` (done/open/new).
  if (e.action === "set_status") {
    const d = (e.detail ?? "").toLowerCase();
    if (d === "done") return "marked done";
    if (d === "open") return "reopened";
    if (d === "new") return "reset to new";
    return "changed status";
  }
  const v = ACTION_VERBS[e.action];
  if (v) return v;
  // Tolerate namespaced variants like "rule.set.v2" -> match on the prefix.
  const dot = e.action.indexOf(".");
  if (dot > 0 && e.action.slice(0, dot) === "rule") return "changed a sender rule";
  // Fall back to the raw slug (rendered in mono via .act).
  return e.action || "did something";
}

/** An available undo for a row: a human label + the exact inverse call. */
interface UndoSpec {
  label: string;
  run: () => Promise<unknown>;
}

/** Strict decimal id parse for audit targets — mirrors the server's SQLite
 *  CAST semantics. Number() alone accepts hex/exponent/whitespace forms
 *  ("0x1F" -> 31) that CAST would map to 0, so an undo could fire against a
 *  different id than the row enriched/displayed. Digits-only + safe range. */
function parseAuditId(raw: string | null | undefined): number | null {
  if (!raw || !/^\d+$/.test(raw)) return null;
  const id = Number(raw);
  return Number.isSafeInteger(id) && id > 0 ? id : null;
}

/**
 * The safe inverse for a row, or null if there isn't one. Only SUCCESSFUL rows
 * with a reversible action qualify:
 *  - archive (detail "ok")            -> re-add the INBOX label (same inverse the
 *                                        5s archive undo toast fires)
 *  - set_status -> "done"             -> set status back to "open"
 *  - rule.create carrying the rule id -> delete that rule
 */
function undoFor(e: AuditEntry): UndoSpec | null {
  if (e.action === "archive" && e.detail === "ok") {
    const id = parseAuditId(e.target);
    if (id !== null) {
      return { label: "restore", run: () => api.actionLabel(id, [INBOX], []) };
    }
  }
  if (e.action === "set_status" && e.detail === "done") {
    const id = parseAuditId(e.target);
    if (id !== null) {
      return { label: "reopen", run: () => api.setStatus(id, "open") };
    }
  }
  if (e.action === "rule.create" || e.action === "create_rule") {
    // handlers.rs stores the created rule id in `detail` (target is the pattern).
    // Only offer undo when that id is actually present + parseable.
    const id = parseAuditId(e.detail);
    if (id !== null) {
      return { label: "delete rule", run: () => api.deleteRule(id) };
    }
  }
  return null;
}

export function AuditView() {
  const pushToast = useStore((s) => s.pushToast);
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [idx, setIdx] = useState(0);

  const load = useCallback(async () => {
    const e = await api.getAudit(200);
    setEntries(e);
    setError(null);
  }, []);

  useEffect(() => {
    let alive = true;
    load()
      .catch((e) => {
        if (alive) setError(e instanceof ApiError ? e.message : "audit failed");
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [load]);

  // Newest first — sort defensively by ts (fall back to id) so we don't depend
  // on the server's ordering.
  const rows = useMemo(
    () =>
      [...entries].sort((a, b) => {
        const ta = new Date(a.ts).getTime();
        const tb = new Date(b.ts).getTime();
        if (!Number.isNaN(ta) && !Number.isNaN(tb) && ta !== tb) return tb - ta;
        return b.id - a.id;
      }),
    [entries],
  );

  // Keep selection in range as data loads.
  useEffect(() => {
    setIdx((i) => Math.max(0, Math.min(i, Math.max(0, rows.length - 1))));
  }, [rows.length]);

  const doUndo = useCallback(
    async (e: AuditEntry) => {
      const spec = undoFor(e);
      if (!spec) return;
      try {
        await spec.run();
        pushToast(`undone: ${actionVerb(e)}`, "info");
        // The undo lands as its own audit row; re-pull so it shows immediately.
        await load();
      } catch (err) {
        pushToast(err instanceof ApiError ? err.message : "undo failed", "error");
      }
    },
    [pushToast, load],
  );

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next",
        handler: () => setIdx((i) => Math.min(rows.length - 1, i + 1)),
      },
      {
        key: "k",
        description: "prev",
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
    ],
    [rows.length],
  );
  useKeys("modal", bindings, [bindings]);

  if (loading) return <div className="side-loading">loading audit…</div>;
  if (error) return <div className="side-error">{error}</div>;
  if (rows.length === 0)
    return (
      <div className="side-empty">No agent or app actions recorded yet.</div>
    );

  return (
    <div className="audit">
      {rows.map((e, i) => {
        const sel = i === idx;
        const chip = actorChip(e.actor);
        const verb = actionVerb(e);
        const undo = undoFor(e);
        // Prefer the resolved sender · subject; fall back to the raw target.
        const hasResolved = !!(e.target_sender || e.target_subject);
        return (
          <div
            key={e.id}
            className={`audit-row${sel ? " sel" : ""}`}
            onClick={() => setIdx(i)}
          >
            <span className={`actor-chip actor-${chip.cls}`}>{chip.label}</span>
            <span className="act" title={e.action}>
              {verb}
            </span>
            <span
              className="tgt"
              style={sel ? { whiteSpace: "normal" } : undefined}
              title={
                hasResolved
                  ? [e.target_sender, e.target_subject]
                      .filter(Boolean)
                      .join(" · ")
                  : (e.target ?? undefined)
              }
            >
              {hasResolved ? (
                <>
                  {e.target_sender && (
                    <span className="tgt-sender">{e.target_sender}</span>
                  )}
                  {e.target_sender && e.target_subject ? (
                    <span className="tgt-sep"> · </span>
                  ) : null}
                  {e.target_subject && (
                    <span className="tgt-subject">{e.target_subject}</span>
                  )}
                </>
              ) : (
                <>{e.target ?? ""}</>
              )}
              {sel && e.detail ? (
                <span className="detail"> — {e.detail}</span>
              ) : null}
            </span>
            {undo && (
              <button
                type="button"
                className="audit-undo"
                onClick={(ev) => {
                  ev.stopPropagation();
                  void doUndo(e);
                }}
                title={`undo — ${undo.label}`}
              >
                {undo.label}
              </button>
            )}
            <span className="ts" title={e.ts}>
              {relAge(e.ts) || "now"}
            </span>
          </div>
        );
      })}
      <div className="audit-foot">
        <kbd>j</kbd>/<kbd>k</kbd> select
      </div>
    </div>
  );
}
