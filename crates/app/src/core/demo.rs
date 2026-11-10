//! Development demo mode (`--demo`): fake adapters, in-memory secrets and a temporary
//! data folder. Nothing leaves the computer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::config::Config;
use adsconsole_core::db::ConnectionMeta;
use adsconsole_core::paths::EnvOverrides;
use adsconsole_core::secrets::{MemoryStore, SecretBackend, SecretStore};
use adsconsole_mcp::fake::FakePlatform;
use async_trait::async_trait;
use serde_json::json;

use super::connections::{
    MetaLoginRequest, MetaTokenRequest, OpenFn, PlatformConnections, ResolvedGoogleAdd,
};
use super::error::AppError;
use super::runtime::{Adapters, BootIssue, BootOptions, Runtime};
use super::services::Services;

pub struct DemoOptions {
    pub port: u16,
    /// When the sample approval appears after boot.
    pub pending_after: Duration,
}

impl Default for DemoOptions {
    fn default() -> Self {
        DemoOptions {
            port: 7459,
            pending_after: Duration::from_secs(2),
        }
    }
}

/// Temporary folder for the demo (stays on disk until the system cleans the temp folder).
pub fn temp_dir() -> std::io::Result<PathBuf> {
    Ok(tempfile::Builder::new()
        .prefix("adsconsole-demo-")
        .tempdir()?
        .keep())
}

/// In the demo the connections are samples: adding or reconnecting is refused.
struct DemoConnections;

fn refused() -> AppError {
    AppError::new(
        "DEMO_MODE",
        "Unavailable in demo mode. Open AdsConsole without --demo to connect real accounts.",
    )
}

#[async_trait]
impl PlatformConnections for DemoConnections {
    async fn add_google(
        &self,
        _: ResolvedGoogleAdd,
        _: OpenFn,
    ) -> Result<ConnectionMeta, AppError> {
        Err(refused())
    }
    async fn add_meta_login(
        &self,
        _: MetaLoginRequest,
        _: OpenFn,
    ) -> Result<ConnectionMeta, AppError> {
        Err(refused())
    }
    async fn add_meta_token(&self, _: MetaTokenRequest) -> Result<ConnectionMeta, AppError> {
        Err(refused())
    }
    async fn reconnect(&self, _: &ConnectionMeta, _: OpenFn) -> Result<ConnectionMeta, AppError> {
        Err(refused())
    }
    fn remove(&self, _: &ConnectionMeta) -> Result<(), AppError> {
        Err(refused())
    }
}

pub async fn boot(data_dir: PathBuf, opts: DemoOptions) -> Result<Services, BootIssue> {
    let config_path = data_dir.join("config.json");
    if !config_path.exists() {
        let config = Config {
            port: opts.port,
            ..Config::default()
        };
        std::fs::create_dir_all(&data_dir)
            .and_then(|_| config.save(&config_path).map_err(std::io::Error::other))
            .map_err(|e| BootIssue::Fatal {
                message: e.to_string(),
            })?;
    }
    let google = FakePlatform::new(Platform::GoogleAds);
    let meta = FakePlatform::new(Platform::MetaAds);
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
    let g = google.clone();
    let boot = BootOptions {
        data_dir,
        env: EnvOverrides::default(),
        open_secrets: Box::new(move |_| Ok((store.clone(), SecretBackend::File))),
        make_adapters: Box::new(move |db, _| {
            // Both fakes use the same connection ids; only one of them fits in the database.
            if let Err(e) = g.seed_connections(&db) {
                tracing::warn!(error = %e, "failed to seed the demo");
            }
            Adapters {
                google: g.clone(),
                meta: meta.clone(),
            }
        }),
    };
    let rt = Arc::new(Runtime::boot(boot).await?);
    if let Err(e) = rt.engine(Platform::GoogleAds).refresh_accounts().await {
        tracing::warn!(error = %e.message, "failed to load the demo accounts");
    }
    let engine = rt.engine(Platform::GoogleAds).clone();
    tokio::spawn(async move {
        tokio::time::sleep(opts.pending_after).await;
        let input = json!({"account_id": "111", "name": "Black Friday", "daily_budget": 80});
        let _ = engine
            .call_tool("create_campaign", input, Some("demo".into()))
            .await;
    });
    Ok(Services::new(rt, Arc::new(DemoConnections)))
}
