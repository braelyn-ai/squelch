# squelch-mcp

The **agent door**: an MCP server exposing squelch's read-only intelligence to any MCP client (Claude Code, Claude Desktop, whatever you run). Agents get ranked summaries — they never hold the Gmail credential, never see raw mailbox access, and structurally cannot see auth mail.

## Tools

Seven tools, read-mostly by design:

- `get_inbox_updates` — ranked updates since a timestamp
- `get_thread` — a sanitized thread by thread id or message id
- `get_deadlines` — bills and deadlines due within N days
- `get_shipments` — tracked packages (en-route by default)
- `search_mail` — hybrid keyword + semantic search
- `set_sender_rule` / `list_sender_rules` — the one write: LOCAL sender disposition (surface / squelch / filtered). It touches squelch's own database only, never Gmail, and writes an audit row.

Sealed messages (2FA codes, password resets, login alerts) are absent from every response — not redacted, absent, indistinguishable from not existing.

## Transports

Transport is chosen in `main` and only there; tool logic is transport-agnostic.

```sh
squelch-mcp                    # stdio (default)
squelch-mcp --http [addr]      # Streamable HTTP, mounted at /mcp
```

`SQUELCH_MCP_HTTP` selects HTTP mode via env. Default HTTP bind is loopback `127.0.0.1:8848`; front it with a reverse proxy to expose it. In normal deployments you don't run this binary at all — `squelchd serve` mounts the same service at `/mcp` alongside the human door.

Point an MCP client at it:

```json
{
  "mcpServers": {
    "squelch": { "type": "http", "url": "http://127.0.0.1:8848/mcp" }
  }
}
```

## Env

- `SQUELCH_DB_PATH` — SQLite path (shared XDG default)
- `SQUELCH_ACCOUNT_EMAIL` — account to serve
- `SQUELCH_MCP_ALLOWED_HOSTS` — allowed Host headers behind a proxy
