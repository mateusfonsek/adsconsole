//! Comandos chamados pela UI. Cada um só embrulha um método do núcleo (`crate::core`).

use tauri_specta::{Builder, ErrorHandlingMode, collect_commands};

/// Versão do app (do `Cargo.toml`).
#[tauri::command]
#[specta::specta]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .error_handling(ErrorHandlingMode::Throw)
        .commands(collect_commands![app_version])
}
