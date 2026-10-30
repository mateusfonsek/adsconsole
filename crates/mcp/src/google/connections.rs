//! Conexões BYO do Google Ads: adicionar (JSON do client + OAuth), reconectar e remover.

use std::io;
use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::db::{ConnectionMeta, ConnectionStatus};
use adsconsole_core::routing::normalize_account_id;
use adsconsole_core::secrets::{Secret, connection_secret_key};
use adsconsole_google_ads::client_json::ClientCreds;
use adsconsole_google_ads::error::GoogleError;
use adsconsole_google_ads::oauth::authorize;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::GoogleAdapter;
use super::errors::{Scope, translate_in};
use crate::adapter::PlatformAdapter;
use crate::engine::internal;
use crate::error::{ToolError, codes};

/// Nomes dos segredos de uma conexão no `SecretStore`.
pub(crate) const CLIENT_ID: &str = "client_id";
pub(crate) const CLIENT_SECRET: &str = "client_secret";
pub(crate) const REFRESH_TOKEN: &str = "refresh_token";
const SECRET_NAMES: [&str; 3] = [CLIENT_ID, CLIENT_SECRET, REFRESH_TOKEN];

/// Espera máxima pelo login no navegador.
const LOGIN_WAIT: Duration = Duration::from_secs(300);

/// Nível de acesso do projeto do Cloud na Google Ads API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessLevel {
    Test,
    Explorer,
    Basic,
    Standard,
}

impl AccessLevel {
    /// Operações por dia permitidas; `None` = ilimitado.
    pub fn daily_limit(self) -> Option<u64> {
        match self {
            AccessLevel::Test => Some(15_000),
            AccessLevel::Explorer => Some(2_880),
            AccessLevel::Basic => Some(15_000),
            AccessLevel::Standard => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AccessLevel::Test => "Test",
            AccessLevel::Explorer => "Explorer",
            AccessLevel::Basic => "Basic",
            AccessLevel::Standard => "Standard",
        }
    }
}

/// `extra` de uma conexão do Google em `connections_meta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConnectionExtra {
    pub project_id: String,
    #[serde(default)]
    pub login_customer_id: Option<String>,
    pub access_level: AccessLevel,
}

impl ConnectionExtra {
    pub(crate) fn from_meta(meta: &ConnectionMeta) -> Result<ConnectionExtra, ToolError> {
        serde_json::from_value(meta.extra.clone()).map_err(|e| {
            ToolError::new(
                codes::INTERNAL,
                format!("Dados da conexão {} corrompidos: {e}", meta.connection_id),
            )
        })
    }
}

/// Pedido de nova conexão. Sem `Debug`: `client_json` traz o client secret.
#[derive(Clone)]
pub struct AddGoogleConnection {
    pub name: String,
    /// Conteúdo do JSON baixado do Console (client do tipo App para computador).
    pub client_json: Vec<u8>,
    pub login_customer_id: Option<String>,
    pub access_level: AccessLevel,
}

fn normalize_login(raw: Option<&str>) -> Result<Option<String>, ToolError> {
    let Some(id) = raw
        .map(|r| normalize_account_id(Platform::GoogleAds, r))
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    if !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(ToolError::new(
            codes::INVALID_INPUT,
            format!("login_customer_id inválido: {id}. Use só os dígitos do id da MCC."),
        )
        .with_field("login_customer_id"));
    }
    Ok(Some(id))
}

/// Erro do OAuth no navegador. Sem conexão gravada ainda: nada a marcar.
fn login_error(err: GoogleError) -> ToolError {
    match err {
        GoogleError::Timeout => {
            let mut e = ToolError::new(
                codes::EXECUTION_FAILED,
                "O login não foi concluído a tempo. Tente de novo.",
            );
            e.retryable = true;
            e
        }
        other => translate_in(other, None, "oauth"),
    }
}

/// Grava os 3 segredos e a linha em `connections_meta` (status `Ok`). Se algo
/// falha no meio, apaga o que já tinha gravado. Também é o jeito de semear uma
/// conexão nos testes.
pub fn save_connection(
    adapter: &GoogleAdapter,
    connection_id: &str,
    name: &str,
    creds: &ClientCreds,
    refresh: &Secret,
    login_customer_id: Option<&str>,
    access_level: AccessLevel,
) -> Result<ConnectionMeta, ToolError> {
    let login = normalize_login(login_customer_id)?;
    let extra = ConnectionExtra {
        project_id: creds.project_id.clone(),
        login_customer_id: login,
        access_level,
    };
    let meta = ConnectionMeta {
        connection_id: connection_id.to_string(),
        platform: Platform::GoogleAds,
        name: name.to_string(),
        is_default: false,
        status: ConnectionStatus::Ok,
        expires_at: None,
        extra: serde_json::to_value(&extra).map_err(internal)?,
        created_at: Utc::now(),
    };
    let secrets = adapter.secrets();
    let result = (|| {
        for (key, value) in [
            (CLIENT_ID, creds.client_id.as_str()),
            (CLIENT_SECRET, creds.client_secret.expose()),
            (REFRESH_TOKEN, refresh.expose()),
        ] {
            secrets.set(&connection_secret_key(connection_id, key), value)?;
        }
        adapter.db().upsert_connection(&meta)
    })();
    adapter.forget_client(connection_id);
    if let Err(e) = result {
        delete_secrets(adapter, connection_id);
        return Err(internal(e));
    }
    Ok(meta)
}

/// Apaga os segredos da conexão; falhas só vão para o log (sem os valores).
fn delete_secrets(adapter: &GoogleAdapter, connection_id: &str) {
    for name in SECRET_NAMES {
        if let Err(e) = adapter
            .secrets()
            .delete(&connection_secret_key(connection_id, name))
        {
            tracing::warn!(connection_id, secret = name, error = %e, "falha ao apagar segredo");
        }
    }
}

/// Nova conexão: lê o JSON do client, faz o login no navegador (até 5 min), grava
/// segredos e conexão e lista as contas. Se a listagem falha, desfaz tudo e devolve
/// o erro.
pub async fn add_google_connection(
    adapter: &GoogleAdapter,
    req: AddGoogleConnection,
    open: &(dyn Fn(&str) -> io::Result<()> + Send + Sync),
) -> Result<ConnectionMeta, ToolError> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(
            ToolError::new(codes::INVALID_INPUT, "Informe um nome para a conexão.")
                .with_field("name"),
        );
    }
    let creds = ClientCreds::from_json(&req.client_json).map_err(|e| {
        ToolError::new(codes::INVALID_INPUT, e.to_string()).with_field("client_json")
    })?;
    // Antes do login, para o usuário não autorizar à toa.
    let login = normalize_login(req.login_customer_id.as_deref())?;
    let refresh = authorize(adapter.endpoints(), &creds, open, LOGIN_WAIT)
        .await
        .map_err(login_error)?;
    let connection_id = format!("g_{}", uuid::Uuid::now_v7().simple());
    let meta = save_connection(
        adapter,
        &connection_id,
        name,
        &creds,
        &refresh,
        login.as_deref(),
        req.access_level,
    )?;
    let listed = match adapter.list_accounts(&connection_id).await {
        Ok(accounts) => adapter
            .db()
            .replace_accounts(Platform::GoogleAds, &connection_id, &accounts)
            .map_err(internal),
        Err(e) => Err(e),
    };
    if let Err(e) = listed {
        if let Err(cleanup) = remove_google_connection(adapter, &connection_id) {
            tracing::warn!(
                connection_id = connection_id.as_str(),
                error = %cleanup,
                "falha ao desfazer a conexão nova"
            );
        }
        return Err(e);
    }
    Ok(meta)
}

/// Refaz o OAuth com o client guardado, troca o refresh token, volta o status para
/// `Ok` e descarta o cliente em cache.
pub async fn reconnect_google_connection(
    adapter: &GoogleAdapter,
    connection_id: &str,
    open: &(dyn Fn(&str) -> io::Result<()> + Send + Sync),
) -> Result<ConnectionMeta, ToolError> {
    let (_, extra) = adapter.connection(connection_id)?;
    let creds = adapter.stored_creds(connection_id, &extra.project_id)?;
    let refresh = authorize(adapter.endpoints(), &creds, open, LOGIN_WAIT)
        .await
        .map_err(login_error)?;
    // A conexão pode ter sido removida enquanto o login esperava: nada a gravar.
    adapter.connection(connection_id)?;
    let key = connection_secret_key(connection_id, REFRESH_TOKEN);
    adapter
        .secrets()
        .set(&key, refresh.expose())
        .map_err(internal)?;
    adapter.forget_client(connection_id);
    let finished = adapter
        .db()
        .set_connection_status(connection_id, ConnectionStatus::Ok)
        .map_err(internal)
        .and_then(|()| adapter.connection(connection_id));
    let (meta, _) = match finished {
        Ok(found) => found,
        Err(e) => {
            if let Err(del) = adapter.secrets().delete(&key) {
                tracing::warn!(connection_id, error = %del, "falha ao apagar o refresh token novo");
            }
            adapter.forget_client(connection_id);
            return Err(e);
        }
    };
    // Conta nova no login (ou removida) entra no cache; falha aqui não desfaz o login.
    if let Err(e) = adapter.refresh_accounts(Some(connection_id)).await {
        tracing::warn!(connection_id, error = %e, "falha ao atualizar contas após reconectar");
    }
    Ok(meta)
}

/// Apaga a conexão (o cache de contas vai junto) e depois os segredos. Conexão
/// inexistente não é erro. Primeiro a linha: se ela falha, a conexão continua
/// inteira; segredo que não sai depois disso só vai para o log.
pub fn remove_google_connection(
    adapter: &GoogleAdapter,
    connection_id: &str,
) -> Result<(), ToolError> {
    adapter
        .db()
        .delete_connection(connection_id)
        .map_err(internal)?;
    adapter.forget_client(connection_id);
    delete_secrets(adapter, connection_id);
    Ok(())
}

/// Escopo de tradução de erro para uma conexão sem conta (sem moeda).
pub(crate) fn scope<'a>(
    adapter: &'a GoogleAdapter,
    connection_id: &'a str,
    extra: &'a ConnectionExtra,
) -> Scope<'a> {
    Scope {
        db: adapter.db(),
        connection_id,
        project_id: &extra.project_id,
        currency: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_timeout_message() {
        let e = login_error(GoogleError::Timeout);
        assert_eq!(
            e.message,
            "O login não foi concluído a tempo. Tente de novo."
        );
        assert!(e.retryable);
        let e = login_error(GoogleError::Auth("access_denied".into()));
        assert_eq!(e.code, codes::API_ERROR);
        assert!(e.message.contains("access_denied"));
    }

    #[test]
    fn access_level_limits_and_serde() {
        assert_eq!(AccessLevel::Test.daily_limit(), Some(15_000));
        assert_eq!(AccessLevel::Explorer.daily_limit(), Some(2_880));
        assert_eq!(AccessLevel::Basic.daily_limit(), Some(15_000));
        assert_eq!(AccessLevel::Standard.daily_limit(), None);
        assert_eq!(
            serde_json::to_value(AccessLevel::Explorer).unwrap(),
            serde_json::json!("explorer")
        );
        let extra: ConnectionExtra = serde_json::from_value(serde_json::json!({
            "project_id": "p", "login_customer_id": null, "access_level": "basic"
        }))
        .unwrap();
        assert_eq!(extra.access_level, AccessLevel::Basic);
    }

    #[test]
    fn login_customer_id_normalized() {
        assert_eq!(
            normalize_login(Some("123-456-7890")).unwrap().as_deref(),
            Some("1234567890")
        );
        assert_eq!(normalize_login(Some("  ")).unwrap(), None);
        assert_eq!(
            normalize_login(Some("abc")).unwrap_err().field.as_deref(),
            Some("login_customer_id")
        );
    }
}
