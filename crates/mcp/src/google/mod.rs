//! Google Ads adapter: BYO connections, client cache, per-account context and
//! error translation. The tools live in [`tools`].

pub mod connections;
pub mod ctx;
pub mod errors;
mod plan;
mod tools;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use adsconsole_core::Platform;
use adsconsole_core::db::{AccountInfo, ConnectionMeta, ConnectionStatus, Db};
use adsconsole_core::routing::RoutingError;
use adsconsole_core::secrets::{Secret, SecretStore, connection_secret_key};
use adsconsole_google_ads::client::GoogleAdsClient;
use adsconsole_google_ads::client_json::ClientCreds;
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_google_ads::error::GoogleError;
use adsconsole_google_ads::meter::OpsMeter;
use adsconsole_google_ads::retry::RetryPolicy;
use adsconsole_google_ads::token::TokenManager;
use chrono::Utc;
use serde_json::Value;

use crate::adapter::{Applied, Call, PlatformAdapter, ToolSpec, Validated};
use crate::engine::{internal, refresh_connections};
use crate::error::{ToolError, codes};
use connections::{CLIENT_ID, CLIENT_SECRET, ConnectionExtra, REFRESH_TOKEN};
use ctx::{Ctx, UploadPoll};
use errors::translate_in;
use plan::{DirectExec, WritePlan};
use tools::{GTool, WriteTool, accounts};

const CUSTOMER_QUERY: &str = "SELECT customer.id, customer.descriptive_name, \
     customer.currency_code, customer.time_zone, customer.status, customer.manager \
     FROM customer";
const CLIENT_QUERY: &str = "SELECT customer_client.id, customer_client.descriptive_name, \
     customer_client.currency_code, customer_client.time_zone, customer_client.status, \
     customer_client.manager, customer_client.level FROM customer_client \
     WHERE customer_client.level = 1";
/// Cap on child accounts read per MCC.
const MAX_CHILDREN: usize = 10_000;
/// Cached status of a manager account (MCC), which has no campaigns.
const MANAGER_STATUS: &str = "MANAGER";

/// Records the sent operations in `google_ops`, under the connection's Cloud project.
struct DbMeter {
    db: Arc<Db>,
    project_id: String,
}

impl OpsMeter for DbMeter {
    fn record(&self, ops: u32) {
        if let Err(e) = self.db.record_google_ops(&self.project_id, ops, Utc::now()) {
            tracing::warn!(project_id = self.project_id.as_str(), error = %e, "failed to count operations");
        }
    }
}

/// Clients per connection. A connection's generation goes up on every `forget_client`: a
/// client built with secrets read before that doesn't go into the cache.
#[derive(Default)]
struct ClientCache {
    clients: HashMap<String, GoogleAdsClient>,
    generation: HashMap<String, u64>,
}

pub struct GoogleAdapter {
    db: Arc<Db>,
    secrets: Arc<dyn SecretStore>,
    endpoints: GoogleEndpoints,
    retry: RetryPolicy,
    /// One client per connection (cached token and operations meter).
    clients: Mutex<ClientCache>,
    /// Tools that take an account, with the spec already built (the schema is generated once).
    tools: Vec<(ToolSpec, GTool)>,
    /// `upload_video` wait (see [`GoogleAdapter::set_poll`]).
    poll: Mutex<UploadPoll>,
    /// Video file as it was at `validate`, per call (see [`tools::assets`]).
    seen_files: Mutex<HashMap<String, tools::assets::FileStamp>>,
}

/// Warns from 80% of the access level's daily limit.
pub fn quota_warning(ctx: &Ctx) -> Option<String> {
    let limit = ctx.access_level.daily_limit()?;
    let used = match ctx.db.google_ops_last_24h(&ctx.project_id, Utc::now()) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read the operation count");
            return None;
        }
    };
    (used * 5 >= limit * 4).then(|| {
        format!(
            "This connection has used {used} of {limit} operations in the last 24 h ({} access level).",
            ctx.access_level.label()
        )
    })
}

fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Converts a `customer` or `customer_client` row (already without the resource key).
fn account_from_row(v: &Value, fallback_id: &str, manager_id: Option<&str>) -> AccountInfo {
    let id = text(v, "id").unwrap_or_else(|| fallback_id.to_string());
    let is_manager = v.get("manager").and_then(Value::as_bool).unwrap_or(false);
    AccountInfo {
        name: text(v, "descriptiveName")
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Account {id}")),
        currency: text(v, "currencyCode").unwrap_or_default(),
        timezone: text(v, "timeZone"),
        status: if is_manager {
            Some(MANAGER_STATUS.to_string())
        } else {
            text(v, "status")
        },
        manager_id: manager_id.map(str::to_string),
        account_id: id,
    }
}

/// An account the connection lists but can't query (canceled, no permission) is
/// left out instead of failing the whole listing.
fn skippable(e: &GoogleError) -> bool {
    matches!(e, GoogleError::Api { errors, .. } if errors.iter().any(|x| matches!(
        x.code.as_str(),
        "CUSTOMER_NOT_ENABLED" | "USER_PERMISSION_DENIED"
    )))
}

async fn fetch_accounts(client: &GoogleAdsClient) -> Result<Vec<AccountInfo>, GoogleError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut managers = Vec::new();
    // Directly accessible accounts first (no login); then the children, in MCC order.
    for id in client.list_accessible_customers().await? {
        let page = match client.search(&id, None, CUSTOMER_QUERY, None).await {
            Ok(p) => p,
            Err(e) if skippable(&e) => {
                tracing::warn!(customer_id = id.as_str(), error = %e, "account skipped in listing");
                continue;
            }
            Err(e) => return Err(e),
        };
        let Some(row) = page.results.first().and_then(|r| r.get("customer")) else {
            continue;
        };
        let info = account_from_row(row, &id, None);
        if info.status.as_deref() == Some(MANAGER_STATUS) {
            managers.push(info.account_id.clone());
        }
        if seen.insert(info.account_id.clone()) {
            out.push(info);
        }
    }
    for manager in managers {
        let rows = match client
            .search_all(&manager, Some(&manager), CLIENT_QUERY, MAX_CHILDREN)
            .await
        {
            Ok(r) => r,
            Err(e) if skippable(&e) => {
                tracing::warn!(customer_id = manager.as_str(), error = %e, "mcc skipped in listing");
                continue;
            }
            Err(e) => return Err(e),
        };
        for row in &rows {
            let Some(v) = row.get("customerClient") else {
                continue;
            };
            let info = account_from_row(v, "", Some(&manager));
            if !info.account_id.is_empty() && seen.insert(info.account_id.clone()) {
                out.push(info);
            }
        }
    }
    Ok(out)
}

impl GoogleAdapter {
    pub fn new(
        db: Arc<Db>,
        secrets: Arc<dyn SecretStore>,
        endpoints: GoogleEndpoints,
    ) -> Arc<GoogleAdapter> {
        Self::with_retry(db, secrets, endpoints, RetryPolicy::default())
    }

    /// Like [`GoogleAdapter::new`], with a different backoff (tests use milliseconds).
    pub fn with_retry(
        db: Arc<Db>,
        secrets: Arc<dyn SecretStore>,
        endpoints: GoogleEndpoints,
        retry: RetryPolicy,
    ) -> Arc<GoogleAdapter> {
        Self::with_tools(db, secrets, endpoints, retry, tools::all())
    }

    pub(crate) fn with_tools(
        db: Arc<Db>,
        secrets: Arc<dyn SecretStore>,
        endpoints: GoogleEndpoints,
        retry: RetryPolicy,
        tools: Vec<GTool>,
    ) -> Arc<GoogleAdapter> {
        Arc::new(GoogleAdapter {
            db,
            secrets,
            endpoints,
            retry,
            clients: Mutex::new(ClientCache::default()),
            tools: tools.into_iter().map(|t| (t.spec(), t)).collect(),
            poll: Mutex::new(UploadPoll::default()),
            seen_files: Mutex::new(HashMap::new()),
        })
    }

    /// Tests only: reads the video upload status every `every` and waits at
    /// most `max` (instead of 15 s and the input's `wait_minutes`).
    #[doc(hidden)]
    pub fn set_poll(&self, every: std::time::Duration, max: std::time::Duration) {
        *self.poll.lock().unwrap_or_else(|e| e.into_inner()) = UploadPoll {
            every,
            max: Some(max),
        };
    }

    pub(crate) fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub(crate) fn secrets(&self) -> &dyn SecretStore {
        self.secrets.as_ref()
    }

    pub(crate) fn endpoints(&self) -> &GoogleEndpoints {
        &self.endpoints
    }

    fn lock_seen_files(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, tools::assets::FileStamp>> {
        self.seen_files.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_clients(&self) -> std::sync::MutexGuard<'_, ClientCache> {
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drops the cached client (credentials replaced or connection removed).
    pub(crate) fn forget_client(&self, connection_id: &str) {
        let mut cache = self.lock_clients();
        cache.clients.remove(connection_id);
        *cache
            .generation
            .entry(connection_id.to_string())
            .or_default() += 1;
    }

    /// Google connection with its `extra` already parsed; otherwise `UNKNOWN_CONNECTION`.
    pub(crate) fn connection(
        &self,
        connection_id: &str,
    ) -> Result<(ConnectionMeta, ConnectionExtra), ToolError> {
        let meta = self
            .db
            .get_connection(connection_id)
            .map_err(internal)?
            .filter(|c| c.platform == Platform::GoogleAds)
            .ok_or_else(|| RoutingError::UnknownConnection(connection_id.to_string()))?;
        let extra = ConnectionExtra::from_meta(&meta)?;
        Ok((meta, extra))
    }

    fn secret(&self, connection_id: &str, name: &str) -> Result<Secret, ToolError> {
        self.secrets
            .get(&connection_secret_key(connection_id, name))
            .map_err(internal)?
            .filter(|v| !v.is_empty())
            .map(Secret::new)
            .ok_or_else(|| {
                ToolError::new(
                    codes::CONNECTION_NEEDS_RECONNECT,
                    format!(
                        "The credentials of connection {connection_id} are not in the secret store."
                    ),
                )
                .with_suggestion("Remove and add the connection again in AdsConsole → Connections.")
            })
    }

    /// The connection's stored OAuth client.
    pub(crate) fn stored_creds(
        &self,
        connection_id: &str,
        project_id: &str,
    ) -> Result<ClientCreds, ToolError> {
        Ok(ClientCreds {
            client_id: self.secret(connection_id, CLIENT_ID)?.expose().to_string(),
            client_secret: self.secret(connection_id, CLIENT_SECRET)?,
            project_id: project_id.to_string(),
        })
    }

    fn client(
        &self,
        connection_id: &str,
        extra: &ConnectionExtra,
    ) -> Result<GoogleAdsClient, ToolError> {
        loop {
            let generation = {
                let cache = self.lock_clients();
                if let Some(c) = cache.clients.get(connection_id) {
                    return Ok(c.clone());
                }
                cache.generation.get(connection_id).copied().unwrap_or(0)
            };
            let creds = self.stored_creds(connection_id, &extra.project_id)?;
            let refresh = self.secret(connection_id, REFRESH_TOKEN)?;
            let tokens = Arc::new(TokenManager::new(self.endpoints.clone(), creds, refresh));
            let meter = Arc::new(DbMeter {
                db: self.db.clone(),
                project_id: extra.project_id.clone(),
            });
            let client =
                GoogleAdsClient::new(self.endpoints.clone(), tokens, meter).with_retry(self.retry);
            let mut cache = self.lock_clients();
            // Secrets replaced while building: build again with the current ones.
            if cache.generation.get(connection_id).copied().unwrap_or(0) != generation {
                continue;
            }
            // If another call built one at the same time, the first one stays (same cached token).
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
            .accounts_for(Platform::GoogleAds, account_id)
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

    /// Context of the call's account, from the account cache and the connection.
    pub fn ctx(&self, call: &Call) -> Result<Ctx, ToolError> {
        let (meta, extra) = self.connection(&call.connection_id)?;
        if meta.status == ConnectionStatus::NeedsReconnect {
            return Err(RoutingError::NeedsReconnect(meta.connection_id).into());
        }
        let account = self.cached_account(&call.connection_id, &call.account_id)?;
        let client = self.client(&call.connection_id, &extra)?;
        Ok(Ctx {
            client,
            customer_id: account.account_id,
            // Only the MCC the account was listed under. The connection's `login_customer_id`
            // is not used here: on a directly accessible account, an unrelated MCC gives
            // `USER_PERMISSION_DENIED`.
            login: account.manager_id,
            currency: account.currency,
            project_id: extra.project_id,
            access_level: extra.access_level,
            connection_id: meta.connection_id,
            db: self.db.clone(),
            upload_poll: *self.poll.lock().unwrap_or_else(|e| e.into_inner()),
        })
    }

    /// Refreshes the account cache of `connection` (or of every `Ok` connection).
    /// A connection that fails keeps its old cache; it is only an error if all of them fail.
    pub(crate) async fn refresh_accounts(&self, connection: Option<&str>) -> Result<(), ToolError> {
        let targets = match connection {
            Some(c) => vec![c.to_string()],
            None => self.connections(),
        };
        refresh_connections(self, &self.db, Platform::GoogleAds, &targets)
            .await
            .map(|_| ())
    }

    fn tool(&self, name: &str) -> Option<&GTool> {
        self.tools
            .iter()
            .find(|(spec, _)| spec.name == name)
            .map(|(_, t)| t)
    }

    fn write_tool(&self, name: &str) -> Result<&dyn WriteTool, ToolError> {
        match self.tool(name) {
            Some(GTool::Write(tool)) => Ok(tool.as_ref()),
            _ => Err(not_implemented(name)),
        }
    }

    /// Diagnostics: the operations the write would send, summarized and without the image
    /// bytes (see [`plan::describe_operations`]). Sends nothing to Google.
    pub async fn describe_operations(&self, call: &Call) -> Result<Vec<String>, ToolError> {
        let (_, plan) = self.plan(call).await?;
        Ok(plan::describe_operations(&plan.operations))
    }

    /// Context and plan of a write (rebuilt at every step; see [`tools::WriteTool`]).
    async fn plan(&self, call: &Call) -> Result<(Ctx, WritePlan), ToolError> {
        let tool = self.write_tool(&call.tool)?;
        let ctx = self.ctx(call)?;
        let plan = tool.plan(&ctx, &call.input).await?;
        Ok((ctx, plan))
    }

    /// Google error in `validate`/`execute`, with the tool's adjustment
    /// ([`WriteTool::map_error`]).
    fn map_write_error(&self, tool: &str, e: ToolError) -> ToolError {
        match self.write_tool(tool) {
            Ok(t) => t.map_error(e),
            Err(_) => e,
        }
    }
}

/// The call's key in `seen_files`: connection, account, tool and input.
fn file_key(call: &Call) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        call.connection_id, call.account_id, call.tool, call.input
    )
}

fn not_implemented(tool: &str) -> ToolError {
    ToolError::new(codes::UNKNOWN_TOOL, format!("Unknown tool: {tool}"))
}

#[async_trait::async_trait]
impl PlatformAdapter for GoogleAdapter {
    fn platform(&self) -> Platform {
        Platform::GoogleAds
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let mut specs = vec![accounts::spec()];
        specs.extend(self.tools.iter().map(|(spec, _)| spec.clone()));
        specs
    }

    async fn list_accounts(&self, connection_id: &str) -> Result<Vec<AccountInfo>, ToolError> {
        let (meta, extra) = self.connection(connection_id)?;
        if meta.status == ConnectionStatus::NeedsReconnect {
            return Err(RoutingError::NeedsReconnect(meta.connection_id).into());
        }
        let client = self.client(connection_id, &extra)?;
        fetch_accounts(&client).await.map_err(|e| {
            let scope = connections::scope(self, connection_id, &extra);
            translate_in(e, Some(&scope), accounts::NAME)
        })
    }

    async fn read(&self, call: &Call) -> Result<Value, ToolError> {
        if call.tool == accounts::NAME {
            return accounts::run(self, &call.input).await;
        }
        let Some(GTool::Read(tool)) = self.tool(&call.tool) else {
            return Err(not_implemented(&call.tool));
        };
        let ctx = self.ctx(call)?;
        let mut out = tool.run(&ctx, &call.input).await?;
        if let Some(warning) = quota_warning(&ctx)
            && let Some(obj) = out.as_object_mut()
        {
            obj.insert("quota_warning".into(), Value::String(warning));
        }
        Ok(out)
    }

    async fn precheck(&self, call: &Call) -> Result<(), ToolError> {
        let account = self.cached_account(&call.connection_id, &call.account_id)?;
        match account.status.as_deref() {
            Some("ENABLED") => Ok(()),
            Some(MANAGER_STATUS) => Err(ToolError::new(
                codes::ACCOUNT_NOT_ACTIVE,
                "Manager accounts (MCC) have no campaigns; use a child account.",
            )
            .with_suggestion("Use list_accounts to see the child accounts (manager_id).")),
            other => Err(ToolError::new(
                codes::ACCOUNT_NOT_ACTIVE,
                format!(
                    "Account {} is not active in Google Ads (status {}).",
                    call.account_id,
                    other.unwrap_or("unknown")
                ),
            )),
        }
    }

    async fn validate(&self, call: &Call) -> Result<Validated, ToolError> {
        let (ctx, plan) = self.plan(call).await?;
        let validated = plan::validate(&ctx, &call.tool, &plan)
            .await
            .map_err(|e| self.map_write_error(&call.tool, e))?;
        if let Some(DirectExec::YoutubeUpload(v)) = &plan.direct {
            self.lock_seen_files().insert(file_key(call), v.stamp);
        }
        Ok(validated)
    }

    async fn snapshot(&self, call: &Call) -> Result<Value, ToolError> {
        let (ctx, plan) = self.plan(call).await?;
        plan::snapshot(&ctx, &call.tool, &plan).await
    }

    async fn execute(&self, call: &Call, before: &Value) -> Result<Applied, ToolError> {
        let (ctx, plan) = self.plan(call).await?;
        if let Some(DirectExec::YoutubeUpload(v)) = &plan.direct {
            let seen = self.lock_seen_files().remove(&file_key(call));
            tools::assets::check_unchanged(seen, v)?;
        }
        let applied = plan::execute(&ctx, &call.tool, &plan, before)
            .await
            .map_err(|e| self.map_write_error(&call.tool, e))?;
        Ok(match self.write_tool(&call.tool) {
            Ok(t) => t.map_applied(applied),
            Err(_) => applied,
        })
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
            tool: String::new(),
            input: Value::Null,
        };
        let ctx = self.ctx(&call)?;
        plan::current_state(&ctx, "current_state", target).await
    }

    fn connections(&self) -> Vec<String> {
        match self.db.list_connections(Platform::GoogleAds) {
            Ok(list) => list
                .into_iter()
                .filter(|c| c.status == ConnectionStatus::Ok)
                .map(|c| c.connection_id)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "failed to list google connections");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use adsconsole_core::config::ToolGroup;
    use adsconsole_core::secrets::MemoryStore;
    use async_trait::async_trait;
    use schemars::JsonSchema;
    use serde_json::json;

    use super::*;
    use crate::google::connections::{AccessLevel, save_connection};
    use crate::google::tools::ReadTool;

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct NoInput {}

    struct Echo;

    #[async_trait]
    impl ReadTool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec::read::<NoInput>("echo", "Echo.", ToolGroup::Read)
        }
        async fn run(&self, ctx: &Ctx, _input: &Value) -> Result<Value, ToolError> {
            Ok(json!({ "customer": ctx.customer_id }))
        }
    }

    fn adapter(ops: u32) -> Arc<GoogleAdapter> {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let adapter = GoogleAdapter::with_tools(
            db.clone(),
            Arc::new(MemoryStore::default()),
            GoogleEndpoints::default(),
            RetryPolicy::default(),
            vec![GTool::Read(Box::new(Echo))],
        );
        let creds = ClientCreds {
            client_id: "cid".into(),
            client_secret: Secret::new("s"),
            project_id: "p".into(),
        };
        save_connection(
            &adapter,
            "g1",
            "G1",
            &creds,
            &Secret::new("r"),
            None,
            AccessLevel::Explorer,
        )
        .unwrap();
        let acct = AccountInfo {
            account_id: "111".into(),
            name: "Account".into(),
            currency: "BRL".into(),
            timezone: None,
            status: Some("ENABLED".into()),
            manager_id: None,
        };
        db.replace_accounts(Platform::GoogleAds, "g1", &[acct])
            .unwrap();
        db.record_google_ops("p", ops, Utc::now()).unwrap();
        adapter
    }

    fn echo_call() -> Call {
        Call {
            connection_id: "g1".into(),
            account_id: "111".into(),
            tool: "echo".into(),
            input: json!({}),
        }
    }

    #[tokio::test]
    async fn reads_carry_quota_warning_from_80_percent() {
        let out = adapter(2304).read(&echo_call()).await.unwrap();
        assert_eq!(out["customer"], "111");
        assert_eq!(
            out["quota_warning"],
            "This connection has used 2304 of 2880 operations in the last 24 h (Explorer access level)."
        );
        let out = adapter(2000).read(&echo_call()).await.unwrap();
        assert!(out.get("quota_warning").is_none(), "{out}");
    }

    /// Store that, on the first refresh token read, simulates a concurrent reconnect:
    /// replaces the token and calls `forget_client`.
    struct RacingStore {
        inner: MemoryStore,
        adapter: std::sync::OnceLock<std::sync::Weak<GoogleAdapter>>,
        refresh_reads: std::sync::atomic::AtomicUsize,
    }

    impl SecretStore for RacingStore {
        fn get(&self, key: &str) -> Result<Option<String>, adsconsole_core::CoreError> {
            let value = self.inner.get(key)?;
            if key.ends_with("/refresh_token")
                && self
                    .refresh_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
            {
                self.inner.set(key, "r2")?;
                if let Some(a) = self.adapter.get().and_then(|w| w.upgrade()) {
                    a.forget_client("g1");
                }
            }
            Ok(value)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), adsconsole_core::CoreError> {
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<(), adsconsole_core::CoreError> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn client_built_before_forget_is_not_cached() {
        let store = Arc::new(RacingStore {
            inner: MemoryStore::default(),
            adapter: std::sync::OnceLock::new(),
            refresh_reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let adapter = GoogleAdapter::new(
            Arc::new(Db::open_in_memory().unwrap()),
            store.clone(),
            GoogleEndpoints::default(),
        );
        let _ = store.adapter.set(Arc::downgrade(&adapter));
        let creds = ClientCreds {
            client_id: "cid".into(),
            client_secret: Secret::new("s"),
            project_id: "p".into(),
        };
        save_connection(
            &adapter,
            "g1",
            "G1",
            &creds,
            &Secret::new("r1"),
            None,
            AccessLevel::Explorer,
        )
        .unwrap();
        let (_, extra) = adapter.connection("g1").unwrap();
        adapter.client("g1", &extra).unwrap();
        // First build discarded (generation changed); the second one read the new token.
        assert_eq!(
            store
                .refresh_reads
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(adapter.lock_clients().clients.contains_key("g1"));
        adapter.client("g1", &extra).unwrap();
        assert_eq!(
            store
                .refresh_reads
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the second call uses the cache"
        );
    }

    #[test]
    fn tools_lists_list_accounts_without_account() {
        let specs = adapter(0).tools();
        let list = specs.iter().find(|s| s.name == "list_accounts").unwrap();
        assert!(!list.takes_account);
        assert!(list.output_schema.is_some());
        assert!(specs.iter().any(|s| s.name == "echo"));
    }
}
