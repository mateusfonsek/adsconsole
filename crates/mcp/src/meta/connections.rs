//! Meta connections: browser login (app in development mode) or a pasted token
//! (System User). The functions here are the ones the UI calls.

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
/// How many days before expiry the warning starts.
pub const EXPIRY_WARNING_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaConnKind {
    /// User token obtained through the browser login (or pasted).
    Login,
    /// Pasted System User token.
    SystemUser,
}

/// The `extra` of a Meta connection in `connections_meta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionExtra {
    pub kind: MetaConnKind,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub config_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Login return page (published app); reused on reconnect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
}

impl ConnectionExtra {
    pub fn from_meta(meta: &ConnectionMeta) -> Result<ConnectionExtra, ToolError> {
        serde_json::from_value(meta.extra.clone())
            .map_err(|e| internal(format!("extra of connection {}: {e}", meta.connection_id)))
    }
}

/// Connection request through the browser login.
pub struct AddMetaLogin {
    pub name: String,
    pub app: AppCreds,
    /// Loopback port; `0` picks a free one.
    pub port: u16,
}

/// Connection request through a pasted token (System User or user).
pub struct AddMetaToken {
    pub name: String,
    pub access_token: Secret,
    pub app_id: Option<String>,
    pub app_secret: Option<Secret>,
}

fn check_name(name: &str) -> Result<(), ToolError> {
    if name.trim().is_empty() {
        return Err(
            ToolError::new(codes::INVALID_INPUT, "Give the connection a name.").with_field("name"),
        );
    }
    Ok(())
}

/// Rejects a token that is invalid or lacks the required permissions.
fn check_token(info: &TokenInfo) -> Result<(), ToolError> {
    if !info.is_valid {
        return Err(
            ToolError::new(codes::INVALID_INPUT, "The provided token is not valid.")
                .with_field("access_token"),
        );
    }
    let missing = missing_scopes(info);
    if !missing.is_empty() {
        return Err(ToolError::new(
            codes::ACCESS_DENIED,
            format!("The token is missing permissions: {}.", missing.join(", ")),
        )
        .with_suggestion(
            "Add these permissions to the Facebook Login for Business configuration \
             (or to the System User token) and connect again.",
        ));
    }
    Ok(())
}

/// Stores the secrets and the `connections_meta` row (status `Ok`). On error, deletes the
/// secrets already stored.
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
            tracing::warn!(connection_id, error = %e, "failed to delete secret");
        }
    }
}

fn new_connection_id() -> String {
    format!("m_{}", uuid::Uuid::now_v7().simple())
}

/// Lists the accounts of the newly stored connection and fills the cache; on failure, undoes the connection.
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
                tracing::warn!(error = %cleanup.message, "failed to undo the connection");
            }
            Err(e)
        }
    }
}

/// Browser login → long-lived token → `debug_token` → store → list the accounts.
pub async fn add_meta_login_connection(
    adapter: &MetaAdapter,
    req: AddMetaLogin,
    open: &(dyn Fn(&str) -> io::Result<()> + Send + Sync),
) -> Result<ConnectionMeta, ToolError> {
    check_name(&req.name)?;
    let endpoints = adapter.endpoints();
    let token = login(endpoints, &req.app, req.port, open, LOGIN_WAIT)
        .await
        .map_err(|e| translate_in(e, None, "the connection"))?;
    let info = debug_token(
        endpoints,
        &token,
        Some((&req.app.app_id, &req.app.app_secret)),
    )
    .await
    .map_err(|e| translate_in(e, None, "the connection"))?;
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

/// Pasted token: `debug_token` → store → list the accounts.
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
        .map_err(|e| translate_in(e, None, "the connection"))?;
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

/// Repeats the browser login of a login connection and replaces the token.
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
            "System User token connections are renewed by pasting a new token.",
        )
        .with_suggestion("Use the option to replace the connection's token."));
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
        .map_err(|e| translate_in(e, None, "the reconnection"))?;
    let info = debug_token(endpoints, &token, Some((&app.app_id, &app.app_secret)))
        .await
        .map_err(|e| translate_in(e, None, "the reconnection"))?;
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
        tracing::warn!(connection_id, error = %e.message, "failed to refresh the accounts");
    }
    Ok(meta)
}

/// Replaces the token of any connection with a pasted one, after validating it.
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
        .map_err(|e| translate_in(e, None, "the token replacement"))?;
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
        tracing::warn!(connection_id, error = %e.message, "failed to refresh the accounts");
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

/// Meta connections whose token expires within 7 days (or already expired).
pub fn expiring_connections(db: &Db, now: DateTime<Utc>) -> Result<Vec<ConnectionMeta>, ToolError> {
    let limit = now + chrono::Duration::days(EXPIRY_WARNING_DAYS);
    Ok(db
        .list_connections(Platform::MetaAds)
        .map_err(internal)?
        .into_iter()
        .filter(|c| c.expires_at.is_some_and(|t| t <= limit))
        .collect())
}

/// Text of the expiry warning, or `None` if more than 7 days remain (or it never expires).
pub(crate) fn token_warning(
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<String> {
    let at = expires_at?;
    let date = at.format("%d/%m/%Y");
    if at <= now {
        return Some(format!(
            "This connection's Meta access expired on {date}. Reconnect it in AdsConsole → Connections."
        ));
    }
    let secs = (at - now).num_seconds();
    let days = (secs + 86_399) / 86_400;
    (days <= EXPIRY_WARNING_DAYS).then(|| {
        let unit = if days == 1 { "day" } else { "days" };
        format!(
            "This connection's Meta access expires on {date} ({days} {unit}). Reconnect it in AdsConsole → Connections."
        )
    })
}
