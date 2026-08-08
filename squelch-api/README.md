# squelch-api

The **human door**: the bearer-authenticated `/client/*` axum router used by squelch's own clients (the desktop app, scripts you trust). This is the richer of the two doors and the only surface with write actions. Primarily a library — `squelchd serve` mounts the router in the unified daemon; the `squelch-api` binary here is a thin dev harness for running the door standalone.

## Surface

Reads: updates (banded standing/new/open), threads, hybrid search, stats, the triage usage ledger (`/client/usage`: per-day token/cost history for both LLM triage stages, with each stage's model id), the triage budget config (`/client/triage-config`: the effective per-stage daily caps, where each came from, and trailing-14d spend averages), shipments, receipts, sender rules, the audit log, and sealed-mail metadata with an explicit audited reveal.

Writes (the only mutation surface in the system): status lifecycle, archive, label, and send. Every action requires an explicit `confirm` flag, uses the separate write-slot Gmail credential (minted via `squelchd auth --write`; sync can only ever load the read slot), and lands in the audit log. Sends pass through an outbound secret guard: a matched body yields a 422 listing redacted match kinds, and only an explicit `override_guard` resend goes out.

The triage budget caps are also tunable here at runtime: `POST /client/triage-config` persists overrides for the Stage-1 global cap and the three Stage-2 caps (thread/sender/global), which the sync engine re-reads at the start of each triage pass — no restart, and no Gmail credential involved.

## Auth

Two kinds of bearer, checked in this order:

1. **`SQUELCH_API_TOKEN`**, the self-host master token, compared in constant time. First-class forever: it works with an empty token table and is the way back in after revoking the last device. Unset or blank means "not configured", which is a supported posture, not a refusal to serve.
2. **Per-device tokens** (`sqd_…`), issued by `squelchd token issue` or by a pairing claim. Stored only as a SHA-256 hash, verified with a point lookup on every request (no cache, so `squelchd token revoke <id>` bites on the very next request), and listed with `squelchd token list`.

A daemon with neither still serves; it 401s everything until a token exists. `POST /client/pair` is the ONE `/client/*` route outside the bearer layer — a device claiming a pairing code has no credential yet, by definition. Every pairing failure is an identical empty 401, and that route carries no CORS layer, so a web page cannot claim codes cross-origin.

CORS on the rest of `/client/*` is configured for webview / vite dev origins, but the token is the boundary — CORS is a courtesy.

## Dev binary

```sh
SQUELCH_API_TOKEN=... cargo run --bin squelch-api
```

| Variable | Meaning | Default |
|---|---|---|
| `SQUELCH_API_TOKEN` | master bearer token; optional, device tokens work without it | — |
| `SQUELCH_API_HTTP` | bind address | `127.0.0.1:8849` |
| `SQUELCH_DB_PATH` | SQLite path | shared XDG default |
| `SQUELCH_ACCOUNT_EMAIL` | account email | `me@localhost` |

Loopback only by default; front with a reverse proxy to expose it. In production use `squelchd serve` instead, which hosts this router and the agent door on one port.
