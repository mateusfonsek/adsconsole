//! Commands called by the UI. Each one only wraps a method of the core (`crate::core`).

use std::sync::Arc;

use adsconsole_core::Platform;
use adsconsole_core::config::{Theme, ToolGroup, WriteMode};
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;
use tauri_specta::{Builder, ErrorHandlingMode, collect_commands, collect_events};

use super::state::AppState;
use super::{AppEventMsg, emit};
use crate::core::AppError;
use crate::core::changes::{ChangeView, HistoryFilter, PendingView};
use crate::core::clients::{AiClient, masked, snippet};
use crate::core::connections::{
    ConnectionView, ConsoleLinksView, GoogleAddRequest, MetaLoginRequest, MetaTokenRequest, OpenFn,
};
use crate::core::events::{AppEvent, QuitOutcome, prepare_quit};
use crate::core::runtime::{Runtime, ServerState};
use crate::core::services::BootStateView;
use crate::core::settings::{AppSettingsView, ToolSettingsView};

type R<T> = Result<T, AppError>;

fn browser() -> OpenFn {
    Arc::new(|url| open::that(url))
}

/// App version (from `Cargo.toml`).
#[tauri::command]
#[specta::specta]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ---- Boot ----

#[tauri::command]
#[specta::specta]
pub fn boot_state(state: State<'_, AppState>) -> BootStateView {
    state.boot_state()
}

#[tauri::command]
#[specta::specta]
pub fn retry_keychain(app: AppHandle) -> BootStateView {
    super::start(&app);
    tauri::Manager::state::<AppState>(&app).boot_state()
}

#[tauri::command]
#[specta::specta]
pub fn use_file_backend(app: AppHandle, state: State<'_, AppState>) -> R<BootStateView> {
    Runtime::use_file_backend(&state.data_dir)?;
    super::start(&app);
    Ok(state.boot_state())
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct OnboardingState {
    pub has_connection: bool,
    pub client_seen: bool,
}

#[tauri::command]
#[specta::specta]
pub fn onboarding_state(state: State<'_, AppState>) -> R<OnboardingState> {
    let s = state.services()?;
    Ok(OnboardingState {
        has_connection: !s.connections.list()?.is_empty(),
        client_seen: s.rt.config().client_seen,
    })
}

// ---- Connections ----

#[tauri::command]
#[specta::specta]
pub fn list_connections(state: State<'_, AppState>) -> R<Vec<ConnectionView>> {
    state.services()?.connections.list()
}

#[tauri::command]
#[specta::specta]
pub async fn add_google_connection(
    state: State<'_, AppState>,
    req: GoogleAddRequest,
) -> R<ConnectionView> {
    state
        .services()?
        .connections
        .add_google(req, browser())
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn add_meta_login(
    state: State<'_, AppState>,
    req: MetaLoginRequest,
) -> R<ConnectionView> {
    state
        .services()?
        .connections
        .add_meta_login(req, browser())
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn add_meta_token(
    state: State<'_, AppState>,
    req: MetaTokenRequest,
) -> R<ConnectionView> {
    state.services()?.connections.add_meta_token(req).await
}

#[tauri::command]
#[specta::specta]
pub async fn reconnect_connection(state: State<'_, AppState>, id: String) -> R<ConnectionView> {
    state
        .services()?
        .connections
        .reconnect(&id, browser())
        .await
}

#[tauri::command]
#[specta::specta]
pub fn remove_connection(state: State<'_, AppState>, id: String) -> R<()> {
    state.services()?.connections.remove(&id)
}

#[tauri::command]
#[specta::specta]
pub fn set_default_connection(state: State<'_, AppState>, id: String) -> R<()> {
    state.services()?.connections.set_default(&id)
}

#[tauri::command]
#[specta::specta]
pub async fn test_connection(state: State<'_, AppState>, id: String) -> R<u32> {
    state.services()?.connections.test(&id).await
}

#[tauri::command]
#[specta::specta]
pub fn cancel_login(state: State<'_, AppState>) -> R<()> {
    state.services()?.connections.cancel_login();
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn google_console_links(state: State<'_, AppState>, path: String) -> R<ConsoleLinksView> {
    state.services()?.connections.google_console_links(&path)
}

// ---- MCPs ----

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ServerStatusView {
    pub state: ServerState,
    pub google_enabled: bool,
    pub meta_enabled: bool,
    pub google_tools: u32,
    pub meta_tools: u32,
    pub google_write_mode: WriteMode,
    pub meta_write_mode: WriteMode,
}

#[tauri::command]
#[specta::specta]
pub fn server_status(state: State<'_, AppState>) -> R<ServerStatusView> {
    let s = state.services()?;
    let c = s.rt.config();
    let tools = |p: Platform| s.rt.engine(p).registry().enabled(&c.server(p)).len() as u32;
    Ok(ServerStatusView {
        state: s.rt.server_state(),
        google_enabled: c.server(Platform::GoogleAds).enabled,
        meta_enabled: c.server(Platform::MetaAds).enabled,
        google_tools: tools(Platform::GoogleAds),
        meta_tools: tools(Platform::MetaAds),
        google_write_mode: c.server(Platform::GoogleAds).write_mode,
        meta_write_mode: c.server(Platform::MetaAds).write_mode,
    })
}

#[tauri::command]
#[specta::specta]
pub fn set_server_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    platform: Platform,
    enabled: bool,
) -> R<()> {
    state.services()?.rt.set_server_enabled(platform, enabled)?;
    emit(&app, AppEvent::ServerStatusChanged);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn set_port(app: AppHandle, state: State<'_, AppState>, port: u16) -> R<ServerState> {
    let st = state.services()?.rt.set_port(port).await?;
    emit(&app, AppEvent::ServerStatusChanged);
    Ok(st)
}

#[tauri::command]
#[specta::specta]
pub fn reveal_token(state: State<'_, AppState>) -> R<String> {
    Ok(state.services()?.rt.token())
}

#[tauri::command]
#[specta::specta]
pub fn client_snippet_preview(
    state: State<'_, AppState>,
    client: AiClient,
    platform: Platform,
) -> R<crate::core::clients::Snippet> {
    let s = state.services()?;
    let token = s.rt.token();
    let mut snip = snippet(client, platform, s.rt.config().port, &token);
    snip.text = masked(&snip, &token);
    Ok(snip)
}

#[tauri::command]
#[specta::specta]
pub fn copy_client_snippet(
    app: AppHandle,
    state: State<'_, AppState>,
    client: AiClient,
    platform: Platform,
) -> R<()> {
    let s = state.services()?;
    let text = snippet(client, platform, s.rt.config().port, &s.rt.token()).text;
    app.clipboard()
        .write_text(text)
        .map_err(|e| AppError::new("CLIPBOARD", e.to_string()))
}

// ---- Tools ----

#[tauri::command]
#[specta::specta]
pub fn get_tool_settings(state: State<'_, AppState>, platform: Platform) -> R<ToolSettingsView> {
    Ok(state.services()?.settings.tools(platform))
}

#[tauri::command]
#[specta::specta]
pub fn set_write_mode(
    app: AppHandle,
    state: State<'_, AppState>,
    platform: Platform,
    mode: WriteMode,
) -> R<()> {
    state.services()?.settings.set_write_mode(platform, mode)?;
    emit(&app, AppEvent::ServerStatusChanged);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_tool_group(
    state: State<'_, AppState>,
    platform: Platform,
    group: ToolGroup,
    enabled: bool,
) -> R<()> {
    state
        .services()?
        .settings
        .set_group(platform, group, enabled)
}

#[tauri::command]
#[specta::specta]
pub fn set_tool(
    state: State<'_, AppState>,
    platform: Platform,
    name: String,
    enabled: bool,
) -> R<()> {
    state
        .services()?
        .settings
        .set_tool(platform, &name, enabled)
}

#[tauri::command]
#[specta::specta]
pub fn set_approval_wait(state: State<'_, AppState>, platform: Platform, secs: u32) -> R<()> {
    state
        .services()?
        .settings
        .set_approval_wait(platform, secs as u64)
}

// ---- Approvals and history ----

#[tauri::command]
#[specta::specta]
pub fn list_pending(state: State<'_, AppState>) -> R<Vec<PendingView>> {
    state.services()?.changes.pending()
}

#[tauri::command]
#[specta::specta]
pub fn list_changes(state: State<'_, AppState>, filter: HistoryFilter) -> R<Vec<ChangeView>> {
    state.services()?.changes.history(filter)
}

#[tauri::command]
#[specta::specta]
pub fn get_change(state: State<'_, AppState>, id: String) -> R<ChangeView> {
    state.services()?.changes.get(&id)
}

#[tauri::command]
#[specta::specta]
pub async fn approve(state: State<'_, AppState>, id: String) -> R<ChangeView> {
    state.services()?.changes.approve(&id).await
}

#[tauri::command]
#[specta::specta]
pub async fn reject(state: State<'_, AppState>, id: String) -> R<ChangeView> {
    state.services()?.changes.reject(&id).await
}

#[tauri::command]
#[specta::specta]
pub async fn undo(state: State<'_, AppState>, id: String) -> R<crate::core::changes::UndoResult> {
    state.services()?.changes.undo(&id).await
}

// ---- App ----

#[tauri::command]
#[specta::specta]
pub fn get_app_settings(state: State<'_, AppState>) -> R<AppSettingsView> {
    let mut view = state.services()?.settings.app();
    view.demo = state.demo;
    Ok(view)
}

#[tauri::command]
#[specta::specta]
pub fn set_language(app: AppHandle, state: State<'_, AppState>, language: String) -> R<()> {
    state.services()?.settings.set_language(&language)?;
    super::tray::refresh(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_theme(state: State<'_, AppState>, theme: Theme) -> R<()> {
    state.services()?.settings.set_theme(theme)
}

#[tauri::command]
#[specta::specta]
pub fn set_autostart(app: AppHandle, state: State<'_, AppState>, enabled: bool) -> R<()> {
    let launcher = app.autolaunch();
    let r = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    r.map_err(|e| AppError::new("AUTOSTART", e.to_string()))?;
    state.services()?.settings.set_autostart(enabled)
}

#[tauri::command]
#[specta::specta]
pub fn open_data_dir(app: AppHandle, state: State<'_, AppState>) -> R<()> {
    app.opener()
        .open_path(state.data_dir.display().to_string(), None::<&str>)
        .map_err(|e| AppError::new("OPEN_FAILED", e.to_string()))
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct UpdateView {
    pub version: String,
    pub url: String,
}

/// Checks now for a new version (the app also checks once a day).
#[tauri::command]
#[specta::specta]
pub async fn check_update() -> Option<UpdateView> {
    let http = reqwest::Client::new();
    crate::core::jobs::check_update(
        &http,
        crate::core::jobs::GITHUB_API,
        crate::core::settings::REPO,
        env!("CARGO_PKG_VERSION"),
    )
    .await
    .map(|u| UpdateView {
        version: u.version,
        url: u.url,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn quit_app(
    app: AppHandle,
    state: State<'_, AppState>,
    reject_pending: bool,
) -> R<QuitOutcome> {
    let outcome = match state.services() {
        Ok(s) => prepare_quit(&s.changes, reject_pending).await?,
        Err(_) => QuitOutcome::Ready,
    };
    if outcome == QuitOutcome::Ready {
        app.exit(0);
    }
    Ok(outcome)
}

pub fn builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .error_handling(ErrorHandlingMode::Throw)
        .events(collect_events![AppEventMsg])
        .commands(collect_commands![
            app_version,
            boot_state,
            retry_keychain,
            use_file_backend,
            onboarding_state,
            list_connections,
            add_google_connection,
            add_meta_login,
            add_meta_token,
            reconnect_connection,
            remove_connection,
            set_default_connection,
            test_connection,
            cancel_login,
            google_console_links,
            server_status,
            set_server_enabled,
            set_port,
            reveal_token,
            client_snippet_preview,
            copy_client_snippet,
            get_tool_settings,
            set_write_mode,
            set_tool_group,
            set_tool,
            set_approval_wait,
            list_pending,
            list_changes,
            get_change,
            approve,
            reject,
            undo,
            get_app_settings,
            set_language,
            set_theme,
            set_autostart,
            open_data_dir,
            check_update,
            quit_app,
        ])
}
