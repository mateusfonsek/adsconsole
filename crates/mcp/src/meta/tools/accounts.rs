//! `list_ad_accounts`: contas do cache, opcionalmente atualizadas antes na Meta.

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
    /// Id da conexão; sem ele, lista as contas de todas.
    #[serde(default)]
    connection: Option<String>,
    /// Consulta a Meta antes de responder.
    #[serde(default)]
    refresh: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AccountOut {
    /// `act_<dígitos>`.
    account_id: String,
    name: String,
    currency: String,
    timezone: Option<String>,
    /// `ACTIVE`, `DISABLED`, `UNSETTLED`, `CLOSED`... Só `ACTIVE` aceita escritas.
    status: Option<String>,
    connection: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListAdAccountsOutput {
    accounts: Vec<AccountOut>,
    /// Aviso de vencimento do acesso de alguma conexão listada.
    #[serde(skip_serializing_if = "Option::is_none")]
    aviso_token: Option<String>,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec::read::<ListAdAccountsInput>(
        NAME,
        "Lista as contas de anúncios da Meta de cada conexão, com moeda e status. \
         Com refresh: true, consulta a Meta antes.",
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
        aviso_token: (!warnings.is_empty()).then(|| warnings.join(" ")),
    };
    serde_json::to_value(out).map_err(internal)
}
