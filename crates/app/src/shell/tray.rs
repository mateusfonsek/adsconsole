//! Ícone na barra de menu (macOS) ou bandeja (Windows/Linux), com o número de
//! aprovações pendentes e o status de cada MCP.

use adsconsole_core::Platform;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, Wry};

use super::state::AppState;
use super::{quit, window};
use crate::core::events::tray_title;
use crate::core::runtime::ServerState;

const TRAY: &str = "main";

pub fn create(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let menu = build_menu(app)?;
    let mut builder = TrayIconBuilder::with_id(TRAY)
        .tooltip("AdsConsole")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| on_menu(app, event.id.as_ref()));
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    #[cfg(target_os = "macos")]
    {
        builder = builder.icon_as_template(false);
    }
    builder.build(app)
}

/// Refaz o menu e o contador (chamado a cada evento).
pub fn refresh(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY) else {
        return;
    };
    if let Ok(menu) = build_menu(app) {
        let _ = tray.set_menu(Some(menu));
    }
    let services = app.state::<AppState>().services().ok();
    let pending = services
        .as_ref()
        .and_then(|s| s.changes.pending().ok())
        .map_or(0, |p| p.len() as u32);
    let reconnect = services
        .as_ref()
        .and_then(|s| s.connections.list().ok())
        .is_some_and(|l| {
            l.iter()
                .any(|c| c.status == crate::core::connections::ConnStatusView::NeedsReconnect)
        });
    #[cfg(target_os = "macos")]
    let _ = tray.set_title(tray_title(pending, reconnect));
    let tooltip = match tray_title(pending, reconnect) {
        Some(n) => format!("AdsConsole · {n}"),
        None => "AdsConsole".to_string(),
    };
    let _ = tray.set_tooltip(Some(tooltip));
}

fn lang(app: &AppHandle) -> String {
    app.state::<AppState>()
        .services()
        .map(|s| s.settings.app().language)
        .unwrap_or_else(|_| crate::core::settings::system_language())
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let en = lang(app) == "en";
    let t = |pt: &str, en_text: &str| {
        if en {
            en_text.to_string()
        } else {
            pt.to_string()
        }
    };
    let services = app.state::<AppState>().services().ok();
    let menu = Menu::new(app)?;

    for p in Platform::ALL {
        let label = match services.as_ref() {
            None => format!("{} · {}", p.display_name(), t("iniciando", "starting")),
            Some(s) => {
                let enabled = s.rt.config().server(p).enabled;
                let status = match (s.rt.server_state(), enabled) {
                    (ServerState::Error { .. }, _) => t("erro", "error"),
                    (_, false) => t("pausado", "paused"),
                    _ => t("ligado", "running"),
                };
                format!("{} · {status}", p.display_name())
            }
        };
        menu.append(&MenuItem::with_id(
            app,
            format!("status_{}", p.as_str()),
            label,
            false,
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "open",
        t("Abrir AdsConsole", "Open AdsConsole"),
        true,
        None::<&str>,
    )?)?;
    for p in Platform::ALL {
        let enabled = services
            .as_ref()
            .is_some_and(|s| s.rt.config().server(p).enabled);
        let text = if enabled {
            t(
                &format!("Pausar {}", p.display_name()),
                &format!("Pause {}", p.display_name()),
            )
        } else {
            t(
                &format!("Retomar {}", p.display_name()),
                &format!("Resume {}", p.display_name()),
            )
        };
        menu.append(&MenuItem::with_id(
            app,
            format!("toggle_{}", p.as_str()),
            text,
            services.is_some(),
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        t("Sair", "Quit"),
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "open" => window::show(app),
        "quit" => quit::request(app),
        other => {
            let Some(p) = other
                .strip_prefix("toggle_")
                .and_then(|name| Platform::ALL.into_iter().find(|p| p.as_str() == name))
            else {
                return;
            };
            if let Ok(s) = app.state::<AppState>().services() {
                let enabled = s.rt.config().server(p).enabled;
                if let Err(e) = s.rt.set_server_enabled(p, !enabled) {
                    tracing::warn!(error = %e, "falha ao pausar/retomar pela barra de menu");
                }
                super::emit(app, crate::core::events::AppEvent::ServerStatusChanged);
            }
        }
    }
}
