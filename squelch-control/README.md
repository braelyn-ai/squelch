# squelch-control: the Passband hosted signup control plane

One invite code plus one Google consent becomes one provisioned tenant daemon.
This crate is the web half of the hosted tier described in
[`docs/HOSTED.md`](../docs/HOSTED.md); the VPS half is `squelch-warden`.

```
browser ── GET /  ──► signup form (invite code + address)
        ── POST /signup ──► validate, open session, 303 to Google
        ── Google consent ──►
        ── GET /oauth/callback ──► exchange, SEAL, provision via warden, success page
```

## What this process holds, and what it cannot do

| Holds | Does not hold |
|---|---|
| the confidential **web** OAuth client id + secret | any age **identity** (private key) |
| the box's age **recipient** (a public key) | any tenant's mail, database, or decrypted token |
| tenant labels, mailbox addresses, invite **hashes** | invite codes, pairing codes, tokens at rest |

A plaintext refresh token exists in this process's memory for the length of one
callback: between the token exchange returning and the age encryption finishing.
It is never written to the store, never logged, and never rendered. What leaves
is ASCII armor addressed to the VPS, which only the tenant's own daemon can open
(it is handed the identity file by systemd as `SQUELCH_CRED_AGE_IDENTITY`).

The corollary worth stating plainly: **a full compromise of this Railway service
yields no mailbox.** It yields the ability to provision new tenants and to seal
things nobody can open, plus a list of labels and addresses. The refresh tokens
are on the VPS, encrypted to a key that is not here.

## Environment

Everything is validated at startup and a bad value is a refusal to boot.

| Variable | Required | Meaning |
|---|---|---|
| `SQUELCH_CONTROL_BIND` | no | Listener. Default `127.0.0.1:8852`; the container entrypoint widens it to `0.0.0.0:$PORT`. |
| `SQUELCH_CONTROL_PUBLIC_URL` | **yes** | This service's externally visible origin, e.g. `https://signup.passband.email`. The Google `redirect_uri` is this plus `/oauth/callback`. |
| `SQUELCH_CONTROL_BASE_DOMAIN` | **yes** | The hosted base domain, e.g. `passband.email`. Tenant URLs are `https://<label>.<base domain>`. Never hardcoded. |
| `SQUELCH_CONTROL_CLIENT_ID` | **yes** | The confidential **web** OAuth client id (not the desktop client the self-hosted daemon uses). |
| `SQUELCH_CONTROL_CLIENT_SECRET` | **yes** | Its secret. |
| `SQUELCH_CONTROL_COOKIE_KEY` | **yes** | HMAC key for the signup cookie. base64 or hex, at least 32 bytes decoded. `openssl rand -base64 48`. |
| `SQUELCH_CONTROL_AGE_RECIPIENT` | **yes** | The VPS box's age recipient (`age1...`), from `age-keygen`. The public half only. |
| `SQUELCH_CONTROL_WARDEN_URL` | **yes** | The warden's base URL, e.g. `https://warden.passband.email`. |
| `SQUELCH_CONTROL_WARDEN_TOKEN` | **yes** | Bearer presented to the warden. Must match `SQUELCH_WARDEN_TOKEN` on the VPS. |
| `SQUELCH_CONTROL_DB_PATH` | no | Control store. Default `/data/control.sqlite3`. |
| `SQUELCH_CONTROL_TRUSTED_PROXY_HOPS` | no | How many proxies write `X-Forwarded-For` in front of this listener. `0` (default) meters the TCP peer, which behind a platform edge means one shared rate-limit bucket. Set `1` on Railway. |
| `SQUELCH_CONTROL_LOG` | no | `tracing` filter. Default `info`. |

The Google endpoints are **pinned constants**, deliberately not environment
variables: those requests carry the client secret, and "which host do we send
the secret to" must not be a deploy-time typo.

## CLI

```sh
squelch-control serve                     # the signup site
squelch-control invite issue --count 5    # mint codes; printed ONCE, on stdout
squelch-control invite list               # ids and status. Never codes or hashes
squelch-control invite revoke <id>        # revoke an unused code
squelch-control tenants                   # what has been provisioned
```

Invite codes are Crockford `XXXX-XXXX`, the same shape as the daemon's pairing
codes, stored only as a lowercase hex SHA-256, and single use. A lost code is
re-issued, never recovered.

The invite commands talk to the store directly and need none of the serving
configuration, so codes can be minted on a box with no OAuth client in its
environment.

## Deploying to Railway

A **separate Railway service** off this same repo, alongside the APNs relay
(root `Dockerfile`) and the consent broker (`Dockerfile.broker`). Railway picks
this one per-service:

1. New service from the repo. Set `RAILWAY_DOCKERFILE_PATH=Dockerfile.control`.
   (`railway.toml` at the repo root belongs to the relay and must not be
   inherited.)
2. Attach a volume mounted at `/data` for the control store.
3. Set every required variable from the table above.
4. Add the custom domain `signup.<base domain>` and point a CNAME at the
   Railway target. The same hostname must be registered as an authorized
   redirect URI (`https://signup.<base domain>/oauth/callback`) on the web
   OAuth client in Google Cloud Console.
5. Health check: `GET /healthz`.

DNS for the rest of the hosted tier (`*.<base domain>` and
`warden.<base domain>` pointing at the VPS) is in `deploy/hosted/SETUP.md`.

## Notes on the surface

- No JavaScript, no external assets, `default-src 'none'` CSP, `no-store`, and
  `X-Frame-Options: DENY` on every page.
- Per-route rate limits: the form is metered like a page, `POST /signup` is
  tight (it is the one route where a stranger can guess at a secret), and
  `/oauth/callback` is the most generous, because refusing it destroys a consent
  the user has already granted.
- Every invite failure is one message. Wrong, spent, revoked, and malformed are
  indistinguishable from the outside.
- `/mcp` does not exist here, and the hosted Caddy configuration refuses it for
  tenants too. The hosted MVP ships the human door only.
