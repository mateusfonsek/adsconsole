//! Labels: `manage_labels` (create, edit and remove account labels) and `apply_labels`
//! (attach labels to, or detach them from, campaigns, ad groups, ads and keywords).
//!
//! Creating labels and attaching them have `RemoveCreated` as the inverse; edits are
//! `FieldChange`s with `RestoreFields`; detaching recreates the links on undo
//! (`RecreateLinks`); removing a label is irreversible (Google also drops its links).

use std::collections::{BTreeMap, HashSet};

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::gaql;
use adsconsole_google_ads::resources::{self, TempIds};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::create_common::bounded_text;
use super::edit::{Edits, Show};
use super::links::{self, MAX_LINKS};
use super::reports::{invalid, text, valid_id};
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Write(Box::new(ManageLabels)),
        GTool::Write(Box::new(ApplyLabels)),
    ]
}

const MANAGE_LABELS: &str = "manage_labels";
const APPLY_LABELS: &str = "apply_labels";

const MAX_LABELS: usize = 50;
const MAX_LABELS_PER_APPLY: usize = 20;
const MAX_TARGETS: usize = 500;
const NAME_CHARS: usize = 80;
const DESCRIPTION_CHARS: usize = 200;
/// Names per `resource_name IN (...)` query.
const IN_CHUNK: usize = 200;
const SUMMARY_ITEMS: usize = 5;

const REMOVE_WARNING: &str = "Removing a label also detaches it from every campaign, ad group, \
                              ad and keyword. This change cannot be undone.";

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

/// `#RRGGBB`, returned in uppercase.
fn color(raw: &str, field: &str) -> Result<String, ToolError> {
    let c = raw.trim();
    let ok = c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return Err(invalid(format!(
            "{field} must be a color in the #RRGGBB format (got '{c}')."
        ))
        .with_field(field));
    }
    Ok(c.to_uppercase())
}

/// Optional description: up to 200 characters; empty clears it.
fn description(raw: &str, field: &str) -> Result<String, ToolError> {
    let d = raw.trim();
    if d.chars().count() > DESCRIPTION_CHARS {
        return Err(invalid(format!(
            "{field} can have at most {DESCRIPTION_CHARS} characters."
        ))
        .with_field(field));
    }
    Ok(d.to_string())
}

/// Numeric ids without repeats; errors on `field[i]`.
fn ids(items: &[String], field: &str) -> Result<Vec<String>, ToolError> {
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in items.iter().enumerate() {
        let f = format!("{field}[{i}]");
        let id = valid_id(raw.trim(), &f)?;
        if out.contains(&id) {
            return Err(invalid(format!("{field}: {id} appears more than once.")).with_field(f));
        }
        out.push(id);
    }
    Ok(out)
}

/// Rows of the account's labels with these ids, keyed by id. A missing or removed label
/// is `NOT_FOUND`.
async fn read_labels(
    ctx: &Ctx,
    tool: &str,
    label_ids: &[String],
    field: &str,
) -> Result<BTreeMap<String, Value>, ToolError> {
    let query = format!(
        "SELECT label.resource_name, label.id, label.name, label.status, \
         label.text_label.description, label.text_label.background_color \
         FROM label WHERE label.id IN ({})",
        label_ids.join(", ")
    );
    let rows = ctx
        .client
        .search_all(
            &ctx.customer_id,
            ctx.login.as_deref(),
            &query,
            label_ids.len(),
        )
        .await
        .map_err(|e| translate(e, Some(ctx), tool))?;
    let found: BTreeMap<String, Value> = rows
        .into_iter()
        .filter(|r| text(r, "label.status").as_deref() != Some("REMOVED"))
        .filter_map(|r| text(&r, "label.id").map(|id| (id, r)))
        .collect();
    let missing: Vec<String> = label_ids
        .iter()
        .filter(|id| !found.contains_key(*id))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(ToolError::new(
            codes::NOT_FOUND,
            format!(
                "Label(s) {} not found in account {}.",
                missing.join(", "),
                ctx.customer_id
            ),
        )
        .with_field(field)
        .with_suggestion(
            "List the labels with gaql_search: SELECT label.id, label.name FROM label",
        ));
    }
    Ok(found)
}

fn label_name(row: &Value) -> String {
    format!("'{}'", text(row, "label.name").unwrap_or_default())
}

// ------------------------------------------------------------ manage_labels

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum LabelAction {
    Create,
    Update,
    Remove,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LabelIn {
    /// `update` and `remove`: id of the label.
    #[serde(default)]
    label_id: Option<String>,
    /// `create` (required) and `update`: name, up to 80 characters, unique in the account.
    #[serde(default)]
    name: Option<String>,
    /// `create` and `update`: description, up to 200 characters (empty clears it).
    #[serde(default)]
    description: Option<String>,
    /// `create` and `update`: background color, `#RRGGBB`.
    #[serde(default)]
    background_color: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ManageLabelsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// `create`, `update` or `remove`.
    action: LabelAction,
    /// Up to 50 per call.
    labels: Vec<LabelIn>,
}

struct ManageLabels;

#[async_trait]
impl WriteTool for ManageLabels {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ManageLabelsInput>(
            MANAGE_LABELS,
            "Manages the labels of a Google Ads account: create, update (name, description, \
             color) or remove. Labels organize campaigns, ad groups, ads and keywords (attach \
             them with apply_labels) and can be used in GAQL filters. Removing a label cannot \
             be undone.",
            ToolGroup::Edit,
            true,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ManageLabelsInput = parse_input(input)?;
        let n = input.labels.len();
        if n == 0 || n > MAX_LABELS {
            return Err(
                invalid(format!("Provide 1 to {MAX_LABELS} labels (got {n})."))
                    .with_field("labels"),
            );
        }
        match input.action {
            LabelAction::Create => create_labels(ctx, &input.labels),
            LabelAction::Update => update_labels(ctx, &input.labels).await,
            LabelAction::Remove => remove_labels(ctx, &input.labels).await,
        }
    }
}

fn create_labels(ctx: &Ctx, labels: &[LabelIn]) -> Result<WritePlan, ToolError> {
    let mut temps = TempIds::new(&ctx.customer_id);
    let mut operations = Vec::new();
    let mut watch = Vec::new();
    let mut order = Vec::new();
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for (i, l) in labels.iter().enumerate() {
        let f = |k: &str| format!("labels[{i}].{k}");
        let name = bounded_text(l.name.as_deref().unwrap_or(""), NAME_CHARS, &f("name"))?;
        if !seen.insert(name.to_lowercase()) {
            return Err(
                invalid(format!("Label '{name}' appears more than once.")).with_field(f("name"))
            );
        }
        let mut text_label = serde_json::Map::new();
        if let Some(d) = &l.description {
            text_label.insert(
                "description".into(),
                json!(description(d, &f("description"))?),
            );
        }
        if let Some(c) = &l.background_color {
            text_label.insert(
                "backgroundColor".into(),
                json!(color(c, &f("background_color"))?),
            );
        }
        let rn = temps.next("labels");
        let mut body = json!({ "resourceName": rn, "name": name });
        if !text_label.is_empty() {
            body["textLabel"] = Value::Object(text_label);
        }
        operations.push(json!({ "labelOperation": { "create": body } }));
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: "label",
            fields: vec!["label.name"],
        });
        order.push(rn);
        names.push(format!("'{name}'"));
    }
    Ok(WritePlan {
        summary: format!("Create {} label(s): {}", names.len(), list(&names)),
        warning: None,
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::RemoveCreated { order },
        direct: None,
    })
}

fn label_ids(labels: &[LabelIn]) -> Result<Vec<String>, ToolError> {
    let raw: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| {
            l.label_id
                .clone()
                .ok_or_else(|| invalid(format!("Provide labels[{i}].label_id.")))
                .map_err(|e| e.with_field(format!("labels[{i}].label_id")))
        })
        .collect::<Result<_, _>>()?;
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in raw.iter().enumerate() {
        let f = format!("labels[{i}].label_id");
        let id = valid_id(raw.trim(), &f)?;
        if out.contains(&id) {
            return Err(invalid(format!("Label {id} appears more than once.")).with_field(f));
        }
        out.push(id);
    }
    Ok(out)
}

async fn update_labels(ctx: &Ctx, labels: &[LabelIn]) -> Result<WritePlan, ToolError> {
    let ids = label_ids(labels)?;
    let found = read_labels(ctx, MANAGE_LABELS, &ids, "labels").await?;
    let mut edits = Edits::new("Labels", &ctx.currency);
    for (i, (l, id)) in labels.iter().zip(&ids).enumerate() {
        let f = |k: &str| format!("labels[{i}].{k}");
        let row = &found[id];
        let rn = resources::rn(&ctx.customer_id, "labels", id);
        let what = label_name(row);
        let before = edits.len();
        if let Some(n) = &l.name {
            let name = bounded_text(n, NAME_CHARS, &f("name"))?;
            edits.set(
                &rn,
                "label.name",
                row,
                json!(name),
                &format!("name of {what}"),
                Show::Quoted,
            )?;
        }
        if let Some(d) = &l.description {
            let d = description(d, &f("description"))?;
            edits.set(
                &rn,
                "label.text_label.description",
                row,
                json!(d),
                &format!("description of {what}"),
                Show::Quoted,
            )?;
        }
        if let Some(c) = &l.background_color {
            let c = color(c, &f("background_color"))?;
            edits.set(
                &rn,
                "label.text_label.background_color",
                row,
                json!(c),
                &format!("color of {what}"),
                Show::Plain,
            )?;
        }
        if edits.len() == before {
            return Err(invalid(format!(
                "labels[{i}]: provide name, description or background_color to change."
            ))
            .with_field(format!("labels[{i}]")));
        }
    }
    edits.into_plan(None)
}

async fn remove_labels(ctx: &Ctx, labels: &[LabelIn]) -> Result<WritePlan, ToolError> {
    let ids = label_ids(labels)?;
    let found = read_labels(ctx, MANAGE_LABELS, &ids, "labels").await?;
    let mut operations = Vec::new();
    let mut names = Vec::new();
    for id in &ids {
        let rn = resources::rn(&ctx.customer_id, "labels", id);
        operations.push(resources::remove_op(&rn).map_err(internal)?);
        names.push(label_name(&found[id]));
    }
    Ok(WritePlan {
        summary: format!("Remove {} label(s): {}", names.len(), list(&names)),
        warning: Some(REMOVE_WARNING.to_string()),
        operations,
        field_changes: Vec::new(),
        watch: Vec::new(),
        inverse: InverseSpec::None,
        direct: None,
    })
}

// ------------------------------------------------------------- apply_labels

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ApplyAction {
    Attach,
    Detach,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AdRef {
    ad_group_id: String,
    ad_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct KeywordRef {
    ad_group_id: String,
    /// Criterion id of the keyword in the ad group.
    criterion_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ApplyLabelsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// `attach` or `detach`.
    action: ApplyAction,
    /// Labels to attach or detach (up to 20); each one applies to every target.
    label_ids: Vec<String>,
    #[serde(default)]
    campaign_ids: Vec<String>,
    #[serde(default)]
    ad_group_ids: Vec<String>,
    #[serde(default)]
    ads: Vec<AdRef>,
    #[serde(default)]
    keywords: Vec<KeywordRef>,
}

/// One kind of target: link collection, how the summary names it, and the id parts.
struct Targets {
    collection: &'static str,
    noun: &'static str,
    ids: Vec<String>,
}

fn pair(a: &str, b: &str, field: &str) -> Result<String, ToolError> {
    Ok(format!(
        "{}~{}",
        valid_id(a.trim(), &format!("{field}.ad_group_id"))?,
        valid_id(b.trim(), field)?
    ))
}

fn targets(input: &ApplyLabelsInput) -> Result<Vec<Targets>, ToolError> {
    let ads = input
        .ads
        .iter()
        .enumerate()
        .map(|(i, a)| pair(&a.ad_group_id, &a.ad_id, &format!("ads[{i}].ad_id")))
        .collect::<Result<Vec<_>, _>>()?;
    let kws = input
        .keywords
        .iter()
        .enumerate()
        .map(|(i, k)| {
            pair(
                &k.ad_group_id,
                &k.criterion_id,
                &format!("keywords[{i}].criterion_id"),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = vec![
        Targets {
            collection: "campaignLabels",
            noun: "campaign(s)",
            ids: ids(&input.campaign_ids, "campaign_ids")?,
        },
        Targets {
            collection: "adGroupLabels",
            noun: "ad group(s)",
            ids: ids(&input.ad_group_ids, "ad_group_ids")?,
        },
        Targets {
            collection: "adGroupAdLabels",
            noun: "ad(s)",
            ids: ads,
        },
        Targets {
            collection: "adGroupCriterionLabels",
            noun: "keyword(s)",
            ids: kws,
        },
    ];
    for t in &mut out {
        let mut seen = HashSet::new();
        t.ids.retain(|id| seen.insert(id.clone()));
    }
    let total: usize = out.iter().map(|t| t.ids.len()).sum();
    if total == 0 {
        return Err(invalid(
            "Provide at least one target: campaign_ids, ad_group_ids, ads or keywords.",
        ));
    }
    if total > MAX_TARGETS {
        return Err(invalid(format!(
            "At most {MAX_TARGETS} targets per call (got {total})."
        )));
    }
    Ok(out)
}

/// Which of these link names exist in the account.
pub(crate) async fn existing_links(
    ctx: &Ctx,
    tool: &str,
    names: &[String],
    status_field: Option<&str>,
) -> Result<HashSet<String>, ToolError> {
    let mut by_resource: BTreeMap<&'static str, Vec<&String>> = BTreeMap::new();
    for n in names {
        by_resource
            .entry(links::gaql_resource(n)?)
            .or_default()
            .push(n);
    }
    let mut out = HashSet::new();
    for (resource, names) in by_resource {
        let camel = adsconsole_google_ads::fields::camel(resource);
        for chunk in names.chunks(IN_CHUNK) {
            let quoted: Vec<String> = chunk.iter().map(|n| gaql::quote(n)).collect();
            let mut select = format!("{resource}.resource_name");
            if let Some(s) = status_field {
                select.push_str(&format!(", {resource}.{s}"));
            }
            let query = format!(
                "SELECT {select} FROM {resource} WHERE {resource}.resource_name IN ({})",
                quoted.join(", ")
            );
            let rows = ctx
                .client
                .search_all(&ctx.customer_id, ctx.login.as_deref(), &query, chunk.len())
                .await
                .map_err(|e| translate(e, Some(ctx), tool))?;
            for r in rows {
                let removed = status_field.is_some_and(|s| {
                    text(
                        &r,
                        &format!("{camel}.{}", adsconsole_google_ads::fields::camel(s)),
                    )
                    .as_deref()
                        == Some("REMOVED")
                });
                if !removed && let Some(n) = text(&r, &format!("{camel}.resourceName")) {
                    out.insert(n);
                }
            }
        }
    }
    Ok(out)
}

struct ApplyLabels;

#[async_trait]
impl WriteTool for ApplyLabels {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ApplyLabelsInput>(
            APPLY_LABELS,
            "Attaches labels to, or detaches them from, Google Ads campaigns, ad groups, ads \
             and keywords (create labels with manage_labels). Targets that already have the \
             label (attach) or don't have it (detach) are skipped.",
            ToolGroup::Edit,
            true,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ApplyLabelsInput = parse_input(input)?;
        let label_ids = ids(&input.label_ids, "label_ids")?;
        if label_ids.is_empty() || label_ids.len() > MAX_LABELS_PER_APPLY {
            return Err(
                invalid(format!("Provide 1 to {MAX_LABELS_PER_APPLY} label_ids."))
                    .with_field("label_ids"),
            );
        }
        let targets = targets(&input)?;
        let labels = read_labels(ctx, APPLY_LABELS, &label_ids, "label_ids").await?;

        // label × target, grouped for the summary.
        let mut wanted: Vec<(String, &Targets)> = Vec::new();
        for t in &targets {
            for id in &t.ids {
                for l in &label_ids {
                    let rn = resources::rn(&ctx.customer_id, t.collection, &format!("{id}~{l}"));
                    wanted.push((rn, t));
                }
            }
        }
        if wanted.len() > MAX_LINKS {
            return Err(invalid(format!(
                "Labels × targets must be at most {MAX_LINKS} (got {}).",
                wanted.len()
            )));
        }
        let names: Vec<String> = wanted.iter().map(|(n, _)| n.clone()).collect();
        let existing = existing_links(ctx, APPLY_LABELS, &names, None).await?;
        let attach = input.action == ApplyAction::Attach;
        let todo: Vec<&(String, &Targets)> = wanted
            .iter()
            .filter(|(n, _)| existing.contains(n) != attach)
            .collect();
        if todo.is_empty() {
            let msg = if attach {
                "Every target already has these labels."
            } else {
                "None of the targets has these labels."
            };
            return Err(ToolError::new(codes::INVALID_STATE, msg));
        }
        let skipped = wanted.len() - todo.len();

        let mut operations = Vec::new();
        let mut watch = Vec::new();
        let mut done_names = Vec::new();
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for (rn, t) in &todo {
            operations.push(if attach {
                links::create_op(rn)?
            } else {
                resources::remove_op(rn).map_err(internal)?
            });
            watch.push(links::watch(rn)?);
            done_names.push(rn.clone());
            match counts.iter_mut().find(|(noun, _)| *noun == t.noun) {
                Some((_, c)) => *c += 1,
                None => counts.push((t.noun, 1)),
            }
        }
        let label_list: Vec<String> = label_ids.iter().map(|id| label_name(&labels[id])).collect();
        let what = counts
            .iter()
            .map(|(noun, c)| format!("{c} {noun}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut summary = if attach {
            format!("Attach label(s) {} ({what} links)", list(&label_list))
        } else {
            format!("Detach label(s) {} ({what} links)", list(&label_list))
        };
        if skipped > 0 {
            let why = if attach {
                "already attached"
            } else {
                "not attached"
            };
            summary.push_str(&format!("; {skipped} skipped ({why})"));
        }
        Ok(WritePlan {
            summary,
            warning: None,
            operations,
            field_changes: Vec::new(),
            watch,
            inverse: if attach {
                InverseSpec::RemoveCreated { order: done_names }
            } else {
                InverseSpec::RecreateLinks { names: done_names }
            },
            direct: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_format() {
        assert_eq!(color(" #a1b2c3 ", "c").unwrap(), "#A1B2C3");
        assert!(color("a1b2c3", "c").is_err());
        assert!(color("#a1b2cz", "c").is_err());
        assert!(color("#abc", "c").is_err());
    }

    #[test]
    fn ids_reject_repeats() {
        assert_eq!(ids(&["1".into(), " 2".into()], "x").unwrap(), ["1", "2"]);
        assert!(ids(&["1".into(), "1".into()], "x").is_err());
        assert!(ids(&["a".into()], "x").is_err());
    }
}
