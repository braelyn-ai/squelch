# Notification latency and the notify lane

Design notes for issue #177, "make faster". The complaint that opened it: mail
notifications arrive later than competitors', and a user who gets buzzed by
Superhuman first opens Superhuman.

This document is the record of what was measured, what the measurement turned
up that nobody was looking for, and the design those findings point at. It is
notes and decisions, not a locked spec.

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

A user who writes "always surface my broker" gets silence.

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

Today it never does. `triage/events.rs` justifies that as "even a contentless
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
- `ineligible` is not `declined_by_model`. Only the latter is rescuable.
- The decline ledger is append-only.
