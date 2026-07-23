// TRIAGE PIPELINE — the "How triage works" explainer diagram on the Settings
// page, mounted directly above the Triage budget section. A flat horizontal
// flow of small cards (no gradients, theme tokens only):
//
//   mail arrives → seal check → sender rules → stage-1 LLM → confident?
//     → stage-2 LLM → sitrep/inbox
//
// with a dashed fallback branch off stage 1 ("API/budget unavailable →
// heuristic scores") so it's clear mail never gets stuck. The stage cards show
// the LIVE model names + daily caps from GET /client/triage-config — this is
// the "what do these numbers change" legend for the budget fields below it.
// Self-contained: fetches its own config (best-effort; caps render as "—"
// until it loads) so the budget section's logic stays untouched.

import { useEffect, useState } from "react";
import { api } from "../api";
import type { TriageConfig } from "../api";

/** A single pipeline card. `accent` picks the color of the small note line:
 *  "lock" = sealed/auth purple, "brass" = the app accent, default = faint. */
function Node({
  name,
  tooltip,
  note,
  accent,
  caps,
}: {
  name: string;
  tooltip: string;
  note?: string;
  accent?: "lock" | "brass";
  caps?: string;
}) {
  return (
    <div
      className={`tp-node${accent === "lock" ? " lock" : ""}`}
      title={tooltip}
    >
      <div className="tp-name">{name}</div>
      {note && (
        <div className={`tp-note${accent === "lock" ? " lock" : ""}`}>
          {note}
        </div>
      )}
      {caps && <div className="tp-caps">{caps}</div>}
    </div>
  );
}

function Arrow() {
  return (
    <span className="tp-arrow" aria-hidden="true">
      {"→"}
    </span>
  );
}

export function TriagePipelineSection() {
  const [cfg, setCfg] = useState<TriageConfig | null>(null);
  useEffect(() => {
    let alive = true;
    api
      .getTriageConfig()
      .then((c) => alive && setCfg(c))
      .catch(() => {
        /* diagram is informational; caps just stay "—" */
      });
    return () => {
      alive = false;
    };
  }, []);

  const s1Model = cfg?.stage1.model ?? "—";
  const s1Cap = cfg ? String(cfg.stage1.global_daily_cap) : "—";
  const s2Model = cfg?.stage2_model ?? "—";
  const s2Thread = cfg ? String(cfg.thread_daily_cap) : "—";
  const s2Sender = cfg ? String(cfg.sender_daily_cap) : "—";
  const s2Global = cfg ? String(cfg.global_daily_cap) : "—";

  return (
    <section className="set-section">
      <div className="set-label">How triage works</div>
      <div className="tp-flow">
        <Node
          name="mail arrives"
          tooltip="Every new message from the mail server enters this pipeline the moment it is synced."
        />
        <Arrow />
        <Node
          name="seal check"
          accent="lock"
          note="deterministic — auth codes never reach a model"
          tooltip="Verification codes and magic links are sealed by fixed pattern rules before any model can see them."
        />
        <Arrow />
        <Node
          name="sender rules"
          note="explicit rules decide, no LLM"
          tooltip="A matching rule you wrote for a sender or pattern decides the disposition without any model — a Surface/Squelch rule settles the email immediately, while a Filtered rule skips stage 1 but still continues to stage 2."
        />
        <Arrow />
        <div className="tp-stage1-col">
          <Node
            name={`stage 1 · ${s1Model}`}
            accent="brass"
            note="every email"
            caps={`cap ${s1Cap}/day global`}
            tooltip="A small model scores every remaining email; the global daily cap below bounds how many calls it may make."
          />
          <div
            className="tp-branch"
            title="If the API is unreachable or the daily budget is spent, deterministic heuristic scores take over so no mail ever waits on a model."
          >
            API/budget unavailable {"→"} heuristic scores
          </div>
        </div>
        <Arrow />
        <Node
          name="confident?"
          note="sure scores are final; unsure ones escalate"
          tooltip="High-confidence stage-1 scores are accepted as-is; only uncertain emails are escalated to stage 2."
        />
        <Arrow />
        <Node
          name={`stage 2 · ${s2Model}`}
          accent="brass"
          note="escalations only"
          caps={`caps ${s2Thread}/thread · ${s2Sender}/sender · ${s2Global}/day`}
          tooltip="A more capable model re-reads only the uncertain emails, bounded by the per-thread, per-sender, and global daily caps below."
        />
        <Arrow />
        <Node
          name="sitrep / inbox"
          tooltip="The final score routes each email into your sitrep, inbox, or the quiet zones."
        />
      </div>
    </section>
  );
}
