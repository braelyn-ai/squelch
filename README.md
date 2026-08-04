# squelch

A local-first email intelligence service. It reads your Gmail (read-only), decides what actually deserves attention, catches bills and deadlines, and exposes that intelligence over MCP so an agent you already run can surface it to you. Your agent never holds your Gmail credential and never gets raw access to your mailbox.

The name comes from the radio control that mutes everything below a signal threshold. Same idea here: noise stays below the squelch line, signal comes through.

## How it works

```
Gmail (REST API, gmail.readonly OAuth)
        |  polling via history.list
        v
squelchd ── sync ──> SQLite ──> triage (seal -> rules -> 2-stage LLM)
        |
        ├── /mcp      agent door: 7 read tools, no writes,
        |             auth emails (2FA, resets) structurally absent
        └── /client   human door: bearer-authed rich API for your
                      own clients, holds the only write actions
```

- **Sync**: polls Gmail every 45s with a read-only token. Sent mail seeds a "people I know" contact list, which is the strongest cheap triage signal.
- **Triage**: deterministic first, models second. Seal detection runs before anything else — auth mail is sealed at ingest and never reaches any LLM. Your sender rules also decide deterministically: a squelch/surface rule settles that sender with zero model spend. Everything else is stored with heuristic seed values (the rules ladder: bills, known contacts, alerts, newsletters, cold sales), then refined within the sync cycle by a small Stage-1 model (default `claude-haiku-4-5`) that scores every one of those emails: importance, tier, deadline, one-liner, plus a per-property "why". Rows the small model isn't confident about — and filtered-rule mail whose natural-language `want_text` needs actual judgment — escalate to a more capable Stage-2 model (default `claude-sonnet-5`). A filtered rule's `want_text` is your own standing instruction for that sender, passed to Stage-2 verbatim and read in whichever polarity you wrote it: "only tell me about school closures" and "i don't care about the emails saying the new version is approved" both work. Each stage has its own daily budget caps (one global cap for Stage-1, which sees nearly everything; per-thread, per-sender, and global caps for Stage-2) and its own token/cost ledger; no API key or an exhausted budget just means rows keep their heuristic values.
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

To turn on LLM triage, provide an API key: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or the explicit `SQUELCH_STAGE2_API_KEY` (provider sniffed from the key prefix). Both stages share the one key/provider; without a key, triage runs heuristic-only. Models, prices, and budgets are tunable under `[stage1]` / `[stage2]` in `~/.config/squelch/config.toml` or via `SQUELCH_STAGE1_*` / `SQUELCH_STAGE2_*` env vars, and the daily caps can be overridden at runtime (no restart) from the desktop app's Settings.

### 3. Authorize and run

```sh
set -a; source .env; set +a

cargo run --bin squelchd -- auth     # one-time browser consent, token lands in the OS keyring
cargo run --bin squelchd -- serve    # sync + both doors on one port
```

On a headless box use `squelchd auth --headless` and forward the port: `ssh -L 8847:127.0.0.1:8847 yourbox`. Grant write scopes later with `squelchd auth --write` (only needed for archive/send actions) — that runs two consent flows, minting the write credential and re-minting the read one, so the two slots stay in sync.

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

| Component | What it is |
|---|---|
| `squelch-core` | types, SQLite store, seal detection, two-stage triage (rules + LLM), Gmail sync, OAuth |
| [`squelch-mcp`](squelch-mcp/README.md) | the agent door (rmcp server, stdio or HTTP) |
| [`squelch-api`](squelch-api/README.md) | the human door (axum, bearer auth, actions, audit log) |
| [`squelchd`](squelchd/README.md) | the daemon binary: `auth`, `run`, `serve` |
| [`squelch-tui`](squelch-tui/README.md) | local ratatui viewer for setup and debugging |
| [`squelch-desktop`](squelch-desktop/README.md) | the Tauri desktop client over the human door |
| `squelch-client-swift` | the native macOS client over the human door |
| [`squelch-relay`](squelch-relay/README.md) | blind APNs ping relay for the future iOS app |

Deployment notes for a Linux server live in [`deploy/DEPLOY.md`](deploy/DEPLOY.md). The desktop client design lives in [`docs/UX-DIRECTIONS.md`](docs/UX-DIRECTIONS.md).

## Building the macOS client

`squelch-client-swift` builds with `swiftc` directly — no Xcode needed for a local build.

```sh
cd squelch-client-swift
./build.sh          # debug
./build.sh run      # build and launch
./build.sh release  # optimized
```

Local builds sign with whatever `Developer ID Application` certificate is in the keychain, falling back to ad-hoc when there is none. That is deliberate and worth knowing about: keychain ACLs match on a bundle's *designated requirement*, and an ad-hoc signature's requirement is a hash of the build itself, so every recompile looks like a new app and re-prompts for access to the stored credentials. A Developer ID requirement is keyed to the team and stays constant across rebuilds. Local builds also get `get-task-allow` so a debugger can attach; it is injected into a throwaway copy of the entitlements and never reaches a release, which the notary service would reject for carrying it.

`VERSION` holds the user-facing version; the build number is the git commit count, so it only ever increases. Override either with `MARKETING_VERSION=` / `BUILD_NUMBER=`.

### Releases

`./build-release.sh` produces a bundle that opens by double-click on any Mac: Developer ID signature, hardened runtime, Apple notarization, stapled ticket. Ad-hoc builds from `build.sh` do not — Gatekeeper blocks them everywhere but the machine that built them.

```sh
./build-release.sh              # sign, notarize, staple, package
./build-release.sh --no-notary  # sign only; fast, still Gatekeeper-warned
```

It picks up whatever `Developer ID Application` certificate is in the keychain; set `SIGN_ID` to choose explicitly. Notarization needs an **app-specific password** (generated at [appleid.apple.com](https://appleid.apple.com) under Sign-In and Security — *not* your Apple ID password), stored once:

```sh
xcrun notarytool store-credentials squelch-notary \
  --apple-id <your apple id> --team-id <your team id>
```

Override the profile name with `NOTARY_PROFILE=`. Apple's turnaround is typically 2–15 minutes; on rejection the script fetches and prints the reason.

## Security posture

- The sync credential is scoped `gmail.readonly`. The write credential (`gmail.modify` + `gmail.send`) lives in a separate slot and is only reachable from the human door's action handlers, which require an explicit confirm flag, run an outbound secret scan on sends, and audit every attempt.
- Auth emails (2FA codes, password resets, login alerts) are sealed at ingest and never appear in any MCP response, any LLM call, or any list endpoint. Revealing one takes an explicit authenticated request and writes an audit row.
- Email content is treated as untrusted input everywhere. Tokens never appear in logs.
