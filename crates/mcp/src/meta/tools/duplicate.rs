//! `duplicate`: copia campanha, ad set ou anúncio (pausado). Até 3 anúncios, cópia
//! síncrona; de 4 a 51, requisição assíncrona; acima disso, recusa.

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

/// Cópia síncrona até este número de anúncios.
const SYNC_MAX_ADS: usize = 3;
/// Cópia assíncrona até este número de anúncios.
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
    /// campaign, adset ou ad.
    object_type: ObjectType,
    object_id: String,
    /// Copia os filhos (ad sets e anúncios). Padrão true.
    #[serde(default)]
    deep_copy: Option<bool>,
    /// Sufixo do nome das cópias (padrão " (cópia)").
    #[serde(default)]
    rename_suffix: Option<String>,
    /// Só para adset: campanha de destino.
    #[serde(default)]
    target_campaign_id: Option<String>,
    /// Só para ad: ad set de destino.
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
            "Duplica uma campanha, ad set ou anúncio (as cópias nascem pausadas). Até 51 \
             anúncios por vez.",
            ToolGroup::Edit,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: DuplicateInput = parse_input(input)?;
        let (kind, what) = match i.object_type {
            ObjectType::Campaign => (ObjectKind::Campaign, "Campanha"),
            ObjectType::Adset => (ObjectKind::AdSet, "Ad set"),
            ObjectType::Ad => (ObjectKind::Ad, "Anúncio"),
        };
        if i.target_campaign_id.is_some() && i.object_type != ObjectType::Adset {
            return Err(invalid(
                "target_campaign_id",
                "target_campaign_id só vale para adset.",
            ));
        }
        if i.target_adset_id.is_some() && i.object_type != ObjectType::Ad {
            return Err(invalid(
                "target_adset_id",
                "target_adset_id só vale para ad.",
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
                "A Meta só duplica até 51 anúncios por vez; duplique os ad sets separadamente.",
            ));
        }
        let mut params = Map::new();
        params.insert("deep_copy".into(), json!(deep.to_string()));
        params.insert("status_option".into(), json!("PAUSED"));
        params.insert(
            "rename_options".into(),
            json!({ "rename_suffix": i.rename_suffix.unwrap_or_else(|| " (cópia)".into()) }),
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
                "Duplicar {} {} ({ads} anúncio(s), {}); a cópia nasce pausada",
                what.to_lowercase(),
                i.object_id,
                if asynchronous {
                    "assíncrono"
                } else {
                    "síncrono"
                }
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
