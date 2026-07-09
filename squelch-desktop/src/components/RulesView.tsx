// Rules audit (`T`). Lists sender rules from GET /client/rules with their
// disposition; j/k selects, x deletes the selected rule (DELETE /client/rules/
// {id}) with a confirmation toast. Read-first: creation is out of scope for the
// read side; this is the "what's shaping my inbox" audit surface.

import { useCallback, useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api";
import type { SenderRule } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { shortDate } from "../lib/format";

export function RulesView() {
  const pushToast = useStore((s) => s.pushToast);
  const [rules, setRules] = useState<SenderRule[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [idx, setIdx] = useState(0);

  const load = useCallback(() => {
    let alive = true;
    setLoading(true);
    api
      .listRules()
      .then((r) => {
        if (alive) {
          setRules(r);
          setError(null);
        }
      })
      .catch((e) => {
        if (alive) setError(e instanceof ApiError ? e.message : "rules failed");
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => load(), [load]);

  const del = async (rule: SenderRule | undefined) => {
    if (!rule) return;
    try {
      await api.deleteRule(rule.id);
      pushToast(`deleted rule ${rule.match_pattern}`, "success");
      setRules((rs) => rs.filter((r) => r.id !== rule.id));
      setIdx((i) => Math.max(0, Math.min(i, rules.length - 2)));
    } catch (e) {
      pushToast(e instanceof ApiError ? e.message : "delete failed", "error");
    }
  };

  const bindings = useMemo(
    () => [
      { key: "j", description: "next", handler: () => setIdx((i) => Math.min(rules.length - 1, i + 1)) },
      { key: "k", description: "prev", handler: () => setIdx((i) => Math.max(0, i - 1)) },
      { key: "x", description: "delete rule", handler: () => void del(rules[idx]) },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [rules, idx],
  );
  useKeys("modal", bindings, [bindings]);

  if (loading) return <div className="side-loading">loading rules…</div>;
  if (error) return <div className="side-error">{error}</div>;
  if (rules.length === 0) return <div className="side-empty">no rules yet.</div>;

  return (
    <div>
      <div className="side-empty" style={{ marginBottom: 6 }}>
        j/k select · <kbd>x</kbd> delete
      </div>
      {rules.map((r, i) => (
        <div
          key={r.id}
          className={`rule-row num${i === idx ? " sel" : ""}`}
          style={{ background: i === idx ? "var(--bg-row-sel)" : undefined }}
          onClick={() => setIdx(i)}
        >
          <span className={`disp disp-${r.disposition}`}>{r.disposition}</span>
          <span className="pat" title={r.want_text}>
            {r.match_pattern}
          </span>
          <span style={{ color: "var(--fg-faint)" }}>{shortDate(r.updated_at)}</span>
        </div>
      ))}
    </div>
  );
}
