# Self-hosting squelchd with Docker

The container path: every `v*` tag pushed to this repo publishes
`ghcr.io/braelyn-ai/squelchd` (linux/amd64 + linux/arm64) via
`.github/workflows/release.yml`. Configuration is environment variables only —
the full reference table lives in [DEPLOY.md](DEPLOY.md#environment-variables);
this file covers what the image adds on top.

## What the image bakes in

| Variable | Baked default | Why |
|---|---|---|
| `SQUELCH_BIND` | `0.0.0.0:8848` | Loopback-only would make the published port unreachable; control exposure with `-p 127.0.0.1:8848:8848` or your proxy. |
| `SQUELCH_CRED_BACKEND` | `file` | No keyring in a container. |
| `SQUELCH_DB_PATH` | `/data/squelch.db` | One volume holds everything. |
| `SQUELCH_CREDENTIALS_PATH` | `/data/credentials.json` | Same volume. |
| `HOME` | `/data` | Puts the one-time fastembed weights download at `/data/.local/share/squelch/models`, inside the volume. |

The entrypoint chowns `/data` (mounted volumes arrive root-owned) and then
drops to the unprivileged `squelch` user via setpriv; the daemon is PID 1 and
takes SIGTERM directly for graceful shutdown.

## Pulling the image

The package is public — `docker pull ghcr.io/braelyn-ai/squelchd:latest`
works with no registry login. To build the same image from source instead
(context must be the workspace root; the repo-root Dockerfile builds the
relay, not the daemon):

```sh
docker build -f squelchd/Dockerfile -t squelchd .
```

## docker-compose.yml

```yaml
x-squelch-env: &squelch-env
  # Bearer for every /client/* route: openssl rand -hex 32
  SQUELCH_API_TOKEN: ${SQUELCH_API_TOKEN:?set in .env}
  SQUELCH_ACCOUNT_EMAIL: you@gmail.com
  # Your GCP "Desktop app" OAuth client.
  SQUELCH_CLIENT_ID: ${SQUELCH_CLIENT_ID:?set in .env}
  SQUELCH_CLIENT_SECRET: ${SQUELCH_CLIENT_SECRET:?set in .env}
  # REQUIRED behind tailscale serve / any proxy, or /mcp returns 403:
  # SQUELCH_MCP_ALLOWED_HOSTS: <box>.<tailnet>.ts.net

services:
  squelchd:
    image: ghcr.io/braelyn-ai/squelchd:latest
    restart: unless-stopped
    ports:
      - "127.0.0.1:8848:8848"   # loopback; front with `tailscale serve --bg 8848`
    volumes:
      - squelch-data:/data
    environment: *squelch-env

  # One-off OAuth consent runs (never started by `up`; see below). Host
  # networking because `squelchd auth` binds 127.0.0.1:8847 by design — the
  # Google desktop-app flow requires a loopback redirect — and a published
  # port can't reach a loopback bind inside a container.
  auth:
    image: ghcr.io/braelyn-ai/squelchd:latest
    profiles: ["auth"]
    network_mode: host
    volumes:
      - squelch-data:/data
    environment: *squelch-env
    command: auth --headless --port 8847

volumes:
  squelch-data:
```

Secrets go in an `.env` file next to the compose file (mode 0600):

```ini
SQUELCH_API_TOKEN=<openssl rand -hex 32>
SQUELCH_CLIENT_ID=<client id>
SQUELCH_CLIENT_SECRET=<client secret>
```

## Headless OAuth (one-time, and on reauth)

Same flow as bare-metal (DEPLOY.md §4), with the auth service standing in for
the binary. From your laptop, forward the fixed port to the box:

```sh
ssh -L 8847:127.0.0.1:8847 box
```

On the box, READ credential only:

```sh
docker compose run --rm auth
```

Or READ + WRITE (two consent URLs, write first — what you want on a fresh box
if you'll ever send/modify mail):

```sh
docker compose run --rm auth auth --write --headless --port 8847
```

Open the printed `accounts.google.com` URL(s) in your laptop browser; the
redirect tunnels back through SSH into the auth container. Credentials land in
the shared `/data` volume, owned by the service user. Then:

```sh
docker compose up -d squelchd
docker compose logs -f squelchd
# => squelchd: serving agent door http://0.0.0.0:8848/mcp and human door ...
```

Smoke-test exactly as in DEPLOY.md §7 (curl `/client/stats` with and without
the bearer, MCP `initialize` against `/mcp`).

## Upgrades

```sh
docker compose pull && docker compose up -d squelchd
```

Pin a version (`image: ghcr.io/braelyn-ai/squelchd:0.1.0`) if you'd rather
upgrades be deliberate; `latest` tracks the newest tag.
