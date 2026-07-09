// AUDIT LOG side view ('A'). Human review of what the AI agent (over the /mcp
// door) and this app (the /client door) have done. GET /client/audit, newest
// first. Read-only scrollback with j/k selection: the selected row expands its
// full target + detail; everything else truncates.
//
// Two action sources coexist and must both render gracefully:
//  - existing app rows from actor "client-api" (reveal / archive / label / …)
//  - new agent rows from actor "agent" (rule.set / rule.create / … ), added
//    server-side in parallel.
// Unknown actors/actions fall back to their raw string rather than breaking.
//
// Follows the SidePanel conditional-mount contract: registers j/k into the
// existing "modal" context via useKeys — it must NOT push a second context
// (SideViews' SidePanel already pushed "modal"; Esc is owned there).

import { useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api";
import type { AuditEntry } from "../api";
import { useKeys } from "../keys";
import { relAge } from "../lib/format";

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

// Map raw action strings to a readable verb phrase. Covers the known agent
// actions (rule.*) and existing app actions (reveal / archive / label / send),
// with a graceful fallback to the raw action for anything unrecognized.
const ACTION_VERBS: Record<string, string> = {
  "rule.set": "set a sender rule",
  "rule.create": "created a sender rule",
  "rule.update": "updated a sender rule",
  "rule.delete": "deleted a sender rule",
  archive: "archived a message",
  label: "relabeled a message",
  send: "sent a reply",
  reveal: "revealed a sealed message",
  status: "changed a message's status",
};

function actionVerb(action: string): string {
  if (ACTION_VERBS[action]) return ACTION_VERBS[action];
  // Tolerate namespaced variants like "rule.set.v2" -> match on the prefix.
  const dot = action.indexOf(".");
  if (dot > 0) {
    const head = action.slice(0, dot);
    const generic: Record<string, string> = {
      rule: "changed a sender rule",
      message: "acted on a message",
    };
    if (generic[head]) return generic[head];
  }
  return action || "did something";
}

export function AuditView() {
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [idx, setIdx] = useState(0);

  useEffect(() => {
    let alive = true;
    api
      .getAudit(200)
      .then((e) => {
        if (alive) {
          setEntries(e);
          setError(null);
        }
      })
      .catch((e) => {
        if (alive) setError(e instanceof ApiError ? e.message : "audit failed");
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, []);

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
        const verb = actionVerb(e.action);
        return (
          <div
            key={e.id}
            className={`audit-row num${sel ? " sel" : ""}`}
            onClick={() => setIdx(i)}
          >
            <span className={`actor-chip actor-${chip.cls}`}>{chip.label}</span>
            <span className="act" title={e.action}>
              {verb}
            </span>
            <span
              className="tgt"
              style={sel ? { whiteSpace: "normal" } : undefined}
              title={e.target ?? undefined}
            >
              {e.target ?? ""}
              {sel && e.detail ? (
                <span className="detail"> — {e.detail}</span>
              ) : null}
            </span>
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
