//! Liga o núcleo ao Tauri: plugins, janela, barra de menu, eventos e comandos.

pub mod commands;
mod jobs;
mod notify;
pub mod quit;
pub mod state;
mod tray;
mod window;

use std::sync::Arc;

use adsconsole_core::paths::EnvOverrides;
use tauri::{AppHandle, Manager};
use tauri_specta::{Builder, Event};

use crate::core::events::{AppEvent, bridge};
use crate::core::services::{BootStateView, Services, boot_real, data_dir};
use state::AppState;

/// Envelope tipado dos eventos do app para a UI.
#[derive(Debug, Clone, serde::Serialize, specta::Type, tauri_specta::Event)]
pub struct AppEventMsg(pub AppEvent);

pub fn emit(app: &AppHandle, ev: AppEvent) {
    if let Err(e) = AppEventMsg(ev).emit(app) {
        tracing::warn!(error = %e, "falha ao enviar evento à UI");
    }
    tray::refresh(app);
}

pub fn run(builder: Builder<tauri::Wry>, demo: bool) {
    let env = match EnvOverrides::from_env() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let result = tauri::Builder::default()
        // A instância única vem antes de tudo: uma segunda cópia só traz a primeira à frente.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            window::show(app)
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            let dir = if demo {
                crate::core::demo::temp_dir()?
            } else {
                data_dir(&env)?
            };
            app.manage(AppState::new(env, dir, demo));
            if let Some(w) = app.get_webview_window(window::MAIN) {
                window::keep_alive_on_close(&w);
            }
            tray::create(app.handle())?;
            start(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!());
    match result {
        Ok(app) => app.run(|app, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                let pending = app
                    .state::<AppState>()
                    .services()
                    .ok()
                    .and_then(|s| s.changes.pending().ok())
                    .map_or(0, |p| p.len() as u32);
                if crate::core::events::block_exit(code, pending) {
                    api.prevent_exit();
                    quit::request(app);
                }
            }
        }),
        Err(e) => {
            eprintln!("o AdsConsole parou com erro: {e}");
            std::process::exit(1);
        }
    }
}

/// Faz o boot (de novo, depois de uma decisão sobre o keychain) e liga os eventos.
pub fn start(app: &AppHandle) {
    let state = app.state::<AppState>();
    let booted = tauri::async_runtime::block_on(async {
        if state.demo {
            crate::core::demo::boot(state.data_dir.clone(), Default::default()).await
        } else {
            boot_real(state.data_dir.clone(), state.env.clone()).await
        }
    });
    match booted {
        Ok(services) => {
            let services = Arc::new(services);
            state.set_boot_state(BootStateView {
                issue: None,
                notices: services.rt.notices().to_vec(),
            });
            state.set_services(Some(services.clone()));
            spawn_event_loop(app.clone(), services.clone());
            jobs::spawn(app.clone(), services);
        }
        Err(issue) => {
            state.set_services(None);
            state.set_boot_state(BootStateView {
                issue: Some(issue),
                notices: Vec::new(),
            });
        }
    }
    tray::refresh(app);
}

fn spawn_event_loop(app: AppHandle, services: Arc<Services>) {
    let mut rx = {
        let _guard = tauri::async_runtime::handle().inner().enter();
        bridge(services.rt.engines())
    };
    let lang = move |s: &Services| s.settings.app().language;
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let AppEvent::ApprovalRequested {
                summary, change_id, ..
            } = &ev
            {
                // O desfazer pedido no próprio app é aprovado na hora: não notifica.
                let from_app = services
                    .rt
                    .db()
                    .get_change(change_id)
                    .ok()
                    .flatten()
                    .is_some_and(|c| c.requested_by.as_deref() == Some(crate::core::changes::APP));
                if !from_app {
                    notify::show(&app, &lang(&services), notify::Notice::Approval { summary });
                }
            }
            emit(&app, ev);
        }
    });
}
