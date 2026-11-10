//! Creation: `create_campaign` (campaign + ad sets + creatives + ads, all paused) and
//! `create_ad` (in an existing ad set). A failure midway undoes what was created.

use adsconsole_core::Decimal;
use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::to_minor_units;
use adsconsole_meta_ads::fields::ObjectKind;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::creative::{CreativeInput, creative_params};
use super::reports::get_object;
use super::{MTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::plan::{InverseSpec, MetaPlan, Owner, REF_PREFIX, Step, Watch};

pub(crate) fn tools() -> Vec<MTool> {
    vec![
        MTool::Write(Box::new(CreateCampaign)),
        MTool::Write(Box::new(CreateAd)),
    ]
}

const MAX_ADSETS: usize = 10;
const DEFAULT_BID_STRATEGY: &str = "LOWEST_COST_WITHOUT_CAP";
const MAX_ADS: usize = 10;

fn invalid(field: impl Into<String>, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

fn minor(ctx: &Ctx, field: &str, amount: Decimal) -> Result<Value, ToolError> {
    to_minor_units(amount, ctx.exponent)
        .map(|u| json!(u.to_string()))
        .map_err(|e| invalid(field, format!("{field}: {e}")))
}

fn r(key: &str) -> String {
    format!("{REF_PREFIX}{key}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
enum Objective {
    #[serde(rename = "OUTCOME_AWARENESS")]
    Awareness,
    #[serde(rename = "OUTCOME_TRAFFIC")]
    Traffic,
    #[serde(rename = "OUTCOME_ENGAGEMENT")]
    Engagement,
    #[serde(rename = "OUTCOME_LEADS")]
    Leads,
    #[serde(rename = "OUTCOME_APP_PROMOTION")]
    AppPromotion,
    #[serde(rename = "OUTCOME_SALES")]
    Sales,
}

impl Objective {
    fn as_str(self) -> &'static str {
        match self {
            Objective::Awareness => "OUTCOME_AWARENESS",
            Objective::Traffic => "OUTCOME_TRAFFIC",
            Objective::Engagement => "OUTCOME_ENGAGEMENT",
            Objective::Leads => "OUTCOME_LEADS",
            Objective::AppPromotion => "OUTCOME_APP_PROMOTION",
            Objective::Sales => "OUTCOME_SALES",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CampaignIn {
    name: String,
    objective: Objective,
    /// Special ad categories (CREDIT, EMPLOYMENT, HOUSING...). Default: none.
    #[serde(default)]
    special_ad_categories: Vec<String>,
    /// Campaign (daily) budget, in the account currency. Set it either on the campaign or on each ad set.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    lifetime_budget: Option<Decimal>,
    #[serde(default)]
    bid_strategy: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdIn {
    name: String,
    creative: CreativeInput,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdsetIn {
    name: String,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    daily_budget: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    lifetime_budget: Option<Decimal>,
    /// LINK_CLICKS, LANDING_PAGE_VIEWS, REACH, IMPRESSIONS, OFFSITE_CONVERSIONS, LEAD_GENERATION...
    optimization_goal: String,
    /// Default IMPRESSIONS.
    #[serde(default)]
    billing_event: Option<String>,
    #[serde(default)]
    bid_strategy: Option<String>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    bid_amount: Option<Decimal>,
    /// targeting_spec (geo_locations required).
    targeting: Value,
    /// ISO 8601.
    #[serde(default)]
    start_time: Option<String>,
    /// Required with lifetime_budget.
    #[serde(default)]
    end_time: Option<String>,
    /// Pixel/event or Page, for sales and leads.
    #[serde(default)]
    promoted_object: Option<Value>,
    #[serde(default)]
    destination_type: Option<String>,
    /// 1 to 10 ads.
    ads: Vec<AdIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateCampaignInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign: CampaignIn,
    /// 1 to 10 ad sets.
    adsets: Vec<AdsetIn>,
}

/// targeting with geo and `targeting_automation.advantage_audience` (0 when not given).
fn targeting(field: &str, t: &Value) -> Result<Value, ToolError> {
    if !t["geo_locations"].is_object() {
        return Err(invalid(
            field,
            "Provide at least one location in targeting.geo_locations.",
        ));
    }
    let mut t = t.clone();
    // VERIFY(v26.0): Meta requires an explicit targeting_automation.advantage_audience.
    if t["targeting_automation"]["advantage_audience"].is_null() {
        let auto = t
            .as_object_mut()
            .and_then(|m| {
                m.entry("targeting_automation")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
            })
            .ok_or_else(|| invalid(field, "Invalid targeting."))?;
        auto.insert("advantage_audience".into(), json!(0));
    }
    Ok(t)
}

fn object_name(c: &CreativeInput, ad: &str) -> String {
    let kind = match c {
        CreativeInput::Image { .. } => "image",
        CreativeInput::Video { .. } => "video",
        CreativeInput::Carousel { .. } => "carousel",
        CreativeInput::ExistingPost { .. } => "post",
    };
    format!("{ad} ({kind})")
}

/// Steps of a creative and of the ad that uses it.
fn ad_steps(
    ctx: &Ctx,
    at: &str,
    keys: (&str, &str),
    adset: &str,
    ad_owner: Owner,
    name: &str,
    creative: Creative<'_>,
) -> Result<Vec<Step>, ToolError> {
    let (creative_key, ad_key) = keys;
    let mut steps = Vec::new();
    let creative_ref = match creative {
        Creative::New(c) => {
            let mut p = creative_params(c, &format!("{at}.creative"))?;
            p.insert("name".into(), json!(object_name(c, name)));
            steps.push(Step::Create {
                key: creative_key.to_string(),
                path: format!("{}/adcreatives", ctx.account_id),
                params: p,
                owner: Owner::Keep,
            });
            r(creative_key)
        }
        Creative::Existing(id) => id.to_string(),
    };
    steps.push(Step::Create {
        key: ad_key.to_string(),
        path: format!("{}/ads", ctx.account_id),
        params: params(json!({
            "name": name,
            "adset_id": adset,
            "creative": { "creative_id": creative_ref },
            "status": "PAUSED",
        })),
        owner: ad_owner,
    });
    Ok(steps)
}

enum Creative<'a> {
    New(&'a CreativeInput),
    Existing(&'a str),
}

fn params(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn watch(key: &str, kind: ObjectKind) -> Watch {
    Watch {
        object_id: r(key),
        kind,
        fields: vec!["name", "status"],
    }
}

struct CreateCampaign;

#[async_trait::async_trait]
impl WriteTool for CreateCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateCampaignInput>(
            "create_campaign",
            "Creates a full campaign, paused: campaign, ad sets (audience, budget, schedule), \
             creatives (image, video or carousel) and ads. If any step fails, what was \
             already created is deleted.",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: CreateCampaignInput = parse_input(input)?;
        let c = &i.campaign;
        if c.name.trim().is_empty() {
            return Err(invalid("campaign.name", "Give the campaign a name."));
        }
        if i.adsets.is_empty() || i.adsets.len() > MAX_ADSETS {
            return Err(invalid(
                "adsets",
                format!("Provide 1 to {MAX_ADSETS} ad sets."),
            ));
        }
        if c.daily_budget.is_some() && c.lifetime_budget.is_some() {
            return Err(invalid(
                "campaign.daily_budget",
                "Use daily_budget or lifetime_budget, not both.",
            ));
        }
        let cbo = c.daily_budget.is_some() || c.lifetime_budget.is_some();

        let mut campaign = params(json!({
            "name": c.name.trim(),
            "objective": c.objective.as_str(),
            "status": "PAUSED",
            "special_ad_categories": c.special_ad_categories,
        }));
        if let Some(b) = c.daily_budget {
            campaign.insert(
                "daily_budget".into(),
                minor(ctx, "campaign.daily_budget", b)?,
            );
        }
        if let Some(b) = c.lifetime_budget {
            campaign.insert(
                "lifetime_budget".into(),
                minor(ctx, "campaign.lifetime_budget", b)?,
            );
        }
        if !cbo {
            // VERIFY(v26.0): without a campaign budget, Meta requires this field explicitly.
            campaign.insert("is_adset_budget_sharing_enabled".into(), json!(false));
        }
        // Without a strategy, Meta now asks for a bid amount; the default is no cap.
        match (&c.bid_strategy, cbo) {
            (Some(b), _) => {
                campaign.insert("bid_strategy".into(), json!(b));
            }
            (None, true) => {
                campaign.insert("bid_strategy".into(), json!(DEFAULT_BID_STRATEGY));
            }
            (None, false) => {}
        }
        let mut steps = vec![Step::Create {
            key: "campaign".into(),
            path: format!("{}/campaigns", ctx.account_id),
            params: campaign,
            owner: Owner::Root,
        }];
        let mut watches = vec![watch("campaign", ObjectKind::Campaign)];
        let mut ads_total = 0;
        let mut promoted = true;

        for (n, s) in i.adsets.iter().enumerate() {
            let at = format!("adsets[{n}]");
            let key = format!("adset_{}", n + 1);
            if s.ads.is_empty() || s.ads.len() > MAX_ADS {
                return Err(invalid(
                    format!("{at}.ads"),
                    format!("Provide 1 to {MAX_ADS} ads per ad set."),
                ));
            }
            let has_budget = s.daily_budget.is_some() || s.lifetime_budget.is_some();
            if cbo && has_budget {
                return Err(invalid(
                    format!("{at}.daily_budget"),
                    "With a campaign budget, the ad sets do not take their own budget.",
                ));
            }
            if !cbo && !has_budget {
                return Err(invalid(
                    format!("{at}.daily_budget"),
                    "Provide the budget on the campaign or on each ad set.",
                ));
            }
            if s.daily_budget.is_some() && s.lifetime_budget.is_some() {
                return Err(invalid(
                    format!("{at}.daily_budget"),
                    "Use daily_budget or lifetime_budget, not both.",
                ));
            }
            let lifetime = s.lifetime_budget.is_some() || c.lifetime_budget.is_some();
            if lifetime && s.end_time.is_none() {
                return Err(invalid(
                    format!("{at}.end_time"),
                    "A lifetime budget requires end_time on the ad set.",
                ));
            }
            let mut p = params(json!({
                "name": s.name,
                "campaign_id": r("campaign"),
                "status": "PAUSED",
                "optimization_goal": s.optimization_goal,
                "billing_event": s.billing_event.clone().unwrap_or_else(|| "IMPRESSIONS".into()),
                "targeting": targeting(&format!("{at}.targeting"), &s.targeting)?,
            }));
            if let Some(b) = s.daily_budget {
                p.insert(
                    "daily_budget".into(),
                    minor(ctx, &format!("{at}.daily_budget"), b)?,
                );
            }
            if let Some(b) = s.lifetime_budget {
                p.insert(
                    "lifetime_budget".into(),
                    minor(ctx, &format!("{at}.lifetime_budget"), b)?,
                );
            }
            if let Some(b) = s.bid_amount {
                p.insert(
                    "bid_amount".into(),
                    minor(ctx, &format!("{at}.bid_amount"), b)?,
                );
            }
            for (k, v) in [
                (
                    "bid_strategy",
                    s.bid_strategy
                        .clone()
                        .or_else(|| {
                            (!cbo && s.bid_amount.is_none()).then(|| DEFAULT_BID_STRATEGY.into())
                        })
                        .map(Value::from),
                ),
                ("start_time", s.start_time.clone().map(Value::from)),
                ("end_time", s.end_time.clone().map(Value::from)),
                ("promoted_object", s.promoted_object.clone()),
                (
                    "destination_type",
                    s.destination_type.clone().map(Value::from),
                ),
            ] {
                if let Some(v) = v {
                    p.insert(k.into(), v);
                }
            }
            promoted &= s.promoted_object.is_some();
            steps.push(Step::Create {
                key: key.clone(),
                path: format!("{}/adsets", ctx.account_id),
                params: p,
                owner: Owner::Under("campaign".into()),
            });
            watches.push(watch(&key, ObjectKind::AdSet));
            for (m, ad) in s.ads.iter().enumerate() {
                let ad_at = format!("{at}.ads[{m}]");
                let ad_key = format!("ad_{}_{}", n + 1, m + 1);
                let creative_key = format!("creative_{}_{}", n + 1, m + 1);
                steps.extend(ad_steps(
                    ctx,
                    &ad_at,
                    (&creative_key, &ad_key),
                    &r(&key),
                    Owner::Under(key.clone()),
                    &ad.name,
                    Creative::New(&ad.creative),
                )?);
                watches.push(watch(&ad_key, ObjectKind::Ad));
                ads_total += 1;
            }
        }

        let budget = match (c.daily_budget, c.lifetime_budget) {
            (Some(d), _) => format!("budget {d} {}/day", ctx.currency),
            (_, Some(l)) => format!("budget {l} {} lifetime", ctx.currency),
            _ => "budget on each ad set".to_string(),
        };
        let warning = match c.objective {
            _ if promoted => None,
            Objective::Sales | Objective::Leads => {
                Some("For sales and leads, Meta usually requires promoted_object (pixel or Page).")
            }
            // Seen on the real account: without the Page on the ad set, Meta rejects the ad.
            Objective::Engagement => Some(
                "For engagement, Meta requires promoted_object with the Page on each ad set: \
                 {\"page_id\": \"<Page id>\"}.",
            ),
            _ => None,
        }
        .map(str::to_string);
        Ok(MetaPlan {
            summary: format!(
                "Create campaign '{}' ({}, paused) with {} ad sets and {ads_total} ads; {budget}. \
                 Undo deletes the campaign; the creatives stay in the account.",
                c.name.trim(),
                c.objective.as_str(),
                i.adsets.len(),
            ),
            warning,
            steps,
            watch: watches,
            inverse: InverseSpec::DeleteCreated,
            ..MetaPlan::default()
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateAdInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    adset_id: String,
    name: String,
    /// New creative (instead of creative_id).
    #[serde(default)]
    creative: Option<CreativeInput>,
    /// Creative that already exists in the account.
    #[serde(default)]
    creative_id: Option<String>,
}

struct CreateAd;

#[async_trait::async_trait]
impl WriteTool for CreateAd {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateAdInput>(
            "create_ad",
            "Creates a paused ad in an existing ad set, with a new or existing creative.",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: CreateAdInput = parse_input(input)?;
        if i.name.trim().is_empty() {
            return Err(invalid("name", "Give the ad a name."));
        }
        let creative = match (&i.creative, &i.creative_id) {
            (Some(c), None) => Creative::New(c),
            (None, Some(id)) => Creative::Existing(id),
            _ => {
                return Err(invalid(
                    "creative",
                    "Provide creative or creative_id (exactly one).",
                ));
            }
        };
        get_object(ctx, "create_ad", "Ad set", &i.adset_id, "id,account_id").await?;
        let steps = ad_steps(
            ctx,
            "",
            ("creative", "ad"),
            &i.adset_id,
            Owner::Root,
            i.name.trim(),
            creative,
        )?;
        Ok(MetaPlan {
            summary: format!(
                "Create ad '{}' (paused) in ad set {}",
                i.name.trim(),
                i.adset_id
            ),
            steps,
            watch: vec![watch("ad", ObjectKind::Ad)],
            inverse: InverseSpec::DeleteCreated,
            ..MetaPlan::default()
        })
    }
}
