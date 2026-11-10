//! Local validation of Demand Gen ads (no images are loaded).

use adsconsole_google_ads::images::ImageKind;
use serde_json::Value;

use super::super::asset_group::{asset_rn, youtube_id_or_link};
use super::super::create_common::{bounded_text, count, image_in, image_list, text_list};
use super::super::edit::final_url;
use super::super::reports::invalid;
use super::cta;
use super::{
    AdPlan, BUSINESS_NAME_CHARS, CardPlan, Common, DESCRIPTION_CHARS, DESCRIPTIONS, DemandGenAd,
    HEADLINE_CHARS, HEADLINES, LONG_HEADLINES, MAX_CARDS, MAX_IMAGES, MAX_VIDEOS, MIN_CARDS,
    VideoIn,
};
use crate::error::ToolError;
use crate::google::ctx::Ctx;

fn url_in(raw: &str, field: &str) -> Result<String, ToolError> {
    let u = raw.trim();
    final_url(u, field)?;
    Ok(u.to_string())
}

/// `call_to_action` must be a value from Google's fixed list (any locale); the
/// table's canonical form is sent.
fn cta_in(raw: &Option<String>, field: &str) -> Result<Option<String>, ToolError> {
    raw.as_deref()
        .map(|t| {
            cta::canonical(t).map(str::to_string).ok_or_else(|| {
                invalid(format!(
                    "{field}: '{}' is not in Google's list of button texts. {}",
                    t.trim(),
                    cta::options()
                ))
                .with_field(field)
            })
        })
        .transpose()
}

fn common(
    ctx: &Ctx,
    business: &str,
    logo: &Value,
    url: &str,
    f: &dyn Fn(&str) -> String,
) -> Result<Common, ToolError> {
    Ok(Common {
        business: bounded_text(business, BUSINESS_NAME_CHARS, &f("business_name"))?,
        logo: image_in(ctx, logo, ImageKind::Logo, f("logo"))?,
        url: url_in(url, &f("final_url"))?,
    })
}

fn video_in(ctx: &Ctx, raw: &str, field: &str) -> Result<VideoIn, ToolError> {
    let v = raw.trim();
    if v.starts_with("customers/") {
        return Ok(VideoIn::Asset(asset_rn(ctx, v, field)?));
    }
    Ok(VideoIn::Id(youtube_id_or_link(v, field)?))
}

/// Local validation of an ad (no images are loaded). `p` is the field prefix.
pub(super) fn prepare_ad(ctx: &Ctx, ad: &DemandGenAd, p: &str) -> Result<AdPlan, ToolError> {
    let f = |name: &str| format!("{p}.{name}");
    Ok(match ad {
        DemandGenAd::MultiAsset(a) => {
            let headlines = text_list(&a.headlines, HEADLINES, "headlines", &f("headlines"))?;
            let descriptions = text_list(
                &a.descriptions,
                DESCRIPTIONS,
                "descriptions",
                &f("descriptions"),
            )?;
            let c = common(ctx, &a.business_name, &a.logo, &a.final_url, &f)?;
            let i = &a.images;
            let images = vec![
                (
                    "marketingImages",
                    image_list(
                        ctx,
                        &i.landscape,
                        ImageKind::Landscape,
                        (1, MAX_IMAGES),
                        "1.91:1 images",
                        &f("images.landscape"),
                    )?,
                ),
                (
                    "squareMarketingImages",
                    image_list(
                        ctx,
                        &i.square,
                        ImageKind::Square,
                        (1, MAX_IMAGES),
                        "1:1 images",
                        &f("images.square"),
                    )?,
                ),
                (
                    "portraitMarketingImages",
                    image_list(
                        ctx,
                        &i.portrait,
                        ImageKind::Portrait,
                        (0, MAX_IMAGES),
                        "4:5 images",
                        &f("images.portrait"),
                    )?,
                ),
            ];
            let total: usize = images.iter().map(|(_, l)| l.len()).sum();
            if total > MAX_IMAGES {
                return Err(invalid(format!(
                    "{}: at most {MAX_IMAGES} images per ad (got {total}).",
                    f("images")
                ))
                .with_field(f("images")));
            }
            AdPlan::Multi {
                c,
                headlines,
                descriptions,
                images,
                cta: cta_in(&a.call_to_action, &f("call_to_action"))?,
            }
        }
        DemandGenAd::VideoResponsive(a) => {
            let headlines = text_list(&a.headlines, HEADLINES, "headlines", &f("headlines"))?;
            let long_headlines = text_list(
                &a.long_headlines,
                LONG_HEADLINES,
                "long headlines",
                &f("long_headlines"),
            )?;
            let descriptions = text_list(
                &a.descriptions,
                DESCRIPTIONS,
                "descriptions",
                &f("descriptions"),
            )?;
            let c = common(ctx, &a.business_name, &a.logo, &a.final_url, &f)?;
            count(a.videos.len(), 1, MAX_VIDEOS, "videos", &f("videos"))?;
            let mut videos: Vec<VideoIn> = Vec::new();
            for (i, raw) in a.videos.iter().enumerate() {
                let vf = f(&format!("videos[{i}]"));
                let v = video_in(ctx, raw, &vf)?;
                if videos.contains(&v) {
                    return Err(
                        invalid(format!("{vf}: the video appears more than once.")).with_field(vf)
                    );
                }
                videos.push(v);
            }
            AdPlan::Video {
                c,
                headlines,
                long_headlines,
                descriptions,
                videos,
            }
        }
        DemandGenAd::Carousel(a) => {
            let headline = bounded_text(&a.headline, HEADLINE_CHARS, &f("headline"))?;
            let description = bounded_text(&a.description, DESCRIPTION_CHARS, &f("description"))?;
            let c = common(ctx, &a.business_name, &a.logo, &a.final_url, &f)?;
            count(a.cards.len(), MIN_CARDS, MAX_CARDS, "cards", &f("cards"))?;
            let mut cards = Vec::new();
            for (k, card) in a.cards.iter().enumerate() {
                let cf = |name: &str| f(&format!("cards[{k}].{name}"));
                cards.push(CardPlan {
                    headline: bounded_text(&card.headline, HEADLINE_CHARS, &cf("headline"))?,
                    // The role (1.91:1 or 1:1) comes from the dimensions, at load time.
                    image: image_in(ctx, &card.image, ImageKind::Landscape, cf("image"))?,
                    url: url_in(&card.final_url, &cf("final_url"))?,
                    cta: cta_in(&card.call_to_action, &cf("call_to_action"))?,
                });
            }
            AdPlan::Carousel {
                c,
                headline,
                description,
                cards,
            }
        }
    })
}
