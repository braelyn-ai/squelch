//! Notification delivery on the human door: `GET /client/events` (SSE) and
//! `GET /client/events/{id}`.
//!
//! The `events` table in squelch-core is the single source of truth; this module
//! is a reader of it and NOTHING ELSE. In particular it never writes a
//! delivered/cursor flag anywhere: every consumer (the resident macOS app today,
//! an iOS device tomorrow) carries its OWN `after=<id>` cursor, which is the one
//! decision that keeps a second client a bolt-on instead of a refactor.
//!
//! Two shapes, one log:
//! - The resident Mac app holds one SSE connection open and renders each frame
//!   through `UNUserNotificationCenter`.
//! - The iOS Notification Service Extension is woken by an opaque APNs ping
//!   carrying only an event id, and fetches exactly that one row by id — which
//!   is why the by-id route exists from day one rather than as a retrofit.
//!
//! SECURITY: sealed mail can never appear here. That is enforced upstream (the
//! emission decision requires `sensitivity='normal'`, and sealing a message
//! retracts its event row), so this module has no sealed-handling of its own —
//! there is nothing to handle.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
    response::{
        IntoResponse, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
};
use serde::Deserialize;
use squelch_core::store::{SqliteStore, Store};
use squelch_core::types::{AccountId, Event};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

use crate::error::ApiError;
use crate::handlers::blocking;
use crate::state::ApiState;

/// Rows one replay read pulls at a time. The replay loops until drained, so this
/// only bounds how long the store mutex is held per read — a client resuming
/// after a week offline must not lock out the sync engine with one giant SELECT.
const REPLAY_BATCH: usize = 100;

/// Frames buffered toward the client before the pump task blocks. Backpressure
/// is the correct behaviour here: a stalled client should slow its own pump, not
/// grow an unbounded queue in the daemon.
const CHANNEL_BUFFER: usize = 64;

/// Comment-ping interval. Idle SSE connections are the classic thing for a proxy
/// (or a laptop's NAT) to reap; a notification feed can legitimately be silent
/// for hours.
const KEEPALIVE: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// The client's own cursor: send everything with `id > after`, then follow.
    after: Option<i64>,
}

/// Where a connection starts reading, given its `after` param and the newest id
/// in the log.
///
/// WITH `after`: exactly there (exclusive). `after=0` is the legitimate "replay
/// the whole log" cursor, so it is honoured; a negative value is clamped to it
/// rather than 400ing, since the only way to produce one is a client bug and the
/// harmless reading is obvious.
///
/// WITHOUT `after`: the newest id, i.e. LIVE-ONLY, no replay. A client with no
/// cursor is a fresh install (or one that lost its state), and replaying the
/// backlog at it would fire a notification storm for mail the user has long
/// since dealt with. A client that wants history asks for it by id.
fn start_cursor(after: Option<i64>, latest: i64) -> i64 {
    match after {
        Some(a) => a.max(0),
        None => latest,
    }
}

/// The wake channel for one connection.
///
/// Prefers the notifier the daemon wired onto the state, falls back to one
/// attached directly to the store (test harnesses, embedders that only did half
/// the wiring), and otherwise hands back a private channel plus its sender as an
/// anchor. The anchor matters: a channel whose sender has been dropped reports
/// `Closed` immediately, which would end the stream on connect. With no notifier
/// in the process nothing can append events either, so parking forever is the
/// honest behaviour — replay and keep-alive still work.
fn subscribe(state: &ApiState) -> (broadcast::Receiver<i64>, Option<broadcast::Sender<i64>>) {
    if let Some(tx) = state.event_notifier() {
        return (tx.subscribe(), None);
    }
    if let Some(tx) = state.store.event_notifier() {
        return (tx.subscribe(), None);
    }
    let (tx, rx) = broadcast::channel(1);
    (rx, Some(tx))
}

/// Encode one event as an SSE frame: the SSE `id:` field is the event id (so a
/// browser-style client's `Last-Event-ID` and our `after=` are the same number),
/// `data:` is the JSON of the core [`Event`].
fn frame(ev: &Event) -> Option<SseEvent> {
    // `id` panics on embedded newlines; an integer has none.
    SseEvent::default().id(ev.id.to_string()).json_data(ev).ok()
}

/// Send every event past `cursor`, oldest first, in bounded batches.
///
/// Returns the advanced cursor, or `None` when the connection is over: the
/// client hung up, or a store read failed. A failed read deliberately ends the
/// stream instead of retrying — the client reconnects with its own cursor, which
/// is the recovery path the whole design already depends on.
async fn pump(
    store: &Arc<SqliteStore>,
    account_id: AccountId,
    mut cursor: i64,
    tx: &mpsc::Sender<Result<SseEvent, Infallible>>,
) -> Option<i64> {
    loop {
        let s = store.clone();
        let batch =
            tokio::task::spawn_blocking(move || s.events_after(account_id, cursor, REPLAY_BATCH))
                .await
                .ok()?
                .ok()?;
        let drained = batch.len() < REPLAY_BATCH;
        for ev in batch {
            // Advance FIRST: an event we cannot encode must not stall the cursor
            // on that row forever.
            cursor = ev.id;
            match frame(&ev) {
                Some(f) => tx.send(Ok(f)).await.ok()?,
                None => continue,
            }
        }
        if drained {
            return Some(cursor);
        }
    }
}

/// Drive one connection until the client leaves or the log's notifier closes.
///
/// `_anchor` keeps a fallback broadcast channel alive for the whole connection;
/// see [`subscribe`].
async fn run(
    store: Arc<SqliteStore>,
    account_id: AccountId,
    start: i64,
    mut wake: broadcast::Receiver<i64>,
    _anchor: Option<broadcast::Sender<i64>>,
    tx: mpsc::Sender<Result<SseEvent, Infallible>>,
) {
    // Replay first (a no-op for a live-only connection, whose cursor is already
    // the newest id).
    let mut cursor = match pump(&store, account_id, start, &tx).await {
        Some(c) => c,
        None => return,
    };

    loop {
        tokio::select! {
            recv = wake.recv() => match recv {
                // A new id, or we fell behind and lost some wakes — identical
                // handling, because the payload is only a hint and the table is
                // the truth. Re-read from OUR cursor and everything missed comes
                // back.
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    cursor = match pump(&store, account_id, cursor, &tx).await {
                        Some(c) => c,
                        None => return,
                    };
                }
                // The store (and with it the process) is going away.
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // The client hung up. A send would notice too, but a send needs a
            // wake and a quiet mailbox may never deliver one — without this arm
            // every closed connection parks a task here until the next append.
            _ = tx.closed() => return,
        }
    }
}

/// Resolve when the daemon starts shutting down, or never if nothing wired a
/// shutdown signal in. A dropped sender counts as shutdown: the thing that owned
/// the signal is gone.
async fn shutting_down(shutdown: Option<watch::Receiver<bool>>) {
    match shutdown {
        Some(mut rx) => {
            let _ = rx.wait_for(|stopping| *stopping).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// `GET /client/events` — the live notification feed, `text/event-stream`.
///
/// `?after=<id>` replays everything past that cursor and then follows. WITHOUT
/// `after` there is NO replay at all: see [`start_cursor`] for why a cursorless
/// client must not be handed the backlog.
///
/// Nothing about the connection is persisted. Cursors are per-connection local
/// state and stay client-owned.
pub async fn events_stream(
    State(state): State<ApiState>,
    Query(q): Query<EventsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    // SUBSCRIBE BEFORE READING THE CURSOR. Anything appended in the window
    // between the two lands in the broadcast queue, and the wake makes us re-read
    // the table past our cursor — so the replay/live seam cannot drop an event.
    // Doing it the other way round is a silent, unreproducible lost notification.
    let (wake, anchor) = subscribe(&state);

    let store = state.store.clone();
    let account_id = state.account_id;
    let latest = {
        let store = store.clone();
        blocking(move || store.latest_event_id(account_id)).await?
    };
    let start = start_cursor(q.after, latest);

    let (tx, rx) = mpsc::channel::<Result<SseEvent, Infallible>>(CHANNEL_BUFFER);
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        // One shutdown gate over BOTH replay and follow. On shutdown the pump
        // future is dropped mid-flight, `tx` drops, and the response body ends —
        // which is what lets axum's graceful shutdown actually finish. Losing an
        // in-flight frame is fine and by design: the client reconnects with its
        // own cursor and re-reads it.
        tokio::select! {
            _ = shutting_down(shutdown) => {}
            _ = run(store, account_id, start, wake, anchor, tx) => {}
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::new().interval(KEEPALIVE)))
}

/// `GET /client/events/{id}` — one event as JSON, scoped to the account.
///
/// This is what the iOS Notification Service Extension calls after an opaque
/// push, over the user's own tailnet. 404 for an unknown id AND for another
/// account's id, identically: an id is a guessable integer, so the response must
/// not distinguish "no such event" from "not yours".
pub async fn get_event(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.store.clone();
    let account_id = state.account_id;
    let event = blocking(move || store.event_by_id(account_id, id)).await?;
    event.map(Json).ok_or_else(ApiError::not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cursor_starts_live_only() {
        // A fresh install joins at the head: 42 events already in the log, and it
        // is told about none of them.
        assert_eq!(start_cursor(None, 42), 42);
        // An empty log is the same rule, and lands on the "replay everything"
        // cursor by coincidence rather than by exception.
        assert_eq!(start_cursor(None, 0), 0);
    }

    #[test]
    fn a_cursor_is_honoured_verbatim() {
        assert_eq!(start_cursor(Some(7), 42), 7);
        // Explicit zero means "I have never seen anything; replay the log".
        assert_eq!(start_cursor(Some(0), 42), 0);
        // A cursor past the head is not an error: nothing to replay, and the next
        // real event still has a bigger id.
        assert_eq!(start_cursor(Some(99), 42), 99);
        // Garbage clamps to the harmless reading instead of 400ing.
        assert_eq!(start_cursor(Some(-5), 42), 0);
    }
}
