//! Leituras do Google Ads (gaql_search com cursor, relatórios) contra o wiremock.

mod common;

use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::error::codes;
use common::*;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const SEARCH: &str = "/v26/customers/111/googleAds:search";

async fn read(
    h: &GoogleHarness,
    tool: &str,
    input: Value,
) -> Result<Value, adsconsole_mcp::ToolError> {
    let mut input = input;
    input["account_id"] = json!("111");
    h.adapter.read(&call(CONN, "111", tool, input)).await
}

/// Consultas GAQL enviadas à conta 111, na ordem.
async fn queries(h: &GoogleHarness) -> Vec<String> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == SEARCH)
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["query"].as_str().unwrap().to_string()
        })
        .collect()
}

fn big_row(id: u32) -> Value {
    json!({ "campaign": { "id": id.to_string(), "name": "x".repeat(30_000) } })
}

async fn mount_pages(h: &GoogleHarness) {
    // Página 2 (corpo com pageToken) tem prioridade sobre a página 1.
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .and(body_string_contains("\"pageToken\":\"p2\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [big_row(6), big_row(7)]
        })))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": (1..=5).map(big_row).collect::<Vec<_>>(),
            "nextPageToken": "p2"
        })))
        .mount(&h.server)
        .await;
}

#[tokio::test]
async fn gaql_cursor_resumes_without_gaps() {
    let h = google_harness().await;
    mount_pages(&h).await;
    let q = "SELECT campaign.id, campaign.name FROM campaign";
    let mut ids = Vec::new();
    let mut cursor: Option<String> = None;
    let mut first_len = None;
    loop {
        let mut input = json!({ "query": q });
        if let Some(c) = &cursor {
            input["cursor"] = json!(c);
        }
        let out = read(&h, "gaql_search", input).await.unwrap();
        let rows = out["rows"].as_array().unwrap();
        assert!(serde_json::to_vec(&out).unwrap().len() <= 100_000);
        first_len.get_or_insert((rows.len(), out["truncated"].clone()));
        ids.extend(rows.iter().map(|r| {
            r["campaign"]["id"]
                .as_str()
                .unwrap()
                .parse::<u32>()
                .unwrap()
        }));
        match out.get("next_cursor").and_then(Value::as_str) {
            Some(c) => cursor = Some(c.to_string()),
            None => {
                assert_eq!(out["truncated"], json!(false));
                break;
            }
        }
    }
    assert_eq!(ids, (1..=7).collect::<Vec<u32>>());
    assert_eq!(first_len.unwrap(), (3, json!(true)));
}

#[tokio::test]
async fn gaql_cursor_is_opaque_base64url_json() {
    let h = google_harness().await;
    mount_pages(&h).await;
    let out = read(
        &h,
        "gaql_search",
        json!({ "query": "select campaign.id from campaign" }),
    )
    .await
    .unwrap();
    let c = out["next_cursor"].as_str().unwrap();
    assert!(
        c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    // Cursor de outra forma (não-base64, ou JSON sem os campos) é INVALID_INPUT.
    for bad in ["isto não é cursor!", "e30", "aGVsbG8"] {
        let err = read(
            &h,
            "gaql_search",
            json!({ "query": "SELECT campaign.id FROM campaign", "cursor": bad }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, codes::INVALID_INPUT, "{bad}");
    }
}

#[tokio::test]
async fn gaql_rejects_non_select() {
    let h = google_harness().await;
    for q in [
        "DELETE FROM campaign",
        "",
        "selectx campaign.id FROM campaign",
    ] {
        let err = read(&h, "gaql_search", json!({ "query": q }))
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_INPUT, "{q:?}");
    }
    assert!(queries(&h).await.is_empty());
}

fn campaign_row() -> Value {
    json!({
        "campaign": {
            "resourceName": "customers/111/campaigns/5", "id": "5", "name": "Marca",
            "status": "ENABLED", "advertisingChannelType": "SEARCH",
            "biddingStrategyType": "MAXIMIZE_CONVERSIONS",
            "startDateTime": "2026-01-05 00:00:00", "endDateTime": "2026-12-31 23:59:59"
        },
        "campaignBudget": { "amountMicros": "50000000" },
        "metrics": {
            "impressions": "1000", "clicks": "50", "costMicros": "12340000",
            "conversions": 3.5, "conversionsValue": 700.25, "ctr": 0.05,
            "averageCpc": 246800.0
        }
    })
}

#[tokio::test]
async fn get_campaigns_normalizes() {
    let h = google_harness().await;
    let mut weird = campaign_row();
    weird["campaign"]["id"] = json!("6");
    weird["campaign"]["status"] = json!("SOMETHING_NEW");
    mock_search(
        &h.server,
        "111",
        "FROM campaign",
        json!([campaign_row(), weird]),
    )
    .await;
    let out = read(&h, "get_campaigns", json!({})).await.unwrap();
    let c = &out["campaigns"][0];
    assert_eq!(c["campaign_id"], "5");
    assert_eq!(c["status"], "ACTIVE");
    assert_eq!(c["channel"], "SEARCH");
    assert_eq!(c["cost"], json!(12.34));
    assert_eq!(c["budget"], json!(50.0));
    assert_eq!(c["average_cpc"], json!(0.2468));
    assert_eq!(c["impressions"], 1000);
    assert_eq!(c["clicks"], 50);
    assert_eq!(c["conversions"], json!(3.5));
    assert_eq!(c["bidding_strategy"], "MAXIMIZE_CONVERSIONS");
    // Status desconhecido não derruba a leitura: fica o texto cru.
    assert_eq!(out["campaigns"][1]["status"], "SOMETHING_NEW");
    let q = &queries(&h).await[0];
    assert!(q.contains("DURING LAST_30_DAYS"), "{q}");
    assert!(q.contains("campaign.status != 'REMOVED'"), "{q}");
    assert!(q.contains("campaign_budget.amount_micros"), "{q}");
}

#[tokio::test]
async fn get_campaigns_filters_status_and_dates() {
    let h = google_harness().await;
    mock_search(&h.server, "111", "FROM campaign", json!([])).await;
    read(
        &h,
        "get_campaigns",
        json!({
            "status": ["ACTIVE", "PAUSED"],
            "date_range": { "start": "2026-01-01", "end": "2026-01-31" }
        }),
    )
    .await
    .unwrap();
    let q = &queries(&h).await[0];
    assert!(q.contains("BETWEEN '2026-01-01' AND '2026-01-31'"), "{q}");
    assert!(
        q.contains("campaign.status IN ('ENABLED', 'PAUSED')"),
        "{q}"
    );
    let err = read(
        &h,
        "get_campaigns",
        json!({ "date_range": { "start": "2026-02-01", "end": "2026-01-01" } }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn ids_and_limits_are_validated() {
    let h = google_harness().await;
    for (tool, input) in [
        ("get_campaign_details", json!({ "campaign_id": "5 OR 1=1" })),
        ("get_search_terms", json!({ "campaign_id": "x'" })),
        ("get_search_terms", json!({ "limit": 1001 })),
        ("get_search_terms", json!({ "limit": 0 })),
        ("get_change_history", json!({ "days": 31 })),
        ("get_change_history", json!({ "days": 0 })),
        ("get_change_history", json!({ "limit": 5000 })),
    ] {
        let err = read(&h, tool, input.clone()).await.unwrap_err();
        assert_eq!(err.code, codes::INVALID_INPUT, "{tool} {input}");
    }
    assert!(queries(&h).await.is_empty());
}

#[tokio::test]
async fn search_terms_ordered_and_limited() {
    let h = google_harness().await;
    mock_search(
        &h.server,
        "111",
        "FROM search_term_view",
        json!([{
            "searchTermView": { "searchTerm": "tênis", "status": "NONE" },
            "campaign": { "id": "5" }, "adGroup": { "id": "9" },
            "metrics": { "impressions": "10", "clicks": "2", "costMicros": "1500000", "conversions": 1.0 }
        }]),
    )
    .await;
    let out = read(&h, "get_search_terms", json!({ "campaign_id": "5" }))
        .await
        .unwrap();
    let t = &out["terms"][0];
    assert_eq!(t["search_term"], "tênis");
    assert_eq!(t["campaign_id"], "5");
    assert_eq!(t["ad_group_id"], "9");
    assert_eq!(t["cost"], json!(1.5));
    let q = &queries(&h).await[0];
    assert!(q.contains("campaign.id = 5"), "{q}");
    assert!(q.contains("ORDER BY metrics.impressions DESC"), "{q}");
    assert!(q.ends_with("LIMIT 100"), "{q}");
}

#[tokio::test]
async fn change_history_has_date_and_limit() {
    let h = google_harness().await;
    mock_search(
        &h.server,
        "111",
        "FROM change_event",
        json!([{
            "changeEvent": {
                "changeDateTime": "2026-10-16 10:00:00.000000", "userEmail": "a@b.c",
                "clientType": "GOOGLE_ADS_WEB_CLIENT", "changeResourceType": "CAMPAIGN",
                "changeResourceName": "customers/111/campaigns/5",
                "resourceChangeOperation": "UPDATE", "changedFields": "name",
                "campaign": "customers/111/campaigns/5"
            }
        }]),
    )
    .await;
    let out = read(&h, "get_change_history", json!({})).await.unwrap();
    let e = &out["events"][0];
    assert_eq!(e["user_email"], "a@b.c");
    assert_eq!(e["resource_type"], "CAMPAIGN");
    assert_eq!(e["operation"], "UPDATE");
    assert_eq!(e["campaign_id"], "5");
    let q = &queries(&h).await[0];
    // A API exige intervalo finito: limite inferior e superior, ambos datetime.
    let at = |op: &str| {
        let pat = format!("change_event.change_date_time {op} '");
        let rest = &q[q.find(&pat).expect(q) + pat.len()..];
        rest[..rest.find('\'').unwrap()].to_string()
    };
    let (lo, hi) = (at(">="), at("<="));
    let fmt = "%Y-%m-%d %H:%M:%S";
    let lo = chrono::NaiveDateTime::parse_from_str(&lo, fmt).expect(&lo);
    let hi = chrono::NaiveDateTime::parse_from_str(&hi, fmt).expect(&hi);
    let span = hi - lo;
    assert!(
        span >= chrono::Duration::days(6) && span <= chrono::Duration::days(7),
        "{q}"
    );
    assert!(
        (chrono::Utc::now().naive_utc() - hi).num_seconds().abs() < 60,
        "{q}"
    );
    assert!(q.ends_with("LIMIT 100"), "{q}");
}

#[tokio::test]
async fn recommendations_and_conversion_actions() {
    let h = google_harness().await;
    mock_search(
        &h.server,
        "111",
        "FROM recommendation",
        json!([{ "recommendation": {
            "resourceName": "customers/111/recommendations/abc", "type": "KEYWORD",
            "campaign": "customers/111/campaigns/5", "impact": { "baseMetrics": { "clicks": 1.0 } }
        }}]),
    )
    .await;
    mock_search(
        &h.server,
        "111",
        "FROM conversion_action",
        json!([{ "conversionAction": {
            "id": "77", "name": "Compra", "category": "PURCHASE", "status": "ENABLED",
            "type": "WEBPAGE", "primaryForGoal": true
        }}]),
    )
    .await;
    let out = read(&h, "get_recommendations", json!({ "types": ["KEYWORD"] }))
        .await
        .unwrap();
    let r = &out["recommendations"][0];
    assert_eq!(r["type"], "KEYWORD");
    assert_eq!(r["campaign_id"], "5");
    assert_eq!(r["impact"]["baseMetrics"]["clicks"], json!(1.0));
    assert!(queries(&h).await[0].contains("recommendation.type IN ('KEYWORD')"));
    let out = read(&h, "get_conversion_actions", json!({})).await.unwrap();
    let a = &out["conversion_actions"][0];
    assert_eq!(a["id"], "77");
    assert_eq!(a["status"], "ACTIVE");
    assert_eq!(a["primary_for_goal"], true);
}

#[tokio::test]
async fn campaign_details_shapes() {
    let h = google_harness().await;
    let s = &h.server;
    mock_search(s, "111", "FROM campaign WHERE", json!([campaign_row()])).await;
    mock_search(
        s,
        "111",
        "FROM ad_group WHERE",
        json!([{ "adGroup": { "id": "9", "name": "Grupo", "status": "PAUSED", "type": "SEARCH_STANDARD", "cpcBidMicros": "1500000" }}]),
    )
    .await;
    mock_search(
        s,
        "111",
        "FROM ad_group_criterion",
        json!([
            { "adGroup": { "id": "9" }, "adGroupCriterion": { "criterionId": "1", "status": "ENABLED",
              "negative": false, "keyword": { "text": "tênis", "matchType": "EXACT" }, "cpcBidMicros": "2000000" }},
            { "adGroup": { "id": "9" }, "adGroupCriterion": { "criterionId": "2", "status": "ENABLED",
              "negative": true, "keyword": { "text": "grátis", "matchType": "BROAD" }}}
        ]),
    )
    .await;
    mock_search(
        s,
        "111",
        "FROM ad_group_ad",
        json!([{ "adGroup": { "id": "9" }, "adGroupAd": { "status": "ENABLED", "ad": {
        "id": "30", "type": "RESPONSIVE_SEARCH_AD", "finalUrls": ["https://x.com"],
        "responsiveSearchAd": {
            "headlines": [{ "text": "H1", "pinnedField": "HEADLINE_1" }, { "text": "H2" }],
            "descriptions": [{ "text": "D1" }]
        }}}}]),
    )
    .await;
    mock_search(
        s,
        "111",
        "FROM campaign_criterion",
        json!([
            { "campaignCriterion": { "criterionId": "10", "type": "KEYWORD", "negative": true,
              "keyword": { "text": "barato", "matchType": "PHRASE" }}},
            { "campaignCriterion": { "criterionId": "11", "type": "LOCATION", "negative": false,
              "location": { "geoTargetConstant": "geoTargetConstants/1001773" }}},
            { "campaignCriterion": { "criterionId": "12", "type": "LANGUAGE",
              "language": { "languageConstant": "languageConstants/1014" }}},
            { "campaignCriterion": { "criterionId": "13", "type": "DEVICE", "negative": true,
              "device": { "type": "MOBILE" }}},
            { "campaignCriterion": { "criterionId": "14", "type": "AD_SCHEDULE",
              "adSchedule": { "dayOfWeek": "MONDAY", "startHour": 8, "startMinute": "ZERO",
                              "endHour": 18, "endMinute": "ZERO" }}}
        ]),
    )
    .await;
    mock_search(
        s,
        "111",
        "FROM campaign_asset",
        json!([{ "campaignAsset": { "fieldType": "SITELINK", "status": "ENABLED" },
                 "asset": { "id": "400", "name": "Link", "type": "SITELINK" }}]),
    )
    .await;
    let out = read(&h, "get_campaign_details", json!({ "campaign_id": "5" }))
        .await
        .unwrap();
    assert_eq!(out["campaign"]["campaign_id"], "5");
    assert_eq!(out["campaign"]["status"], "ACTIVE");
    assert_eq!(out["campaign"]["start_date"], "2026-01-05");
    assert_eq!(out["campaign"]["end_date"], "2026-12-31");
    let q = &queries(&h).await[0];
    assert!(
        q.contains("campaign.start_date_time, campaign.end_date_time"),
        "{q}"
    );
    // A API exige campaign.id no SELECT de toda consulta que filtra por ele.
    let all = queries(&h).await;
    assert!(all.len() >= 6, "{all:?}");
    for q in all {
        let w = q.find(" WHERE ").unwrap();
        if q[w..].contains("campaign.id") {
            assert!(q[..w].contains("campaign.id"), "{q}");
        }
    }
    let g = &out["ad_groups"][0];
    assert_eq!(g["ad_group_id"], "9");
    assert_eq!(g["status"], "PAUSED");
    assert_eq!(g["cpc_bid"], json!(1.5));
    assert_eq!(g["keywords"].as_array().unwrap().len(), 2);
    assert_eq!(g["keywords"][0]["text"], "tênis");
    assert_eq!(g["keywords"][0]["cpc_bid"], json!(2.0));
    assert_eq!(g["keywords"][1]["negative"], true);
    let ad = &g["ads"][0];
    assert_eq!(ad["ad_id"], "30");
    assert_eq!(ad["headlines"][0]["text"], "H1");
    assert_eq!(ad["headlines"][0]["pinned"], "HEADLINE_1");
    assert_eq!(ad["descriptions"][0]["text"], "D1");
    assert_eq!(ad["final_urls"][0], "https://x.com");
    assert_eq!(out["negatives"][0]["text"], "barato");
    assert_eq!(out["negatives"].as_array().unwrap().len(), 1);
    assert_eq!(
        out["targeting"]["locations"][0]["geo_target_constant"],
        "geoTargetConstants/1001773"
    );
    assert_eq!(
        out["targeting"]["languages"][0]["language_constant"],
        "languageConstants/1014"
    );
    assert_eq!(out["targeting"]["devices"][0]["device"], "MOBILE");
    assert_eq!(out["targeting"]["devices"][0]["negative"], true);
    assert_eq!(out["targeting"]["ad_schedule"][0]["day_of_week"], "MONDAY");
    assert_eq!(out["assets"][0]["field_type"], "SITELINK");
    assert_eq!(out["assets"][0]["asset_id"], "400");
    assert_eq!(out["asset_groups"], json!([]));
    assert_eq!(out["truncated"], false);
    let qs = queries(&h).await;
    assert!((5..=7).contains(&qs.len()), "{qs:?}");
    assert!(qs.iter().all(|q| q.contains("LIMIT ")), "{qs:?}");
}

#[tokio::test]
async fn campaign_details_pmax_has_asset_groups() {
    let h = google_harness().await;
    let s = &h.server;
    let mut row = campaign_row();
    row["campaign"]["advertisingChannelType"] = json!("PERFORMANCE_MAX");
    mock_search(s, "111", "FROM campaign WHERE", json!([row])).await;
    mock_search(s, "111", "FROM campaign_criterion", json!([])).await;
    mock_search(s, "111", "FROM campaign_asset", json!([])).await;
    mock_search(
        s,
        "111",
        "FROM asset_group WHERE",
        json!([{ "assetGroup": { "id": "70", "name": "AG", "status": "ENABLED" }}]),
    )
    .await;
    mock_search(
        s,
        "111",
        "FROM asset_group_asset",
        json!([{ "assetGroup": { "id": "70" }, "assetGroupAsset": { "fieldType": "HEADLINE", "status": "ENABLED" },
                 "asset": { "id": "1", "type": "TEXT", "textAsset": { "text": "Oi" }}}]),
    )
    .await;
    mock_search(
        s,
        "111",
        "FROM asset_group_signal",
        json!([{ "assetGroup": { "id": "70" }, "assetGroupSignal": { "searchTheme": { "text": "tênis" }}}]),
    )
    .await;
    let out = read(&h, "get_campaign_details", json!({ "campaign_id": "5" }))
        .await
        .unwrap();
    let ag = &out["asset_groups"][0];
    assert_eq!(ag["asset_group_id"], "70");
    assert_eq!(ag["assets"]["HEADLINE"][0]["text"], "Oi");
    assert_eq!(ag["signals"][0]["searchTheme"]["text"], "tênis");
    assert_eq!(out["ad_groups"], json!([]));
    assert!(queries(&h).await.len() <= 7);
}

#[tokio::test]
async fn campaign_details_not_found() {
    let h = google_harness().await;
    mock_search(&h.server, "111", "FROM campaign WHERE", json!([])).await;
    let err = read(&h, "get_campaign_details", json!({ "campaign_id": "5" }))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
}

#[tokio::test]
async fn read_tools_declare_output_schema() {
    let h = google_harness().await;
    let tools = h.adapter.tools();
    for name in [
        "gaql_search",
        "get_campaigns",
        "get_campaign_details",
        "get_search_terms",
        "get_recommendations",
        "get_conversion_actions",
        "get_change_history",
    ] {
        let t = tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name}"));
        assert!(t.output_schema.is_some(), "{name}");
        assert!(t.takes_account, "{name}");
    }
}

#[tokio::test]
async fn campaign_details_is_capped_at_100kb() {
    let h = google_harness().await;
    let s = &h.server;
    mock_search(s, "111", "FROM campaign WHERE", json!([campaign_row()])).await;
    mock_search(s, "111", "FROM campaign_criterion", json!([])).await;
    mock_search(s, "111", "FROM campaign_asset", json!([])).await;
    mock_search(
        s,
        "111",
        "FROM ad_group WHERE",
        json!([{ "adGroup": { "id": "9", "name": "G", "status": "ENABLED" }}]),
    )
    .await;
    mock_search(s, "111", "FROM ad_group_ad", json!([])).await;
    let kws: Vec<Value> = (0..1500)
        .map(|i| {
            json!({ "adGroup": { "id": "9" }, "adGroupCriterion": {
            "criterionId": i.to_string(), "status": "ENABLED", "negative": false,
            "keyword": { "text": format!("palavra-chave numero {i}"), "matchType": "BROAD" }}})
        })
        .collect();
    mock_search(s, "111", "FROM ad_group_criterion", json!(kws)).await;
    let out = read(&h, "get_campaign_details", json!({ "campaign_id": "5" }))
        .await
        .unwrap();
    assert!(serde_json::to_vec(&out).unwrap().len() <= 100_000);
    assert_eq!(out["truncated"], true);
    let n = out["ad_groups"][0]["keywords"].as_array().unwrap().len();
    assert!(n > 0 && n < 1500, "{n}");
}

#[tokio::test]
async fn hard_limit_sets_truncated() {
    let h = google_harness().await;
    let rows = |n: usize| -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({ "recommendation": {
                "resourceName": format!("customers/111/recommendations/{i}"), "type": "KEYWORD" }})
            })
            .collect()
    };
    mock_search(&h.server, "111", "FROM recommendation", json!(rows(500))).await;
    let out = read(&h, "get_recommendations", json!({})).await.unwrap();
    assert_eq!(out["truncated"], true);
    assert_eq!(out["recommendations"].as_array().unwrap().len(), 500);
}

async fn pmax_base(h: &GoogleHarness) {
    let mut row = campaign_row();
    row["campaign"]["advertisingChannelType"] = json!("PERFORMANCE_MAX");
    mock_search(&h.server, "111", "FROM campaign WHERE", json!([row])).await;
    mock_search(&h.server, "111", "FROM campaign_criterion", json!([])).await;
    mock_search(&h.server, "111", "FROM campaign_asset", json!([])).await;
}

#[tokio::test]
async fn campaign_details_trims_signals() {
    let h = google_harness().await;
    pmax_base(&h).await;
    mock_search(
        &h.server,
        "111",
        "FROM asset_group WHERE",
        json!([{ "assetGroup": { "id": "70", "name": "AG", "status": "ENABLED" }}]),
    )
    .await;
    mock_search(&h.server, "111", "FROM asset_group_asset", json!([])).await;
    let sig: Vec<Value> = (0..500)
        .map(|i| {
            json!({ "assetGroup": { "id": "70" }, "assetGroupSignal": {
            "searchTheme": { "text": format!("{i}{}", "t".repeat(400)) }}})
        })
        .collect();
    mock_search(&h.server, "111", "FROM asset_group_signal", json!(sig)).await;
    let out = read(&h, "get_campaign_details", json!({ "campaign_id": "5" }))
        .await
        .unwrap();
    assert!(serde_json::to_vec(&out).unwrap().len() <= 100_000);
    assert_eq!(out["truncated"], true);
    let n = out["asset_groups"][0]["signals"].as_array().unwrap().len();
    assert!(n > 0 && n < 500, "{n}");
}

#[tokio::test]
async fn campaign_details_drops_whole_groups_when_needed() {
    let h = google_harness().await;
    pmax_base(&h).await;
    // 100 grupos com nome de ~2 KB: só os cascos já passam de 100 KB.
    let groups: Vec<Value> = (0..100)
        .map(|i| json!({ "assetGroup": { "id": i.to_string(), "name": "n".repeat(2_000), "status": "ENABLED" }}))
        .collect();
    mock_search(&h.server, "111", "FROM asset_group WHERE", json!(groups)).await;
    mock_search(
        &h.server,
        "111",
        "FROM asset_group_asset",
        json!([{ "assetGroup": { "id": "0" }, "assetGroupAsset": { "fieldType": "HEADLINE", "status": "ENABLED" },
                 "asset": { "id": "1", "type": "TEXT", "textAsset": { "text": "Oi" }}}]),
    )
    .await;
    mock_search(&h.server, "111", "FROM asset_group_signal", json!([])).await;
    let out = read(&h, "get_campaign_details", json!({ "campaign_id": "5" }))
        .await
        .unwrap();
    assert!(serde_json::to_vec(&out).unwrap().len() <= 100_000);
    assert_eq!(out["truncated"], true);
    let n = out["asset_groups"].as_array().unwrap().len();
    assert!(n > 0 && n < 100, "{n}");
    // Sem chaves de tipo vazias.
    for g in out["asset_groups"].as_array().unwrap() {
        assert!(
            g["assets"]
                .as_object()
                .unwrap()
                .values()
                .all(|v| !v.as_array().unwrap().is_empty())
        );
    }
}

// ------------------------------------------------- keyword_ideas / keyword_metrics

const IDEAS: &str = "/v26/customers/111:generateKeywordIdeas";
const HISTORY: &str = "/v26/customers/111:generateKeywordHistoricalMetrics";
const FORECAST: &str = "/v26/customers/111:generateKeywordForecastMetrics";

async fn mount_post(h: &GoogleHarness, p: &str, status: u16, body: Value) {
    Mock::given(method("POST"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&h.server)
        .await;
}

async fn bodies(h: &GoogleHarness, p: &str) -> Vec<Value> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == p)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn auth_error_body() -> Value {
    json!({ "error": {
        "code": 403, "message": "The caller does not have permission", "status": "PERMISSION_DENIED",
        "details": [{
            "@type": "type.googleapis.com/google.ads.googleads.v26.errors.GoogleAdsFailure",
            "errors": [{
                "errorCode": { "authorizationError": "DEVELOPER_TOKEN_PROHIBITED" },
                "message": "Developer token is not allowed"
            }]
        }]
    }})
}

#[tokio::test]
async fn keyword_ideas_body_and_output() {
    let h = google_harness().await;
    mount_post(
        &h,
        IDEAS,
        200,
        json!({ "results": [{
            "text": "tênis corrida",
            "keywordIdeaMetrics": {
                "avgMonthlySearches": "1900", "competition": "HIGH",
                "lowTopOfPageBidMicros": "500000", "highTopOfPageBidMicros": "2250000"
            }
        }]}),
    )
    .await;
    let out = read(&h, "keyword_ideas", json!({ "keywords": ["tênis"] }))
        .await
        .unwrap();
    let idea = &out["ideas"][0];
    assert_eq!(idea["text"], "tênis corrida");
    assert_eq!(idea["avg_monthly_searches"], 1900);
    assert_eq!(idea["competition"], "HIGH");
    assert_eq!(idea["low_top_of_page_bid"], json!(0.5));
    assert_eq!(idea["high_top_of_page_bid"], json!(2.25));
    assert_eq!(out["truncated"], false);
    let b = &bodies(&h, IDEAS).await[0];
    assert_eq!(b["keywordSeed"]["keywords"], json!(["tênis"]));
    assert_eq!(b["language"], "languageConstants/1014");
    assert_eq!(b["geoTargetConstants"], json!(["geoTargetConstants/2076"]));
    assert_eq!(b["keywordPlanNetwork"], "GOOGLE_SEARCH");
}

#[tokio::test]
async fn keyword_ideas_seed_variants() {
    let h = google_harness().await;
    mount_post(&h, IDEAS, 200, json!({})).await;
    let out = read(&h, "keyword_ideas", json!({ "url": "https://a.com/x" }))
        .await
        .unwrap();
    assert_eq!(out["ideas"], json!([]));
    read(
        &h,
        "keyword_ideas",
        json!({ "url": "https://a.com", "keywords": ["a"], "language_id": "1000", "geo_ids": ["2840"] }),
    )
    .await
    .unwrap();
    let b = bodies(&h, IDEAS).await;
    assert_eq!(b[0]["urlSeed"]["url"], "https://a.com/x");
    assert_eq!(b[1]["keywordAndUrlSeed"]["url"], "https://a.com");
    assert_eq!(b[1]["keywordAndUrlSeed"]["keywords"], json!(["a"]));
    assert_eq!(b[1]["language"], "languageConstants/1000");
    assert_eq!(
        b[1]["geoTargetConstants"],
        json!(["geoTargetConstants/2840"])
    );
}

#[tokio::test]
async fn keyword_ideas_needs_seed() {
    let h = google_harness().await;
    let err = read(&h, "keyword_ideas", json!({})).await.unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(bodies(&h, IDEAS).await.is_empty());
}

#[tokio::test]
async fn keyword_tools_reject_bad_input() {
    let h = google_harness().await;
    let long = "k".repeat(81);
    let many: Vec<String> = (0..1001).map(|i| format!("k{i}")).collect();
    for (tool, input) in [
        ("keyword_ideas", json!({ "url": "ftp://a.com" })),
        (
            "keyword_ideas",
            json!({ "keywords": ["a"], "language_id": "pt'" }),
        ),
        (
            "keyword_ideas",
            json!({ "keywords": ["a"], "geo_ids": ["x"] }),
        ),
        ("keyword_ideas", json!({ "keywords": ["a"], "limit": 501 })),
        ("keyword_ideas", json!({ "keywords": ["a"], "limit": 0 })),
        ("keyword_ideas", json!({ "keywords": [long.clone()] })),
        ("keyword_metrics", json!({ "keywords": [] })),
        ("keyword_metrics", json!({ "keywords": many })),
        ("keyword_metrics", json!({ "keywords": [long] })),
        (
            "keyword_metrics",
            json!({ "keywords": ["a"], "forecast": { "daily_budget": 10, "days": 91 } }),
        ),
        (
            "keyword_metrics",
            json!({ "keywords": ["a"], "forecast": { "daily_budget": 10, "days": 0 } }),
        ),
        (
            "keyword_metrics",
            json!({ "keywords": ["a"], "forecast": { "daily_budget": 10.123 } }),
        ),
    ] {
        let err = read(&h, tool, input.clone()).await.unwrap_err();
        assert_eq!(err.code, codes::INVALID_INPUT, "{tool} {input}");
    }
    assert!(bodies(&h, IDEAS).await.is_empty());
    assert!(bodies(&h, HISTORY).await.is_empty());
}

#[tokio::test]
async fn keyword_ideas_limit_and_truncation() {
    let h = google_harness().await;
    let results: Vec<Value> = (0..3)
        .map(|i| json!({ "text": format!("{i}{}", "x".repeat(60_000)), "keywordIdeaMetrics": {} }))
        .collect();
    mount_post(&h, IDEAS, 200, json!({ "results": results })).await;
    let out = read(
        &h,
        "keyword_ideas",
        json!({ "keywords": ["a"], "limit": 500 }),
    )
    .await
    .unwrap();
    assert_eq!(out["truncated"], true);
    assert!(serde_json::to_vec(&out).unwrap().len() <= 100_000);
    assert_eq!(bodies(&h, IDEAS).await[0]["pageSize"], 500);
}

#[tokio::test]
async fn keyword_tools_basic_level_message() {
    let h = google_harness().await;
    mount_post(&h, IDEAS, 403, auth_error_body()).await;
    mount_post(&h, HISTORY, 403, auth_error_body()).await;
    for (tool, input) in [
        ("keyword_ideas", json!({ "keywords": ["a"] })),
        ("keyword_metrics", json!({ "keywords": ["a"] })),
    ] {
        let err = read(&h, tool, input).await.unwrap_err();
        assert_eq!(err.code, codes::ACCESS_LEVEL_INSUFFICIENT, "{tool}");
        assert!(err.message.contains("nível Basic"), "{}", err.message);
        assert!(
            err.suggestion
                .as_deref()
                .unwrap()
                .contains("brand verification"),
            "{tool}"
        );
    }
}

#[tokio::test]
async fn keyword_metrics_with_forecast() {
    let h = google_harness().await;
    mount_post(
        &h,
        HISTORY,
        200,
        json!({ "results": [{
            "text": "tênis",
            "keywordMetrics": {
                "avgMonthlySearches": "1000", "competition": "LOW",
                "lowTopOfPageBidMicros": "1000000", "highTopOfPageBidMicros": "3000000",
                "monthlySearchVolumes": [{ "year": "2026", "month": "JANUARY", "monthlySearches": "900" }]
            }
        }]}),
    )
    .await;
    mount_post(
        &h,
        FORECAST,
        200,
        json!({ "campaignForecastMetrics": {
            "impressions": 1200.5, "clicks": 80.25, "costMicros": "45500000", "conversions": 3.5
        }}),
    )
    .await;
    let out = read(
        &h,
        "keyword_metrics",
        json!({ "keywords": ["tênis"], "forecast": { "daily_budget": 50, "max_cpc": 1.5, "days": 10 } }),
    )
    .await
    .unwrap();
    let hist = &out["historical"][0];
    assert_eq!(hist["text"], "tênis");
    assert_eq!(hist["avg_monthly_searches"], 1000);
    assert_eq!(hist["low_top_of_page_bid"], json!(1.0));
    assert_eq!(hist["high_top_of_page_bid"], json!(3.0));
    assert_eq!(hist["monthly_searches"][0]["monthly_searches"], 900);
    let f = &out["forecast"];
    assert_eq!(f["cost"], json!(45.5));
    assert_eq!(f["impressions"], json!(1200.5));
    assert_eq!(f["clicks"], json!(80.25));
    assert_eq!(f["conversions"], json!(3.5));

    let hb = &bodies(&h, HISTORY).await[0];
    assert_eq!(hb["keywords"], json!(["tênis"]));
    assert_eq!(hb["language"], "languageConstants/1014");
    let fb = &bodies(&h, FORECAST).await[0];
    let c = &fb["campaign"];
    assert_eq!(c["keywordPlanNetwork"], "GOOGLE_SEARCH");
    let m = &c["biddingStrategy"]["manualCpcBiddingStrategy"];
    assert_eq!(m["dailyBudgetMicros"], "50000000");
    assert_eq!(m["maxCpcBidMicros"], "1500000");
    let kw = &c["adGroups"][0]["biddableKeywords"][0]["keyword"];
    assert_eq!(kw["text"], "tênis");
    assert_eq!(kw["matchType"], "BROAD");
    let p = &fb["forecastPeriod"];
    let tomorrow = chrono::Utc::now().date_naive() + chrono::Duration::days(1);
    assert_eq!(p["startDate"], tomorrow.to_string());
    assert_eq!(
        p["endDate"],
        (tomorrow + chrono::Duration::days(9)).to_string()
    );
}

#[tokio::test]
async fn keyword_metrics_without_forecast_skips_endpoint() {
    let h = google_harness().await;
    mount_post(&h, HISTORY, 200, json!({})).await;
    let out = read(&h, "keyword_metrics", json!({ "keywords": ["a"] }))
        .await
        .unwrap();
    assert_eq!(out["historical"], json!([]));
    assert!(out.get("forecast").is_none());
    assert!(bodies(&h, FORECAST).await.is_empty());
}

#[tokio::test]
async fn forecast_defaults_max_cpc_to_highest_historical_bid() {
    let h = google_harness().await;
    mount_post(
        &h,
        HISTORY,
        200,
        json!({ "results": [
            { "text": "a", "keywordMetrics": { "highTopOfPageBidMicros": "2000000" } },
            { "text": "b", "keywordMetrics": { "highTopOfPageBidMicros": "3500000" } }
        ]}),
    )
    .await;
    mount_post(&h, FORECAST, 200, json!({ "campaignForecastMetrics": {} })).await;
    read(
        &h,
        "keyword_metrics",
        json!({ "keywords": ["a", "b"], "forecast": { "daily_budget": 10 } }),
    )
    .await
    .unwrap();
    let fb = &bodies(&h, FORECAST).await[0];
    assert_eq!(
        fb["campaign"]["biddingStrategy"]["manualCpcBiddingStrategy"]["maxCpcBidMicros"],
        "3500000"
    );
}

#[tokio::test]
async fn forecast_without_any_bid_needs_max_cpc() {
    let h = google_harness().await;
    mount_post(
        &h,
        HISTORY,
        200,
        json!({ "results": [{ "text": "a", "keywordMetrics": {} }] }),
    )
    .await;
    mount_post(&h, FORECAST, 200, json!({})).await;
    let err = read(
        &h,
        "keyword_metrics",
        json!({ "keywords": ["a"], "forecast": { "daily_budget": 10 } }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("forecast.max_cpc"));
    assert_eq!(err.message, "Informe max_cpc para a previsão.");
    assert!(bodies(&h, FORECAST).await.is_empty());
}
