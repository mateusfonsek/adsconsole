//! Targeting: `manage_targeting` (location, language, device, ad schedule, audience,
//! placement and topic) as campaign or ad group criteria.
//!
//! `add` is reversible (`RemoveCreated`), `set_bid_modifier` is a `FieldChange` on
//! `*_criterion.bid_modifier` (`RestoreFields`) and `remove` is irreversible. Remove
//! and bid changes read the criterion first and require the id to be of the requested type.
//!
//! The per-type builders (`location_criterion`, …) are `pub(crate)` for the targeting
//! of Display Network ad groups.

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::resources::{self, TempIds};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::edit::{Edits, Show};
use super::keywords::{IRREVERSIBLE_WARNING, create_op, criterion_temp, required};
use super::reports::{invalid, text, valid_id};
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![GTool::Write(Box::new(ManageTargeting))]
}

const MANAGE_TARGETING: &str = "manage_targeting";
const MAX_CRITERIA: usize = 200;
const SUMMARY_ITEMS: usize = 5;
const BID_MIN: f64 = 0.1;
const BID_MAX: f64 = 10.0;

// --------------------------------------------------------------- builders

/// Bid modifier between 0.1 and 10.0 (`allow_zero`: 0 is also valid, the device exclusion).
pub(crate) fn bid_modifier_value(v: f64, allow_zero: bool, field: &str) -> Result<f64, ToolError> {
    if v.is_finite() && ((BID_MIN..=BID_MAX).contains(&v) || (allow_zero && v == 0.0)) {
        return Ok(v);
    }
    let zero = if allow_zero { " (or 0 to exclude)" } else { "" };
    Err(invalid(format!(
        "{field} must be between {BID_MIN} and {BID_MAX}{zero}."
    ))
    .with_field(field))
}

/// `{location: {geoTargetConstant}}` (+ `negative`). `field` is the item prefix
/// (e.g. `criteria[0]`) used in errors.
pub(crate) fn location_criterion(
    geo_id: Option<&str>,
    negative: bool,
    field: &str,
) -> Result<Value, ToolError> {
    let id = id_of(geo_id, &format!("{field}.geo_id"))?;
    let mut body =
        json!({ "location": { "geoTargetConstant": format!("geoTargetConstants/{id}") } });
    if negative {
        body["negative"] = json!(true);
    }
    Ok(body)
}

pub(crate) fn language_criterion(
    language_id: Option<&str>,
    field: &str,
) -> Result<Value, ToolError> {
    let id = id_of(language_id, &format!("{field}.language_id"))?;
    Ok(json!({ "language": { "languageConstant": format!("languageConstants/{id}") } }))
}

/// Device `MOBILE`, `DESKTOP` or `TABLET` with a required bid modifier (0 excludes it).
pub(crate) fn device_criterion(
    device: Option<&str>,
    bid_modifier: Option<f64>,
    field: &str,
) -> Result<Value, ToolError> {
    let f = format!("{field}.device");
    let raw = required(&device, &f)?.trim().to_uppercase();
    if !["MOBILE", "DESKTOP", "TABLET"].contains(&raw.as_str()) {
        return Err(invalid(format!(
            "{f}: unknown device '{raw}'; use MOBILE, DESKTOP or TABLET."
        ))
        .with_field(f));
    }
    let bf = format!("{field}.bid_modifier");
    let bid = bid_modifier_value(*required(&bid_modifier, &bf)?, true, &bf)?;
    Ok(json!({ "device": { "type": raw }, "bidModifier": bid }))
}

/// Ad schedule: day, `start_hour` 0..=23, `end_hour` 1..=24 and `start < end`; minutes `ZERO`.
pub(crate) fn ad_schedule_criterion(
    day: Option<&str>,
    start_hour: Option<i64>,
    end_hour: Option<i64>,
    bid_modifier: Option<f64>,
    field: &str,
) -> Result<Value, ToolError> {
    let df = format!("{field}.day");
    let day = required(&day, &df)?.trim().to_uppercase();
    if ![
        "MONDAY",
        "TUESDAY",
        "WEDNESDAY",
        "THURSDAY",
        "FRIDAY",
        "SATURDAY",
        "SUNDAY",
    ]
    .contains(&day.as_str())
    {
        return Err(
            invalid(format!("{df}: unknown day '{day}'; use MONDAY to SUNDAY.")).with_field(df),
        );
    }
    let sf = format!("{field}.start_hour");
    let ef = format!("{field}.end_hour");
    let start = *required(&start_hour, &sf)?;
    let end = *required(&end_hour, &ef)?;
    if !(0..=23).contains(&start) {
        return Err(invalid(format!("{sf} must be between 0 and 23.")).with_field(sf));
    }
    if !(1..=24).contains(&end) {
        return Err(invalid(format!("{ef} must be between 1 and 24.")).with_field(ef));
    }
    if start >= end {
        return Err(
            invalid(format!("{ef} must be greater than start_hour ({start}).")).with_field(ef),
        );
    }
    let mut body = json!({ "adSchedule": {
        "dayOfWeek": day, "startHour": start, "startMinute": "ZERO",
        "endHour": end, "endMinute": "ZERO" } });
    if let Some(b) = bid_modifier {
        body["bidModifier"] = json!(bid_modifier_value(
            b,
            false,
            &format!("{field}.bid_modifier")
        )?);
    }
    Ok(body)
}

pub(crate) fn audience_criterion(
    ctx: &Ctx,
    user_list_id: Option<&str>,
    field: &str,
) -> Result<Value, ToolError> {
    let id = id_of(user_list_id, &format!("{field}.user_list_id"))?;
    Ok(json!({ "userList": {
        "userList": resources::rn(&ctx.customer_id, "userLists", &id) } }))
}

/// Placement: `http(s)://…` URL or domain (`example.com`).
pub(crate) fn placement_criterion(url: Option<&str>, field: &str) -> Result<Value, ToolError> {
    let f = format!("{field}.url");
    let url = required(&url, &f)?.trim();
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let name = host.split(['/', '?', '#']).next().unwrap_or_default();
    let ok = !url.contains(char::is_whitespace)
        && !name.is_empty()
        && name.contains('.')
        && !name.starts_with(['.', '-'])
        && !name.ends_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-');
    if !ok {
        return Err(invalid(format!(
            "{f}: provide an http(s) URL or a domain (got '{url}')."
        ))
        .with_field(f));
    }
    Ok(json!({ "placement": { "url": url } }))
}

pub(crate) fn topic_criterion(topic_id: Option<&str>, field: &str) -> Result<Value, ToolError> {
    let id = id_of(topic_id, &format!("{field}.topic_id"))?;
    Ok(json!({ "topic": { "topicConstant": format!("topicConstants/{id}") } }))
}

/// Adds the parent (`campaign`, `adGroup`) to a criterion body.
pub(crate) fn with_parent(mut body: Value, parent_field: &str, parent_rn: &str) -> Value {
    body[parent_field] = json!(parent_rn);
    body
}

fn id_of(v: Option<&str>, field: &str) -> Result<String, ToolError> {
    valid_id(required(&v, field)?, field)
}

// -------------------------------------------------------------------- input

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Action {
    Add,
    Remove,
    SetBidModifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Location,
    Language,
    Device,
    AdSchedule,
    Audience,
    Placement,
    Topic,
}

impl Kind {
    /// The criterion's API `type`.
    fn api_type(self) -> &'static str {
        match self {
            Kind::Location => "LOCATION",
            Kind::Language => "LANGUAGE",
            Kind::Device => "DEVICE",
            Kind::AdSchedule => "AD_SCHEDULE",
            Kind::Audience => "USER_LIST",
            Kind::Placement => "PLACEMENT",
            Kind::Topic => "TOPIC",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Location => "location",
            Kind::Language => "language",
            Kind::Device => "device",
            Kind::AdSchedule => "ad schedule",
            Kind::Audience => "audience",
            Kind::Placement => "placement",
            Kind::Topic => "topic",
        }
    }
    fn allows_ad_group(self) -> bool {
        matches!(self, Kind::Audience | Kind::Placement | Kind::Topic)
    }
    fn has_bid_modifier(self) -> bool {
        self != Kind::Language
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CriterionIn {
    /// `location`, `language`, `device`, `ad_schedule`, `audience`, `placement` or `topic`.
    #[serde(rename = "type")]
    kind: Kind,
    /// `location`: location id (`geo_target_constant`).
    #[serde(default)]
    geo_id: Option<String>,
    /// `location`: `true` excludes the location.
    #[serde(default)]
    negative: Option<bool>,
    /// `language`: language id (`language_constant`).
    #[serde(default)]
    language_id: Option<String>,
    /// `device`: `MOBILE`, `DESKTOP` or `TABLET`.
    #[serde(default)]
    device: Option<String>,
    /// `device` (required), `ad_schedule` (optional) and `set_bid_modifier`: 0.1 to
    /// 10.0 (a device also accepts 0, which excludes it).
    #[serde(default)]
    bid_modifier: Option<f64>,
    /// `ad_schedule`: `MONDAY` to `SUNDAY`.
    #[serde(default)]
    day: Option<String>,
    /// `ad_schedule`: 0 to 23.
    #[serde(default)]
    start_hour: Option<i64>,
    /// `ad_schedule`: 1 to 24, greater than `start_hour`.
    #[serde(default)]
    end_hour: Option<i64>,
    /// `audience`: user list id.
    #[serde(default)]
    user_list_id: Option<String>,
    /// `placement`: http(s) URL or domain.
    #[serde(default)]
    url: Option<String>,
    /// `topic`: topic id.
    #[serde(default)]
    topic_id: Option<String>,
    /// `remove` and `set_bid_modifier`: id of the existing criterion.
    #[serde(default)]
    criterion_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ManageTargetingInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    campaign_id: String,
    /// With it, the criterion goes on the ad group (only `audience`, `placement` and
    /// `topic`); without it, on the campaign.
    #[serde(default)]
    ad_group_id: Option<String>,
    /// `add`, `remove` or `set_bid_modifier`.
    action: Action,
    /// 1 to 200 per call.
    criteria: Vec<CriterionIn>,
}

/// Where the criteria live.
struct Level {
    collection: &'static str,
    resource: &'static str,
    parent_field: &'static str,
    parent_collection: &'static str,
    parent_filter: &'static str,
    bid_field: &'static str,
    parent_id: String,
    label: String,
}

impl Level {
    fn of(input: &ManageTargetingInput) -> Result<Level, ToolError> {
        let campaign = valid_id(&input.campaign_id, "campaign_id")?;
        Ok(match &input.ad_group_id {
            None => Level {
                collection: "campaignCriteria",
                resource: "campaign_criterion",
                parent_field: "campaign",
                parent_collection: "campaigns",
                parent_filter: "campaign.id",
                bid_field: "campaign_criterion.bid_modifier",
                label: format!("campaign {campaign}"),
                parent_id: campaign,
            },
            Some(ag) => {
                let ag = valid_id(ag, "ad_group_id")?;
                Level {
                    collection: "adGroupCriteria",
                    resource: "ad_group_criterion",
                    parent_field: "adGroup",
                    parent_collection: "adGroups",
                    parent_filter: "ad_group.id",
                    bid_field: "ad_group_criterion.bid_modifier",
                    label: format!("ad group {ag}"),
                    parent_id: ag,
                }
            }
        })
    }

    fn is_ad_group(&self) -> bool {
        self.resource == "ad_group_criterion"
    }

    /// The criterion's `type` field in the row read (camelCase).
    fn type_path(&self) -> String {
        format!(
            "{}.type",
            adsconsole_google_ads::fields::camel(self.resource)
        )
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

fn limit(n: usize) -> Result<(), ToolError> {
    if n == 0 {
        return Err(invalid("Provide at least one criterion.").with_field("criteria"));
    }
    if n > MAX_CRITERIA {
        return Err(invalid(format!(
            "At most {MAX_CRITERIA} criteria per call (got {n})."
        ))
        .with_field("criteria"));
    }
    Ok(())
}

// --------------------------------------------------------------------- tool

struct ManageTargeting;

#[async_trait]
impl WriteTool for ManageTargeting {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ManageTargetingInput>(
            MANAGE_TARGETING,
            "Manages the targeting of a Google Ads campaign (or ad group): add, remove or \
             set the bid modifier of location, language, device, ad schedule, audience, \
             placement and topic criteria. Location, language, device and ad schedule apply \
             only at the campaign level. Find location and language ids with `gaql_search` \
             on `geo_target_constant` and `language_constant`. The remove action cannot be \
             undone.",
            ToolGroup::Edit,
            true,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ManageTargetingInput = parse_input(input)?;
        let lvl = Level::of(&input)?;
        limit(input.criteria.len())?;
        match input.action {
            Action::Add => add(ctx, &lvl, &input.criteria),
            Action::Remove => remove(ctx, &lvl, &input.criteria).await,
            Action::SetBidModifier => set_bid_modifier(ctx, &lvl, &input.criteria).await,
        }
    }
}

/// Body (without parent) and description of a new criterion.
fn build(ctx: &Ctx, c: &CriterionIn, i: usize) -> Result<(Value, String), ToolError> {
    let f = format!("criteria[{i}]");
    Ok(match c.kind {
        Kind::Location => (
            location_criterion(c.geo_id.as_deref(), c.negative.unwrap_or(false), &f)?,
            format!(
                "location {}{}",
                c.geo_id.as_deref().unwrap_or_default(),
                if c.negative == Some(true) {
                    " (excluded)"
                } else {
                    ""
                }
            ),
        ),
        Kind::Language => (
            language_criterion(c.language_id.as_deref(), &f)?,
            format!("language {}", c.language_id.as_deref().unwrap_or_default()),
        ),
        Kind::Device => (
            device_criterion(c.device.as_deref(), c.bid_modifier, &f)?,
            format!(
                "device {} (bid modifier {})",
                c.device.as_deref().unwrap_or_default().to_uppercase(),
                c.bid_modifier.unwrap_or_default()
            ),
        ),
        Kind::AdSchedule => (
            ad_schedule_criterion(
                c.day.as_deref(),
                c.start_hour,
                c.end_hour,
                c.bid_modifier,
                &f,
            )?,
            format!(
                "ad schedule {} {}h-{}h",
                c.day.as_deref().unwrap_or_default().to_uppercase(),
                c.start_hour.unwrap_or_default(),
                c.end_hour.unwrap_or_default()
            ),
        ),
        Kind::Audience => (
            audience_criterion(ctx, c.user_list_id.as_deref(), &f)?,
            format!("audience {}", c.user_list_id.as_deref().unwrap_or_default()),
        ),
        Kind::Placement => (
            placement_criterion(c.url.as_deref(), &f)?,
            format!("placement {}", c.url.as_deref().unwrap_or_default().trim()),
        ),
        Kind::Topic => (
            topic_criterion(c.topic_id.as_deref(), &f)?,
            format!("topic {}", c.topic_id.as_deref().unwrap_or_default()),
        ),
    })
}

fn wrong_level(kind: Kind, i: usize) -> ToolError {
    invalid(format!(
        "The '{}' criterion applies only at the campaign level; remove ad_group_id.",
        kind.label()
    ))
    .with_field(format!("criteria[{i}].type"))
}

fn add(ctx: &Ctx, lvl: &Level, criteria: &[CriterionIn]) -> Result<WritePlan, ToolError> {
    let parent_rn = resources::rn(&ctx.customer_id, lvl.parent_collection, &lvl.parent_id);
    let mut temps = TempIds::new(&ctx.customer_id);
    let (mut operations, mut watch, mut order, mut names) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, c) in criteria.iter().enumerate() {
        if lvl.is_ad_group() && !c.kind.allows_ad_group() {
            return Err(wrong_level(c.kind, i));
        }
        let (body, name) = build(ctx, c, i)?;
        let rn = criterion_temp(ctx, &mut temps, lvl.collection, &lvl.parent_id);
        operations.push(create_op(
            &rn,
            with_parent(body, lvl.parent_field, &parent_rn),
        )?);
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: lvl.resource,
            fields: vec![if lvl.is_ad_group() {
                "ad_group_criterion.status"
            } else {
                "campaign_criterion.status"
            }],
        });
        order.push(rn);
        names.push(name);
    }
    Ok(WritePlan {
        summary: format!(
            "Adds {} targeting criterion(s) to {}: {}",
            names.len(),
            lvl.label,
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

/// Rows of the given criteria (`criterion_id`), in input order, with the resource name.
/// The id must exist at the level, not be removed and be of the requested type;
/// otherwise `NOT_FOUND` on `criteria[i].criterion_id` (and nothing is sent).
async fn existing(
    ctx: &Ctx,
    lvl: &Level,
    criteria: &[CriterionIn],
) -> Result<Vec<(String, Value)>, ToolError> {
    let mut ids: Vec<String> = Vec::new();
    for (i, c) in criteria.iter().enumerate() {
        let field = format!("criteria[{i}].criterion_id");
        let id = id_of(c.criterion_id.as_deref(), &field)?;
        if ids.contains(&id) {
            return Err(
                invalid(format!("Criterion {id} appears more than once.")).with_field(field)
            );
        }
        if lvl.is_ad_group() && !c.kind.allows_ad_group() {
            return Err(wrong_level(c.kind, i));
        }
        ids.push(id);
    }
    let types: Vec<String> = {
        let mut t: Vec<&str> = criteria.iter().map(|c| c.kind.api_type()).collect();
        t.sort_unstable();
        t.dedup();
        t.iter().map(|t| format!("'{t}'")).collect()
    };
    let r = lvl.resource;
    let query = format!(
        "SELECT {r}.resource_name, {r}.criterion_id, {r}.type, {r}.negative, {r}.bid_modifier \
         FROM {r} WHERE {} = {} AND {r}.criterion_id IN ({}) AND {r}.type IN ({}) \
         AND {r}.status != 'REMOVED'",
        lvl.parent_filter,
        lvl.parent_id,
        ids.join(", "),
        types.join(", "),
    );
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
        .await
        .map_err(|e| translate(e, Some(ctx), MANAGE_TARGETING))?;
    ids.iter()
        .zip(criteria)
        .enumerate()
        .map(|(i, (id, c))| {
            let rn = resources::rn(
                &ctx.customer_id,
                lvl.collection,
                &format!("{}~{id}", lvl.parent_id),
            );
            let rn_path = format!("{}.resourceName", adsconsole_google_ads::fields::camel(r));
            page.results
                .iter()
                .find(|row| {
                    text(row, &rn_path).as_deref() == Some(&rn)
                        && text(row, &lvl.type_path()).as_deref() == Some(c.kind.api_type())
                })
                .map(|row| (rn, row.clone()))
                .ok_or_else(|| {
                    ToolError::new(
                        codes::NOT_FOUND,
                        format!(
                            "Criterion {id} of type '{}' not found in {}.",
                            c.kind.label(),
                            lvl.label
                        ),
                    )
                    .with_field(format!("criteria[{i}].criterion_id"))
                })
        })
        .collect()
}

async fn remove(ctx: &Ctx, lvl: &Level, criteria: &[CriterionIn]) -> Result<WritePlan, ToolError> {
    let found = existing(ctx, lvl, criteria).await?;
    let mut operations = Vec::new();
    let mut names = Vec::new();
    for ((rn, _), c) in found.iter().zip(criteria) {
        operations.push(resources::remove_op(rn).map_err(internal)?);
        names.push(format!(
            "{} {}",
            c.kind.label(),
            c.criterion_id.as_deref().unwrap_or_default()
        ));
    }
    Ok(WritePlan {
        summary: format!(
            "Removes {} targeting criterion(s) from {}: {}",
            names.len(),
            lvl.label,
            list(&names)
        ),
        warning: Some(IRREVERSIBLE_WARNING.to_string()),
        operations,
        field_changes: Vec::new(),
        watch: Vec::new(),
        inverse: InverseSpec::None,
        direct: None,
    })
}

async fn set_bid_modifier(
    ctx: &Ctx,
    lvl: &Level,
    criteria: &[CriterionIn],
) -> Result<WritePlan, ToolError> {
    for (i, c) in criteria.iter().enumerate() {
        if !c.kind.has_bid_modifier() {
            return Err(invalid(format!(
                "The '{}' criterion has no bid modifier.",
                c.kind.label()
            ))
            .with_field(format!("criteria[{i}].type")));
        }
        let f = format!("criteria[{i}].bid_modifier");
        bid_modifier_value(*required(&c.bid_modifier, &f)?, c.kind == Kind::Device, &f)?;
    }
    let found = existing(ctx, lvl, criteria).await?;
    let mut edits = Edits::new(lvl.label.clone(), &ctx.currency);
    for ((rn, row), c) in found.iter().zip(criteria) {
        let bid = c.bid_modifier.unwrap_or_default();
        let label = format!(
            "bid modifier of {} {}",
            c.kind.label(),
            c.criterion_id.as_deref().unwrap_or_default()
        );
        edits.set(rn, lvl.bid_field, row, json!(bid), &label, Show::Plain)?;
    }
    edits.into_plan(None)
}
