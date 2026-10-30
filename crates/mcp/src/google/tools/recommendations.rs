//! Recomendações: `apply_recommendation` e `dismiss_recommendation`.
//!
//! Não usam o `googleAds:mutate`: são POSTs em `recommendations:apply` e
//! `recommendations:dismiss` (`DirectExec::Post`). O plano lê a recomendação por GAQL
//! (ela precisa existir e não estar dispensada) e as duas são irreversíveis.

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::fields;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::edit::read_one;
use super::keywords::IRREVERSIBLE_WARNING;
use super::reports::{invalid, text};
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::plan::{DirectExec, InverseSpec, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Write(Box::new(ApplyRecommendation)),
        GTool::Write(Box::new(DismissRecommendation)),
    ]
}

const APPLY: &str = "apply_recommendation";
const DISMISS: &str = "dismiss_recommendation";

#[derive(Debug, Deserialize, JsonSchema)]
struct ApplyInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Nome do recurso (`customers/123/recommendations/abc`) ou só o id.
    recommendation: String,
    /// Parâmetros da aplicação, no formato da `ApplyRecommendationOperation` do Google
    /// para o tipo da recomendação (ex.: `{"campaignBudget": {"newBudgetAmountMicros": "…"}}`).
    #[serde(default)]
    parameters: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DismissInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Nome do recurso (`customers/123/recommendations/abc`) ou só o id.
    recommendation: String,
}

/// Nome do recurso a partir do nome completo ou do id puro.
fn recommendation_rn(ctx: &Ctx, raw: &str) -> Result<String, ToolError> {
    let field = "recommendation";
    let raw = raw.trim();
    let prefix = format!("customers/{}/recommendations/", ctx.customer_id);
    let id = if raw.starts_with("customers/") {
        raw.strip_prefix(&prefix).ok_or_else(|| {
            invalid(format!(
                "A recomendação {raw} não é da conta {}.",
                ctx.customer_id
            ))
            .with_field(field)
        })?
    } else {
        raw
    };
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(
            invalid("Informe o id (ou o nome completo) da recomendação.").with_field(field),
        );
    }
    Ok(format!("{prefix}{id}"))
}

/// Tipo da recomendação; `NOT_FOUND` se não existe e `INVALID_INPUT` se já foi dispensada.
async fn lookup(ctx: &Ctx, tool: &str, rn: &str) -> Result<String, ToolError> {
    let query = format!(
        "{} LIMIT 1",
        fields::read_query(
            "recommendation",
            rn,
            &[
                "recommendation.resource_name",
                "recommendation.type",
                "recommendation.dismissed"
            ]
        )
    );
    let row = read_one(ctx, tool, &query).await?.ok_or_else(|| {
        ToolError::new(
            codes::NOT_FOUND,
            format!(
                "Recomendação {rn} não encontrada na conta {}.",
                ctx.customer_id
            ),
        )
        .with_field("recommendation")
    })?;
    if text(&row, "recommendation.dismissed").as_deref() == Some("true") {
        return Err(
            invalid(format!("A recomendação {rn} já foi dispensada.")).with_field("recommendation")
        );
    }
    Ok(text(&row, "recommendation.type").unwrap_or_else(|| "desconhecido".into()))
}

fn post_plan(summary: String, suffix: &str, operation: Value) -> WritePlan {
    WritePlan {
        summary,
        warning: Some(IRREVERSIBLE_WARNING.to_string()),
        operations: Vec::new(),
        field_changes: Vec::new(),
        watch: Vec::new(),
        inverse: InverseSpec::None,
        direct: Some(DirectExec::Post {
            path_suffix: suffix.to_string(),
            body: json!({ "operations": [operation] }),
        }),
    }
}

struct ApplyRecommendation;

#[async_trait]
impl WriteTool for ApplyRecommendation {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ApplyInput>(
            APPLY,
            "Aplica uma recomendação do Google Ads na conta (as recomendações vêm de \
             `gaql_search` em `recommendation`). Não pode ser desfeito.",
            ToolGroup::Edit,
            false,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ApplyInput = parse_input(input)?;
        let rn = recommendation_rn(ctx, &input.recommendation)?;
        let mut op = Map::new();
        match input.parameters {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) => {
                if m.contains_key("resourceName") {
                    return Err(invalid("parameters não pode trazer resourceName.")
                        .with_field("parameters"));
                }
                op = m;
            }
            Some(_) => {
                return Err(invalid("parameters deve ser um objeto.").with_field("parameters"));
            }
        }
        op.insert("resourceName".into(), json!(rn));
        let kind = lookup(ctx, APPLY, &rn).await?;
        Ok(post_plan(
            format!("Aplicar a recomendação {kind} ({rn})"),
            "/recommendations:apply",
            Value::Object(op),
        ))
    }
}

struct DismissRecommendation;

#[async_trait]
impl WriteTool for DismissRecommendation {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<DismissInput>(
            DISMISS,
            "Dispensa uma recomendação do Google Ads. Não pode ser desfeito.",
            ToolGroup::Edit,
            false,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: DismissInput = parse_input(input)?;
        let rn = recommendation_rn(ctx, &input.recommendation)?;
        let kind = lookup(ctx, DISMISS, &rn).await?;
        Ok(post_plan(
            format!("Dispensar a recomendação {kind} ({rn})"),
            "/recommendations:dismiss",
            json!({ "resourceName": rn }),
        ))
    }
}
