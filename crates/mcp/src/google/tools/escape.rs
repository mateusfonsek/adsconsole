//! Escape `mutate`: operações brutas do `googleAds:mutate`, sem desfazer. Fica no
//! grupo Escape, desligado por padrão.

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
    /// `MutateOperation`s da API REST v26 do Google Ads (ex.:
    /// `{"campaignOperation": {"update": {...}, "updateMask": "name"}}`), enviadas como
    /// estão, num único mutate tudo ou nada.
    operations: Vec<Value>,
}

struct Mutate;

#[async_trait]
impl WriteTool for Mutate {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<MutateInput>(
            NAME,
            "Envia operações brutas ao googleAds:mutate da conta (validadas antes com \
             validateOnly). Use só quando nenhuma ferramenta específica resolve.",
            ToolGroup::Escape,
            false,
        )
        .destructive()
    }

    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: MutateInput = parse_input(input)?;
        Ok(WritePlan {
            summary: format!(
                "Envia {} operação(ões) brutas ao googleAds:mutate.",
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
