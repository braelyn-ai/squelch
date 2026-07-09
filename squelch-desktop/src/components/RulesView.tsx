// RULES MANAGEMENT (`T`). The full sender-rule surface — and the audit view for
// what the user's AI agent has written over MCP. Lists every rule from
// GET /client/rules as a dense table: match pattern, disposition chip, want_text
// (truncated; full on selection), a client-side match count against the
// currently-loaded updates (0 = a likely-dead rule, rendered dim), and a
// relative updated-at.
//
// Keys: j/k select · n new (blank editor) · Enter/e edit (create-new+delete-old,
// see RuleEditor) · x delete (undo-first: the 5s toast recreates the rule).
// Refreshes on open and after every mutation.

import { useCallback, useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api";
import type { SenderRule } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { relAge } from "../lib/format";
import { openRuleEditorRequest } from "./ruleEditorBus";
import { DISPOSITION_LABEL } from "./RuleEditor";

export function RulesView() {
  const pushToast = useStore((s) => s.pushToast);
  const pushUndo = useStore((s) => s.pushUndo);
  const sitrep = useStore((s) => s.sitrep);

  const [rules, setRules] = useState<SenderRule[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [idx, setIdx] = useState(0);

  // Client-side match counts: how many currently-loaded updates each rule
  // matched, keyed by rule id. No new endpoint — we read the store's updates.
  const matchCounts = useMemo(() => {
    const counts = new Map<number, number>();
    for (const u of [...sitrep.standing, ...sitrep.new, ...sitrep.open]) {
      if (u.matched_rule != null) {
        counts.set(u.matched_rule, (counts.get(u.matched_rule) ?? 0) + 1);
      }
    }
    return counts;
  }, [sitrep]);

  const load = useCallback(() => {
    setLoading(true);
    api
      .listRules()
      .then((r) => {
        setRules(r);
        setError(null);
      })
      .catch((e) => {
        setError(e instanceof ApiError ? e.message : "rules failed");
      })
      .finally(() => setLoading(false));
  }, []);

  // Fetch on open and keep the selection index in range as the list changes.
  useEffect(() => load(), [load]);
  useEffect(() => {
    setIdx((i) => Math.max(0, Math.min(i, Math.max(0, rules.length - 1))));
  }, [rules.length]);

  const create = () => {
    openRuleEditorRequest({ rule: null, onSaved: () => load() });
  };

  const edit = (rule: SenderRule | undefined) => {
    if (!rule) return;
    openRuleEditorRequest({ rule, onSaved: () => load() });
  };

  const del = async (rule: SenderRule | undefined) => {
    if (!rule) return;
    try {
      await api.deleteRule(rule.id);
      // Optimistic removal; re-fetch happens on undo or next open.
      setRules((rs) => rs.filter((r) => r.id !== rule.id));
      // Undo-first: the 5s toast recreates the rule from its cached values.
      pushUndo({
        kind: "rule_delete",
        messageId: rule.id,
        label: `deleted rule ${rule.match_pattern}`,
        revert: async () => {
          await api.createRule({
            match_pattern: rule.match_pattern,
            want: rule.want_text,
            disposition: rule.disposition,
          });
          load();
        },
      });
    } catch (e) {
      pushToast(e instanceof ApiError ? e.message : "delete failed", "error");
    }
  };

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next",
        handler: () => setIdx((i) => Math.min(rules.length - 1, i + 1)),
      },
      {
        key: "k",
        description: "prev",
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
      { key: "n", description: "new rule", handler: () => create() },
      { key: "e", description: "edit rule", handler: () => edit(rules[idx]) },
      { key: "Enter", description: "edit rule", handler: () => edit(rules[idx]) },
      { key: "x", description: "delete rule", handler: () => void del(rules[idx]) },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [rules, idx],
  );
  useKeys("modal", bindings, [bindings]);

  if (loading && rules.length === 0)
    return <div className="side-loading">loading rules…</div>;
  if (error) return <div className="side-error">{error}</div>;
  if (rules.length === 0)
    return (
      <div className="side-empty">
        no rules yet — press <kbd>n</kbd> to create one, or <kbd>t</kbd> on any
        message.
      </div>
    );

  return (
    <div className="rules">
      {rules.map((r, i) => {
        const sel = i === idx;
        const count = matchCounts.get(r.id) ?? 0;
        return (
          <div
            key={r.id}
            className={`rule-row${sel ? " sel" : ""}`}
            onClick={() => setIdx(i)}
          >
            <span className={`disp disp-${r.disposition}`}>
              {DISPOSITION_LABEL[r.disposition]}
            </span>
            <span className="pat mono">{r.match_pattern}</span>
            <span
              className="want"
              style={sel ? { whiteSpace: "normal" } : undefined}
              title={r.want_text}
            >
              {r.want_text || <span className="want-empty">—</span>}
            </span>
            <span
              className={`matches${count === 0 ? " dead" : ""}`}
              title={
                count === 0
                  ? "no currently-loaded updates match this rule"
                  : `${count} loaded update(s) matched`
              }
            >
              {count}×
            </span>
            <span className="when">{relAge(r.updated_at) || "—"}</span>
          </div>
        );
      })}
      <div className="rules-foot">
        <kbd>j</kbd>/<kbd>k</kbd> select · <kbd>n</kbd> new · <kbd>e</kbd>/
        <kbd>↵</kbd> edit · <kbd>x</kbd> delete
      </div>
    </div>
  );
}
