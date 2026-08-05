# Auth verification pass — findings

Pre-release verification of every `squelchd auth` path (2026-08-05, branch
`auth-verification`). Scope: `squelchd/src/bin/squelchd.rs`,
`squelch-core/src/auth/{mod,transfer}.rs`, `squelch-core/src/credentials.rs`.
Code and test fixes are committed here; doc corrections below are for the docs
owner and were NOT applied (README.md and docs/ are owned by another agent).

## A. Bugs found and fixed

Commits: `cabef86` (squelch-core), `3768e28` (squelchd).

1. **A hung token endpoint wedged the daemon forever.** The refresh client in
   `refresh_stored_token_detailed` set no timeout (unlike `guarded_http`, whose
   own doc comment claimed every path was bounded). A token endpoint that
   accepted the connection and said nothing would hang the sync loop's blocking
   refresh and `auth --import` verification indefinitely. Fixed: the refresh
   shares the 30s `EXCHANGE_HTTP_TIMEOUT`, and `pub(crate)` seams
   (`refresh_stored_token_detailed_at`, `verify_transfer_credential_at`) let
   tests point the pinned Google URLs at a scripted socket. Prod callers can
   only reach the pinned constants. Wire test:
   `a_hung_token_endpoint_cannot_wedge_a_refresh`.

2. **The second consent of `auth --write --headless` failed with "Address
   already in use" on Linux.** Both flows bind the same fixed port (8847), the
   first flow's accepted connection lands in server-side TIME_WAIT (the
   listener answers the redirect and closes first), and std's
   `TcpListener::bind` sets no `SO_REUSEADDR`. Reproduced empirically in a
   Linux container; macOS happens to allow the rebind, which is why local dev
   never saw it. Same failure applied to `--export --write
   --expose-consent-listener`. Fixed: the consent listener binds through
   socket2 with `SO_REUSEADDR` (permits binding over TIME_WAIT remnants only;
   hijacking a live listener would need `SO_REUSEPORT`, which stays off).
   Regression test: `a_fixed_port_survives_back_to_back_consents`.

3. **`SQUELCH_BROKER_URL=""` broke plain `auth`.** clap reports an empty env
   var as a present value, so a blank leftover in a compose file silently
   turned every `auth` run into a `Broker("")` run that died on URL parse.
   Fixed: a blank AMBIENT value is no broker at all; a TYPED `--broker ""`
   still fails loudly in `broker_base`. Test:
   `an_empty_ambient_broker_url_is_no_broker_at_all`.

4. **A typed `--port` that nothing would bind was silently ignored.** Only
   `--headless` and `--export --expose-consent-listener` bind a fixed port;
   `auth --port 9100` bound an ephemeral port and said nothing, leaving the
   operator's tunnel pointing at a port nobody listens on. Now warned on
   stderr. Test: `a_typed_port_nothing_will_bind_is_called_out`.

5. **The export same-account refusal printed Google's mailbox strings
   undisarmed.** Every other externally-sourced string that reaches a terminal
   goes through `printable()`; the "first consent authorized X but this one
   authorized Y" error did not. Fixed via `check_export_same_account`. Test:
   `a_mismatched_export_names_both_mailboxes_without_painting_the_terminal`.

6. **The `--expose-consent-listener` banner printed the requested port, not
   the bound one** (`--port 0` printed "port 0"). Now reads the listener's
   actual address.

## B. Verified held (adversarial tests added, no bug found)

New wire-level tests fake Google on a scripted local socket and run the real
verification end to end:

- `--import` of a blob minted by a different OAuth client fails at paste time
  naming `SQUELCH_CLIENT_ID` (`invalid_client` off the wire, not a canned
  string).
- A verified blob whose token opens a different mailbox than configured is
  refused before anything stores, naming both addresses.
- A refresh naming no scopes fails closed (the slot would be the blob's word
  alone).
- An oversized profile answer is refused before a byte is read; garbage from
  the token endpoint errors without echoing the refresh token.
- The happy path and the scope-UNION case pass: a Read-slot credential whose
  refresh reports readonly+modify+send is accepted. This is deliberate —
  Google unions grants per Cloud project, so `check_scope_grant` is a subset
  floor, and an exact match would refuse the Read entry of every
  `--export --write` blob. Do not "fix" it.

Already solid before this pass (existing tests confirmed by review): blob
codec refusals (prefix, version, empty, duplicate kinds, truncation), stdin
cap, atomic multi-slot store with keyring rollback, 0600 file modes including
`--export --out` over a pre-existing looser file, stdout/stderr split on
export, terminal-echo suppression on TTY import, CSRF state handling on both
listener policies, response-body caps, secrets absent from Debug/error paths,
transport resolution vs ambient env.

## C. Known limitations (deliberate, documented here so nobody re-finds them)

- The token-exchange response body (inside the oauth2 crate) is not
  size-capped the way profile/broker bodies are; the endpoint is pinned to
  `oauth2.googleapis.com` over TLS with redirects refused, so the exposure is
  Google itself.
- `--export --out` follows symlinks like any Unix tool; the blob lands 0600
  wherever the path points. Keep `--out` inside a directory you own.
- Terminal-echo restore is best effort by construction: a run killed mid-paste
  needs `stty sane` (the code comments say the same).
- Pre-existing and out of this pass's scope: `cargo test --workspace` does not
  compile at HEAD because `squelch-mcp/src/server.rs:534` calls
  `store.stats(acct)` without the new `bands_since` argument (verified present
  at `41aeceb`, before this branch's work; that crate is owned by other
  in-flight work). Everything else is green:
  `cargo test --workspace --exclude squelch-mcp` passes (squelch-core 578,
  squelchd 18, all other crates green).

## D. Doc corrections (for the docs owner — not applied)

> **APPLIED 2026-08-05** on the `dry-run` branch, alongside the onboarding
> dry-run fixes (`ONBOARDING-FINDINGS.md`): items 4 and 5 were independently
> found and fixed by that pass; 1, 2, 3, 6, 7, and 8 were applied from this
> list. Line numbers below cite the pre-fix tree.

Verified against code, each with the load-bearing citation:

1. **README.md:78 recommends the broker as a live route.** docs/BROKER.md:3-8
   says DO NOT DEPLOY and the flow is unbuildable for the Desktop-type client
   README step 1 prescribes (Google allows Desktop clients loopback redirects
   only, so the broker's `/callback` can never be registered). Replace with a
   deprecation note pointing at `--export`/`--import`; stop linking
   deploy/DEPLOY.md §8 as a how-to. Related: DEPLOY.md §8 (lines 259-330) is a
   full broker runbook whose step at 305-308 is impossible; docs/HOSTED.md:93,
   115, 185-189 still describe the relay as pending/planned, contradicting the
   correction block at HOSTED.md:82-91; BROKER.md:271-273 describes `--broker`
   in the present tense with no pointer to its own DO-NOT-DEPLOY header.

2. **README.md:72 and BROKER.md:82-83 claim the scope check proves slot
   membership both ways** ("stops a hand-edited kind from filing a modify+send
   token in the Read slot"). It cannot: `check_scope_grant` is a subset floor
   because Google unions grants per project (see
   `a_read_slot_credential_carrying_the_union_grant_still_passes`). The
   guarantee holds one direction only (a readonly token cannot land in the
   Write slot). Reword: the separation squelch enforces is which slot each
   code path may load, not a capability difference in the token.

3. **README.md:150 "The sync credential is scoped gmail.readonly" overstates
   the token's reach** (same union caveat; also appears at
   squelch-api/README.md:9 and squelchd/README.md:15). The request is
   readonly; the invariant is structural: sync gets a READ-bound store, the
   write slot is loaded only by human-door action handlers.

4. **docs/GETTING-STARTED.md:155-157 SSH-tunnel fallback cannot work with the
   compose file in the same doc.** `--headless` binds the CONTAINER's
   loopback; the §2 service uses bridge networking and `docker compose run`
   publishes nothing. Needs the `network_mode: host` one-off service shape
   from deploy/DOCKER.md:55-66.

5. **docs/GETTING-STARTED.md:127 "Your browser opens" during a containerized
   `--export`.** The image has no browser; the URL prints on stderr and must
   be copied to the laptop browser. Say so, or first-timers sit waiting.

6. **README.md:54 "token lands in the OS keyring" is macOS-only.** Default
   backend is keyring on macOS, mode-0600 JSON file everywhere else
   (config.rs:99-117); README's env list also omits `SQUELCH_CRED_BACKEND`
   and `SQUELCH_CREDENTIALS_PATH`.

7. **docs/GETTING-STARTED.md:46-48 says Testing status costs only a consent
   warning.** It also expires refresh tokens every 7 days; the daemon's own
   `invalid_grant` message names publishing to "In production" as the fix
   (credentials.rs). Add the consequence and a troubleshooting row for
   "invalid_grant roughly weekly".

8. Minor: deploy/DEPLOY.md:142 and DOCKER.md:56 say plain `auth` binds a
   fixed port (only `--headless` does); DEPLOY.md/DOCKER.md never mention
   `--export`/`--import` although README presents them as the browserless
   answer; squelchd/README.md:8-10 lists only three of the auth flags.

## E. Manual E2E checklist (real Google consents; cannot be automated)

Run with a real Google account and the production OAuth client. Items marked
LINUX need a Linux host or container; they are the ones this pass's bugs hid
on.

Fresh loopback consent
- [ ] `squelchd auth` on a machine with a browser: consent opens, token stored
      (macOS: keyring service "squelch"; Linux: `credentials.json` mode 0600
      — check `ls -l`), daemon syncs with it.
- [ ] Approve the consent signed in as the WRONG Google account: refused with
      both addresses named, "nothing was stored", and re-running recovers.

Double consent (`--write`)
- [ ] `squelchd auth --write`: exactly two consent screens (WRITE then READ);
      both slots re-minted; announcement lines only after each store's
      read-back.
- [ ] LINUX `squelchd auth --write --headless` through one
      `ssh -L 8847:127.0.0.1:8847` tunnel: the SECOND consent binds port 8847
      immediately (this was the EADDRINUSE bug; it must not say "Address
      already in use").
- [ ] Abort the second consent (close the tab, wait for timeout): the WRITE
      credential from the first flow is stored, the message says so, and
      re-running `auth --write` finishes cleanly.

Export / import (fresh account or revoked grant recommended)
- [ ] `squelchd auth --export 2>err.txt >out.txt` on the laptop: `out.txt` is
      EXACTLY one line starting `squelch-cred-v1.`; every prompt, URL, and
      warning is in `err.txt`; the mailbox Google named appears on stderr.
- [ ] `squelchd auth --export --write`: two consents; approving the second as
      a DIFFERENT account refuses with both mailboxes named and prints no
      blob.
- [ ] `squelchd auth --export --out cred.txt` where `cred.txt` pre-exists
      mode 0644: file ends up 0600 and imports byte-identically.
- [ ] On the daemon host:
      `docker exec -i <container> squelchd auth --import < cred.txt` — both
      credentials verified against Google before ANY store, both slots
      announced, sync starts with the read slot. Delete `cred.txt` after.
- [ ] `squelchd auth --import` at a TTY: the paste is NOT echoed; after the
      run, typed characters ARE echoed again (type `echo ok`). Kill the
      process mid-paste and confirm `stty sane` recovers the terminal.
- [ ] Import the same blob on a host with a DIFFERENT
      `SQUELCH_CLIENT_ID`/`SECRET`: refused at paste time naming
      SQUELCH_CLIENT_ID, nothing stored (check both slots unchanged).
- [ ] Import a blob exported for a different mailbox than the daemon's
      `account_email`: refused before any network call, both addresses named.

Containerized export (`--expose-consent-listener`)
- [ ] LINUX `docker run -p 8847:8847 ... auth --export
      --expose-consent-listener --port 8847`: banner names port 8847, consent
      from the laptop browser lands, blob prints. A stray
      `curl 'host:8847/?code=x&state=y'` mid-wait gets 400 and does NOT end
      the flow.

Hygiene (while any consent is waiting)
- [ ] `ps aux | grep squelchd` shows no token, blob, or secret in argv.
- [ ] Shell history after an import contains no blob (stdin only).
- [ ] The consent URL printed contains the client_id but never the
      client_secret.

Refresh lifecycle
- [ ] After import, force a refresh (wait an hour or edit `expires_at` in the
      credentials file to the past): sync obtains a fresh token without
      re-consent, file re-persisted 0600.
- [ ] Revoke the grant at myaccount.google.com/permissions: next refresh
      fails with the `invalid_grant` message naming `squelchd auth` as the
      fix, and does not loop hot.
