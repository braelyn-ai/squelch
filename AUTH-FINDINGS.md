# Auth: standing limitations and manual verification

Covers `squelchd auth` across `squelchd/src/bin/squelchd.rs`,
`squelch-core/src/auth/{mod,transfer}.rs`, and `squelch-core/src/credentials.rs`.
Everything here is a deliberate constraint or open work; fixed bugs and applied
doc corrections live in the git history, not in this file.

## A. Known limitations (deliberate)

- The token-exchange response body (inside the oauth2 crate) is not
  size-capped the way profile/broker bodies are; the endpoint is pinned to
  `oauth2.googleapis.com` over TLS with redirects refused, so the exposure is
  Google itself.
- `--export --out` follows symlinks like any Unix tool; the blob lands 0600
  wherever the path points. Keep `--out` inside a directory you own.
- Terminal-echo restore is best effort by construction: a run killed mid-paste
  needs `stty sane`.
- `check_scope_grant` is a subset floor, not an exact match, because Google
  unions grants per Cloud project. A Read-slot credential whose refresh reports
  readonly+modify+send is accepted on purpose — an exact match would refuse the
  Read entry of every `--export --write` blob. Do not "fix" it.

## B. Manual E2E checklist (real Google consents; cannot be automated)

Run with a real Google account and the production OAuth client. Items marked
LINUX need a Linux host or container.

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
      immediately, and must not say "Address already in use".
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
