mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use common::meta::*;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
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

/// Conta ativa, campanha c1 da conta com `ads` anúncios, e GET das cópias.
async fn setup(h: &MetaHarness, ads: usize) {
    Mock::given(method("GET"))
        .and(path("/act_111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "act_111", "account_status": 1})),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/c1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "c1", "account_id": "111", "name": "Original"})),
        )
        .mount(&h.server)
        .await;
    let list: Vec<Value> = (0..ads).map(|i| json!({"id": format!("a{i}")})).collect();
    Mock::given(method("GET"))
        .and(path("/c1/ads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": list})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/c2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "c2", "name": "Original (cópia)", "status": "PAUSED"})),
        )
        .mount(&h.server)
        .await;
}

async fn non_get(h: &MetaHarness) -> Vec<(String, String, BTreeMap<String, String>)> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() != "GET")
        .map(|r| (r.method.to_string(), r.url.path().to_string(), form(r)))
        .collect()
}

#[tokio::test]
async fn sync_copy_up_to_three() {
    let h = meta_harness().await;
    setup(&h, 3).await;
    Mock::given(method("POST"))
        .and(path("/c1/copies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "copied_campaign_id": "c2",
            "ad_object_ids": [{"ad_object_type": "campaign", "source_id": "c1", "copied_id": "c2"}]
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    let out = engine(&h)
        .call_tool(
            "duplicate",
            json!({"account_id": "act_111", "object_type": "campaign", "object_id": "c1"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(
        out["result"]["c2"],
        json!({"name": "Original (cópia)", "status": "PAUSED"})
    );
    let calls = non_get(&h).await;
    assert_eq!(calls.len(), 1);
    let f = &calls[0].2;
    assert_eq!(f["deep_copy"], "true");
    assert_eq!(f["status_option"], "PAUSED");
    assert_eq!(
        serde_json::from_str::<Value>(&f["rename_options"]).unwrap(),
        json!({"rename_suffix": " (cópia)"})
    );
    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(rec.inverse.unwrap().input, json!({"ids": ["c2"]}));
    assert_eq!(rec.progress.unwrap()["created"][0]["id"], "c2");
}

#[tokio::test]
async fn async_copy_four_to_fifty_one() {
    let h = meta_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(5), Duration::from_secs(5));
    setup(&h, 10).await;
    Mock::given(method("POST"))
        .and(path("/act_111/async_batch_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "set9"})))
        .expect(1)
        .mount(&h.server)
        .await;
    let polls = Arc::new(Mutex::new(0));
    Mock::given(method("GET"))
        .and(path("/set9"))
        .respond_with(move |_: &Request| {
            let mut n = polls.lock().unwrap();
            *n += 1;
            if *n < 3 {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": "set9", "status": "IN_PROGRESS"}))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "set9", "status": "COMPLETED", "success_count": 1, "error_count": 0,
                    "requests": {"data": [{"status": "SUCCESS",
                        "result": "{\"copied_campaign_id\":\"c2\",\"ad_object_ids\":[]}"}]}
                }))
            }
        })
        .mount(&h.server)
        .await;
    let out = engine(&h)
        .call_tool(
            "duplicate",
            json!({"account_id": "act_111", "object_type": "campaign", "object_id": "c1", "rename_suffix": " v2"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    let calls = non_get(&h).await;
    assert_eq!(calls.len(), 1);
    let batch: Value = serde_json::from_str(&calls[0].2["adbatch"]).unwrap();
    assert_eq!(batch[0]["relative_url"], "c1/copies");
    let body = batch[0]["body"].as_str().unwrap();
    assert!(
        body.contains("deep_copy=true") && body.contains("status_option=PAUSED"),
        "{body}"
    );
    assert!(body.contains("rename_suffix"), "{body}");
    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(rec.inverse.unwrap().input, json!({"ids": ["c2"]}));
}

#[tokio::test]
async fn over_fifty_one_refused() {
    let h = meta_harness().await;
    setup(&h, 52).await;
    let err = engine(&h)
        .call_tool(
            "duplicate",
            json!({"account_id": "act_111", "object_type": "campaign", "object_id": "c1"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(err.message.contains("51"), "{}", err.message);
    assert!(non_get(&h).await.is_empty());
}

#[tokio::test]
async fn undo_deletes_copy() {
    let h = meta_harness().await;
    setup(&h, 1).await;
    Mock::given(method("POST"))
        .and(path("/c1/copies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"copied_campaign_id": "c2"})))
        .mount(&h.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/c2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .expect(1)
        .mount(&h.server)
        .await;
    let e = engine(&h);
    let out = e
        .call_tool(
            "duplicate",
            json!({"account_id": "act_111", "object_type": "campaign", "object_id": "c1"}),
            None,
        )
        .await
        .unwrap();
    e.call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn async_timeout_keeps_request_id() {
    let h = meta_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(5), Duration::from_millis(40));
    setup(&h, 5).await;
    Mock::given(method("POST"))
        .and(path("/act_111/async_batch_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "set9"})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/set9"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "set9", "status": "IN_PROGRESS"})),
        )
        .mount(&h.server)
        .await;
    let err = engine(&h)
        .call_tool(
            "duplicate",
            json!({"account_id": "act_111", "object_type": "campaign", "object_id": "c1"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::EXECUTION_FAILED);
    assert_eq!(err.details.unwrap()["async_request_set_id"], "set9");
}

#[tokio::test]
async fn adset_copy_to_other_campaign() {
    let h = meta_harness().await;
    setup(&h, 1).await;
    Mock::given(method("GET"))
        .and(path("/s1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "s1", "account_id": "111", "name": "S"})),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s1/ads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "a1"}]})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "s2", "name": "S (cópia)", "status": "PAUSED"})),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/s1/copies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"copied_adset_id": "s2"})))
        .mount(&h.server)
        .await;
    engine(&h)
        .call_tool(
            "duplicate",
            json!({"account_id": "act_111", "object_type": "adset", "object_id": "s1", "target_campaign_id": "c1"}),
            None,
        )
        .await
        .unwrap();
    let calls = non_get(&h).await;
    assert_eq!(calls[0].2["campaign_id"], "c1");
    let err = engine(&h)
        .call_tool(
            "duplicate",
            json!({"account_id": "act_111", "object_type": "campaign", "object_id": "c1", "target_adset_id": "s1"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
}
