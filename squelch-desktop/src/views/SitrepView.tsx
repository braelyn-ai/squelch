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
  Mails,
  Pencil,
  Package,
  Truck,
  PackageCheck,
} from "lucide-react";
import { api, ApiError } from "../api";
import type {
  AttentionUpdate,
  Receipt as ReceiptRecord,
  SenderRule,
  Shipment,
  ShipmentStatus,
} from "../api";
import { openExternal } from "../lib/opener";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import {
  deadlineChip,
  lastChecked,
  loudAge,
  relAge,
  importanceColor,
  importanceMeter,
} from "../lib/format";
import { senderDisplayName, faviconUrl } from "../lib/avatar";
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
      <div className="dash-main">
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

      {/* ---- SHIPMENTS + RECEIPTS (tall right-hand column) ---- */}
      <aside className="dash-right">
        <ShipmentsColumn />
        <ReceiptsZone />
      </aside>
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
        <span className="ob-lead">
          <span
            className="ob-meter"
            style={{ color: importanceColor(u.importance) }}
            title={`importance ${u.importance}`}
            aria-label={`importance ${u.importance}`}
          >
            {importanceMeter(u.importance)}
          </span>
          {chip ? (
            <span className={`chip ${overdue ? "overdue" : "upcoming"}`}>
              {chip.text}
            </span>
          ) : (
            <span className="ob-nodate">no due date</span>
          )}
        </span>
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

  return (
    <section className="zone zone-transit">
      <div className="zone-head">
        <span className="glyph">
          <Truck size={15} />
        </span>
        <h2>Shipments</h2>
        {rows.length > 0 && <span className="zone-count">{rows.length}</span>}
        <span className="zone-sub">en route · delivered today</span>
      </div>
      {rows.length === 0 ? (
        <p className="zone-empty">No shipments.</p>
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
 * actions — deliberately the densest zone: each row is JUST the clean merchant
 * name (left) and the total (right, Plex Mono). No subject, no body, no
 * affordances. A running count sits in the header ("RECEIPTS · 6").
 */
function ReceiptsZone() {
  const [receipts, setReceipts] = useState<ReceiptRecord[] | null>(null);

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

  const rows = receipts ?? [];

  return (
    <section className="zone zone-receipts">
      <div className="zone-head">
        <span className="glyph">
          <Receipt size={15} />
        </span>
        <h2>Receipts</h2>
        {rows.length > 0 && <span className="zone-count">{rows.length}</span>}
        <span className="zone-sub">paid · records</span>
      </div>
      {rows.length === 0 ? (
        <p className="zone-empty">No receipts.</p>
      ) : (
        <div className="receipts-list">
          {rows.map((r) => {
            const sender = r.from_name
              ? `${r.from_name} <${r.from_addr}>`
              : r.from_addr;
            return (
              <div className="receipt-row" key={r.id}>
                <span className="receipt-sender" title={r.from_addr}>
                  {senderDisplayName(sender)}
                </span>
                <span className="receipt-amount">{receiptAmount(r)}</span>
              </div>
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

  return (
    <section className="zone zone-newsletters">
      <div className="zone-head">
        <span className="glyph">
          <Mails size={15} />
        </span>
        <h2>Newsletters</h2>
        {newsletters.length > 0 && (
          <span className="zone-count">{newsletters.length}</span>
        )}
        <span className="zone-sub">recurring noise · choose what you want</span>
      </div>
      {newsletters.length === 0 ? (
        <p className="zone-empty">No newsletters this week.</p>
      ) : (
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
      )}
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
