//! Operações da Demand Gen: grupo, critérios, assets novos e anúncios.

use adsconsole_google_ads::images::ImageKind;
use adsconsole_google_ads::resources::TempIds;
use serde_json::{Value, json};

use super::super::create_common::{ImagePool, create_unnamed, geo_lang_criteria, text_assets};
use super::super::keywords::create_op;
use super::super::reports::invalid;
use super::{AdPlan, CardPlan, Common, GroupPlan, VideoIn};
use crate::error::ToolError;
use crate::google::ctx::Ctx;

/// Estado da montagem compartilhado pela chamada: ids, operações, imagens e vídeos
/// já criados (id do YouTube → asset).
pub(super) struct Builder<'a> {
    pub ctx: &'a Ctx,
    pub temps: TempIds,
    /// Campanha, grupos, critérios e cards (assets que referenciam outros assets).
    pub ops: Vec<Value>,
    /// Imagens e vídeos novos: vão antes de tudo (ver [`Builder::finish`]).
    pub assets: Vec<Value>,
    pub pool: ImagePool,
    pub videos: Vec<(String, String)>,
    /// `adGroupAd` de todos os grupos: vão depois de qualquer criação de asset
    /// (ver [`Builder::finish`]).
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

    /// Cards do carrossel: imagens (todas na mesma proporção) e os assets de card.
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
        // Asset existente (proporção desconhecida) segue os outros cards.
        // Regra documentada do Google, confirmada só indiretamente (T15.5, 2026-10-18).
        let kind = resolved
            .iter()
            .find_map(|(_, k)| *k)
            .unwrap_or(ImageKind::Landscape);
        let mut refs = Vec::new();
        let mut used: Vec<String> = Vec::new();
        for (card, (img_rn, k)) in cards.iter().zip(resolved) {
            // O Google recusa a mesma imagem em dois cards (mesmo arquivo, URL, asset ou
            // conteúdo): `DUPLICATE_IMAGE_ACROSS_CAROUSEL_CARDS`.
            if used.contains(&img_rn) {
                let f = &card.image.field;
                return Err(invalid(format!(
                    "{f}: a imagem se repete em outro card; cada card do carrossel precisa de uma imagem diferente."
                ))
                .with_field(f.clone()));
            }
            used.push(img_rn.clone());
            if k.is_some_and(|k| k != kind) {
                let f = &card.image.field;
                return Err(invalid(format!(
                    "{f}: todos os cards do carrossel usam a mesma proporção (1.91:1 ou 1:1)."
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

    /// Campo `ad` do `adGroupAd`, criando antes os assets que ele usa.
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
                // Confirmado na conta real (2026-10-18, T15.3): business_name é AdTextAsset no vídeo responsivo.
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

    /// Operações na ordem de envio: imagens e vídeos novos, depois campanha, grupos e
    /// cards, e por fim os anúncios.
    ///
    /// O Google trata operações seguidas do mesmo tipo como um lote, e um temporário
    /// criado num lote só pode ser referenciado por operações de lotes seguintes
    /// (inferido dos erros reais da conta: card criado logo depois da imagem dele dava
    /// `RESOURCE_NOT_FOUND`, assim como o anúncio que usava o card). Por isso imagem e
    /// card nunca ficam colados: a campanha e o grupo (outros tipos) ficam entre eles, e
    /// os anúncios, que referenciam assets e cards, vão depois de todos.
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
        // Confirmado na conta real (2026-10-18, T15.2): grupo de Demand Gen sem `type`.
        self.ops.push(create_op(
            &ag_rn,
            json!({ "name": g.name, "campaign": campaign_rn, "status": "ENABLED" }),
        )?);
        // Confirmado na conta real (2026-10-18, T15.1): local e idioma da Demand Gen como critérios do grupo
        // (adGroupCriterion location/language).
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
