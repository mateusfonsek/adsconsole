//! Notificações do sistema, no idioma do app.

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

pub enum Notice<'a> {
    Approval { summary: &'a str },
    Reconnect { name: &'a str },
    Expiring { name: &'a str, days: i32 },
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
        Notice::Reconnect { name } => (
            if en {
                "Reconnect needed"
            } else {
                "Reconecte a conexão"
            },
            if en {
                format!("{name} lost access. Open AdsConsole → Connections.")
            } else {
                format!("{name} perdeu o acesso. Abra AdsConsole → Conexões.")
            },
        ),
        Notice::Expiring { name, days } => (
            if en {
                "Access expiring"
            } else {
                "Acesso vencendo"
            },
            if en {
                format!("{name} expires in {days} day(s). Reconnect in AdsConsole → Connections.")
            } else {
                format!("{name} vence em {days} dia(s). Reconecte em AdsConsole → Conexões.")
            },
        ),
    };
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!(error = %e, "falha ao mostrar a notificação");
    }
}
