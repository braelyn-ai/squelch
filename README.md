# squelch

A local-first email intelligence service. It reads your Gmail (read-only), decides what actually deserves attention, catches bills and deadlines, and exposes that intelligence over MCP so an agent you already run can surface it to you. Your agent never holds your Gmail credential and never gets raw access to your mailbox.

The name comes from the radio control that mutes everything below a signal threshold. Same idea here: noise stays below the squelch line, signal comes through.

## How it works

```
Gmail (REST API, gmail.readonly OAuth)
        |  polling via history.list
        v
squelchd ── sync ──> SQLite ──> triage (rules first, LLM later)
        |
        ├── /mcp      agent door: 6 read tools, no writes,
        |             auth emails (2FA, resets) structurally absent
        └── /client   human door: bearer-authed rich API for your
                      own clients, holds the only write actions
```

- **Sync**: polls Gmail every 45s with a read-only token. Sent mail seeds a "people I know" contact list, which is the strongest cheap triage signal.
- **Triage**: a rules ladder decides most mail with no model call (bills, known contacts, alerts, newsletters, cold sales). The ambiguous middle is queued for a budgeted LLM pass.
- **Two doors**: agents connect to `/mcp` and get ranked summaries. They cannot send, archive, delete, or see auth-related mail. Your own clients connect to `/client` with a bearer token and get search, threads, sender rules, the sitrep lifecycle, and gated actions (archive, label, send) backed by a separate write-scoped token that only the action handlers can load.

## Getting started

### 1. Create a Google Cloud OAuth client

1. Go to [console.cloud.google.com](https://console.cloud.google.com), create a project
2. Enable the **Gmail API** for the project
3. Configure the OAuth consent screen (External), add yourself as a test user
4. Create credentials: OAuth client ID, type **Desktop app**

### 2. Configure

Create a `.env` in the repo root (it is gitignored):

```sh
SQUELCH_CLIENT_ID=<your client id>
SQUELCH_CLIENT_SECRET=<your client secret>
SQUELCH_ACCOUNT_EMAIL=you@gmail.com
SQUELCH_API_TOKEN=$(openssl rand -hex 32)   # for the human door
```

Optional: `SQUELCH_DB_PATH` (default `~/.local/share/squelch/squelch.db`), `SQUELCH_BIND` (default `127.0.0.1:8848`), `SQUELCH_POLL_SECS` (default 45), `SQUELCH_MCP_ALLOWED_HOSTS` if you front the server with a proxy like `tailscale serve`.

### 3. Authorize and run

```sh
set -a; source .env; set +a

cargo run --bin squelchd -- auth     # one-time browser consent, token lands in the OS keyring
cargo run --bin squelchd -- serve    # sync + both doors on one port
```

On a headless box use `squelchd auth --headless` and forward the port: `ssh -L 8847:127.0.0.1:8847 yourbox`. Grant write scopes later with `squelchd auth --write` (only needed for archive/send actions).

### 4. Connect an agent

Point any MCP client at the streamable HTTP endpoint:

```json
{
  "mcpServers": {
    "squelch": { "type": "http", "url": "http://127.0.0.1:8848/mcp" }
  }
}
```

To reach it from another machine on a tailnet: `tailscale serve --bg 8848`, set `SQUELCH_MCP_ALLOWED_HOSTS=<your-host>.ts.net`, and use the `https://<your-host>.ts.net/mcp` URL.

### 5. Browse locally

```sh
cargo run --bin squelch-tui    # ranked digest, squelch line, sender rule tuning
```

## Workspace layout

| Crate | What it is |
|---|---|
| `squelch-core` | types, SQLite store, triage rules, seal detection, Gmail sync, OAuth |
| `squelch-mcp` | the agent door (rmcp server, stdio or HTTP) |
| `squelch-api` | the human door (axum, bearer auth, actions, audit log) |
| `squelchd` | the daemon binary: `auth`, `run`, `serve` |
| `squelch-tui` | local ratatui viewer for setup and debugging |

Deployment notes for a Linux server live in [`deploy/DEPLOY.md`](deploy/DEPLOY.md). The desktop client design lives in [`docs/UX-DIRECTIONS.md`](docs/UX-DIRECTIONS.md).

## Security posture

- The sync credential is scoped `gmail.readonly`. The write credential (`gmail.modify` + `gmail.send`) lives in a separate slot and is only reachable from the human door's action handlers, which require an explicit confirm flag, run an outbound secret scan on sends, and audit every attempt.
- Auth emails (2FA codes, password resets, login alerts) are sealed at ingest and never appear in any MCP response, any LLM call, or any list endpoint. Revealing one takes an explicit authenticated request and writes an audit row.
- Email content is treated as untrusted input everywhere. Tokens never appear in logs.
