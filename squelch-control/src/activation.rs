//! The activation poller: the one writer of `users.first_paired_at`.
//!
//! Issue #89's question — did anyone we invited ever run the client? — is
//! answered by the tenant's own daemon (`squelchd token first-paired`), read
//! through the warden's `GET /v1/tenants/{label}/devices`, and stamped onto
//! the person here. The board then reads the store and only the store: a page
//! render must never exec into a pod, so this poller is the single path by
//! which the fact travels, and its staleness bound (one poll interval) is
//! invisible on an admin screen.
//!
//! PRIVACY: one timestamp per tenant crosses the wire, for tenants this
//! process already provisioned. No device names, no counts, no addresses.
//! Log lines carry labels and tallies — labels are already what this crate
//! logs a tenant by — never an email.
//!
//! The candidate set is bounded and SELF-QUIESCING: a user leaves it forever
//! the moment the stamp lands, an abandoned signup ages out of it at the
//! store's ninety-day window, and a tenant that is torn down stops matching
//! the `tenants.status = 'active'` join. There is no per-tenant backoff
//! state because the poll interval is the backoff and the window is the
//! give-up.

use crate::state::ControlState;

/// How many tenants one pass may ask the warden about. Each ask is a
/// `pods/exec` in the cluster, so the pass is small and SERIAL — a burst of
/// concurrent execs would trade cluster load for a latency nobody is waiting
/// on. Anything left over is simply next pass's work.
pub const ACTIVATION_BATCH: i64 = 25;

/// One activation pass: ask the warden about every user who signed up but has
/// no recorded client pairing, and stamp the ones whose daemon reports one.
/// Returns how many were stamped.
///
/// Per-tenant errors are eaten deliberately: the next pass is the retry, and
/// a tenant whose pod is mid-roll (the warden's 503 `not_running`) or whose
/// daemon predates `token first-paired` (a terse 500) heals on its own — the
/// former on the next tick, the latter on the next fleet roll.
pub async fn poll_first_paired(state: &ControlState) -> usize {
    let labels = match state.store().users_awaiting_first_pair(ACTIVATION_BATCH).await {
        Ok(labels) => labels,
        Err(e) => {
            tracing::error!(error = %e, "activation: could not read the candidate set");
            return 0;
        }
    };

    let mut stamped = 0usize;
    for label in labels {
        match state.warden().first_paired(&label).await {
            Ok(Some(Some(at))) => match state.store().mark_user_first_paired(&label, at).await {
                Ok(true) => {
                    stamped += 1;
                    tracing::info!(%label, "activation: first client pairing recorded");
                }
                // Someone else stamped between the read and the write, or the
                // tenant's person vanished: either way there is nothing left
                // to do, and first-stamp-wins means nothing was disturbed.
                Ok(false) => {}
                Err(e) => tracing::error!(error = %e, %label, "activation: stamp failed"),
            },
            // Not yet paired, or the warden no longer knows the label: both
            // resolve themselves — the first by a human, the second by the
            // candidate join dropping the row.
            Ok(Some(None)) | Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, %label, "activation: warden not answering"),
        }
    }
    stamped
}
