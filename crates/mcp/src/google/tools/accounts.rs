//! `list_accounts`: contas do cache, opcionalmente atualizadas antes no Google.

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
    /// Id da conexão; sem ele, lista as contas de todas.
    #[serde(default)]
    connection: Option<String>,
    /// Consulta o Google antes de responder (gasta operações da cota do projeto).
    #[serde(default)]
    refresh: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AccountOut {
    account_id: String,
    name: String,
    currency: String,
    timezone: Option<String>,
    /// `ENABLED`, `CANCELED`, `SUSPENDED`, `CLOSED` ou `MANAGER` (MCC, sem campanhas).
    status: Option<String>,
    /// MCC pela qual a conta é acessada.
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
        "Lista as contas do Google Ads de cada conexão, incluindo as filhas de MCC \
         (manager_id). Com refresh: true, consulta o Google antes.",
        ToolGroup::Read,
    )
    .output::<ListAccountsOutput>()
    .no_account()
}

pub(crate) async fn run(adapter: &GoogleAdapter, input: &Value) -> Result<Value, ToolError> {
    let input: ListAccountsInput = serde_json::from_value(input.clone())
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Entrada inválida: {e}")))?;
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
