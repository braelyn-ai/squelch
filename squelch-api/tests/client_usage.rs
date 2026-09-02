//! GET /client/usage — the spend report.
//!
//! THE CONTRACT UNDER TEST is that the endpoint ENUMERATES the usage ledger
//! rather than naming the categories it knows. A literal map of known
//! categories silently drops whatever an extractor added later accrues. So the
//! load-bearing case here is a category this file invents: if that reports, an
//! extractor added next year reports too, with no edit to the endpoint.

mod common;

use axum::http::StatusCode;
use common::{Harness, authed, body_json, harness};
use serde_json::Value;
use squelch_api::{ApiState, router};
use squelch_core::store::{Store, UsageTokens};
use tower::ServiceExt;

/// Stage-1 per-MTok prices used throughout, distinct from the Stage-2 pair so a
/// category costed with the wrong one is visible in the assertion.
const S1_IN: f64 = 0.8;
const S1_OUT: f64 = 4.0;
const S2_IN: f64 = 3.0;
const S2_OUT: f64 = 15.0;
/// The NOTIFY fast lane's prices, a third distinct pair for the same reason:
/// the lane is the one ledger category that runs neither stage's model, so a
/// cost costed off either stage's numbers has to be visible in the assertion.
const N_IN: f64 = 0.1;
const N_OUT: f64 = 0.5;

/// A harness whose state carries known models and prices for both stages and
/// for the notify lane.
fn priced_harness(seed: impl FnOnce(&squelch_core::store::SqliteStore, i64)) -> Harness {
    let (state, store, acct) = common::state_with(seed);
    let state: ApiState = state
        .with_stage2_model("sonnet-test", Some("anthropic".into()))
        .with_stage2_prices(S2_IN, S2_OUT)
        .with_stage1_config("haiku-test", S1_IN, S1_OUT, 500)
        .with_notify_config("notify-test", N_IN, N_OUT);
    Harness {
        app: router(state),
        store,
        acct,
    }
}

async fn get_usage(app: axum::Router) -> Value {
    let resp = app.oneshot(authed("GET", "/client/usage")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// THE REGRESSION. `extract_banking` is a real extractor category; `extract_fictional`
/// is invented here and exists nowhere in the codebase — which is the point.
#[tokio::test]
async fn categories_include_ledger_writers_the_endpoint_never_heard_of() {
    let app = priced_harness(|store, acct| {
        store
            .stage1_bump_usage(
                acct,
                "2026-07-09",
                UsageTokens {
                    input: 1_000,
                    output: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .stage2_bump_usage(
                acct,
                "2026-07-09",
                UsageTokens {
                    input: 2_000,
                    output: 200,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .extract_bump_usage(
                acct,
                "2026-07-09",
                "extract_banking",
                UsageTokens {
                    input: 4_000,
                    output: 400,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .extract_bump_usage(
                acct,
                "2026-07-09",
                "extract_fictional",
                UsageTokens {
                    input: 8_000,
                    output: 800,
                    ..Default::default()
                },
            )
            .unwrap();
    })
    .app;

    let body = get_usage(app).await;
    let categories = body["categories"].as_object().expect("categories object");

    let mut names: Vec<&str> = categories.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["extract_banking", "extract_fictional", "stage1", "stage2"],
        "every ledger category reports, including one the endpoint cannot know about"
    );

    assert_eq!(categories["extract_fictional"]["totals"]["calls"], 1);
    assert_eq!(
        categories["extract_fictional"]["totals"]["input_tokens"],
        8_000
    );
}

/// Only Stage-2 runs the capable model. Stage-1 and every extractor share the
/// stage-1 config and cap (see `extract_pass`), so they cost at stage-1 rates —
/// costing an extractor at Stage-2 prices would overstate it ~4x here.
#[tokio::test]
async fn extractors_cost_at_stage1_rates_and_only_stage2_uses_stage2_rates() {
    let app = priced_harness(|store, acct| {
        store
            .stage1_bump_usage(
                acct,
                "2026-07-09",
                UsageTokens {
                    input: 1_000_000,
                    output: 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .stage2_bump_usage(
                acct,
                "2026-07-09",
                UsageTokens {
                    input: 1_000_000,
                    output: 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .extract_bump_usage(
                acct,
                "2026-07-09",
                "extract_banking",
                UsageTokens {
                    input: 1_000_000,
                    output: 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
    })
    .app;

    let body = get_usage(app).await;
    let cost = |name: &str| {
        body["categories"][name]["totals"]["est_cost_usd"]
            .as_f64()
            .unwrap()
    };

    // Exactly 1 MTok each way, so the cost IS the price pair summed.
    assert!((cost("stage1") - (S1_IN + S1_OUT)).abs() < 1e-9);
    assert!((cost("stage2") - (S2_IN + S2_OUT)).abs() < 1e-9);
    assert!(
        (cost("extract_banking") - (S1_IN + S1_OUT)).abs() < 1e-9,
        "an extractor runs the small model, so it costs like stage-1"
    );

    // And each category is labelled with the model that actually produced it.
    assert_eq!(body["categories"]["extract_banking"]["model"], "haiku-test");
    assert_eq!(body["categories"]["stage2"]["model"], "sonnet-test");
}

/// THE THIRD ARM. `notify` is the fast lane (docs/NOTIFY.md §11.5) and it runs
/// its OWN small model, not the stage-1 one every extractor shares. Falling
/// through to the stage-1 arm would bill a Haiku call at the capable model's
/// rates and overstate the cheapest pass in the pipeline by the whole ratio
/// between them, which is exactly the shape of bug that hid for ten days when
/// `extract_banking` was missing from this endpoint.
#[tokio::test]
async fn the_notify_category_costs_at_its_own_model_not_stage1s() {
    let app = priced_harness(|store, acct| {
        for category in ["notify", "extract_banking"] {
            store
                .extract_bump_usage(
                    acct,
                    "2026-07-09",
                    category,
                    UsageTokens {
                        input: 1_000_000,
                        output: 1_000_000,
                        ..Default::default()
                    },
                )
                .unwrap();
        }
    })
    .app;

    let body = get_usage(app).await;
    let cost = |name: &str| {
        body["categories"][name]["totals"]["est_cost_usd"]
            .as_f64()
            .unwrap()
    };

    // Exactly 1 MTok each way, so the cost IS the price pair summed.
    assert!(
        (cost("notify") - (N_IN + N_OUT)).abs() < 1e-9,
        "notify costs at the notify prices, got {}",
        cost("notify")
    );
    // ...and not at the arm it would otherwise have fallen through to. The
    // control is the extractor sitting beside it in the same ledger.
    assert!(
        (cost("extract_banking") - (S1_IN + S1_OUT)).abs() < 1e-9,
        "the neighbouring category still costs at stage-1 rates"
    );
    assert!(cost("notify") < cost("extract_banking"));

    // And the label names the model that actually produced the spend, from the
    // SAME arm as the prices: a category priced as Stage-1 but labelled with
    // the notify model would read as a cost regression in the app.
    assert_eq!(body["categories"]["notify"]["model"], "notify-test");
    assert_eq!(body["categories"]["extract_banking"]["model"], "haiku-test");
}

/// The flat top-level fields predate `categories` and stay Stage-2, so an older
/// client reading them is unaffected by everything above.
#[tokio::test]
async fn top_level_fields_stay_stage2_for_older_clients() {
    let app = priced_harness(|store, acct| {
        store
            .stage1_bump_usage(
                acct,
                "2026-07-09",
                UsageTokens {
                    input: 1_000,
                    output: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .stage2_bump_usage(
                acct,
                "2026-07-09",
                UsageTokens {
                    input: 1_000_000,
                    output: 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .extract_bump_usage(
                acct,
                "2026-07-09",
                "extract_banking",
                UsageTokens {
                    input: 9_999,
                    output: 9_999,
                    ..Default::default()
                },
            )
            .unwrap();
    })
    .app;

    let body = get_usage(app).await;
    assert_eq!(body["model"], "sonnet-test");
    assert_eq!(body["provider"], "anthropic");
    assert_eq!(body["totals"]["input_tokens"], 1_000_000);
    assert_eq!(body["rows"].as_array().unwrap().len(), 1);
    assert!(
        (body["totals"]["est_cost_usd"].as_f64().unwrap() - (S2_IN + S2_OUT)).abs() < 1e-9,
        "top-level totals are Stage-2 alone, never a sum across categories"
    );
}

/// The two STAGES are permanent parts of the pipeline, so they report at zero
/// rather than vanishing — a reader who escalated nothing today is owed
/// "Stage 2: nothing", not a missing section. Extractors are the opposite: they
/// appear only once the ledger has seen them, which is what lets a new one
/// report without an edit to the endpoint.
#[tokio::test]
async fn stages_always_report_extractors_only_once_seen() {
    let Harness { app, .. } = harness(|_, _| {});
    let body = get_usage(app).await;

    let categories = body["categories"].as_object().unwrap();
    let mut names: Vec<&str> = categories.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["stage1", "stage2"], "stages, and nothing else");

    assert_eq!(categories["stage1"]["totals"]["calls"], 0);
    assert!(categories["stage1"]["rows"].as_array().unwrap().is_empty());
    assert_eq!(body["totals"]["calls"], 0);
}
