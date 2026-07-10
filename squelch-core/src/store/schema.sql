-- squelch local store schema. Multi-tenant shaped: every account-owned row
-- carries account_id. Applied on open (idempotent).

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS accounts (
    id         INTEGER PRIMARY KEY,
    email      TEXT UNIQUE NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    gmail_msg_id TEXT NOT NULL,
    thread_id   TEXT NOT NULL,
    from_addr   TEXT NOT NULL,
    from_name   TEXT,
    subject     TEXT NOT NULL,
    received_at TEXT NOT NULL,
    snippet     TEXT NOT NULL,
    body        TEXT NOT NULL DEFAULT '',
    -- Server-side-sanitized (ammonia) HTML body, NULL for plain-text-only mail.
    -- Populated at ingest; served ONLY through the human door
    -- (GET /client/thread/{id}). Never crosses /mcp: the agent door flattens to
    -- `body` text only. Sealed mail stores this like `body` (storage is fine;
    -- serving is guarded — sealed threads are NotFound on every non-local door).
    body_html   TEXT,
    is_sent     INTEGER NOT NULL DEFAULT 0,
    UNIQUE(account_id, gmail_msg_id)
);

CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(account_id, thread_id);
CREATE INDEX IF NOT EXISTS idx_messages_received ON messages(account_id, received_at);

CREATE TABLE IF NOT EXISTS contacts (
    account_id INTEGER NOT NULL,
    addr       TEXT NOT NULL,
    sent_count INTEGER NOT NULL DEFAULT 0,
    first_seen TEXT NOT NULL,
    PRIMARY KEY(account_id, addr)
);

CREATE TABLE IF NOT EXISTS sender_rules (
    id            INTEGER PRIMARY KEY,
    account_id    INTEGER NOT NULL,
    match_pattern TEXT NOT NULL,
    want_text     TEXT NOT NULL,
    disposition   TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    UNIQUE(account_id, match_pattern)
);

-- ATTENTION LIFECYCLE (sitrep seen-ledger):
--   status       'new' | 'open' | 'done'. A row starts 'new'; the first time it
--                flows OUT through a read door (MCP get_inbox_updates OR
--                GET /client/updates) it is promoted 'new' -> 'open' and stamped
--                surfaced_at. A successful archive/send, or an explicit dismiss,
--                sets status='done' + resolved_at.
--   surfaced_at  first time ANY door surfaced this row (NULL until then). The
--                seen-ledger: answers "did anyone (agent or human) see this yet".
--   resolved_at  when the row reached status='done'.
-- Sealed rows carry these columns like any other row, but stay structurally
-- absent from every non-local surface, so they never get surfaced/stamped.
CREATE TABLE IF NOT EXISTS triage (
    message_id      INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL,
    importance      INTEGER NOT NULL DEFAULT 0,
    tier            TEXT NOT NULL DEFAULT 'noise',
    sensitivity     TEXT NOT NULL DEFAULT 'normal',
    sealed_kind     TEXT,
    one_line        TEXT NOT NULL DEFAULT '',
    reason          TEXT NOT NULL DEFAULT '',
    deadline        TEXT,
    matched_rule_id INTEGER,
    model_used      TEXT,
    status          TEXT NOT NULL DEFAULT 'new',
    surfaced_at     TEXT,
    resolved_at     TEXT,
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_triage_sensitivity ON triage(account_id, sensitivity);
CREATE INDEX IF NOT EXISTS idx_triage_status ON triage(account_id, status);

CREATE TABLE IF NOT EXISTS deadlines (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    amount     REAL,
    currency   TEXT,
    due_at     TEXT NOT NULL,
    past_due   INTEGER NOT NULL DEFAULT 0,
    source     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_deadlines_due ON deadlines(account_id, due_at);

-- PACKAGE TRACKING. One row per (account, tracking_number): a shipment currently
-- (or formerly) in transit, extracted from NON-SEALED shipping/delivery mail by
-- the ingest pipeline. A new email about an existing tracking number UPDATES the
-- row (status state machine + last_update/last_message_id); a tracking number is
-- REQUIRED to create a row (it is the dedupe/track key — mail with no number is
-- skipped). `status` follows the ordered<shipped<out_for_delivery<delivered
-- ladder (exception is a flag); the ingest upsert never regresses a delivered
-- shipment.
--
-- SECURITY (structural, not filtered): SEALED MAIL NEVER PRODUCES A SHIPMENT. The
-- ingest path runs shipment detection ONLY for sensitivity='normal' mail, so this
-- table has no sealed rows BY CONSTRUCTION — there is nothing to leak and no
-- sealed join is needed on read.
CREATE TABLE IF NOT EXISTS shipments (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL,
    tracking_number TEXT NOT NULL,
    carrier         TEXT NOT NULL,
    item_name       TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'shipped',
    tracking_url    TEXT,
    last_message_id INTEGER,
    first_seen      TEXT NOT NULL,
    last_update     TEXT NOT NULL,
    UNIQUE(account_id, tracking_number)
);

CREATE INDEX IF NOT EXISTS idx_shipments_status ON shipments(account_id, status);

-- RECEIPTS. One row per (account, message): a record of money ALREADY PAID,
-- extracted from NON-SEALED past-transaction mail (payment received / receipt /
-- order confirmation / you were charged) by the ingest pipeline. Receipts are
-- records, not obligations and not signals — they live ONLY in the desktop
-- "Receipts" category and are auto-resolved (triage.status='done') at ingest so
-- they never surface as New/Attention/Aging clutter. `from_addr`/`from_name` are
-- stored so the client can render a clean merchant name with no extra join.
-- `amount`/`currency` are best-effort — a receipt with no parseable total is
-- still a receipt (amount NULL). A receipt and a shipment can COEXIST (an order
-- confirmation with a total AND tracking is both); receipt detection is
-- independent of shipment detection.
--
-- SECURITY (structural, not filtered): SEALED MAIL NEVER PRODUCES A RECEIPT. The
-- ingest path runs receipt detection ONLY for sensitivity='normal' mail, so this
-- table has no sealed rows BY CONSTRUCTION — there is nothing to leak and no
-- sealed join is needed on read.
CREATE TABLE IF NOT EXISTS receipts (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    message_id  INTEGER NOT NULL,
    from_addr   TEXT NOT NULL,
    from_name   TEXT,
    amount      REAL,
    currency    TEXT,
    received_at TEXT NOT NULL,
    UNIQUE(account_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_receipts_received ON receipts(account_id, received_at);

-- Gmail sync cursor, keyed by a logical mailbox string. The Gmail REST engine
-- stores exactly one row keyed mailbox='history': uidvalidity is unused (0) and
-- last_uid holds the account's historyId (a u64 that fits in SQLite's i64
-- INTEGER). Column names are retained from the IMAP era to avoid a migration
-- (the schema applies fresh on open; dev dbs must be deleted to pick up shape
-- changes).
CREATE TABLE IF NOT EXISTS sync_state (
    account_id  INTEGER NOT NULL,
    mailbox     TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL,
    last_uid    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account_id, mailbox)
);

-- STAGE-2 BUDGET (circuit breaker). model_calls counts Anthropic API attempts
-- (incremented BEFORE the call, so retry storms can't exceed the cap). Two
-- accounting scopes share this one table, keyed by thread_id:
--   * per-thread-per-day: thread_id = the message's real thread id.
--   * global-per-account-per-day: thread_id = the sentinel '__global__' (no
--     real Gmail thread can collide — Gmail thread ids are hex, never that
--     literal). This avoids a schema addition; both caps are checked before
--     each call. (Schema applies fresh; dev dbs get reset.)
CREATE TABLE IF NOT EXISTS wake_budget (
    account_id  INTEGER NOT NULL,
    thread_id   TEXT NOT NULL,
    day         TEXT NOT NULL,
    model_calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account_id, thread_id, day)
);

-- STAGE-2 USAGE LEDGER. One row per (account, UTC day): running totals of the
-- Anthropic Messages API usage the Stage-2 pass consumed. `calls` counts
-- SUCCESSFUL classify responses that carried a usage block; input/output tokens
-- are summed from each response's usage. Read by GET /client/stats to surface
-- today's usage + an estimated cost (cost is computed at read time from the
-- config-driven per-MTok prices, so it is NOT stored here). Schema applies fresh
-- on open; dev dbs get reset.
CREATE TABLE IF NOT EXISTS stage2_usage (
    account_id    INTEGER NOT NULL,
    day           TEXT NOT NULL,
    calls         INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account_id, day)
);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(subject, body);

-- ON-BOX SEMANTIC RECALL (v1). A sqlite-vec `vec0` virtual table holding one
-- embedding per NON-SEALED message. The rowid is the local `messages.id`, so a
-- KNN hit joins straight back to `messages`. `account_id` is a vec0 METADATA
-- column so KNN queries can constrain by account in the WHERE clause (multi-
-- tenant scoping, same as every other owned table).
--
-- SECURITY (structural exclusion, not filtering): SEALED MESSAGES ARE NEVER
-- INSERTED HERE. The embed-at-write path is gated on `sensitivity='normal'`, so
-- sealed content is absent from the vector space entirely — there is nothing to
-- leak into semantic search. `semantic_search` additionally re-joins `triage`
-- and re-excludes sealed rows (belt-and-suspenders).
--
-- DIMENSION: float[384] matches the default BGE-small-en-v1.5 model. The store
-- asserts the configured embedder's dims == 384 at open time. Changing the model
-- dimension requires editing this literal AND resetting the dev db (schema
-- applies fresh — no migrations). The vec0 extension is statically linked and
-- registered via sqlite3_auto_extension before the connection opens.
CREATE VIRTUAL TABLE IF NOT EXISTS message_vecs USING vec0(
    message_id INTEGER PRIMARY KEY,
    embedding  FLOAT[384],
    account_id INTEGER
);

-- Audit log for the HUMAN DOOR (squelch-api /client/*). Every sealed-body
-- reveal (and, later, every write action) appends a row here BEFORE returning.
-- This table is human-door-only; it is never read or written by MCP, sync, or
-- triage. account_id scopes rows to an account like every other owned table.
CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    ts         TEXT NOT NULL,
    actor      TEXT NOT NULL,
    action     TEXT NOT NULL,
    target     TEXT,
    detail     TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_account_ts ON audit_log(account_id, ts);
