//! Inversas: `restore_fields` (volta campos ao "antes"), `delete_objects` (apaga o que
//! uma criação fez) e `delete_image` (apaga uma imagem enviada). Ocultas: só o desfazer
//! as chama.

use std::collections::BTreeMap;

use adsconsole_core::config::ToolGroup;
use adsconsole_meta_ads::fields::{self, FieldKind, ObjectKind};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{MTool, WriteTool, parse_input};
use crate::adapter::ToolSpec;
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::plan::{
    DELETE_IMAGE, DELETE_OBJECTS, DirectExec, FieldChange, InverseSpec, MetaPlan, RESTORE_FIELDS,
    Step, Watch, parse_kind,
};

pub(crate) fn hidden() -> Vec<MTool> {
    vec![
        MTool::Write(Box::new(RestoreFields)),
        MTool::Write(Box::new(DeleteObjects)),
        MTool::Write(Box::new(DeleteImage)),
    ]
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreInput {
    /// `{id: {campo: valor}}`, no formato das ferramentas.
    updates: BTreeMap<String, BTreeMap<String, Value>>,
    /// `{id: "campaign" | "adset" | "ad"}`.
    #[serde(default)]
    kinds: BTreeMap<String, String>,
}

struct RestoreFields;

#[async_trait::async_trait]
impl WriteTool for RestoreFields {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<RestoreInput>(
            RESTORE_FIELDS,
            "Volta campos de campanhas, ad sets e anúncios aos valores anteriores.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
        .hidden()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: RestoreInput = parse_input(input)?;
        let mut plan = MetaPlan {
            summary: format!(
                "Restaurar os valores anteriores de {} objeto(s)",
                i.updates.len()
            ),
            inverse: InverseSpec::RestoreFields,
            ..MetaPlan::default()
        };
        for (id, values) in &i.updates {
            let kind = i.kinds.get(id).and_then(|k| parse_kind(k)).ok_or_else(|| {
                ToolError::new(
                    codes::INVALID_INPUT,
                    format!("Tipo do objeto {id} ausente."),
                )
            })?;
            let mut params = Map::new();
            let mut watched = Vec::new();
            for (name, value) in values {
                let def = fields::find(name, kind).ok_or_else(|| {
                    ToolError::new(codes::INVALID_INPUT, format!("Campo {name} não editável."))
                })?;
                let param = fields::to_param(def, value, ctx.exponent)
                    .map_err(|e| ToolError::new(codes::INVALID_INPUT, e))?;
                // Campo vazio no "antes" (mudança gravada antes de a escrita marcar esses
                // casos como sem desfazer): não há como voltar a vazio, então recusa em vez
                // de dizer que desfez.
                if param.is_null() {
                    return Err(ToolError::new(
                        codes::NOT_REVERSIBLE,
                        format!(
                            "Não dá para desfazer: {name} de {id} não tinha valor antes, e não \
                             há como voltar a vazio pelo AdsConsole. Mude pelo Gerenciador de \
                             Anúncios."
                        ),
                    )
                    .with_field(name.clone()));
                }
                params.insert(def.name.to_string(), param);
                watched.push(def.name);
                plan.field_changes.push(FieldChange {
                    object_id: id.clone(),
                    kind,
                    field: def.name,
                    value: value.clone(),
                });
                if kind == ObjectKind::AdSet
                    && def.value == FieldKind::Minor
                    && def.name.ends_with("_budget")
                {
                    plan.budget_objects.push(id.clone());
                }
            }
            if params.is_empty() {
                continue;
            }
            plan.steps.push(Step::Update {
                id: id.clone(),
                params,
            });
            plan.watch.push(Watch {
                object_id: id.clone(),
                kind,
                fields: watched,
            });
        }
        plan.budget_objects.dedup();
        Ok(plan)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteInput {
    /// Ids a apagar, na ordem.
    ids: Vec<String>,
}

struct DeleteObjects;

#[async_trait::async_trait]
impl WriteTool for DeleteObjects {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<DeleteInput>(
            DELETE_OBJECTS,
            "Apaga objetos criados por uma mudança do AdsConsole.",
            ToolGroup::Edit,
            false,
        )
        .destructive()
        .hidden()
    }

    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: DeleteInput = parse_input(input)?;
        if i.ids.is_empty() {
            return Err(
                ToolError::new(codes::INVALID_INPUT, "Nenhum id para apagar.").with_field("ids"),
            );
        }
        Ok(MetaPlan {
            summary: format!("Apagar {}", i.ids.join(", ")),
            steps: i.ids.into_iter().map(|id| Step::Delete { id }).collect(),
            ..MetaPlan::default()
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteImageInput {
    /// image_hash de `upload_image`.
    hash: String,
}

struct DeleteImage;

#[async_trait::async_trait]
impl WriteTool for DeleteImage {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<DeleteImageInput>(
            DELETE_IMAGE,
            "Apaga uma imagem enviada por upload_image.",
            ToolGroup::Media,
            false,
        )
        .destructive()
        .hidden()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: DeleteImageInput = parse_input(input)?;
        if i.hash.trim().is_empty() {
            return Err(
                ToolError::new(codes::INVALID_INPUT, "Informe o hash da imagem.")
                    .with_field("hash"),
            );
        }
        // O POST com method=delete foi o que funcionou na conta real.
        let mut params = Map::new();
        params.insert("hash".into(), Value::String(i.hash.clone()));
        params.insert("method".into(), Value::String("delete".into()));
        Ok(MetaPlan {
            summary: format!("Apagar a imagem {}", i.hash),
            direct: Some(DirectExec::GraphPost {
                path: format!("{}/adimages", ctx.account_id),
                params,
            }),
            ..MetaPlan::default()
        })
    }
}
