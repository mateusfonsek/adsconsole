//! Escape: `graph_get` e `graph_post` brutos, desligados por padrão (grupo Escape).

use std::collections::BTreeMap;

use adsconsole_core::config::ToolGroup;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{MTool, ReadTool, WriteTool, own_path, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::errors::translate;
use crate::meta::plan::{DirectExec, MetaPlan};
use crate::truncate::{ITEMS_BUDGET, MAX_RESPONSE_BYTES, fit};

pub(crate) fn escape() -> Vec<MTool> {
    vec![
        MTool::Read(Box::new(GraphGet)),
        MTool::Write(Box::new(GraphPost)),
    ]
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Caminho relativo à conta (`act_…/campaigns`) ou a um id de objeto.
    path: String,
    /// Parâmetros da query (ex.: `fields`).
    #[serde(default)]
    params: BTreeMap<String, String>,
}

struct GraphGet;

#[async_trait::async_trait]
impl ReadTool for GraphGet {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetInput>(
            "graph_get",
            "GET bruto na Graph API v26.0, para o que as outras ferramentas não cobrem.",
            ToolGroup::Escape,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: GetInput = parse_input(input)?;
        own_path(ctx, &i.path)?;
        let query: Vec<(&str, String)> = i
            .params
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let mut out = ctx
            .client
            .get(i.path.trim_start_matches('/'), &query)
            .await
            .map_err(|e| translate(e, Some(ctx), "graph_get"))?;
        if out.to_string().len() > MAX_RESPONSE_BYTES
            && let Some(data) = out.get_mut("data").and_then(Value::as_array_mut)
        {
            let (kept, truncated) = fit(std::mem::take(data), ITEMS_BUDGET);
            *data = kept;
            out["truncated"] = Value::Bool(truncated);
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PostInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Caminho relativo à conta (`act_…/campaigns`) ou a um id de objeto.
    path: String,
    /// Parâmetros do POST.
    #[serde(default)]
    params: Map<String, Value>,
}

struct GraphPost;

#[async_trait::async_trait]
impl WriteTool for GraphPost {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<PostInput>(
            "graph_post",
            "POST bruto na Graph API v26.0. Não passa pelas validações das outras ferramentas.",
            ToolGroup::Escape,
            false,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: PostInput = parse_input(input)?;
        own_path(ctx, &i.path)?;
        let path = i.path.trim_start_matches('/').to_string();
        if path.split('/').any(|seg| seg == "copies") {
            return Err(ToolError::new(
                codes::INVALID_INPUT,
                "Para duplicar, use a ferramenta duplicate.",
            )
            .with_field("path"));
        }
        Ok(MetaPlan {
            summary: format!("POST {path}"),
            direct: Some(DirectExec::GraphPost {
                path,
                params: i.params,
            }),
            ..MetaPlan::default()
        })
    }
}
