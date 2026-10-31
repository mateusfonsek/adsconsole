//! Conexões com Google Ads e Meta Ads: listar, adicionar pelos assistentes, reconectar,
//! remover e tornar padrão. As chamadas reais ficam atrás de `PlatformConnections`
//! para os testes e o modo demo não abrirem navegador nem chamarem as APIs.

use std::path::Path;
use std::sync::{Arc, Mutex};

use adsconsole_core::Platform;
use adsconsole_core::db::{ConnectionMeta, ConnectionStatus};
use adsconsole_core::secrets::Secret;
use adsconsole_google_ads::client_json::ClientCreds;
use adsconsole_google_ads::endpoints::console_links;
use adsconsole_mcp::google::GoogleAdapter;
use adsconsole_mcp::google::connections::{
    AccessLevel, AddGoogleConnection, add_google_connection, reconnect_google_connection,
    remove_google_connection,
};
use adsconsole_mcp::meta::MetaAdapter;
use adsconsole_mcp::meta::connections::{
    AddMetaLogin, AddMetaToken, EXPIRY_WARNING_DAYS, add_meta_login_connection,
    add_meta_token_connection, reconnect_meta_connection, remove_meta_connection,
};
use adsconsole_meta_ads::oauth::AppCreds;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::error::AppError;
use super::runtime::Runtime;

/// Abre uma URL no navegador do sistema.
pub type OpenFn = Arc<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;

/// Maior JSON de client aceito (o do Google tem menos de 1 KB).
const MAX_CLIENT_JSON: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnStatusView {
    Ok,
    ExpiringSoon { days: i64 },
    NeedsReconnect,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ConnectionView {
    pub id: String,
    pub platform: Platform,
    pub name: String,
    pub is_default: bool,
    pub status: ConnStatusView,
    pub expires_at: Option<DateTime<Utc>>,
    pub accounts: u32,
    /// Google: nível de acesso; Meta: tipo de token.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum AccessLevelView {
    Test,
    Explorer,
    Basic,
    Standard,
}

impl From<AccessLevelView> for AccessLevel {
    fn from(v: AccessLevelView) -> Self {
        match v {
            AccessLevelView::Test => AccessLevel::Test,
            AccessLevelView::Explorer => AccessLevel::Explorer,
            AccessLevelView::Basic => AccessLevel::Basic,
            AccessLevelView::Standard => AccessLevel::Standard,
        }
    }
}

#[derive(Debug, Clone, Deserialize, specta::Type)]
pub struct GoogleAddRequest {
    pub name: String,
    pub client_json_path: String,
    pub login_customer_id: Option<String>,
    pub access_level: AccessLevelView,
}

/// `GoogleAddRequest` com o JSON já lido e validado.
pub struct ResolvedGoogleAdd {
    pub name: String,
    pub client_json: Vec<u8>,
    pub login_customer_id: Option<String>,
    pub access_level: AccessLevelView,
}

#[derive(Clone, Deserialize, specta::Type)]
pub struct MetaLoginRequest {
    pub name: String,
    pub app_id: String,
    pub config_id: String,
    pub app_secret: String,
    pub relay_url: Option<String>,
}

#[derive(Clone, Deserialize, specta::Type)]
pub struct MetaTokenRequest {
    pub name: String,
    pub access_token: String,
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ConsoleLinksView {
    pub api: String,
    pub consent: String,
    pub clients: String,
}

#[async_trait]
pub trait PlatformConnections: Send + Sync {
    async fn add_google(
        &self,
        req: ResolvedGoogleAdd,
        open: OpenFn,
    ) -> Result<ConnectionMeta, AppError>;
    async fn add_meta_login(
        &self,
        req: MetaLoginRequest,
        open: OpenFn,
    ) -> Result<ConnectionMeta, AppError>;
    async fn add_meta_token(&self, req: MetaTokenRequest) -> Result<ConnectionMeta, AppError>;
    async fn reconnect(
        &self,
        meta: &ConnectionMeta,
        open: OpenFn,
    ) -> Result<ConnectionMeta, AppError>;
    fn remove(&self, meta: &ConnectionMeta) -> Result<(), AppError>;
}

/// As chamadas de verdade, sobre os adaptadores do `mcp`.
pub struct RealConnections {
    pub google: Arc<GoogleAdapter>,
    pub meta: Arc<MetaAdapter>,
}

fn opener(open: &OpenFn) -> impl Fn(&str) -> std::io::Result<()> + Send + Sync + use<> {
    let open = open.clone();
    move |url: &str| open(url)
}

#[async_trait]
impl PlatformConnections for RealConnections {
    async fn add_google(
        &self,
        req: ResolvedGoogleAdd,
        open: OpenFn,
    ) -> Result<ConnectionMeta, AppError> {
        let req = AddGoogleConnection {
            name: req.name,
            client_json: req.client_json,
            login_customer_id: req.login_customer_id,
            access_level: req.access_level.into(),
        };
        Ok(add_google_connection(&self.google, req, &opener(&open)).await?)
    }

    async fn add_meta_login(
        &self,
        req: MetaLoginRequest,
        open: OpenFn,
    ) -> Result<ConnectionMeta, AppError> {
        let req = AddMetaLogin {
            name: req.name,
            app: AppCreds {
                app_id: req.app_id,
                app_secret: Secret::new(req.app_secret),
                config_id: req.config_id,
                relay_url: req.relay_url.filter(|u| !u.trim().is_empty()),
            },
            port: 0,
        };
        Ok(add_meta_login_connection(&self.meta, req, &opener(&open)).await?)
    }

    async fn add_meta_token(&self, req: MetaTokenRequest) -> Result<ConnectionMeta, AppError> {
        let req = AddMetaToken {
            name: req.name,
            access_token: Secret::new(req.access_token),
            app_id: req.app_id.filter(|s| !s.trim().is_empty()),
            app_secret: req.app_secret.filter(|s| !s.is_empty()).map(Secret::new),
        };
        Ok(add_meta_token_connection(&self.meta, req).await?)
    }

    async fn reconnect(
        &self,
        meta: &ConnectionMeta,
        open: OpenFn,
    ) -> Result<ConnectionMeta, AppError> {
        let open = opener(&open);
        Ok(match meta.platform {
            Platform::GoogleAds => {
                reconnect_google_connection(&self.google, &meta.connection_id, &open).await?
            }
            Platform::MetaAds => {
                reconnect_meta_connection(&self.meta, &meta.connection_id, 0, &open).await?
            }
        })
    }

    fn remove(&self, meta: &ConnectionMeta) -> Result<(), AppError> {
        match meta.platform {
            Platform::GoogleAds => remove_google_connection(&self.google, &meta.connection_id)?,
            Platform::MetaAds => remove_meta_connection(&self.meta, &meta.connection_id)?,
        }
        Ok(())
    }
}

pub struct ConnectionsService {
    rt: Arc<Runtime>,
    ops: Arc<dyn PlatformConnections>,
    login: Mutex<Option<CancellationToken>>,
}

impl ConnectionsService {
    pub fn new(rt: Arc<Runtime>, ops: Arc<dyn PlatformConnections>) -> Self {
        ConnectionsService {
            rt,
            ops,
            login: Mutex::new(None),
        }
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.rt
    }

    pub fn list(&self) -> Result<Vec<ConnectionView>, AppError> {
        let now = Utc::now();
        let mut out = Vec::new();
        for p in [Platform::GoogleAds, Platform::MetaAds] {
            let mut conns = self.rt.db().list_connections(p)?;
            conns.sort_by_key(|c| c.name.to_lowercase());
            for c in conns {
                let accounts = self.rt.db().list_accounts(p, Some(&c.connection_id))?.len();
                out.push(view(c, accounts as u32, now));
            }
        }
        Ok(out)
    }

    pub fn set_default(&self, id: &str) -> Result<(), AppError> {
        let meta = self.get(id)?;
        self.rt.db().set_default_connection(meta.platform, id)?;
        Ok(())
    }

    /// Links do Console do projeto do JSON (para o assistente, antes de conectar).
    pub fn google_console_links(
        &self,
        client_json_path: &str,
    ) -> Result<ConsoleLinksView, AppError> {
        let creds = read_client_json(client_json_path)?.1;
        let l = console_links(&creds.project_id);
        Ok(ConsoleLinksView {
            api: l.api,
            consent: l.consent,
            clients: l.clients,
        })
    }

    pub async fn add_google(
        &self,
        req: GoogleAddRequest,
        open: OpenFn,
    ) -> Result<ConnectionView, AppError> {
        let (client_json, _) = read_client_json(&req.client_json_path)?;
        let resolved = ResolvedGoogleAdd {
            name: req.name,
            client_json,
            login_customer_id: req.login_customer_id.filter(|s| !s.trim().is_empty()),
            access_level: req.access_level,
        };
        let meta = self
            .cancellable(self.ops.add_google(resolved, open))
            .await?;
        self.after_change(meta).await
    }

    pub async fn add_meta_login(
        &self,
        req: MetaLoginRequest,
        open: OpenFn,
    ) -> Result<ConnectionView, AppError> {
        let meta = self.cancellable(self.ops.add_meta_login(req, open)).await?;
        self.after_change(meta).await
    }

    pub async fn add_meta_token(&self, req: MetaTokenRequest) -> Result<ConnectionView, AppError> {
        let meta = self.ops.add_meta_token(req).await?;
        self.after_change(meta).await
    }

    pub async fn reconnect(&self, id: &str, open: OpenFn) -> Result<ConnectionView, AppError> {
        let current = self.get(id)?;
        let meta = self.cancellable(self.ops.reconnect(&current, open)).await?;
        self.after_change(meta).await
    }

    pub fn remove(&self, id: &str) -> Result<(), AppError> {
        let meta = self.get(id)?;
        self.ops.remove(&meta)
    }

    /// Atualiza as contas de todas as conexões da plataforma e devolve quantas a
    /// conexão `id` enxerga.
    pub async fn test(&self, id: &str) -> Result<u32, AppError> {
        let meta = self.get(id)?;
        self.rt.engine(meta.platform).refresh_accounts().await?;
        Ok(self.rt.db().list_accounts(meta.platform, Some(id))?.len() as u32)
    }

    /// Cancela o login no navegador em andamento, se houver.
    pub fn cancel_login(&self) {
        if let Some(t) = self.login.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.cancel();
        }
    }

    async fn cancellable<T>(
        &self,
        fut: impl Future<Output = Result<T, AppError>>,
    ) -> Result<T, AppError> {
        let token = CancellationToken::new();
        if let Some(old) = self
            .login
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(token.clone())
        {
            old.cancel();
        }
        tokio::select! {
            r = fut => r,
            _ = token.cancelled() => Err(AppError::new("LOGIN_CANCELLED", "Login cancelado.")),
        }
    }

    async fn after_change(&self, meta: ConnectionMeta) -> Result<ConnectionView, AppError> {
        let accounts = self
            .rt
            .db()
            .list_accounts(meta.platform, Some(&meta.connection_id))?
            .len();
        Ok(view(meta, accounts as u32, Utc::now()))
    }

    fn get(&self, id: &str) -> Result<ConnectionMeta, AppError> {
        self.rt.db().get_connection(id)?.ok_or_else(|| {
            AppError::new(
                "UNKNOWN_CONNECTION",
                format!("Conexão {id} não encontrada."),
            )
        })
    }
}

fn read_client_json(path: &str) -> Result<(Vec<u8>, ClientCreds), AppError> {
    let field = |e: &dyn std::fmt::Display| {
        AppError::new("INVALID_INPUT", e.to_string()).with_field("client_json")
    };
    let path = Path::new(path);
    let len = std::fs::metadata(path).map_err(|e| field(&e))?.len();
    if len > MAX_CLIENT_JSON {
        return Err(field(
            &"Arquivo grande demais para ser o JSON de um client OAuth.",
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| field(&e))?;
    let creds = ClientCreds::from_json(&bytes).map_err(|e| field(&e))?;
    Ok((bytes, creds))
}

fn view(c: ConnectionMeta, accounts: u32, now: DateTime<Utc>) -> ConnectionView {
    let status = match (c.status, c.expires_at) {
        (ConnectionStatus::NeedsReconnect, _) => ConnStatusView::NeedsReconnect,
        (_, Some(at)) if at <= now => ConnStatusView::NeedsReconnect,
        (_, Some(at)) if (at - now).num_days() < EXPIRY_WARNING_DAYS => {
            ConnStatusView::ExpiringSoon {
                days: (at - now).num_days(),
            }
        }
        _ => ConnStatusView::Ok,
    };
    let detail = ["access_level", "kind"]
        .iter()
        .find_map(|k| c.extra.get(*k).and_then(|v| v.as_str()).map(str::to_owned));
    ConnectionView {
        id: c.connection_id,
        platform: c.platform,
        name: c.name,
        is_default: c.is_default,
        status,
        expires_at: c.expires_at,
        accounts,
        detail,
    }
}
