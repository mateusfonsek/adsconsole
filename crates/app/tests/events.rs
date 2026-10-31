mod common;

use std::sync::Arc;

use adsconsole_app::core::changes::ChangesService;
use adsconsole_app::core::events::{AppEvent, QuitOutcome, bridge, prepare_quit, tray_title};
use adsconsole_app::core::runtime::Runtime;
use adsconsole_core::Platform;
use adsconsole_core::config::{ServerSettings, WriteMode};
use common::{free_port, options, write_config};
use serde_json::json;

async fn approval_runtime() -> (Arc<Runtime>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), |c| {
        c.port = free_port();
        c.servers.insert(
            Platform::GoogleAds,
            ServerSettings {
                write_mode: WriteMode::Approval,
                approval_wait_secs: 0,
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
    (rt, dir)
}

async fn request(rt: &Runtime, name: &str) -> String {
    let out = rt
        .engine(Platform::GoogleAds)
        .call_tool(
            "create_campaign",
            json!({"account_id": "111", "name": name, "daily_budget": 30}),
            None,
        )
        .await
        .unwrap();
    out["change_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn bridge_maps_engine_events() {
    let (rt, _dir) = approval_runtime().await;
    let mut rx = bridge(rt.engines());
    let id = request(&rt, "x").await;
    match rx.recv().await.unwrap() {
        AppEvent::ApprovalRequested {
            change_id,
            platform,
            summary,
        } => {
            assert_eq!(change_id, id);
            assert_eq!(platform, Platform::GoogleAds);
            assert!(!summary.is_empty());
        }
        other => panic!("esperava ApprovalRequested, veio {other:?}"),
    }
    rt.engine(Platform::GoogleAds).approve(&id).await.unwrap();
    assert_eq!(
        rx.recv().await.unwrap(),
        AppEvent::ChangeUpdated { change_id: id }
    );
}

#[test]
fn tray_title_counts() {
    assert_eq!(tray_title(0), None);
    assert_eq!(tray_title(3), Some("3".to_string()));
    assert_eq!(tray_title(120), Some("99+".to_string()));
}

#[tokio::test]
async fn quit_with_pending_rejects_all() {
    let (rt, _dir) = approval_runtime().await;
    let svc = ChangesService::new(rt.clone());
    let a = request(&rt, "a").await;
    request(&rt, "b").await;

    assert_eq!(
        prepare_quit(&svc, false).await.unwrap(),
        QuitOutcome::NeedsConfirmation { pending: 2 }
    );
    assert_eq!(svc.pending().unwrap().len(), 2);

    assert_eq!(prepare_quit(&svc, true).await.unwrap(), QuitOutcome::Ready);
    assert!(svc.pending().unwrap().is_empty());
    assert_eq!(svc.get(&a).unwrap().status, "rejected");
}

#[tokio::test]
async fn quit_without_pending_is_ready() {
    let (rt, _dir) = approval_runtime().await;
    let svc = ChangesService::new(rt);
    assert_eq!(prepare_quit(&svc, false).await.unwrap(), QuitOutcome::Ready);
}
