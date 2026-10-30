//! `create_search_campaign`: uma campanha de Pesquisa completa num único
//! `googleAds:mutate` atômico, com ids temporários.
//!
//! Ordem das operações: orçamento → campanha (pausada) → critérios de campanha (local,
//! idioma, negativas) → sitelinks → por grupo: grupo → keywords → negativas → RSAs.
//! A inversa remove a campanha e depois o orçamento; os filhos somem com a campanha.
//! Grupos, keywords e anúncios nascem ativos: a campanha pausada já impede a veiculação.

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
    /// Texto (até 80 caracteres e 10 palavras).
    #[serde(default)]
    text: Option<String>,
    /// `EXACT`, `PHRASE` ou `BROAD` (minúsculas também valem).
    #[serde(default)]
    #[schemars(with = "Option<super::keywords::MatchType>")]
    match_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RsaIn {
    /// 3 a 15 títulos de até 30 caracteres.
    headlines: Vec<String>,
    /// 2 a 4 descrições de até 90 caracteres.
    descriptions: Vec<String>,
    /// URL final (`https://…`).
    final_url: String,
    /// Caminho 1 da URL exibida (até 15 caracteres).
    #[serde(default)]
    path1: Option<String>,
    /// Caminho 2 da URL exibida (até 15 caracteres).
    #[serde(default)]
    path2: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdGroupIn {
    name: String,
    /// Lance de CPC máximo do grupo, na moeda da conta. Só com `manual_cpc`.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
    /// 1 a 200 keywords.
    keywords: Vec<KeywordSpec>,
    /// Keywords negativas do grupo.
    #[serde(default)]
    negatives: Vec<KeywordSpec>,
    /// 1 a 3 anúncios responsivos de pesquisa.
    ads: Vec<RsaIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSearchInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    name: String,
    /// Orçamento diário, na moeda da conta.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Estratégia de lance; padrão `maximize_clicks` sem limite de CPC.
    /// `{"maximize_clicks": {"cpc_cap": 2.5}}`, `{"maximize_conversions": {"target_cpa": 40}}`,
    /// `{"maximize_conversion_value": {"target_roas": 3.5}}` ou `"manual_cpc"`.
    #[serde(default)]
    #[schemars(with = "Option<Bidding>")]
    bidding: Option<Value>,
    /// Exibir também nos parceiros de pesquisa (padrão false).
    #[serde(default)]
    search_partners: bool,
    /// Exibir também na Rede de Display (padrão false).
    #[serde(default)]
    display_network: bool,
    /// Ids de local (`geoTargetConstants`), ex.: "2076" (Brasil).
    #[serde(default)]
    locations: Vec<String>,
    /// Ids de idioma (`languageConstants`), ex.: "1014" (português).
    #[serde(default)]
    languages: Vec<String>,
    /// AAAA-MM-DD.
    #[serde(default)]
    start_date: Option<String>,
    /// AAAA-MM-DD.
    #[serde(default)]
    end_date: Option<String>,
    /// Keywords negativas da campanha.
    #[serde(default)]
    negatives: Vec<KeywordSpec>,
    /// Até 20 sitelinks da campanha.
    #[serde(default)]
    sitelinks: Vec<SitelinkIn>,
    /// 1 a 20 grupos de anúncios.
    ad_groups: Vec<AdGroupIn>,
}

struct CreateSearchCampaign;

#[async_trait]
impl WriteTool for CreateSearchCampaign {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<CreateSearchInput>(
            CREATE_SEARCH_CAMPAIGN,
            "Cria uma campanha de Pesquisa completa no Google Ads numa única operação \
             atômica: orçamento, campanha (nasce pausada), local, idioma, negativas, \
             sitelinks e grupos de anúncios com keywords e anúncios responsivos. Desfazer \
             remove a campanha e o orçamento; os assets dos sitelinks continuam na conta \
             (o Google não remove assets).",
            ToolGroup::Create,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: CreateSearchInput = parse_input(input)?;
        build(ctx, &input)
    }
}

/// Critérios de keyword (positivas ou negativas) de `parent_field` = `parent_rn`.
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
        "grupos de anúncios",
        "ad_groups",
    )?;
    count(
        input.negatives.len(),
        0,
        MAX_NEGATIVES,
        "negativas",
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
                "{}: já existe um grupo chamado '{group_name}' nesta campanha.",
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
            "negativas",
            &field("negatives"),
        )?;
        count(group.ads.len(), 1, MAX_ADS, "anúncios", &field("ads"))?;

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
                    "{}: o lance por grupo só vale com a estratégia manual_cpc.",
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
            "Criar campanha Search '{name}' (pausada), {} ad groups, {keywords} keywords, \
             orçamento {} {}/dia",
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
