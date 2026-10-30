//! `create_demand_gen_campaign`: uma campanha Demand Gen completa num único
//! `googleAds:mutate` atômico, com ids temporários.
//!
//! Ordem das operações: orçamento (não compartilhado) → campanha (`DEMAND_GEN`,
//! pausada) → por grupo: grupo → local e idioma do grupo → por anúncio: imagens,
//! vídeos e cards novos → `adGroupAd` (multi-asset, vídeo responsivo ou carrossel).
//!
//! Toda a validação local roda antes de carregar qualquer imagem; os mesmos bytes
//! viram um asset só ([`ImagePool`]) e o mesmo vídeo também. O orçamento mínimo não é
//! conferido aqui: vem do `validateOnly` (`BUDGET_TOO_LOW`). A inversa remove a
//! campanha e depois o orçamento; os assets ficam na conta.

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
/// (mínimo, máximo, caracteres) dos textos.
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
    /// Imagens 1.91:1 (mínimo 600x314), ao menos uma.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    landscape: Vec<Value>,
    /// Imagens 1:1 (mínimo 300x300), ao menos uma.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    square: Vec<Value>,
    /// Imagens 4:5 (mínimo 480x600).
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    portrait: Vec<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MultiAssetIn {
    /// 1 a 5 títulos de até 40 caracteres.
    #[serde(default)]
    headlines: Vec<String>,
    /// 1 a 5 descrições de até 90 caracteres.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Nome da empresa (até 25 caracteres).
    business_name: String,
    /// Logo 1:1 (mínimo 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// Imagens: cada uma `{"file_path": "/abs/x.png"}`, `{"url": "https://…"}` ou
    /// `{"asset": "customers/…/assets/…"}` (png, jpeg ou gif, até 5 MB).
    #[serde(default)]
    images: ImagesIn,
    /// URL final (`https://…`).
    final_url: String,
    /// Texto do botão, da lista fixa do Google (ex.: "Comprar agora", "Saber mais",
    /// "Visitar site", "Inscrever-se", "Fale conosco", "Instalar", "Reservar agora",
    /// "Fazer download", "Obter cotação", "Ver mais"; também vale o inglês, como "Shop
    /// Now"). Outro texto é recusado.
    #[serde(default)]
    call_to_action: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct VideoResponsiveIn {
    /// 1 a 5 títulos de até 40 caracteres.
    #[serde(default)]
    headlines: Vec<String>,
    /// 1 a 5 títulos longos de até 90 caracteres.
    #[serde(default)]
    long_headlines: Vec<String>,
    /// 1 a 5 descrições de até 90 caracteres.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Nome da empresa (até 25 caracteres).
    business_name: String,
    /// Logo 1:1 (mínimo 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// 1 a 5 vídeos: id do YouTube (11 caracteres) ou asset de vídeo existente
    /// (`customers/…/assets/…`).
    #[serde(default)]
    videos: Vec<String>,
    /// URL final (`https://…`).
    final_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CardIn {
    /// Título do card (até 40 caracteres).
    headline: String,
    /// Imagem 1.91:1 ou 1:1, a mesma proporção em todos os cards.
    #[schemars(with = "ImageRef")]
    image: Value,
    /// URL final do card (`https://…`).
    final_url: String,
    /// Texto do botão do card, da lista fixa do Google (as mesmas opções do anúncio).
    #[serde(default)]
    call_to_action: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CarouselIn {
    /// Título (até 40 caracteres).
    headline: String,
    /// Descrição (até 90 caracteres).
    description: String,
    /// Nome da empresa (até 25 caracteres).
    business_name: String,
    /// Logo 1:1 (mínimo 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// 2 a 10 cards.
    #[serde(default)]
    cards: Vec<CardIn>,
    /// URL final (`https://…`).
    final_url: String,
}

/// Anúncio Demand Gen: `{"multi_asset": {…}}`, `{"video_responsive": {…}}` ou
/// `{"carousel": {…}}`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DemandGenAd {
    /// Imagens, títulos e descrições combinados pelo Google.
    MultiAsset(MultiAssetIn),
    /// Vídeos do YouTube com títulos e descrições.
    VideoResponsive(VideoResponsiveIn),
    /// Carrossel de 2 a 10 cards.
    Carousel(CarouselIn),
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdGroupIn {
    name: String,
    /// Ids de local (`geoTargetConstants`) do grupo, ex.: "2076" (Brasil).
    #[serde(default)]
    locations: Vec<String>,
    /// Ids de idioma (`languageConstants`) do grupo, ex.: "1014" (português).
    #[serde(default)]
    languages: Vec<String>,
    /// 1 a 5 anúncios.
    #[serde(default)]
    ads: Vec<DemandGenAd>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateDemandGenInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Orçamento diário, na moeda da conta (o mínimo da conta vem do Google).
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Estratégia de lance; padrão `maximize_clicks`. Ou
    /// `{"maximize_conversions": {"target_cpa": 40}}`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// 1 a 10 grupos de anúncios (local e idioma ficam no grupo).
    #[serde(default)]
    ad_groups: Vec<AdGroupIn>,
}

struct CreateDemandGenCampaign;

#[async_trait]
impl WriteTool for CreateDemandGenCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateDemandGenInput>(
            CREATE_DEMAND_GEN_CAMPAIGN,
            "Cria uma campanha Demand Gen completa no Google Ads numa única operação \
             atômica: orçamento, campanha (nasce pausada) e de 1 a 10 grupos de anúncios, \
             cada um com local, idioma e de 1 a 5 anúncios (multi_asset, video_responsive \
             ou carousel). Caminho oficial para anúncios de vídeo, imagem e carrossel; a \
             API não cria campanhas do canal VIDEO. Imagens vêm de arquivo local (caminho \
             absoluto), URL https ou asset existente (png, jpeg ou gif, até 5 MB). \
             Desfazer remove a campanha e o orçamento; os assets criados continuam na \
             conta (o Google não remove assets).",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreateDemandGenInput = parse_input(input)?;
        build(ctx, &input).await
    }
}

/// Estratégia da Demand Gen: padrão `maximize_clicks`; ou `maximize_conversions`.
fn dg_bidding(v: Option<&Value>) -> Result<Bidding, ToolError> {
    match parse_bidding(v)? {
        b @ (Bidding::MaximizeClicks { .. } | Bidding::MaximizeConversions { .. }) => Ok(b),
        _ => Err(
            invalid("A Demand Gen aceita só maximize_clicks ou maximize_conversions.")
                .with_field("bidding"),
        ),
    }
}

/// Vídeo do anúncio: id do YouTube (vira asset novo) ou asset existente.
#[derive(PartialEq)]
enum VideoIn {
    Id(String),
    Asset(String),
}

/// Partes comuns aos três formatos.
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
        /// (chave da API, imagens): 1.91:1, 1:1 e 4:5.
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
    /// Imagens do anúncio, na ordem em que são resolvidas.
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
    // 1. Validação local de tudo, antes de qualquer carga de imagem.
    let name = name_text(&input.name, "name")?;
    let bidding = dg_bidding(input.bidding.as_ref())?;
    count(
        input.ad_groups.len(),
        1,
        MAX_AD_GROUPS,
        "grupos de anúncios",
        "ad_groups",
    )?;
    let mut groups: Vec<GroupPlan> = Vec::new();
    for (g, group) in input.ad_groups.iter().enumerate() {
        let p = format!("ad_groups[{g}]");
        let nf = format!("{p}.name");
        let n = name_text(&group.name, &nf)?;
        if groups.iter().any(|x| x.name == n) {
            return Err(invalid(format!(
                "{nf}: já existe um grupo chamado '{n}' nesta campanha."
            ))
            .with_field(nf));
        }
        criterion_ids(&group.locations, &format!("{p}.locations"))?;
        criterion_ids(&group.languages, &format!("{p}.languages"))?;
        count(group.ads.len(), 1, MAX_ADS, "anúncios", &format!("{p}.ads"))?;
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

    // 2. Grupos e anúncios, com as imagens carregadas.
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
            "Criar campanha Demand Gen '{name}' (pausada), {} ad groups, {ads} anúncios, \
             orçamento {} {}/dia; imagens de {}",
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
