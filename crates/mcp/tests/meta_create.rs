mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use common::meta::*;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, Request, ResponseTemplate};

fn form(r: &Request) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(&r.body).into_owned().collect()
}

fn engine(h: &MetaHarness) -> Engine {
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Direct,
            ..ServerSettings::default()
        },
    );
    Engine::new(
        h.adapter.clone(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap()
}

/// Fakes Meta: each create POST returns a new id per type (c, s, k, a); an object GET
/// returns the name and status PAUSED; validate_only answers success; `fail` makes the
/// Nth real POST to `fail_path` return 400.
async fn fake_meta(h: &MetaHarness, fail: Option<(&'static str, usize)>) {
    Mock::given(method("GET"))
        .and(path("/act_111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "act_111", "account_status": 1})),
        )
        .mount(&h.server)
        .await;
    let names: Arc<Mutex<BTreeMap<String, String>>> = Arc::default();
    let counter: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();
    let n2 = names.clone();
    Mock::given(method("POST"))
        .and(path_regex(r"^/act_111/(campaigns|adsets|adcreatives|ads)$"))
        .respond_with(move |r: &Request| {
            let f = form(r);
            if f.contains_key("execution_options") {
                return ResponseTemplate::new(200).set_body_json(json!({"success": true}));
            }
            let edge = r.url.path().rsplit('/').next().unwrap().to_string();
            let mut c = counter.lock().unwrap();
            let n = c.entry(edge.clone()).or_default();
            *n += 1;
            if let Some((p, k)) = fail
                && p == edge
                && *n == k
            {
                return ResponseTemplate::new(400).set_body_json(
                    json!({"error": {"message": "Invalid parameter", "code": 100}}),
                );
            }
            let prefix = match edge.as_str() {
                "campaigns" => "c",
                "adsets" => "s",
                "adcreatives" => "k",
                _ => "a",
            };
            let id = format!("{prefix}{n}");
            n2.lock()
                .unwrap()
                .insert(id.clone(), f.get("name").cloned().unwrap_or_default());
            ResponseTemplate::new(200).set_body_json(json!({ "id": id }))
        })
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/[a-z]\d+$"))
        .respond_with(move |r: &Request| {
            let id = r.url.path()[1..].to_string();
            let name = names.lock().unwrap().get(&id).cloned().unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(
                json!({"id": id, "name": name, "status": "PAUSED", "account_id": "111"}),
            )
        })
        .mount(&h.server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
}

async fn real_posts(h: &MetaHarness) -> Vec<(String, BTreeMap<String, String>)> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && !form(r).contains_key("execution_options"))
        .map(|r| (r.url.path().to_string(), form(r)))
        .collect()
}

fn image_creative() -> Value {
    json!({"type": "image", "page_id": "p1", "link": "https://shop.example/x", "message": "Buy now",
           "headline": "Offer", "image_hash": "abc", "call_to_action": "SHOP_NOW"})
}

fn input(adsets: Value) -> Value {
    json!({
        "account_id": "act_111",
        "campaign": {"name": "Test campaign", "objective": "OUTCOME_TRAFFIC"},
        "adsets": adsets,
    })
}

fn adset(name: &str, ads: usize) -> Value {
    let ads: Vec<Value> = (0..ads)
        .map(|i| json!({"name": format!("{name} ad {i}"), "creative": image_creative()}))
        .collect();
    json!({"name": name, "daily_budget": 30, "optimization_goal": "LINK_CLICKS",
           "targeting": {"geo_locations": {"countries": ["BR"]}}, "ads": ads})
}

#[tokio::test]
async fn create_campaign_full_sequence() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let out = engine(&h)
        .call_tool(
            "create_campaign",
            input(json!([adset("S1", 1), adset("S2", 1)])),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    let posts = real_posts(&h).await;
    let order: Vec<&str> = posts
        .iter()
        .map(|(p, _)| p.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "campaigns",
            "adsets",
            "adcreatives",
            "ads",
            "adsets",
            "adcreatives",
            "ads"
        ]
    );
    let (_, campaign) = &posts[0];
    assert_eq!(campaign["status"], "PAUSED");
    assert_eq!(campaign["special_ad_categories"], "[]");
    assert_eq!(campaign["is_adset_budget_sharing_enabled"], "false");
    assert!(!campaign.contains_key("daily_budget"));
    let (_, s1) = &posts[1];
    assert_eq!(s1["campaign_id"], "c1");
    assert_eq!(s1["status"], "PAUSED");
    assert_eq!(s1["daily_budget"], "3000");
    assert_eq!(s1["billing_event"], "IMPRESSIONS");
    let (_, creative) = &posts[2];
    let spec: Value = serde_json::from_str(&creative["object_story_spec"]).unwrap();
    assert_eq!(spec["page_id"], "p1");
    assert_eq!(spec["link_data"]["image_hash"], "abc");
    assert_eq!(spec["link_data"]["name"], "Offer");
    assert_eq!(
        spec["link_data"]["call_to_action"],
        json!({"type": "SHOP_NOW", "value": {"link": "https://shop.example/x"}})
    );
    let (_, ad) = &posts[3];
    assert_eq!(ad["adset_id"], "s1");
    assert_eq!(
        serde_json::from_str::<Value>(&ad["creative"]).unwrap(),
        json!({"creative_id": "k1"})
    );
    assert_eq!(ad["status"], "PAUSED");
    let (_, s2) = &posts[4];
    assert_eq!(s2["campaign_id"], "c1");

    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    let inverse = rec.inverse.unwrap();
    assert_eq!(inverse.tool, "delete_objects");
    assert_eq!(inverse.input, json!({"ids": ["c1"]}));
    let after = rec.after.unwrap();
    assert_eq!(
        after["c1"],
        json!({"name": "Test campaign", "status": "PAUSED"})
    );
    assert_eq!(
        after.as_object().unwrap().len(),
        5,
        "campaign, 2 ad sets and 2 ads"
    );
    let summary = out["summary"].as_str().unwrap();
    assert!(
        summary.contains("2 ad sets") && summary.contains("2 ads"),
        "{summary}"
    );
    assert!(
        summary.contains("the creatives stay in the account"),
        "{summary}"
    );
}

#[tokio::test]
async fn create_without_id_shows_the_response() {
    // Seen on the real account: the ad POST returned 200 without `id`. The error must show
    // the response (without tokens) so it is clear what Meta returned.
    let h = meta_harness().await;
    Mock::given(method("POST"))
        .and(path("/act_111/ads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "link": "https://graph.facebook.com/x?access_token=EAAsecret"
        })))
        .with_priority(1)
        .mount(&h.server)
        .await;
    fake_meta(&h, None).await;
    let err = engine(&h)
        .call_tool("create_campaign", input(json!([adset("S1", 1)])), None)
        .await
        .unwrap_err();
    assert!(err.message.contains("success"), "{}", err.message);
    assert!(!err.message.contains("EAAsecret"), "{}", err.message);
}

#[tokio::test]
async fn budget_rules() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let e = engine(&h);
    let mut both = input(json!([adset("S1", 1)]));
    both["campaign"]["daily_budget"] = json!(100);
    let err = e
        .call_tool("create_campaign", both, None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);

    let mut none = adset("S1", 1);
    none.as_object_mut().unwrap().remove("daily_budget");
    let err = e
        .call_tool("create_campaign", input(json!([none])), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);

    let mut life = adset("S1", 1);
    life.as_object_mut().unwrap().remove("daily_budget");
    life["lifetime_budget"] = json!(500);
    let err = e
        .call_tool("create_campaign", input(json!([life])), None)
        .await
        .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("adsets[0].end_time"));
    assert!(real_posts(&h).await.is_empty());
}

#[tokio::test]
async fn campaign_budget_cbo() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let mut s = adset("S1", 1);
    s.as_object_mut().unwrap().remove("daily_budget");
    let mut i = input(json!([s]));
    i["campaign"]["daily_budget"] = json!(100);
    let out = engine(&h)
        .call_tool("create_campaign", i, None)
        .await
        .unwrap();
    let posts = real_posts(&h).await;
    assert_eq!(posts[0].1["daily_budget"], "10000");
    assert!(!posts[0].1.contains_key("is_adset_budget_sharing_enabled"));
    assert!(!posts[1].1.contains_key("daily_budget"));
    assert!(
        out["summary"].as_str().unwrap().contains("100 BRL/day"),
        "{}",
        out["summary"]
    );
}

#[tokio::test]
async fn creative_shapes() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let e = engine(&h);
    let with = |creative: Value| {
        input(
            json!([{"name": "S", "daily_budget": 30, "optimization_goal": "LINK_CLICKS",
            "targeting": {"geo_locations": {"countries": ["BR"]}},
            "ads": [{"name": "A", "creative": creative}]}]),
        )
    };
    let err = e
        .call_tool(
            "create_campaign",
            with(json!({"type": "video", "page_id": "p1", "link": "https://x.example", "message": "m", "video_id": "v1"})),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(err.field.unwrap().ends_with("thumbnail_image_hash"));
    let err = e
        .call_tool(
            "create_campaign",
            with(json!({"type": "carousel", "page_id": "p1", "link": "https://x.example", "message": "m",
                        "cards": [{"link": "https://x.example/1", "image_hash": "h1"}]})),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    let mut bad_cta = image_creative();
    bad_cta["call_to_action"] = json!("BUY_IT_NOW");
    let err = e
        .call_tool("create_campaign", with(bad_cta), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    let mut http = image_creative();
    http["link"] = json!("http://shop.example");
    let err = e
        .call_tool("create_campaign", with(http), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(real_posts(&h).await.is_empty());

    // Valid video with thumbnail and carousel.
    e.call_tool(
        "create_campaign",
        with(json!({"type": "video", "page_id": "p1", "instagram_user_id": "ig1", "link": "https://x.example",
                    "message": "m", "video_id": "v1", "thumbnail_url": "https://cdn.example/t.jpg"})),
        None,
    )
    .await
    .unwrap();
    let posts = real_posts(&h).await;
    let spec: Value = serde_json::from_str(&posts[2].1["object_story_spec"]).unwrap();
    assert_eq!(spec["instagram_user_id"], "ig1");
    assert_eq!(spec["video_data"]["video_id"], "v1");
    assert_eq!(spec["video_data"]["image_url"], "https://cdn.example/t.jpg");
    assert_eq!(spec["video_data"]["call_to_action"]["type"], "LEARN_MORE");
}

#[tokio::test]
async fn advantage_audience_defaulted() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    engine(&h)
        .call_tool("create_campaign", input(json!([adset("S1", 1)])), None)
        .await
        .unwrap();
    let posts = real_posts(&h).await;
    let t: Value = serde_json::from_str(&posts[1].1["targeting"]).unwrap();
    assert_eq!(t["targeting_automation"]["advantage_audience"], 0);
    assert_eq!(t["geo_locations"]["countries"][0], "BR");
}

#[tokio::test]
async fn targeting_needs_geo() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let mut s = adset("S1", 1);
    s["targeting"] = json!({"age_min": 20});
    let err = engine(&h)
        .call_tool("create_campaign", input(json!([s])), None)
        .await
        .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("adsets[0].targeting"));
}

#[tokio::test]
async fn create_campaign_rolls_back_on_ad_failure() {
    let h = meta_harness().await;
    fake_meta(&h, Some(("ads", 2))).await;
    let err = engine(&h)
        .call_tool(
            "create_campaign",
            input(json!([adset("S1", 1), adset("S2", 1)])),
            None,
        )
        .await
        .unwrap_err();
    assert_ne!(
        err.code,
        codes::PARTIAL_ROLLBACK,
        "the rollback must not be blocked by the local limiter"
    );
    let d = err.details.unwrap();
    assert_eq!(d["failed_step"], "ad_2_1");
    let deletes: Vec<String> = h
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(deletes.contains(&"/c1".to_string()), "{deletes:?}");
    assert_eq!(
        deletes.last().unwrap(),
        "/c1",
        "the campaign is the last one deleted"
    );
}

#[tokio::test]
async fn sales_without_promoted_object_warns() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Approval,
            approval_wait_secs: 0,
            ..ServerSettings::default()
        },
    );
    let e = Engine::new(
        h.adapter.clone(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap();
    let mut i = input(json!([adset("S1", 1)]));
    i["campaign"]["objective"] = json!("OUTCOME_SALES");
    e.call_tool("create_campaign", i, None).await.unwrap();
    let w = e.pending().unwrap()[0].warning.clone().unwrap();
    assert!(w.contains("promoted_object"), "{w}");
}

#[tokio::test]
async fn engagement_without_promoted_object_warns() {
    // On the real account, engagement without promoted_object is rejected at the ad.
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Approval,
            approval_wait_secs: 0,
            ..ServerSettings::default()
        },
    );
    let e = Engine::new(
        h.adapter.clone(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap();
    let mut i = input(json!([adset("S1", 1)]));
    i["campaign"]["objective"] = json!("OUTCOME_ENGAGEMENT");
    e.call_tool("create_campaign", i, None).await.unwrap();
    let w = e.pending().unwrap()[0].warning.clone().unwrap_or_default();
    assert!(w.contains("page_id"), "{w}");
}

#[tokio::test]
async fn create_ad_in_existing_adset() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    Mock::given(method("GET"))
        .and(path("/s99"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "s99", "account_id": "999"})),
        )
        .with_priority(1)
        .mount(&h.server)
        .await;
    let e = engine(&h);
    let err = e
        .call_tool(
            "create_ad",
            json!({"account_id": "act_111", "adset_id": "s99", "name": "A", "creative": image_creative()}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);

    let out = e
        .call_tool(
            "create_ad",
            json!({"account_id": "act_111", "adset_id": "s5", "name": "New ad", "creative": image_creative()}),
            None,
        )
        .await
        .unwrap();
    let posts = real_posts(&h).await;
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1].1["adset_id"], "s5");
    assert_eq!(posts[1].1["status"], "PAUSED");
    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        rec.inverse.unwrap().input,
        json!({"ids": ["a1"]}),
        "the creative stays"
    );
    e.call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap();

    // With an existing creative, only the ad.
    e.call_tool(
        "create_ad",
        json!({"account_id": "act_111", "adset_id": "s5", "name": "Other", "creative_id": "k9"}),
        None,
    )
    .await
    .unwrap();
    let posts = real_posts(&h).await;
    let last = &posts.last().unwrap().1;
    assert_eq!(
        serde_json::from_str::<Value>(&last["creative"]).unwrap(),
        json!({"creative_id": "k9"})
    );
}

#[tokio::test]
async fn existing_post_creative() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let e = engine(&h);
    let with = |creative: Value| {
        input(
            json!([{"name": "S", "daily_budget": 30, "optimization_goal": "LINK_CLICKS",
            "targeting": {"geo_locations": {"countries": ["BR"]}},
            "ads": [{"name": "A", "creative": creative}]}]),
        )
    };
    e.call_tool(
        "create_campaign",
        with(json!({"type": "existing_post", "object_story_id": "123_456"})),
        None,
    )
    .await
    .unwrap();
    let posts = real_posts(&h).await;
    let creative = &posts[2].1;
    assert_eq!(creative["object_story_id"], "123_456");
    assert!(!creative.contains_key("object_story_spec"));
    let err = e
        .call_tool(
            "create_campaign",
            with(json!({"type": "existing_post", "object_story_id": "456"})),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(err.field.unwrap().ends_with("object_story_id"));
}

#[tokio::test]
async fn default_bid_strategy_without_cap() {
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    engine(&h)
        .call_tool("create_campaign", input(json!([adset("S1", 1)])), None)
        .await
        .unwrap();
    let posts = real_posts(&h).await;
    assert_eq!(posts[1].1["bid_strategy"], "LOWEST_COST_WITHOUT_CAP");
    // With bid_amount, the user's chosen strategy stays (or Meta's default).
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let mut s = adset("S1", 1);
    s["bid_amount"] = json!(2);
    s["bid_strategy"] = json!("LOWEST_COST_WITH_BID_CAP");
    engine(&h)
        .call_tool("create_campaign", input(json!([s])), None)
        .await
        .unwrap();
    let posts = real_posts(&h).await;
    assert_eq!(posts[1].1["bid_strategy"], "LOWEST_COST_WITH_BID_CAP");
    // With a campaign budget, the strategy goes on the campaign.
    let h = meta_harness().await;
    fake_meta(&h, None).await;
    let mut s = adset("S1", 1);
    s.as_object_mut().unwrap().remove("daily_budget");
    let mut i = input(json!([s]));
    i["campaign"]["daily_budget"] = json!(100);
    engine(&h)
        .call_tool("create_campaign", i, None)
        .await
        .unwrap();
    let posts = real_posts(&h).await;
    assert_eq!(posts[0].1["bid_strategy"], "LOWEST_COST_WITHOUT_CAP");
    assert!(!posts[1].1.contains_key("bid_strategy"));
}
