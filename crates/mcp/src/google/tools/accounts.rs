//! `list_accounts`: cached accounts, optionally refreshed from Google first.

use adsconsole_core::Platform;
use adsconsole_core::config::ToolGroup;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::ToolSpec;
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::GoogleAdapter;

pub(crate) const NAME: &str = "list_accounts";

#[derive(Debug, Deserialize, JsonSchema)]
struct ListAccountsInput {
    /// Connection id. Without it, lists the accounts of every connection.
    #[serde(default)]
    connection: Option<String>,
    /// Queries Google before answering (uses operations from the project quota).
    #[serde(default)]
    refresh: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AccountOut {
    account_id: String,
    name: String,
    currency: String,
    timezone: Option<String>,
    /// `ENABLED`, `CANCELED`, `SUSPENDED`, `CLOSED` or `MANAGER` (MCC, no campaigns).
    status: Option<String>,
    /// MCC through which the account is accessed.
    manager_id: Option<String>,
    connection: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListAccountsOutput {
    accounts: Vec<AccountOut>,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec::read::<ListAccountsInput>(
        NAME,
        "Lists the Google Ads accounts of each connection, including MCC child \
         accounts (manager_id). With refresh: true, queries Google first.",
        ToolGroup::Read,
    )
    .output::<ListAccountsOutput>()
    .no_account()
}

pub(crate) async fn run(adapter: &GoogleAdapter, input: &Value) -> Result<Value, ToolError> {
    let input: ListAccountsInput = serde_json::from_value(input.clone())
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Invalid input: {e}")))?;
    let connection = input.connection.as_deref().filter(|c| !c.is_empty());
    if let Some(c) = connection {
        adapter.connection(c)?;
    }
    if input.refresh {
        adapter.refresh_accounts(connection).await?;
    }
    let accounts = adapter
        .db()
        .list_accounts(Platform::GoogleAds, connection)
        .map_err(internal)?
        .into_iter()
        .map(|(connection, a)| AccountOut {
            account_id: a.account_id,
            name: a.name,
            currency: a.currency,
            timezone: a.timezone,
            status: a.status,
            manager_id: a.manager_id,
            connection,
        })
        .collect();
    serde_json::to_value(ListAccountsOutput { accounts }).map_err(internal)
}
