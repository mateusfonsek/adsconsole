mod common;

use std::sync::Arc;

use adsconsole_app::core::AppError;
use adsconsole_app::core::connections::{
    AccessLevelView, ConnStatusView, ConnectionsService, GoogleAddRequest, MetaLoginRequest,
    MetaTokenRequest, OpenFn, PlatformConnections, ResolvedGoogleAdd,
};
use adsconsole_core::Platform;
use adsconsole_core::db::{ConnectionMeta, ConnectionStatus};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use common::{boot_on, free_port};
use serde_json::json;
use tokio::sync::Notify;

/// Fake connections: the Meta login waits until someone releases the `Notify`.
struct FakeConnections {
    release: Arc<Notify>,
    /// Database where the "login" saves the connection before blocking (simulates listing accounts).
    db: Option<Arc<adsconsole_core::db::Db>>,
    removed: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl PlatformConnections for FakeConnections {
    async fn add_google(
        &self,
        _: ResolvedGoogleAdd,
        _: OpenFn,
    ) -> Result<ConnectionMeta, AppError> {
        Err(AppError::new("INTERNAL", "not used"))
    }
    async fn add_meta_login(
        &self,
        _: MetaLoginRequest,
        _: OpenFn,
    ) -> Result<ConnectionMeta, AppError> {
        if let Some(db) = &self.db {
            db.upsert_connection(&meta(
                "m_new",
                Platform::MetaAds,
                ConnectionStatus::Ok,
                None,
            ))
            .unwrap();
        }
        self.release.notified().await;
        Err(AppError::new("INTERNAL", "should not finish"))
    }
    async fn add_meta_token(&self, _: MetaTokenRequest) -> Result<ConnectionMeta, AppError> {
        Err(AppError::new("INTERNAL", "not used"))
    }
    async fn reconnect(&self, _: &ConnectionMeta, _: OpenFn) -> Result<ConnectionMeta, AppError> {
        Err(AppError::new("INTERNAL", "not used"))
    }
    fn remove(&self, m: &ConnectionMeta) -> Result<(), AppError> {
        self.removed.lock().unwrap().push(m.connection_id.clone());
        if let Some(db) = &self.db {
            db.delete_connection(&m.connection_id).unwrap();
        }
        Ok(())
    }
}

fn no_browser() -> OpenFn {
    Arc::new(|_| Ok(()))
}

async fn service(dir: &std::path::Path) -> (Arc<ConnectionsService>, Arc<Notify>) {
    service_with(dir, false).await
}

async fn service_with(
    dir: &std::path::Path,
    saves: bool,
) -> (Arc<ConnectionsService>, Arc<Notify>) {
    let (rt, _) = boot_on(dir, free_port()).await;
    let rt = Arc::new(rt);
    let release = Arc::new(Notify::new());
    let svc = ConnectionsService::new(
        rt.clone(),
        Arc::new(FakeConnections {
            release: release.clone(),
            db: saves.then(|| rt.db().clone()),
            removed: Default::default(),
        }),
    );
    (Arc::new(svc), release)
}

fn meta(
    id: &str,
    p: Platform,
    status: ConnectionStatus,
    expires_in_days: Option<i64>,
) -> ConnectionMeta {
    ConnectionMeta {
        connection_id: id.into(),
        platform: p,
        name: id.into(),
        is_default: false,
        status,
        expires_at: expires_in_days.map(|d| Utc::now() + Duration::days(d) + Duration::minutes(5)),
        extra: json!({}),
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn list_marks_expiring_and_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _) = service(dir.path()).await;
    let db = svc.runtime().db().clone();
    db.upsert_connection(&meta(
        "m_exp",
        Platform::MetaAds,
        ConnectionStatus::Ok,
        Some(3),
    ))
    .unwrap();
    db.upsert_connection(&meta(
        "m_far",
        Platform::MetaAds,
        ConnectionStatus::Ok,
        Some(30),
    ))
    .unwrap();
    db.upsert_connection(&meta(
        "m_bad",
        Platform::MetaAds,
        ConnectionStatus::NeedsReconnect,
        None,
    ))
    .unwrap();

    let list = svc.list().unwrap();
    let find = |id: &str| list.iter().find(|c| c.id == id).unwrap().status.clone();
    assert_eq!(find("m_exp"), ConnStatusView::ExpiringSoon { days: 3 });
    assert_eq!(find("m_far"), ConnStatusView::Ok);
    assert_eq!(find("m_bad"), ConnStatusView::NeedsReconnect);
    // Google comes before Meta.
    assert_eq!(list.first().unwrap().platform, Platform::GoogleAds);
}

#[tokio::test]
async fn set_default_unsets_others() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _) = service(dir.path()).await;
    svc.set_default("conn_a").unwrap();
    svc.set_default("conn_b").unwrap();
    let defaults: Vec<_> = svc
        .list()
        .unwrap()
        .into_iter()
        .filter(|c| c.is_default)
        .map(|c| c.id)
        .collect();
    assert_eq!(defaults, vec!["conn_b".to_string()]);
}

#[tokio::test]
async fn cancel_login_leaves_no_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _release) = service(dir.path()).await;
    let before = svc.list().unwrap().len();
    let s = svc.clone();
    let pending = tokio::spawn(async move {
        s.add_meta_login(
            MetaLoginRequest {
                name: "x".into(),
                app_id: "1".into(),
                config_id: "2".into(),
                app_secret: "s".into(),
                relay_url: None,
            },
            no_browser(),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    svc.cancel_login();
    let err = pending.await.unwrap().unwrap_err();
    assert_eq!(err.code, "LOGIN_CANCELLED");
    assert_eq!(svc.list().unwrap().len(), before);
}

#[tokio::test]
async fn web_client_json_is_field_error() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _) = service(dir.path()).await;
    let path = dir.path().join("client.json");
    std::fs::write(&path, r#"{"web":{"client_id":"a"}}"#).unwrap();

    let err = svc
        .google_console_links(&path.to_string_lossy())
        .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("client_json"));
    assert!(err.message.contains("Desktop app"), "{}", err.message);

    let err = svc
        .add_google(
            GoogleAddRequest {
                name: "g".into(),
                client_json_path: path.to_string_lossy().into(),
                login_customer_id: None,
                access_level: AccessLevelView::Explorer,
            },
            no_browser(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("client_json"));
}

#[tokio::test]
async fn console_links_use_project_id() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _) = service(dir.path()).await;
    let path = dir.path().join("client.json");
    std::fs::write(
        &path,
        r#"{"installed":{"client_id":"a","client_secret":"b","project_id":"proj-1"}}"#,
    )
    .unwrap();
    let links = svc.google_console_links(&path.to_string_lossy()).unwrap();
    assert!(links.api.contains("project=proj-1"));
    assert!(links.consent.contains("project=proj-1"));
    assert!(links.clients.contains("project=proj-1"));
}

#[tokio::test]
async fn cancel_after_save_removes_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _release) = service_with(dir.path(), true).await;
    let before: Vec<_> = svc.list().unwrap().into_iter().map(|c| c.id).collect();
    let s = svc.clone();
    let pending = tokio::spawn(async move {
        s.add_meta_login(
            MetaLoginRequest {
                name: "x".into(),
                app_id: "1".into(),
                config_id: "2".into(),
                app_secret: "s".into(),
                relay_url: None,
            },
            no_browser(),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(svc.list().unwrap().iter().any(|c| c.id == "m_new"));
    svc.cancel_login();
    assert_eq!(pending.await.unwrap().unwrap_err().code, "LOGIN_CANCELLED");
    let after: Vec<_> = svc.list().unwrap().into_iter().map(|c| c.id).collect();
    assert_eq!(after, before);
}
