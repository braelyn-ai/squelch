//! The per-route latency middleware: templates, not paths, and nothing a client
//! typed ever reaches a label.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{authed, state_with};
use squelch_api::{record_http_metrics, router};
use squelch_core::metrics::{SyncMetrics, render};
use tower::ServiceExt;

#[tokio::test]
async fn latency_is_keyed_by_route_template_and_unmatched_paths_collapse() {
    let (state, _store, _acct) = state_with(|_, _| {});
    let metrics = SyncMetrics::new();
    let app = router(state).layer(axum::middleware::from_fn_with_state(
        metrics.clone(),
        record_http_metrics,
    ));

    // A path parameter that does not exist: the 404 is recorded under the
    // TEMPLATE, and the id never appears in the exposition.
    app.clone()
        .oneshot(authed("GET", "/client/thread/thread-id-that-must-not-leak"))
        .await
        .unwrap();
    app.clone()
        .oneshot(authed("GET", "/client/stats"))
        .await
        .unwrap();
    // No bearer: the 401 is still attributed to its route, not to "unmatched".
    app.clone()
        .oneshot(
            Request::builder()
                .uri("/client/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Nothing routes here; the path is the client's and must collapse.
    app.oneshot(authed("GET", "/no/such/route?q=secret-query"))
        .await
        .unwrap();

    let text = render(&metrics, None);
    let count =
        |labels: &str| format!("squelchd_http_request_duration_seconds_count{{{labels}}} 1\n");
    assert!(
        text.contains(&count(
            r#"route="/client/thread/{thread_id}",method="GET",status="4xx""#
        )),
        "{text}"
    );
    assert!(text.contains(&count(r#"route="/client/stats",method="GET",status="2xx""#)));
    assert!(text.contains(&count(r#"route="/client/stats",method="GET",status="4xx""#)));
    assert!(text.contains(&count(r#"route="unmatched",method="GET",status="4xx""#)));
    assert!(
        !text.contains("must-not-leak"),
        "a path parameter became a label"
    );
    assert!(
        !text.contains("secret-query"),
        "an unmatched path became a label"
    );
    assert!(!text.contains("/no/such/route"));
}
