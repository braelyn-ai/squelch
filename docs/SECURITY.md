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

**Enforcement.** `passband/Sources/Passband/Views/EmailWebView.swift` —
five independent layers, each sufficient alone. Layer 1 is §1 above; the rest,
numbered as the source comments number them:

2. `allowsContentJavaScript = false`: page script cannot run at all. Our own injected
   `WKUserScript` is governed separately and still runs.
3. CSP injected as the FIRST child of `<head>` by
   `Coordinator.document(html:allowRemote:)`: `default-src 'none'; style-src
   'unsafe-inline'; img-src passband-img: data:` (or `img-src data:` alone when
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
  `Trackers.strip` ALONE — `ImageProxy.rewrite`, the `passband-img:` CSP, the
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
  `passband-img://local/<hmac>?u=<encoded>`. Default-port `http://` targets are
  upgraded by splicing only the scheme to satisfy App Transport Security without
  normalizing or otherwise changing the source URL. `@import`/`@font-face` are
  skipped on purpose: they load no image, are dead under `default-src 'none'`,
  and rewriting them would hand the launch warmer requests the reader never made.
- `Model/ImageSchemeHandler.swift` is the only responder and
  `ImageProxy.original(from:)` its only parser: it requires this launch's HMAC and
  an http(s) target. The key is 256 random bits per process, never written down, so
  a signature cannot be replayed across launches. Provenance, not secrecy — mail can
  spell `passband-img:` itself in a kept `<style>` block, which `rewrite` neuters to
  `passband-img-blocked:` before minting anything.
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
- Keep the signature check: without it a hand-written `url(passband-img://…)` in a
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
- **Human door only.** `squelch-api` (`/client/*`, bearer auth) carries sealed
  **metadata** at `/client/sealed` and exactly one body at
  `/client/sealed/{id}/reveal`, which appends the audit row **before** returning
  and sets `Cache-Control: no-store`.

**Human-door credentials.** Two kinds, checked in this order by
`squelch-api/src/auth.rs`:

1. `SQUELCH_API_TOKEN`, the **optional** master token, compared in constant time.
   Unset or blank is a supported configuration: the door still serves and 401s
   everything until a device token exists. It is never deprecated, because it is
   the way back in after revoking the last device.
2. **Issued per-device tokens** (`sqd_…`, `squelch-core/src/store/sqlite/device_tokens.rs`),
   minted by `squelchd token issue` or a pairing claim. Stored as a hex SHA-256
   and verified by hashing what was presented, so the plaintext exists once. Named
   and individually revocable, effective on the very next request because nothing
   caches the lookup.

**Surfaces outside the bearer**, each on its own router merged outside the bearer
layer so the boundary is visible in `lib.rs`. Two are machine-facing, and the
rest are the console.

- `GET /t/{token}` — the read-tracking pixel (§3). One response, always.
- `POST /client/pair` — the pairing claim, which has to be unauthenticated: it is
  how a device with no credential gets its first one. Every failure (wrong,
  expired, already claimed, burned, malformed, store error) is one bare 401 with
  no body. The code is ~40 bits, which is only defensible because it is one-shot,
  expires in minutes, and **burns after 5 misses** — a miss is charged against the
  live code, so guessing spends the user's code rather than being free. **No CORS
  layer** on this router, unlike `/client/*`, so a random web page cannot read the
  minted token out of a cross-origin response.

Both touch the store mutex the whole daemon shares, so both are bounded
(`PIXEL_CONCURRENCY` / `PAIR_CONCURRENCY`, 4 each); the device-token branch of the
bearer check is bounded the same way, since any caller can push a `sqd_`-shaped
guess into it. The pixel bails out when its slots are full (it can answer without
the store); the claim **waits**, because an answer that varied with load would be
a signal the uniform 401 exists to remove.

**The console (`/console`).** Server-rendered HTML for the person who owns this
daemon (`squelch-api/src/console.rs`), on both tiers. **A console session is a
device token**: signing in claims a pairing code exactly the way `/client/pair` does, and
the `sqd_` token that comes back is set as a cookie and verified on every later
request through the same `verify_device_token`. No session table, no signing key,
no third credential type — so revocation, the audit trail, the one-shot claim and
the ten-minute TTL are inherited rather than reinvented, and `squelchd token list`
shows a browser for what it is.

| Route | What gates it |
|---|---|
| `GET /console` | nothing. Renders the home page with a valid session cookie and the login page without one |
| `POST /console/login-code` | a pasted pairing code — the store's own claim: one-shot, ~40 bits, ten minutes, **burns after 5 misses**, queued on the same `PAIR_CONCURRENCY` slots |
| `GET /console/callback?code=` | the same claim, on a code the control plane minted. Nothing about the hop is trusted here: a code that was burned, replayed, expired or never minted fails exactly like a typo |
| `POST /console/pair`, `POST /console/revoke/{id}`, `POST /console/logout` | a verified session cookie, checked ahead of the handler |

**Cookie posture.** `HttpOnly`, `SameSite=Lax`, `Path=/`, no `Domain`, `Secure`
whenever the origin is https, 30-day `Max-Age`. Deliberately
**not** `__Host-` prefixed: the prefix requires `Secure`, a plain-http loopback
run cannot set it, and a cookie name that only works in production is a name
whose absence nobody notices until production — so the two properties the prefix
would buy are set explicitly instead. `Lax` and not `Strict`, which was learned
live: the SSO landing is a navigation chain that started at accounts.google.com,
Chrome withholds `Strict` cookies from every request in a cross-site-initiated
chain including the same-site 303 hop back to `/console`, and the first thing a
freshly signed-in user saw was the login page again. `Lax` still withholds the
cookie from cross-site POSTs, and the mutating routes are guarded below
regardless. Sign-out **revokes** the token rather than
only dropping the cookie, and every refusal of a cookie that would not verify
clears it on the way out.

**The one escape hatch: `[console] allow_insecure_cookie`.** Off by default and
meant to stay there. It exists for the self-host serving the console over plain
http on a LAN, who otherwise has a console that cannot work at all: a browser
will not store a `Secure` cookie from `http://`. It is read as a statement about
the whole origin rather than as a cookie flag, so with it on the daemon also
builds its pairing deep link with `http://`, compares `Origin` against that same
`http://` origin, and stops offering the SSO link. Those move together
deliberately: a login page that renders and then refuses the POST from it is not
a working console. **The cookie is a live device token**, so turning this on puts
a revocable credential on the wire in the clear for anything on the path to take,
and it is the reason to prefer TLS or loopback. When it is on, the login page
carries a banner and the daemon warns at startup.

**CSRF, two independent controls.** `SameSite=Lax`, plus an
`Origin`/`Sec-Fetch-Site` check in front of every mutating POST — including the
*unauthenticated* login POST, so a cross-site page cannot sign a browser into an
account of the attacker's choosing either. `Sec-Fetch-Site` is believed
absolutely (page script cannot set it); where it is absent `Origin` is compared
whole, scheme included; where both are absent the answer is no.

**No CORS layer** on this router, for the reason `/client/pair` has none and more
so: a bearer is carried by a client that has one, a cookie is carried
automatically by any browser pointed at us.

**No token on any page, and uniform refusals.** A pairing code renders exactly
once, on the page that mints it, which is that page's entire purpose; a device
token appears only in a `Set-Cookie`. Wrong, expired, already claimed, burned and
"the store could not answer" are one login page with one sentence, byte for byte.
There is no bare 401 anywhere in the tree, because a person is reading it. Pages
carry `default-src 'none'; style-src 'unsafe-inline'; form-action 'self';
frame-ancestors 'none'; base-uri 'none'` and fetch no script, font or image.

**The Google hop is the control plane's, and it is hosted-only.** Google forbids
wildcard redirect URIs, so a per-tenant hostname cannot run OAuth itself. The
login page links to `GET /console/auth?tenant=<label>` on `squelch-control`,
which: is rate-limited on **its own** budget, tighter than signup's, shared with
`/app/auth` below (they are the only two routes that open a server-side session
with nothing presented at all); sends **every well-formed label** to Google
without looking it up, because answering a real label differently from an
unprovisioned one is a directory of which hosted addresses exist; **discovers**
the mailbox from Google on the way back and compares it constant-time against the
store's owner for that label, and only then calls the warden — so guessing a real
label cannot make a pairing code exist, let alone show one; and takes **no**
`return` or `next` parameter anywhere in the flow, so there is no open redirect:
the destination is constructed from this deployment's own base domain and the
validated label. The redirect carrying the live code is `Cache-Control:
no-store, no-cache` and `Referrer-Policy: no-referrer`. Every identity-shaped
refusal is one page. The link renders only when `SQUELCH_CONSOLE_SSO_URL` is set
— hosted tenants get it from the warden (`SQUELCH_WARDEN_CONSOLE_SSO_URL`), a
self-host never sets it, and without it the console is the pasted-code form
alone.

**`GET /app/auth` is that hop with its input removed, for the native app.** The
app has no label to send (its user knows their address, not their tenant record)
and no console to be returned to, so this route accepts **nothing** — no query
string at all — and the tenant is found by **reverse lookup** on the address
Google verified (`active_tenant_for_email`, `status = 'active'` part of the
question). Same consent (`openid email`, online, no refresh token), same session
table, same warden-minted **pairing code** as the ticket; what differs is the
ending, a page carrying a `passband://pair?url=…&code=…` deep link built from
this deployment's own tenant URL, under the same `no-store` / `no-referrer`
headers. **Taking no input is what removes the oracle rather than adding one:**
the lookup key is an address Google vouched for on this request, so the only
mailbox anybody can ask about is the one they just proved they hold, and there is
no label space to walk. That is why this route may say plainly that a signed-in
account has no mailbox here, where `/console/auth` may not. Both logins share one
rate-limit bucket and one carve-out of the session table
(`MAX_IDENTITY_SESSIONS`), so alternating them cannot buy a flooder a second
budget or crowd a paying signup out. It exists so somebody who already has a
mailbox never touches an invite code: invites provision tenants, and this flow is
for people whose tenant already runs.

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

**The sent listing (human-door-only route).** `GET /client/sent`
(`store::sent_listing`) is the **only** listing in the codebase that reads
`is_sent = 1`; every other one on both doors filters it out, and the agent door has
no sent route at all — what the user writes is not the agent's to page through. It
serves metadata only (recipients, subject, snippet, sent-at, read-receipt count),
newest first, behind the same bearer as the rest of `/client/*`.

Because the usual `is_sent = 0` filter is what normally keeps this mail out of
reach, the sealed guard here **fails closed**: an INNER `JOIN triage` *plus*
`sensitivity != 'sealed'`, so a sent row whose triage row is missing is excluded
rather than `COALESCE`d to visible. Sent mail is written with its triage row in the
same transaction, so a missing one is a broken row, not an untriaged one. On top of
the per-row guard sits a thread-level belt (`NOT EXISTS` over sealed siblings, the
same shape as `list_drafts`): seal detection is per-message content, so the user's
own reply in a thread sealed by a sibling commits as `normal` — yet `thread_view`
404s that thread, and listing the row would leak `Re: <sealed subject>` behind a
dead click. The
`messages.to_addrs` column it reads is parsed at ingest from To/Cc and is NULL on
received mail; the one-shot backfill that fills it for pre-existing sent mail
(`SyncEngine::backfill_sent_recipients`) runs on the **read** credential and
fetches `format=metadata` headers only.

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
