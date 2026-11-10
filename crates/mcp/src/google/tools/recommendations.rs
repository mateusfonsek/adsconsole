//! Recommendations: `apply_recommendation` and `dismiss_recommendation`.
//!
//! They don't use `googleAds:mutate`: they are POSTs to `recommendations:apply` and
//! `recommendations:dismiss` (`DirectExec::Post`). The plan reads the recommendation
//! through GAQL (it must exist and not be dismissed), and both are irreversible.

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
    /// Resource name (`customers/123/recommendations/abc`) or just the id.
    recommendation: String,
    /// Apply parameters, in the format of Google's `ApplyRecommendationOperation` for
    /// the recommendation type (e.g. `{"campaignBudget": {"newBudgetAmountMicros": "…"}}`).
    #[serde(default)]
    parameters: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DismissInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Resource name (`customers/123/recommendations/abc`) or just the id.
    recommendation: String,
}

/// Resource name from the full name or the bare id.
fn recommendation_rn(ctx: &Ctx, raw: &str) -> Result<String, ToolError> {
    let field = "recommendation";
    let raw = raw.trim();
    let prefix = format!("customers/{}/recommendations/", ctx.customer_id);
    let id = if raw.starts_with("customers/") {
        raw.strip_prefix(&prefix).ok_or_else(|| {
            invalid(format!(
                "Recommendation {raw} does not belong to account {}.",
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
        return Err(invalid("Provide the recommendation id (or its full name).").with_field(field));
    }
    Ok(format!("{prefix}{id}"))
}

/// Recommendation type; `NOT_FOUND` if it doesn't exist, `INVALID_INPUT` if it was
/// already dismissed.
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
                "Recommendation {rn} not found in account {}.",
                ctx.customer_id
            ),
        )
        .with_field("recommendation")
    })?;
    if text(&row, "recommendation.dismissed").as_deref() == Some("true") {
        return Err(
            invalid(format!("Recommendation {rn} was already dismissed."))
                .with_field("recommendation"),
        );
    }
    Ok(text(&row, "recommendation.type").unwrap_or_else(|| "unknown".into()))
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
            "Applies a Google Ads recommendation to the account (list recommendations with \
             `gaql_search` on `recommendation`). Cannot be undone.",
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
                    return Err(invalid("parameters must not include resourceName.")
                        .with_field("parameters"));
                }
                op = m;
            }
            Some(_) => {
                return Err(invalid("parameters must be an object.").with_field("parameters"));
            }
        }
        op.insert("resourceName".into(), json!(rn));
        let kind = lookup(ctx, APPLY, &rn).await?;
        Ok(post_plan(
            format!("Apply recommendation {kind} ({rn})"),
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
            "Dismisses a Google Ads recommendation. Cannot be undone.",
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
            format!("Dismiss recommendation {kind} ({rn})"),
            "/recommendations:dismiss",
            json!({ "resourceName": rn }),
        ))
    }
}
