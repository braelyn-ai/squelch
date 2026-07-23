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
    -- Raw `List-Unsubscribe` header value (comma-separated <mailto:…>/<https:…>
    -- entries), NULL when the mail carried no such header. Captured at ingest;
    -- consumed ONLY by the human door's unsubscribe endpoint (never /mcp).
    list_unsubscribe TEXT,
    -- 1 when the mail advertised RFC 8058 one-click unsubscribe
    -- (`List-Unsubscribe-Post: List-Unsubscribe=One-Click`).
    list_unsub_one_click INTEGER NOT NULL DEFAULT 0,
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
    -- Per-property triage justifications, a JSON object
    -- {importance?,deadline?,tier?} of short human-readable reasons for each
    -- property's value. NULL for rows written before this feature. HUMAN-DOOR
    -- ONLY: served flattened into GET /client/updates; NEVER crosses /mcp (the
    -- agent-door read path leaves it unset).
    field_reasons   TEXT,
    deadline        TEXT,
    matched_rule_id INTEGER,
    -- STAGE-1 LLM marker. NULL = this row still needs the Stage-1 LLM refine
    -- pass (its heuristic seed values are provisional). Stamped with the Stage-1
    -- model id once refined, 'rule' for a row an explicit/Filtered sender rule
    -- already decided (no Stage-1 model spend), or 'heuristic-only' when the
    -- Stage-1 pass fell back to the seed (API down / refusal / permanent error).
    stage1_model_used TEXT,
    -- Set to 1 by the Stage-1 pass (confident=false), or at ingest for a Filtered
    -- rule needing want_text evaluation, to mark the row for Stage-2 escalation.
    needs_stage2    INTEGER NOT NULL DEFAULT 0,
    -- STAGE-2 LLM marker. NULL = not yet Stage-2 processed. The Stage-2 queue
    -- predicate is `stage1_model_used IS NOT NULL AND needs_stage2=1 AND
    -- model_used IS NULL AND sensitivity='normal'`.
    model_used      TEXT,
    -- CATEGORIZE-THEN-EXTRACT. The stage-1 (and, on escalation, stage-2) LLM
    -- assigns one coarse category: 'general' | 'invoice' | 'banking_statement' |
    -- 'transaction_alert'. NULL = a pre-feature or heuristic-only row (the LLM
    -- never looked). A category with a registered SPECIALIST EXTRACTOR (see
    -- triage/extract/) queues the row for a second, structured extraction pass.
    -- Sealed rows never run the LLM stages, so their category stays NULL — which
    -- is exactly what structurally excludes them from every extractor queue.
    category        TEXT,
    -- SPECIALIST-EXTRACTOR marker. NULL = this row has not yet been through its
    -- category's extractor (or its category has no extractor). Stamped with the
    -- extractor model id once run, or a sentinel ('skip-*') when deliberately
    -- skipped. The extractor queue predicate is `category IN (<extractable>) AND
    -- extractor_model_used IS NULL AND sensitivity='normal'`.
    extractor_model_used TEXT,
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
-- independent of shipment detection. A landing receipt may also AUTO-CLOSE one
-- matching open bill (conservative merchant+amount+recency rules, audited as
-- bill.auto_close — see triage/receipt_match.rs).
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

-- CALENDAR UPDATES. One row per (account, message): an invite / updated
-- invitation / cancellation / RSVP response, extracted from NON-SEALED calendar
-- mail (Google Calendar notification subjects, Outlook invite shapes,
-- ics-bearing mail) by the ingest pipeline. Like receipts, these are RECORDS of
-- scheduling state (the user's real calendar is the source of truth) — they
-- live ONLY in the desktop "Calendar" zone and are auto-resolved
-- (triage.status='done') at ingest so they never surface as New/Attention/Aging
-- clutter. `kind` is invite|update|cancellation|response;
-- `event_title`/`starts_at`/`organizer` are best-effort (NULL is fine — the
-- classification is driven by the structural subject shape, not extraction).
-- The /client/calendar window filters on received_at (mail arrival), NOT
-- starts_at (event time).
--
-- SECURITY (structural, not filtered): SEALED MAIL NEVER PRODUCES A CALENDAR
-- UPDATE. The ingest path runs calendar detection ONLY for sensitivity='normal'
-- mail, so this table has no sealed rows BY CONSTRUCTION — there is nothing to
-- leak and no sealed join is needed on read.
CREATE TABLE IF NOT EXISTS calendar_updates (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    message_id  INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    event_title TEXT,
    starts_at   TEXT,
    organizer   TEXT,
    received_at TEXT NOT NULL,
    UNIQUE(account_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_calendar_received ON calendar_updates(account_id, received_at);

-- BANKING. One row per (account, message): a bank/credit-card STATEMENT (a
-- periodic record) or a TRANSACTION ALERT (a "you spent" / deposit / low-balance
-- notice), extracted by the banking SPECIALIST EXTRACTOR (triage/extract/banking.rs)
-- from a NON-SEALED row the LLM categorized 'banking_statement' or
-- 'transaction_alert'. Like receipts, these are RECORDS, not obligations — the
-- extractor auto-resolves the triage row (status='done') so they leave the
-- attention bands and live only in the desktop "Banking" zone. `kind` is
-- 'statement' | 'transaction_alert'. For a statement, `amount` is the TOTAL
-- statement balance (never the minimum payment / amount due); for an alert it is
-- the transaction amount. `institution` is a clean display name (e.g. "Chase").
-- `account_hint` is ONLY ever a masked last-4 tail ("…1234") — a full account
-- number is never stored (the extractor instructs the model AND post-validates,
-- reducing anything longer to a "…NNNN" tail or NULL). All extracted fields are
-- best-effort (NULL is fine).
--
-- Differentiation from receipts (documented invariant): a receipt (money paid
-- for a purchase) and a banking row must never DOUBLE-CREATE. The deterministic
-- receipt detector runs at INGEST, before the LLM categorizer; if a message
-- already produced a receipt, the banking extractor SKIPS it (the extractor
-- queue excludes messages that have a receipts row).
--
-- SECURITY (structural, not filtered): SEALED MAIL NEVER PRODUCES A BANKING ROW.
-- Sealed rows never run the LLM stages, so they carry category=NULL and are
-- structurally absent from the extractor queue; the extractor additionally
-- enforces the release-mode sealed guard. This table has no sealed rows BY
-- CONSTRUCTION — no sealed join is needed on read.
CREATE TABLE IF NOT EXISTS banking (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL,
    message_id   INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    institution  TEXT,
    amount       REAL,
    currency     TEXT,
    account_hint TEXT,
    received_at  TEXT NOT NULL,
    UNIQUE(account_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_banking_received ON banking(account_id, received_at);

-- UNSUBSCRIBES. One row per (account, sender_addr): the human-door record that
-- the user asked to stop hearing from a sender, plus the "did they honor it?"
-- violation ledger. Created/updated by POST /client/unsubscribe (upsert: a fresh
-- request RESETS violation_count/last_violation_at/resolution — the user
-- re-asked, so the 72h grace clock restarts). `method` is always 'browser' —
-- the client opens the unsubscribe link; the server never delivers anything
-- itself (legacy one_click/mailto values can only exist in pre-revision dev
-- DBs). `source_message_id` is the message the unsubscribe was actioned from
-- (nullable; the message may later be deleted). `resolution` is blocked|
-- dismissed|NULL — NULL means the request is still outstanding and the violation
-- detector is armed.
--
-- VIOLATION SEMANTICS (applied in the ingest transaction, store side): when a
-- NON-SENT inbound message is stored, if an unsubscribes row exists for
-- (account_id, lower(from_addr)) with resolution IS NULL and the message's
-- received_at is more than 72h after requested_at, violation_count is
-- incremented and last_violation_at set to that received_at.
CREATE TABLE IF NOT EXISTS unsubscribes (
    id                INTEGER PRIMARY KEY,
    account_id        INTEGER NOT NULL,
    sender_addr       TEXT NOT NULL,
    requested_at      TEXT NOT NULL,
    method            TEXT NOT NULL,
    source_message_id INTEGER,
    violation_count   INTEGER NOT NULL DEFAULT 0,
    last_violation_at TEXT,
    resolution        TEXT,
    UNIQUE(account_id, sender_addr)
);

CREATE INDEX IF NOT EXISTS idx_unsubscribes_requested ON unsubscribes(account_id, requested_at);

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

-- LLM USAGE LEDGER. One row per (account, UTC day, category): running totals of
-- the Messages API usage each triage stage consumed. `category` is 'stage1',
-- 'stage2', or a specialist-extractor line such as 'extract_banking' — each is a
-- SEPARATE category the client renders independently (extractors run on the
-- stage-1 model but bill to their own line for cost visibility). `calls` counts
-- SUCCESSFUL classify responses that carried a
-- usage block; input/output tokens are summed from each response's usage. Read
-- by GET /client/stats + /client/usage to surface usage + an estimated cost
-- (cost is computed at read time from the config-driven per-MTok prices, so it
-- is NOT stored here). Schema applies fresh on open; dev dbs get reset.
CREATE TABLE IF NOT EXISTS stage2_usage (
    account_id    INTEGER NOT NULL,
    day           TEXT NOT NULL,
    category      TEXT NOT NULL DEFAULT 'stage2',
    calls         INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account_id, day, category)
);

-- RUNTIME APP SETTINGS. A tiny per-account key/value table for operator knobs a
-- client can change at runtime WITHOUT editing config.toml or restarting the
-- daemon. Currently holds the Stage-2 daily-cap overrides
-- (stage2_thread_daily_cap / stage2_sender_daily_cap / stage2_global_daily_cap),
-- set via the human door's POST /client/triage-config. Precedence at read time:
-- an app_settings OVERRIDE wins over the config/env value, which wins over the
-- built-in default. `value` is stored as TEXT (parsed by the reader). Schema
-- applies fresh on open; dev dbs get reset.
CREATE TABLE IF NOT EXISTS app_settings (
    account_id INTEGER NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    UNIQUE(account_id, key)
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
-- This table is never read or written by MCP. Besides the human door, ONE
-- internal writer exists: the ingest receipt->bill auto-close appends a row
-- (actor='ingest', action='bill.auto_close') when a receipt resolves an open
-- bill, so that state change is always explainable to the user. account_id
-- scopes rows to an account like every other owned table.
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
