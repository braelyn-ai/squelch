# squelch-relay

**Status: design only. No code yet.**

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

## Daemon and client integration (lives in the main repo, not here)

Planned, in dependency order:

1. `events` table + triage emission rules + in-process broadcast
   (`squelch-core`). Emission rules decided up front: never on initial
   backfill; sealed mail is at most a contentless event (probably skipped in
   v1 — an OTP on a lock screen would undo the seal design); deterministically
   squelched senders are silent.
2. `GET /client/events` SSE with replay cursor, and `GET /client/events/{id}`
   (id-addressable from day one — the iOS NSE fetches by id; trivial now, a
   retrofit later).
3. macOS Swift client consumes SSE (`URLSession.bytes` on the existing
   `APIClient` plumbing, reconnect with the persisted last-seen id), renders
   through `UNUserNotificationCenter`, notification click focuses the app and
   deep-links to the thread. Residency: last-window-closed does not terminate,
   optional menu bar extra, `SMAppService` login item. The app is already a
   real bundle (`dev.squelch.client`) so notification authorization just works;
   note that re-signing ad-hoc can reset the user's notification permission
   grant.
4. iOS era: `POST /client/devices` registration on the human door (device
   token, human door, bearer-authed), a pusher task in squelchd reading the
   events table and POSTing to this relay, dropping tokens on 410. NSE +
   shared-package rendering as described above.

**Per-channel cursors, not a global "delivered" flag** — the Mac's SSE consumer
and each phone track their own `after=<id>` independently. A single-consumer
assumption is the one thing that would make iOS a refactor instead of a bolt-on.

## Deployment sketch

A single small binary (Rust/axum to match the rest of the codebase — but it is
a separate deployable with its own release cadence, not a workspace member of
the daemon's runtime). Stateless, so any small VPS or fly.io-class host works;
scale is a rounding error. TLS terminated by the host. The `.p8` key is the
only secret.

## Open questions

- **Sender auth / abuse.** Device tokens are already unguessable capabilities,
  so v1 may be unauthenticated-but-rate-limited (per token + per IP). The
  alternative is anonymous registration that issues a relay token. Decide when
  the abuse surface is real, i.e. at distribution.
- **Event id opacity.** Monotonic ints leak count/frequency to the relay.
  Probably fine; could switch to random ids if it ever bothers us.
- **Batching.** Should `POST /v1/push` take many tokens per event? (Yes,
  probably — multi-device fan-out belongs in one request.)
- **Sandbox vs production APNs** routing — per-request flag or per-deployment?
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
