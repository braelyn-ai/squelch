//! The bearer gate on every control-plane route.
//!
//! One credential, shared with exactly one caller: `SQUELCH_WARDEN_TOKEN`,
//! held by the control plane on Railway and by this process. There is no second
//! tier and no per-caller token, because there is no second caller — a warden
//! is a private appliance for one control plane.
//!
//! FAIL-CLOSED, and uniform. A missing header, an unparseable one, and a wrong
//! token all end as a bare 401 with no body. Nothing about the presented value
//! is logged or echoed, and the compare is constant time via
//! [`squelch_httpauth::ct_eq`] — the same one the human door and the relay use,
//! so a fix to any of them lands in one place.
//!
//! What this gate protects is worth naming: past it, a caller can create a
//! systemd unit and write files as root. That is why the token has a minimum
//! length checked at startup ([`crate::config::MIN_TOKEN_LEN`]) and why the
//! listener is loopback-only with TLS terminated by Caddy in front.

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
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if ct_eq(presented.as_bytes(), state.token().as_bytes()) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}
