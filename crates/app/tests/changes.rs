mod common;

use std::sync::Arc;

use adsconsole_app::core::changes::{ChangesService, HistoryFilter};
use adsconsole_app::core::runtime::Runtime;
use adsconsole_core::Platform;
use adsconsole_core::config::{ServerSettings, WriteMode};
use adsconsole_mcp::fake::FakePlatform;
use chrono::{Duration, Utc};
use common::{free_port, options, write_config};
use serde_json::json;

struct Setup {
    rt: Arc<Runtime>,
    svc: ChangesService,
    fake: Arc<FakePlatform>,
    _dir: tempfile::TempDir,
}

async fn setup(mode: WriteMode) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), |c| {
        c.port = free_port();
        c.servers.insert(
            Platform::GoogleAds,
            ServerSettings {
                write_mode: mode,
                approval_wait_secs: 0,
                ..ServerSettings::default()
            },
        );
    });
    let (opts, fakes) = options(dir.path(), true);
    let rt = Arc::new(Runtime::boot(opts).await.unwrap());
    rt.engine(Platform::GoogleAds)
        .refresh_accounts()
        .await
        .unwrap();
    Setup {
        svc: ChangesService::new(rt.clone()),
        rt,
        fake: fakes.google,
        _dir: dir,
    }
}

/// Pede uma criação de campanha pela IA; no modo aprovação volta pendente.
async fn request_create(s: &Setup, name: &str) -> serde_json::Value {
    s.rt.engine(Platform::GoogleAds)
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": name, "daily_budget": 30}),
            Some("ia".into()),
        )
        .await
        .unwrap_or_else(|e| serde_json::to_value(e).unwrap())
}

async fn pending_id(s: &Setup, name: &str) -> String {
    let out = request_create(s, name).await;
    assert_eq!(out["status"], "pending", "{out}");
    out["change_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn pending_then_approve_applies() {
    let s = setup(WriteMode::Approval).await;
    let id = pending_id(&s, "x").await;
    let pending = s.svc.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].change.id, id);
    assert_eq!(pending[0].blocked_reason, None);
    let done = s.svc.approve(&id).await.unwrap();
    assert_eq!(done.status, "applied");
    assert!(s.svc.pending().unwrap().is_empty());
}

#[tokio::test]
async fn approve_blocked_when_read_only() {
    let s = setup(WriteMode::Approval).await;
    let id = pending_id(&s, "x").await;
    s.rt.update_config(|c| {
        c.servers.entry(Platform::GoogleAds).or_default().write_mode = WriteMode::ReadOnly
    })
    .unwrap();
    assert_eq!(
        s.svc.pending().unwrap()[0].blocked_reason.as_deref(),
        Some("read_only")
    );
    let err = s.svc.approve(&id).await.unwrap_err();
    assert_eq!(err.code, "READ_ONLY_MODE");
    assert_eq!(s.svc.reject(&id).await.unwrap().status, "rejected");
}

#[tokio::test]
async fn approve_expired_is_error() {
    let s = setup(WriteMode::Approval).await;
    let id = pending_id(&s, "x").await;
    let db = s.rt.db();
    db.delete_pending(&id).unwrap();
    db.insert_pending(&id, Utc::now() - Duration::seconds(1), None)
        .unwrap();
    let err = s.svc.approve(&id).await.unwrap_err();
    assert_eq!(err.code, "APPROVAL_EXPIRED");
}

#[tokio::test]
async fn reject_all_pending_counts() {
    let s = setup(WriteMode::Approval).await;
    let a = pending_id(&s, "a").await;
    pending_id(&s, "b").await;
    assert_eq!(s.svc.reject_all_pending().await.unwrap(), 2);
    assert!(s.svc.pending().unwrap().is_empty());
    assert_eq!(s.svc.get(&a).unwrap().status, "rejected");
}

#[tokio::test]
async fn history_exposes_orphans_and_undo() {
    let s = setup(WriteMode::Direct).await;
    s.fake.execute_in_steps(vec!["c1", "s1"], Some(1));
    request_create(&s, "falha").await;
    let ok = request_create(&s, "ok").await;
    assert_eq!(ok["status"], "applied", "{ok}");

    let history = s
        .svc
        .history(HistoryFilter {
            limit: 50,
            ..HistoryFilter::default()
        })
        .unwrap();
    let failed = history.iter().find(|c| c.status == "failed").unwrap();
    assert_eq!(failed.orphaned_ids, vec!["c1".to_string()]);

    let applied = history.iter().find(|c| c.status == "applied").unwrap();
    assert!(applied.reversible);
    let undone = s.svc.undo(&applied.id).await.unwrap();
    assert_eq!(undone.status, "applied", "{undone:?}");
    assert!(undone.change_id.is_some());
    assert!(s.svc.get(&applied.id).unwrap().undone_by.is_some());
}

#[tokio::test]
async fn history_filters_by_status_and_since() {
    let s = setup(WriteMode::Direct).await;
    request_create(&s, "a").await;
    let all = s
        .svc
        .history(HistoryFilter {
            limit: 50,
            ..HistoryFilter::default()
        })
        .unwrap();
    assert_eq!(all.len(), 1);
    let none = s
        .svc
        .history(HistoryFilter {
            status: Some("failed".into()),
            limit: 50,
            ..HistoryFilter::default()
        })
        .unwrap();
    assert!(none.is_empty());
    let future = s
        .svc
        .history(HistoryFilter {
            since: Some(Utc::now() + Duration::hours(1)),
            limit: 50,
            ..HistoryFilter::default()
        })
        .unwrap();
    assert!(future.is_empty());
}

#[tokio::test]
async fn undo_conflict_reports_state() {
    let s = setup(WriteMode::Direct).await;
    let created = request_create(&s, "x").await;
    let cid = created["result"]["campaign_id"]
        .as_str()
        .or_else(|| created["after"]["campaign_id"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| panic!("sem campaign_id: {created}"));
    let renamed =
        s.rt.engine(Platform::GoogleAds)
            .call_tool(
                "update_campaign",
                json!({"account_id": "111", "campaign_id": cid, "name": "y"}),
                Some("ia".into()),
            )
            .await
            .unwrap();
    s.fake.set_campaign_name_externally("111", &cid, "z");
    let err = s
        .svc
        .undo(renamed["change_id"].as_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(err.code, "UNDO_CONFLICT");
    assert!(err.details.is_some());
}

#[tokio::test]
async fn undo_from_app_does_not_wait_for_approval() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), |c| {
        c.port = free_port();
        c.servers.insert(
            Platform::GoogleAds,
            ServerSettings {
                write_mode: WriteMode::Approval,
                approval_wait_secs: 120,
                ..ServerSettings::default()
            },
        );
    });
    let (opts, _) = options(dir.path(), true);
    let rt = Arc::new(Runtime::boot(opts).await.unwrap());
    rt.engine(Platform::GoogleAds)
        .refresh_accounts()
        .await
        .unwrap();
    let svc = ChangesService::new(rt.clone());
    // Cria e aprova uma campanha (a aprovação acontece em paralelo à chamada da IA).
    let engine = rt.engine(Platform::GoogleAds).clone();
    let call = tokio::spawn(async move {
        engine
            .call_tool(
                "create_campaign",
                json!({"account_id": "111", "name": "x", "daily_budget": 30}),
                Some("ia".into()),
            )
            .await
    });
    let id = loop {
        if let Some(p) = svc.pending().unwrap().first() {
            break p.change.id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    svc.approve(&id).await.unwrap();
    call.await.unwrap().unwrap();

    let undone = tokio::time::timeout(std::time::Duration::from_secs(5), svc.undo(&id))
        .await
        .expect("o desfazer pelo app não pode esperar a aprovação")
        .unwrap();
    assert_eq!(undone.status, "applied", "{undone:?}");
    assert!(svc.pending().unwrap().is_empty());
}
