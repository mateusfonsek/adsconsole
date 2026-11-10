//! Generic Google Ads writes: `restore_fields`, `remove_resources` and the `mutate`
//! escape, against wiremock.

mod common;

use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, ToolGroup, WriteMode};
use adsconsole_core::db::{ChangeFilter, ChangeStatus};
use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::diff::diff;
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use common::*;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RN: &str = "customers/111/campaigns/5";
const MUTATE_PATH: &str = "/v26/customers/111/googleAds:mutate";

fn engine(h: &GoogleHarness, escape: bool) -> Engine {
    let mut settings = ServerSettings {
        write_mode: WriteMode::Direct,
        ..ServerSettings::default()
    };
    if escape {
        settings.groups.insert(ToolGroup::Escape, true);
    }
    let mut config = Config::default();
    config.servers.insert(Platform::GoogleAds, settings);
    Engine::new(
        h.adapter.clone(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap()
}

async fn mock_mutate_ok(server: &MockServer, responses: Value) {
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "mutateOperationResponses": responses })),
        )
        .mount(server)
        .await;
}

/// Campaign name read: `first` on the first query, `then` on the following ones.
async fn mock_campaign_name(server: &MockServer, first: &str, then: &str) {
    let row = |name: &str| json!([{ "campaign": { "resourceName": RN, "name": name } }]);
    Mock::given(method("POST"))
        .and(path("/v26/customers/111/googleAds:search"))
        .and(body_string_contains("FROM campaign WHERE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": row(first) })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
    mock_search(server, "111", "FROM campaign WHERE", row(then)).await;
}

async fn mutate_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == MUTATE_PATH)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn restore_call() -> adsconsole_mcp::Call {
    call(
        CONN,
        "111",
        "restore_fields",
        json!({ "updates": { RN: { "campaign.name": "new" } } }),
    )
}

#[tokio::test]
async fn restore_fields_roundtrip() {
    let h = google_harness().await;
    mock_campaign_name(&h.server, "old", "new").await;
    mock_mutate_ok(
        &h.server,
        json!([{ "campaignResult": { "resourceName": RN } }]),
    )
    .await;
    let c = restore_call();

    let validated = h.adapter.validate(&c).await.unwrap();
    assert!(!validated.summary.is_empty());
    let before = h.adapter.snapshot(&c).await.unwrap();
    assert_eq!(before, json!({ RN: { "campaign.name": "old" } }));
    let applied = h.adapter.execute(&c, &before).await.unwrap();

    let bodies = mutate_bodies(&h.server).await;
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert_eq!(bodies[0]["validateOnly"], true);
    assert_eq!(bodies[1]["validateOnly"], false);
    assert_eq!(bodies[1]["partialFailure"], false);
    let op = &bodies[1]["mutateOperations"][0]["campaignOperation"];
    assert_eq!(op["updateMask"], "name");
    assert_eq!(op["update"]["resourceName"], RN);
    assert_eq!(op["update"]["name"], "new");

    assert_eq!(applied.after, json!({ RN: { "campaign.name": "new" } }));
    let inverse = applied.inverse.unwrap();
    assert_eq!(inverse.tool, "restore_fields");
    assert_eq!(inverse.input["updates"][RN]["campaign.name"], "old");
    assert!(inverse.input.get("account_id").is_none());
    assert!(inverse.input.get("connection").is_none());
}

#[tokio::test]
async fn current_state_matches_after_shape() {
    let h = google_harness().await;
    mock_campaign_name(&h.server, "old", "new").await;
    mock_mutate_ok(
        &h.server,
        json!([{ "campaignResult": { "resourceName": RN } }]),
    )
    .await;
    let c = restore_call();
    let before = h.adapter.snapshot(&c).await.unwrap();
    let applied = h.adapter.execute(&c, &before).await.unwrap();
    let target = applied.inverse.unwrap().target;

    let current = h.adapter.current_state(CONN, "111", &target).await.unwrap();
    assert_eq!(current, applied.after);
    assert!(diff(&applied.after, &current).is_empty());
}

#[tokio::test]
async fn current_state_sees_external_change() {
    let h = google_harness().await;
    mock_search(
        &h.server,
        "111",
        "FROM campaign WHERE",
        json!([{ "campaign": { "resourceName": RN, "name": "other" } }]),
    )
    .await;
    let target = json!({ "watch": [
        { "resource_name": RN, "resource": "campaign", "fields": ["campaign.name"] }
    ]});
    let current = h.adapter.current_state(CONN, "111", &target).await.unwrap();
    let after = json!({ RN: { "campaign.name": "new" } });
    assert_eq!(diff(&after, &current).len(), 1);
}

#[tokio::test]
async fn current_state_rejects_unsafe_watch() {
    let h = google_harness().await;
    let target = json!({ "watch": [
        { "resource_name": RN, "resource": "campaign", "fields": ["campaign.name FROM x"] }
    ]});
    let err = h
        .adapter
        .current_state(CONN, "111", &target)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INTERNAL);
}

#[tokio::test]
async fn restore_fields_rejects_bad_input() {
    let h = google_harness().await;
    for input in [
        json!({ "updates": { RN: { "campaign.nope": 1 } } }),
        json!({ "updates": { "customers/222/campaigns/5": { "campaign.name": "x" } } }),
        json!({ "updates": { RN: { "campaign_budget.amount_micros": 1 } } }),
        json!({ "updates": {} }),
    ] {
        let c = call(CONN, "111", "restore_fields", input.clone());
        let err = h.adapter.validate(&c).await.unwrap_err();
        assert_eq!(err.code, codes::INVALID_INPUT, "{input}");
    }
    assert!(mutate_bodies(&h.server).await.is_empty());
}

#[tokio::test]
async fn remove_resources_sends_removes_in_order() {
    let h = google_harness().await;
    let budget = "customers/111/campaignBudgets/7";
    mock_mutate_ok(
        &h.server,
        json!([
            { "campaignResult": { "resourceName": RN } },
            { "campaignBudgetResult": { "resourceName": budget } }
        ]),
    )
    .await;
    let c = call(
        CONN,
        "111",
        "remove_resources",
        json!({ "resource_names": [RN, budget] }),
    );
    h.adapter.validate(&c).await.unwrap();
    let before = h.adapter.snapshot(&c).await.unwrap();
    assert_eq!(before, json!({}));
    let applied = h.adapter.execute(&c, &before).await.unwrap();
    assert!(applied.inverse.is_none());
    let bodies = mutate_bodies(&h.server).await;
    assert_eq!(
        bodies[1]["mutateOperations"],
        json!([
            { "campaignOperation": { "remove": RN } },
            { "campaignBudgetOperation": { "remove": budget } }
        ])
    );
    let spec = h
        .adapter
        .tools()
        .into_iter()
        .find(|s| s.name == "remove_resources")
        .unwrap();
    assert!(!spec.exposed && spec.destructive && !spec.reversible);
    let spec = h
        .adapter
        .tools()
        .into_iter()
        .find(|s| s.name == "restore_fields")
        .unwrap();
    assert!(!spec.exposed && spec.idempotent && spec.reversible);
}

#[tokio::test]
async fn validation_error_records_nothing() {
    let h = google_harness().await;
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_string(include_str!(
            "../../google-ads/tests/fixtures/budget_minimum.json"
        )))
        .mount(&h.server)
        .await;
    let engine = engine(&h, true);
    let err = engine
        .call_tool(
            "mutate",
            json!({ "account_id": "111", "operations": [
                { "campaignBudgetOperation": { "create": { "amountMicros": "1000000" } } }
            ]}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::BUDGET_TOO_LOW, "{err:?}");
    let bodies = mutate_bodies(&h.server).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["validateOnly"], true);
    let changes = h.db.list_changes(&ChangeFilter::default()).unwrap();
    assert!(changes.is_empty(), "{changes:?}");
}

#[tokio::test]
async fn escape_mutate_off_by_default() {
    let h = google_harness().await;
    mock_mutate_ok(
        &h.server,
        json!([{ "campaignResult": { "resourceName": RN } }]),
    )
    .await;
    let input = json!({ "account_id": "111", "operations": [
        { "campaignOperation": { "remove": RN } }
    ]});
    let err = engine(&h, false)
        .call_tool("mutate", input.clone(), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::TOOL_DISABLED);
    assert!(mutate_bodies(&h.server).await.is_empty());

    let out = engine(&h, true)
        .call_tool("mutate", input, None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], false);
    let bodies = mutate_bodies(&h.server).await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[1]["mutateOperations"],
        json!([{ "campaignOperation": { "remove": RN } }])
    );
    let changes = h.db.list_changes(&ChangeFilter::default()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].status, ChangeStatus::Applied);
    assert!(!changes[0].reversible);
    assert!(changes[0].inverse.is_none());

    let spec = engine(&h, true).registry().get("mutate").cloned().unwrap();
    assert!(spec.exposed && spec.destructive && !spec.reversible);
    assert_eq!(spec.group, ToolGroup::Escape);
}

#[tokio::test]
async fn escape_mutate_rejects_empty_operations() {
    let h = google_harness().await;
    let err = engine(&h, true)
        .call_tool(
            "mutate",
            json!({ "account_id": "111", "operations": [] }),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(mutate_bodies(&h.server).await.is_empty());
}

#[tokio::test]
async fn restore_fields_without_snapshot_is_internal_and_sends_nothing() {
    let h = google_harness().await;
    mock_mutate_ok(
        &h.server,
        json!([{ "campaignResult": { "resourceName": RN } }]),
    )
    .await;
    for before in [json!({}), json!(null), json!("x")] {
        let err = h
            .adapter
            .execute(&restore_call(), &before)
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::INTERNAL, "{before}");
    }
    assert!(mutate_bodies(&h.server).await.is_empty());
}
