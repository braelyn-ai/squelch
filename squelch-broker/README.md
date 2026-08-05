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

**Set `SQUELCH_BROKER_TRUSTED_PROXY_HOPS` when you deploy behind a TLS proxy**
(`1` on Railway). Without it the rate limiters key on the TCP peer, which behind
a proxy is the proxy: every client in the world then shares one bucket, one of
them can 429 all the rest, and the per-client session cap stays off because
there is no client to count. It is opt-in because `X-Forwarded-For` is
caller-supplied, and believing it wholesale would let anyone mint unlimited
identities. The number is your assertion of how much of that header your own
infrastructure appended: only the rightmost `N` entries are read, and anything
that does not resolve (header missing, too few entries, an entry that is not an
address) falls back to the peer.

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `SQUELCH_BROKER_PUBLIC_URL` | yes | — | The externally visible base URL. It is what `redirect_uri` must equal (`<url>/callback`) and what the `/link` URL is built from, so it cannot be derived from the bind address. `http(s)`, origin only: a path, query, fragment, or userinfo is a refusal to boot. |
| `SQUELCH_BROKER_BIND` | no | `127.0.0.1:8851` | Listen address. Loopback by design, TLS terminated by a proxy in front. |
| `SQUELCH_BROKER_TRUSTED_PROXY_HOPS` | no | `0` | How many proxies you run in front of this listener. `0` trusts nothing, meters the TCP peer, and disables the per-client session cap. `N` keys on the Nth-from-the-right `X-Forwarded-For` entry; `1` is the single-TLS-proxy case (Railway). Max 8. |
| `SQUELCH_BROKER_LOG` | no | `info` | `tracing` env filter. |

## Endpoints

`GET /healthz` → `200 ok`. No auth, outside every rate limiter: it is the
liveness probe and must answer while a client is being throttled.

Nothing else is authenticated either, and that is the design: every stranger's
self-hosted daemon is a legitimate client, so there is no credential anyone
could be asked for. The defenses are strict validation of the registered consent
URL (including a **scope allowlist**: a registration may only ask for the scopes
`squelchd` itself asks for, `gmail.readonly` / `gmail.modify` / `gmail.send`, so
nobody can park a link on our domain requesting full mailbox control),
high-entropy identifiers minted by the daemon, a session table capped at 4096
with a 10 minute TTL, a per-client ceiling of 16 live sessions, and per-client
token buckets.

Each route has its own bucket, because a 429 costs a different thing on each:

| Route | Per minute | Why |
|---|---|---|
| `POST /v1/sessions` | 30 | A client registers once per consent. This is the only route that allocates, and its legitimate traffic is a trickle. |
| `POST /v1/claim` | 600 | A daemon polls every 2s for the whole 10 minute window. |
| `GET /link` | 300 | One human click, but browsers prefetch and mail clients scan links. |
| `GET /callback` | 1200 | Refusing it destroys consent the user ALREADY granted: Google does not redirect twice. |

"Client" is the TCP peer unless `SQUELCH_BROKER_TRUSTED_PROXY_HOPS` says
otherwise, so behind a proxy with that unset the whole deployment shares each
bucket and the per-client session cap is inert. Set it.

The four routes, their bodies, and their status codes are specified in
`docs/BROKER.md` and implemented to match. Three decisions the contract left
open:

- A repeat callback for an already-parked session answers `409` with the "link
  already used" page (the contract names the page, not the code).
- `403` on a claim-token mismatch carries `{"error": "…"}` like every other
  refusal here, while the contracted `404` carries `{"status": "unknown"}`.
- A client at its live-session ceiling gets `429`, not `503`: it is holding the
  sessions itself, and `503` would tell its daemon to wait for somebody else.

## The interstitial

`GET /link` is a decision, not a doorway, and the thing it may **not** do is
vouch. Registration is unauthenticated, so the broker knows nothing about who
parked a link: "your own copy of squelchd is asking" would be equally true of a
stranger's, said in our voice on our domain, which is precisely the credibility
an attacker cannot forge for themselves. So the page says someone ran
`squelchd auth` and sent this link here and that it may not have been the
reader, leads with the consequence of approving, lists the requested scopes in
plain English (parsed back out of the registered URL, so the description and the
link cannot disagree), and carries a stop condition: if you did not run
`squelchd auth` yourself in the last few minutes, close the page. The "incapable
of minting tokens" line stays, below all of that, where it is a bound on us
rather than reassurance about a threat it does not cover.

## Privacy

Session ids, claim tokens and their hashes, authorization codes, and consent
URLs are **never logged** — not at debug, not on an error path. Counts,
statuses, and timings only. No error message echoes the value it rejected,
because those strings travel back over the wire into somebody else's log. The
HTML pages are self-contained (inline CSS, no scripts, no external assets, no
fonts to fetch), carry `Cache-Control: no-store`, `Referrer-Policy:
no-referrer`, `Content-Security-Policy: default-src 'none'; style-src
'unsafe-inline'; frame-ancestors 'none'`, and `X-Frame-Options: DENY`, and never
render a code. The CSP is that self-contained property stated to the browser
instead of only to the tests. The address a session was registered from is held
for the per-client cap and is never logged or rendered.

## Development

```sh
cargo test -p squelch-broker
```

No fixtures and no setup: the only state this service has is a `HashMap` that
lives for ten minutes.
