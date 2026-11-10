//! Structured Google Ads reads: campaigns, details, search terms, recommendations,
//! conversion actions and change history.
//!
//! Conventions: money in account currency units (`adsconsole_core::money`), normalized
//! statuses (`ACTIVE`/`PAUSED`/`REMOVED`; an unknown value is kept as is), ids as text,
//! and every query has a `LIMIT`.

use std::collections::BTreeMap;

use adsconsole_core::Status;
use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::{self, Decimal};
use adsconsole_google_ads::fields;
use adsconsole_google_ads::gaql::{self, DatePreset, DateRange};
use adsconsole_google_ads::resources;
use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{GTool, ReadTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::truncate::{ITEMS_BUDGET, fit_items};

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Read(Box::new(GetCampaigns)),
        GTool::Read(Box::new(GetCampaignDetails)),
        GTool::Read(Box::new(GetSearchTerms)),
        GTool::Read(Box::new(GetRecommendations)),
        GTool::Read(Box::new(GetConversionActions)),
        GTool::Read(Box::new(GetChangeHistory)),
    ]
}

const CAMPAIGNS_TOOL: &str = "get_campaigns";
const CAMPAIGN_DETAILS_TOOL: &str = "get_campaign_details";
const SEARCH_TERMS_TOOL: &str = "get_search_terms";
const RECOMMENDATIONS_TOOL: &str = "get_recommendations";
const CONVERSION_ACTIONS_TOOL: &str = "get_conversion_actions";
const CHANGE_HISTORY_TOOL: &str = "get_change_history";

// ---------------------------------------------------------------- helpers

pub(crate) fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, msg)
}

pub(crate) fn valid_id(s: &str, field: &str) -> Result<String, ToolError> {
    gaql::id(s)
        .map(str::to_string)
        .map_err(|e| invalid(e.to_string()).with_field(field))
}

pub(crate) fn bounded(
    value: Option<u32>,
    default: u32,
    min: u32,
    max: u32,
    field: &str,
) -> Result<u32, ToolError> {
    let v = value.unwrap_or(default);
    if (min..=max).contains(&v) {
        Ok(v)
    } else {
        Err(invalid(format!("{field} must be between {min} and {max}.")).with_field(field))
    }
}

/// Dotted path (`"metrics.costMicros"`) inside the API row.
pub(crate) fn at<'a>(row: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(row, |v, k| v.get(k))
        .filter(|v| !v.is_null())
}

pub(crate) fn text(row: &Value, path: &str) -> Option<String> {
    match at(row, path)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn text_or_empty(row: &Value, path: &str) -> String {
    text(row, path).unwrap_or_default()
}

/// API int64 (sent as text) or number.
pub(crate) fn int(row: &Value, path: &str) -> i64 {
    match at(row, path) {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        _ => 0,
    }
}

pub(crate) fn float(row: &Value, path: &str) -> f64 {
    match at(row, path) {
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

pub(crate) fn flag(row: &Value, path: &str) -> bool {
    at(row, path).and_then(Value::as_bool).unwrap_or(false)
}

/// Micros (text, integer or double, like `averageCpc`) to currency units.
pub(crate) fn money_at(row: &Value, path: &str) -> Option<Decimal> {
    match at(row, path)? {
        Value::String(s) => s.parse::<i64>().ok().map(money::from_micros),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Some(money::from_micros(i)),
            None => Decimal::try_from(n.as_f64()?)
                .ok()
                .map(|d| (d / Decimal::from(1_000_000)).round_dp(6).normalize()),
        },
        _ => None,
    }
}

fn money_or_zero(row: &Value, path: &str) -> Decimal {
    money_at(row, path).unwrap_or_default()
}

/// `ENABLED` → `ACTIVE`, etc. A value the core doesn't know is kept as is.
pub(crate) fn normalize_status(raw: &str) -> String {
    match Status::from_google(raw) {
        Ok(Status::Active) => "ACTIVE".into(),
        Ok(Status::Paused) => "PAUSED".into(),
        Ok(Status::Removed) => "REMOVED".into(),
        _ => raw.to_string(),
    }
}

fn status_at(row: &Value, path: &str) -> String {
    text(row, path)
        .map(|s| normalize_status(&s))
        .unwrap_or_default()
}

/// Last numeric segment of a resource name (`customers/1/campaigns/5` → `5`).
fn last_id(rn: Option<String>) -> Option<String> {
    rn.and_then(|r| r.rsplit('/').next().map(str::to_string))
}

async fn search(
    ctx: &Ctx,
    tool: &str,
    query: &str,
    max_rows: usize,
) -> Result<Vec<Value>, ToolError> {
    ctx.client
        .search_all(&ctx.customer_id, ctx.login.as_deref(), query, max_rows)
        .await
        .map_err(|e| translate(e, Some(ctx), tool))
}

pub(crate) fn to_json<T: Serialize>(out: T) -> Result<Value, ToolError> {
    serde_json::to_value(out).map_err(internal)
}

// ------------------------------------------------------------ date range

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
enum PresetIn {
    #[serde(rename = "TODAY")]
    Today,
    #[serde(rename = "YESTERDAY")]
    Yesterday,
    #[serde(rename = "LAST_7_DAYS")]
    Last7Days,
    #[serde(rename = "LAST_14_DAYS")]
    Last14Days,
    #[serde(rename = "LAST_30_DAYS")]
    Last30Days,
    #[serde(rename = "THIS_MONTH")]
    ThisMonth,
    #[serde(rename = "LAST_MONTH")]
    LastMonth,
}

/// Date range: a preset or a `YYYY-MM-DD` interval. Default: `LAST_30_DAYS`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
enum DateRangeIn {
    Preset { preset: PresetIn },
    Between { start: String, end: String },
}

impl DateRangeIn {
    fn to_gaql(&self) -> Result<String, ToolError> {
        let range = match self {
            Self::Preset { preset } => DateRange::Preset(match preset {
                PresetIn::Today => DatePreset::Today,
                PresetIn::Yesterday => DatePreset::Yesterday,
                PresetIn::Last7Days => DatePreset::Last7Days,
                PresetIn::Last14Days => DatePreset::Last14Days,
                PresetIn::Last30Days => DatePreset::Last30Days,
                PresetIn::ThisMonth => DatePreset::ThisMonth,
                PresetIn::LastMonth => DatePreset::LastMonth,
            }),
            Self::Between { start, end } => DateRange::Between {
                start: parse_date(start, "date_range.start")?,
                end: parse_date(end, "date_range.end")?,
            },
        };
        gaql::date_clause(&range).map_err(|e| invalid(e.to_string()).with_field("date_range"))
    }
}

fn parse_date(s: &str, field: &str) -> Result<NaiveDate, ToolError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| invalid(format!("Invalid date ({s:?}); use YYYY-MM-DD.")).with_field(field))
}

fn date_clause(range: &Option<DateRangeIn>) -> Result<String, ToolError> {
    match range {
        Some(r) => r.to_gaql(),
        None => gaql::date_clause(&DateRange::default()).map_err(|e| invalid(e.to_string())),
    }
}

// ----------------------------------------------------------- get_campaigns

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
enum StatusIn {
    #[serde(rename = "ACTIVE", alias = "ENABLED")]
    Active,
    #[serde(rename = "PAUSED")]
    Paused,
    #[serde(rename = "REMOVED")]
    Removed,
}

impl StatusIn {
    fn gaql(self) -> &'static str {
        match self {
            Self::Active => "ENABLED",
            Self::Paused => "PAUSED",
            Self::Removed => "REMOVED",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetCampaignsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    #[serde(default)]
    date_range: Option<DateRangeIn>,
    /// Default: all except removed ones.
    #[serde(default)]
    status: Option<Vec<StatusIn>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CampaignRow {
    campaign_id: String,
    name: String,
    /// `ACTIVE`, `PAUSED` or `REMOVED`.
    status: String,
    /// Channel type (`SEARCH`, `PERFORMANCE_MAX`, ...).
    channel: String,
    /// Daily budget, in the account currency.
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    budget: Option<Decimal>,
    bidding_strategy: String,
    impressions: i64,
    clicks: i64,
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    cost: Decimal,
    conversions: f64,
    conversions_value: f64,
    ctr: f64,
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    average_cpc: Decimal,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetCampaignsOutput {
    campaigns: Vec<CampaignRow>,
    /// There are more campaigns than the size limit; use `gaql_search` with a cursor for the rest.
    truncated: bool,
}

const CAMPAIGN_LIMIT: usize = 1000;

struct GetCampaigns;

#[async_trait]
impl ReadTool for GetCampaigns {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetCampaignsInput>(
            CAMPAIGNS_TOOL,
            "Lists the account's campaigns with budget and metrics for the date range \
             (default: last 30 days, removed campaigns excluded). Money in the account currency.",
            ToolGroup::Read,
        )
        .output::<GetCampaignsOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: GetCampaignsInput = parse_input(input)?;
        let status_clause = match input.status.as_deref() {
            None => "campaign.status != 'REMOVED'".to_string(),
            Some([]) => return Err(invalid("status cannot be empty.").with_field("status")),
            Some(list) => {
                let items: Vec<String> = list.iter().map(|s| gaql::quote(s.gaql())).collect();
                format!("campaign.status IN ({})", items.join(", "))
            }
        };
        let query = format!(
            "SELECT campaign.id, campaign.name, campaign.status, \
             campaign.advertising_channel_type, campaign.bidding_strategy_type, \
             campaign_budget.amount_micros, metrics.impressions, metrics.clicks, \
             metrics.cost_micros, metrics.conversions, metrics.conversions_value, \
             metrics.ctr, metrics.average_cpc FROM campaign \
             WHERE {} AND {status_clause} ORDER BY metrics.cost_micros DESC LIMIT {CAMPAIGN_LIMIT}",
            date_clause(&input.date_range)?
        );
        let rows = search(ctx, CAMPAIGNS_TOOL, &query, CAMPAIGN_LIMIT).await?;
        let capped = rows.len() >= CAMPAIGN_LIMIT;
        let mut campaigns: Vec<CampaignRow> = rows.iter().map(campaign_row).collect();
        let truncated = fit_items(&mut campaigns, ITEMS_BUDGET) || capped;
        to_json(GetCampaignsOutput {
            campaigns,
            truncated,
        })
    }
}

fn campaign_row(r: &Value) -> CampaignRow {
    CampaignRow {
        campaign_id: text_or_empty(r, "campaign.id"),
        name: text_or_empty(r, "campaign.name"),
        status: status_at(r, "campaign.status"),
        channel: text_or_empty(r, "campaign.advertisingChannelType"),
        budget: money_at(r, "campaignBudget.amountMicros"),
        bidding_strategy: text_or_empty(r, "campaign.biddingStrategyType"),
        impressions: int(r, "metrics.impressions"),
        clicks: int(r, "metrics.clicks"),
        cost: money_or_zero(r, "metrics.costMicros"),
        conversions: float(r, "metrics.conversions"),
        conversions_value: float(r, "metrics.conversionsValue"),
        ctr: float(r, "metrics.ctr"),
        average_cpc: money_or_zero(r, "metrics.averageCpc"),
    }
}

// ---------------------------------------------------- get_campaign_details

#[derive(Debug, Deserialize, JsonSchema)]
struct GetCampaignDetailsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CampaignInfo {
    campaign_id: String,
    name: String,
    status: String,
    channel: String,
    bidding_strategy: String,
    start_date: Option<String>,
    end_date: Option<String>,
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    budget: Option<Decimal>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdText {
    text: String,
    /// Pinned position (`HEADLINE_1`, ...), if any.
    pinned: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdOut {
    ad_id: String,
    ad_type: String,
    status: String,
    final_urls: Vec<String>,
    headlines: Vec<AdText>,
    descriptions: Vec<AdText>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct KeywordOut {
    criterion_id: String,
    text: String,
    match_type: String,
    status: String,
    /// Ad group negative keyword.
    negative: bool,
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdGroupOut {
    ad_group_id: String,
    name: String,
    status: String,
    ad_group_type: String,
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
    keywords: Vec<KeywordOut>,
    ads: Vec<AdOut>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct NegativeOut {
    criterion_id: String,
    text: String,
    match_type: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AssetOut {
    asset_id: String,
    name: Option<String>,
    asset_type: String,
    field_type: String,
    status: String,
    /// Text, for text assets.
    text: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AssetGroupOut {
    asset_group_id: String,
    name: String,
    status: String,
    /// Assets grouped by `field_type` (`HEADLINE`, `LONG_HEADLINE`, `MARKETING_IMAGE`, ...).
    assets: BTreeMap<String, Vec<AssetOut>>,
    /// Audience signals (`audience`, `searchTheme`), as the API returns them.
    signals: Vec<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LocationOut {
    geo_target_constant: String,
    negative: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LanguageOut {
    language_constant: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DeviceOut {
    device: String,
    negative: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ScheduleOut {
    day_of_week: String,
    start_hour: i64,
    start_minute: String,
    end_hour: i64,
    end_minute: String,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
struct TargetingOut {
    locations: Vec<LocationOut>,
    languages: Vec<LanguageOut>,
    devices: Vec<DeviceOut>,
    ad_schedule: Vec<ScheduleOut>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetCampaignDetailsOutput {
    campaign: CampaignInfo,
    /// Empty for Performance Max (which uses `asset_groups`).
    ad_groups: Vec<AdGroupOut>,
    /// Campaign negative keywords.
    negatives: Vec<NegativeOut>,
    /// Performance Max only.
    asset_groups: Vec<AssetGroupOut>,
    /// Assets linked to the campaign (sitelinks, callouts, ...).
    assets: Vec<AssetOut>,
    targeting: TargetingOut,
    /// The campaign has more items than the query or response size limit; use
    /// `gaql_search` with a cursor for the rest.
    truncated: bool,
}

/// LIMITs of each `get_campaign_details` query.
const LIMIT_AD_GROUPS: usize = 200;
const LIMIT_KEYWORDS: usize = 2000;
const LIMIT_ADS: usize = 1000;
const LIMIT_CRITERIA: usize = 1000;
const LIMIT_ASSETS: usize = 500;
const LIMIT_ASSET_GROUPS: usize = 100;
const LIMIT_ASSET_GROUP_ASSETS: usize = 1000;
const LIMIT_SIGNALS: usize = 500;
const PMAX: &str = "PERFORMANCE_MAX";

struct GetCampaignDetails;

#[async_trait]
impl ReadTool for GetCampaignDetails {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetCampaignDetailsInput>(
            CAMPAIGN_DETAILS_TOOL,
            "Gets the details of one campaign: settings, ad groups with keywords and ads, \
             negative keywords, asset groups (Performance Max), assets and targeting.",
            ToolGroup::Read,
        )
        .output::<GetCampaignDetailsOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: GetCampaignDetailsInput = parse_input(input)?;
        let id = valid_id(&input.campaign_id, "campaign_id")?;
        let tool = CAMPAIGN_DETAILS_TOOL;
        let campaign_rn = resources::rn(&ctx.customer_id, "campaigns", &id);

        let rows = search(
            ctx,
            tool,
            &format!(
                "SELECT campaign.id, campaign.name, campaign.status, \
                 campaign.advertising_channel_type, campaign.bidding_strategy_type, \
                 campaign.start_date_time, campaign.end_date_time, campaign_budget.amount_micros \
                 FROM campaign WHERE campaign.id = {id} LIMIT 1"
            ),
            1,
        )
        .await?;
        let Some(row) = rows.first() else {
            return Err(ToolError::new(
                codes::NOT_FOUND,
                format!("Campaign {id} not found in account {}.", ctx.customer_id),
            )
            .with_field("campaign_id"));
        };
        let campaign = CampaignInfo {
            campaign_id: text_or_empty(row, "campaign.id"),
            name: text_or_empty(row, "campaign.name"),
            status: status_at(row, "campaign.status"),
            channel: text_or_empty(row, "campaign.advertisingChannelType"),
            bidding_strategy: text_or_empty(row, "campaign.biddingStrategyType"),
            start_date: text(row, "campaign.startDateTime")
                .map(|d| fields::date_part(&d).to_string()),
            end_date: text(row, "campaign.endDateTime").map(|d| fields::date_part(&d).to_string()),
            budget: money_at(row, "campaignBudget.amountMicros"),
        };
        let pmax = campaign.channel == PMAX;

        let criteria_q = format!(
            "SELECT campaign.id, campaign_criterion.criterion_id, campaign_criterion.type, \
             campaign_criterion.negative, campaign_criterion.keyword.text, \
             campaign_criterion.keyword.match_type, \
             campaign_criterion.location.geo_target_constant, \
             campaign_criterion.language.language_constant, campaign_criterion.device.type, \
             campaign_criterion.ad_schedule.day_of_week, \
             campaign_criterion.ad_schedule.start_hour, \
             campaign_criterion.ad_schedule.start_minute, \
             campaign_criterion.ad_schedule.end_hour, \
             campaign_criterion.ad_schedule.end_minute FROM campaign_criterion \
             WHERE campaign.id = {id} AND campaign_criterion.status != 'REMOVED' \
             LIMIT {LIMIT_CRITERIA}"
        );
        let assets_q = format!(
            "SELECT campaign.id, campaign_asset.field_type, campaign_asset.status, asset.id, asset.name, \
             asset.type, asset.text_asset.text FROM campaign_asset \
             WHERE campaign.id = {id} AND campaign_asset.status != 'REMOVED' \
             LIMIT {LIMIT_ASSETS}"
        );

        let mut out = GetCampaignDetailsOutput {
            campaign,
            ad_groups: Vec::new(),
            negatives: Vec::new(),
            asset_groups: Vec::new(),
            assets: Vec::new(),
            targeting: TargetingOut::default(),
            truncated: false,
        };

        let (criteria, assets) = tokio::try_join!(
            search(ctx, tool, &criteria_q, LIMIT_CRITERIA),
            search(ctx, tool, &assets_q, LIMIT_ASSETS),
        )?;
        let mut capped = criteria.len() >= LIMIT_CRITERIA || assets.len() >= LIMIT_ASSETS;
        fill_criteria(&mut out, &criteria);
        out.assets = assets
            .iter()
            .map(|r| asset_out(r, "campaignAsset"))
            .collect();

        if pmax {
            let (groups, hit) = asset_groups(ctx, tool, &campaign_rn).await?;
            out.asset_groups = groups;
            capped |= hit;
        } else {
            let (groups, hit) = ad_groups(ctx, tool, &id).await?;
            out.ad_groups = groups;
            capped |= hit;
        }
        out.truncated = trim_details(&mut out) || capped;
        to_json(out)
    }
}

/// Pops the last item of `v` and returns the bytes it took in the JSON (comma included).
fn pop_bytes<T: Serialize>(v: &mut Vec<T>) -> Option<usize> {
    let item = v.pop()?;
    Some(serde_json::to_vec(&item).map(|b| b.len()).unwrap_or(0) + 1)
}

/// Cuts the larger items until the response fits in [`ITEMS_BUDGET`]: ads, then
/// keywords, asset group assets and signals, and finally campaign criteria and assets;
/// as a last resort, drops whole groups. The serialized output never exceeds
/// [`ITEMS_BUDGET`] (except for the campaign's fixed part). Returns `true` if it cut anything.
fn trim_details(out: &mut GetCampaignDetailsOutput) -> bool {
    let mut size = serde_json::to_vec(out).map(|b| b.len()).unwrap_or(0);
    if size <= ITEMS_BUDGET {
        return false;
    }
    macro_rules! shrink {
        ($next:expr) => {
            while size > ITEMS_BUDGET {
                match $next {
                    Some(n) => size = size.saturating_sub(n),
                    None => break,
                }
            }
        };
    }
    shrink!(
        out.ad_groups
            .iter_mut()
            .rev()
            .find_map(|g| pop_bytes(&mut g.ads))
    );
    shrink!(
        out.ad_groups
            .iter_mut()
            .rev()
            .find_map(|g| pop_bytes(&mut g.keywords))
    );
    shrink!(
        out.asset_groups
            .iter_mut()
            .rev()
            .find_map(|g| { g.assets.values_mut().rev().find_map(pop_bytes) })
    );
    shrink!(
        out.asset_groups
            .iter_mut()
            .rev()
            .find_map(|g| pop_bytes(&mut g.signals))
    );
    shrink!(pop_bytes(&mut out.assets));
    shrink!(pop_bytes(&mut out.negatives));
    shrink!(pop_bytes(&mut out.targeting.locations));
    shrink!(pop_bytes(&mut out.targeting.ad_schedule));
    shrink!(pop_bytes(&mut out.targeting.devices));
    shrink!(pop_bytes(&mut out.targeting.languages));
    for g in &mut out.asset_groups {
        g.assets.retain(|_, v| !v.is_empty());
    }
    // The sizes above are estimates: measure for real and, if still over, drop whole
    // groups from the end (asset groups, then ad groups).
    let measure =
        |o: &GetCampaignDetailsOutput| serde_json::to_vec(o).map(|b| b.len()).unwrap_or(0);
    while measure(out) > ITEMS_BUDGET {
        if out.asset_groups.pop().is_none() && out.ad_groups.pop().is_none() {
            break;
        }
    }
    true
}

fn fill_criteria(out: &mut GetCampaignDetailsOutput, rows: &[Value]) {
    for r in rows {
        let negative = flag(r, "campaignCriterion.negative");
        match text(r, "campaignCriterion.type").as_deref() {
            Some("KEYWORD") if negative => out.negatives.push(NegativeOut {
                criterion_id: text_or_empty(r, "campaignCriterion.criterionId"),
                text: text_or_empty(r, "campaignCriterion.keyword.text"),
                match_type: text_or_empty(r, "campaignCriterion.keyword.matchType"),
            }),
            Some("LOCATION") => out.targeting.locations.push(LocationOut {
                geo_target_constant: text_or_empty(
                    r,
                    "campaignCriterion.location.geoTargetConstant",
                ),
                negative,
            }),
            Some("LANGUAGE") => out.targeting.languages.push(LanguageOut {
                language_constant: text_or_empty(r, "campaignCriterion.language.languageConstant"),
            }),
            Some("DEVICE") => out.targeting.devices.push(DeviceOut {
                device: text_or_empty(r, "campaignCriterion.device.type"),
                negative,
            }),
            Some("AD_SCHEDULE") => out.targeting.ad_schedule.push(ScheduleOut {
                day_of_week: text_or_empty(r, "campaignCriterion.adSchedule.dayOfWeek"),
                start_hour: int(r, "campaignCriterion.adSchedule.startHour"),
                start_minute: text_or_empty(r, "campaignCriterion.adSchedule.startMinute"),
                end_hour: int(r, "campaignCriterion.adSchedule.endHour"),
                end_minute: text_or_empty(r, "campaignCriterion.adSchedule.endMinute"),
            }),
            _ => {}
        }
    }
}

/// `resource` is the row key of the link resource (`campaignAsset`, `assetGroupAsset`).
fn asset_out(r: &Value, resource: &str) -> AssetOut {
    AssetOut {
        asset_id: text_or_empty(r, "asset.id"),
        name: text(r, "asset.name"),
        asset_type: text_or_empty(r, "asset.type"),
        field_type: text_or_empty(r, &format!("{resource}.fieldType")),
        status: status_at(r, &format!("{resource}.status")),
        text: text(r, "asset.textAsset.text"),
    }
}

/// The boolean says whether any query hit its own LIMIT.
async fn ad_groups(ctx: &Ctx, tool: &str, id: &str) -> Result<(Vec<AdGroupOut>, bool), ToolError> {
    let groups_q = format!(
        "SELECT campaign.id, ad_group.id, ad_group.name, ad_group.status, ad_group.type, \
         ad_group.cpc_bid_micros FROM ad_group \
         WHERE campaign.id = {id} AND ad_group.status != 'REMOVED' \
         ORDER BY ad_group.id LIMIT {LIMIT_AD_GROUPS}"
    );
    let keywords_q = format!(
        "SELECT campaign.id, ad_group.id, ad_group_criterion.criterion_id, ad_group_criterion.status, \
         ad_group_criterion.negative, ad_group_criterion.keyword.text, \
         ad_group_criterion.keyword.match_type, ad_group_criterion.cpc_bid_micros \
         FROM ad_group_criterion WHERE campaign.id = {id} \
         AND ad_group_criterion.type = 'KEYWORD' AND ad_group_criterion.status != 'REMOVED' \
         ORDER BY ad_group.id LIMIT {LIMIT_KEYWORDS}"
    );
    let ads_q = format!(
        "SELECT campaign.id, ad_group.id, ad_group_ad.status, ad_group_ad.ad.id, ad_group_ad.ad.type, \
         ad_group_ad.ad.final_urls, ad_group_ad.ad.responsive_search_ad.headlines, \
         ad_group_ad.ad.responsive_search_ad.descriptions FROM ad_group_ad \
         WHERE campaign.id = {id} AND ad_group_ad.status != 'REMOVED' \
         ORDER BY ad_group.id LIMIT {LIMIT_ADS}"
    );
    let (groups, keywords, ads) = tokio::try_join!(
        search(ctx, tool, &groups_q, LIMIT_AD_GROUPS),
        search(ctx, tool, &keywords_q, LIMIT_KEYWORDS),
        search(ctx, tool, &ads_q, LIMIT_ADS),
    )?;
    let capped = groups.len() >= LIMIT_AD_GROUPS
        || keywords.len() >= LIMIT_KEYWORDS
        || ads.len() >= LIMIT_ADS;
    let mut out: Vec<AdGroupOut> = groups
        .iter()
        .map(|r| AdGroupOut {
            ad_group_id: text_or_empty(r, "adGroup.id"),
            name: text_or_empty(r, "adGroup.name"),
            status: status_at(r, "adGroup.status"),
            ad_group_type: text_or_empty(r, "adGroup.type"),
            cpc_bid: money_at(r, "adGroup.cpcBidMicros"),
            keywords: Vec::new(),
            ads: Vec::new(),
        })
        .collect();
    for r in &keywords {
        let gid = text_or_empty(r, "adGroup.id");
        if let Some(g) = out.iter_mut().find(|g| g.ad_group_id == gid) {
            g.keywords.push(KeywordOut {
                criterion_id: text_or_empty(r, "adGroupCriterion.criterionId"),
                text: text_or_empty(r, "adGroupCriterion.keyword.text"),
                match_type: text_or_empty(r, "adGroupCriterion.keyword.matchType"),
                status: status_at(r, "adGroupCriterion.status"),
                negative: flag(r, "adGroupCriterion.negative"),
                cpc_bid: money_at(r, "adGroupCriterion.cpcBidMicros"),
            });
        }
    }
    for r in &ads {
        let gid = text_or_empty(r, "adGroup.id");
        if let Some(g) = out.iter_mut().find(|g| g.ad_group_id == gid) {
            g.ads.push(AdOut {
                ad_id: text_or_empty(r, "adGroupAd.ad.id"),
                ad_type: text_or_empty(r, "adGroupAd.ad.type"),
                status: status_at(r, "adGroupAd.status"),
                final_urls: strings(r, "adGroupAd.ad.finalUrls"),
                headlines: ad_texts(r, "adGroupAd.ad.responsiveSearchAd.headlines"),
                descriptions: ad_texts(r, "adGroupAd.ad.responsiveSearchAd.descriptions"),
            });
        }
    }
    Ok((out, capped))
}

fn strings(r: &Value, path: &str) -> Vec<String> {
    at(r, path)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn ad_texts(r: &Value, path: &str) -> Vec<AdText> {
    at(r, path)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|t| AdText {
                    text: text_or_empty(t, "text"),
                    pinned: text(t, "pinnedField").filter(|p| p != "UNSPECIFIED"),
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn asset_groups(
    ctx: &Ctx,
    tool: &str,
    campaign_rn: &str,
) -> Result<(Vec<AssetGroupOut>, bool), ToolError> {
    let rn = gaql::quote(campaign_rn);
    let groups_q = format!(
        "SELECT asset_group.id, asset_group.name, asset_group.status FROM asset_group \
         WHERE asset_group.campaign = {rn} AND asset_group.status != 'REMOVED' \
         ORDER BY asset_group.id LIMIT {LIMIT_ASSET_GROUPS}"
    );
    let assets_q = format!(
        "SELECT asset_group.id, asset_group_asset.field_type, asset_group_asset.status, \
         asset.id, asset.name, asset.type, asset.text_asset.text FROM asset_group_asset \
         WHERE asset_group.campaign = {rn} AND asset_group_asset.status != 'REMOVED' \
         LIMIT {LIMIT_ASSET_GROUP_ASSETS}"
    );
    let signals_q = format!(
        "SELECT asset_group.id, asset_group_signal.audience.audience, \
         asset_group_signal.search_theme.text FROM asset_group_signal \
         WHERE asset_group.campaign = {rn} LIMIT {LIMIT_SIGNALS}"
    );
    let (groups, assets, signals) = tokio::try_join!(
        search(ctx, tool, &groups_q, LIMIT_ASSET_GROUPS),
        search(ctx, tool, &assets_q, LIMIT_ASSET_GROUP_ASSETS),
        search(ctx, tool, &signals_q, LIMIT_SIGNALS),
    )?;
    let capped = groups.len() >= LIMIT_ASSET_GROUPS
        || assets.len() >= LIMIT_ASSET_GROUP_ASSETS
        || signals.len() >= LIMIT_SIGNALS;
    let mut out: Vec<AssetGroupOut> = groups
        .iter()
        .map(|r| AssetGroupOut {
            asset_group_id: text_or_empty(r, "assetGroup.id"),
            name: text_or_empty(r, "assetGroup.name"),
            status: status_at(r, "assetGroup.status"),
            assets: BTreeMap::new(),
            signals: Vec::new(),
        })
        .collect();
    for r in &assets {
        let gid = text_or_empty(r, "assetGroup.id");
        if let Some(g) = out.iter_mut().find(|g| g.asset_group_id == gid) {
            let asset = asset_out(r, "assetGroupAsset");
            g.assets
                .entry(asset.field_type.clone())
                .or_default()
                .push(asset);
        }
    }
    for r in &signals {
        let gid = text_or_empty(r, "assetGroup.id");
        if let (Some(g), Some(signal)) = (
            out.iter_mut().find(|g| g.asset_group_id == gid),
            at(r, "assetGroupSignal"),
        ) {
            g.signals.push(signal.clone());
        }
    }
    Ok((out, capped))
}

// -------------------------------------------------------- get_search_terms

#[derive(Debug, Deserialize, JsonSchema)]
struct GetSearchTermsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    #[serde(default)]
    campaign_id: Option<String>,
    #[serde(default)]
    date_range: Option<DateRangeIn>,
    /// Default 100, maximum 1000.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchTermOut {
    search_term: String,
    /// Term status (`ADDED`, `EXCLUDED`, `NONE`, ...), as Google reports it.
    status: String,
    campaign_id: String,
    ad_group_id: String,
    impressions: i64,
    clicks: i64,
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    cost: Decimal,
    conversions: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetSearchTermsOutput {
    terms: Vec<SearchTermOut>,
    truncated: bool,
}

struct GetSearchTerms;

#[async_trait]
impl ReadTool for GetSearchTerms {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetSearchTermsInput>(
            SEARCH_TERMS_TOOL,
            "Lists the search terms that triggered the ads, sorted by impressions.",
            ToolGroup::Read,
        )
        .output::<GetSearchTermsOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: GetSearchTermsInput = parse_input(input)?;
        let limit = bounded(input.limit, 100, 1, 1000, "limit")?;
        let mut filters = vec![date_clause(&input.date_range)?];
        if let Some(c) = input.campaign_id.as_deref() {
            filters.push(format!("campaign.id = {}", valid_id(c, "campaign_id")?));
        }
        let query = format!(
            "SELECT search_term_view.search_term, search_term_view.status, campaign.id, \
             ad_group.id, metrics.impressions, metrics.clicks, metrics.cost_micros, \
             metrics.conversions FROM search_term_view WHERE {} \
             ORDER BY metrics.impressions DESC LIMIT {limit}",
            filters.join(" AND ")
        );
        let rows = search(ctx, SEARCH_TERMS_TOOL, &query, limit as usize).await?;
        let mut terms: Vec<SearchTermOut> = rows
            .iter()
            .map(|r| SearchTermOut {
                search_term: text_or_empty(r, "searchTermView.searchTerm"),
                status: text_or_empty(r, "searchTermView.status"),
                campaign_id: text_or_empty(r, "campaign.id"),
                ad_group_id: text_or_empty(r, "adGroup.id"),
                impressions: int(r, "metrics.impressions"),
                clicks: int(r, "metrics.clicks"),
                cost: money_or_zero(r, "metrics.costMicros"),
                conversions: float(r, "metrics.conversions"),
            })
            .collect();
        let truncated = fit_items(&mut terms, ITEMS_BUDGET);
        to_json(GetSearchTermsOutput { terms, truncated })
    }
}

// ------------------------------------------------------ get_recommendations

#[derive(Debug, Deserialize, JsonSchema)]
struct GetRecommendationsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Recommendation types (e.g. `KEYWORD`, `TARGET_CPA_OPT_IN`). Without a filter, all.
    #[serde(default)]
    types: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RecommendationOut {
    resource_name: String,
    #[serde(rename = "type")]
    kind: String,
    campaign_id: Option<String>,
    /// Estimated impact (`baseMetrics`/`potentialMetrics`), as the API returns it.
    impact: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetRecommendationsOutput {
    recommendations: Vec<RecommendationOut>,
    truncated: bool,
}

const LIMIT_RECOMMENDATIONS: usize = 500;

struct GetRecommendations;

#[async_trait]
impl ReadTool for GetRecommendations {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetRecommendationsInput>(
            RECOMMENDATIONS_TOOL,
            "Lists Google's optimization recommendations for the account.",
            ToolGroup::Read,
        )
        .output::<GetRecommendationsOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: GetRecommendationsInput = parse_input(input)?;
        let filter = match input.types.as_deref() {
            None | Some([]) => String::new(),
            Some(types) => {
                let items: Vec<String> = types.iter().map(|t| gaql::quote(t)).collect();
                format!(" WHERE recommendation.type IN ({})", items.join(", "))
            }
        };
        let query = format!(
            "SELECT recommendation.resource_name, recommendation.type, \
             recommendation.campaign, recommendation.impact FROM recommendation{filter} \
             LIMIT {LIMIT_RECOMMENDATIONS}"
        );
        let rows = search(ctx, RECOMMENDATIONS_TOOL, &query, LIMIT_RECOMMENDATIONS).await?;
        let capped = rows.len() >= LIMIT_RECOMMENDATIONS;
        let mut recommendations: Vec<RecommendationOut> = rows
            .iter()
            .map(|r| RecommendationOut {
                resource_name: text_or_empty(r, "recommendation.resourceName"),
                kind: text_or_empty(r, "recommendation.type"),
                campaign_id: last_id(text(r, "recommendation.campaign")),
                impact: at(r, "recommendation.impact")
                    .cloned()
                    .unwrap_or(Value::Null),
            })
            .collect();
        let truncated = fit_items(&mut recommendations, ITEMS_BUDGET) || capped;
        to_json(GetRecommendationsOutput {
            recommendations,
            truncated,
        })
    }
}

// --------------------------------------------------- get_conversion_actions

#[derive(Debug, Deserialize, JsonSchema)]
struct GetConversionActionsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ConversionActionOut {
    id: String,
    name: String,
    category: String,
    status: String,
    #[serde(rename = "type")]
    kind: String,
    primary_for_goal: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetConversionActionsOutput {
    conversion_actions: Vec<ConversionActionOut>,
    truncated: bool,
}

const LIMIT_CONVERSION_ACTIONS: usize = 500;

struct GetConversionActions;

#[async_trait]
impl ReadTool for GetConversionActions {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetConversionActionsInput>(
            CONVERSION_ACTIONS_TOOL,
            "Lists the account's conversion actions.",
            ToolGroup::Read,
        )
        .output::<GetConversionActionsOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let _: GetConversionActionsInput = parse_input(input)?;
        let query = format!(
            "SELECT conversion_action.id, conversion_action.name, conversion_action.category, \
             conversion_action.status, conversion_action.type, \
             conversion_action.primary_for_goal FROM conversion_action \
             LIMIT {LIMIT_CONVERSION_ACTIONS}"
        );
        let rows = search(
            ctx,
            CONVERSION_ACTIONS_TOOL,
            &query,
            LIMIT_CONVERSION_ACTIONS,
        )
        .await?;
        let capped = rows.len() >= LIMIT_CONVERSION_ACTIONS;
        let mut conversion_actions: Vec<ConversionActionOut> = rows
            .iter()
            .map(|r| ConversionActionOut {
                id: text_or_empty(r, "conversionAction.id"),
                name: text_or_empty(r, "conversionAction.name"),
                category: text_or_empty(r, "conversionAction.category"),
                status: status_at(r, "conversionAction.status"),
                kind: text_or_empty(r, "conversionAction.type"),
                primary_for_goal: flag(r, "conversionAction.primaryForGoal"),
            })
            .collect();
        let truncated = fit_items(&mut conversion_actions, ITEMS_BUDGET) || capped;
        to_json(GetConversionActionsOutput {
            conversion_actions,
            truncated,
        })
    }
}

// ------------------------------------------------------ get_change_history

#[derive(Debug, Deserialize, JsonSchema)]
struct GetChangeHistoryInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// How many days back (default 7, 1 to 30).
    #[serde(default)]
    days: Option<u32>,
    /// Default 100, maximum 1000.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ChangeEventOut {
    date_time: String,
    user_email: Option<String>,
    client_type: String,
    resource_type: String,
    resource_name: String,
    operation: String,
    /// Changed fields, comma-separated.
    changed_fields: Option<String>,
    campaign_id: Option<String>,
    ad_group_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetChangeHistoryOutput {
    events: Vec<ChangeEventOut>,
    truncated: bool,
}

struct GetChangeHistory;

#[async_trait]
impl ReadTool for GetChangeHistory {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetChangeHistoryInput>(
            CHANGE_HISTORY_TOOL,
            "Lists recent changes made to the account (change_event), newest first.",
            ToolGroup::Read,
        )
        .output::<GetChangeHistoryOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: GetChangeHistoryInput = parse_input(input)?;
        let days = bounded(input.days, 7, 1, 30, "days")?;
        let limit = bounded(input.limit, 100, 1, 1000, "limit")?;
        // The API requires a bounded change_date_time range and keeps only 30 days: the
        // lower bound sits one day inside the window (the account timezone may run ahead).
        let now = Utc::now();
        let window = Duration::days(i64::from(days)).min(Duration::days(29));
        let fmt = "%Y-%m-%d %H:%M:%S";
        let since = (now - window).format(fmt).to_string();
        let until = now.format(fmt).to_string();
        let query = format!(
            "SELECT change_event.change_date_time, change_event.user_email, \
             change_event.client_type, change_event.change_resource_type, \
             change_event.change_resource_name, change_event.resource_change_operation, \
             change_event.changed_fields, change_event.campaign, change_event.ad_group \
             FROM change_event WHERE change_event.change_date_time >= {} \
             AND change_event.change_date_time <= {} \
             ORDER BY change_event.change_date_time DESC LIMIT {limit}",
            gaql::quote(&since),
            gaql::quote(&until)
        );
        let rows = search(ctx, CHANGE_HISTORY_TOOL, &query, limit as usize).await?;
        let mut events: Vec<ChangeEventOut> = rows
            .iter()
            .map(|r| ChangeEventOut {
                date_time: text_or_empty(r, "changeEvent.changeDateTime"),
                user_email: text(r, "changeEvent.userEmail"),
                client_type: text_or_empty(r, "changeEvent.clientType"),
                resource_type: text_or_empty(r, "changeEvent.changeResourceType"),
                resource_name: text_or_empty(r, "changeEvent.changeResourceName"),
                operation: text_or_empty(r, "changeEvent.resourceChangeOperation"),
                changed_fields: text(r, "changeEvent.changedFields"),
                campaign_id: last_id(text(r, "changeEvent.campaign")),
                ad_group_id: last_id(text(r, "changeEvent.adGroup")),
            })
            .collect();
        let truncated = fit_items(&mut events, ITEMS_BUDGET);
        to_json(GetChangeHistoryOutput { events, truncated })
    }
}
