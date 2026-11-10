//! Demand Gen operations: ad group, criteria, new assets and ads.

use adsconsole_google_ads::images::ImageKind;
use adsconsole_google_ads::resources::TempIds;
use serde_json::{Value, json};

use super::super::create_common::{ImagePool, create_unnamed, geo_lang_criteria, text_assets};
use super::super::keywords::create_op;
use super::super::reports::invalid;
use super::{AdPlan, CardPlan, Common, GroupPlan, VideoIn};
use crate::error::ToolError;
use crate::google::ctx::Ctx;

/// Build state shared across the call: ids, operations, and the images and videos
/// already created (YouTube id → asset).
pub(super) struct Builder<'a> {
    pub ctx: &'a Ctx,
    pub temps: TempIds,
    /// Campaign, ad groups, criteria and cards (assets that reference other assets).
    pub ops: Vec<Value>,
    /// New images and videos: they go before everything (see [`Builder::finish`]).
    pub assets: Vec<Value>,
    pub pool: ImagePool,
    pub videos: Vec<(String, String)>,
    /// `adGroupAd` of every ad group: they go after every asset creation
    /// (see [`Builder::finish`]).
    pub ads: Vec<Value>,
}

impl Builder<'_> {
    async fn logo(&mut self, c: &Common) -> Result<String, ToolError> {
        let l = &c.logo;
        self.pool
            .resolve(
                self.ctx,
                &mut self.temps,
                &mut self.assets,
                &l.r,
                l.kind,
                &l.field,
            )
            .await
    }

    fn video(&mut self, v: &VideoIn) -> Result<String, ToolError> {
        let id = match v {
            VideoIn::Asset(rn) => return Ok(rn.clone()),
            VideoIn::Id(id) => id,
        };
        if let Some((_, rn)) = self.videos.iter().find(|(x, _)| x == id) {
            return Ok(rn.clone());
        }
        let rn = self.temps.next("assets");
        self.assets.push(create_op(
            &rn,
            json!({ "youtubeVideoAsset": { "youtubeVideoId": id } }),
        )?);
        self.videos.push((id.clone(), rn.clone()));
        Ok(rn)
    }

    /// Carousel cards: images (all in the same aspect ratio) and the card assets.
    async fn cards(&mut self, cards: &[CardPlan]) -> Result<Vec<Value>, ToolError> {
        let mut resolved = Vec::new();
        for card in cards {
            let img = &card.image;
            resolved.push(
                self.pool
                    .resolve_fit(
                        self.ctx,
                        &mut self.temps,
                        &mut self.assets,
                        &img.r,
                        &[ImageKind::Landscape, ImageKind::Square],
                        &img.field,
                    )
                    .await?,
            );
        }
        // An existing asset (unknown aspect ratio) follows the other cards.
        // Documented Google rule, confirmed only indirectly (2026-10-18).
        let kind = resolved
            .iter()
            .find_map(|(_, k)| *k)
            .unwrap_or(ImageKind::Landscape);
        let mut refs = Vec::new();
        let mut used: Vec<String> = Vec::new();
        for (card, (img_rn, k)) in cards.iter().zip(resolved) {
            // Google refuses the same image on two cards (same file, URL, asset or
            // content): `DUPLICATE_IMAGE_ACROSS_CAROUSEL_CARDS`.
            if used.contains(&img_rn) {
                let f = &card.image.field;
                return Err(invalid(format!(
                    "{f}: the image is repeated on another card; each carousel card needs a different image."
                ))
                .with_field(f.clone()));
            }
            used.push(img_rn.clone());
            if k.is_some_and(|k| k != kind) {
                let f = &card.image.field;
                return Err(invalid(format!(
                    "{f}: all carousel cards must use the same aspect ratio (1.91:1 or 1:1)."
                ))
                .with_field(f.clone()));
            }
            let key = if kind == ImageKind::Square {
                "squareMarketingImageAsset"
            } else {
                "marketingImageAsset"
            };
            let mut body = json!({ key: img_rn, "headline": card.headline });
            if let Some(cta) = &card.cta {
                body["callToActionText"] = json!(cta);
            }
            let rn = self.temps.next("assets");
            self.ops.push(create_op(
                &rn,
                json!({ "finalUrls": [card.url], "demandGenCarouselCardAsset": body }),
            )?);
            refs.push(json!({ "asset": rn }));
        }
        Ok(refs)
    }

    /// The `ad` field of the `adGroupAd`, creating the assets it uses first.
    async fn ad(&mut self, ad: &AdPlan) -> Result<Value, ToolError> {
        Ok(match ad {
            AdPlan::Multi {
                c,
                headlines,
                descriptions,
                images,
                cta,
            } => {
                let logo = self.logo(c).await?;
                let mut info = json!({
                    "headlines": text_assets(headlines),
                    "descriptions": text_assets(descriptions),
                    "businessName": c.business,
                    "logoImages": [{ "asset": logo }],
                });
                for (key, list) in images.iter().filter(|(_, l)| !l.is_empty()) {
                    info[*key] = self
                        .pool
                        .resolve_list(self.ctx, &mut self.temps, &mut self.assets, list)
                        .await?;
                }
                if let Some(cta) = cta {
                    info["callToActionText"] = json!(cta);
                }
                json!({ "finalUrls": [c.url], "demandGenMultiAssetAd": info })
            }
            AdPlan::Video {
                c,
                headlines,
                long_headlines,
                descriptions,
                videos,
            } => {
                let logo = self.logo(c).await?;
                let videos = videos
                    .iter()
                    .map(|v| Ok(json!({ "asset": self.video(v)? })))
                    .collect::<Result<Vec<_>, ToolError>>()?;
                // Verified on a real account (2026-10-18): business_name is an AdTextAsset in the video responsive ad.
                json!({ "finalUrls": [c.url], "demandGenVideoResponsiveAd": {
                    "headlines": text_assets(headlines),
                    "longHeadlines": text_assets(long_headlines),
                    "descriptions": text_assets(descriptions),
                    "videos": videos,
                    "logoImages": [{ "asset": logo }],
                    "businessName": { "text": c.business },
                } })
            }
            AdPlan::Carousel {
                c,
                headline,
                description,
                cards,
            } => {
                let logo = self.logo(c).await?;
                let cards = self.cards(cards).await?;
                json!({ "finalUrls": [c.url], "demandGenCarouselAd": {
                    "businessName": c.business,
                    "logoImage": { "asset": logo },
                    "headline": { "text": headline },
                    "description": { "text": description },
                    "carouselCards": cards,
                } })
            }
        })
    }

    /// Operations in send order: new images and videos, then campaign, ad groups and
    /// cards, and finally the ads.
    ///
    /// Google treats consecutive operations of the same type as one batch, and a
    /// temporary id created in a batch can only be referenced by operations in later
    /// batches (inferred from real account errors: a card created right after its image
    /// got `RESOURCE_NOT_FOUND`, and so did the ad that used the card). So an image and
    /// a card are never adjacent: the campaign and the ad group (other types) sit
    /// between them, and the ads, which reference assets and cards, go after all of them.
    pub(super) fn finish(self) -> Vec<Value> {
        let mut ops = self.assets;
        ops.extend(self.ops);
        ops.extend(self.ads);
        ops
    }

    pub(super) async fn group(
        &mut self,
        campaign_rn: &str,
        g: &GroupPlan,
    ) -> Result<(), ToolError> {
        let ag_rn = self.temps.next("adGroups");
        // Verified on a real account (2026-10-18): a Demand Gen ad group has no `type`.
        self.ops.push(create_op(
            &ag_rn,
            json!({ "name": g.name, "campaign": campaign_rn, "status": "ENABLED" }),
        )?);
        // Verified on a real account (2026-10-18): Demand Gen location and language are ad
        // group criteria (adGroupCriterion location/language).
        self.ops.extend(geo_lang_criteria(
            "adGroupCriterionOperation",
            "adGroup",
            &ag_rn,
            &g.locations,
            &g.languages,
            &format!("{}.", g.field),
        )?);
        for ad in &g.ads {
            let body = self.ad(ad).await?;
            self.ads.push(create_unnamed(
                "adGroupAdOperation",
                json!({ "adGroup": ag_rn, "status": "ENABLED", "ad": body }),
            ));
        }
        Ok(())
    }
}
