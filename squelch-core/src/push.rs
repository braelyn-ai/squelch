//! The APNs pusher: the second delivery adapter on the durable `events` log.
//!
//! THE RELAY IS BLIND — this task reads full [`Event`] rows but serializes only
//! [`PushRequest`] (event id + collapse id), so adding a field there is a
//! security change. Its own `sync_state` cursor ([`CURSOR_KEY`]) advances only
//! after a delivery: at-least-once, never a skip. Bearer and device tokens are
//! never logged.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};

use crate::config::Config;
use crate::error::{CoreError, Result};
use crate::store::{Store, SyncState};
use crate::types::{AccountId, Event};

/// The pusher's cursor row in `sync_state`: `uidvalidity` unused, `last_uid`
/// holds the last event id the relay accepted. A key of its own, distinct from
/// the sync engine's `'history'` row, so neither channel can stall the other.
pub const CURSOR_KEY: &str = "apns_push_cursor";

/// Events read per store round-trip. The drain loops until the table is caught
/// up, so this only bounds how long the store mutex is held per read.
const BATCH: usize = 50;

/// The relay's own per-request token ceiling. Registered devices are chunked to
/// it rather than truncated — every device gets the ping.
const MAX_TOKENS_PER_REQUEST: usize = 100;

/// APNs caps `apns-collapse-id` at 64 BYTES and rejects anything longer.
const MAX_COLLAPSE_ID: usize = 64;

/// Coarse fallback tick. The broadcast wake is the fast path; this covers a
/// missed or `Lagged` wake and a daemon that started with a stale cursor.
const IDLE_INTERVAL: Duration = Duration::from_secs(60);

/// Backoff after a relay failure: the events are already durable, so there is no
/// reason to hammer a relay that is down.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Whole-request budget. The relay's own fan-out budget is 30s, so anything past
/// ~35s means the relay itself is gone rather than slow.
const HTTP_TIMEOUT: Duration = Duration::from_secs(35);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The complete wire body, and THE PRIVACY BOUNDARY: the content-bearing fields
/// of [`Event`] (sender, one_line, tier, importance, kind, deadline) are absent
/// and must stay absent. Adding a field here is a security change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushRequest<'a> {
    pub device_tokens: &'a [String],
    /// Monotonic event id; the relay forwards it verbatim, never interprets it.
    pub event_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<&'a str>,
}

/// One token's outcome, as the relay reports it. `status` is the APNs HTTP
/// status VERBATIM; `410` is data (delete the row), not an error.
#[derive(Debug, Clone, Deserialize)]
struct PushResult {
    token: String,
    status: u16,
}

#[derive(Debug, Clone, Deserialize)]
struct PushResponse {
    #[serde(default)]
    results: Vec<PushResult>,
}

/// What one event's fan-out produced across every token chunk. The relay
/// answering 200 only means the RELAY is up; each token carries its own APNs
/// status, and this tally decides whether the cursor may move.
#[derive(Debug, Default, PartialEq, Eq)]
struct PushOutcome {
    /// Tokens APNs retired (`410`), for the caller to delete.
    dead: Vec<String>,
    /// Tokens APNs accepted (`200`).
    delivered: usize,
    /// Tokens that neither landed nor retired — `0` for an APNs the relay could
    /// not reach or that timed out, `429` throttling, `5xx`. All retryable.
    failed: usize,
}

impl PushOutcome {
    /// Nothing landed and something retryable failed: the APNs hop is out while
    /// the relay hop is fine — leave the cursor, back off, retry. Deliberately
    /// NOT "any failure": a partial delivery advances, because retrying would
    /// re-push the event to every device that already got it.
    fn nothing_landed(&self) -> bool {
        self.delivered == 0 && self.failed > 0
    }
}

/// `thread-<thread id>`, clamped to APNs' 64-BYTE collapse-id limit on a char
/// boundary (Gmail thread ids are hex, but the truncation must not be able to
/// split a UTF-8 sequence and produce a body the relay rejects).
fn collapse_id(thread_id: &str) -> String {
    const PREFIX: &str = "thread-";
    let budget = MAX_COLLAPSE_ID - PREFIX.len();
    let mut end = thread_id.len().min(budget);
    while end > 0 && !thread_id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{PREFIX}{}", &thread_id[..end])
}

fn next_backoff(current: Option<Duration>) -> Duration {
    match current {
        None => BACKOFF_BASE,
        Some(d) => (d * 2).min(BACKOFF_CAP),
    }
}

/// A device token reduced to something safe to log: at most 8 characters, enough
/// to correlate with a `devices` row and useless as a capability.
fn token_prefix(token: &str) -> &str {
    let mut end = token.len().min(8);
    while end > 0 && !token.is_char_boundary(end) {
        end -= 1;
    }
    &token[..end]
}

/// The pusher's HTTP client. REDIRECTS ARE REFUSED: every request carries the
/// relay bearer, and reqwest's default policy would let a `307` walk that
/// token-bearing POST to a host of the relay's choosing. Off, a 3xx is simply a
/// non-2xx — backoff, cursor unmoved.
fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// The APNs pusher task. Construct with [`Pusher::from_config`], which returns
/// `Ok(None)` when no relay is configured.
pub struct Pusher {
    store: Arc<dyn Store>,
    account_id: AccountId,
    /// Fully-resolved `{relay_url}/v1/push`.
    push_url: String,
    /// The relay bearer. NEVER logged, never included in an error.
    relay_token: Option<String>,
    topic: Option<String>,
    environment: Option<String>,
    http: reqwest::Client,
}

impl Pusher {
    /// Build the pusher, or `Ok(None)` when `pusher.relay_url` is unset — the
    /// feature flag for the whole thing, so a daemon with no relay configured
    /// never builds a client aimed at one. Disabled and broken stay distinct
    /// answers: `Err` means a relay WAS named and the client could not be built.
    pub fn from_config(
        store: Arc<dyn Store>,
        account_id: AccountId,
        config: &Config,
    ) -> Result<Option<Self>> {
        let Some(relay_url) = config
            .pusher
            .relay_url
            .as_deref()
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
        else {
            return Ok(None);
        };
        // The error carries no url and no bearer; neither reached the builder.
        let http = http_client()
            .map_err(|e| CoreError::Other(anyhow::anyhow!("APNs relay HTTP client: {e}")))?;
        Ok(Some(Self {
            store,
            account_id,
            push_url: format!("{}/v1/push", relay_url.trim_end_matches('/')),
            relay_token: config.pusher.relay_token.clone().filter(|t| !t.is_empty()),
            topic: config.pusher.topic.clone().filter(|t| !t.is_empty()),
            environment: config.pusher.environment.clone().filter(|t| !t.is_empty()),
            http,
        }))
    }

    /// TEST HOOK: aim a pusher at an explicit base URL (a mock relay on an
    /// ephemeral port) without going through env/config.
    #[doc(hidden)]
    pub fn for_test(store: Arc<dyn Store>, account_id: AccountId, base_url: &str) -> Self {
        Self {
            store,
            account_id,
            push_url: format!("{}/v1/push", base_url.trim_end_matches('/')),
            relay_token: None,
            topic: None,
            environment: None,
            // The SAME client the real path uses, redirect policy included —
            // otherwise the redirect test would prove nothing about production.
            http: http_client().expect("test http client"),
        }
    }

    /// TEST HOOK: set the relay bearer / topic / environment pass-throughs.
    #[doc(hidden)]
    pub fn with_relay_auth(mut self, token: Option<&str>) -> Self {
        self.relay_token = token.map(|t| t.to_string());
        self
    }

    /// The pusher's persisted cursor. A first-ever start has none and joins at
    /// the HEAD of the log: a phone registering today must not get the backlog.
    fn cursor(&self) -> Result<i64> {
        match self.store.sync_state(self.account_id, CURSOR_KEY)? {
            Some(s) => Ok(s.last_uid as i64),
            None => {
                let latest = self.store.latest_event_id(self.account_id)?;
                self.set_cursor(latest)?;
                Ok(latest)
            }
        }
    }

    fn set_cursor(&self, id: i64) -> Result<()> {
        self.store.set_sync_state(
            self.account_id,
            CURSOR_KEY,
            &SyncState {
                uidvalidity: 0,
                last_uid: id.max(0) as u64,
            },
        )
    }

    /// Push everything past the cursor, oldest first, one event at a time.
    /// Returns `Err` the moment the relay stops answering 200 — or a 200 carries
    /// no delivered token — with the cursor left where the last accepted event
    /// put it, so unsent events are retried rather than skipped.
    async fn drain(&self) -> Result<()> {
        let mut cursor = self.cursor()?;
        loop {
            let batch = self.store.events_after(self.account_id, cursor, BATCH)?;
            if batch.is_empty() {
                return Ok(());
            }
            let mut tokens: Vec<String> = self
                .store
                .list_devices(self.account_id)?
                .into_iter()
                .map(|d| d.token)
                .collect();

            // Nothing registered: an event nobody can receive is history, not
            // backlog. Advance without opening a socket to the relay.
            if tokens.is_empty() {
                if let Some(last) = batch.last() {
                    cursor = last.id;
                    self.set_cursor(cursor)?;
                }
                continue;
            }

            for ev in &batch {
                let outcome = self.push_event(ev, &tokens).await?;
                let dead = &outcome.dead;
                for token in dead {
                    // The relay passes APNs' 410 back verbatim so THIS daemon
                    // owns the cleanup; the relay remembers nothing.
                    match self.store.delete_device_by_token(self.account_id, token) {
                        Ok(true) => eprintln!(
                            "squelch: APNs pusher dropped an unregistered device (token {}…)",
                            token_prefix(token)
                        ),
                        Ok(false) => {}
                        Err(e) => eprintln!("squelch: APNs pusher could not drop a device: {e}"),
                    }
                }
                if !dead.is_empty() {
                    tokens.retain(|t| !dead.contains(t));
                    if tokens.is_empty() {
                        // Every device just died. The remaining events have
                        // nowhere to go; skip to the end of the batch.
                        cursor = batch.last().map(|e| e.id).unwrap_or(cursor);
                        self.set_cursor(cursor)?;
                        break;
                    }
                }
                // The relay answered, APNs did not: every surviving token came
                // back 0/429/5xx, so this event reached nobody. The relay's own
                // 200 is not evidence of delivery — bail, cursor unmoved.
                if outcome.nothing_landed() {
                    eprintln!(
                        "squelch: APNs took none of {} device(s) for this event; cursor unmoved",
                        outcome.failed
                    );
                    return Err(relay_failure());
                }
                // Advance only after a delivery — the at-least-once guarantee:
                // a hop dying mid-batch leaves the rest to be retried.
                cursor = ev.id;
                self.set_cursor(cursor)?;
            }
        }
    }

    /// POST one event to the relay for every registered token (chunked to the
    /// relay's ceiling), returning the per-token tally: a 200 from the relay
    /// says nothing about whether APNs took anything.
    async fn push_event(&self, ev: &Event, tokens: &[String]) -> Result<PushOutcome> {
        let collapse = collapse_id(&ev.thread_id);
        let mut outcome = PushOutcome::default();
        for chunk in tokens.chunks(MAX_TOKENS_PER_REQUEST) {
            let body = PushRequest {
                device_tokens: chunk,
                event_id: ev.id,
                collapse_id: Some(collapse.clone()),
                topic: self.topic.as_deref(),
                environment: self.environment.as_deref(),
            };
            let mut req = self.http.post(&self.push_url).json(&body);
            if let Some(token) = &self.relay_token {
                req = req.bearer_auth(token);
            }
            // The error is NOT propagated: reqwest's Display embeds the URL and
            // the builder held the bearer. Only the class of failure escapes.
            let resp = req.send().await.map_err(|_| {
                eprintln!("squelch: APNs relay unreachable; cursor unmoved");
                relay_failure()
            })?;
            let status = resp.status();
            if !status.is_success() {
                eprintln!(
                    "squelch: APNs relay refused the push (HTTP {}); cursor unmoved",
                    status.as_u16()
                );
                return Err(relay_failure());
            }
            let parsed: PushResponse = resp.json().await.map_err(|_| {
                eprintln!("squelch: APNs relay returned an unreadable body; cursor unmoved");
                relay_failure()
            })?;
            for result in parsed.results {
                match result.status {
                    200 => outcome.delivered += 1,
                    // The device is gone. Data, not an error.
                    410 => outcome.dead.push(result.token),
                    // Retryable: `0` is the relay's word for an unreachable or
                    // timed-out APNs; 429/5xx come through verbatim. Log the
                    // status only — never the token value or a reason string.
                    other => {
                        outcome.failed += 1;
                        eprintln!(
                            "squelch: APNs push for one device returned status {other} (token {}…)",
                            token_prefix(&result.token)
                        );
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// Run until shutdown, waking on the events broadcast, a coarse
    /// [`IDLE_INTERVAL`] tick, or the shutdown watch. After a relay failure only
    /// shutdown may cut the backoff short — a wake must not turn a dead relay
    /// into a hot retry loop.
    pub async fn run(
        self,
        mut wake: broadcast::Receiver<i64>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let mut backoff: Option<Duration> = None;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            // Drain FIRST: a daemon that restarts with a stale cursor catches up
            // immediately instead of waiting out an idle interval.
            match self.drain().await {
                Ok(()) => backoff = None,
                Err(_) => {
                    let delay = next_backoff(backoff);
                    eprintln!(
                        "squelch: APNs pusher backing off {}s (events are durable; nothing is lost)",
                        delay.as_secs()
                    );
                    backoff = Some(delay);
                }
            }

            match backoff {
                Some(delay) => {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() { return Ok(()); }
                        }
                    }
                }
                None => {
                    tokio::select! {
                        _ = tokio::time::sleep(IDLE_INTERVAL) => {}
                        recv = wake.recv() => {
                            // A new id and `Lagged` are handled identically:
                            // the payload is a hint, the table is the truth.
                            if matches!(recv, Err(broadcast::error::RecvError::Closed)) {
                                return Ok(());
                            }
                        }
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() { return Ok(()); }
                        }
                    }
                }
            }
        }
    }
}

/// One opaque failure value for every relay problem. Carries NO url, NO status
/// text, and above all no bearer — the specific cause is already on stderr.
fn relay_failure() -> CoreError {
    CoreError::Other(anyhow::anyhow!("relay push failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EventKind, Tier};
    use chrono::Utc;

    fn event(id: i64, thread_id: &str) -> Event {
        Event {
            id,
            kind: EventKind::Urgent,
            message_id: 7,
            thread_id: thread_id.to_string(),
            tier: Tier::PastDue,
            importance: 95,
            sender: "alice@example.com".to_string(),
            one_line: "wire transfer needs approval today".to_string(),
            deadline: Some("2026-08-01T00:00:00Z".to_string()),
            created_at: Utc::now(),
        }
    }

    /// THE INVARIANT: if this test fails, the relay stopped being blind.
    #[test]
    fn body_is_blind() {
        let ev = event(4711, "abc123");
        let tokens = vec!["aa".repeat(32)];
        let body = PushRequest {
            device_tokens: &tokens,
            event_id: ev.id,
            collapse_id: Some(collapse_id(&ev.thread_id)),
            topic: Some("dev.squelch.ios"),
            environment: Some("production"),
        };
        let json = serde_json::to_value(&body).unwrap();
        let obj = json.as_object().unwrap();

        // The shape is closed: exactly these keys, nothing else.
        let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "collapse_id",
                "device_tokens",
                "environment",
                "event_id",
                "topic"
            ]
        );
        assert_eq!(obj["event_id"], serde_json::json!(4711));
        assert_eq!(obj["collapse_id"], serde_json::json!("thread-abc123"));

        // And nothing about the mail survives into the encoded bytes.
        let raw = serde_json::to_string(&body).unwrap();
        for leak in [
            "alice@example.com",
            "wire transfer",
            "past_due",
            "urgent",
            "95",
            "2026-08-01",
        ] {
            assert!(!raw.contains(leak), "content leaked onto the wire: {leak}");
        }
    }

    /// The optional pass-throughs vanish from the body entirely when unset, so a
    /// default deployment sends the smallest possible request.
    #[test]
    fn optional_passthroughs_are_omitted_not_nulled() {
        let tokens = vec!["bb".repeat(32)];
        let body = PushRequest {
            device_tokens: &tokens,
            event_id: 1,
            collapse_id: None,
            topic: None,
            environment: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("device_tokens"));
        assert!(obj.contains_key("event_id"));
    }

    #[test]
    fn collapse_id_clamps_to_64_bytes_on_a_char_boundary() {
        assert_eq!(collapse_id("abc"), "thread-abc");
        assert_eq!(collapse_id(""), "thread-");

        // Long ASCII: exactly 64 bytes, never more.
        let long = "f".repeat(200);
        let c = collapse_id(&long);
        assert_eq!(c.len(), MAX_COLLAPSE_ID);
        assert!(c.starts_with("thread-"));

        // A multi-byte thread id truncates DOWN to a boundary rather than
        // splitting a sequence into invalid UTF-8 the relay would reject.
        let wide = "é".repeat(100); // 2 bytes each
        let c = collapse_id(&wide);
        assert!(c.len() <= MAX_COLLAPSE_ID);
        assert!(c.is_char_boundary(c.len()));
        // 57 bytes of budget holds 28 two-byte chars (56 bytes), not 28.5.
        assert_eq!(c, format!("thread-{}", "é".repeat(28)));
    }

    #[test]
    fn backoff_doubles_from_the_base_and_caps() {
        assert_eq!(next_backoff(None), BACKOFF_BASE);
        assert_eq!(next_backoff(Some(BACKOFF_BASE)), BACKOFF_BASE * 2);
        assert_eq!(next_backoff(Some(BACKOFF_CAP)), BACKOFF_CAP);
        assert_eq!(next_backoff(Some(BACKOFF_CAP * 4)), BACKOFF_CAP);
    }

    /// The relay answering 200 is not delivery: APNs taking nothing is a failure
    /// the cursor must not survive; a partial delivery is not.
    #[test]
    fn nothing_landed_is_the_apns_hop_being_out() {
        let outcome = |delivered, failed, dead: &[&str]| PushOutcome {
            dead: dead.iter().map(|t| t.to_string()).collect(),
            delivered,
            failed,
        };

        // Every token came back 0/429/5xx: nobody got it, retry.
        assert!(outcome(0, 3, &[]).nothing_landed());
        assert!(outcome(0, 1, &["gone"]).nothing_landed());

        // Something landed: the event was delivered, however partially.
        assert!(!outcome(1, 2, &[]).nothing_landed());
        assert!(!outcome(3, 0, &[]).nothing_landed());

        // Nothing failed. Zero delivered here means every device retired (410),
        // which is cleanup the drain loop already handles — not a retry.
        assert!(!outcome(0, 0, &["a", "b"]).nothing_landed());
        assert!(!PushOutcome::default().nothing_landed());
    }

    #[test]
    fn token_prefix_never_exposes_the_capability() {
        let token = "abcdef0123456789".repeat(4);
        assert_eq!(token_prefix(&token), "abcdef01");
        assert_eq!(token_prefix("short"), "short");
        assert_eq!(token_prefix(""), "");
    }

    /// No relay configured => no pusher, and `Ok(None)` rather than `Err`:
    /// disabled is the normal case, not a misconfiguration.
    #[test]
    fn from_config_is_none_without_a_relay_url() {
        let store: Arc<dyn Store> = Arc::new(crate::store::SqliteStore::open_in_memory().unwrap());
        let mut config = Config::default();
        assert!(
            Pusher::from_config(store.clone(), 1, &config)
                .expect("no relay is not an error")
                .is_none()
        );

        config.pusher.relay_url = Some("   ".to_string());
        assert!(
            Pusher::from_config(store.clone(), 1, &config)
                .expect("a blank relay is not an error")
                .is_none()
        );

        config.pusher.relay_url = Some("https://relay.example.com/".to_string());
        let p = Pusher::from_config(store, 1, &config)
            .expect("building the client")
            .expect("a named relay builds a pusher");
        // The trailing slash does not become a double slash on the path.
        assert_eq!(p.push_url, "https://relay.example.com/v1/push");
    }
}
