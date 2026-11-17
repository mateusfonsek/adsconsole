//! Labels, extensions (asset links) and the GAQL field catalog, against wiremock.

mod common;

use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::error::codes;
use common::*;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MUTATE_PATH: &str = "/v26/customers/111/googleAds:mutate";
const FIELDS_PATH: &str = "/v26/googleAdsFields:search";

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

/// Bodies of the mutates that applied (not `validateOnly`).
async fn applied_mutates(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == MUTATE_PATH)
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
        .filter(|b| b["validateOnly"] == json!(false))
        .collect()
}

/// validate → snapshot → execute, like the Engine in Direct mode.
async fn apply(h: &GoogleHarness, tool: &str, input: Value) -> adsconsole_mcp::Applied {
    let mut input = input;
    input["account_id"] = json!("111");
    let c = call(CONN, "111", tool, input);
    h.adapter.validate(&c).await.unwrap();
    let before = h.adapter.snapshot(&c).await.unwrap();
    h.adapter.execute(&c, &before).await.unwrap()
}

async fn plan_err(h: &GoogleHarness, tool: &str, input: Value) -> adsconsole_mcp::ToolError {
    let mut input = input;
    input["account_id"] = json!("111");
    h.adapter
        .validate(&call(CONN, "111", tool, input))
        .await
        .unwrap_err()
}

fn label_row(id: &str, name: &str) -> Value {
    json!({ "label": { "resourceName": format!("customers/111/labels/{id}"), "id": id,
                       "name": name, "status": "ENABLED" } })
}

// ------------------------------------------------------------------ labels

#[tokio::test]
async fn create_labels_keeps_temp_names_and_undo_removes_them() {
    let h = google_harness().await;
    mock_mutate_ok(
        &h.server,
        json!([{ "labelResult": { "resourceName": "customers/111/labels/77" } }]),
    )
    .await;
    let applied = apply(
        &h,
        "manage_labels",
        json!({ "action": "create",
                "labels": [{ "name": " VIP ", "background_color": "#ff0000", "description": "Top" }] }),
    )
    .await;
    let bodies = applied_mutates(&h.server).await;
    assert_eq!(
        bodies[0]["mutateOperations"],
        json!([{ "labelOperation": { "create": {
            "resourceName": "customers/111/labels/-1", "name": "VIP",
            "textLabel": { "backgroundColor": "#FF0000", "description": "Top" } } } }])
    );
    let inv = applied.inverse.unwrap();
    assert_eq!(inv.tool, "remove_resources");
    assert_eq!(
        inv.input,
        json!({ "resource_names": ["customers/111/labels/77"] })
    );
}

#[tokio::test]
async fn manage_labels_rejects_bad_input() {
    let h = google_harness().await;
    let e = plan_err(
        &h,
        "manage_labels",
        json!({ "action": "create", "labels": [{ "name": "A", "background_color": "red" }] }),
    )
    .await;
    assert_eq!(e.field.as_deref(), Some("labels[0].background_color"));
    let e = plan_err(
        &h,
        "manage_labels",
        json!({ "action": "create", "labels": [{ "name": "A" }, { "name": "a" }] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    let e = plan_err(
        &h,
        "manage_labels",
        json!({ "action": "update", "labels": [{}] }),
    )
    .await;
    assert_eq!(e.field.as_deref(), Some("labels[0].label_id"));
}

#[tokio::test]
async fn update_label_is_reversible_field_change() {
    let h = google_harness().await;
    // Both the plan's read (by id) and the snapshot (by name) see the current name.
    mock_search(
        &h.server,
        "111",
        "FROM label WHERE",
        json!([label_row("5", "Old")]),
    )
    .await;
    mock_mutate_ok(
        &h.server,
        json!([{ "labelResult": { "resourceName": "customers/111/labels/5" } }]),
    )
    .await;
    let applied = apply(
        &h,
        "manage_labels",
        json!({ "action": "update", "labels": [{ "label_id": "5", "name": "New" }] }),
    )
    .await;
    let bodies = applied_mutates(&h.server).await;
    assert_eq!(
        bodies[0]["mutateOperations"][0]["labelOperation"]["updateMask"],
        json!("name")
    );
    let inv = applied.inverse.unwrap();
    assert_eq!(inv.tool, "restore_fields");
    assert_eq!(
        inv.input["updates"]["customers/111/labels/5"]["label.name"],
        json!("Old")
    );
}

#[tokio::test]
async fn missing_label_is_not_found() {
    let h = google_harness().await;
    mock_search(&h.server, "111", "FROM label", json!([])).await;
    let e = plan_err(
        &h,
        "apply_labels",
        json!({ "action": "attach", "label_ids": ["9"], "campaign_ids": ["5"] }),
    )
    .await;
    assert_eq!(e.code, codes::NOT_FOUND);
    assert_eq!(e.field.as_deref(), Some("label_ids"));
}

#[tokio::test]
async fn attach_skips_existing_links_and_sends_no_names() {
    let h = google_harness().await;
    mock_search(
        &h.server,
        "111",
        "FROM label WHERE",
        json!([label_row("9", "VIP")]),
    )
    .await;
    // Campaign 5 already has the label; 6 and ad group 3 don't.
    mock_search(
        &h.server,
        "111",
        "FROM campaign_label WHERE campaign_label.resource_name IN",
        json!([{ "campaignLabel": { "resourceName": "customers/111/campaignLabels/5~9" } }]),
    )
    .await;
    mock_search(&h.server, "111", "FROM ad_group_label WHERE", json!([])).await;
    mock_mutate_ok(
        &h.server,
        json!([
            { "campaignLabelResult": { "resourceName": "customers/111/campaignLabels/6~9" } },
            { "adGroupLabelResult": { "resourceName": "customers/111/adGroupLabels/3~9" } }
        ]),
    )
    .await;
    let mut input = json!({ "action": "attach", "label_ids": ["9"], "campaign_ids": ["5", "6"],
                            "ad_group_ids": ["3"], "account_id": "111" });
    let v = h
        .adapter
        .validate(&call(CONN, "111", "apply_labels", input.clone()))
        .await
        .unwrap();
    assert!(
        v.summary.contains("1 campaign(s), 1 ad group(s)"),
        "{}",
        v.summary
    );
    assert!(
        v.summary.contains("1 skipped (already attached)"),
        "{}",
        v.summary
    );
    input.as_object_mut().unwrap().remove("account_id");
    let applied = apply(&h, "apply_labels", input).await;
    let bodies = applied_mutates(&h.server).await;
    assert_eq!(
        bodies[0]["mutateOperations"],
        json!([
            { "campaignLabelOperation": { "create": {
                "campaign": "customers/111/campaigns/6", "label": "customers/111/labels/9" } } },
            { "adGroupLabelOperation": { "create": {
                "adGroup": "customers/111/adGroups/3", "label": "customers/111/labels/9" } } }
        ])
    );
    let inv = applied.inverse.unwrap();
    assert_eq!(inv.tool, "remove_resources");
    assert_eq!(
        inv.input,
        json!({ "resource_names": ["customers/111/campaignLabels/6~9", "customers/111/adGroupLabels/3~9"] })
    );
}

#[tokio::test]
async fn attach_when_everything_is_attached_is_invalid_state() {
    let h = google_harness().await;
    mock_search(
        &h.server,
        "111",
        "FROM label WHERE",
        json!([label_row("9", "VIP")]),
    )
    .await;
    mock_search(
        &h.server,
        "111",
        "FROM campaign_label WHERE",
        json!([{ "campaignLabel": { "resourceName": "customers/111/campaignLabels/5~9" } }]),
    )
    .await;
    let e = plan_err(
        &h,
        "apply_labels",
        json!({ "action": "attach", "label_ids": ["9"], "campaign_ids": ["5"] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_STATE);
}

#[tokio::test]
async fn detach_undo_recreates_the_links() {
    let h = google_harness().await;
    let link = "customers/111/adGroupAdLabels/3~4~9";
    mock_search(
        &h.server,
        "111",
        "FROM label WHERE",
        json!([label_row("9", "VIP")]),
    )
    .await;
    mock_search(
        &h.server,
        "111",
        "FROM ad_group_ad_label WHERE ad_group_ad_label.resource_name IN",
        json!([{ "adGroupAdLabel": { "resourceName": link } }]),
    )
    .await;
    mock_mutate_ok(
        &h.server,
        json!([{ "adGroupAdLabelResult": { "resourceName": link } }]),
    )
    .await;
    let applied = apply(
        &h,
        "apply_labels",
        json!({ "action": "detach", "label_ids": ["9"], "ads": [{ "ad_group_id": "3", "ad_id": "4" }] }),
    )
    .await;
    let bodies = applied_mutates(&h.server).await;
    assert_eq!(
        bodies[0]["mutateOperations"],
        json!([{ "adGroupAdLabelOperation": { "remove": link } }])
    );
    let inv = applied.inverse.unwrap();
    assert_eq!(inv.tool, "restore_links");
    assert_eq!(inv.input, json!({ "resource_names": [link] }));

    // The inverse itself: creates the link from its name, and undoing it removes it again.
    let restored = apply(&h, "restore_links", inv.input.clone()).await;
    let bodies = applied_mutates(&h.server).await;
    assert_eq!(
        bodies[1]["mutateOperations"],
        json!([{ "adGroupAdLabelOperation": { "create": {
            "adGroupAd": "customers/111/adGroupAds/3~4", "label": "customers/111/labels/9" } } }])
    );
    assert_eq!(restored.inverse.unwrap().tool, "remove_resources");
    let spec = h
        .adapter
        .tools()
        .into_iter()
        .find(|s| s.name == "restore_links")
        .unwrap();
    assert!(!spec.exposed && spec.reversible);
}

#[tokio::test]
async fn restore_links_rejects_other_accounts_and_non_links() {
    let h = google_harness().await;
    for rn in [
        "customers/222/campaignLabels/5~9",
        "customers/111/campaigns/5",
        "customers/111/campaignLabels/5~-1",
    ] {
        let e = plan_err(&h, "restore_links", json!({ "resource_names": [rn] })).await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{rn}");
    }
}

// -------------------------------------------------------------- extensions

#[tokio::test]
async fn add_extensions_links_new_assets_and_undo_removes_links() {
    let h = google_harness().await;
    mock_mutate_ok(
        &h.server,
        json!([
            { "assetResult": { "resourceName": "customers/111/assets/100" } },
            { "campaignAssetResult": { "resourceName": "customers/111/campaignAssets/5~100~CALLOUT" } },
            { "assetResult": { "resourceName": "customers/111/assets/101" } },
            { "campaignAssetResult": { "resourceName": "customers/111/campaignAssets/5~101~STRUCTURED_SNIPPET" } }
        ]),
    )
    .await;
    let applied = apply(
        &h,
        "manage_extensions",
        json!({ "action": "add", "campaign_id": "5", "callouts": ["Free shipping"],
                "structured_snippets": [{ "header": "brands", "values": ["A", "B", "C"] }] }),
    )
    .await;
    let bodies = applied_mutates(&h.server).await;
    assert_eq!(
        bodies[0]["mutateOperations"],
        json!([
            { "assetOperation": { "create": { "resourceName": "customers/111/assets/-1",
                "calloutAsset": { "calloutText": "Free shipping" } } } },
            { "campaignAssetOperation": { "create": { "campaign": "customers/111/campaigns/5",
                "asset": "customers/111/assets/-1", "fieldType": "CALLOUT" } } },
            { "assetOperation": { "create": { "resourceName": "customers/111/assets/-2",
                "structuredSnippetAsset": { "header": "Brands", "values": ["A", "B", "C"] } } } },
            { "campaignAssetOperation": { "create": { "campaign": "customers/111/campaigns/5",
                "asset": "customers/111/assets/-2", "fieldType": "STRUCTURED_SNIPPET" } } }
        ])
    );
    let inv = applied.inverse.unwrap();
    assert_eq!(inv.tool, "remove_resources");
    assert_eq!(
        inv.input,
        json!({ "resource_names": [
            "customers/111/campaignAssets/5~100~CALLOUT",
            "customers/111/campaignAssets/5~101~STRUCTURED_SNIPPET"
        ] })
    );
}

#[tokio::test]
async fn account_level_sitelink() {
    let h = google_harness().await;
    mock_mutate_ok(
        &h.server,
        json!([
            { "assetResult": { "resourceName": "customers/111/assets/100" } },
            { "customerAssetResult": { "resourceName": "customers/111/customerAssets/100~SITELINK" } }
        ]),
    )
    .await;
    let applied = apply(
        &h,
        "manage_extensions",
        json!({ "action": "add",
                "sitelinks": [{ "link_text": "Shop", "final_url": "https://example.com/shop" }] }),
    )
    .await;
    let bodies = applied_mutates(&h.server).await;
    assert_eq!(
        bodies[0]["mutateOperations"][1],
        json!({ "customerAssetOperation": { "create": {
            "asset": "customers/111/assets/-1", "fieldType": "SITELINK" } } })
    );
    assert_eq!(
        applied.inverse.unwrap().input,
        json!({ "resource_names": ["customers/111/customerAssets/100~SITELINK"] })
    );
}

#[tokio::test]
async fn extensions_input_errors() {
    let h = google_harness().await;
    let cases = [
        (json!({ "action": "add", "campaign_id": "5" }), None),
        (
            json!({ "action": "add", "campaign_id": "5", "ad_group_id": "3", "callouts": ["x"] }),
            Some("ad_group_id"),
        ),
        (
            json!({ "action": "add", "callouts": ["this callout is far too long"] }),
            Some("callouts[0]"),
        ),
        (
            json!({ "action": "add", "structured_snippets": [{ "header": "Marcas", "values": ["a", "b", "c"] }] }),
            Some("structured_snippets[0].header"),
        ),
        (json!({ "action": "remove", "callouts": ["x"] }), None),
        (
            json!({ "action": "remove", "links": [{ "asset_id": "1", "field_type": "call-out" }] }),
            Some("links[0].field_type"),
        ),
    ];
    for (input, field) in cases {
        let e = plan_err(&h, "manage_extensions", input.clone()).await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{input}");
        assert_eq!(e.field.as_deref(), field, "{input}");
    }
}

#[tokio::test]
async fn remove_extension_not_linked_is_not_found_and_removed_counts_as_unlinked() {
    let h = google_harness().await;
    mock_search(
        &h.server,
        "111",
        "FROM campaign_asset WHERE",
        json!([{ "campaignAsset": { "resourceName": "customers/111/campaignAssets/5~100~CALLOUT",
                                    "status": "REMOVED" } }]),
    )
    .await;
    let e = plan_err(
        &h,
        "manage_extensions",
        json!({ "action": "remove", "campaign_id": "5",
                "links": [{ "asset_id": "100", "field_type": "callout" }] }),
    )
    .await;
    assert_eq!(e.code, codes::NOT_FOUND);
    assert!(e.message.contains("100 (CALLOUT)"), "{}", e.message);
}

#[tokio::test]
async fn remove_extension_is_reversible() {
    let h = google_harness().await;
    let link = "customers/111/adGroupAssets/3~100~SITELINK";
    mock_search(
        &h.server,
        "111",
        "FROM ad_group_asset WHERE",
        json!([{ "adGroupAsset": { "resourceName": link, "status": "ENABLED" } }]),
    )
    .await;
    mock_mutate_ok(
        &h.server,
        json!([{ "adGroupAssetResult": { "resourceName": link } }]),
    )
    .await;
    let applied = apply(
        &h,
        "manage_extensions",
        json!({ "action": "remove", "ad_group_id": "3",
                "links": [{ "asset_id": "100", "field_type": "SITELINK" }] }),
    )
    .await;
    let inv = applied.inverse.unwrap();
    assert_eq!(inv.tool, "restore_links");
    assert_eq!(inv.input, json!({ "resource_names": [link] }));
}

// ------------------------------------------------------------ GAQL fields

async fn mock_fields(server: &MockServer, needle: &str, results: Value) {
    Mock::given(method("POST"))
        .and(path(FIELDS_PATH))
        .and(body_string_contains(needle))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": results })))
        .mount(server)
        .await;
}

async fn read(
    h: &GoogleHarness,
    tool: &str,
    input: Value,
) -> Result<Value, adsconsole_mcp::ToolError> {
    let mut input = input;
    input["account_id"] = json!("111");
    h.adapter.read(&call(CONN, "111", tool, input)).await
}

#[tokio::test]
async fn gaql_fields_lists_attributes_metrics_and_segments() {
    let h = google_harness().await;
    mock_fields(
        &h.server,
        "WHERE name = 'campaign'",
        json!([{ "name": "campaign", "category": "RESOURCE",
                 "attributeResources": ["customer", "campaign_budget"],
                 "metrics": ["metrics.clicks", "metrics.cost_micros"],
                 "segments": ["segments.device", "segments.date"] }]),
    )
    .await;
    mock_fields(
        &h.server,
        "LIKE 'campaign.%'",
        json!([
            { "name": "campaign.status", "dataType": "ENUM", "selectable": true,
              "filterable": true, "sortable": true,
              "enumValues": ["UNSPECIFIED", "UNKNOWN", "ENABLED", "PAUSED", "REMOVED"] },
            { "name": "campaign.id", "dataType": "INT64", "selectable": true,
              "filterable": true, "sortable": true },
            { "name": "campaign.labels", "dataType": "RESOURCE_NAME", "selectable": true,
              "isRepeated": true, "filterable": true }
        ]),
    )
    .await;
    let out = read(&h, "get_gaql_fields", json!({ "resource": "campaign" }))
        .await
        .unwrap();
    assert_eq!(
        out["attributes"][0],
        json!({ "name": "campaign.id", "type": "INT64" })
    );
    assert_eq!(
        out["attributes"][2],
        json!({ "name": "campaign.status", "type": "ENUM",
                "values": ["ENABLED", "PAUSED", "REMOVED"] })
    );
    assert_eq!(
        out["attributes"][1],
        json!({ "name": "campaign.labels", "type": "RESOURCE_NAME", "repeated": true,
                "not_sortable": true })
    );
    assert_eq!(out["segments"], json!(["segments.date", "segments.device"]));
    assert_eq!(
        out["related_resources"],
        json!(["campaign_budget", "customer"])
    );
    assert!(out.get("truncated").is_none());

    let filtered = read(
        &h,
        "get_gaql_fields",
        json!({ "resource": "campaign", "filter": "STATUS" }),
    )
    .await
    .unwrap();
    assert_eq!(filtered["attributes"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["metrics"], json!([]));

    // The catalog is cached: the two calls sent only the first two queries.
    let sent = h
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == FIELDS_PATH)
        .count();
    assert_eq!(sent, 2);
}

#[tokio::test]
async fn gaql_fields_unknown_resource() {
    let h = google_harness().await;
    mock_fields(&h.server, "WHERE name = 'campaigns'", json!([])).await;
    let e = read(&h, "get_gaql_fields", json!({ "resource": "campaigns" }))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    let e = read(
        &h,
        "get_gaql_fields",
        json!({ "resource": "campaign.status" }),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn gaql_query_error_suggests_the_field_catalog() {
    let h = google_harness().await;
    Mock::given(method("POST"))
        .and(path("/v26/customers/111/googleAds:search"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": {
            "code": 400, "status": "INVALID_ARGUMENT", "message": "Request contains an invalid argument.",
            "details": [{
                "@type": "type.googleapis.com/google.ads.googleads.v26.errors.GoogleAdsFailure",
                "errors": [{ "errorCode": { "queryError": "UNRECOGNIZED_FIELD" },
                             "message": "Unrecognized field in the query: 'campaign.foo'." }]
            }]
        } })))
        .mount(&h.server)
        .await;
    let e = read(
        &h,
        "gaql_search",
        json!({ "query": "SELECT campaign.foo FROM campaign" }),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, codes::API_ERROR);
    assert!(e.suggestion.unwrap().contains("get_gaql_fields"));
}
