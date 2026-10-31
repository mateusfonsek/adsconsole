//! Eventos que a UI recebe em tempo real e regras puras da barra de menu e da saída.

use adsconsole_core::Platform;
use adsconsole_mcp::engine::{Engine, EngineEvent};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use super::changes::ChangesService;
use super::error::AppError;

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppEvent {
    ApprovalRequested {
        change_id: String,
        platform: Platform,
        summary: String,
    },
    /// Qualquer outra mudança de estado de uma mudança (aplicada, falhou, recusada, expirou).
    ChangeUpdated {
        change_id: String,
    },
    ServerStatusChanged,
    ConnectionStatusChanged,
    UpdateAvailable {
        version: String,
        url: String,
    },
    FirstCallReceived,
    /// O usuário pediu para sair com aprovações pendentes; a UI pede confirmação.
    QuitRequested {
        pending: u32,
    },
}

/// Junta os eventos dos engines num canal só, já no formato da UI.
pub fn bridge(engines: [&Engine; 2]) -> mpsc::Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel(256);
    for engine in engines {
        let mut sub = engine.subscribe();
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                let ev = match sub.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if tx.send(map(ev)).await.is_err() {
                    return;
                }
            }
        });
    }
    rx
}

fn map(ev: EngineEvent) -> AppEvent {
    match ev {
        EngineEvent::ApprovalRequested {
            change_id,
            platform,
            summary,
            ..
        } => AppEvent::ApprovalRequested {
            change_id,
            platform,
            summary,
        },
        EngineEvent::ChangeApplied { change_id }
        | EngineEvent::ChangeFailed { change_id }
        | EngineEvent::ChangeRejected { change_id }
        | EngineEvent::ChangeExpired { change_id } => AppEvent::ChangeUpdated { change_id },
    }
}

/// Texto ao lado do ícone da barra de menu: o número de aprovações pendentes.
pub fn tray_title(pending: u32) -> Option<String> {
    match pending {
        0 => None,
        1..=99 => Some(pending.to_string()),
        _ => Some("99+".to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuitOutcome {
    Ready,
    NeedsConfirmation { pending: u32 },
}

/// Sem pendências, pode sair. Com pendências, só sai com `reject_pending`, que recusa
/// todas antes (a IA recebe "recusado" em vez de ficar sem resposta).
pub async fn prepare_quit(
    changes: &ChangesService,
    reject_pending: bool,
) -> Result<QuitOutcome, AppError> {
    let pending = changes.pending()?.len() as u32;
    if pending == 0 {
        return Ok(QuitOutcome::Ready);
    }
    if !reject_pending {
        return Ok(QuitOutcome::NeedsConfirmation { pending });
    }
    changes.reject_all_pending().await?;
    Ok(QuitOutcome::Ready)
}
