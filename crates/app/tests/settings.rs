mod common;

use std::sync::Arc;

use adsconsole_app::core::settings::SettingsService;
use adsconsole_core::Platform;
use adsconsole_core::config::{Config, Theme, ToolGroup, WriteMode};
use common::{boot_on, free_port};

async fn service(dir: &std::path::Path) -> SettingsService {
    let (rt, _) = boot_on(dir, free_port()).await;
    SettingsService::new(Arc::new(rt))
}

#[tokio::test]
async fn escape_group_off_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(dir.path()).await;
    let view = svc.tools(Platform::GoogleAds);
    let escape = view.groups.iter().find(|g| g.group == ToolGroup::Escape);
    if let Some(g) = escape {
        assert!(!g.enabled);
    }
    let read = view
        .groups
        .iter()
        .find(|g| g.group == ToolGroup::Read)
        .unwrap();
    assert!(read.enabled);
    assert!(read.tools.iter().all(|t| t.enabled));
}

#[tokio::test]
async fn set_tool_hides_it_from_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(dir.path()).await;
    svc.set_tool(Platform::GoogleAds, "create_campaign", false)
        .unwrap();
    let rt = svc.runtime();
    let settings = rt.config().server(Platform::GoogleAds);
    assert!(
        !rt.engine(Platform::GoogleAds)
            .registry()
            .is_enabled("create_campaign", &settings)
    );
    let err = svc
        .set_tool(Platform::GoogleAds, "nao_existe", false)
        .unwrap_err();
    assert_eq!(err.code, "INVALID_INPUT");
}

#[tokio::test]
async fn set_group_clears_tool_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(dir.path()).await;
    svc.set_tool(Platform::GoogleAds, "create_campaign", false)
        .unwrap();
    svc.set_group(Platform::GoogleAds, ToolGroup::Create, true)
        .unwrap();
    let view = svc.tools(Platform::GoogleAds);
    let create = view
        .groups
        .iter()
        .find(|g| g.group == ToolGroup::Create)
        .unwrap();
    assert!(create.tools.iter().all(|t| t.enabled));
}

#[tokio::test]
async fn write_mode_change_applies() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(dir.path()).await;
    svc.set_write_mode(Platform::MetaAds, WriteMode::ReadOnly)
        .unwrap();
    assert_eq!(svc.tools(Platform::MetaAds).write_mode, WriteMode::ReadOnly);
}

#[tokio::test]
async fn approval_wait_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(dir.path()).await;
    assert_eq!(
        svc.set_approval_wait(Platform::GoogleAds, 29)
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
    svc.set_approval_wait(Platform::GoogleAds, 30).unwrap();
    svc.set_approval_wait(Platform::GoogleAds, 900).unwrap();
    assert_eq!(
        svc.set_approval_wait(Platform::GoogleAds, 901)
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
    assert_eq!(svc.tools(Platform::GoogleAds).approval_wait_secs, 900);
}

#[tokio::test]
async fn settings_persist_across_boot() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(dir.path()).await;
    svc.set_language("en").unwrap();
    svc.set_theme(Theme::Dark).unwrap();
    assert_eq!(svc.set_language("fr").unwrap_err().code, "INVALID_INPUT");
    let saved = Config::load(&dir.path().join("config.json"))
        .unwrap()
        .config;
    assert_eq!(saved.language.as_deref(), Some("en"));
    assert_eq!(saved.theme, Theme::Dark);
    let app = svc.app();
    assert_eq!(app.language, "en");
    assert_eq!(app.theme, Theme::Dark);
    assert!(app.repo_url.starts_with("https://github.com/"));
}
