// USAGE — a routed main view (bottom rail group). Two clearly-labeled
// categories, structured for two sources now, only Triage populated:
//
//   TRIAGE    — the server-side stage-2 spend (GET /client/usage). A compact
//               daily breakdown with a hand-rolled inline bar per day (divs, no
//               chart lib; brass fill, sparingly), plus totals + the model label.
//   ASSISTANT — an explicit placeholder for the future embedded BYOK search
//               agent. No invented numbers.
//
// Precision-instrument styled: engraved uppercase section labels, Plex Mono for
// all numbers, brass reserved for the bars + the connected accents.

import { useEffect, useState } from "react";
import { api } from "../api";
import type { UsageResponse } from "../api";
import "../styles/settings.css";

/** Compact integer formatting for token counts (e.g. 1.2M, 34.0k, 812). */
function fmtNum(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

function fmtCost(n: number): string {
  return `$${n.toFixed(n < 1 ? 4 : 2)}`;
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

  const rows = usage?.rows ?? [];
  // Per-day total tokens drive the bar width; scale to the busiest day.
  const dayTotals = rows.map((r) => r.input_tokens + r.output_tokens);
  const peak = Math.max(1, ...dayTotals);

  return (
    <div className="routed-view">
      <header className="routed-head">
        <h2>Usage</h2>
      </header>
      <div className="routed-body usage">
        {/* TRIAGE ------------------------------------------------------- */}
        <section className="set-section">
          <div className="usage-cat-head">
            <span className="set-label">Triage</span>
            {usage && (
              <span className="usage-model mono">
                {usage.model}
                {usage.provider ? ` · ${usage.provider}` : ""}
              </span>
            )}
          </div>
          <p className="usage-sub">
            Server-side stage-2 classification spend (your squelch server's key).
          </p>

          {loading && <div className="usage-empty">loading…</div>}
          {!loading && error && <div className="usage-empty err">{error}</div>}
          {!loading && !error && rows.length === 0 && (
            <div className="usage-empty">No triage usage yet.</div>
          )}

          {!loading && !error && rows.length > 0 && usage && (
            <>
              <div className="usage-totals">
                <div className="usage-stat">
                  <span className="k">calls</span>
                  <span className="v mono">{usage.totals.calls}</span>
                </div>
                <div className="usage-stat">
                  <span className="k">in + out tokens</span>
                  <span className="v mono">
                    {fmtNum(
                      usage.totals.input_tokens + usage.totals.output_tokens,
                    )}
                  </span>
                </div>
                <div className="usage-stat">
                  <span className="k">est cost</span>
                  <span className="v mono brass">
                    {fmtCost(usage.totals.est_cost_usd)}
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

        {/* ASSISTANT (placeholder) -------------------------------------- */}
        <section className="set-section">
          <span className="set-label">Assistant</span>
          <div className="usage-placeholder">
            The embedded assistant isn't built yet. When it is, its usage (your
            own API key, tracked locally) will appear here.
          </div>
        </section>
      </div>
    </div>
  );
}
