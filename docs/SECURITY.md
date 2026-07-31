# Squelch security model

One section per surface: the **invariant**, the **enforcement point** (`file:symbol`
— the line that actually holds it), and **what a maintainer must not break**.
Verified against the tree on 2026-07-30; stale source comments are flagged, not
trusted.

## 1. HTML sanitization at ingest

**Invariant.** `messages.body_html` never contains active content: no `<script>`, no
`on*` handlers, no `javascript:`/`data:` URLs, no forms, no nested browsing contexts,
no `<meta>`/`<link>`/`<base>`.

**Enforcement.** `squelch-core/src/sync/html.rs:sanitize_email_html` — a closed
`ammonia::Builder` allow-list, run once at ingest before storage; pure, no I/O,
fixture-tested in the same file. Four deliberate deviations from ammonia's
defaults: `url_schemes` narrowed to `{http, https, mailto}`; `<style>` kept with
CSS verbatim (`rm_clean_content_tags` then `add_tags` — that order, or ammonia
panics); `style`/`class`/`id` added to the generic attribute set; `link_rel`
pinned to `noopener noreferrer`.

**Do not break.**
- Never widen `url_schemes`; `data:` coming back re-admits inline
  `data:text/html` payloads and is the only reason `data:` image URIs drop today.
- Never add a tag that creates a browsing context, submits, or loads a document
  (`iframe`, `object`, `embed`, `form`, `link`, `meta`, `base`).
- `<style>` is the ONLY raw-text tag allowed. Ammonia escapes `<` in text *and* in
  attribute values, so every surviving `<img` is a real tag — the client's regex
  image passes (§3) depend on that for correctness and for linear runtime.
- HTML never crosses the agent door; MCP serves flattened text only.
- `body_html` is **baked at ingest** — a sanitizer change does not retroactively fix
  stored mail. Delete the `sync_state` `history` row and restart `squelchd` to
  re-ingest (triage is preserved).

## 2. Rendering sandbox

**Invariant.** A body renders with no script execution, no network except the image
proxy, no navigation, no cookies, nothing on disk.

**Enforcement.** `squelch-client-swift/Sources/Squelch/Views/EmailWebView.swift` —
five independent layers, each sufficient alone. Layer 1 is §1 above; the rest,
numbered as the source comments number them:

2. `allowsContentJavaScript = false`: page script cannot run at all. Our own injected
   `WKUserScript` is governed separately and still runs.
3. CSP injected as the FIRST child of `<head>` by
   `Coordinator.document(html:allowRemote:)`: `default-src 'none'; style-src
   'unsafe-inline'; img-src squelch-img: data:` (or `img-src data:` alone when
   remote images are off), plus `<meta name="referrer" content="no-referrer">`. No
   `script-src`, and no `http:`/`https:` anywhere in the policy.
4. Navigation refused. `Coordinator.decideNavigation` allows exactly the loads we
   started (`pendingOwnLoads` is a counter, not a flag: `loadHTMLString` is async,
   so two loads would otherwise race one permission) and cancels everything else; a
   `linkActivated` URL goes to `Opener.open`, which re-guards to http(s), and
   `FrameRelay.webView(_:createWebViewWith:…)` returns nil — no popups.
5. A shared **non-persistent** `WKWebsiteDataStore`; `baseURL: nil` for an opaque origin.

**Do not break.**
- The delegate signature
  `webView(_:decidePolicyFor:decisionHandler: @escaping @MainActor @Sendable …)`
  must match exactly. A near-miss leaves the requirement unimplemented and WebKit
  silently defaults to **allowing every navigation**.
- `FrameRelay` with no owner keeps returning `.cancel`: a pooled frame is unowned,
  and nobody is entitled to navigate it.
- `WebFramePool.Key` is `(message, allowRemote, document)` — all three, which is what
  makes a pool hit a byte-identical document under an identical CSP. Dropping
  `allowRemote` reuses a permissive frame for a strict render; dropping `message`
  leaves "one sender's mail in another's frame" to a hash collision.
- A body with **no message id** (the sealed reveal) is never pooled —
  `Coordinator.release` refuses it because `loadedPoolKey` is nil. Keep
  `WebFramePool.discard` unwiring the relay too: the content controller retains it,
  so merely dropping the frame leaves a live callback target.

## 3. Image loading and trackers

**Invariant.** Remote images may load, but only through our own audited fetch path,
never with cookies or a referrer, and never for a reference we did not mint.

**Current behavior — VERIFIED, and not what old comments claimed.** Remote images
**load by default**: `Lib/Prefs.swift` registers `squelch.pref.loadRemoteImages =
true`, `SettingsView.swift:MailSection` exposes it as Mail → images → *Always* /
*On demand*, and `EmailWebView.allowRemote == prefs.loadRemoteImages || optedIn`.
There is no per-message `i` keystroke gate — that was the retired desktop client.
On *On demand* `img-src` collapses to `data:`, an opt-in bar appears, and **no
request is made** (the proxy scheme is absent from the policy, so the document
refuses it before the handler is reached).

**Enforcement.**
- `Lib/Trackers.swift:strip` drops `<img>` elements on strong signals only: both
  declared dimensions ≤ 2px, `display:none`/`visibility:hidden`, or a short
  path-scoped known-endpoint list. The bias is deliberately asymmetric — a missed
  tracker is one opaque referrer-less GET, a false strip visibly breaks the mail.
  **No reserialization**: every non-`<img>` byte survives verbatim, because a DOM
  round-trip would rewrite markup ammonia already vetted.
- `Lib/ImageProxy.swift:rewrite` rewrites every http(s) image reference — `<img
  src>`, `style="…"` `url()`, `<style>` block CSS — to
  `squelch-img://local/<hmac>?u=<encoded>`. `@import`/`@font-face` are skipped on
  purpose: they load no image, are dead under `default-src 'none'`, and rewriting
  them would hand the launch warmer requests the reader never made.
- `Model/ImageSchemeHandler.swift` is the only responder and
  `ImageProxy.original(from:)` its only parser: it requires this launch's HMAC and
  an http(s) target. The key is 256 random bits per process, never written down, so
  a signature cannot be replayed across launches. Provenance, not secrecy — mail can
  spell `squelch-img:` itself in a kept `<style>` block, which `rewrite` neuters to
  `squelch-img-blocked:` before minting anything.
- `Model/ImageStore.swift` owns the fetch: ephemeral session, no cookies, empty
  referrer, `image/*` only, redirects re-guarded per hop, files named `sha256(url)`
  and a manifest holding **no URLs** — a directory listing the reader's mail URLs
  would be a readable index of their correspondence.

**Do not break.**
- The order in `EmailWebView.Prepared.make`: `Trackers.strip` → `ImageRepeats`
  dedupe → the `hasNetworkImages`/`extractLinks` reads → `ImageProxy.rewrite`
  **last**. Trackers first so a pixel can never be the "first occurrence" that
  suppresses a real image; rewrite last because after it nothing recognises a
  reference as remote.
- Never put `http:`/`https:` back in `img-src`. That one change turns every missed
  rewrite from a broken-image glyph into an un-proxied request, and reopens the
  CSS-background gap `html.rs` used to document as a KNOWN TRADE-OFF.
- Keep the signature check: without it a hand-written `url(squelch-img://…)` in a
  kept `<style>` fetches a tracker while the UI says the mail has no remote content.
- **Stale comment, flagged:** `Lib/Trackers.swift`'s header still calls the CSP
  `img-src http:/https:/data:` — that predates `ImageProxy`; the live policy is
  `squelch-img: data:`. Its argument still holds (a host-agnostic CSP cannot tell
  a tracker from a hero image, so the strip pass is the only seam that can); only
  the parenthetical is wrong.

## 4. Sealed mail and the two-door split

**Invariant.** Auth mail (OTPs, password resets, magic links, login alerts,
verification) never reaches an LLM, never crosses the agent door, and is never
queryable as normal mail — not even for an instant.

**Enforcement, in order.**
- **Detection first.** `squelch-core/src/sync/ingest.rs` calls
  `triage::seal::detect_sealed` right after parse and returns early — before Stage-1,
  before shipment/receipt/calendar extraction, before anything else reads the body. It
  biases to **recall over precision**: a false seal only hides benign mail from the
  agent, a false negative leaks a code. A concrete reader-addressed code (`otp_code`)
  seals even past the marketing guard, which exists so auth-vendor newsletters
  discussing 2FA as a product don't seal.
- **Atomic ingest.** `squelch-core/src/store/sqlite.rs:ingest_message` writes the
  message row and the triage row (`sensitivity='sealed'`) in ONE transaction — no
  window in which a sealed message is queryable as normal mail.
- **SQL absence.** Every serving and queueing query gates on `sensitivity='normal'`
  / `!= 'sealed'`. Sealed rows are *absent*, never redacted.
- **Release-mode guards.** `squelch-core/src/triage/mod.rs:stage1_sealed_guard`,
  `stage2_sealed_guard`, `stage2_llm_triage` return `Err(CoreError::InvalidInput)` on
  a sealed row — real runtime checks (they replaced `debug_assert!`, which compiled
  out in release), redacted to the invariant plus the message id.
- **Agent door re-check.** `squelch-mcp/src/server.rs:SquelchServer` re-queries the
  sealed set (`thread_is_sealed`) and drops overlapping results; `get_thread` collapses
  a sealed thread and a nonexistent one into one `resource_not_found`, so existence
  cannot be inferred.
- **Human door only.** `squelch-api` (`/client/*`, bearer auth, constant-time
  compare, refuses to serve without a token) carries sealed **metadata** at
  `/client/sealed` and exactly one body at `/client/sealed/{id}/reveal`, which
  appends the audit row **before** returning and sets `Cache-Control: no-store`.

**Do not break.** Seal detection stays the first thing that touches a parsed body —
any pass moved above it is a pass that has read an OTP. Sealed *absence* is the
agent-door contract: do not add a `sensitivity` field to agent-door types "so callers
can filter". Keep the guards returning errors, the reveal audited-before-served and
`no-store`, and writes human-door only.

## 5. Outbound secret guard

**Invariant.** A secret-looking outgoing body is blocked unless the caller
explicitly overrides, and the matched text is never copied anywhere — not into a
response, a log, or the audit row.

**Enforcement.** `squelch-api/src/guard.rs:scan` / `scan_kinds`, called from
`handlers::action_send` before the message leaves the process. Detects `pem_block`,
`api_key` (vendor prefixes), `otp_code` (6–8 digits near an auth word), `long_hex`
(≥32), `long_base64` (≥40). A non-empty match set returns HTTP 422 listing only the
**kinds**; `override_guard: true` sends anyway and writes a `guard_override:<kinds>`
audit row. False positives are acceptable precisely because it is overridable.

**Do not break.** `GuardMatch::kind()` is the only thing that may leave the process —
never return, log, or audit the matched substring. Keep the override audited; an
un-audited bypass is not a bypass we can reason about.

**Post-send echo (audit contract).** After a successful send,
`handlers::echo_sent` fetches the sent message back (`format=raw`, write token) and
ingests it locally so the thread shows the reply immediately. It is strictly
best-effort — the mail has already left, so nothing here may fail the request — and
audits under its own action `send.echo`, alongside the `send` row's own outcomes
(`rejected:confirm`, `rejected:empty_body`, `blocked:guard`, `guard_override:<kinds>`,
`rejected:no_write_credential`, `failed:target`, `rejected:no_recipient`,
`rejected:compose`, `failed:gmail`, `ok`):

| `send.echo` detail | meaning |
| --- | --- |
| `skipped:no_id` | Gmail's send response carried no message id; nothing to fetch back. |
| `failed:fetch` | the read-back GET or its base64url decode failed. |
| `failed:ingest` | the local store write failed. |
| `ok:<local id>` | echoed; `<local id>` is the local `messages.id`. |

The echoed row goes through the SAME seal-first ingest as any other message
(`sync::ingest::ingest_sent`, `is_sent: true`), so it runs no LLM and — because
seal detection precedes the `is_sent` branch — a reply quoting an OTP lands
`sealed`, exactly as backfilled SENT mail does. Keep the echo's failures audited
and swallowed; keep it out of core's write surface (the fetch lives in
`squelch-api/src/gmail_write.rs`, core takes bytes).
