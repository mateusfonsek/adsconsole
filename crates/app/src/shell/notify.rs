//! Notificações do sistema, no idioma do app.

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

pub enum Notice<'a> {
    Approval { summary: &'a str },
}

pub fn show(app: &AppHandle, lang: &str, notice: Notice) {
    let en = lang == "en";
    let (title, body) = match notice {
        Notice::Approval { summary } => (
            if en {
                "Approval needed"
            } else {
                "Aprovação pendente"
            },
            summary.to_string(),
        ),
    };
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!(error = %e, "falha ao mostrar a notificação");
    }
}
