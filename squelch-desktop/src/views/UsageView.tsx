// USAGE — a routed main view (bottom rail group). Server-side triage spend plus
// the client-side assistant tally:
//
//   TRIAGE    — the server's triage spend (GET /client/usage). The triage
//               pipeline reports its stages as separate ledger categories
//               (stage-1 runs on every email, stage-2 on escalations); each is
//               rendered as its own section — a compact daily breakdown with a
//               hand-rolled inline bar per day (divs, no chart lib; brass fill,
//               sparingly), plus totals + the model label. The view loops over
//               whatever categories arrive, falling back to the flat single-
//               category shape older servers send.
//   ASSISTANT — the embedded BYOK ⌘K assistant's spend, tracked client-side in
//               localStorage (its calls go straight from this machine to your
//               own provider key; the server never sees them).
//
// Precision-instrument styled: engraved uppercase section labels, Plex Mono for
// all numbers, brass reserved for the bars + the connected accents.

import { useEffect, useState } from "react";
import { api } from "../api";
import type { UsageResponse, UsageRow, UsageTotals } from "../api";
import { getAssistantUsage, type AssistantUsage } from "../api/assistant/usage";
import "../styles/settings.css";

/** Rough $/1M rates for the assistant models, for a local cost estimate. */
const ASSISTANT_RATES: Record<string, { in: number; out: number }> = {
  "claude-haiku-4-5": { in: 1, out: 5 },
  "claude-opus-4-8": { in: 5, out: 25 },
};

function estAssistantCost(u: AssistantUsage): number {
  const r =
    ASSISTANT_RATES[u.lastModel ?? ""] ?? ASSISTANT_RATES["claude-haiku-4-5"];
  return (
    (u.inputTokens / 1_000_000) * r.in + (u.outputTokens / 1_000_000) * r.out
  );
}

/** Compact integer formatting for token counts (e.g. 1.2M, 34.0k, 812). */
function fmtNum(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

function fmtCost(n: number): string {
  return `$${n.toFixed(n < 1 ? 4 : 2)}`;
}

/** A category resolved for rendering: the wire map's key promoted to `id` plus a
 *  derived display `label`, alongside the entry's rows/totals/model/provider. */
interface ResolvedCategory {
  id: string;
  label: string;
  rows: UsageRow[];
  totals: UsageTotals;
  provider: string | null;
  model: string;
}

/** Human display name derived from a category id (the wire carries no label). */
function categoryLabel(id: string): string {
  switch (id) {
    case "stage1":
      return "Stage 1 — every email";
    case "stage2":
      return "Stage 2 — escalations";
    default:
      return id;
  }
}

/** One-line description under a category header. */
function categorySub(id: string): string {
  switch (id) {
    case "stage1":
      return "Server-side stage-1 classification (runs on every email; your squelch server's key).";
    case "stage2":
      return "Server-side stage-2 classification (escalations; your squelch server's key).";
    default:
      return "Server-side classification spend (your squelch server's key).";
  }
}

/**
 * Normalize a usage response into a list of categories to render. Prefers the
 * per-stage `categories` breakdown — an OBJECT MAP keyed by id, so we
 * Object.entries() it and promote each key to `id` — and falls back to the flat
 * single-category shape older servers send (surfaced as one "Triage" category).
 */
function categoriesOf(u: UsageResponse): ResolvedCategory[] {
  if (u.categories && Object.keys(u.categories).length > 0) {
    return Object.entries(u.categories).map(([id, c]) => ({
      id,
      label: categoryLabel(id),
      rows: c.rows,
      totals: c.totals,
      provider: c.provider ?? null,
      model: c.model,
    }));
  }
  return [
    {
      id: "triage",
      label: "Triage",
      rows: u.rows,
      totals: u.totals,
      provider: u.provider,
      model: u.model,
    },
  ];
}

export function UsageView() {
  const [usage, setUsage] = useState<UsageResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let alive = true;
    // Last 30 days of daily rows.
    api
      .getUsage(30)
      .then((u) => {
        if (alive) {
          setUsage(u);
          setLoading(false);
        }
      })
      .catch((e) => {
        if (alive) {
          setError(e instanceof Error ? e.message : "failed to load usage");
          setLoading(false);
        }
      });
    return () => {
      alive = false;
    };
  }, []);

  // Client-side assistant tally (localStorage). Read once on mount — it only
  // changes when an ask completes, and the view remounts on nav.
  const [asst, setAsst] = useState<AssistantUsage | null>(null);
  useEffect(() => {
    setAsst(getAssistantUsage());
  }, []);

  const categories = usage ? categoriesOf(usage) : [];

  return (
    <div className="routed-view">
      <header className="routed-head">
        <h2>Usage</h2>
      </header>
      <div className="routed-body usage">
        {/* TRIAGE (server-side, one section per ledger category) -------- */}
        {loading && (
          <section className="set-section">
            <div className="usage-cat-head">
              <span className="set-label">Triage</span>
            </div>
            <div className="usage-empty">loading…</div>
          </section>
        )}
        {!loading && error && (
          <section className="set-section">
            <div className="usage-cat-head">
              <span className="set-label">Triage</span>
            </div>
            <div className="usage-empty err">{error}</div>
          </section>
        )}
        {!loading &&
          !error &&
          categories.map((c) => (
            <UsageCategorySection key={c.id} cat={c} />
          ))}

        {/* ASSISTANT ---------------------------------------------------- */}
        <section className="set-section">
          <div className="usage-cat-head">
            <span className="set-label">Assistant</span>
            {asst?.lastModel && (
              <span className="usage-model mono">{asst.lastModel}</span>
            )}
          </div>
          <p className="usage-sub">
            The ⌘K "ask your inbox" assistant (your own key, tracked on this
            machine).
          </p>
          {(!asst || asst.asks === 0) && (
            <div className="usage-empty">
              No assistant usage yet. Press ⌘K to ask your inbox a question.
            </div>
          )}
          {asst && asst.asks > 0 && (
            <div className="usage-totals">
              <div className="usage-stat">
                <span className="k">asks</span>
                <span className="v mono">{asst.asks}</span>
              </div>
              <div className="usage-stat">
                <span className="k">in + out tokens</span>
                <span className="v mono">
                  {fmtNum(asst.inputTokens + asst.outputTokens)}
                </span>
              </div>
              <div className="usage-stat">
                <span className="k">est cost</span>
                <span className="v mono brass">
                  {fmtCost(estAssistantCost(asst))}
                </span>
              </div>
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

/** One server-side ledger category: header + totals + per-day bar table. */
function UsageCategorySection({ cat }: { cat: ResolvedCategory }) {
  const rows = cat.rows;
  const label = cat.label;
  // Per-day total tokens drive the bar width; scale to the busiest day.
  const dayTotals = rows.map((r) => r.input_tokens + r.output_tokens);
  const peak = Math.max(1, ...dayTotals);

  return (
    <section className="set-section">
      <div className="usage-cat-head">
        <span className="set-label">{label}</span>
        <span className="usage-model mono">
          {cat.model}
          {cat.provider ? ` · ${cat.provider}` : ""}
        </span>
      </div>
      <p className="usage-sub">{categorySub(cat.id)}</p>

      {rows.length === 0 ? (
        <div className="usage-empty">No {label.toLowerCase()} usage yet.</div>
      ) : (
        <>
          <div className="usage-totals">
            <div className="usage-stat">
              <span className="k">calls</span>
              <span className="v mono">{cat.totals.calls}</span>
            </div>
            <div className="usage-stat">
              <span className="k">in + out tokens</span>
              <span className="v mono">
                {fmtNum(cat.totals.input_tokens + cat.totals.output_tokens)}
              </span>
            </div>
            <div className="usage-stat">
              <span className="k">est cost</span>
              <span className="v mono brass">
                {fmtCost(cat.totals.est_cost_usd)}
              </span>
            </div>
          </div>

          <table className="usage-table">
            <thead>
              <tr>
                <th>day</th>
                <th className="num">calls</th>
                <th className="num">in</th>
                <th className="num">out</th>
                <th className="bar-col">tokens</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r, i) => {
                const total = dayTotals[i];
                const pct = Math.round((total / peak) * 100);
                return (
                  <tr key={r.day}>
                    <td className="mono day">{r.day}</td>
                    <td className="num mono">{r.calls}</td>
                    <td className="num mono">{fmtNum(r.input_tokens)}</td>
                    <td className="num mono">{fmtNum(r.output_tokens)}</td>
                    <td className="bar-col">
                      <div
                        className="usage-bar"
                        style={{ width: `${pct}%` }}
                        title={`${total} tokens`}
                        aria-hidden="true"
                      />
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </>
      )}
    </section>
  );
}
