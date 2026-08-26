//! Integration tests for `/client/groups` — send groups on the human door.
//!
//! What a store unit test cannot see, and what these cover:
//!
//! - the door's 404 is the SAME answer for "gone" and "not yours", including on
//!   the history route, which resolves the group before it queries so an id
//!   belonging to nobody cannot answer 200-with-an-empty-list and become an
//!   oracle for which ids exist,
//! - a duplicate name is a 400 the user can read, not the 500 a raw UNIQUE
//!   violation collapses to,
//! - the membership cap is served rather than assumed, so the app and the daemon
//!   cannot disagree about it across release trains,
//! - a group write leaves an AUDIT row carrying the SHAPE and never the
//!   addresses,
//! - and the history route answers with the mail that predates the group, which
//!   is the half of the feature a recorded-sends-only design would miss.

use axum::http::StatusCode;
use serde_json::{Value, json};
use squelch_core::store::Store;
use tower::ServiceExt;

mod common;
use common::{Harness, authed, authed_json, body_json, harness, json_request, sent_msg};

/// Create a group and hand back its id.
async fn create(h: &Harness, body: Value) -> (StatusCode, Value) {
    let resp = h
        .app
        .clone()
        .oneshot(authed_json("POST", "/client/groups", body))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn get(h: &Harness, uri: &str) -> (StatusCode, Value) {
    let resp = h.app.clone().oneshot(authed("GET", uri)).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

fn investors() -> Value {
    json!({
        "name": "Preseed Investors",
        "mode": "individual",
        "note": "the seed round",
        "members": [
            {"addr": "ann@fund.com", "display_name": "Ann"},
            {"addr": "bo@fund.com"},
            {"addr": "cy@fund.com"},
        ],
    })
}

#[tokio::test]
async fn create_then_read_round_trips_the_whole_group() {
    let h = harness(|_, _| {});
    let (status, group) = create(&h, investors()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(group["name"], "Preseed Investors");
    assert_eq!(group["slug"], "preseed investors");
    assert_eq!(group["mode"], "individual");
    assert_eq!(group["member_count"], 3);
    assert_eq!(group["members"].as_array().unwrap().len(), 3);

    let id = group["id"].as_i64().unwrap();
    let (status, fetched) = get(&h, &format!("/client/groups/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["members"][0]["display_name"], "Ann");
}

/// The listing is the sidebar. Membership is a click away, not a payload.
#[tokio::test]
async fn the_listing_carries_counts_but_no_addresses() {
    let h = harness(|_, _| {});
    create(&h, investors()).await;

    let (status, list) = get(&h, "/client/groups").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["member_count"], 3);
    assert!(
        list[0].get("members").is_none(),
        "an empty membership is omitted, not serialized: {list}"
    );
}

/// A present-but-empty `q` is the composer asking before anything was typed.
#[tokio::test]
async fn autocomplete_matches_names_and_declines_an_empty_fragment() {
    let h = harness(|_, _| {});
    create(&h, investors()).await;

    let (_, hits) = get(&h, "/client/groups?q=pres").await;
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["name"], "Preseed Investors");

    let (_, none) = get(&h, "/client/groups?q=").await;
    assert!(
        none.as_array().unwrap().is_empty(),
        "an empty fragment must not list every group"
    );

    let (_, all) = get(&h, "/client/groups").await;
    assert_eq!(
        all.as_array().unwrap().len(),
        1,
        "no `q` at all is the Groups page, which does list them"
    );
}

#[tokio::test]
async fn a_duplicate_name_is_a_readable_400() {
    let h = harness(|_, _| {});
    create(&h, investors()).await;

    let (status, body) = create(
        &h,
        json!({"name": "  preseed   INVESTORS ", "members": [{"addr": "d@x.com"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("already exists"),
        "expected a legible conflict, got {body}"
    );
}

#[tokio::test]
async fn a_member_that_is_not_an_address_is_refused_at_the_door() {
    let h = harness(|_, _| {});
    let (status, body) = create(
        &h,
        json!({"name": "Typos", "members": [{"addr": "ann at fund dot com"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("not an email"));
}

/// A group that loses its `mode` on the wire must fall back to the VISIBLE
/// shape. Defaulting to `bcc` would let a dropped field silently conceal an
/// audience; defaulting to `to` is at worst a mode the user can see and fix.
#[tokio::test]
async fn an_absent_mode_defaults_to_the_visible_shape() {
    let h = harness(|_, _| {});
    let (_, group) = create(
        &h,
        json!({"name": "No Mode", "members": [{"addr": "a@x.com"}]}),
    )
    .await;
    assert_eq!(group["mode"], "to");
}

#[tokio::test]
async fn update_replaces_membership_and_delete_is_idempotent() {
    let h = harness(|_, _| {});
    let (_, group) = create(&h, investors()).await;
    let id = group["id"].as_i64().unwrap();

    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "PUT",
            &format!("/client/groups/{id}"),
            json!({
                "name": "Preseed Investors",
                "mode": "bcc",
                "members": [{"addr": "ann@fund.com"}, {"addr": "zed@fund.com"}],
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated = body_json(resp).await;
    assert_eq!(updated["mode"], "bcc");
    assert_eq!(updated["member_count"], 2);

    let resp = h
        .app
        .clone()
        .oneshot(authed("DELETE", &format!("/client/groups/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Gone means gone, on every verb.
    let (status, _) = get(&h, &format!("/client/groups/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let resp = h
        .app
        .clone()
        .oneshot(authed("DELETE", &format!("/client/groups/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// THE ORACLE GUARD: history resolves the group first, so an id that is not
/// this account's answers 404 rather than an empty 200 that would confirm the
/// id is merely unused.
#[tokio::test]
async fn history_for_an_unknown_group_is_404_not_an_empty_page() {
    let h = harness(|_, _| {});
    let (status, _) = get(&h, "/client/groups/9999/history").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The half of the feature that makes it useful on day one.
#[tokio::test]
async fn history_answers_with_mail_that_predates_the_group() {
    let h = harness(|store, acct| {
        // Seeded WITHOUT recipients, then filled through `set_message_to_addrs`
        // — the recipients-backfill sweep's own entry point, which is how every
        // sent row that predates the column gets its display recipients AND its
        // normalized index. Seeding the column directly would skip the index and
        // test a state the daemon cannot actually produce.
        let id = store
            .upsert_message(&sent_msg(acct, "g1", "t1", "Update #1", ""))
            .unwrap();
        store
            .set_triage(
                id,
                acct,
                0,
                squelch_core::types::Tier::Noise,
                squelch_core::types::Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        store
            .set_message_to_addrs(acct, id, "Ann <ann@fund.com>, bo@fund.com")
            .unwrap();
    });

    let (_, group) = create(&h, investors()).await;
    let id = group["id"].as_i64().unwrap();

    let (status, page) = get(&h, &format!("/client/groups/{id}/history")).await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "expected the pre-existing mail: {page}");
    assert_eq!(items[0]["subject"], "Update #1");
    assert_eq!(items[0]["reached"], 2);
    assert_eq!(items[0]["group_size"], 3);
    assert!(
        items[0].get("group_send_id").is_none(),
        "a derived entry names no recorded send"
    );
}

/// The cap is a wire fact, not a constant compiled into an app that ships on a
/// different train.
#[tokio::test]
async fn the_membership_cap_is_served() {
    let h = harness(|_, _| {});
    let (status, limits) = get(&h, "/client/groups/limits").await;
    assert_eq!(status, StatusCode::OK);
    assert!(limits["max_members"].as_u64().unwrap() >= 1);
}

/// A group is the address book behind an irreversible action. The write is
/// traced — and the trace records the SHAPE, never who is in it.
#[tokio::test]
async fn a_write_is_audited_by_shape_and_never_by_address() {
    let h = harness(|_, _| {});
    create(&h, investors()).await;

    let entries = h.store.list_audit(h.acct, 50).unwrap();
    let created = entries
        .iter()
        .find(|e| e.action == "group.create")
        .expect("a group write must leave a trace");
    assert_eq!(created.detail.as_deref(), Some("individual:3"));

    let rendered = serde_json::to_string(&entries).unwrap();
    assert!(
        !rendered.contains("ann@fund.com"),
        "the audit log must not re-list the audience: {rendered}"
    );
}

/// Every route on this tree is behind the bearer, groups included.
#[tokio::test]
async fn the_group_routes_are_behind_the_bearer() {
    let h = harness(|_, _| {});
    for (method, uri) in [
        ("GET", "/client/groups"),
        ("GET", "/client/groups/1"),
        ("GET", "/client/groups/1/history"),
        ("DELETE", "/client/groups/1"),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} answered without a bearer"
        );
    }
    let resp = h
        .app
        .clone()
        .oneshot(json_request("POST", "/client/groups", &investors(), false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
