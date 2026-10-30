//! Leituras de apoio: `get_pages`, `search_targeting`, `estimate_audience`,
//! `search_ad_library`.

use adsconsole_core::config::ToolGroup;
use adsconsole_core::secrets::Secret;
use adsconsole_meta_ads::error::MetaError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MTool, ReadTool, parse_input, without_token};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::errors::translate;

pub(crate) fn tools() -> Vec<MTool> {
    vec![
        MTool::Read(Box::new(GetPages)),
        MTool::Read(Box::new(GetPagePosts)),
        MTool::Read(Box::new(SearchTargeting)),
        MTool::Read(Box::new(EstimateAudience)),
        MTool::Read(Box::new(SearchAdLibrary)),
    ]
}

fn invalid(field: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PagesInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
}

struct GetPages;

#[async_trait::async_trait]
impl ReadTool for GetPages {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<PagesInput>(
            "get_pages",
            "Lista as Páginas do Facebook (e as contas do Instagram ligadas a elas) que a \
             conexão pode usar nos criativos.",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, _input: &Value) -> Result<Value, ToolError> {
        let page = ctx
            .client
            .get_all(
                "me/accounts",
                &[
                    (
                        "fields",
                        "id,name,category,instagram_business_account{id,username},tasks".into(),
                    ),
                    ("limit", "100".into()),
                ],
                1000,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), "get_pages"))?;
        let pages: Vec<Value> = page
            .data
            .iter()
            .map(|p| {
                let ig = &p["instagram_business_account"];
                json!({
                    "page_id": p["id"],
                    "name": p["name"],
                    "category": p["category"],
                    "instagram": if ig.is_object() {
                        json!({ "id": ig["id"], "username": ig["username"] })
                    } else {
                        Value::Null
                    },
                    "can_advertise": p["tasks"].as_array().is_some_and(|t| t.iter().any(|x| x == "ADVERTISE")),
                })
            })
            .collect();
        Ok(json!({ "pages": pages }))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PagePostsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Página (de get_pages).
    page_id: String,
    /// Máximo de posts (padrão 25).
    #[serde(default)]
    limit: Option<usize>,
}

struct GetPagePosts;

#[async_trait::async_trait]
impl ReadTool for GetPagePosts {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<PagePostsInput>(
            "get_page_posts",
            "Lista os posts publicados de uma Página, com o object_story_id para anunciar um \
             post existente (criativo existing_post).",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: PagePostsInput = parse_input(input)?;
        let tool = "get_page_posts";
        let pages = ctx
            .client
            .get_all(
                "me/accounts",
                &[
                    ("fields", "id,access_token".into()),
                    ("limit", "100".into()),
                ],
                1000,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
        // O token da Página só é usado aqui; nunca sai no resultado.
        let token = pages
            .data
            .iter()
            .find(|p| p["id"].as_str() == Some(i.page_id.as_str()))
            .and_then(|p| p["access_token"].as_str())
            .ok_or_else(|| {
                ToolError::new(
                    codes::NOT_FOUND,
                    format!(
                        "A Página {} não está entre as Páginas desta conexão.",
                        i.page_id
                    ),
                )
                .with_field("page_id")
            })?;
        let limit = i.limit.unwrap_or(25).clamp(1, 100);
        let page = ctx
            .client
            .with_token(Secret::new(token))
            .get_all(
                &format!("{}/published_posts", i.page_id),
                &[
                    (
                        "fields",
                        "id,message,created_time,permalink_url,is_eligible_for_promotion".into(),
                    ),
                    ("limit", limit.to_string()),
                ],
                limit,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
        let posts: Vec<Value> = page
            .data
            .iter()
            .map(|p| {
                json!({
                    "object_story_id": p["id"],
                    "message": p["message"],
                    "created_time": p["created_time"],
                    "permalink_url": p["permalink_url"],
                    "can_promote": p["is_eligible_for_promotion"],
                })
            })
            .collect();
        Ok(json!({ "posts": posts }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TargetingType {
    Interest,
    Behavior,
    Demographic,
    Geo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum GeoType {
    Country,
    Region,
    City,
    Zip,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TargetingInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// interest, behavior, demographic ou geo.
    #[serde(rename = "type")]
    kind: TargetingType,
    /// Texto da busca (obrigatório em interest e geo; filtra behavior e demographic).
    #[serde(default)]
    query: Option<String>,
    /// Só em geo: country, region, city, zip (padrão: os quatro).
    #[serde(default)]
    geo_types: Vec<GeoType>,
    /// Máximo de resultados (padrão 25).
    #[serde(default)]
    limit: Option<usize>,
}

struct SearchTargeting;

#[async_trait::async_trait]
impl ReadTool for SearchTargeting {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<TargetingInput>(
            "search_targeting",
            "Busca opções de segmentação: interesses, comportamentos, dados demográficos ou \
             locais (ids para usar em targeting).",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: TargetingInput = parse_input(input)?;
        let limit = i.limit.unwrap_or(25).clamp(1, 500);
        let q = i.query.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let mut query: Vec<(&str, String)> = Vec::new();
        match i.kind {
            TargetingType::Interest | TargetingType::Geo => {
                let Some(q) = q else {
                    return Err(invalid("query", "Informe o texto da busca."));
                };
                query.push(("q", q.to_string()));
                if i.kind == TargetingType::Interest {
                    query.push(("type", "adinterest".into()));
                } else {
                    query.push(("type", "adgeolocation".into()));
                    let types: Vec<&str> = if i.geo_types.is_empty() {
                        vec!["country", "region", "city", "zip"]
                    } else {
                        i.geo_types
                            .iter()
                            .map(|t| match t {
                                GeoType::Country => "country",
                                GeoType::Region => "region",
                                GeoType::City => "city",
                                GeoType::Zip => "zip",
                            })
                            .collect()
                    };
                    query.push((
                        "location_types",
                        serde_json::to_string(&types).unwrap_or_default(),
                    ));
                }
                query.push(("limit", limit.to_string()));
            }
            TargetingType::Behavior | TargetingType::Demographic => {
                query.push(("type", "adTargetingCategory".into()));
                let class = if i.kind == TargetingType::Behavior {
                    "behaviors"
                } else {
                    "demographics"
                };
                query.push(("class", class.into()));
            }
        }
        let body = ctx
            .client
            .get("search", &query)
            .await
            .map_err(|e| translate(e, Some(ctx), "search_targeting"))?;
        let needle = q.map(str::to_lowercase);
        let results: Vec<Value> = body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| match (&needle, i.kind) {
                (Some(n), TargetingType::Behavior | TargetingType::Demographic) => r["name"]
                    .as_str()
                    .is_some_and(|name| name.to_lowercase().contains(n)),
                _ => true,
            })
            .take(limit)
            .map(|r| {
                json!({
                    "id": r["id"],
                    "key": r["key"],
                    "name": r["name"],
                    "type": r["type"],
                    "audience_size_lower": r["audience_size_lower_bound"],
                    "audience_size_upper": r["audience_size_upper_bound"],
                    "path": r["path"],
                    "country_code": r["country_code"],
                    "region": r["region"],
                })
            })
            .collect();
        Ok(json!({ "results": results }))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AudienceInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// targeting_spec da Meta (geo_locations, idade, gênero, interesses...).
    targeting: Value,
    /// Meta de otimização da estimativa (padrão REACH).
    #[serde(default)]
    optimization_goal: Option<String>,
}

struct EstimateAudience;

#[async_trait::async_trait]
impl ReadTool for EstimateAudience {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<AudienceInput>(
            "estimate_audience",
            "Estima quantas pessoas um público alcança (faixa de usuários por mês).",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: AudienceInput = parse_input(input)?;
        if !i.targeting["geo_locations"].is_object() {
            return Err(invalid(
                "targeting",
                "Informe ao menos um local em targeting.geo_locations.",
            ));
        }
        let body = ctx
            .client
            .get(
                &format!("{}/delivery_estimate", ctx.account_id),
                &[
                    ("targeting_spec", i.targeting.to_string()),
                    (
                        "optimization_goal",
                        i.optimization_goal.unwrap_or_else(|| "REACH".into()),
                    ),
                ],
            )
            .await
            .map_err(|e| translate(e, Some(ctx), "estimate_audience"))?;
        let e = &body["data"][0];
        Ok(json!({
            "users_lower": e["estimate_mau_lower_bound"],
            "users_upper": e["estimate_mau_upper_bound"],
            "ready": e["estimate_ready"],
        }))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LibraryInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Termos da busca.
    query: String,
    /// Países onde os anúncios foram veiculados (ISO-2, 1 a 10).
    countries: Vec<String>,
    /// ALL (padrão), POLITICAL_AND_ISSUE_ADS...
    #[serde(default)]
    ad_type: Option<String>,
    /// ALL (padrão), ACTIVE ou INACTIVE.
    #[serde(default)]
    active_status: Option<String>,
    /// Máximo de anúncios (padrão 25).
    #[serde(default)]
    limit: Option<usize>,
}

struct SearchAdLibrary;

const LIBRARY_FIELDS: &str = "id,page_id,page_name,ad_creation_time,ad_delivery_start_time,\
    ad_delivery_stop_time,ad_creative_bodies,ad_creative_link_titles,ad_snapshot_url,publisher_platforms";

#[async_trait::async_trait]
impl ReadTool for SearchAdLibrary {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<LibraryInput>(
            "search_ad_library",
            "Busca anúncios públicos na Biblioteca de Anúncios da Meta, por termo e país.",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: LibraryInput = parse_input(input)?;
        let countries: Vec<String> = i
            .countries
            .iter()
            .map(|c| c.trim().to_uppercase())
            .collect();
        if countries.is_empty()
            || countries.len() > 10
            || countries
                .iter()
                .any(|c| c.len() != 2 || !c.chars().all(|x| x.is_ascii_alphabetic()))
        {
            return Err(invalid(
                "countries",
                "Informe de 1 a 10 países com o código de 2 letras (ex.: BR).",
            ));
        }
        if i.query.trim().is_empty() {
            return Err(invalid("query", "Informe os termos da busca."));
        }
        let limit = i.limit.unwrap_or(25).clamp(1, 500);
        let page = ctx
            .client
            .get_all(
                "ads_archive",
                &[
                    ("search_terms", i.query.trim().to_string()),
                    (
                        "ad_reached_countries",
                        serde_json::to_string(&countries).unwrap_or_default(),
                    ),
                    ("ad_type", i.ad_type.unwrap_or_else(|| "ALL".into())),
                    (
                        "ad_active_status",
                        i.active_status.unwrap_or_else(|| "ALL".into()),
                    ),
                    ("fields", LIBRARY_FIELDS.into()),
                    ("limit", limit.min(100).to_string()),
                ],
                limit,
            )
            .await
            .map_err(|e| match e {
                MetaError::Api { code: 10, .. }
                | MetaError::Api {
                    subcode: Some(2332002),
                    ..
                } => ToolError::new(
                    codes::ACCESS_DENIED,
                    "A Biblioteca de Anúncios exige confirmar a identidade da pessoa no Facebook \
                     e aceitar os termos da API da biblioteca.",
                )
                .with_suggestion(
                    "Siga os passos em https://www.facebook.com/ads/library/api e tente de novo.",
                ),
                other => translate(other, Some(ctx), "search_ad_library"),
            })?;
        let ads: Vec<Value> = page
            .data
            .into_iter()
            .map(|mut ad| {
                if let Some(url) = ad["ad_snapshot_url"].as_str() {
                    ad["ad_snapshot_url"] = Value::String(without_token(url));
                }
                ad
            })
            .collect();
        Ok(json!({ "ads": ads, "truncated": page.next.is_some() }))
    }
}
