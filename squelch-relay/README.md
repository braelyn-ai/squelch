# squelch-relay

**Status: v1 implemented, and the daemon now has a caller.** `squelchd` ships an
APNs pusher (`squelch_core::push`) that registers devices on the human door and
POSTs `/v1/push` for each new event; it is off unless `SQUELCH_RELAY_URL` is set.
Deployment is a root `Dockerfile` + `railway.toml` (Railway), so the service can
go up whenever the iOS app needs it. Still nothing in production, and the iOS app
does not exist yet.

A tiny, blind APNs relay. It exists so that a future squelch iOS app can receive
push notifications without any user's mail content — or any user's daemon — ever
touching shared infrastructure beyond a single opaque ping.

## Why this exists

The notification architecture puts all *decisions* in the daemon: triage writes
notification-worthy events (surfaced above the squelch line, urgent tier,
deadline detected) to a durable `events` table in SQLite with monotonic ids.
Delivery is a per-platform adapter reading that one log:

```
triage verdict
     v
events table (SQLite, monotonic id)    <- one source of truth, in squelchd
     ├── SSE  GET /client/events        -> macOS Swift client (resident app)
     └── APNs pusher (this relay)       -> iOS
```

The client story is all-Swift: `squelch-client-swift` on macOS today, an iOS
app later, sharing models and API-client code. On the Mac the app stays
resident (window hides on close, the process lingers), holds one SSE connection
to the human door, and posts through `UNUserNotificationCenter` — no relay
involved. iOS has no resident-process option: a native app that isn't
foregrounded can only be woken by APNs.

APNs requires holding the `.p8` signing key bound to the app identifier and the
developer account. That key cannot ship inside every user's daemon — anyone who
extracted it could push to every install of the app. So at public-distribution
scale, *something* hosted has to hold the key and forward pushes. This service
is that something, kept as small and as blind as possible.

(For personal use / TestFlight, a relay is technically unnecessary — squelchd
could hold the key and speak HTTP/2 to `api.push.apple.com` directly. We're
building the relay anyway so the distribution story exists from day one and the
daemon only ever learns one delivery protocol.)

## The core principle: push the ping, not the content

The APNs payload carries **no mail content, no subject, no sender — only an
opaque event id** plus a generic fallback alert ("New mail surfaced").

On the phone, a Notification Service Extension (`mutable-content: 1`) wakes,
fetches the real event from the user's own daemon over their tailnet
(`GET /client/events/{id}` on the human door, bearer-authed), and rewrites the
notification with actual content. If the daemon is unreachable, the generic
alert shows as-is.

Because both clients are Swift, the "turn event N into notification title/body"
code is written once in a shared package: the macOS app runs it on events
arriving over SSE, the iOS NSE runs the identical code on events fetched after
a ping. Same for the API client and event models. The relay never needs to know
any of it exists.

What each party sees:

| Party | Sees |
|---|---|
| relay | device tokens, event ids, timing |
| Apple | the same, plus the generic alert text |
| user's daemon | everything (as it already does) |

Same posture as sealed mail: infrastructure gets timing metadata, never content.

## Shape of the service

Stateless. One endpoint that matters. No database in v1.

- `POST /v1/push` — body: device token(s), opaque event id, optional collapse
  id (e.g. per-thread so a busy thread coalesces). Relay signs an APNs JWT
  (cached ~50 min per Apple's rules), forwards over HTTP/2, and returns the
  per-token APNs status verbatim.
- APNs `410 Unregistered` is passed straight back so the **daemon** deletes the
  dead device row. The relay remembers nothing.
- Payload it constructs: `mutable-content: 1`, generic `alert`, custom
  `event_id`, collapse id if given. Visible notification (not
  `content-available` background push — those get throttled; a visible
  mutable notification runs the NSE reliably).

Config: APNs auth key (`.p8`), key id, team id, bundle id(s), sandbox/production
toggle. That's the whole surface.

Logging: request timing and status codes only. Never log tokens or payloads.

## Running

```sh
SQUELCH_RELAY_APNS_KEY_PATH=/etc/squelch/AuthKey_ABC123.p8 \
SQUELCH_RELAY_APNS_KEY_ID=ABC123XYZ9 \
SQUELCH_RELAY_APNS_TEAM_ID=TEAM123456 \
SQUELCH_RELAY_APNS_TOPICS=dev.squelch.ios \
SQUELCH_RELAY_AUTH_TOKEN=$(openssl rand -hex 32) \
  squelch-relay
```

Config is environment-only and validated at startup: a bad value is a refusal to
boot, never a surprise on the first real push. Running `/v1/push` without
authentication is allowed but never *defaulted* into — a missing
`SQUELCH_RELAY_AUTH_TOKEN` is a refusal to boot unless
`SQUELCH_RELAY_ALLOW_ANONYMOUS=1` says the operator meant it.

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `SQUELCH_RELAY_BIND` | no | `127.0.0.1:8850` | Listen address. Loopback by design — TLS is terminated by a proxy in front. |
| `SQUELCH_RELAY_APNS_KEY_PATH` | one of | — | Path to the `.p8` (PKCS#8 PEM). |
| `SQUELCH_RELAY_APNS_KEY` | one of | — | The same PEM inline, for secret-manager injection. Setting both is an error. |
| `SQUELCH_RELAY_APNS_KEY_ID` | yes | — | The key id from the Apple developer portal (JWT `kid`). |
| `SQUELCH_RELAY_APNS_TEAM_ID` | yes | — | The team id (JWT `iss`). |
| `SQUELCH_RELAY_APNS_TOPICS` | yes | — | Comma-separated `apns-topic` allowlist (bundle ids). The FIRST is the default. |
| `SQUELCH_RELAY_APNS_ENV` | no | `production` | `production` or `sandbox`; a request may override per push. |
| `SQUELCH_RELAY_AUTH_TOKEN` | yes* | — | Bearer required by `POST /v1/push` (constant-time compare). Minimum 32 characters. |
| `SQUELCH_RELAY_ALLOW_ANONYMOUS` | no | `0` | `1` serves `POST /v1/push` open-but-rate-limited, and is the *only* way to omit the bearer. Setting both is an error. |
| `SQUELCH_RELAY_LOG` | no | `info` | `tracing` env filter. |
| `SQUELCH_RELAY_APNS_URL_OVERRIDE` | no | — | **Test only.** Points the relay at a mock APNs base URL instead of Apple. Never set in production; the binary logs a warning when it is. |

### Endpoints

`GET /healthz` → `200 ok`. No auth, outside the rate limiter: it is the liveness
probe and must answer even while a client is being throttled.

`POST /v1/push`:

```json
{
  "device_tokens": ["<hex>", "..."],
  "event_id": 4711,
  "collapse_id": "thread-99",
  "topic": "dev.squelch.ios",
  "environment": "production"
}
```

`device_tokens` is 1-100 hex tokens of 16-200 characters. `event_id` is an
opaque string (≤256 bytes) or integer, forwarded into the payload verbatim — the
relay never interprets it. `collapse_id` (≤64 bytes, APNs' own limit), `topic`
(must be in the allowlist), and `environment` are optional. The encoded APNs
payload is additionally capped at 4 KB — Apple's own limit — so no single
request can be multiplied by the token count into an amplifier.

Response is always `200` when the request itself was well-formed:

```json
{"results": [
  {"token": "<hex>", "status": 200, "apns_id": "..."},
  {"token": "<hex>", "status": 410, "reason": "Unregistered"}
]}
```

`status` is the APNs HTTP status **verbatim**, in request order. `410
Unregistered` is data, not an error: the daemon drops the row, the relay
remembers nothing. A token whose request never reached APNs reports `status: 0`
with reason `unreachable` rather than failing the batch, and one that had not
finished within the 30-second whole-batch budget reports `status: 0` with reason
`timeout`. Only a malformed request gets a 4xx, and an over-budget client gets a
`429` (120 requests/minute per client IP, in-memory and per-process; the limiter
sits *inside* the auth layer, so unauthenticated junk cannot spend a real
client's budget).

### Development

```sh
cargo test -p squelch-relay
```

`tests/fixture_test_key.p8` is a committed throwaway ES256 key so the JWT tests
need no setup. It is not an Apple credential and never was.

## Daemon and client integration (lives in the main repo, not here)

In dependency order. **Steps 1-4 are done**; step 4's iOS half (the app, the NSE,
and the shared rendering package) is what remains.

1. **Done.** `events` table + triage emission rules + in-process broadcast
   (`squelch-core`). Emission rules decided up front: never on initial
   backfill; sealed mail is at most a contentless event (probably skipped in
   v1 — an OTP on a lock screen would undo the seal design); deterministically
   squelched senders are silent.
2. **Done.** `GET /client/events` SSE with replay cursor, and
   `GET /client/events/{id}`
   (id-addressable from day one — the iOS NSE fetches by id; trivial now, a
   retrofit later).
3. **Done.** macOS Swift client consumes SSE (`URLSession.bytes` on the existing
   `APIClient` plumbing, reconnect with the persisted last-seen id), renders
   through `UNUserNotificationCenter`, notification click focuses the app and
   deep-links to the thread. Residency: last-window-closed does not terminate,
   optional menu bar extra, `SMAppService` login item. The app is already a
   real bundle (`dev.squelch.client`) so notification authorization just works;
   note that re-signing ad-hoc can reset the user's notification permission
   grant.
4. iOS era. **Daemon half done:** `POST /client/devices` +
   `POST /client/devices/unregister` on the human door (bearer-authed, idempotent
   — iOS re-registers every launch; the token rides in the body, never in a URL
   path, because a path segment is what every access log keeps), and the pusher
   task in squelchd reading the
   `events` table past its own `sync_state` cursor and POSTing one `/v1/push`
   per event, oldest first, dropping tokens on `410`. The cursor advances only
   after a 200, so a relay outage delays pushes and never skips them; a cold
   start joins at the head rather than replaying history; with no devices
   registered the cursor advances without any request at all. Configured
   entirely by `SQUELCH_RELAY_URL` / `SQUELCH_RELAY_TOKEN` (+ optional
   `SQUELCH_RELAY_TOPIC` / `SQUELCH_RELAY_APNS_ENV`) — no relay URL, no task.
   **Remaining:** the iOS app itself, the NSE, and the shared rendering package.

**Per-channel cursors, not a global "delivered" flag** — the Mac's SSE consumer
and each phone track their own `after=<id>` independently. A single-consumer
assumption is the one thing that would make iOS a refactor instead of a bolt-on.

## Deployment sketch

A single small binary (Rust/axum to match the rest of the codebase). It is a
workspace member so it shares the lockfile and one `cargo test`, but it stays a
separate deployable on its own release cadence: nothing in the daemon links
against it, and it links against nothing of the daemon's — no `squelch-core`,
no store, no mail types. Stateless, so any small VPS or fly.io-class host works;
scale is a rounding error. TLS terminated by the host. The `.p8` key is the
only secret.

Concretely: the repo root carries a `Dockerfile` and `railway.toml` that build
and run only this crate, so `railway up` is the whole deploy. Nothing is
deployed yet.

## Open questions

- **Sender auth / abuse.** Device tokens are already unguessable capabilities,
  so v1 may be unauthenticated-but-rate-limited (per token + per IP). The
  alternative is anonymous registration that issues a relay token. Decide when
  the abuse surface is real, i.e. at distribution. *v1 ships both halves of the
  cheap answer: a static bearer (required by default, opt-out typed) plus a
  per-IP limiter mounted inside the auth layer. The limiter is in-memory and
  keyed on the TCP peer, so behind the TLS proxy the whole deployment shares one
  bucket — `X-Forwarded-For` is caller-supplied and trusting it would hand any
  client unlimited identities. That shared bucket is exactly why auth is the
  outer layer: unauthenticated floods must not spend it. The bucket map is
  hard-capped and swept on a clock so address rotation cannot grow it without
  bound or turn the limiter itself into the bottleneck. Real per-tenant limiting
  waits for real tenants.*

  The distribution-grade answer, when strangers' daemons exist, is **App Attest
  registration grants**. Note that even without them, misdirection is mostly
  defanged: the relay constructs the payload, so a caller who somehow has
  someone else's device token can only cause a generic "New mail surfaced"
  banner — the NSE resolves the event id against the *victim's own* daemon,
  which either has no such event or shows their own real mail. Content spoofing
  is structurally impossible; a leaked token buys annoyance, and Apple is
  explicit that device tokens are not secrets, so at scale "unguessable" is not
  enough. The grant flow closes that: at registration the iOS app obtains an
  App Attest assertion (Apple vouching this request came from a genuine install
  on this device), presents it with its token to a relay `register` endpoint,
  and gets back a grant — an HMAC over the token under a relay-held key. The
  phone hands the grant to its daemon with the token; every `/v1/push` must
  carry it; the relay verifies statelessly. A leaked token alone is then
  useless. One extra endpoint, one extra opaque blob in the daemon's device
  row, still no database.
- **Event id opacity.** Monotonic ints leak count/frequency to the relay.
  Probably fine; could switch to random ids if it ever bothers us. *The wire
  format takes a string or an integer, so that switch needs no relay change.*
- **Batching.** Should `POST /v1/push` take many tokens per event? (Yes,
  probably — multi-device fan-out belongs in one request.) *Settled: yes, up to
  100 tokens, fanned out 8 at a time, each reporting its own APNs status.*
- **Sandbox vs production APNs** routing — per-request flag or per-deployment?
  *Settled: per-deployment default, per-request override.*
- **Mac over APNs too?** Native macOS apps can register for remote
  notifications — no App Store required, but it does require Developer ID
  signing with a push-entitled provisioning profile (ad-hoc bundles can't get
  an APNs token), and Developer ID profiles only get the *production* APNs
  environment. SSE stays the local-first default; this is a distribution-era
  option, not a plan.

## Rejected alternatives

- **APNs key in the daemon** — fine for personal use, key-extraction disaster
  at distribution scale. Kept as a dev-mode possibility only.
- **Web Push / PWA** — iOS 16.4+ supports Web Push for home-screen PWAs with
  end-to-end encrypted payloads and self-generated keys: no Apple account, no
  relay, ideologically pure. Requires the iOS app to be a PWA, and the client
  line is native Swift. Noted with respect, discarded.
- **Background fetch / silent polling on iOS** — throttled by the OS into
  uselessness for timely mail.
- **ntfy / UnifiedPush** — still routes through someone's APNs upstream on
  iOS, and puts a third-party app in the loop.
