# squelchd

The daemon. One process that hosts everything: the Gmail sync loop and a single HTTP server mounting both doors — the agent door (`/mcp`) and the human door (`/client/*`). This crate sits at the top of the workspace: it depends on `squelch-core`, `squelch-mcp`, and `squelch-api`; nothing depends on it.

## Subcommands

```sh
squelchd auth              # one-time OAuth consent (gmail.readonly), token -> keyring
squelchd auth --write      # BOTH credentials: write (gmail.modify + gmail.send), then read
squelchd auth --headless   # headless box: prints consent URL, binds loopback :8847
squelchd run               # sync loop only, no HTTP (back-compat)
squelchd serve [--bind A]  # the unified daemon: sync + both doors on one port
```

`auth` mints the read credential the sync engine uses. `auth --write` runs two consent flows back to back — the write credential first, then the read one — so the two never drift apart after a renewal. They stay two tokens in two separate slots: the write one is reachable only by the human door's action handlers, and sync and triage never touch it. On a headless host, forward the consent port with `ssh -L 8847:127.0.0.1:8847 <host>` and open the printed URLs locally; both flows reuse that one port.

## Configuration

Config comes from `~/.config/squelch/config.toml` (override with `--config`) plus environment variables (a `.env` in the repo root is loaded via dotenvy):

| Variable | Meaning | Default |
|---|---|---|
| `SQUELCH_CLIENT_ID` / `SQUELCH_CLIENT_SECRET` | your Google Cloud "Desktop app" OAuth client | required for `auth` |
| `SQUELCH_ACCOUNT_EMAIL` | the Gmail account | required |
| `SQUELCH_API_TOKEN` | bearer token for `/client/*` | required for `serve` |
| `SQUELCH_BIND` | bind address for `serve` | `127.0.0.1:8848` |
| `SQUELCH_DB_PATH` | SQLite path | `~/.local/share/squelch/squelch.db` |
| `SQUELCH_POLL_SECS` | Gmail poll interval | `45` |
| `SQUELCH_MCP_ALLOWED_HOSTS` | extra Host values when fronted by a proxy | — |

The listener defaults to loopback and never silently widens. To expose it beyond the machine, front it with a reverse proxy (e.g. `tailscale serve --bg 8848`) and set `SQUELCH_MCP_ALLOWED_HOSTS`.

## Run it

```sh
set -a; source .env; set +a
cargo run --bin squelchd -- auth
cargo run --bin squelchd -- serve
```

See the [root README](../README.md) for the full setup walkthrough and [deploy/DEPLOY.md](../deploy/DEPLOY.md) for the Linux server deployment.
