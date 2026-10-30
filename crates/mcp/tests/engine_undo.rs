use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::{ChangeRecord, ChangeStatus, Db, new_change_id};
use adsconsole_mcp::engine::{Engine, SharedConfig};
use adsconsole_mcp::error::codes;
use adsconsole_mcp::fake::FakePlatform;
use chrono::Utc;
use serde_json::json;

fn by() -> Option<String> {
    Some("teste".into())
}

fn config_for(mode: WriteMode, wait_secs: u64) -> SharedConfig {
    let mut config = Config::default();
    config.servers.insert(
        Platform::GoogleAds,
        ServerSettings {
            write_mode: mode,
            approval_wait_secs: wait_secs,
            ..ServerSettings::default()
        },
    );
    Arc::new(RwLock::new(config))
}

struct Setup {
    engine: Engine,
    fake: Arc<FakePlatform>,
    db: Arc<Db>,
    config: SharedConfig,
}

async fn setup_mode(mode: WriteMode, wait_secs: u64) -> Setup {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let fake = FakePlatform::new(Platform::GoogleAds);
    fake.seed_connections(&db).unwrap();
    let config = config_for(mode, wait_secs);
    let engine = Engine::new(fake.clone(), db.clone(), config.clone()).unwrap();
    engine.refresh_accounts().await.unwrap();
    Setup {
        engine,
        fake,
        db,
        config,
    }
}

async fn setup() -> Setup {
    setup_mode(WriteMode::Direct, 120).await
}

fn set_mode(config: &SharedConfig, mode: WriteMode) {
    let mut c = config.write().unwrap();
    let mut s = c.server(Platform::GoogleAds);
    s.write_mode = mode;
    c.servers.insert(Platform::GoogleAds, s);
}

fn set_tool(config: &SharedConfig, tool: &str, on: bool) {
    let mut c = config.write().unwrap();
    let mut s = c.server(Platform::GoogleAds);
    s.tools.insert(tool.into(), on);
    c.servers.insert(Platform::GoogleAds, s);
}

/// Cria uma campanha "x" na conta 111; devolve (change_id, campaign_id).
async fn create(engine: &Engine, name: &str) -> (String, String) {
    let out = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": name, "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    (
        out["change_id"].as_str().unwrap().to_string(),
        out["result"]["campaign_id"].as_str().unwrap().to_string(),
    )
}

async fn rename(engine: &Engine, campaign: &str, name: &str) -> String {
    let out = engine
        .call_tool(
            "update_campaign",
            json!({"account_id": "111", "campaign_id": campaign, "name": name}),
            by(),
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    out["change_id"].as_str().unwrap().to_string()
}

fn name_of(fake: &FakePlatform, campaign: &str) -> String {
    fake.campaign("111", campaign).unwrap().name
}

fn executes(fake: &FakePlatform, tool: &str) -> usize {
    let entry = format!("execute:{tool}");
    fake.log().iter().filter(|l| **l == entry).count()
}

#[tokio::test]
async fn undo_update_restores_before() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let upd = rename(&s.engine, &cid, "y").await;
    assert_eq!(name_of(&s.fake, &cid), "y");

    let out = s.engine.undo(&upd, false, by()).await.unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(name_of(&s.fake, &cid), "x");

    let undo_id = out["change_id"].as_str().unwrap();
    let orig = s.db.get_change(&upd).unwrap().unwrap();
    assert_eq!(orig.status, ChangeStatus::Undone);
    assert_eq!(orig.undone_by.as_deref(), Some(undo_id));
    let undo = s.db.get_change(undo_id).unwrap().unwrap();
    assert_eq!(undo.undo_of.as_deref(), Some(upd.as_str()));
    assert_eq!(undo.tool, "update_campaign");
    assert_eq!(undo.account_id, "111");
    assert_eq!(undo.connection_id, orig.connection_id);
    assert_eq!(undo.requested_by.as_deref(), Some("teste"));
}

#[tokio::test]
async fn undo_create_removes_and_is_not_reversible() {
    let s = setup().await;
    let (chg, cid) = create(&s.engine, "x").await;
    let out = s.engine.undo(&chg, false, by()).await.unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], false);
    assert!(s.fake.campaign("111", &cid).is_none());
    let undo =
        s.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(undo.tool, "remove_campaign");
}

#[tokio::test]
async fn undo_conflict_then_force() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let upd = rename(&s.engine, &cid, "y").await;
    s.fake.set_campaign_name_externally("111", &cid, "z");

    let err = s.engine.undo(&upd, false, by()).await.unwrap_err();
    assert_eq!(err.code, codes::UNDO_CONFLICT, "{err:?}");
    let diffs = &err.details.as_ref().unwrap()["diffs"];
    assert_eq!(diffs.as_array().unwrap().len(), 1);
    assert_eq!(diffs[0]["path"], "/name");
    assert_eq!(diffs[0]["expected"], "y");
    assert_eq!(diffs[0]["actual"], "z");
    assert_eq!(
        err.suggestion.as_deref(),
        Some("O objeto mudou depois desta alteração. Use force=true para desfazer mesmo assim.")
    );
    assert_eq!(name_of(&s.fake, &cid), "z");
    assert_eq!(
        s.db.get_change(&upd).unwrap().unwrap().status,
        ChangeStatus::Applied
    );

    let out = s.engine.undo(&upd, true, by()).await.unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(name_of(&s.fake, &cid), "x");
}

#[tokio::test]
async fn undo_twice_refused() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let upd = rename(&s.engine, &cid, "y").await;
    let before = executes(&s.fake, "update_campaign");
    let first = s.engine.undo(&upd, false, by()).await.unwrap();
    let err = s.engine.undo(&upd, false, by()).await.unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
    assert_eq!(
        err.message,
        format!("Já desfeita por {}.", first["change_id"].as_str().unwrap())
    );
    assert_eq!(executes(&s.fake, "update_campaign"), before + 1);
}

#[tokio::test]
async fn undo_pending_undo_refused() {
    let s = setup_mode(WriteMode::Approval, 1).await;
    let pending = s
        .engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    let chg = pending["change_id"].as_str().unwrap().to_string();

    // Original ainda pendente: desfazer não se aplica.
    let err = s.engine.undo(&chg, false, by()).await.unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
    assert_eq!(
        err.message,
        "A mudança ainda está pendente. Recuse-a no AdsConsole em vez de desfazer."
    );

    let rec = s.engine.approve(&chg).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied);

    let out = s.engine.undo(&chg, false, by()).await.unwrap();
    assert_eq!(out["status"], "pending", "{out}");
    let undo_id = out["change_id"].as_str().unwrap();

    let err = s.engine.undo(&chg, false, by()).await.unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
    assert_eq!(
        err.message,
        format!("Já existe um desfazer pendente: {undo_id}.")
    );
    assert_eq!(executes(&s.fake, "remove_campaign"), 0);
}

#[tokio::test]
async fn undo_irreversible_refused() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let out = s
        .engine
        .call_tool(
            "remove_campaign",
            json!({"account_id": "111", "campaign_id": cid}),
            by(),
        )
        .await
        .unwrap();
    let chg = out["change_id"].as_str().unwrap();
    let err = s.engine.undo(chg, false, by()).await.unwrap_err();
    assert_eq!(err.code, codes::NOT_REVERSIBLE);
}

#[tokio::test]
async fn undo_unknown_and_failed_refused() {
    let s = setup().await;
    let err = s
        .engine
        .undo("chg_nao_existe", false, by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::CHANGE_NOT_FOUND);

    let (_, cid) = create(&s.engine, "x").await;
    s.fake.fail_next_execute(adsconsole_mcp::ToolError::new(
        codes::EXECUTION_FAILED,
        "falhou",
    ));
    let err = s
        .engine
        .call_tool(
            "update_campaign",
            json!({"account_id": "111", "campaign_id": cid, "name": "y"}),
            by(),
        )
        .await
        .unwrap_err();
    let failed = err.details.unwrap()["change_id"]
        .as_str()
        .unwrap()
        .to_string();
    let err = s.engine.undo(&failed, false, by()).await.unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
}

#[tokio::test]
async fn undo_respects_read_only() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let upd = rename(&s.engine, &cid, "y").await;
    set_mode(&s.config, WriteMode::ReadOnly);
    let log_len = s.fake.log().len();

    let err = s.engine.undo(&upd, false, by()).await.unwrap_err();
    assert_eq!(err.code, codes::READ_ONLY_MODE);
    let err = s
        .engine
        .call_tool("undo_change", json!({"change_id": upd}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::READ_ONLY_MODE);
    // Nada chegou ao adapter: nem current_state, nem precheck.
    assert_eq!(s.fake.log()[log_len..], [] as [String; 0]);
    assert_eq!(name_of(&s.fake, &cid), "y");

    // As leituras do histórico seguem funcionando em só leitura.
    let list = s
        .engine
        .call_tool("list_changes", json!({}), by())
        .await
        .unwrap();
    assert_eq!(list["changes"].as_array().unwrap().len(), 2);
    let st = s
        .engine
        .call_tool("get_change_status", json!({"change_id": upd}), by())
        .await
        .unwrap();
    assert_eq!(st["status"], "applied");
}

#[tokio::test]
async fn undo_of_undo_redoes() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let upd = rename(&s.engine, &cid, "y").await;
    let undo = s.engine.undo(&upd, false, by()).await.unwrap();
    assert_eq!(name_of(&s.fake, &cid), "x");
    let undo_id = undo["change_id"].as_str().unwrap();
    let redo = s.engine.undo(undo_id, false, by()).await.unwrap();
    assert_eq!(redo["status"], "applied", "{redo}");
    assert_eq!(name_of(&s.fake, &cid), "y");
    assert_eq!(
        s.db.get_change(undo_id).unwrap().unwrap().status,
        ChangeStatus::Undone
    );
}

#[tokio::test]
async fn undo_change_tool_dispatches_to_undo() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let upd = rename(&s.engine, &cid, "y").await;
    s.fake.set_campaign_name_externally("111", &cid, "z");
    let err = s
        .engine
        .call_tool("undo_change", json!({"change_id": upd}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::UNDO_CONFLICT);
    let out = s
        .engine
        .call_tool(
            "undo_change",
            json!({"change_id": upd, "force": true}),
            by(),
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(name_of(&s.fake, &cid), "x");

    let spec = s.engine.registry().get("undo_change").unwrap();
    assert!(!spec.takes_account && spec.exposed && spec.reversible && !spec.destructive);
    for name in ["list_changes", "get_change_status"] {
        let spec = s.engine.registry().get(name).unwrap();
        assert!(!spec.takes_account && spec.exposed);
        assert_eq!(spec.kind, adsconsole_mcp::ToolKind::Read);
    }
}

#[tokio::test]
async fn undo_change_respects_toggle() {
    let s = setup().await;
    let (chg, _) = create(&s.engine, "x").await;
    set_tool(&s.config, "undo_change", false);
    let err = s
        .engine
        .call_tool("undo_change", json!({"change_id": chg}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::TOOL_DISABLED);
    // O toggle da inversa (remove_campaign) não importa para o desfazer.
    set_tool(&s.config, "undo_change", true);
    set_tool(&s.config, "remove_campaign", false);
    let out = s
        .engine
        .call_tool("undo_change", json!({"change_id": chg}), by())
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
}

#[tokio::test]
async fn history_tools() {
    let s = setup().await;
    let (c1, cid) = create(&s.engine, "x").await;
    let c2 = rename(&s.engine, &cid, "y").await;
    let out = s
        .engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "444", "name": "w", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    let c3 = out["change_id"].as_str().unwrap().to_string();

    let page = s
        .engine
        .call_tool("list_changes", json!({"limit": 2}), by())
        .await
        .unwrap();
    let items = page["changes"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["change_id"], c3.as_str());
    assert_eq!(items[1]["change_id"], c2.as_str());
    assert_eq!(page["next_cursor"], "2");
    let item = &items[1];
    for key in [
        "change_id",
        "tool",
        "account_id",
        "status",
        "summary",
        "reversible",
        "requested_by",
        "undo_of",
        "undone_by",
        "created_at",
    ] {
        assert!(item.get(key).is_some(), "falta {key}: {item}");
    }
    assert_eq!(item["tool"], "update_campaign");
    assert_eq!(item["status"], "applied");
    assert_eq!(item["requested_by"], "teste");
    assert!(item.get("before").is_none());

    let page = s
        .engine
        .call_tool("list_changes", json!({"limit": 2, "cursor": "2"}), by())
        .await
        .unwrap();
    let items = page["changes"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["change_id"], c1.as_str());
    assert!(page.get("next_cursor").is_none());

    let by_account = s
        .engine
        .call_tool("list_changes", json!({"account_id": "444"}), by())
        .await
        .unwrap();
    let items = by_account["changes"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["change_id"], c3.as_str());

    let by_status = s
        .engine
        .call_tool("list_changes", json!({"status": "undone"}), by())
        .await
        .unwrap();
    assert!(by_status["changes"].as_array().unwrap().is_empty());
    let err = s
        .engine
        .call_tool("list_changes", json!({"status": "nope"}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    let err = s
        .engine
        .call_tool("list_changes", json!({"cursor": "abc"}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);

    let st = s
        .engine
        .call_tool("get_change_status", json!({"change_id": c2}), by())
        .await
        .unwrap();
    assert_eq!(st["change_id"], c2.as_str());
    assert_eq!(st["before"]["name"], "x");
    assert_eq!(st["after"]["name"], "y");
    assert_eq!(st["status"], "applied");

    let err = s
        .engine
        .call_tool("get_change_status", json!({"change_id": "chg_x"}), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::CHANGE_NOT_FOUND);
}

#[tokio::test]
async fn list_changes_limit_capped_and_scoped_to_platform() {
    let s = setup().await;
    for i in 0..3 {
        create(&s.engine, &format!("c{i}")).await;
    }
    // Uma mudança de outra plataforma no mesmo banco não aparece.
    let mut other =
        s.db.get_change(&create(&s.engine, "m").await.0)
            .unwrap()
            .unwrap();
    other.change_id = new_change_id();
    other.platform = Platform::MetaAds;
    s.db.insert_change(&other).unwrap();
    let page = s
        .engine
        .call_tool("list_changes", json!({"limit": 500}), by())
        .await
        .unwrap();
    assert_eq!(page["changes"].as_array().unwrap().len(), 4);
    assert!(page.get("next_cursor").is_none());
    let err = s
        .engine
        .call_tool(
            "get_change_status",
            json!({"change_id": other.change_id}),
            by(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::CHANGE_NOT_FOUND);
}

fn orphan(platform: Platform) -> ChangeRecord {
    let now = Utc::now();
    ChangeRecord {
        change_id: new_change_id(),
        platform,
        account_id: "111".into(),
        connection_id: "conn_a".into(),
        tool: "update_campaign".into(),
        input: json!({"account_id": "111", "campaign_id": "c1", "name": "y"}),
        summary: "Alterar a campanha c1".into(),
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
        progress: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn orphaned_approved_marked_failed_on_start() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let fake = FakePlatform::new(Platform::GoogleAds);
    fake.seed_connections(&db).unwrap();
    let mine = orphan(Platform::GoogleAds);
    let theirs = orphan(Platform::MetaAds);
    db.insert_change(&mine).unwrap();
    db.insert_change(&theirs).unwrap();

    Engine::new(fake, db.clone(), config_for(WriteMode::Direct, 120)).unwrap();

    let rec = db.get_change(&mine.change_id).unwrap().unwrap();
    assert_eq!(rec.status, ChangeStatus::Failed);
    assert!(!rec.reversible);
    let error = rec.error.unwrap();
    assert_eq!(error["code"], codes::EXECUTION_FAILED);
    assert_eq!(
        error["message"],
        "Execução interrompida; confira o estado na plataforma antes de repetir."
    );
    let other = db.get_change(&theirs.change_id).unwrap().unwrap();
    assert_eq!(other.status, ChangeStatus::Approved);
}

#[tokio::test]
async fn concurrent_undos_direct_execute_once() {
    let s = setup().await;
    let (_, cid) = create(&s.engine, "x").await;
    let upd = rename(&s.engine, &cid, "y").await;
    let before = executes(&s.fake, "update_campaign");
    let (a, b) = tokio::join!(
        s.engine
            .call_tool("undo_change", json!({"change_id": upd}), by()),
        s.engine
            .call_tool("undo_change", json!({"change_id": upd}), by()),
    );
    let (ok, err) = match (a, b) {
        (Ok(ok), Err(err)) | (Err(err), Ok(ok)) => (ok, err),
        other => panic!("esperava um sucesso e uma recusa: {other:?}"),
    };
    assert_eq!(ok["status"], "applied", "{ok}");
    assert_eq!(err.code, codes::INVALID_STATE, "{err:?}");
    assert_eq!(executes(&s.fake, "update_campaign"), before + 1);
    assert_eq!(name_of(&s.fake, &cid), "x");
}

#[tokio::test]
async fn two_pending_undos_approved_execute_once() {
    let s = setup_mode(WriteMode::Approval, 0).await;
    let created = s
        .engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    let rec = s
        .engine
        .approve(created["change_id"].as_str().unwrap())
        .await
        .unwrap();
    let cid = rec.after.unwrap()["campaign_id"]
        .as_str()
        .unwrap()
        .to_string();
    let upd = s
        .engine
        .call_tool(
            "update_campaign",
            json!({"account_id": "111", "campaign_id": cid, "name": "y"}),
            by(),
        )
        .await
        .unwrap();
    let upd = upd["change_id"].as_str().unwrap().to_string();
    assert_eq!(
        s.engine.approve(&upd).await.unwrap().status,
        ChangeStatus::Applied
    );

    // Um desfazer pela via normal e um segundo inserido direto (como se as checagens
    // do undo tivessem corrido em paralelo).
    let first = s.engine.undo(&upd, false, by()).await.unwrap();
    assert_eq!(first["status"], "pending", "{first}");
    let u1 = first["change_id"].as_str().unwrap().to_string();
    let mut twin = s.db.get_change(&u1).unwrap().unwrap();
    twin.change_id = new_change_id();
    s.db.insert_change(&twin).unwrap();
    s.db.insert_pending(
        &twin.change_id,
        Utc::now() + chrono::Duration::hours(1),
        None,
    )
    .unwrap();

    let before = executes(&s.fake, "update_campaign");
    let (a, b) = tokio::join!(s.engine.approve(&u1), s.engine.approve(&twin.change_id));
    let (a, b) = (a.unwrap(), b.unwrap());
    let statuses = [a.status, b.status];
    assert!(statuses.contains(&ChangeStatus::Applied), "{statuses:?}");
    assert!(statuses.contains(&ChangeStatus::Failed), "{statuses:?}");
    let failed = if a.status == ChangeStatus::Failed {
        &a
    } else {
        &b
    };
    assert!(!failed.reversible);
    let error = failed.error.as_ref().unwrap();
    assert_eq!(error["code"], codes::INVALID_STATE);
    let applied = if a.status == ChangeStatus::Applied {
        &a
    } else {
        &b
    };
    assert_eq!(
        error["message"],
        format!("Já desfeita por {}.", applied.change_id)
    );
    assert_eq!(executes(&s.fake, "update_campaign"), before + 1);
    assert_eq!(name_of(&s.fake, &cid), "x");
}

/// Modo aprovação: cria "x", renomeia para "y" (ambas aprovadas) e devolve
/// (id da renomeação, id da campanha).
async fn approved_rename(s: &Setup) -> (String, String) {
    let created = s
        .engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 30}),
            by(),
        )
        .await
        .unwrap();
    let rec = s
        .engine
        .approve(created["change_id"].as_str().unwrap())
        .await
        .unwrap();
    let cid = rec.after.unwrap()["campaign_id"]
        .as_str()
        .unwrap()
        .to_string();
    let upd = s
        .engine
        .call_tool(
            "update_campaign",
            json!({"account_id": "111", "campaign_id": cid, "name": "y"}),
            by(),
        )
        .await
        .unwrap();
    let upd = upd["change_id"].as_str().unwrap().to_string();
    assert_eq!(
        s.engine.approve(&upd).await.unwrap().status,
        ChangeStatus::Applied
    );
    (upd, cid)
}

#[tokio::test]
async fn pending_undo_rechecks_conflict_on_approve() {
    let s = setup_mode(WriteMode::Approval, 0).await;
    let (upd, cid) = approved_rename(&s).await;
    let pending = s.engine.undo(&upd, false, by()).await.unwrap();
    assert_eq!(pending["status"], "pending", "{pending}");
    let undo_id = pending["change_id"].as_str().unwrap().to_string();

    // Edição externa enquanto o desfazer espera na fila.
    s.fake.set_campaign_name_externally("111", &cid, "z");
    let before = executes(&s.fake, "update_campaign");
    let rec = s.engine.approve(&undo_id).await.unwrap();

    assert_eq!(rec.status, ChangeStatus::Failed);
    assert!(!rec.reversible);
    let error = rec.error.as_ref().unwrap();
    assert_eq!(error["code"], codes::UNDO_CONFLICT, "{error}");
    let diffs = error["details"]["diffs"].as_array().unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0]["path"], "/name");
    assert_eq!(diffs[0]["expected"], "y");
    assert_eq!(diffs[0]["actual"], "z");
    assert_eq!(
        error["suggestion"],
        "O objeto mudou depois desta alteração. Use force=true para desfazer mesmo assim."
    );
    assert_eq!(executes(&s.fake, "update_campaign"), before);
    assert_eq!(name_of(&s.fake, &cid), "z");
    let orig = s.db.get_change(&upd).unwrap().unwrap();
    assert_eq!(orig.status, ChangeStatus::Applied);
    assert!(orig.undone_by.is_none());
    let stored = s.db.get_change(&undo_id).unwrap().unwrap();
    assert_eq!(stored.status, ChangeStatus::Failed);
    assert!(stored.after.is_none());
}

#[tokio::test]
async fn pending_forced_undo_applies_despite_conflict() {
    let s = setup_mode(WriteMode::Approval, 0).await;
    let (upd, cid) = approved_rename(&s).await;
    let pending = s.engine.undo(&upd, true, by()).await.unwrap();
    assert_eq!(pending["status"], "pending", "{pending}");
    let undo_id = pending["change_id"].as_str().unwrap().to_string();
    assert!(s.db.get_change(&undo_id).unwrap().unwrap().undo_force);

    s.fake.set_campaign_name_externally("111", &cid, "z");
    let before = executes(&s.fake, "update_campaign");
    let rec = s.engine.approve(&undo_id).await.unwrap();

    assert_eq!(rec.status, ChangeStatus::Applied, "{:?}", rec.error);
    assert_eq!(executes(&s.fake, "update_campaign"), before + 1);
    assert_eq!(name_of(&s.fake, &cid), "x");
    let orig = s.db.get_change(&upd).unwrap().unwrap();
    assert_eq!(orig.status, ChangeStatus::Undone);
    assert_eq!(orig.undone_by.as_deref(), Some(undo_id.as_str()));
}

#[tokio::test]
async fn pending_undo_fails_when_state_read_fails_on_approve() {
    let s = setup_mode(WriteMode::Approval, 0).await;
    let (upd, cid) = approved_rename(&s).await;
    let pending = s.engine.undo(&upd, false, by()).await.unwrap();
    let undo_id = pending["change_id"].as_str().unwrap().to_string();

    s.fake
        .fail_next_current_state(adsconsole_mcp::ToolError::new(
            codes::EXECUTION_FAILED,
            "fora do ar",
        ));
    let before = executes(&s.fake, "update_campaign");
    let rec = s.engine.approve(&undo_id).await.unwrap();

    assert_eq!(rec.status, ChangeStatus::Failed);
    assert_eq!(rec.error.as_ref().unwrap()["message"], "fora do ar");
    assert_eq!(
        s.db.get_change(&undo_id).unwrap().unwrap().status,
        ChangeStatus::Failed
    );
    assert_eq!(executes(&s.fake, "update_campaign"), before);
    assert_eq!(name_of(&s.fake, &cid), "y");
}
