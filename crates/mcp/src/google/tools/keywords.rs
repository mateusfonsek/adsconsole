//! Keywords and negative keywords: `manage_keywords` and `manage_negatives`.
//!
//! Creates use temporary ids (`{parent}~-N`) and have `RemoveCreated` as the inverse;
//! pause, enable and bid changes are `FieldChange`s with `RestoreFields`; remove and
//! match type changes are irreversible (`InverseSpec::None`).
//!
//! The criterion builders (`criterion_temp`, `create_op`, `keyword_body`,
//! `keyword_text`) are `pub(crate)` for the targeting and campaign creation tools.

use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::Decimal;
use adsconsole_google_ads::fields;
use adsconsole_google_ads::resources::{self, TempIds};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::edit::{EditStatus, Edits, Show, positive_micros};
use super::reports::{at, invalid, text, valid_id};
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Write(Box::new(ManageKeywords)),
        GTool::Write(Box::new(ManageNegatives)),
    ]
}

const MANAGE_KEYWORDS: &str = "manage_keywords";
const MANAGE_NEGATIVES: &str = "manage_negatives";

const MAX_KEYWORDS: usize = 200;
const MAX_TEXT_CHARS: usize = 80;
const MAX_TEXT_WORDS: usize = 10;
const MAX_LIST_NAME_CHARS: usize = 255;
/// How many keywords the summary shows before "and N more".
const SUMMARY_ITEMS: usize = 5;

pub(crate) const IRREVERSIBLE_WARNING: &str = "This change cannot be undone.";
const MATCH_TYPE_WARNING: &str = "Google does not allow changing the match type; \
                                  the old keyword will be removed and a new one created.";

// ---------------------------------------------------------------- helpers

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum MatchType {
    Exact,
    Phrase,
    Broad,
}

impl MatchType {
    pub(crate) fn to_google(self) -> &'static str {
        match self {
            MatchType::Exact => "EXACT",
            MatchType::Phrase => "PHRASE",
            MatchType::Broad => "BROAD",
        }
    }
}

/// Required match type, case-insensitive.
pub(crate) fn match_type(v: &Option<String>, field: &str) -> Result<MatchType, ToolError> {
    let raw = required(v, field)?;
    match raw.trim().to_uppercase().as_str() {
        "EXACT" => Ok(MatchType::Exact),
        "PHRASE" => Ok(MatchType::Phrase),
        "BROAD" => Ok(MatchType::Broad),
        _ => Err(invalid(format!(
            "{field}: unknown match type '{raw}'; use EXACT, PHRASE or BROAD."
        ))
        .with_field(field)),
    }
}

/// Input keyword text: 1 to 80 characters and up to 10 words. `field` is the field
/// named in the error (e.g. `keywords[0].text`).
pub(crate) fn keyword_text(raw: Option<&str>, field: &str) -> Result<String, ToolError> {
    let t = raw
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| invalid(format!("Provide {field}.")).with_field(field))?;
    if t.chars().count() > MAX_TEXT_CHARS {
        return Err(invalid(format!(
            "{field}: a keyword can have at most {MAX_TEXT_CHARS} characters ('{t}')."
        ))
        .with_field(field));
    }
    if t.split_whitespace().count() > MAX_TEXT_WORDS {
        return Err(invalid(format!(
            "{field}: a keyword can have at most {MAX_TEXT_WORDS} words ('{t}')."
        ))
        .with_field(field));
    }
    Ok(t.to_string())
}

/// Required input value; when missing, `INVALID_INPUT` naming the field.
pub(crate) fn required<'a, T>(v: &'a Option<T>, field: &str) -> Result<&'a T, ToolError> {
    v.as_ref()
        .ok_or_else(|| invalid(format!("Provide {field}.")).with_field(field))
}

/// Required numeric id.
fn required_id(v: &Option<String>, field: &str) -> Result<String, ToolError> {
    valid_id(required(v, field)?, field)
}

/// Temporary name of a child criterion of `parent_id`: `customers/{c}/{collection}/{parent}~-N`.
pub(crate) fn criterion_temp(
    ctx: &Ctx,
    temps: &mut TempIds,
    collection: &str,
    parent_id: &str,
) -> String {
    let t = temps.next(collection);
    let n = t.rsplit('/').next().unwrap_or_default();
    resources::rn(&ctx.customer_id, collection, &format!("{parent_id}~{n}"))
}

/// `{"<x>Operation": {"create": {resourceName: rn, ..body}}}`.
pub(crate) fn create_op(rn: &str, mut body: Value) -> Result<Value, ToolError> {
    body["resourceName"] = json!(rn);
    let key = resources::op_key(rn).map_err(internal)?;
    Ok(json!({ key: { "create": body } }))
}

/// Body of a keyword criterion: parent (`adGroup`, `campaign` or `sharedSet`) and
/// `keyword {text, matchType}`; `negative: true` for a negative keyword.
pub(crate) fn keyword_body(
    parent_field: &str,
    parent_rn: &str,
    text: &str,
    match_type: &str,
    negative: bool,
) -> Value {
    let mut body = json!({
        parent_field: parent_rn,
        "keyword": { "text": text, "matchType": match_type },
    });
    if negative {
        body["negative"] = json!(true);
    }
    body
}

fn unique(ids: &mut Vec<String>, id: &str, field: &str) -> Result<(), ToolError> {
    if ids.iter().any(|i| i == id) {
        return Err(invalid(format!("Criterion {id} appears more than once.")).with_field(field));
    }
    ids.push(id.to_string());
    Ok(())
}

/// Short list for the summary: up to 5 items and "and N more".
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

fn described(text: &str, match_type: &str) -> String {
    format!("'{text}' [{match_type}]")
}

/// At most [`MAX_KEYWORDS`] per call.
fn at_most_max(n: usize) -> Result<(), ToolError> {
    if n > MAX_KEYWORDS {
        return Err(invalid(format!(
            "At most {MAX_KEYWORDS} keywords per call (got {n})."
        ))
        .with_field("keywords"));
    }
    Ok(())
}

/// Between 1 and [`MAX_KEYWORDS`] per call.
fn limit_keywords(n: usize) -> Result<(), ToolError> {
    if n == 0 {
        return Err(invalid("Provide at least one keyword.").with_field("keywords"));
    }
    at_most_max(n)
}

// --------------------------------------------------------- manage_keywords

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum KeywordAction {
    Add,
    Remove,
    Pause,
    Enable,
    SetBid,
    ChangeMatchType,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct KeywordIn {
    /// `add`: keyword text (up to 80 characters and 10 words).
    #[serde(default)]
    text: Option<String>,
    /// `add`: match type. `change_match_type`: the new match type.
    /// `EXACT`, `PHRASE` or `BROAD` (lowercase also works).
    #[serde(default)]
    #[schemars(with = "Option<MatchType>")]
    match_type: Option<String>,
    /// Every action except `add`: criterion id of the keyword in the ad group.
    #[serde(default)]
    criterion_id: Option<String>,
    /// `add` (optional) and `set_bid`: CPC bid, in the account currency.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ManageKeywordsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    ad_group_id: String,
    /// `add`, `remove`, `pause`, `enable`, `set_bid` or `change_match_type`.
    action: KeywordAction,
    /// Up to 200 per call.
    keywords: Vec<KeywordIn>,
}

struct ManageKeywords;

#[async_trait]
impl WriteTool for ManageKeywords {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ManageKeywordsInput>(
            MANAGE_KEYWORDS,
            "Manages the keywords of a Google Ads ad group: add (keywords are created \
             paused), remove, pause, enable, set the bid or change the match type. The \
             remove and change_match_type actions cannot be undone.",
            ToolGroup::Edit,
            true,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ManageKeywordsInput = parse_input(input)?;
        let ag = valid_id(&input.ad_group_id, "ad_group_id")?;
        limit_keywords(input.keywords.len())?;
        if input.action == KeywordAction::Add {
            return add_keywords(
                ctx,
                &ag,
                &input.keywords,
                &mut TempIds::new(&ctx.customer_id),
            );
        }
        let found = existing_keywords(ctx, &ag, &input.keywords).await?;
        let head = format!("Ad group {ag}");
        match input.action {
            KeywordAction::Add => unreachable!("handled above"),
            KeywordAction::Pause | KeywordAction::Enable => {
                let status = if input.action == KeywordAction::Pause {
                    EditStatus::Paused
                } else {
                    EditStatus::Active
                };
                let google = status.to_google()?;
                let mut edits = Edits::new(&head, &ctx.currency);
                for (rn, row) in &found {
                    let label = format!("status of {}", quoted(row));
                    edits.set(
                        rn,
                        "ad_group_criterion.status",
                        row,
                        json!(google),
                        &label,
                        Show::Status,
                    )?;
                }
                edits.into_plan(None)
            }
            KeywordAction::SetBid => {
                let mut edits = Edits::new(&head, &ctx.currency);
                for (i, ((rn, row), kw)) in found.iter().zip(&input.keywords).enumerate() {
                    let field = format!("keywords[{i}].cpc_bid");
                    let bid = *required(&kw.cpc_bid, &field)?;
                    let label = format!("bid of {}", quoted(row));
                    edits.money(
                        rn,
                        "ad_group_criterion.cpc_bid_micros",
                        row,
                        bid,
                        &field,
                        &label,
                    )?;
                }
                edits.into_plan(None)
            }
            KeywordAction::Remove => {
                let mut operations = Vec::new();
                let mut names = Vec::new();
                for (rn, row) in &found {
                    operations.push(resources::remove_op(rn).map_err(internal)?);
                    names.push(quoted(row));
                }
                Ok(irreversible(
                    format!(
                        "Removes {} keyword(s) from ad group {ag}: {}",
                        names.len(),
                        list(&names)
                    ),
                    IRREVERSIBLE_WARNING,
                    operations,
                    Vec::new(),
                ))
            }
            KeywordAction::ChangeMatchType => change_match_types(ctx, &ag, &input.keywords, &found),
        }
    }
}

/// Plan with no inverse, with a warning.
fn irreversible(
    summary: String,
    warning: &str,
    operations: Vec<Value>,
    watch: Vec<Watch>,
) -> WritePlan {
    WritePlan {
        summary,
        warning: Some(warning.to_string()),
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::None,
        direct: None,
    }
}

/// `'text'` of the row read from `ad_group_criterion`.
fn quoted(row: &Value) -> String {
    format!(
        "'{}'",
        text(row, "adGroupCriterion.keyword.text").unwrap_or_default()
    )
}

fn add_keywords(
    ctx: &Ctx,
    ag: &str,
    keywords: &[KeywordIn],
    temps: &mut TempIds,
) -> Result<WritePlan, ToolError> {
    let ag_rn = resources::rn(&ctx.customer_id, "adGroups", ag);
    let mut operations = Vec::new();
    let mut watch = Vec::new();
    let mut order = Vec::new();
    let mut names = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let text = keyword_text(kw.text.as_deref(), &format!("keywords[{i}].text"))?;
        let mt = match_type(&kw.match_type, &format!("keywords[{i}].match_type"))?;
        let mut body = keyword_body("adGroup", &ag_rn, &text, mt.to_google(), false);
        body["status"] = json!("PAUSED");
        if let Some(bid) = kw.cpc_bid {
            let micros = positive_micros(bid, &ctx.currency, &format!("keywords[{i}].cpc_bid"))?;
            body["cpcBidMicros"] = json!(micros.to_string());
        }
        let rn = criterion_temp(ctx, temps, "adGroupCriteria", ag);
        operations.push(create_op(&rn, body)?);
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: "ad_group_criterion",
            fields: vec!["ad_group_criterion.status"],
        });
        order.push(rn);
        names.push(described(&text, mt.to_google()));
    }
    Ok(WritePlan {
        summary: format!(
            "Adds {} keyword(s) to ad group {ag} (created paused): {}",
            names.len(),
            list(&names)
        ),
        warning: None,
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::RemoveCreated { order },
        direct: None,
    })
}

/// Rows (`adGroupCriterion`) of the given keywords, in input order, with the resource
/// name. A repeated id is `INVALID_INPUT`; an id not in the ad group is `NOT_FOUND`.
async fn existing_keywords(
    ctx: &Ctx,
    ag: &str,
    keywords: &[KeywordIn],
) -> Result<Vec<(String, Value)>, ToolError> {
    let mut ids: Vec<String> = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let field = format!("keywords[{i}].criterion_id");
        let id = required_id(&kw.criterion_id, &field)?;
        unique(&mut ids, &id, &field)?;
    }
    let query = format!(
        "SELECT ad_group_criterion.resource_name, ad_group_criterion.criterion_id, \
         ad_group_criterion.status, ad_group_criterion.cpc_bid_micros, \
         ad_group_criterion.keyword.text, ad_group_criterion.keyword.match_type \
         FROM ad_group_criterion WHERE ad_group.id = {ag} \
         AND ad_group_criterion.criterion_id IN ({}) \
         AND ad_group_criterion.type = 'KEYWORD' AND ad_group_criterion.negative = false \
         AND ad_group_criterion.status != 'REMOVED'",
        ids.join(", ")
    );
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
        .await
        .map_err(|e| translate(e, Some(ctx), MANAGE_KEYWORDS))?;
    ids.iter()
        .enumerate()
        .map(|(i, id)| {
            let rn = resources::rn(&ctx.customer_id, "adGroupCriteria", &format!("{ag}~{id}"));
            page.results
                .iter()
                .find(|r| text(r, "adGroupCriterion.resourceName").as_deref() == Some(&rn))
                .map(|r| (rn, r.clone()))
                .ok_or_else(|| {
                    ToolError::new(
                        codes::NOT_FOUND,
                        format!("Keyword {id} not found in ad group {ag}."),
                    )
                    .with_field(format!("keywords[{i}].criterion_id"))
                })
        })
        .collect()
}

/// Creates the criterion with the new match type and removes the old one, in the same
/// mutate. The new keyword inherits the old one's status and bid (it is a replacement,
/// not an extra keyword).
fn change_match_types(
    ctx: &Ctx,
    ag: &str,
    keywords: &[KeywordIn],
    found: &[(String, Value)],
) -> Result<WritePlan, ToolError> {
    let ag_rn = resources::rn(&ctx.customer_id, "adGroups", ag);
    let mut temps = TempIds::new(&ctx.customer_id);
    let mut operations = Vec::new();
    let mut watch = Vec::new();
    let mut names = Vec::new();
    for (i, ((old_rn, row), kw)) in found.iter().zip(keywords).enumerate() {
        let new_type = match_type(&kw.match_type, &format!("keywords[{i}].match_type"))?;
        let old_type = text(row, "adGroupCriterion.keyword.matchType").unwrap_or_default();
        let kw_text = text(row, "adGroupCriterion.keyword.text").unwrap_or_default();
        if old_type == new_type.to_google() {
            return Err(
                invalid(format!("Keyword '{kw_text}' is already {old_type}."))
                    .with_field(format!("keywords[{i}].match_type")),
            );
        }
        let mut body = keyword_body("adGroup", &ag_rn, &kw_text, new_type.to_google(), false);
        body["status"] =
            json!(text(row, "adGroupCriterion.status").unwrap_or_else(|| "PAUSED".into()));
        if let Some(bid) = at(row, "adGroupCriterion.cpcBidMicros") {
            body["cpcBidMicros"] = bid.clone();
        }
        let rn = criterion_temp(ctx, &mut temps, "adGroupCriteria", ag);
        operations.push(create_op(&rn, body)?);
        operations.push(resources::remove_op(old_rn).map_err(internal)?);
        watch.push(Watch {
            resource_name: rn,
            resource: "ad_group_criterion",
            fields: vec!["ad_group_criterion.status"],
        });
        names.push(format!("'{kw_text}' {old_type} → {}", new_type.to_google()));
    }
    Ok(irreversible(
        format!(
            "Changes the match type of {} keyword(s) in ad group {ag}: {}",
            names.len(),
            list(&names)
        ),
        MATCH_TYPE_WARNING,
        operations,
        watch,
    ))
}

// ------------------------------------------------------- manage_negatives

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum NegativeLevel {
    Campaign,
    AdGroup,
    SharedList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum NegativeAction {
    Add,
    Remove,
    CreateList,
    AttachList,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NegativeIn {
    /// `add` and `create_list`: negative keyword text (up to 80 characters and 10 words).
    #[serde(default)]
    text: Option<String>,
    /// `add` and `create_list`: `EXACT`, `PHRASE` or `BROAD`.
    #[serde(default)]
    #[schemars(with = "Option<MatchType>")]
    match_type: Option<String>,
    /// `remove`: criterion id of the negative keyword.
    #[serde(default)]
    criterion_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ManageNegativesInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// `add` and `remove`: where the negative keyword lives. `create_list` is always
    /// `shared_list` and `attach_list` is always `campaign`.
    #[serde(default)]
    level: Option<NegativeLevel>,
    /// `level=campaign` and `attach_list`; with `create_list`, attaches the new list to this campaign.
    #[serde(default)]
    campaign_id: Option<String>,
    /// `level=ad_group`.
    #[serde(default)]
    ad_group_id: Option<String>,
    /// `level=shared_list` and `attach_list`.
    #[serde(default)]
    shared_set_id: Option<String>,
    /// `add`, `remove`, `create_list` or `attach_list`.
    action: NegativeAction,
    /// Up to 200 per call.
    #[serde(default)]
    keywords: Option<Vec<NegativeIn>>,
    /// `create_list`: list name.
    #[serde(default)]
    list_name: Option<String>,
}

/// Where the negative keywords of a level live.
struct Level {
    collection: &'static str,
    parent_field: &'static str,
    parent_collection: &'static str,
    parent_id: String,
    negative: bool,
    resource: &'static str,
    watch_fields: Vec<&'static str>,
    /// GAQL filter of the parent (`campaign.id`, …) and of the resource type read.
    parent_filter: &'static str,
    /// Extra `remove` condition: only keyword negatives not yet removed.
    remove_where: &'static str,
    label: String,
}

impl Level {
    fn parent_rn(&self, ctx: &Ctx) -> String {
        resources::rn(&ctx.customer_id, self.parent_collection, &self.parent_id)
    }
}

fn level_of(input: &ManageNegativesInput) -> Result<Level, ToolError> {
    Ok(match *required(&input.level, "level")? {
        NegativeLevel::Campaign => {
            let id = required_id(&input.campaign_id, "campaign_id")?;
            Level {
                collection: "campaignCriteria",
                parent_field: "campaign",
                parent_collection: "campaigns",
                label: format!("campaign {id}"),
                parent_id: id,
                negative: true,
                resource: "campaign_criterion",
                watch_fields: vec!["campaign_criterion.negative", "campaign_criterion.status"],
                parent_filter: "campaign.id",
                remove_where: "campaign_criterion.negative = TRUE AND campaign_criterion.type = 'KEYWORD' AND campaign_criterion.status != 'REMOVED'",
            }
        }
        NegativeLevel::AdGroup => {
            let id = required_id(&input.ad_group_id, "ad_group_id")?;
            Level {
                collection: "adGroupCriteria",
                parent_field: "adGroup",
                parent_collection: "adGroups",
                label: format!("ad group {id}"),
                parent_id: id,
                negative: true,
                resource: "ad_group_criterion",
                watch_fields: vec!["ad_group_criterion.negative", "ad_group_criterion.status"],
                parent_filter: "ad_group.id",
                remove_where: "ad_group_criterion.negative = TRUE AND ad_group_criterion.type = 'KEYWORD' AND ad_group_criterion.status != 'REMOVED'",
            }
        }
        NegativeLevel::SharedList => {
            let id = required_id(&input.shared_set_id, "shared_set_id")?;
            Level {
                collection: "sharedCriteria",
                parent_field: "sharedSet",
                parent_collection: "sharedSets",
                label: format!("shared list {id}"),
                parent_id: id,
                negative: false,
                resource: "shared_criterion",
                watch_fields: vec!["shared_criterion.keyword.text"],
                parent_filter: "shared_set.id",
                remove_where: "shared_criterion.type = 'KEYWORD'",
            }
        }
    })
}

/// Creates the negative keyword criteria of `lvl` under `parent_rn` (which may be
/// temporary). Returns the temporary names in creation order.
#[allow(clippy::too_many_arguments)]
fn negative_creates(
    ctx: &Ctx,
    temps: &mut TempIds,
    lvl: &Level,
    parent_rn: &str,
    parent_key: &str,
    keywords: &[NegativeIn],
    operations: &mut Vec<Value>,
    watch: &mut Vec<Watch>,
    names: &mut Vec<String>,
) -> Result<Vec<String>, ToolError> {
    let mut created = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let text = keyword_text(kw.text.as_deref(), &format!("keywords[{i}].text"))?;
        let mt = match_type(&kw.match_type, &format!("keywords[{i}].match_type"))?;
        let body = keyword_body(
            lvl.parent_field,
            parent_rn,
            &text,
            mt.to_google(),
            lvl.negative,
        );
        let rn = criterion_temp(ctx, temps, lvl.collection, parent_key);
        operations.push(create_op(&rn, body)?);
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: lvl.resource,
            fields: lvl.watch_fields.clone(),
        });
        created.push(rn);
        names.push(described(&text, mt.to_google()));
    }
    Ok(created)
}

struct ManageNegatives;

#[async_trait]
impl WriteTool for ManageNegatives {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ManageNegativesInput>(
            MANAGE_NEGATIVES,
            "Manages Google Ads negative keywords: add or remove them on a campaign, an ad \
             group or a shared list, create a shared negative keyword list, and attach a \
             list to a campaign. The remove action cannot be undone.",
            ToolGroup::Edit,
            true,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ManageNegativesInput = parse_input(input)?;
        let keywords = input.keywords.as_deref().unwrap_or_default();
        match input.action {
            NegativeAction::Add => {
                let lvl = level_of(&input)?;
                limit_keywords(keywords.len())?;
                let parent_rn = lvl.parent_rn(ctx);
                let (mut ops, mut watch, mut names) = (Vec::new(), Vec::new(), Vec::new());
                let order = negative_creates(
                    ctx,
                    &mut TempIds::new(&ctx.customer_id),
                    &lvl,
                    &parent_rn,
                    &lvl.parent_id,
                    keywords,
                    &mut ops,
                    &mut watch,
                    &mut names,
                )?;
                Ok(WritePlan {
                    summary: format!(
                        "Adds {} negative keyword(s) to {}: {}",
                        names.len(),
                        lvl.label,
                        list(&names)
                    ),
                    warning: None,
                    operations: ops,
                    field_changes: Vec::new(),
                    watch,
                    inverse: InverseSpec::RemoveCreated { order },
                    direct: None,
                })
            }
            NegativeAction::Remove => {
                let lvl = level_of(&input)?;
                limit_keywords(keywords.len())?;
                let found = existing_negatives(ctx, &lvl, keywords).await?;
                let operations = found
                    .iter()
                    .map(|(rn, _)| resources::remove_op(rn).map_err(internal))
                    .collect::<Result<Vec<_>, _>>()?;
                let names: Vec<String> = found.iter().map(|(_, d)| d.clone()).collect();
                Ok(irreversible(
                    format!(
                        "Removes {} negative keyword(s) from {}: {}",
                        names.len(),
                        lvl.label,
                        list(&names)
                    ),
                    IRREVERSIBLE_WARNING,
                    operations,
                    Vec::new(),
                ))
            }
            NegativeAction::CreateList => {
                // The list may be created empty (no keywords), so at least one is not required.
                at_most_max(keywords.len())?;
                create_list(ctx, &input, keywords)
            }
            NegativeAction::AttachList => attach_list(ctx, &input),
        }
    }
}

/// Keyword negatives of `lvl` with the given ids, in input order: resource name and
/// `'text' [TYPE]`. An id that is not a keyword negative (a positive keyword, a
/// location, a device, or already removed) is `NOT_FOUND`, so `remove` never deletes
/// another kind of criterion.
async fn existing_negatives(
    ctx: &Ctx,
    lvl: &Level,
    keywords: &[NegativeIn],
) -> Result<Vec<(String, String)>, ToolError> {
    let mut ids: Vec<String> = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let field = format!("keywords[{i}].criterion_id");
        let id = required_id(&kw.criterion_id, &field)?;
        unique(&mut ids, &id, &field)?;
    }
    let r = lvl.resource;
    let query = format!(
        "SELECT {r}.resource_name, {r}.criterion_id, {r}.keyword.text, {r}.keyword.match_type \
         FROM {r} WHERE {} = {} AND {r}.criterion_id IN ({}) AND {}",
        lvl.parent_filter,
        lvl.parent_id,
        ids.join(", "),
        lvl.remove_where
    );
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
        .await
        .map_err(|e| translate(e, Some(ctx), MANAGE_NEGATIVES))?;
    let root = fields::camel(r);
    ids.iter()
        .enumerate()
        .map(|(i, id)| {
            let rn = resources::rn(
                &ctx.customer_id,
                lvl.collection,
                &format!("{}~{id}", lvl.parent_id),
            );
            page.results
                .iter()
                .find(|row| text(row, &format!("{root}.resourceName")).as_deref() == Some(&rn))
                .map(|row| {
                    let t = text(row, &format!("{root}.keyword.text")).unwrap_or_default();
                    let m = text(row, &format!("{root}.keyword.matchType")).unwrap_or_default();
                    (rn, described(&t, &m))
                })
                .ok_or_else(|| {
                    ToolError::new(
                        codes::NOT_FOUND,
                        format!("Negative keyword {id} not found in {}.", lvl.label),
                    )
                    .with_field(format!("keywords[{i}].criterion_id"))
                })
        })
        .collect()
}

/// The given `level` must match the action (`create_list` and `attach_list` fix it).
fn expect_level(input: &ManageNegativesInput, expected: NegativeLevel) -> Result<(), ToolError> {
    match input.level {
        Some(l) if l != expected => Err(invalid(format!(
            "For {} the level is always {}.",
            if expected == NegativeLevel::SharedList {
                "create_list"
            } else {
                "attach_list"
            },
            if expected == NegativeLevel::SharedList {
                "shared_list"
            } else {
                "campaign"
            }
        ))
        .with_field("level")),
        _ => Ok(()),
    }
}

fn create_list(
    ctx: &Ctx,
    input: &ManageNegativesInput,
    keywords: &[NegativeIn],
) -> Result<WritePlan, ToolError> {
    expect_level(input, NegativeLevel::SharedList)?;
    let name = required(&input.list_name, "list_name")?.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_LIST_NAME_CHARS {
        return Err(invalid(format!(
            "list_name must have 1 to {MAX_LIST_NAME_CHARS} characters."
        ))
        .with_field("list_name"));
    }
    let campaign_id = match &input.campaign_id {
        Some(_) => Some(required_id(&input.campaign_id, "campaign_id")?),
        None => None,
    };
    let mut temps = TempIds::new(&ctx.customer_id);
    let set_rn = temps.next("sharedSets");
    let set_id = set_rn.rsplit('/').next().unwrap_or_default().to_string();
    let mut operations = vec![create_op(
        &set_rn,
        json!({ "name": name, "type": "NEGATIVE_KEYWORDS" }),
    )?];
    let mut watch = vec![Watch {
        resource_name: set_rn.clone(),
        resource: "shared_set",
        fields: vec!["shared_set.name", "shared_set.status"],
    }];
    let lvl = Level {
        collection: "sharedCriteria",
        parent_field: "sharedSet",
        parent_collection: "sharedSets",
        parent_id: set_id.clone(),
        negative: false,
        resource: "shared_criterion",
        watch_fields: vec!["shared_criterion.keyword.text"],
        parent_filter: "shared_set.id",
        remove_where: "shared_criterion.type = 'KEYWORD'",
        label: String::new(),
    };
    let mut names = Vec::new();
    let mut order = negative_creates(
        ctx,
        &mut temps,
        &lvl,
        &set_rn,
        &set_id,
        keywords,
        &mut operations,
        &mut watch,
        &mut names,
    )?;
    let mut summary = format!(
        "Creates negative keyword list '{name}' with {} keyword(s)",
        names.len()
    );
    if let Some(cid) = campaign_id {
        let rn = resources::rn(
            &ctx.customer_id,
            "campaignSharedSets",
            &format!("{cid}~{set_id}"),
        );
        operations.push(create_op(
            &rn,
            json!({
                "campaign": resources::rn(&ctx.customer_id, "campaigns", &cid),
                "sharedSet": set_rn,
            }),
        )?);
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: "campaign_shared_set",
            fields: vec!["campaign_shared_set.campaign", "campaign_shared_set.status"],
        });
        order.push(rn);
        summary.push_str(&format!(" and attaches it to campaign {cid}"));
    }
    order.push(set_rn);
    if !names.is_empty() {
        summary.push_str(&format!(": {}", list(&names)));
    }
    Ok(WritePlan {
        summary,
        warning: None,
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::RemoveCreated { order },
        direct: None,
    })
}

fn attach_list(ctx: &Ctx, input: &ManageNegativesInput) -> Result<WritePlan, ToolError> {
    expect_level(input, NegativeLevel::Campaign)?;
    let cid = required_id(&input.campaign_id, "campaign_id")?;
    let sid = required_id(&input.shared_set_id, "shared_set_id")?;
    // The name is deterministic (`{campaign}~{list}`), so it needs no temporary id.
    let rn = resources::rn(
        &ctx.customer_id,
        "campaignSharedSets",
        &format!("{cid}~{sid}"),
    );
    let key = resources::op_key(&rn).map_err(internal)?;
    let operation = json!({ key: { "create": {
        "campaign": resources::rn(&ctx.customer_id, "campaigns", &cid),
        "sharedSet": resources::rn(&ctx.customer_id, "sharedSets", &sid),
    } } });
    Ok(WritePlan {
        summary: format!("Attaches negative keyword list {sid} to campaign {cid}"),
        warning: None,
        operations: vec![operation],
        field_changes: Vec::new(),
        watch: vec![Watch {
            resource_name: rn.clone(),
            resource: "campaign_shared_set",
            fields: vec!["campaign_shared_set.campaign", "campaign_shared_set.status"],
        }],
        inverse: InverseSpec::RemoveCreated { order: vec![rn] },
        direct: None,
    })
}
