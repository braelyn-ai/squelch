# squelch-broker: the consent relay

The broker fixes headless consent UX (docker on a NAS, a VPS) without ever being
trusted. It parks a Google OAuth authorization code for a few minutes so the
daemon that requested it can claim it. It holds **no OAuth client credentials,
no tokens, no mail** — the auth code it parks is cryptographically useless
without the PKCE verifier, which never leaves the daemon. We are not trusted
with tokens because we're nice; we are incapable of minting them.

Deployed as its own service (`auth.passband.email`), separate from the APNs
relay. See `docs/HOSTED.md` for the two-client OAuth architecture that makes
this shape load-bearing: self-hosted daemons exchange and refresh tokens
directly with Google, so our uptime is never their dependency.

## Flow

1. Daemon generates a PKCE verifier+challenge, a random `session_id`, and a
   random `claim_token` (32 bytes each, base64url, no padding). It builds the
   full Google consent URL itself (its own client credentials, `redirect_uri` =
   the broker's `/callback`, `state` = the session id).
2. Daemon registers the session (`POST /v1/sessions`), sending the consent URL
   and the SHA-256 of the claim token — never the token itself.
3. Daemon prints `https://auth.passband.email/link?s=<session_id>`. The user
   opens it on any device with a browser.
4. `/link` shows a small interstitial (what is being authorized, and the
   "incapable of minting" line) and forwards to Google consent.
5. Google redirects the auth code to `/callback`; the broker parks it in
   memory: one session, one code, first write wins, short TTL.
6. Daemon polls `POST /v1/claim` with the claim token. One successful claim
   returns the code and deletes the session.
7. Daemon exchanges the code **itself** with Google: PKCE verifier + its client
   credentials + the same `redirect_uri`. The broker is out of the loop forever.

## Wire contract (v1)

All bodies JSON. No authentication on any route: every stranger's self-hosted
daemon is a legitimate client. Defense is per-IP rate limiting, a global
session cap, high-entropy identifiers, and holding nothing worth stealing.

### `POST /v1/sessions`

```json
{ "session_id": "<base64url, 32 bytes decoded>",
  "claim_token_hash": "<lowercase hex SHA-256, 64 chars>",
  "auth_url": "https://accounts.google.com/o/oauth2/v2/auth?..." }
```

`auth_url` is validated strictly or the broker is an open redirect wearing our
domain: scheme `https`, host exactly `accounts.google.com`, path exactly
`/o/oauth2/v2/auth`, and query params `redirect_uri` == this broker's own
`/callback` URL, `state` == `session_id`, `response_type` == `code`,
`code_challenge` present, `code_challenge_method` == `S256`, `client_id`
present.

Responses: `201 {"expires_in": <secs>}` · `400` malformed/invalid ·
`409` duplicate session id · `429` rate limited · `503` session table full.

### `GET /link?s=<session_id>`

Human-facing interstitial HTML: names the product, states what is being
authorized and what the broker cannot do, links to the parked `auth_url`.
Unknown or expired session → `404` HTML telling the user to re-run
`squelchd auth`. Self-contained page: no external assets, no scripts.

### `GET /callback?code=...&state=...` (or `error=...&state=...`)

Google's redirect target. `state` looks up the session; the code (or the
error, e.g. `access_denied`) is parked. First write wins; a repeat callback
for an already-parked session gets a "link already used" page. Unknown
`state` → `404`. Success → HTML "authorized — return to your terminal."
The page never displays the code.

### `POST /v1/claim`

```json
{ "session_id": "...", "claim_token": "..." }
```

The broker hashes the presented token and compares constant-time against the
registered hash. Responses, all `200` unless noted:

- `{"status": "pending"}` — consent not completed yet; keep polling.
- `{"status": "complete", "code": "..."}` — one-time: the session is deleted
  in the same operation. A second claim is `404`.
- `{"status": "denied", "error": "access_denied"}` — user refused consent;
  session deleted.
- `403` — claim token mismatch.
- `404 {"status": "unknown"}` — expired, already claimed, or never existed.

## Invariants

- **In-memory only.** An auth code lives seconds; durability is a liability.
  A broker restart loses pending consents; the recovery is re-running
  `squelchd auth`.
- **TTL ~10 minutes** from registration, sessions and parked codes alike;
  expired sessions purge lazily on access plus a periodic sweep.
- **Never logged:** session ids, claim tokens and hashes, auth codes, auth
  URLs. Counts, statuses, and timings only.
- **Constant-time** comparison for the claim token hash.
- **One-time claim:** returning the code and deleting the session are atomic
  under one lock.

## Hosted seam

Sessions carry a `kind` (only `SelfHost` today). The hosted signup flow
(`docs/HOSTED.md`) will add a kind whose consent URL belongs to the
confidential web client and whose claim path is the control plane rather than
a polling daemon — the store, TTL, validation, and callback machinery are
shared; only who built the URL and who claims differ.

## Daemon side

`squelchd auth --broker <url>` runs the flow above instead of the loopback
listener; everything downstream (scope plan, credential kinds, storage
backends) is unchanged. Client credentials remain env/config-driven
(`SQUELCH_CLIENT_ID`/`SQUELCH_CLIENT_SECRET`); once Google verification
clears, "embedded credentials" is the daemon image baking those env defaults
in — no code change.
