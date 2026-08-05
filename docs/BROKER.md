# squelch-broker: the consent relay

> **STATUS 2026-08-04: DEPRECATED IN PLACE — DO NOT DEPLOY for the self-host
> tier. The code-parking flow below cannot be granted by Google.** The crate is
> built, hardened, and tested; the flow it implements is the one flow Google
> will not authorize for the client type self-host requires. It is superseded
> by `squelchd auth --export` / `--import` (shipped — see
> [GETTING-STARTED.md](GETTING-STARTED.md) §3), with the encrypted token
> courier that saves this crate tracked in
> [issue #15](https://github.com/braelyn-ai/squelch/issues/15). Details in
> "The blocker" immediately below, and the replacement design in "Where this
> goes instead". The hosted tier's web-client callback is unaffected and is
> where this code lands next.

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

   **Blob format (v1).** One line, no internal whitespace, because it has to
   survive a copy-paste through a terminal, an SSH session, and a chat client:

   ```
   squelch-cred-v1.<base64url-unpadded JSON>
   ```

   The prefix is load-bearing twice: it lets a mis-paste be refused by name
   instead of as a serde error, and it makes the string greppable in a support
   thread. The JSON is `{"version":1,"account":"<the mailbox Google reported>",
   "credentials":[{"kind":"read"|"write","token":<StoredToken>}]}` — one entry
   per credential, so `--export --write` moves both slots in one paste.

   **Nothing in the blob is evidence about the blob.** It carries no signature,
   so `account` and `kind` are claims by whoever pasted it. Import runs three
   cheap local refusals first (an `account` that is not the daemon's configured
   `account_email`, trimmed and case-insensitive; any entry with no
   `refresh_token`, since an access token alone dies in an hour, long after
   anyone would connect the failure back to the paste; an empty credential list
   or a repeated `kind`), and then takes every remaining entry to Google BEFORE
   it writes anything: a refresh against this host's OAuth client, `users.
   getProfile` on the result held against `account_email`, and the granted
   scopes held against what that entry's slot requires. That last one is what
   stops a hand-edited `kind` from filing a modify+send token in the Read slot.
   One failed entry stores none of them. base64url is an encoding, not
   encryption: the blob is plaintext credential material and nothing in this
   path pretends otherwise, which is exactly why what it says about itself
   settles nothing.

   **Writing the blob to a file.** `--export --out <path>` creates it mode 0600.
   A `> cred.txt` redirect takes the ambient umask instead, so prefix that form
   with `umask 077`.

   **Same OAuth client on both machines.** A refresh token is bound to the
   client that minted it and to the granted scopes, never to a host — that is
   the whole reason moving it works. It is also the constraint: the exporting
   machine and the daemon must use the same `client_id`/`client_secret`, or
   every refresh is `invalid_client`. Once the image bakes in verified
   credentials this is automatic; anyone using their own Google credentials on
   two machines hits it first.

   **Exporting from inside a container.** The published image is the easiest way
   to get the binary onto a laptop, but under `docker run -p 8847:8847` the
   consent listener bound to the container's own `127.0.0.1` is unreachable.
   `--export --expose-consent-listener` binds `0.0.0.0:<--port>` for the length
   of one consent. The `redirect_uri` stays loopback either way (Google accepts
   nothing else from a Desktop client): it names the *browser's* `127.0.0.1`.
   What reaches an exposed listener is at most an authorization code, inert
   without the PKCE verifier held in that process and checked against a per-run
   `state` first — a real reduction, not zero, which is why it is opt-in. The
   other half of that exposure is availability: strangers can also connect and
   say nothing, so on this bind every connection has a read timeout, the wait
   has a deadline, and a mismatched `state` is answered 400 and waited out
   rather than ending the export.
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
4. `/link` shows an interstitial: who could have sent this link (unknowable, and
   it says so), what approving grants, the requested scopes in plain English, a
   stop condition, and then the link onward to Google consent.
5. Google redirects the auth code to `/callback`; the broker parks it in
   memory: one session, one code, first write wins, short TTL.
6. Daemon polls `POST /v1/claim` with the claim token. One successful claim
   returns the code and deletes the session.
7. Daemon exchanges the code **itself** with Google: PKCE verifier + its client
   credentials + the same `redirect_uri`. The broker is out of the loop forever.

## Wire contract (v1)

All bodies JSON. No authentication on any route: every stranger's self-hosted
daemon is a legitimate client. Defense is per-client rate limiting, a global
session cap plus a per-client one, high-entropy identifiers, and holding no
token or credential. Not "nothing worth stealing": see the correction at the top
of this document, since a stranger's registration is served in our voice on our
domain, which is what the scope allowlist and the rewritten interstitial exist
to bound.

Each route has its own token bucket, because a 429 costs a different thing on
each: `POST /v1/sessions` 30/min (it allocates, and a real client registers
once), `POST /v1/claim` 600/min (a daemon polls it every 2s for ten minutes),
`GET /link` 300/min, `GET /callback` 1200/min (refusing it destroys consent the
user already granted, and Google does not redirect twice). "Client" is the TCP
peer unless the deployment sets `SQUELCH_BROKER_TRUSTED_PROXY_HOPS`, which is
also what enables the per-client live-session cap (16): behind a proxy with it
unset, every client shares one identity and a per-client cap would be a cap on
the whole deployment.

Every response body is a few hundred bytes, and the daemon reads at most
**64 KiB** of one (declared length or not) before treating the answer as off
contract and ending the flow. The hosts this feature exists for are NASes and
Pis; an unbounded body is an OOM on them.

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
present, and `scope` a non-empty **subset of the scopes squelchd requests**
(`gmail.readonly`, `gmail.modify`, `gmail.send` — squelch-core's
`GMAIL_READONLY_SCOPE` and `WRITE_SCOPES`). Without that last check a stranger
registers a consent URL asking for anything Google will grant (full
`https://mail.google.com/`) and the broker serves the page that asks a human to
grant it. Every checked parameter, `scope` included, may appear only once.

Responses: `201 {"expires_in": <secs>}` · `400` malformed/invalid ·
`409` duplicate session id · `429` rate limited, or this client already holds
its ceiling of live sessions · `503` session table full.

### `GET /link?s=<session_id>`

Human-facing interstitial HTML. It **cannot** say who parked the link
(registration is unauthenticated), so it does not: it says someone ran
`squelchd auth` and sent this link here and that it may not have been the
reader, leads with what approving grants, lists the requested scopes in plain
English, gives the stop condition ("if you did not run `squelchd auth` yourself
in the last few minutes, close this page"), and only then links to the parked
`auth_url`. The "incapable of minting tokens" line sits below that, as a bound
on the broker rather than as reassurance about who is asking. Unknown or expired
session → `404` HTML telling the user to re-run `squelchd auth`. Self-contained
page: no external assets, no scripts, `Content-Security-Policy: default-src
'none'; style-src 'unsafe-inline'; frame-ancestors 'none'` and
`X-Frame-Options: DENY`.

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
  URLs, and the client address a session was registered from (held only for the
  per-client cap). Counts, statuses, and timings only.
- **Constant-time** comparison for the claim token hash.
- **One-time claim:** returning the code and deleting the session are atomic
  under one lock.

## Hosted seam

Sessions carry a `kind` (only `SelfHost` today). The hosted signup flow
(`docs/HOSTED.md`) will add a kind whose consent URL belongs to the
confidential web client and whose claim path is the control plane rather than
a polling daemon — the store, TTL, validation, and callback machinery are
shared; only who built the URL and who claims differ.

The seam enforces itself now rather than when the second kind lands:
`POST /v1/claim` decides what it serves with an exhaustive match on the kind and
no catch-all arm, so adding a variant is a compile error at that decision point
instead of a variant silently inheriting the polling path.

## Daemon side

`squelchd auth --broker <url>` runs the flow above instead of the loopback
listener; everything downstream (scope plan, credential kinds, storage
backends) is unchanged.

The exchange is guarded, and identically for both flows, because a code says
nothing about who approved it: whichever Google session was signed in on the
browser is the one that consented, and neither a loopback listener nor a broker
can see that. Before a token is stored the daemon checks the granted scope set
covers what it asked for (a partial consent is fatal, not a warning) and calls
`users.getProfile` to confirm the mailbox behind the token is the configured
`account_email`. A mismatch is refused by name and nothing is written.

Client credentials remain env/config-driven
(`SQUELCH_CLIENT_ID`/`SQUELCH_CLIENT_SECRET`); once Google verification
clears, "embedded credentials" is the daemon image baking those env defaults
in — no code change.
