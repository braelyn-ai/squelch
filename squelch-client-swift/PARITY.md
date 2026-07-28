# squelch-client-swift — parity with `squelch-desktop`

A native macOS/SwiftUI rewrite of the Tauri + React client, targeting macOS 26
so it can use **real Liquid Glass** rather than CSS that imitates it.

**Status: functional parity reached.** Every view, surface, keybinding and
behavior below is implemented natively. The single remaining webview is the
sanitized-HTML email body, which is the one place a webview is genuinely
required. Open questions are recorded at the bottom.

---

## Build

```bash
./build.sh            # debug  -> build/Squelch.app
./build.sh release    # optimized
./build.sh run        # build + launch
```

`build.sh` drives `swiftc` directly and assembles the bundle. It is the
canonical build because `xcodebuild` on this machine intermittently fails to
load `IDESimulatorFoundation` — fatal to `xcodebuild` even for a macOS-only
target, while `swiftc` itself is unaffected. An `xcodegen` project is also
checked in and produces the same bundle when `xcodebuild` is healthy:

```bash
xcodegen generate
xcodebuild -project Squelch.xcodeproj -scheme Squelch -destination 'platform=macOS' build
```

Current state: **50 Swift source files, debug and release both build clean with
zero warnings.**

---

## The Liquid Glass work (the point of the rewrite)

The web build could only ever draw translucent rectangles: CSS
`backdrop-filter` cannot sample the native window backdrop, so every "glass"
card there was hand-drawn, including its fake specular hairline. Here the
material is AppKit's own.

| API | Where it is used | Why |
| --- | --- | --- |
| `.glassEffect(_:in:)` | `squelchGlass()` — every zone card, modal, panel, chip | The real material: samples the window backdrop, refracts it, draws its own specular edge |
| `Glass.regular` / `.clear` | panes vs. chrome (`GlassLevel`) | Rail and chips stay maximally see-through; content panes carry more presence |
| `.tint(_:)` | brand + tier tints on glass | Carries squelch blue **into** the material instead of painting a blue box on a grey one; semantic surfaces tint with their tier color |
| `.interactive()` | rail item, buttons, toasts | Material responds to press/hover |
| `GlassEffectContainer` | sidebar rail, sitrep body, toast stack, ⌘K bar, triage palette | Adjacent glass **merges and separates fluidly** — the signature behavior with no web equivalent |
| `.glassEffectID(_:in:)` + `@Namespace` | rail active indicator, ask bar, triage palette | Matched-geometry glass: the active capsule **flows** between rail icons; the palette **stretches** as its list narrows |
| `.buttonStyle(.glass)` / `.glassProminent` | every control in the app | No hand-rolled button backgrounds anywhere |
| `NSVisualEffectView` (`.underWindowBackground`) | `WindowBackdrop` | The layer the whole language sits on; window is non-opaque so glass has real content to refract |

**Identity, not stock grey.** The accent is the saturated squelch blue
`#2b7fd4` (`Palette.accent`), used only to mark state. The bundled **Newsreader**
serif (converted woff2 → ttf, registered via `ATSApplicationFontsPath`) appears
in exactly one place per screen — the sitrep hero headline, the thread subject,
the connect wordmark — so it reads as voice rather than wallpaper. Tier
semantics are fixed: coral overdue, amber deadline, green signal/synced,
periwinkle auth/sealed.

The window scrim is deliberately not thin. It is tuned against the hard case —
a dark busy wallpaper behind a light-theme window — because below that alpha the
backdrop bleeds through enough that light mode reads as a muddy dark one.

---

## Views

| View | Status | Notes |
| --- | --- | --- |
| Sitrep dashboard | **done** | Editorial hero (serif, spelled counts, greeting + name), For-your-eyes ranked list w/ in-place expander, Attention aggregate + deduped sender chips, Newsletters, status strip |
| Sitrep right rail | **done** | Calendar · Shipments · Banking · Receipts, each with its empty state (the rail never disappears) |
| Emails band list | **done** | Flat inbox, newest-first, importance meter, hover/keyboard selection model |
| Auth | **done** | In-focus panel w/ digit boxes, filter chips, Live/Archive sections, decision rail, shredder card |
| Rules | **done** | Dense table, client-side match counts, undo-first delete |
| Audit | **done** | Verb-phrase actions, resolved sender·subject, per-row undo |
| Usage | **done** | Per-category (stage1/stage2) sections + daily bars, assistant tally |
| Settings | **done** | General / Mail / Triage / Assistant / Account, sub-nav persisted |
| Connect (first run) | **done** | Serif wordmark on tinted glass |

## Surfaces

| Surface | Status | Notes |
| --- | --- | --- |
| Fullscreen thread viewer | **done** | Newest-first stack, single scroll surface, j/k + h/l queue nav, done+next |
| Email body renderer | **done** | `WKWebView`, five security layers (below) |
| Attachment strip | **done** | Lazy thumbnails, **native PDFKit preview** (stronger than the web build's `<embed>` + blob URL), size/stored states |
| Side panels (browse / search) | **done** | Conditional-mount so the modal context is never pinned |
| Compose review ceremony | **done** | edit → review → guard verdict → explicit override |
| Triage-fix palette (`v`) | **done** | Ranked, ambiguity shown not guessed |
| ⌘K ask-your-inbox bar | **done** | BYOK agent loop w/ citations |
| 2FA code modal | **done** | Auto-reveal, 30s countdown, copy pauses the timer |
| Shortcuts overlay | **done** | Grouped cheat-sheet |
| Toast stack + undo | **done** | 5s undo, click or `u` |
| Unsubscribe-violation prompt | **done** | One at a time, session-level suppression |
| Rule editor | **done** | tune / create / edit (create-then-delete so a failure can't lose the rule) |
| Process mode (`p`) | **done** | Card deck over live bands |
| Reveal panel | **done** | One-time, view-state only, never persisted |
| Triage debug overlay (dev) | **done** | Full triage row |
| Connection banner + daemon-down pane | **done** | Stale-data vs. nothing-ever-loaded, 401 vs. transport |

---

## Keyboard

The context system is ported 1:1 (`Keys/KeyDispatch.swift`), including the
parts that are easy to get subtly wrong:

- **Context stack** — `list` / `sitrep` / `modal` / `thread` / `global`. The
  active context composes with `global`, so 1–5 nav and ⌘K keep firing from
  inside a modal.
- **Registration order** — last registered wins within a context, which is how a
  nested overlay's Escape beats the surface underneath.
- **Two-pass matching** — an exact (case-sensitive) match always beats a
  case-folded one, so `A` (audit) and `a` (browse) coexist while a shifted
  letter with no exact binding still falls back to its lowercase sibling.
- **Meta matched exactly**, like shift — a `meta` binding fires only with ⌘
  held and a plain binding never fires while ⌘ is held, so ⌘[ / ⌘] never
  collide with bare `[` / `]`.
- **Declining handlers** — a handler returning `false` passes the key on. Used
  by the newsletter `e` (defers to For-your-eyes when nothing is hovered) and
  compose's `Enter` (lets the body field type a newline).
- **Input guard** — single-letter bindings are suppressed while a text field has
  focus unless they opt in via `allowInInput`.
- **Live handlers without re-registration** — views hand the registry a
  `BindingsBox` refreshed each render, so closures see current state without
  reordering the stack (re-registering would let a stale surface steal keys from
  an overlay above it).

Full keymap: digits 1–5 view nav · ⌘[ / ⌘] history · ⌘K ask bar · ⌘, settings · j/k/arrows ·
Enter open · r reply · e/d done · v fix triage · t tune · p process · a browse ·
/ search · u undo · T rules · A audit · g auth · `\` theme · `?` help · Esc
close · thread viewer h/l queue nav, u unsubscribe · rules n/e/x · browse ± noise.

Menu-bar equivalents (⌘1–5, ⌘[, ⌘], ⌘K, ⌘F, ⌘R, ⌘Z, ⌘,) exist for discoverability;
the registry remains the authority on dispatch semantics.

---

## Behaviors

| Behavior | Status | Notes |
| --- | --- | --- |
| 10s sitrep poll + refresh on focus | **done** | Shared in-flight guard so a manual poke never races a scheduled pull |
| Manual refresh (poke + double re-pull) | **done** | 400ms then 1600ms, matching the daemon's fire-and-forget poke |
| Thread prefetch / instant open | **done** | LRU + per-entry TTL; staggered warming so a fresh list never stampedes the daemon |
| Frame-height memory | **done** | A reopened message paints at final size on the first frame |
| Optimistic updates + 5s undo | **done** | Band removal with selection repair and a restore thunk on failure |
| Stale-data degradation | **done** | Banner when data exists, down-pane when nothing ever loaded |
| Light / dark / **auto** theme | **done** | Auto (follow system) is the native-app addition and the default |
| 2FA arrival detection | **done** | Persisted seen-set, 2-minute freshness window, silent first-run seeding |
| Auth countdown rings | **done** | 60s sweep, resumes correctly mid-flight |
| Favicon avatars (robot/brand only) | **done** | Human correspondents never resolved over the network; verdict cached across launches |
| Newsletter derivation | **done** | Pipeline `marketing` classification preferred; legacy heuristic only as a migration bridge |
| Tracker stripping | **done** | Conservative: tiny-declared, CSS-hidden, or a known endpoint from the deliberately short list |
| Quoted-history collapse | **done** | Same heuristic for text (native) and HTML (injected script) |

---

## Backend contract

Every `/client/*` route the desktop client used is implemented in
`Model/APIClient.swift`, with `Model/WireTypes.swift` mirroring the serde output
exactly. Routes: `updates`, `thread/{id}`, `search`, `stats`, `usage`,
`triage-config` (GET/POST), `audit`, `shipments`, `receipts`, `calendar`,
`banking`, `marketing`, `triage-debug/{id}`, `attachments/{id}`, `rules`
(GET/POST/DELETE), `unsubscribe`, `unsubscribes`, `unsubscribes/resolution`,
`sealed`, `sealed/{id}/reveal`, `updates/{id}/status`, `actions/{archive,label,send}`,
`refresh`, `retriage`, `shredder` (GET/POST), `shredder/run`, `triage-feedback`
(GET/POST).

Hardening carried over: 15s request timeout (30s for attachments), the token
only ever in an `Authorization` header, and error messages that never echo the
token or URL. Every wire enum decodes leniently, so a newer daemon adding a
tier/kind value cannot break an older client's page decode.

---

## Security posture

**Credentials.** Same keychain service (`squelch-desktop`) and same account
slots (`server_url`, `api_token`, `assistant_api_key`) as the Tauri shell, so an
existing install's credentials are picked up with no re-entry.

**The BYOK assistant key is unreachable from the view layer.**
`AssistantKeyStore.read()` is `fileprivate` to `Keychain.swift`, and `LLMProxy`
lives in that same file for exactly that reason. The key is never a parameter,
never a return value, never in an error message, and never in a log — the view
layer can only ask *whether* a key exists and *which provider* it routes to.
This mirrors the Rust `llm_complete` proxy, where the secret likewise never
crossed into JS. Provider is inferred from the key's real prefix, never from a
caller-supplied value.

**Email HTML rendering** (`Views/EmailWebView.swift`) — five independent layers,
each sufficient alone:

1. Server-side sanitization (ammonia) at ingest.
2. `allowsContentJavaScript = false` — page content cannot execute, whatever it
   contains. Our own measuring script is governed separately and still runs.
3. Injected CSP meta as the first child of `<head>`:
   `default-src 'none'; style-src 'unsafe-inline'; img-src <gate>` — no
   `script-src` at all. The gate is the per-message remote-image decision.
4. **Navigation refused.** The delegate allows exactly the initial in-memory
   load and cancels everything else; a link click is handed to the system
   browser via `Opener`, which re-guards to http/https only. (Note: the delegate
   closure must be `@MainActor @Sendable` or Swift treats the method as an
   unrelated near-miss, leaving the requirement unimplemented and WebKit
   defaulting to allow. This bit once during development and is now commented at
   the call site.)
5. Non-persistent `WKWebsiteDataStore` — no cookie jar to read or write.

Plus: tracking pixels stripped before render, `no-referrer` on the document,
remote images gated behind the existing preference (with images on demand, **no
network request is made** for unopened mail), and extracted links rendered as
native buttons rather than live anchors.

**Sealed mail.** Reveals are human-initiated only (the arrival flow's
auto-reveal is the one documented exception and is itself audited server-side).
Revealed bodies live in view state and die with the view — never persisted,
never lifted into the global store, never logged.

---

## Deliberate differences from the web build

These are improvements or native-platform equivalents, not gaps:

1. **PDF preview is native PDFKit**, not an `<embed>` fed by a blob URL. No
   webview, no blob CSP grant.
2. **No image byte-pinning cache.** The Tauri build fetched every image through
   Rust and inlined it as a `data:` URI because mutating a `srcdoc` reloads the
   iframe, which was the flicker it was fighting. There is no `srcdoc` here;
   WKWebView's own URL cache plus a warmed thread covers it.
3. **`system` theme option** added and made the default. A mac app that ignores
   the system appearance is wrong.
4. **`synced now ago`** → `synced just now`. The web build concatenated
   unconditionally and produced that phrase whenever the age token was "now";
   its own masthead already phrased it correctly.
5. **Menu-bar commands** for the ⌘-chords, so they appear in Help search and
   read as native. Deliberately *no* menu shortcut for `\` — a menu item with no
   modifier fires even while a text field has focus, which would make typing a
   backslash impossible. The registry's input guard handles it correctly.
6. No drag-strip hack: `isMovableByWindowBackground` plus a hidden title bar is
   the native equivalent of `data-tauri-drag-region`.
7. **`\` (theme) and `?` (help) are GLOBAL**, not inbox-only. The help overlay
   files them under "App", so pressing `?` on the sitrep should open help rather
   than do nothing.
8. **Toasts auto-dismiss after 6s.** The web build only removed them on click,
   so a long session accumulated them indefinitely. Undos keep their own 5s
   window and click target unchanged.
9. **`correctTriage` does not decode its response.** The only caller needs
   success/failure; coupling "the correction landed" to "the feedback row
   decoded exactly as expected" would show an error toast for a write that
   actually succeeded.
10. **Theme sync needs no pub/sub.** The web build hand-rolled a listener set so
    every mount tracked the theme; `@Observable` gives that for free.

---

## Open questions

1. **Keychain access prompt.** The app is ad-hoc signed (`codesign -s -`), so
   its code identity differs from the Tauri app that created the keychain items
   and macOS prompts before handing them over. Clicking "Always Allow" resolves
   it for that build. **During development it re-prompts after every rebuild**,
   because an ad-hoc signature's identity is derived from the binary itself and
   the binary changes each time. A real signing identity removes this entirely;
   that is a distribution decision, not a code one.

   This surfaced a genuine bug, now fixed: the keychain read ran on the main
   actor, so the prompt froze the whole UI until it was answered. All keychain
   I/O (`SettingsStore.loadAsync` / `saveAsync`,
   `AssistantKeyStore.statusAsync`) now runs on a background executor and the
   app keeps painting while the panel is up.
2. **OpenAI assistant keys** are accepted and stored, and `LLMProxy` routes them
   to the chat-completions endpoint, but the agent loop speaks the Anthropic
   message/tool-use shape and refuses non-Anthropic keys with a clear message —
   exactly as the web build did. Wiring the OpenAI tool-call shape is a small,
   separate piece of work.
3. **Newsletter hero fill color.** The web build sampled the hero image's
   dominant color to tint the card well. Here the thumbnail sits on a neutral
   well. Purely cosmetic; the sampling pass can be added with `NSImage` +
   `CIAreaAverage` if wanted.
4. **`xcodebuild` is unreliable on this machine** (see Build). `build.sh` is the
   supported path; the `.xcodeproj` is kept in sync for when the toolchain is
   repaired.
