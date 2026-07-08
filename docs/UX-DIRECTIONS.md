# Desktop client — UX directions

Design session record (2026-07-08). Direction D ("Sitrep") is the chosen chassis; A–C are
documented for future development — each survives as a mode or a rejected-for-a-reason.

## Chosen: D — "Sitrep"

**Thesis:** the agent (OpenClaw) watches the inbox continuously; the human drops in
intermittently. The UI answers *"what changed since anyone last looked, and what's been
quietly left behind"* — not "what happened today." Not a day brief; a situation report.

```
┌ squelch ── sitrep ────────────── last checked: 4h ago (via OpenClaw) ┐
│ ⚠ STANDING                                                          │
│   🔴 PG&E $142 · 4 DAYS PAST DUE          🟡 Verizon autopay · Jul 11│
│ 🆕 SINCE LAST CHECK (3)                                              │
│   92  Sarah Chen · redlines need sign-off               [r][e][d]    │
│   78  ci · deploy #4412 failed on main                               │
│   🔒 Google · login code · 12m                                       │
│ ⏳ STILL OPEN — aging, escalating                                     │
│   84  Alex · invoice question              ← 2 WEEKS, 3 nudges       │
│   71  Dad · weekend plans?                 ← 5 days                  │
│ ◌ 312 squelched since last check · [a]ll mail · [T]rules             │
└──────────────────────────────────────────────────────────────────────┘
```

**Bands (three different clocks):**
- **STANDING** — deadlines/past-due. Immune to squelch level and to time; never rotates
  out until resolved.
- **NEW** — delta since last check *by any door*. Requires the server-side seen-ledger:
  items are stamped `surfaced_at` when they flow out via MCP `get_inbox_updates` or
  `/client/updates`. "New" = never surfaced through anything. If the agent already told
  the human, the desktop doesn't perform surprise.
- **STILL OPEN** — attention lifecycle `new → open → done`. Actions (reply/archive)
  auto-resolve; `d` dismisses explicitly; unresolved items age here sorted by
  age × importance with escalating visual weight. The anti-rot mechanism.

**Server prerequisites (additive):** `status` + `surfaced_at` on triage rows;
`since`/`status` params on `/client/updates`; MCP read path stamps the ledger.

**Keys:** j/k nav · r reply · e archive · d done · p process-mode · a browse-all ·
t tune sender · T rules audit · squelch knob on the noise divider.

## Action feel (locked): undo-first

- Archive/label fire instantly on keystroke (keystroke = consent; client sends the API's
  required `confirm: true` automatically). 5s undo toast; archive reverts by re-adding
  the INBOX label.
- **Send** gets ceremony instead: compose → review pane showing the outbound-guard
  verdict + recipients → second Enter fires. Friction proportional to irreversibility.
- Rejected: confirm-everything (nag habituation is its own failure mode).

## Documented alternatives

### A — "Triage deck" (survives as process-mode, `p`)
One card at a time, single-key verbs, empty-queue-is-done. Fastest processing; rejected
as chassis because it makes email a treadmill and hides the standing situation.

### B — "Radio console" (survives as browse-all, `a`)
Persistent ranked board split by a draggable squelch line; deadline strip pinned;
detail/action pane right. The TUI's grown-up sibling. Great glanceability; rejected as
chassis because it centers *the mailbox* rather than *the situation* — and the agent
already watches the mailbox.

### C — "Day brief" (superseded by D)
Digest-first dashboard (deadlines timeline, needs-reply stack, per-rule digests).
Rejected: assumes email is a daily ritual; squelch's whole premise is that it isn't.

## Standing constraints (from the architecture)

- Client talks only to the human door (`/client/*`, bearer-authed, on baddiebox over
  tailnet). Stack: **Tauri 2 + React** (TypeScript, Vite). Keep the Rust side of the
  Tauri shell thin — it's a window + secure token storage; all intelligence stays
  server-side behind `/client`.
- Sealed mail: metadata in lists (lock chip); body only via explicit per-message reveal
  (audited server-side, `Cache-Control: no-store`, never persisted client-side).
- Writes exist only here (human door). The agent door has none.
