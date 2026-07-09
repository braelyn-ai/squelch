// SITREP VIEW — the fully-abstracted dashboard. THE DEFAULT SURFACE ON LAUNCH.
//
// ZERO individual email rows: this is the situation report, not the mailbox.
// Four soft-card zones, light/dark aware:
//   a. OBLIGATIONS — deadline-centric cards from band=standing (avatar + sender,
//      amount + due date, past-due loud). Actions: done (d / button), view
//      (hands off to the Emails view with the item selected).
//   b. ATTENTION — aggregate only: "N new since <relative last check>" +
//      deduped sender chips (from band=new). Click → Emails.
//   c. AGING — band=open filtered age>7d: "N items sitting over a week" + per
//      item sender + duration only (no subjects — abstraction). Click → Emails.
//   d. STATUS STRIP — auth chip (→ Auth), last sync/check, today's triage cost,
//      rules count.
//
// Minimal keymap in its own "sitrep" KeyContext: j/k move between obligation
// cards, d marks the focused obligation done, Enter/v views it in Emails. The
// global 1..5 nav (App) works here too. Everything drilling into an actual
// email is deliberately absent — the "view" hand-off is the escape hatch.

import { useEffect, useMemo, useState } from "react";
import {
  KeyRound,
  Clock,
  SlidersHorizontal,
  Check,
  ArrowUpRight,
  TriangleAlert,
  Bell,
  Hourglass,
  Receipt,
} from "lucide-react";
import { api, ApiError } from "../api";
import type { AttentionUpdate } from "../api";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { deadlineChip, lastChecked, loudAge, relAge } from "../lib/format";
import { senderDisplayName } from "../lib/avatar";
import { Avatar } from "../components/Avatar";
import { dispatchDone } from "../lib/dispatch";
import "../styles/sitrep-dash.css";

// Aging threshold for zone (c): only items sitting longer than a week.
const AGING_DAYS = 7;
const AGING_MS = AGING_DAYS * 86_400_000;

/** Whole ms since an ISO stamp, or 0 if missing/invalid/future. */
function ageMs(iso: string | null | undefined): number {
  if (!iso) return 0;
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return 0;
  const d = Date.now() - t;
  return d > 0 ? d : 0;
}

/** Best-effort money amount pulled from an update's one_line (e.g. "$142.00"). */
function amountFrom(u: AttentionUpdate): string | null {
  const m = u.one_line.match(/\$\s?[\d,]+(?:\.\d{2})?/);
  return m ? m[0].replace(/\s/, "") : null;
}

export function SitrepView() {
  const setView = useStore((s) => s.setView);
  const viewInEmails = useStore((s) => s.viewInEmails);

  return (
    <div className="sitrep-dash">
      <header className="dash-header">
        <span className="brand">
          squelch <span className="dim">· sitrep</span>
        </span>
        <span className="dash-sub">the situation, abstracted</span>
      </header>

      <SitrepBody onView={viewInEmails} onGoto={setView} />
    </div>
  );
}

function SitrepBody({
  onView,
  onGoto,
}: {
  onView: (id: number) => void;
  onGoto: (v: "emails" | "auth" | "rules") => void;
}) {
  const sitrep = useStore((s) => s.sitrep);
  const lastRefresh = useStore((s) => s.lastRefresh);
  const { standing, new: fresh, open, stats, sealed } = sitrep;

  // --- rules count (cheap, lazily fetched once) -----------------------------
  const [rulesCount, setRulesCount] = useState<number | null>(null);
  useEffect(() => {
    let alive = true;
    api
      .listRules()
      .then((r) => alive && setRulesCount(r.length))
      .catch((e) => {
        // Non-fatal: just omit the chip. Never surface the token/url.
        if (alive && !(e instanceof ApiError)) setRulesCount(null);
      });
    return () => {
      alive = false;
    };
  }, []);

  // --- zone (c) aging items: open, sitting > a week -------------------------
  const aging = useMemo(
    () =>
      open
        .filter((u) => ageMs(u.surfaced_at ?? u.resolved_at) > AGING_MS)
        .sort(
          (a, b) =>
            ageMs(b.surfaced_at ?? b.resolved_at) -
            ageMs(a.surfaced_at ?? a.resolved_at),
        ),
    [open],
  );

  // --- zone (a) obligation keymap: j/k across cards, d done, Enter/v view ---
  const [obIdx, setObIdx] = useState(0);
  useEffect(() => {
    setObIdx((i) => Math.max(0, Math.min(i, Math.max(0, standing.length - 1))));
  }, [standing.length]);

  useKeyContext("sitrep");
  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next obligation",
        handler: () => setObIdx((i) => Math.min(standing.length - 1, i + 1)),
      },
      {
        key: "k",
        description: "prev obligation",
        handler: () => setObIdx((i) => Math.max(0, i - 1)),
      },
      {
        key: "d",
        description: "mark done",
        handler: () => {
          const u = standing[obIdx];
          if (u) void dispatchDone(u);
        },
      },
      {
        key: "Enter",
        description: "view in Emails",
        handler: () => {
          const u = standing[obIdx];
          if (u) onView(u.id);
        },
      },
      {
        key: "v",
        description: "view in Emails",
        handler: () => {
          const u = standing[obIdx];
          if (u) onView(u.id);
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [standing, obIdx],
  );
  useKeys("sitrep", bindings, [bindings]);

  return (
    <div className="dash-zones">
      {/* ---- (a) OBLIGATIONS ---- */}
      <section className="zone zone-obligations">
        <div className="zone-head">
          <span className="glyph">
            <TriangleAlert size={15} />
          </span>
          <h2>Obligations</h2>
          <span className="zone-count">{standing.length}</span>
          <span className="zone-sub">deadlines · immune to time</span>
        </div>
        {standing.length === 0 ? (
          <p className="zone-empty">Nothing standing — clear board.</p>
        ) : (
          <div className="ob-grid">
            {standing.map((u, i) => (
              <ObligationCard
                key={u.id}
                update={u}
                focused={i === obIdx}
                onFocus={() => setObIdx(i)}
                onDone={() => void dispatchDone(u)}
                onView={() => onView(u.id)}
              />
            ))}
          </div>
        )}
      </section>

      {/* ---- (b) ATTENTION ---- */}
      <section
        className={`zone zone-attention${fresh.length ? " clickable" : ""}`}
        onClick={fresh.length ? () => onGoto("emails") : undefined}
        role={fresh.length ? "button" : undefined}
        tabIndex={fresh.length ? -1 : undefined}
      >
        <div className="zone-head">
          <span className="glyph">
            <Bell size={15} />
          </span>
          <h2>Attention</h2>
          {fresh.length > 0 && <ArrowUpRight size={14} className="goto-hint" />}
        </div>
        {fresh.length === 0 ? (
          <p className="zone-empty">Nothing new since anyone last looked.</p>
        ) : (
          <>
            <p className="attn-lead">
              <b>{fresh.length}</b> new since{" "}
              {lastChecked(stats?.last_surfaced_at)}
            </p>
            <SenderChips items={fresh} />
          </>
        )}
      </section>

      {/* ---- (c) AGING ---- */}
      <section className="zone zone-aging">
        <div className="zone-head">
          <span className="glyph">
            <Hourglass size={15} />
          </span>
          <h2>Aging</h2>
          {aging.length > 0 && <span className="zone-count">{aging.length}</span>}
        </div>
        {aging.length === 0 ? (
          <p className="zone-empty">Nothing left rotting — nice.</p>
        ) : (
          <>
            <p className="aging-lead">
              <b>{aging.length}</b>{" "}
              {aging.length === 1 ? "item" : "items"} sitting over a week
            </p>
            <div className="aging-list">
              {aging.map((u) => (
                <button
                  key={u.id}
                  type="button"
                  className="aging-row"
                  onClick={() => onView(u.id)}
                  title="view in Emails"
                >
                  <Avatar sender={u.sender} size={20} />
                  <span className="sender">{senderDisplayName(u.sender)}</span>
                  <span className="dur">
                    {loudAge(u.surfaced_at ?? u.resolved_at).toLowerCase()}
                  </span>
                </button>
              ))}
            </div>
          </>
        )}
      </section>

      {/* ---- (d) STATUS STRIP ---- */}
      <StatusStrip
        authCount={sealed.length}
        lastCheckIso={stats?.last_surfaced_at}
        lastRefresh={lastRefresh}
        costUsd={stats?.stage2?.est_cost_usd_today}
        rulesCount={rulesCount}
        onAuth={() => onGoto("auth")}
        onRules={() => onGoto("rules")}
      />
    </div>
  );
}

// ---- zone (a): a single obligation card ------------------------------------

function ObligationCard({
  update: u,
  focused,
  onFocus,
  onDone,
  onView,
}: {
  update: AttentionUpdate;
  focused: boolean;
  onFocus: () => void;
  onDone: () => void;
  onView: () => void;
}) {
  const chip = deadlineChip(u.deadline);
  const overdue = chip?.overdue ?? false;
  const amount = amountFrom(u);

  return (
    <div
      className={`ob-card${focused ? " focused" : ""}${overdue ? " overdue" : ""}`}
      onClick={onFocus}
      role="button"
      tabIndex={-1}
    >
      <div className="ob-top">
        <Avatar sender={u.sender} size={26} />
        <span className="ob-sender" title={u.sender}>
          {senderDisplayName(u.sender)}
        </span>
        {amount && (
          <span className="ob-amount">
            <Receipt size={13} /> {amount}
          </span>
        )}
      </div>

      {/* If we couldn't extract an amount, the one_line carries the meaning —
          it's abstracted (a digest line), not the raw email. */}
      {!amount && <p className="ob-line" title={u.one_line}>{u.one_line}</p>}

      <div className="ob-bottom">
        {chip ? (
          <span className={`chip ${overdue ? "overdue" : "upcoming"}`}>
            {chip.text}
          </span>
        ) : (
          <span className="ob-nodate">no due date</span>
        )}
        <span className="ob-actions">
          <button
            type="button"
            className="ob-btn"
            onClick={(e) => {
              e.stopPropagation();
              onDone();
            }}
            title="mark done (d)"
          >
            <Check size={14} /> done
          </button>
          <button
            type="button"
            className="ob-btn"
            onClick={(e) => {
              e.stopPropagation();
              onView();
            }}
            title="view in Emails"
          >
            <ArrowUpRight size={14} /> view
          </button>
        </span>
      </div>
    </div>
  );
}

// ---- zone (b): deduped sender chips ----------------------------------------

function SenderChips({ items }: { items: AttentionUpdate[] }) {
  // Dedupe by sender, keep first occurrence; cap so the zone stays glanceable.
  const chips = useMemo(() => {
    const seen = new Set<string>();
    const out: AttentionUpdate[] = [];
    for (const u of items) {
      const key = u.sender.toLowerCase();
      if (seen.has(key)) continue;
      seen.add(key);
      out.push(u);
    }
    return out;
  }, [items]);

  const shown = chips.slice(0, 12);
  const extra = chips.length - shown.length;

  return (
    <div className="attn-chips">
      {shown.map((u) => (
        <span key={u.id} className="sender-chip" title={u.sender}>
          <Avatar sender={u.sender} size={18} />
          {senderDisplayName(u.sender)}
        </span>
      ))}
      {extra > 0 && <span className="sender-chip more">+{extra} more</span>}
    </div>
  );
}

// ---- zone (d): status strip ------------------------------------------------

function StatusStrip({
  authCount,
  lastCheckIso,
  lastRefresh,
  costUsd,
  rulesCount,
  onAuth,
  onRules,
}: {
  authCount: number;
  lastCheckIso: string | null | undefined;
  lastRefresh: number | null;
  costUsd: number | null | undefined;
  rulesCount: number | null;
  onAuth: () => void;
  onRules: () => void;
}) {
  const syncedIso = lastRefresh ? new Date(lastRefresh).toISOString() : null;
  return (
    <div className="status-strip">
      {authCount > 0 && (
        <button type="button" className="status-chip auth" onClick={onAuth} title="auth messages">
          <KeyRound size={13} /> {authCount} auth
        </button>
      )}
      <span className="status-item" title="last check by any door">
        <Clock size={13} /> synced {relAge(syncedIso ?? lastCheckIso) || "just now"} ago
      </span>
      {typeof costUsd === "number" && (
        <span className="status-item" title="today's stage-2 triage cost estimate">
          triage: ${costUsd.toFixed(2)} today
        </span>
      )}
      {rulesCount !== null && (
        <button
          type="button"
          className="status-chip"
          onClick={onRules}
          title="sender rules"
        >
          <SlidersHorizontal size={13} /> {rulesCount}{" "}
          {rulesCount === 1 ? "rule" : "rules"}
        </button>
      )}
    </div>
  );
}
