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
- **Sender avatars are local initials for humans; favicons for robots only
  (privacy).** By default sender avatars are derived locally — initials from the
  display name (fallback: address local-part) over a deterministic, theme-aware
  color palette hashed from the address. **Human correspondents NEVER trigger a
  network fetch** (no Gravatar, no favicon): resolving a human's avatar remotely
  would leak the human correspondent graph off-device, and that must never
  happen. The **only** exception is **robot senders** — automated mailboxes whose
  local-part matches known service shapes (`no-reply`, `notifications`, `alerts`,
  `billing`, `receipts`, `support`, `security`, `newsletter`, … see `ROBOT_LOCAL`
  in `lib/avatar.ts`, matched on the segment before any `+tag`). For those we show
  the sending domain's favicon from DuckDuckGo's icon service
  (`https://icons.duckduckgo.com/ip3/<base-domain>.ico`). The base domain is
  derived by peeling one leading mail-ish subdomain label
  (`mail.`/`email.`/`notifications.`/… , two-label minimum). This is a **one
  cached hit per domain**, not a per-message beacon: verdicts (`ok`/`failed`) are
  cached per-domain in memory + `localStorage` (`squelch.favicons`), so each
  domain resolves at most once and an `img` error / blank response falls back to
  the initials avatar forever. A robot mailbox names a service, not a person, so
  this leaks nothing about who a human talks to. Favicons are round-cropped with a
  subtle border so light logos read on the light theme. (CSP: `img-src` allows
  `https://icons.duckduckgo.com`.)
- **Icons, not emojis.** User-facing glyphs use `lucide-react` icons, never emoji
  or dingbat characters (emoji render inconsistently across platforms/themes and
  ignore our color tokens). Import icons individually for tree-shaking; size them
  16–18px inline and let them inherit `currentColor` so they follow the theme and
  the surrounding text tone automatically. Keep tier/state COLOR semantics via the
  existing CSS vars — an icon may replace a colored dot where it reads better, but
  the color meaning stays. Current mappings: auth pills/chips → `KeyRound`; theme
  toggle → `Sun`/`Moon`; reveal banner → `Lock`; bands → `TriangleAlert`
  (Standing) / `Sparkles` (Since last check) / `Hourglass` (Still open); auth
  kinds (`AuthView`/`RevealPanel`, via `authKindIcon`) → `KeyRound` (login code) /
  `LockKeyhole` (password reset) / `MailCheck` (sign-in link) / `ShieldAlert`
  (sign-in alert) / `BadgeCheck` (verification). Keyboard-notation characters in
  `<kbd>`/hints (`↵`, `⌘`, `→`, `\`) are keycaps, not emoji, and stay.

_(Avatar-favicon + icon guidance added 2026-07-09.)_

---

# Sidebar navigation + Sitrep as the abstracted dashboard (2026-07-09)

**User decision, implementing the "abstract over single emails" principle
structurally.** The app gains a persistent left nav and the Sitrep name is
redefined: it is now the *fully-abstracted dashboard* (the default surface on
launch), and the original band-list-of-email-rows chassis lives on unchanged as
the **Emails** view.

## Sidebar (icon rail)

A slim ~52px icon rail (`src/components/Sidebar.tsx`) routes the primary views,
theme-aware in both palettes, with hover tooltips and an active-state accent:

1. **Sitrep** (`Gauge`) — the abstracted dashboard, default on launch.
2. **Emails** (`Mail`) — the band list (formerly the whole "sitrep" chassis).
3. **Auth** (`KeyRound`) — login codes / password resets / sign-in alerts.
4. **Rules** (`SlidersHorizontal`) — sender rules.
5. **Audit** (`ScrollText`) — agent & app actions.

**Number keys 1–5 switch views** (registered in the `global` KeyContext in
`App.tsx` so they fire from every view, including modal panels; digits were
otherwise unbound). The rail order and the key mapping share one source of
truth: `MAIN_VIEWS` in `src/state/store.ts` (`store.activeView` / `setView`).

## Routed views vs. side panels

Auth / Rules / Audit were **promoted from side panels to routed main views**
(cleaner with a persistent rail). Their historical keybinds still work: `g`
(auth), `T` (rules), `A` (audit) now *route to the view* instead of opening a
panel. The three inner components are reused **verbatim** — a host
(`src/views/RoutedView.tsx`) pushes the `modal` KeyContext they already register
into, so their `j/k/n/e/x/r/Enter` bindings light up unchanged.

**Side panels / overlays are retained** for the drill-in / ceremony surfaces:
thread drill-in, browse-all (`a`), search (`/`), reveal, rule editor, compose,
and process mode. The `SideView` union is trimmed to `thread | browse | search`.

## Sitrep — the abstracted dashboard (zero email rows)

`src/views/SitrepView.tsx` is rebuilt as four soft-card zones (light/dark aware,
`src/styles/sitrep-dash.css`), with **no individual email rows** by design:

- **(a) Obligations** — deadline cards from `band=standing`: avatar + sender,
  amount (parsed best-effort from `one_line`; falls back to the one_line when
  absent) + due-date chip, past-due state visually loud. Actions: **done**
  (`d` / button, the existing status endpoint via `dispatchDone`) and **view**
  (hands off to the Emails view with that item selected, via `viewInEmails`).
- **(b) Attention** — aggregate only: "N new since <relative last check>"
  (`stats.bands.new` sense + `last_surfaced_at`) plus deduped sender chips from
  `band=new`; the zone clicks through to Emails.
- **(c) Aging** — from `band=open` filtered to age > 7d: "N items sitting over a
  week" + per-item sender + duration ONLY (no subjects/one_lines — abstraction);
  each row clicks through to Emails.
- **(d) Status strip** — auth chip (→ Auth), last sync/check relative time,
  today's triage cost (`stats.stage2.est_cost_usd_today`, shown "triage: $0.02
  today"; the `stage2` stats field is optional and rendered only when present),
  and a rules-count chip (→ Rules).

Each zone has its own empty state ("Nothing standing — clear board.", etc.).
Sitrep owns a minimal `sitrep` KeyContext: `j/k` move between obligation cards,
`d` marks the focused one done, `Enter`/`v` view it in Emails; the global 1–5
nav composes over it. All existing Emails-view behavior, the dispatchCore
two-pass semantics, the SidePanel conditional-mount pattern, and the themes are
preserved.

## 2FA — "present, don't read" (2026-07-09)

**Thesis:** people don't *read* login-code emails — they request a code, glance
at it, and move on. The client should present the code, not the message. Under
`gmail.readonly` we can't (and don't want to) mark anything read in Gmail; the
**seen-set + the ring's expiry ARE the read state**, and the Auth tab stays a
calm history rather than an unread pile.

- **Arrival detection** (`src/state/useAuthArrival.ts`) rides the existing 10s
  sealed-metadata poll (`useSitrep`). A persisted seen-set
  (`localStorage["squelch.auth-seen"]`, capped ~200 ids) records every sealed id
  we've processed. On the first tick of a session the whole current backlog is
  seeded **silently** so history never fires. Thereafter a sealed entry that is
  *not* in the set and whose `received_at` is within ~2 minutes counts as a fresh
  arrival and fires the flow **once** (late-arriving history is recorded as seen
  but stays quiet).
- **Ring timer** (`src/components/AuthRing.tsx`, store `authRings` + `RING_MS`):
  the auth rail icon gains an SVG countdown ring — a 60s `stroke-dashoffset`
  sweep (pure CSS keyframes, `--ring-circ` = circumference), rotated to start at
  12 o'clock — that removes itself `onAnimationEnd`. Both themes; stroke is the
  `--lock` accent. If the app mounts mid-sweep the animation is shortened to the
  remaining time so it never over-runs. The ring fires for **every** auth kind
  (resets and sign-in alerts included).
- **Code modal** (`src/components/AuthCodeModal.tsx`) fires for code kinds only
  (`otp` / `login_code` / `verification`; resets/alerts get the ring but no
  modal). On arrival we **auto-reveal** the body (`api.revealSealed`, which the
  server audits — every auto-reveal writes an audit row, same as a manual one)
  and extract the code client-side (`src/lib/authCode.ts`): prefer a 4–8 digit
  run **closest** to a code word (code / verification / OTP / one-time / pin,
  ±80 chars), else the longest standalone 4–8 digit run; space/hyphen-split codes
  are re-joined. If nothing is found the modal shows sender + kind with an **Open
  Auth** button instead of a code. The modal is a conditional-mount overlay in
  its own `modal` KeyContext (canonical pattern): sender avatar +
  `senderDisplayName` + kind label, the code **huge** (mono, letter-spaced),
  **Copy** (`c`; `navigator.clipboard` with a hidden-textarea fallback for the
  Tauri webview — no new plugin needed) and **Dismiss** (`Esc`/`Enter`). The code
  lives in store state only and is dropped from the queue on dismiss; multiple
  arrivals queue **newest-first**.

## Newsletters — the rule-onboarding surface (2026-07-09)

A fifth Sitrep zone (after Aging, before the status strip;
`src/lib/newsletters.ts` + `NewslettersZone` in `SitrepView.tsx`). It turns the
recurring noise pile into the place the product **teaches the rule (Minga) flow**
— "choose what you want to see" — instead of nagging.

- **Data:** fetches `tier=noise` updates (`limit 200`) plus `listRules()`,
  filters to the last 7 days client-side (the wire `AttentionUpdate` carries no
  `received_at`, so we date on `surfaced_at`), and groups by sender address.
- **Heuristic** (calibrated against `squelch-core/src/triage` rung-5 reason
  strings, read-only): a sender qualifies when its reason matches the
  newsletter/bulk shape — the engine's exact literal is
  `"bulk/list mail (unsubscribe footer)"` (we substring-match `"unsubscribe
  footer"`, with an `unsubscribe|newsletter|marketing|digest|…` backstop) — **or**
  it's a robot/brand sender (`isRobotSender`/`isBrandSender`) with ≥2 noise
  messages in the window. Senders whose window is **entirely receipts** are
  excluded via the distinct `"order confirmation / receipt"` reason (plus an
  `order|receipt|shipment|tracking` backstop); a sender with both newsletter and
  receipt evidence still qualifies.
- **Cards:** avatar + `senderDisplayName`, "N this week", latest `one_line`
  truncated as the summary. **Rule state:** if a rule matches the sender
  (client-side glob match of `match_pattern` against the address, most-specific
  wins) the card shows a chip — disposition label + truncated `want_text` —
  that opens the RuleEditor to **edit** it. If **no** rule matches, an inviting
  CTA ("Choose what you want to see →") opens the RuleEditor **prefilled**:
  `*@domain` (favicon-normalized so mail subdomains collapse to the brand),
  disposition **filtered** preselected, want field autofocused — the Minga first
  choice being "stop showing me these, unless…". The zone's empty state is
  "No newsletters this week."
- **Keyboard:** cards are focusable (`tabIndex`); `Enter` on a focused card opens
  the editor (edit or create), same as a click — folded in without disturbing the
  `sitrep` context's obligation `j/k`.

Both flows preserve the dispatchCore/keymap/modal patterns, the ruleEditorBus
overlay contract (extended with optional `disposition`/`want`/`pattern` prefill),
and the fresh dashboard structure.


---

## Visual identity — "Precision instrument" (committed 2026-07-09)

This supersedes the earlier generic light-theme note. The brand anchor is the
app icon: a **machined brass radio squelch knob**. The whole UI is the
instrument that knob belongs to — warm paper, hairline structure, engraved
labels, mono readouts, and exactly one accent: **brass**. The failure mode of
this direction is steampunk; **restraint is the spec.**

### The restraint rule (load-bearing)

**Brass (`--brass` `#a8834a`) marks STATE, never decorates, and stays under
~10% of any screen.** It is the focus ring, text selection, the active sidebar
item, the primary button fill, the noise-filter knob/slider, and the 2FA
countdown ring. Everything else is warm ink on warm paper with hairline borders.
Signal/deadline/past-due tiers keep their own semantic hues (below) — they are
not brass. If a screen looks brassy, it's wrong.

### Palette

Two palettes, one CSS-var system, keyed on `data-theme` (`src/styles/global.css`).
Light is default; dark is a translation, not a redesign.

| Token | Light | Dark | Role |
| --- | --- | --- | --- |
| `--bg` | `#f7f4ee` | `#17150f` | warm paper / warm near-black base |
| `--bg-raised` | `#fffdf9` | `#201d16` | raised card |
| `--bg-sunken` | `#f1ede4` | `#131109` | wells (readouts, code) |
| `--bg-row` | `#efece4` | `#262218` | hover row |
| `--bg-row-sel` | `#f0e6d4` | `#322a1b` | selection — faint brass wash |
| `--border` | `#e3ded2` | `#322d22` | hairline (1px, low contrast) |
| `--border-strong` | `#d2ccbd` | `#46402f` | stronger hairline |
| `--fg` | `#26231e` | `#ece6d8` | ink |
| `--fg-dim` | `#5f5a50` | `#a49d8c` | dim ink |
| `--fg-faint` | `#918b7d` | `#6f6857` | faint ink (labels) |
| `--brass` | `#a8834a` | `#c19a5b` | **THE accent** — state marker |
| `--brass-soft` | `#f0e6d4` | `#322a1b` | brass wash (active/selection) |
| `--brass-line` | `#c8ab7d` | `#6e5a34` | brass-tinted hairline (squelch line) |
| `--accent` (signal) | `#5c7a4e` | `#8aa374` | desaturated bronze-green |
| `--red` (past-due) | `#b3392f` | `#d9635a` | overdue |
| `--amber` (deadline) | `#a8710a` | `#d29a4a` | warm amber |
| `--lock` (sealed/auth) | `#7a5a86` | `#b295bd` | auth surface — distinct, tuned in |

Shadows are nearly absent (`--shadow-card` ~ 1px @ 3% alpha); **hairlines carry
the structure.**

### Type

Bundled **locally** via `@fontsource` (imported in `main.tsx`, weights 400/500/600)
— **no font CDNs** (privacy). `--sans` = **IBM Plex Sans** (body); `--mono` =
**IBM Plex Mono** for all data: importance meters, timestamps, patterns/globs,
2FA codes.

- **Body:** Plex Sans, 13.5px / 1.55.
- **Engraved-label** (zone/band/section headers): Plex Sans **600**, UPPERCASE,
  **+0.08em** tracking, small (**11px**), `--fg-faint`. Tokenized as
  `--label-size` / `--label-track` and applied to `.band-head`, `.zone-head h2`,
  `.field label`, `.reveal-card .banner`, `.sc-group-title`.
- **Data / mono:** Plex Mono, tabular-nums. The **2FA code readout** is the
  showpiece — 40px Plex Mono, `0.28em` letterspacing, sunken panel with a brass
  hairline top-edge so it reads like an instrument display.

### Components

- **Radii:** tightened to **4–6px** everywhere (`--radius` 6, `--radius-sm` 4) —
  machined, not rounded.
- **Buttons — machined-flat:** 1px border, a subtle top-light gradient
  (`--btn-topfill`) so the face reads slightly domed, `:active` translates down
  0.5px like a pressed keycap. `.primary` = brass-filled (the one loud control
  per surface).
- **Importance meter:** shown as a **5-block glyph** `▰▰▰▱▱` in Plex Mono
  (`importanceMeter()` in `lib/format.ts`; filled = `ceil(n/20)`), colored by
  `importanceColor()`, with the numeric value as the title/aria text. Used
  consistently in **email rows** (`.row .imp.meter`, replacing the bare number)
  **and obligation cards** (`.ob-meter`).
- **Noise divider (the squelch line):** a **brass-tinted hairline gradient**
  (`--squelch-line` via `--brass-line`). The noise-filter knob button + the
  browse `range` slider (the literal squelch knob) go brass.
- **Focus / selection:** brass outline (`2px`) + `::selection` brass wash.
- **Cards:** shadows near-zero; hairline borders only.

### Window chrome (macOS)

`tauri.conf.json`: `titleBarStyle: "Overlay"` + `hiddenTitle: true` (traffic
lights stay, native title hidden). A slim non-interactive **drag strip**
(`.drag-strip`, `data-tauri-drag-region`) covers the top `--chrome-top` (28px)
of the window but stops `280px` short of the right edge so it never swallows the
Emails-header control cluster; header buttons also opt out with
`-webkit-app-region: no-drag`. The sidebar rail gets `--chrome-top` top padding
so the rail icons clear the traffic lights.

### Navigation history

Browser-style **back / forward** over the routed views: **⌘[ back / ⌘] forward**
(registered on the `global` key context in `App.tsx`, `allowInInput` so chords
work with a field focused). The store (`state/store.ts`) holds a `history`
stack of `{ view, selectedId }` entries + a `historyIndex` cursor (cap
`HISTORY_CAP = 50`). `setView` and `viewInEmails` **push** (truncating any
forward branch, deduping an identical current entry, dropping oldest past the
cap); `goBack`/`goForward` move the cursor and **re-apply** the entry's view +
selection without re-pushing. `disconnect` resets history to the launch anchor.

The keymap gains **meta-modifier** support: `KeyBinding` grows an optional
`meta?: boolean`, and `dispatchCore` matches meta like `shift` — the event's ⌘
state must **equal** the binding's, so a bare key never fires under ⌘ and a ⌘
chord never fires without it (two-pass exact/case-fold semantics preserved).
`normalize` no longer bakes `meta` into the key string. Covered by new tests in
`dispatchCore.test.ts` (39 pass total).
