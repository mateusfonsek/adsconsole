mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use chrono::{Duration, Utc};
use common::meta::*;
use serde_json::{Map, Value, json};
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

/// Objeto "vivo" no wiremock: GET devolve o estado; POST sem validate_only aplica os campos.
async fn live_object(h: &MetaHarness, id: &str, initial: Value) -> Arc<Mutex<Map<String, Value>>> {
    let state = Arc::new(Mutex::new(initial.as_object().unwrap().clone()));
    let read = state.clone();
    Mock::given(method("GET"))
        .and(path(format!("/{id}")))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(Value::Object(read.lock().unwrap().clone()))
        })
        .mount(&h.server)
        .await;
    let write = state.clone();
    Mock::given(method("POST"))
        .and(path(format!("/{id}")))
        .respond_with(move |r: &Request| {
            let f = form(r);
            if !f.contains_key("execution_options") {
                let mut s = write.lock().unwrap();
                for (k, v) in f {
                    if k == "creative" {
                        let c: Value = serde_json::from_str(&v).unwrap();
                        s.insert("creative".into(), json!({"id": c["creative_id"]}));
                    } else if k == "targeting" {
                        s.insert(k, serde_json::from_str(&v).unwrap());
                    } else {
                        s.insert(k, json!(v));
                    }
                }
            }
            ResponseTemplate::new(200).set_body_json(json!({"success": true}))
        })
        .mount(&h.server)
        .await;
    state
}

async fn active_account(h: &MetaHarness, id: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/{id}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": id, "account_status": 1})),
        )
        .mount(&h.server)
        .await;
}

async fn posts(h: &MetaHarness) -> Vec<BTreeMap<String, String>> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && !form(r).contains_key("execution_options"))
        .map(form)
        .collect()
}

#[tokio::test]
async fn update_campaign_fields() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    let c = live_object(&h, "c1", json!({"id": "c1", "account_id": "111", "name": "Velha", "status": "PAUSED", "daily_budget": "5000"})).await;
    let e = engine(&h);
    let out = e
        .call_tool(
            "update_campaign",
            json!({"account_id": "act_111", "campaign_id": "c1", "name": "Nova", "status": "ACTIVE"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(
        out["result"],
        json!({"c1": {"name": "Nova", "status": "ACTIVE"}})
    );
    let p = posts(&h).await;
    assert_eq!(p.len(), 1, "um POST só, com os dois campos");
    assert_eq!(p[0]["name"], "Nova");
    assert_eq!(p[0]["status"], "ACTIVE");
    e.call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap();
    let s = c.lock().unwrap();
    assert_eq!(s["name"], "Velha");
    assert_eq!(s["status"], "PAUSED");
}

#[tokio::test]
async fn needs_at_least_one_field() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "c1",
        json!({"id": "c1", "account_id": "111", "name": "x"}),
    )
    .await;
    let err = engine(&h)
        .call_tool(
            "update_campaign",
            json!({"account_id": "act_111", "campaign_id": "c1"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    let err = engine(&h)
        .call_tool(
            "update_campaign",
            json!({"account_id": "act_111", "campaign_id": "c1", "status": "DELETED"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("status"));
}

#[tokio::test]
async fn update_adset_budget_minor_units() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "s1",
        json!({"id": "s1", "account_id": "111", "daily_budget": "1000"}),
    )
    .await;
    let e = engine(&h);
    e.call_tool(
        "update_adset",
        json!({"account_id": "act_111", "adset_id": "s1", "daily_budget": 25.4}),
        None,
    )
    .await
    .unwrap();
    assert_eq!(posts(&h).await[0]["daily_budget"], "2540");
    let err = e
        .call_tool(
            "update_adset",
            json!({"account_id": "act_111", "adset_id": "s1", "daily_budget": 25.405}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("daily_budget"));
}

#[tokio::test]
async fn jpy_budget() {
    let h = meta_harness().await;
    seed_meta_connection(
        &h,
        "m_jp",
        "TJ",
        None,
        &[meta_account("act_7", "JPY", "ACTIVE")],
    );
    active_account(&h, "act_7").await;
    live_object(
        &h,
        "s7",
        json!({"id": "s7", "account_id": "7", "daily_budget": "500"}),
    )
    .await;
    let e = engine(&h);
    e.call_tool(
        "update_adset",
        json!({"account_id": "act_7", "adset_id": "s7", "daily_budget": 1000}),
        None,
    )
    .await
    .unwrap();
    assert_eq!(posts(&h).await[0]["daily_budget"], "1000");
    let err = e
        .call_tool(
            "update_adset",
            json!({"account_id": "act_7", "adset_id": "s7", "daily_budget": 10.5}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn fifth_budget_change_refused() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "s1",
        json!({"id": "s1", "account_id": "111", "daily_budget": "1000"}),
    )
    .await;
    let now = Utc::now();
    for m in [50, 40, 30, 20] {
        h.db.record_budget_change("s1", now - Duration::minutes(m))
            .unwrap();
    }
    let err = engine(&h)
        .call_tool(
            "update_adset",
            json!({"account_id": "act_111", "adset_id": "s1", "daily_budget": 30}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::BUDGET_CHANGE_LIMIT);
    assert_eq!(err.field.as_deref(), Some("daily_budget"));
    let at = (now - Duration::minutes(50) + Duration::hours(1))
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string();
    assert!(err.message.contains(&at), "{} não cita {at}", err.message);
    assert!(posts(&h).await.is_empty());
    let all = h.server.received_requests().await.unwrap();
    assert!(
        all.iter().all(|r| r.method.as_str() == "GET"),
        "nenhum POST, nem validate_only"
    );
    // Mudar só o nome continua permitido.
    engine(&h)
        .call_tool(
            "update_adset",
            json!({"account_id": "act_111", "adset_id": "s1", "name": "Novo"}),
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn undo_counts_budget_change() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "s1",
        json!({"id": "s1", "account_id": "111", "daily_budget": "1000"}),
    )
    .await;
    let now = Utc::now();
    for m in [30, 20, 10] {
        h.db.record_budget_change("s1", now - Duration::minutes(m))
            .unwrap();
    }
    let e = engine(&h);
    let out = e
        .call_tool(
            "update_adset",
            json!({"account_id": "act_111", "adset_id": "s1", "daily_budget": 30}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        h.db.budget_changes_since("s1", now - Duration::hours(1))
            .unwrap()
            .len(),
        4
    );
    let err = e
        .call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::BUDGET_CHANGE_LIMIT);
}

#[tokio::test]
async fn budget_type_switch_refused() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "s1",
        json!({"id": "s1", "account_id": "111", "lifetime_budget": "100000"}),
    )
    .await;
    let e = engine(&h);
    let err = e
        .call_tool(
            "update_adset",
            json!({"account_id": "act_111", "adset_id": "s1", "daily_budget": 30}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("daily_budget"));
    let err = e
        .call_tool(
            "update_adset",
            json!({"account_id": "act_111", "adset_id": "s1", "daily_budget": 30, "lifetime_budget": 900}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn campaign_budget_without_cbo_refused() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "c1",
        json!({"id": "c1", "account_id": "111", "name": "x", "daily_budget": "0"}),
    )
    .await;
    let err = engine(&h)
        .call_tool(
            "update_campaign",
            json!({"account_id": "act_111", "campaign_id": "c1", "daily_budget": 50}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(err.message.contains("ad sets"), "{}", err.message);
}

#[tokio::test]
async fn targeting_replaced_whole() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    let old =
        json!({"geo_locations": {"countries": ["BR"]}, "age_min": 18, "interests": [{"id": "1"}]});
    let new = json!({"geo_locations": {"countries": ["PT"]}});
    let s = live_object(
        &h,
        "s1",
        json!({"id": "s1", "account_id": "111", "targeting": old}),
    )
    .await;
    let e = engine(&h);
    let out = e
        .call_tool(
            "update_adset",
            json!({"account_id": "act_111", "adset_id": "s1", "targeting": new}),
            None,
        )
        .await
        .unwrap();
    assert!(
        out["summary"].as_str().unwrap().contains("substitui"),
        "{}",
        out["summary"]
    );
    e.call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap();
    let p = posts(&h).await;
    let restored: Value = serde_json::from_str(&p[1]["targeting"]).unwrap();
    assert_eq!(restored, old);
    assert_eq!(s.lock().unwrap()["targeting"], old);
}

#[tokio::test]
async fn update_ad_swaps_creative() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    let a = live_object(
        &h,
        "a1",
        json!({"id": "a1", "account_id": "111", "creative": {"id": "8"}}),
    )
    .await;
    let e = engine(&h);
    let out = e
        .call_tool(
            "update_ad",
            json!({"account_id": "act_111", "ad_id": "a1", "creative_id": "9"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["result"], json!({"a1": {"creative": "9"}}));
    let sent: Value = serde_json::from_str(&posts(&h).await[0]["creative"]).unwrap();
    assert_eq!(sent, json!({"creative_id": "9"}));
    e.call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap();
    assert_eq!(a.lock().unwrap()["creative"], json!({"id": "8"}));
}

#[tokio::test]
async fn archive_warns() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "a1",
        json!({"id": "a1", "account_id": "111", "status": "PAUSED"}),
    )
    .await;
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
    e.call_tool(
        "update_ad",
        json!({"account_id": "act_111", "ad_id": "a1", "status": "ARCHIVED"}),
        None,
    )
    .await
    .unwrap();
    let w = e.pending().unwrap()[0].warning.clone().unwrap();
    assert!(w.contains("arquivados"), "{w}");
}

#[tokio::test]
async fn other_account_object_not_found() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "c9",
        json!({"id": "c9", "account_id": "999", "name": "x"}),
    )
    .await;
    let err = engine(&h)
        .call_tool(
            "update_campaign",
            json!({"account_id": "act_111", "campaign_id": "c9", "name": "y"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
    assert!(posts(&h).await.is_empty());
}

#[tokio::test]
async fn approval_mode_rechecks_budget_limit() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    live_object(
        &h,
        "s1",
        json!({"id": "s1", "account_id": "111", "daily_budget": "1000"}),
    )
    .await;
    let now = Utc::now();
    for m in [30, 20, 10] {
        h.db.record_budget_change("s1", now - Duration::minutes(m))
            .unwrap();
    }
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
    // As duas passam na validação (3 registradas) e vão para a fila.
    let mut ids = Vec::new();
    for budget in [30, 40] {
        let out = e
            .call_tool(
                "update_adset",
                json!({"account_id": "act_111", "adset_id": "s1", "daily_budget": budget}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "pending", "{out}");
        ids.push(out["change_id"].as_str().unwrap().to_string());
    }
    let first = e.approve(&ids[0]).await.unwrap();
    assert_eq!(first.status, adsconsole_core::db::ChangeStatus::Applied);
    let second = e.approve(&ids[1]).await.unwrap();
    assert_eq!(second.status, adsconsole_core::db::ChangeStatus::Failed);
    assert_eq!(second.error.unwrap()["code"], codes::BUDGET_CHANGE_LIMIT);
    assert_eq!(posts(&h).await.len(), 1, "a 2ª não chega à API");
}

#[tokio::test]
async fn direct_write_carries_warnings() {
    let h = meta_harness().await;
    seed_meta_connection(
        &h,
        "m_soon",
        "T5",
        Some(Utc::now() + Duration::days(5)),
        &[meta_account("act_5", "BRL", "ACTIVE")],
    );
    active_account(&h, "act_5").await;
    live_object(
        &h,
        "c5",
        json!({"id": "c5", "account_id": "5", "name": "Velha", "status": "PAUSED"}),
    )
    .await;
    let out = engine(&h)
        .call_tool(
            "update_campaign",
            json!({"account_id": "act_5", "campaign_id": "c5", "status": "ARCHIVED"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    let warning = out["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("5 dias"), "sem o aviso do token: {out}");
    assert!(
        warning.contains("rquiv"),
        "sem o aviso do arquivamento: {out}"
    );
}

#[tokio::test]
async fn filling_empty_field_is_not_undoable() {
    let h = meta_harness().await;
    active_account(&h, "act_111").await;
    let c = live_object(
        &h,
        "c1",
        json!({"id": "c1", "account_id": "111", "name": "Velha"}),
    )
    .await;
    let e = engine(&h);
    let out = e
        .call_tool(
            "update_campaign",
            json!({"account_id": "act_111", "campaign_id": "c1", "name": "Nova", "spend_cap": 500}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(out["reversible"], false, "{out}");
    let warning = out["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("spend_cap"), "{out}");
    let err = e
        .call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_REVERSIBLE);

    // Mudança antiga, gravada com o desfazer: o desfazer recusa em vez de fingir.
    let id = out["change_id"].as_str().unwrap();
    let mut rec = h.db.get_change(id).unwrap().unwrap();
    rec.reversible = true;
    rec.inverse = Some(adsconsole_core::db::InverseOp {
        tool: "restore_fields".into(),
        input: json!({"updates": {"c1": {"name": "Velha", "spend_cap": null}}, "kinds": {"c1": "campaign"}}),
        target: json!({"watch": [{"object_id": "c1", "kind": "campaign", "fields": ["name", "spend_cap"]}]}),
    });
    h.db.update_change(&rec).unwrap();
    let err = e
        .call_tool("undo_change", json!({"change_id": id, "force": true}), None)
        .await
        .unwrap_err();
    assert!(err.message.contains("spend_cap"), "{}", err.message);
    assert_eq!(c.lock().unwrap()["name"], "Nova", "nada foi aplicado");
    let rec = h.db.get_change(id).unwrap().unwrap();
    assert_eq!(rec.status, adsconsole_core::db::ChangeStatus::Applied);
}
