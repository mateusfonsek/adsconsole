//! `create_search_campaign`: a complete Search campaign in a single atomic
//! `googleAds:mutate`, with temporary ids.
//!
//! Operation order: budget → campaign (paused) → campaign criteria (location,
//! language, negative keywords) → sitelinks → per ad group: ad group → keywords →
//! negative keywords → RSAs. The inverse removes the campaign and then the budget; the
//! children go away with the campaign. Ad groups, keywords and ads are created enabled:
//! the paused campaign already prevents serving.

use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::Decimal;
use adsconsole_google_ads::resources::TempIds;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::create_common::{
    Bidding, SitelinkIn, bounded_text, budget_op, campaign_base, campaign_dates, count,
    create_unnamed, geo_lang_ops, name_text, parse_bidding, sitelink_ops,
};
use super::edit::{DESCRIPTIONS, HEADLINES, ad_texts, final_url, positive_micros};
use super::keywords::{create_op, keyword_body, keyword_text, match_type};
use super::reports::invalid;
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::ToolError;
use crate::google::ctx::Ctx;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![GTool::Write(Box::new(CreateSearchCampaign))]
}

const CREATE_SEARCH_CAMPAIGN: &str = "create_search_campaign";
const MAX_AD_GROUPS: usize = 20;
const MAX_KEYWORDS: usize = 200;
const MAX_NEGATIVES: usize = 200;
const MAX_ADS: usize = 3;
const PATH_CHARS: usize = 15;

#[derive(Debug, Deserialize, JsonSchema)]
struct KeywordSpec {
    /// Text (up to 80 characters and 10 words).
    #[serde(default)]
    text: Option<String>,
    /// `EXACT`, `PHRASE` or `BROAD` (lowercase also accepted).
    #[serde(default)]
    #[schemars(with = "Option<super::keywords::MatchType>")]
    match_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RsaIn {
    /// 3 to 15 headlines of up to 30 characters.
    headlines: Vec<String>,
    /// 2 to 4 descriptions of up to 90 characters.
    descriptions: Vec<String>,
    /// Final URL (`https://…`).
    final_url: String,
    /// Display URL path 1 (up to 15 characters).
    #[serde(default)]
    path1: Option<String>,
    /// Display URL path 2 (up to 15 characters).
    #[serde(default)]
    path2: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdGroupIn {
    name: String,
    /// Max CPC bid of the ad group, in the account currency. Only with `manual_cpc`.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
    /// 1 to 200 keywords.
    keywords: Vec<KeywordSpec>,
    /// Negative keywords of the ad group.
    #[serde(default)]
    negatives: Vec<KeywordSpec>,
    /// 1 to 3 responsive search ads.
    ads: Vec<RsaIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSearchInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Daily budget, in the account currency.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Bidding strategy; defaults to `maximize_clicks` with no CPC cap.
    /// `{"maximize_clicks": {"cpc_cap": 2.5}}`, `{"maximize_conversions": {"target_cpa": 40}}`,
    /// `{"maximize_conversion_value": {"target_roas": 3.5}}` or `"manual_cpc"`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// Also serve on search partners (default false).
    #[serde(default)]
    search_partners: bool,
    /// Also serve on the Display Network (default false).
    #[serde(default)]
    display_network: bool,
    /// Location ids (`geoTargetConstants`), e.g. "2076" (Brazil).
    #[serde(default)]
    locations: Vec<String>,
    /// Language ids (`languageConstants`), e.g. "1014" (Portuguese).
    #[serde(default)]
    languages: Vec<String>,
    /// YYYY-MM-DD.
    #[serde(default)]
    start_date: Option<String>,
    /// YYYY-MM-DD.
    #[serde(default)]
    end_date: Option<String>,
    /// Negative keywords of the campaign.
    #[serde(default)]
    negatives: Vec<KeywordSpec>,
    /// Up to 20 campaign sitelinks.
    #[serde(default)]
    sitelinks: Vec<SitelinkIn>,
    /// 1 to 20 ad groups.
    ad_groups: Vec<AdGroupIn>,
}

struct CreateSearchCampaign;

#[async_trait]
impl WriteTool for CreateSearchCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateSearchInput>(
            CREATE_SEARCH_CAMPAIGN,
            "Creates a complete Google Ads Search campaign in a single atomic operation: \
             budget, campaign (created paused), locations, languages, negative keywords, \
             sitelinks and ad groups with keywords and responsive search ads. Undo removes \
             the campaign and the budget; the sitelink assets stay in the account (Google \
             does not remove assets).",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreateSearchInput = parse_input(input)?;
        build(ctx, &input)
    }
}

/// Keyword criteria (positive or negative) of `parent_field` = `parent_rn`.
fn keyword_ops(
    op_key: &str,
    parent_field: &str,
    parent_rn: &str,
    items: &[KeywordSpec],
    negative: bool,
    field: &str,
) -> Result<Vec<Value>, ToolError> {
    items
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let text = keyword_text(k.text.as_deref(), &format!("{field}[{i}].text"))?;
            let mt = match_type(&k.match_type, &format!("{field}[{i}].match_type"))?;
            Ok(create_unnamed(
                op_key,
                keyword_body(parent_field, parent_rn, &text, mt.to_google(), negative),
            ))
        })
        .collect()
}

fn rsa_op(ad_group_rn: &str, ad: &RsaIn, field: &str) -> Result<Value, ToolError> {
    let headlines = ad_texts(&ad.headlines, HEADLINES, &format!("{field}.headlines"))?;
    let descriptions = ad_texts(
        &ad.descriptions,
        DESCRIPTIONS,
        &format!("{field}.descriptions"),
    )?;
    let url = ad.final_url.trim();
    final_url(url, &format!("{field}.final_url"))?;
    let mut rsa = json!({ "headlines": headlines, "descriptions": descriptions });
    for (key, value) in [("path1", &ad.path1), ("path2", &ad.path2)] {
        if let Some(p) = value {
            rsa[key] = json!(bounded_text(p, PATH_CHARS, &format!("{field}.{key}"))?);
        }
    }
    Ok(create_unnamed(
        "adGroupAdOperation",
        json!({ "adGroup": ad_group_rn, "status": "ENABLED",
                "ad": { "finalUrls": [url], "responsiveSearchAd": rsa } }),
    ))
}

fn build(ctx: &Ctx, input: &CreateSearchInput) -> Result<WritePlan, ToolError> {
    let name = name_text(&input.name, "name")?;
    let bidding = parse_bidding(input.bidding.as_ref())?;
    count(
        input.ad_groups.len(),
        1,
        MAX_AD_GROUPS,
        "ad groups",
        "ad_groups",
    )?;
    count(
        input.negatives.len(),
        0,
        MAX_NEGATIVES,
        "negative keywords",
        "negatives",
    )?;

    let mut temps = TempIds::new(&ctx.customer_id);
    let budget_rn = temps.next("campaignBudgets");
    let campaign_rn = temps.next("campaigns");
    let mut operations = vec![budget_op(
        &budget_rn,
        &name,
        input.daily_budget,
        &ctx.currency,
        false,
    )?];

    let mut campaign = campaign_base(
        &campaign_rn,
        &budget_rn,
        &name,
        "SEARCH",
        &bidding,
        &ctx.currency,
    )?;
    campaign["networkSettings"] = json!({
        "targetGoogleSearch": true,
        "targetSearchNetwork": input.search_partners,
        "targetContentNetwork": input.display_network,
    });
    campaign_dates(
        &mut campaign,
        input.start_date.as_deref(),
        input.end_date.as_deref(),
    )?;
    operations.push(create_op(&campaign_rn, campaign)?);

    operations.extend(geo_lang_ops(
        &campaign_rn,
        &input.locations,
        &input.languages,
    )?);
    operations.extend(keyword_ops(
        "campaignCriterionOperation",
        "campaign",
        &campaign_rn,
        &input.negatives,
        true,
        "negatives",
    )?);
    operations.extend(sitelink_ops(&mut temps, &campaign_rn, &input.sitelinks)?);

    let mut names: Vec<String> = Vec::new();
    let mut keywords = 0;
    for (g, group) in input.ad_groups.iter().enumerate() {
        let field = |name: &str| format!("ad_groups[{g}].{name}");
        let group_name = name_text(&group.name, &field("name"))?;
        if names.contains(&group_name) {
            return Err(invalid(format!(
                "{}: an ad group named '{group_name}' already exists in this campaign.",
                field("name")
            ))
            .with_field(field("name")));
        }
        names.push(group_name.clone());
        count(
            group.keywords.len(),
            1,
            MAX_KEYWORDS,
            "keywords",
            &field("keywords"),
        )?;
        count(
            group.negatives.len(),
            0,
            MAX_NEGATIVES,
            "negative keywords",
            &field("negatives"),
        )?;
        count(group.ads.len(), 1, MAX_ADS, "ads", &field("ads"))?;

        let ag_rn = temps.next("adGroups");
        let mut ad_group = json!({
            "name": group_name,
            "campaign": campaign_rn,
            "type": "SEARCH_STANDARD",
            "status": "ENABLED",
        });
        if let Some(bid) = group.cpc_bid {
            if !bidding.is_manual() {
                return Err(invalid(format!(
                    "{}: an ad group bid is only allowed with the manual_cpc strategy.",
                    field("cpc_bid")
                ))
                .with_field(field("cpc_bid")));
            }
            let micros = positive_micros(bid, &ctx.currency, &field("cpc_bid"))?;
            ad_group["cpcBidMicros"] = json!(micros.to_string());
        }
        operations.push(create_op(&ag_rn, ad_group)?);
        operations.extend(keyword_ops(
            "adGroupCriterionOperation",
            "adGroup",
            &ag_rn,
            &group.keywords,
            false,
            &field("keywords"),
        )?);
        operations.extend(keyword_ops(
            "adGroupCriterionOperation",
            "adGroup",
            &ag_rn,
            &group.negatives,
            true,
            &field("negatives"),
        )?);
        for (a, ad) in group.ads.iter().enumerate() {
            operations.push(rsa_op(&ag_rn, ad, &format!("ad_groups[{g}].ads[{a}]"))?);
        }
        keywords += group.keywords.len();
    }

    Ok(WritePlan {
        summary: format!(
            "Create Search campaign '{name}' (paused), {} ad groups, {keywords} keywords, \
             budget {} {}/day",
            input.ad_groups.len(),
            input.daily_budget.normalize(),
            ctx.currency
        ),
        warning: None,
        operations,
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
