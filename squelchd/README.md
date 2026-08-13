# squelchd

The daemon. One process that hosts everything: the Gmail sync loop and a single HTTP server mounting both doors — the agent door (`/mcp`) and the human door (`/client/*`). This crate sits at the top of the workspace: it depends on `squelch-core`, `squelch-mcp`, and `squelch-api`; nothing depends on it.

## Subcommands

```sh
squelchd auth              # one-time OAuth consent (requests gmail.readonly); token -> macOS keyring / 0600 file on Linux
squelchd auth --write      # BOTH credentials: write (gmail.modify + gmail.send), then read
squelchd auth --headless   # headless box: prints consent URL, binds loopback :8847
squelchd auth --export     # consent here, print a one-line blob to move elsewhere (--out FILE for 0600; --write for both slots)
squelchd auth --import     # store a blob from stdin, verified against Google first
squelchd auth --broker URL # consent via a relay — undeployable for self-host, see docs/BROKER.md
squelchd run               # sync loop only, no HTTP (back-compat)
squelchd serve [--bind A]  # the unified daemon: sync + both doors on one port
squelchd pair [--url BASE] # mint a pairing code + deep link for a new device
squelchd token issue --name NAME   # mint a device token, printed once
squelchd token list                # every token, revoked ones included; no secrets
squelchd token revoke ID           # kill one device, effective on its next request
```

`auth` mints the read credential the sync engine uses. `auth --write` runs two consent flows back to back — the write credential first, then the read one — so the two never drift apart after a renewal. They stay two tokens in two separate slots: the write one is reachable only by the human door's action handlers, and sync and triage never touch it. On a headless host, forward the consent port with `ssh -L 8847:127.0.0.1:8847 <host>` and open the printed URLs locally; both flows reuse that one port.

## Configuration

Config comes from `~/.config/squelch/config.toml` (override with `--config`) plus environment variables (a `.env` in the repo root is loaded via dotenvy):

| Variable | Meaning | Default |
|---|---|---|
| `SQUELCH_CLIENT_ID` / `SQUELCH_CLIENT_SECRET` | your Google Cloud "Desktop app" OAuth client | required for `auth` |
| `SQUELCH_ACCOUNT_EMAIL` | the Gmail account | required |
| `SQUELCH_API_TOKEN` | master bearer for `/client/*`; optional, see below | — |
| `SQUELCH_BIND` | bind address for `serve` | `127.0.0.1:8848` |
| `SQUELCH_DB_PATH` | SQLite path | `~/.local/share/squelch/squelch.db` |
| `SQUELCH_POLL_SECS` | Gmail poll interval | `45` |
| `SQUELCH_MCP_ALLOWED_HOSTS` | extra Host values when fronted by a proxy | — |

The listener defaults to loopback and never silently widens. To expose it beyond the machine, front it with a reverse proxy (e.g. `tailscale serve --bg 8848`) and set `SQUELCH_MCP_ALLOWED_HOSTS`.

### Carrier polling

Package tracking keeps working from mail alone, but the daemon can also ask the carriers directly. That is bring-your-own-credentials and off until you provide some: no credentials means no poller task and no carrier API is ever contacted.

```toml
[carriers]
poll_interval_hours = 6        # baseline cadence for an in-flight package
ofd_poll_interval_mins = 60    # tighter cadence once it is out for delivery
max_age_days = 45              # stop chasing a package this old
max_failures = 5               # consecutive permanent failures before retiring it

[carriers.ups]                 # SQUELCH_UPS_CLIENT_ID / _CLIENT_SECRET
client_id = "..."
client_secret = "..."

[carriers.fedex]               # SQUELCH_FEDEX_CLIENT_ID / _CLIENT_SECRET
client_id = "..."
client_secret = "..."

[carriers.usps]                # SQUELCH_USPS_CONSUMER_KEY / _CONSUMER_SECRET
consumer_key = "..."
consumer_secret = "..."

[carriers.dhl]                 # SQUELCH_DHL_API_KEY / SQUELCH_DHL_DAILY_CAP
api_key = "..."
daily_cap = 200
```

Each carrier is independent, and a credential pair set in the environment materializes a carrier the file never mentions, so a container needs no `config.toml`. The cadence knobs have `SQUELCH_CARRIERS_*` equivalents. Where to get each carrier's credentials, what the cadence actually does, and the metrics to alert on: [docs/SHIPMENTS.md](../docs/SHIPMENTS.md).

## Human-door credentials

`SQUELCH_API_TOKEN` is the master key and is never going away: it works with an empty token table, so it is the way back in after revoking every device. It is now optional, though. `serve` comes up without it and accepts **per-device tokens** instead:

```sh
squelchd pair                      # prints XXXX-XXXX + a passband://pair deep link, good for 10 minutes
squelchd token issue --name phone  # or mint one directly; the sqd_ token is printed ONCE, on stdout
squelchd token list                # id, name, created, last used, revoked
squelchd token revoke 3            # effective on the next request; nothing is cached
```

Each token is stored only as a hash, so a lost one is re-issued rather than recovered. Pairing codes are single-use, expire in ten minutes, burn after a few failed claims, and one mint supersedes the last. A daemon with no master token and no device tokens serves and 401s everything, which is exactly the state `squelchd pair` gets you out of.

The link `pair` prints names the address `serve` would bind (`SQUELCH_BIND`, else `127.0.0.1:8848`), so a daemon on a non-default port advertises the right one. Pass `--url` whenever the device is not this machine, and prefer an https front (`tailscale serve`): over plain http to another host, the code and the token it buys are readable in transit, which `pair` warns about.

## Run it

```sh
set -a; source .env; set +a
cargo run --bin squelchd -- auth
cargo run --bin squelchd -- serve
```

See the [root README](../README.md) for the full setup walkthrough and [deploy/DEPLOY.md](../deploy/DEPLOY.md) for the Linux server deployment.
