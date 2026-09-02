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
    -- DISPLAY RECIPIENTS of mail the user SENT: the To + Cc + Bcc mailboxes as
    -- the headers spelled them, comma-joined `Name <addr>` (bare addr with no
    -- display name). NULL on received mail — a message the user did not send
    -- has no "to" worth showing — and NULL on sent rows ingested before this
    -- column existed, which the one-shot recipients backfill fills in. Empty
    -- string means "looked, and the headers named nobody". Consumed ONLY by the
    -- human door's sent listing; the agent door has no sent surface at all.
    to_addrs    TEXT,
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
-- Sender lookups: the escalation context assembles one sender's track record per
-- queued row, inside a per-row loop that runs every sync tick. Unindexed, that is
-- a full scan of `messages JOIN triage` per escalated row, taken while holding
-- the store's single connection mutex.
CREATE INDEX IF NOT EXISTS idx_messages_from ON messages(account_id, from_addr);

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
-- THE LOOKUP INDEX. Every reader compares `addr` under COLLATE NOCASE (the
-- Sent-history harvest lowercases, the per-message Sent ingest keeps the
-- header's spelling, so neither side can be assumed normalized), and SQLite
-- uses an index only when its collation MATCHES the comparison's. The primary
-- key above is BINARY, so `addr = ?2 COLLATE NOCASE` walked every contact of
-- the account instead of seeking one. In the standing band that walk is
-- CORRELATED, once per message in the 30-day window: on a thousand-message
-- store the band cost 140ms and the header's `stats()` 300ms, per client, every
-- 10s, under the store mutex both doors and the sync loop wait on; a bigger
-- mailbox paid seconds, and that queue was the whole p95. With this index both
-- read in ~2ms. Measured 2026-08-27; tests/attention.rs pins the seek.
CREATE INDEX IF NOT EXISTS idx_contacts_addr_nocase
    ON contacts(account_id, addr COLLATE NOCASE);

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
--   remind_at    a PENDING reminder (see the column comments below).
--   reminded_at  a reminder that already fired.
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
    -- 'rule' when a sender rule already decided the row (no model spend),
    -- 'human' when the account owner corrected the verdict by hand, 'n/a' for
    -- sealed and sent mail, or one of the two NO-VERDICT sentinels:
    --   'stale-skip'      too old for the pass's horizon; no model was asked.
    --   'heuristic-only'  a model WAS asked and did not answer (refusal or a
    --                     permanent failure), so the seed stands.
    -- Both leave the row on its heuristic seed and they are easy to conflate,
    -- which is why they are separate strings: they are opposite facts, and the
    -- row is the only place either one is recorded. Rows stamped before the
    -- split may carry 'heuristic-only' for either reason and cannot be
    -- re-attributed — nothing records when a row was processed.
    stage1_model_used TEXT,
    -- Set to 1 by `triage::router::should_escalate` over the Stage-1 verdict, or
    -- at ingest for a Filtered rule needing want_text evaluation, to mark the row
    -- for Stage-2 escalation.
    needs_stage2    INTEGER NOT NULL DEFAULT 0,
    -- How many times this row has been RE-EVALUATED (see `triage_revisits`).
    -- Bounded by `RevisitConfig::max_per_message_lifetime` so a verdict that
    -- keeps answering "check again tomorrow" terminates instead of billing
    -- forever.
    revisit_count   INTEGER NOT NULL DEFAULT 0,
    -- WHICH router arm escalated the row ('buried_bill' | 'unverified_urgency' |
    -- 'scam_shape' | 'exception' | 'invoice' | 'sender_corrected' | 'boundary' |
    -- 'model_unsure'). NULL when the row did not escalate. Stored so the
    -- escalation MIX can be inspected: the arms are the design, and tuning them
    -- blind to which one fires is guesswork.
    escalation_reason TEXT,
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
    -- SHIPMENTS-EXTRACTOR trigger + marker, stamped at INGEST from a LOOSE
    -- shipping signal (`triage::shipment::has_loose_shipping_signal`) rather than
    -- from an LLM category:
    --   NULL        no shipping signal at ingest — this row never queues.
    --   'pending'   queued for the shipments extractor.
    --   anything    a processed marker: the extractor model id, or one of the
    --               'stale-skip' / 'apply-failed' / 'extract-failed' sentinels.
    -- Its own column and its own queue BECAUSE the extract queue routes on
    -- `triage.category` (there is no shipping category) and excludes
    -- receipt-bearing rows, which most order confirmations are. Re-ingest
    -- PRESERVES a processed marker and only refreshes 'pending'; sealing NULLs
    -- it, and retriage re-pends it.
    ship_extract_model TEXT,
    -- WHEN A HUMAN LAST ASKED for this row to be re-triaged (RFC3339 UTC), NULL
    -- when nobody ever has. Stamped by `Store::retriage_reset` on exactly the
    -- rows it requeues, and read by the LLM passes as a FORCE: a re-triage is an
    -- explicit request, so for `RETRIAGE_FORCE_WINDOW` after it the row bypasses
    -- the age-based stale skip that would otherwise stamp it processed without a
    -- model call. The window is what keeps the force from outliving the request:
    -- the same row can re-enter a queue months later through a revisit, and a
    -- permanent stamp would quietly buy every one of those a frontier call.
    retriage_at     TEXT,
    -- "REMIND ME LATER" (RFC3339 UTC), NULL when no reminder is pending. Setting
    -- it also marks the thread done, so a snoozed mail leaves every band at once
    -- instead of sitting there nagging until its date arrives. The sweep in the
    -- sync loop is what un-defers it: at `remind_at <= now` the row goes back to
    -- status='open' and the stamp MOVES to `reminded_at`, so exactly one of the
    -- two is ever set — pending or fired, never both.
    remind_at       TEXT,
    -- WHEN A PENDING REMINDER FIRED, NULL until one does. Read as a standing-band
    -- arm (see `STANDING_BAND`): a fired reminder outranks tier, because the user
    -- personally declared this mail owed attention and the triage model's opinion
    -- of it stopped mattering at that moment. Cleared when a NEW reminder is set,
    -- and left alone by a plain clear — clearing a pending reminder says nothing
    -- about one that already came due.
    reminded_at     TEXT,
    -- WHEN THE USER OPENED THIS MAIL, NULL until they do.
    --
    -- NOT `surfaced_at`, AND THE DIFFERENCE IS THE WHOLE POINT. A row is
    -- surfaced the first time it flows out through ANY read door, which
    -- includes appearing in a list the user scrolled past; it means "this was
    -- in front of them". This says they opened it. One is what we showed, the
    -- other is what they read, and only the second can answer "how much of my
    -- mail do I still have to open myself".
    --
    -- WRITTEN BY THE CLIENT SAYING SO (`POST /client/thread/{id}/opened`), not
    -- by the thread GET, and that route's comment has the reasoning: the client
    -- prefetches threads nobody looked at, and opens warmed ones without asking
    -- this daemon anything. Serving a body is evidence of neither.
    --
    -- IT SEES ONLY PASSBAND. Mail read in Gmail on a phone is never stamped
    -- here, so anything computed from it UNDERSTATES opens and flatters the
    -- product. Every consumer has to know that; see `Store::share_open_rate`,
    -- which is the only one, and which says so.
    opened_at       TEXT,
    -- MAY THIS MESSAGE EVER NOTIFY, and from when (RFC3339 UTC). NULL means
    -- never: a backfill row, the user's own sent copy, or mail that was already
    -- older than `notify.freshness_window_secs` the first time we saw it.
    --
    -- Written by the SYNC ENGINE on the row's FIRST INSERT only and preserved
    -- verbatim on conflict, because only the engine knows which sync path an
    -- ingest is on — and that, not the sender's `Date:` header, is the honest
    -- answer to "is this new mail or are we re-reading the archive". The stamp
    -- replaces a per-emission freshness check that measured `messages.received_at`,
    -- which could not tell old mail we are backfilling from new mail we were
    -- slow to reach and so ate 24.7% of notify-worthy mail outright
    -- (docs/NOTIFY.md §2a). Emission now measures `now - notify_eligible_at`
    -- against `notify.rescue_window_secs`, so a verdict that came back late
    -- still buzzes and only a genuinely stale one is dropped.
    --
    -- NOT BACKFILLED: every row that predates the column is NULL, which is the
    -- silent direction.
    notify_eligible_at TEXT,
    status          TEXT NOT NULL DEFAULT 'new',
    surfaced_at     TEXT,
    resolved_at     TEXT,
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_triage_sensitivity ON triage(account_id, sensitivity);
CREATE INDEX IF NOT EXISTS idx_triage_status ON triage(account_id, status);
-- `idx_triage_remind_at ON triage(account_id, remind_at) WHERE remind_at IS NOT
-- NULL` is created in migrate.rs, NOT here, for the same reason as
-- `idx_shipments_order_ref` below: this file runs in full on every open, BEFORE
-- the column migrations, so an index over a migrated column would fail ("no such
-- column: remind_at") on every pre-existing DB and make the store unopenable.

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
    -- CARRIER POLLING. Everything below is filled by the carrier API, never by
    -- mail, and NULL/0 until the first poll. The carrier is ground truth for
    -- `status` (see `triage::shipment::ShipmentStatus::reconcile_carrier`), but
    -- a poll NEVER moves `last_message_id` — no message backs it.
    --
    -- The carrier's own latest status string, verbatim. Recorded even when it
    -- maps to no `status` value, so the client can show what the carrier said.
    carrier_status_raw TEXT,
    -- Carrier-estimated delivery; NULL when the carrier gives none.
    eta             TEXT,
    -- When the package landed, stamped by whichever path saw it first (a
    -- delivered email or a poll) and never overwritten after.
    delivered_at    TEXT,
    -- Last poll ATTEMPT, success or permanent failure. NULL = never polled.
    last_polled_at  TEXT,
    -- Consecutive PERMANENT poll failures (an unknown/expired number), the
    -- retirement cap for the poll queue. Transient errors do not count, and a
    -- successful poll resets it to 0.
    poll_failures   INTEGER NOT NULL DEFAULT 0,
    -- The RETAILER's identifier for the purchase ("112-3456789-1234567"), when
    -- the mail carried one. NOT a dedupe key — `tracking_number` still is — but
    -- the join back to `shipment_orders`, so a tracking-bearing email can absorb
    -- the staged order it belongs to. NULL when no order reference was found.
    order_ref       TEXT,
    -- The MERCHANT NAMESPACE for `order_ref`: the registrable domain of the
    -- feeding message's sender, lowercased. An order reference is only unique
    -- WITHIN a merchant — "Order #1042" from two shops is two purchases — so
    -- every order_ref lookup is scoped by this column. NULL alongside a NULL
    -- order_ref, and on rows written before the column existed.
    order_merchant  TEXT,
    -- IMMUTABLE PROVENANCE: the message that CREATED this row, written once on
    -- INSERT and never updated. `last_message_id` moves to whichever mail most
    -- recently advanced the row, so it answers "who touched this last", not
    -- "who minted this" — and the extractor's phantom reaping must only ever
    -- reach rows the message in hand actually created.
    created_by_message_id INTEGER,
    -- Which message's extraction supplied the CURRENT `item_name`. Usually
    -- `last_message_id`, but three paths DONATE a name onto a row another mail
    -- feeds (staged-order promotion, the order-ref-only adoption, the thread
    -- adoption), and sealing a message must scrub the text it contributed
    -- wherever it landed. NULL when no name, or on pre-column rows.
    item_name_msg   INTEGER,
    -- WHICH MECHANISM supplied the current `item_name`: 'regex' (the ingest
    -- detector lifting it out of a subject or body) or 'llm' (the shipments
    -- extractor). The sibling of `item_name_msg`, which answers WHICH MESSAGE.
    -- An llm name replaces a regex one outright; a regex name never replaces an
    -- llm one; within a source, longer-wins. Reset to 'regex' whenever the name
    -- is scrubbed (sealing, re-triage), so a source marker whose name is gone
    -- cannot lock a later regex name out.
    --
    -- `shipment_orders` deliberately has NO such column: only the extractor ever
    -- writes that table, so every name in it is 'llm' by construction.
    item_name_source TEXT NOT NULL DEFAULT 'regex',
    -- USER CLEAR (RFC3339, NULL = not cleared): the user said "stop showing me
    -- this". READ-SIDE ONLY, and it is never un-set: a listing hides the row
    -- only while `last_update <= cleared_at`, so the moment anything advances
    -- `last_update` past this stamp the row returns by itself. The row keeps
    -- being polled the whole time — polling is what produces that update.
    cleared_at      TEXT,
    UNIQUE(account_id, tracking_number)
);

CREATE INDEX IF NOT EXISTS idx_shipments_status ON shipments(account_id, status);
-- `idx_shipments_order_ref ON shipments(account_id, order_ref)` is created in
-- migrate.rs, NOT here. This file runs in full on every open, BEFORE the column
-- migrations, so an index over a migrated column would fail ("no such column:
-- order_ref") on every pre-existing DB and make the store unopenable.

-- ORDERS STAGING. A purchase the shipments extractor recognized but that carries
-- NO TRACKING NUMBER yet (the order confirmation arrives days before the ship
-- notice). Keyed by (account, merchant, order_ref) instead of a tracking number,
-- so it cannot live in `shipments` — that table's identity IS the tracking
-- number. When the ship notice lands with both the order reference and a number,
-- the staged row is promoted into `shipments` and deleted here.
--
-- The MERCHANT is part of the key on purpose: "Order #1042" is unique only
-- within the shop that issued it, and an unnamespaced key lets one retailer's
-- confirmation donate its product name onto another retailer's package.
-- `order_merchant` is NOT NULL (defaulting to '' for an underivable sender)
-- because SQLite treats every NULL in a UNIQUE index as distinct, which would
-- silently disable the staging upsert's ON CONFLICT.
--
-- SECURITY: written only from the shipments extractor, whose queue gates on
-- sensitivity='normal', so this table has no sealed rows BY CONSTRUCTION.
-- Sealing a message still deletes the rows it fed and scrubs the names it merely
-- donated (`item_name_msg`), and so does a re-triage.
CREATE TABLE IF NOT EXISTS shipment_orders (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    order_ref TEXT NOT NULL,
    order_merchant TEXT NOT NULL DEFAULT '',
    item_name TEXT NOT NULL DEFAULT '',
    thread_id TEXT NOT NULL DEFAULT '',
    last_message_id INTEGER,
    -- Which message's extraction supplied the CURRENT `item_name` — the pointer
    -- above moves to the latest feeder, so it cannot answer that on its own.
    item_name_msg INTEGER,
    first_seen TEXT NOT NULL,
    last_update TEXT NOT NULL,
    UNIQUE(account_id, order_merchant, order_ref)
);

CREATE INDEX IF NOT EXISTS idx_shipment_orders_msg ON shipment_orders(account_id, last_message_id);

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
-- The two shapes every triage queue asks of this table, once per queued row:
-- "has the owner ever corrected this SENDER" (the router's strongest signal) and
-- "did they correct THIS MESSAGE" (the row a model may never overwrite).
CREATE INDEX IF NOT EXISTS idx_triage_feedback_sender
    ON triage_feedback(account_id, sender);
-- NOCASE twin of the sender index: the triage queues probe this table with
-- `sender = m.from_addr COLLATE NOCASE` once per candidate row, and a BINARY
-- index cannot serve a NOCASE comparison (see `idx_contacts_addr_nocase`).
CREATE INDEX IF NOT EXISTS idx_triage_feedback_sender_nocase
    ON triage_feedback(account_id, sender COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_triage_feedback_msg
    ON triage_feedback(account_id, message_id);

-- SCHEDULED RE-EVALUATIONS. A verdict is true as of a moment, and some mail
-- stops mattering when a date passes (see `crate::triage::revisit`). Each row is
-- one future point at which a message should be scored again.
--
-- A row fires at most once: `fired_at` moves NULL -> a timestamp and stays. A
-- FIRED row is then permanent history — the audit trail of why a message's
-- verdict changed after the fact — while PENDING rows are replaced wholesale
-- each time a new verdict lands, so a re-triaged message carries one current
-- schedule rather than the accumulated intentions of every verdict it ever had.
-- The pass reads `fired_at IS NULL AND revisit_at <= now`, which is exactly the
-- index below.
CREATE TABLE IF NOT EXISTS triage_revisits (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    -- When to look again (RFC3339 UTC).
    revisit_at TEXT NOT NULL,
    -- Short reason, shown back to the model on re-evaluation. MODEL-AUTHORED for
    -- source='model', therefore untrusted text: neutralized before it renders
    -- inside any prompt.
    reason     TEXT NOT NULL,
    -- 'model' (the classifier named the date) | 'deadline' | 'fye_stale' (both
    -- automatic; see `crate::triage::revisit::RevisitSource`).
    source     TEXT NOT NULL,
    created_at TEXT NOT NULL,
    -- NULL while pending; stamped when the revisit pass consumes the row.
    fired_at   TEXT
);

CREATE INDEX IF NOT EXISTS idx_triage_revisits_due
    ON triage_revisits(account_id, fired_at, revisit_at);
CREATE INDEX IF NOT EXISTS idx_triage_revisits_msg
    ON triage_revisits(account_id, message_id);

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

-- GMAIL-SIDE INBOX UNREAD COUNTS (labels/INBOX messagesUnread/threadsUnread),
-- refreshed once per sync cycle. One row per account, overwritten each fetch:
-- this mirrors Gmail's current state, it is not a history. Sync's own tables
-- cannot answer it — they hold only what the backfill window ingested, and
-- nothing here ever learns that Gmail marked a message read.
--
-- NO ROW MEANS NEVER FETCHED, which readers must serve as absence: zero unread
-- is a real and different answer.
CREATE TABLE IF NOT EXISTS inbox_unread (
    account_id INTEGER NOT NULL PRIMARY KEY,
    messages   INTEGER NOT NULL,
    threads    INTEGER NOT NULL,
    fetched_at TEXT NOT NULL
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
-- `input_tokens` is the UNCACHED prompt remainder; prompt-cache writes and
-- reads sit in their own columns because they price differently (kept in sync
-- with the additive migration in migrate.rs for pre-existing DBs).
CREATE TABLE IF NOT EXISTS stage2_usage (
    account_id    INTEGER NOT NULL,
    day           TEXT NOT NULL,
    category      TEXT NOT NULL DEFAULT 'stage2',
    calls         INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
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
    -- The part's normalized Content-ID (brackets stripped), NULL when it declared
    -- none. What an <img src="cid:..."> in the stored body_html resolves against.
    -- Deliberately NOT in the UNIQUE key below: widening that key would re-open
    -- the duplicate-file ingest DoS it exists to absorb. Two inline parts alike in
    -- everything but their cid therefore still collapse to one row, and the
    -- second img is simply dropped from the body rather than painted broken.
    content_id  TEXT,
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
-- storm guard proper is `triage.notify_eligible_at`, stamped once at ingest:
-- a backfill row is structurally unable to reach this table, and a re-scan
-- re-ingests rows that already carry their stamp, so it cannot manufacture one.
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
    -- Opaque client-minted label for the account this device filed the
    -- registration under, echoed back on every push aimed at it. The receiving
    -- extension has nothing else to go on: event ids are per-daemon ints, so a
    -- phone holding two mailboxes cannot tell whose event 41 just arrived. NULL
    -- for anything registered before the field existed, and for the macOS
    -- client, which never asks a daemon to push at all.
    tag                TEXT,
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
    -- The composer's other two recipient lists, comma-joined exactly as
    -- to_addr holds the visible ones. A draft that could not hold these would
    -- restore addressed to fewer people than it was written for -- and for a
    -- bcc that loss is silent, since nothing else on screen would show the
    -- audience had ever been set.
    cc_addr             TEXT NOT NULL DEFAULT '',
    bcc_addr            TEXT NOT NULL DEFAULT '',
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

-- PER-DEVICE HUMAN-DOOR TOKENS. One row per credential the human door will
-- accept, issued by `squelchd token issue` or by a successful pairing claim.
-- This is what lets the door say "this phone, revoked" instead of "rotate the
-- one shared secret and re-key every client".
--
-- SQUELCH_API_TOKEN is NOT represented here and never will be: the env var stays
-- the self-host master key, checked first and independently, so a store that has
-- lost every row still has an operator way in.
--
-- `token_hash` is the lowercase hex SHA-256 of the FULL presented token, prefix
-- included. THE PLAINTEXT IS NEVER STORED — it exists once, at mint, on its way
-- to the operator's terminal or the pairing response. UNIQUE both dedupes the
-- (astronomically unlikely) collision and gives the per-request point lookup its
-- index; verification is a lookup by hash, so there is no scan to time.
--
-- `revoked_at` is a TOMBSTONE, not a delete: revocation must be visible in
-- `token list` after the fact, and reusing an id would be a footgun for anything
-- that recorded one. A row with `revoked_at` set can never authenticate again.
--
-- `last_used_at` is liveness only, written at most once a minute per token (see
-- `verify_device_token`) so an SSE-driven client cannot turn every request into
-- a write.
CREATE TABLE IF NOT EXISTS device_tokens (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL,   -- -> accounts.id
    token_hash   TEXT NOT NULL UNIQUE,
    -- Operator/device-supplied label, trimmed and length-capped by the store.
    -- Never secret, and the only thing `token list` prints besides timestamps.
    name         TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    last_used_at TEXT,
    revoked_at   TEXT
);

-- `token list` reads exactly "every token for my account".
CREATE INDEX IF NOT EXISTS idx_device_tokens_account ON device_tokens(account_id);

-- PAIRING CODES. The short-lived, human-transcribable bridge from a paired
-- device to a device token: `squelchd pair` mints one, the app presents it to
-- POST /client/pair, and the claim trades it for a token.
--
-- ONE ACTIVE CODE PER ACCOUNT, enforced by the mint deleting every prior row for
-- the account. A code is worth ~40 bits, which is only safe because it is
-- single-use, expires in minutes, and burns after a handful of misses — so
-- letting several accumulate would multiply the guessing surface for no gain.
-- That also bounds the table at one row per account, which is why there is no
-- index on `code_hash`: the claim's lookup has nothing to scan.
--
-- `code_hash` follows the same discipline as `device_tokens.token_hash`
-- (lowercase hex SHA-256 of the normalized code); the plaintext is never stored.
-- NOT UNIQUE, deliberately: uniqueness would be a cross-account oracle at mint.
--
-- `failed_attempts` counts misses against the live code, not against a caller
-- (the claim is unauthenticated and has no caller identity). At the cap the row
-- is DELETED, which is why burn, expiry and a wrong code are one answer: the
-- claim finds no active row in all three cases and cannot tell them apart.
--
-- `claimed_at` is the one-shot marker. It is a stamp rather than a delete so a
-- replay of a just-used code takes the same "no active row" path as everything
-- else, and the successful pairing stays visible until the next mint.
-- `id` is AUTOINCREMENT, which almost nothing here is, and the audit log is why.
-- The mint SUPERSEDES by DELETE, so a plain INTEGER PRIMARY KEY recycles the
-- rowid and two consecutive pairing windows both audit as `code:1` — mint, claim
-- and burn rows from different windows become impossible to tell apart. A
-- monotonic id costs one extra row in `sqlite_sequence` on a table that holds at
-- most one row, and buys an unambiguous ledger.
CREATE TABLE IF NOT EXISTS pairing_codes (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      INTEGER NOT NULL,   -- -> accounts.id
    code_hash       TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    failed_attempts INTEGER NOT NULL DEFAULT 0,
    claimed_at      TEXT,
    created_at      TEXT NOT NULL
);

-- The mint's supersede reads exactly "every code for my account".
CREATE INDEX IF NOT EXISTS idx_pairing_codes_account ON pairing_codes(account_id);

-- NORMALIZED SENT RECIPIENTS: one row per (sent message, bare address), written
-- from the SAME faithful mailbox list `messages.to_addrs` is rendered from.
--
-- `to_addrs` is a DISPLAY string (`"Doe, Jane" <j@x>, bob@y`), so the only way to
-- ask "who did I send this to" against it is a `LIKE '%addr%'` scan per address
-- — which is what the send-group history would have had to do, per member, per
-- query. This table makes that an indexed join instead.
--
-- FAITHFUL, NOT FILTERED — deliberately unlike `contacts`, which drops the
-- account's own address and robot addresses to protect "people I know". This
-- answers "who did this go to", so a note to self and a `support@` both belong.
-- `addr` is lowercased and deduped per message; display names live in
-- `to_addrs` and are no part of the key.
--
-- SENT MAIL ONLY. Received mail has no `to_addrs` and writes nothing here, so
-- the table can never be read as an inbound-recipient index.
CREATE TABLE IF NOT EXISTS message_recipients (
    account_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,   -- -> messages.id
    addr       TEXT NOT NULL,      -- lowercased bare address, no display name
    PRIMARY KEY(account_id, message_id, addr)
);

-- The group-history read is "every sent message naming any of these addresses",
-- so the address is the leading column.
CREATE INDEX IF NOT EXISTS idx_message_recipients_addr
    ON message_recipients(account_id, addr);

-- A NAMED AUDIENCE the user can address as one ("preseed investors" -> 12
-- mailboxes). HUMAN-DOOR DATA ONLY: served by `/client/groups`, never visible on
-- /mcp — who the user talks to as a bloc is not something the agent door was
-- handed (two-door invariant).
--
-- NOT NAMED `groups`: `GROUPS` is a SQLite window-frame keyword, and while
-- SQLite would accept it unquoted today, every query naming it is one grammar
-- change away from a syntax error.
--
-- `mode` IS THE GROUP'S OWN PROPERTY, not the composer's, because it is a fact
-- about the audience rather than about one message: an investor list is
-- individually-addressed every time or it is not one.
--   'to'         one message, every member in To — they see each other
--   'bcc'        one message, every member in Bcc — they do not
--   'individual' one message PER member, sent separately
-- Unrecognized values read as 'to' (the visible, least surprising shape); see
-- `GroupMode::parse`.
--
-- `slug` is the lowercased, whitespace-collapsed `name`: the uniqueness key, and
-- what composer autocomplete matches, so "Preseed Investors" and
-- "preseed investors" cannot both exist.
CREATE TABLE IF NOT EXISTS send_groups (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    name       TEXT NOT NULL,
    slug       TEXT NOT NULL,
    mode       TEXT NOT NULL DEFAULT 'to',
    note       TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(account_id, slug)
);

-- MEMBERSHIP. `addr` is the lowercased bare address and half the primary key, so
-- adding someone twice is a no-op rather than a duplicate recipient. The
-- `display_name` is a convenience copy for rendering the member list without a
-- join; `contacts` remains the source of truth for who anyone is.
--
-- ON DELETE CASCADE: `PRAGMA foreign_keys = ON` is set at the top of this file,
-- so dropping a group takes its membership with it.
CREATE TABLE IF NOT EXISTS group_members (
    group_id     INTEGER NOT NULL REFERENCES send_groups(id) ON DELETE CASCADE,
    account_id   INTEGER NOT NULL,
    addr         TEXT NOT NULL,
    display_name TEXT,
    added_at     TEXT NOT NULL,
    PRIMARY KEY(group_id, addr)
);

-- The member-side of the history join, and the composer's "expand this group".
CREATE INDEX IF NOT EXISTS idx_group_members_addr
    ON group_members(account_id, addr);

-- ONE ROW PER GROUP SEND, whatever shape it took: a single To/Bcc message or a
-- fan-out of N. This is the RECORDED half of a group's history; the derived half
-- (`message_recipients` joined against `group_members`) is what makes a group
-- created today show the year of mail that preceded it. A group send is recorded
-- here AND matches the derived query, so the history read excludes any message a
-- row here already claims.
--
-- `recipients` is a SNAPSHOT COUNT taken at send time. Membership is mutable, and
-- "sent to 12" must not silently become "sent to 15" when three people join
-- later.
--
-- `group_id` does NOT cascade-delete: deleting a group is a statement about who
-- you will address next, not a licence to erase what you already sent. The rows
-- outlive the group, and the history read tolerates a dangling id.
CREATE TABLE IF NOT EXISTS group_sends (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    group_id   INTEGER NOT NULL,
    subject    TEXT NOT NULL,
    mode       TEXT NOT NULL,
    sent_at    TEXT NOT NULL,
    recipients INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_group_sends_group
    ON group_sends(account_id, group_id, sent_at);

-- ONE ROW PER (GROUP SEND, RECIPIENT). For a To/Bcc send every row names the
-- same `message_id`; for a fan-out each names its own, which is what makes read
-- receipts per-recipient in that mode.
--
-- `message_id` is NULL until the echo of that send lands (and stays NULL when it
-- never does — a sealed or failed echo), exactly like `send_trackers.message_id`.
--
-- `status` is 'sent' | 'failed'. A FAN-OUT CAN PARTIALLY FAIL, and that is a
-- first-class state rather than an error: eleven investors got the update and one
-- did not, and the one thing worse than that is not knowing which. `error` holds
-- the redacted reason for the failed row.
CREATE TABLE IF NOT EXISTS group_send_recipients (
    group_send_id INTEGER NOT NULL REFERENCES group_sends(id) ON DELETE CASCADE,
    account_id    INTEGER NOT NULL,
    addr          TEXT NOT NULL,
    message_id    INTEGER,
    status        TEXT NOT NULL DEFAULT 'sent',
    error         TEXT,
    PRIMARY KEY(group_send_id, addr)
);
