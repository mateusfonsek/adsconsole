//! Live app state: configuration, database, secrets, one `Engine` per platform and the
//! MCP server. The UI and the server use the same engines.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::Config;
use adsconsole_core::db::Db;
use adsconsole_core::paths::EnvOverrides;
use adsconsole_core::secrets::{SecretBackend, SecretStore, SecretStoreError, ensure_mcp_token};
use adsconsole_mcp::adapter::PlatformAdapter;
use adsconsole_mcp::engine::{Engine, SharedConfig};
use adsconsole_mcp::http::{RunningServer, ServerOptions, start};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::error::AppError;

pub type OpenSecrets = Box<
    dyn Fn(Option<SecretBackend>) -> Result<(Arc<dyn SecretStore>, SecretBackend), SecretStoreError>
        + Send
        + Sync,
>;
pub type MakeAdapters = Box<dyn Fn(Arc<Db>, Arc<dyn SecretStore>) -> Adapters + Send + Sync>;

pub struct Adapters {
    pub google: Arc<dyn PlatformAdapter>,
    pub meta: Arc<dyn PlatformAdapter>,
}

/// What boot needs from outside. Secrets and adapters are injected so tests (and demo
/// mode) never touch the keychain or the real APIs.
pub struct BootOptions {
    pub data_dir: PathBuf,
    pub env: EnvOverrides,
    pub open_secrets: OpenSecrets,
    pub make_adapters: MakeAdapters,
}

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BootIssue {
    /// The keychain chosen earlier did not respond: the app does not start without a user decision.
    KeychainUnavailable,
    /// `config.json` was invalid and was reset to the defaults (a warning; the app starts).
    ConfigReset { message: String },
    /// Unexpected error opening the data folder or the database.
    Fatal { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerState {
    Running { port: u16 },
    Error { port: u16, error: AppError },
}

pub struct Runtime {
    data_dir: PathBuf,
    config_path: PathBuf,
    config: SharedConfig,
    db: Arc<Db>,
    google: Engine,
    meta: Engine,
    token: String,
    notices: Vec<BootIssue>,
    server: tokio::sync::Mutex<Option<RunningServer>>,
    state: RwLock<ServerState>,
}

impl Runtime {
    pub async fn boot(opts: BootOptions) -> Result<Runtime, BootIssue> {
        let fatal = |e: &dyn std::fmt::Display| BootIssue::Fatal {
            message: e.to_string(),
        };
        std::fs::create_dir_all(&opts.data_dir).map_err(|e| fatal(&e))?;
        let config_path = opts.data_dir.join("config.json");
        let loaded = Config::load(&config_path).map_err(|e| fatal(&e))?;
        let mut notices = Vec::new();
        if let Some(message) = loaded.warning {
            notices.push(BootIssue::ConfigReset { message });
        }
        let mut config = loaded.config;

        let (secrets, backend) =
            (opts.open_secrets)(config.secret_backend).map_err(|e| match e {
                SecretStoreError::KeychainUnavailable => BootIssue::KeychainUnavailable,
            })?;
        if config.secret_backend != Some(backend) {
            config.secret_backend = Some(backend);
            config.save(&config_path).map_err(|e| fatal(&e))?;
        }

        let db = Arc::new(Db::open(&opts.data_dir.join("adsconsole.db")).map_err(|e| fatal(&e))?);
        let token =
            ensure_mcp_token(secrets.as_ref(), opts.env.token.as_deref()).map_err(|e| fatal(&e))?;
        let adapters = (opts.make_adapters)(db.clone(), secrets);
        let port = config.effective_port(&opts.env);
        let config: SharedConfig = Arc::new(RwLock::new(config));
        let google = Engine::new(adapters.google, db.clone(), config.clone())
            .map_err(|e| fatal(&e.message))?;
        let meta = Engine::new(adapters.meta, db.clone(), config.clone())
            .map_err(|e| fatal(&e.message))?;

        let rt = Runtime {
            data_dir: opts.data_dir,
            config_path,
            config,
            db,
            google,
            meta,
            token,
            notices,
            server: tokio::sync::Mutex::new(None),
            state: RwLock::new(ServerState::Running { port }),
        };
        let state = rt.start_server(port).await;
        rt.set_state(state);
        Ok(rt)
    }

    /// Writes `secret_backend: file` to the folder's config so the next boot uses the file.
    pub fn use_file_backend(data_dir: &Path) -> Result<(), AppError> {
        let path = data_dir.join("config.json");
        let mut config = Config::load(&path)?.config;
        config.secret_backend = Some(SecretBackend::File);
        config.save(&path)?;
        Ok(())
    }

    pub fn engine(&self, p: Platform) -> &Engine {
        match p {
            Platform::GoogleAds => &self.google,
            Platform::MetaAds => &self.meta,
        }
    }

    pub fn engines(&self) -> [&Engine; 2] {
        [&self.google, &self.meta]
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Changes the shared configuration (takes effect immediately for the server) and saves it.
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) -> Result<Config, AppError> {
        let mut guard = self.config.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        f(&mut next);
        next.save(&self.config_path)?;
        *guard = next.clone();
        Ok(next)
    }

    pub fn notices(&self) -> &[BootIssue] {
        &self.notices
    }

    pub fn token(&self) -> String {
        self.token.clone()
    }

    pub fn server_state(&self) -> ServerState {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub async fn last_request_at(&self) -> Option<DateTime<Utc>> {
        self.server
            .lock()
            .await
            .as_ref()
            .and_then(|s| s.last_request_at())
    }

    pub fn set_server_enabled(&self, p: Platform, enabled: bool) -> Result<(), AppError> {
        self.update_config(|c| {
            c.servers.entry(p).or_default().enabled = enabled;
        })?;
        Ok(())
    }

    /// Stops the server and starts it on the new port. If the new port fails, the saved port
    /// stays the old one and the server tries to come back on it.
    pub async fn set_port(&self, port: u16) -> Result<ServerState, AppError> {
        if port == 0 {
            return Err(
                AppError::new("INVALID_PORT", "Use a port from 1 to 65535.").with_field("port")
            );
        }
        let state = self.start_server(port).await;
        if let ServerState::Running { .. } = state {
            self.update_config(|c| c.port = port)?;
            self.set_state(state.clone());
            return Ok(state);
        }
        let old = self.config().port;
        let restored = self.start_server(old).await;
        self.set_state(restored);
        Ok(state)
    }

    async fn start_server(&self, port: u16) -> ServerState {
        let mut slot = self.server.lock().await;
        if let Some(running) = slot.take() {
            running.shutdown().await;
        }
        let opts = ServerOptions {
            port,
            token: self.token.clone(),
            allow_ephemeral_port: false,
        };
        match start(
            vec![self.google.clone(), self.meta.clone()],
            self.config.clone(),
            opts,
        )
        .await
        {
            Ok(server) => {
                *slot = Some(server);
                ServerState::Running { port }
            }
            Err(e) => ServerState::Error {
                port,
                error: e.into(),
            },
        }
    }

    fn set_state(&self, s: ServerState) {
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = s;
    }
}
