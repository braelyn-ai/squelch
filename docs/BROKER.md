# squelch-broker: the consent relay

> **STATUS 2026-08-04: DO NOT DEPLOY for the self-host tier. The code-parking
> flow below cannot be granted by Google.** The crate is built, hardened, and
> tested; the flow it implements is the one flow Google will not authorize for
> the client type self-host requires. Details in "The blocker" immediately
> below, and the replacement design in "Where this goes instead". The hosted
> tier's web-client callback is unaffected and is where this code lands next.

The broker was designed to fix headless consent UX (docker on a NAS, a VPS)
without ever being trusted. It parks a Google OAuth authorization code for a few
minutes so the daemon that requested it can claim it. It holds **no OAuth client
credentials, no tokens, no mail** — the auth code it parks is cryptographically
useless without the PKCE verifier, which never leaves the daemon.

One correction to that framing, from the security audit: the broker holds no
tokens, but in this design it does hold **consent origination for the whole
fleet** — it stores the consent URL and decides where each user's browser goes.
That is worth stealing, and "holds nothing worth stealing" was too strong.

## The blocker

Self-host must own its refresh token outright: a refresh token is bound to the
client that minted it, so if the daemon's client were confidential (held by us)
our uptime would be in the path of every hourly refresh. That forces a
**Desktop-type client**, whose secret Google treats as non-confidential.

Google permits Desktop clients exactly one kind of redirect target: **loopback**
(`http://127.0.0.1:port`, `http://[::1]:port`). Custom URI schemes are removed
("no longer supported due to the risk of app impersonation") and out-of-band
copy/paste is removed. So `https://auth.passband.email/callback` **cannot be
registered** on the client self-hosted daemons use, and the OAuth code never
reaches the broker.

The two ways out of that are both worse than the thing they buy:

- **Use a Web-type client for self-host.** Its secret is confidential and must
  not ship in a public image, and refresh needs it, so the daemon depends on our
  infra forever. This is precisely what `docs/HOSTED.md` rejected.
- **Publish a Web client's secret in the image anyway.** Then anyone can drive a
  Google-verified, Passband-branded consent screen, and Google resets the client
  when they notice, breaking every deployed daemon at once.

Google's own OOB migration guide offers nothing for browserless hosts: the
loopback flow "requires you to be listening on a local web server."

## Where this goes instead

The code lands on the machine running the *browser*, so the only real question
is how to move it to the daemon. Two steps, neither of which needs an OAuth
redirect on our domain:

1. **`squelchd auth --export` / `--import` (ship first).** Run consent on any
   machine that has a browser and the binary, where the sanctioned loopback flow
   works unchanged; it prints a token blob that you paste into the headless
   daemon over stdin. This is rclone's `rclone authorize` pattern. Zero infra,
   zero new Google configuration, available today. The pasted blob is a live
   refresh token, so it is read from stdin and never from argv.
2. **The broker as an encrypted token courier (saves this crate).** The daemon
   generates an ephemeral X25519 keypair and prints a link carrying the session
   id; the exporting machine encrypts the token blob to that public key and
   posts the ciphertext; the daemon claims and decrypts. Google is never
   redirected to our domain, so the client-type wall does not apply. The session
   store, TTL, one-time claim, rate limiting, and interstitial all survive; only
   the parked payload changes from an auth code to ciphertext, and the
   registration validation changes from consent-URL shape to key material.

   This makes the trust claim *stronger*, not weaker: today's version is
   incapable of minting because of PKCE, that version is incapable of reading
   because of end-to-end encryption.

Everything below this line describes the flow as implemented and audited. It is
accurate about the code and blocked as a deployment.

Deployed as its own service (`auth.passband.email`), separate from the APNs
relay. See `docs/HOSTED.md` for the two-client OAuth architecture.

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
