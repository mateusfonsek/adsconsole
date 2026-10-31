//! Agenda as rotinas de fundo do núcleo (`core::jobs`) e avisa a UI e o sistema.

use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;

use super::{emit, notify};
use crate::core::connections::ConnStatusView;
use crate::core::events::AppEvent;
use crate::core::jobs::{
    GITHUB_API, check_update, first_call_tick, newly_needing_reconnect, refresh_all,
};
use crate::core::services::Services;
use crate::core::settings::REPO;

const FIRST_CALL_EVERY: Duration = Duration::from_secs(5);
const CONNECTIONS_EVERY: Duration = Duration::from_secs(60);
const REFRESH_EVERY: Duration = Duration::from_secs(6 * 3600);
const DAILY: Duration = Duration::from_secs(24 * 3600);
const FIRST_DAILY_AFTER: Duration = Duration::from_secs(30);

pub fn spawn(app: AppHandle, s: Arc<Services>) {
    let (a, svc) = (app.clone(), s.clone());
    tauri::async_runtime::spawn(async move {
        while !svc.rt.config().client_seen {
            tokio::time::sleep(FIRST_CALL_EVERY).await;
            if matches!(first_call_tick(&svc.rt).await, Ok(true)) {
                emit(&a, AppEvent::FirstCallReceived);
            }
        }
    });

    let (a, svc) = (app.clone(), s.clone());
    tauri::async_runtime::spawn(async move {
        let mut before = svc.connections.list().unwrap_or_default();
        loop {
            tokio::time::sleep(CONNECTIONS_EVERY).await;
            let Ok(after) = svc.connections.list() else {
                continue;
            };
            let lang = svc.settings.app().language;
            for name in newly_needing_reconnect(&before, &after) {
                notify::show(&a, &lang, notify::Notice::Reconnect { name: &name });
            }
            let changed = before.len() != after.len()
                || before
                    .iter()
                    .zip(&after)
                    .any(|(b, c)| b.id != c.id || b.status != c.status);
            if changed {
                emit(&a, AppEvent::ConnectionStatusChanged);
            }
            before = after;
        }
    });

    let svc = s.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH_EVERY).await;
            refresh_all(svc.rt.engines()).await;
        }
    });

    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_DAILY_AFTER).await;
        let http = reqwest::Client::new();
        loop {
            if let Some(u) = check_update(&http, GITHUB_API, REPO, env!("CARGO_PKG_VERSION")).await
            {
                emit(
                    &app,
                    AppEvent::UpdateAvailable {
                        version: u.version,
                        url: u.url,
                    },
                );
            }
            let lang = s.settings.app().language;
            let soonest = s
                .connections
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| match c.status {
                    ConnStatusView::ExpiringSoon { days } => Some((days, c.name)),
                    _ => None,
                })
                .min_by_key(|(d, _)| *d);
            if let Some((days, name)) = soonest {
                notify::show(&app, &lang, notify::Notice::Expiring { name: &name, days });
            }
            tokio::time::sleep(DAILY).await;
        }
    });
}
