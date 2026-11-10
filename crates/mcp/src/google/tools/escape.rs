//! Escape `mutate`: raw `googleAds:mutate` operations, with no undo. Belongs to the
//! Escape group, off by default.

use adsconsole_core::config::ToolGroup;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::ToolError;
use crate::google::ctx::Ctx;
use crate::google::plan::{InverseSpec, WritePlan};

pub(crate) const NAME: &str = "mutate";

pub(crate) fn mutate() -> GTool {
    GTool::Write(Box::new(Mutate))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MutateInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Google Ads REST API v26 `MutateOperation`s (e.g.
    /// `{"campaignOperation": {"update": {...}, "updateMask": "name"}}`), sent as is in
    /// a single all-or-nothing mutate.
    operations: Vec<Value>,
}

struct Mutate;

#[async_trait]
impl WriteTool for Mutate {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<MutateInput>(
            NAME,
            "Sends raw operations to the account's googleAds:mutate (validated first with \
             validateOnly). Use only when no specific tool can do the job.",
            ToolGroup::Escape,
            false,
        )
        .destructive()
    }

    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: MutateInput = parse_input(input)?;
        Ok(WritePlan {
            summary: format!(
                "Sends {} raw operation(s) to googleAds:mutate.",
                input.operations.len()
            ),
            warning: None,
            operations: input.operations,
            field_changes: Vec::new(),
            watch: Vec::new(),
            inverse: InverseSpec::None,
            direct: None,
        })
    }
}
