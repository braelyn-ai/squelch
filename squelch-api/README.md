# squelch-api

The **human door**: the bearer-authenticated `/client/*` axum router used by squelch's own clients (the desktop app, scripts you trust). This is the richer of the two doors and the only surface with write actions. Primarily a library — `squelchd serve` mounts the router in the unified daemon; the `squelch-api` binary here is a thin dev harness for running the door standalone.

## Surface

Reads: updates (banded standing/new/open), threads, hybrid search, stats, the triage usage ledger (`/client/usage`: per-day token/cost history for both LLM triage stages, with each stage's model id), the triage budget config (`/client/triage-config`: the effective per-stage daily caps, where each came from, and trailing-14d spend averages), shipments, receipts, sender rules, the audit log, and sealed-mail metadata with an explicit audited reveal.

Writes (the only mutation surface in the system): status lifecycle, archive, label, and send. Every action requires an explicit `confirm` flag, uses the separate write-slot Gmail credential (minted via `squelchd auth --write`; sync can only ever load the read slot), and lands in the audit log. Sends pass through an outbound secret guard: a matched body yields a 422 listing redacted match kinds, and only an explicit `override_guard` resend goes out.

The triage budget caps are also tunable here at runtime: `POST /client/triage-config` persists overrides for the Stage-1 global cap and the three Stage-2 caps (thread/sender/global), which the sync engine re-reads at the start of each triage pass — no restart, and no Gmail credential involved.

Auth is a single bearer token compared in constant time. CORS is configured for the desktop webview / vite dev origins, but the token is the boundary — CORS is a courtesy.

## Dev binary

```sh
SQUELCH_API_TOKEN=... cargo run --bin squelch-api
```

| Variable | Meaning | Default |
|---|---|---|
| `SQUELCH_API_TOKEN` | bearer token, required — refuses to start without it | — |
| `SQUELCH_API_HTTP` | bind address | `127.0.0.1:8849` |
| `SQUELCH_DB_PATH` | SQLite path | shared XDG default |
| `SQUELCH_ACCOUNT_EMAIL` | account email | `me@localhost` |

Loopback only by default; front with a reverse proxy to expose it. In production use `squelchd serve` instead, which hosts this router and the agent door on one port.
