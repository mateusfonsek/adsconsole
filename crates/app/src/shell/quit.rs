//! Sair do app: com aprovações pendentes, a UI pede confirmação antes.

use tauri::{AppHandle, Manager};

use super::state::AppState;
use super::{emit, window};
use crate::core::events::{AppEvent, QuitOutcome, prepare_quit};

/// Pedido de saída vindo da barra de menu.
pub fn request(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Ok(services) = state.services() else {
        app.exit(0);
        return;
    };
    match tauri::async_runtime::block_on(prepare_quit(&services.changes, false)) {
        Ok(QuitOutcome::NeedsConfirmation { pending }) => {
            window::show(app);
            emit(app, AppEvent::QuitRequested { pending });
        }
        _ => app.exit(0),
    }
}
