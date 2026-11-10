//! Shared pieces of the campaign-creation tools (Search, Performance Max, Demand Gen
//! and Display): budget, base campaign, bidding strategy, location and language, and
//! sitelinks. Everything is validated locally, with the error pointing at the input
//! field, before any call.
//!
//! Every campaign is created `PAUSED`. Creates use temporary ids only where another
//! operation references the resource (budget, campaign, ad group, asset); criteria,
//! ads and links go without a `resourceName` and Google generates the name.

use adsconsole_core::money::Decimal;
use adsconsole_google_ads::resources::TempIds;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Number, Value, json};

use adsconsole_google_ads::fields;
use adsconsole_google_ads::images::{self, ImageKind, ImageRef, Loaded};

use super::asset_group::asset_rn;
use super::edit::{date, final_url, positive_micros};
use super::reports::{invalid, valid_id};
use super::targeting::{language_criterion, location_criterion, with_parent};
use crate::engine::internal;
use crate::error::ToolError;
use crate::google::ctx::Ctx;

/// Maximum size, in UTF-8 bytes, of a campaign, budget or ad group name.
pub(crate) const MAX_NAME_BYTES: usize = 255;
const MAX_SITELINKS: usize = 20;
const SITELINK_TEXT_CHARS: usize = 25;
const SITELINK_DESCRIPTION_CHARS: usize = 35;
const EU_POLITICAL: &str = "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING";

/// Bidding strategy of the new campaign. Input: `{"maximize_clicks": {"cpc_cap": 2.5}}`,
/// `{"maximize_conversions": {"target_cpa": 40}}`,
/// `{"maximize_conversion_value": {"target_roas": 3.5}}` or `{"manual_cpc": {}}`. The bare
/// name (`"manual_cpc"`, `"maximize_clicks"`) is also accepted by [`parse_bidding`].
#[derive(Debug, Clone, Deserialize, JsonSchema)]
// The variant names are the input keys (`maximize_clicks`, …).
#[allow(clippy::enum_variant_names)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Bidding {
    /// Maximize clicks (`targetSpend`).
    MaximizeClicks {
        /// CPC cap, in the account currency.
        #[serde(default, with = "rust_decimal::serde::float_option")]
        #[schemars(with = "Option<f64>")]
        cpc_cap: Option<Decimal>,
    },
    /// Maximize conversions.
    MaximizeConversions {
        /// Target CPA, in the account currency.
        #[serde(default, with = "rust_decimal::serde::float_option")]
        #[schemars(with = "Option<f64>")]
        target_cpa: Option<Decimal>,
    },
    /// Maximize conversion value.
    MaximizeConversionValue {
        /// Target ROAS (e.g. 3.5 = 350%).
        #[serde(default)]
        target_roas: Option<f64>,
    },
    /// Manual CPC (bids per ad group).
    ManualCpc {},
}

impl Default for Bidding {
    fn default() -> Self {
        Bidding::MaximizeClicks { cpc_cap: None }
    }
}

impl Bidding {
    pub(crate) fn is_manual(&self) -> bool {
        matches!(self, Bidding::ManualCpc {})
    }

    /// The strategy's field on the campaign and its value, with money in micros.
    /// Errors point at `bidding.<variant>.<target>`.
    pub(crate) fn api_field(&self, currency: &str) -> Result<(&'static str, Value), ToolError> {
        let micros = |v: &Option<Decimal>, key: &str, field: &str| -> Result<Value, ToolError> {
            Ok(match v {
                Some(d) => json!({ key: positive_micros(*d, currency, field)?.to_string() }),
                None => json!({}),
            })
        };
        Ok(match self {
            Bidding::MaximizeClicks { cpc_cap } => (
                "targetSpend",
                micros(
                    cpc_cap,
                    "cpcBidCeilingMicros",
                    "bidding.maximize_clicks.cpc_cap",
                )?,
            ),
            Bidding::MaximizeConversions { target_cpa } => (
                "maximizeConversions",
                micros(
                    target_cpa,
                    "targetCpaMicros",
                    "bidding.maximize_conversions.target_cpa",
                )?,
            ),
            Bidding::MaximizeConversionValue { target_roas } => {
                let value = match target_roas {
                    Some(r) => {
                        let n = Number::from_f64(*r).filter(|_| *r > 0.0).ok_or_else(|| {
                            let f = "bidding.maximize_conversion_value.target_roas";
                            invalid(format!("{f} must be greater than zero.")).with_field(f)
                        })?;
                        json!({ "targetRoas": n })
                    }
                    None => json!({}),
                };
                ("maximizeConversionValue", value)
            }
            Bidding::ManualCpc {} => ("manualCpc", json!({})),
        })
    }
}

/// Bidding strategy from the input (the `bidding` field, received as raw JSON); when
/// absent, the default `maximize_clicks`. The bare strategy name (`"manual_cpc"`) is
/// also accepted, as `{"manual_cpc": {}}`. Invalid value: `INVALID_INPUT` on
/// `bidding`.
pub(crate) fn parse_bidding(v: Option<&Value>) -> Result<Bidding, ToolError> {
    let v = match v {
        None | Some(Value::Null) => return Ok(Bidding::default()),
        Some(Value::String(name)) => json!({ name: {} }),
        Some(other) => other.clone(),
    };
    serde_json::from_value(v).map_err(|e| {
        invalid(format!(
            "Invalid bidding ({e}); use maximize_clicks, maximize_conversions, \
             maximize_conversion_value or manual_cpc."
        ))
        .with_field("bidding")
    })
}

/// Campaign or ad group name: trimmed, not empty, and at most [`MAX_NAME_BYTES`]
/// bytes in UTF-8 (Google's limit is in bytes).
pub(crate) fn name_text(raw: &str, field: &str) -> Result<String, ToolError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(invalid(format!("Provide {field}.")).with_field(field));
    }
    if t.len() > MAX_NAME_BYTES {
        return Err(invalid(format!(
            "{field} can have at most {MAX_NAME_BYTES} bytes in UTF-8 (got {}).",
            t.len()
        ))
        .with_field(field));
    }
    Ok(t.to_string())
}

/// Budget name of a new campaign: `<name> (budget <UTC yyyymmdd-HHMMSSmmm>)`, unique
/// because Google does not accept two budgets with the same name. The campaign-name
/// part is cut (at a character boundary) so the total fits in [`MAX_NAME_BYTES`]
/// bytes.
///
/// The timestamp makes `plan` non-deterministic only in this name, which is safe: the
/// budget name is not read by `watch` and is not part of the inverse (which uses the
/// resource name).
pub(crate) fn budget_name(campaign_name: &str) -> String {
    budget_name_at(campaign_name, chrono::Utc::now())
}

fn budget_name_at(campaign_name: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let suffix = format!(" (budget {})", now.format("%Y%m%d-%H%M%S%3f"));
    let max = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut cut = campaign_name.len().min(max);
    while !campaign_name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}", &campaign_name[..cut])
}

/// Start and end dates (YYYY-MM-DD, start ≤ end) in the campaign body. In v26 they go
/// as `startDateTime` (00:00:00) and `endDateTime` (23:59:59); see `fields.rs`.
pub(crate) fn campaign_dates(
    campaign: &mut Value,
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<(), ToolError> {
    let start = start_date.map(|s| date(s, "start_date")).transpose()?;
    let end = end_date.map(|e| date(e, "end_date")).transpose()?;
    if let (Some(s), Some(e)) = (start, end)
        && s > e
    {
        return Err(invalid("start_date must not be after end_date.").with_field("end_date"));
    }
    for (gaql, value) in [
        (
            "campaign.start_date_time",
            start_date.map(fields::start_of_day),
        ),
        ("campaign.end_date_time", end_date.map(fields::end_of_day)),
    ] {
        if let Some(v) = value {
            let f = fields::find(gaql).ok_or_else(|| internal(format!("field {gaql}")))?;
            set_path(campaign, f.json_path, json!(v));
        }
    }
    Ok(())
}

/// Writes `v` into `obj` following the segments of `path`, creating the objects in between.
pub(crate) fn set_path(obj: &mut Value, path: &[&str], v: Value) {
    let Some((last, head)) = path.split_last() else {
        return;
    };
    let mut cur = obj;
    for seg in head {
        if !cur[*seg].is_object() {
            cur[*seg] = json!({});
        }
        cur = &mut cur[*seg];
    }
    cur[*last] = v;
}

/// A count between `min` and `max`; otherwise `INVALID_INPUT` on `field`.
pub(crate) fn count(
    n: usize,
    min: usize,
    max: usize,
    what: &str,
    field: &str,
) -> Result<(), ToolError> {
    if n < min || n > max {
        return Err(
            invalid(format!("{field}: provide {min} to {max} {what} (got {n}).")).with_field(field),
        );
    }
    Ok(())
}

/// Required input text, trimmed, 1 to `max` characters.
pub(crate) fn bounded_text(raw: &str, max: usize, field: &str) -> Result<String, ToolError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(invalid(format!("Provide {field}.")).with_field(field));
    }
    if t.chars().count() > max {
        return Err(invalid(format!(
            "{field} can have at most {max} characters ('{t}')."
        ))
        .with_field(field));
    }
    Ok(t.to_string())
}

/// Create `campaignBudgetOperation`: daily budget (`daily_budget`, in the account
/// currency) with `deliveryMethod: STANDARD`, named with the campaign's [`budget_name`].
pub(crate) fn budget_op(
    temp: &str,
    campaign_name: &str,
    daily_budget: Decimal,
    currency: &str,
    shared: bool,
) -> Result<Value, ToolError> {
    let micros = positive_micros(daily_budget, currency, "daily_budget")?;
    Ok(json!({ "campaignBudgetOperation": { "create": {
        "resourceName": temp,
        "name": budget_name(campaign_name),
        "amountMicros": micros.to_string(),
        "deliveryMethod": "STANDARD",
        "explicitlyShared": shared,
    } } }))
}

/// Campaign body (`create`): paused, with budget, channel type, bidding strategy and
/// the EU political advertising declaration. The caller adds the rest (networks,
/// dates) and wraps it in `campaignOperation`.
pub(crate) fn campaign_base(
    temp: &str,
    budget_rn: &str,
    name: &str,
    channel: &str,
    bidding: &Bidding,
    currency: &str,
) -> Result<Value, ToolError> {
    let (key, value) = bidding.api_field(currency)?;
    Ok(json!({
        "resourceName": temp,
        "name": name,
        "status": "PAUSED",
        "advertisingChannelType": channel,
        "campaignBudget": budget_rn,
        key: value,
        "containsEuPoliticalAdvertising": EU_POLITICAL,
    }))
}

/// Create operation without a `resourceName` (Google generates the name).
pub(crate) fn create_unnamed(op_key: &str, body: Value) -> Value {
    json!({ op_key: { "create": body } })
}

/// Input ids (`locations`, `languages`): numeric and without repeats. Errors on
/// `field[i]`.
pub(crate) fn criterion_ids(items: &[String], field: &str) -> Result<Vec<String>, ToolError> {
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in items.iter().enumerate() {
        let f = format!("{field}[{i}]");
        let id = valid_id(raw.trim(), &f)?;
        if out.contains(&id) {
            return Err(invalid(format!("{f}: id {id} appears more than once.")).with_field(f));
        }
        out.push(id);
    }
    Ok(out)
}

/// Location criteria (`locations`: `geoTargetConstants` ids) and language criteria
/// (`languages`: `languageConstants` ids) of `parent_field` = `parent_rn`, as `op_key`
/// operations without a `resourceName`. Errors on `{prefix}locations[i]` /
/// `{prefix}languages[i]`.
pub(crate) fn geo_lang_criteria(
    op_key: &str,
    parent_field: &str,
    parent_rn: &str,
    locations: &[String],
    languages: &[String],
    prefix: &str,
) -> Result<Vec<Value>, ToolError> {
    let mut ops = Vec::new();
    let lf = format!("{prefix}locations");
    for (i, id) in criterion_ids(locations, &lf)?.iter().enumerate() {
        let body = location_criterion(Some(id), false, &format!("{lf}[{i}]"))?;
        ops.push(create_unnamed(
            op_key,
            with_parent(body, parent_field, parent_rn),
        ));
    }
    let gf = format!("{prefix}languages");
    for (i, id) in criterion_ids(languages, &gf)?.iter().enumerate() {
        let body = language_criterion(Some(id), &format!("{gf}[{i}]"))?;
        ops.push(create_unnamed(
            op_key,
            with_parent(body, parent_field, parent_rn),
        ));
    }
    Ok(ops)
}

/// Campaign location and language criteria (see [`geo_lang_criteria`]).
pub(crate) fn geo_lang_ops(
    campaign_rn: &str,
    locations: &[String],
    languages: &[String],
) -> Result<Vec<Value>, ToolError> {
    geo_lang_criteria(
        "campaignCriterionOperation",
        "campaign",
        campaign_rn,
        locations,
        languages,
        "",
    )
}

/// Texts of an ad: `min` to `max` items of 1 to `len` characters, without repeats.
/// Count errors on `field`, item errors on `field[i]`.
pub(crate) fn text_list(
    items: &[String],
    (min, max, len): (usize, usize, usize),
    what: &str,
    field: &str,
) -> Result<Vec<String>, ToolError> {
    count(items.len(), min, max, what, field)?;
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in items.iter().enumerate() {
        let f = format!("{field}[{i}]");
        let t = bounded_text(raw, len, &f)?;
        if out.contains(&t) {
            return Err(invalid(format!("{f}: '{t}' appears more than once.")).with_field(f));
        }
        out.push(t);
    }
    Ok(out)
}

/// `[{"text": …}]` (`AdTextAsset`) of the texts.
pub(crate) fn text_assets(items: &[String]) -> Value {
    Value::Array(items.iter().map(|t| json!({ "text": t })).collect())
}

/// Input sitelink.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SitelinkIn {
    /// Link text (up to 25 characters).
    pub link_text: String,
    /// Final URL of the link (`https://…`).
    pub final_url: String,
    /// Description line 1 (up to 35 characters); provide both lines or neither.
    #[serde(default)]
    pub description1: Option<String>,
    /// Description line 2 (up to 35 characters).
    #[serde(default)]
    pub description2: Option<String>,
}

/// For each sitelink (up to 20): the asset (`sitelinkAsset` + `finalUrls`, with a
/// temporary id) and the `campaignAsset` link with `fieldType: SITELINK`. Errors point
/// at `sitelinks[i].<field>`.
pub(crate) fn sitelink_ops(
    temps: &mut TempIds,
    campaign_rn: &str,
    sitelinks: &[SitelinkIn],
) -> Result<Vec<Value>, ToolError> {
    if sitelinks.len() > MAX_SITELINKS {
        return Err(invalid(format!(
            "At most {MAX_SITELINKS} sitelinks (got {}).",
            sitelinks.len()
        ))
        .with_field("sitelinks"));
    }
    let mut ops = Vec::new();
    for (i, s) in sitelinks.iter().enumerate() {
        let f = |name: &str| format!("sitelinks[{i}].{name}");
        let mut asset = json!({
            "linkText": bounded_text(&s.link_text, SITELINK_TEXT_CHARS, &f("link_text"))?
        });
        match (&s.description1, &s.description2) {
            (Some(d1), Some(d2)) => {
                asset["description1"] = json!(bounded_text(
                    d1,
                    SITELINK_DESCRIPTION_CHARS,
                    &f("description1")
                )?);
                asset["description2"] = json!(bounded_text(
                    d2,
                    SITELINK_DESCRIPTION_CHARS,
                    &f("description2")
                )?);
            }
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                let missing = f(if s.description1.is_none() {
                    "description1"
                } else {
                    "description2"
                });
                return Err(invalid(format!(
                    "sitelinks[{i}]: provide both description lines or neither."
                ))
                .with_field(missing));
            }
        }
        let url = s.final_url.trim();
        final_url(url, &f("final_url"))?;
        let asset_rn = temps.next("assets");
        ops.push(json!({ "assetOperation": { "create": {
            "resourceName": asset_rn,
            "finalUrls": [url],
            "sitelinkAsset": asset,
        } } }));
        ops.push(create_unnamed(
            "campaignAssetOperation",
            json!({ "campaign": campaign_rn, "asset": asset_rn, "fieldType": "SITELINK" }),
        ));
    }
    Ok(ops)
}

/// Input [`ImageRef`] (received as raw JSON, so the error points at the field): an
/// invalid format is `INVALID_INPUT` on `field`.
pub(crate) fn image_ref(v: &Value, field: &str) -> Result<ImageRef, ToolError> {
    ImageRef::from_value(v).map_err(|e| invalid(format!("{field}: {e}.")).with_field(field))
}

/// Image ready for the create, in the `kind` role: a file or URL loaded and checked
/// (aspect ratio and minimum size), or an existing asset of this account (no local
/// check). Errors: `INVALID_INPUT` on `field` (or `field.asset`).
pub(crate) async fn load_image(
    ctx: &Ctx,
    r: &ImageRef,
    kind: ImageKind,
    field: &str,
) -> Result<Loaded, ToolError> {
    let loaded = load_unchecked(ctx, r, field).await?;
    if let Loaded::New(img) = &loaded {
        check_kind(kind, img.width, img.height, field)?;
    }
    Ok(loaded)
}

/// Loaded image (or existing asset of this account), without checking the aspect ratio.
async fn load_unchecked(ctx: &Ctx, r: &ImageRef, field: &str) -> Result<Loaded, ToolError> {
    let loaded = images::load(r, images::http_client())
        .await
        .map_err(|e| invalid(format!("{field}: {e}.")).with_field(field))?;
    Ok(match loaded {
        Loaded::Existing(rn) => Loaded::Existing(asset_rn(ctx, &rn, &format!("{field}.asset"))?),
        new => new,
    })
}

fn check_kind(kind: ImageKind, w: u32, h: u32, field: &str) -> Result<(), ToolError> {
    images::check(kind, w, h).map_err(|e| invalid(format!("{field}: {e}.")).with_field(field))
}

/// The first role in `kinds` that the `w`x`h` image fits.
fn first_fit(kinds: &[ImageKind], w: u32, h: u32, field: &str) -> Result<ImageKind, ToolError> {
    let mut why = Vec::new();
    for k in kinds {
        match images::check(*k, w, h) {
            Ok(()) => return Ok(*k),
            Err(e) => why.push(e),
        }
    }
    Err(invalid(format!("{field}: {}.", why.join("; "))).with_field(field))
}

/// Parsed input image reference, with its role and field.
pub(crate) struct ImageIn {
    pub r: ImageRef,
    pub kind: ImageKind,
    pub field: String,
}

/// Parsed image reference, checked if it is an existing asset (nothing is loaded).
pub(crate) fn image_in(
    ctx: &Ctx,
    v: &Value,
    kind: ImageKind,
    field: String,
) -> Result<ImageIn, ToolError> {
    let r = image_ref(v, &field)?;
    if let ImageRef::Asset { asset } = &r {
        asset_rn(ctx, asset, &format!("{field}.asset"))?;
    }
    Ok(ImageIn { r, kind, field })
}

/// List of images for one role: `min` to `max`, each parsed with [`image_in`]
/// (`field[i]`), with no reference repeated.
pub(crate) fn image_list(
    ctx: &Ctx,
    items: &[Value],
    kind: ImageKind,
    (min, max): (usize, usize),
    what: &str,
    field: &str,
) -> Result<Vec<ImageIn>, ToolError> {
    count(items.len(), min, max, what, field)?;
    let mut out: Vec<ImageIn> = Vec::new();
    for (i, v) in items.iter().enumerate() {
        let img = image_in(ctx, v, kind, format!("{field}[{i}]"))?;
        if out.iter().any(|x| x.r == img.r) {
            return Err(
                invalid(format!("{}: the image appears more than once.", img.field))
                    .with_field(img.field),
            );
        }
        out.push(img);
    }
    Ok(out)
}

/// Warning for creates with new assets.
pub(crate) const ASSETS_WARNING: &str = "New assets (texts, images and videos) stay in the \
     account: Google does not remove assets, so undo removes only the campaign and the budget.";

/// Image sources, without repeats, in the order they appear (for the summary).
pub(crate) fn image_sources<'a>(imgs: impl IntoIterator<Item = &'a ImageIn>) -> Vec<String> {
    let mut sources: Vec<String> = Vec::new();
    for img in imgs {
        let s = image_source_label(&img.r);
        if !sources.contains(&s) {
            sources.push(s);
        }
    }
    sources
}

/// Total bytes of new images per call (30 MB).
pub(crate) const MAX_NEW_IMAGE_BYTES: usize = 30 * 1024 * 1024;

/// Source of an image, for the summary: the file path, the URL host or the id of the
/// existing asset.
pub(crate) fn image_source_label(r: &ImageRef) -> String {
    match r {
        ImageRef::File { file_path } => file_path.clone(),
        ImageRef::Url { url } => url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| url.clone()),
        ImageRef::Asset { asset } => {
            format!("asset {}", asset.rsplit('/').next().unwrap_or_default())
        }
    }
}

/// Reference, asset name and dimensions (new images only).
type PooledRef = (ImageRef, String, Option<(u32, u32)>);

/// Images of one create. The same reference is loaded once; the same bytes (by
/// SHA-256, from a file or a URL) become a single asset, named
/// [`LoadedImage::content_name`](adsconsole_google_ads::images::LoadedImage::content_name).
/// New bytes add up to at most [`MAX_NEW_IMAGE_BYTES`].
#[derive(Default)]
pub(crate) struct ImagePool {
    refs: Vec<PooledRef>,
    /// (SHA-256, asset name) of the new images.
    hashes: Vec<(String, String)>,
    new_bytes: usize,
}

impl ImagePool {
    /// Asset name of image `r` in the `kind` role. New image: loaded and checked
    /// ([`load_image`]) and, if its bytes have not been seen yet, created in `ops`.
    pub(crate) async fn resolve(
        &mut self,
        ctx: &Ctx,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        r: &ImageRef,
        kind: ImageKind,
        field: &str,
    ) -> Result<String, ToolError> {
        if let Some((_, rn, dims)) = self.refs.iter().find(|(x, ..)| x == r) {
            if let Some((w, h)) = dims {
                check_kind(kind, *w, *h, field)?;
            }
            return Ok(rn.clone());
        }
        let loaded = load_image(ctx, r, kind, field).await?;
        self.register(r.clone(), loaded, temps, ops, field)
    }

    /// Like [`Self::resolve`], with the role picked by the image: the first of `kinds`
    /// it fits. Existing asset: role `None` (unknown without loading it).
    pub(crate) async fn resolve_fit(
        &mut self,
        ctx: &Ctx,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        r: &ImageRef,
        kinds: &[ImageKind],
        field: &str,
    ) -> Result<(String, Option<ImageKind>), ToolError> {
        if let Some((_, rn, dims)) = self.refs.iter().find(|(x, ..)| x == r) {
            let kind = dims
                .map(|(w, h)| first_fit(kinds, w, h, field))
                .transpose()?;
            return Ok((rn.clone(), kind));
        }
        let loaded = load_unchecked(ctx, r, field).await?;
        let kind = match &loaded {
            Loaded::New(img) => Some(first_fit(kinds, img.width, img.height, field)?),
            Loaded::Existing(_) => None,
        };
        let rn = self.register(r.clone(), loaded, temps, ops, field)?;
        Ok((rn, kind))
    }

    /// `[{"asset": rn}]` (`AdImageAsset`) of the images, each in its role; the same
    /// bytes twice in the list are refused.
    pub(crate) async fn resolve_list(
        &mut self,
        ctx: &Ctx,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        imgs: &[ImageIn],
    ) -> Result<Value, ToolError> {
        let mut rns: Vec<String> = Vec::new();
        for img in imgs {
            let rn = self
                .resolve(ctx, temps, ops, &img.r, img.kind, &img.field)
                .await?;
            if rns.contains(&rn) {
                return Err(invalid(format!(
                    "{}: the same image appears more than once.",
                    img.field
                ))
                .with_field(img.field.clone()));
            }
            rns.push(rn);
        }
        Ok(Value::Array(
            rns.into_iter().map(|rn| json!({ "asset": rn })).collect(),
        ))
    }

    /// Registers an image that is already loaded and checked; returns the asset name.
    pub(crate) fn register(
        &mut self,
        r: ImageRef,
        loaded: Loaded,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        field: &str,
    ) -> Result<String, ToolError> {
        let (rn, dims) = match loaded {
            Loaded::Existing(rn) => (rn, None),
            Loaded::New(img) => {
                let hash = img.sha256_hex();
                let dims = Some((img.width, img.height));
                match self.hashes.iter().find(|(h, _)| *h == hash) {
                    Some((_, rn)) => (rn.clone(), dims),
                    None => {
                        self.new_bytes += img.bytes.len();
                        if self.new_bytes > MAX_NEW_IMAGE_BYTES {
                            let top = field.split(['.', '[']).next().unwrap_or(field);
                            return Err(invalid(format!(
                                "{field}: the new images in this call exceed 30 MB \
                                 in total; use smaller images or existing assets."
                            ))
                            .with_field(top));
                        }
                        let rn = temps.next("assets");
                        ops.push(images::image_asset_op(&rn, &img.content_name(), &img));
                        self.hashes.push((hash, rn.clone()));
                        (rn, dims)
                    }
                }
            }
        };
        self.refs.push((r, rn.clone(), dims));
        Ok(rn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidding_parses_name_and_struct_forms() {
        let parse = |v: Value| parse_bidding(Some(&v));
        assert!(parse(json!("manual_cpc")).unwrap().is_manual());
        assert!(parse(json!({ "manual_cpc": {} })).unwrap().is_manual());
        assert!(matches!(
            parse(json!("maximize_clicks")).unwrap(),
            Bidding::MaximizeClicks { cpc_cap: None }
        ));
        assert!(matches!(
            parse_bidding(None).unwrap(),
            Bidding::MaximizeClicks { cpc_cap: None }
        ));
        assert!(parse(Value::Null).is_ok());
        for bad in [
            json!("magic_bid"),
            json!(3),
            json!({ "maximize_clicks": { "cpc_cap": "x" } }),
        ] {
            let e = parse(bad.clone()).unwrap_err();
            assert_eq!(e.code, crate::error::codes::INVALID_INPUT, "{bad}");
            assert_eq!(e.field.as_deref(), Some("bidding"), "{bad}");
        }
        let b: Bidding =
            serde_json::from_value(json!({ "maximize_clicks": { "cpc_cap": 1.5 } })).unwrap();
        assert_eq!(
            b.api_field("BRL").unwrap(),
            ("targetSpend", json!({ "cpcBidCeilingMicros": "1500000" }))
        );
        assert_eq!(
            Bidding::default().api_field("BRL").unwrap(),
            ("targetSpend", json!({}))
        );
    }

    #[test]
    fn campaign_base_is_paused() {
        let c = campaign_base(
            "customers/1/campaigns/-2",
            "customers/1/campaignBudgets/-1",
            "X",
            "PERFORMANCE_MAX",
            &Bidding::MaximizeConversions { target_cpa: None },
            "BRL",
        )
        .unwrap();
        assert_eq!(c["status"], "PAUSED");
        assert_eq!(c["containsEuPoliticalAdvertising"], EU_POLITICAL);
        assert_eq!(c["maximizeConversions"], json!({}));
    }

    #[test]
    fn budget_name_is_bounded_and_timestamped() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-17T13:04:05.678Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            budget_name_at("Search", now),
            "Search (budget 20261017-130405678)"
        );
        // Name at the limit, with 2- and 3-byte characters: cut at a character boundary.
        for name in [
            "ã".repeat(127),
            format!("a{}", "€".repeat(84)),
            "x".repeat(255),
        ] {
            assert!(name.len() >= 253, "{}", name.len());
            let b = budget_name_at(&name, now);
            assert!(b.len() <= MAX_NAME_BYTES, "{} bytes", b.len());
            assert!(b.ends_with(" (budget 20261017-130405678)"), "{b}");
            assert!(name.starts_with(b.split(" (budget ").next().unwrap()));
            // Valid UTF-8 string (the bytes turn back into a str).
            assert!(std::str::from_utf8(b.as_bytes()).is_ok());
        }
    }

    #[test]
    fn name_limit_is_in_bytes() {
        assert!(name_text(&"ã".repeat(127), "name").is_ok()); // 254 bytes
        let e = name_text(&"ã".repeat(128), "name").unwrap_err(); // 256 bytes, 128 chars
        assert_eq!(e.field.as_deref(), Some("name"));
        assert!(name_text("  ", "ad_groups[0].name").is_err());
    }

    #[test]
    fn dates_are_set_by_path_segments() {
        let mut c = json!({});
        campaign_dates(&mut c, Some("2026-11-01"), Some("2026-12-31")).unwrap();
        assert_eq!(
            c,
            json!({ "startDateTime": "2026-11-01 00:00:00", "endDateTime": "2026-12-31 23:59:59" })
        );
        let mut c = json!({});
        set_path(&mut c, &["a", "b"], json!(1));
        assert_eq!(c, json!({ "a": { "b": 1 } }));
        let e = campaign_dates(&mut c, Some("2026-12-01"), Some("2026-11-01")).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("end_date"));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    /// The same bytes from a file and from a URL become one asset, named by content.
    #[tokio::test]
    async fn same_bytes_from_file_and_url_are_one_asset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(png(300, 300), "image/png"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("logo.png");
        std::fs::write(&file, png(300, 300)).unwrap();
        let by_file = ImageRef::File {
            file_path: file.to_string_lossy().into_owned(),
        };
        let by_url = ImageRef::Url {
            url: format!("{}/logo.png", server.uri()),
        };
        let client = reqwest::Client::new();
        let mut pool = ImagePool::default();
        let mut temps = TempIds::new("1");
        let mut ops = Vec::new();
        let mut rns = Vec::new();
        for r in [&by_file, &by_url] {
            let loaded = images::load_with(&client, r, images::AllowHttp::ForTests)
                .await
                .unwrap();
            rns.push(
                pool.register(r.clone(), loaded, &mut temps, &mut ops, "brand.logo")
                    .unwrap(),
            );
        }
        assert_eq!(rns[0], rns[1]);
        assert_eq!(ops.len(), 1, "{ops:?}");
        let name = ops[0]["assetOperation"]["create"]["name"].as_str().unwrap();
        assert!(name.starts_with("img-") && name.len() == 20, "{name}");
    }

    #[test]
    fn new_image_bytes_are_capped_per_call() {
        let mut pool = ImagePool::default();
        let mut temps = TempIds::new("1");
        let mut ops = Vec::new();
        let five_mb = images::MAX_IMAGE_BYTES;
        for i in 0..7u8 {
            let img = images::LoadedImage {
                bytes: vec![i; five_mb],
                width: 1200,
                height: 1200,
                mime: "image/png".into(),
            };
            let r = ImageRef::File {
                file_path: format!("/img{i}.png"),
            };
            let field = format!("asset_groups[0].images.square[{i}]");
            let out = pool.register(r, Loaded::New(img), &mut temps, &mut ops, &field);
            if i < 6 {
                out.unwrap(); // 6 x 5 MB = 30 MB: still fits
            } else {
                let e = out.unwrap_err();
                assert_eq!(e.field.as_deref(), Some("asset_groups"), "{e:?}");
                assert!(e.message.contains("30 MB"), "{}", e.message);
            }
        }
        assert_eq!(ops.len(), 6);
    }

    #[test]
    fn image_sources_for_the_summary() {
        let f = |v: Value| image_source_label(&ImageRef::from_value(&v).unwrap());
        assert_eq!(f(json!({ "file_path": "/a/b.png" })), "/a/b.png");
        assert_eq!(
            f(json!({ "url": "https://cdn.example.com/x.png?y=1" })),
            "cdn.example.com"
        );
        assert_eq!(f(json!({ "asset": "customers/1/assets/77" })), "asset 77");
    }

    #[test]
    fn duplicate_location_is_refused() {
        let e =
            geo_lang_ops("customers/1/campaigns/-2", &["1".into(), "1".into()], &[]).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("locations[1]"));
    }
}
