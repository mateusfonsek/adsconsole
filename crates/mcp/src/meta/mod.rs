//! Adapter da Meta Ads: conexões BYO, cache de clientes, contexto por conta,
//! pré-checagem da conta e tradução de erros. As ferramentas ficam em [`tools`].

pub mod connections;
pub mod ctx;
pub mod errors;
mod plan;
mod tools;
#[cfg(test)]
mod write_tests;

pub use tools::scrub;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adsconsole_core::Platform;
use adsconsole_core::db::{AccountInfo, ConnectionMeta, ConnectionStatus, Db};
use adsconsole_core::money::currency_exponent;
use adsconsole_core::routing::RoutingError;
use adsconsole_core::secrets::{Secret, SecretStore, connection_secret_key};
use adsconsole_meta_ads::client::MetaClient;
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::limiter::AccountLimiter;
use adsconsole_meta_ads::retry::RetryPolicy;
use chrono::Utc;
use serde_json::Value;

use crate::adapter::{Applied, Call, PlatformAdapter, Progress, ToolSpec, Validated};
use crate::engine::{internal, refresh_connections};
use crate::error::{ToolError, codes};
use connections::{ACCESS_TOKEN, APP_SECRET, ConnectionExtra, token_warning};
use ctx::{Ctx, UploadPoll};
use errors::translate_in;
use plan::MetaPlan;
use tools::{MTool, WriteTool, accounts};

/// Teto de contas lidas por conexão.
const MAX_ACCOUNTS: usize = 10_000;
/// Validade do `account_status` lido no `precheck` (que roda duas vezes por escrita).
const STATUS_TTL: Duration = Duration::from_secs(10);
const ACCOUNT_FIELDS: &str =
    "id,account_id,name,currency,timezone_name,account_status,business{id,name}";

/// Nome do `account_status` da Meta.
pub fn status_label(code: i64) -> String {
    match code {
        1 => "ACTIVE".into(),
        2 => "DISABLED".into(),
        3 => "UNSETTLED".into(),
        7 => "PENDING_RISK_REVIEW".into(),
        8 => "PENDING_SETTLEMENT".into(),
        9 => "IN_GRACE_PERIOD".into(),
        100 => "PENDING_CLOSURE".into(),
        101 => "CLOSED".into(),
        201 => "ANY_ACTIVE".into(),
        202 => "ANY_CLOSED".into(),
        n => format!("STATUS_{n}"),
    }
}

/// O status em palavras, para as mensagens.
pub fn status_words(label: &str) -> &'static str {
    match label {
        "ACTIVE" => "ativa",
        "DISABLED" => "desativada",
        "UNSETTLED" => "com pagamento pendente",
        "PENDING_RISK_REVIEW" => "em análise de risco",
        "PENDING_SETTLEMENT" => "com acerto de pagamento pendente",
        "IN_GRACE_PERIOD" => "no período de carência",
        "PENDING_CLOSURE" => "em encerramento",
        "CLOSED" | "ANY_CLOSED" => "fechada",
        _ => "inativa",
    }
}

fn account_from(v: &Value) -> Option<AccountInfo> {
    let id = v["id"].as_str()?.to_string();
    Some(AccountInfo {
        name: v["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Conta {id}")),
        currency: v["currency"].as_str().unwrap_or_default().to_string(),
        timezone: v["timezone_name"].as_str().map(str::to_string),
        status: v["account_status"].as_i64().map(status_label),
        manager_id: None,
        account_id: id,
    })
}

/// Clientes por conexão; a geração sobe a cada `forget_client` (ver o adapter do Google).
#[derive(Default)]
struct ClientCache {
    clients: HashMap<String, MetaClient>,
    generation: HashMap<String, u64>,
}

pub struct MetaAdapter {
    db: Arc<Db>,
    secrets: Arc<dyn SecretStore>,
    endpoints: MetaEndpoints,
    retry: RetryPolicy,
    clients: Mutex<ClientCache>,
    limiter: Arc<AccountLimiter>,
    tools: Vec<(ToolSpec, MTool)>,
    poll: Mutex<UploadPoll>,
    /// `account_status` lido no `precheck`, por (conexão, conta).
    statuses: Mutex<HashMap<(String, String), (Instant, i64)>>,
}

impl MetaAdapter {
    pub fn new(
        db: Arc<Db>,
        secrets: Arc<dyn SecretStore>,
        endpoints: MetaEndpoints,
    ) -> Arc<MetaAdapter> {
        Self::with_retry(db, secrets, endpoints, RetryPolicy::default())
    }

    /// Como [`MetaAdapter::new`], com outro backoff (os testes usam milissegundos).
    pub fn with_retry(
        db: Arc<Db>,
        secrets: Arc<dyn SecretStore>,
        endpoints: MetaEndpoints,
        retry: RetryPolicy,
    ) -> Arc<MetaAdapter> {
        Self::with_tools(db, secrets, endpoints, retry, tools::all())
    }

    pub(crate) fn with_tools(
        db: Arc<Db>,
        secrets: Arc<dyn SecretStore>,
        endpoints: MetaEndpoints,
        retry: RetryPolicy,
        tools: Vec<MTool>,
    ) -> Arc<MetaAdapter> {
        Arc::new(MetaAdapter {
            db,
            secrets,
            endpoints,
            retry,
            clients: Mutex::new(ClientCache::default()),
            limiter: AccountLimiter::new(),
            tools: tools.into_iter().map(|t| (t.spec(), t)).collect(),
            poll: Mutex::new(UploadPoll::default()),
            statuses: Mutex::new(HashMap::new()),
        })
    }

    /// Só para testes: espera de uploads e cópias assíncronas (no lugar de 10 s e 10 min).
    #[doc(hidden)]
    pub fn set_poll(&self, every: Duration, max: Duration) {
        *self.poll.lock().unwrap_or_else(|e| e.into_inner()) = UploadPoll { every, max };
    }

    pub(crate) fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub(crate) fn secrets(&self) -> &dyn SecretStore {
        self.secrets.as_ref()
    }

    pub(crate) fn endpoints(&self) -> &MetaEndpoints {
        &self.endpoints
    }

    fn lock_clients(&self) -> std::sync::MutexGuard<'_, ClientCache> {
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Descarta o cliente em cache (token trocado ou conexão removida).
    pub(crate) fn forget_client(&self, connection_id: &str) {
        let mut cache = self.lock_clients();
        cache.clients.remove(connection_id);
        *cache
            .generation
            .entry(connection_id.to_string())
            .or_default() += 1;
    }

    /// Conexão da Meta com o `extra` já lido; senão `UNKNOWN_CONNECTION`.
    pub(crate) fn connection(
        &self,
        connection_id: &str,
    ) -> Result<(ConnectionMeta, ConnectionExtra), ToolError> {
        let meta = self
            .db
            .get_connection(connection_id)
            .map_err(internal)?
            .filter(|c| c.platform == Platform::MetaAds)
            .ok_or_else(|| RoutingError::UnknownConnection(connection_id.to_string()))?;
        let extra = ConnectionExtra::from_meta(&meta)?;
        Ok((meta, extra))
    }

    pub(crate) fn secret(&self, connection_id: &str, name: &str) -> Result<Secret, ToolError> {
        self.secrets
            .get(&connection_secret_key(connection_id, name))
            .map_err(internal)?
            .filter(|v| !v.is_empty())
            .map(Secret::new)
            .ok_or_else(|| {
                ToolError::new(
                    codes::CONNECTION_NEEDS_RECONNECT,
                    format!(
                        "As credenciais da conexão {connection_id} não estão no armazenamento de segredos."
                    ),
                )
                .with_suggestion("Remova e adicione a conexão de novo no AdsConsole → Conexões.")
            })
    }

    fn client(&self, connection_id: &str) -> Result<MetaClient, ToolError> {
        loop {
            let generation = {
                let cache = self.lock_clients();
                if let Some(c) = cache.clients.get(connection_id) {
                    return Ok(c.clone());
                }
                cache.generation.get(connection_id).copied().unwrap_or(0)
            };
            let token = self.secret(connection_id, ACCESS_TOKEN)?;
            let app_secret = self.secret(connection_id, APP_SECRET).ok();
            let client = MetaClient::new(self.endpoints.clone(), token, app_secret)
                .with_retry(self.retry)
                .with_limiter(self.limiter.clone());
            let mut cache = self.lock_clients();
            if cache.generation.get(connection_id).copied().unwrap_or(0) != generation {
                continue;
            }
            return Ok(cache
                .clients
                .entry(connection_id.to_string())
                .or_insert(client)
                .clone());
        }
    }

    fn cached_account(
        &self,
        connection_id: &str,
        account_id: &str,
    ) -> Result<AccountInfo, ToolError> {
        self.db
            .accounts_for(Platform::MetaAds, account_id)
            .map_err(internal)?
            .into_iter()
            .find(|(cid, _)| cid == connection_id)
            .map(|(_, a)| a)
            .ok_or_else(|| {
                RoutingError::AccountNotInConnection {
                    account_id: account_id.to_string(),
                    connection: connection_id.to_string(),
                }
                .into()
            })
    }

    /// Contexto da conta da chamada, a partir do cache de contas e da conexão.
    pub fn ctx(&self, call: &Call) -> Result<Ctx, ToolError> {
        let (meta, _) = self.connection(&call.connection_id)?;
        if meta.status == ConnectionStatus::NeedsReconnect {
            return Err(RoutingError::NeedsReconnect(meta.connection_id).into());
        }
        let account = self.cached_account(&call.connection_id, &call.account_id)?;
        let client = self
            .client(&call.connection_id)?
            .for_account(&account.account_id);
        Ok(Ctx {
            client,
            exponent: currency_exponent(&account.currency),
            currency: account.currency,
            account_id: account.account_id,
            connection_id: meta.connection_id,
            db: self.db.clone(),
            token_warning: token_warning(meta.expires_at, Utc::now()),
            upload_poll: *self.poll.lock().unwrap_or_else(|e| e.into_inner()),
        })
    }

    /// Atualiza o cache de contas de `connection` (ou de todas as conexões `Ok`).
    pub(crate) async fn refresh_accounts(&self, connection: Option<&str>) -> Result<(), ToolError> {
        let targets = match connection {
            Some(c) => vec![c.to_string()],
            None => self.connections(),
        };
        refresh_connections(self, &self.db, Platform::MetaAds, &targets)
            .await
            .map(|_| ())
    }

    fn tool(&self, name: &str) -> Option<&MTool> {
        self.tools
            .iter()
            .find(|(spec, _)| spec.name == name)
            .map(|(_, t)| t)
    }

    fn write_tool(&self, name: &str) -> Result<&dyn WriteTool, ToolError> {
        match self.tool(name) {
            Some(MTool::Write(tool)) => Ok(tool.as_ref()),
            _ => Err(not_implemented(name)),
        }
    }

    /// Contexto e plano de uma escrita (refeito em cada etapa; ver [`WriteTool`]).
    async fn plan(&self, call: &Call) -> Result<(Ctx, MetaPlan), ToolError> {
        let tool = self.write_tool(&call.tool)?;
        let ctx = self.ctx(call)?;
        let plan = tool.plan(&ctx, &call.input).await?;
        Ok((ctx, plan))
    }

    /// `account_status` da conta, do cache de 10 s ou lido da Meta.
    async fn account_status(&self, ctx: &Ctx) -> Result<i64, ToolError> {
        let key = (ctx.connection_id.clone(), ctx.account_id.clone());
        if let Some((at, status)) = self
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            && at.elapsed() < STATUS_TTL
        {
            return Ok(*status);
        }
        let v = ctx
            .client
            .get(&ctx.account_id, &[("fields", "account_status".into())])
            .await
            .map_err(|e| errors::translate(e, Some(ctx), "precheck"))?;
        let status = v["account_status"]
            .as_i64()
            .ok_or_else(|| internal("a Meta não devolveu account_status"))?;
        self.statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (Instant::now(), status));
        Ok(status)
    }

    /// Grava o status lido no cache de contas, se mudou.
    fn update_cached_status(&self, ctx: &Ctx, label: &str) -> Result<(), ToolError> {
        let mut accounts: Vec<AccountInfo> = self
            .db
            .list_accounts(Platform::MetaAds, Some(&ctx.connection_id))
            .map_err(internal)?
            .into_iter()
            .map(|(_, a)| a)
            .collect();
        let mut changed = false;
        for a in accounts
            .iter_mut()
            .filter(|a| a.account_id == ctx.account_id)
        {
            if a.status.as_deref() != Some(label) {
                a.status = Some(label.to_string());
                changed = true;
            }
        }
        if changed {
            self.db
                .replace_accounts(Platform::MetaAds, &ctx.connection_id, &accounts)
                .map_err(internal)?;
        }
        Ok(())
    }
}

fn not_implemented(tool: &str) -> ToolError {
    ToolError::new(
        codes::UNKNOWN_TOOL,
        format!("Ferramenta desconhecida: {tool}"),
    )
}

#[async_trait::async_trait]
impl PlatformAdapter for MetaAdapter {
    fn platform(&self) -> Platform {
        Platform::MetaAds
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let mut specs = vec![accounts::spec()];
        specs.extend(self.tools.iter().map(|(spec, _)| spec.clone()));
        specs
    }

    async fn list_accounts(&self, connection_id: &str) -> Result<Vec<AccountInfo>, ToolError> {
        let (meta, _) = self.connection(connection_id)?;
        if meta.status == ConnectionStatus::NeedsReconnect {
            return Err(RoutingError::NeedsReconnect(meta.connection_id).into());
        }
        let client = self.client(connection_id)?;
        let page = client
            .get_all(
                "me/adaccounts",
                &[("fields", ACCOUNT_FIELDS.into()), ("limit", "100".into())],
                MAX_ACCOUNTS,
            )
            .await
            .map_err(|e: MetaError| {
                translate_in(e, Some((self.db.as_ref(), connection_id)), accounts::NAME)
            })?;
        Ok(page.data.iter().filter_map(account_from).collect())
    }

    async fn read(&self, call: &Call) -> Result<Value, ToolError> {
        if call.tool == accounts::NAME {
            return accounts::run(self, &call.input).await;
        }
        let Some(MTool::Read(tool)) = self.tool(&call.tool) else {
            return Err(not_implemented(&call.tool));
        };
        let ctx = self.ctx(call)?;
        let mut out = scrub(tool.run(&ctx, &call.input).await?);
        if let Some(warning) = &ctx.token_warning
            && let Some(obj) = out.as_object_mut()
        {
            obj.insert("aviso_token".into(), Value::String(warning.clone()));
        }
        Ok(out)
    }

    async fn precheck(&self, call: &Call) -> Result<(), ToolError> {
        let ctx = self.ctx(call)?;
        let status = self.account_status(&ctx).await?;
        let label = status_label(status);
        self.update_cached_status(&ctx, &label)?;
        if status == 1 {
            return Ok(());
        }
        Err(ToolError::new(
            codes::ACCOUNT_NOT_ACTIVE,
            format!(
                "A conta {} está {}; a Meta não deixa editar nem apagar objetos nela.",
                ctx.account_id,
                status_words(&label)
            ),
        )
        .with_details(serde_json::json!({ "account_status": status, "status": label })))
    }

    async fn validate(&self, call: &Call) -> Result<Validated, ToolError> {
        let (ctx, plan) = self.plan(call).await?;
        plan::validate(&ctx, &call.tool, &plan).await
    }

    async fn snapshot(&self, call: &Call) -> Result<Value, ToolError> {
        let (ctx, plan) = self.plan(call).await?;
        plan::snapshot(&ctx, &call.tool, &plan).await
    }

    async fn execute(&self, call: &Call, before: &Value) -> Result<Applied, ToolError> {
        struct Ignore;
        impl Progress for Ignore {
            fn created(&self, _ids: &Value) {}
        }
        self.execute_tracked(call, before, &Ignore).await
    }

    async fn execute_tracked(
        &self,
        call: &Call,
        before: &Value,
        progress: &dyn Progress,
    ) -> Result<Applied, ToolError> {
        let (ctx, plan) = self.plan(call).await?;
        plan::execute(&ctx, &call.tool, &plan, before, progress).await
    }

    async fn current_state(
        &self,
        connection_id: &str,
        account_id: &str,
        target: &Value,
    ) -> Result<Value, ToolError> {
        let call = Call {
            connection_id: connection_id.to_string(),
            account_id: account_id.to_string(),
            tool: "current_state".into(),
            input: Value::Null,
        };
        let ctx = self.ctx(&call)?;
        plan::current_state(&ctx, "current_state", target).await
    }

    fn connections(&self) -> Vec<String> {
        match self.db.list_connections(Platform::MetaAds) {
            Ok(list) => list
                .into_iter()
                .filter(|c| c.status == ConnectionStatus::Ok)
                .map(|c| c.connection_id)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "falha ao listar as conexões da Meta");
                Vec::new()
            }
        }
    }
}
