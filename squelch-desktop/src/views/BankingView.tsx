// BANKING view — statements & transaction alerts, GET /client/banking, newest
// first. Dense record language (same as the Sitrep receipts rail): institution
// (fallback "bank"), a quiet lowercase kind tag ("statement" / "alert"), the
// masked account hint when present, the amount right-aligned in mono (— when
// null; a statement shows the TOTAL balance the server extracted), and a
// relative age. Human door only — a sealed message can never produce a row here.
//
// Follows the SidePanel/RoutedView conditional-mount contract: registers j/k +
// Enter into the existing "modal" context via useKeys — it must NOT push a
// second context (RoutedView already pushed "modal"; Esc is owned there).
// Clicking or Enter opens the underlying email via viewInEmails.

import { useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api";
import type { BankingRecord } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { relAge } from "../lib/format";

/** Quiet lowercase tag for the kind column. */
const KIND_TAG: Record<BankingRecord["kind"], string> = {
  statement: "statement",
  transaction_alert: "alert",
};

/**
 * Format a banking amount. Uses the record's currency when present (falls back to
 * USD), two decimals, tabular. A null amount renders "—".
 */
function bankingAmount(r: BankingRecord): string {
  if (r.amount === null || r.amount === undefined || Number.isNaN(r.amount)) {
    return "—";
  }
  const currency = r.currency ?? "USD";
  try {
    return r.amount.toLocaleString("en-US", {
      style: "currency",
      currency,
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    });
  } catch {
    // Unknown/invalid currency code → plain 2-decimal with a $ fallback.
    return `$${r.amount.toLocaleString("en-US", {
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    })}`;
  }
}

export function BankingView() {
  const viewInEmails = useStore((s) => s.viewInEmails);
  const [rows, setRows] = useState<BankingRecord[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [idx, setIdx] = useState(0);

  useEffect(() => {
    let alive = true;
    api
      .getBanking()
      .then((r) => alive && setRows(r))
      .catch((e) => {
        if (alive) {
          setError(e instanceof ApiError ? e.message : "banking failed");
          setRows([]);
        }
      });
    return () => {
      alive = false;
    };
  }, []);

  const list = rows ?? [];

  // Keep selection in range as data loads.
  useEffect(() => {
    setIdx((i) => Math.max(0, Math.min(i, Math.max(0, list.length - 1))));
  }, [list.length]);

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next",
        handler: () => setIdx((i) => Math.min(list.length - 1, i + 1)),
      },
      {
        key: "k",
        description: "prev",
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
      {
        key: "Enter",
        description: "open email",
        handler: () => {
          const r = list[idx];
          if (r) viewInEmails(r.message_id);
        },
      },
    ],
    [list, idx, viewInEmails],
  );
  useKeys("modal", bindings, [bindings]);

  if (rows === null) return <div className="side-loading">loading banking…</div>;
  if (error && list.length === 0)
    return <div className="side-error">{error}</div>;
  if (list.length === 0)
    return <div className="side-empty">No statements or alerts.</div>;

  return (
    <div className="banking">
      {list.map((r, i) => {
        const sel = i === idx;
        return (
          <button
            type="button"
            key={r.id}
            className={`banking-row${sel ? " sel" : ""}`}
            onClick={() => {
              setIdx(i);
              viewInEmails(r.message_id);
            }}
            title="view this email"
          >
            <span className="bank-inst" title={r.institution ?? undefined}>
              {r.institution ?? "bank"}
            </span>
            <span className={`bank-kind ${r.kind}`}>{KIND_TAG[r.kind]}</span>
            {r.account_hint && (
              <span className="bank-acct">{r.account_hint}</span>
            )}
            <span className="bank-amount">{bankingAmount(r)}</span>
            <span className="bank-age" title={r.received_at}>
              {relAge(r.received_at) || "now"}
            </span>
          </button>
        );
      })}
      <div className="banking-foot">
        <kbd>j</kbd>/<kbd>k</kbd> select · <kbd>Enter</kbd> open
      </div>
    </div>
  );
}
