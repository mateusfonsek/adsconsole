//! Edição: `update_campaign`, `update_adset`, `update_ad`. Cada campo vira um
//! `FieldChange`; o desfazer volta ao "antes" com `restore_fields`.

use adsconsole_core::config::ToolGroup;
use adsconsole_core::{Decimal, Status};
use adsconsole_meta_ads::fields::{FIELDS, FieldKind, ObjectKind, find, normalize, to_param};
use rust_decimal::prelude::ToPrimitive;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::reports::get_object;
use super::{MTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::plan::{FieldChange, InverseSpec, MetaPlan, Step, Watch};

pub(crate) fn tools() -> Vec<MTool> {
    vec![
        MTool::Write(Box::new(UpdateCampaign)),
        MTool::Write(Box::new(UpdateAdset)),
        MTool::Write(Box::new(UpdateAd)),
    ]
}

const ARCHIVE_WARNING: &str = "Objetos arquivados não voltam a veicular sem serem reativados.";

fn invalid(field: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

fn status_value(field: &str, s: Status) -> Result<Value, ToolError> {
    match s {
        Status::Active | Status::Paused | Status::Archived => Ok(json!(s.to_meta())),
        Status::Removed => Err(invalid(
            field,
            "Apagar não é permitido por aqui (é irreversível na Meta); use ARCHIVED.",
        )),
    }
}

fn money(field: &str, d: Decimal) -> Result<Value, ToolError> {
    d.to_f64()
        .map(|f| {
            if d.fract().is_zero() {
                json!(d.to_i64())
            } else {
                json!(f)
            }
        })
        .ok_or_else(|| invalid(field, "Valor inválido."))
}

/// Tem orçamento desse tipo? (A Meta devolve ausente ou `"0"` quando não tem.)
fn has_budget(obj: &Value, field: &str) -> bool {
    match &obj[field] {
        Value::String(s) => s.parse::<i64>().is_ok_and(|n| n > 0),
        Value::Number(n) => n.as_i64().is_some_and(|n| n > 0),
        _ => false,
    }
}

/// Campos que a Meta pode deixar sem valor (sem limite de gasto, sem data de término...).
fn may_be_empty(kind: FieldKind) -> bool {
    matches!(
        kind,
        FieldKind::Minor | FieldKind::Enum | FieldKind::DateTime
    )
}

/// `fields=` da leitura do objeto antes de editar: id, conta e os campos que podem
/// estar vazios (para avisar que preencher um deles não tem desfazer).
fn object_fields(kind: ObjectKind) -> String {
    let mut out = vec!["id", "account_id"];
    out.extend(
        FIELDS
            .iter()
            .filter(|f| f.kinds.contains(&kind) && may_be_empty(f.value))
            .map(|f| f.read),
    );
    out.join(",")
}

/// Monta o plano de edição de um objeto a partir dos campos `(nome, valor normalizado)`.
/// `obj` é o objeto lido com [`object_fields`].
fn edit_plan(
    ctx: &Ctx,
    obj: &Value,
    id: &str,
    kind: ObjectKind,
    what: &str,
    changes: Vec<(&'static str, Value)>,
) -> Result<MetaPlan, ToolError> {
    if changes.is_empty() {
        return Err(ToolError::new(
            codes::INVALID_INPUT,
            "Informe pelo menos um campo para alterar.",
        ));
    }
    let mut params = Map::new();
    let mut plan = MetaPlan {
        inverse: InverseSpec::RestoreFields,
        ..MetaPlan::default()
    };
    let mut names = Vec::new();
    let mut warnings = Vec::new();
    let mut empty = Vec::new();
    for (name, value) in changes {
        let def = find(name, kind)
            .ok_or_else(|| invalid(name, format!("{name} não é editável aqui.")))?;
        let param = to_param(def, &value, ctx.exponent).map_err(|e| invalid(name, e))?;
        if may_be_empty(def.value) && normalize(def, obj, ctx.exponent).is_null() {
            empty.push(def.name);
        }
        params.insert(def.name.to_string(), param);
        names.push(def.name);
        if def.value == FieldKind::Minor && name.ends_with("_budget") && kind == ObjectKind::AdSet {
            plan.budget_objects.push(id.to_string());
        }
        if name == "status" && value == json!("ARCHIVED") {
            warnings.push(ARCHIVE_WARNING.to_string());
        }
        plan.field_changes.push(FieldChange {
            object_id: id.to_string(),
            kind,
            field: def.name,
            value,
        });
    }
    let mut summary = format!("Alterar {what} {id}: {}", names.join(", "));
    if names.contains(&"targeting") {
        summary.push_str(" (o novo público substitui o anterior inteiro)");
    }
    plan.summary = summary;
    // A execução confere de novo com o "antes" e, nesse caso, grava sem desfazer.
    if !empty.is_empty() {
        warnings.push(format!(
            "Não vai dar para desfazer: {} não tinha valor antes, e não há como voltar a \
             vazio pelo AdsConsole.",
            empty.join(", ")
        ));
    }
    plan.warning = (!warnings.is_empty()).then(|| warnings.join(" "));
    plan.steps.push(Step::Update {
        id: id.to_string(),
        params,
    });
    plan.watch.push(Watch {
        object_id: id.to_string(),
        kind,
        fields: names,
    });
    plan.budget_objects.dedup();
    Ok(plan)
}

/// Orçamento: só um dos dois por vez, e do mesmo tipo que o objeto já usa.
fn budget_changes(
    obj: &Value,
    daily: Option<Decimal>,
    lifetime: Option<Decimal>,
    changes: &mut Vec<(&'static str, Value)>,
) -> Result<(), ToolError> {
    match (daily, lifetime) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "daily_budget",
                "Informe daily_budget ou lifetime_budget, não os dois.",
            ));
        }
        (Some(d), None) => {
            if has_budget(obj, "lifetime_budget") {
                return Err(invalid(
                    "daily_budget",
                    "Este objeto usa orçamento total (lifetime_budget); a Meta não troca o tipo depois de criado.",
                ));
            }
            changes.push(("daily_budget", money("daily_budget", d)?));
        }
        (None, Some(l)) => {
            if has_budget(obj, "daily_budget") {
                return Err(invalid(
                    "lifetime_budget",
                    "Este objeto usa orçamento diário (daily_budget); a Meta não troca o tipo depois de criado.",
                ));
            }
            changes.push(("lifetime_budget", money("lifetime_budget", l)?));
        }
        (None, None) => {}
    }
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateCampaignInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign_id: String,
    #[serde(default)]
    name: Option<String>,
    /// ACTIVE, PAUSED ou ARCHIVED.
    #[serde(default)]
    status: Option<Status>,
    /// Orçamento diário da campanha (só com orçamento de campanha), na moeda da conta.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    lifetime_budget: Option<Decimal>,
    /// LOWEST_COST_WITHOUT_CAP, LOWEST_COST_WITH_BID_CAP, COST_CAP...
    #[serde(default)]
    bid_strategy: Option<String>,
    /// Limite de gasto da campanha, na moeda da conta.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    spend_cap: Option<Decimal>,
}

struct UpdateCampaign;

#[async_trait::async_trait]
impl WriteTool for UpdateCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateCampaignInput>(
            "update_campaign",
            "Altera nome, status, orçamento (de campanha), estratégia de lance ou limite de \
             gasto de uma campanha.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: UpdateCampaignInput = parse_input(input)?;
        let obj = get_object(
            ctx,
            "update_campaign",
            "Campanha",
            &i.campaign_id,
            &object_fields(ObjectKind::Campaign),
        )
        .await?;
        let mut changes = Vec::new();
        if let Some(n) = i.name {
            changes.push(("name", json!(n)));
        }
        if let Some(s) = i.status {
            changes.push(("status", status_value("status", s)?));
        }
        if (i.daily_budget.is_some() || i.lifetime_budget.is_some())
            && !has_budget(&obj, "daily_budget")
            && !has_budget(&obj, "lifetime_budget")
        {
            let field = if i.daily_budget.is_some() {
                "daily_budget"
            } else {
                "lifetime_budget"
            };
            return Err(invalid(
                field,
                "Esta campanha não usa orçamento de campanha; mude o orçamento nos ad sets.",
            ));
        }
        budget_changes(&obj, i.daily_budget, i.lifetime_budget, &mut changes)?;
        if let Some(b) = i.bid_strategy {
            changes.push(("bid_strategy", json!(b)));
        }
        if let Some(c) = i.spend_cap {
            changes.push(("spend_cap", money("spend_cap", c)?));
        }
        edit_plan(
            ctx,
            &obj,
            &i.campaign_id,
            ObjectKind::Campaign,
            "a campanha",
            changes,
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateAdsetInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    adset_id: String,
    #[serde(default)]
    name: Option<String>,
    /// ACTIVE, PAUSED ou ARCHIVED.
    #[serde(default)]
    status: Option<Status>,
    /// Na moeda da conta. A Meta só permite 4 mudanças de orçamento por hora por ad set.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    lifetime_budget: Option<Decimal>,
    #[serde(default)]
    bid_strategy: Option<String>,
    /// Lance máximo/alvo, na moeda da conta.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    bid_amount: Option<Decimal>,
    /// targeting_spec completo: substitui o público inteiro.
    #[serde(default)]
    targeting: Option<Value>,
    /// ISO 8601.
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    end_time: Option<String>,
}

struct UpdateAdset;

#[async_trait::async_trait]
impl WriteTool for UpdateAdset {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateAdsetInput>(
            "update_adset",
            "Altera nome, status, orçamento, lance, público ou agenda de um ad set. O \
             orçamento só pode mudar 4 vezes por hora.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: UpdateAdsetInput = parse_input(input)?;
        let obj = get_object(
            ctx,
            "update_adset",
            "Ad set",
            &i.adset_id,
            &object_fields(ObjectKind::AdSet),
        )
        .await?;
        let mut changes = Vec::new();
        if let Some(n) = i.name {
            changes.push(("name", json!(n)));
        }
        if let Some(s) = i.status {
            changes.push(("status", status_value("status", s)?));
        }
        budget_changes(&obj, i.daily_budget, i.lifetime_budget, &mut changes)?;
        if let Some(b) = i.bid_strategy {
            changes.push(("bid_strategy", json!(b)));
        }
        if let Some(b) = i.bid_amount {
            changes.push(("bid_amount", money("bid_amount", b)?));
        }
        if let Some(t) = i.targeting {
            if !t["geo_locations"].is_object() {
                return Err(invalid(
                    "targeting",
                    "Informe ao menos um local em targeting.geo_locations.",
                ));
            }
            changes.push(("targeting", t));
        }
        if let Some(t) = i.start_time {
            changes.push(("start_time", json!(t)));
        }
        if let Some(t) = i.end_time {
            changes.push(("end_time", json!(t)));
        }
        edit_plan(
            ctx,
            &obj,
            &i.adset_id,
            ObjectKind::AdSet,
            "o ad set",
            changes,
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateAdInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    ad_id: String,
    #[serde(default)]
    name: Option<String>,
    /// ACTIVE, PAUSED ou ARCHIVED.
    #[serde(default)]
    status: Option<Status>,
    /// Troca o criativo do anúncio.
    #[serde(default)]
    creative_id: Option<String>,
}

struct UpdateAd;

#[async_trait::async_trait]
impl WriteTool for UpdateAd {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateAdInput>(
            "update_ad",
            "Altera nome ou status de um anúncio, ou troca o criativo dele.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: UpdateAdInput = parse_input(input)?;
        let obj = get_object(
            ctx,
            "update_ad",
            "Anúncio",
            &i.ad_id,
            &object_fields(ObjectKind::Ad),
        )
        .await?;
        let mut changes = Vec::new();
        if let Some(n) = i.name {
            changes.push(("name", json!(n)));
        }
        if let Some(s) = i.status {
            changes.push(("status", status_value("status", s)?));
        }
        if let Some(c) = i.creative_id {
            changes.push(("creative", json!(c)));
        }
        edit_plan(ctx, &obj, &i.ad_id, ObjectKind::Ad, "o anúncio", changes)
    }
}
