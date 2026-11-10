//! Editing: `update_campaign`, `update_adset`, `update_ad`. Each field becomes a
//! `FieldChange`; undo goes back to "before" with `restore_fields`.

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

const ARCHIVE_WARNING: &str = "Archived objects do not deliver again unless reactivated.";

fn invalid(field: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

fn status_value(field: &str, s: Status) -> Result<Value, ToolError> {
    match s {
        Status::Active | Status::Paused | Status::Archived => Ok(json!(s.to_meta())),
        Status::Removed => Err(invalid(
            field,
            "Deleting is not allowed here (it is irreversible on Meta); use ARCHIVED.",
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
        .ok_or_else(|| invalid(field, "Invalid value."))
}

/// Does it have a budget of this type? (Meta returns it missing or `"0"` when it does not.)
fn has_budget(obj: &Value, field: &str) -> bool {
    match &obj[field] {
        Value::String(s) => s.parse::<i64>().is_ok_and(|n| n > 0),
        Value::Number(n) => n.as_i64().is_some_and(|n| n > 0),
        _ => false,
    }
}

/// Fields Meta may leave without a value (no spend cap, no end date...).
fn may_be_empty(kind: FieldKind) -> bool {
    matches!(
        kind,
        FieldKind::Minor | FieldKind::Enum | FieldKind::DateTime
    )
}

/// `fields=` of the object read before editing: id, account and the fields that may be
/// empty (to warn that filling one of them has no undo).
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

/// Builds the edit plan of an object from the `(name, normalized value)` fields.
/// `obj` is the object read with [`object_fields`].
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
            "Provide at least one field to change.",
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
            .ok_or_else(|| invalid(name, format!("{name} is not editable here.")))?;
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
    let mut summary = format!("Change {what} {id}: {}", names.join(", "));
    if names.contains(&"targeting") {
        summary.push_str(" (the new audience replaces the whole previous one)");
    }
    plan.summary = summary;
    // Execution checks again against "before" and, in that case, stores no undo.
    if !empty.is_empty() {
        warnings.push(format!(
            "Undo will not be possible: {} had no value before, and AdsConsole cannot set \
             it back to empty.",
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

/// Budget: only one of the two at a time, and of the same type the object already uses.
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
                "Provide daily_budget or lifetime_budget, not both.",
            ));
        }
        (Some(d), None) => {
            if has_budget(obj, "lifetime_budget") {
                return Err(invalid(
                    "daily_budget",
                    "This object uses a lifetime budget (lifetime_budget); Meta does not allow switching the budget type after creation.",
                ));
            }
            changes.push(("daily_budget", money("daily_budget", d)?));
        }
        (None, Some(l)) => {
            if has_budget(obj, "daily_budget") {
                return Err(invalid(
                    "lifetime_budget",
                    "This object uses a daily budget (daily_budget); Meta does not allow switching the budget type after creation.",
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
    /// ACTIVE, PAUSED or ARCHIVED.
    #[serde(default)]
    status: Option<Status>,
    /// Campaign daily budget (only with a campaign budget), in the account currency.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    lifetime_budget: Option<Decimal>,
    /// LOWEST_COST_WITHOUT_CAP, LOWEST_COST_WITH_BID_CAP, COST_CAP...
    #[serde(default)]
    bid_strategy: Option<String>,
    /// Campaign spend cap, in the account currency.
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
            "Changes the name, status, (campaign) budget, bid strategy or spend cap of a \
             campaign.",
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
            "Campaign",
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
                "This campaign does not use a campaign budget; change the budget on its ad sets.",
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
            "campaign",
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
    /// ACTIVE, PAUSED or ARCHIVED.
    #[serde(default)]
    status: Option<Status>,
    /// In the account currency. Meta allows only 4 budget changes per hour per ad set.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    lifetime_budget: Option<Decimal>,
    #[serde(default)]
    bid_strategy: Option<String>,
    /// Maximum/target bid, in the account currency.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    bid_amount: Option<Decimal>,
    /// Full targeting_spec: replaces the whole audience.
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
            "Changes the name, status, budget, bid, audience or schedule of an ad set. The \
             budget can change only 4 times per hour.",
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
                    "Provide at least one location in targeting.geo_locations.",
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
        edit_plan(ctx, &obj, &i.adset_id, ObjectKind::AdSet, "ad set", changes)
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
    /// ACTIVE, PAUSED or ARCHIVED.
    #[serde(default)]
    status: Option<Status>,
    /// Replaces the ad's creative.
    #[serde(default)]
    creative_id: Option<String>,
}

struct UpdateAd;

#[async_trait::async_trait]
impl WriteTool for UpdateAd {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateAdInput>(
            "update_ad",
            "Changes the name or status of an ad, or replaces its creative.",
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
            "Ad",
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
        edit_plan(ctx, &obj, &i.ad_id, ObjectKind::Ad, "ad", changes)
    }
}
