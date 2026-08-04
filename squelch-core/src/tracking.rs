//! Outbound read tracking: minting the per-send token, and the poller that
//! drains observed opens off the relay.
//!
//! THE TOKEN IS THE WHOLE SECRET. It is the only thing that leaves this machine
//! (as the path segment of the pixel URL), it is what any open sink presents
//! back, and it is never logged. A tracker that does not exist records nothing,
//! so a stranger guessing at `/t/…` cannot manufacture an open.
//!
//! The poller is the pusher's mirror image on the same relay: same bearer, same
//! refusal to follow redirects, same persisted `sync_state` cursor, advanced
//! only after a row is durably written. Presenting cursor `N` is also what tells
//! the relay it may delete rows `<= N`, so a cursor that runs ahead of the
//! store would delete an open nobody recorded.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use tokio::sync::watch;

use crate::config::Config;
use crate::error::{CoreError, Result};
use crate::store::{NewEvent, OPEN_PROXIED, OPEN_UNKNOWN, Store, SyncState};
use crate::types::{AccountId, EventKind, Tier};

/// The opens poller's cursor row in `sync_state`: `uidvalidity` unused,
/// `last_uid` holds the highest relay row id durably written here. Its own key,
/// distinct from the pusher's and the sync engine's.
pub const CURSOR_KEY: &str = "opens_poll_cursor";

/// Entropy per token. 24 bytes is 192 bits and encodes to EXACTLY 32 unpadded
/// base64url characters, so the minted token needs no truncation (which would
/// silently cost entropy) and no padding (which would need escaping in a URL).
const TOKEN_BYTES: usize = 24;

/// How often the relay is asked for new opens. Opens are not time-critical and
/// the relay holds them until the cursor collects them, so this is deliberately
/// lazy.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Backoff after a relay failure: the relay keeps the rows until we present a
/// cursor past them, so there is nothing to lose by waiting.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_secs(300);

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The `sender` on an `opened` event is the ACCOUNT'S OWN address (it is the
/// user's own outbound mail that was read), and `one_line` is that message's
/// subject. `importance` is the default notify threshold rather than a score —
/// nothing modelled an open — so a client filtering on the threshold does not
/// silently drop them.
const OPENED_IMPORTANCE: u8 = 50;

/// Mint a fresh tracking token: 32 url-safe characters over 192 bits of OS
/// entropy. `Err` only when the OS refuses to produce randomness, which must
/// fail the send's tracking rather than fall back to something guessable.
pub fn mint_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|_| CoreError::Other(anyhow::anyhow!("no OS entropy for a tracking token")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// What a fetch's `User-Agent` implies about the open. Gmail proxies every
/// remote image through `GoogleImageProxy`, and that fetch can be a cache warm
/// rather than a human reading, so it is labelled rather than counted as a read.
/// Everything else — including no User-Agent at all — is `unknown`.
pub fn classify_user_agent(user_agent: Option<&str>) -> &'static str {
    match user_agent {
        Some(ua) if ua.contains("GoogleImageProxy") => OPEN_PROXIED,
        _ => OPEN_UNKNOWN,
    }
}

/// One open as the relay reports it. `id` is the relay's own row id and the
/// cursor's unit; it is never stored here, only compared.
#[derive(Debug, Clone, Deserialize)]
struct RelayOpen {
    id: i64,
    token: String,
    opened_at: i64,
    #[serde(default)]
    user_agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OpensResponse {
    #[serde(default)]
    opens: Vec<RelayOpen>,
}

fn next_backoff(current: Option<Duration>) -> Duration {
    match current {
        None => BACKOFF_BASE,
        Some(d) => (d * 2).min(BACKOFF_CAP),
    }
}

/// The poller's HTTP client. REDIRECTS ARE REFUSED, for the same reason
/// [`crate::push`] refuses them: every request carries the relay bearer, and the
/// default policy would let a `307` walk that token to a host of the relay's
/// choosing. Off, a 3xx is simply a non-2xx — backoff, cursor unmoved.
fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// One opaque failure value for every relay problem. Carries NO url and above
/// all no bearer — the class of failure is already on stderr.
fn relay_failure() -> CoreError {
    CoreError::Other(anyhow::anyhow!("relay opens poll failed"))
}

/// The opens poller task. Construct with [`OpensPoller::from_config`], which
/// returns `Ok(None)` when no relay is configured.
pub struct OpensPoller {
    store: Arc<dyn Store>,
    account_id: AccountId,
    /// Fully-resolved `{relay_url}/v1/opens`.
    opens_url: String,
    /// The relay bearer. NEVER logged, never included in an error.
    relay_token: Option<String>,
    http: reqwest::Client,
}

impl OpensPoller {
    /// Build the poller, or `Ok(None)` when `pusher.relay_url` is unset — the
    /// relay is the only place remote opens can come from, so no relay means no
    /// task and no socket. `Err` means a relay WAS named and the client could
    /// not be built.
    ///
    /// NOTE the asymmetry with `[tracking] base_url`: that gates MINTING (can a
    /// pixel exist at all), this gates COLLECTION through the relay. The
    /// daemon's own `/t/:token` route needs neither.
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
            .map_err(|e| CoreError::Other(anyhow::anyhow!("opens poller HTTP client: {e}")))?;
        Ok(Some(Self {
            store,
            account_id,
            opens_url: format!("{}/v1/opens", relay_url.trim_end_matches('/')),
            relay_token: config.pusher.relay_token.clone().filter(|t| !t.is_empty()),
            http,
        }))
    }

    /// TEST HOOK: aim a poller at an explicit base URL (a mock relay on an
    /// ephemeral port) without going through env/config.
    #[doc(hidden)]
    pub fn for_test(store: Arc<dyn Store>, account_id: AccountId, base_url: &str) -> Self {
        Self {
            store,
            account_id,
            opens_url: format!("{}/v1/opens", base_url.trim_end_matches('/')),
            relay_token: None,
            // The SAME client the real path uses, redirect policy included.
            http: http_client().expect("test http client"),
        }
    }

    /// TEST HOOK: set the relay bearer.
    #[doc(hidden)]
    pub fn with_relay_auth(mut self, token: Option<&str>) -> Self {
        self.relay_token = token.map(|t| t.to_string());
        self
    }

    /// The poller's persisted cursor. A first-ever start has none and joins at
    /// ZERO — the opposite of the pusher, which joins at HEAD: the relay is
    /// holding these rows FOR this daemon and deletes them only once collected,
    /// so skipping to the end would throw away opens that already happened.
    fn cursor(&self) -> Result<i64> {
        Ok(match self.store.sync_state(self.account_id, CURSOR_KEY)? {
            Some(s) => s.last_uid as i64,
            None => 0,
        })
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

    async fn fetch(&self, cursor: i64) -> Result<Vec<RelayOpen>> {
        let mut req = self.http.get(&self.opens_url).query(&[("cursor", cursor)]);
        if let Some(token) = &self.relay_token {
            req = req.bearer_auth(token);
        }
        // The error is NOT propagated: reqwest's Display embeds the URL and the
        // builder held the bearer. Only the class of failure escapes.
        let resp = req.send().await.map_err(|_| {
            eprintln!("squelch: opens relay unreachable; cursor unmoved");
            relay_failure()
        })?;
        let status = resp.status();
        if !status.is_success() {
            eprintln!(
                "squelch: opens relay refused the poll (HTTP {}); cursor unmoved",
                status.as_u16()
            );
            return Err(relay_failure());
        }
        let parsed: OpensResponse = resp.json().await.map_err(|_| {
            eprintln!("squelch: opens relay returned an unreadable body; cursor unmoved");
            relay_failure()
        })?;
        Ok(parsed.opens)
    }

    /// Collect everything past the cursor, page by page, until the relay has
    /// nothing left. Each row is written BEFORE its id becomes the cursor, so a
    /// crash re-collects rather than loses; the relay's own dedupe is the cursor
    /// it was last shown.
    async fn drain(&self) -> Result<()> {
        loop {
            let cursor = self.cursor()?;
            let opens = self.fetch(cursor).await?;
            if opens.is_empty() {
                return Ok(());
            }
            // A page whose ids are all at or below the cursor cannot move it;
            // collecting it again would spin forever against a broken relay.
            if opens.iter().all(|o| o.id <= cursor) {
                eprintln!("squelch: opens relay replayed collected rows; stopping this pass");
                return Ok(());
            }

            let mut newly_opened: Vec<String> = Vec::new();
            for open in &opens {
                if open.id <= cursor {
                    continue;
                }
                let recorded = self.store.record_open(
                    self.account_id,
                    &open.token,
                    open.opened_at,
                    open.user_agent.as_deref(),
                    classify_user_agent(open.user_agent.as_deref()),
                )?;
                // Durable first, cursor second — and per row, so a failure
                // mid-page leaves the rest to be re-collected.
                self.set_cursor(open.id)?;
                if recorded && !newly_opened.iter().any(|t| t == &open.token) {
                    newly_opened.push(open.token.clone());
                }
            }
            for token in &newly_opened {
                self.notify_open(token);
            }
        }
    }

    /// Append the `opened` event that wakes the existing SSE + APNs pipeline.
    /// BEST-EFFORT: the open is already durable, and `events` is UNIQUE on
    /// `message_id`, so the second open of a message is silently collapsed into
    /// the first — one nudge per message, not per pixel fetch.
    fn notify_open(&self, token: &str) {
        let tracked = match self.store.tracked_message(self.account_id, token) {
            // The tracker exists but its echo never backfilled a local message
            // id: the open is recorded, there is just nothing to point a
            // notification at.
            Ok(None) => return,
            Ok(Some(t)) => t,
            Err(e) => {
                eprintln!("squelch: opens poller could not resolve a tracked message: {e}");
                return;
            }
        };
        let ev = NewEvent {
            account_id: self.account_id,
            message_id: tracked.message_id,
            thread_id: tracked.thread_id,
            kind: EventKind::Opened,
            tier: Tier::Signal,
            importance: OPENED_IMPORTANCE,
            sender: tracked.from_addr,
            one_line: tracked.subject,
            deadline: None,
        };
        if let Err(e) = self.store.append_event(&ev) {
            eprintln!("squelch: opens poller could not append an opened event: {e}");
        }
    }

    /// Run until shutdown, waking on the poll tick or the shutdown watch. After
    /// a relay failure the backoff replaces the tick; only shutdown cuts it
    /// short, so a dead relay never becomes a hot retry loop.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut backoff: Option<Duration> = None;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            // Drain FIRST: a daemon that restarts with a stale cursor collects
            // immediately instead of waiting out a poll interval.
            match self.drain().await {
                Ok(()) => backoff = None,
                Err(_) => {
                    let delay = next_backoff(backoff);
                    eprintln!(
                        "squelch: opens poller backing off {}s (the relay holds the rows; nothing is lost)",
                        delay.as_secs()
                    );
                    backoff = Some(delay);
                }
            }

            let delay = backoff.unwrap_or(POLL_INTERVAL);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    #[test]
    fn minted_tokens_are_32_url_safe_chars_and_never_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let t = mint_token().expect("OS entropy");
            assert_eq!(t.len(), 32, "24 bytes is exactly 32 unpadded base64url chars");
            assert!(
                t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "token must survive a URL path segment unescaped: {t}"
            );
            assert!(seen.insert(t), "minted the same token twice");
        }
    }

    #[test]
    fn only_the_google_proxy_is_classified_proxied() {
        assert_eq!(
            classify_user_agent(Some(
                "Mozilla/5.0 (Windows NT 5.1; rv:11.0) Gecko Firefox/11.0 (via ggpht.com GoogleImageProxy)"
            )),
            OPEN_PROXIED
        );
        assert_eq!(classify_user_agent(Some("Apple Mail/16.0")), OPEN_UNKNOWN);
        assert_eq!(classify_user_agent(Some("")), OPEN_UNKNOWN);
        assert_eq!(classify_user_agent(None), OPEN_UNKNOWN);
    }

    /// No relay configured => no poller, and `Ok(None)` rather than `Err`.
    #[test]
    fn from_config_is_none_without_a_relay_url() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut config = Config::default();
        assert!(
            OpensPoller::from_config(store.clone(), 1, &config)
                .expect("no relay is not an error")
                .is_none()
        );

        config.pusher.relay_url = Some("   ".to_string());
        assert!(
            OpensPoller::from_config(store.clone(), 1, &config)
                .expect("a blank relay is not an error")
                .is_none()
        );

        // A tracking base_url alone does NOT start the poller: minting and
        // collecting are separately configured.
        config.pusher.relay_url = None;
        config.tracking.base_url = Some("https://track.example.com".to_string());
        assert!(
            OpensPoller::from_config(store.clone(), 1, &config)
                .expect("tracking without a relay is not an error")
                .is_none()
        );

        config.pusher.relay_url = Some("https://relay.example.com/".to_string());
        let p = OpensPoller::from_config(store, 1, &config)
            .expect("building the client")
            .expect("a named relay builds a poller");
        // The trailing slash does not become a double slash on the path.
        assert_eq!(p.opens_url, "https://relay.example.com/v1/opens");
    }

    /// A first-ever start collects from zero, not from HEAD: the relay is
    /// holding rows for this daemon and only deletes what it has handed over.
    #[test]
    fn a_fresh_cursor_starts_at_zero() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = SqliteStore::open_in_memory().unwrap().ensure_account("x@y.z").unwrap();
        let poller = OpensPoller::for_test(store, acct, "http://127.0.0.1:1");
        assert_eq!(poller.cursor().unwrap(), 0);
        poller.set_cursor(41).unwrap();
        assert_eq!(poller.cursor().unwrap(), 41);
    }
}
