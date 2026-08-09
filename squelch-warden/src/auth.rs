//! The bearer gate on every control-plane route.
//!
//! One credential, shared with exactly one caller: `SQUELCH_WARDEN_TOKEN`,
//! held by the control plane on Railway and by this process. There is no second
//! tier and no per-caller token, because there is no second caller: a warden is
//! a private appliance for one control plane.
//!
//! FAIL-CLOSED, and uniform. A missing header, an unparseable one, and a wrong
//! token all end as a bare 401 with no body. Nothing about the presented value
//! is logged or echoed, and the compare is constant time via
//! [`squelch_httpauth::ct_eq`], the same one the human door and the relay use,
//! so a fix to any of them lands in one place.
//!
//! What this gate protects is worth naming: past it, a caller can create
//! workloads in the namespace that holds every tenant's mail and exec into a
//! tenant's pod. That is why the token has a minimum length checked at startup
//! ([`crate::config::MIN_TOKEN_LEN`]), why the pod sits behind its own
//! NetworkPolicy with TLS terminated at the ingress controller, and why a
//! refusal is counted here rather than passing silently.
//!
//! The counting is a COUNT. Not the presented value, not the header, not the
//! address, not the path: a log that quoted any of those would be a log holding
//! near-credentials, and an operator looking for "is somebody hammering the
//! warden" needs a number and a rate, which is what
//! [`crate::ratelimit`] in front of this gate turns into a 429 anyway.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};
use squelch_httpauth::{ct_eq, parse_bearer};

use crate::WardenState;

/// Gate a request on the configured bearer token.
pub async fn require_bearer(
    State(state): State<WardenState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(presented) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer)
    else {
        return Err(refuse(&state, "absent"));
    };

    if ct_eq(presented.as_bytes(), state.token().as_bytes()) {
        Ok(next.run(req).await)
    } else {
        Err(refuse(&state, "mismatch"))
    }
}

/// Count the refusal, say so at warn, and answer the one 401 this service has.
///
/// `shape` distinguishes "no usable Authorization header" from "a bearer that
/// is not ours", which is the difference between a scanner and somebody with a
/// stale or guessed token. Neither is material: it says nothing about the value
/// presented.
fn refuse(state: &WardenState, shape: &'static str) -> StatusCode {
    let failures = state.record_auth_failure();
    tracing::warn!(shape, failures, "refused an unauthenticated request");
    StatusCode::UNAUTHORIZED
}
