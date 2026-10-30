mod common;

use adsconsole_core::Platform;
use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::error::codes;
use common::meta::*;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, Request, ResponseTemplate};

fn query(r: &Request) -> std::collections::HashMap<String, String> {
    r.url.query_pairs().into_owned().collect()
}

async fn read(
    h: &MetaHarness,
    account: &str,
    tool: &str,
    input: Value,
) -> Result<Value, adsconsole_mcp::ToolError> {
    h.adapter.read(&meta_call(account, tool, input)).await
}

fn campaign(id: &str, spend: &str) -> Value {
    json!({
        "id": id, "name": format!("Campanha {id}"), "status": "PAUSED", "effective_status": "PAUSED",
        "objective": "OUTCOME_TRAFFIC", "daily_budget": "2540", "bid_strategy": "LOWEST_COST_WITHOUT_CAP",
        "insights": {"data": [{
            "spend": spend, "impressions": "1000", "reach": "800", "clicks": "50", "ctr": "5.0",
            "cpc": "0.25", "cpm": "12.34",
            "actions": [{"action_type": "link_click", "value": "40"}, {"action_type": "post_engagement", "value": "7"}]
        }]}
    })
}

#[tokio::test]
async fn campaigns_with_insights() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/act_111/campaigns"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [campaign("c1", "12.34")]})),
        )
        .mount(&h.server)
        .await;
    let out = read(
        &h,
        "act_111",
        "get_campaigns",
        json!({"period": {"since": "2026-10-01", "until": "2026-10-15"}}),
    )
    .await
    .unwrap();
    let c = &out["campaigns"][0];
    assert_eq!(c["campaign_id"], "c1");
    assert_eq!(c["daily_budget"], 25.4);
    assert_eq!(c["metrics"]["spend"], 12.34);
    assert_eq!(c["metrics"]["impressions"], 1000);
    assert_eq!(c["metrics"]["clicks"], 50);
    assert_eq!(c["metrics"]["cpm"], 12.34);
    assert_eq!(c["metrics"]["results"], 40.0);
    assert_eq!(
        c["metrics"]["actions"][0],
        json!({"type": "link_click", "value": 40.0})
    );
    assert_eq!(out["truncated"], false);
    let req = &h.server.received_requests().await.unwrap()[0];
    let fields = &query(req)["fields"];
    assert!(
        fields.contains(r#"insights.time_range({"since":"2026-10-01","until":"2026-10-15"})"#),
        "{fields}"
    );
}

#[tokio::test]
async fn campaigns_default_period_and_status_filter() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/act_111/campaigns"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&h.server)
        .await;
    read(
        &h,
        "act_111",
        "get_campaigns",
        json!({"status": ["ACTIVE", "PAUSED"]}),
    )
    .await
    .unwrap();
    let req = &h.server.received_requests().await.unwrap()[0];
    let q = query(req);
    assert!(
        q["fields"].contains("insights.date_preset(last_30d)"),
        "{}",
        q["fields"]
    );
    assert_eq!(q["effective_status"], r#"["ACTIVE","PAUSED"]"#);
}

#[tokio::test]
async fn campaigns_jpy_budget() {
    let h = meta_harness().await;
    seed_meta_connection(
        &h,
        "m_jp",
        "TJ",
        None,
        &[meta_account("act_7", "JPY", "ACTIVE")],
    );
    Mock::given(method("GET"))
        .and(path("/act_7/campaigns"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{
            "id": "c7", "name": "x", "status": "PAUSED", "daily_budget": "1000"
        }]})))
        .mount(&h.server)
        .await;
    let mut call = meta_call("act_7", "get_campaigns", json!({}));
    call.connection_id = "m_jp".into();
    let out = h.adapter.read(&call).await.unwrap();
    assert_eq!(out["campaigns"][0]["daily_budget"], 1000);
    assert!(out["campaigns"][0]["metrics"].is_null());
}

#[tokio::test]
async fn invalid_period_refused() {
    let h = meta_harness().await;
    let err = read(
        &h,
        "act_111",
        "get_campaigns",
        json!({"period": {"since": "2026-10-15", "until": "2026-10-01"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    let err = read(&h, "act_111", "get_campaigns", json!({"period": "ontem"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
}

async fn mock_details(h: &MetaHarness, account_id: &str, ads: usize) {
    Mock::given(method("GET"))
        .and(path("/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "name": "Campanha", "account_id": account_id, "status": "PAUSED",
            "objective": "OUTCOME_TRAFFIC", "daily_budget": "5000"
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/c1/adsets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{
            "id": "s1", "name": "Ad set", "status": "PAUSED", "lifetime_budget": "100000",
            "targeting": {"geo_locations": {"countries": ["BR"]}}
        }]})))
        .mount(&h.server)
        .await;
    let big = "x".repeat(1_000);
    let list: Vec<Value> = (0..ads)
        .map(|i| json!({"id": format!("a{i}"), "name": format!("Anúncio {i}"), "status": "PAUSED",
                        "adset_id": "s1", "creative": {"id": "k1", "object_story_spec": {"link_data": {"message": big}}}}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/c1/ads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": list})))
        .mount(&h.server)
        .await;
}

#[tokio::test]
async fn details_with_children() {
    let h = meta_harness().await;
    mock_details(&h, "111", 2).await;
    let out = read(
        &h,
        "act_111",
        "get_campaign_details",
        json!({"campaign_id": "c1"}),
    )
    .await
    .unwrap();
    assert_eq!(out["campaign"]["campaign_id"], "c1");
    assert_eq!(out["campaign"]["daily_budget"], 50);
    assert_eq!(out["adsets"][0]["lifetime_budget"], 1000);
    assert_eq!(
        out["adsets"][0]["targeting"]["geo_locations"]["countries"][0],
        "BR"
    );
    assert_eq!(out["ads"].as_array().unwrap().len(), 2);
    assert_eq!(out["ads"][1]["creative"]["id"], "k1");
    assert_eq!(out["truncated"], false);
}

#[tokio::test]
async fn details_hard_cap() {
    let h = meta_harness().await;
    mock_details(&h, "111", 300).await;
    let out = read(
        &h,
        "act_111",
        "get_campaign_details",
        json!({"campaign_id": "c1"}),
    )
    .await
    .unwrap();
    assert!(
        out.to_string().len() <= 100_000,
        "{}",
        out.to_string().len()
    );
    assert_eq!(out["truncated"], true);
    assert_eq!(out["adsets"].as_array().unwrap().len(), 1);
    assert!(out["ads"].as_array().unwrap().len() < 300);
}

#[tokio::test]
async fn details_other_account_not_found() {
    let h = meta_harness().await;
    mock_details(&h, "999", 1).await;
    let err = read(
        &h,
        "act_111",
        "get_campaign_details",
        json!({"campaign_id": "c1"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
}

#[tokio::test]
async fn insights_breakdowns_and_period() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/act_111/insights"))
        .and(query_param("level", "campaign"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{
            "campaign_id": "c1", "campaign_name": "C", "age": "25-34", "gender": "female",
            "spend": "3.50", "impressions": "100", "date_start": "2026-10-01", "date_stop": "2026-10-31"
        }]})))
        .mount(&h.server)
        .await;
    let out = read(
        &h,
        "act_111",
        "get_insights",
        json!({"level": "campaign", "breakdowns": ["age", "gender"], "time_increment": "monthly"}),
    )
    .await
    .unwrap();
    let row = &out["rows"][0];
    assert_eq!(row["campaign_id"], "c1");
    assert_eq!(row["age"], "25-34");
    assert_eq!(row["metrics"]["spend"], 3.5);
    let req = &h.server.received_requests().await.unwrap()[0];
    let q = query(req);
    assert_eq!(q["breakdowns"], "age,gender");
    assert_eq!(q["time_increment"], "monthly");
    assert_eq!(q["date_preset"], "last_30d");

    let err = read(
        &h,
        "act_111",
        "get_insights",
        json!({"level": "campaign", "breakdowns": ["cor"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("breakdowns"));
    let err = read(
        &h,
        "act_111",
        "get_insights",
        json!({"level": "ad", "fields": ["senha"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("fields"));
}

#[tokio::test]
async fn insights_object_must_be_of_account() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/s9"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "s9", "account_id": "999"})),
        )
        .mount(&h.server)
        .await;
    let err = read(
        &h,
        "act_111",
        "get_insights",
        json!({"level": "adset", "object_id": "s9"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
}

#[tokio::test]
async fn insights_cursor_resumes_without_gaps() {
    let h = meta_harness().await;
    let big = "y".repeat(400);
    let rows = |from: usize, to: usize| -> Vec<Value> {
        (from..to)
            .map(|i| json!({"ad_id": format!("a{i}"), "ad_name": big, "spend": "1.00"}))
            .collect()
    };
    for (after, next, from, to) in [
        (None, Some("P2"), 0, 100),
        (Some("P2"), Some("P3"), 100, 200),
        (Some("P3"), None, 200, 300),
    ] {
        let mut body = json!({"data": rows(from, to)});
        if let Some(n) = next {
            body["paging"] = json!({"cursors": {"after": n}, "next": "https://graph/next"});
        }
        let m = Mock::given(method("GET")).and(path("/act_111/insights"));
        let m = match after {
            Some(a) => m.and(query_param("after", a)),
            None => m.and(|r: &Request| !r.url.query().unwrap_or("").contains("after=")),
        };
        m.respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&h.server)
            .await;
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let mut input = json!({"level": "ad", "limit": 500});
        if let Some(c) = &cursor {
            input["cursor"] = json!(c);
        }
        let out = read(&h, "act_111", "get_insights", input).await.unwrap();
        assert!(out.to_string().len() <= 100_000);
        for r in out["rows"].as_array().unwrap() {
            seen.push(r["ad_id"].as_str().unwrap().to_string());
        }
        match out["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    let expected: Vec<String> = (0..300).map(|i| format!("a{i}")).collect();
    assert_eq!(seen, expected);
    let _ = Platform::MetaAds;
}

#[tokio::test]
async fn pages_without_tokens() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/me/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "p1", "name": "Loja", "category": "Varejo", "access_token": "EAAPAGE",
             "instagram_business_account": {"id": "ig1", "username": "loja"}, "tasks": ["ADVERTISE", "ANALYZE"]},
            {"id": "p2", "name": "Outra", "access_token": "EAAP2", "tasks": ["ANALYZE"]}
        ]})))
        .mount(&h.server)
        .await;
    let out = read(&h, "act_111", "get_pages", json!({})).await.unwrap();
    assert!(!out.to_string().contains("EAAP"), "{out}");
    let p = &out["pages"][0];
    assert_eq!(p["page_id"], "p1");
    assert_eq!(p["instagram"]["username"], "loja");
    assert_eq!(p["can_advertise"], true);
    assert_eq!(out["pages"][1]["can_advertise"], false);
    assert!(out["pages"][1]["instagram"].is_null());
}

#[tokio::test]
async fn targeting_types_map_to_endpoints() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("type", "adinterest"))
        .and(query_param("q", "futebol"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "6003", "name": "Futebol", "audience_size_lower_bound": 1000, "audience_size_upper_bound": 2000, "path": ["Esportes", "Futebol"]}
        ]})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("type", "adTargetingCategory"))
        .and(query_param("class", "behaviors"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "b1", "name": "Viajantes frequentes", "type": "behaviors"},
            {"id": "b2", "name": "Compradores online", "type": "behaviors"}
        ]})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("type", "adTargetingCategory"))
        .and(query_param("class", "demographics"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "d1", "name": "Pais"}]})),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("type", "adgeolocation"))
        .and(query_param("location_types", r#"["city"]"#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"key": "269969", "name": "Campinas", "type": "city", "country_code": "BR", "region": "São Paulo"}
        ]})))
        .mount(&h.server)
        .await;
    let out = read(
        &h,
        "act_111",
        "search_targeting",
        json!({"type": "interest", "query": "futebol"}),
    )
    .await
    .unwrap();
    assert_eq!(out["results"][0]["id"], "6003");
    assert_eq!(out["results"][0]["audience_size_upper"], 2000);
    assert_eq!(out["results"][0]["path"], json!(["Esportes", "Futebol"]));
    let out = read(
        &h,
        "act_111",
        "search_targeting",
        json!({"type": "behavior", "query": "viaj"}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["results"].as_array().unwrap().len(),
        1,
        "filtra localmente"
    );
    let out = read(
        &h,
        "act_111",
        "search_targeting",
        json!({"type": "demographic"}),
    )
    .await
    .unwrap();
    assert_eq!(out["results"][0]["name"], "Pais");
    let out = read(
        &h,
        "act_111",
        "search_targeting",
        json!({"type": "geo", "query": "campinas", "geo_types": ["city"]}),
    )
    .await
    .unwrap();
    assert_eq!(out["results"][0]["key"], "269969");
    assert_eq!(out["results"][0]["country_code"], "BR");
    let err = read(
        &h,
        "act_111",
        "search_targeting",
        json!({"type": "interest"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn audience_requires_geo() {
    let h = meta_harness().await;
    let err = read(
        &h,
        "act_111",
        "estimate_audience",
        json!({"targeting": {"age_min": 18}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(err.message.contains("geo_locations"));
}

#[tokio::test]
async fn audience_estimate_parsed() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/act_111/delivery_estimate"))
        .and(query_param("optimization_goal", "REACH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{
            "estimate_mau_lower_bound": 120000, "estimate_mau_upper_bound": 150000, "estimate_ready": true
        }]})))
        .mount(&h.server)
        .await;
    let t = json!({"geo_locations": {"countries": ["BR"]}});
    let out = read(&h, "act_111", "estimate_audience", json!({"targeting": t}))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({"users_lower": 120000, "users_upper": 150000, "ready": true})
    );
    let req = &h.server.received_requests().await.unwrap()[0];
    let spec: Value = serde_json::from_str(&query(req)["targeting_spec"]).unwrap();
    assert_eq!(spec, t);
}

#[tokio::test]
async fn ad_library_access_message() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/ads_archive"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Application does not have permission for this action", "code": 10, "error_subcode": 2332002
        }})))
        .mount(&h.server)
        .await;
    let err = read(
        &h,
        "act_111",
        "search_ad_library",
        json!({"query": "tênis", "countries": ["BR"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::ACCESS_DENIED);
    assert!(
        err.message.contains("Biblioteca de Anúncios"),
        "{}",
        err.message
    );
    assert!(
        err.suggestion
            .unwrap()
            .contains("facebook.com/ads/library/api")
    );
    let err = read(
        &h,
        "act_111",
        "search_ad_library",
        json!({"query": "x", "countries": []}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("countries"));
}

#[tokio::test]
async fn ad_library_results() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/ads_archive"))
        .and(query_param("ad_reached_countries", r#"["BR"]"#))
        .and(query_param("search_terms", "tênis"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "lib1", "page_name": "Marca", "ad_snapshot_url": "https://www.facebook.com/ads/archive/render_ad/?id=1&access_token=SEGREDO"}
        ]})))
        .mount(&h.server)
        .await;
    let out = read(
        &h,
        "act_111",
        "search_ad_library",
        json!({"query": "tênis", "countries": ["br"]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ads"][0]["page_name"], "Marca");
    assert!(
        !out.to_string().contains("SEGREDO"),
        "a URL não leva o token"
    );
}

#[tokio::test]
async fn page_posts_use_page_token_without_returning_it() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/me/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "p1", "name": "Loja", "access_token": "PAGETOKEN"}
        ]})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/p1/published_posts"))
        .and(|r: &Request| {
            r.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer PAGETOKEN")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "p1_777", "message": "Promoção", "created_time": "2026-10-01T10:00:00+0000",
             "permalink_url": "https://www.facebook.com/p1/posts/777", "access_token": "x"}
        ]})))
        .mount(&h.server)
        .await;
    let out = read(&h, "act_111", "get_page_posts", json!({"page_id": "p1"}))
        .await
        .unwrap();
    assert_eq!(out["posts"][0]["object_story_id"], "p1_777");
    assert_eq!(out["posts"][0]["message"], "Promoção");
    assert!(!out.to_string().contains("PAGETOKEN"));
    let err = read(&h, "act_111", "get_page_posts", json!({"page_id": "p9"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
}
