# Notification latency and the notify lane

Design notes for issue #177, "make faster". The complaint that opened it: mail
notifications arrive later than competitors', and a user who gets buzzed by
Superhuman first opens Superhuman.

This document is the record of what was measured, what the measurement turned
up that nobody was looking for, and the design those findings point at. §1-10
are the notes; §11 is the locked contract the build was done against, and it
wins wherever the notes and the contract disagree.

## 1. What we measured

Against the author's live daemon on 2026-09-01, 25 events over 14 days, timing
from each message's `Date:` header to its `events` row:

| | lag |
|---|---|
| floor (quiet daemon) | 10.1s |
| typical | 15-50s |
| busy daemon | 60-130s |
| worst observed | 623s |

The floor decomposes roughly like this. Every number but the model call is
measured or bounded by config; the model call is derived from the usage ledger
(187 output tokens per Stage-1 call, thinking at `effort=low` included) rather
than timed directly, so treat it as an estimate with a wide band.

| hop | cost | ours? |
|---|---|---|
| Gmail: `Date:` header to visible in `history.list` | ~1-3s | no, and competitors pay it too |
| poll wait (`sync.poll_secs = 5`) | 0-5s, avg 2.5s | yes |
| `history.list` x2 (INBOX then SENT, serial) | ~0.3-0.6s | yes |
| `format=raw` GET, one message at a time | ~0.2-0.5s each | yes |
| ingest, sanitize, ONNX embed (awaited inline) | ~0.1-0.4s | yes, minor |
| Stage-1 Opus classify | ~3-5s (est.) | yes, the big one |
| `append_event` to broadcast to relay to APNs | ~0.3-1s | fine as is |

Sums to 8-14s against a 10.1s observed floor. About 7 of those 10 seconds are
ours, and the model call is more than half of that.

**The tail is a different problem from the floor.** `SyncEngine::poll_loop` is
one serial conga line:

```
refresh_inbox_unread -> reminder_pass -> poll_once -> stage1_pass
  -> stage2_pass -> extract_pass -> revisit_pass -> backfill_missing_vectors
  -> sleep(poll_secs)
```

New mail cannot be *seen* until the previous cycle's refinement work drains. On
the measured day that was 163 LLM calls producing ~31,300 output tokens, all
generated serially inside the loop that also polls Gmail. A message arriving
while `extract_marketing` grinds through 36 calls waits a minute before anyone
asks Gmail about it.

## 2. What we were not looking for

### 2a. A quarter of notify-worthy mail never notifies at all

Over 14 days, 73 notify-worthy messages arrived *promptly* (ingested well
inside the freshness window, so not backfill). **18 of them, 24.7%, produced no
event ever.** The worst:

```
deadline  imp 93   ingest lag 884s   "Travelers says mortgage company never paid"
deadline  imp 76   ingest lag 855s   "Second failed $39.99 Splitwise charge"
deadline  imp 76   ingest lag 217s   "Second failed $24.99 DistroKid charge"
```

Cause: today the only emission site that fires in practice is Stage-1's apply
site, and `events::worthy_kind`'s storm guard hard-drops anything whose `Date:`
header is more than `notify.freshness_window_secs` (900) old. So queueing does
not merely delay a notification. Past fifteen minutes it deletes it, silently,
with no counter and no log line.

That makes the loop split and the false-negative fix the same work.

### 2b. Rule-decided rows can never notify

`stage1_queue` selects `stage1_model_used IS NULL`. A rule-decided row carries
`'rule'`, so it never reaches the Stage-1 emission site, and the ingest site is
gated on `!llm_available`. With an LLM configured there is no path left.
Confirmed against the live DB: 7 rule rows, 0 events.

Correction, on reading `ingest_message`: only a FILTERED rule stamps
`'rule'` today (it skips to Stage-2, whose emission site then refuses it on
`rule == Filtered`). Squelch and Surface rows enter the Stage-1 queue like
any other mail and emit from its apply site, so "always surface my broker"
does buzz, one Opus call late. The seven silent rows are Filtered ones, and
Filtered staying silent is a separate question this issue leaves alone: the
fast lane records it as `suppressed`, consistent with `worthy_kind`.

### 2c. Sealed mail can never notify

By design, and the design is wrong. See §6.

## 3. The design: two lanes

The core decision (Braelyn, this thread): **importance for triage and final
placement is a different question from importance for notification.** One
verdict currently answers both, and the two want opposite error profiles.

- Placement: precision matters, a wrong tier is visible in the client forever.
- Notification: recall matters. A buzz that did not strictly need to happen
  costs the user a glance. A missed buzz costs them the thing the product
  exists to catch. `UNIQUE(message_id)` on `events` means a buzz can never be
  retracted, which is *why* the current code waits for Opus, and it is also why
  it drops mail on the floor when Opus is late.

| lane | decides | model | records |
|---|---|---|---|
| fast, at ingest | buzz / no buzz, plus `one_line` | small + fast, slim schema, no thinking | `fast/sent` or `fast/declined_by_model` |
| deliberate, Stage-1 | tier, importance, placement | Opus 5, unchanged | `deliberate/sent`, only when rescuing |
| structural gates | sent copy, squelch rule | none | `ineligible`, never rescuable |

**The property that makes this safe: the fast lane is strictly additive.**
Stage-1's apply site emits unconditionally on a successful verdict
(`sync/mod.rs`, the `Ok(true)` arm). So the fast lane can only *add* an early
buzz; it can never suppress one, and Opus remains the backstop for anything the
fast lane declined. Recall goes up, never down. This is the same "additive
only" discipline the escalation router already applies to `ModelUnsure`.

That guarantee is a lie unless §5 is also true, because today the backstop is
exactly what the storm guard eats.

## 4. The decline ledger

When the fast lane declines, record it. When the deliberate pass later
disagrees and buzzes, record that too.

This is worth more than an audit trail: it is a continuously updating eval
corpus generated for free. Every decline that Opus rescues is a labeled false
negative of the notify model, on real mail, with no hand labeling. Every
decline Opus agrees with is a true negative.

```
notify_decisions(message_id, lane, decision, notify_importance, model_used, created_at)
```

One row per (message, lane), **never updated**. A rescued message ends with two
rows: `fast/declined_by_model`, then `deliberate/sent`.

**Why a table and not columns on `triage`.** The same trap `triage_reviews`
exists to avoid: folding confirmations into `triage_feedback` would make the
error corpus read a 100% correction rate. If a rescue overwrites a
`notify_verdict` column, the decline evaporates at exactly the moment it became
interesting, and notify-lane accuracy reads 100% forever.

**Record both directions.** Fast lane buzzed and Opus disagrees is the
false-positive rate. Braelyn's call is that a wrong buzz is survivable
("they open the client and the email is sorted properly"), and that is right,
but survivable is not the same as unmeasured. `notify_rescued` against
`notify_overturned` is the pair of counters that decides whether the threshold
moves, in the shape `record_stage1(Stage1Verdict)` already uses.

**Not every decline is a decline.** Sealed mail, the user's own sent copy, a
squelch rule: those are not "the small model said no", they are "never a
candidate". If they all land as `declined`, the false-negative rate reads
catastrophic for no reason, and worse, a rescue path that asks only "was this
declined?" can fire on a row it must never touch. `declined_by_model` and
`ineligible` are different values and only the former is rescuable.

## 5. Notify eligibility is stamped at ingest

The storm guard exists to stop a fresh install's 30-day backfill from storming
the user's phone. It implements that by measuring the age of the sender's
`Date:` header, which is wrong twice over:

1. `received_at` is sender-controlled. The code already has to clamp
   future-dating to stop a forged header buying permanent freshness.
2. It cannot tell "old mail we are backfilling" from "new mail we were slow
   to get to", so it eats the second along with the first. That is §2a.

`IngestOrigin::Incremental` vs `Backfill` is already a first-class fact at
ingest. Stamp notify eligibility there instead. Backfill rows become
structurally un-notifiable, which is *stronger* than the current date
heuristic, and the window then measures from when we first saw the message, so
a late rescue still lands.

Open: whether a rescue has a ceiling. A buzz 40 minutes after arrival is
probably still welcome, and the eligibility stamp is the knob either way.

## 6. Sealed mail notifies

Today the daemon never emits for it (the Mac app compensates by polling
`/client/sealed` every 30s, iOS gets nothing; §11.6 has the corrected
picture). `triage/events.rs` justifies that as "even a contentless
ping on a lock screen would undo the seal". That rationale does not survive
contact with how push actually works: `PushRequest` is `{event_id,
collapse_id}`, the relay is blind by construction, and nothing about the
message crosses APNs. The entire exposure is what the *device* renders, which
is ours to decide. The rationale conflates "we pinged" with "we showed the
code".

Meanwhile the product cost is severe. A 2FA code you just requested is the most
latency-sensitive mail that exists and the shortest-lived; codes expire in
minutes. It is currently the one thing that never buzzes.

**The seal invariant proper is untouched.** `detect_sealed` runs at parse
before anything reads the body, so the sealed notify decision is
*deterministic*. No model is involved, no agent-door surface, no new LLM
exposure. Which produces the right ordering by accident: sealed mail gets the
**fastest** path in the system, zero model calls, bounded only by poll, fetch
and push.

**HARD CONSTRAINT.** `events` has no `sensitivity` column and not one query
gates on it. That is safe today only because sealed rows cannot produce events.
Readers are the SSE feed and `get_event`, both behind the bearer; the agent
door has zero references to `events`. So sender *metadata* is acceptable (same
class as `/client/sealed`, already sanctioned) but **a sealed event row must
carry no body-derived content**. `one_line` is generated from `sealed_kind`
alone: "Login code arrived", "Password reset requested". Never the subject,
never the code. This belongs in SECURITY.md's "Do not break" list, because it
is exactly the kind of thing a later change "improves" by making the
notification more useful.

The code itself stays behind `/client/sealed/{id}/reveal`, which already writes
the audit row before returning. Tap, Face ID, code.

**Per kind, not one blanket rule.** Live distribution: `otp` 15,
`password_reset` 8, `verification` 6, `login_alert` 6.

- `otp`, `magic_link`, `verification`: requested by the user, who is standing
  there waiting. Immediate, time-sensitive interruption level.
- `password_reset`: same, with the unsolicited case below.
- `login_alert`: *not* requested. "New sign-in from Kyiv" is not
  latency-critical, it is importance-critical. Loud, but a different shape.

**The unsolicited code is a feature, not an edge case.** An OTP you did not
request means someone already has your password and is standing at the 2FA
gate. Today that is invisible. With sealed notifications it is a buzz within
seconds, and no competitor can do it because none of them classify auth mail as
a category at all.

Two notes. The seal detector deliberately biases toward over-sealing, so today
a false seal makes benign mail silently un-notifiable; this change strictly
improves that. And "show the actual code in the notification" should exist as a
setting that defaults **off**.

## 7. Model choice

Two independent levers, and people reach for the second one first.

**Schema.** Stage-1 spends 187 output tokens per call. A notify verdict is
`{should_notify, notify_importance, one_line}`: no `reason`, no
`importance_reason`, no `deadline_reason`, and `effort: None` so no thinking.
Call it 50 tokens. That is roughly a 3x cut on generation before the model
changes at all.

**Model.** Haiku 4.5 on ~50 tokens lands around 0.4-0.6s against Opus's 3-5s.
`Stage1Config::effort` already documents `None` as the setting for Haiku 4.5,
so the wire supports this today.

Together the model hop stops being interesting: it falls below Google's own
1-3s delivery lag.

**Do not add a third provider for this.** The wire already speaks Anthropic and
OpenAI (`Stage2Provider`), so an experiment is a config line, but a new vendor
means a per-tenant key, Bifrost gateway config for hosted, and a new name in a
privacy policy that was just rewritten because it made false claims about where
mail goes. Groq- and Cerebras-hosted open models genuinely are faster per
token, and they would be shaving ~300ms off a hop already under the noise
floor. The fast lane does not need near-Opus intelligence: it is recall-biased
binary triage with Opus re-deciding right behind it.

If Haiku's recall disappoints on the eval corpus, the ladder is **Sonnet 5**
next. Same vendor, same plumbing, still meaningfully faster than Opus, no
policy change.

Useful prior: 448 rows in the live DB carry `stage1_model_used =
claude-haiku-4-5` from before the Opus swap, and `squelch-eval` exists. Measure
the disagreement rate before shipping rather than arguing about it.

## 8. Ordering of work

1. **Loop split** plus the ingest-time eligibility stamp (§5). Depends on no
   open question, carries no quality risk, and by itself fixes the 24.7% drop
   rate and the whole tail.
2. **Fast notify lane** (§3, §4, §7) plus sealed notifications (§6).
3. **Gmail push** (`users.watch` + Pub/Sub), which buys the remaining ~2.5s of
   poll wait. Deferred and written up separately: a hosted tenant can be reached
   by control receiving the Pub/Sub push, but a self-hosted daemon has no public
   endpoint, so self-host needs either a Pub/Sub streaming pull or to keep
   polling. That fork is the whole design and it is worth less than items 1 and 2.

## 9. Open questions

- Does a rescue have a time ceiling, or is any late buzz better than none?
- May the fast lane skip the model entirely on a confident heuristic seed
  (bills, known contacts)? Under the new error profile a wrong buzz is
  survivable, so the path that was disabled for being "a regex whose verdict
  nothing could retract" becomes viable again. Sealed mail is already the
  killer case for it.
- Does `notify_importance` get stored, or just a boolean? Recommendation:
  store the score. It costs nothing and it is the only way the threshold ever
  gets tuned from real data.
- Should a rescued notification be able to *update* the text of a buzz the
  fast lane already sent? Currently impossible (`UNIQUE(message_id)`), and
  probably fine, but worth deciding rather than inheriting.

## 10. Do not break

- Seal detection stays the first thing that touches a parsed body. The fast
  lane runs *after* it and never sees a sealed body.
- A sealed event row carries `sealed_kind`-derived text only. No subject, no
  body, no code. `events` is not gated on `sensitivity` and must not need to be.
- The fast lane may only add a buzz. Nothing it does may mark a row in a way
  that stops Stage-1 from emitting its own.
- `suppressed` is not `declined_by_model`. Only the latter is rescuable.
- The decline ledger is append-only.

## 11. Locked contract

Decided 2026-09-01. The four open questions in §9, answered, then the exact
shape of every piece. Names here are the names in the code.

### 11.1 The four calls

1. **A rescue has a ceiling: `notify.rescue_window_secs`, default 3600,**
   measured from the moment we first saw the message (§11.3), never from the
   sender's `Date:`. A late verdict inside the hour still buzzes; past it the
   drop is *counted* (`expired` in the ledger and a metric), never silent.
   One hour is the horizon past which "you probably have not seen this" stops
   being true, and it is also what keeps a multi-hour gateway outage from
   turning into a storm the moment it ends.
2. **The fast lane does not skip the model on a heuristic seed.** Two
   deterministic paths exist and only two: sealed mail (§11.6) and a standing
   Squelch/Filtered rule (`suppressed`). Everything else asks the notify model
   when one is configured. With NO model configured the confident seed decides,
   exactly as the ingest site does today, recorded `model_used='heuristic'`.
   The reason is the ledger: a heuristic buzz is not a labeled example of
   anything, and the whole value of §4 is that every row is one. The
   known-contact floor (`stage1.known_contact_importance`) is applied to the
   model's `notify_importance` the way Stage-1 applies it to `importance`, so
   the [[known-contact guarantee]] holds in both lanes without a skip.
3. **The score is stored.** `notify_importance` lands in the ledger row and is
   the event row's `importance`.
4. **A sent buzz is never rewritten.** `UNIQUE(message_id)` on `events`
   stands. When the deliberate lane would have buzzed a message the fast lane
   already did, the ledger records `would_send` and nothing else happens.

### 11.2 Two loops, not one

`SyncEngine::poll_loop` becomes two futures run under one `tokio::select!`
over `&self` (no spawning, no `'static`, no new Arcs), each with its own
shutdown watch:

```
poll_lane:   refresh_inbox_unread -> reminder_pass -> poll_once
             -> stamp_sync_success -> [wake refine_lane if anything ingested]
             -> sleep(poll_secs) | refresh | shutdown
refine_lane: stage1_pass -> stage2_pass -> extract_pass -> revisit_pass
             -> backfill_missing_vectors
             -> wait: refine_wake.notified() | sleep(poll_secs) | shutdown
```

- The poll lane's `Err` (Gmail auth/quota) ends the select and bubbles to
  `run()`'s backoff exactly as today; the refine lane is cancelled at its next
  await, which leaves the in-flight row queued (`stage1_model_used` NULL), the
  same state a crash leaves it in.
- `refine_wake` is a `tokio::sync::Notify` held by the engine, poked once per
  poll tick that ingested `> 0` rows. It coalesces like `refresh` does.
- `stamp_sync_success` stays in the poll lane. Sync freshness measures polling,
  not refinement, which is what the staleness alert was always meant to mean.
- `run_once`'s first-run sequence (backfill, then the three passes inline,
  then `backfill_missing_vectors`) is unchanged.
- The fast lane (§11.5) is spawned from inside `fetch_raw_and_ingest`, so it
  lives in neither loop and blocks neither.

### 11.3 Eligibility is a stamp, written once

A new column, added in `migrate.rs` with `add_column_if_missing` beside the
other column migrations (columns need the seam; tables do not, see §11.4),
and declared in `schema.sql`'s `triage` too so a fresh DB has it:

```
triage.notify_eligible_at  TEXT NULL   -- RFC3339 UTC, or NULL = never notifiable
```

Not backfilled: every pre-existing row is NULL, which is the silent direction.

Written by `ingest_message` on the triage row's **first insert only** and
preserved verbatim on conflict (`notify_eligible_at = triage.notify_eligible_at`
in the `DO UPDATE SET`). Computed by the engine before the store call:

```
TriagedMessage.notify_eligible_at = Some(now)  iff
    origin == IngestOrigin::Incremental
    && !message.is_sent
    && events::is_fresh(message.received_at, &config.notify, now)
otherwise None
```

`is_fresh` keeps its sender-controlled-date bounds (window floor and the
one-hour future skew ceiling) and keeps `notify.freshness_window_secs` as its
knob; the difference is that it now runs ONCE, at first sight, instead of at
every emission site. A catch-up re-scan therefore cannot storm (rows it
re-ingests already exist and keep their stamp, NULL included), a catch-up
after a week-long outage buzzes only for mail that was fresh when we finally
saw it, and a backfill row is NULL forever.

`EventContext` loses `received_at` and gains `notify_eligible_at:
Option<DateTime<Utc>>`. `worthy_kind` replaces the `is_fresh` check with:

```
let Some(at) = ctx.notify_eligible_at else { return None };     // never eligible
if now - at > rescue_window { return None (caller records `expired`) };
```

`Stage1Queued`, `Stage2Queued` and `SeedVerdict`/`triage_seed_verdict` carry
the stamp so every emission site has it. `ingest_context` reads it off the
`TriagedMessage`.

### 11.4 The ledger

A new table, in `schema.sql` (the whole file is `CREATE ... IF NOT EXISTS`
and runs on every open, so a new TABLE needs no migration seam; `events` and
`triage_revisits` are the precedents, comment block and all):

```sql
CREATE TABLE IF NOT EXISTS notify_decisions (
    id                INTEGER PRIMARY KEY,
    account_id        INTEGER NOT NULL,
    message_id        INTEGER NOT NULL,
    lane              TEXT NOT NULL,      -- 'fast' | 'deliberate'
    decision          TEXT NOT NULL,      -- see below
    notify_importance INTEGER,            -- 0-100, NULL when no model scored it
    model_used        TEXT,               -- model id | 'sealed' | 'heuristic' | NULL
    latency_ms        INTEGER,            -- first sight -> this decision, fast lane only
    created_at        TEXT NOT NULL,
    UNIQUE(message_id, lane)
);
CREATE INDEX IF NOT EXISTS idx_notify_decisions_account_created
    ON notify_decisions(account_id, created_at);
```

Written with `INSERT OR IGNORE`. **Never UPDATEd, never DELETEd.** One row per
(message, lane). A row is written **only for messages with a
`notify_eligible_at` stamp**: backfill and stale mail generate no rows at all,
which is what keeps the table from being 95% noise.

`decision` values, one closed set for both lanes:

| decision | meaning | rescuable |
|---|---|---|
| `sent` | this lane appended the `events` row | n/a |
| `would_send` | verdict was buzz-worthy, an event already existed (the other lane sent) | n/a |
| `declined_by_model` | a model scored it and it fell below the line | **yes** |
| `unavailable` | no model answer: no key, timeout, budget, transport, config failure | **yes** |
| `suppressed` | a structural gate silenced an eligible message (Squelch/Filtered rule) | no |
| `expired` | eligible, but past `rescue_window_secs` when this lane got to it | no |

The cross-lane facts are joins, not columns: *rescued* = `deliberate/sent` on
a message whose fast row is `declined_by_model` or `unavailable`; *overturned*
= `deliberate/declined_by_model` on a message whose fast row is `sent`;
*confirmed* = `deliberate/would_send` on a `fast/sent`. Each lane records only
what IT decided, so neither can be wrong about the other.

The ledger carries **no email-derived text**. Not the one_line, not the
subject, nothing. It cannot leak by construction, which is why it needs no
sensitivity gate.

**Store API** (`Store` trait + `SqliteStore`):
`record_notify_decision(&NewNotifyDecision) -> Result<bool>` (false = ignored
duplicate), and one read for the eval query,
`notify_decisions_since(account_id, since, limit) -> Vec<NotifyDecision>`.

### 11.5 The fast lane

New module `squelch-core/src/sync/notify_lane.rs`. `NotifyLane` is an
`Arc`-held struct built once by `SyncEngine::new` from clones of what it needs
(store `Arc`, the reqwest client, `NotifyConfig`, `stage1.known_contact_importance`,
the resolved LLM if any, `SyncMetrics`, `account_id`) plus a
`tokio::sync::Semaphore(notify.fast_concurrency)` and a
`Mutex<Option<Instant>>` "disabled until". `ResolvedLlm` is not `Clone`
(deliberately no `Debug` either, it holds the key); give it a manual `Clone`
impl, and nothing else. The gateway needs the model id qualified exactly as
`squelch-api/src/assistant.rs` does for the assistant:
`if llm::is_gateway_url(url) { llm::qualify_gateway_model(&cfg.model) }`, and
that qualified id is what `model_used` records.

**Entry.** `fetch_raw_and_ingest`, right after `ingest_one` commits and BEFORE
`embed_and_store`, calls `NotifyLane::candidate(&triaged, id, rules)` and, for
`Some`, `tokio::spawn(lane.clone().run(candidate))`. The poll loop never
awaits it. Tests call `run` directly.

**`candidate` is pure and is the whole gate**, in this order:

```
is_sent                                -> None            (not a candidate)
notify_eligible_at.is_none()           -> None            (backfill / stale / re-ingest)
sensitivity != Normal                  -> Candidate::Sealed { message_id, thread_id,
                                          sender, sealed_kind, eligible_at }
rule in {Squelch, Filtered}            -> Candidate::Suppressed { message_id, eligible_at }
otherwise                              -> Candidate::Model { message_id, thread_id, sender,
                                          subject, body, is_known_contact,
                                          seed: (tier, deadline: Option<DateTime>),
                                          eligible_at }
```

`Candidate::Sealed` has no `subject` and no `body` field. That is the
type-level enforcement of §10's first bullet: the sealed path cannot read
what it does not have.

**`run`** per variant:

- `Sealed` → `events::sealed_event(..)` (§11.6) → `append_event` → ledger
  `sent` / `would_send` with `model_used='sealed'`, `notify_importance` = the
  kind's fixed importance.
- `Suppressed` → ledger `suppressed`. No event.
- `Model` →
  1. No resolved LLM: build `EventContext` from the SEED (tier, importance,
     one_line, deadline) and emit via `worthy_kind` iff the seed was
     confident; ledger `sent`/`would_send`/`declined_by_model` with
     `model_used='heuristic'`, or `unavailable` for a non-confident seed.
  2. Lane disabled-until in the future → `unavailable`.
  3. Daily budget: the same check-then-increment `gate_budget` does, on
     `wake_budget` key `__notify_fast__` against `notify.daily_cap` (config
     only, no `app_settings` override, like `revisit.daily_cap`), with a
     `CapKind::NotifyFast` (plus its `WarnDays` field) so the exhausted
     notice logs once a day. Exhausted → `unavailable`.
  4. Acquire the semaphore permit; call `notify_llm::classify_at` wrapped in
     `tokio::time::timeout(notify.fast_timeout_secs)`. **Single attempt, no
     retry loop, no backoff:** a 429 or a 5xx is `unavailable` immediately.
     The deliberate lane is the retry. A config-level failure
     (`llm::is_config_failure`) additionally sets disabled-until = now + 10
     min and refunds the charge; log once per day via `warn_once_per_day`.
  5. On a verdict: bill usage to the ledger category `notify` via
     `extract_bump_usage`. That category is NOT priced at Stage-1 rates the
     way every extractor is: `/client/usage` (`handlers.rs::get_usage`) and
     `metrics.rs::estimate_cost_usd` both grow a third arm that prices
     `notify` at `notify.price_*` against `notify.model`, or the ledger
     would bill Haiku calls at Opus prices and lie by 25x.
     `importance = max(notify_importance,
     known_contact floor if is_known_contact)`; build an `EventContext` with
     `tier = seed.tier`, `deadline = seed.deadline`, the model's `one_line`
     (through `truncate_one_line`), the LIVE rule (`current_rule`), and run
     `worthy_kind`. `Some(kind)` → `append_event` → `sent`/`would_send`.
     `None` → `declined_by_model`. Refusal → `unavailable`.
  6. Record `latency_ms = now - eligible_at` on every fast-lane row and
     observe `squelch_notify_fast_seconds` on `sent` only.

The seed's tier and deadline decide the event's *kind* through the unchanged
`worthy_kind` precedence (urgent > deadline > surfaced); the model decides
only *importance* and *one_line*. Structure answers "what shape", the model
answers "how much and what to say".

**`notify_llm`** (`squelch-core/src/triage/notify_llm.rs`): its own static
system prompt (recall-biased, Stage-1's importance anchors verbatim, the
one_line rules verbatim including the no-em-dash rule, and
`stage1_llm::TRUST_RULE` appended LAST), reusing `stage2::build_user_message`
over a `RowContext` with `max_body_chars = notify.max_body_chars` so the fence
is the one every other prompt uses. Schema:

```json
{ "type": "object", "additionalProperties": false,
  "required": ["notify_importance", "one_line"],
  "properties": { "notify_importance": { "type": "integer" },
                  "one_line": { "type": "string" } } }
```

Nothing else. No reason fields, no confidence, no category. `effort` from
`notify.effort` (default `None`, which is what Haiku 4.5 requires).

**`NotifyConfig`** gains, each with a `SQUELCH_NOTIFY_*` env override in
`Config::apply_env_overrides` next to `SQUELCH_NOTIFY_MIN_IMPORTANCE`
(`env_override_effort` for `effort`):

| field | default | note |
|---|---|---|
| `rescue_window_secs: u64` | 3600 | §11.1 |
| `fast_enabled: bool` | true | kill switch for the model path; sealed and suppressed still record |
| `sealed_enabled: bool` | **false** for now | §11.6: flips once the paired app ships; off = sealed rows record nothing and emit nothing |
| `model: String` | `claude-haiku-4-5` | qualified for a gateway at call time |
| `effort: Option<String>` | `None` | |
| `max_body_chars: usize` | 1500 | |
| `fast_timeout_secs: u64` | 8 | hard cap on one call, retries included (there are none) |
| `fast_concurrency: usize` | 4 | semaphore permits |
| `daily_cap: u32` | 1000 | `wake_budget` key `__notify_fast__` |
| `price_in_per_mtok: f64` | 1.0 | Haiku 4.5 |
| `price_out_per_mtok: f64` | 5.0 | |

`freshness_window_secs` gets the env override it never had.

### 11.6 Sealed notifications

`events::sealed_event(account_id, message_id, thread_id, sender, kind) ->
NewEvent`, a pure table with no other inputs:

| `sealed_kind` | `one_line` | `importance` | `kind` |
|---|---|---|---|
| `otp` | `Login code arrived` | 90 | urgent |
| `magic_link` | `Sign-in link arrived` | 90 | urgent |
| `verification` | `Verification email arrived` | 80 | urgent |
| `password_reset` | `Password reset requested` | 90 | urgent |
| `login_alert` | `New sign-in alert` | 85 | urgent |

`tier = signal`, `deadline = NULL`, `sender = from_addr` (metadata, the same
class `/client/sealed` already serves). All five kinds are `urgent` because
all five want a time-sensitive interruption: the requested ones because the
code expires, the login alert because someone may be past the password right
now. The "different shape" §6 wanted is the wording.

`events` gains `sealed_kind TEXT NULL` (`add_column_if_missing` in
migrate.rs, and the column in `schema.sql`), `NewEvent` and `Event`
gain `sealed_kind: Option<SealedKind>` (serialized `#[serde(default,
skip_serializing_if = "Option::is_none")]`, so the wire is unchanged for every
existing row and every existing client). It exists so a client can route the
tap to the sealed reveal flow instead of a thread fetch that
`thread_guard_and_subject` 404s, and pick an icon. It is NOT a gate: no query
reads it to decide what to serve, and none may.

`correct_triage`'s redaction of an event when a human seals a message
afterwards is unchanged and still applies.

**What the client already does, which §6 did not know.** Sealed mail is not
silent on the Mac today: the app polls `/client/sealed` (the live account
through `SitrepPoller` into `AuthArrival`, every other account through
`BackgroundAuthWatch` every 30s), dedups on a per-account set of sealed
*message ids* (`AuthSeenSet`, two-minute fresh window), rings, auto-reveals
`otp`/`verification` through the audited reveal endpoint, and shows the code
in a modal; background accounts get a banner titled
`AuthCopy.label(kind) · account` whose tap opens the Auth list. It is silent
on iOS, which cannot poll in the background, and it is up to 30s late on the
Mac. That polling path is the fallback and stays. The event is the fast
signal for it, and the client must treat it as one:

- SSE (`EventStream.consume`): an `Event` with `sealed_kind != nil` is NOT an
  event banner and NOT a `noteLiveEvent` thread arrival. It is handed to the
  auth path keyed by `message_id`, so `AuthSeenSet` dedups it against the
  poll: live account → the same `AuthArrival` ring/auto-reveal, background →
  `postAuth`.
- iOS NSE: `sealed_kind != nil` renders the auth-shaped banner (title
  `AuthCopy.label(kind) · account`, body `from <sender display name>`), sound
  on, `passband.route = authRoute`, so the tap lands on the Auth list rather
  than a thread fetch.
- The `Event` decoder gains `sealed_kind: SealedKind?`; `SealedKind` is
  already total-by-construction (`.unknown(raw)`), and every other field stays
  exactly as it is: the SSE consumer ADVANCES THE CURSOR on an undecodable
  frame, so a required field removed on the daemon is a notification lost
  forever.
- No new string may flow into `Analytics` from any of this; the vocabulary is
  closed and `otp`/`login_alert`/`urgent`/`surfaced` are not in it.

Until an app carrying that lands, a sealed event on an old Mac app is a second
banner with a dead tap. So **`notify.sealed_enabled` defaults to `false` in
the PR that builds this**, and flips to `true` in the release that pairs the
daemon with that app. The daemon side is complete either way; the knob is an
ordering guard, not a design hedge.

### 11.7 The deliberate lane's ledger writes

`emit_event` returns an enum instead of `()`:

```
enum Emitted { New(i64), AlreadyNotified, NotWorthy, Expired }
```

(`Expired` when the only reason `worthy_kind` refused was the rescue window;
`worthy_kind` grows a sibling `worthy_kind_or_expired` or returns a
`Refusal` enum, builder's choice, so the two are distinguishable without
re-deriving them at the call site.) Each refine-site caller (Stage-1 apply
`Ok(true)`, the seed fallback in `emit_seed_event`, Stage-2 apply `Ok(true)`)
then records `deliberate/{sent|would_send|declined_by_model|expired}` with
`model_used` = the stage's model id (or `heuristic-only` for the fallback)
and `notify_importance` = the applied importance, **iff the row carries a
`notify_eligible_at`**. Squelch/Filtered → `suppressed`. `INSERT OR IGNORE`
means a re-triage or a Stage-2 pass after Stage-1 already wrote the row is a
no-op, which is the append-only rule doing its job.

### 11.8 Metrics

In `SyncMetrics`, following `record_stage1`'s shape (a closed enum per
label, one `AtomicU64` per combination, every series rendered even at zero,
`squelchd_` prefix, `_total` on counters):

- `squelchd_notify_decisions_total{lane, decision}` counter: 2 lanes x 6
  decisions, all twelve series always present. `record_notify(NotifyLane,
  NotifyDecision)`.
- `squelchd_notify_fast_seconds` histogram, first sight to event row, fast
  lane `sent` only. Buckets: 0.5, 1, 2, 4, 8, 16, 32, 60. `render_http` is
  the hand-rolled histogram precedent.
- There is no separate expired family: it is
  `decisions_total{decision="expired"}`.

### 11.9 Ordering of the build

- **Wave 1: §11.2 + §11.3.** Loop split, eligibility stamp, `EventContext`
  change, `rescue_window_secs`, and the §11.8 decisions counter family in
  full (both enums, all twelve series) with only `deliberate/expired`
  recorded yet, so the drop that used to be silent is visible on `/metrics`
  from this wave on. Independent of every other section; fixes §2a and the
  tail by itself.
- **Wave 2a: plumbing.** §11.4 table + store API, §11.5's `NotifyConfig`
  fields and `notify_llm` module, §11.6's `sealed_event` + `events.sealed_kind`
  + `Event.sealed_kind`, `Emitted`. Everything compiles and is unit-tested;
  nothing is wired into the engine yet.
- **Wave 2b: wiring.** `NotifyLane`, the spawn in `fetch_raw_and_ingest`, the
  deliberate-lane ledger writes at the three refine sites, metrics, the
  `docs/SECURITY.md` §4 "Do not break" additions, this doc's §10 kept true.
- **Wave 3 (Swift, independent of 2b):** the client half of §11.6. Runs
  beside Wave 2 because it touches only `passband/`.

### 11.10 Tests, and where the harnesses are

- Engine tests are the in-file `mod tests` at the bottom of `sync/mod.rs`.
  Gmail is mocked by `serve_mock` (an axum router on a loopback port, fed in
  through `with_api_base`). The LLM is mocked one layer down by `mock_once` /
  `mock_seq` in the classifier modules (`stage1_llm.rs`, `stage2.rs`), which
  is how `notify_llm::classify_at` gets tested too.
- Nothing today drives a pass against a mock LLM through the engine; the
  emission sites are tested by hand-written mirrors (`ingest_and_notify`,
  `refine_row_and_notify`). Wave 1 changes that for one property, because
  it is the property the whole issue is about: **a message arriving while
  the refine lane is inside a model call must still be ingested.** The
  deterministic shape: `Config` with `stage2.anthropic_api_key` set and
  `anthropic_base_url` pointed at a loopback mock (plain http is allowed for
  loopback by design) that HOLDS its response on a `oneshot` the test
  controls; ingest message A on tick 1 so the refine lane parks in the
  mock; serve message B from the Gmail mock and poke `refresh`; poll the
  store until B's row exists (bounded wait); only then release the oneshot.
  No sleeps as assertions.
- The fast lane is tested by calling `NotifyLane::run` directly on a
  constructed `Candidate`, against `mock_once`: importance 80 → `fast/sent`
  + an event; 20 → `declined_by_model`; a 500 → `unavailable` after EXACTLY
  ONE request (assert the mock saw one); a mock that never answers →
  `unavailable` inside `fast_timeout_secs`; no LLM → confident seed emits
  with `model_used='heuristic'`.
- Sealed: feed a subject and body containing a six-digit code; assert the
  event row's `one_line`, `sender`, and the ledger row contain neither the
  code nor a word of the subject, and that `Candidate::Sealed` cannot be
  constructed with a body (it has no field for one).
- Ledger: a second insert for the same (message, lane) is ignored and the
  first row is byte-identical afterwards; backfill and stale rows produce
  no ledger row at all; the four cross-lane joins in §11.4 come out right
  on a fixture that exercises each.
- Migration tests live in `store/sqlite/tests/migrate.rs`: build the old
  partial schema, `migrate` twice (idempotent), assert `PRAGMA table_info`,
  and for the table go through `SqliteStore::init`.
- CI is exactly `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`, on the pinned
  1.94 toolchain. On this Mac prefix cargo with
  `DEVELOPER_DIR=/Library/Developer/CommandLineTools`.

### 11.11 Rollout, in order

1. Merge. The author's self-hosted daemon runs it first.
2. After a few days: `SELECT lane, decision, count(*) FROM notify_decisions
   GROUP BY 1,2` and the rescued/overturned joins from §11.4. That number,
   not an argument, decides whether the threshold or the model moves.
3. Hosted: verify on the LIVE gateway (not the docs) that the provider key's
   `models` and every tenant VK's `allowed_models` carry
   `anthropic/claude-haiku-4-5`. `squelch-control`'s `DEFAULT_LLM_MODELS`
   already lists it in both spellings, so this is `llm sync` plus a per-tenant
   `llm mint` only where a VK predates that list. Then the daemon roll.
4. The fast lane costs roughly a thousandth of a dollar per message on Haiku
   against Stage-1's few cents on Opus: it is inside the noise of the $5/month
   tenant VK budget, but it does draw from the same pool.
