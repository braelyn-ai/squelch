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
    -- Sanitized HTML body baked at ingest (docs/SECURITY.md §1), NULL for
    -- plain-text-only mail. Served ONLY through the human door; never crosses
    -- /mcp, which flattens to `body` text. Sealed mail stores it like `body` —
    -- storage is fine, serving is guarded.
    body_html   TEXT,
    is_sent     INTEGER NOT NULL DEFAULT 0,
    -- Raw `List-Unsubscribe` header value, NULL when absent. Consumed ONLY by
    -- the human door's unsubscribe endpoint.
    list_unsubscribe TEXT,
    -- 1 when the mail advertised RFC 8058 one-click unsubscribe
    -- (`List-Unsubscribe-Post: List-Unsubscribe=One-Click`).
    list_unsub_one_click INTEGER NOT NULL DEFAULT 0,
    -- EMAIL-AUTHENTICATION VERDICT, read at ingest from the TOPMOST
    -- `Authentication-Results` header (the one Gmail itself wrote): 1 = DMARC
    -- passed or an aligned DKIM signature did, 0 = Gmail evaluated it and
    -- neither held, NULL = never evaluated (no Gmail header, or the row was
    -- ingested before this column existed).
    --
    -- NULL MUST READ AS "NO". Its only consumer is the human door's PERMISSIVE
    -- known-sender tracking-pixel bypass, so absence of proof withholds the
    -- bypass rather than granting it. Nothing is backfilled: any re-sync refills
    -- the column through the message upsert.
    auth_pass   INTEGER,
    UNIQUE(account_id, gmail_msg_id)
);

CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(account_id, thread_id);
CREATE INDEX IF NOT EXISTS idx_messages_received ON messages(account_id, received_at);

CREATE TABLE IF NOT EXISTS contacts (
    account_id INTEGER NOT NULL,
    addr       TEXT NOT NULL,
    sent_count INTEGER NOT NULL DEFAULT 0,
    first_seen TEXT NOT NULL,
    -- Recency + pretty-name columns for recipient autocomplete. last_sent_at is
    -- RFC3339 UTC (lexicographically ordered); display_name comes from the Sent
    -- harvest's To/Cc mailbox names and may lag for post-harvest contacts.
    last_sent_at TEXT,
    display_name TEXT,
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
--                flows OUT through ANY read door it is promoted to 'open' and
--                stamped surfaced_at. An archive/send/dismiss sets 'done' +
--                resolved_at.
--   surfaced_at  first time any door surfaced this row, NULL until then.
--   resolved_at  when the row reached status='done'.
-- Sealed rows carry these columns but are structurally absent from every
-- non-local surface, so they are never surfaced or stamped.
CREATE TABLE IF NOT EXISTS triage (
    message_id      INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL,
    importance      INTEGER NOT NULL DEFAULT 0,
    tier            TEXT NOT NULL DEFAULT 'noise',
    sensitivity     TEXT NOT NULL DEFAULT 'normal',
    sealed_kind     TEXT,
    one_line        TEXT NOT NULL DEFAULT '',
    reason          TEXT NOT NULL DEFAULT '',
    -- Per-property triage justifications: a JSON object
    -- {importance?,deadline?,tier?} of short human-readable reasons. HUMAN-DOOR
    -- ONLY — the agent-door read path leaves it unset, so it never crosses /mcp.
    field_reasons   TEXT,
    deadline        TEXT,
    matched_rule_id INTEGER,
    -- STAGE-1 LLM marker. NULL = still needs the Stage-1 refine pass (its
    -- heuristic seed values are provisional). Otherwise the Stage-1 model id,
    -- 'rule' when a sender rule already decided the row (no model spend), or
    -- 'heuristic-only' when the pass fell back to the seed.
    stage1_model_used TEXT,
    -- Set to 1 by the Stage-1 pass (confident=false), or at ingest for a Filtered
    -- rule needing want_text evaluation, to mark the row for Stage-2 escalation.
    needs_stage2    INTEGER NOT NULL DEFAULT 0,
    -- STAGE-2 LLM marker. NULL = not yet Stage-2 processed. The Stage-2 queue
    -- predicate is `stage1_model_used IS NOT NULL AND needs_stage2=1 AND
    -- model_used IS NULL AND sensitivity='normal'`.
    model_used      TEXT,
    -- CATEGORIZE-THEN-EXTRACT: one coarse LLM category ('general' | 'invoice' |
    -- 'banking_statement' | 'transaction_alert'), NULL when no LLM ever looked.
    -- A category with a registered specialist extractor queues the row for a
    -- structured second pass. SEALED ROWS NEVER RUN THE LLM STAGES, so their
    -- category stays NULL — which is what excludes them from every queue.
    category        TEXT,
    -- SPECIALIST-EXTRACTOR marker. NULL = not yet through its category's
    -- extractor (or the category has none); otherwise the extractor model id or
    -- a 'skip-*' sentinel. Queue predicate: `category IN (<extractable>) AND
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

-- PACKAGE TRACKING. One row per (account, tracking_number), extracted from
-- NON-SEALED shipping mail. The tracking number is REQUIRED — it is the dedupe
-- key, so mail without one is skipped — and a later email about the same number
-- UPDATES the row. `status` walks the ordered<shipped<out_for_delivery<delivered
-- ladder (exception is a flag) and never regresses a delivered shipment.
--
-- SECURITY: detection runs ONLY for sensitivity='normal' mail, so this table has
-- no sealed rows BY CONSTRUCTION and needs no sealed join on read.
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

-- RECEIPTS. One row per (account, message): money ALREADY PAID, extracted from
-- NON-SEALED past-transaction mail. Records, not obligations — auto-resolved
-- (triage.status='done') at ingest so they live only in the Receipts category.
-- `from_addr`/`from_name` are stored so the client renders a merchant name with
-- no extra join; `amount`/`currency` are best-effort (NULL is still a receipt).
-- Detection is independent of shipment detection, so one mail can be both. A
-- landing receipt may also AUTO-CLOSE one matching open bill (audited as
-- bill.auto_close — see triage/receipt_match.rs).
--
-- SECURITY: detection runs ONLY for sensitivity='normal' mail, so this table has
-- no sealed rows BY CONSTRUCTION and needs no sealed join on read.
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

-- CALENDAR UPDATES. One row per (account, message): invite / update /
-- cancellation / RSVP response, extracted from NON-SEALED calendar mail. Like
-- receipts these are RECORDS (the user's real calendar is the source of truth),
-- auto-resolved at ingest so they live only in the Calendar zone.
-- `event_title`/`starts_at`/`organizer` are best-effort — classification comes
-- from the structural subject shape, not extraction. The /client/calendar window
-- filters on received_at (mail arrival), NOT starts_at (event time).
--
-- SECURITY: detection runs ONLY for sensitivity='normal' mail, so this table has
-- no sealed rows BY CONSTRUCTION and needs no sealed join on read.
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

-- BANKING. One row per (account, message): a STATEMENT or a TRANSACTION ALERT,
-- extracted by the banking specialist extractor from a NON-SEALED row the LLM
-- categorized. Like receipts these are RECORDS, so the extractor auto-resolves
-- the triage row (status='done') and they live only in the Banking zone. For a
-- statement `amount` is the TOTAL balance, never the minimum payment; for an
-- alert it is the transaction amount. `account_hint` is ONLY ever a masked
-- last-4 tail ("…1234") — the extractor post-validates, reducing anything longer
-- to a tail or NULL, so a full account number is never stored.
--
-- A receipt and a banking row must never DOUBLE-CREATE: the deterministic
-- receipt detector runs at INGEST, before the categorizer, and the extractor
-- queue excludes any message that already has a receipts row.
--
-- SECURITY: sealed rows never run the LLM stages, so they carry category=NULL
-- and are absent from the extractor queue (the extractor also enforces the
-- release-mode sealed guard). No sealed rows here BY CONSTRUCTION.
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

-- MARKETING. One row per message the LLM categorized `marketing`: brand, the
-- offer in one line, the discount, a promo code, and when it expires. Written by
-- the marketing specialist extractor (triage/extract/marketing.rs).
--
-- SECURITY: like `banking`, NO sealed rows BY CONSTRUCTION — sealed mail never
-- runs the LLM stages, so it carries a NULL category and is absent from every
-- extractor queue.
--
-- This extractor does NOT auto-resolve its triage row (banking does). Marketing
-- is noise-tier already, and resolving it would drop it out of the flat Emails
-- inbox, whose whole promise is that it hides nothing.
CREATE TABLE IF NOT EXISTS marketing (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    message_id  INTEGER NOT NULL,
    brand       TEXT,
    offer       TEXT,
    discount    TEXT,
    -- Shape-validated in the extractor: short, alphanumeric, never a sentence
    -- or a URL. See sanitize_code.
    code        TEXT,
    -- YYYY-MM-DD, and only when plausible relative to received_at.
    expires_at  TEXT,
    received_at TEXT NOT NULL,
    UNIQUE(account_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_marketing_received ON marketing(account_id, received_at);

-- UNSUBSCRIBES. One row per (account, sender_addr): the human-door record that
-- the user asked a sender to stop, plus the "did they honor it?" violation
-- ledger. A fresh request RESETS violation_count/last_violation_at/resolution,
-- restarting the 72h grace clock. `method` is always 'browser' — the client
-- opens the link; the server never delivers anything itself.
-- `source_message_id` is nullable (the message may later be deleted).
-- `resolution` is blocked|dismissed|NULL, NULL meaning the request is still
-- outstanding and the violation detector armed.
--
-- VIOLATION SEMANTICS, applied in the ingest transaction: storing a NON-SENT
-- inbound message whose sender has an unresolved row, more than 72h after
-- requested_at, increments violation_count and stamps last_violation_at.
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

-- AUTH-MAIL SHREDDER (retention). One row per message the retention pass moved
-- to Gmail Trash, so the "N shredded" figure is a real ledger and a message can
-- never be shredded twice.
--
-- WRITTEN BY THE HUMAN DOOR ONLY: the sync daemon holds a `gmail.readonly`
-- credential by hard invariant and physically cannot trash anything.
--
-- POLICY (both keys live in `app_settings`, per account):
--   shred_enabled    '1' | '0'  — DEFAULT OFF; deleting mail on a timer only
--                                 ever happens after a deliberate opt-in.
--   shred_after_days  integer   — default 30. Sealed mail older than this is
--                                 trashed.
--
-- TRASH, NEVER DELETE: the pass calls Gmail's /trash, so everything it touches
-- stays recoverable. The write credential is `gmail.modify`, which CANNOT
-- permanently delete — the blast radius is capped by the scope itself.
CREATE TABLE IF NOT EXISTS shred_log (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL,
    message_id   INTEGER NOT NULL,
    gmail_msg_id TEXT NOT NULL,
    sender       TEXT NOT NULL,
    kind         TEXT,
    received_at  TEXT NOT NULL,
    shredded_at  TEXT NOT NULL,
    UNIQUE(account_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_shred_at ON shred_log(account_id, shredded_at);

-- TRIAGE FEEDBACK. Every human override of the triage pipeline lands here. This
-- is a TRAINING SET, not a UI log: "the model said X, the human said Y" pairs to
-- refine prompts and heuristics against what actually went wrong.
--
-- 1. `original` carries a JSON SNAPSHOT of the whole triage row at correction
--    time, including which model produced it — a label without the features that
--    produced it is near-worthless for refinement.
-- 2. `sender`/`subject` are DENORMALIZED rather than joined from `messages`,
--    because the shredder and the user both delete mail, and a training set that
--    silently loses its inputs cannot be told apart from an empty one.
--
-- NOT unique per message: correcting the same email twice is two facts, and a
-- human changing their mind is itself signal. Append-only, ordered by
-- `corrected_at`.
CREATE TABLE IF NOT EXISTS triage_feedback (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL,
    message_id   INTEGER NOT NULL,
    corrected_at TEXT NOT NULL,
    -- Which axis the human overrode: 'tier' | 'category'.
    dimension    TEXT NOT NULL,
    -- What triage had (NULL when the row never had a value for that axis).
    from_value   TEXT,
    -- What the human says it should have been.
    to_value     TEXT NOT NULL,
    -- JSON snapshot of the triage row as it was (note 1 above).
    original     TEXT NOT NULL,
    -- Denormalized on purpose (note 2 above).
    sender       TEXT NOT NULL,
    subject      TEXT NOT NULL,
    -- Optional free-text from the human ("this is a receipt, not a bill").
    note         TEXT
);

CREATE INDEX IF NOT EXISTS idx_triage_feedback_at
    ON triage_feedback(account_id, corrected_at);

-- Gmail sync cursor, keyed by a logical mailbox string. Exactly one row is
-- stored, keyed mailbox='history': uidvalidity is unused (0) and last_uid holds
-- the account's historyId (a u64 that fits SQLite's i64 INTEGER).
CREATE TABLE IF NOT EXISTS sync_state (
    account_id  INTEGER NOT NULL,
    mailbox     TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL,
    last_uid    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account_id, mailbox)
);

-- STAGE-2 BUDGET (circuit breaker). model_calls counts API attempts, incremented
-- BEFORE the call so retry storms cannot exceed the cap. Two scopes share the
-- table, keyed by thread_id:
--   * per-thread-per-day: thread_id = the message's real thread id.
--   * global-per-account-per-day: thread_id = the sentinel '__global__', which
--     no real Gmail thread id (hex) can collide with.
CREATE TABLE IF NOT EXISTS wake_budget (
    account_id  INTEGER NOT NULL,
    thread_id   TEXT NOT NULL,
    day         TEXT NOT NULL,
    model_calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account_id, thread_id, day)
);

-- LLM USAGE LEDGER. One row per (account, UTC day, category): running API usage
-- totals. `category` is 'stage1', 'stage2', or a specialist-extractor line such
-- as 'extract_banking' — extractors run on the stage-1 model but bill to their
-- own line so per-specialist cost stays visible. `calls` counts SUCCESSFUL
-- classify responses that carried a usage block. Cost is NOT stored: the human
-- door computes it at read time from config-driven per-MTok prices.
CREATE TABLE IF NOT EXISTS stage2_usage (
    account_id    INTEGER NOT NULL,
    day           TEXT NOT NULL,
    category      TEXT NOT NULL DEFAULT 'stage2',
    calls         INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account_id, day, category)
);

-- RUNTIME APP SETTINGS. A per-account key/value table for knobs a client can
-- change WITHOUT editing config.toml or restarting the daemon — the Stage-2
-- daily-cap overrides and the shredder policy keys. Precedence at read time: an
-- app_settings OVERRIDE beats config/env, which beats the built-in default.
-- `value` is TEXT, parsed by the reader.
CREATE TABLE IF NOT EXISTS app_settings (
    account_id INTEGER NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    UNIQUE(account_id, key)
);

-- EMAIL ATTACHMENTS. One row per attachment part extracted from a message's full
-- RFC822 at ingest — real attachments AND cid-inline parts. `data` holds the
-- decoded bytes, or NULL when the part exceeded the ingest cap (10 MB per
-- attachment / 25 MB per message): the metadata still lands (downloadable=false
-- on the wire, byte endpoint 410). `size_bytes` is always the real decoded size.
--
-- SECURITY: attachments are STORED for sealed mail like the body, and guarded on
-- serving — `attachment_bytes` requires the parent's sensitivity='normal', so a
-- sealed parent is indistinguishable from a nonexistent id (both 404), and the
-- metadata-listing thread view already 404s any sealed thread.
CREATE TABLE IF NOT EXISTS attachments (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    message_id  INTEGER NOT NULL,
    filename    TEXT NOT NULL,
    mime        TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    data        BLOB,              -- NULL when over the ingest cap (metadata only)
    UNIQUE(account_id, message_id, filename, size_bytes)
);

CREATE INDEX IF NOT EXISTS idx_attachments_message ON attachments(account_id, message_id);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(subject, body);

-- ON-BOX SEMANTIC RECALL. A sqlite-vec `vec0` table holding one embedding per
-- NON-SEALED message; the rowid is `messages.id`, so a KNN hit joins straight
-- back. `account_id` is a vec0 METADATA column so KNN queries can constrain by
-- account in the WHERE clause.
--
-- SECURITY: SEALED MESSAGES ARE NEVER INSERTED HERE — the embed-at-write path is
-- gated on `sensitivity='normal'`, so sealed content is absent from the vector
-- space entirely. `semantic_search` re-excludes sealed rows anyway.
--
-- DIMENSION: float[384] matches BGE-small-en-v1.5, and the store asserts the
-- configured embedder matches at attach time. Changing it means editing this
-- literal AND resetting the dev db — there are no migrations for a vec0 table.
CREATE VIRTUAL TABLE IF NOT EXISTS message_vecs USING vec0(
    message_id INTEGER PRIMARY KEY,
    embedding  FLOAT[384],
    account_id INTEGER
);

-- Audit log for the HUMAN DOOR (squelch-api /client/*). Every sealed-body reveal
-- and every write action appends a row here BEFORE returning; MCP never reads or
-- writes it. One internal writer also exists: the ingest receipt->bill
-- auto-close (actor='ingest', action='bill.auto_close'), so that state change
-- stays explainable.
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

-- NOTIFICATION EVENT LOG. The source of truth for "something worth interrupting
-- the user happened", written by the SYNC ENGINE at each triage verdict — never
-- by a store write method, because only the engine knows which sync path it is
-- on and whether the mail is fresh. Delivery adapters each track their OWN
-- monotonic cursor; there is deliberately no global 'delivered' flag, which
-- would make the second client a refactor.
--
-- Every column besides the ids is a DENORMALIZED SNAPSHOT at emission time, so a
-- client can render the notification from this row alone (the iOS NSE fetches
-- one row by id after an opaque push and has no second round-trip to spend).
--
-- ONE EVENT PER MESSAGE, EVER: UNIQUE(message_id) plus INSERT OR IGNORE at the
-- writer, which is what makes re-ingest and the refine passes idempotent. The
-- storm guard proper is the freshness window in `triage::events`.
--
-- SECURITY: sealed mail is never represented here — the emission decision
-- requires sensitivity='normal', and sealed rows carry no Stage-1 pass. An OTP
-- on a lock screen would undo the entire seal design. A human sealing a message
-- AFTER it notified makes `correct_triage` REDACT the row (sender/one_line/
-- deadline blanked, structure kept). It does NOT delete: `id` is the rowid, and
-- deleting the newest row would let the next insert reuse that id, which every
-- durable cursor would then skip forever.
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY,   -- monotonic; the clients' cursor
    account_id  INTEGER NOT NULL,
    message_id  INTEGER NOT NULL UNIQUE,
    thread_id   TEXT NOT NULL,
    kind        TEXT NOT NULL,         -- urgent | deadline | surfaced | opened
    tier        TEXT NOT NULL,
    importance  INTEGER NOT NULL,
    sender      TEXT NOT NULL,
    one_line    TEXT NOT NULL,
    deadline    TEXT,                  -- RFC3339 snapshot, or NULL
    created_at  TEXT NOT NULL
);

-- The read pattern is exactly "rows after my cursor, for my account".
CREATE INDEX IF NOT EXISTS idx_events_account_id ON events(account_id, id);

-- REGISTERED PUSH DEVICES. One row per APNs device token the user's phone handed
-- to their own daemon over the human door. Only the pusher task reads it.
--
-- `token` is UNIQUE and re-registration is an UPSERT, because iOS hands the app
-- its token on EVERY launch and a chatty app would otherwise grow one row per
-- cold start. `last_registered_at` is the liveness signal.
--
-- Rows die exactly two ways: the human door's DELETE, and APNs answering `410
-- Unregistered` — the relay passes that status back verbatim precisely so this
-- daemon, not shared infrastructure, owns the cleanup.
--
-- PRIVACY: a device token is the user's capability material. It is never logged
-- (the pusher logs row ids, or an 8-char prefix at most) and never crosses the
-- agent door.
CREATE TABLE IF NOT EXISTS devices (
    id                 INTEGER PRIMARY KEY,
    account_id         INTEGER NOT NULL,
    -- Hex APNs device token, UNIQUE across accounts (one device, one row).
    -- Re-registering another account's token is REFUSED, not rebound: the
    -- upsert's conflict update carries
    -- `WHERE devices.account_id = excluded.account_id`, so registration can never
    -- silently repoint an existing device's pushes.
    token              TEXT NOT NULL UNIQUE,
    platform           TEXT NOT NULL DEFAULT 'ios',
    created_at         TEXT NOT NULL,
    last_registered_at TEXT NOT NULL
);

-- The pusher's read is exactly "every token for my account".
CREATE INDEX IF NOT EXISTS idx_devices_account ON devices(account_id);

-- LOCAL DRAFTS. One unsent composition per reply target, plus ONE new-message
-- draft per account (`reply_to_message_id IS NULL`).
--
-- HUMAN-DOOR DATA ONLY: served exclusively by /client/drafts, never synced to
-- Gmail Drafts, and never visible on /mcp — an unsent draft is the user's own
-- thinking, not mail the agent door was handed (two-door invariant).
--
-- Uniqueness is two PARTIAL indexes rather than UNIQUE(account_id,
-- reply_to_message_id), because SQLite treats NULLs as distinct in a UNIQUE:
-- the new-message draft would otherwise be insertable without bound.
CREATE TABLE IF NOT EXISTS drafts (
    id                  INTEGER PRIMARY KEY,
    account_id          INTEGER NOT NULL,
    reply_to_message_id INTEGER,          -- NULL = the new-message draft
    to_addr             TEXT NOT NULL DEFAULT '',
    subject             TEXT NOT NULL DEFAULT '',
    body                TEXT NOT NULL DEFAULT '',
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_drafts_reply
    ON drafts(account_id, reply_to_message_id) WHERE reply_to_message_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_drafts_new
    ON drafts(account_id) WHERE reply_to_message_id IS NULL;

-- OUTBOUND READ TRACKING. One `send_trackers` row per tracked send: the minted
-- token IS the pixel URL's path segment, so it is the only thing a recipient's
-- mail client ever hands back. Tracking is opt-in per send AND requires
-- `[tracking] base_url`; an untracked send writes nothing here.
--
-- `message_id` is the LOCAL messages.id of the echoed copy of the sent mail,
-- backfilled after the send's echo lands, and NULL when the echo did not (a
-- sealed or failed echo). Timestamps are unix seconds, not the RFC3339 text the
-- mail tables use — nothing here is a mail date, and the opens feed carries
-- epoch integers end to end.
--
-- PRIVACY: a token is unguessable capability material minted per send. It is
-- never logged and never crosses the agent door; /mcp does not know these
-- tables exist.
CREATE TABLE IF NOT EXISTS send_trackers (
    token      TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL,
    message_id INTEGER,
    created_at INTEGER NOT NULL
);

-- ONE ROW PER OBSERVED OPEN, append-only. Rows arrive two ways: the daemon's own
-- unauthenticated GET /t/:token, and the opens poller draining the relay. Both
-- insert ONLY for a token that names an existing `send_trackers` row, so an
-- unknown token leaves no trace.
--
-- `classification` is what the fetch's User-Agent implies, never a claim about a
-- human: 'proxied' for Gmail's image proxy (an open that may be a cache warm),
-- 'unknown' otherwise.
CREATE TABLE IF NOT EXISTS message_opens (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    token          TEXT NOT NULL,
    opened_at      INTEGER NOT NULL,
    user_agent     TEXT,
    classification TEXT NOT NULL DEFAULT 'unknown'
);

-- The read pattern is exactly "every open for this token".
CREATE INDEX IF NOT EXISTS idx_message_opens_token ON message_opens(token);
