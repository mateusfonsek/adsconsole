//! `keyword_ideas` and `keyword_metrics` (KeywordPlanIdeaService over REST).
//!
//! They require the Google Ads API Basic access level: any authorization error
//! becomes `ACCESS_LEVEL_INSUFFICIENT` with guidance on applying for Basic.

use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::{self, Decimal};
use adsconsole_google_ads::client::CallKind;
use adsconsole_google_ads::error::GoogleError;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::reports::{at, bounded, int, invalid, money_at, text, to_json, valid_id};
use super::{GTool, ReadTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::truncate::{ITEMS_BUDGET, fit_items};

const IDEAS_TOOL: &str = "keyword_ideas";
const METRICS_TOOL: &str = "keyword_metrics";
const DEFAULT_LANGUAGE: &str = "1014";
const DEFAULT_GEO: &str = "2076";
const MAX_KEYWORD_CHARS: usize = 80;
const MAX_METRICS_KEYWORDS: usize = 1000;

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Read(Box::new(KeywordIdeas)),
        GTool::Read(Box::new(KeywordMetrics)),
    ]
}

// ---------------------------------------------------------------- helpers

fn keywords_ok(keywords: &[String]) -> Result<(), ToolError> {
    for k in keywords {
        if k.trim().is_empty() || k.chars().count() > MAX_KEYWORD_CHARS {
            return Err(invalid(format!(
                "Each keyword must have 1 to {MAX_KEYWORD_CHARS} characters."
            ))
            .with_field("keywords"));
        }
    }
    Ok(())
}

/// The body's `language` and `geoTargetConstants`, with validated ids.
fn targeting(
    language_id: &Option<String>,
    geo_ids: &Option<Vec<String>>,
) -> Result<(String, Vec<String>), ToolError> {
    let language = valid_id(
        language_id.as_deref().unwrap_or(DEFAULT_LANGUAGE),
        "language_id",
    )?;
    let geos = match geo_ids {
        Some(g) if !g.is_empty() => g
            .iter()
            .map(|g| valid_id(g, "geo_ids"))
            .collect::<Result<_, _>>()?,
        _ => vec![DEFAULT_GEO.to_string()],
    };
    Ok((
        format!("languageConstants/{language}"),
        geos.iter()
            .map(|g| format!("geoTargetConstants/{g}"))
            .collect(),
    ))
}

/// Authorization errors (and those already translated to access errors) become the Basic
/// access level notice.
fn basic_level_error(e: ToolError) -> ToolError {
    ToolError::new(
        codes::ACCESS_LEVEL_INSUFFICIENT,
        "keyword_ideas and keyword_metrics require the Google Ads API Basic access level.",
    )
    .with_suggestion(
        "Complete brand verification for the Google Cloud project and apply for Basic API \
         access; then set the access level on the connection.",
    )
    .with_details(e.details.unwrap_or(Value::Null))
}

fn is_authorization(e: &GoogleError) -> bool {
    matches!(e, GoogleError::Api { errors, .. }
        if errors.iter().any(|x| x.category == "authorizationError"))
}

async fn post(ctx: &Ctx, tool: &str, suffix: &str, body: &Value) -> Result<Value, ToolError> {
    match ctx
        .client
        .post(
            &ctx.customer_id,
            ctx.login.as_deref(),
            suffix,
            body,
            CallKind::Read,
        )
        .await
    {
        Ok(v) => Ok(v),
        Err(e) if is_authorization(&e) => Err(basic_level_error(translate(e, Some(ctx), tool))),
        Err(e) => {
            let t = translate(e, Some(ctx), tool);
            if t.code == codes::ACCESS_DENIED || t.code == codes::ACCESS_LEVEL_INSUFFICIENT {
                Err(basic_level_error(t))
            } else {
                Err(t)
            }
        }
    }
}

fn results(resp: &Value) -> &[Value] {
    resp.get("results")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

// ---------------------------------------------------------- keyword_ideas

#[derive(Debug, Deserialize, JsonSchema)]
struct KeywordIdeasInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Seed keywords. Provide `keywords`, `url` or both.
    #[serde(default)]
    keywords: Option<Vec<String>>,
    /// Page (http/https) used as a seed.
    #[serde(default)]
    url: Option<String>,
    /// Language id. Default "1014" (Portuguese).
    #[serde(default)]
    language_id: Option<String>,
    /// Location ids. Default ["2076"] (Brazil).
    #[serde(default)]
    geo_ids: Option<Vec<String>>,
    /// Default 50, maximum 500.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct IdeaOut {
    text: String,
    avg_monthly_searches: i64,
    competition: Option<String>,
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    low_top_of_page_bid: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    high_top_of_page_bid: Option<Decimal>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct KeywordIdeasOutput {
    ideas: Vec<IdeaOut>,
    truncated: bool,
}

struct KeywordIdeas;

#[async_trait]
impl ReadTool for KeywordIdeas {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<KeywordIdeasInput>(
            IDEAS_TOOL,
            "Gets keyword ideas from seed keywords or a page (Keyword Planner). Requires the \
             Google Ads API Basic access level.",
            ToolGroup::Read,
        )
        .output::<KeywordIdeasOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: KeywordIdeasInput = parse_input(input)?;
        let limit = bounded(input.limit, 50, 1, 500, "limit")?;
        let (language, geos) = targeting(&input.language_id, &input.geo_ids)?;
        let keywords = input.keywords.unwrap_or_default();
        keywords_ok(&keywords)?;
        let url = match input.url.as_deref() {
            Some(u) => Some(valid_url(u)?),
            None => None,
        };
        let mut body = json!({
            "language": language,
            "geoTargetConstants": geos,
            "keywordPlanNetwork": "GOOGLE_SEARCH",
            "pageSize": limit,
        });
        match (keywords.is_empty(), url) {
            (true, None) => {
                return Err(
                    invalid("Provide keywords, url or both as the seed.").with_field("keywords")
                );
            }
            (false, None) => body["keywordSeed"] = json!({ "keywords": keywords }),
            (true, Some(u)) => body["urlSeed"] = json!({ "url": u }),
            (false, Some(u)) => {
                body["keywordAndUrlSeed"] = json!({ "url": u, "keywords": keywords })
            }
        }
        let resp = post(ctx, IDEAS_TOOL, ":generateKeywordIdeas", &body).await?;
        let mut ideas: Vec<IdeaOut> = results(&resp)
            .iter()
            .take(limit as usize)
            .map(|r| IdeaOut {
                text: text(r, "text").unwrap_or_default(),
                avg_monthly_searches: int(r, "keywordIdeaMetrics.avgMonthlySearches"),
                competition: text(r, "keywordIdeaMetrics.competition"),
                low_top_of_page_bid: money_at(r, "keywordIdeaMetrics.lowTopOfPageBidMicros"),
                high_top_of_page_bid: money_at(r, "keywordIdeaMetrics.highTopOfPageBidMicros"),
            })
            .collect();
        let truncated = fit_items(&mut ideas, ITEMS_BUDGET);
        to_json(KeywordIdeasOutput { ideas, truncated })
    }
}

fn valid_url(u: &str) -> Result<String, ToolError> {
    match url::Url::parse(u) {
        Ok(p) if matches!(p.scheme(), "http" | "https") && p.host_str().is_some() => {
            Ok(u.to_string())
        }
        _ => Err(invalid("url must be an http or https address.").with_field("url")),
    }
}

// --------------------------------------------------------- keyword_metrics

#[derive(Debug, Deserialize, JsonSchema)]
struct ForecastIn {
    /// Daily budget, in the account currency.
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    daily_budget: Decimal,
    /// Max CPC, in the account currency. Without it, uses the highest historical top-of-page bid (high range).
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    max_cpc: Option<Decimal>,
    /// Forecast days, starting tomorrow (1 to 90; default 30).
    #[serde(default)]
    days: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct KeywordMetricsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// 1 to 1000 keywords, up to 80 characters each.
    keywords: Vec<String>,
    #[serde(default)]
    language_id: Option<String>,
    #[serde(default)]
    geo_ids: Option<Vec<String>>,
    /// With `forecast`, also returns the forecast for a Search campaign (broad match).
    #[serde(default)]
    forecast: Option<ForecastIn>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct MonthlyOut {
    year: String,
    month: String,
    monthly_searches: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct HistoricalOut {
    text: String,
    avg_monthly_searches: i64,
    competition: Option<String>,
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    low_top_of_page_bid: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    high_top_of_page_bid: Option<Decimal>,
    monthly_searches: Vec<MonthlyOut>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ForecastOut {
    impressions: f64,
    clicks: f64,
    #[serde(with = "rust_decimal::serde::float")]
    #[schemars(with = "f64")]
    cost: Decimal,
    conversions: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct KeywordMetricsOutput {
    historical: Vec<HistoricalOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forecast: Option<ForecastOut>,
    truncated: bool,
}

struct KeywordMetrics;

fn micros(amount: Decimal, currency: &str, field: &str) -> Result<i64, ToolError> {
    if amount <= Decimal::ZERO {
        return Err(invalid(format!("{field} must be greater than zero.")).with_field(field));
    }
    money::to_micros_checked(amount, currency)
        .map_err(|e| invalid(format!("{field}: {e}")).with_field(field))
}

fn f64_at(v: &Value, path: &str) -> f64 {
    super::reports::float(v, path)
}

#[async_trait]
impl ReadTool for KeywordMetrics {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<KeywordMetricsInput>(
            METRICS_TOOL,
            "Gets historical keyword metrics and, optionally, a performance forecast (Keyword \
             Planner). Without forecast.max_cpc, the forecast uses the highest historical \
             top-of-page bid (high range). Requires the Google Ads API Basic access level.",
            ToolGroup::Read,
        )
        .output::<KeywordMetricsOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: KeywordMetricsInput = parse_input(input)?;
        if input.keywords.is_empty() || input.keywords.len() > MAX_METRICS_KEYWORDS {
            return Err(
                invalid(format!("Provide 1 to {MAX_METRICS_KEYWORDS} keywords."))
                    .with_field("keywords"),
            );
        }
        keywords_ok(&input.keywords)?;
        let (language, geos) = targeting(&input.language_id, &input.geo_ids)?;
        let forecast = match &input.forecast {
            Some(f) => {
                let days = bounded(f.days, 30, 1, 90, "forecast.days")?;
                let budget = micros(f.daily_budget, &ctx.currency, "forecast.daily_budget")?;
                let cpc = f
                    .max_cpc
                    .map(|c| micros(c, &ctx.currency, "forecast.max_cpc"))
                    .transpose()?;
                Some((days, budget, cpc))
            }
            None => None,
        };

        let body = json!({
            "keywords": input.keywords,
            "language": language,
            "geoTargetConstants": geos,
            "keywordPlanNetwork": "GOOGLE_SEARCH",
        });
        let resp = post(
            ctx,
            METRICS_TOOL,
            ":generateKeywordHistoricalMetrics",
            &body,
        )
        .await?;
        let mut historical: Vec<HistoricalOut> = results(&resp)
            .iter()
            .map(|r| HistoricalOut {
                text: text(r, "text").unwrap_or_default(),
                avg_monthly_searches: int(r, "keywordMetrics.avgMonthlySearches"),
                competition: text(r, "keywordMetrics.competition"),
                low_top_of_page_bid: money_at(r, "keywordMetrics.lowTopOfPageBidMicros"),
                high_top_of_page_bid: money_at(r, "keywordMetrics.highTopOfPageBidMicros"),
                monthly_searches: at(r, "keywordMetrics.monthlySearchVolumes")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|m| MonthlyOut {
                                year: text(m, "year").unwrap_or_default(),
                                month: text(m, "month").unwrap_or_default(),
                                monthly_searches: int(m, "monthlySearches"),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        // The forecast's default CPC comes from the full history, before truncation.
        let top_bid = historical
            .iter()
            .filter_map(|h| h.high_top_of_page_bid)
            .max();
        let truncated = fit_items(&mut historical, ITEMS_BUDGET);

        let forecast = match forecast {
            Some((days, budget, cpc)) => {
                let cpc = match cpc {
                    Some(c) => c,
                    None => top_bid
                        .and_then(|b| money::to_micros(b).ok())
                        .filter(|m| *m > 0)
                        .ok_or_else(|| {
                            invalid("Provide max_cpc for the forecast.")
                                .with_field("forecast.max_cpc")
                        })?,
                };
                Some(run_forecast(ctx, &input.keywords, &language, &geos, days, budget, cpc).await?)
            }
            None => None,
        };
        to_json(KeywordMetricsOutput {
            historical,
            forecast,
            truncated,
        })
    }
}

async fn run_forecast(
    ctx: &Ctx,
    keywords: &[String],
    language: &str,
    geos: &[String],
    days: u32,
    daily_budget_micros: i64,
    max_cpc_micros: i64,
) -> Result<ForecastOut, ToolError> {
    let start = Utc::now().date_naive() + Duration::days(1);
    let end = start + Duration::days(i64::from(days) - 1);
    let manual = json!({
        "dailyBudgetMicros": daily_budget_micros.to_string(),
        "maxCpcBidMicros": max_cpc_micros.to_string(),
    });
    let biddable: Vec<Value> = keywords
        .iter()
        .map(|k| json!({ "keyword": { "text": k, "matchType": "BROAD" } }))
        .collect();
    let body = json!({
        "currencyCode": ctx.currency,
        "forecastPeriod": { "startDate": start.to_string(), "endDate": end.to_string() },
        "campaign": {
            "keywordPlanNetwork": "GOOGLE_SEARCH",
            "languageConstants": [language],
            "geoModifiers": geos.iter().map(|g| json!({ "geoTargetConstant": g })).collect::<Vec<_>>(),
            "biddingStrategy": { "manualCpcBiddingStrategy": manual },
            "adGroups": [{ "biddableKeywords": biddable }],
        },
    });
    let resp = post(ctx, METRICS_TOOL, ":generateKeywordForecastMetrics", &body).await?;
    let m = "campaignForecastMetrics";
    Ok(ForecastOut {
        impressions: f64_at(&resp, &format!("{m}.impressions")),
        clicks: f64_at(&resp, &format!("{m}.clicks")),
        cost: money_at(&resp, &format!("{m}.costMicros")).unwrap_or_default(),
        conversions: f64_at(&resp, &format!("{m}.conversions")),
    })
}
