use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::{ChangeStatus, Db};
use adsconsole_mcp::engine::{Engine, EngineEvent, PENDING_TTL};
use adsconsole_mcp::error::codes;
use adsconsole_mcp::fake::FakePlatform;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use tempfile::TempDir;

fn by() -> Option<String> {
    Some("teste".into())
}

async fn engine_on(fake: Arc<FakePlatform>, db: Arc<Db>, wait_secs: u64) -> Engine {
    let mut config = Config::default();
    config.servers.insert(
        Platform::GoogleAds,
        ServerSettings {
            write_mode: WriteMode::Approval,
            approval_wait_secs: wait_secs,
            ..ServerSettings::default()
        },
    );
    let engine = Engine::new(fake, db, Arc::new(RwLock::new(config))).unwrap();
    engine.refresh_accounts().await.unwrap();
    engine
}

/// Modo aprovação sobre um banco em arquivo (o teste de reinício reabre o mesmo arquivo).
async fn setup_approval(wait_secs: u64) -> (Engine, Arc<FakePlatform>, Arc<Db>, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("adsconsole.db")).unwrap());
    let fake = FakePlatform::new(Platform::GoogleAds);
    fake.seed_connections(&db).unwrap();
    let engine = engine_on(fake.clone(), db.clone(), wait_secs).await;
    (engine, fake, db, dir)
}

fn create_input(name: &str) -> Value {
    json!({"account_id": "111", "name": name, "daily_budget": 30})
}

/// Chama a ferramenta esperando o corpo `pending` e devolve o `change_id`.
async fn submit_pending(engine: &Engine, tool: &str, input: Value) -> String {
    let out = engine.call_tool(tool, input, by()).await.unwrap();
    assert_eq!(out["status"], "pending", "{out}");
    assert_eq!(
        out["message"],
        "Aguardando aprovação no AdsConsole. Consulte o resultado com get_change_status."
    );
    assert!(out["summary"].is_string());
    out["change_id"].as_str().unwrap().to_string()
}

/// Cria uma campanha passando pela fila e devolve o id dela na fake.
async fn create_approved(engine: &Engine, name: &str) -> String {
    let id = submit_pending(engine, "create_campaign", create_input(name)).await;
    let rec = engine.approve(&id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied);
    rec.after.unwrap()["campaign_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn executes(fake: &FakePlatform, tool: &str) -> usize {
    let entry = format!("execute:{tool}");
    fake.log().iter().filter(|l| **l == entry).count()
}

/// Responde a cada `ApprovalRequested` com `decide(engine, id)`.
fn auto_decide(engine: &Engine, approve: bool) -> tokio::task::JoinHandle<()> {
    let mut rx = engine.subscribe();
    let engine = engine.clone();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let EngineEvent::ApprovalRequested { change_id, .. } = ev {
                if approve {
                    engine.approve(&change_id).await.unwrap();
                } else {
                    engine.reject(&change_id).await.unwrap();
                }
                return;
            }
        }
    })
}

#[tokio::test]
async fn approve_while_waiting_returns_applied() {
    let (engine, fake, _db, _dir) = setup_approval(5).await;
    let decider = auto_decide(&engine, true);
    let out = engine
        .call_tool("create_campaign", create_input("Aprovada"), by())
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let cid = out["result"]["campaign_id"].as_str().unwrap();
    assert_eq!(fake.campaign("111", cid).unwrap().name, "Aprovada");
    decider.await.unwrap();
    assert!(engine.pending().unwrap().is_empty());
}

#[tokio::test]
async fn reject_while_waiting() {
    let (engine, fake, db, _dir) = setup_approval(5).await;
    let decider = auto_decide(&engine, false);
    let err = engine
        .call_tool("create_campaign", create_input("Recusada"), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::APPROVAL_REJECTED);
    assert_eq!(err.message, "O usuário recusou esta mudança no AdsConsole.");
    decider.await.unwrap();
    assert!(!fake.log().iter().any(|l| l.starts_with("execute:")));
    let id = err.details.unwrap()["change_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        db.get_change(&id).unwrap().unwrap().status,
        ChangeStatus::Rejected
    );
    assert!(engine.pending().unwrap().is_empty());
}

#[tokio::test]
async fn approve_after_timeout_executes() {
    let (engine, fake, db, _dir) = setup_approval(1).await;
    let id = submit_pending(&engine, "create_campaign", create_input("Depois")).await;
    assert_eq!(executes(&fake, "create_campaign"), 0);
    let pend = engine.pending().unwrap();
    assert_eq!(pend.len(), 1);
    assert_eq!(pend[0].change.change_id, id);
    assert_eq!(pend[0].change.status, ChangeStatus::Pending);

    let rec = engine.approve(&id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied);
    // Ruling H: get_change_status chega na Task 10; o banco mostra o resultado.
    let stored = db.get_change(&id).unwrap().unwrap();
    assert_eq!(stored.status, ChangeStatus::Applied);
    assert_eq!(executes(&fake, "create_campaign"), 1);
    assert!(engine.pending().unwrap().is_empty());
}

#[tokio::test]
async fn double_approve_executes_once() {
    let (engine, fake, _db, _dir) = setup_approval(0).await;
    let id = submit_pending(&engine, "create_campaign", create_input("Dupla")).await;
    let (a, b) = tokio::join!(engine.approve(&id), engine.approve(&id));
    let (oks, errs): (Vec<_>, Vec<_>) = [a, b].into_iter().partition(|r| r.is_ok());
    assert_eq!(oks.len(), 1);
    assert_eq!(errs.len(), 1);
    let err = errs.into_iter().next().unwrap().unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
    assert!(
        err.message.starts_with("Esta mudança já foi decidida: "),
        "{}",
        err.message
    );
    assert_eq!(executes(&fake, "create_campaign"), 1);
}

#[tokio::test]
async fn approve_after_reject_fails() {
    let (engine, fake, _db, _dir) = setup_approval(0).await;
    let id = submit_pending(&engine, "create_campaign", create_input("X")).await;
    let rec = engine.reject(&id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Rejected);
    let err = engine.approve(&id).await.unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
    assert_eq!(err.message, "Esta mudança já foi decidida: rejected.");
    assert_eq!(
        engine.reject(&id).await.unwrap_err().code,
        codes::INVALID_STATE
    );
    assert_eq!(executes(&fake, "create_campaign"), 0);
}

#[tokio::test]
async fn approve_unknown_change_is_not_found() {
    let (engine, _fake, _db, _dir) = setup_approval(0).await;
    assert_eq!(
        engine.approve("chg_nao_existe").await.unwrap_err().code,
        codes::CHANGE_NOT_FOUND
    );
    assert_eq!(
        engine.reject("chg_nao_existe").await.unwrap_err().code,
        codes::CHANGE_NOT_FOUND
    );
}

#[tokio::test]
async fn approve_expired_fails() {
    let (engine, fake, db, _dir) = setup_approval(0).await;
    let id = submit_pending(&engine, "create_campaign", create_input("Velha")).await;
    let pend = engine.pending().unwrap();
    let ttl = pend[0].expires_at - pend[0].change.created_at;
    assert!((ttl - PENDING_TTL).num_seconds().abs() <= 1, "{ttl}");

    let mut rx = engine.subscribe();
    let expired = engine
        .expire_stale(Utc::now() + Duration::hours(25))
        .unwrap();
    assert_eq!(expired, vec![id.clone()]);
    assert!(matches!(
        rx.try_recv().unwrap(),
        EngineEvent::ChangeExpired { change_id } if change_id == id
    ));
    let err = engine.approve(&id).await.unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
    assert_eq!(err.message, "Esta mudança já foi decidida: expired.");
    assert!(db.list_pending(None).unwrap().is_empty());
    assert_eq!(executes(&fake, "create_campaign"), 0);
    // Nada mais a expirar.
    assert!(
        engine
            .expire_stale(Utc::now() + Duration::hours(25))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expire_stale_keeps_fresh_pending() {
    let (engine, _fake, _db, _dir) = setup_approval(0).await;
    submit_pending(&engine, "create_campaign", create_input("Nova")).await;
    assert!(engine.expire_stale(Utc::now()).unwrap().is_empty());
    assert_eq!(engine.pending().unwrap().len(), 1);
}

#[tokio::test]
async fn waiting_call_sees_expiration() {
    let (engine, _fake, _db, _dir) = setup_approval(5).await;
    let mut rx = engine.subscribe();
    let expirer = engine.clone();
    let task = tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let EngineEvent::ApprovalRequested { .. } = ev {
                expirer
                    .expire_stale(Utc::now() + Duration::hours(25))
                    .unwrap();
                return;
            }
        }
    });
    let err = engine
        .call_tool("create_campaign", create_input("Expira"), by())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::APPROVAL_EXPIRED);
    task.await.unwrap();
}

#[tokio::test]
async fn approve_after_restart() {
    let (engine1, fake, db1, dir) = setup_approval(1).await;
    let id = submit_pending(&engine1, "create_campaign", create_input("Reinício")).await;
    drop(engine1);
    drop(db1);

    let db2 = Arc::new(Db::open(&dir.path().join("adsconsole.db")).unwrap());
    let engine2 = engine_on(fake.clone(), db2, 1).await;
    let pend = engine2.pending().unwrap();
    assert_eq!(pend.len(), 1);
    assert_eq!(pend[0].change.change_id, id);
    let rec = engine2.approve(&id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied);
    let cid = rec.after.unwrap()["campaign_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(fake.campaign("111", &cid).unwrap().name, "Reinício");
}

#[tokio::test]
async fn pending_snapshot_and_warning() {
    let (engine, _fake, _db, _dir) = setup_approval(0).await;
    let cid = create_approved(&engine, "Nome antigo").await;

    let upd = submit_pending(
        &engine,
        "update_campaign",
        json!({"account_id": "111", "campaign_id": cid, "name": "Nome novo"}),
    )
    .await;
    let pend = engine.pending().unwrap();
    let p = pend.iter().find(|p| p.change.change_id == upd).unwrap();
    assert_eq!(p.change.before.as_ref().unwrap()["name"], "Nome antigo");
    assert!(p.warning.is_none(), "{:?}", p.warning);
    engine.reject(&upd).await.unwrap();

    let rem = submit_pending(
        &engine,
        "remove_campaign",
        json!({"account_id": "111", "campaign_id": cid}),
    )
    .await;
    let pend = engine.pending().unwrap();
    let p = pend.iter().find(|p| p.change.change_id == rem).unwrap();
    assert!(
        p.warning
            .as_deref()
            .unwrap()
            .contains("não pode ser desfeita"),
        "{:?}",
        p.warning
    );
}

#[tokio::test]
async fn failed_execution_after_approval_is_ok_with_failed_record() {
    let (engine, fake, db, _dir) = setup_approval(0).await;
    let id = submit_pending(&engine, "create_campaign", create_input("Falha")).await;
    fake.fail_next_execute(adsconsole_mcp::ToolError::new(
        codes::EXECUTION_FAILED,
        "boom",
    ));
    let rec = engine.approve(&id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Failed);
    assert_eq!(
        db.get_change(&id).unwrap().unwrap().status,
        ChangeStatus::Failed
    );
}

#[tokio::test]
async fn validation_failure_never_queues() {
    let (engine, fake, _db, _dir) = setup_approval(0).await;
    let err = engine
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": "x", "daily_budget": 10}),
            by(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::BUDGET_TOO_LOW);
    assert!(engine.pending().unwrap().is_empty());
    assert!(!fake.log().iter().any(|l| l.starts_with("snapshot:")));
}

#[tokio::test]
async fn approve_past_deadline_expires() {
    let (engine, fake, db, _dir) = setup_approval(0).await;
    let id = submit_pending(&engine, "create_campaign", create_input("Vencida")).await;
    db.delete_pending(&id).unwrap();
    db.insert_pending(&id, Utc::now() - Duration::minutes(1), None)
        .unwrap();
    let mut rx = engine.subscribe();
    let err = engine.approve(&id).await.unwrap_err();
    assert_eq!(err.code, codes::APPROVAL_EXPIRED);
    assert!(matches!(
        rx.try_recv().unwrap(),
        EngineEvent::ChangeExpired { change_id } if change_id == id
    ));
    assert_eq!(
        db.get_change(&id).unwrap().unwrap().status,
        ChangeStatus::Expired
    );
    assert!(engine.pending().unwrap().is_empty());
    assert_eq!(executes(&fake, "create_campaign"), 0);
}

#[tokio::test]
async fn dropped_waiter_keeps_pending() {
    let (engine, fake, _db, _dir) = setup_approval(30).await;
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        engine.call_tool("create_campaign", create_input("Solta"), by()),
    )
    .await;
    assert!(dropped.is_err());
    let pend = engine.pending().unwrap();
    assert_eq!(pend.len(), 1);
    let rec = engine.approve(&pend[0].change.change_id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied);
    assert_eq!(executes(&fake, "create_campaign"), 1);
}
