//! Google Ads tool registry.

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

/// Read tool: receives the account's [`Ctx`] and the call input.
#[async_trait]
pub(crate) trait ReadTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError>;
}

/// Write tool: only builds the [`WritePlan`]; the adapter validates, applies and
/// builds the inverse (see [`super::plan`]).
///
/// The adapter calls `plan` again at each step (`validate`, `snapshot`, `execute`),
/// so it must be deterministic for the same input and free of side effects. It may
/// read (reads count toward the operations quota).
#[async_trait]
pub(crate) trait WriteTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError>;

    /// Tool-specific adjustment of the already translated `validateOnly` or mutate
    /// error (e.g. a refused Display campaign becomes `DISPLAY_NOT_AVAILABLE`).
    /// Default: the error as is.
    fn map_error(&self, e: ToolError) -> ToolError {
        e
    }

    /// Tool-specific adjustment of the applied result (e.g. `upload_asset` returns
    /// only `{asset_resource_name}`). Default: as is.
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

/// Tools that take an account. `list_accounts` is not here: it has no `Ctx` and is
/// handled directly in the adapter's `read` (see [`accounts`]).
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

/// Tool input; a format error becomes `INVALID_INPUT`.
pub(crate) fn parse_input<T: DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone())
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Invalid input: {e}")))
}

/// Refuses a resource name from another account.
pub(crate) fn own_resource(ctx: &Ctx, rn: &str) -> Result<(), ToolError> {
    if rn.starts_with(&format!("customers/{}/", ctx.customer_id)) {
        return Ok(());
    }
    Err(ToolError::new(
        codes::INVALID_INPUT,
        format!(
            "Resource {rn} does not belong to account {}.",
            ctx.customer_id
        ),
    ))
}
