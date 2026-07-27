// AUTH PAGE — the dedicated home for auth-related mail: login codes, password
// resets, sign-in alerts, verifications. Metadata-only by default (bodies are
// NEVER fetched to render the list), with one deliberate exception documented
// under REVEAL below.
//
// SHAPE. Two columns. The main column leads with IN FOCUS — the selected
// message blown up, and for a code kind its digits in individual boxes with a
// copy button, because "get the code and get out" is the entire job of this
// page. Under it: filter chips, then the messages split into LIVE (last hour)
// and ARCHIVE. The right rail holds NEEDS A DECISION — sign-in alerts and reset
// requests, which do not hand you a code but do ask you a question — and the
// shredder panel.
//
// REVEAL is the one place a body is touched, and it is always a human action.
// A reveal writes an audit row server-side, so this page never reveals on load:
// digits appear immediately ONLY for codes the arrival flow already revealed
// this session (state/useAuthArrival auto-reveals code kinds as they land, so
// that reveal is already paid for and already audited). Everything else shows a
// covered placeholder until you press R. Revealed codes live in this
// component's state and die with it — never persisted, never lifted into the
// global store.
//
// DECISIONS ("That was me" / "Not me") are device-local; see lib/authDecisions
// for what that costs and when it should be promoted server-side.
//
// KEYS: follows the SidePanel contract — registers into the existing "modal"
// context via useKeys and never pushes its own (RoutedView already pushed it).

import { useCallback, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { Copy, Check, Archive, Eye, ShieldAlert, Trash2 } from "lucide-react";
import { api, ApiError } from "../api";
import type { SealedMeta, ShredStats } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { relAge } from "../lib/format";
import { authKindLabel, authKindIcon } from "../lib/authCopy";
import { extractCode, isCodeKind } from "../lib/authCode";
import { copyText } from "../lib/clipboard";
import { Avatar } from "./Avatar";
import { senderDisplayName } from "../lib/avatar";
import { RevealPanel } from "./RevealPanel";
import {
  decisionsSnapshot,
  getDecision,
  needsDecision,
  setDecision,
  subscribeDecisions,
} from "../lib/authDecisions";
import "../styles/auth.css";

/** Anything newer than this counts as LIVE and heads the list. */
const LIVE_MS = 60 * 60 * 1000;
/** Cards in the decision rail before it stops growing. */
const MAX_DECISION_CARDS = 6;

type Filter = "all" | "live" | "codes" | "risk";

const FILTERS: { key: Filter; label: string }[] = [
  { key: "all", label: "Everything" },
  { key: "live", label: "Last hour" },
  { key: "codes", label: "Codes only" },
  { key: "risk", label: "Risk" },
];

/** ms since an ISO stamp; huge when missing/invalid (=> treated as old). */
function ageMs(iso: string | null | undefined): number {
  if (!iso) return Number.MAX_SAFE_INTEGER;
  const t = new Date(iso).getTime();
  return Number.isNaN(t) ? Number.MAX_SAFE_INTEGER : Date.now() - t;
}

function matches(m: SealedMeta, f: Filter): boolean {
  switch (f) {
    case "live":
      return ageMs(m.received_at) <= LIVE_MS;
    case "codes":
      return isCodeKind(m.kind);
    case "risk":
      // The ones that mean someone may be trying to get in.
      return needsDecision(m.kind);
    default:
      return true;
  }
}

export function AuthView() {
  const items = useStore((s) => s.sitrep.sealed);
  const authQueue = useStore((s) => s.authQueue);
  const pushToast = useStore((s) => s.pushToast);

  const [filter, setFilter] = useState<Filter>("all");
  const [idx, setIdx] = useState(0);
  const [revealing, setRevealing] = useState<SealedMeta | null>(null);
  const [copied, setCopied] = useState(false);
  const [busy, setBusy] = useState<number | null>(null);
  /** Codes revealed THIS SESSION, by message id. `null` = revealed but no code
   *  could be read. Component state only — never persisted. */
  const [codes, setCodes] = useState<Record<number, string | null>>({});
  /** Ids archived from this page, hidden optimistically (the list is a poll). */
  const [archived, setArchived] = useState<Set<number>>(new Set());

  useSyncExternalStore(subscribeDecisions, decisionsSnapshot);

  const visible = useMemo(
    () => items.filter((m) => !archived.has(m.id)),
    [items, archived],
  );
  const shown = useMemo(
    () => visible.filter((m) => matches(m, filter)),
    [visible, filter],
  );
  const focus: SealedMeta | undefined = shown[idx];

  // Keep the selection in range as the list refreshes or the filter narrows.
  useEffect(() => {
    setIdx((i) => Math.max(0, Math.min(i, Math.max(0, shown.length - 1))));
  }, [shown.length]);
  useEffect(() => setCopied(false), [focus?.id]);

  // Seed from the arrival flow: a code auto-revealed as it landed is already in
  // memory and already audited, so showing it here costs nothing extra.
  useEffect(() => {
    if (authQueue.length === 0) return;
    setCodes((prev) => {
      let changed = false;
      const next = { ...prev };
      for (const e of authQueue) {
        if (!(e.meta.id in next)) {
          next[e.meta.id] = e.code;
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [authQueue]);

  const live = shown.filter((m) => ageMs(m.received_at) <= LIVE_MS);
  const archive = shown.filter((m) => ageMs(m.received_at) > LIVE_MS);
  const open = visible.filter((m) => needsDecision(m.kind) && getDecision(m.id) === null);

  const focusCode = focus ? codes[focus.id] : undefined;
  const focusRevealed = focus ? focus.id in codes : false;

  /** Reveal one message. Code kinds resolve to digits inline; everything else
   *  opens the full one-time reveal panel, which is the honest affordance for a
   *  message whose point is its prose, not a number. */
  const reveal = useCallback(
    async (m: SealedMeta) => {
      if (!isCodeKind(m.kind)) {
        setRevealing(m);
        return;
      }
      if (m.id in codes) return;
      setBusy(m.id);
      try {
        const r = await api.revealSealed(m.id);
        setCodes((s) => ({ ...s, [m.id]: extractCode(r.body) }));
      } catch (e) {
        setCodes((s) => ({ ...s, [m.id]: null }));
        pushToast(e instanceof ApiError ? e.message : "reveal failed", "error");
      } finally {
        setBusy(null);
      }
    },
    [codes, pushToast],
  );

  const copy = useCallback(async () => {
    if (!focusCode) return;
    if (await copyText(focusCode)) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    }
  }, [focusCode]);

  /** "Used it — archive": the code is spent, get it out of the inbox. */
  const archiveFocused = useCallback(
    async (m: SealedMeta) => {
      setBusy(m.id);
      try {
        await api.actionArchive(m.id);
        setArchived((s) => new Set(s).add(m.id));
        pushToast("archived", "success");
      } catch (e) {
        pushToast(
          e instanceof ApiError ? e.message : "archive failed",
          "error",
        );
      } finally {
        setBusy(null);
      }
    },
    [pushToast],
  );

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next",
        handler: () => setIdx((i) => Math.min(shown.length - 1, i + 1)),
      },
      { key: "k", description: "prev", handler: () => setIdx((i) => Math.max(0, i - 1)) },
      {
        key: "ArrowDown",
        description: "next",
        handler: () => setIdx((i) => Math.min(shown.length - 1, i + 1)),
      },
      {
        key: "ArrowUp",
        description: "prev",
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
      {
        key: "r",
        description: "reveal",
        handler: () => {
          if (focus) void reveal(focus);
        },
      },
      {
        key: "Enter",
        description: "reveal",
        handler: () => {
          if (focus) void reveal(focus);
        },
      },
      { key: "c", description: "copy code", handler: () => void copy() },
    ],
    [shown.length, focus, reveal, copy],
  );
  useKeys("modal", bindings, [bindings]);

  return (
    <div className="authp">
      <header className="authp-head">
        <h2>
          Auth <span className="authp-tag">codes, alerts &amp; resets — nothing else</span>
        </h2>
        <div className="authp-headright">
          <span className="authp-pill">
            <i className="dot" /> {live.length} live · {open.length} awaiting you
          </span>
          <span className="authp-keys">
            <kbd>j</kbd>
            <kbd>k</kbd> move <kbd>c</kbd> copy <kbd>r</kbd> reveal
          </span>
        </div>
      </header>

      <div className="authp-body">
        <main className="authp-main">
          {focus ? (
            <FocusPanel
              meta={focus}
              code={focusCode}
              revealed={focusRevealed}
              busy={busy === focus.id}
              copied={copied}
              onCopy={() => void copy()}
              onReveal={() => void reveal(focus)}
              onArchive={() => void archiveFocused(focus)}
            />
          ) : (
            <div className="authp-empty">
              nothing here — login codes, password resets and sign-in alerts
              land on this page as they arrive.
            </div>
          )}

          <div className="authp-filters">
            {FILTERS.map((f) => (
              <button
                key={f.key}
                type="button"
                className={`authp-chip${filter === f.key ? " on" : ""}`}
                onClick={() => {
                  setFilter(f.key);
                  setIdx(0);
                }}
              >
                {f.label}
              </button>
            ))}
            <span className="authp-count">
              {shown.length} of {visible.length} shown
            </span>
          </div>

          <div className="authp-list">
            <Section
              label="Live · last hour"
              rows={live}
              all={shown}
              idx={idx}
              codes={codes}
              onSelect={setIdx}
              onOpen={(m) => void reveal(m)}
            />
            <Section
              label="Archive"
              rows={archive}
              all={shown}
              idx={idx}
              codes={codes}
              onSelect={setIdx}
              onOpen={(m) => void reveal(m)}
            />
          </div>
        </main>

        <aside className="authp-rail">
          <div className="authp-railhead">
            <span>Needs a decision</span>
            <span className="authp-open">{open.length} open</span>
          </div>
          {open.length === 0 ? (
            <div className="authp-railempty">
              nothing to rule on. sign-in alerts and reset requests show up here.
            </div>
          ) : (
            open
              .slice(0, MAX_DECISION_CARDS)
              .map((m) => (
                <DecisionCard
                  key={m.id}
                  meta={m}
                  onMine={() => setDecision(m.id, "mine")}
                  onNotMine={() => {
                    // NOT a dismissal: record the verdict, then open the message
                    // so the human can read what actually happened and act.
                    setDecision(m.id, "not-mine");
                    setRevealing(m);
                  }}
                />
              ))
          )}
          <ShredderCard />
        </aside>
      </div>

      {revealing && (
        <RevealPanel meta={revealing} onClose={() => setRevealing(null)} />
      )}
    </div>
  );
}

/** The blown-up selected message: identity, kind, and — for a code — digits. */
function FocusPanel({
  meta,
  code,
  revealed,
  busy,
  copied,
  onCopy,
  onReveal,
  onArchive,
}: {
  meta: SealedMeta;
  code: string | null | undefined;
  revealed: boolean;
  busy: boolean;
  copied: boolean;
  onCopy: () => void;
  onReveal: () => void;
  onArchive: () => void;
}) {
  const KindIcon = authKindIcon(meta.kind);
  const isCode = isCodeKind(meta.kind);

  return (
    <section className="authp-focus">
      <div className="authp-focushead">
        <span className="authp-label">In focus</span>
        <span className="authp-arrived">arrived {relAge(meta.received_at)} ago</span>
      </div>

      <div className="authp-who">
        <Avatar sender={meta.sender} size={44} />
        <div className="authp-whotext">
          <span className="authp-sender" title={meta.sender}>
            {senderDisplayName(meta.sender)}
          </span>
          <span className="authp-subject" title={meta.subject}>
            {meta.subject}
          </span>
        </div>
        <span className="authp-kind">
          <KindIcon size={13} /> {authKindLabel(meta.kind)}
        </span>
      </div>

      <div className="authp-payoff">
        {isCode ? (
          <>
            {revealed && code ? (
              <div className="authp-digits" aria-label="login code">
                {code.split("").map((d, i) => (
                  <span key={i} className="authp-digit">
                    {d}
                  </span>
                ))}
              </div>
            ) : (
              <div className="authp-digits covered" aria-hidden="true">
                {Array.from({ length: 6 }, (_, i) => (
                  <span key={i} className="authp-digit blank">
                    ·
                  </span>
                ))}
              </div>
            )}

            <div className="authp-actions">
              {revealed && code ? (
                <>
                  <button type="button" className="authp-btn primary" onClick={onCopy}>
                    {copied ? <Check size={15} /> : <Copy size={15} />}
                    {copied ? "Copied" : "Copy code"}
                  </button>
                  <button
                    type="button"
                    className="authp-btn"
                    disabled={busy}
                    onClick={onArchive}
                  >
                    <Archive size={15} /> Used it — archive
                  </button>
                </>
              ) : (
                <button
                  type="button"
                  className="authp-btn primary"
                  disabled={busy}
                  onClick={onReveal}
                >
                  <Eye size={15} /> {busy ? "Revealing…" : "Reveal code"}{" "}
                  <kbd>r</kbd>
                </button>
              )}
              <p className="authp-note">
                {revealed && code
                  ? "Held in memory for this screen only — squelch never stores it. Revealing was written to the audit log."
                  : revealed
                    ? "Revealed, but no code could be read from this one. Open it to read it yourself."
                    : "Nothing is read until you ask. Revealing fetches the body once and records an audit entry."}
              </p>
            </div>
          </>
        ) : (
          <div className="authp-actions prose">
            <button type="button" className="authp-btn primary" onClick={onReveal}>
              <Eye size={15} /> Open it <kbd>r</kbd>
            </button>
            <p className="authp-note">
              This one is a message, not a code. Opening it fetches the body once
              and records an audit entry.
            </p>
          </div>
        )}
      </div>
    </section>
  );
}

/** One labelled group of rows. Renders nothing when the group is empty. */
function Section({
  label,
  rows,
  all,
  idx,
  codes,
  onSelect,
  onOpen,
}: {
  label: string;
  rows: SealedMeta[];
  /** The full filtered list, so a row can report its real selection index. */
  all: SealedMeta[];
  idx: number;
  codes: Record<number, string | null>;
  onSelect: (i: number) => void;
  onOpen: (m: SealedMeta) => void;
}) {
  if (rows.length === 0) return null;
  return (
    <>
      <div className="authp-sec">{label}</div>
      {rows.map((m) => {
        const i = all.indexOf(m);
        const KindIcon = authKindIcon(m.kind);
        const code = codes[m.id];
        return (
          <div
            key={m.id}
            className={`authp-row${i === idx ? " sel" : ""}`}
            onClick={() => onSelect(i)}
            onDoubleClick={() => onOpen(m)}
            role="button"
            tabIndex={-1}
          >
            <Avatar sender={m.sender} />
            <span className="sender" title={m.sender}>
              {senderDisplayName(m.sender)}
            </span>
            <span className="subject" title={m.subject}>
              {m.subject}
            </span>
            {code ? (
              // Already revealed this session — no reason to make them ask twice.
              <span className="authp-rowcode num">{code}</span>
            ) : (
              <span className="authp-rowkind">
                <KindIcon size={12} /> {authKindLabel(m.kind)}
              </span>
            )}
            <span className="age">{relAge(m.received_at)}</span>
          </div>
        );
      })}
    </>
  );
}

/** A sign-in alert / reset request awaiting the human's verdict. */
function DecisionCard({
  meta,
  onMine,
  onNotMine,
}: {
  meta: SealedMeta;
  onMine: () => void;
  onNotMine: () => void;
}) {
  const reset = meta.kind === "password_reset";
  return (
    <div className="authp-card">
      <div className="authp-cardhead">
        <Avatar sender={meta.sender} size={20} />
        <span className="authp-cardwho" title={meta.sender}>
          {senderDisplayName(meta.sender)}
        </span>
        <span className="authp-cardage">{relAge(meta.received_at)}</span>
      </div>
      {/* Subject only — the body is sealed and this card never fetches it. */}
      <div className="authp-cardtext" title={meta.subject}>
        {meta.subject}
      </div>
      <div className="authp-cardactions">
        <button type="button" className="authp-btn primary sm" onClick={onMine}>
          {reset ? "I asked" : "That was me"}
        </button>
        <button type="button" className="authp-btn danger sm" onClick={onNotMine}>
          {reset ? "I did not" : "Not me"}
        </button>
      </div>
    </div>
  );
}

/**
 * The shredder panel. Every number here comes from the server's `shred_log`
 * ledger — nothing on this card is estimated. The switch is the ONLY way
 * automatic deletion ever turns on, and the card is explicit that shredding
 * means Gmail's Trash (recoverable), not a permanent delete.
 */
function ShredderCard() {
  const pushToast = useStore((s) => s.pushToast);
  const [stats, setStats] = useState<ShredStats | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    // Ask the server to apply the policy now rather than waiting out its hourly
    // timer; it is a no-op server-side when the shredder is off or unable to
    // run, so this is safe to fire on every open.
    api
      .runShredder()
      .then((r) => alive && setStats(r.stats))
      .catch(() => {
        // Fall back to a plain read — an older daemon may not have the route.
        api
          .getShredder()
          .then((s) => alive && setStats(s))
          .catch(() => {});
      });
    return () => {
      alive = false;
    };
  }, []);

  async function toggle() {
    if (!stats || busy) return;
    setBusy(true);
    try {
      const next = await api.setShredder({ enabled: !stats.enabled });
      setStats(next);
      if (next.enabled) {
        const run = await api.runShredder();
        setStats(run.stats);
        if (run.shredded > 0) {
          pushToast(`shredded ${run.shredded} old auth message(s)`, "success");
        }
      }
    } catch (e) {
      pushToast(
        e instanceof ApiError ? e.message : "could not change the shredder",
        "error",
      );
    } finally {
      setBusy(false);
    }
  }

  if (!stats) return null;

  return (
    <div className="authp-card shredder">
      <div className="authp-cardhead">
        <Trash2 size={14} />
        <span className="authp-cardwho">Auto-shredder</span>
        <button
          type="button"
          className={`authp-switch${stats.enabled ? " on" : ""}`}
          disabled={busy || !stats.write_ready}
          onClick={() => void toggle()}
          title={
            stats.write_ready
              ? "move auth mail older than the window to Gmail Trash"
              : "needs the write credential"
          }
          aria-pressed={stats.enabled}
        >
          <span className="knob" />
        </button>
      </div>

      {!stats.write_ready ? (
        <div className="authp-cardtext">
          <ShieldAlert size={13} /> Needs the write credential — run{" "}
          <code>squelchd auth --write</code>. Nothing can be shredded until then.
        </div>
      ) : stats.enabled ? (
        <div className="authp-cardtext">
          Auth mail older than {stats.after_days} days moves to Gmail Trash
          automatically, where it stays recoverable for 30 days.
          {stats.pending > 0 && ` ${stats.pending} eligible right now.`}
        </div>
      ) : (
        <div className="authp-cardtext">
          Off. Turn it on and auth mail older than {stats.after_days} days moves
          to Gmail Trash — recoverable there for 30 days, never deleted outright.
          {stats.pending > 0 && ` ${stats.pending} would go on the next pass.`}
        </div>
      )}

      <div className="authp-cardfoot">
        {stats.shredded_recent} shredded in the last 30 days
        {stats.shredded_total > stats.shredded_recent &&
          ` · ${stats.shredded_total} all time`}
      </div>
    </div>
  );
}
