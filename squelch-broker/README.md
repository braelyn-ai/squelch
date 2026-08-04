# squelch-broker

**Status: v1 implemented, daemon side shipped (`squelchd auth --broker <url>`),
no deployment yet.** Nothing runs at `auth.passband.email`.

The consent relay. It parks a Google OAuth authorization code for a few minutes
so a headless daemon (docker on a NAS, a VPS) can finish consent without an
`ssh -L` tunnel. Design and the binding wire contract live in
[`docs/BROKER.md`](../docs/BROKER.md); this file is how to run it.

It holds **no OAuth client credentials, no tokens, no mail**. The code it parks
is cryptographically useless without the PKCE verifier, which never leaves the
daemon. Deployed as its own service, separate from the APNs relay: they share a
posture and nothing else.

## Running

```sh
SQUELCH_BROKER_PUBLIC_URL=https://auth.passband.email \
  squelch-broker
```

Config is environment-only and validated at startup: a bad value is a refusal to
boot, never a surprise on somebody's first consent.

In a container it is `Dockerfile.broker` at the repo root, whose entrypoint binds
`0.0.0.0:$PORT` because the default below is loopback and would be unreachable.
The Railway service is [`deploy/DEPLOY.md` §8](../deploy/DEPLOY.md).

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `SQUELCH_BROKER_PUBLIC_URL` | yes | — | The externally visible base URL. It is what `redirect_uri` must equal (`<url>/callback`) and what the `/link` URL is built from, so it cannot be derived from the bind address. `http(s)`, origin only: a path, query, fragment, or userinfo is a refusal to boot. |
| `SQUELCH_BROKER_BIND` | no | `127.0.0.1:8851` | Listen address. Loopback by design, TLS terminated by a proxy in front. |
| `SQUELCH_BROKER_LOG` | no | `info` | `tracing` env filter. |

## Endpoints

`GET /healthz` → `200 ok`. No auth, outside both rate limiters: it is the
liveness probe and must answer while a client is being throttled.

Nothing else is authenticated either, and that is the design: every stranger's
self-hosted daemon is a legitimate client, so there is no credential anyone
could be asked for. The defenses are strict validation of the registered consent
URL, high-entropy identifiers minted by the daemon, a session table capped at
4096 with a 10 minute TTL, and per-IP token buckets — 600/min for the two JSON
routes, 300/min for the two human-facing pages, metered separately so a link
scanner cannot spend a daemon's polling budget. The client IP is the TCP peer,
so behind the expected TLS proxy the whole deployment shares one bucket:
`X-Forwarded-For` is caller-supplied and trusting it would hand any client
unlimited identities.

The four routes, their bodies, and their status codes are specified in
`docs/BROKER.md` and implemented to match. Two decisions the contract left open:

- A repeat callback for an already-parked session answers `409` with the "link
  already used" page (the contract names the page, not the code).
- `403` on a claim-token mismatch carries `{"error": "…"}` like every other
  refusal here, while the contracted `404` carries `{"status": "unknown"}`.

## Privacy

Session ids, claim tokens and their hashes, authorization codes, and consent
URLs are **never logged** — not at debug, not on an error path. Counts,
statuses, and timings only. No error message echoes the value it rejected,
because those strings travel back over the wire into somebody else's log. The
HTML pages are self-contained (inline CSS, no scripts, no external assets, no
fonts to fetch), carry `Cache-Control: no-store` and `Referrer-Policy:
no-referrer`, and never render a code.

## Development

```sh
cargo test -p squelch-broker
```

No fixtures and no setup: the only state this service has is a `HashMap` that
lives for ten minutes.
