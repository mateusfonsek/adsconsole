//! `get_gaql_fields`: the GAQL field catalog of a resource (`GoogleAdsFieldService`), so
//! queries use real field names instead of guesses.
//!
//! Two catalog queries: the resource itself (related resources, metrics and segments
//! selectable with it in `FROM`) and its own attributes (type, flags and enum values).
//! The catalog depends only on the API version; the client keeps it in memory.

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::gaql;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::reports::{invalid, to_json};
use super::{GTool, ReadTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::truncate::ITEMS_BUDGET;

pub(crate) const NAME: &str = "get_gaql_fields";

const MAX_FILTER_CHARS: usize = 60;

pub(crate) fn tool() -> GTool {
    GTool::Read(Box::new(GetGaqlFields))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetGaqlFieldsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// GAQL resource as used in FROM, in snake_case (e.g. `campaign`, `ad_group`,
    /// `keyword_view`, `search_term_view`).
    resource: String,
    /// Optional case-insensitive text: keeps only the names that contain it (e.g.
    /// `bidding`, `conversions`, `device`). Applies to attributes, metrics, segments and
    /// related resources.
    #[serde(default)]
    filter: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct Attribute {
    /// Full name to use in the query (e.g. `campaign.status`).
    name: String,
    /// Data type (`STRING`, `INT64`, `ENUM`, `DATE`, `RESOURCE_NAME`, ...).
    #[serde(rename = "type")]
    data_type: String,
    /// Accepted values of an `ENUM` field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    values: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    repeated: bool,
    /// Cannot go in SELECT.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    not_selectable: bool,
    /// Cannot go in WHERE.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    not_filterable: bool,
    /// Cannot go in ORDER BY.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    not_sortable: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetGaqlFieldsOutput {
    resource: String,
    /// The resource's own fields.
    attributes: Vec<Attribute>,
    /// Resources whose fields may also be selected with `FROM <resource>` (call this tool
    /// with one of them to see its fields).
    related_resources: Vec<String>,
    /// Segments compatible with `FROM <resource>`.
    segments: Vec<String>,
    /// Metrics compatible with `FROM <resource>`.
    metrics: Vec<String>,
    /// Some names were left out to fit the response; use `filter` to narrow it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

/// A resource name: lowercase letters, digits and `_`, starting with a letter.
fn resource_name(raw: &str) -> Result<String, ToolError> {
    let r = raw.trim();
    let valid = r.starts_with(|c: char| c.is_ascii_lowercase())
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        return Ok(r.to_string());
    }
    let mut e = invalid(format!(
        "resource must be a GAQL resource in snake_case, as written in FROM (got '{r}')."
    ))
    .with_field("resource");
    if let Some((head, _)) = r.split_once('.') {
        e = e.with_suggestion(format!(
            "Pass resource: '{head}' and filter: '{r}' to look up that field."
        ));
    }
    Err(e)
}

fn filter_text(raw: Option<&str>) -> Result<Option<String>, ToolError> {
    let Some(f) = raw.map(str::trim).filter(|f| !f.is_empty()) else {
        return Ok(None);
    };
    if f.chars().count() > MAX_FILTER_CHARS {
        return Err(invalid(format!(
            "filter can have at most {MAX_FILTER_CHARS} characters."
        ))
        .with_field("filter"));
    }
    Ok(Some(f.to_lowercase()))
}

fn strings(row: &Value, key: &str) -> Vec<String> {
    row.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Flag that defaults to `false` when absent (proto3 JSON omits `false`).
fn flag(row: &Value, key: &str) -> bool {
    row.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn attribute(row: &Value) -> Option<Attribute> {
    let name = row.get("name")?.as_str()?.to_string();
    Some(Attribute {
        name,
        data_type: row
            .get("dataType")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_string(),
        values: strings(row, "enumValues")
            .into_iter()
            .filter(|v| v != "UNSPECIFIED" && v != "UNKNOWN")
            .collect(),
        repeated: flag(row, "isRepeated"),
        not_selectable: !flag(row, "selectable"),
        not_filterable: !flag(row, "filterable"),
        not_sortable: !flag(row, "sortable"),
    })
}

/// Size of `v` as JSON (the item plus its separator).
fn json_len<T: Serialize>(v: &T) -> usize {
    serde_json::to_vec(v).map(|b| b.len() + 1).unwrap_or(0)
}

/// Keeps the response under the byte budget: names are cut first from the metrics, then
/// the segments, then the attributes (the resource's own fields matter most).
fn fit(out: &mut GetGaqlFieldsOutput) {
    if json_len(out) <= ITEMS_BUDGET {
        return;
    }
    out.truncated = true;
    let mut size = json_len(out);
    while size > ITEMS_BUDGET {
        if let Some(m) = out.metrics.pop() {
            size -= json_len(&m);
        } else if let Some(s) = out.segments.pop() {
            size -= json_len(&s);
        } else if let Some(r) = out.related_resources.pop() {
            size -= json_len(&r);
        } else if out.attributes.len() > 1 {
            let a = out.attributes.pop().expect("len > 1");
            size -= json_len(&a);
        } else {
            break;
        }
    }
}

struct GetGaqlFields;

#[async_trait]
impl ReadTool for GetGaqlFields {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GetGaqlFieldsInput>(
            NAME,
            "Lists the GAQL fields of a Google Ads resource: its attributes (type, accepted \
             enum values, whether they can be filtered or sorted), the related resources, \
             segments and metrics that can be selected with FROM <resource>. Use it before \
             writing a gaql_search query instead of guessing field names. The catalog only \
             changes with the API version, so reuse the answer.",
            ToolGroup::Read,
        )
        .output::<GetGaqlFieldsOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: GetGaqlFieldsInput = parse_input(input)?;
        let resource = resource_name(&input.resource)?;
        let filter = filter_text(input.filter.as_deref())?;
        let search = |q: String| async move {
            ctx.client
                .search_fields(&q)
                .await
                .map_err(|e| translate(e, Some(ctx), NAME))
        };

        let head = search(format!(
            "SELECT name, category, attribute_resources, metrics, segments WHERE name = {}",
            gaql::quote(&resource)
        ))
        .await?;
        let Some(row) = head.iter().find(|r| {
            r.get("name").and_then(Value::as_str) == Some(resource.as_str())
                && r.get("category").and_then(Value::as_str) == Some("RESOURCE")
        }) else {
            return Err(ToolError::new(
                codes::NOT_FOUND,
                format!("'{resource}' is not a GAQL resource."),
            )
            .with_field("resource")
            .with_suggestion(
                "Use the name written in FROM, in snake_case (e.g. campaign, ad_group, \
                 ad_group_ad, keyword_view, search_term_view, campaign_criterion).",
            ));
        };

        let prefix = format!("{resource}.");
        let attrs = search(format!(
            "SELECT name, selectable, filterable, sortable, data_type, is_repeated, enum_values \
             WHERE name LIKE {}",
            gaql::quote(&format!("{prefix}%"))
        ))
        .await?;

        let keep = |name: &str| filter.as_ref().is_none_or(|f| name.contains(f.as_str()));
        let names = |key: &str| {
            let mut v: Vec<String> = strings(row, key).into_iter().filter(|n| keep(n)).collect();
            v.sort();
            v
        };
        let mut attributes: Vec<Attribute> = attrs
            .iter()
            .filter_map(attribute)
            .filter(|a| a.name.starts_with(&prefix) && keep(&a.name))
            .collect();
        attributes.sort_by(|a, b| a.name.cmp(&b.name));

        let mut out = GetGaqlFieldsOutput {
            resource,
            attributes,
            related_resources: names("attributeResources"),
            segments: names("segments"),
            metrics: names("metrics"),
            truncated: false,
        };
        fit(&mut out);
        to_json(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resource_validation() {
        assert_eq!(resource_name(" ad_group ").unwrap(), "ad_group");
        assert!(resource_name("AdGroup").is_err());
        assert!(resource_name("campaign; DROP").is_err());
        assert!(resource_name("").is_err());
        let e = resource_name("campaign.status").unwrap_err();
        assert!(e.suggestion.unwrap().contains("filter: 'campaign.status'"));
    }

    #[test]
    fn attribute_flags_default_false() {
        let a = attribute(&json!({
            "name": "campaign.status", "dataType": "ENUM", "selectable": true,
            "filterable": true, "sortable": true,
            "enumValues": ["UNSPECIFIED", "UNKNOWN", "ENABLED", "PAUSED", "REMOVED"]
        }))
        .unwrap();
        assert_eq!(a.values, ["ENABLED", "PAUSED", "REMOVED"]);
        assert!(!a.not_filterable && !a.not_sortable && !a.not_selectable && !a.repeated);
        let b = attribute(
            &json!({ "name": "campaign.labels", "dataType": "RESOURCE_NAME",
                                   "selectable": true, "isRepeated": true }),
        )
        .unwrap();
        assert!(b.repeated && b.not_filterable && b.not_sortable && !b.not_selectable);
        let v = serde_json::to_value(&b).unwrap();
        assert!(v.get("values").is_none() && v.get("not_selectable").is_none());
    }

    #[test]
    fn fit_cuts_metrics_first() {
        let mut out = GetGaqlFieldsOutput {
            resource: "campaign".into(),
            attributes: vec![
                attribute(&json!({ "name": "campaign.id", "selectable": true })).unwrap(),
            ],
            related_resources: vec!["customer".into()],
            segments: vec!["segments.date".into()],
            metrics: (0..20_000).map(|i| format!("metrics.m{i}")).collect(),
            truncated: false,
        };
        fit(&mut out);
        assert!(out.truncated);
        assert!(serde_json::to_vec(&out).unwrap().len() <= ITEMS_BUDGET);
        assert_eq!(out.segments.len(), 1);
        assert_eq!(out.attributes.len(), 1);
    }
}
