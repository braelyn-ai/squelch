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
- Access to the container image. **The repo is private, so the image is too.**
  Either get added as a collaborator and use a GitHub token as below, or build
  from source with `cargo build --release -p squelchd`. Sort this out first; it
  is the most common place people get stuck at step 2.
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

## 2. Put the daemon on the NAS

If the image is private and you have access, log in once on the NAS with a
GitHub token that has `read:packages`:

```sh
echo "$GITHUB_PAT" | docker login ghcr.io -u <github-username> --password-stdin
```

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
      SQUELCH_API_TOKEN: ${SQUELCH_API_TOKEN:?set in .env}
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
SQUELCH_API_TOKEN=<paste the output of: openssl rand -hex 32>
SQUELCH_CLIENT_ID=<client id from step 1>
SQUELCH_CLIENT_SECRET=<client secret from step 1>
```

`SQUELCH_API_TOKEN` is the password for the human door. It is the only thing
standing between anything on your network and your mail, so generate it with
`openssl rand -hex 32` rather than typing something.

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
docker run --rm -it -p 8847:8847 \
  -e SQUELCH_CLIENT_ID=<client id> \
  -e SQUELCH_CLIENT_SECRET=<client secret> \
  ghcr.io/braelyn-ai/squelchd:latest \
  auth --export --expose-consent-listener > cred.txt
```

The `umask 077` matters: the shell creates `cred.txt`, not squelchd, and the
default umask would make it world readable. Running squelchd directly rather
than in a container, use `--export --out cred.txt` instead, which writes the
file mode 0600 itself.

Your browser opens, you approve, and `cred.txt` ends up holding one line. Then
on the NAS, from the compose directory:

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

Prefer to keep it on the NAS? If you can SSH there, the older route still works:
`ssh -L 8847:127.0.0.1:8847 nas`, then `docker compose run --rm squelchd auth
--headless --port 8847`, and open the printed URL in your Mac browser.

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
- **api token**: the `SQUELCH_API_TOKEN` value from your `.env`.
- Click **Test**. A green "connected · saved" is the whole confirmation. Settings
  save when you click away from a field, so Test is a re check rather than a
  save button.

If Test fails, jump to troubleshooting below; the error text names the cause.

## 6. Optional extras

**Write actions.** Archive, label, and send need a second, separately scoped
credential that only the action handlers can load. Sync and triage never touch
it. Mint it by exporting with `--write`, which runs two consent screens and
carries both credentials in one blob:

```sh
umask 077
docker run --rm -it -p 8847:8847 \
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
| `401` from the client or curl | The token does not match. Compare the app's api token against `SQUELCH_API_TOKEN` in `.env`, and confirm the container restarted after you changed it. |
| Connection refused from the Mac | The port is not published to the LAN. Check for `"8848:8848"` rather than `"127.0.0.1:8848:8848"`, and that the NAS firewall allows it. |
| `403` on `/mcp` but the client works | Expected until you set `SQUELCH_MCP_ALLOWED_HOSTS` to the hostname you are using. The human door does not do host checks. |
| `invalid_client` on refresh, or an import that says the blob was minted by a different OAuth client | The daemon and the exporting machine used different OAuth clients. Re export with the same client ID and secret. |
| Import refuses and names two addresses | Google says the credential opens a different Gmail account than `SQUELCH_ACCOUNT_EMAIL`. Consent as the right account, or fix the variable. |
| "Google hasn't verified this app" | Expected for your own OAuth client. Continue past it. |
| `denied: denied` pulling the image | No access to the private package. Log in to ghcr.io with a `read:packages` token, or build from source. |
| Client connects but is empty | The first sync is still running. Watch `docker compose logs -f squelchd`. |

## Where things live

- Mail database and credentials: the `squelch-data` volume at `/data`.
- Back up that volume, not the container.
- Upgrades: `docker compose pull && docker compose up -d`. Pin a version tag
  instead of `latest` if you would rather upgrades be deliberate.
