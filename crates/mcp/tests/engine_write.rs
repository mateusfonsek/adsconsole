use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::{AccountInfo, ChangeFilter, ChangeStatus, Db};
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use adsconsole_mcp::fake::FakePlatform;
use adsconsole_mcp::{Applied, Call, PlatformAdapter, ToolError, ToolSpec, Validated};
use serde_json::{Value, json};

async fn setup(mode: WriteMode) -> (Engine, Arc<FakePlatform>, Arc<Db>) {
    let fake = FakePlatform::new(Platform::GoogleAds);
    let (engine, db) = setup_with(mode, fake.clone(), &fake).await;
    (engine, fake, db)
}

async fn setup_with(
    mode: WriteMode,
    adapter: Arc<dyn PlatformAdapter>,
    fake: &FakePlatform,
) -> (Engine, Arc<Db>) {
    let db = Arc::new(Db::open_in_memory().unwrap());
    fake.seed_connections(&db).unwrap();
    let mut config = Config::default();
    config.servers.insert(
        Platform::GoogleAds,
        ServerSettings {
            write_mode: mode,
            ..ServerSettings::default()
        },
    );
    let engine = Engine::new(adapter, db.clone(), Arc::new(RwLock::new(config))).unwrap();
    engine.refresh_accounts().await.unwrap();
    (engine, db)
}

/// Fake that yields between the snapshot and the execute, so that concurrent writes
/// without the account mutex really interleave.
struct YieldingFake(Arc<FakePlatform>, std::time::Duration);

#[async_trait::async_trait]
impl PlatformAdapter for YieldingFake {
    fn platform(&self) -> Platform {
        self.0.platform()
    }
    fn tools(&self) -> Vec<ToolSpec> {
        self.0.tools()
    }
    async fn list_accounts(&self, connection_id: &str) -> Result<Vec<AccountInfo>, ToolError> {
        self.0.list_accounts(connection_id).await
    }
    async fn read(&self, call: &Call) -> Result<Value, ToolError> {
        self.0.read(call).await
    }
    async fn precheck(&self, call: &Call) -> Result<(), ToolError> {
        self.0.precheck(call).await
    }
    async fn validate(&self, call: &Call) -> Result<Validated, ToolError> {
        self.0.validate(call).await
    }
    async fn snapshot(&self, call: &Call) -> Result<Value, ToolError> {
        let before = self.0.snapshot(call).await;
        tokio::time::sleep(self.1).await;
        before
    }
    async fn execute(&self, call: &Call, before: &Value) -> Result<Applied, ToolError> {
        self.0.execute(call, before).await
    }
    async fn current_state(
        &self,
        connection_id: &str,
        account_id: &str,
        target: &Value,
    ) -> Result<Value, ToolError> {
        self.0
            .current_state(connection_id, account_id, target)
            .await
    }
    fn connections(&self) -> Vec<String> {
        self.0.connections()
    }
}

/// Entries of the fake's log after the first `skip` (the setup ones).
fn log_since(fake: &FakePlatform, skip: usize) -> Vec<String> {
    fake.log().into_iter().skip(skip).collect()
}

fn acct(id: &str) -> AccountInfo {
    AccountInfo {
        account_id: id.into(),
        name: format!("Account {id}"),
        currency: "BRL".into(),
        timezone: None,
        status: Some("ACTIVE".into()),
        manager_id: None,
    }
}

fn by() -> Option<String> {
    Some("teste".into())
}

fn all_changes(db: &Db) -> Vec<adsconsole_core::db::ChangeRecord> {
    db.list_changes(&ChangeFilter {
        limit: 100,
        ..ChangeFilter::default()
    })
    .unwrap()
}

#[tokio::test]
async fn read_routes_to_connection() {
    let (engine, _fake, db) = setup(WriteMode::Direct).await;
    let err = engine
        .call_tool("get_campaigns", json!({"account_id": "333"}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::AMBIGUOUS_ACCOUNT);

    db.set_default_connection(Platform::GoogleAds, "conn_b")
        .unwrap();
    let out = engine
        .call_tool("get_campaigns", json!({"account_id": "333"}), by())
        .await
        .unwrap();
    assert!(out["campaigns"].is_array());
}

#[tokio::test]
async fn read_only_mode_refuses_without_calling_adapter() {
    let (engine, fake, db) = setup(WriteMode::ReadOnly).await;
    let setup_log = fake.log().len();
    let err = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::READ_ONLY_MODE);
    assert_eq!(
        err.message,
        "The write mode of Google Ads is read-only. Change it in AdsConsole → Tools."
    );
    assert!(
        log_since(&fake, setup_log).is_empty(),
        "log: {:?}",
        fake.log()
    );
    assert!(all_changes(&db).is_empty());
}

#[tokio::test]
async fn read_only_mode_refuses_uncached_account_without_refresh() {
    let (engine, fake, db) = setup(WriteMode::ReadOnly).await;
    fake.add_account_to("conn_a", acct("555"));
    let setup_log = fake.log().len();
    for account in ["555", "999", "333"] {
        let err = engine
            .call_tool(
                "create_campaign",
                json!({"account_id": account, "name": "x", "daily_budget": 30}),
                by(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::READ_ONLY_MODE, "account {account}");
    }
    assert!(
        log_since(&fake, setup_log).is_empty(),
        "log: {:?}",
        fake.log()
    );
    assert!(all_changes(&db).is_empty());
}

#[tokio::test]
async fn direct_write_records_history() {
    let (engine, _fake, db) = setup(WriteMode::Direct).await;
    let out = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(out["reversible"], true);
    assert!(out.get("warning").is_none());
    assert_eq!(out["result"]["status"], "PAUSED");
    assert!(out["summary"].as_str().is_some_and(|s| !s.is_empty()));
    let id = out["change_id"].as_str().unwrap();
    assert!(id.starts_with("chg_"));

    let rec = db.get_change(id).unwrap().unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied);
    assert!(rec.reversible);
    assert_eq!(rec.requested_by.as_deref(), Some("teste"));
    assert_eq!(rec.inverse.as_ref().unwrap().tool, "remove_campaign");
    assert_eq!(rec.connection_id, "conn_a");
    assert_eq!(rec.summary, out["summary"].as_str().unwrap());
    assert_eq!(rec.after.as_ref(), Some(&out["result"]));
}

#[tokio::test]
async fn irreversible_write_warns() {
    let (engine, _fake, _db) = setup(WriteMode::Direct).await;
    let created = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    let cid = created["result"]["campaign_id"].as_str().unwrap();
    let out = engine
        .call_tool(
            "remove_campaign",
            json!({"account_id": "111", "campaign_id": cid}),
            by(),
        )
        .await
        .unwrap();
    assert_eq!(out["reversible"], false);
    assert_eq!(out["warning"], "This change cannot be undone.");
}

#[tokio::test]
async fn validation_error_records_nothing() {
    let (engine, _fake, db) = setup(WriteMode::Direct).await;
    let err = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 10}),
            by(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::BUDGET_TOO_LOW);
    assert!(all_changes(&db).is_empty());
}

#[tokio::test]
async fn closed_account_refused() {
    let (engine, _fake, db) = setup(WriteMode::Direct).await;
    let err = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "222", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCOUNT_NOT_ACTIVE);
    assert!(all_changes(&db).is_empty());
}

#[tokio::test]
async fn execution_failure_recorded_as_failed() {
    let (engine, fake, db) = setup(WriteMode::Direct).await;
    fake.fail_next_execute(ToolError::new(codes::EXECUTION_FAILED, "API caiu"));
    let err = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::EXECUTION_FAILED);
    assert_eq!(err.message, "API caiu");
    let id = err.details.as_ref().unwrap()["change_id"].as_str().unwrap();
    let rec = db.get_change(id).unwrap().unwrap();
    assert_eq!(rec.status, ChangeStatus::Failed);
    let stored = rec.error.expect("error filled in");
    assert_eq!(stored["code"], codes::EXECUTION_FAILED);
    assert_eq!(stored["message"], "API caiu");
    assert!(rec.after.is_none());
    assert!(!rec.reversible, "a change that failed is not reversible");
}

#[tokio::test]
async fn disabled_and_hidden_tools() {
    let (engine, fake, _db) = setup(WriteMode::Direct).await;
    let setup_log = fake.log().len();
    let err = engine
        .call_tool("raw_mutate", json!({"account_id": "111"}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::TOOL_DISABLED);
    assert_eq!(
        err.suggestion.as_deref(),
        Some("Turn it on in AdsConsole → Tools")
    );
    let err = engine
        .call_tool("does_not_exist", json!({}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::UNKNOWN_TOOL);
    assert!(log_since(&fake, setup_log).is_empty());
}

#[tokio::test]
async fn invalid_account_ref_is_invalid_input() {
    let (engine, _fake, _db) = setup(WriteMode::Direct).await;
    let err = engine
        .call_tool("get_campaigns", json!({}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("account_id"));
}

#[tokio::test]
async fn cache_refresh_on_unknown_account() {
    let (engine, fake, _db) = setup(WriteMode::Direct).await;
    fake.add_account_to("conn_a", acct("555"));
    let out = engine
        .call_tool("get_campaigns", json!({"account_id": "555"}), by())
        .await
        .unwrap();
    assert!(out["campaigns"].is_array());

    let err = engine
        .call_tool("get_campaigns", json!({"account_id": "999"}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCOUNT_NOT_FOUND);
}

#[tokio::test]
async fn same_account_writes_are_serialized() {
    let fake = FakePlatform::new(Platform::GoogleAds);
    let adapter = Arc::new(YieldingFake(
        fake.clone(),
        std::time::Duration::from_millis(5),
    ));
    let (engine, db) = setup_with(WriteMode::Direct, adapter, &fake).await;
    let created = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "initial", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    let cid = created["result"]["campaign_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut set = tokio::task::JoinSet::new();
    for i in 0..10 {
        let engine = engine.clone();
        let cid = cid.clone();
        set.spawn(async move {
            engine
                .call_tool(
                    "update_campaign",
                    json!({"account_id": "111", "campaign_id": cid, "name": format!("n{i}")}),
                    Some("teste".into()),
                )
                .await
        });
    }
    let mut ids = Vec::new();
    while let Some(r) = set.join_next().await {
        let out = r.unwrap().unwrap();
        assert_eq!(out["status"], "applied");
        ids.push(out["change_id"].as_str().unwrap().to_string());
    }

    // (before.name → after.name) must form ONE chain starting from "initial".
    let mut next: HashMap<String, String> = HashMap::new();
    for id in &ids {
        let rec = db.get_change(id).unwrap().unwrap();
        assert_eq!(rec.status, ChangeStatus::Applied);
        let before = rec.before.unwrap()["name"].as_str().unwrap().to_string();
        let after = rec.after.unwrap()["name"].as_str().unwrap().to_string();
        assert!(
            next.insert(before.clone(), after).is_none(),
            "two updates started from the same state {before}"
        );
    }
    let mut cur = "initial".to_string();
    let mut steps = 0;
    while let Some(n) = next.get(&cur) {
        cur = n.clone();
        steps += 1;
    }
    assert_eq!(
        steps, 10,
        "the chain does not cover all the updates: {next:?}"
    );
}

#[tokio::test]
async fn google_dashed_id_normalized() {
    let (engine, _fake, db) = setup(WriteMode::Direct).await;
    engine
        .call_tool("get_campaigns", json!({"account_id": "1-11"}), by())
        .await
        .unwrap();
    let out = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "1-11", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    let rec = db
        .get_change(out["change_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.account_id, "111");
    assert_eq!(rec.input["account_id"], "111");
}

#[tokio::test]
async fn subscribe_sees_change_applied() {
    let (engine, _fake, _db) = setup(WriteMode::Direct).await;
    let mut rx = engine.subscribe();
    let out = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    match rx.try_recv().unwrap() {
        adsconsole_mcp::engine::EngineEvent::ChangeApplied { change_id } => {
            assert_eq!(Value::String(change_id), out["change_id"])
        }
        other => panic!("evento inesperado: {other:?}"),
    }
    assert_eq!(engine.platform(), Platform::GoogleAds);
    assert!(engine.registry().get("get_campaigns").is_some());
}

#[tokio::test]
async fn failing_connection_does_not_break_routing() {
    let (engine, fake, _db) = setup(WriteMode::Direct).await;
    fake.fail_list_accounts("conn_b");
    fake.add_account_to("conn_a", acct("555"));
    // The refresh skips conn_b and finds 555 in conn_a.
    let out = engine
        .call_tool("get_campaigns", json!({"account_id": "555"}), by())
        .await
        .unwrap();
    assert!(out["campaigns"].is_array());
    assert!(engine.refresh_accounts().await.is_ok());
    // Account 444 stays in conn_b's old cache.
    engine
        .call_tool("get_campaigns", json!({"account_id": "444"}), by())
        .await
        .unwrap();
}

#[tokio::test]
async fn all_connections_failing_reports_routing_error() {
    let (engine, fake, _db) = setup(WriteMode::Direct).await;
    fake.fail_list_accounts("conn_a");
    fake.fail_list_accounts("conn_b");
    let err = engine.refresh_accounts().await.unwrap_err();
    assert_eq!(err.code, codes::CONNECTION_NEEDS_RECONNECT);
    let err = engine
        .call_tool("get_campaigns", json!({"account_id": "999"}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCOUNT_NOT_FOUND);
}

#[tokio::test]
async fn dropped_call_still_finishes_the_change() {
    let fake = FakePlatform::new(Platform::GoogleAds);
    let adapter = Arc::new(YieldingFake(
        fake.clone(),
        std::time::Duration::from_millis(100),
    ));
    let (engine, db) = setup_with(WriteMode::Direct, adapter, &fake).await;
    // The client "disconnects" in the middle of apply (during the slow snapshot).
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(20),
        engine.call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        ),
    )
    .await;
    assert!(dropped.is_err(), "the call should have been cut off");
    let id = all_changes(&db)[0].change_id.clone();
    let mut status = None;
    for _ in 0..50 {
        status = Some(db.get_change(&id).unwrap().unwrap().status);
        if status == Some(ChangeStatus::Applied) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(status, Some(ChangeStatus::Applied));
}

#[tokio::test]
async fn access_denied_triggers_background_refresh() {
    let (engine, fake, _db) = setup(WriteMode::Direct).await;
    fake.fail_next_read(ToolError::new(codes::ACCESS_DENIED, "access denied"));
    let err = engine
        .call_tool("get_campaigns", json!({ "account_id": "111" }), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCESS_DENIED);
    let mut found = false;
    for _ in 0..20 {
        if fake.log().contains(&"list_accounts:conn_a".to_string()) {
            // the setup already did a refresh; count occurrences
            if fake
                .log()
                .iter()
                .filter(|l| *l == "list_accounts:conn_a")
                .count()
                >= 2
            {
                found = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(found, "background refresh did not happen: {:?}", fake.log());
}

#[tokio::test]
async fn access_denied_refresh_is_coalesced() {
    let (engine, fake, _db) = setup(WriteMode::Direct).await;
    let rounds = || {
        fake.log()
            .iter()
            .filter(|l| *l == "list_accounts:conn_a")
            .count()
    };
    let before = rounds();
    for _ in 0..2 {
        fake.fail_next_read(ToolError::new(codes::ACCESS_DENIED, "access denied"));
        let err = engine
            .call_tool("get_campaigns", json!({ "account_id": "111" }), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ACCESS_DENIED);
    }
    for _ in 0..20 {
        if rounds() > before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // Margin for a second (undue) refresh to show up.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(rounds(), before + 1, "{:?}", fake.log());
}

#[tokio::test]
async fn progress_recorded_per_step() {
    let (engine, fake, db) = setup(WriteMode::Direct).await;
    fake.execute_in_steps(vec!["c1", "s1", "a1"], None);
    let out = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    let rec = db
        .get_change(out["change_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let ids: Vec<&str> = rec.progress.as_ref().unwrap()["created"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["c1", "s1", "a1"]);
}

#[tokio::test]
async fn failed_steps_keep_progress() {
    let (engine, fake, db) = setup(WriteMode::Direct).await;
    fake.execute_in_steps(vec!["c1", "s1", "a1"], Some(2));
    let err = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap_err();
    let rec = db
        .get_change(err.details.as_ref().unwrap()["change_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.status, ChangeStatus::Failed);
    assert_eq!(
        rec.progress.unwrap()["created"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn interrupted_change_keeps_created_ids() {
    let fake = FakePlatform::new(Platform::GoogleAds);
    let db = Arc::new(Db::open_in_memory().unwrap());
    fake.seed_connections(&db).unwrap();
    let now = chrono::Utc::now();
    let change = adsconsole_core::db::ChangeRecord {
        change_id: adsconsole_core::db::new_change_id(),
        platform: Platform::GoogleAds,
        account_id: "111".into(),
        connection_id: "conn_a".into(),
        tool: "create_campaign".into(),
        input: json!({"account_id": "111", "name": "x", "daily_budget": 30}),
        summary: "Create".into(),
        before: None,
        after: None,
        inverse: None,
        reversible: true,
        status: ChangeStatus::Approved,
        requested_by: None,
        undo_of: None,
        undo_force: false,
        undone_by: None,
        error: None,
        progress: Some(json!({"created": [{"id": "c1"}, {"id": "s1"}]})),
        created_at: now,
        updated_at: now,
    };
    db.insert_change(&change).unwrap();
    Engine::new(fake, db.clone(), Arc::new(RwLock::new(Config::default()))).unwrap();
    let rec = db.get_change(&change.change_id).unwrap().unwrap();
    assert_eq!(rec.status, ChangeStatus::Failed);
    let err = rec.error.unwrap();
    assert_eq!(err["code"], codes::EXECUTION_FAILED);
    assert_eq!(err["details"]["orphaned_ids"], json!(["c1", "s1"]));
    assert!(err["message"].as_str().unwrap().contains("c1"));
}
