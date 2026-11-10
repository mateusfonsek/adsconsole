//! Supporting reads: `get_pages`, `get_page_posts`, `search_targeting`,
//! `estimate_audience`, `search_ad_library`.

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
            "Lists the Facebook Pages (and the Instagram accounts linked to them) that the \
             connection can use in creatives.",
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
    /// Page (from get_pages).
    page_id: String,
    /// Maximum number of posts (default 25).
    #[serde(default)]
    limit: Option<usize>,
}

struct GetPagePosts;

#[async_trait::async_trait]
impl ReadTool for GetPagePosts {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<PagePostsInput>(
            "get_page_posts",
            "Lists the published posts of a Page, with the object_story_id needed to advertise \
             an existing post (existing_post creative).",
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
        // The Page token is used only here; it never appears in the result.
        let token = pages
            .data
            .iter()
            .find(|p| p["id"].as_str() == Some(i.page_id.as_str()))
            .and_then(|p| p["access_token"].as_str())
            .ok_or_else(|| {
                ToolError::new(
                    codes::NOT_FOUND,
                    format!("Page {} is not among this connection's Pages.", i.page_id),
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
    /// interest, behavior, demographic or geo.
    #[serde(rename = "type")]
    kind: TargetingType,
    /// Search text (required for interest and geo; filters behavior and demographic).
    #[serde(default)]
    query: Option<String>,
    /// geo only: country, region, city, zip (default: all four).
    #[serde(default)]
    geo_types: Vec<GeoType>,
    /// Maximum number of results (default 25).
    #[serde(default)]
    limit: Option<usize>,
}

struct SearchTargeting;

#[async_trait::async_trait]
impl ReadTool for SearchTargeting {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<TargetingInput>(
            "search_targeting",
            "Searches targeting options: interests, behaviors, demographics or locations \
             (ids to use in targeting).",
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
                    return Err(invalid("query", "Provide the search text."));
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
    /// Meta targeting_spec (geo_locations, age, gender, interests...).
    targeting: Value,
    /// Optimization goal of the estimate (default REACH).
    #[serde(default)]
    optimization_goal: Option<String>,
}

struct EstimateAudience;

#[async_trait::async_trait]
impl ReadTool for EstimateAudience {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<AudienceInput>(
            "estimate_audience",
            "Estimates how many people an audience reaches (range of monthly users).",
            ToolGroup::Read,
        )
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let i: AudienceInput = parse_input(input)?;
        if !i.targeting["geo_locations"].is_object() {
            return Err(invalid(
                "targeting",
                "Provide at least one location in targeting.geo_locations.",
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
    /// Search terms.
    query: String,
    /// Countries where the ads ran (ISO-2, 1 to 10).
    countries: Vec<String>,
    /// ALL (default), POLITICAL_AND_ISSUE_ADS...
    #[serde(default)]
    ad_type: Option<String>,
    /// ALL (default), ACTIVE or INACTIVE.
    #[serde(default)]
    active_status: Option<String>,
    /// Maximum number of ads (default 25).
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
            "Searches public ads in the Meta Ad Library, by term and country.",
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
                "Provide 1 to 10 countries as 2-letter codes (e.g. BR).",
            ));
        }
        if i.query.trim().is_empty() {
            return Err(invalid("query", "Provide the search terms."));
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
                    "The Ad Library requires the person to confirm their identity on Facebook \
                     and accept the Ad Library API terms.",
                )
                .with_suggestion(
                    "Follow the steps at https://www.facebook.com/ads/library/api and try again.",
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
