//! Campaign, ad group and ad edits: `update_campaign`, `update_ad_group` and
//! `update_ad`. All reversible through the `restore_fields` inverse.
//!
//! The plan reads the resource (to validate and build the "before → after" summary)
//! and uses [`Edits`] to turn it into update operations, `field_changes` and `watch`.

use adsconsole_core::Status;
use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::{self, Decimal};
use adsconsole_google_ads::fields::{self, EditableField};
use adsconsole_google_ads::resources;
use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::prelude::ToPrimitive;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Number, Value, json};

use super::reports::{at, invalid, normalize_status, text, valid_id};
use super::{GTool, WriteTool, own_resource, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::google::plan::{FieldChange, InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Write(Box::new(UpdateCampaign)),
        GTool::Write(Box::new(UpdateAdGroup)),
        GTool::Write(Box::new(UpdateAd)),
    ]
}

const UPDATE_CAMPAIGN: &str = "update_campaign";
const UPDATE_AD_GROUP: &str = "update_ad_group";
const UPDATE_AD: &str = "update_ad";

pub(crate) const SHARED_BUDGET_WARNING: &str =
    "This budget is shared with other campaigns, and all of them will be affected.";
pub(crate) const PINS_WARNING: &str = "The current headline/description pins will be removed.";
const BIDDING_SWITCH: &str = "Changing the bidding strategy type is not supported yet; \
                              adjust the target of the current strategy.";

// ---------------------------------------------------------------- helpers

/// Status accepted in edits (`ENABLED` is an alias of `ACTIVE`).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub(crate) enum EditStatus {
    #[serde(rename = "ACTIVE", alias = "ENABLED")]
    Active,
    #[serde(rename = "PAUSED")]
    Paused,
}

impl EditStatus {
    pub(crate) fn to_google(self) -> Result<&'static str, ToolError> {
        let s = match self {
            EditStatus::Active => Status::Active,
            EditStatus::Paused => Status::Paused,
        };
        s.to_google().map_err(internal)
    }
}

/// How the value appears in the summary.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Show {
    /// `'text'`
    Quoted,
    /// As is (numbers).
    Plain,
    /// Only the date of a `YYYY-MM-DD HH:mm:ss` datetime.
    Date,
    /// `ENABLED` → `ACTIVE`.
    Status,
    /// Number followed by the account currency.
    Money,
    /// List length.
    Count,
    /// Quoted items, comma-separated.
    List,
}

fn show(v: &Value, how: Show) -> String {
    if v.is_null() {
        return "—".into();
    }
    match (how, v) {
        (Show::Quoted, Value::String(s)) => format!("'{s}'"),
        (Show::Status, Value::String(s)) => normalize_status(s),
        (Show::Date, Value::String(s)) => fields::date_part(s).to_string(),
        (Show::Count, Value::Array(a)) => a.len().to_string(),
        (Show::List, Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => format!("'{s}'"),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        (_, Value::String(s)) => s.clone(),
        (_, other) => other.to_string(),
    }
}

/// Micros → units in the [`fields::normalize`] format (integer when exact).
pub(crate) fn units(micros: i64) -> Value {
    let d = money::from_micros(micros).normalize();
    if d.scale() == 0 {
        return d.to_i64().map_or(Value::Null, |i| json!(i));
    }
    d.to_f64()
        .and_then(Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

/// Input money amount (> 0, no more decimals than the currency allows) in micros.
pub(crate) fn positive_micros(
    amount: Decimal,
    currency: &str,
    field: &str,
) -> Result<i64, ToolError> {
    if amount <= Decimal::ZERO {
        return Err(invalid(format!("{field} must be greater than zero.")).with_field(field));
    }
    money::to_micros_checked(amount, currency)
        .map_err(|e| invalid(format!("{field}: {e}")).with_field(field))
}

/// First row of the query (the queries here already have `LIMIT 1`).
pub(crate) async fn read_one(
    ctx: &Ctx,
    tool: &str,
    query: &str,
) -> Result<Option<Value>, ToolError> {
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), query, None)
        .await
        .map_err(|e| translate(e, Some(ctx), tool))?;
    Ok(page.results.into_iter().next())
}

fn not_found(what: &str, ctx: &Ctx) -> ToolError {
    ToolError::new(
        codes::NOT_FOUND,
        format!("{what} not found in account {}.", ctx.customer_id),
    )
}

pub(crate) fn date(s: &str, field: &str) -> Result<NaiveDate, ToolError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| invalid(format!("{field} must be in YYYY-MM-DD format.")).with_field(field))
}

/// Edits of existing resources, grouped by resource name (in the order they
/// appear), with the "field before → after" summary.
pub(crate) struct Edits {
    head: String,
    currency: String,
    groups: Vec<(String, Vec<(&'static EditableField, Value)>)>,
    parts: Vec<String>,
}

impl Edits {
    /// `head` starts the summary, e.g. `Campaign 123`.
    pub(crate) fn new(head: impl Into<String>, currency: &str) -> Self {
        Self {
            head: head.into(),
            currency: currency.to_string(),
            groups: Vec::new(),
            parts: Vec::new(),
        }
    }

    /// Sets `gaql` on `rn` to `new` (already normalized). The summary's "before"
    /// comes from `row`, a search row with the field's resource at the root.
    pub(crate) fn set(
        &mut self,
        rn: &str,
        gaql: &'static str,
        row: &Value,
        new: Value,
        label: &str,
        how: Show,
    ) -> Result<(), ToolError> {
        let field = fields::find(gaql).ok_or_else(|| {
            ToolError::new(codes::INTERNAL, format!("Field not in the table: {gaql}"))
        })?;
        let before = fields::normalize(field, row);
        let mut part = format!("{label} {} → {}", show(&before, how), show(&new, how));
        if matches!(how, Show::Money) {
            part.push(' ');
            part.push_str(&self.currency);
        }
        self.parts.push(part);
        match self.groups.iter_mut().find(|(r, _)| r == rn) {
            Some((_, changes)) => changes.push((field, new)),
            None => self.groups.push((rn.to_string(), vec![(field, new)])),
        }
        Ok(())
    }

    /// Like [`Edits::set`], for an input money amount in the account currency.
    pub(crate) fn money(
        &mut self,
        rn: &str,
        gaql: &'static str,
        row: &Value,
        amount: Decimal,
        input_field: &str,
        label: &str,
    ) -> Result<(), ToolError> {
        let micros = positive_micros(amount, &self.currency, input_field)?;
        self.set(rn, gaql, row, units(micros), label, Show::Money)
    }

    /// Plan with the `restore_fields` inverse. With no edits, `INVALID_INPUT`.
    pub(crate) fn into_plan(self, warning: Option<String>) -> Result<WritePlan, ToolError> {
        if self.groups.is_empty() {
            return Err(invalid("Provide at least one field to change."));
        }
        let mut operations = Vec::new();
        let mut field_changes = Vec::new();
        let mut watch = Vec::new();
        for (rn, changes) in &self.groups {
            operations.push(fields::update_op(rn, changes).map_err(invalid)?);
            for (f, v) in changes {
                field_changes.push(FieldChange {
                    resource_name: rn.clone(),
                    field: f.gaql,
                    value: v.clone(),
                });
            }
            watch.push(Watch {
                resource_name: rn.clone(),
                resource: changes[0].0.resource,
                fields: changes.iter().map(|(f, _)| f.gaql).collect(),
            });
        }
        Ok(WritePlan {
            summary: format!("{}: {}", self.head, self.parts.join(", ")),
            warning,
            operations,
            field_changes,
            watch,
            inverse: InverseSpec::RestoreFields,
            direct: None,
        })
    }
}

// --------------------------------------------------------- update_campaign

/// Target of the campaign's current bidding strategy.
#[derive(Debug, Deserialize, JsonSchema)]
// The variant names are the input keys (`maximize_clicks`, …).
#[allow(clippy::enum_variant_names)]
#[serde(rename_all = "snake_case")]
enum BiddingIn {
    /// Maximize clicks (`TARGET_SPEND`).
    MaximizeClicks {
        /// CPC cap, in the account currency.
        #[serde(default, with = "rust_decimal::serde::float_option")]
        #[schemars(with = "Option<f64>")]
        cpc_cap: Option<Decimal>,
    },
    /// Maximize conversions.
    MaximizeConversions {
        /// Target CPA, in the account currency.
        #[serde(default, with = "rust_decimal::serde::float_option")]
        #[schemars(with = "Option<f64>")]
        target_cpa: Option<Decimal>,
    },
    /// Maximize conversion value.
    MaximizeConversionValue {
        /// Target ROAS (e.g. 3.5 = 350%).
        #[serde(default)]
        target_roas: Option<f64>,
    },
}

impl BiddingIn {
    fn strategy_type(&self) -> &'static str {
        match self {
            BiddingIn::MaximizeClicks { .. } => "TARGET_SPEND",
            BiddingIn::MaximizeConversions { .. } => "MAXIMIZE_CONVERSIONS",
            BiddingIn::MaximizeConversionValue { .. } => "MAXIMIZE_CONVERSION_VALUE",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateCampaignInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign_id: String,
    #[serde(default)]
    name: Option<String>,
    /// `ACTIVE` or `PAUSED`.
    #[serde(default)]
    status: Option<EditStatus>,
    /// Daily budget, in the account currency.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
    /// Only adjusts the target of the current strategy; does not change the strategy type.
    #[serde(default)]
    bidding: Option<BiddingIn>,
    /// YYYY-MM-DD.
    #[serde(default)]
    start_date: Option<String>,
    /// YYYY-MM-DD.
    #[serde(default)]
    end_date: Option<String>,
}

struct UpdateCampaign;

#[async_trait]
impl WriteTool for UpdateCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateCampaignInput>(
            UPDATE_CAMPAIGN,
            "Edits a Google Ads campaign: name, status, daily budget, target of the current \
             bidding strategy, and start and end dates.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: UpdateCampaignInput = parse_input(input)?;
        let id = valid_id(&input.campaign_id, "campaign_id")?;
        if input.name.is_none()
            && input.status.is_none()
            && input.daily_budget.is_none()
            && input.bidding.is_none()
            && input.start_date.is_none()
            && input.end_date.is_none()
        {
            return Err(invalid("Provide at least one field to change."));
        }
        if let (Some(s), Some(e)) = (&input.start_date, &input.end_date)
            && date(s, "start_date")? > date(e, "end_date")?
        {
            return Err(invalid("start_date must not be after end_date.").with_field("end_date"));
        }
        let query = format!(
            "SELECT campaign.resource_name, campaign.name, campaign.status, \
             campaign.bidding_strategy_type, campaign.bidding_strategy, \
             campaign.campaign_budget, campaign.start_date_time, campaign.end_date_time, \
             campaign.target_spend.cpc_bid_ceiling_micros, \
             campaign.maximize_conversions.target_cpa_micros, \
             campaign.maximize_conversion_value.target_roas, \
             campaign_budget.amount_micros, campaign_budget.explicitly_shared \
             FROM campaign WHERE campaign.id = {id} AND campaign.status != 'REMOVED' LIMIT 1"
        );
        let row = read_one(ctx, UPDATE_CAMPAIGN, &query)
            .await?
            .ok_or_else(|| not_found(&format!("Campaign {id}"), ctx))?;
        let rn = resources::rn(&ctx.customer_id, "campaigns", &id);
        let mut edits = Edits::new(format!("Campaign {id}"), &ctx.currency);
        let mut warning = None;

        if let Some(name) = &input.name {
            if name.trim().is_empty() {
                return Err(invalid("The name cannot be empty.").with_field("name"));
            }
            edits.set(
                &rn,
                "campaign.name",
                &row,
                json!(name),
                "name",
                Show::Quoted,
            )?;
        }
        if let Some(status) = input.status {
            let s = status.to_google()?;
            edits.set(
                &rn,
                "campaign.status",
                &row,
                json!(s),
                "status",
                Show::Status,
            )?;
        }
        if let Some(amount) = input.daily_budget {
            let budget = text(&row, "campaign.campaignBudget").ok_or_else(|| {
                invalid("The campaign has no budget of its own.").with_field("daily_budget")
            })?;
            own_resource(ctx, &budget)?;
            if super::reports::flag(&row, "campaignBudget.explicitlyShared") {
                warning = Some(SHARED_BUDGET_WARNING.to_string());
            }
            edits.money(
                &budget,
                "campaign_budget.amount_micros",
                &row,
                amount,
                "daily_budget",
                "budget",
            )?;
        }
        if let Some(bidding) = &input.bidding {
            let current = text(&row, "campaign.biddingStrategyType").unwrap_or_default();
            if current != bidding.strategy_type() {
                return Err(invalid(BIDDING_SWITCH).with_field("bidding"));
            }
            if text(&row, "campaign.biddingStrategy").is_some() {
                return Err(invalid(
                    "The campaign uses a portfolio bidding strategy; adjust the target on the strategy itself.",
                )
                .with_field("bidding"));
            }
            let no_target =
                || invalid("Provide the bidding strategy target.").with_field("bidding");
            match bidding {
                BiddingIn::MaximizeClicks { cpc_cap } => edits.money(
                    &rn,
                    "campaign.target_spend.cpc_bid_ceiling_micros",
                    &row,
                    cpc_cap.ok_or_else(no_target)?,
                    "bidding",
                    "CPC cap",
                )?,
                BiddingIn::MaximizeConversions { target_cpa } => edits.money(
                    &rn,
                    "campaign.maximize_conversions.target_cpa_micros",
                    &row,
                    target_cpa.ok_or_else(no_target)?,
                    "bidding",
                    "target CPA",
                )?,
                BiddingIn::MaximizeConversionValue { target_roas } => {
                    let roas = target_roas.ok_or_else(no_target)?;
                    let value = Number::from_f64(roas)
                        .filter(|_| roas > 0.0)
                        .ok_or_else(|| {
                            invalid("target_roas must be greater than zero.").with_field("bidding")
                        })?;
                    edits.set(
                        &rn,
                        "campaign.maximize_conversion_value.target_roas",
                        &row,
                        Value::Number(value),
                        "target ROAS",
                        Show::Plain,
                    )?;
                }
            }
        }
        // Dates: v26 only has start_date_time/end_date_time (see fields.rs).
        // With only one of the dates, compare it with the campaign's current other date.
        let current_date =
            |path: &str| text(&row, path).and_then(|d| date(fields::date_part(&d), path).ok());
        match (&input.start_date, &input.end_date) {
            (Some(s), None) => {
                if let Some(end) = current_date("campaign.endDateTime")
                    && date(s, "start_date")? > end
                {
                    return Err(invalid(format!(
                        "start_date cannot be after the campaign's current end date ({end})."
                    ))
                    .with_field("start_date"));
                }
            }
            (None, Some(e)) => {
                if let Some(start) = current_date("campaign.startDateTime")
                    && date(e, "end_date")? < start
                {
                    return Err(invalid(format!(
                        "end_date cannot be before the campaign's current start date ({start})."
                    ))
                    .with_field("end_date"));
                }
            }
            _ => {}
        }
        if let Some(s) = &input.start_date {
            date(s, "start_date")?;
            edits.set(
                &rn,
                "campaign.start_date_time",
                &row,
                json!(fields::start_of_day(s)),
                "start",
                Show::Date,
            )?;
        }
        if let Some(e) = &input.end_date {
            date(e, "end_date")?;
            edits.set(
                &rn,
                "campaign.end_date_time",
                &row,
                json!(fields::end_of_day(e)),
                "end",
                Show::Date,
            )?;
        }
        edits.into_plan(warning)
    }
}

// --------------------------------------------------------- update_ad_group

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateAdGroupInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    ad_group_id: String,
    #[serde(default)]
    name: Option<String>,
    /// `ACTIVE` or `PAUSED`.
    #[serde(default)]
    status: Option<EditStatus>,
    /// The ad group's max CPC bid, in the account currency.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
}

struct UpdateAdGroup;

#[async_trait]
impl WriteTool for UpdateAdGroup {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateAdGroupInput>(
            UPDATE_AD_GROUP,
            "Edits a Google Ads ad group: name, status and CPC bid.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: UpdateAdGroupInput = parse_input(input)?;
        let id = valid_id(&input.ad_group_id, "ad_group_id")?;
        if input.name.is_none() && input.status.is_none() && input.cpc_bid.is_none() {
            return Err(invalid("Provide at least one field to change."));
        }
        let query = format!(
            "SELECT ad_group.resource_name, ad_group.name, ad_group.status, \
             ad_group.cpc_bid_micros FROM ad_group \
             WHERE ad_group.id = {id} AND ad_group.status != 'REMOVED' LIMIT 1"
        );
        let row = read_one(ctx, UPDATE_AD_GROUP, &query)
            .await?
            .ok_or_else(|| not_found(&format!("Ad group {id}"), ctx))?;
        let rn = resources::rn(&ctx.customer_id, "adGroups", &id);
        let mut edits = Edits::new(format!("Ad group {id}"), &ctx.currency);
        if let Some(name) = &input.name {
            if name.trim().is_empty() {
                return Err(invalid("The name cannot be empty.").with_field("name"));
            }
            edits.set(
                &rn,
                "ad_group.name",
                &row,
                json!(name),
                "name",
                Show::Quoted,
            )?;
        }
        if let Some(status) = input.status {
            let s = status.to_google()?;
            edits.set(
                &rn,
                "ad_group.status",
                &row,
                json!(s),
                "status",
                Show::Status,
            )?;
        }
        if let Some(bid) = input.cpc_bid {
            edits.money(
                &rn,
                "ad_group.cpc_bid_micros",
                &row,
                bid,
                "cpc_bid",
                "CPC bid",
            )?;
        }
        edits.into_plan(None)
    }
}

// --------------------------------------------------------------- update_ad

pub(crate) const HEADLINES: (usize, usize, usize) = (3, 15, 30);
pub(crate) const DESCRIPTIONS: (usize, usize, usize) = (2, 4, 90);

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateAdInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    ad_group_id: String,
    ad_id: String,
    /// `ACTIVE` or `PAUSED`.
    #[serde(default)]
    status: Option<EditStatus>,
    /// Final URLs (replace the current ones).
    #[serde(default)]
    final_urls: Option<Vec<String>>,
    /// Responsive search ads only: 3 to 15 headlines of up to 30 characters
    /// (replace the current ones, with no pins).
    #[serde(default)]
    headlines: Option<Vec<String>>,
    /// Responsive search ads only: 2 to 4 descriptions of up to 90 characters.
    #[serde(default)]
    descriptions: Option<Vec<String>>,
}

/// RSA texts as `AdTextAsset`s, within the count and length limits.
pub(crate) fn ad_texts(
    items: &[String],
    (min, max, len): (usize, usize, usize),
    field: &str,
) -> Result<Value, ToolError> {
    if items.len() < min || items.len() > max {
        return Err(invalid(format!("{field}: provide {min} to {max} items.")).with_field(field));
    }
    if let Some(t) = items
        .iter()
        .find(|t| t.trim().is_empty() || t.chars().count() > len)
    {
        return Err(invalid(format!(
            "{field}: each item must have 1 to {len} characters ('{t}')."
        ))
        .with_field(field));
    }
    Ok(Value::Array(
        items.iter().map(|t| json!({ "text": t })).collect(),
    ))
}

pub(crate) fn final_urls(urls: &[String]) -> Result<Value, ToolError> {
    if urls.is_empty() {
        return Err(invalid("Provide at least one final URL.").with_field("final_urls"));
    }
    for u in urls {
        final_url(u, "final_urls")?;
    }
    Ok(json!(urls))
}

/// Final URL `http(s)://…`; `field` is the field named in the error.
pub(crate) fn final_url(url: &str, field: &str) -> Result<(), ToolError> {
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(());
    }
    Err(invalid(format!("Invalid final URL: {url}")).with_field(field))
}

struct UpdateAd;

#[async_trait]
impl WriteTool for UpdateAd {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateAdInput>(
            UPDATE_AD,
            "Edits a Google Ads ad: status, final URLs and, on a responsive search ad, \
             headlines and descriptions.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: UpdateAdInput = parse_input(input)?;
        let group = valid_id(&input.ad_group_id, "ad_group_id")?;
        let ad = valid_id(&input.ad_id, "ad_id")?;
        // Local checks before the read.
        let headlines = input
            .headlines
            .as_deref()
            .map(|h| ad_texts(h, HEADLINES, "headlines"))
            .transpose()?;
        let descriptions = input
            .descriptions
            .as_deref()
            .map(|d| ad_texts(d, DESCRIPTIONS, "descriptions"))
            .transpose()?;
        let urls = input.final_urls.as_deref().map(final_urls).transpose()?;
        if input.status.is_none() && headlines.is_none() && descriptions.is_none() && urls.is_none()
        {
            return Err(invalid("Provide at least one field to change."));
        }

        let query = format!(
            "SELECT ad_group_ad.resource_name, ad_group_ad.status, ad_group_ad.ad.type, \
             ad_group_ad.ad.final_urls, ad_group_ad.ad.responsive_search_ad.headlines, \
             ad_group_ad.ad.responsive_search_ad.descriptions FROM ad_group_ad \
             WHERE ad_group.id = {group} AND ad_group_ad.ad.id = {ad} \
             AND ad_group_ad.status != 'REMOVED' LIMIT 1"
        );
        let row = read_one(ctx, UPDATE_AD, &query)
            .await?
            .ok_or_else(|| not_found(&format!("Ad {ad} in ad group {group}"), ctx))?;
        if (headlines.is_some() || descriptions.is_some())
            && text(&row, "adGroupAd.ad.type").as_deref() != Some("RESPONSIVE_SEARCH_AD")
        {
            return Err(invalid(
                "Headlines and descriptions can only be edited on a responsive search ad.",
            )
            .with_field(if headlines.is_some() {
                "headlines"
            } else {
                "descriptions"
            }));
        }
        let ad_group_ad = resources::rn(&ctx.customer_id, "adGroupAds", &format!("{group}~{ad}"));
        // Verified on a real account: texts and URLs go on the `ad` resource (`AdOperation`),
        // not on ad_group_ad.
        let ad_rn = resources::rn(&ctx.customer_id, "ads", &ad);
        let ad_row = json!({ "ad": row["adGroupAd"]["ad"] });
        let pinned = |path: &str| {
            at(&row, path)
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|i| at(i, "pinnedField").is_some()))
        };
        let warning = ((headlines.is_some()
            && pinned("adGroupAd.ad.responsiveSearchAd.headlines"))
            || (descriptions.is_some() && pinned("adGroupAd.ad.responsiveSearchAd.descriptions")))
        .then(|| PINS_WARNING.to_string());

        let mut edits = Edits::new(format!("Ad {ad}"), &ctx.currency);
        if let Some(status) = input.status {
            let s = status.to_google()?;
            edits.set(
                &ad_group_ad,
                "ad_group_ad.status",
                &row,
                json!(s),
                "status",
                Show::Status,
            )?;
        }
        if let Some(u) = urls {
            edits.set(
                &ad_rn,
                "ad.final_urls",
                &ad_row,
                u,
                "final URLs",
                Show::List,
            )?;
        }
        if let Some(h) = headlines {
            edits.set(
                &ad_rn,
                "ad.responsive_search_ad.headlines",
                &ad_row,
                h,
                "headlines",
                Show::Count,
            )?;
        }
        if let Some(d) = descriptions {
            edits.set(
                &ad_rn,
                "ad.responsive_search_ad.descriptions",
                &ad_row,
                d,
                "descriptions",
                Show::Count,
            )?;
        }
        edits.into_plan(warning)
    }
}
