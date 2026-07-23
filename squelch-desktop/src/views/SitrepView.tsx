// SITREP VIEW — the fully-abstracted dashboard. THE DEFAULT SURFACE ON LAUNCH.
//
// ZERO individual email rows: this is the situation report, not the mailbox.
// Four soft-card zones, light/dark aware:
//   a. OBLIGATIONS — deadline-centric rows from band=standing (avatar + sender,
//      one-line + amount + due date, past-due loud). Actions: done (d / button),
//      open email (opens the obligation's thread fullscreen in the ThreadViewer).
//   b. ATTENTION — aggregate only: "N new since <relative last check>" +
//      deduped sender chips (from band=new). Click → Emails.
//   c. AGING — band=open filtered age>7d: "N items sitting over a week" + per
//      item sender + duration only (no subjects — abstraction). Click → Emails.
//   d. STATUS STRIP — auth chip (→ Auth), last sync/check, today's triage cost,
//      rules count.
//
// Minimal keymap in its own "sitrep" KeyContext: j/k move between obligation
// rows, d marks the focused obligation done, Enter/v opens it fullscreen. The
// global 1..5 nav (App) works here too. Obligations + aging rows open the email
// fullscreen (ThreadViewer); the Attention zone still routes to the Emails list.

import { useEffect, useMemo, useState } from "react";
import {
  KeyRound,
  SlidersHorizontal,
  ArrowUpRight,
  TriangleAlert,
  Bell,
  Hourglass,
  Receipt,
  Mails,
  Pencil,
  Package,
  Truck,
  PackageCheck,
  RefreshCw,
  CalendarDays,
} from "lucide-react";
import { api, ApiError } from "../api";
import type {
  AttentionUpdate,
  CalendarUpdate,
  Receipt as ReceiptRecord,
  SenderRule,
  Shipment,
  ShipmentStatus,
} from "../api";
import { openExternal } from "../lib/opener";
import { useStore, triggerMailRefresh } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { deadlineChip, lastChecked, loudAge, relAge } from "../lib/format";
import { senderDisplayName, faviconUrl } from "../lib/avatar";
import { getUserName } from "../lib/identity";
import { reasonFor } from "../lib/reasons";
import { Avatar } from "../components/Avatar";
import { dispatchDone } from "../lib/dispatch";
import {
  deriveNewsletters,
  domainPattern,
  type Newsletter,
} from "../lib/newsletters";
import { DISPOSITION_LABEL } from "../components/RuleEditor";
import { openRuleEditorRequest } from "../components/ruleEditorBus";
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

const SMALL_WORDS = [
  "Zero",
  "One",
  "Two",
  "Three",
  "Four",
  "Five",
  "Six",
  "Seven",
  "Eight",
  "Nine",
];
/** Spell small counts for the editorial hero ("Two obligations…"). */
function spell(n: number): string {
  return n >= 0 && n < SMALL_WORDS.length ? SMALL_WORDS[n] : String(n);
}

/** Obligations that are overdue or due by end of today — the "need you" set. */
function needTodayCount(items: AttentionUpdate[]): number {
  const end = new Date();
  end.setHours(23, 59, 59, 999);
  const cutoff = end.getTime();
  return items.filter((u) => {
    if (!u.deadline) return false;
    const t = new Date(u.deadline).getTime();
    return !Number.isNaN(t) && t <= cutoff;
  }).length;
}

function greeting(): string {
  const h = new Date().getHours();
  if (h < 12) return "Good morning";
  if (h < 18) return "Good afternoon";
  return "Good evening";
}

/**
 * DASH HERO — the editorial centerpiece (design concept): a tiny greeting label
 * (with the human's name), and a big Newsreader-serif headline stating what
 * needs them today.
 */
function DashHero({ standing }: { standing: AttentionUpdate[] }) {
  const today = needTodayCount(standing);
  const total = standing.length;
  const name = getUserName();

  let title: string;
  if (today > 0) {
    title = `${spell(today)} obligation${today === 1 ? "" : "s"} need${
      today === 1 ? "s" : ""
    } you today.`;
  } else if (total > 0) {
    title = `${spell(total)} obligation${total === 1 ? "" : "s"} on your plate.`;
  } else {
    title = "You're all clear.";
  }

  return (
    <div className="dash-hero">
      <span className="hero-greeting">
        {greeting()}
        {name ? `, ${name}` : ""}
      </span>
      <h1 className="hero-title">{title}</h1>
    </div>
  );
}

export function SitrepView() {
  const setView = useStore((s) => s.setView);
  const openThread = useStore((s) => s.openThread);

  return (
    <div className="sitrep-dash">
      <SitrepBody
        onView={(u) => openThread(u.thread_id)}
        onGoto={setView}
      />
    </div>
  );
}

function SitrepBody({
  onView,
  onGoto,
}: {
  onView: (u: AttentionUpdate) => void;
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
        key: "e",
        description: "mark done",
        handler: () => {
          const u = standing[obIdx];
          if (u) void dispatchDone(u);
        },
      },
      {
        key: "Enter",
        description: "open email",
        handler: () => {
          const u = standing[obIdx];
          if (u) onView(u);
        },
      },
      {
        key: "v",
        description: "open email",
        handler: () => {
          const u = standing[obIdx];
          if (u) onView(u);
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [standing, obIdx],
  );
  useKeys("sitrep", bindings, [bindings]);

  const needNow = needTodayCount(standing);
  const today = new Date().toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
  const syncedIso = lastRefresh ? new Date(lastRefresh).toISOString() : null;
  const rel = syncedIso ? relAge(syncedIso) : "";
  const syncLabel = !syncedIso
    ? "syncing…"
    : rel === "now" || rel === ""
      ? "synced just now"
      : `synced ${rel} ago`;

  return (
    <>
      {/* Pinned masthead (top-left brand + top-right status) — static. The
          drag-region attribute makes its EMPTY space a window-drag handle
          (Tauri only drags when the mousedown target is this element itself,
          so the brand/status children stay click-through-free). */}
      <header className="dash-header" data-tauri-drag-region>
        <div className="hdr-brand">
          <span className="brand">squelch</span>
          <span className="dash-sub">sitrep</span>
        </div>
        <div className="hdr-status">
          {needNow > 0 && (
            <span className="need-pill" title="obligations that need you today">
              <span className="need-dot" />
              {needNow} need you now
            </span>
          )}
          <span className="hdr-date">{today}</span>
          <span className="hdr-sync">{syncLabel}</span>
        </div>
      </header>

      {/* Body: the LEFT column (hero + main zones) scrolls; the right bar is
          static. The vertical separator sits between them. */}
      <div className="dash-body">
      <div className="dash-left">
      <DashHero standing={standing} />
      <div className="dash-main">
      {/* ---- (a) OBLIGATIONS — hidden entirely when empty ---- */}
      {standing.length > 0 && (
        <section className="zone zone-obligations">
          <div className="zone-head">
            <span className="glyph">
              <TriangleAlert size={15} />
            </span>
            <h2>Obligations</h2>
            <span className="zone-count">{standing.length}</span>
            <span className="zone-sub">deadlines · immune to time</span>
          </div>
          <div className="ob-list">
            {standing.map((u, i) => (
              <ObligationRow
                key={u.id}
                update={u}
                focused={i === obIdx}
                onFocus={() => setObIdx(i)}
                onView={() => onView(u)}
              />
            ))}
          </div>
        </section>
      )}

      {/* ---- (b) ATTENTION — hidden entirely when empty ---- */}
      {fresh.length > 0 && (
        <section
          className="zone zone-attention clickable"
          onClick={() => onGoto("emails")}
          role="button"
          tabIndex={-1}
        >
          <div className="zone-head">
            <span className="glyph">
              <Bell size={15} />
            </span>
            <h2>Attention</h2>
            <ArrowUpRight size={14} className="goto-hint" />
          </div>
          <p className="attn-lead">
            <b>{fresh.length}</b> new since{" "}
            {lastChecked(stats?.last_surfaced_at)}
          </p>
          <SenderChips items={fresh} />
        </section>
      )}

      {/* ---- (c) AGING — hidden entirely when empty ---- */}
      {aging.length > 0 && (
        <section className="zone zone-aging">
          <div className="zone-head">
            <span className="glyph">
              <Hourglass size={15} />
            </span>
            <h2>Aging</h2>
            <span className="zone-count">{aging.length}</span>
          </div>
          <p className="aging-lead">
            <b>{aging.length}</b> {aging.length === 1 ? "item" : "items"} sitting
            over a week
          </p>
          <div className="aging-list">
            {aging.map((u) => (
              <button
                key={u.id}
                type="button"
                className="aging-row"
                onClick={() => onView(u)}
                title="open email"
              >
                <Avatar sender={u.sender} size={20} />
                <span className="sender">{senderDisplayName(u.sender)}</span>
                <span className="dur">
                  {loudAge(u.surfaced_at ?? u.resolved_at).toLowerCase()}
                </span>
              </button>
            ))}
          </div>
        </section>
      )}

      {/* ---- NEWSLETTERS (rule-onboarding surface) ---- */}
      <NewslettersZone />

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
      </div>

      {/* ---- CALENDAR + SHIPMENTS + RECEIPTS (static right-hand column) ---- */}
      <aside className="dash-right">
        <CalendarZone />
        <ShipmentsColumn />
        <ReceiptsZone />
      </aside>
      </div>
    </>
  );
}

// ---- zone (a): a single obligation row -------------------------------------

function ObligationRow({
  update: u,
  focused,
  onFocus,
  onView,
}: {
  update: AttentionUpdate;
  focused: boolean;
  onFocus: () => void;
  onView: () => void;
}) {
  const chip = deadlineChip(u.deadline);
  const overdue = chip?.overdue ?? false;
  const amount = amountFrom(u);

  // Click anywhere on the row opens the email; done is keyboard-only (e/d),
  // same as the inbox — no per-row checkmark button.
  return (
    <div
      className={`ob-row${focused ? " focused" : ""}${overdue ? " overdue" : ""}`}
      onClick={() => {
        onFocus();
        onView();
      }}
      role="button"
      tabIndex={-1}
      title="open email"
    >
      <Avatar sender={u.sender} size={22} />
      <span className="ob-sender" title={u.sender}>
        {senderDisplayName(u.sender)}
      </span>
      {/* The abstracted one-liner carries the meaning; it truncates first. */}
      <p className="ob-line" title={u.one_line}>
        {u.one_line}
      </p>
      {amount && (
        <span className="ob-amount">
          <Receipt size={13} /> {amount}
        </span>
      )}
      {chip ? (
        <span
          className={`chip ${overdue ? "overdue" : "upcoming"}`}
          title={reasonFor(u, "deadline", chip.text)}
        >
          {chip.text}
        </span>
      ) : (
        <span
          className="ob-nodate"
          title={u.field_reasons?.deadline ?? undefined}
        >
          no due date
        </span>
      )}
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

// ---- IN TRANSIT zone: shipment tracking ------------------------------------

// Carriers with a favicon we can resolve via the existing avatar service.
// amazon/unknown fall through to the lucide Package glyph (no clean single
// domain / a generic package).
const CARRIER_DOMAIN: Partial<Record<Shipment["carrier"], string>> = {
  ups: "ups.com",
  usps: "usps.com",
  fedex: "fedex.com",
  dhl: "dhl.com",
};

// Status → chip class + label. Colors defined in sitrep-dash.css:
//   out_for_delivery = amber/loud, shipped = signal bronze-green (accent),
//   exception = red, ordered = faint/muted, delivered = muted w/ checkmark
//   (delivered-today items surface here via the includeDelivered filter).
const SHIP_STATUS: Record<
  ShipmentStatus,
  { cls: string; label: string }
> = {
  ordered: { cls: "ordered", label: "ordered" },
  shipped: { cls: "shipped", label: "shipped" },
  out_for_delivery: { cls: "ofd", label: "out for delivery" },
  delivered: { cls: "delivered", label: "delivered" },
  exception: { cls: "exception", label: "exception" },
};

/** Title-case a carrier for display ("ups" -> "UPS", "amazon" -> "Amazon"). */
function carrierLabel(carrier: Shipment["carrier"]): string {
  if (carrier === "unknown") return "carrier";
  if (carrier === "ups" || carrier === "usps" || carrier === "dhl") {
    return carrier.toUpperCase();
  }
  if (carrier === "fedex") return "FedEx";
  return carrier.charAt(0).toUpperCase() + carrier.slice(1);
}

/**
 * True if an RFC3339 timestamp falls on the current LOCAL calendar day. Parses
 * defensively: a missing/unparseable stamp returns false (older delivered items
 * without a good stamp stay hidden, which is the safe/quiet default).
 */
function isToday(iso: string | null | undefined): boolean {
  if (!iso) return false;
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) return false;
  const now = new Date();
  return (
    t.getFullYear() === now.getFullYear() &&
    t.getMonth() === now.getMonth() &&
    t.getDate() === now.getDate()
  );
}

/**
 * SHIPMENTS zone, rendered as the tall right-hand column. Fetches with
 * includeDelivered=true then keeps a shipment when it's still active
 * (status !== "delivered") OR it was delivered TODAY (local calendar day).
 * Yesterday's-and-older deliveries drop out. View-only by design: no j/k, but
 * each card's Track button is a real focusable/clickable affordance.
 */
function ShipmentsColumn() {
  const [shipments, setShipments] = useState<Shipment[] | null>(null);

  useEffect(() => {
    let alive = true;
    api
      .getShipments(true)
      .then((s) => alive && setShipments(s))
      .catch(() => {
        // Non-fatal: leave the zone empty rather than surface token/url.
        if (alive) setShipments([]);
      });
    return () => {
      alive = false;
    };
  }, []);

  const rows = useMemo(
    () =>
      (shipments ?? []).filter(
        (s) => s.status !== "delivered" || isToday(s.last_update),
      ),
    [shipments],
  );

  // The right rail always shows its cards (unlike the left zones, which hide
  // when empty) — so an empty state stands in when there's nothing en route.
  return (
    <section className="zone zone-transit">
      <div className="zone-head">
        <span className="glyph">
          <Truck size={15} />
        </span>
        <h2>Shipments</h2>
        {rows.length > 0 && <span className="zone-count">{rows.length}</span>}
      </div>
      {rows.length === 0 ? (
        <p className="zone-empty">Nothing en route.</p>
      ) : (
        <div className="transit-grid">
          {rows.map((s) => (
            <ShipmentCard key={s.id} shipment={s} />
          ))}
        </div>
      )}
    </section>
  );
}

// ---- CALENDAR zone: today's invite/update/cancellation mail ----------------

/** Compact when-column for a calendar row: time if the event is today
 *  ("3:00 PM"), short date otherwise, empty when no start parsed. */
function calWhen(c: CalendarUpdate): string {
  if (!c.starts_at) return "";
  const d = new Date(c.starts_at);
  if (Number.isNaN(d.getTime())) return "";
  if (isToday(c.starts_at)) {
    return d.toLocaleTimeString(undefined, {
      hour: "numeric",
      minute: "2-digit",
    });
  }
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** Row tag for the non-default kinds; a plain invite needs no label. */
const CAL_KIND_TAG: Partial<Record<CalendarUpdate["kind"], string>> = {
  update: "updated",
  cancellation: "canceled",
  response: "rsvp",
};

/**
 * CALENDAR zone, at the top of the right-hand column: calendar mail from the
 * last 24h (server window). Same abstraction as Receipts — these messages are
 * auto-resolved out of the attention bands at ingest, so this rail is the ONLY
 * place they surface; clicking a row opens the underlying email. Records, not
 * an agenda: rows are ordered by arrival, cancellations strike through.
 */
function CalendarZone() {
  const [updates, setUpdates] = useState<CalendarUpdate[] | null>(null);
  const viewInEmails = useStore((s) => s.viewInEmails);

  useEffect(() => {
    let alive = true;
    api
      .getCalendar()
      .then((c) => alive && setUpdates(c))
      .catch(() => {
        // Non-fatal: leave the zone empty rather than surface token/url.
        if (alive) setUpdates([]);
      });
    return () => {
      alive = false;
    };
  }, []);

  const rows = updates ?? [];

  // The right rail always shows its cards — an empty state stands in when no
  // calendar mail arrived in the window (no hiding like the left zones).
  return (
    <section className="zone zone-calendar">
      <div className="zone-head">
        <span className="glyph">
          <CalendarDays size={15} />
        </span>
        <h2>Calendar</h2>
        {rows.length > 0 && <span className="zone-count">{rows.length}</span>}
      </div>
      {rows.length === 0 ? (
        <p className="zone-empty">No calendar updates.</p>
      ) : (
        <div className="cal-list">
          {rows.map((c) => {
            const tag = CAL_KIND_TAG[c.kind];
            return (
              <button
                type="button"
                className={`cal-row${c.kind === "cancellation" ? " canceled" : ""}`}
                key={c.id}
                onClick={() => viewInEmails(c.message_id)}
                title="view this email"
              >
                <span className="cal-title">
                  {c.event_title ?? c.organizer ?? "calendar event"}
                </span>
                {tag && <span className="cal-tag">{tag}</span>}
                <span className="cal-when">{calWhen(c)}</span>
              </button>
            );
          })}
        </div>
      )}
    </section>
  );
}

// ---- RECEIPTS zone: thin records of money already paid ---------------------

/** Format a receipt total: "$3.49", or "—" when the amount didn't parse. */
function receiptAmount(r: ReceiptRecord): string {
  if (r.amount === null || r.amount === undefined || Number.isNaN(r.amount)) {
    return "—";
  }
  // USD-only in v0 (the server always emits USD). Two decimals, thousands sep.
  return `$${r.amount.toLocaleString("en-US", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })}`;
}

/**
 * RECEIPTS zone, stacked under Shipments in the right-hand column. Records, not
 * actions — each row is the clean merchant name (left) and the total (right,
 * Plex Mono). Only TODAY's receipts show (a fresh daily digest, like a paper
 * receipt you'd glance at and file); clicking a row opens that email.
 */
function ReceiptsZone() {
  const [receipts, setReceipts] = useState<ReceiptRecord[] | null>(null);
  const viewInEmails = useStore((s) => s.viewInEmails);

  useEffect(() => {
    let alive = true;
    api
      .getReceipts()
      .then((r) => alive && setReceipts(r))
      .catch(() => {
        // Non-fatal: leave the zone empty rather than surface token/url.
        if (alive) setReceipts([]);
      });
    return () => {
      alive = false;
    };
  }, []);

  // Only receipts that arrived today (the abstraction: a daily receipts digest).
  const rows = (receipts ?? []).filter((r) => isToday(r.received_at));

  // The right rail always shows this card — an empty state stands in when no
  // receipts came in today (it does not hide like the left-column zones).
  return (
    <section className="zone zone-receipts">
      <div className="zone-head">
        <span className="glyph">
          <Receipt size={15} />
        </span>
        <h2>Receipts</h2>
        {rows.length > 0 && <span className="zone-count">{rows.length}</span>}
      </div>
      {rows.length === 0 ? (
        <p className="zone-empty">No receipts today.</p>
      ) : (
      <div className="receipts-list">
        {rows.map((r) => {
          const sender = r.from_name
            ? `${r.from_name} <${r.from_addr}>`
            : r.from_addr;
          return (
            <button
              type="button"
              className="receipt-row"
              key={r.id}
              onClick={() => viewInEmails(r.message_id)}
              title="view this email"
            >
              <span className="receipt-sender" title={r.from_addr}>
                {senderDisplayName(sender)}
              </span>
              <span className="receipt-amount">{receiptAmount(r)}</span>
            </button>
          );
        })}
      </div>
      )}
    </section>
  );
}

function CarrierBadge({ carrier }: { carrier: Shipment["carrier"] }) {
  const domain = CARRIER_DOMAIN[carrier];
  const [failed, setFailed] = useState(!domain);

  if (domain && !failed) {
    return (
      <img
        className="transit-carrier-icon"
        src={faviconUrl(domain)}
        width={24}
        height={24}
        alt=""
        aria-hidden="true"
        title={carrierLabel(carrier)}
        referrerPolicy="no-referrer"
        onError={() => setFailed(true)}
      />
    );
  }
  // amazon / unknown / failed favicon → neutral package glyph.
  return (
    <span className="transit-carrier-glyph" title={carrierLabel(carrier)}>
      <Package size={16} />
    </span>
  );
}

function ShipmentCard({ shipment: s }: { shipment: Shipment }) {
  const st = SHIP_STATUS[s.status] ?? SHIP_STATUS.ordered;
  const title =
    s.item_name.trim() || `Package via ${carrierLabel(s.carrier)}`;
  const canTrack = !!s.tracking_url;
  const delivered = s.status === "delivered";

  return (
    <div className={`transit-card${delivered ? " delivered" : ""}`}>
      <div className="transit-top">
        <CarrierBadge carrier={s.carrier} />
        <span className="transit-name" title={title}>
          {title}
        </span>
      </div>
      <div className="transit-bottom">
        <span className={`transit-chip ${st.cls}`}>
          {delivered && <PackageCheck size={12} />}
          {st.label}
        </span>
        {canTrack && (
          <button
            type="button"
            className="transit-track"
            onClick={() => void openExternal(s.tracking_url!)}
            title={`track ${s.tracking_number} · ${carrierLabel(s.carrier)}`}
          >
            <ArrowUpRight size={13} /> Track
          </button>
        )}
      </div>
    </div>
  );
}

// ---- NEWSLETTERS zone: the rule-onboarding surface -------------------------

// Pull a generous window of noise-tier updates and filter to the last 7 days
// client-side (the wire model carries no received_at; we date on surfaced_at).
const NL_FETCH_LIMIT = 200;

function NewslettersZone() {
  const [updates, setUpdates] = useState<AttentionUpdate[] | null>(null);
  const [rules, setRules] = useState<SenderRule[]>([]);

  // Fetch noise updates + rules once; re-fetch after a rule save so chips/CTAs
  // reflect the new rule immediately.
  const load = useMemo(
    () => async () => {
      try {
        const [page, rl] = await Promise.all([
          api.getUpdates({ tier: "noise", limit: NL_FETCH_LIMIT }),
          api.listRules(),
        ]);
        setUpdates(page.items);
        setRules(rl);
      } catch (e) {
        // Non-fatal: leave the zone empty rather than surfacing token/url.
        if (!(e instanceof ApiError)) setUpdates([]);
        else setUpdates([]);
      }
    },
    [],
  );
  useEffect(() => {
    void load();
  }, [load]);

  const newsletters = useMemo(
    () => (updates ? deriveNewsletters(updates, rules) : []),
    [updates, rules],
  );

  function editRule(nl: Newsletter) {
    if (!nl.rule) return;
    openRuleEditorRequest({ rule: nl.rule, onSaved: () => void load() });
  }
  function createRule(nl: Newsletter) {
    // Prefill *@domain (favicon-normalized so mail-subdomains collapse to the
    // brand), disposition "filtered" (the onboarding default), and land focus on
    // the want field so the human describes what they DO want to see.
    openRuleEditorRequest({
      sender: nl.address,
      pattern: domainPattern(nl.address),
      disposition: "filtered",
      onSaved: () => void load(),
    });
  }

  // Hidden entirely when there are no newsletters (and while still loading).
  if (newsletters.length === 0) return null;

  return (
    <section className="zone zone-newsletters">
      <div className="zone-head">
        <span className="glyph">
          <Mails size={15} />
        </span>
        <h2>Newsletters</h2>
        <span className="zone-count">{newsletters.length}</span>
        <span className="zone-sub">recurring noise · choose what you want</span>
      </div>
      <div className="nl-grid">
        {newsletters.map((nl) => (
          <NewsletterCard
            key={nl.address}
            nl={nl}
            onEdit={() => editRule(nl)}
            onCreate={() => createRule(nl)}
          />
        ))}
      </div>
    </section>
  );
}

function truncate(s: string, n: number): string {
  return s.length > n ? s.slice(0, n - 1).trimEnd() + "…" : s;
}

function NewsletterCard({
  nl,
  onEdit,
  onCreate,
}: {
  nl: Newsletter;
  onEdit: () => void;
  onCreate: () => void;
}) {
  const hasRule = nl.rule !== null;
  // Enter (with the card focused) opens the right editor; click does the same.
  const open = hasRule ? onEdit : onCreate;

  return (
    <div
      className={`nl-card${hasRule ? " ruled" : ""}`}
      role="button"
      tabIndex={0}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          open();
        }
      }}
    >
      <div className="nl-top">
        <Avatar sender={nl.sender} size={24} />
        <span className="nl-sender" title={nl.sender}>
          {senderDisplayName(nl.sender)}
        </span>
        <span className="nl-count">{nl.count} this week</span>
      </div>

      {nl.summary && (
        <p className="nl-summary" title={nl.summary}>
          {truncate(nl.summary, 90)}
        </p>
      )}

      {hasRule ? (
        <div className="nl-rulechip" title="edit this rule">
          <span className="nl-disp">{DISPOSITION_LABEL[nl.rule!.disposition]}</span>
          {nl.rule!.want_text && (
            <span className="nl-want">{truncate(nl.rule!.want_text, 48)}</span>
          )}
          <Pencil size={12} className="nl-pencil" />
        </div>
      ) : (
        <div className="nl-cta">
          Choose what you want to see <ArrowUpRight size={13} />
        </div>
      )}
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
  const [refreshing, setRefreshing] = useState(false);
  const onRefresh = async () => {
    if (refreshing) return;
    setRefreshing(true);
    try {
      await triggerMailRefresh();
    } finally {
      setRefreshing(false);
    }
  };
  return (
    <div className="status-strip">
      {authCount > 0 && (
        <button type="button" className="status-chip auth" onClick={onAuth} title="auth messages">
          <KeyRound size={13} /> {authCount} auth
        </button>
      )}
      <button
        type="button"
        className="status-chip refresh"
        onClick={() => void onRefresh()}
        disabled={refreshing}
        title="check for new mail now"
      >
        <RefreshCw size={13} className={refreshing ? "spin" : undefined} />
        <span className="status-item" title="last check by any door">
          synced {relAge(syncedIso ?? lastCheckIso) || "just now"} ago
        </span>
      </button>
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
