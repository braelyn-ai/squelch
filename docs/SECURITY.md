# Squelch security model

One section per surface: the **invariant**, the **enforcement point** (`file:symbol`
— the line that actually holds it), and **what a maintainer must not break**.
Verified against the tree on 2026-07-30; §6 was written against it on 2026-08-18.
Stale source comments are flagged, not trusted.

## 1. HTML sanitization at ingest

**Invariant.** `messages.body_html` never contains active content: no `<script>`, no
`on*` handlers, no `javascript:`/`data:` URLs, no forms, no nested browsing contexts,
no `<meta>`/`<link>`/`<base>`.

**Enforcement.** `squelch-core/src/sync/html.rs:sanitize_email_html` — a closed
`ammonia::Builder` allow-list, run once at ingest before storage; pure, no I/O,
fixture-tested in the same file. Four deliberate deviations from ammonia's
defaults: `url_schemes` narrowed to `{http, https, mailto, cid}`; `<style>` kept with
CSS verbatim (`rm_clean_content_tags` then `add_tags` — that order, or ammonia
panics); `style`/`class`/`id` added to the generic attribute set; `link_rel`
pinned to `noopener noreferrer`.

**Do not break.**
- Never widen `url_schemes`; `data:` coming back re-admits inline
  `data:text/html` payloads and is the only reason `data:` image URIs drop today.
  `cid:` is the one addition ever made, and only because it is a pointer with
  nothing behind it: it names a part of the same message (RFC 2392), reaches no
  host, and no renderer we ship resolves it. It is kept so the client can pair the
  reference with the attachment row carrying that Content-ID; dropping the `src`
  instead leaves an anonymous `<img>` nothing can resolve. The client rewrites the
  resolvable ones to `passband-cid:` and deletes the rest before any web view sees
  the body (§3), and `img-src` never names `cid:`.
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
   `Coordinator.document(html:allowRemote:)`, built by `Lib/MailCSP.swift`:
   `default-src 'none'; style-src 'unsafe-inline'; img-src passband-img:
   passband-cid: data:` (`img-src passband-cid: data:` when remote images are
   off), plus `<meta name="referrer" content="no-referrer">`. No `script-src`, and
   no `http:`/`https:`/`cid:` anywhere in the policy. `passband-cid:` is
   unconditional because those bytes are a part of the same message fetched from
   the user's own daemon, so the remote-image opt-in has nothing to gate (§3).
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
- **The auth half of the gate.** The `From` header is free text, so contact
  membership alone cannot authorize a tracker bypass.
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
- **`cid:` references are the local mirror of all this.** `Lib/CidImages.swift`
  matches each `<img src="cid:…">` against this message's own attachment rows by
  normalized Content-ID (§1 keeps the scheme so there is something to match) and
  rewrites the ones that resolve to `passband-cid://local/<hmac>?a=<id>&n=<name>`.
  A reference that matches nothing, or whose part fails the same inline gate the
  attachment strip renders under (`AttachmentKinds.isInline` — stored bytes, a
  non-svg image mime, within `inlineMaxBytes`), has its **whole `<img>` tag
  dropped**: pre-migration mail resolves nothing at all, and a body with a gap
  reads as a message while one full of broken glyphs reads as a bug. The HMAC is
  over both values the parser returns and is what scopes a reference to its own
  message — without it a body could hand-write `?a=91` and read another message's
  attachment. Bytes come from the authenticated human door
  (`GET /client/attachments/{id}`), served by the same `ImageSchemeHandler` under
  the same live-set discipline, refused again over `inlineMaxBytes`.

**Do not break.**
- The order in `EmailWebView.Prepared.make`: `Trackers.strip` → `ImageRepeats`
  dedupe → the `hasNetworkImages`/`extractLinks` reads → `CidImages.rewrite` →
  `ImageProxy.rewrite` **last**. Trackers first so a pixel can never be the "first
  occurrence" that suppresses a real image; rewrite last because after it nothing
  recognises a reference as remote.
- `Prepared.cacheKey` folds in every attachment field the cid rewrite reads (id,
  content-id, downloadable, mime, size). Drop them and two bodies that share html
  but not parts collide in `PreparedBodies` — which is one message's photo pasted
  into another's.
- Never put `http:`/`https:` in `img-src`. A missed rewrite must fail closed
  instead of becoming an un-proxied request.
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

**The recipient headers a send states.** `to`, `cc` and `bcc` come from the caller
and go into the outgoing message's headers, so `gmail_write::build_reply_rfc822` and
`build_forward_rfc822` reject any of them carrying CR or LF **before** composing: a
smuggled newline is how a recipient gets appended that the sender never approved,
and in a `Bcc` that addition is invisible to everyone downstream by construction.

The two copy lists reach that check by different routes, and the difference is worth
stating because it decides which guard is load-bearing:

- `bcc` is filtered through `addrs_excluding` first, which parses to bare addresses
  and **cuts the value at an embedded header token** (`parse_addr_list`), so a
  smuggled line is dropped and the honest prefix survives. Fewer recipients, never
  a forged one.
- A stated `cc` is written **verbatim** — running it through `cc_excluding` would
  delete the display names the sender typed — so on that path the builder's CR/LF
  refusal is the whole of the sanitization, and it refuses rather than repairs.

**A Bcc is recorded in exactly one place: the audit ledger.** Every other recipient
is legible in the delivered mail; a blind copy is stripped by Gmail from the copies
the visible recipients receive, so nothing outside this machine records that it
happened. `handlers::action_send` therefore writes `ok:bcc:<n>` as the send's audit
detail. **Counts, never addresses** — the ledger's job is that a send of this shape
happened, not who it named, and the same rule that keeps matched secret text out of
an audit row keeps recipients out of one. A stated `cc` gets no line of its own,
deliberately: it is legible in the delivered mail, which is the test for whether the
ledger has to carry it.

## 4b. Provider spam

**Invariant.** Mail the provider filed as spam is stored and readable on one
human-door page, and is otherwise absent: it never reaches an LLM, never crosses
the agent door, is never embedded, and never fires a notification.

**Why it is a security rule and not only a cost one.** Spam is attacker-authored
text selected for its ability to talk a reader into things, and a Stage-1 prompt
is a reader. It is also the bulk of what arrives, so "score it like anything
else" would mean spending the most model calls on the least trustworthy corpus.
The same reasoning is why the agent door gets **no spam at all** rather than
spam it is told to distrust: everything the agent reads is text it may act on.

**Enforcement, in order.**
- **Fetched only when asked for.** The poll loop does not walk the `SPAM` label
  at all — not on backfill, not on a history tick, not on a catch-up.
  `SyncEngine::sync_spam_window` runs when the page that shows the folder is
  opened, capped at `sync.spam_max` newest-first. That is a cost decision rather
  than a security one, but it has a security effect worth stating: the untrusted
  corpus is not continuously ingested, so on an install where nobody ever opens
  the page it is never fetched at all. The store's upsert keeps
  `is_spam = MIN(stored, incoming)`, so a message ever seen under a visible
  label cannot be hidden by a later spam sighting.
- **Never triaged.** `sync/ingest.rs` returns a neutral `tier=noise` row before
  Stage-1 — after the seal check, so a misfiled OTP is still sealed and sealed
  still outranks spam. `ingest_message` stamps the `'n/a'` stage markers, which
  is what keeps the row out of both LLM queues.
- **Never embedded.** `SyncEngine::ingest_one` returns `None` for a spam row, the
  same structural gate sealed mail gets. An embedding is a similarity claim, and
  spam is written to imitate the mail it impersonates.
- **SQL absence.** Every band, queue, count and search leg pairs `is_spam = 0`
  with its `is_sent = 0`. The one caller asking for the other side is
  `GET /client/updates?spam=only`; an unrecognized value is a 400, never a
  silent full listing.
- **Agent door.** `store::thread_view` (the `/mcp` shape) selects `is_spam = 0`,
  so a thread of nothing but spam is `NotFound` — the same shape sealed mail
  gets. `hybrid_search` is gated on all three of its queries.
- **Never notifies.** `triage::events::worthy_kind` returns `None` for a spam
  row as an explicit arm, not as a side effect of its neutral tier.
- **The one write.** `POST /client/actions/not_spam` removes the label in Gmail
  FIRST and only then clears the flag locally, so a refused write leaves both
  sides agreeing. There is deliberately no route the other way: squelch cannot
  show anyone the effects of training a filter it does not read.

**Guard tests.** `squelch-core/src/store/sqlite/tests/spam.rs` seeds one spam row
beside one ordinary row with overlapping search terms and walks every listing at
once, so a predicate dropped in a refactor fails there rather than in production.
The sync suite asserts the "not fetched routinely" half against the mock's CALL
LOG rather than against what landed: "no spam rows appeared" would also pass if
the walk ran and the folder happened to be empty, and the thing being avoided is
the request.

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

**Forwards are scanned twice over.** Forwarding is the classic exfiltration shape —
a clean note wrapped around someone else's secret — so `handlers::forward_send`
scans the ORIGINAL as well as the note, unions the kinds and issues ONE 422 / one
`guard_override` row for both. The scan runs on the raw bytes re-fetched from Gmail
and about to be composed, never on the stored row: the stored body is a sanitized,
flattened memory of the message and the bytes on the wire are the original.

*Exactly what is scanned* on a forward: the sender's typed note, the original's
decoded text bodies — every part in mail-parser's `text_body` list — an html-to-text
conversion of its `html_body` view, **and** the contents of every `text/*` and
`message/*` attachment part. Both body views are needed because when a `text/plain`
part exists mail-parser never converts the html alternative down, so an alternative
whose text half is innocuous and whose html half carries the key would otherwise ship
unread. The text attachments are scanned because a PEM key pasted into `notes.txt`
and dragged onto a mail is the same exfiltration shape as one pasted into the body —
arguably the more natural one — and mail-parser has already transcoded each such part
to UTF-8, so reading it as text costs nothing; `message/*` parts (an attached email,
a bounce report) are plain RFC822 text and are read the same way. Two known limits,
stated rather than implied: **attachment bytes under any other mime are not scanned**
(a key inside a zip, or an `id_rsa` carrying an `application/octet-stream` mime,
passes), and **html attribute values are not scanned** (the converter yields visible
text, so a secret inside an `href` or a `data-` attribute survives). The guard is a seatbelt against the accident, not a DLP
boundary against a determined sender — it is overridable by design.

**Forwarded HTML goes out verbatim, trackers included.** The original's markup is
embedded as it arrived (the only edit is stripping the `<meta>` tags that DECLARE a
charset — a `charset` attribute or the `http-equiv="Content-Type"` spelling — which
would contradict the part's own `charset="UTF-8"`; a `<meta>` that merely mentions the
word survives). The forward's two body parts then ship
`Content-Transfer-Encoding: quoted-printable`, which is an encoding rather than an
edit: a stranger's newsletter html is routinely one line far past RFC 5322's 998-octet
limit, and re-emitting it raw put illegal lines and raw 8-bit bytes on the wire. It
decodes back byte-for-byte at the far end. All of which means the ORIGINAL sender's
tracking pixels are re-armed and will fire toward whoever the forward is addressed to.
This is deliberate and matches every mainstream mail client: passband strips trackers
out of what it RENDERS to its user, not out of what its user chooses to pass on.
Rewriting a stranger's markup on the way out would forward something other than what
arrived, and the user's decision is to send *this message*.

**Do not break.** `GuardMatch::kind()` is the only thing that may leave the process —
never return, log, or audit the matched substring. Keep the override audited; an
un-audited bypass is not a bypass we can reason about.

**Post-send echo (audit contract).** After a successful send,
`handlers::echo_sent` fetches the sent message back (`format=raw`, write token) and
ingests it locally so the thread shows the reply immediately. It is strictly
best-effort — the mail has already left, so nothing here may fail the request — and
audits under its own action `send.echo`, alongside the `send` row's own outcomes
(`rejected:confirm`, `rejected:forward_and_reply`, `rejected:empty_body` — skipped for
a forward, whose note may legitimately be empty — `blocked:guard`,
`guard_override:<kinds>`, `rejected:no_write_credential` (including the forward whose
raw fetch found the write credential dead — a 403 telling the user to re-run
`squelchd auth --write`, never the 502 that would blame Gmail), `failed:target`,
`rejected:no_recipient`, `rejected:too_large` (the original exceeds
`MAX_FORWARD_RAW_BYTES` = 20 MiB decoded, refused with a 413 before the four-to-five-x
re-encode allocates), `failed:fetch_original` (the forwarded original could not be
read back, so nothing was sent), `rejected:compose`, `failed:gmail`, `ok`,
`ok:forward`):

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

## 6. The embedded assistant

**Invariant.** The agent inside Passband reads only what the human door serves, so
sealed mail is absent from it for the same reason it is absent from the door; and it
cannot touch the mailbox without a human tap that has already happened by the time
the call goes out.

**Enforcement.**
- **Reads inherit §4.** Every tool call in
  `passband/Sources/Passband/Assistant/AgentTools.swift` goes through `APIClient`,
  i.e. `/client/*`. The assistant cannot read a 2FA code because the door it knocks
  on has none — nothing is filtered here, and nothing needs to be. It holds no
  sealed tool: `/client/sealed` and the reveal route are not in its inventory.
- **Three tiers, and only one of them stops.** Fourteen tools: reads are never
  gated; safe writes touch this database only (status, sender rules, drafts) and are
  reversible in the app; the four that touch Gmail (archive, label, send,
  unsubscribe) route through `AgentTools.confirmed(_:_:)`, where only `.approved`
  reaches `perform`. That single funnel is the entire reason the `confirm: true`
  those calls carry is a true statement rather than a default.
- **The daemon does not take the client's word for it.** Every mutating `/client/*`
  route demands the flag itself and audits the attempt, and a send still meets the
  outbound secret guard (§5). A client that lied would be recorded doing it.
- **A half-arrived instruction is not an instruction.**
  `passband/Sources/Passband/Assistant/Assistant.swift` executes a `tool_use` only
  after its message has closed, so a partially streamed tool input never runs.
- **The key.** Self-host is BYOK and the key is read only inside
  `LLMProxy` (`passband/Sources/Passband/Model/Keychain.swift`) at call time —
  never a parameter, a return value, or an error string — over sessions that refuse
  every redirect, because URLSession carries `x-api-key` verbatim across a hop.
  Hosted holds no key in the app at all: `POST /client/assistant/messages`
  (`squelch-api/src/assistant.rs`) relays with a daemon-held credential and streams
  the bytes back, human door only, and 404s when no gateway is configured. The
  conversation body is treated like mail content and logged in neither direction.

**What a maintainer must not break.**
- **Prompt injection is the standing threat, and the gate is the answer to it.**
  Mail bodies reach this model as data, so assume the text is adversarial and
  assume it will eventually ask for an action. Everything that touches the mailbox
  must keep going through `confirmed`; a new ungated write is how this section stops
  being true.
- Do not hand the assistant a sealed route, and do not "helpfully" widen a tool's
  read to a store call that bypasses the door.
- Do not log the relay body, in either direction, at any level.
- A decline is a legitimate answer with `is_error` false. Do not turn it into a
  failure the model feels invited to retry.
