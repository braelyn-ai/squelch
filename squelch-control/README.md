# squelch-control: the Passband hosted signup control plane

One invite code plus one Google consent becomes one provisioned tenant daemon.
This crate is the web half of the hosted tier described in
[`docs/HOSTED.md`](../docs/HOSTED.md); the cluster half is `squelch-warden`,
which runs in-cluster and turns these two calls into a tenant's pod.

```
browser ── GET /  ──► signup form (invite code + address)
        ── POST /signup ──► validate, open session, 303 to Google
        ── Google consent ──►
        ── GET /oauth/callback ──► exchange, CREATE tenant (learn its recipient),
                                   SEAL to it, INSTALL credential, success page
```

## Provisioning is two calls

Wire v2 splits provisioning so that no single key opens two mailboxes:

1. `POST /v1/tenants {label, account_email}` → `201 {recipient}`. The warden
   mints an age identity **for that tenant**, writes it into that tenant's
   Kubernetes Secret, and answers with the public half. The tenant is now
   `pending`: no credential, nothing running.
2. `PUT /v1/tenants/{label}/credentials {cred_read_ciphertext}` → `200`
   `{pair_code, pair_url, deep_link}`. The warden installs the sealed credential,
   applies the workload, and mints the first pairing code.

A failure between the two leaves a **pending** tenant. That is retriable by
design: the invite is not spent, the address is held for that Google account, and
a retry walks call 1 again and gets the **same** recipient back. `POST /signup`
therefore lets a pending label through its availability check; the warden decides
whether the mailbox coming back is the one that reserved it (a different account
gets a 409, which this crate reports as "already taken").

## What this process holds, and what it cannot do

| Holds | Does not hold |
|---|---|
| the confidential **web** OAuth client id + secret | any age **identity** (private key) |
| one tenant's age **recipient** (a public key), for the length of one signup | any long-lived recipient, and no key shared between tenants |
| tenant labels, mailbox addresses, invite **hashes** | invite codes, pairing codes, tokens or ciphertext at rest |

A plaintext refresh token exists in this process's memory for the length of one
callback: between the token exchange returning and the age encryption finishing.
It is never written to the store, never logged, and never rendered. What leaves
is ASCII armor addressed to **that one tenant**, which only that tenant's daemon
can open (its pod mounts the identity Secret and reads it via
`SQUELCH_CRED_AGE_IDENTITY`).

The plaintext inside the armor is rendered by
`squelch_core::credentials::credentials_file_plaintext`, so the on-disk shape the
daemon reads is decided in exactly one place. A bare `StoredToken` would encrypt
and decrypt fine and then deserialize into an empty slot map, which surfaces as
"no stored credentials" on a box nobody can easily debug.

The corollary worth stating plainly: **a full compromise of this Railway service
yields no mailbox.** It yields the ability to provision new tenants and to seal
things nobody can open, plus a list of labels and addresses. The refresh tokens
are in the cluster, each encrypted to a key that is not here and that opens
nothing else.

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
| `SQUELCH_CONTROL_WARDEN_URL` | **yes** | The warden's base URL, e.g. `https://warden.passband.email`. |
| `SQUELCH_CONTROL_WARDEN_TOKEN` | **yes** | Bearer presented to the warden. Must match `SQUELCH_WARDEN_TOKEN` in the cluster. |
| `SQUELCH_CONTROL_DB_PATH` | no | Control store. Default `/data/control.sqlite3`. |
| `SQUELCH_CONTROL_TRUSTED_PROXY_HOPS` | no | How many proxies write `X-Forwarded-For` in front of this listener. `0` (default) meters the TCP peer, which behind a platform edge means one shared rate-limit bucket. Set `1` on Railway. |
| `SQUELCH_CONTROL_LOG` | no | `tracing` filter. Default `info`. |

There is deliberately **no age recipient variable**. It existed in v1, when one
key opened every tenant; under v2 the recipient arrives per signup in the answer
to the first provisioning call and this process never holds one between requests.
An old `SQUELCH_CONTROL_AGE_RECIPIENT` left in a deployment's environment is now
ignored, so delete it: a variable that looks load-bearing and is not is worse
than no variable at all.

The Google endpoints are **pinned constants**, deliberately not environment
variables: those requests carry the client secret, and "which host do we send
the secret to" must not be a deploy-time typo.

## CLI

```sh
squelch-control serve                     # the signup site
squelch-control invite issue --count 5    # mint codes; printed ONCE, on stdout
squelch-control invite issue --ttl 7      # ...good for a week instead of 30 days
squelch-control invite list               # ids and status. Never codes or hashes
squelch-control invite revoke <id>        # revoke an unused code
squelch-control tenants                   # what has been provisioned
```

Invite codes are Crockford `XXXX-XXXX-XXXX-XXXX` (80 bits), stored only as a
lowercase hex SHA-256, single use, and good for 30 days unless `--ttl` says
otherwise. A lost code is re-issued, never recovered. Codes minted before the
space grew are still accepted and expire 30 days after they were issued.

They are longer than the daemon's pairing codes on purpose. A pairing code is
read aloud, lives for minutes, and only works against a daemon the guesser must
already be able to reach; an invite is pasted out of an email, sits in a table
for weeks, and one hit against a public form is a free tenant.

A code is **held** from the moment the form is posted until the signup finishes
or fails, so one code cannot open two signups at once. The hold names the
session, lapses with it, and is handed back immediately when a signup fails, so
a retry does not have to wait it out.

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
`warden.<base domain>` pointing at the k3s node) is in `deploy/hosted/SETUP.md`.

## Notes on the surface

- No JavaScript, no external assets, `default-src 'none'` CSP, `no-store`, and
  `X-Frame-Options: DENY` on every page.
- Per-route rate limits: the form is metered like a page, `POST /signup` is
  tight (it is the one route where a stranger can guess at a secret), and
  `/oauth/callback` is the most generous, because refusing it destroys a consent
  the user has already granted.
- Every invite failure is one message. Wrong, spent, revoked, and malformed are
  indistinguishable from the outside.
- `/mcp` does not exist here, and a tenant's Ingress answers 404 for it too. The
  hosted MVP ships the human door only.
