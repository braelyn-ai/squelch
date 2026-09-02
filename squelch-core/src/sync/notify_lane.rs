//! THE FAST LANE (docs/NOTIFY.md §11.5 and §11.6): the notification decision
//! made at INGEST, in front of a user waiting for their phone to buzz, instead
//! of behind whatever the refine lane happens to be grinding.
//!
//! It is not a triage pass and must never become one. Ingest hands it one
//! message; it answers "does this deserve to interrupt someone right now",
//! appends the `events` row when the answer is yes, and writes one
//! `notify_decisions` row whatever the answer was. Tier, deadline, category and
//! revisit schedule stay the deliberate lane's business, and that lane runs
//! behind this one on the capable model and can still rescue a buzz this one
//! declined. Structure answers "what shape"; the notify model answers "how much,
//! and what to say".
//!
//! THE LANE IS SPAWNED, NEVER AWAITED. [`super::SyncEngine::fetch_raw_and_ingest`]
//! builds a [`Candidate`] and `tokio::spawn`s [`NotifyLane::run`] on it, so a
//! model call that takes eight seconds costs the poll loop nothing. Everything
//! the task needs is OWNED (`String`s, not borrows of the batch's `TriagedMessage`)
//! for exactly that reason.
//!
//! THE SEAL INVARIANT IS ENFORCED BY TYPE HERE, not by a check somebody has to
//! remember. A sealed message becomes [`Candidate::Sealed`], which has no
//! `subject` field and no `body` field, and the only function it can reach is
//! [`events::sealed_event`], whose signature has no parameter for either. A
//! sealed body therefore cannot reach a model or an `events` row through this
//! module without first adding a field to an enum and a parameter to a pure
//! table, in two files, on purpose (docs/SECURITY.md §4).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::{BudgetGate, BudgetLedger, CapKind, NOTIFY_FAST_BUDGET_KEY, NOTIFY_USAGE_CATEGORY};
use crate::config::{NotifyConfig, ResolvedLlm, Stage2Provider};
use crate::metrics::{NotifyDecision, NotifyLane as LaneLabel, SyncMetrics};
use crate::store::{NewNotifyDecision, Store, TriagedMessage};
use crate::triage::DeadlineHit;
use crate::triage::events::{self, EventContext, Refusal};
use crate::triage::llm::{self, LlmOutcome};
use crate::triage::notify_llm::{self, NotifyInput};
use crate::triage::stage2::truncate_one_line;
use crate::types::{AccountId, Disposition, SealedKind, SenderRule, Sensitivity, Tier};

/// How long the lane stops asking after a CONFIG-LEVEL rejection (bad
/// credential, disallowed model, spent gateway budget). Ten minutes because a
/// config failure is shared by every message rather than a verdict about this
/// one: without it a mailbox taking mail at any rate would fire one doomed
/// request per message, in front of a user who is getting no notifications
/// either way, and the ingest path would carry the latency of a 4xx per row for
/// as long as the outage lasted. The deliberate lane keeps emitting throughout,
/// so the cost of being wrong about the duration is latency, never a
/// notification.
const DISABLE_AFTER_CONFIG_FAILURE: Duration = Duration::from_secs(600);

/// The heuristic seed a [`Candidate::Model`] carries: what ingest already
/// decided about the message, which is what the lane falls back to when there is
/// no model to ask, and what supplies the event's SHAPE even when there is one.
///
/// The model is asked exactly two things (`notify_importance` and `one_line`)
/// and the tier and deadline below are what turn them into an event KIND through
/// the unchanged [`events::worthy_kind`] precedence. That split is the design:
/// structure answers "what shape", the model answers "how much and what to say".
#[derive(Clone)]
pub struct Seed {
    pub tier: Tier,
    /// The seed's own score, already carrying Stage-1's known-contact floor.
    /// Used only on the no-model path; a model verdict replaces it.
    pub importance: u8,
    /// The seed's line, likewise used only on the no-model path.
    pub one_line: String,
    /// THE WHOLE HIT, not just its `due_at`. [`events::event_for`] stores the
    /// date and [`events::worthy_kind`] reads only `is_some()`, so a bare
    /// `DateTime` would do — and would mean reconstituting a `DeadlineHit` with
    /// invented `kind`/`source` strings at the one site that already holds the
    /// real one.
    pub deadline: Option<DeadlineHit>,
    /// `TriagedMessage::confident`. THE GATE ON THE NO-MODEL PATH: a guess is
    /// not grounds for waking anyone, and with no model configured nothing will
    /// ever refine it (docs/NOTIFY.md §11.5, `Model` step 1).
    pub confident: bool,
}

/// One message the fast lane may have something to say about, in the three
/// shapes it can take. Produced by [`candidate`], which is the whole gate;
/// consumed by [`NotifyLane::run`].
///
/// OWNED, not borrowed: the value crosses a `tokio::spawn` boundary, and the
/// `TriagedMessage` it came from belongs to the ingest batch.
///
/// NO `Debug`, for the reason [`crate::config::ResolvedLlm`] has none: the
/// `Model` variant carries a subject and a body, and a derive is where somebody
/// adding a field later reaches for the usual attribute. A `{c:?}` anywhere in
/// this crate's logging would put an email body on stderr, which is the one
/// thing docs/SECURITY.md forbids by name.
#[derive(Clone)]
pub enum Candidate {
    /// A sealed message (docs/NOTIFY.md §11.6). THERE IS NO `subject` FIELD AND
    /// NO `body` FIELD, and that absence is the seal invariant's enforcement on
    /// this path rather than a coincidence of what the ping happens to need:
    /// [`events::sealed_event`] derives every word of the notification from
    /// `kind` alone, and this variant cannot hand it anything else because it is
    /// not carrying anything else. `thread_id` and `sender` are metadata of
    /// exactly the class `/client/sealed` already serves to an authenticated
    /// client. ADDING A BODY FIELD HERE WOULD BE THE SEAL UNDONE ON A LOCK
    /// SCREEN.
    Sealed {
        message_id: i64,
        thread_id: String,
        sender: String,
        kind: SealedKind,
    },
    /// A standing Squelch/Filtered rule already answered this. Deterministic, so
    /// no model is asked; the row is recorded rather than dropped, because
    /// "silenced on purpose" and "we never got to it" are different facts about
    /// the same silence and the ledger exists to tell them apart.
    Suppressed { message_id: i64 },
    /// Ordinary mail: ask the notify model, or fall back to the seed when there
    /// is none.
    Model {
        message_id: i64,
        thread_id: String,
        sender: String,
        subject: String,
        /// ALREADY CUT TO WHAT THE PROMPT CAN USE (see [`candidate`]), not the
        /// flattened body ingest holds.
        body: String,
        /// Someone the user has written to. Read at ingest through the same
        /// lookup Stage-1's floor uses, so the [[known-contact guarantee]] holds
        /// in this lane too: it goes into the prompt's TRUSTED CONTEXT block and
        /// is the condition on the floor applied to the model's score.
        is_known_contact: bool,
        seed: Seed,
    },
}

impl Candidate {
    /// The `messages.id` every variant carries, for the gates that are the same
    /// question whatever shape the candidate took.
    fn message_id(&self) -> i64 {
        match self {
            Candidate::Sealed { message_id, .. }
            | Candidate::Suppressed { message_id, .. }
            | Candidate::Model { message_id, .. } => *message_id,
        }
    }
}

// NO VARIANT CARRIES AN `eligible_at`, and that is the one place this module
// departs from docs/NOTIFY.md §11.5's field list.
//
// `candidate` still GATES on the stamp — no stamp, no candidate — but the value
// it gated on is the one INGEST COMPUTED, and on a `catch_up()` re-ingest that
// is not the value the database holds (`ingest_message` writes
// `notify_eligible_at` on first insert only and preserves the stored one on
// conflict). The difference is not cosmetic: the stamp is what the rescue window
// and every latency number are measured FROM, and a BACKFILLED row — stamped
// NULL, silent forever — whose `Date:` still falls inside the freshness window
// when a catch-up re-ingests it computes a `Some` the database never accepted.
// Buzzing off that would push a month of archived mail the backfill deliberately
// silenced.
//
// So [`NotifyLane::run`] reads the STORED stamp once, up front, for all three
// variants, through [`Store::notify_eligible_at`] — the point read that is not
// gated on `sensitivity`, because `triage_seed_verdict` selects `sensitivity =
// 'normal'` and cannot see a sealed row at all. A field nothing is allowed to
// read is worse than no field.

/// THE GATE, and all of it (docs/NOTIFY.md §11.5). Pure: no store, no clock, no
/// model. `None` means this message is not the fast lane's business at all and
/// NOTHING is recorded about it — which is what keeps `notify_decisions` from
/// being 95% backfill.
///
/// The order below is the contract's order and each arm is load bearing:
///
/// - `is_sent`: the user's own outbox never notifies the user.
/// - no stamp: backfill, a sent copy, or mail already stale the first time we
///   saw it (docs/NOTIFY.md §11.3). NULL is forever and NULL is silent.
/// - `is_spam`: the provider already sorted this. Redundant with the stamp
///   today only by accident — spam rows are ingested on the incremental path
///   like any other — so it is spelled out here as well as inside
///   [`events::worthy_kind`], the same way the seal check is.
/// - sealed: the kind-derived ping, if the operator has turned it on.
/// - a Squelch/Filtered rule: recorded, never asked about.
/// - everything else: a [`Candidate::Model`].
///
/// `notify.fast_enabled` IS NOT CHECKED HERE, and that is a deliberate move off
/// this function since round 1. It is the kill switch for the MODEL PATH, and
/// [`NotifyLane::run_model`] is where the model path begins: checked here, it
/// would take the SEED fallback down with it (`run_model` step 1, the confident
/// heuristic verdict the ingest site owned before this lane existed), and on a
/// daemon with no LLM configured at all — where the deliberate lane never runs
/// either, because `stage1_pass`/`stage2_pass` need a `pass_setup()` — flipping
/// the switch to shed latency would silence the mailbox outright. The field's
/// own doc promises the opposite in so many words: "turning this off costs
/// latency, never a notification."
///
/// `known_contact` is a CLOSURE rather than a `bool` because it is a store read
/// and only the last arm needs the answer; it is the same shape (and the same
/// lookup) [`crate::sync::ingest::ingest_with_rules`] takes.
pub fn candidate(
    triaged: &TriagedMessage,
    message_id: i64,
    rules: &[SenderRule],
    cfg: &NotifyConfig,
    known_contact: impl FnOnce(&str) -> bool,
) -> Option<Candidate> {
    if triaged.message.is_sent {
        return None;
    }
    // GATE ONLY. The computed stamp answers "could this ever notify"; what the
    // rescue window and the latency are measured from is the STORED one, which
    // only [`NotifyLane::run`] can read (see the block above).
    triaged.notify_eligible_at?;
    if triaged.message.is_spam {
        return None;
    }
    if triaged.sensitivity != Sensitivity::Normal {
        // A sealed row with no kind is not a shape the detector produces, and a
        // ping whose wording came from nowhere is not one this module can build:
        // `sealed_event` reads the kind and nothing else.
        let kind = triaged.sealed_kind?;
        if !cfg.sealed_enabled {
            return None;
        }
        return Some(Candidate::Sealed {
            message_id,
            thread_id: triaged.message.thread_id.clone(),
            sender: triaged.message.from_addr.clone(),
            kind,
        });
    }
    let rule = triaged
        .matched_rule
        .and_then(|id| rules.iter().find(|r| r.id == id))
        .map(|r| r.disposition);
    if matches!(
        rule,
        Some(Disposition::Squelch) | Some(Disposition::Filtered)
    ) {
        return Some(Candidate::Suppressed { message_id });
    }
    Some(Candidate::Model {
        message_id,
        thread_id: triaged.message.thread_id.clone(),
        sender: triaged.message.from_addr.clone(),
        subject: triaged.message.subject.clone(),
        // CUT HERE, NOT AT THE PROMPT. `build_user_message` reads at most
        // `max_body_chars` (1500) of this and throws the rest away, but the
        // candidate is what a spawned task HOLDS while it waits: the timeout
        // bounds the call and the semaphore bounds concurrency, and neither
        // bounds the queue, so an endpoint that hangs rather than erroring
        // costs `fast_timeout_secs` per permit and lets tasks pile up to the
        // daily cap. Retaining a full flattened body per queued task — a size
        // the SENDER picks, and a large HTML marketing mail flattens to tens of
        // KB — is a megabyte-scale idle footprint on a daemon that has been
        // OOM-killed on a 4 GB box, in exchange for bytes no prompt will read.
        //
        // `+ 1` ON PURPOSE: `truncate_flagged` marks the cut with
        // "[body truncated to N chars]" only when what it is handed is LONGER
        // than the cap, so cutting to exactly the cap here would silently drop
        // that marker and tell the model a clipped body was the whole mail.
        // One extra scalar keeps the "there is more" signal intact.
        body: crate::text::truncate_chars(
            &triaged.message.body,
            cfg.max_body_chars.saturating_add(1),
        ),
        is_known_contact: known_contact(&triaged.message.from_addr),
        seed: Seed {
            tier: triaged.tier,
            importance: triaged.importance,
            one_line: triaged.one_line.clone(),
            deadline: triaged.deadline.clone(),
            confident: triaged.confident,
        },
    })
}

/// What one decided message costs the ledger: the score, if a model produced
/// one, and who produced it. Bundled so [`NotifyLane::emit`] takes a readable
/// number of arguments rather than five positional scalars in a row.
struct Verdict<'a> {
    importance: u8,
    one_line: &'a str,
    /// The qualified model id, `"heuristic"`, or `"sealed"`. Stored verbatim in
    /// `notify_decisions.model_used`, which is what lets the eval query in
    /// docs/NOTIFY.md §11.11 split the lane's accuracy by who answered.
    model_used: &'a str,
}

/// `model_used` for a verdict nobody asked a model for.
const HEURISTIC: &str = "heuristic";
/// `model_used` for the kind-derived sealed ping.
const SEALED: &str = "sealed";

/// The fast lane itself: everything one spawned decision needs, resolved once.
///
/// Built lazily by [`super::SyncEngine::notify_lane`] rather than in
/// `SyncEngine::new` — see that method for why — and held as an `Arc` so every
/// spawned task shares ONE semaphore and ONE disabled-until, which is the only
/// way either of them bounds anything.
pub struct NotifyLane<S: Store> {
    store: Arc<S>,
    /// The engine's client, cloned: `reqwest::Client` is an `Arc` inside, so
    /// this shares the connection pool rather than opening a second one.
    http: reqwest::Client,
    cfg: NotifyConfig,
    /// `stage1.known_contact_importance`, applied to the MODEL's score the way
    /// Stage-1 applies it to its own, so the [[known-contact guarantee]] does
    /// not depend on which lane happened to answer first.
    known_contact_floor: u8,
    /// `None` disables the model path: the seed decides, and says so in the
    /// ledger. Never `Debug`-formatted; it holds the API key.
    llm: Option<ResolvedLlm>,
    metrics: Arc<SyncMetrics>,
    account_id: AccountId,
    /// Fast-lane calls in flight. A burst of mail is a burst of spawned tasks,
    /// and without this each one is a socket and a share of the gateway's rate
    /// limit; over the limit they wait rather than fail.
    permits: tokio::sync::Semaphore,
    /// Set on a CONFIG-LEVEL failure; until it passes, the lane records
    /// `unavailable` without issuing a request. `std::sync::Mutex` deliberately:
    /// nothing awaits while it is held, and an async mutex would buy a lock that
    /// can be held across a suspension point, which is the one thing this must
    /// never do.
    disabled_until: std::sync::Mutex<Option<Instant>>,
    /// The message ids this process is deciding RIGHT NOW, and the other half of
    /// the re-entry guard (see [`InFlight`] and [`NotifyLane::run`]). The ledger
    /// probe answers "has this lane ALREADY decided"; this answers "is it
    /// deciding", which no durable row can, because the row is written last.
    in_flight: std::sync::Mutex<std::collections::HashSet<i64>>,
    /// SHARED with the engine, not a second copy: the budget-exhausted notice is
    /// rate-limited to one per UTC day per [`CapKind`], and two `WarnDays` would
    /// make "once a day" mean twice.
    warn_days: Arc<std::sync::Mutex<super::WarnDays>>,
}

impl<S: Store + 'static> NotifyLane<S> {
    /// Resolve the lane from what the engine already resolved. `llm` is the
    /// engine's own [`ResolvedLlm`], cloned, so both lanes speak to the same
    /// endpoint with the same key.
    #[allow(clippy::too_many_arguments)] // one struct's fields, spelled out once
    pub(super) fn new(
        store: Arc<S>,
        http: reqwest::Client,
        mut cfg: NotifyConfig,
        known_contact_floor: u8,
        llm: Option<ResolvedLlm>,
        metrics: Arc<SyncMetrics>,
        account_id: AccountId,
        warn_days: Arc<std::sync::Mutex<super::WarnDays>>,
    ) -> Self {
        // THE PREFIX IS A PROPERTY OF THE ENDPOINT. `squelch-api`'s assistant
        // does exactly this for exactly this reason: a gateway routes on
        // `anthropic/claude-haiku-4-5` and 400s on the bare id, and the direct
        // Anthropic API is the other way round. `unwrap_or` keeps an id we
        // cannot qualify (an unknown vendor) exactly as configured rather than
        // dropping it.
        //
        // WRITTEN BACK INTO `cfg.model`, WHICH IS THE POINT. `notify_llm` reads
        // the model id off this config and puts it on the wire, and the ledger
        // records `self.cfg.model` too, so there is exactly ONE string and the
        // request and the row cannot disagree about which model answered. The
        // earlier shape — a qualified copy in a `model_used` field beside an
        // unqualified `cfg` — was a request 400ing on every gateway deployment
        // under a ledger row naming a model nothing had asked, which is the
        // [[fleet LLM outage]] failure again: a wrong-spelled id, a 400 that
        // reads as config, and a lane that looks configured.
        //
        // AND THE PROVIDER, NOT JUST THE URL, which docs/NOTIFY.md §11.5's
        // one-line prescription of this gate left out. `is_gateway_url` is
        // literally `url != API_URL`, so OpenAI's own endpoint reads as "a
        // gateway": an `OPENAI_API_KEY` daemon would get `anthropic/` stapled
        // onto its notify model, `notify_llm::classify_at` would POST that id to
        // OpenAI, OpenAI would answer 400 for an unknown model, and
        // `is_config_failure` would park the lane ten minutes at a time forever,
        // with no `SQUELCH_NOTIFY_MODEL` value able to fix it (the prefix is
        // applied after the config is read). The prefix is a property of ONE
        // endpoint shape: an Anthropic-wire request through a fronting gateway.
        // Stage-1/Stage-2 never hit this because the warden hands them an
        // already qualified id.
        if matches!(&llm, Some(l) if l.provider == Stage2Provider::Anthropic
                        && llm::is_gateway_url(&l.url))
            && let Some(q) = llm::qualify_gateway_model(&cfg.model)
        {
            cfg.model = q;
        }
        let permits = tokio::sync::Semaphore::new(cfg.fast_concurrency.max(1));
        Self {
            store,
            http,
            cfg,
            known_contact_floor,
            llm,
            metrics,
            account_id,
            permits,
            disabled_until: std::sync::Mutex::new(None),
            in_flight: std::sync::Mutex::new(std::collections::HashSet::new()),
            warn_days,
        }
    }

    /// Decide one candidate and record what was decided. Spawned, never awaited:
    /// every failure inside is logged (id + decision, never content) and
    /// swallowed, because a notification is worth neither a panic nor a stalled
    /// poll loop.
    pub async fn run(self: Arc<Self>, c: Candidate) {
        let message_id = c.message_id();

        // THIS LANE IS ALREADY DECIDING THIS MESSAGE, in another task, right
        // now. THE TWO GUARDS BELOW ARE ONE PAIR and neither is sufficient
        // alone: the ledger probe is a durable read that covers a re-ingest
        // across a restart, and it is a TOCTOU read — the row it looks for is
        // written LAST, after the model call — so two overlapping runs both see
        // `false`. Overlap is not exotic: `history_walk` holds the cursor when
        // the SENT half fails, so the next tick (`sync.poll_secs`, 5s) re-walks
        // the same INBOX ids while an 8-second `fast_timeout_secs` call is still
        // out. Both tasks would pay for a billed call and a `__notify_fast__`
        // unit, and if the two verdicts straddle `min_importance` the loser's
        // row wins the `INSERT OR IGNORE` while the winner's event buzzes the
        // phone: a `declined_by_model` row for a message this lane demonstrably
        // sent, in the corpus docs/NOTIFY.md §11.11's rescued/overturned joins
        // are read off.
        //
        // RAII, so a timeout, an early return or a panic cannot leak the claim
        // and silence that message for the life of the process.
        let Some(_claim) = InFlight::claim(&self.in_flight, message_id) else {
            return;
        };

        // THIS LANE ALREADY ANSWERED, so it does not answer twice. Re-ingest is
        // routine — `catch_up` re-walks the whole window and a failed SENT half
        // holds the cursor so the next tick re-walks the same INBOX ids — and
        // `candidate` gates on the stamp INGEST COMPUTED, which is `Some` again
        // for any message still inside `freshness_window_secs`. Without this the
        // second run pays for a second model call and, when it lands on the
        // other side of the line from the first, buzzes the user while the
        // ledger's `INSERT OR IGNORE` keeps the first answer: a `declined` row
        // for a message this lane demonstrably sent, in the corpus
        // docs/NOTIFY.md §11.11's joins are read off.
        //
        // A store error reads as "not answered" and lets the run proceed, the
        // same direction every other uncertain gate in this module takes: the
        // cost of being wrong is one duplicated call, and `UNIQUE(message_id)`
        // on `events` still means at most one buzz.
        if self
            .store
            .notify_decision_exists(self.account_id, message_id, LaneLabel::Fast)
            .unwrap_or(false)
        {
            return;
        }

        // THE STAMP AS THE STORE KEPT IT, not the one ingest computed, for all
        // three variants and read exactly once (see the block above
        // [`candidate`] for why the two differ and why buzzing off the computed
        // one would push a month of archived mail).
        //
        // A missing row or a store error reads as "no stamp" and so as silence,
        // WITH NO LEDGER ROW: the ledger records only messages that carry a
        // stamp (docs/NOTIFY.md §11.4), and a row we could not confirm carries
        // one is not one.
        let eligible_at = match self.store.notify_eligible_at(self.account_id, message_id) {
            Ok(at) => at,
            Err(e) => {
                eprintln!("squelch: notify stamp re-read failed for message {message_id} ({e})");
                None
            }
        };
        let Some(eligible_at) = eligible_at else {
            return;
        };

        match c {
            Candidate::Sealed {
                message_id,
                thread_id,
                sender,
                kind,
            } => {
                // STILL SEALED? The mirror of the model path's guard, from the
                // other side of the same read, and it protects the same thing:
                // an `events` row `correct_triage` cannot reach.
                //
                // `candidate` is PURE and reads the FRESH heuristic triage, and
                // the seed detector has no idea a person ruled on this message.
                // So a row a user un-sealed ("this is not a login code") is
                // still a `Candidate::Sealed` on the next re-ingest inside
                // `freshness_window_secs`, and without this the lane would mint
                // an Urgent, importance-90 "Login code arrived" ping — pushed
                // to a lock screen and routed to the Auth list, where the
                // message is not — for mail the user has explicitly called
                // ordinary. The stored row is the record of that ruling; the
                // candidate is a snapshot of what a regex thought at ingest.
                //
                // `Unsealed` is therefore the REFUSAL here, exactly inverting
                // the model path's use of the same helper: `triage_seed_verdict`
                // selects `sensitivity = 'normal'`, so a row it can see is a row
                // that is no longer sealed. Recorded `suppressed` rather than
                // dropped, for the reason [`NotifyLane::emit`] records it: the
                // row is what arms the re-entry probe, and a silent return would
                // hand the next tick the same candidate.
                match self.seal_state(message_id) {
                    // "Or gone" cannot be this arm in practice: the
                    // `notify_eligible_at` read above found the triage row and
                    // nothing awaits between there and here, so the row is
                    // still there. Even if it were not, a `sealed_event` names
                    // no content and `append_event` would carry a dead
                    // message id, which is the same thing a deletion mid-ping
                    // already produces.
                    SealState::SealedOrGone => {}
                    SealState::Unsealed => {
                        self.record(
                            message_id,
                            NotifyDecision::Suppressed,
                            None,
                            Some(SEALED),
                            eligible_at,
                            Utc::now(),
                        );
                        return;
                    }
                    // A store error is a fact about the database, not about the
                    // message: nothing emitted and nothing recorded, so the next
                    // re-ingest re-reads.
                    SealState::Unknown => return,
                }
                let ev =
                    events::sealed_event(self.account_id, message_id, &thread_id, &sender, kind);
                let importance = ev.importance;
                let now = Utc::now();
                match self.store.append_event(&ev) {
                    Ok(Some(_)) => self.record(
                        message_id,
                        NotifyDecision::Sent,
                        Some(importance),
                        Some(SEALED),
                        eligible_at,
                        now,
                    ),
                    Ok(None) => self.record(
                        message_id,
                        NotifyDecision::WouldSend,
                        Some(importance),
                        Some(SEALED),
                        eligible_at,
                        now,
                    ),
                    // NO LEDGER ROW ON A STORE ERROR. `sent` means "this lane
                    // appended the events row" and nothing else may be written
                    // in its place: a ledger that guesses is a ledger the
                    // rescued/overturned joins cannot be read off.
                    Err(e) => eprintln!(
                        "squelch: notify fast lane could not append the sealed event for \
                         message {message_id} ({e})"
                    ),
                }
            }
            Candidate::Suppressed { message_id } => self.record(
                message_id,
                NotifyDecision::Suppressed,
                None,
                None,
                eligible_at,
                Utc::now(),
            ),
            // CLONED, not moved: `_claim` borrows `self.in_flight` and must
            // outlive the whole decision, which is the entire point of it.
            Candidate::Model { .. } => self.clone().run_model(c, eligible_at).await,
        }
    }

    /// The model path (docs/NOTIFY.md §11.5, `Model`): seed fallback, then the
    /// disabled gate, the daily cap, a permit, and one attempt inside the
    /// timeout. `eligible_at` is the STORED stamp [`NotifyLane::run`] read.
    async fn run_model(self: Arc<Self>, c: Candidate, eligible_at: DateTime<Utc>) {
        let Candidate::Model {
            message_id,
            thread_id,
            sender,
            subject,
            body,
            is_known_contact,
            seed,
        } = c
        else {
            return;
        };

        let m = ModelRow {
            message_id,
            thread_id,
            sender,
            seed,
            eligible_at,
        };

        // 1. NO MODEL TO ASK. The seed is all there is and it is authoritative,
        //    exactly as the ingest site treated it before this lane existed. A
        //    non-confident seed has nothing coming to refine it either, so it is
        //    `unavailable` rather than a decline: nobody scored this.
        //
        //    THIS ARM IS FOR A DAEMON WITH NO LLM AT ALL, and only that. The
        //    deliberate lane never runs there (`stage1_pass` needs a
        //    `pass_setup()`), so if the seed did not decide, nothing would, and
        //    an operator flipping the kill switch to shed latency would silence
        //    the mailbox. That is why the switch is read BELOW this arm rather
        //    than in `candidate`: with a model configured the seed must never
        //    decide, because a regex verdict nothing can retract is exactly
        //    what the model path replaced (docs/NOTIFY.md §11.1, second call).
        let Some(llm) = self.llm.clone() else {
            if !m.seed.confident {
                self.record(
                    m.message_id,
                    NotifyDecision::Unavailable,
                    None,
                    Some(HEURISTIC),
                    m.eligible_at,
                    Utc::now(),
                );
                return;
            }
            let v = Verdict {
                importance: m.seed.importance,
                one_line: &m.seed.one_line,
                model_used: HEURISTIC,
            };
            self.emit(&m, v, Utc::now());
            return;
        };

        // 2. THE KILL SWITCH, then the config-failure park. Both are "there is a
        //    model and this lane may not ask it": the row records `unavailable`
        //    with no model named (nobody was asked, so nothing scored it), and
        //    the deliberate lane, which runs whenever a model is configured,
        //    owns the buzz. `unavailable` is the rescuable word on purpose: a
        //    later `deliberate/sent` on such a row reads as a rescue, which is
        //    what it is.
        //    See [`DISABLE_AFTER_CONFIG_FAILURE`] for the second gate.
        if !self.cfg.fast_enabled || self.is_disabled() {
            self.record(
                m.message_id,
                NotifyDecision::Unavailable,
                None,
                None,
                m.eligible_at,
                Utc::now(),
            );
            return;
        }

        // 3. THE DAILY CAP, on its own `wake_budget` key and its own warn slot.
        //    Charged BEFORE the call, like every other pass, so a burst cannot
        //    exceed it; refunded only on a config failure, which is the one
        //    class that spends no tokens.
        let day = Utc::now().format("%Y-%m-%d").to_string();
        match self.budget().gate(
            NOTIFY_FAST_BUDGET_KEY,
            &day,
            self.cfg.daily_cap,
            CapKind::NotifyFast,
            "notify fast lane",
            "remaining messages",
        ) {
            BudgetGate::Proceed => {}
            BudgetGate::Exhausted | BudgetGate::SkipRow => {
                self.record(
                    m.message_id,
                    NotifyDecision::Unavailable,
                    None,
                    None,
                    m.eligible_at,
                    Utc::now(),
                );
                return;
            }
        }

        // 4. A PERMIT, then ONE attempt inside the timeout. No retry loop and no
        //    backoff: a 429 or a 5xx is `unavailable` immediately and the
        //    deliberate lane is the retry (docs/NOTIFY.md §11.5).
        let Ok(_permit) = self.permits.acquire().await else {
            self.record(
                m.message_id,
                NotifyDecision::Unavailable,
                None,
                None,
                m.eligible_at,
                Utc::now(),
            );
            return;
        };

        // 4b. SEALED WHILE THIS TASK WAS QUEUED, asked HERE — after the permit,
        //     immediately before the body goes on the wire. The guard in
        //     [`NotifyLane::emit`] protects the `events` row; this one protects
        //     THE PROMPT, and they are not the same window. `tokio::time::timeout`
        //     wraps only the call, never the wait for a permit, so at
        //     `fast_concurrency` 4 a forty-message burst queues the last task
        //     over a minute behind the first and a cap-sized day queues far
        //     longer. The message is committed and visible in the client for all
        //     of that, so a user who spots an auth mail the detector missed and
        //     seals it would otherwise still have its subject and body sent to
        //     the notify model (docs/SECURITY.md §4). One indexed point read on a
        //     path that already does three, and it also stops paying for calls
        //     about mail that was sealed or deleted while queued.
        match self.seal_state(m.message_id) {
            SealState::Unsealed => {}
            SealState::SealedOrGone => {
                // REFUNDED, for the same reason a config failure is: step 3
                // charges BEFORE the call so a burst cannot exceed the cap, and
                // no call was made. Letting a seal eat a `__notify_fast__` unit
                // would spend a day's cap on messages nobody was ever going to
                // be asked about.
                self.budget()
                    .refund(NOTIFY_FAST_BUDGET_KEY, &day, "notify fast lane");
                self.record(
                    m.message_id,
                    NotifyDecision::Suppressed,
                    None,
                    None,
                    m.eligible_at,
                    Utc::now(),
                );
                return;
            }
            // A store error is not a fact about the message, so nothing is
            // recorded and nothing is asked: the next re-ingest re-reads.
            SealState::Unknown => {
                self.budget()
                    .refund(NOTIFY_FAST_BUDGET_KEY, &day, "notify fast lane");
                return;
            }
        }

        let input = NotifyInput {
            from_addr: &m.sender,
            subject: &subject,
            body: &body,
            is_known_contact,
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(self.cfg.fast_timeout_secs),
            notify_llm::classify_at(
                &self.http,
                &llm.url,
                &llm.api_key,
                &self.cfg,
                llm.provider,
                &input,
            ),
        )
        .await;

        let now = Utc::now();
        match outcome {
            Ok(Ok(LlmOutcome::Ok(out, usage))) => {
                if let Some(u) = usage {
                    // ITS OWN LEDGER CATEGORY, priced off `notify.price_*`
                    // rather than the Stage-1 rates every extractor shares —
                    // otherwise a Haiku call bills at Opus prices and the
                    // cheapest pass in the pipeline reads as the most expensive.
                    if let Err(e) = self.store.extract_bump_usage(
                        self.account_id,
                        &day,
                        NOTIFY_USAGE_CATEGORY,
                        u.into(),
                    ) {
                        eprintln!("squelch: notify usage ledger write failed ({e})");
                    }
                }
                // THE KNOWN-CONTACT FLOOR, applied to the model's score the way
                // Stage-1 applies it to its own. It is what lets this lane ask a
                // small model at all without putting the [[known-contact
                // guarantee]] at that model's mercy.
                let scored = out.notify_importance.clamp(0, 100) as u8;
                let importance = if is_known_contact {
                    scored.max(self.known_contact_floor)
                } else {
                    scored
                };
                let one_line = truncate_one_line(&out.one_line);
                self.emit(
                    &m,
                    Verdict {
                        importance,
                        one_line: &one_line,
                        // THE STRING THE REQUEST JUST CARRIED, read from the
                        // same field `notify_llm::classify_at` read (see
                        // [`NotifyLane::new`]): one source, so `model_used` is
                        // a fact about the call rather than a label beside it.
                        model_used: &self.cfg.model,
                    },
                    now,
                );
            }
            // A CONFIG-LEVEL REJECTION is a fact about the deployment, not about
            // this message: park the lane, give the charge back (a 4xx in ~0ms
            // spends no tokens, and letting it eat a daily cap keyed on the UTC
            // day would outlive the outage by hours), and say so once a day.
            Ok(Ok(LlmOutcome::Failed(kind))) if llm::is_config_failure(&kind) => {
                self.disable_for(DISABLE_AFTER_CONFIG_FAILURE);
                self.budget()
                    .refund(NOTIFY_FAST_BUDGET_KEY, &day, "notify fast lane");
                self.metrics.record_llm_config_failure();
                // ITS OWN WARN SLOT, not the daily cap's: see
                // [`CapKind::NotifyFastConfig`]. These two notices are the only
                // two diagnoses for a lane that stopped notifying, and a shared
                // slot means the first one of the day silences the other.
                if self.budget().warn_once(CapKind::NotifyFastConfig, &day) {
                    eprintln!(
                        "squelch: notify fast lane config-level failure ({kind}); pausing the \
                         lane for 10 minutes (the triage passes still notify)"
                    );
                }
                self.record(
                    m.message_id,
                    NotifyDecision::Unavailable,
                    None,
                    None,
                    m.eligible_at,
                    now,
                );
            }
            // Every other no-answer is the same fact to the ledger: nobody
            // scored this message, and the deliberate lane may still rescue it.
            Ok(Ok(LlmOutcome::Refused)) | Ok(Ok(LlmOutcome::Failed(_))) | Ok(Err(_)) | Err(_) => {
                self.record(
                    m.message_id,
                    NotifyDecision::Unavailable,
                    None,
                    None,
                    m.eligible_at,
                    now,
                );
            }
        }
    }

    /// Turn a scored verdict into an event, or into the ledger row saying why
    /// there is none. Shared by the model path and the no-model seed path, so
    /// the two cannot drift on what counts as worthy.
    fn emit(&self, m: &ModelRow, v: Verdict<'_>, now: DateTime<Utc>) {
        // SEALED WHILE THE CALL WAS IN FLIGHT, which is the one race in this
        // module with a SECURITY consequence rather than a bookkeeping one.
        // `correct_triage` redacts the `events` row when a human seals a
        // message, but it can only redact a row that EXISTS at seal time; a
        // model call takes seconds, so a user who seals the mail they can
        // already see in the client while this lane is waiting on it would get
        // an events row appended AFTERWARDS, carrying a one_line derived from
        // the body they have just declared auth, served over SSE to every
        // cursor forever and pushed to a lock screen with nothing left to
        // redact it. The deliberate lane is guarded against exactly this
        // (`stage1_apply`/`stage2_apply` return `Ok(false)` on a row sealed
        // mid-pass); this is that guard.
        //
        // AND THE ROW IS RECORDED `suppressed`, WHICH IS THE HALF ROUND 1 LEFT
        // OUT AND IS THE DIFFERENCE BETWEEN CLOSING THE WINDOW AND MOVING IT.
        // Returning silently delays the leak instead of stopping it: with no
        // ledger row, `run`'s re-entry probe stays false, and `ingest_message`'s
        // triage upsert used to revert the human seal on any re-ingest inside
        // `freshness_window_secs` (a catch-up, or the routine INBOX re-walk a
        // held cursor causes), so the very next tick would build a fresh
        // `Candidate::Model` over the sealed body, send it to the notify model,
        // and append an events row AFTER `correct_triage` had already run its
        // redaction — a model-authored line about a sealed body, replayed to
        // every cursor forever, with nothing left that can redact it. That
        // upsert now preserves a human-decided sensitivity (`messages.rs`), and
        // this row is the second, independent break in the same chain: a
        // `suppressed` row arms the probe, so a re-run never re-asks. A seal is
        // a structural silence in exactly §11.4's sense, and it is not
        // rescuable, which is the vocabulary's own test for the word.
        //
        // `triage_seed_verdict` is the read because it selects `sensitivity =
        // 'normal'`, so `None` IS the answer "not normal any more" — or "the row
        // is gone", which wants the same silence. A store ERROR is a fact about
        // the database rather than about the message: still no event, but no row
        // either, matching the deliberate lane, which writes no ledger row for a
        // row it abandoned mid-pass.
        match self.seal_state(m.message_id) {
            SealState::Unsealed => {}
            SealState::SealedOrGone => {
                self.record(
                    m.message_id,
                    NotifyDecision::Suppressed,
                    Some(v.importance),
                    Some(v.model_used),
                    m.eligible_at,
                    now,
                );
                return;
            }
            SealState::Unknown => return,
        }
        // THE RULE AS IT STANDS NOW, not as it stood at ingest. The reactive
        // squelch — a user blocking a sender whose mail is being scored this
        // second — is the case this exists for, and it is the common one. It is
        // not answered here but handed to `worthy_kind`, which refuses a
        // Squelch/Filtered rule with `Refusal::Suppressed` BEFORE it looks at
        // the score or the rescue window; the `Err(Refusal::Suppressed)` arm
        // below writes the row. An earlier draft duplicated that test right
        // here, which made the arm below unreachable and left the live-rule path
        // with no coverage at all. One test, one row, one place to change it.
        let rule = self.current_rule(&m.sender);
        let ctx = EventContext {
            account_id: self.account_id,
            message_id: m.message_id,
            thread_id: &m.thread_id,
            sender: &m.sender,
            one_line: v.one_line,
            notify_eligible_at: Some(m.eligible_at),
            // A `Model` candidate is non-sealed, non-sent and non-spam BY
            // CONSTRUCTION: `candidate` refuses all three before this variant
            // exists. Spelled out rather than plumbed so the invariants
            // `worthy_kind` re-checks are visibly the same ones.
            sensitivity: Sensitivity::Normal,
            is_sent: false,
            is_spam: false,
            rule,
            // THE SEED DECIDES THE SHAPE. urgent > deadline > surfaced is
            // structure, and structure is not what a small model was asked.
            tier: m.seed.tier,
            importance: v.importance,
            deadline: m.seed.deadline.as_ref(),
        };
        match events::event_for(&ctx, &self.cfg, now) {
            Ok(ev) => match self.store.append_event(&ev) {
                Ok(Some(_)) => self.record(
                    m.message_id,
                    NotifyDecision::Sent,
                    Some(v.importance),
                    Some(v.model_used),
                    m.eligible_at,
                    now,
                ),
                // `UNIQUE(message_id)`: a sent buzz is never rewritten, so the
                // other lane got here first and this one only records that it
                // agreed.
                Ok(None) => self.record(
                    m.message_id,
                    NotifyDecision::WouldSend,
                    Some(v.importance),
                    Some(v.model_used),
                    m.eligible_at,
                    now,
                ),
                Err(e) => eprintln!(
                    "squelch: notify fast lane could not append an event for message {} ({e})",
                    m.message_id
                ),
            },
            Err(Refusal::NotWorthy) => self.record(
                m.message_id,
                NotifyDecision::DeclinedByModel,
                Some(v.importance),
                Some(v.model_used),
                m.eligible_at,
                now,
            ),
            // THE LIVE RULE, not the one `candidate` saw. A sender the user
            // squelched between ingest and this verdict arrives here rather
            // than as a `Candidate::Suppressed`, and it is still `suppressed`:
            // the score is not why it stayed quiet, so calling it a decline
            // would put the user's own standing instruction in the pile the
            // deliberate lane is measured against rescuing (docs/NOTIFY.md
            // §11.4).
            Err(Refusal::Suppressed) => self.record(
                m.message_id,
                NotifyDecision::Suppressed,
                Some(v.importance),
                Some(v.model_used),
                m.eligible_at,
                now,
            ),
            // Worthy, and too late. The lane runs at ingest, so this is nearly
            // always a machine that was asleep or a store that was slow; it is
            // still the drop docs/NOTIFY.md §2a exists to make visible.
            //
            // ASK THE STORE FIRST, exactly as `SyncEngine::emit_event` does and
            // for the same reason: `worthy_kind` refuses on the rescue ceiling
            // WITHOUT touching the store, so it cannot know the phone already
            // buzzed. A task that resumed past the window behind a laptop sleep
            // or a `fast_concurrency` backlog, for mail the deliberate lane
            // already emitted, would otherwise book a DELIVERED notification as
            // a missed one — in the single series §2a exists to make believable.
            // A store error reads as "no event", the honest direction for a
            // count whose whole job is to be believed when it says one went
            // missing.
            Err(Refusal::Expired) => {
                let decision = if self
                    .store
                    .message_has_event(self.account_id, m.message_id)
                    .unwrap_or(false)
                {
                    NotifyDecision::WouldSend
                } else {
                    NotifyDecision::Expired
                };
                self.record(
                    m.message_id,
                    decision,
                    Some(v.importance),
                    Some(v.model_used),
                    m.eligible_at,
                    now,
                );
            }
        }
    }

    /// One ledger row plus its counter, which is the ONLY way this lane says
    /// anything happened. Best-effort: a failed write is logged (id + decision)
    /// and swallowed.
    fn record(
        &self,
        message_id: i64,
        decision: NotifyDecision,
        notify_importance: Option<u8>,
        model_used: Option<&str>,
        eligible_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) {
        // FIRST SIGHT TO THIS DECISION. Negative is impossible unless the clock
        // went backwards; clamped rather than wrapped, because a dashboard is
        // more useful wrong by milliseconds than unreadable for the life of the
        // process.
        let latency = (now - eligible_at)
            .num_milliseconds()
            .clamp(0, i64::from(u32::MAX)) as u32;
        let row = NewNotifyDecision {
            account_id: self.account_id,
            message_id,
            lane: LaneLabel::Fast,
            decision,
            notify_importance,
            model_used: model_used.map(str::to_string),
            latency_ms: Some(latency),
        };
        // THE METRIC IS THE LEDGER'S SHADOW, NEVER A SECOND BOOKKEEPING, which
        // is the rule `SyncEngine::record_deliberate` states and enforces for
        // its own lane: hanging the counter off the insert makes the two agree
        // by construction — one miss, one row, one count. Firing it first and
        // discarding the `bool` would let an IGNORED duplicate, or a write that
        // failed on a locked WAL under the two lanes' contention, claim a
        // decision `SELECT lane, decision, count(*) FROM notify_decisions` does
        // not hold, and §11.11 reads that query beside the graph.
        match self.store.record_notify_decision(&row) {
            Ok(true) => {
                self.metrics.record_notify(LaneLabel::Fast, decision);
                // THE HISTOGRAM IS ABOUT DELIVERED NOTIFICATIONS, so only a
                // `sent` observes it: a decline has no latency anybody
                // experienced, and folding one in would make the p95 a statement
                // about how fast we say no.
                if decision == NotifyDecision::Sent {
                    self.metrics.observe_notify_fast(latency as f64 / 1000.0);
                }
            }
            Ok(false) => {}
            Err(e) => eprintln!(
                "squelch: notify ledger write failed for message {message_id} ({}): {e}",
                decision.as_str()
            ),
        }
    }

    /// Is this row still unsealed, and still there? Asked TWICE on the model
    /// path on purpose — once before the request is built, once before the
    /// event is appended — because the two protect different things (the prompt
    /// and the `events` row) across different windows (the permit queue and the
    /// call itself).
    ///
    /// AND ONCE ON THE SEALED PATH, where the answer is read the other way
    /// round: there `Unsealed` is the refusal, because a person who un-sealed a
    /// row outranks the seed detector that keeps calling it auth.
    ///
    /// `triage_seed_verdict` is the read because its `WHERE` carries
    /// `sensitivity = 'normal'`: `Ok(None)` is "sealed, or gone", which want the
    /// same silence, and neither can be told from the other by this query — nor
    /// needs to be, since both end the decision.
    fn seal_state(&self, message_id: i64) -> SealState {
        match self.store.triage_seed_verdict(self.account_id, message_id) {
            Ok(Some(_)) => SealState::Unsealed,
            Ok(None) => SealState::SealedOrGone,
            Err(e) => {
                eprintln!(
                    "squelch: notify fast lane could not confirm message {message_id} is still \
                     unsealed ({e}); not emitting"
                );
                SealState::Unknown
            }
        }
    }

    /// The sender's rule as the list stands NOW. A store error reads as "no
    /// rule", which is the direction that lets a notification through: the
    /// deliberate lane asks the same question again behind this one.
    fn current_rule(&self, from_addr: &str) -> Option<Disposition> {
        let rules = self.store.list_sender_rules(self.account_id).ok()?;
        events::current_rule(from_addr, &rules)
    }

    /// Is the lane parked after a config-level failure? A poisoned lock reads as
    /// "not disabled": the failure mode of asking is one wasted 4xx, and the
    /// failure mode of never asking again is a lane that stays silently dead.
    fn is_disabled(&self) -> bool {
        let Ok(until) = self.disabled_until.lock() else {
            return false;
        };
        until.is_some_and(|t| Instant::now() < t)
    }

    fn disable_for(&self, d: Duration) {
        if let Ok(mut until) = self.disabled_until.lock() {
            *until = Some(Instant::now() + d);
        }
    }

    /// The engine's budget gate, over the engine's own `WarnDays`.
    fn budget(&self) -> BudgetLedger<'_, S> {
        BudgetLedger {
            store: &*self.store,
            account_id: self.account_id,
            warn_days: &self.warn_days,
        }
    }
}

/// One message's claim on this process's fast lane, released on drop.
///
/// A plain `HashSet<i64>` behind a `std::sync::Mutex`, never held across an
/// await: the lock is taken for one `insert` and one `remove` and nothing else,
/// which is what lets it be the sync mutex rather than an async one in a
/// function full of suspension points.
struct InFlight<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<i64>>,
    message_id: i64,
}

impl<'a> InFlight<'a> {
    /// Claim `message_id`, or `None` if another task in this process holds it.
    fn claim(
        set: &'a std::sync::Mutex<std::collections::HashSet<i64>>,
        message_id: i64,
    ) -> Option<Self> {
        // A POISONED LOCK IS RECOVERED, not treated as a refusal. Nothing that
        // can panic runs while this lock is held, so poisoning here means some
        // OTHER task panicked with the guard alive; the set is a plain
        // `HashSet` and cannot be left half-modified by it. Refusing instead
        // would take the fast lane dark for the life of the process, which is a
        // far worse answer than proceeding over a set we know is intact.
        let mut set_guard = set.lock().unwrap_or_else(|p| p.into_inner());
        if !set_guard.insert(message_id) {
            return None;
        }
        drop(set_guard);
        Some(Self { set, message_id })
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // THE SAME RECOVERY `claim` MAKES, and it has to be: the two are one
        // guard, and disagreeing about poisoning is how the claim leaks. `claim`
        // keeps succeeding over a poisoned-but-intact set while a `Ok(..)`-only
        // release quietly does nothing, so the id stays in the set and every
        // later `run` for that message returns at the claim — the fast lane
        // silent for it for the life of the process, which is the exact failure
        // the RAII comment in `run` promises this type prevents.
        let mut set = self.set.lock().unwrap_or_else(|p| p.into_inner());
        set.remove(&self.message_id);
    }
}

/// What the seal re-read found. `Unknown` is a store error, which is NOT the
/// same answer as `SealedOrGone`: one is a fact about the message, the other is
/// a fact about the database, and only the first belongs in the ledger.
enum SealState {
    Unsealed,
    SealedOrGone,
    Unknown,
}

/// [`Candidate::Model`] minus the two fields only the model call reads, so the
/// emission half is not carrying a subject and a body it has no business with
/// once the request is out.
struct ModelRow {
    message_id: i64,
    thread_id: String,
    sender: String,
    seed: Seed,
    /// The STORED stamp (see [`NotifyLane::run_model`]), which is what every
    /// latency and every rescue-window test downstream is measured from.
    eligible_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Stage1Config, Stage2Provider};
    use crate::store::SqliteStore;
    use crate::sync::ingest::{RawFetched, ingest_with_rules};
    use crate::types::EventKind;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ---- a loopback model mock that RECORDS its requests --------------------
    //
    // Recording is not decoration here. "One attempt, no retry" and "no request
    // at all while the lane is parked" are properties about the NUMBER of
    // requests, and a mock that accepts exactly one connection cannot tell
    // "asked once" from "asked three times and the mock hung up". And the model
    // id the lane actually PUT ON THE WIRE is a property of the request body:
    // asserting it only on the ledger row is how a lane that 400s on every
    // gateway deployment passes its own suite.

    /// Read one whole HTTP request: headers, then exactly `content-length`
    /// bytes. A single `read` would truncate a 6 KB prompt at the first segment
    /// boundary and every body assertion would pass or fail by luck. Same shape
    /// as `notify_llm`'s, and here for the same reason.
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::with_capacity(16384);
        let mut chunk = [0u8; 4096];
        loop {
            let n = match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            let Some(head_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let want: usize = text[..head_end]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                })
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + want {
                break;
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    /// What a mock does after it has RECORDED a request, which is the axis the
    /// race tests need: a request that has landed but not yet been answered is
    /// exactly the window a mid-call seal, or a second overlapping `run`, lives
    /// in.
    #[derive(Clone)]
    enum Hold {
        /// Answer at once.
        None,
        /// Accept and answer nothing, ever: the timeout's test.
        Forever,
        /// Answer once the watch flips true, so a test can act while the call is
        /// demonstrably in flight rather than after a sleep it hopes is long
        /// enough.
        Until(tokio::sync::watch::Receiver<bool>),
    }

    /// A mock that answers every request with `(status, body)` and records each
    /// one it saw, whole. `hang` makes it accept and never answer, which is how
    /// the timeout is tested without a sleep standing in for an assertion.
    async fn mock(
        status: u16,
        body: impl Into<String>,
        hang: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        mock_held(status, body, if hang { Hold::Forever } else { Hold::None }).await
    }

    /// Bounded polling with a deadline. NOT a sleep standing in for an
    /// assertion: the condition is the assertion and the deadline only bounds
    /// how long a broken build takes to say so.
    async fn until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{what}");
    }

    async fn mock_held(
        status: u16,
        body: impl Into<String>,
        hold: Hold,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let body: Arc<str> = Arc::from(body.into());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let sink = sink.clone();
                let body = body.clone();
                let hold = hold.clone();
                tokio::spawn(async move {
                    let req = read_request(&mut sock).await;
                    sink.lock().unwrap().push(req);
                    match hold {
                        Hold::None => {}
                        // Hold the socket open, answering nothing, until the
                        // caller's timeout fires and drops it.
                        Hold::Forever => std::future::pending::<()>().await,
                        Hold::Until(mut rx) => {
                            while !*rx.borrow_and_update() {
                                if rx.changed().await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// An Anthropic-shaped verdict at `importance`, with a one_line the tests
    /// assert on so an event carrying the SEED's line instead would fail.
    ///
    /// BUILT WITH `serde_json`, not a raw string literal: the payload is JSON
    /// nested inside a JSON string, and a hand-escaped version of that is how a
    /// test ends up asserting on a parse failure it mistook for a verdict.
    fn verdict(importance: i64) -> String {
        let verdict = serde_json::json!({
            "notify_importance": importance,
            "one_line": "The model wrote this line",
        })
        .to_string();
        serde_json::json!({
            "content": [{"type": "text", "text": verdict}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 900, "output_tokens": 20},
        })
        .to_string()
    }

    fn cfg() -> NotifyConfig {
        NotifyConfig {
            // One second, so the timeout test finishes in a second rather than
            // eight, and is still an order of magnitude above loopback noise.
            fast_timeout_secs: 1,
            ..NotifyConfig::default()
        }
    }

    /// A lane over `store`, pointed at `url` (or with no model at all).
    fn lane(
        store: &Arc<SqliteStore>,
        acct: AccountId,
        url: Option<&str>,
        cfg: NotifyConfig,
    ) -> Arc<NotifyLane<SqliteStore>> {
        let llm = url.map(|u| ResolvedLlm {
            api_key: "sk-test".to_string(),
            provider: Stage2Provider::Anthropic,
            url: u.to_string(),
        });
        Arc::new(NotifyLane::new(
            store.clone(),
            reqwest::Client::new(),
            cfg,
            Config::default().stage1.known_contact_importance,
            llm,
            SyncMetrics::new(),
            acct,
            Arc::new(std::sync::Mutex::new(super::super::WarnDays::default())),
        ))
    }

    /// Ingest one RFC822 through the real pipeline and the real store, stamped
    /// eligible at `now` exactly as the incremental path stamps it. Returns the
    /// message id and the candidate the lane would have been spawned on.
    fn ingest(
        store: &Arc<SqliteStore>,
        acct: AccountId,
        msgid: &str,
        eml: &str,
        now: DateTime<Utc>,
        cfg: &NotifyConfig,
    ) -> (i64, Option<Candidate>) {
        let f = RawFetched {
            account_id: acct,
            gmail_msg_id: msgid.to_string(),
            gmail_thread_id: None,
            raw: eml.as_bytes().to_vec(),
            internal_date: Some(now),
            is_sent: false,
            is_spam: false,
            account_addr: "me@example.com".to_string(),
        };
        let rules = store.list_sender_rules(acct).unwrap();
        let mut triaged = ingest_with_rules(&f, &Stage1Config::default(), now, &rules, |addr| {
            store.is_known_contact(acct, addr).unwrap_or(false)
        });
        triaged.notify_eligible_at = super::super::notify_eligible_stamp(
            &triaged,
            super::super::IngestOrigin::Incremental,
            cfg,
            now,
        );
        let id = store.ingest_message(&triaged).unwrap();
        let c = candidate(&triaged, id, &rules, cfg, |addr| {
            store.is_known_contact(acct, addr).unwrap_or(false)
        });
        (id, c)
    }

    fn store() -> (Arc<SqliteStore>, AccountId) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        (store, acct)
    }

    /// A plain personal note from a stranger: normal sensitivity, not spam, and
    /// nowhere near confident, so the SEED never decides it and only the model's
    /// score can.
    fn note_eml(at: DateTime<Utc>) -> String {
        format!(
            "From: Dana <dana@elsewhere.example>\r\n\
             To: me@example.com\r\n\
             Subject: quick question about thursday\r\n\
             Date: {}\r\n\
             \r\n\
             Are you free thursday afternoon? Let me know either way.\r\n",
            at.to_rfc2822()
        )
    }

    /// The one ledger row this lane wrote for `message_id`.
    fn ledger(
        store: &SqliteStore,
        acct: AccountId,
        message_id: i64,
    ) -> Option<crate::store::NotifyDecisionRow> {
        store
            .notify_decisions_since(acct, Utc::now() - chrono::Duration::hours(1), 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == message_id)
    }

    #[tokio::test]
    async fn a_high_score_buzzes_with_the_models_line_and_the_seeds_shape() {
        let (store, acct) = store();
        let now = Utc::now();
        let v = verdict(80);
        let (url, seen) = mock(200, v, false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        let c = c.expect("ordinary fresh mail is a model candidate");

        lane(&store, acct, Some(&url), cfg()).run(c).await;

        let row = ledger(&store, acct, mid).expect("a ledger row");
        assert_eq!(row.decision, NotifyDecision::Sent);
        assert_eq!(row.notify_importance, Some(80));
        // QUALIFIED, because `is_gateway_url` is "anything that is not
        // api.anthropic.com" and a loopback mock is exactly that. The ledger
        // records the id the REQUEST carried, which is the only spelling that
        // answers "which model produced this score" without a second lookup.
        assert_eq!(
            row.model_used.as_deref(),
            Some("anthropic/claude-haiku-4-5")
        );
        assert!(
            row.latency_ms.is_some(),
            "the fast lane always times itself"
        );
        assert_eq!(seen.lock().unwrap().len(), 1);

        let evs = store.events_after(acct, 0, 10).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].message_id, mid);
        assert_eq!(
            evs[0].one_line, "The model wrote this line",
            "the notify model writes the line the user reads"
        );
        assert_eq!(evs[0].importance, 80);
        assert_eq!(
            evs[0].kind,
            EventKind::Surfaced,
            "the SEED's tier and deadline decide the kind, not the score"
        );
        assert_eq!(evs[0].sealed_kind, None);
    }

    #[tokio::test]
    async fn a_low_score_is_declined_and_appends_nothing() {
        let (store, acct) = store();
        let now = Utc::now();
        let v = verdict(20);
        let (url, _) = mock(200, v, false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());

        lane(&store, acct, Some(&url), cfg()).run(c.unwrap()).await;

        let row = ledger(&store, acct, mid).expect("a declined row is still a row");
        assert_eq!(row.decision, NotifyDecision::DeclinedByModel);
        assert_eq!(
            row.notify_importance,
            Some(20),
            "the score is stored even when it loses: it is the label the \
             threshold moves from"
        );
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
    }

    /// A 5xx is retryable at every other call site in the pipeline and is NOT
    /// retried here: the window this lane runs in is measured in seconds and
    /// `llm::BACKOFF_CAP` is 60. The deliberate lane is the retry.
    #[tokio::test]
    async fn a_500_is_unavailable_after_exactly_one_request() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(500, r#"{"error":"boom"}"#, false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());

        lane(&store, acct, Some(&url), cfg()).run(c.unwrap()).await;

        assert_eq!(
            ledger(&store, acct, mid).unwrap().decision,
            NotifyDecision::Unavailable
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "one attempt, no backoff: a retry loop here would sleep away the \
             window the lane exists for"
        );
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_model_that_never_answers_gives_up_inside_the_timeout() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, _) = mock(200, "{}", /* hang */ true).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());

        let started = Instant::now();
        lane(&store, acct, Some(&url), cfg()).run(c.unwrap()).await;
        let elapsed = started.elapsed();

        assert_eq!(
            ledger(&store, acct, mid).unwrap().decision,
            NotifyDecision::Unavailable
        );
        // BOUNDED, not slept: the assertion is that the deadline FIRED, and the
        // ceiling is generous enough that a loaded CI box cannot fail it.
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(6),
            "gave up after {elapsed:?}, not at the 1s deadline"
        );
    }

    /// THE NO-MODEL PATH, which is the whole behaviour the ingest site used to
    /// own: with nothing to wait for, a CONFIDENT seed above the line is the
    /// final word and notifies, recorded as `heuristic` so the ledger never
    /// counts a regex as a labelled example of a model's judgement.
    #[tokio::test]
    async fn with_no_model_a_confident_seed_notifies_as_heuristic() {
        let (store, acct) = store();
        let now = Utc::now();
        let alert = format!(
            "From: Monitoring <alerts@monitoring.example>\r\n\
             To: me@example.com\r\n\
             Subject: Incident: checkout api is down\r\n\
             Date: {}\r\n\
             \r\n\
             A high-severity incident was opened for the checkout service.\r\n",
            now.to_rfc2822()
        );
        let (mid, c) = ingest(&store, acct, "g-alert", &alert, now, &cfg());

        lane(&store, acct, None, cfg()).run(c.unwrap()).await;

        let row = ledger(&store, acct, mid).unwrap();
        assert_eq!(row.decision, NotifyDecision::Sent);
        assert_eq!(row.model_used.as_deref(), Some("heuristic"));
        let evs = store.events_after(acct, 0, 10).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].message_id, mid);
    }

    /// THE KNOWN-CONTACT GUARANTEE, in this lane. A small model scoring someone
    /// the user actually writes to at 10 must not be the last word, so the same
    /// floor Stage-1 applies to its own score applies to the model's.
    #[tokio::test]
    async fn the_known_contact_floor_lifts_a_model_score_that_fell_through() {
        let (store, acct) = store();
        let now = Utc::now();
        // Seed the contact the only way contacts are made: a sent message TO them.
        let sent = format!(
            "From: me@example.com\r\n\
             To: Dana <dana@elsewhere.example>\r\n\
             Subject: re: thursday\r\n\
             Date: {}\r\n\
             \r\n\
             sounds good\r\n",
            now.to_rfc2822()
        );
        let f = RawFetched {
            account_id: acct,
            gmail_msg_id: "g-sent".to_string(),
            gmail_thread_id: None,
            raw: sent.as_bytes().to_vec(),
            internal_date: Some(now),
            is_sent: true,
            is_spam: false,
            account_addr: "me@example.com".to_string(),
        };
        let triaged = ingest_with_rules(&f, &Stage1Config::default(), now, &[], |_| false);
        store.ingest_message(&triaged).unwrap();
        assert!(
            store
                .is_known_contact(acct, "dana@elsewhere.example")
                .unwrap()
        );

        let v = verdict(10);
        let (url, _) = mock(200, v, false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        match &c {
            Some(Candidate::Model {
                is_known_contact, ..
            }) => assert!(*is_known_contact),
            _ => panic!("expected a model candidate"),
        }

        lane(&store, acct, Some(&url), cfg()).run(c.unwrap()).await;

        let row = ledger(&store, acct, mid).unwrap();
        assert_eq!(row.decision, NotifyDecision::Sent);
        assert_eq!(
            row.notify_importance,
            Some(Config::default().stage1.known_contact_importance),
            "the floor, not the 10 the model said"
        );
        assert_eq!(store.events_after(acct, 0, 10).unwrap().len(), 1);
    }

    /// A CLAIM IS ALWAYS RELEASED, poisoned lock included. `claim` recovers
    /// from poisoning on purpose (the set is a plain `HashSet` and cannot be
    /// left half-modified, and refusing would take the lane dark), so the
    /// release has to make the same call: a `Drop` that gave up on `Err` would
    /// leave the id in the set forever, and every later `run` for that message
    /// would return at the claim — the fast lane permanently silent for it,
    /// which is exactly what the RAII guard exists to prevent.
    ///
    /// The panic below is deliberate and prints; it is the only way a
    /// `std::sync::Mutex` becomes poisoned.
    #[test]
    fn a_poisoned_in_flight_set_still_releases_its_claim() {
        let set: Arc<Mutex<std::collections::HashSet<i64>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        let victim = set.clone();
        let _ = std::thread::spawn(move || {
            let _held = victim.lock().unwrap();
            panic!("another task died holding the guard");
        })
        .join();
        assert!(set.is_poisoned(), "the set is poisoned but intact");

        {
            let _claim = InFlight::claim(&set, 7).expect("a claim over a poisoned but intact set");
            assert!(InFlight::claim(&set, 7).is_none(), "and it excludes");
        }
        assert!(
            InFlight::claim(&set, 7).is_some(),
            "the claim was released: a message must not go dark for the life of \
             the process because some unrelated task panicked"
        );
    }

    /// A SPAWNED TASK CARRIES ONLY WHAT THE PROMPT CAN READ. The candidate is
    /// what a task HOLDS while it waits for a permit, and neither the timeout
    /// nor the semaphore bounds the queue: an endpoint that hangs rather than
    /// erroring costs `fast_timeout_secs` per permit and lets tasks pile up to
    /// `daily_cap`. A full flattened body per queued task is a size the SENDER
    /// picks, on a daemon that has been OOM-killed on a 4 GB box, in exchange
    /// for bytes `build_user_message` throws away.
    ///
    /// AND THE TRUNCATION MARKER SURVIVES, which is the half a naive cut
    /// breaks: `truncate_flagged` marks the cut only when what it is handed is
    /// LONGER than the cap, so trimming to exactly the cap here would tell the
    /// model a clipped body was the whole mail. Both are asserted, because the
    /// second is invisible from the first.
    #[tokio::test]
    async fn a_long_body_is_cut_at_the_gate_and_still_reads_as_truncated() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(200, verdict(80), false).await;
        let long = format!(
            "From: Marketing <news@shop.example>\r\n\
             To: me@example.com\r\n\
             Subject: our biggest sale\r\n\
             Date: {}\r\n\
             \r\n\
             {}\r\n",
            now.to_rfc2822(),
            "everything must go and here is another sentence about it. ".repeat(400)
        );
        let cfg = cfg();
        let (_, c) = ingest(&store, acct, "g-long", &long, now, &cfg);
        let c = c.expect("a model candidate");
        let Candidate::Model { ref body, .. } = c else {
            panic!("expected a model candidate");
        };
        assert!(
            body.chars().count() <= cfg.max_body_chars + 1,
            "the candidate holds {} chars of a body the prompt reads {} of",
            body.chars().count(),
            cfg.max_body_chars
        );

        lane(&store, acct, Some(&url), cfg.clone()).run(c).await;

        let req = seen.lock().unwrap().first().cloned().expect("one request");
        assert!(
            req.contains(&format!("[body truncated to {} chars]", cfg.max_body_chars)),
            "the model must still be told the body was cut"
        );
    }

    /// THE SEAL, end to end. The subject and body below both carry a six-digit
    /// code; nothing the lane writes may contain either, and the candidate has
    /// no field that could have carried them in the first place.
    #[tokio::test]
    async fn a_sealed_ping_says_only_that_a_code_arrived() {
        let (store, acct) = store();
        let now = Utc::now();
        let eml = format!(
            "From: 123456@x.example\r\n\
             To: me@example.com\r\n\
             Subject: Your verification code is 483920\r\n\
             Date: {}\r\n\
             \r\n\
             Your one-time passcode is 483920. Enter this code to continue.\r\n",
            now.to_rfc2822()
        );
        let on = NotifyConfig {
            sealed_enabled: true,
            ..cfg()
        };
        let (mid, c) = ingest(&store, acct, "g-otp", &eml, now, &on);
        let c = c.expect("a sealed row is a candidate once the knob is on");
        match &c {
            Candidate::Sealed { kind, sender, .. } => {
                assert_eq!(*kind, SealedKind::Otp);
                assert_eq!(sender, "123456@x.example");
            }
            _ => panic!("expected a sealed candidate"),
        }

        lane(&store, acct, None, on).run(c).await;

        let evs = store.events_after(acct, 0, 10).unwrap();
        assert_eq!(evs.len(), 1);
        let ev = &evs[0];
        assert_eq!(ev.one_line, "Login code arrived");
        assert_eq!(ev.sealed_kind, Some(SealedKind::Otp));
        assert_eq!(ev.kind, EventKind::Urgent);
        assert_eq!(ev.importance, 90);

        let row = ledger(&store, acct, mid).unwrap();
        assert_eq!(row.decision, NotifyDecision::Sent);
        assert_eq!(row.model_used.as_deref(), Some("sealed"));
        assert_eq!(row.notify_importance, Some(90));

        // NOT ONE WORD OF THE MAIL, on either row. The sender is metadata
        // `/client/sealed` already serves and it happens to be all digits here
        // ON PURPOSE, so a test that only looked for "483920" could not pass by
        // accident.
        for text in [
            ev.one_line.as_str(),
            // THE ROW THAT REACHES A LOCK SCREEN, not just the candidate: the
            // sender is asserted at both ends, because a change that appended a
            // subject fragment to `sealed_event`'s `sender` argument at this
            // module's call site would pass an assertion made only on the
            // candidate (docs/NOTIFY.md §11.10).
            ev.sender.as_str(),
            row.model_used.as_deref().unwrap_or_default(),
        ] {
            assert!(!text.contains("483920"), "the code leaked into {text:?}");
            assert!(!text.contains("passcode"), "the body leaked into {text:?}");
            assert!(
                !text.contains("verification"),
                "the subject leaked into {text:?}"
            );
        }
    }

    /// A PERSON WHO SAID "THIS IS NOT A LOGIN CODE" OUTRANKS THE DETECTOR, on
    /// the sealed path too. `detect_sealed` biases to recall on purpose, so it
    /// over-seals; `correct_triage` is how that is undone. But `candidate` is
    /// PURE and reads the FRESH heuristic triage, so the very next re-ingest
    /// inside `freshness_window_secs` builds a `Candidate::Sealed` for the row
    /// again and nothing in the gate can know better.
    ///
    /// Without the re-read in `run`'s sealed arm that is an Urgent,
    /// importance-90 "Login code arrived" on a lock screen, routed to the Auth
    /// list where the message is not, for mail the user has explicitly called
    /// ordinary — the mirror of the model path's guard, which docs/SECURITY.md
    /// §4 states only in the normal-to-sealed direction.
    #[tokio::test]
    async fn a_row_a_person_un_sealed_never_pings_as_a_login_code() {
        let (store, acct) = store();
        let now = Utc::now();
        let eml = format!(
            "From: Newsletter <news@auth-vendor.example>\r\n\
             To: me@example.com\r\n\
             Subject: Your verification code is 483920\r\n\
             Date: {}\r\n\
             \r\n\
             Your one-time passcode is 483920. Enter this code to continue.\r\n",
            now.to_rfc2822()
        );
        let on = NotifyConfig {
            sealed_enabled: true,
            ..cfg()
        };
        let (mid, c) = ingest(&store, acct, "g-otp", &eml, now, &on);
        assert!(matches!(c, Some(Candidate::Sealed { .. })));

        // THE USER SAYS IT IS ORDINARY MAIL.
        store
            .correct_triage(
                acct,
                mid,
                crate::types::TriageAxis::Sensitivity,
                "normal",
                None,
                now,
            )
            .unwrap();

        // THE ROUTINE RE-WALK: same gmail id, same seed detector, same verdict.
        let (mid2, c2) = ingest(&store, acct, "g-otp", &eml, now, &on);
        assert_eq!(mid2, mid);
        assert!(
            store.sealed_messages(acct).unwrap().is_empty(),
            "the re-ingest must not re-seal what a person un-sealed: only a \
             PERSON outranks a person, in both directions"
        );
        assert!(
            matches!(c2, Some(Candidate::Sealed { .. })),
            "the sharp edge: the gate is pure and the detector has not changed \
             its mind, so the candidate is built anyway"
        );

        lane(&store, acct, None, on).run(c2.unwrap()).await;

        assert!(
            store.events_after(acct, 0, 10).unwrap().is_empty(),
            "no auth ping for mail the user has declared ordinary"
        );
        let row = ledger(&store, acct, mid).expect("recorded, not dropped");
        assert_eq!(
            row.decision,
            NotifyDecision::Suppressed,
            "and the row is what stops the NEXT re-walk asking again"
        );
    }

    #[tokio::test]
    async fn the_sealed_knob_off_makes_a_sealed_row_no_candidate_at_all() {
        let (store, acct) = store();
        let now = Utc::now();
        let eml = format!(
            "From: Bank <noreply@bank.example>\r\n\
             To: me@example.com\r\n\
             Subject: Your verification code\r\n\
             Date: {}\r\n\
             \r\n\
             Your one-time passcode is 483920. Enter this code to continue.\r\n",
            now.to_rfc2822()
        );
        // Default config: `sealed_enabled` is FALSE until a client ships that
        // routes the tap to the auth flow.
        let (mid, c) = ingest(&store, acct, "g-otp", &eml, now, &cfg());
        assert!(c.is_none(), "off means no ping AND no ledger row");
        assert_eq!(
            store.sealed_messages(acct).unwrap().len(),
            1,
            "it WAS sealed"
        );
        assert!(ledger(&store, acct, mid).is_none());
    }

    #[tokio::test]
    async fn a_squelched_sender_is_recorded_suppressed_and_never_asked_about() {
        let (store, acct) = store();
        let now = Utc::now();
        store
            .set_sender_rule(
                acct,
                "*@elsewhere.example",
                "not urgent",
                Disposition::Squelch,
            )
            .unwrap();
        let (url, seen) = mock(200, "{}", false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        assert!(matches!(c, Some(Candidate::Suppressed { .. })));

        lane(&store, acct, Some(&url), cfg()).run(c.unwrap()).await;

        assert_eq!(
            ledger(&store, acct, mid).unwrap().decision,
            NotifyDecision::Suppressed
        );
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
        assert_eq!(seen.lock().unwrap().len(), 0, "a rule needs no model");
    }

    /// BACKFILL PRODUCES NO ROW AT ALL. Not a `declined_by_model`, not a
    /// `suppressed`: the ledger records only messages carrying a stamp, which is
    /// what keeps it from being mostly noise (docs/NOTIFY.md §11.4).
    #[tokio::test]
    async fn an_unstamped_row_is_not_a_candidate_and_leaves_no_ledger_row() {
        let (store, acct) = store();
        let now = Utc::now();
        let f = RawFetched {
            account_id: acct,
            gmail_msg_id: "g-back".to_string(),
            gmail_thread_id: None,
            raw: note_eml(now).into_bytes(),
            internal_date: Some(now),
            is_sent: false,
            is_spam: false,
            account_addr: "me@example.com".to_string(),
        };
        let rules = store.list_sender_rules(acct).unwrap();
        let mut triaged = ingest_with_rules(&f, &Stage1Config::default(), now, &rules, |_| false);
        triaged.notify_eligible_at = super::super::notify_eligible_stamp(
            &triaged,
            super::super::IngestOrigin::Backfill,
            &cfg(),
            now,
        );
        let mid = store.ingest_message(&triaged).unwrap();
        assert_eq!(triaged.notify_eligible_at, None);

        let c = candidate(&triaged, mid, &rules, &cfg(), |_| false);
        assert!(c.is_none());
        assert!(ledger(&store, acct, mid).is_none());
    }

    /// THE STORED STAMP IS THE ONE THAT DECIDES, which is this module's second
    /// departure from docs/NOTIFY.md §11.5 (no variant carries an
    /// `eligible_at`) and the only guard behind it.
    ///
    /// The divergence is not hypothetical. A backfill walk stamps NULL — silent
    /// forever, deliberately, or a first sync of a year's archive is a year of
    /// buzzing. Then a `catch_up()` re-walks the same window on the INCREMENTAL
    /// path, and for anything whose `Date:` is still inside
    /// `freshness_window_secs` `notify_eligible_stamp` computes a fresh `Some`
    /// that `ingest_message` refuses to store. `candidate` gates on the
    /// COMPUTED one and so hands back a full `Candidate::Model`; only `run`'s
    /// re-read of the STORED stamp stops it.
    ///
    /// Put `eligible_at` back on the variants and every other test in this file
    /// still passes while this one goes red — which is why it is here.
    #[tokio::test]
    async fn a_backfilled_row_re_walked_incrementally_still_never_buzzes() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(200, verdict(90), false).await;

        // FIRST SIGHT IS THE BACKFILL: stored stamp NULL.
        let f = RawFetched {
            account_id: acct,
            gmail_msg_id: "g-back".to_string(),
            gmail_thread_id: None,
            raw: note_eml(now).into_bytes(),
            internal_date: Some(now),
            is_sent: false,
            is_spam: false,
            account_addr: "me@example.com".to_string(),
        };
        let rules = store.list_sender_rules(acct).unwrap();
        let mut triaged = ingest_with_rules(&f, &Stage1Config::default(), now, &rules, |_| false);
        triaged.notify_eligible_at = super::super::notify_eligible_stamp(
            &triaged,
            super::super::IngestOrigin::Backfill,
            &cfg(),
            now,
        );
        assert_eq!(triaged.notify_eligible_at, None);
        let mid = store.ingest_message(&triaged).unwrap();

        // THEN THE CATCH-UP, on the incremental path, inside the freshness
        // window: the stamp RECOMPUTES to `Some` and the store keeps its NULL.
        let (mid2, c) = ingest(&store, acct, "g-back", &note_eml(now), now, &cfg());
        assert_eq!(mid2, mid, "a re-walk is the same row");
        assert!(
            matches!(c, Some(Candidate::Model { .. })),
            "the gate cannot tell: it sees only the stamp ingest just computed"
        );
        assert_eq!(
            store.notify_eligible_at(acct, mid).unwrap(),
            None,
            "and the store never accepted it"
        );

        lane(&store, acct, Some(&url), cfg()).run(c.unwrap()).await;

        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "a month of archived mail must not buy a fresh rescue window by \
             being re-walked"
        );
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
        assert!(
            ledger(&store, acct, mid).is_none(),
            "and no ledger row either: the table records only stamped messages"
        );
    }

    /// A CONFIG FAILURE IS ABOUT THE DEPLOYMENT, NOT THE MESSAGE: park the lane,
    /// give the charge back, and let the next message through the gate without a
    /// second doomed request. The mock seeing exactly one request across two
    /// candidates is the whole assertion.
    #[tokio::test]
    async fn a_401_parks_the_lane_and_refunds_the_charge() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"x"}}"#,
            false,
        )
        .await;
        let lane = lane(&store, acct, Some(&url), cfg());
        let day = Utc::now().format("%Y-%m-%d").to_string();

        let (mid1, c1) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        lane.clone().run(c1.unwrap()).await;
        assert_eq!(
            ledger(&store, acct, mid1).unwrap().decision,
            NotifyDecision::Unavailable
        );
        assert_eq!(
            store
                .stage2_budget_used(acct, NOTIFY_FAST_BUDGET_KEY, &day)
                .unwrap(),
            0,
            "a 4xx in ~0ms spends no tokens; leaving it charged would outlive \
             the outage by hours, because the budget key is the UTC day"
        );

        let second = note_eml(now).replace("dana@elsewhere.example", "sam@elsewhere.example");
        let (mid2, c2) = ingest(&store, acct, "g2", &second, now, &cfg());
        lane.run(c2.unwrap()).await;
        assert_eq!(
            ledger(&store, acct, mid2).unwrap().decision,
            NotifyDecision::Unavailable
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the lane is parked: the second message cost no request"
        );
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
    }

    /// THE MODEL ID THE REQUEST CARRIES, which is the half a ledger assertion
    /// cannot see. A gateway resolves a provider from the `anthropic/` prefix
    /// BEFORE it looks at the key, the allow-list or the budget, and answers 400
    /// "could not auto resolve a provider for the request" without it — a 400
    /// `llm::is_config_failure` reads as config, so the lane would park itself
    /// ten minutes at a time, forever, on every hosted tenant, while every test
    /// that asserted only `row.model_used` passed. That is the [[fleet LLM
    /// outage]] shape exactly, so the assertion is made on the wire.
    #[tokio::test]
    async fn the_qualified_model_id_goes_on_the_wire_and_not_just_in_the_ledger() {
        let (store, acct) = store();
        let now = Utc::now();
        // A loopback mock IS a gateway to `is_gateway_url`, which is "anything
        // that is not api.anthropic.com".
        let (url, seen) = mock(200, verdict(80), false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());

        lane(&store, acct, Some(&url), cfg()).run(c.unwrap()).await;

        let req = seen.lock().unwrap().first().cloned().expect("one request");
        let body = req.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let sent: serde_json::Value = serde_json::from_str(&body).expect("a JSON request body");
        assert_eq!(
            sent["model"].as_str(),
            Some("anthropic/claude-haiku-4-5"),
            "the gateway routes on the prefix; a bare id 400s before anything \
             else is consulted"
        );
        // ONE STRING, so the ledger cannot describe a call that was never made.
        assert_eq!(
            ledger(&store, acct, mid).unwrap().model_used.as_deref(),
            sent["model"].as_str()
        );

        // AND THE OTHER DIRECTION: the direct Anthropic API refuses the prefix,
        // so `new` must leave the configured id alone there. Asserted on the
        // config the request is built from, which is the same field
        // `notify_llm::classify_at` reads.
        let direct = lane(&store, acct, Some(llm::API_URL), cfg());
        assert_eq!(direct.cfg.model, NotifyConfig::default().model);
    }

    /// RE-INGEST IS ROUTINE — `catch_up` re-walks the whole window, and a failed
    /// SENT half holds the cursor so the next tick re-walks the same INBOX ids —
    /// and `candidate` recomputes a `Some` stamp for anything still inside the
    /// freshness window. A second run must therefore cost nothing: no second
    /// paid call, no second daily-cap unit, no second counter bump, and above
    /// all no chance of a verdict on the other side of the line appending an
    /// event while `INSERT OR IGNORE` leaves the ledger saying `declined`.
    #[tokio::test]
    async fn a_re_ingested_message_is_never_decided_twice() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(200, verdict(80), false).await;
        let day = now.format("%Y-%m-%d").to_string();
        let lane = lane(&store, acct, Some(&url), cfg());

        let (mid, c1) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        lane.clone().run(c1.expect("a model candidate")).await;

        // THE SAME gmail id THROUGH THE SAME PIPELINE, which is what a re-walk
        // is: `UNIQUE(account_id, gmail_msg_id)` collapses the message row and
        // `notify_eligible_stamp` computes a fresh `Some` off a `Date:` that is
        // still minutes old.
        let (mid2, c2) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        assert_eq!(mid2, mid, "a re-ingest is the same row");
        lane.clone()
            .run(c2.expect("still a candidate: the stamp recomputes"))
            .await;

        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the second run must not pay for a second model call"
        );
        assert_eq!(
            store
                .stage2_budget_used(acct, NOTIFY_FAST_BUDGET_KEY, &day)
                .unwrap(),
            1,
            "nor charge a second daily-cap unit"
        );
        let rows = store
            .notify_decisions_since(acct, now - chrono::Duration::hours(1), 100)
            .unwrap();
        assert_eq!(rows.len(), 1, "one decision per (message, lane), forever");
        assert_eq!(rows[0].decision, NotifyDecision::Sent);
        assert_eq!(store.events_after(acct, 0, 10).unwrap().len(), 1);
        // THE METRIC IS THE LEDGER'S SHADOW, NEVER A SECOND BOOKKEEPING: the
        // counter hangs off the insert's `Ok(true)`, so an IGNORED duplicate
        // cannot make `squelchd_notify_decisions_total{lane="fast"}` claim a
        // decision `SELECT lane, decision, count(*) FROM notify_decisions` does
        // not hold — and §11.11 reads that query beside the graph.
        let text = crate::metrics::render(&lane.metrics, None);
        assert!(
            text.contains("squelchd_notify_decisions_total{lane=\"fast\",decision=\"sent\"} 1\n"),
            "one row, one count"
        );
    }

    /// THE SEAL RACE, which is the one bug on this path with a security
    /// consequence rather than a bookkeeping one. `correct_triage` redacts an
    /// events row when a human seals a message, but only a row that EXISTS at
    /// seal time; a model call takes seconds, so a user sealing the mail they
    /// can already see while this lane waits on it would otherwise get an event
    /// appended AFTERWARDS carrying a one_line written from the body they just
    /// declared auth, served over SSE forever with nothing left to redact it.
    ///
    /// SEALED WHILE THE TASK WAS QUEUED, which is the wider half of the window:
    /// `tokio::time::timeout` wraps the call but not the wait for a permit, so a
    /// burst can queue a task minutes behind the message it is about. The
    /// candidate is already built with the subject and body, so the state the
    /// lane meets is bit-for-bit the state the race produces — and the mock
    /// seeing ZERO requests is the assertion: the prompt is guarded, not just
    /// the events row.
    #[tokio::test]
    async fn a_message_sealed_before_the_call_never_reaches_the_model() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(200, verdict(90), false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        let c = c.expect("a model candidate");

        store
            .correct_triage(
                acct,
                mid,
                crate::types::TriageAxis::Sensitivity,
                "sealed",
                None,
                now,
            )
            .unwrap();

        lane(&store, acct, Some(&url), cfg()).run(c).await;

        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "a body the user has declared auth may never go on the wire to a model"
        );
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
        let row = ledger(&store, acct, mid).expect("the seal is recorded, not dropped");
        assert_eq!(
            row.decision,
            NotifyDecision::Suppressed,
            "a seal is a structural silence, and the row is what stops the next \
             re-ingest asking again"
        );
        assert_eq!(row.notify_importance, None, "nobody scored it");
        assert_eq!(
            store
                .stage2_budget_used(
                    acct,
                    NOTIFY_FAST_BUDGET_KEY,
                    &now.format("%Y-%m-%d").to_string()
                )
                .unwrap(),
            0,
            "and the cap unit charged before the call is given back: no call was made"
        );
    }

    /// THE SEAL RACE PROPER: the request is out and the user seals the message
    /// before the answer comes back. Driven on a gated mock rather than by
    /// sealing beforehand, because the pre-call guard above would otherwise
    /// swallow it and this arm — the last thing between a model-authored line
    /// about a sealed body and an `events` row `correct_triage` has already run
    /// past — would have no coverage at all.
    ///
    /// THE LEDGER ROW IS HALF THE FIX. Round 1 returned here silently, which
    /// only moved the leak: with no row, `run`'s re-entry probe stays false, and
    /// the very next re-ingest inside `freshness_window_secs` re-ran the whole
    /// lane over a body the seal had made unreadable. Asserted below by running
    /// the lane a second time.
    #[tokio::test]
    async fn a_message_sealed_while_the_call_is_in_flight_never_gets_an_event() {
        let (store, acct) = store();
        let now = Utc::now();
        let (release, rx) = tokio::sync::watch::channel(false);
        let (url, seen) = mock_held(200, verdict(90), Hold::Until(rx)).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        let lane = lane(&store, acct, Some(&url), cfg());

        let running = tokio::spawn(lane.clone().run(c.expect("a model candidate")));
        let landed = seen.clone();
        until("the notify call never went out", || {
            landed.lock().unwrap().len() == 1
        })
        .await;

        // THE SEAL LANDS WITH THE REQUEST IN FLIGHT, which is the whole point:
        // the lane has already read the body and is holding it.
        store
            .correct_triage(
                acct,
                mid,
                crate::types::TriageAxis::Sensitivity,
                "sealed",
                None,
                now,
            )
            .unwrap();
        release.send(true).unwrap();
        running.await.unwrap();

        assert!(
            store.events_after(acct, 0, 10).unwrap().is_empty(),
            "a body the user has declared auth may never reach an events row"
        );
        let row = ledger(&store, acct, mid).expect("the seal is recorded");
        assert_eq!(row.decision, NotifyDecision::Suppressed);
        assert_eq!(
            row.notify_importance,
            Some(90),
            "a model DID score it; the score is the label, the seal is the reason"
        );

        // AND THE RE-ENTRY PROBE IS ARMED. Re-ingest is routine and the triage
        // upsert used to revert a human seal on one, so a lane that recorded
        // nothing here would ask the model about this body again on the next
        // tick and mint an un-redactable events row.
        let (mid2, c2) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        assert_eq!(mid2, mid);
        assert_eq!(
            store.sealed_messages(acct).unwrap().len(),
            1,
            "and the re-ingest did not un-seal it: a person outranks a seed"
        );
        // AND IT IS STILL A `Candidate::Model`, which is the sharp edge: the gate
        // is pure and reads the FRESH heuristic triage, and the seed detector
        // has no idea a person sealed this. Nothing in `candidate` can know. The
        // ledger row is what stops the run.
        assert!(matches!(c2, Some(Candidate::Model { .. })));
        lane.run(c2.unwrap()).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the recorded row is what stops a second call on a sealed body"
        );
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
    }

    /// TWO OVERLAPPING RUNS, WHICH THE LEDGER PROBE ALONE CANNOT STOP: its row
    /// is written LAST, so a second `run` that starts while the first is inside
    /// its call reads `false` and proceeds. Real, not theoretical: a held cursor
    /// re-walks the same INBOX ids every `sync.poll_secs` (5s) while
    /// `fast_timeout_secs` is 8.
    #[tokio::test]
    async fn two_overlapping_runs_decide_once() {
        let (store, acct) = store();
        let now = Utc::now();
        let day = now.format("%Y-%m-%d").to_string();
        let (release, rx) = tokio::sync::watch::channel(false);
        let (url, seen) = mock_held(200, verdict(80), Hold::Until(rx)).await;
        let (mid, c1) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        let lane = lane(&store, acct, Some(&url), cfg());

        let first = tokio::spawn(lane.clone().run(c1.expect("a model candidate")));
        let landed = seen.clone();
        until("the first call never went out", || {
            landed.lock().unwrap().len() == 1
        })
        .await;

        // THE SECOND TICK, with the first call still outstanding and no ledger
        // row written yet. It must return without a call, a charge or a row.
        let (mid2, c2) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        assert_eq!(mid2, mid, "a re-ingest is the same row");
        lane.clone().run(c2.expect("still a candidate")).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the second run must not pay for a second model call while the \
             first is still out"
        );

        release.send(true).unwrap();
        first.await.unwrap();

        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(
            store
                .stage2_budget_used(acct, NOTIFY_FAST_BUDGET_KEY, &day)
                .unwrap(),
            1,
            "nor charge a second daily-cap unit"
        );
        let rows = store
            .notify_decisions_since(acct, now - chrono::Duration::hours(1), 100)
            .unwrap();
        assert_eq!(rows.len(), 1, "one decision per (message, lane), forever");
        assert_eq!(rows[0].decision, NotifyDecision::Sent);
        assert_eq!(store.events_after(acct, 0, 10).unwrap().len(), 1);
    }

    /// THE REACTIVE SQUELCH, the mirror of the deliberate lane's own test: a
    /// rule added AFTER the candidate was built is read at emission and the row
    /// is `suppressed`, not `declined_by_model`. The distinction is the ledger's
    /// whole job here — a decline is the pile the deliberate lane is measured
    /// against rescuing, and the user's own standing instruction does not belong
    /// in it (docs/NOTIFY.md §11.4).
    #[tokio::test]
    async fn a_rule_added_after_ingest_records_suppressed_and_never_a_decline() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(200, verdict(80), false).await;
        let (mid, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        let c = c.expect("a model candidate: no rule existed at ingest");
        assert!(matches!(c, Candidate::Model { .. }));

        // THE USER BLOCKS THE SENDER while the message is being scored.
        store
            .set_sender_rule(
                acct,
                "*@elsewhere.example",
                "not urgent",
                Disposition::Squelch,
            )
            .unwrap();

        let lane = lane(&store, acct, Some(&url), cfg());
        lane.clone().run(c).await;

        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the call had already gone out"
        );
        let row = ledger(&store, acct, mid).expect("recorded");
        assert_eq!(row.decision, NotifyDecision::Suppressed);
        assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
        let text = crate::metrics::render(&lane.metrics, None);
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"fast\",decision=\"suppressed\"} 1\n"
            ),
            "the standing instruction is counted as one"
        );
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"fast\",decision=\"declined_by_model\"} 0\n"
            ),
            "and never as a decline the deliberate lane could rescue"
        );
    }

    /// AN OPENAI DAEMON KEEPS ITS BARE MODEL ID. `is_gateway_url` is
    /// `url != API_URL`, so OpenAI's own endpoint reads as a gateway: qualifying
    /// on the URL alone stapled `anthropic/` onto the id, OpenAI 400s on an
    /// unknown model, `is_config_failure` reads a 400 as config, and the lane
    /// parks itself ten minutes at a time forever with no config value able to
    /// fix it. The prefix belongs to ONE endpoint shape, so the provider is half
    /// the test.
    #[tokio::test]
    async fn an_openai_daemon_puts_the_bare_model_id_on_the_wire() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(200, "{}", false).await;
        let (_, c) = ingest(&store, acct, "g1", &note_eml(now), now, &cfg());
        let lane = Arc::new(NotifyLane::new(
            store.clone(),
            reqwest::Client::new(),
            cfg(),
            Config::default().stage1.known_contact_importance,
            Some(ResolvedLlm {
                api_key: "sk-test".to_string(),
                provider: Stage2Provider::OpenAI,
                url: url.clone(),
            }),
            SyncMetrics::new(),
            acct,
            Arc::new(std::sync::Mutex::new(super::super::WarnDays::default())),
        ));
        assert_eq!(
            lane.cfg.model,
            NotifyConfig::default().model,
            "no `anthropic/` on an OpenAI deployment"
        );

        lane.run(c.expect("a model candidate")).await;

        let req = seen.lock().unwrap().first().cloned().expect("one request");
        let body = req.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let sent: serde_json::Value = serde_json::from_str(&body).expect("a JSON request body");
        assert_eq!(
            sent["model"].as_str(),
            Some(NotifyConfig::default().model.as_str()),
            "OpenAI has never heard of `anthropic/claude-haiku-4-5` and answers \
             400, which parks the lane forever"
        );
    }

    /// THE KILL SWITCH IS FOR THE MODEL PATH, and only that. With a model
    /// configured and the switch off the lane asks nobody, writes `unavailable`
    /// with no model named, and the deliberate lane owns the buzz: the seed
    /// must NOT decide here, because a regex verdict nothing can retract is
    /// exactly what the model path replaced (docs/NOTIFY.md §11.1). The seed
    /// path is for a daemon with no LLM at all, which the switch does not
    /// touch, so flipping it can never silence a mailbox.
    #[tokio::test]
    async fn the_kill_switch_silences_the_model_and_not_the_mailbox() {
        let (store, acct) = store();
        let now = Utc::now();
        let (url, seen) = mock(200, verdict(80), false).await;
        let off = NotifyConfig {
            fast_enabled: false,
            ..cfg()
        };
        let alert = format!(
            "From: Monitoring <alerts@monitoring.example>\r\n\
             To: me@example.com\r\n\
             Subject: Incident: checkout api is down\r\n\
             Date: {}\r\n\
             \r\n\
             A high-severity incident was opened for the checkout service.\r\n",
            now.to_rfc2822()
        );

        // WITH a model configured: no request, no event, and a row that says
        // so in the rescuable word, so the deliberate lane's later `sent` on
        // it reads as the rescue it is.
        let (mid, c) = ingest(&store, acct, "g-alert", &alert, now, &off);
        lane(&store, acct, Some(&url), off.clone())
            .run(c.expect("still a candidate: the switch is not this gate"))
            .await;
        assert_eq!(seen.lock().unwrap().len(), 0, "no model was asked");
        let row = ledger(&store, acct, mid).expect("recorded");
        assert_eq!(row.decision, NotifyDecision::Unavailable);
        assert_eq!(row.model_used, None, "nobody scored it, so nobody is named");
        assert!(
            store.events_after(acct, 0, 10).unwrap().is_empty(),
            "a confident seed does not buzz while a model is configured"
        );

        // WITHOUT a model: the switch changes nothing, and the confident seed
        // is the mailbox's only voice, so it still buzzes.
        let (mid2, c2) = ingest(&store, acct, "g-alert-2", &alert, now, &off);
        lane(&store, acct, None, off)
            .run(c2.expect("candidate"))
            .await;
        let row = ledger(&store, acct, mid2).expect("recorded");
        assert_eq!(row.decision, NotifyDecision::Sent);
        assert_eq!(row.model_used.as_deref(), Some("heuristic"));
        assert_eq!(store.events_after(acct, 0, 10).unwrap().len(), 1);
    }
}
