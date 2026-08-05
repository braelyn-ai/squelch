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
Tracking pixels are stripped from mail whose sender the user has never written to.

Outbound read tracking is the mirror image of this section and is documented
separately in [TRACKING.md](TRACKING.md).

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
- **The strip is CONDITIONAL as of 2026-08-04.** `GET /client/thread/{id}` carries
  a per-message `sender_known`, true iff BOTH halves hold: the sender is a
  Sent-derived contact (`Store::is_known_contact` — a `contacts` row with
  `sent_count > 0`, the same predicate Stage-1 uses) AND the message's stored
  email-authentication verdict is a pass. When it is true
  `EmailWebView.Prepared.make` renders the unstripped body: someone the user
  writes to is allowed to learn they opened the mail. The bypass is scoped to
  `Trackers.strip` ALONE — `ImageProxy.rewrite`, the `squelch-img:` CSP, the
  ingest sanitizer, and the remote-images default all still apply, so an allowed
  pixel still needs remote images on before it fetches. `allowTrackers` is part of
  `Prepared.cacheKey`, and `ImageWarmer` always prefetches the STRIPPED body so
  warming can never report an open the reader never made.
- **The auth half of the gate** (closes issue #10 — the `From` header alone is
  free text, so the contact half by itself let anyone who knows one of the user's
  correspondents spoof their way past the strip).
  `squelch-core/src/sync/ingest.rs:extract_auth_pass` computes the verdict once at
  ingest into `messages.auth_pass`;
  `squelch-api/src/handlers.rs:get_thread` ANDs it into `sender_known`. Gated
  there and not inside `is_known_contact`, because that call also feeds triage's
  known-contact importance floor, which must not move.
  The verdict is read from the **TOPMOST** `Authentication-Results` header: Gmail
  PREPENDS its own and does not strip copies the sender wrote, so every occurrence
  below the first is attacker-controlled text. It counts only when the authserv-id
  is `mx.google.com`, a Google-written header (`X-Google-Smtp-Source`, or a
  `Received` handed off `by mx.google.com`) sits ABOVE it, and the POP marker
  `X-Gmail-Fetch-Info` is absent. Every `pass` must bind to the RFC 5322 From
  domain — `dmarc=pass` via an aligned `header.from=`, `dkim=pass` via an aligned
  signing domain. Two `From` headers yield no verdict at all.
  **NULL DENIES THE BYPASS.** Only `Some(true)` is a pass. `Some(false)` is "Gmail
  evaluated it and neither method held"; `NULL` is "never evaluated" — no Gmail
  header, an undecodable one, two `From` headers, or a row ingested before the
  column existed. Absence of proof withholds a permissive feature.
  **Nothing is backfilled.** `store/sqlite/migrate.rs` adds the column to an
  existing `messages` table with every historical row left NULL; a re-sync refills
  it through the message upsert.
  **Residual risks, accepted:**
  - The evidence is header SHAPE, not a signature — nothing in the bytes is
    cryptographically bound to Google. On an ingestion path where Gmail prepends
    nothing (POP with the marker somehow absent, or `users.messages.import`/
    `insert` performed with the user's own credentials) a sender who writes both a
    fake `Received: … by mx.google.com` and a fake verdict still gets through.
    Closing it needs the Gmail API's own delivery metadata, which this parser
    cannot see.
  - **No public-suffix list.** Alignment is equality or a dot-suffixed child of
    the signing domain, so a signature by a registry suffix such as `co.uk` would
    align with `victim.co.uk`. Not attacker-reachable — it needs a DKIM key for
    the suffix itself.
  - `header.i=` alone is accepted when `header.d=` is absent (the form Gmail
    usually emits), which DELEGATES the binding to Gmail enforcing RFC 6376 §3.5:
    an `i=` domain must be `d=` or a subdomain of it. When both are present that
    relationship is checked here instead of assumed.
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
- `auth_pass` is three-valued and the comparison must stay `== Some(true)`. Any
  rewrite that treats NULL as a pass — `!= Some(false)`, `unwrap_or(true)`, a
  `NOT NULL DEFAULT 1` column, a backfill "so old mail isn't penalised" — silently
  grants the bypass to every row that predates the column and to every message on
  an ingestion path Gmail never stamped.
- Read the verdict from the FIRST `Authentication-Results` header, never the last.
  `Message::header_raw` resolves to the LAST occurrence — the forgeable one — and
  `headers_raw()` DROPS non-UTF-8 headers, which would let an attacker hide the
  genuine verdict and promote one they wrote.
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

**Local drafts (human-door-only table).** `drafts`
(`squelch-core/src/store/sqlite/drafts.rs`, served only by `/client/drafts`) holds
unsent compositions, one per reply target plus one new-message slot. It is **never
synced to Gmail Drafts** and **never visible on `/mcp`** — an unsent draft is the
user's own thinking, not mail the agent door was handed. It is also **never
audited**: the audit log is the ledger of reveals and Gmail writes, and a
composition that never left the machine is neither, so audit rows would only add a
record of what the user was drafting. Reads and writes carry `Cache-Control:
no-store`, like the reveal.
- `handlers::put_draft` resolves the parent through the same lookup `send` uses, so
  a draft can never be **saved** against sealed mail (sealed and unknown are one 404).
- A **post-hoc** seal scrubs it: both seal paths — `feedback.rs:correct_triage`'s
  seal branch (hand correction) and `messages.rs:ingest_message` (a re-ingest whose
  triage row lands `sealed`) — `DELETE FROM drafts` for that message id in the same
  transaction as the seal. `list_drafts` additionally filters a sealed parent
  (`NOT EXISTS` on `triage`, the same shape as `deadlines`) as a belt.

**What enforces the split, and what token scope cannot.** The two-door split is
enforced by three structural facts, none of which involve OAuth scope: the agent
door exposes **no write tools at all**, the write credential is loaded **only** by
human-door action handlers and never by sync or triage, and sealed rows are
**absent** from every serving query.

Scope is defense in depth on top of that, and it is worth being precise about how
much it buys. Google unions grants **per Cloud project**: with incremental
authorization a newly issued access token also covers every scope the user has
previously granted the project, even when those grants were requested from a
different client. So once a user has run `squelchd auth --write`, the token behind
the *read* slot carries `gmail.modify` and `gmail.send` too, however narrow the
request that minted it was. "The agent door holds a token that physically cannot
send" is therefore **not a claim we get to make**.

This is also why `judge_transfer_credential` holds an imported credential to a
scope **floor** (does the grant cover what this slot needs?) rather than an exact
match. An exact match would refuse the Read entry of every `--export --write`
blob, because both entries legitimately report the union. Do not "tighten" it.

Real token-level separation would require a **second Google Cloud project**, not
merely a second OAuth client, since the union is per project. That doubles the
verification and CASA burden for a property the structural enforcement above
already provides. We are deliberately not doing it.

**Do not break.** Seal detection stays the first thing that touches a parsed body —
any pass moved above it is a pass that has read an OTP. Sealed *absence* is the
agent-door contract: do not add a `sensitivity` field to agent-door types "so callers
can filter". Keep the guards returning errors, the reveal audited-before-served and
`no-store`, and writes human-door only. Keep `drafts` off the agent door and out of
the audit log, and keep both seal paths scrubbing it — a draft outliving its parent's
seal is a quotation of auth mail the user has already decided is auth.

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
| `skipped:sealed` | the outbound copy tripped seal detection; nothing was committed. |
| `failed:fetch` | the read-back GET timed out (5s), failed, decoded badly, or returned zero bytes. |
| `failed:ingest` | the local store write failed. |
| `ok:<local id>` | echoed; `<local id>` is the local `messages.id`. |

The echoed row goes through the SAME seal-first ingest as any other message
(`sync::ingest::ingest_sent`, `is_sent: true`), so it runs no LLM. Because seal
detection precedes the `is_sent` branch, a reply quoting an OTP trips it — and such
a copy is **not written at all** (`Ok(None)`, `skipped:sealed`). Committing it would
put a sealed row in the thread, and `thread_guard_and_subject` 404s any thread
holding one, so the echo would hide the counterparty's mail the user was reading a
second ago; skipping degrades to "the reply appears on the next backfill", the
pre-echo status quo. The read-back is also capped at **5s**
(`ECHO_FETCH_TIMEOUT_SECS`): the client's POST timeout is 15s and `action_send` has
already made two serial Gmail calls, so bookkeeping may not spend the rest of that
budget. Keep the echo's failures audited and swallowed; keep it out of core's write
surface (the fetch lives in `squelch-api/src/gmail_write.rs`, core takes bytes).
