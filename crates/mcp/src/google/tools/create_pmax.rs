//! `create_pmax_campaign`: a complete Performance Max campaign in a single atomic
//! `googleAds:mutate`, with temporary ids.
//!
//! Operation order: budget (not shared) → campaign (paused, with brand guidelines) →
//! location and language → brand (business name, logo and 4:1 logo, linked through
//! `campaignAsset`) → per asset group: text, image and video assets → `assetGroup` →
//! `assetGroupAsset` per type → `assetGroupSignal`.
//!
//! All local validation (texts, URLs, videos, signals, counts, image references) runs
//! before any image is loaded. Images are loaded (file or URL) and checked on every
//! `plan`; the same bytes become a single asset ([`ImagePool`]), and so does the same
//! video. The inverse removes the campaign and then the budget; the assets stay in the
//! account (Google does not remove assets).

use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::Decimal;
use adsconsole_google_ads::images::{ImageKind, ImageRef};
use adsconsole_google_ads::resources::TempIds;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::asset_group::{FieldType, SEARCH_THEME_MAX, collection_rn, list, youtube_id_or_link};
use super::create_common::{
    ASSETS_WARNING, Bidding, ImageIn, ImagePool, bounded_text, budget_op, campaign_base, count,
    create_unnamed, geo_lang_ops, image_in, image_sources, name_text, parse_bidding,
};
use super::edit::final_url;
use super::keywords::create_op;
use super::reports::invalid;
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::ToolError;
use crate::google::ctx::Ctx;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![GTool::Write(Box::new(CreatePmaxCampaign))]
}

const CREATE_PMAX_CAMPAIGN: &str = "create_pmax_campaign";
const MAX_ASSET_GROUPS: usize = 5;
const BUSINESS_NAME_CHARS: usize = 25;
/// At least one headline of up to 15 characters and one description of up to 60.
const SHORT_HEADLINE: usize = 15;
const SHORT_DESCRIPTION: usize = 60;
/// Google's limits per asset group.
const MAX_IMAGES: usize = 20;
const MAX_VIDEOS: usize = 5;
const MAX_SEARCH_THEMES: usize = 25;
const URL_EXPANSION: &str = "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION";
#[derive(Debug, Deserialize, JsonSchema)]
struct BrandIn {
    /// Business name (up to 25 characters).
    business_name: String,
    /// Square 1:1 logo (minimum 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// Landscape 4:1 logo (minimum 512x128).
    #[serde(default)]
    #[schemars(with = "Option<ImageRef>")]
    landscape_logo: Option<Value>,
}

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

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct SignalsIn {
    /// Audience names (`customers/123/audiences/456`).
    #[serde(default)]
    audiences: Vec<String>,
    /// Up to 25 search themes (up to 80 characters each).
    #[serde(default)]
    search_themes: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AssetGroupIn {
    name: String,
    /// Final URLs (at least one).
    #[serde(default)]
    final_urls: Vec<String>,
    /// 3 to 15 headlines of up to 30 characters; at least one of up to 15.
    #[serde(default)]
    headlines: Vec<String>,
    /// 1 to 5 long headlines of up to 90 characters.
    #[serde(default)]
    long_headlines: Vec<String>,
    /// 2 to 5 descriptions of up to 90 characters; at least one of up to 60.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Images: each one `{"file_path": "/abs/x.png"}`, `{"url": "https://…"}` or
    /// `{"asset": "customers/…/assets/…"}` (png, jpeg or gif, up to 5 MB).
    #[serde(default)]
    images: ImagesIn,
    /// Up to 5 YouTube video ids (11 characters, not the URL).
    #[serde(default)]
    youtube_video_ids: Vec<String>,
    /// Audience signals and search themes.
    #[serde(default)]
    signals: Option<SignalsIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreatePmaxInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Daily budget, in the account currency.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Bidding strategy; defaults to `maximize_conversions`.
    /// `{"maximize_conversions": {"target_cpa": 40}}` or
    /// `{"maximize_conversion_value": {"target_roas": 3.5}}`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// Let Google replace the final URL and generate texts from the site (default false).
    #[serde(default)]
    final_url_expansion: bool,
    /// Location ids (`geoTargetConstants`), e.g. "2076" (Brazil).
    #[serde(default)]
    locations: Vec<String>,
    /// Language ids (`languageConstants`), e.g. "1014" (Portuguese).
    #[serde(default)]
    languages: Vec<String>,
    /// Campaign brand: business name and logos.
    brand: BrandIn,
    /// 1 to 5 asset groups.
    #[serde(default)]
    asset_groups: Vec<AssetGroupIn>,
}

struct CreatePmaxCampaign;

#[async_trait]
impl WriteTool for CreatePmaxCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreatePmaxInput>(
            CREATE_PMAX_CAMPAIGN,
            "Creates a complete Google Ads Performance Max campaign in a single atomic \
             operation: budget, campaign (created paused), locations, languages, brand \
             (business name and logos) and 1 to 5 asset groups with headlines, \
             descriptions, images, videos and signals. Images come from a local file \
             (absolute path), an https URL or an existing asset (png, jpeg or gif, up to 5 \
             MB). Performance Max requires conversion tracking set up in the account \
             (bidding maximize_conversions or maximize_conversion_value). Undo removes the \
             campaign and the budget; the created assets stay in the account (Google does \
             not remove assets).",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreatePmaxInput = parse_input(input)?;
        build(ctx, &input).await
    }
}

/// PMax strategy: defaults to `maximize_conversions`; only conversion strategies are allowed.
fn pmax_bidding(v: Option<&Value>) -> Result<Bidding, ToolError> {
    if v.is_none_or(Value::is_null) {
        return Ok(Bidding::MaximizeConversions { target_cpa: None });
    }
    match parse_bidding(v)? {
        b @ (Bidding::MaximizeConversions { .. } | Bidding::MaximizeConversionValue { .. }) => {
            Ok(b)
        }
        _ => Err(invalid(
            "Performance Max accepts only maximize_conversions or maximize_conversion_value.",
        )
        .with_field("bidding")),
    }
}

/// Texts of one asset group type: count and length from [`FieldType`], without
/// repeats and, with `short`, at least one of up to `short` characters.
fn group_texts(
    items: &[String],
    ft: FieldType,
    short: Option<usize>,
    field: &str,
) -> Result<Vec<String>, ToolError> {
    let max_len = ft.text_max().unwrap_or_default();
    count(
        items.len(),
        ft.min(),
        ft.max().unwrap_or(usize::MAX),
        ft.label(),
        field,
    )?;
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in items.iter().enumerate() {
        let f = format!("{field}[{i}]");
        let t = bounded_text(raw, max_len, &f)?;
        if out.contains(&t) {
            return Err(invalid(format!("{f}: '{t}' appears more than once.")).with_field(f));
        }
        out.push(t);
    }
    if let Some(n) = short
        && !out.iter().any(|t| t.chars().count() <= n)
    {
        return Err(invalid(format!(
            "{field}: provide at least one item of up to {n} characters."
        ))
        .with_field(field));
    }
    Ok(out)
}

/// A validated asset group, ready to become operations.
struct GroupPlan {
    name: String,
    urls: Vec<String>,
    texts: Vec<(FieldType, Vec<String>)>,
    images: Vec<(FieldType, ImageIn)>,
    videos: Vec<String>,
    audiences: Vec<String>,
    themes: Vec<String>,
}

/// Local validation of an asset group (no images are loaded).
fn prepare_group(
    ctx: &Ctx,
    group: &AssetGroupIn,
    g: usize,
    name: String,
) -> Result<GroupPlan, ToolError> {
    let field = |name: &str| format!("asset_groups[{g}].{name}");

    if group.final_urls.is_empty() {
        return Err(invalid(format!(
            "{}: provide at least one final URL.",
            field("final_urls")
        ))
        .with_field(field("final_urls")));
    }
    let mut urls = Vec::new();
    for (i, u) in group.final_urls.iter().enumerate() {
        let u = u.trim();
        final_url(u, &field(&format!("final_urls[{i}]")))?;
        urls.push(u.to_string());
    }

    let texts = vec![
        (
            FieldType::Headline,
            group_texts(
                &group.headlines,
                FieldType::Headline,
                Some(SHORT_HEADLINE),
                &field("headlines"),
            )?,
        ),
        (
            FieldType::LongHeadline,
            group_texts(
                &group.long_headlines,
                FieldType::LongHeadline,
                None,
                &field("long_headlines"),
            )?,
        ),
        (
            FieldType::Description,
            group_texts(
                &group.descriptions,
                FieldType::Description,
                Some(SHORT_DESCRIPTION),
                &field("descriptions"),
            )?,
        ),
    ];

    let imgs = &group.images;
    count(
        imgs.landscape.len(),
        1,
        MAX_IMAGES,
        "1.91:1 images",
        &field("images.landscape"),
    )?;
    count(
        imgs.square.len(),
        1,
        MAX_IMAGES,
        "1:1 images",
        &field("images.square"),
    )?;
    let total = imgs.landscape.len() + imgs.square.len() + imgs.portrait.len();
    if total > MAX_IMAGES {
        return Err(invalid(format!(
            "{}: at most {MAX_IMAGES} images per asset group (got {total}).",
            field("images")
        ))
        .with_field(field("images")));
    }
    let mut images: Vec<(FieldType, ImageIn)> = Vec::new();
    for (items, kind, ft, key) in [
        (
            &imgs.landscape,
            ImageKind::Landscape,
            FieldType::MarketingImage,
            "landscape",
        ),
        (
            &imgs.square,
            ImageKind::Square,
            FieldType::SquareMarketingImage,
            "square",
        ),
        (
            &imgs.portrait,
            ImageKind::Portrait,
            FieldType::PortraitMarketingImage,
            "portrait",
        ),
    ] {
        for (i, v) in items.iter().enumerate() {
            let img = image_in(ctx, v, kind, field(&format!("images.{key}[{i}]")))?;
            if images.iter().any(|(t, x)| *t == ft && x.r == img.r) {
                return Err(
                    invalid(format!("{}: the image appears more than once.", img.field))
                        .with_field(img.field),
                );
            }
            images.push((ft, img));
        }
    }

    count(
        group.youtube_video_ids.len(),
        0,
        MAX_VIDEOS,
        "videos",
        &field("youtube_video_ids"),
    )?;
    let mut videos: Vec<String> = Vec::new();
    for (i, v) in group.youtube_video_ids.iter().enumerate() {
        let f = field(&format!("youtube_video_ids[{i}]"));
        let id = youtube_id_or_link(v, &f)?;
        if videos.contains(&id) {
            return Err(invalid(format!("{f}: video {id} appears more than once.")).with_field(f));
        }
        videos.push(id);
    }

    let signals = group.signals.as_ref();
    let mut audiences: Vec<String> = Vec::new();
    for (i, a) in signals
        .map(|s| s.audiences.as_slice())
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let f = field(&format!("signals.audiences[{i}]"));
        let rn = collection_rn(ctx, a, "audiences", &f)?;
        if audiences.contains(&rn) {
            return Err(invalid(format!("{f}: {rn} appears more than once.")).with_field(f));
        }
        audiences.push(rn);
    }
    let raw_themes = signals
        .map(|s| s.search_themes.as_slice())
        .unwrap_or_default();
    count(
        raw_themes.len(),
        0,
        MAX_SEARCH_THEMES,
        "search themes",
        &field("signals.search_themes"),
    )?;
    let themes = raw_themes
        .iter()
        .enumerate()
        .map(|(i, t)| {
            bounded_text(
                t,
                SEARCH_THEME_MAX,
                &field(&format!("signals.search_themes[{i}]")),
            )
        })
        .collect::<Result<_, _>>()?;

    Ok(GroupPlan {
        name,
        urls,
        texts,
        images,
        videos,
        audiences,
        themes,
    })
}

/// Operations of an asset group: new assets → asset group → links → signals. `videos`
/// holds the videos already created in this call (id → asset), to reuse across groups.
async fn group_ops(
    ctx: &Ctx,
    temps: &mut TempIds,
    pool: &mut ImagePool,
    videos: &mut Vec<(String, String)>,
    ops: &mut Vec<Value>,
    campaign_rn: &str,
    group: &GroupPlan,
) -> Result<(), ToolError> {
    let mut links: Vec<(String, FieldType)> = Vec::new();
    for (ft, items) in &group.texts {
        for t in items {
            let rn = temps.next("assets");
            ops.push(create_op(&rn, json!({ "textAsset": { "text": t } }))?);
            links.push((rn, *ft));
        }
    }
    for (ft, img) in &group.images {
        let rn = pool
            .resolve(ctx, temps, ops, &img.r, img.kind, &img.field)
            .await?;
        // Different references with the same bytes in the same type.
        if links.iter().any(|(a, t)| *a == rn && t == ft) {
            return Err(invalid(format!(
                "{}: the same image appears more than once.",
                img.field
            ))
            .with_field(img.field.clone()));
        }
        links.push((rn, *ft));
    }
    for id in &group.videos {
        let rn = match videos.iter().find(|(v, _)| v == id) {
            Some((_, rn)) => rn.clone(),
            None => {
                let rn = temps.next("assets");
                ops.push(create_op(
                    &rn,
                    json!({ "youtubeVideoAsset": { "youtubeVideoId": id } }),
                )?);
                videos.push((id.clone(), rn.clone()));
                rn
            }
        };
        links.push((rn, FieldType::YoutubeVideo));
    }

    let ag_rn = temps.next("assetGroups");
    ops.push(create_op(
        &ag_rn,
        json!({ "name": group.name, "campaign": campaign_rn, "finalUrls": group.urls,
                "status": "ENABLED" }),
    )?);
    for (asset, ft) in &links {
        ops.push(create_unnamed(
            "assetGroupAssetOperation",
            json!({ "assetGroup": ag_rn, "asset": asset, "fieldType": ft.api() }),
        ));
    }
    for aud in &group.audiences {
        ops.push(create_unnamed(
            "assetGroupSignalOperation",
            json!({ "assetGroup": ag_rn, "audience": { "audience": aud } }),
        ));
    }
    for t in &group.themes {
        ops.push(create_unnamed(
            "assetGroupSignalOperation",
            json!({ "assetGroup": ag_rn, "searchTheme": { "text": t } }),
        ));
    }
    Ok(())
}

async fn build(ctx: &Ctx, input: &CreatePmaxInput) -> Result<WritePlan, ToolError> {
    // 1. Validate everything locally, before loading any image.
    let name = name_text(&input.name, "name")?;
    let bidding = pmax_bidding(input.bidding.as_ref())?;
    count(
        input.asset_groups.len(),
        1,
        MAX_ASSET_GROUPS,
        "asset groups",
        "asset_groups",
    )?;
    let mut groups: Vec<GroupPlan> = Vec::new();
    for (g, group) in input.asset_groups.iter().enumerate() {
        let f = format!("asset_groups[{g}].name");
        let n = name_text(&group.name, &f)?;
        if groups.iter().any(|p| p.name == n) {
            return Err(invalid(format!(
                "{f}: an asset group named '{n}' already exists in this campaign."
            ))
            .with_field(f));
        }
        groups.push(prepare_group(ctx, group, g, n)?);
    }
    let brand = &input.brand;
    let business = bounded_text(
        &brand.business_name,
        BUSINESS_NAME_CHARS,
        "brand.business_name",
    )?;
    let logo = image_in(ctx, &brand.logo, ImageKind::Logo, "brand.logo".into())?;
    let wide_logo = brand
        .landscape_logo
        .as_ref()
        .map(|v| {
            image_in(
                ctx,
                v,
                ImageKind::LandscapeLogo,
                "brand.landscape_logo".into(),
            )
        })
        .transpose()?;

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
    let mut campaign = campaign_base(
        &campaign_rn,
        &budget_rn,
        &name,
        "PERFORMANCE_MAX",
        &bidding,
        &ctx.currency,
    )?;
    // The brand name and logos go on the campaign (campaignAsset), not on the asset groups.
    campaign["brandGuidelinesEnabled"] = json!(true);
    // Verified on a real account (2026-10-18): Campaign.asset_automation_settings replaces url_expansion_opt_out.
    if !input.final_url_expansion {
        campaign["assetAutomationSettings"] = json!([{
            "assetAutomationType": URL_EXPANSION,
            "assetAutomationStatus": "OPTED_OUT",
        }]);
    }
    ops.push(create_op(&campaign_rn, campaign)?);
    ops.extend(geo_lang_ops(
        &campaign_rn,
        &input.locations,
        &input.languages,
    )?);

    // 2. Brand and asset groups, with the images loaded.
    let biz_rn = temps.next("assets");
    ops.push(create_op(
        &biz_rn,
        json!({ "textAsset": { "text": business } }),
    )?);
    let link = |asset: &str, ft: &str| {
        create_unnamed(
            "campaignAssetOperation",
            json!({ "campaign": campaign_rn, "asset": asset, "fieldType": ft }),
        )
    };
    ops.push(link(&biz_rn, "BUSINESS_NAME"));
    let mut pool = ImagePool::default();
    for (img, ft) in
        std::iter::once((&logo, "LOGO")).chain(wide_logo.iter().map(|w| (w, "LANDSCAPE_LOGO")))
    {
        let rn = pool
            .resolve(ctx, &mut temps, &mut ops, &img.r, img.kind, &img.field)
            .await?;
        ops.push(link(&rn, ft));
    }
    let mut videos: Vec<(String, String)> = Vec::new();
    for group in &groups {
        group_ops(
            ctx,
            &mut temps,
            &mut pool,
            &mut videos,
            &mut ops,
            &campaign_rn,
            group,
        )
        .await?;
    }

    let sources = image_sources(
        std::iter::once(&logo)
            .chain(wide_logo.iter())
            .chain(groups.iter().flat_map(|g| g.images.iter().map(|(_, i)| i))),
    );

    Ok(WritePlan {
        summary: format!(
            "Create Performance Max campaign '{name}' (paused), {} asset groups, \
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

    #[test]
    fn bidding_defaults_to_conversions() {
        assert!(matches!(
            pmax_bidding(None).unwrap(),
            Bidding::MaximizeConversions { target_cpa: None }
        ));
        assert!(pmax_bidding(Some(&Value::Null)).is_ok());
        assert!(pmax_bidding(Some(&json!("maximize_conversion_value"))).is_ok());
        let e = pmax_bidding(Some(&json!("manual_cpc"))).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("bidding"));
    }
}
