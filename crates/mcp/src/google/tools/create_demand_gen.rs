//! `create_demand_gen_campaign`: a complete Demand Gen campaign in a single atomic
//! `googleAds:mutate`, with temporary ids.
//!
//! Operation order: budget (not shared) → campaign (`DEMAND_GEN`, paused) → per ad
//! group: ad group → the ad group's location and language → per ad: new images,
//! videos and cards → `adGroupAd` (multi-asset, video responsive or carousel).
//!
//! All local validation runs before any image is loaded; the same bytes become a
//! single asset ([`ImagePool`]), and so does the same video. The minimum budget is not
//! checked here: it comes from `validateOnly` (`BUDGET_TOO_LOW`). The inverse removes
//! the campaign and then the budget; the assets stay in the account.

use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::Decimal;
use adsconsole_google_ads::images::ImageRef;
use adsconsole_google_ads::resources::TempIds;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::asset_group::list;
use super::create_common::{
    ASSETS_WARNING, Bidding, ImageIn, ImagePool, budget_op, campaign_base, count, criterion_ids,
    image_sources, name_text, parse_bidding,
};
use super::keywords::create_op;
use super::reports::invalid;
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::ToolError;
use crate::google::ctx::Ctx;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

mod cta;
mod ops;
mod prepare;

use ops::Builder;
use prepare::prepare_ad;

pub(crate) fn tools() -> Vec<GTool> {
    vec![GTool::Write(Box::new(CreateDemandGenCampaign))]
}

const CREATE_DEMAND_GEN_CAMPAIGN: &str = "create_demand_gen_campaign";
const MAX_AD_GROUPS: usize = 10;
const MAX_ADS: usize = 5;
/// (minimum, maximum, characters) of the texts.
const HEADLINES: (usize, usize, usize) = (1, 5, 40);
const LONG_HEADLINES: (usize, usize, usize) = (1, 5, 90);
const DESCRIPTIONS: (usize, usize, usize) = (1, 5, 90);
const HEADLINE_CHARS: usize = 40;
const DESCRIPTION_CHARS: usize = 90;
const BUSINESS_NAME_CHARS: usize = 25;
const MAX_IMAGES: usize = 20;
const MAX_VIDEOS: usize = 5;
const MIN_CARDS: usize = 2;
const MAX_CARDS: usize = 10;

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ImagesIn {
    /// 1.91:1 images (minimum 600x314), at least one.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    landscape: Vec<Value>,
    /// 1:1 images (minimum 300x300), at least one.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    square: Vec<Value>,
    /// 4:5 images (minimum 480x600).
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    portrait: Vec<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MultiAssetIn {
    /// 1 to 5 headlines of up to 40 characters.
    #[serde(default)]
    headlines: Vec<String>,
    /// 1 to 5 descriptions of up to 90 characters.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Business name (up to 25 characters).
    business_name: String,
    /// 1:1 logo (minimum 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// Images: each one `{"file_path": "/abs/x.png"}`, `{"url": "https://…"}` or
    /// `{"asset": "customers/…/assets/…"}` (png, jpeg or gif, up to 5 MB).
    #[serde(default)]
    images: ImagesIn,
    /// Final URL (`https://…`).
    final_url: String,
    /// Button text, from Google's fixed list (e.g. "Shop Now", "Learn More", "Visit
    /// Site", "Sign Up", "Contact Us", "Install", "Book Now", "Download", "Get Quote",
    /// "See More"; Google's official translation in any locale, such as pt-BR, is also
    /// accepted). Any other text is refused.
    #[serde(default)]
    call_to_action: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct VideoResponsiveIn {
    /// 1 to 5 headlines of up to 40 characters.
    #[serde(default)]
    headlines: Vec<String>,
    /// 1 to 5 long headlines of up to 90 characters.
    #[serde(default)]
    long_headlines: Vec<String>,
    /// 1 to 5 descriptions of up to 90 characters.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Business name (up to 25 characters).
    business_name: String,
    /// 1:1 logo (minimum 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// 1 to 5 videos: a YouTube id (11 characters) or an existing video asset
    /// (`customers/…/assets/…`).
    #[serde(default)]
    videos: Vec<String>,
    /// Final URL (`https://…`).
    final_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CardIn {
    /// Card headline (up to 40 characters).
    headline: String,
    /// 1.91:1 or 1:1 image, the same aspect ratio on every card.
    #[schemars(with = "ImageRef")]
    image: Value,
    /// Final URL of the card (`https://…`).
    final_url: String,
    /// Card button text, from Google's fixed list (the same options as the ad).
    #[serde(default)]
    call_to_action: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CarouselIn {
    /// Headline (up to 40 characters).
    headline: String,
    /// Description (up to 90 characters).
    description: String,
    /// Business name (up to 25 characters).
    business_name: String,
    /// 1:1 logo (minimum 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// 2 to 10 cards.
    #[serde(default)]
    cards: Vec<CardIn>,
    /// Final URL (`https://…`).
    final_url: String,
}

/// Demand Gen ad: `{"multi_asset": {…}}`, `{"video_responsive": {…}}` or
/// `{"carousel": {…}}`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DemandGenAd {
    /// Images, headlines and descriptions combined by Google.
    MultiAsset(MultiAssetIn),
    /// YouTube videos with headlines and descriptions.
    VideoResponsive(VideoResponsiveIn),
    /// Carousel of 2 to 10 cards.
    Carousel(CarouselIn),
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdGroupIn {
    name: String,
    /// Location ids (`geoTargetConstants`) of the ad group, e.g. "2076" (Brazil).
    #[serde(default)]
    locations: Vec<String>,
    /// Language ids (`languageConstants`) of the ad group, e.g. "1014" (Portuguese).
    #[serde(default)]
    languages: Vec<String>,
    /// 1 to 5 ads.
    #[serde(default)]
    ads: Vec<DemandGenAd>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateDemandGenInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Daily budget, in the account currency (the account minimum comes from Google).
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Bidding strategy; defaults to `maximize_clicks`. Or
    /// `{"maximize_conversions": {"target_cpa": 40}}`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// 1 to 10 ad groups (location and language are set on the ad group).
    #[serde(default)]
    ad_groups: Vec<AdGroupIn>,
}

struct CreateDemandGenCampaign;

#[async_trait]
impl WriteTool for CreateDemandGenCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateDemandGenInput>(
            CREATE_DEMAND_GEN_CAMPAIGN,
            "Creates a complete Google Ads Demand Gen campaign in a single atomic \
             operation: budget, campaign (created paused) and 1 to 10 ad groups, each with \
             locations, languages and 1 to 5 ads (multi_asset, video_responsive or \
             carousel). This is the official path for video, image and carousel ads; the \
             API does not create VIDEO channel campaigns. Images come from a local file \
             (absolute path), an https URL or an existing asset (png, jpeg or gif, up to 5 \
             MB). Undo removes the campaign and the budget; the created assets stay in \
             the account (Google does not remove assets).",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreateDemandGenInput = parse_input(input)?;
        build(ctx, &input).await
    }
}

/// Demand Gen strategy: defaults to `maximize_clicks`; or `maximize_conversions`.
fn dg_bidding(v: Option<&Value>) -> Result<Bidding, ToolError> {
    match parse_bidding(v)? {
        b @ (Bidding::MaximizeClicks { .. } | Bidding::MaximizeConversions { .. }) => Ok(b),
        _ => Err(
            invalid("Demand Gen accepts only maximize_clicks or maximize_conversions.")
                .with_field("bidding"),
        ),
    }
}

/// Ad video: a YouTube id (becomes a new asset) or an existing asset.
#[derive(PartialEq)]
enum VideoIn {
    Id(String),
    Asset(String),
}

/// Parts shared by the three formats.
struct Common {
    business: String,
    logo: ImageIn,
    url: String,
}

struct CardPlan {
    headline: String,
    image: ImageIn,
    url: String,
    cta: Option<String>,
}

enum AdPlan {
    Multi {
        c: Common,
        headlines: Vec<String>,
        descriptions: Vec<String>,
        /// (API key, images): 1.91:1, 1:1 and 4:5.
        images: Vec<(&'static str, Vec<ImageIn>)>,
        cta: Option<String>,
    },
    Video {
        c: Common,
        headlines: Vec<String>,
        long_headlines: Vec<String>,
        descriptions: Vec<String>,
        videos: Vec<VideoIn>,
    },
    Carousel {
        c: Common,
        headline: String,
        description: String,
        cards: Vec<CardPlan>,
    },
}

impl AdPlan {
    /// Images of the ad, in the order they are resolved.
    fn images(&self) -> Vec<&ImageIn> {
        match self {
            AdPlan::Multi { c, images, .. } => std::iter::once(&c.logo)
                .chain(images.iter().flat_map(|(_, l)| l.iter()))
                .collect(),
            AdPlan::Video { c, .. } => vec![&c.logo],
            AdPlan::Carousel { c, cards, .. } => std::iter::once(&c.logo)
                .chain(cards.iter().map(|k| &k.image))
                .collect(),
        }
    }
}

struct GroupPlan {
    name: String,
    locations: Vec<String>,
    languages: Vec<String>,
    ads: Vec<AdPlan>,
    field: String,
}

async fn build(ctx: &Ctx, input: &CreateDemandGenInput) -> Result<WritePlan, ToolError> {
    // 1. Validate everything locally, before loading any image.
    let name = name_text(&input.name, "name")?;
    let bidding = dg_bidding(input.bidding.as_ref())?;
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
        criterion_ids(&group.locations, &format!("{p}.locations"))?;
        criterion_ids(&group.languages, &format!("{p}.languages"))?;
        count(group.ads.len(), 1, MAX_ADS, "ads", &format!("{p}.ads"))?;
        let ads = group
            .ads
            .iter()
            .enumerate()
            .map(|(a, ad)| prepare_ad(ctx, ad, &format!("{p}.ads[{a}]")))
            .collect::<Result<_, _>>()?;
        groups.push(GroupPlan {
            name: n,
            locations: group.locations.clone(),
            languages: group.languages.clone(),
            ads,
            field: p,
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
        "DEMAND_GEN",
        &bidding,
        &ctx.currency,
    )?;
    ops.push(create_op(&campaign_rn, campaign)?);

    // 2. Ad groups and ads, with the images loaded.
    let mut b = Builder {
        ctx,
        temps,
        ops,
        assets: Vec::new(),
        pool: ImagePool::default(),
        videos: Vec::new(),
        ads: Vec::new(),
    };
    for g in &groups {
        b.group(&campaign_rn, g).await?;
    }

    let ads: usize = groups.iter().map(|g| g.ads.len()).sum();
    let sources = image_sources(
        groups
            .iter()
            .flat_map(|g| g.ads.iter().flat_map(AdPlan::images)),
    );
    Ok(WritePlan {
        summary: format!(
            "Creates Demand Gen campaign '{name}' (paused), {} ad groups, {ads} ads, \
             budget {} {}/day; images from {}",
            groups.len(),
            input.daily_budget.normalize(),
            ctx.currency,
            list(&sources)
        ),
        warning: Some(ASSETS_WARNING.to_string()),
        operations: b.finish(),
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
    use serde_json::json;

    #[test]
    fn bidding_defaults_to_clicks() {
        assert!(matches!(
            dg_bidding(None).unwrap(),
            Bidding::MaximizeClicks { cpc_cap: None }
        ));
        assert!(dg_bidding(Some(&json!("maximize_conversions"))).is_ok());
        for bad in [json!("manual_cpc"), json!("maximize_conversion_value")] {
            let e = dg_bidding(Some(&bad)).unwrap_err();
            assert_eq!(e.field.as_deref(), Some("bidding"), "{bad}");
        }
    }
}
