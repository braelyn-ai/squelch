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

Where the code comes from, when the waitlist is configured (the trio in the
table below; unset, none of it is mounted and both URLs are a 404):

```
site     ── POST /waitlist ──► one row per address, same 200 for a duplicate
operator ── GET  /admin ──────► token login, then the list
         ── POST /admin/approve ──► mint an invite, email it through Resend
         ── POST /admin/send ─────► revoke that code, mint and mail a fresh one
```

The admin POSTs take a `SameSite=Strict` session cookie AND a same-origin
`Origin`/`Sec-Fetch-Site`, because "same site" includes every sibling
`passband.app` name and a page on one of those could otherwise press these
buttons.

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

## One consent, both slots

Signup asks for `gmail.readonly`, `gmail.modify`, and `gmail.send` **in one
consent** (squelch-core's `GMAIL_READONLY_SCOPE` + `WRITE_SCOPES`, never spelled
out here), and seals the resulting grant into **both** credential slots: `email`
for the Read side and `email#write` for the Write side. Hosted Passband ships
compose, archive, and label, and the daemon's action path loads the Write slot,
which nothing else on a hosted box can fill: there is no second consent screen a
tenant can reach from inside the app.

The scope check after the exchange is therefore a floor over all three. Google
unions grants across a Cloud project, so a token may report **more** than was
asked for and that passes; **less** does not. A partial consent (a box unchecked
on Google's screen) provisions nothing at all: the callback stops before call 1,
hands the invite code back, and says all three permissions are needed. A tenant
sealed on a partial grant would look fine until the first archive or send, with
no way back to Google from the app.

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
| `SQUELCH_CONTROL_PUBLIC_URL` | **yes** | This service's externally visible origin, e.g. `https://signup.passband.app`. Deliberately NOT on the tenant base domain: wildcard subdomains there mean "a tenant" and nothing else. The Google `redirect_uri` is this plus `/oauth/callback`. |
| `SQUELCH_CONTROL_BASE_DOMAIN` | **yes** | The hosted base domain, e.g. `passband.email`. Tenant URLs are `https://<label>.<base domain>`. Never hardcoded. |
| `SQUELCH_CONTROL_CLIENT_ID` | **yes** | The confidential **web** OAuth client id (not the desktop client the self-hosted daemon uses). |
| `SQUELCH_CONTROL_CLIENT_SECRET` | **yes** | Its secret. |
| `SQUELCH_CONTROL_COOKIE_KEY` | **yes** | HMAC key for the signup cookie. base64 or hex, at least 32 bytes decoded. `openssl rand -base64 48`. |
| `SQUELCH_CONTROL_WARDEN_URL` | **yes** | The warden's base URL, e.g. `https://warden.passband.app` (product domain, same reasoning as the public URL). |
| `SQUELCH_CONTROL_WARDEN_TOKEN` | **yes** | Bearer presented to the warden. Must match `SQUELCH_WARDEN_TOKEN` in the cluster. |
| `SQUELCH_CONTROL_DB_PATH` | no | Control store. Default `/data/control.sqlite3`. |
| `SQUELCH_CONTROL_BIFROST_URL` | pair | The Bifrost LLM gateway's governance origin, `https://` only. With the token below it is all-or-nothing: both set mints a per-tenant virtual key at signup, neither set provisions keyless tenants, anything partial (including a budget or model list on their own) refuses to boot. |
| `SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN` | pair | The gateway admin's `username:password`, sent as HTTP Basic on every governance call (a session bearer expires after 30 days and does not belong here). Exactly one `:` between two nonempty halves, at least 32 characters total. It can mint unbounded LLM spend; treat it like the warden bearer. |
| `SQUELCH_CONTROL_LLM_BUDGET_USD` | no | Monthly USD budget stamped on each minted key. Default `5.00`. Only meaningful with the gateway pair set; set alone it refuses to boot. |
| `SQUELCH_CONTROL_LLM_MODELS` | no | Comma-separated model allow-list stamped on each minted key. Default `claude-haiku-4-5,claude-sonnet-5`. Never empty (the gateway treats an empty list as deny-all). Only meaningful with the gateway pair set; set alone it refuses to boot. |
| `SQUELCH_CONTROL_ADMIN_TOKEN` | trio | The operator's password for `/admin`, where waitlist rows are approved. At least 32 characters: `openssl rand -base64 32`. With the two below it is all-or-nothing: all three set mounts the waitlist and admin routes, none set leaves them unmounted (a 404, not a 403), anything partial refuses to boot. |
| `SQUELCH_CONTROL_RESEND_API_KEY` | trio | Resend sending key, used for the one call that mails an approved applicant their invite. Printable ASCII, no spaces. Mint it sending-only and domain-restricted. |
| `SQUELCH_CONTROL_INVITE_FROM` | trio | The `From:` invites are sent as, e.g. `Passband <invites@passband.app>`. Must be an address on a domain **verified at Resend** or every send is refused. |
| `SQUELCH_CONTROL_WAITLIST_ORIGIN` | no | The one browser origin allowed to post the public waitlist form, echoed as `Access-Control-Allow-Origin` on that route only. Default `https://passband.app`. Only meaningful with the trio above set; set alone it refuses to boot. |
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
squelch-control llm mint <label>          # mint + install a Bifrost virtual key (also rotates)
squelch-control llm revoke <label>        # revoke the recorded key and forget it
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

The `llm` commands need the Bifrost trio, the warden pair, and the store — but
still no OAuth client or cookie key. `llm mint` backfills a tenant that signed
up while Bifrost was down (signup is fail-soft about the key: an outage costs
the tenant its key, never the signup), and rotates one that has a key already;
rotation prints the old key's id, which stays live in Bifrost until revoked
there. Key **values** are never printed, stored, or logged — only ids are.

## Deploying to Railway

A **separate Railway service** off this same repo, alongside the APNs relay
(root `Dockerfile`) and the consent broker (`Dockerfile.broker`). The one thing
that decides which of the three a service builds is its **config-as-code file**,
so that is step 1:

1. New service from the repo. Set its **Config-as-code file path** to
   `railway.control.toml`, which pins `dockerfilePath = "Dockerfile.control"`
   along with the health check and restart policy.

   Do not rely on `RAILWAY_DOCKERFILE_PATH` for this. Left at the default,
   a service inherits the root `railway.toml` — which belongs to the relay and
   pins `dockerfilePath = "Dockerfile"` — and **config-as-code outranks the
   service variable**. This service's first deploys built and shipped the relay
   image with the variable set correctly, which is a green deploy serving the
   wrong binary and no error anywhere. Setting the variable as well is harmless;
   it is just not what makes this work.
2. Attach a volume mounted at `/data` for the control store.
3. Set every required variable from the table above.
4. Add the custom domain for the signup surface (`signup.passband.app` — the
   product domain, never the tenant base domain) and point a CNAME at the
   Railway target. The same hostname must be registered as an authorized
   redirect URI (`https://signup.passband.app/oauth/callback`) on the web
   OAuth client in Google Cloud Console.
5. Health check: `GET /healthz`.

DNS for the rest of the hosted tier (`*.<base domain>` tenant wildcard and
`warden.passband.app` pointing at the k3s node) is in `deploy/hosted/SETUP.md`;
what the live deployment actually looks like — box, domains, secret inventory —
is `deploy/hosted/PRODUCTION.md`.

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
