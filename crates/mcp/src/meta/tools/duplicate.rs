//! `duplicate`: copies a campaign, ad set or ad (paused). Up to 3 ads, a synchronous copy;
//! from 4 to 51, an async request; above that, refuses.

use adsconsole_core::config::ToolGroup;
use adsconsole_meta_ads::fields::ObjectKind;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::reports::get_object;
use super::{MTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::errors::translate;
use crate::meta::plan::{DirectExec, MetaPlan};

pub(crate) fn tools() -> Vec<MTool> {
    vec![MTool::Write(Box::new(Duplicate))]
}

/// Synchronous copy up to this number of ads.
const SYNC_MAX_ADS: usize = 3;
/// Async copy up to this number of ads.
const ASYNC_MAX_ADS: usize = 51;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ObjectType {
    Campaign,
    Adset,
    Ad,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DuplicateInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// campaign, adset or ad.
    object_type: ObjectType,
    object_id: String,
    /// Copies the children (ad sets and ads). Default true.
    #[serde(default)]
    deep_copy: Option<bool>,
    /// Name suffix of the copies (default " (copy)").
    #[serde(default)]
    rename_suffix: Option<String>,
    /// adset only: destination campaign.
    #[serde(default)]
    target_campaign_id: Option<String>,
    /// ad only: destination ad set.
    #[serde(default)]
    target_adset_id: Option<String>,
}

fn invalid(field: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

struct Duplicate;

#[async_trait::async_trait]
impl WriteTool for Duplicate {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<DuplicateInput>(
            "duplicate",
            "Duplicates a campaign, ad set or ad (copies are created paused). Up to 51 \
             ads at a time.",
            ToolGroup::Edit,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: DuplicateInput = parse_input(input)?;
        let (kind, what) = match i.object_type {
            ObjectType::Campaign => (ObjectKind::Campaign, "Campaign"),
            ObjectType::Adset => (ObjectKind::AdSet, "Ad set"),
            ObjectType::Ad => (ObjectKind::Ad, "Ad"),
        };
        if i.target_campaign_id.is_some() && i.object_type != ObjectType::Adset {
            return Err(invalid(
                "target_campaign_id",
                "target_campaign_id applies only to adset.",
            ));
        }
        if i.target_adset_id.is_some() && i.object_type != ObjectType::Ad {
            return Err(invalid(
                "target_adset_id",
                "target_adset_id applies only to ad.",
            ));
        }
        get_object(ctx, "duplicate", what, &i.object_id, "id,account_id").await?;
        let deep = i.deep_copy.unwrap_or(true);
        let ads = match (i.object_type, deep) {
            (ObjectType::Ad, _) => 1,
            (_, false) => 0,
            _ => ctx
                .client
                .get_all(
                    &format!("{}/ads", i.object_id),
                    &[("fields", "id".into()), ("limit", "100".into())],
                    ASYNC_MAX_ADS + 1,
                )
                .await
                .map_err(|e| translate(e, Some(ctx), "duplicate"))?
                .data
                .len(),
        };
        if ads > ASYNC_MAX_ADS {
            return Err(invalid(
                "object_id",
                "Meta duplicates at most 51 ads at a time; duplicate the ad sets separately.",
            ));
        }
        let mut params = Map::new();
        params.insert("deep_copy".into(), json!(deep.to_string()));
        params.insert("status_option".into(), json!("PAUSED"));
        params.insert(
            "rename_options".into(),
            json!({ "rename_suffix": i.rename_suffix.unwrap_or_else(|| " (copy)".into()) }),
        );
        if let Some(c) = i.target_campaign_id {
            params.insert("campaign_id".into(), json!(c));
        }
        if let Some(s) = i.target_adset_id {
            params.insert("adset_id".into(), json!(s));
        }
        let asynchronous = ads > SYNC_MAX_ADS;
        Ok(MetaPlan {
            summary: format!(
                "Duplicate {} {} ({ads} ad(s), {}); the copy is created paused",
                what.to_lowercase(),
                i.object_id,
                if asynchronous { "async" } else { "sync" }
            ),
            direct: Some(DirectExec::Copy {
                object_id: i.object_id,
                kind,
                params,
                asynchronous,
            }),
            ..MetaPlan::default()
        })
    }
}
