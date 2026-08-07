# Onboarding dry-run findings

A fresh-user walkthrough of README "Getting started" and docs/GETTING-STARTED.md,
performed 2026-08-04 on a Mac with Docker (OrbStack), no prior context, source
access, and a deliberately fake Google OAuth client (real consent cannot be
scripted). Each finding says where the doc lies, misleads, or leaves a stranger
stuck. Fixes land in the same branch; this file is the record of *why*.

Legend: **[lie]** doc states something false · **[gap]** doc omits something a
stranger needs · **[footgun]** doc-suggested command that misbehaves ·
**[stale]** references something retired/renamed · **[verified]** claim tested
and confirmed accurate.

## 1. README workspace table links to a crate that no longer exists — [stale]

`README.md` links `squelch-desktop/README.md` ("the Tauri desktop client").
The directory is gone — the desktop client was retired 2026-07-30 and the
commit that removed it is in this branch's history. First link a curious new
user clicks, 404 on GitHub.

## 2. README "Building the macOS client" builds from a directory that doesn't exist — [lie]

The whole section says `cd squelch-client-swift`. The client lives in
`passband/` (and is named Passband). Anyone following the README verbatim gets
`cd: no such file or directory` at the first command. `docs/SECURITY.md` also
cites `squelch-client-swift/Sources/...` for its CSP enforcement pointer —
same rename, same fix.

## 3. No documented way to build the squelchd *image* from source — [gap]

The root `Dockerfile` builds **squelch-relay** (the Railway push relay), not
the daemon. A user told "build from source" who types `docker build .` gets an
APNs relay with no explanation. The actual image is:

```sh
docker build -f squelchd/Dockerfile -t squelchd .
```

(context must be the workspace root — the file itself says so, but no
onboarding doc does). GETTING-STARTED's only offered fallback is
`cargo build --release -p squelchd`, a bare binary that doesn't help someone
whose deployment shape is "Docker on a NAS" — the entire premise of that guide.

Verified: `docker build -f squelchd/Dockerfile .` succeeds from a clean
checkout (arm64 Mac, roughly 15 minutes cold).

## 4. GETTING-STARTED's compose hardcodes the placeholder mailbox — [footgun]

The `docker-compose.yml` in §2 sets `SQUELCH_ACCOUNT_EMAIL: you@gmail.com`
inline, and no step ever says to change it. `.env` gets three real values;
the fourth stays a placeholder in the yaml. The failure arrives two steps
later, at import time, as a wrong-mailbox refusal naming `you@gmail.com` —
correct behavior, mystifying error, self-inflicted by the doc.

Verified (local refusal, no network):

```
error: credential error: this blob was exported for someoneelse@gmail.com,
but this daemon is configured for you@gmail.com; nothing was stored. Export
again while signed in as you@gmail.com, or fix account_email in the config.
```

## 5. The documented export command corrupts cred.txt and hides the consent URL — [footgun]

§3 says:

```sh
docker run --rm -it -p 8847:8847 ... auth --export --expose-consent-listener > cred.txt
```

squelchd correctly writes the blob to stdout and everything human-facing
(including the consent URL) to stderr — but `-t` allocates a pty, and a pty
**merges the two streams**. Result with `-it` plus a redirect: the terminal
shows nothing, and `cred.txt` receives the consent URL, the progress text,
*and* the blob interleaved — a file `--import` then refuses as damaged. The
flag pair sabotages the redirect the same line depends on.

Fix: drop `-t` (and `-i`, which nothing reads): streams stay separate, URL on
the terminal, clean blob in the file. (Verified: see §12 test log.)

Related, same step: "Your browser opens, you approve" — it does not. Inside a
container `webbrowser::open` has no browser to reach; squelchd prints the URL
with a "copy the URL above manually" note (verified). The doc now says so.

## 6. GETTING-STARTED's SSH fallback cannot work as written — [lie]

§3's "prefer to keep it on the NAS" route:

```sh
ssh -L 8847:127.0.0.1:8847 nas
docker compose run --rm squelchd auth --headless --port 8847
```

`docker compose run` on that compose file joins the default bridge network.
The consent listener binds 127.0.0.1:8847 **inside the container's network
namespace**; the SSH tunnel lands on the NAS host's loopback, where nothing
listens. (`run` doesn't publish ports either, and the service only maps 8848
anyway.) deploy/DOCKER.md solves exactly this with a dedicated `auth` service
using `network_mode: host` — GETTING-STARTED's compose file has no such
service, so the fallback it describes is not runnable from the file it gives
you.

Verified: with the listener up inside the container, `curl 127.0.0.1:8847`
from the host is connection-refused.

## 7. README `.env` recipe regenerates the API token on every source — [footgun]

README §2 puts `SQUELCH_API_TOKEN=$(openssl rand -hex 32)` *inside* `.env`,
then §3 has you `source .env`. Every shell that sources the file mints a
different token: restart the daemon from a new terminal and every client you
configured yesterday gets 401. The command substitution belongs in the step
that *creates* the file, with a literal value stored.

## 8. Private-repo scaffolding is about to become false everywhere — [stale]

"**The repo is private, so the image is too**" (GETTING-STARTED §Before you
start), the GHCR PAT login blocks (GETTING-STARTED §2, DOCKER.md §Pulling),
and the `denied: denied` troubleshooting row all describe the pre-flip world.
After the flip the image is public and every one of these paragraphs sends a
new user on a pointless collaborator/PAT errand. Replaced with: public image,
no login, plus the real build-from-source command (finding 3).

## 9. README still sells `auth --broker` — [stale]

README §3 ends with a paragraph recommending `squelchd auth --broker <url>`
and pointing at DEPLOY.md §8 to run your own. docs/BROKER.md has said **DO NOT
DEPLOY** since 2026-08-04: Google's desktop-client policy makes the
code-parking flow undeployable for self-host. The replacement is exactly the
`--export/--import` flow the README already documents two paragraphs earlier;
the successor design (encrypted token courier via Passband.app) is GitHub
issue #15. Removed from README; BROKER.md gets a status banner up top;
DEPLOY.md §8 gets a do-not-deploy pointer.

## 10. DEPLOY.md §1 understates the build prerequisites — [gap]

`cargo build --release -p squelchd` on a fresh box needs more than rustup:

- a C/C++ toolchain (`build-essential`/`g++`): rusqlite compiles bundled
  SQLite, and `ort` links a static onnxruntime that wants `libstdc++` — the
  squelchd Dockerfile has to install `g++` for exactly this reason;
- glibc ≥ 2.38 (Debian 13 "trixie", Ubuntu 24.04+): ort's prebuilt static
  onnxruntime references `__isoc23_strtol` and friends; bookworm's 2.36
  cannot link it. This is why the images are trixie-based. A Debian 12
  Hetzner box — the doc's own example target — fails the build with no hint.

## 11. Compose interpolation guards — [verified]

`docker compose config` without `.env` fails exactly as intended, naming the
variable and the hint: `required variable SQUELCH_CLIENT_ID is missing a
value: set in .env`. Good pattern, kept.

## 12. Dynamic test log

Environment: OrbStack docker 29.4.0, image built from this checkout as
`squelchd-dryrun`, fake OAuth client id/secret, compose file from
GETTING-STARTED §2 verbatim (image swapped for the local build — the only
deviation).

| Documented claim | Result |
|---|---|
| `docker compose config` without `.env` refuses, naming the variable | PASS (finding 11) |
| serve logs a line naming both doors | PASS — `squelchd: serving agent door http://0.0.0.0:8848/mcp and human door http://0.0.0.0:8848/client/*`, printed before the embedder finishes downloading, so it appears fast |
| `/client/stats` without bearer → 401 | PASS |
| `/client/stats` with bearer → JSON | PASS — `{"bands":{...},"last_history_id":null...}` with zero synced mail, matching the "initially sparse client is normal" note |
| `/mcp` initialize from localhost → JSON-RPC result | PASS — 200, SSE stream, `mcp-session-id` header, serverInfo |
| `/mcp` with foreign Host header → 403 until allow-listed | PASS — `Host: nas.local` → 403 Forbidden |
| export prints consent URL to stderr, blob to stdout | PASS — without `-t`, stdout stays empty until consent; URL + guidance on stderr |
| `-it ... > cred.txt` merges streams (finding 5) | CONFIRMED BROKEN — with `-t` the consent URL appears on the container's stdout, i.e. inside `cred.txt` |
| mis-paste refused by name, not serde error | PASS — "this is not a squelch credential blob: it does not start with `squelch-cred-v1.`" |
| wrong-account blob refused locally, names both addresses | PASS — refusal names both, before any network call |
| refresh-token-less blob refused | PASS — "would stop working within the hour; nothing was stored" |
| right-account fake token → Google `invalid_client` | PASS — surfaced as "Google refused the Read credential ... minted by a different one", matching the troubleshooting row |
| SSH-fallback listener unreachable from host (finding 6) | CONFIRMED BROKEN — listener answers inside the container, host gets connection-refused on 8847 |

Two notes on the test environment, neither a docs bug:

- The cold image build drove the host load average past 80 (Spotlight
  indexing the build output) and wedged the Docker VM for several minutes.
  "Budget about 30 minutes" is optimistic on the build-from-source path; the
  rewrite says 10–20 minutes for the build alone.
- Testing on a machine that runs its own squelchd: the local daemon's
  loopback bind on 8848 shadows the container's `0.0.0.0` publish for
  `127.0.0.1` requests, so host-side curls silently interrogated the wrong
  daemon until re-run inside the container. A NAS user can't hit this (the
  doc's curls target `<nas-ip>`), so no doc change — recorded for the next
  person who dry-runs on a dev Mac.
