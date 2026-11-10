//! Tests of the generic Meta write (`MetaPlan`), with tools built just for them via
//! `MetaAdapter::with_tools`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use adsconsole_core::config::{Config, ServerSettings, ToolGroup, WriteMode};
use adsconsole_core::db::{AccountInfo, ChangeStatus, Db};
use adsconsole_core::secrets::{Secret, SecretStore};
use adsconsole_core::{CoreError, Platform};
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::fields::ObjectKind;
use adsconsole_meta_ads::retry::RetryPolicy;
use serde_json::{Map, Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::connections::{ConnectionExtra, MetaConnKind, save_connection};
use super::ctx::Ctx;
use super::plan::{FieldChange, InverseSpec, MetaPlan, Owner, Step, Watch};
use super::tools::{MTool, WriteTool, parse_input};
use super::{MetaAdapter, tools};
use crate::adapter::ToolSpec;
use crate::engine::Engine;
use crate::error::{ToolError, codes};

#[derive(Default)]
struct Store(Mutex<BTreeMap<String, String>>);

impl SecretStore for Store {
    fn get(&self, key: &str) -> Result<Option<String>, CoreError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), CoreError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn params(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

/// Creates campaign → ad set → ad, with refs between them.
struct CreateThree;

#[async_trait::async_trait]
impl WriteTool for CreateThree {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<Value>("test_create3", "test", ToolGroup::Create, true)
    }
    async fn plan(&self, ctx: &Ctx, _input: &Value) -> Result<MetaPlan, ToolError> {
        let act = &ctx.account_id;
        let watch = |key: &str, kind| Watch {
            object_id: format!("$ref:{key}"),
            kind,
            fields: vec!["name", "status"],
        };
        Ok(MetaPlan {
            summary: "Create 3".into(),
            steps: vec![
                Step::Create {
                    key: "campaign".into(),
                    path: format!("{act}/campaigns"),
                    params: params(
                        json!({"name": "C", "status": "PAUSED", "objective": "OUTCOME_TRAFFIC"}),
                    ),
                    owner: Owner::Root,
                },
                Step::Create {
                    key: "adset".into(),
                    path: format!("{act}/adsets"),
                    params: params(
                        json!({"name": "S", "status": "PAUSED", "campaign_id": "$ref:campaign"}),
                    ),
                    owner: Owner::Under("campaign".into()),
                },
                Step::Create {
                    key: "ad".into(),
                    path: format!("{act}/ads"),
                    params: params(
                        json!({"name": "Ad X", "status": "PAUSED", "adset_id": "$ref:adset"}),
                    ),
                    owner: Owner::Under("adset".into()),
                },
            ],
            watch: vec![
                watch("campaign", ObjectKind::Campaign),
                watch("adset", ObjectKind::AdSet),
                watch("ad", ObjectKind::Ad),
            ],
            inverse: InverseSpec::DeleteCreated,
            ..MetaPlan::default()
        })
    }
}

/// Renames a campaign.
struct Rename;

#[async_trait::async_trait]
impl WriteTool for Rename {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<Value>("test_rename", "test", ToolGroup::Edit, true)
    }
    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        #[derive(serde::Deserialize)]
        struct In {
            campaign_id: String,
            name: String,
        }
        let i: In = parse_input(input)?;
        Ok(MetaPlan {
            summary: format!("Rename {}", i.campaign_id),
            steps: vec![Step::Update {
                id: i.campaign_id.clone(),
                params: params(json!({ "name": i.name })),
            }],
            field_changes: vec![FieldChange {
                object_id: i.campaign_id.clone(),
                kind: ObjectKind::Campaign,
                field: "name",
                value: json!(i.name),
            }],
            watch: vec![Watch {
                object_id: i.campaign_id,
                kind: ObjectKind::Campaign,
                fields: vec!["name"],
            }],
            inverse: InverseSpec::RestoreFields,
            ..MetaPlan::default()
        })
    }
}

struct Harness {
    server: MockServer,
    db: Arc<Db>,
    engine: Engine,
}

async fn harness(extra: Vec<MTool>) -> Harness {
    let server = MockServer::start().await;
    let db = Arc::new(Db::open_in_memory().unwrap());
    let store: Arc<dyn SecretStore> = Arc::new(Store::default());
    let mut all = tools::all();
    all.push(MTool::Write(Box::new(CreateThree)));
    all.push(MTool::Write(Box::new(Rename)));
    all.extend(extra);
    let adapter = MetaAdapter::with_tools(
        db.clone(),
        store,
        MetaEndpoints {
            graph_base: server.uri(),
            dialog_url: "https://x.test".into(),
        },
        RetryPolicy {
            base: Duration::from_millis(1),
        },
        all,
    );
    let extra = ConnectionExtra {
        kind: MetaConnKind::Login,
        app_id: None,
        config_id: None,
        user_id: None,
        relay_url: None,
    };
    save_connection(
        &adapter,
        "m_test",
        "Test",
        &Secret::new("T"),
        None,
        &extra,
        None,
    )
    .unwrap();
    db.replace_accounts(
        Platform::MetaAds,
        "m_test",
        &[AccountInfo {
            account_id: "act_111".into(),
            name: "Account".into(),
            currency: "BRL".into(),
            timezone: None,
            status: Some("ACTIVE".into()),
            manager_id: None,
        }],
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/act_111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "act_111", "account_status": 1})),
        )
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Direct,
            ..ServerSettings::default()
        },
    );
    let engine = Engine::new(adapter, db.clone(), Arc::new(RwLock::new(config))).unwrap();
    Harness { server, db, engine }
}

fn form(r: &Request) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(&r.body).into_owned().collect()
}

async fn requests(h: &Harness) -> Vec<Request> {
    h.server.received_requests().await.unwrap()
}

/// Requests other than the account_status GET.
async fn calls(h: &Harness) -> Vec<String> {
    requests(h)
        .await
        .iter()
        .filter(|r| r.url.path() != "/act_111")
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

async fn mock_creates(h: &Harness) {
    for (p, id) in [
        ("/act_111/campaigns", "c1"),
        ("/act_111/adsets", "s1"),
        ("/act_111/ads", "a1"),
    ] {
        Mock::given(method("POST"))
            .and(path(p))
            .and(|r: &Request| !form(r).contains_key("execution_options"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": id})))
            .mount(&h.server)
            .await;
    }
    Mock::given(method("POST"))
        .and(|r: &Request| form(r).contains_key("execution_options"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
}

async fn mock_reads(h: &Harness, status: &str) {
    for (p, name) in [("/c1", "C"), ("/s1", "S"), ("/a1", "Ad X")] {
        let body = json!({"id": &p[1..], "name": name, "status": status});
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&h.server)
            .await;
    }
}

async fn call(h: &Harness, tool: &str, input: Value) -> Result<Value, ToolError> {
    let mut input = input;
    input["account_id"] = json!("act_111");
    h.engine.call_tool(tool, input, Some("test".into())).await
}

#[tokio::test]
async fn validate_only_first_level() {
    let h = harness(vec![]).await;
    mock_creates(&h).await;
    mock_reads(&h, "PAUSED").await;
    let out = call(&h, "test_create3", json!({})).await.unwrap();
    assert_eq!(out["status"], "applied");
    let validations: Vec<Request> = requests(&h)
        .await
        .into_iter()
        .filter(|r| form(r).contains_key("execution_options"))
        .collect();
    assert_eq!(
        validations.len(),
        1,
        "only the campaign goes to validate_only"
    );
    assert_eq!(validations[0].url.path(), "/act_111/campaigns");
    assert_eq!(
        form(&validations[0])["execution_options"],
        "[\"validate_only\"]"
    );
    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    let _ = rec;
}

#[tokio::test]
async fn validate_warning_mentions_dependent_steps() {
    let h = harness(vec![]).await;
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Approval,
            approval_wait_secs: 0,
            ..ServerSettings::default()
        },
    );
    let engine = Engine::new(
        h.engine_adapter(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap();
    mock_creates(&h).await;
    let out = engine
        .call_tool("test_create3", json!({"account_id": "act_111"}), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "pending");
    let pending = engine.pending().unwrap();
    let warning = pending[0].warning.as_deref().unwrap();
    assert!(
        warning.contains("validates the next steps only at creation time"),
        "{warning}"
    );
}

impl Harness {
    fn engine_adapter(&self) -> Arc<dyn crate::PlatformAdapter> {
        self.engine.adapter_for_tests()
    }
}

#[tokio::test]
async fn execute_resolves_refs_and_records_progress() {
    let h = harness(vec![]).await;
    mock_creates(&h).await;
    mock_reads(&h, "PAUSED").await;
    let out = call(&h, "test_create3", json!({})).await.unwrap();
    let reqs = requests(&h).await;
    let adset = reqs
        .iter()
        .find(|r| r.url.path() == "/act_111/adsets" && !form(r).contains_key("execution_options"))
        .unwrap();
    assert_eq!(form(adset)["campaign_id"], "c1");
    let ad = reqs
        .iter()
        .find(|r| r.url.path() == "/act_111/ads")
        .unwrap();
    assert_eq!(form(ad)["adset_id"], "s1");

    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied);
    let ids: Vec<&str> = rec.progress.as_ref().unwrap()["created"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["c1", "s1", "a1"]);
    assert_eq!(
        rec.after.unwrap(),
        json!({
            "c1": {"name": "C", "status": "PAUSED"},
            "s1": {"name": "S", "status": "PAUSED"},
            "a1": {"name": "Ad X", "status": "PAUSED"}
        })
    );
    let inverse = rec.inverse.unwrap();
    assert_eq!(inverse.tool, "delete_objects");
    assert_eq!(inverse.input, json!({"ids": ["c1"]}));
}

#[tokio::test]
async fn rollback_on_mid_failure() {
    let h = harness(vec![]).await;
    Mock::given(method("POST"))
        .and(path("/act_111/ads"))
        .and(|r: &Request| !form(r).contains_key("execution_options"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"message": "Invalid creative", "code": 100}})),
        )
        .mount(&h.server)
        .await;
    mock_creates(&h).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
    let err = call(&h, "test_create3", json!({})).await.unwrap_err();
    let details = err.details.clone().unwrap();
    assert_eq!(details["failed_step"], "ad");
    assert_eq!(details["rolled_back"], json!(["s1", "c1"]));
    let deletes: Vec<String> = calls(&h)
        .await
        .into_iter()
        .filter(|c| c.starts_with("DELETE"))
        .collect();
    assert_eq!(deletes, ["DELETE /s1", "DELETE /c1"]);
    let rec =
        h.db.get_change(details["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(rec.status, ChangeStatus::Failed);
}

#[tokio::test]
async fn rollback_failure_reports_orphans() {
    let h = harness(vec![]).await;
    Mock::given(method("POST"))
        .and(path("/act_111/ads"))
        .and(|r: &Request| !form(r).contains_key("execution_options"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"message": "x", "code": 100}})),
        )
        .mount(&h.server)
        .await;
    mock_creates(&h).await;
    Mock::given(method("DELETE"))
        .and(path("/s1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
    let err = call(&h, "test_create3", json!({})).await.unwrap_err();
    assert_eq!(err.code, codes::PARTIAL_ROLLBACK);
    let details = err.details.unwrap();
    assert_eq!(details["orphaned_ids"], json!(["s1"]));
    assert!(details["cause"]["code"].is_string());
    assert!(
        err.suggestion
            .unwrap()
            .contains("Delete these objects in Ads Manager")
    );
}

#[tokio::test]
async fn maybe_applied_step_names_object() {
    let h = harness(vec![]).await;
    Mock::given(method("POST"))
        .and(path("/act_111/ads"))
        .and(|r: &Request| !form(r).contains_key("execution_options"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    mock_creates(&h).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
    let err = call(&h, "test_create3", json!({})).await.unwrap_err();
    assert_eq!(err.code, codes::EXECUTION_FAILED);
    assert!(err.message.contains("Ad X"), "{}", err.message);
    assert_eq!(err.details.unwrap()["rolled_back"], json!(["s1", "c1"]));
}

#[tokio::test]
async fn undo_create_with_conflict() {
    let h = harness(vec![]).await;
    mock_creates(&h).await;
    mock_reads(&h, "PAUSED").await;
    let out = call(&h, "test_create3", json!({})).await.unwrap();
    let change_id = out["change_id"].as_str().unwrap().to_string();

    // Someone activated the campaign outside AdsConsole.
    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/act_111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "act_111", "account_status": 1})),
        )
        .mount(&h.server)
        .await;
    mock_reads(&h, "ACTIVE").await;
    Mock::given(method("DELETE"))
        .and(path("/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
    let err = h
        .engine
        .call_tool("undo_change", json!({"change_id": change_id}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::UNDO_CONFLICT);
    let out = h
        .engine
        .call_tool(
            "undo_change",
            json!({"change_id": change_id, "force": true}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(out["reversible"], false);
    assert!(calls(&h).await.contains(&"DELETE /c1".to_string()));
}

#[tokio::test]
async fn restore_fields_round_trip() {
    let h = harness(vec![]).await;
    let name = Arc::new(Mutex::new("Old".to_string()));
    let read_name = name.clone();
    Mock::given(method("GET"))
        .and(path("/c9"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "c9", "name": read_name.lock().unwrap().clone()}))
        })
        .mount(&h.server)
        .await;
    let write_name = name.clone();
    Mock::given(method("POST"))
        .and(path("/c9"))
        .respond_with(move |r: &Request| {
            let f = form(r);
            if !f.contains_key("execution_options") {
                *write_name.lock().unwrap() = f["name"].clone();
            }
            ResponseTemplate::new(200).set_body_json(json!({"success": true}))
        })
        .mount(&h.server)
        .await;
    let out = call(
        &h,
        "test_rename",
        json!({"campaign_id": "c9", "name": "New"}),
    )
    .await
    .unwrap();
    assert_eq!(*name.lock().unwrap(), "New");
    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(rec.before.unwrap(), json!({"c9": {"name": "Old"}}));
    assert_eq!(rec.after.unwrap(), json!({"c9": {"name": "New"}}));

    let undo = h
        .engine
        .call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap();
    assert_eq!(*name.lock().unwrap(), "Old");
    assert_eq!(undo["reversible"], true);
    h.engine
        .call_tool("undo_change", json!({"change_id": undo["change_id"]}), None)
        .await
        .unwrap();
    assert_eq!(*name.lock().unwrap(), "New");
}

#[tokio::test]
async fn escape_disabled_by_default() {
    let h = harness(vec![]).await;
    let listed: Vec<String> = h
        .engine
        .registry()
        .enabled(&ServerSettings::default())
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert!(!listed.contains(&"graph_get".to_string()));
    assert!(!listed.contains(&"graph_post".to_string()));
    let post = h.engine.registry().get("graph_post").unwrap();
    assert!(!post.reversible);
    assert!(post.destructive);
    assert_eq!(post.group, ToolGroup::Escape);
    assert!(h.engine.registry().get("graph_get").is_some());
    let hidden = h.engine.registry().get("delete_objects").unwrap();
    assert!(!hidden.exposed);
    assert!(!h.engine.registry().get("restore_fields").unwrap().exposed);
}

#[tokio::test]
async fn graph_get_refuses_other_account() {
    let h = harness(vec![]).await;
    let mut config = Config::default();
    let mut settings = ServerSettings::default();
    settings.groups.insert(ToolGroup::Escape, true);
    settings.write_mode = WriteMode::Direct;
    config.servers.insert(Platform::MetaAds, settings);
    let engine = Engine::new(
        h.engine_adapter(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap();
    let err = engine
        .call_tool(
            "graph_get",
            json!({"account_id": "act_111", "path": "act_999/campaigns"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    let err = engine
        .call_tool(
            "graph_get",
            json!({"account_id": "act_111", "path": "me/adaccounts"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    Mock::given(method("GET"))
        .and(path_regex("^/act_111/campaigns$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "1", "access_token": "x"}]})),
        )
        .mount(&h.server)
        .await;
    let out = engine
        .call_tool(
            "graph_get",
            json!({"account_id": "act_111", "path": "act_111/campaigns", "params": {"fields": "id"}}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["data"][0]["id"], "1");
    assert!(!out.to_string().contains("access_token"));
    let err = engine
        .call_tool(
            "graph_post",
            json!({"account_id": "act_111", "path": "123/copies", "params": {}}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    Mock::given(method("POST"))
        .and(path("/act_111/campaigns"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "c7"})))
        .mount(&h.server)
        .await;
    let out = engine
        .call_tool(
            "graph_post",
            json!({"account_id": "act_111", "path": "act_111/campaigns", "params": {"name": "x"}}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(out["reversible"], false);
    assert_eq!(out["result"]["id"], "c7");
}

#[tokio::test]
async fn graph_post_output_scrubbed() {
    let h = harness(vec![]).await;
    let mut config = Config::default();
    let mut settings = ServerSettings::default();
    settings.groups.insert(ToolGroup::Escape, true);
    settings.write_mode = WriteMode::Direct;
    config.servers.insert(Platform::MetaAds, settings);
    let engine = Engine::new(
        h.engine_adapter(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap();
    Mock::given(method("POST"))
        .and(path("/act_111/campaigns"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c7",
            "access_token": "EAAB",
            "link": "https://graph.facebook.com/v26.0/c7?access_token=EAAB&fields=id"
        })))
        .mount(&h.server)
        .await;
    let out = engine
        .call_tool(
            "graph_post",
            json!({"account_id": "act_111", "path": "act_111/campaigns", "params": {"name": "x"}}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["result"]["id"], "c7");
    assert!(!out.to_string().contains("EAAB"), "{out}");
    assert_eq!(
        out["result"]["link"],
        "https://graph.facebook.com/v26.0/c7?fields=id"
    );
}
