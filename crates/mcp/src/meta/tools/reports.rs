//! Campaign and insights reads: `get_campaigns`, `get_campaign_details`, `get_insights`.

use adsconsole_core::Status;
use adsconsole_core::config::ToolGroup;
use adsconsole_meta_ads::client::{Page, PageCursor};
use adsconsole_meta_ads::dates::{Period, insights_param};
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::fields::{ObjectKind, find, normalize};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{MTool, ReadTool, decode_cursor, encode_cursor, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::errors::translate;
use crate::truncate::{ITEMS_BUDGET, fit};

pub(crate) fn tools() -> Vec<MTool> {
    vec![
        MTool::Read(Box::new(GetCampaigns)),
        MTool::Read(Box::new(GetCampaignDetails)),
        MTool::Read(Box::new(GetInsights)),
    ]
}

const METRICS: &str = "spend,impressions,reach,clicks,ctr,cpc,cpm,actions";
const EXTRA_METRICS: [&str; 5] = [
    "frequency",
    "unique_clicks",
    "cost_per_action_type",
    "action_values",
    "purchase_roas",
];
const BREAKDOWNS: [&str; 8] = [
    "age",
    "gender",
    "country",
    "region",
    "publisher_platform",
    "platform_position",
    "device_platform",
    "impression_device",
];
const TIME_INCREMENTS: [&str; 4] = ["1", "7", "monthly", "all_days"];
const MAX_LIMIT: usize = 500;

fn invalid(field: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

fn check_period(p: &Period) -> Result<(), ToolError> {
    p.check().map_err(|e| invalid("period", e))
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Action type that counts as a "result" for each objective.
fn result_action(objective: Option<&str>) -> Option<&'static str> {
    match objective? {
        "OUTCOME_TRAFFIC" | "LINK_CLICKS" => Some("link_click"),
        "OUTCOME_LEADS" | "LEAD_GENERATION" => Some("lead"),
        "OUTCOME_SALES" | "CONVERSIONS" => Some("purchase"),
        "OUTCOME_ENGAGEMENT" | "POST_ENGAGEMENT" => Some("post_engagement"),
        "OUTCOME_APP_PROMOTION" | "APP_INSTALLS" => Some("mobile_app_install"),
        _ => None,
    }
}

/// Normalized metrics of an insights row. `extra` are additionally requested fields.
pub(crate) fn metrics(row: &Value, objective: Option<&str>, extra: &[String]) -> Value {
    let actions: Vec<Value> = row["actions"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|a| json!({ "type": a["action_type"], "value": num(&a["value"]) }))
                .collect()
        })
        .unwrap_or_default();
    let results = result_action(objective).map(|t| {
        row["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| {
                a["action_type"]
                    .as_str()
                    .is_some_and(|x| x == t || x.ends_with(&format!(".{t}")))
            })
            .filter_map(|a| num(&a["value"]))
            .sum::<f64>()
    });
    let mut out = json!({
        "spend": num(&row["spend"]),
        "impressions": int(&row["impressions"]),
        "reach": int(&row["reach"]),
        "clicks": int(&row["clicks"]),
        "ctr": num(&row["ctr"]),
        "cpc": num(&row["cpc"]),
        "cpm": num(&row["cpm"]),
        "results": results,
        "actions": actions,
    });
    for f in extra {
        out[f.as_str()] = match &row[f.as_str()] {
            Value::String(s) => s.parse::<f64>().map(|n| json!(n)).unwrap_or(json!(s)),
            other => other.clone(),
        };
    }
    out
}

/// Budget (Meta minor units) in the account currency.
fn money(ctx: &Ctx, kind: ObjectKind, name: &str, obj: &Value) -> Value {
    find(name, kind).map_or(Value::Null, |f| normalize(f, obj, ctx.exponent))
}

/// Page of `path` starting at the input cursor, cut by size; returns the kept items and
/// the next cursor.
async fn page(
    ctx: &Ctx,
    tool: &str,
    path: &str,
    query: &[(&str, String)],
    cursor: Option<&str>,
    limit: usize,
    shape: impl Fn(&Value) -> Value,
) -> Result<(Vec<Value>, bool, Option<String>), ToolError> {
    let start = match cursor {
        Some(c) => decode_cursor(c)?,
        None => PageCursor {
            after: None,
            skip: 0,
        },
    };
    let Page { data, starts, next } = ctx
        .client
        .get_from(path, query, &start, limit)
        .await
        .map_err(|e| translate(e, Some(ctx), tool))?;
    let shaped: Vec<Value> = data.iter().map(&shape).collect();
    let (kept, cut) = fit(shaped, ITEMS_BUDGET);
    // Cut by size: continue at the first item left out.
    let next = if cut {
        starts.get(kept.len()).cloned()
    } else {
        next
    };
    Ok((kept, next.is_some(), next.as_ref().map(encode_cursor)))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetCampaignsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Metrics period (default last_30d).
    #[serde(default)]
    period: Option<Period>,
    /// Only campaigns with these effective statuses.
    #[serde(default)]
    status: Vec<Status>,
    /// Maximum number of campaigns (default 50, up to 500).
    #[serde(default)]
    limit: Option<usize>,
    /// `next_cursor` from a previous response.
    #[serde(default)]
    cursor: Option<String>,
}

struct GetCampaigns;

#[async_trait::async_trait]
impl ReadTool for GetCampaigns {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetCampaignsInput>(
            "get_campaigns",
            "Lists the account's campaigns with budget, objective and period metrics \
             (spend, impressions, clicks, results).",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: GetCampaignsInput = parse_input(input)?;
        let period = i.period.unwrap_or_default();
        check_period(&period)?;
        let (k, v) = insights_param(&period);
        let fields = format!(
            "id,name,status,effective_status,objective,daily_budget,lifetime_budget,bid_strategy,\
             insights.{k}({v}){{{METRICS}}}"
        );
        let mut query = vec![("fields", fields), ("limit", "100".to_string())];
        if !i.status.is_empty() {
            let list: Vec<&str> = i.status.iter().map(|s| s.to_meta()).collect();
            query.push((
                "effective_status",
                serde_json::to_string(&list).unwrap_or_default(),
            ));
        }
        let limit = i.limit.unwrap_or(50).clamp(1, MAX_LIMIT);
        let (campaigns, truncated, next) = page(
            ctx,
            "get_campaigns",
            &format!("{}/campaigns", ctx.account_id),
            &query,
            i.cursor.as_deref(),
            limit,
            |c| {
                let objective = c["objective"].as_str();
                json!({
                    "campaign_id": c["id"],
                    "name": c["name"],
                    "status": c["status"],
                    "effective_status": c["effective_status"],
                    "objective": c["objective"],
                    "daily_budget": money(ctx, ObjectKind::Campaign, "daily_budget", c),
                    "lifetime_budget": money(ctx, ObjectKind::Campaign, "lifetime_budget", c),
                    "bid_strategy": c["bid_strategy"],
                    "metrics": c["insights"]["data"].get(0).map(|row| metrics(row, objective, &[])),
                })
            },
        )
        .await?;
        Ok(json!({ "campaigns": campaigns, "truncated": truncated, "next_cursor": next }))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DetailsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign_id: String,
}

struct GetCampaignDetails;

const ADSET_FIELDS: &str = "id,name,status,effective_status,daily_budget,lifetime_budget,\
    bid_strategy,bid_amount,optimization_goal,billing_event,targeting,start_time,end_time,promoted_object";
const AD_FIELDS: &str = "id,name,status,effective_status,adset_id,\
    creative{id,name,object_story_spec,asset_feed_spec,thumbnail_url}";

/// Does the object belong to the call's account? Meta returns `account_id` without `act_`.
pub(crate) fn of_account(ctx: &Ctx, obj: &Value) -> bool {
    let id = match &obj["account_id"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return false,
    };
    format!("act_{}", id.trim_start_matches("act_")) == ctx.account_id
}

pub(crate) fn not_found(what: &str, id: &str) -> ToolError {
    ToolError::new(
        codes::NOT_FOUND,
        format!("{what} {id} not found in this account."),
    )
}

/// GET of an object; a missing one (code 100, subcode 33) becomes `NOT_FOUND`.
pub(crate) async fn get_object(
    ctx: &Ctx,
    tool: &str,
    what: &str,
    id: &str,
    fields: &str,
) -> Result<Value, ToolError> {
    let obj = ctx
        .client
        .get(id, &[("fields", fields.to_string())])
        .await
        .map_err(|e| match e {
            MetaError::Api {
                code: 100,
                subcode: Some(33),
                ..
            } => not_found(what, id),
            other => translate(other, Some(ctx), tool),
        })?;
    if !of_account(ctx, &obj) {
        return Err(not_found(what, id));
    }
    Ok(obj)
}

#[async_trait::async_trait]
impl ReadTool for GetCampaignDetails {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<DetailsInput>(
            "get_campaign_details",
            "Details a campaign: ad sets (budget, bid, audience, schedule) and ads with \
             their creatives.",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: DetailsInput = parse_input(input)?;
        let tool = "get_campaign_details";
        let c = get_object(
            ctx,
            tool,
            "Campaign",
            &i.campaign_id,
            "id,name,account_id,status,effective_status,objective,daily_budget,lifetime_budget,\
             bid_strategy,special_ad_categories,start_time,stop_time",
        )
        .await?;
        let adsets = ctx
            .client
            .get_all(
                &format!("{}/adsets", i.campaign_id),
                &[("fields", ADSET_FIELDS.into()), ("limit", "100".into())],
                500,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
        let ads = ctx
            .client
            .get_all(
                &format!("{}/ads", i.campaign_id),
                &[("fields", AD_FIELDS.into()), ("limit", "100".into())],
                1000,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
        let campaign = json!({
            "campaign_id": c["id"],
            "name": c["name"],
            "status": c["status"],
            "effective_status": c["effective_status"],
            "objective": c["objective"],
            "daily_budget": money(ctx, ObjectKind::Campaign, "daily_budget", &c),
            "lifetime_budget": money(ctx, ObjectKind::Campaign, "lifetime_budget", &c),
            "bid_strategy": c["bid_strategy"],
            "special_ad_categories": c["special_ad_categories"],
            "start_time": c["start_time"],
            "stop_time": c["stop_time"],
        });
        let adsets: Vec<Value> = adsets
            .data
            .iter()
            .map(|s| {
                let mut out = Map::new();
                for k in [
                    "id",
                    "name",
                    "status",
                    "effective_status",
                    "bid_strategy",
                    "optimization_goal",
                    "billing_event",
                    "targeting",
                    "start_time",
                    "end_time",
                    "promoted_object",
                ] {
                    out.insert(
                        if k == "id" {
                            "adset_id".into()
                        } else {
                            k.into()
                        },
                        s[k].clone(),
                    );
                }
                for k in ["daily_budget", "lifetime_budget", "bid_amount"] {
                    out.insert(k.into(), money(ctx, ObjectKind::AdSet, k, s));
                }
                Value::Object(out)
            })
            .collect();
        let ads: Vec<Value> = ads
            .data
            .iter()
            .map(|a| {
                json!({
                    "ad_id": a["id"], "name": a["name"], "status": a["status"],
                    "effective_status": a["effective_status"], "adset_id": a["adset_id"],
                    "creative": a["creative"],
                })
            })
            .collect();
        // Cut the ads first; if it still does not fit, the ad sets.
        let size = |v: &Vec<Value>| serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0);
        let room_for_sets = ITEMS_BUDGET.saturating_sub(campaign.to_string().len());
        let (adsets, cut_sets) = if size(&adsets) > room_for_sets {
            fit(adsets, room_for_sets)
        } else {
            (adsets, false)
        };
        let room_for_ads = room_for_sets.saturating_sub(size(&adsets));
        let (ads, cut_ads) = match ads.len() {
            0 => (ads, false),
            _ if room_for_ads < 1_000 => (Vec::new(), true),
            _ => fit(ads, room_for_ads),
        };
        let truncated = cut_sets || cut_ads;
        Ok(json!({ "campaign": campaign, "adsets": adsets, "ads": ads, "truncated": truncated }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Level {
    Account,
    Campaign,
    Adset,
    Ad,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Account => "account",
            Level::Campaign => "campaign",
            Level::Adset => "adset",
            Level::Ad => "ad",
        }
    }

    /// Ids and names that come with each row.
    fn id_fields(self) -> &'static str {
        match self {
            Level::Account => "account_id,account_name",
            Level::Campaign => "campaign_id,campaign_name,objective",
            Level::Adset => "campaign_id,campaign_name,adset_id,adset_name,objective",
            Level::Ad => "campaign_id,adset_id,ad_id,ad_name,objective",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InsightsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Row level: account, campaign, adset or ad.
    level: Level,
    /// Campaign, ad set or ad; without it, the whole account.
    #[serde(default)]
    object_id: Option<String>,
    /// Period (default last_30d).
    #[serde(default)]
    period: Option<Period>,
    /// age, gender, country, region, publisher_platform, platform_position,
    /// device_platform, impression_device.
    #[serde(default)]
    breakdowns: Vec<String>,
    /// "1" (daily), "7", "monthly" or "all_days".
    #[serde(default)]
    time_increment: Option<String>,
    /// Extra metrics: frequency, unique_clicks, cost_per_action_type, action_values, purchase_roas.
    #[serde(default)]
    fields: Vec<String>,
    /// Maximum number of rows (default 100, up to 500).
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

struct GetInsights;

#[async_trait::async_trait]
impl ReadTool for GetInsights {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<InsightsInput>(
            "get_insights",
            "Metrics of the account, a campaign, an ad set or an ad, by level, period, \
             breakdowns (age, gender, placement...) and time increment.",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: InsightsInput = parse_input(input)?;
        let tool = "get_insights";
        let period = i.period.unwrap_or_default();
        check_period(&period)?;
        if let Some(b) = i
            .breakdowns
            .iter()
            .find(|b| !BREAKDOWNS.contains(&b.as_str()))
        {
            return Err(invalid(
                "breakdowns",
                format!("Unknown breakdown: {b}. Use: {}.", BREAKDOWNS.join(", ")),
            ));
        }
        if let Some(f) = i
            .fields
            .iter()
            .find(|f| !EXTRA_METRICS.contains(&f.as_str()))
        {
            return Err(invalid(
                "fields",
                format!(
                    "Unsupported metric: {f}. Use: {}.",
                    EXTRA_METRICS.join(", ")
                ),
            ));
        }
        if let Some(t) = &i.time_increment
            && !TIME_INCREMENTS.contains(&t.as_str())
        {
            return Err(invalid(
                "time_increment",
                "Use \"1\", \"7\", \"monthly\" or \"all_days\".",
            ));
        }
        let target = match i.object_id.as_deref().filter(|o| *o != ctx.account_id) {
            Some(id) => {
                get_object(ctx, tool, "Object", id, "id,account_id").await?;
                id.to_string()
            }
            None => ctx.account_id.clone(),
        };
        let mut fields = format!("{},date_start,date_stop,{METRICS}", i.level.id_fields());
        for f in &i.fields {
            fields.push(',');
            fields.push_str(f);
        }
        let (k, v) = insights_param(&period);
        let mut query = vec![
            ("level", i.level.as_str().to_string()),
            ("fields", fields),
            (k, v),
            ("limit", "100".to_string()),
        ];
        if !i.breakdowns.is_empty() {
            query.push(("breakdowns", i.breakdowns.join(",")));
        }
        if let Some(t) = &i.time_increment {
            query.push(("time_increment", t.clone()));
        }
        let limit = i.limit.unwrap_or(100).clamp(1, MAX_LIMIT);
        let extra = i.fields.clone();
        let breakdowns = i.breakdowns.clone();
        let (rows, truncated, next) = page(
            ctx,
            tool,
            &format!("{target}/insights"),
            &query,
            i.cursor.as_deref(),
            limit,
            |row| {
                let mut out = Map::new();
                for k in i
                    .level
                    .id_fields()
                    .split(',')
                    .chain(["date_start", "date_stop"])
                {
                    if !row[k].is_null() {
                        out.insert(k.into(), row[k].clone());
                    }
                }
                for b in &breakdowns {
                    out.insert(b.clone(), row[b.as_str()].clone());
                }
                out.insert(
                    "metrics".into(),
                    metrics(row, row["objective"].as_str(), &extra),
                );
                Value::Object(out)
            },
        )
        .await?;
        Ok(json!({ "rows": rows, "truncated": truncated, "next_cursor": next }))
    }
}
