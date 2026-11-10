//! The app services built on a `Runtime`, and the boot with the real adapters.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use adsconsole_core::paths::EnvOverrides;
use adsconsole_core::secrets::{keychain_available, open_secret_store_with};
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_mcp::google::GoogleAdapter;
use adsconsole_mcp::meta::MetaAdapter;
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use serde::Serialize;

use super::changes::ChangesService;
use super::connections::{ConnectionsService, PlatformConnections, RealConnections};
use super::error::AppError;
use super::runtime::{Adapters, BootIssue, BootOptions, Runtime};
use super::settings::SettingsService;

pub struct Services {
    pub rt: Arc<Runtime>,
    pub connections: ConnectionsService,
    pub changes: ChangesService,
    pub settings: SettingsService,
}

impl Services {
    pub fn new(rt: Arc<Runtime>, ops: Arc<dyn PlatformConnections>) -> Self {
        Services {
            connections: ConnectionsService::new(rt.clone(), ops),
            changes: ChangesService::new(rt.clone()),
            settings: SettingsService::new(rt.clone()),
            rt,
        }
    }
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct BootStateView {
    /// A problem that stops the app from working (today only the keychain).
    pub issue: Option<BootIssue>,
    /// Warnings that do not stop it (config reset).
    pub notices: Vec<BootIssue>,
}

/// Data folder: `ADSCONSOLE_DATA_DIR` or the system default.
pub fn data_dir(env: &EnvOverrides) -> Result<PathBuf, AppError> {
    adsconsole_core::paths::data_dir(env).map_err(AppError::from)
}

/// Boot with the real keychain, Google and Meta.
pub async fn boot_real(data_dir: PathBuf, env: EnvOverrides) -> Result<Services, BootIssue> {
    let dir = data_dir.clone();
    let real: Arc<Mutex<Option<RealConnections>>> = Arc::new(Mutex::new(None));
    let slot = real.clone();
    let opts = BootOptions {
        data_dir,
        env,
        open_secrets: Box::new(move |remembered| {
            open_secret_store_with(&dir, remembered, &keychain_available)
        }),
        make_adapters: Box::new(move |db, secrets| {
            let google =
                GoogleAdapter::new(db.clone(), secrets.clone(), GoogleEndpoints::default());
            let meta = MetaAdapter::new(db, secrets, MetaEndpoints::default());
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(RealConnections {
                google: google.clone(),
                meta: meta.clone(),
            });
            Adapters { google, meta }
        }),
    };
    let rt = Arc::new(Runtime::boot(opts).await?);
    let ops = real
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .ok_or_else(|| BootIssue::Fatal {
            message: "adapters were not created".into(),
        })?;
    Ok(Services::new(rt, Arc::new(ops)))
}
