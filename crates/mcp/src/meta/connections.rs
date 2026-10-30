//! Conexões da Meta: login no navegador (app em modo de desenvolvimento) ou token
//! colado (System User). As funções aqui são as que a UI chama.

use std::io;
use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::db::{ConnectionMeta, ConnectionStatus, Db};
use adsconsole_core::secrets::{Secret, connection_secret_key};
use adsconsole_meta_ads::oauth::{AppCreds, login};
use adsconsole_meta_ads::token::{TokenInfo, TokenKind, debug_token, missing_scopes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::MetaAdapter;
use super::errors::translate_in;
use crate::PlatformAdapter;
use crate::engine::internal;
use crate::error::{ToolError, codes};

pub(crate) const APP_SECRET: &str = "app_secret";
pub(crate) const ACCESS_TOKEN: &str = "access_token";
const LOGIN_WAIT: Duration = Duration::from_secs(300);
/// Aviso de vencimento a partir de quantos dias antes.
pub const EXPIRY_WARNING_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaConnKind {
    /// Token de usuário obtido pelo login no navegador (ou colado).
    Login,
    /// Token de System User colado.
    SystemUser,
}

/// O `extra` de uma conexão da Meta em `connections_meta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionExtra {
    pub kind: MetaConnKind,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub config_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Página de retorno do login (app publicado); reusada ao reconectar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
}

impl ConnectionExtra {
    pub fn from_meta(meta: &ConnectionMeta) -> Result<ConnectionExtra, ToolError> {
        serde_json::from_value(meta.extra.clone())
            .map_err(|e| internal(format!("extra da conexão {}: {e}", meta.connection_id)))
    }
}

/// Pedido de conexão por login no navegador.
pub struct AddMetaLogin {
    pub name: String,
    pub app: AppCreds,
    /// Porta do loopback; `0` escolhe uma livre.
    pub port: u16,
}

/// Pedido de conexão por token colado (System User ou usuário).
pub struct AddMetaToken {
    pub name: String,
    pub access_token: Secret,
    pub app_id: Option<String>,
    pub app_secret: Option<Secret>,
}

fn check_name(name: &str) -> Result<(), ToolError> {
    if name.trim().is_empty() {
        return Err(
            ToolError::new(codes::INVALID_INPUT, "Dê um nome para a conexão.").with_field("name"),
        );
    }
    Ok(())
}

/// Recusa token inválido ou sem as permissões necessárias.
fn check_token(info: &TokenInfo) -> Result<(), ToolError> {
    if !info.is_valid {
        return Err(
            ToolError::new(codes::INVALID_INPUT, "O token informado não é válido.")
                .with_field("access_token"),
        );
    }
    let missing = missing_scopes(info);
    if !missing.is_empty() {
        return Err(ToolError::new(
            codes::ACCESS_DENIED,
            format!("Faltam permissões no token: {}.", missing.join(", ")),
        )
        .with_suggestion(
            "Inclua essas permissões na configuração do Login do Facebook para Empresas \
             (ou no token do System User) e conecte de novo.",
        ));
    }
    Ok(())
}

/// Grava os segredos e a linha de `connections_meta` (status `Ok`). Em erro, apaga os
/// segredos já gravados.
pub fn save_connection(
    adapter: &MetaAdapter,
    connection_id: &str,
    name: &str,
    token: &Secret,
    app_secret: Option<&Secret>,
    extra: &ConnectionExtra,
    expires_at: Option<DateTime<Utc>>,
) -> Result<ConnectionMeta, ToolError> {
    let secrets = adapter.secrets();
    let result = (|| {
        secrets
            .set(
                &connection_secret_key(connection_id, ACCESS_TOKEN),
                token.expose(),
            )
            .map_err(internal)?;
        match app_secret {
            Some(s) => secrets.set(
                &connection_secret_key(connection_id, APP_SECRET),
                s.expose(),
            ),
            None => secrets.delete(&connection_secret_key(connection_id, APP_SECRET)),
        }
        .map_err(internal)?;
        let created_at = adapter
            .db()
            .get_connection(connection_id)
            .map_err(internal)?
            .map(|c| c.created_at)
            .unwrap_or_else(Utc::now);
        let meta = ConnectionMeta {
            connection_id: connection_id.to_string(),
            platform: Platform::MetaAds,
            name: name.to_string(),
            is_default: false,
            status: ConnectionStatus::Ok,
            expires_at,
            extra: serde_json::to_value(extra).map_err(internal)?,
            created_at,
        };
        adapter.db().upsert_connection(&meta).map_err(internal)?;
        Ok(meta)
    })();
    adapter.forget_client(connection_id);
    if result.is_err() {
        delete_secrets(adapter, connection_id);
    }
    result
}

fn delete_secrets(adapter: &MetaAdapter, connection_id: &str) {
    for name in [ACCESS_TOKEN, APP_SECRET] {
        if let Err(e) = adapter
            .secrets()
            .delete(&connection_secret_key(connection_id, name))
        {
            tracing::warn!(connection_id, error = %e, "falha ao apagar segredo");
        }
    }
}

fn new_connection_id() -> String {
    format!("m_{}", uuid::Uuid::now_v7().simple())
}

/// Lista as contas da conexão recém-gravada e preenche o cache; em falha, desfaz a conexão.
async fn finish_add(
    adapter: &MetaAdapter,
    meta: ConnectionMeta,
) -> Result<ConnectionMeta, ToolError> {
    match adapter.list_accounts(&meta.connection_id).await {
        Ok(accounts) => {
            adapter
                .db()
                .replace_accounts(Platform::MetaAds, &meta.connection_id, &accounts)
                .map_err(internal)?;
            Ok(meta)
        }
        Err(e) => {
            if let Err(cleanup) = remove_meta_connection(adapter, &meta.connection_id) {
                tracing::warn!(error = %cleanup.message, "falha ao desfazer a conexão");
            }
            Err(e)
        }
    }
}

/// Login no navegador → token de longa duração → `debug_token` → grava → lista as contas.
pub async fn add_meta_login_connection(
    adapter: &MetaAdapter,
    req: AddMetaLogin,
    open: &(dyn Fn(&str) -> io::Result<()> + Send + Sync),
) -> Result<ConnectionMeta, ToolError> {
    check_name(&req.name)?;
    let endpoints = adapter.endpoints();
    let token = login(endpoints, &req.app, req.port, open, LOGIN_WAIT)
        .await
        .map_err(|e| translate_in(e, None, "conectar"))?;
    let info = debug_token(
        endpoints,
        &token,
        Some((&req.app.app_id, &req.app.app_secret)),
    )
    .await
    .map_err(|e| translate_in(e, None, "conectar"))?;
    check_token(&info)?;
    let extra = ConnectionExtra {
        kind: MetaConnKind::Login,
        app_id: Some(req.app.app_id.clone()),
        config_id: Some(req.app.config_id.clone()),
        user_id: info.user_id.clone(),
        relay_url: req.app.relay_url.clone(),
    };
    let id = new_connection_id();
    let meta = save_connection(
        adapter,
        &id,
        req.name.trim(),
        &token,
        Some(&req.app.app_secret),
        &extra,
        info.expires_at,
    )?;
    finish_add(adapter, meta).await
}

/// Token colado: `debug_token` → grava → lista as contas.
pub async fn add_meta_token_connection(
    adapter: &MetaAdapter,
    req: AddMetaToken,
) -> Result<ConnectionMeta, ToolError> {
    check_name(&req.name)?;
    let app = match (&req.app_id, &req.app_secret) {
        (Some(id), Some(secret)) => Some((id.as_str(), secret)),
        _ => None,
    };
    let info = debug_token(adapter.endpoints(), &req.access_token, app)
        .await
        .map_err(|e| translate_in(e, None, "conectar"))?;
    check_token(&info)?;
    let extra = ConnectionExtra {
        kind: if info.kind == TokenKind::SystemUser {
            MetaConnKind::SystemUser
        } else {
            MetaConnKind::Login
        },
        app_id: req.app_id.clone(),
        config_id: None,
        user_id: info.user_id.clone(),
        relay_url: None,
    };
    let id = new_connection_id();
    let meta = save_connection(
        adapter,
        &id,
        req.name.trim(),
        &req.access_token,
        req.app_secret.as_ref(),
        &extra,
        info.expires_at,
    )?;
    finish_add(adapter, meta).await
}

/// Refaz o login de uma conexão por login no navegador e troca o token.
pub async fn reconnect_meta_connection(
    adapter: &MetaAdapter,
    connection_id: &str,
    port: u16,
    open: &(dyn Fn(&str) -> io::Result<()> + Send + Sync),
) -> Result<ConnectionMeta, ToolError> {
    let (meta, extra) = adapter.connection(connection_id)?;
    let (Some(app_id), Some(config_id), MetaConnKind::Login) =
        (extra.app_id.clone(), extra.config_id.clone(), extra.kind)
    else {
        return Err(ToolError::new(
            codes::INVALID_STATE,
            "Conexões por token de System User são renovadas colando um token novo.",
        )
        .with_suggestion("Use a opção de trocar o token da conexão."));
    };
    let app = AppCreds {
        app_id,
        app_secret: adapter.secret(connection_id, APP_SECRET)?,
        config_id,
        relay_url: extra.relay_url.clone(),
    };
    let endpoints = adapter.endpoints();
    let token = login(endpoints, &app, port, open, LOGIN_WAIT)
        .await
        .map_err(|e| translate_in(e, None, "reconectar"))?;
    let info = debug_token(endpoints, &token, Some((&app.app_id, &app.app_secret)))
        .await
        .map_err(|e| translate_in(e, None, "reconectar"))?;
    check_token(&info)?;
    let meta = save_connection(
        adapter,
        connection_id,
        &meta.name,
        &token,
        Some(&app.app_secret),
        &ConnectionExtra {
            user_id: info.user_id.or(extra.user_id),
            ..extra
        },
        info.expires_at,
    )?;
    if let Err(e) = adapter.refresh_accounts(Some(connection_id)).await {
        tracing::warn!(connection_id, error = %e.message, "falha ao atualizar as contas");
    }
    Ok(meta)
}

/// Troca o token de qualquer conexão por um colado, depois de validá-lo.
pub async fn replace_meta_token(
    adapter: &MetaAdapter,
    connection_id: &str,
    token: Secret,
) -> Result<ConnectionMeta, ToolError> {
    let (meta, extra) = adapter.connection(connection_id)?;
    let app_secret = adapter.secret(connection_id, APP_SECRET).ok();
    let app = match (&extra.app_id, &app_secret) {
        (Some(id), Some(secret)) => Some((id.as_str(), secret)),
        _ => None,
    };
    let info = debug_token(adapter.endpoints(), &token, app)
        .await
        .map_err(|e| translate_in(e, None, "trocar o token"))?;
    check_token(&info)?;
    let meta = save_connection(
        adapter,
        connection_id,
        &meta.name,
        &token,
        app_secret.as_ref(),
        &extra,
        info.expires_at,
    )?;
    if let Err(e) = adapter.refresh_accounts(Some(connection_id)).await {
        tracing::warn!(connection_id, error = %e.message, "falha ao atualizar as contas");
    }
    Ok(meta)
}

pub fn remove_meta_connection(adapter: &MetaAdapter, connection_id: &str) -> Result<(), ToolError> {
    adapter
        .db()
        .delete_connection(connection_id)
        .map_err(internal)?;
    adapter.forget_client(connection_id);
    delete_secrets(adapter, connection_id);
    Ok(())
}

/// Conexões da Meta cujo token vence em até 7 dias (ou já venceu).
pub fn expiring_connections(db: &Db, now: DateTime<Utc>) -> Result<Vec<ConnectionMeta>, ToolError> {
    let limit = now + chrono::Duration::days(EXPIRY_WARNING_DAYS);
    Ok(db
        .list_connections(Platform::MetaAds)
        .map_err(internal)?
        .into_iter()
        .filter(|c| c.expires_at.is_some_and(|t| t <= limit))
        .collect())
}

/// Texto do aviso de vencimento, ou `None` se faltam mais de 7 dias (ou não vence).
pub(crate) fn token_warning(
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<String> {
    let at = expires_at?;
    let date = at.format("%d/%m/%Y");
    if at <= now {
        return Some(format!(
            "O acesso desta conexão à Meta venceu em {date}. Reconecte em AdsConsole → Conexões."
        ));
    }
    let secs = (at - now).num_seconds();
    let days = (secs + 86_399) / 86_400;
    (days <= EXPIRY_WARNING_DAYS).then(|| {
        let unit = if days == 1 { "dia" } else { "dias" };
        format!(
            "O acesso desta conexão à Meta vence em {date} ({days} {unit}). Reconecte em AdsConsole → Conexões."
        )
    })
}
