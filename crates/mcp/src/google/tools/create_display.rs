//! `create_display_campaign`: a complete Display campaign in a single atomic
//! `googleAds:mutate`, with temporary ids.
//!
//! Operation order: budget (not shared) → campaign (`DISPLAY`, paused) → location and
//! language → per ad group: ad group (`DISPLAY_STANDARD`) → targeting (audiences,
//! placements, topics) → per ad: new images → responsive display ad.
//!
//! Google has been refusing Display campaign creation through the API; that refusal
//! becomes `DISPLAY_NOT_AVAILABLE`, pointing to Demand Gen ([`WriteTool::map_error`]).

use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::Decimal;
use adsconsole_google_ads::images::{ImageKind, ImageRef};
use adsconsole_google_ads::resources::TempIds;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::asset_group::list;
use super::create_common::{
    ASSETS_WARNING, Bidding, ImageIn, ImagePool, bounded_text, budget_op, campaign_base, count,
    create_unnamed, geo_lang_ops, image_list, image_sources, name_text, parse_bidding, text_assets,
    text_list,
};
use super::edit::{final_url, positive_micros};
use super::keywords::create_op;
use super::reports::invalid;
use super::targeting::{audience_criterion, placement_criterion, topic_criterion, with_parent};
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![GTool::Write(Box::new(CreateDisplayCampaign))]
}

const CREATE_DISPLAY_CAMPAIGN: &str = "create_display_campaign";
const MAX_AD_GROUPS: usize = 20;
const MAX_ADS: usize = 5;
const MAX_TARGETS: usize = 100;
const HEADLINES: (usize, usize, usize) = (1, 5, 30);
const DESCRIPTIONS: (usize, usize, usize) = (1, 5, 90);
const LONG_HEADLINE_CHARS: usize = 90;
const BUSINESS_NAME_CHARS: usize = 25;
/// Responsive display ad limits per image type.
const MAX_MARKETING_IMAGES: usize = 15;
const MAX_LOGOS: usize = 5;
const NOT_AVAILABLE: &str = "Google no longer allows creating Display campaigns through \
     the API. Use create_demand_gen_campaign.";
/// Google codes that mean the Display campaign was refused.
const REFUSAL_CODES: [&str; 2] = [
    "OPERATION_NOT_PERMITTED_FOR_CONTEXT",
    "CANNOT_CREATE_DISPLAY_CAMPAIGN",
];

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct TargetingIn {
    /// Audience list ids (`userLists`) of this account.
    #[serde(default)]
    audiences: Vec<String>,
    /// Sites: an `https://…` URL or a domain (`example.com`).
    #[serde(default)]
    placements: Vec<String>,
    /// Topic ids (`topicConstants`).
    #[serde(default)]
    topics: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RdaIn {
    /// 1 to 5 headlines of up to 30 characters.
    #[serde(default)]
    headlines: Vec<String>,
    /// Long headline (up to 90 characters).
    long_headline: String,
    /// 1 to 5 descriptions of up to 90 characters.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Business name (up to 25 characters).
    business_name: String,
    /// 1.91:1 images (minimum 600x314), 1 to 15. Each one is
    /// `{"file_path": "/abs/x.png"}`, `{"url": "https://…"}` or `{"asset": "customers/…/assets/…"}`.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    marketing_images: Vec<Value>,
    /// 1:1 images (minimum 300x300), 1 to 15.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    square_marketing_images: Vec<Value>,
    /// 4:1 logos (minimum 512x128), up to 5.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    logo_images: Vec<Value>,
    /// 1:1 logos (minimum 128x128), up to 5.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    square_logo_images: Vec<Value>,
    /// Final URL (`https://…`).
    final_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdGroupIn {
    name: String,
    /// Max CPC bid of the ad group, in the account currency. Only with `manual_cpc`.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
    /// Targeting of the ad group.
    #[serde(default)]
    targeting: Option<TargetingIn>,
    /// 1 to 5 responsive display ads.
    #[serde(default)]
    ads: Vec<RdaIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateDisplayInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Daily budget, in the account currency.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Bidding strategy; defaults to `maximize_clicks`. Or
    /// `{"maximize_conversions": {"target_cpa": 40}}` or `"manual_cpc"`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// Location ids (`geoTargetConstants`), e.g. "2076" (Brazil).
    #[serde(default)]
    locations: Vec<String>,
    /// Language ids (`languageConstants`), e.g. "1014" (Portuguese).
    #[serde(default)]
    languages: Vec<String>,
    /// 1 to 20 ad groups.
    #[serde(default)]
    ad_groups: Vec<AdGroupIn>,
}

struct CreateDisplayCampaign;

#[async_trait]
impl WriteTool for CreateDisplayCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateDisplayInput>(
            CREATE_DISPLAY_CAMPAIGN,
            "Creates a Google Ads Display campaign in a single atomic operation: budget, \
             campaign (created paused), locations, languages and ad groups with targeting \
             (audiences, placements, topics) and responsive display ads. Images come from a \
             local file (absolute path), an https URL or an existing asset. Google may \
             refuse new Display campaigns through the API (DISPLAY_NOT_AVAILABLE); in that \
             case use create_demand_gen_campaign. Undo removes the campaign and the budget; \
             the created assets stay in the account.",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreateDisplayInput = parse_input(input)?;
        build(ctx, &input).await
    }

    fn map_error(&self, e: ToolError) -> ToolError {
        // "Maybe applied" (EXECUTION_FAILED) is kept as it came, with the suggestion to
        // check the account.
        if e.code == codes::EXECUTION_FAILED || !is_refusal(&e) {
            return e;
        }
        let mut out = ToolError::new(codes::DISPLAY_NOT_AVAILABLE, NOT_AVAILABLE)
            .with_suggestion("Use create_demand_gen_campaign for image ads.");
        out.details = e.details;
        out
    }
}

/// Google error that refuses the Display campaign: on the `advertising_channel_type`
/// field or with one of the [`REFUSAL_CODES`].
fn is_refusal(e: &ToolError) -> bool {
    let Some(errors) = e
        .details
        .as_ref()
        .and_then(|d| d.get("errors"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    errors.iter().any(|x| {
        let code = x.get("code").and_then(Value::as_str).unwrap_or_default();
        let path = x
            .get("field_path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        path.contains("advertising_channel_type")
            || path.contains("advertisingChannelType")
            || REFUSAL_CODES.contains(&code)
    })
}

fn display_bidding(v: Option<&Value>) -> Result<Bidding, ToolError> {
    match parse_bidding(v)? {
        b @ (Bidding::MaximizeClicks { .. }
        | Bidding::MaximizeConversions { .. }
        | Bidding::ManualCpc {}) => Ok(b),
        _ => Err(
            invalid("Display accepts maximize_clicks, maximize_conversions or manual_cpc.")
                .with_field("bidding"),
        ),
    }
}

struct AdPlan {
    headlines: Vec<String>,
    long_headline: String,
    descriptions: Vec<String>,
    business: String,
    /// (API key, images).
    images: Vec<(&'static str, Vec<ImageIn>)>,
    url: String,
}

struct GroupPlan {
    name: String,
    cpc_micros: Option<i64>,
    criteria: Vec<Value>,
    ads: Vec<AdPlan>,
}

fn prepare_ad(ctx: &Ctx, ad: &RdaIn, p: &str) -> Result<AdPlan, ToolError> {
    let f = |name: &str| format!("{p}.{name}");
    let images = vec![
        (
            "marketingImages",
            image_list(
                ctx,
                &ad.marketing_images,
                ImageKind::Landscape,
                (1, MAX_MARKETING_IMAGES),
                "1.91:1 images",
                &f("marketing_images"),
            )?,
        ),
        (
            "squareMarketingImages",
            image_list(
                ctx,
                &ad.square_marketing_images,
                ImageKind::Square,
                (1, MAX_MARKETING_IMAGES),
                "1:1 images",
                &f("square_marketing_images"),
            )?,
        ),
        (
            "logoImages",
            image_list(
                ctx,
                &ad.logo_images,
                ImageKind::LandscapeLogo,
                (0, MAX_LOGOS),
                "4:1 logos",
                &f("logo_images"),
            )?,
        ),
        (
            "squareLogoImages",
            image_list(
                ctx,
                &ad.square_logo_images,
                ImageKind::Logo,
                (0, MAX_LOGOS),
                "1:1 logos",
                &f("square_logo_images"),
            )?,
        ),
    ];
    let url = ad.final_url.trim();
    final_url(url, &f("final_url"))?;
    Ok(AdPlan {
        headlines: text_list(&ad.headlines, HEADLINES, "headlines", &f("headlines"))?,
        long_headline: bounded_text(&ad.long_headline, LONG_HEADLINE_CHARS, &f("long_headline"))?,
        descriptions: text_list(
            &ad.descriptions,
            DESCRIPTIONS,
            "descriptions",
            &f("descriptions"),
        )?,
        business: bounded_text(&ad.business_name, BUSINESS_NAME_CHARS, &f("business_name"))?,
        images,
        url: url.to_string(),
    })
}

/// Bodies of the ad group's targeting criteria (without the parent). Errors on
/// `{p}.targeting.<kind>[i]`.
fn targeting_criteria(ctx: &Ctx, t: &TargetingIn, p: &str) -> Result<Vec<Value>, ToolError> {
    let mut out = Vec::new();
    let kinds: [(&str, &str, &[String]); 3] = [
        ("audiences", "audiences", &t.audiences),
        ("placements", "placements", &t.placements),
        ("topics", "topics", &t.topics),
    ];
    for (key, label, items) in kinds {
        let lf = format!("{p}.targeting.{key}");
        count(items.len(), 0, MAX_TARGETS, label, &lf)?;
        for (i, raw) in items.iter().enumerate() {
            let f = format!("{lf}[{i}]");
            let v = Some(raw.trim());
            let body = match key {
                "audiences" => audience_criterion(ctx, v, &f),
                "placements" => placement_criterion(v, &f),
                _ => topic_criterion(v, &f),
            }
            .map_err(|e| e.with_field(f.clone()))?;
            if out.contains(&body) {
                return Err(invalid(format!("{f}: '{raw}' appears more than once.")).with_field(f));
            }
            out.push(body);
        }
    }
    Ok(out)
}

async fn build(ctx: &Ctx, input: &CreateDisplayInput) -> Result<WritePlan, ToolError> {
    // 1. Validate everything locally, before loading any image.
    let name = name_text(&input.name, "name")?;
    let bidding = display_bidding(input.bidding.as_ref())?;
    count(
        input.ad_groups.len(),
        1,
        MAX_AD_GROUPS,
        "ad groups",
        "ad_groups",
    )?;
    let mut groups: Vec<GroupPlan> = Vec::new();
    for (g, group) in input.ad_groups.iter().enumerate() {
        let p = format!("ad_groups[{g}]");
        let nf = format!("{p}.name");
        let n = name_text(&group.name, &nf)?;
        if groups.iter().any(|x| x.name == n) {
            return Err(invalid(format!(
                "{nf}: an ad group named '{n}' already exists in this campaign."
            ))
            .with_field(nf));
        }
        let cpc_micros = match group.cpc_bid {
            Some(_) if !bidding.is_manual() => {
                let f = format!("{p}.cpc_bid");
                return Err(invalid(format!(
                    "{f}: an ad group bid is only allowed with the manual_cpc strategy."
                ))
                .with_field(f));
            }
            Some(bid) => Some(positive_micros(
                bid,
                &ctx.currency,
                &format!("{p}.cpc_bid"),
            )?),
            None => None,
        };
        let criteria = match &group.targeting {
            Some(t) => targeting_criteria(ctx, t, &p)?,
            None => Vec::new(),
        };
        count(group.ads.len(), 1, MAX_ADS, "ads", &format!("{p}.ads"))?;
        let ads = group
            .ads
            .iter()
            .enumerate()
            .map(|(a, ad)| prepare_ad(ctx, ad, &format!("{p}.ads[{a}]")))
            .collect::<Result<_, _>>()?;
        groups.push(GroupPlan {
            name: n,
            cpc_micros,
            criteria,
            ads,
        });
    }

    let mut temps = TempIds::new(&ctx.customer_id);
    let budget_rn = temps.next("campaignBudgets");
    let campaign_rn = temps.next("campaigns");
    let mut ops = vec![budget_op(
        &budget_rn,
        &name,
        input.daily_budget,
        &ctx.currency,
        false,
    )?];
    let campaign = campaign_base(
        &campaign_rn,
        &budget_rn,
        &name,
        "DISPLAY",
        &bidding,
        &ctx.currency,
    )?;
    ops.push(create_op(&campaign_rn, campaign)?);
    ops.extend(geo_lang_ops(
        &campaign_rn,
        &input.locations,
        &input.languages,
    )?);

    // 2. Ad groups and ads, with the images loaded.
    let mut pool = ImagePool::default();
    for g in &groups {
        let ag_rn = temps.next("adGroups");
        let mut ad_group = json!({
            "name": g.name,
            "campaign": campaign_rn,
            "type": "DISPLAY_STANDARD",
            "status": "ENABLED",
        });
        if let Some(m) = g.cpc_micros {
            ad_group["cpcBidMicros"] = json!(m.to_string());
        }
        ops.push(create_op(&ag_rn, ad_group)?);
        for body in &g.criteria {
            ops.push(create_unnamed(
                "adGroupCriterionOperation",
                with_parent(body.clone(), "adGroup", &ag_rn),
            ));
        }
        for ad in &g.ads {
            let mut rda = json!({
                "headlines": text_assets(&ad.headlines),
                "longHeadline": { "text": ad.long_headline },
                "descriptions": text_assets(&ad.descriptions),
                "businessName": ad.business,
            });
            for (key, imgs) in ad.images.iter().filter(|(_, l)| !l.is_empty()) {
                rda[*key] = pool.resolve_list(ctx, &mut temps, &mut ops, imgs).await?;
            }
            ops.push(create_unnamed(
                "adGroupAdOperation",
                json!({ "adGroup": ag_rn, "status": "ENABLED",
                        "ad": { "finalUrls": [ad.url], "responsiveDisplayAd": rda } }),
            ));
        }
    }

    let ads: usize = groups.iter().map(|g| g.ads.len()).sum();
    let sources = image_sources(
        groups
            .iter()
            .flat_map(|g| g.ads.iter())
            .flat_map(|a| a.images.iter().flat_map(|(_, l)| l.iter())),
    );
    Ok(WritePlan {
        summary: format!(
            "Create Display campaign '{name}' (paused), {} ad groups, {ads} ads, \
             budget {} {}/day; images from {}",
            groups.len(),
            input.daily_budget.normalize(),
            ctx.currency,
            list(&sources)
        ),
        warning: Some(ASSETS_WARNING.to_string()),
        operations: ops,
        field_changes: Vec::new(),
        watch: vec![Watch {
            resource_name: campaign_rn.clone(),
            resource: "campaign",
            fields: vec!["campaign.status", "campaign.name"],
        }],
        inverse: InverseSpec::RemoveCreated {
            order: vec![campaign_rn, budget_rn],
        },
        direct: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(errors: Value) -> ToolError {
        ToolError::new(codes::API_ERROR, "x").with_details(json!({ "errors": errors }))
    }

    #[test]
    fn refusal_detection() {
        let refused = [
            json!([{ "code": "CANNOT_SET_FIELD",
                     "field_path": "mutate_operations[1].campaign_operation.create.advertising_channel_type" }]),
            json!([{ "code": "OPERATION_NOT_PERMITTED_FOR_CONTEXT", "field_path": null }]),
            json!([{ "code": "CANNOT_CREATE_DISPLAY_CAMPAIGN" }]),
            json!([{ "code": "CANNOT_SET_FIELD",
                     "field_path": "mutateOperations[1].campaignOperation.create.advertisingChannelType" }]),
        ];
        for errors in refused {
            let e = CreateDisplayCampaign.map_error(api_error(errors.clone()));
            assert_eq!(e.code, codes::DISPLAY_NOT_AVAILABLE, "{errors}");
            assert_eq!(e.message, NOT_AVAILABLE);
            assert!(e.details.is_some());
        }
        let other = api_error(json!([{ "code": "BUDGET_BELOW_PER_DAY_MINIMUM",
                                       "field_path": "operations[0].create.amount_micros" }]));
        assert_eq!(
            CreateDisplayCampaign.map_error(other).code,
            codes::API_ERROR
        );
        // Maybe applied: kept as it came, even when it mentions the channel.
        let mut maybe = api_error(json!([{ "code": "OPERATION_NOT_PERMITTED_FOR_CONTEXT",
                                           "field_path": "operations[1].create.advertising_channel_type" }]));
        maybe.code = codes::EXECUTION_FAILED.into();
        maybe.suggestion = Some("check the account".into());
        let kept = CreateDisplayCampaign.map_error(maybe);
        assert_eq!(kept.code, codes::EXECUTION_FAILED);
        assert_eq!(kept.suggestion.as_deref(), Some("check the account"));
        let local = ToolError::new(codes::INVALID_INPUT, "x");
        assert_eq!(
            CreateDisplayCampaign.map_error(local).code,
            codes::INVALID_INPUT
        );
    }
}
