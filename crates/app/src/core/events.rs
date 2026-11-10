//! Events the UI receives in real time, plus pure rules for the menu bar and for quitting.

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
    /// Any other state change of a change (applied, failed, rejected, expired).
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
    /// The user asked to quit with pending approvals; the UI asks for confirmation.
    QuitRequested {
        pending: u32,
    },
}

/// Merges the engines' events into a single channel, already in the UI format.
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

/// Text next to the menu bar icon: the number of pending approvals, prefixed with `!`
/// when any connection needs to reconnect.
pub fn tray_title(pending: u32, needs_reconnect: bool) -> Option<String> {
    let count = match pending {
        0 => None,
        1..=99 => Some(pending.to_string()),
        _ => Some("99+".to_string()),
    };
    match (needs_reconnect, count) {
        (true, Some(n)) => Some(format!("! {n}")),
        (true, None) => Some("!".to_string()),
        (false, n) => n,
    }
}

/// A quit requested by the system (Cmd+Q, Dock, logout: `code` is `None`) with pending
/// approvals is held back so the UI can ask for confirmation. `app.exit(code)` always goes through.
pub fn block_exit(code: Option<i32>, pending: u32) -> bool {
    code.is_none() && pending > 0
}

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuitOutcome {
    Ready,
    NeedsConfirmation { pending: u32 },
}

/// With nothing pending, it can quit. With pending approvals, it only quits with
/// `reject_pending`, which rejects them all first (the AI gets "rejected" instead of
/// being left without an answer).
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
