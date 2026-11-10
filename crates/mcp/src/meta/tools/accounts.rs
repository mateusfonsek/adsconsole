//! `list_ad_accounts`: accounts from the cache, optionally refreshed from Meta first.

use adsconsole_core::Platform;
use adsconsole_core::config::ToolGroup;
use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::parse_input;
use crate::adapter::ToolSpec;
use crate::engine::internal;
use crate::error::ToolError;
use crate::meta::MetaAdapter;
use crate::meta::connections::token_warning;

pub(crate) const NAME: &str = "list_ad_accounts";

#[derive(Debug, Deserialize, JsonSchema)]
struct ListAdAccountsInput {
    /// Connection id; without it, lists the accounts of every connection.
    #[serde(default)]
    connection: Option<String>,
    /// Queries Meta before answering.
    #[serde(default)]
    refresh: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AccountOut {
    /// `act_<digits>`.
    account_id: String,
    name: String,
    currency: String,
    timezone: Option<String>,
    /// `ACTIVE`, `DISABLED`, `UNSETTLED`, `CLOSED`... Only `ACTIVE` accepts writes.
    status: Option<String>,
    connection: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListAdAccountsOutput {
    accounts: Vec<AccountOut>,
    /// Access expiry warning for one of the listed connections.
    #[serde(skip_serializing_if = "Option::is_none")]
    token_warning: Option<String>,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec::read::<ListAdAccountsInput>(
        NAME,
        "Lists the Meta ad accounts of each connection, with currency and status. \
         With refresh: true, queries Meta first.",
        ToolGroup::Read,
    )
    .output::<ListAdAccountsOutput>()
    .no_account()
}

pub(crate) async fn run(adapter: &MetaAdapter, input: &Value) -> Result<Value, ToolError> {
    let input: ListAdAccountsInput = parse_input(input)?;
    let connection = input.connection.as_deref().filter(|c| !c.is_empty());
    if let Some(c) = connection {
        adapter.connection(c)?;
    }
    if input.refresh {
        adapter.refresh_accounts(connection).await?;
    }
    let rows = adapter
        .db()
        .list_accounts(Platform::MetaAds, connection)
        .map_err(internal)?;
    let mut warnings: Vec<String> = Vec::new();
    let now = Utc::now();
    let mut seen = std::collections::BTreeSet::new();
    for (conn, _) in &rows {
        if seen.insert(conn.clone())
            && let Some(meta) = adapter.db().get_connection(conn).map_err(internal)?
            && let Some(w) = token_warning(meta.expires_at, now)
        {
            warnings.push(format!("{}: {w}", meta.name));
        }
    }
    let accounts = rows
        .into_iter()
        .map(|(connection, a)| AccountOut {
            account_id: a.account_id,
            name: a.name,
            currency: a.currency,
            timezone: a.timezone,
            status: a.status,
            connection,
        })
        .collect();
    let out = ListAdAccountsOutput {
        accounts,
        token_warning: (!warnings.is_empty()).then(|| warnings.join(" ")),
    };
    serde_json::to_value(out).map_err(internal)
}
