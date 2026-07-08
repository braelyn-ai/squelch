# Deploying squelch to a headless Linux box (Hetzner + tailnet)

The "baddiebox" runbook. Target: a headless Linux VM on a tailnet. One process
(`squelchd serve`) runs the Gmail sync loop AND serves both doors:

- **Agent door** — MCP Streamable HTTP at `/mcp` (narrow, read-only, sealed-absent).
- **Human door** — authenticated `/client/*` API for your desktop client.

Both bind to loopback `127.0.0.1:8848` by default; `tailscale serve` fronts them
so only your tailnet can reach the box. Everything here also runs on macOS for
dev — the only Linux-specifics are systemd and the `file` credential backend.

---

## 1. Build

On the box (native):

```sh
# Rust toolchain (edition 2024 => needs a recent stable).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo build --release -p squelchd
# => target/release/squelchd  (thin-LTO, symbols stripped)
sudo install -m 0755 target/release/squelchd /usr/local/bin/squelchd
```

**Cross-compile from a Mac (optional):** the release profile is `lto="thin"`,
`strip="symbols"`. `rusqlite` is `bundled` (compiles SQLite from source), so you
need a cross C toolchain. Easiest path is [`cross`](https://github.com/cross-rs/cross):

```sh
cargo install cross
cross build --release --target x86_64-unknown-linux-gnu -p squelchd
scp target/x86_64-unknown-linux-gnu/release/squelchd box:/tmp/
ssh box 'sudo install -m 0755 /tmp/squelchd /usr/local/bin/squelchd'
```

---

## 2. Service account + directories

```sh
sudo useradd --system --home /var/lib/squelch --shell /usr/sbin/nologin squelch
sudo mkdir -p /var/lib/squelch /etc/squelch
sudo chown squelch:squelch /var/lib/squelch
sudo chmod 0700 /var/lib/squelch          # db + file-backend creds live here
sudo chmod 0750 /etc/squelch
```

`/var/lib/squelch` is the ONLY writable path the hardened unit grants
(`ReadWritePaths`). Keep `SQUELCH_DB_PATH` and `SQUELCH_CREDENTIALS_PATH` inside it.

---

## 3. Config + environment

Secrets and tunables go in `/etc/squelch/env` (referenced by the systemd unit).
The box has no keyring, so use the **file** credential backend.

Generate the human-door bearer token:

```sh
API_TOKEN=$(openssl rand -hex 32)
echo "$API_TOKEN"        # copy into your desktop client's config too
```

Write `/etc/squelch/env` (root-owned, mode 0640, group `squelch`):

```ini
# --- human door ---
SQUELCH_API_TOKEN=<paste the openssl rand -hex 32 value>

# --- credentials: file backend (no keyring on the box) ---
SQUELCH_CRED_BACKEND=file
SQUELCH_CREDENTIALS_PATH=/var/lib/squelch/credentials.json

# --- account + storage ---
SQUELCH_ACCOUNT_EMAIL=you@gmail.com
SQUELCH_DB_PATH=/var/lib/squelch/squelch.db

# --- Google OAuth client (your GCP "Desktop app" client) ---
SQUELCH_CLIENT_ID=<client id>
SQUELCH_CLIENT_SECRET=<client secret>

# --- bind (loopback; tailscale serve fronts it) ---
SQUELCH_BIND=127.0.0.1:8848

# --- agent door DNS-rebinding allow-list (REQUIRED behind tailscale serve) ---
# The MCP door only accepts loopback Host headers by default, so requests
# proxied by `tailscale serve` (Host: <box>.<tailnet>.ts.net) get 403. List your
# tailnet hostname here (comma-separated, additive to the loopback defaults).
SQUELCH_MCP_ALLOWED_HOSTS=<box>.<tailnet>.ts.net
```

```sh
sudo chown root:squelch /etc/squelch/env
sudo chmod 0640 /etc/squelch/env
```

A `config.toml` is optional — every value above can come from the env file. If
you prefer a file, drop it at `/var/lib/squelch/config.toml` and pass
`--config` (the unit uses env only by default).

### Environment variables

Every binary (`squelchd`, `squelch-mcp`, `squelch-api`, `squelch-tui`) reads the
**canonical** names below. Two legacy names are still accepted as silent
fallbacks and log a one-line deprecation note to stderr — migrate off them.

| Variable | Required | Default | Notes |
|---|---|---|---|
| `SQUELCH_API_TOKEN` | yes (human door) | — | Bearer for every `/client/*` route; door refuses to serve without it. |
| `SQUELCH_ACCOUNT_EMAIL` | yes | `me@localhost` | Canonical. Legacy alias: `SQUELCH_ACCOUNT`. |
| `SQUELCH_DB_PATH` | no | `~/.local/share/squelch/squelch.db` | Canonical, identical default across all binaries. Legacy alias: `SQUELCH_DB`. |
| `SQUELCH_MCP_ALLOWED_HOSTS` | yes behind a proxy | loopback only | Comma-separated extra Host values for the agent door's DNS-rebinding guard, additive to `localhost,127.0.0.1,::1`. Set to your `*.ts.net` name or `/mcp` returns 403. |
| `SQUELCH_BIND` | no | `127.0.0.1:8848` | `squelchd serve` bind address (both doors). |
| `SQUELCH_API_HTTP` | no | `127.0.0.1:8849` | Standalone `squelch-api` dev bin bind address. |
| `SQUELCH_MCP_HTTP` | no | — | Standalone `squelch-mcp` bin: set to switch from stdio to HTTP (address or empty for the loopback default). |
| `SQUELCH_CRED_BACKEND` | no | `keyring` (macOS) / `file` (Linux) | `keyring` or `file`. |
| `SQUELCH_CREDENTIALS_PATH` | no | `~/.config/squelch/credentials.json` | Used only by the `file` backend. |
| `SQUELCH_CLIENT_ID` / `SQUELCH_CLIENT_SECRET` | yes (OAuth) | — | Your GCP "Desktop app" OAuth client. |

The DNS-rebinding allow-list is read once when the agent door is constructed, so
both `squelchd serve` and the standalone `squelch-mcp --http` honor it identically.

---

## 4. Headless OAuth (read AND write credentials)

`squelchd auth` binds a FIXED loopback port and prints a consent URL; you forward
that port from your laptop over SSH and complete consent in your local browser.
The daemon needs BOTH credentials, stored in SEPARATE slots:

- **READ** (`gmail.readonly`) — used by the sync loop. Plain `auth`.
- **WRITE** (`gmail.modify` + `gmail.send`) — used ONLY by human-door actions.
  `auth --write`, stored in a distinct slot; sync/triage never touch it.

Run each as the `squelch` user so tokens land in the service's credentials file.

### READ credential

```sh
# On your laptop: forward the fixed port to the box.
ssh -L 8847:127.0.0.1:8847 box

# On the box (inside that SSH session), as the service user:
sudo -u squelch --preserve-env=SQUELCH_CRED_BACKEND,SQUELCH_CREDENTIALS_PATH,SQUELCH_ACCOUNT_EMAIL,SQUELCH_CLIENT_ID,SQUELCH_CLIENT_SECRET \
  env $(sudo cat /etc/squelch/env | grep -v '^#' | xargs) \
  squelchd auth --headless --port 8847
```

It prints a `https://accounts.google.com/...` URL. Open it in your LAPTOP
browser; Google redirects to `http://127.0.0.1:8847/...`, which tunnels back to
the box and completes the flow. You should see "Stored Read credentials ...".

### WRITE credential

Repeat with `--write` (use a different local port if the first session is still
up, e.g. `--port 8847` again in a fresh SSH session):

```sh
ssh -L 8847:127.0.0.1:8847 box
sudo -u squelch env $(sudo cat /etc/squelch/env | grep -v '^#' | xargs) \
  squelchd auth --write --headless --port 8847
```

Both credentials now live in `/var/lib/squelch/credentials.json` in separate
slots (`` and `#write`).

---

## 5. systemd

```sh
sudo cp deploy/squelchd.service /etc/systemd/system/squelchd.service
sudo systemctl daemon-reload
sudo systemctl enable --now squelchd
systemctl status squelchd
journalctl -u squelchd -f        # startup line only; NO tokens/bodies are logged
```

Expected log line:

```
squelchd: serving agent door http://127.0.0.1:8848/mcp and human door http://127.0.0.1:8848/client/*
```

---

## 6. Expose over the tailnet

`squelchd` binds loopback only. Publish it to your tailnet with:

```sh
sudo tailscale serve --bg 8848
tailscale serve status
```

Now both doors are reachable at `https://<box>.<tailnet>.ts.net/mcp` and
`.../client/*` — and ONLY from your tailnet. Do not open port 8848 in any
cloud/host firewall.

---

## 7. Smoke test

From a tailnet machine (or on the box against loopback). Replace `$BOX`/`$TOKEN`.

**Human door** (`/client/stats`, bearer-gated):

```sh
curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8848/client/stats
# => {"tier_counts":{...},"total":N,"sealed":N,"last_history_id":...}

# Without the token you must be rejected (401), never 200:
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8848/client/stats
# => 401
```

**Agent door** (`/mcp`, MCP `initialize`):

```sh
curl -sS http://127.0.0.1:8848/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18",
        "capabilities":{},
        "clientInfo":{"name":"smoke","version":"0"}}}'
# => a JSON-RPC result announcing serverInfo + capabilities.
```

If both respond, the box is live. Ctrl-C / `systemctl stop squelchd` shuts down
gracefully: it stops accepting connections, tears down MCP sessions, and lets the
sync loop finish in-flight work and flush before exit.
