//! Per-route latency for both doors: the middleware that feeds
//! [`squelch_core::metrics::HttpMetrics`].
//!
//! Applied with `Router::layer` so it runs AFTER routing, which is what puts
//! the matched template in the request's extensions. A request that matched
//! nothing has no template and records as [`HTTP_ROUTE_UNMATCHED`]: its actual
//! path is whatever the client typed and must never become a label.
//!
//! The clock stops when the handler returns its response HEAD, before the body
//! streams; see the histogram's docs for why that is the number wanted here.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use squelch_core::metrics::{HTTP_ROUTE_UNMATCHED, SyncMetrics};

pub async fn record_http_metrics(
    State(metrics): State<Arc<SyncMetrics>>,
    req: Request,
    next: Next,
) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| HTTP_ROUTE_UNMATCHED.to_owned());
    let method = req.method().clone();
    let started = Instant::now();
    let resp = next.run(req).await;
    metrics.http().observe(
        &route,
        method.as_str(),
        resp.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    resp
}
