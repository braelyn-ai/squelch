# squelch-desktop

The desktop client: a keyboard-first Tauri (v2) + React app over the human door (`/client/*`). The Rust shell is deliberately thin — its jobs are the window, the OS keyring for `{server_url, api_token}`, and packaging; everything else is the webview talking REST to a running `squelchd serve`.

Surfaces: the Sitrep dashboard (default — the situation, not the mailbox), the Emails band list (standing / new / open), auth mail ("present, don't read": countdown rings + code modal, bodies revealed only on explicit audited request), sender rules, the audit log, usage (the per-stage triage token/cost ledger from `/client/usage`), and settings (connection, a plain-language explainer of the two-stage triage pipeline, and the per-stage daily budget caps — edited live via `/client/triage-config`, no daemon restart). Actions are undo-first: archive/done fire immediately with a 5s undo window; send is a two-phase ceremony that surfaces the outbound secret guard's verdict before an override.

## Develop

```sh
bun install
bun run tauri dev     # the real shell: window + keyring
bun run dev           # browser-only fallback (vite)
```

In a plain browser there is no Tauri runtime, so settings transparently fall back to localStorage (a one-time console warning notes this) — the full keyboard UI can be exercised without the Rust shell. Point Connect at your daemon, e.g. `http://127.0.0.1:8848` with your `SQUELCH_API_TOKEN`.

```sh
bun run build         # typecheck + vite build
bun test              # unit tests
bun run tauri build   # packaged app
```

## Layout

| Path | What lives there |
|---|---|
| `src/api/` | typed `/client/*` fetch layer, `ApiError` kinds, keyring/localStorage settings bridge |
| `src/state/` | zustand store (bands, selection, undo queue, connection health) + the 10s sitrep poller |
| `src/keys/` | the keymap: contexts, chords, dispatch |
| `src/views/`, `src/components/` | routed surfaces and their pieces |
| `src-tauri/` | the thin Rust shell (keyring commands, window config) |

The token lives in the OS keyring at rest and is only ever sent as a bearer header — never logged, never in error messages, never in URLs.
