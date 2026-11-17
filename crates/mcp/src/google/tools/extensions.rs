//! `manage_extensions`: assets formerly called extensions (sitelinks, callouts and
//! structured snippets) on the account, a campaign or an ad group.
//!
//! `add` creates each asset and its link in one atomic mutate (asset, link, asset, link,
//! ... so no batch references a temporary created in the same batch); the inverse removes
//! the links (Google does not remove assets, they stay unused in the library). `remove`
//! removes links and its inverse creates them again (`RecreateLinks`).

use std::collections::HashSet;

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::resources::{self, TempIds};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::create_common::{MAX_SITELINKS, SitelinkIn, bounded_text, sitelink_asset};
use super::labels::existing_links;
use super::links;
use super::reports::{invalid, valid_id};
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::plan::{InverseSpec, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![GTool::Write(Box::new(ManageExtensions))]
}

const NAME: &str = "manage_extensions";

const MAX_CALLOUTS: usize = 20;
const MAX_SNIPPETS: usize = 10;
const MAX_REMOVE: usize = 100;
const CALLOUT_CHARS: usize = 25;
const SNIPPET_VALUE_CHARS: usize = 25;
const SNIPPET_MIN_VALUES: usize = 3;
const SNIPPET_MAX_VALUES: usize = 10;
const SUMMARY_ITEMS: usize = 5;

/// Structured snippet headers Google accepts (English, as the API expects them).
const SNIPPET_HEADERS: &[&str] = &[
    "Amenities",
    "Brands",
    "Courses",
    "Degree programs",
    "Destinations",
    "Featured hotels",
    "Insurance coverage",
    "Models",
    "Neighborhoods",
    "Service catalog",
    "Shows",
    "Styles",
    "Types",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Action {
    Add,
    Remove,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SnippetIn {
    /// One of: Amenities, Brands, Courses, Degree programs, Destinations, Featured hotels,
    /// Insurance coverage, Models, Neighborhoods, Service catalog, Shows, Styles, Types.
    header: String,
    /// 3 to 10 values, up to 25 characters each.
    values: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LinkIn {
    asset_id: String,
    /// How the asset is used: `SITELINK`, `CALLOUT`, `STRUCTURED_SNIPPET`, `CALL`, ...
    /// (as listed by get_campaign_details).
    field_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ManageExtensionsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// `add` (create assets and link them) or `remove` (unlink assets).
    action: Action,
    /// Campaign the assets belong to. Omit it and ad_group_id to use the whole account.
    #[serde(default)]
    campaign_id: Option<String>,
    /// Ad group the assets belong to (instead of campaign_id).
    #[serde(default)]
    ad_group_id: Option<String>,
    /// `add`: up to 20 sitelinks.
    #[serde(default)]
    sitelinks: Vec<SitelinkIn>,
    /// `add`: up to 20 callouts, up to 25 characters each.
    #[serde(default)]
    callouts: Vec<String>,
    /// `add`: up to 10 structured snippets.
    #[serde(default)]
    structured_snippets: Vec<SnippetIn>,
    /// `remove`: links to remove (up to 100).
    #[serde(default)]
    links: Vec<LinkIn>,
}

/// Where the links go: collection, id prefix (`{parent}~` or empty) and the summary text.
struct Level {
    collection: &'static str,
    prefix: String,
    label: String,
}

fn level(input: &ManageExtensionsInput) -> Result<Level, ToolError> {
    match (&input.campaign_id, &input.ad_group_id) {
        (Some(_), Some(_)) => Err(invalid(
            "Provide campaign_id or ad_group_id, not both (omit both for the whole account).",
        )
        .with_field("ad_group_id")),
        (Some(c), None) => {
            let id = valid_id(c.trim(), "campaign_id")?;
            Ok(Level {
                collection: "campaignAssets",
                prefix: format!("{id}~"),
                label: format!("campaign {id}"),
            })
        }
        (None, Some(g)) => {
            let id = valid_id(g.trim(), "ad_group_id")?;
            Ok(Level {
                collection: "adGroupAssets",
                prefix: format!("{id}~"),
                label: format!("ad group {id}"),
            })
        }
        (None, None) => Ok(Level {
            collection: "customerAssets",
            prefix: String::new(),
            label: "the account".into(),
        }),
    }
}

fn list(items: &[String]) -> String {
    let mut out = items
        .iter()
        .take(SUMMARY_ITEMS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SUMMARY_ITEMS {
        out.push_str(&format!(", and {} more", items.len() - SUMMARY_ITEMS));
    }
    out
}

fn at_most(n: usize, max: usize, field: &str) -> Result<(), ToolError> {
    if n > max {
        return Err(
            invalid(format!("At most {max} {field} per call (got {n}).")).with_field(field),
        );
    }
    Ok(())
}

fn snippet_header(raw: &str, field: &str) -> Result<&'static str, ToolError> {
    let h = raw.trim();
    SNIPPET_HEADERS
        .iter()
        .find(|x| x.eq_ignore_ascii_case(h))
        .copied()
        .ok_or_else(|| {
            invalid(format!(
                "{field}: unknown header '{h}'. Use one of: {}.",
                SNIPPET_HEADERS.join(", ")
            ))
            .with_field(field)
        })
}

fn snippet_asset(i: usize, s: &SnippetIn) -> Result<(Value, String), ToolError> {
    let header = snippet_header(&s.header, &format!("structured_snippets[{i}].header"))?;
    let field = format!("structured_snippets[{i}].values");
    let n = s.values.len();
    if !(SNIPPET_MIN_VALUES..=SNIPPET_MAX_VALUES).contains(&n) {
        return Err(invalid(format!(
            "{field}: provide {SNIPPET_MIN_VALUES} to {SNIPPET_MAX_VALUES} values (got {n})."
        ))
        .with_field(field));
    }
    let mut seen = HashSet::new();
    let mut values = Vec::new();
    for (k, v) in s.values.iter().enumerate() {
        let v = bounded_text(v, SNIPPET_VALUE_CHARS, &format!("{field}[{k}]"))?;
        if !seen.insert(v.to_lowercase()) {
            return Err(invalid(format!("{field}: '{v}' appears more than once."))
                .with_field(format!("{field}[{k}]")));
        }
        values.push(v);
    }
    Ok((
        json!({ "structuredSnippetAsset": { "header": header, "values": values } }),
        header.to_string(),
    ))
}

struct ManageExtensions;

#[async_trait]
impl WriteTool for ManageExtensions {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ManageExtensionsInput>(
            NAME,
            "Manages Google Ads assets formerly called extensions on the account, a campaign \
             or an ad group: add creates sitelinks, callouts and structured snippets and links \
             them; remove unlinks assets (see the asset ids and field types with \
             get_campaign_details). Both can be undone.",
            ToolGroup::Edit,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ManageExtensionsInput = parse_input(input)?;
        let level = level(&input)?;
        match input.action {
            Action::Add => add(ctx, &input, &level),
            Action::Remove => remove(ctx, &input, &level).await,
        }
    }
}

fn add(ctx: &Ctx, input: &ManageExtensionsInput, level: &Level) -> Result<WritePlan, ToolError> {
    if !input.links.is_empty() {
        return Err(invalid("links is only used with action remove.").with_field("links"));
    }
    at_most(input.sitelinks.len(), MAX_SITELINKS, "sitelinks")?;
    at_most(input.callouts.len(), MAX_CALLOUTS, "callouts")?;
    at_most(
        input.structured_snippets.len(),
        MAX_SNIPPETS,
        "structured_snippets",
    )?;

    // (asset body without a name, field type, text for the summary)
    let mut items: Vec<(Value, &'static str, String)> = Vec::new();
    for (i, s) in input.sitelinks.iter().enumerate() {
        items.push((
            sitelink_asset(i, s)?,
            "SITELINK",
            s.link_text.trim().to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for (i, c) in input.callouts.iter().enumerate() {
        let field = format!("callouts[{i}]");
        let t = bounded_text(c, CALLOUT_CHARS, &field)?;
        if !seen.insert(t.to_lowercase()) {
            return Err(invalid(format!("Callout '{t}' appears more than once.")).with_field(field));
        }
        items.push((
            json!({ "calloutAsset": { "calloutText": t } }),
            "CALLOUT",
            t,
        ));
    }
    for (i, s) in input.structured_snippets.iter().enumerate() {
        let (body, header) = snippet_asset(i, s)?;
        items.push((body, "STRUCTURED_SNIPPET", header));
    }
    if items.is_empty() {
        return Err(invalid(
            "Provide at least one of sitelinks, callouts or structured_snippets.",
        ));
    }

    let mut temps = TempIds::new(&ctx.customer_id);
    let mut operations = Vec::new();
    let mut watch = Vec::new();
    let mut order = Vec::new();
    for (mut body, field_type, _) in items.iter().cloned() {
        let asset_rn = temps.next("assets");
        let asset_id = asset_rn.rsplit('/').next().unwrap_or_default().to_string();
        body["resourceName"] = json!(asset_rn);
        operations.push(json!({ "assetOperation": { "create": body } }));
        let link = resources::rn(
            &ctx.customer_id,
            level.collection,
            &format!("{}{asset_id}~{field_type}", level.prefix),
        );
        operations.push(links::create_op(&link)?);
        watch.push(links::watch(&link)?);
        order.push(link);
    }

    let mut parts = Vec::new();
    for (ft, noun) in [
        ("SITELINK", "sitelink(s)"),
        ("CALLOUT", "callout(s)"),
        ("STRUCTURED_SNIPPET", "structured snippet(s)"),
    ] {
        let texts: Vec<String> = items
            .iter()
            .filter(|(_, f, _)| *f == ft)
            .map(|(_, _, t)| format!("'{t}'"))
            .collect();
        if !texts.is_empty() {
            parts.push(format!("{} {noun} ({})", texts.len(), list(&texts)));
        }
    }
    Ok(WritePlan {
        summary: format!("Add to {}: {}", level.label, parts.join("; ")),
        warning: None,
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::RemoveCreated { order },
        direct: None,
    })
}

async fn remove(
    ctx: &Ctx,
    input: &ManageExtensionsInput,
    level: &Level,
) -> Result<WritePlan, ToolError> {
    if !input.sitelinks.is_empty()
        || !input.callouts.is_empty()
        || !input.structured_snippets.is_empty()
    {
        return Err(invalid(
            "sitelinks, callouts and structured_snippets are only used with action add; \
             to remove, list the links (asset_id and field_type).",
        ));
    }
    let n = input.links.len();
    if n == 0 || n > MAX_REMOVE {
        return Err(
            invalid(format!("Provide 1 to {MAX_REMOVE} links (got {n}).")).with_field("links"),
        );
    }
    let mut names = Vec::new();
    let mut shown = Vec::new();
    for (i, l) in input.links.iter().enumerate() {
        let id = valid_id(l.asset_id.trim(), &format!("links[{i}].asset_id"))?;
        let ft = l.field_type.trim().to_uppercase();
        if ft.is_empty() || !ft.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
            return Err(invalid(format!(
                "links[{i}].field_type must be a field type such as SITELINK or CALLOUT."
            ))
            .with_field(format!("links[{i}].field_type")));
        }
        let rn = resources::rn(
            &ctx.customer_id,
            level.collection,
            &format!("{}{id}~{ft}", level.prefix),
        );
        if names.contains(&rn) {
            return Err(
                invalid(format!("Asset {id} ({ft}) appears more than once."))
                    .with_field(format!("links[{i}]")),
            );
        }
        names.push(rn);
        shown.push(format!("{id} ({ft})"));
    }
    let existing = existing_links(ctx, NAME, &names, Some("status")).await?;
    let missing: Vec<&String> = shown
        .iter()
        .zip(&names)
        .filter(|(_, rn)| !existing.contains(*rn))
        .map(|(s, _)| s)
        .collect();
    if !missing.is_empty() {
        return Err(ToolError::new(
            codes::NOT_FOUND,
            format!(
                "Not linked to {}: {}.",
                level.label,
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .with_field("links")
        .with_suggestion("See the linked assets with get_campaign_details."));
    }
    let mut operations = Vec::new();
    let mut watch = Vec::new();
    for rn in &names {
        operations.push(resources::remove_op(rn).map_err(internal)?);
        watch.push(links::watch(rn)?);
    }
    Ok(WritePlan {
        summary: format!(
            "Unlink {} asset(s) from {}: {}",
            names.len(),
            level.label,
            list(&shown)
        ),
        warning: Some("The assets stay in the account's library.".into()),
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::RecreateLinks { names },
        direct: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_headers_are_case_insensitive() {
        assert_eq!(snippet_header(" brands ", "h").unwrap(), "Brands");
        assert_eq!(
            snippet_header("SERVICE CATALOG", "h").unwrap(),
            "Service catalog"
        );
        assert!(snippet_header("Marcas", "h").is_err());
    }

    #[test]
    fn snippet_values_bounds() {
        let s = |values: &[&str]| SnippetIn {
            header: "Brands".into(),
            values: values.iter().map(|v| v.to_string()).collect(),
        };
        assert!(snippet_asset(0, &s(&["a", "b"])).is_err());
        assert!(snippet_asset(0, &s(&["a", "b", "A"])).is_err());
        assert!(snippet_asset(0, &s(&["a", "b", &"x".repeat(26)])).is_err());
        let (body, header) = snippet_asset(0, &s(&["a", "b", "c"])).unwrap();
        assert_eq!(header, "Brands");
        assert_eq!(
            body["structuredSnippetAsset"]["values"],
            json!(["a", "b", "c"])
        );
    }
}
