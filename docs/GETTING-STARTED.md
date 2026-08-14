# Getting started: squelchd on a NAS, Passband on your Mac

An end to end setup for the common self-hosted shape: the daemon runs in Docker
on a NAS (or any always-on box on your network), and the Passband client runs on
your Mac and talks to it.

Budget about 30 minutes. Most of it is the Google Cloud console, which is the
fiddliest part and the one thing nobody can automate away yet.

If you want the reference material instead of a walkthrough:
[deploy/DOCKER.md](../deploy/DOCKER.md) is the image and compose reference,
[deploy/DEPLOY.md](../deploy/DEPLOY.md) is the bare metal runbook with the full
environment variable table.

## Before you start

You need:

- A NAS or always-on machine that runs Docker, reachable from your Mac.
- A Mac for the client.
- The Gmail account you want triaged.
- The container image: `ghcr.io/braelyn-ai/squelchd` is public, so a plain
  `docker pull` works with no registry login. Prefer building it yourself? From
  a checkout of this repo: `docker build -f squelchd/Dockerfile -t squelchd .`
  — note the `-f`: the Dockerfile at the repo *root* builds the APNs relay,
  not the daemon. Budget 10–20 minutes for a cold source build, and swap the
  `image:` line below for your local tag.
- Optional: an Anthropic or OpenAI API key. Without one, triage still works, it
  just runs on heuristics instead of models.

## 1. Create a Google OAuth client

Gmail access needs an OAuth client that belongs to you. This is per person right
now, because a shared verified client is still pending Google review.

1. Go to [console.cloud.google.com](https://console.cloud.google.com) and create
   a project.
2. Enable the **Gmail API** for that project.
3. Configure the OAuth consent screen as **External**, and add your own Gmail
   address as a test user.
4. Create credentials: **OAuth client ID**, application type **Desktop app**.
5. Keep the client ID and client secret somewhere safe for the next step.

Desktop app is the required type, not a preference. It is the only client type
whose secret can live on your own machine, which is what lets your daemon
refresh its own Gmail token forever without depending on anyone else's server.

When you consent later, Google will warn you that the app is not verified. That
is expected for your own client, and you can continue past it. Verification is
what removes the warning, and it is a per project review process.

One consequence of **Testing** status is easy to miss and expensive to learn
live: Google expires a Testing project's refresh tokens after **seven days**,
so your daemon dies weekly with `invalid_grant` until you re-consent. The fix
is one click and needs no verification review: on the OAuth consent screen
page, publish the app to **In production**. The unverified-app warning stays,
the weekly expiry goes away.

## 2. Put the daemon on the NAS

Create a directory on the NAS with a `docker-compose.yml`:

```yaml
services:
  squelchd:
    image: ghcr.io/braelyn-ai/squelchd:latest
    restart: unless-stopped
    ports:
      # LAN reachable, so the Mac client can connect. See the note below.
      - "8848:8848"
    volumes:
      - squelch-data:/data
    environment:
      # Optional. Unset, the human door serves and refuses everything until you
      # pair a device (step 5); set, it is a master key that always works.
      SQUELCH_API_TOKEN: ${SQUELCH_API_TOKEN:-}
      # The Gmail account being triaged. CHANGE THIS: the credential import in
      # step 3 checks the consented mailbox against it and refuses a mismatch.
      SQUELCH_ACCOUNT_EMAIL: you@gmail.com
      SQUELCH_CLIENT_ID: ${SQUELCH_CLIENT_ID:?set in .env}
      SQUELCH_CLIENT_SECRET: ${SQUELCH_CLIENT_SECRET:?set in .env}
      # Only needed if you want the agent door reachable by hostname too.
      # SQUELCH_MCP_ALLOWED_HOSTS: nas.local
      # Optional: turns on model triage.
      # ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}

volumes:
  squelch-data:
```

And an `.env` beside it, mode 0600:

```ini
SQUELCH_API_TOKEN=<paste the output of: openssl rand -hex 32>   # optional, see below
SQUELCH_CLIENT_ID=<client id from step 1>
SQUELCH_CLIENT_SECRET=<client secret from step 1>
```

`SQUELCH_API_TOKEN` is a master password for the human door: one shared secret
every client holds, which nothing can revoke on its own. It is optional now.
Leave it out and the daemon still serves, refusing every request until you pair a
device in step 5, which gives that device its own named token you can revoke by
itself later. Keeping one set is still worth it as a way back in after revoking
every device, and if you do, generate it with `openssl rand -hex 32` rather than
typing something.

**About that published port.** `"8848:8848"` puts the daemon on your LAN so the
Mac client can reach it. The bearer token then travels over plain HTTP inside
your network, which is a reasonable tradeoff on a home LAN you control and a bad
one on a shared or office network. If you would rather not, publish it as
`"127.0.0.1:8848:8848"` instead and put Tailscale in front with
`tailscale serve --bg 8848`, then point the client at the tailnet hostname over
HTTPS.

## 3. Authorize Gmail

The daemon cannot open a browser, and Google will only deliver a login to a
browser on the same machine that started it. So consent happens on your Mac, and
the resulting credential moves to the NAS.

On your Mac, with Docker installed:

```sh
umask 077
docker run --rm -p 8847:8847 \
  -e SQUELCH_CLIENT_ID=<client id> \
  -e SQUELCH_CLIENT_SECRET=<client secret> \
  ghcr.io/braelyn-ai/squelchd:latest \
  auth --export --expose-consent-listener > cred.txt
```

Two details in that command are load-bearing. The `umask 077`: the shell
creates `cred.txt`, not squelchd, and the default umask would make it world
readable. And no `-t`: squelchd writes the blob to stdout and everything
human-facing (the consent URL included) to stderr, but a pty merges the two
streams — with `-it` the redirect would swallow the consent URL into
`cred.txt` and corrupt the blob. Running squelchd directly rather than in a
container, use `--export --out cred.txt` instead, which writes the file mode
0600 itself.

The container cannot open your browser itself, so it prints the consent URL
(with a "copy the URL above manually" note) — open it, approve, and `cred.txt`
ends up holding one line. Then on the NAS, from the compose directory:

```sh
docker compose run --rm -T squelchd auth --import < cred.txt
```

Delete `cred.txt` afterwards. That one line is a live credential for your
mailbox, so do not paste it into chat and do not leave it in Downloads.

Two things that will bite you if they go wrong:

- **Use the same client ID and secret on both machines.** A Gmail refresh token
  is bound to the OAuth client that minted it. It moves between machines fine
  and between clients not at all.
- **The import checks the account with Google.** Before it stores anything, the
  import refreshes each credential in the blob and asks Google which mailbox the
  result opens. If that is not `SQUELCH_ACCOUNT_EMAIL`, it refuses and names both
  addresses. The blob is unsigned, so what it says about itself is a claim; the
  check exists so you cannot quietly end up syncing someone else's mailbox as
  your own. If one credential in the blob fails, none of them are stored.

`--expose-consent-listener` is opt in because it binds the consent listener on
every interface for the length of one login, which it has to do to be reachable
from your browser through Docker's port mapping. What can reach it in that
window is at most a one time code that is useless without a secret held inside
that container.

Prefer to keep it on the NAS? The consent listener binds loopback *inside* the
container, so an SSH tunnel to the NAS cannot reach it through the compose
service above — the working route is the dedicated `auth` service from
[deploy/DOCKER.md](../deploy/DOCKER.md) (it uses `network_mode: host` for
exactly this reason). Add that service to your compose file, then
`ssh -L 8847:127.0.0.1:8847 nas`, run `docker compose run --rm auth`, and open
the printed URL in your Mac browser.

## 4. Start it and check it is alive

```sh
docker compose up -d
docker compose logs -f squelchd
```

You are looking for a line naming both doors. Then smoke test the human door
from your Mac, which also proves the LAN path the client will use:

```sh
curl -s -o /dev/null -w '%{http_code}\n' http://<nas-ip>:8848/client/stats
# => 401, which is correct: no token presented

curl -s -H "Authorization: Bearer $SQUELCH_API_TOKEN" \
  http://<nas-ip>:8848/client/stats
# => JSON
```

A `401` on the first call is the good outcome. It means the door is up and
refusing strangers.

The first sync takes a while and the mailbox fills in progressively, so an
initially sparse client is normal rather than broken.

## 5. Point Passband at it

Build the client from `passband/`:

```sh
cd passband
./build.sh release      # or ./build.sh run to build and launch
```

Then in the app, open **Settings** and go to **Connection**:

- **server url**: `http://<nas-ip>:8848`, or your tailnet HTTPS URL if you went
  that route. No trailing slash, and include the port.
- **api token**: the `SQUELCH_API_TOKEN` value from your `.env`, or a token of
  this Mac's own. For the second, run this on the NAS:

  ```sh
  docker compose exec -u squelch squelchd squelchd pair
  ```

  (`-u squelch` matters: the daemon runs as that user, and a root-run command
  would leave root-owned files beside the database.) It prints a
  `passband://pair?url=...&code=...` link and an `XXXX-XXXX` code. Open the link
  on the Mac, or type the code into the app. The code is good for one device,
  expires in ten minutes, and a handful of wrong guesses burns it. What you get
  back is a named token, listed by `squelchd token list` and revocable on its own
  with `squelchd token revoke <id>`, which is the difference from the shared
  master token above.
- Click **Test**. A green "connected · saved" is the whole confirmation. Settings
  save when you click away from a field, so Test is a re check rather than a
  save button.

If Test fails, jump to troubleshooting below; the error text names the cause.

## 6. Optional extras

**The console.** The daemon serves a small web console for itself at
`http://<nas-ip>:8848/console` — the address the client uses, plus `/console`. It
is where you see what this mailbox is doing and manage the devices allowed to
reach it: mint a pairing code for a new one, revoke one you have lost.

Signing in is the pairing code you already met in step 5. Run this on the NAS:

```sh
docker compose exec -u squelch squelchd squelchd pair
```

and type the `XXXX-XXXX` code into the page. The browser then IS a paired device:
the session cookie is an ordinary device token, so it appears in
`squelchd token list` under the name `console` and
`squelchd token revoke <id>` ends that session for good. Signing out revokes it
too. The same ten-minute, one-shot, burns-after-a-few-wrong-guesses rules apply
to the code.

Hosted Passband shows a "Continue with Google" button on this page as well. That
is a hosted-only hop through the signup service, because Google will not accept a
redirect URI per tenant subdomain, and it appears only when
`SQUELCH_CONSOLE_SSO_URL` is set — which a self-host does not set. On your own
daemon the console is the code form, and it is the same credential either way.

One caveat: on plain HTTP over the LAN, that session cookie travels in the clear
like the bearer token does. If that bothers you, the Tailscale route from step 2
is what puts the console behind TLS.

**Write actions.** Archive, label, and send need a second credential that only
the human door's action handlers load. Sync and triage never touch it, and the
agent door has no write tools at all. Mint it by exporting with `--write`, which
runs two consent screens and carries both credentials in one blob (again no
`-t`, for the same stream-merging reason as step 3):

```sh
umask 077
docker run --rm -p 8847:8847 \
  -e SQUELCH_CLIENT_ID=<client id> -e SQUELCH_CLIENT_SECRET=<client secret> \
  ghcr.io/braelyn-ai/squelchd:latest \
  auth --export --write --expose-consent-listener > cred.txt
```

Import it the same way. Approve both screens with the same Google account, since
one blob names one mailbox.

**Model triage.** Add `ANTHROPIC_API_KEY` to the environment and restart. Daily
spend caps are on by default and tunable from the app's Settings without a
restart.

**Agent door.** To let an MCP client read your triaged mail, point it at
`http://<nas-ip>:8848/mcp` and set `SQUELCH_MCP_ALLOWED_HOSTS` to the hostname
you use. The agent door defaults to allowing only localhost by name, so a
request arriving as `nas.local` is refused until you list it. The human door has
no such restriction, which is why the client works without this.

## Troubleshooting

| What you see | What it means |
|---|---|
| `401` from the client or curl | The token does not match, or there is no token to match. If you set `SQUELCH_API_TOKEN`, compare it against the app's api token and confirm the container restarted after you changed it. If you did not, the door accepts only paired devices: check `docker compose exec -u squelch squelchd squelchd token list`, and pair again if it is empty or you revoked the row you were using. |
| Connection refused from the Mac | The port is not published to the LAN. Check for `"8848:8848"` rather than `"127.0.0.1:8848:8848"`, and that the NAS firewall allows it. |
| `403` on `/mcp` but the client works | Expected until you set `SQUELCH_MCP_ALLOWED_HOSTS` to the hostname you are using. The human door does not do host checks. |
| `invalid_client` on refresh, or an import that says the blob was minted by a different OAuth client | The daemon and the exporting machine used different OAuth clients. Re export with the same client ID and secret. |
| Import refuses and names two addresses | Google says the credential opens a different Gmail account than `SQUELCH_ACCOUNT_EMAIL`. Consent as the right account, or fix the variable. |
| "Google hasn't verified this app" | Expected for your own OAuth client. Continue past it. |
| `invalid_grant` roughly weekly | Your OAuth consent screen is still in **Testing**, which expires refresh tokens after 7 days. Publish it to **In production** (step 1), then re-run the auth flow once. |
| Client connects but is empty | The first sync is still running. Watch `docker compose logs -f squelchd`. |

## Where things live

- Mail database and credentials: the `squelch-data` volume at `/data`.
- Back up that volume, not the container.
- Upgrades: `docker compose pull && docker compose up -d`. Pin a version tag
  (`ghcr.io/braelyn-ai/squelchd:daemon-0.0.1`) instead of `latest` if you would
  rather upgrades be deliberate — released tags carry the `daemon-` prefix.
