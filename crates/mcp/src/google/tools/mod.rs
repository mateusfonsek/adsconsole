//! Registro das ferramentas do Google Ads.

pub(crate) mod accounts;
pub(crate) mod asset_group;
pub(crate) mod assets;
pub(crate) mod create_common;
pub(crate) mod create_demand_gen;
pub(crate) mod create_display;
pub(crate) mod create_pmax;
pub(crate) mod create_search;
pub(crate) mod edit;
pub(crate) mod escape;
pub(crate) mod gaql;
pub(crate) mod hidden;
pub(crate) mod keyword_planner;
pub(crate) mod keywords;
pub(crate) mod recommendations;
pub(crate) mod reports;
pub(crate) mod targeting;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::ctx::Ctx;
use super::plan::WritePlan;
use crate::adapter::{Applied, ToolSpec};
use crate::error::{ToolError, codes};

/// Ferramenta de leitura: recebe o [`Ctx`] da conta e a entrada da chamada.
#[async_trait]
pub(crate) trait ReadTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError>;
}

/// Ferramenta de escrita: só monta o [`WritePlan`]; validar, aplicar e montar a
/// inversa ficam com o adapter (ver [`super::plan`]).
///
/// O adapter chama `plan` de novo em cada etapa (`validate`, `snapshot`, `execute`),
/// por isso ele precisa ser determinístico para a mesma entrada e não pode ter
/// efeitos colaterais. Pode fazer leituras (elas contam na cota de operações).
#[async_trait]
pub(crate) trait WriteTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError>;

    /// Ajuste da ferramenta sobre o erro já traduzido do `validateOnly` ou do mutate
    /// (ex.: a Display recusada vira `DISPLAY_NOT_AVAILABLE`). Padrão: o erro como veio.
    fn map_error(&self, e: ToolError) -> ToolError {
        e
    }

    /// Ajuste da ferramenta sobre o resultado já aplicado (ex.: `upload_asset` devolve
    /// só `{asset_resource_name}`). Padrão: como veio.
    fn map_applied(&self, a: Applied) -> Applied {
        a
    }
}

pub(crate) enum GTool {
    Read(Box<dyn ReadTool>),
    Write(Box<dyn WriteTool>),
}

impl GTool {
    pub(crate) fn spec(&self) -> ToolSpec {
        match self {
            GTool::Read(t) => t.spec(),
            GTool::Write(t) => t.spec(),
        }
    }
}

/// Ferramentas com conta. `list_accounts` fica fora: não tem `Ctx` e é tratada
/// direto no `read` do adapter (ver [`accounts`]).
pub(crate) fn all() -> Vec<GTool> {
    let mut tools = vec![gaql::tool()];
    tools.extend(reports::tools());
    tools.extend(keyword_planner::tools());
    tools.extend(edit::tools());
    tools.extend(keywords::tools());
    tools.extend(targeting::tools());
    tools.extend(recommendations::tools());
    tools.extend(asset_group::tools());
    tools.extend(create_search::tools());
    tools.extend(create_pmax::tools());
    tools.extend(create_demand_gen::tools());
    tools.extend(create_display::tools());
    tools.extend(assets::tools());
    tools.extend(hidden::hidden());
    tools.push(escape::mutate());
    tools
}

/// Entrada da ferramenta; erro de formato vira `INVALID_INPUT`.
pub(crate) fn parse_input<T: DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone())
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Entrada inválida: {e}")))
}

/// Recusa nome de recurso de outra conta.
pub(crate) fn own_resource(ctx: &Ctx, rn: &str) -> Result<(), ToolError> {
    if rn.starts_with(&format!("customers/{}/", ctx.customer_id)) {
        return Ok(());
    }
    Err(ToolError::new(
        codes::INVALID_INPUT,
        format!("O recurso {rn} não é da conta {}.", ctx.customer_id),
    ))
}
