//! In-memory fake platform: the base of the engine tests and the app demo mode.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use adsconsole_core::config::ToolGroup;
use adsconsole_core::db::{AccountInfo, ConnectionMeta, ConnectionStatus, Db, InverseOp};
use adsconsole_core::{CoreError, Decimal, Platform, Status};
use chrono::Utc;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::adapter::{AccountRef, Applied, Call, PlatformAdapter, Progress, ToolSpec, Validated};
use crate::error::{ToolError, codes};

const CONNECTIONS: [&str; 2] = ["conn_a", "conn_b"];
const NAME_MAX_CHARS: usize = 255;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FakeCampaign {
    #[serde(rename = "campaign_id")]
    pub id: String,
    pub name: String,
    pub status: Status,
    #[serde(with = "rust_decimal::serde::float")]
    pub daily_budget: Decimal,
}

impl FakeCampaign {
    fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListAccountsInput {
    /// Connection id; without it, lists the accounts of all of them.
    #[serde(default)]
    #[allow(dead_code)]
    connection: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetCampaignsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
}

#[derive(Debug, Serialize, JsonSchema)]
#[allow(dead_code)]
struct CampaignOut {
    campaign_id: String,
    name: String,
    status: Status,
    daily_budget: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[allow(dead_code)]
struct GetCampaignsOutput {
    campaigns: Vec<CampaignOut>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateCampaignInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Daily budget in the account currency.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateCampaignInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<Status>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveCampaignInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RawMutateInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    #[serde(flatten)]
    #[allow(dead_code)]
    payload: Map<String, Value>,
}

#[derive(Default)]
struct FakeState {
    accounts: BTreeMap<String, Vec<AccountInfo>>,
    campaigns: BTreeMap<String, Vec<FakeCampaign>>,
    next_id: u64,
    fail_next: Option<ToolError>,
    fail_next_current_state: Option<ToolError>,
    fail_next_read: Option<ToolError>,
    failing_connections: BTreeSet<String>,
    /// `execute_in_steps`: ids to "create" in steps and, optionally, the step that fails.
    steps: Option<(Vec<String>, Option<usize>)>,
    log: Vec<String>,
}

impl FakeState {
    fn campaign(&self, account: &str, id: &str) -> Option<&FakeCampaign> {
        self.campaigns.get(account)?.iter().find(|c| c.id == id)
    }

    fn campaign_mut(&mut self, account: &str, id: &str) -> Option<&mut FakeCampaign> {
        self.campaigns
            .get_mut(account)?
            .iter_mut()
            .find(|c| c.id == id)
    }

    fn existing(&self, account: &str, id: &str) -> Result<&FakeCampaign, ToolError> {
        self.campaign(account, id).ok_or_else(|| not_found(id))
    }
}

pub struct FakePlatform {
    platform: Platform,
    state: Mutex<FakeState>,
}

fn not_found(id: &str) -> ToolError {
    ToolError::new(codes::NOT_FOUND, format!("Campaign {id} not found")).with_field("campaign_id")
}

fn parse<T: DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone())
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Invalid input: {e}")))
}

fn check_name(name: &str) -> Result<(), ToolError> {
    if name.trim().is_empty() || name.chars().count() > NAME_MAX_CHARS {
        return Err(ToolError::new(
            codes::INVALID_INPUT,
            format!("The name must be between 1 and {NAME_MAX_CHARS} characters"),
        )
        .with_field("name"));
    }
    Ok(())
}

fn check_budget(budget: Decimal) -> Result<(), ToolError> {
    let min = Decimal::new(2540, 2);
    if budget < min {
        return Err(ToolError::new(
            codes::BUDGET_TOO_LOW,
            format!("Daily budget {budget} is below the minimum"),
        )
        .with_field("daily_budget")
        .with_suggestion("The daily minimum is 25.40 in the account currency"));
    }
    Ok(())
}

fn unknown_tool(tool: &str) -> ToolError {
    ToolError::new(codes::UNKNOWN_TOOL, format!("Unknown tool: {tool}"))
}

impl FakePlatform {
    /// Already seeded: `conn_a` with 111 (ACTIVE, BRL), 222 (CLOSED, BRL) and 333 (ACTIVE, USD);
    /// `conn_b` with 333 and 444 (ACTIVE, BRL). On Meta, the ids carry the `act_` prefix.
    pub fn new(p: Platform) -> Arc<FakePlatform> {
        let id = |raw: &str| match p {
            Platform::GoogleAds => raw.to_string(),
            Platform::MetaAds => format!("act_{raw}"),
        };
        let acct = |raw: &str, status: &str, currency: &str| AccountInfo {
            account_id: id(raw),
            name: format!("Account {raw}"),
            currency: currency.into(),
            timezone: Some("America/Sao_Paulo".into()),
            status: Some(status.into()),
            manager_id: None,
        };
        let mut state = FakeState::default();
        state.accounts.insert(
            "conn_a".into(),
            vec![
                acct("111", "ACTIVE", "BRL"),
                acct("222", "CLOSED", "BRL"),
                acct("333", "ACTIVE", "USD"),
            ],
        );
        state.accounts.insert(
            "conn_b".into(),
            vec![acct("333", "ACTIVE", "USD"), acct("444", "ACTIVE", "BRL")],
        );
        Arc::new(FakePlatform {
            platform: p,
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, state: &mut FakeState, step: &str, tool: &str) {
        state.log.push(format!("{step}:{tool}"));
    }

    /// Writes `conn_a` and `conn_b` to `connections_meta` (status Ok, no default).
    pub fn seed_connections(&self, db: &Db) -> Result<(), CoreError> {
        for (cid, name) in [("conn_a", "Connection A"), ("conn_b", "Connection B")] {
            db.upsert_connection(&ConnectionMeta {
                connection_id: cid.into(),
                platform: self.platform,
                name: name.into(),
                is_default: false,
                status: ConnectionStatus::Ok,
                expires_at: None,
                extra: json!({}),
                created_at: Utc::now(),
            })?;
        }
        Ok(())
    }

    pub fn campaign(&self, account: &str, id: &str) -> Option<FakeCampaign> {
        self.lock().campaign(account, id).cloned()
    }

    /// Simulates an edit made outside the app (to test an undo conflict).
    pub fn set_campaign_name_externally(&self, account: &str, id: &str, name: &str) {
        if let Some(c) = self.lock().campaign_mut(account, id) {
            c.name = name.into();
        }
    }

    /// The next `execute` fails with `e`.
    pub fn fail_next_execute(&self, e: ToolError) {
        self.lock().fail_next = Some(e);
    }

    /// The next `execute` "creates" `ids` one per step, calling `progress.created` after
    /// each one; with `fail_after = Some(n)`, it fails after creating `n`.
    pub fn execute_in_steps(&self, ids: Vec<&str>, fail_after: Option<usize>) {
        self.lock().steps = Some((ids.into_iter().map(str::to_string).collect(), fail_after));
    }

    /// The next `current_state` fails with `e`.
    pub fn fail_next_current_state(&self, e: ToolError) {
        self.lock().fail_next_current_state = Some(e);
    }

    /// The next read fails with `e`.
    pub fn fail_next_read(&self, e: ToolError) {
        self.lock().fail_next_read = Some(e);
    }

    /// From now on, `list_accounts` of the connection fails (like an expired token).
    pub fn fail_list_accounts(&self, connection: &str) {
        self.lock()
            .failing_connections
            .insert(connection.to_string());
    }

    pub fn remove_account_from(&self, connection: &str, account: &str) {
        if let Some(list) = self.lock().accounts.get_mut(connection) {
            list.retain(|a| a.account_id != account);
        }
    }

    pub fn add_account_to(&self, connection: &str, a: AccountInfo) {
        self.lock()
            .accounts
            .entry(connection.to_string())
            .or_default()
            .push(a);
    }

    /// Entries such as `"validate:create_campaign"`, `"execute:update_campaign"`,
    /// `"list_accounts:conn_a"` and `"current_state:c1"`.
    pub fn log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    fn accounts_of(state: &FakeState, connection_id: &str) -> Result<Vec<AccountInfo>, ToolError> {
        state.accounts.get(connection_id).cloned().ok_or_else(|| {
            ToolError::new(
                codes::UNKNOWN_CONNECTION,
                format!("Unknown connection: {connection_id}"),
            )
        })
    }
}

#[async_trait::async_trait]
impl PlatformAdapter for FakePlatform {
    fn platform(&self) -> Platform {
        self.platform
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::read::<ListAccountsInput>(
                "list_accounts",
                "Lists the ad accounts of the given connection or of all of them.",
                ToolGroup::Read,
            )
            .no_account(),
            ToolSpec::read::<GetCampaignsInput>(
                "get_campaigns",
                "Lists the account's campaigns.",
                ToolGroup::Read,
            )
            .output::<GetCampaignsOutput>(),
            ToolSpec::write::<CreateCampaignInput>(
                "create_campaign",
                "Creates a paused campaign.",
                ToolGroup::Create,
                true,
            ),
            ToolSpec::write::<UpdateCampaignInput>(
                "update_campaign",
                "Changes the name, status or daily budget of a campaign.",
                ToolGroup::Edit,
                true,
            )
            .idempotent(),
            ToolSpec::write::<RemoveCampaignInput>(
                "remove_campaign",
                "Removes a campaign.",
                ToolGroup::Edit,
                false,
            )
            .destructive(),
            ToolSpec::write::<RawMutateInput>(
                "raw_mutate",
                "Sends a raw mutation to the platform.",
                ToolGroup::Escape,
                false,
            ),
        ]
    }

    async fn list_accounts(&self, connection_id: &str) -> Result<Vec<AccountInfo>, ToolError> {
        let mut st = self.lock();
        self.record(&mut st, "list_accounts", connection_id);
        if st.failing_connections.contains(connection_id) {
            return Err(ToolError::new(
                codes::CONNECTION_NEEDS_RECONNECT,
                format!("Connection {connection_id} needs to be reconnected"),
            ));
        }
        Self::accounts_of(&st, connection_id)
    }

    async fn read(&self, call: &Call) -> Result<Value, ToolError> {
        let mut st = self.lock();
        self.record(&mut st, "read", &call.tool);
        if let Some(e) = st.fail_next_read.take() {
            return Err(e);
        }
        match call.tool.as_str() {
            "list_accounts" => {
                let conns: Vec<String> = if call.connection_id.is_empty() {
                    st.accounts.keys().cloned().collect()
                } else {
                    vec![call.connection_id.clone()]
                };
                let mut out = Vec::new();
                for cid in conns {
                    for a in Self::accounts_of(&st, &cid)? {
                        let mut v = serde_json::to_value(&a).unwrap_or(Value::Null);
                        v["connection_id"] = json!(cid);
                        out.push(v);
                    }
                }
                Ok(json!({ "accounts": out }))
            }
            "get_campaigns" => {
                let list: Vec<Value> = st
                    .campaigns
                    .get(&call.account_id)
                    .map(|cs| cs.iter().map(FakeCampaign::to_value).collect())
                    .unwrap_or_default();
                Ok(json!({ "campaigns": list }))
            }
            other => Err(unknown_tool(other)),
        }
    }

    async fn precheck(&self, call: &Call) -> Result<(), ToolError> {
        let mut st = self.lock();
        self.record(&mut st, "precheck", &call.tool);
        let account = Self::accounts_of(&st, &call.connection_id)?
            .into_iter()
            .find(|a| a.account_id == call.account_id)
            .ok_or_else(|| {
                ToolError::new(
                    codes::ACCOUNT_NOT_FOUND,
                    format!("Account {} not found", call.account_id),
                )
            })?;
        match account.status.as_deref() {
            Some("ACTIVE") => Ok(()),
            other => Err(ToolError::new(
                codes::ACCOUNT_NOT_ACTIVE,
                format!(
                    "Account {} is not active (status {})",
                    call.account_id,
                    other.unwrap_or("unknown")
                ),
            )),
        }
    }

    async fn validate(&self, call: &Call) -> Result<Validated, ToolError> {
        let mut st = self.lock();
        self.record(&mut st, "validate", &call.tool);
        let summary = match call.tool.as_str() {
            "create_campaign" => {
                let i: CreateCampaignInput = parse(&call.input)?;
                check_name(&i.name)?;
                check_budget(i.daily_budget)?;
                format!(
                    "Create campaign \"{}\" (paused) with a daily budget of {}",
                    i.name, i.daily_budget
                )
            }
            "update_campaign" => {
                let i: UpdateCampaignInput = parse(&call.input)?;
                st.existing(&call.account_id, &i.campaign_id)?;
                if let Some(n) = &i.name {
                    check_name(n)?;
                }
                if let Some(b) = i.daily_budget {
                    check_budget(b)?;
                }
                format!("Update campaign {}", i.campaign_id)
            }
            "remove_campaign" => {
                let i: RemoveCampaignInput = parse(&call.input)?;
                st.existing(&call.account_id, &i.campaign_id)?;
                format!("Remove campaign {}", i.campaign_id)
            }
            "raw_mutate" => {
                parse::<RawMutateInput>(&call.input)?;
                "Raw mutation".to_string()
            }
            other => return Err(unknown_tool(other)),
        };
        Ok(Validated {
            summary,
            warning: None,
        })
    }

    async fn snapshot(&self, call: &Call) -> Result<Value, ToolError> {
        let mut st = self.lock();
        self.record(&mut st, "snapshot", &call.tool);
        match call.tool.as_str() {
            "update_campaign" => {
                let i: UpdateCampaignInput = parse(&call.input)?;
                let c = st.existing(&call.account_id, &i.campaign_id)?;
                Ok(changed_fields(c, &i))
            }
            "remove_campaign" => {
                let i: RemoveCampaignInput = parse(&call.input)?;
                Ok(st.existing(&call.account_id, &i.campaign_id)?.to_value())
            }
            "create_campaign" | "raw_mutate" => Ok(json!({})),
            other => Err(unknown_tool(other)),
        }
    }

    async fn execute_tracked(
        &self,
        call: &Call,
        before: &Value,
        progress: &dyn Progress,
    ) -> Result<Applied, ToolError> {
        let steps = self.lock().steps.take();
        if let Some((ids, fail_after)) = steps {
            let mut created = Vec::new();
            for (i, id) in ids.iter().enumerate() {
                if fail_after == Some(i) {
                    return Err(ToolError::new(codes::EXECUTION_FAILED, "step failed"));
                }
                created.push(json!({ "key": format!("step{i}"), "id": id, "kind": "fake" }));
                progress.created(&Value::Array(created.clone()));
            }
        }
        self.execute(call, before).await
    }

    async fn execute(&self, call: &Call, _before: &Value) -> Result<Applied, ToolError> {
        let mut st = self.lock();
        self.record(&mut st, "execute", &call.tool);
        if let Some(e) = st.fail_next.take() {
            return Err(e);
        }
        match call.tool.as_str() {
            "create_campaign" => {
                let i: CreateCampaignInput = parse(&call.input)?;
                st.next_id += 1;
                let c = FakeCampaign {
                    id: format!("c{}", st.next_id),
                    name: i.name,
                    status: Status::Paused,
                    daily_budget: i.daily_budget,
                };
                let target = json!({ "campaign_id": c.id });
                let after = c.to_value();
                st.campaigns
                    .entry(call.account_id.clone())
                    .or_default()
                    .push(c);
                Ok(Applied {
                    after,
                    inverse: Some(InverseOp {
                        tool: "remove_campaign".into(),
                        input: target.clone(),
                        target,
                    }),
                })
            }
            "update_campaign" => {
                let i: UpdateCampaignInput = parse(&call.input)?;
                let c = st
                    .campaign_mut(&call.account_id, &i.campaign_id)
                    .ok_or_else(|| not_found(&i.campaign_id))?;
                let restore = changed_fields(c, &i);
                if let Some(n) = &i.name {
                    c.name = n.clone();
                }
                if let Some(s) = i.status {
                    c.status = s;
                }
                if let Some(b) = i.daily_budget {
                    c.daily_budget = b;
                }
                Ok(Applied {
                    after: c.to_value(),
                    inverse: Some(InverseOp {
                        tool: "update_campaign".into(),
                        input: restore,
                        target: json!({ "campaign_id": i.campaign_id }),
                    }),
                })
            }
            "remove_campaign" => {
                let i: RemoveCampaignInput = parse(&call.input)?;
                let list = st
                    .campaigns
                    .get_mut(&call.account_id)
                    .ok_or_else(|| not_found(&i.campaign_id))?;
                let pos = list
                    .iter()
                    .position(|c| c.id == i.campaign_id)
                    .ok_or_else(|| not_found(&i.campaign_id))?;
                list.remove(pos);
                Ok(Applied {
                    after: json!({ "campaign_id": i.campaign_id, "status": "REMOVED" }),
                    inverse: None,
                })
            }
            "raw_mutate" => Ok(Applied {
                after: call.input.clone(),
                inverse: None,
            }),
            other => Err(unknown_tool(other)),
        }
    }

    async fn current_state(
        &self,
        _connection_id: &str,
        account_id: &str,
        target: &Value,
    ) -> Result<Value, ToolError> {
        let id = target["campaign_id"].as_str().unwrap_or_default();
        let mut st = self.lock();
        self.record(&mut st, "current_state", id);
        if let Some(e) = st.fail_next_current_state.take() {
            return Err(e);
        }
        Ok(st
            .campaign(account_id, id)
            .map(FakeCampaign::to_value)
            .unwrap_or(Value::Null))
    }

    fn connections(&self) -> Vec<String> {
        CONNECTIONS.iter().map(|c| c.to_string()).collect()
    }
}

/// `campaign_id` plus the current values of the fields the update will change.
fn changed_fields(c: &FakeCampaign, i: &UpdateCampaignInput) -> Value {
    let full = c.to_value();
    let mut out = Map::new();
    out.insert("campaign_id".into(), json!(c.id));
    for (field, present) in [
        ("name", i.name.is_some()),
        ("status", i.status.is_some()),
        ("daily_budget", i.daily_budget.is_some()),
    ] {
        if present {
            out.insert(field.into(), full[field].clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Call, PlatformAdapter};
    use crate::error::codes;
    use adsconsole_core::db::Db;
    use rust_decimal_macros::dec;
    use serde_json::json;

    fn call(account: &str, tool: &str, input: Value) -> Call {
        Call {
            connection_id: "conn_a".into(),
            account_id: account.into(),
            tool: tool.into(),
            input,
        }
    }

    #[tokio::test]
    async fn fake_validate_rules() {
        let f = FakePlatform::new(Platform::GoogleAds);
        let c = call(
            "111",
            "create_campaign",
            json!({"account_id": "111", "name": "X", "daily_budget": 10}),
        );
        let e = f.validate(&c).await.unwrap_err();
        assert_eq!(e.code, codes::BUDGET_TOO_LOW);
        assert_eq!(e.field.as_deref(), Some("daily_budget"));
        assert_eq!(
            e.suggestion.as_deref(),
            Some("The daily minimum is 25.40 in the account currency")
        );

        let c = call(
            "111",
            "create_campaign",
            json!({"account_id": "111", "name": "", "daily_budget": 30}),
        );
        let e = f.validate(&c).await.unwrap_err();
        assert_eq!(e.code, codes::INVALID_INPUT);
        assert_eq!(e.field.as_deref(), Some("name"));

        let long = "a".repeat(256);
        let c = call(
            "111",
            "create_campaign",
            json!({"account_id": "111", "name": long, "daily_budget": 30}),
        );
        assert_eq!(
            f.validate(&c).await.unwrap_err().field.as_deref(),
            Some("name")
        );

        let ok = call(
            "111",
            "create_campaign",
            json!({"account_id": "111", "name": "Ok", "daily_budget": 25.40}),
        );
        f.validate(&ok).await.unwrap();

        let c = call(
            "111",
            "update_campaign",
            json!({"account_id": "111", "campaign_id": "c99", "name": "Y"}),
        );
        assert_eq!(f.validate(&c).await.unwrap_err().code, codes::NOT_FOUND);

        let c = call("222", "create_campaign", json!({"account_id": "222"}));
        assert_eq!(
            f.precheck(&c).await.unwrap_err().code,
            codes::ACCOUNT_NOT_ACTIVE
        );
        f.precheck(&call(
            "111",
            "create_campaign",
            json!({"account_id": "111"}),
        ))
        .await
        .unwrap();
        assert!(f.log().contains(&"validate:create_campaign".to_string()));
    }

    #[tokio::test]
    async fn fake_create_is_paused_with_inverse() {
        let f = FakePlatform::new(Platform::GoogleAds);
        let c = call(
            "111",
            "create_campaign",
            json!({"account_id": "111", "name": "New", "daily_budget": 30.5}),
        );
        let before = f.snapshot(&c).await.unwrap();
        assert_eq!(before, json!({}));
        let applied = f.execute(&c, &before).await.unwrap();
        assert_eq!(applied.after["status"], "PAUSED");
        assert_eq!(applied.after["daily_budget"], json!(30.5));
        let id = applied.after["campaign_id"].as_str().unwrap().to_string();
        assert!(id.starts_with('c'));
        let inv = applied.inverse.unwrap();
        assert_eq!(inv.tool, "remove_campaign");
        assert_eq!(inv.input, json!({"campaign_id": id}));
        assert_eq!(inv.target, json!({"campaign_id": id}));
        let camp = f.campaign("111", &id).unwrap();
        assert_eq!(camp.status, Status::Paused);
        assert_eq!(camp.daily_budget, dec!(30.5));
        assert!(f.log().contains(&"execute:create_campaign".to_string()));
    }

    #[tokio::test]
    async fn fake_update_snapshot_inverse_and_state() {
        let f = FakePlatform::new(Platform::GoogleAds);
        let create = call(
            "111",
            "create_campaign",
            json!({"account_id": "111", "name": "A", "daily_budget": 30}),
        );
        let id = f.execute(&create, &json!({})).await.unwrap().after["campaign_id"]
            .as_str()
            .unwrap()
            .to_string();
        let up = call(
            "111",
            "update_campaign",
            json!({"account_id": "111", "campaign_id": id, "name": "B"}),
        );
        f.validate(&up).await.unwrap();
        let before = f.snapshot(&up).await.unwrap();
        assert_eq!(before, json!({"campaign_id": id, "name": "A"}));
        let applied = f.execute(&up, &before).await.unwrap();
        assert_eq!(applied.after["name"], "B");
        let inv = applied.inverse.unwrap();
        assert_eq!(inv.tool, "update_campaign");
        assert_eq!(inv.input, json!({"campaign_id": id, "name": "A"}));
        assert_eq!(inv.target, json!({"campaign_id": id}));

        f.set_campaign_name_externally("111", &id, "External");
        let state = f
            .current_state("conn_a", "111", &json!({"campaign_id": id}))
            .await
            .unwrap();
        assert_eq!(
            state,
            json!({"campaign_id": id, "name": "External", "status": "PAUSED", "daily_budget": 30.0})
        );
        let missing = f
            .current_state("conn_a", "111", &json!({"campaign_id": "c999"}))
            .await
            .unwrap();
        assert_eq!(missing, Value::Null);

        let rm = call(
            "111",
            "remove_campaign",
            json!({"account_id": "111", "campaign_id": id}),
        );
        let applied = f.execute(&rm, &json!({})).await.unwrap();
        assert!(applied.inverse.is_none());
        assert!(f.campaign("111", &id).is_none());
    }

    #[tokio::test]
    async fn fake_accounts_connections_and_helpers() {
        let g = FakePlatform::new(Platform::GoogleAds);
        assert_eq!(g.connections(), vec!["conn_a", "conn_b"]);
        let a = g.list_accounts("conn_a").await.unwrap();
        let ids: Vec<_> = a.iter().map(|x| x.account_id.as_str()).collect();
        assert_eq!(ids, vec!["111", "222", "333"]);
        assert_eq!(a[1].status.as_deref(), Some("CLOSED"));
        assert_eq!(a[2].currency, "USD");

        let m = FakePlatform::new(Platform::MetaAds);
        let b = m.list_accounts("conn_b").await.unwrap();
        let ids: Vec<_> = b.iter().map(|x| x.account_id.as_str()).collect();
        assert_eq!(ids, vec!["act_333", "act_444"]);
        assert_eq!(m.platform(), Platform::MetaAds);

        g.remove_account_from("conn_b", "444");
        g.add_account_to(
            "conn_b",
            AccountInfo {
                account_id: "555".into(),
                name: "New".into(),
                currency: "BRL".into(),
                timezone: None,
                status: Some("ACTIVE".into()),
                manager_id: None,
            },
        );
        let ids: Vec<_> = g
            .list_accounts("conn_b")
            .await
            .unwrap()
            .into_iter()
            .map(|x| x.account_id)
            .collect();
        assert_eq!(ids, vec!["333", "555"]);

        // list_accounts as a tool: without a connection, all of them.
        let all = g
            .read(&Call {
                connection_id: "".into(),
                account_id: "".into(),
                tool: "list_accounts".into(),
                input: json!({}),
            })
            .await
            .unwrap();
        assert_eq!(all["accounts"].as_array().unwrap().len(), 5);

        g.fail_next_execute(ToolError::new(codes::EXECUTION_FAILED, "failed"));
        let c = call("111", "raw_mutate", json!({"account_id": "111", "x": 1}));
        assert_eq!(
            g.execute(&c, &json!({})).await.unwrap_err().code,
            codes::EXECUTION_FAILED
        );
        let echo = g.execute(&c, &json!({})).await.unwrap();
        assert_eq!(echo.after, json!({"account_id": "111", "x": 1}));
        assert!(echo.inverse.is_none());

        let db = Db::open_in_memory().unwrap();
        g.seed_connections(&db).unwrap();
        let conns = db.list_connections(Platform::GoogleAds).unwrap();
        assert_eq!(conns.len(), 2);
        assert!(conns.iter().all(|c| !c.is_default));
    }

    #[test]
    fn routing_errors_map_to_codes() {
        use adsconsole_core::routing::RoutingError;
        let e: ToolError = RoutingError::AccountNotFound("9".into()).into();
        assert_eq!(e.code, codes::ACCOUNT_NOT_FOUND);
        assert_eq!(
            e.suggestion.as_deref(),
            Some("Use list_accounts to see the available accounts")
        );
        let e: ToolError = RoutingError::Ambiguous {
            account_id: "1".into(),
            connections: vec![
                ("conn_a".into(), "Connection A".into()),
                ("conn_b".into(), "Connection B".into()),
            ],
        }
        .into();
        assert_eq!(e.code, codes::AMBIGUOUS_ACCOUNT);
        assert_eq!(e.field.as_deref(), Some("connection"));
        assert_eq!(
            e.details,
            Some(json!({"connections": [
                {"connection_id": "conn_a", "name": "Connection A"},
                {"connection_id": "conn_b", "name": "Connection B"},
            ]}))
        );
        assert_eq!(
            e.suggestion.as_deref(),
            Some("Provide connection with one of these ids: conn_a, conn_b.")
        );
        let e: ToolError = RoutingError::NeedsReconnect("conn_a".into()).into();
        assert_eq!(e.code, codes::CONNECTION_NEEDS_RECONNECT);
        assert_eq!(
            e.suggestion.as_deref(),
            Some("Reconnect this connection in AdsConsole → Connections.")
        );
        let e: ToolError = RoutingError::UnknownConnection("x".into()).into();
        assert_eq!(
            e.suggestion.as_deref(),
            Some("Use list_accounts to see the available connections and accounts.")
        );
        let e: ToolError = RoutingError::Ambiguous {
            account_id: "1".into(),
            connections: vec![("a".into(), "A".into()), ("b".into(), "B".into())],
        }
        .into();
        let r = e.to_call_result();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(
            r.structured_content.unwrap()["error"]["code"],
            codes::AMBIGUOUS_ACCOUNT
        );
    }
}
