// Audit log side view. GET /client/audit — the append-only trail of actions
// (incl. sealed reveals). Read-only, newest first. No keys beyond the Esc that
// SideViews owns; it's a scrollback.

import { useEffect, useState } from "react";
import { api, ApiError } from "../api";
import type { AuditEntry } from "../api";
import { dateTime } from "../lib/format";

export function AuditView() {
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

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

  if (loading) return <div className="side-loading">loading audit…</div>;
  if (error) return <div className="side-error">{error}</div>;
  if (entries.length === 0) return <div className="side-empty">no audit entries.</div>;

  return (
    <div>
      {entries.map((e) => (
        <div key={e.id} className="audit-row num">
          <span className="ts">{dateTime(e.ts)}</span>
          <span className="actor">{e.actor}</span>
          <span className="act">{e.action}</span>
          <span className="tgt" title={e.detail ?? undefined}>
            {e.target ?? ""}
          </span>
        </div>
      ))}
    </div>
  );
}
