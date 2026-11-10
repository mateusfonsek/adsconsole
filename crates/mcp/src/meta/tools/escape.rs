//! Escape: raw `graph_get` and `graph_post`, off by default (Escape group).

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
    /// Path relative to the account (`act_…/campaigns`) or to an object id.
    path: String,
    /// Query parameters (e.g. `fields`).
    #[serde(default)]
    params: BTreeMap<String, String>,
}

struct GraphGet;

#[async_trait::async_trait]
impl ReadTool for GraphGet {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetInput>(
            "graph_get",
            "Raw GET on Graph API v26.0, for what the other tools do not cover.",
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
    /// Path relative to the account (`act_…/campaigns`) or to an object id.
    path: String,
    /// POST parameters.
    #[serde(default)]
    params: Map<String, Value>,
}

struct GraphPost;

#[async_trait::async_trait]
impl WriteTool for GraphPost {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<PostInput>(
            "graph_post",
            "Raw POST on Graph API v26.0. Skips the validations of the other tools.",
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
                "To duplicate, use the duplicate tool.",
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
