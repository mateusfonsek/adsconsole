//! `create_pmax_campaign`: uma campanha Performance Max completa num único
//! `googleAds:mutate` atômico, com ids temporários.
//!
//! Ordem das operações: orçamento (não compartilhado) → campanha (pausada, com
//! diretrizes de marca) → local e idioma → marca (nome da empresa, logo e logo 4:1,
//! ligados por `campaignAsset`) → por grupo de recursos: assets de texto, imagem e
//! vídeo → `assetGroup` → `assetGroupAsset` por tipo → `assetGroupSignal`.
//!
//! Toda a validação local (textos, URLs, vídeos, sinais, contagens, referências de
//! imagem) roda antes de carregar qualquer imagem. As imagens são carregadas
//! (arquivo ou URL) e conferidas a cada `plan`; os mesmos bytes viram um asset só
//! ([`ImagePool`]) e o mesmo vídeo também. A inversa remove a campanha e depois o
//! orçamento; os assets ficam na conta (o Google não remove assets).

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
/// Ao menos um título com até 15 caracteres e uma descrição com até 60.
const SHORT_HEADLINE: usize = 15;
const SHORT_DESCRIPTION: usize = 60;
/// Limites do Google por grupo de recursos.
const MAX_IMAGES: usize = 20;
const MAX_VIDEOS: usize = 5;
const MAX_SEARCH_THEMES: usize = 25;
const URL_EXPANSION: &str = "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION";
#[derive(Debug, Deserialize, JsonSchema)]
struct BrandIn {
    /// Nome da empresa (até 25 caracteres).
    business_name: String,
    /// Logo quadrado 1:1 (mínimo 128x128).
    #[schemars(with = "ImageRef")]
    logo: Value,
    /// Logo horizontal 4:1 (mínimo 512x128).
    #[serde(default)]
    #[schemars(with = "Option<ImageRef>")]
    landscape_logo: Option<Value>,
}

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

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct SignalsIn {
    /// Nomes de público (`customers/123/audiences/456`).
    #[serde(default)]
    audiences: Vec<String>,
    /// Até 25 temas de busca (até 80 caracteres cada).
    #[serde(default)]
    search_themes: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AssetGroupIn {
    name: String,
    /// URLs finais (ao menos uma).
    #[serde(default)]
    final_urls: Vec<String>,
    /// 3 a 15 títulos de até 30 caracteres; ao menos um com até 15.
    #[serde(default)]
    headlines: Vec<String>,
    /// 1 a 5 títulos longos de até 90 caracteres.
    #[serde(default)]
    long_headlines: Vec<String>,
    /// 2 a 5 descrições de até 90 caracteres; ao menos uma com até 60.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Imagens: cada uma `{"file_path": "/abs/x.png"}`, `{"url": "https://…"}` ou
    /// `{"asset": "customers/…/assets/…"}` (png, jpeg ou gif, até 5 MB).
    #[serde(default)]
    images: ImagesIn,
    /// Até 5 ids de vídeo do YouTube (11 caracteres, não a URL).
    #[serde(default)]
    youtube_video_ids: Vec<String>,
    /// Sinais de público e temas de busca.
    #[serde(default)]
    signals: Option<SignalsIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreatePmaxInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Orçamento diário, na moeda da conta.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Estratégia de lance; padrão `maximize_conversions`.
    /// `{"maximize_conversions": {"target_cpa": 40}}` ou
    /// `{"maximize_conversion_value": {"target_roas": 3.5}}`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// Deixar o Google trocar a URL final e gerar textos a partir do site (padrão false).
    #[serde(default)]
    final_url_expansion: bool,
    /// Ids de local (`geoTargetConstants`), ex.: "2076" (Brasil).
    #[serde(default)]
    locations: Vec<String>,
    /// Ids de idioma (`languageConstants`), ex.: "1014" (português).
    #[serde(default)]
    languages: Vec<String>,
    /// Marca da campanha: nome da empresa e logos.
    brand: BrandIn,
    /// 1 a 5 grupos de recursos.
    #[serde(default)]
    asset_groups: Vec<AssetGroupIn>,
}

struct CreatePmaxCampaign;

#[async_trait]
impl WriteTool for CreatePmaxCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreatePmaxInput>(
            CREATE_PMAX_CAMPAIGN,
            "Cria uma campanha Performance Max completa no Google Ads numa única operação \
             atômica: orçamento, campanha (nasce pausada), local, idioma, marca (nome da \
             empresa e logos) e de 1 a 5 grupos de recursos com títulos, descrições, \
             imagens, vídeos e sinais. Imagens vêm de arquivo local (caminho absoluto), URL \
             https ou asset existente (png, jpeg ou gif, até 5 MB). A Performance Max exige \
             acompanhamento de conversões configurado na conta (lance maximize_conversions \
             ou maximize_conversion_value). Desfazer remove a campanha e o orçamento; os \
             assets criados continuam na conta (o Google não remove assets).",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreatePmaxInput = parse_input(input)?;
        build(ctx, &input).await
    }
}

/// Estratégia da PMax: padrão `maximize_conversions`; só as de conversão valem.
fn pmax_bidding(v: Option<&Value>) -> Result<Bidding, ToolError> {
    if v.is_none_or(Value::is_null) {
        return Ok(Bidding::MaximizeConversions { target_cpa: None });
    }
    match parse_bidding(v)? {
        b @ (Bidding::MaximizeConversions { .. } | Bidding::MaximizeConversionValue { .. }) => {
            Ok(b)
        }
        _ => Err(invalid(
            "A Performance Max aceita só maximize_conversions ou maximize_conversion_value.",
        )
        .with_field("bidding")),
    }
}

/// Textos de um tipo do grupo: quantidade e tamanho de [`FieldType`], sem repetição
/// e, com `short`, ao menos um de até `short` caracteres.
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
            return Err(invalid(format!("{f}: '{t}' aparece mais de uma vez.")).with_field(f));
        }
        out.push(t);
    }
    if let Some(n) = short
        && !out.iter().any(|t| t.chars().count() <= n)
    {
        return Err(invalid(format!(
            "{field}: informe ao menos um item com até {n} caracteres."
        ))
        .with_field(field));
    }
    Ok(out)
}

/// Um grupo de recursos validado, pronto para virar operações.
struct GroupPlan {
    name: String,
    urls: Vec<String>,
    texts: Vec<(FieldType, Vec<String>)>,
    images: Vec<(FieldType, ImageIn)>,
    videos: Vec<String>,
    audiences: Vec<String>,
    themes: Vec<String>,
}

/// Validação local de um grupo (sem carregar imagens).
fn prepare_group(
    ctx: &Ctx,
    group: &AssetGroupIn,
    g: usize,
    name: String,
) -> Result<GroupPlan, ToolError> {
    let field = |name: &str| format!("asset_groups[{g}].{name}");

    if group.final_urls.is_empty() {
        return Err(invalid(format!(
            "{}: informe ao menos uma URL final.",
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
        "imagens 1.91:1",
        &field("images.landscape"),
    )?;
    count(
        imgs.square.len(),
        1,
        MAX_IMAGES,
        "imagens 1:1",
        &field("images.square"),
    )?;
    let total = imgs.landscape.len() + imgs.square.len() + imgs.portrait.len();
    if total > MAX_IMAGES {
        return Err(invalid(format!(
            "{}: no máximo {MAX_IMAGES} imagens por grupo (vieram {total}).",
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
                    invalid(format!("{}: a imagem aparece mais de uma vez.", img.field))
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
        "vídeos",
        &field("youtube_video_ids"),
    )?;
    let mut videos: Vec<String> = Vec::new();
    for (i, v) in group.youtube_video_ids.iter().enumerate() {
        let f = field(&format!("youtube_video_ids[{i}]"));
        let id = youtube_id_or_link(v, &f)?;
        if videos.contains(&id) {
            return Err(
                invalid(format!("{f}: o vídeo {id} aparece mais de uma vez.")).with_field(f),
            );
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
            return Err(invalid(format!("{f}: {rn} aparece mais de uma vez.")).with_field(f));
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
        "temas de busca",
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

/// Operações de um grupo: assets novos → grupo → vínculos → sinais. `videos` guarda
/// os vídeos já criados nesta chamada (id → asset), para reaproveitar entre grupos.
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
        // Referências diferentes com os mesmos bytes no mesmo tipo.
        if links.iter().any(|(a, t)| *a == rn && t == ft) {
            return Err(invalid(format!(
                "{}: a mesma imagem aparece mais de uma vez.",
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
    // 1. Validação local de tudo, antes de qualquer carga de imagem.
    let name = name_text(&input.name, "name")?;
    let bidding = pmax_bidding(input.bidding.as_ref())?;
    count(
        input.asset_groups.len(),
        1,
        MAX_ASSET_GROUPS,
        "grupos de recursos",
        "asset_groups",
    )?;
    let mut groups: Vec<GroupPlan> = Vec::new();
    for (g, group) in input.asset_groups.iter().enumerate() {
        let f = format!("asset_groups[{g}].name");
        let n = name_text(&group.name, &f)?;
        if groups.iter().any(|p| p.name == n) {
            return Err(invalid(format!(
                "{f}: já existe um grupo chamado '{n}' nesta campanha."
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
    // Nome e logos da marca vão na campanha (campaignAsset), não nos grupos.
    campaign["brandGuidelinesEnabled"] = json!(true);
    // Confirmado na conta real (2026-10-18, T14.1): Campaign.asset_automation_settings substitui o url_expansion_opt_out.
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

    // 2. Marca e grupos, com as imagens carregadas.
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
            "Criar campanha Performance Max '{name}' (pausada), {} asset groups, \
             orçamento {} {}/dia; imagens de {}",
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
