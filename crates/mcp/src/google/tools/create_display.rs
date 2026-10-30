//! `create_display_campaign`: uma campanha de Display completa num único
//! `googleAds:mutate` atômico, com ids temporários.
//!
//! Ordem das operações: orçamento (não compartilhado) → campanha (`DISPLAY`, pausada)
//! → local e idioma → por grupo: grupo (`DISPLAY_STANDARD`) → segmentação (públicos,
//! posicionamentos, tópicos) → por anúncio: imagens novas → anúncio responsivo de
//! Display.
//!
//! O Google tem recusado a criação de campanhas de Display pela API; essa recusa vira
//! `DISPLAY_NOT_AVAILABLE`, apontando para a Demand Gen ([`WriteTool::map_error`]).

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
/// Limites do anúncio responsivo de Display por tipo de imagem.
const MAX_MARKETING_IMAGES: usize = 15;
const MAX_LOGOS: usize = 5;
const NOT_AVAILABLE: &str = "O Google não permite mais criar campanhas de Display por aqui. \
     Use create_demand_gen_campaign.";
/// Códigos do Google que indicam a recusa da Display.
const REFUSAL_CODES: [&str; 2] = [
    "OPERATION_NOT_PERMITTED_FOR_CONTEXT",
    "CANNOT_CREATE_DISPLAY_CAMPAIGN",
];

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct TargetingIn {
    /// Ids de listas de público (`userLists`) desta conta.
    #[serde(default)]
    audiences: Vec<String>,
    /// Sites: URL `https://…` ou domínio (`exemplo.com.br`).
    #[serde(default)]
    placements: Vec<String>,
    /// Ids de tópico (`topicConstants`).
    #[serde(default)]
    topics: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RdaIn {
    /// 1 a 5 títulos de até 30 caracteres.
    #[serde(default)]
    headlines: Vec<String>,
    /// Título longo (até 90 caracteres).
    long_headline: String,
    /// 1 a 5 descrições de até 90 caracteres.
    #[serde(default)]
    descriptions: Vec<String>,
    /// Nome da empresa (até 25 caracteres).
    business_name: String,
    /// Imagens 1.91:1 (mínimo 600x314), de 1 a 15. Cada uma
    /// `{"file_path": "/abs/x.png"}`, `{"url": "https://…"}` ou `{"asset": "customers/…/assets/…"}`.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    marketing_images: Vec<Value>,
    /// Imagens 1:1 (mínimo 300x300), de 1 a 15.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    square_marketing_images: Vec<Value>,
    /// Logos 4:1 (mínimo 512x128), até 5.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    logo_images: Vec<Value>,
    /// Logos 1:1 (mínimo 128x128), até 5.
    #[serde(default)]
    #[schemars(with = "Vec<ImageRef>")]
    square_logo_images: Vec<Value>,
    /// URL final (`https://…`).
    final_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdGroupIn {
    name: String,
    /// Lance de CPC máximo do grupo, na moeda da conta. Só com `manual_cpc`.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
    /// Segmentação do grupo.
    #[serde(default)]
    targeting: Option<TargetingIn>,
    /// 1 a 5 anúncios responsivos de Display.
    #[serde(default)]
    ads: Vec<RdaIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateDisplayInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Orçamento diário, na moeda da conta.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Estratégia de lance; padrão `maximize_clicks`. Ou
    /// `{"maximize_conversions": {"target_cpa": 40}}` ou `"manual_cpc"`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// Ids de local (`geoTargetConstants`), ex.: "2076" (Brasil).
    #[serde(default)]
    locations: Vec<String>,
    /// Ids de idioma (`languageConstants`), ex.: "1014" (português).
    #[serde(default)]
    languages: Vec<String>,
    /// 1 a 20 grupos de anúncios.
    #[serde(default)]
    ad_groups: Vec<AdGroupIn>,
}

struct CreateDisplayCampaign;

#[async_trait]
impl WriteTool for CreateDisplayCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateDisplayInput>(
            CREATE_DISPLAY_CAMPAIGN,
            "Cria uma campanha de Display no Google Ads numa única operação atômica: \
             orçamento, campanha (nasce pausada), local, idioma e grupos de anúncios com \
             segmentação (públicos, posicionamentos, tópicos) e anúncios responsivos de \
             Display. Imagens vêm de arquivo local (caminho absoluto), URL https ou asset \
             existente. O Google pode recusar campanhas de Display novas pela API \
             (DISPLAY_NOT_AVAILABLE); nesse caso use create_demand_gen_campaign. Desfazer \
             remove a campanha e o orçamento; os assets criados continuam na conta.",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreateDisplayInput = parse_input(input)?;
        build(ctx, &input).await
    }

    fn map_error(&self, e: ToolError) -> ToolError {
        // "Talvez aplicado" (EXECUTION_FAILED) fica como veio, com a sugestão de
        // conferir na conta.
        if e.code == codes::EXECUTION_FAILED || !is_refusal(&e) {
            return e;
        }
        let mut out = ToolError::new(codes::DISPLAY_NOT_AVAILABLE, NOT_AVAILABLE)
            .with_suggestion("Use create_demand_gen_campaign para anúncios de imagem.");
        out.details = e.details;
        out
    }
}

/// Erro do Google que recusa a Display: no campo `advertising_channel_type` ou com um
/// dos [`REFUSAL_CODES`].
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
            invalid("A Display aceita maximize_clicks, maximize_conversions ou manual_cpc.")
                .with_field("bidding"),
        ),
    }
}

struct AdPlan {
    headlines: Vec<String>,
    long_headline: String,
    descriptions: Vec<String>,
    business: String,
    /// (chave da API, imagens).
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
                "imagens 1.91:1",
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
                "imagens 1:1",
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
                "logos 4:1",
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
                "logos 1:1",
                &f("square_logo_images"),
            )?,
        ),
    ];
    let url = ad.final_url.trim();
    final_url(url, &f("final_url"))?;
    Ok(AdPlan {
        headlines: text_list(&ad.headlines, HEADLINES, "títulos", &f("headlines"))?,
        long_headline: bounded_text(&ad.long_headline, LONG_HEADLINE_CHARS, &f("long_headline"))?,
        descriptions: text_list(
            &ad.descriptions,
            DESCRIPTIONS,
            "descrições",
            &f("descriptions"),
        )?,
        business: bounded_text(&ad.business_name, BUSINESS_NAME_CHARS, &f("business_name"))?,
        images,
        url: url.to_string(),
    })
}

/// Corpos dos critérios de segmentação do grupo (sem o pai). Erros em
/// `{p}.targeting.<tipo>[i]`.
fn targeting_criteria(ctx: &Ctx, t: &TargetingIn, p: &str) -> Result<Vec<Value>, ToolError> {
    let mut out = Vec::new();
    let kinds: [(&str, &str, &[String]); 3] = [
        ("audiences", "públicos", &t.audiences),
        ("placements", "posicionamentos", &t.placements),
        ("topics", "tópicos", &t.topics),
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
                return Err(invalid(format!("{f}: '{raw}' aparece mais de uma vez.")).with_field(f));
            }
            out.push(body);
        }
    }
    Ok(out)
}

async fn build(ctx: &Ctx, input: &CreateDisplayInput) -> Result<WritePlan, ToolError> {
    // 1. Validação local de tudo, antes de qualquer carga de imagem.
    let name = name_text(&input.name, "name")?;
    let bidding = display_bidding(input.bidding.as_ref())?;
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
        let cpc_micros = match group.cpc_bid {
            Some(_) if !bidding.is_manual() => {
                let f = format!("{p}.cpc_bid");
                return Err(invalid(format!(
                    "{f}: o lance por grupo só vale com a estratégia manual_cpc."
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
        count(group.ads.len(), 1, MAX_ADS, "anúncios", &format!("{p}.ads"))?;
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

    // 2. Grupos e anúncios, com as imagens carregadas.
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
            "Criar campanha Display '{name}' (pausada), {} ad groups, {ads} anúncios, \
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
        // Talvez aplicado: fica como veio, mesmo citando o canal.
        let mut maybe = api_error(json!([{ "code": "OPERATION_NOT_PERMITTED_FOR_CONTEXT",
                                           "field_path": "operations[1].create.advertising_channel_type" }]));
        maybe.code = codes::EXECUTION_FAILED.into();
        maybe.suggestion = Some("confira na conta".into());
        let kept = CreateDisplayCampaign.map_error(maybe);
        assert_eq!(kept.code, codes::EXECUTION_FAILED);
        assert_eq!(kept.suggestion.as_deref(), Some("confira na conta"));
        let local = ToolError::new(codes::INVALID_INPUT, "x");
        assert_eq!(
            CreateDisplayCampaign.map_error(local).code,
            codes::INVALID_INPUT
        );
    }
}
