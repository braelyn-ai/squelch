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


---

# Core product principle: abstract over single emails (2026-07-09)

**The goal is for users to open a single email as seldom as possible.** The UI
abstracts over individual emails — one_lines, bands, deadline chips, digests,
and actions-from-the-list carry the load. Drill-in (reading the actual email)
is the escape hatch, reserved for when it genuinely matters that a human reads
that specific email. Evaluate every feature against: "does this reduce the need
to open emails, or does it drag the user back into reading mail?" The
email-rendering work exists for the escape-hatch case, not as a primary surface.

---

# Aesthetic law (revised 2026-07-09 — supersedes "dark, dense, terminal-adjacent")

## Aesthetic law (current)

**Light-first, friendly, keyboard-first retained; dark mode via toggle.**

- **Light mode by default.** A warm near-white surface, high-contrast dark
  text. Dark mode is opt-in via a header sun/moon toggle (and the `\` keybind),
  persisted in `localStorage` under `squelch-theme` and applied before first
  paint (inline script in `index.html`) so there's no flash.
- **Friendly, not terminal.** Body copy is a native sans stack
  (`system-ui`/`-apple-system`/…). Monospace is reserved for genuinely tabular
  or data fragments: importance scores, timestamps, match patterns/globs.
- **Soft cards over ruled lines.** The three bands (Standing / Since last check
  / Still open) are cards with a subtle border, radius, and background
  differentiation. Rows have real hover states; buttons are real buttons with
  hover/active states and comfortable padding.
- **The squelch line stays.** The noise divider is the product metaphor — it
  remains a distinct visual element (a soft gradient rule), not an ASCII-style
  dash.
- **Density is still deliberate.** This is a power tool, not a marketing site.
  Spacing and line-height loosened, but the sitrep still reads at a glance.

> **Note (user decision, 2026-07-09):** This light-first direction **supersedes**
> the original "dark, dense, terminal-adjacent" aesthetic law. The app should be
> less terminal-looking and more user-friendly. Dark mode is preserved as a
> faithful translation of the old palette, now reachable through the toggle
> rather than being the only skin. Everything below (keyboard-first, action
> feel, security posture) is unchanged.

### Theme system

All colors flow through CSS custom properties on `:root`. Two palettes are
selected by a `data-theme="light"|"dark"` attribute on `<html>`:

- `:root` / `:root[data-theme="light"]` — the light palette (default).
- `:root[data-theme="dark"]` — the dark palette (translated from the original).

Tier semantics are preserved and tuned per background: past-due red, deadline
amber, signal green-ish, noise muted, sealed a distinct lock purple with a
subtle soft-fill treatment. See `src/styles/global.css` for the variable
definitions and `src/state/theme.ts` for the runtime toggle/persistence.

---

## Keyboard-first

Every action has a key. The list context owns
`j`/`k`/`Enter`/`r`/`e`/`d`/`t`/`p`/`a`/`T`/`/`/`u`; modal and input contexts
override as needed. `\` toggles the theme, `?` opens the shortcuts overlay.
Typing into an `<input>`/`<textarea>` suppresses single-letter list bindings
automatically. All bindings flow through the single keymap registry
(`src/keys/useKeys.ts`) so they can't collide silently.

---

## Action feel

Actions get friction proportional to their reversibility.

- **Undo-first** for archive / done / label / rule-delete: the forward action
  fires immediately and a 5s toast (`u` or click) takes it back.
- **Send is the one irreversible action** and gets a two-step compose → review
  ceremony with an outbound-guard verdict before it fires.
- **Reveal of sealed content** is explicit, one-time, audited server-side, and
  never persisted client-side.

---

## Security posture (unchanged)

- HTML email renders in a hard-sandboxed, script-less, opaque-origin iframe with
  a strict CSP; remote images are blocked until per-message opt-in.
- The API token is never logged. Sealed bodies are never lifted into the global
  store or written to disk.

---

# Copy guidelines (2026-07-09)

Rules for all **user-facing** copy — section headers, empty states, tooltips,
aria labels, button/knob labels, the shortcuts overlay, panels. Internal code,
comments, wire-level enum values, and API paths are exempt.

- **No internal jargon.** The word **"sealed"** never appears in the UI — it's an
  implementation detail. Auth-related mail (the `/client/sealed` metadata) is
  presented with auth-centric language: it lives in a dedicated **Auth** tab
  (key `g`) listing login codes, password resets, sign-in alerts and
  verifications, with the existing one-time reveal flow. A compact
  "N auth messages" pill/chip (header + noise line) notices new auth mail and
  opens the tab. Map `sealed_kind` → friendly labels via `lib/authCopy.ts`
  (`otp`→"Login code", `password_reset`→"Password reset",
  `magic_link`→"Sign-in link", `login_alert`→"Sign-in alert",
  `verification`→"Verification").
- **"squelch" is the product name only.** Never use it as a verb or noun in copy.
  "squelched" → "filtered out" / "muted"; the min-importance knob is
  "Noise filter: N", not "squelch: N". Rule dispositions read "surface / mute /
  filter" in the UI (via `DISPOSITION_LABEL`) while the wire values are
  unchanged. Literal CLI command names in empty states (`squelchd auth`,
  `squelchd run`) stay — they're commands, not copy. The app name/title
  "squelch" stays.
- **Age badges only for genuinely aging items.** The STILL OPEN aging badge
  ("← 2 WEEKS") appears only once an item is past the 48h threshold
  (`isAging`/`AGING_THRESHOLD_H` in `lib/format.ts`), and only in the STILL OPEN
  band. Escalating visual weight still ramps for multi-day/week items; fresh open
  rows read calm and show the plain relative time. New rows never carry it;
  Standing rows keep their deadline chip.
- **No remote avatar/asset services, ever (privacy).** Sender avatars are
  derived locally — initials from the display name (fallback: address
  local-part) over a deterministic, theme-aware color palette hashed from the
  address. Never fetch Gravatar, favicons, or any remote avatar/asset: it would
  leak the correspondent graph off-device.

