//! Table of editable fields: update operations with `updateMask`, read queries
//! and normalization. `normalize` is the only read used for the before/after/current
//! values of undo, and it is the exact inverse of `update_op`.

use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use serde_json::{Map, Number, Value, json};
use std::str::FromStr;

use crate::resources::op_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Enum,
    Micros,
    Bool,
    Double,
    Date,
    /// List of `AdTextAsset` (RSA headlines/descriptions). On read, each item keeps
    /// only `text` and `pinnedField`: the output-only fields (`assetPerformanceLabel`,
    /// `policySummaryInfo`, …) change on their own and must not go back in the update.
    AdTexts,
}

#[derive(Debug, PartialEq, Eq)]
pub struct EditableField {
    /// Full GAQL path, e.g. `campaign.target_spend.cpc_bid_ceiling_micros`.
    pub gaql: &'static str,
    /// GAQL resource (snake_case), e.g. `campaign`.
    pub resource: &'static str,
    /// Update mask (relative to the resource, snake_case).
    pub mask: &'static str,
    /// camelCase path relative to the resource (search row and update body).
    pub json_path: &'static [&'static str],
    pub kind: FieldKind,
}

macro_rules! field {
    ($gaql:literal, $res:literal, $mask:literal, [$($p:literal),+], $kind:ident) => {
        EditableField {
            gaql: $gaql,
            resource: $res,
            mask: $mask,
            json_path: &[$($p),+],
            kind: FieldKind::$kind,
        }
    };
}

pub const FIELDS: &[EditableField] = &[
    field!("campaign.name", "campaign", "name", ["name"], Text),
    field!("campaign.status", "campaign", "status", ["status"], Enum),
    // Campaign dates: in v26 only `start_date_time`/`end_date_time` exist, in the format
    // "yyyy-MM-dd HH:mm:ss" (account time zone); `start_date`/`end_date` were removed
    // (confirmed against a real account: `Unknown name "startDate"`). The tools
    // interface stays in YYYY-MM-DD: on write the start becomes `00:00:00` and the end
    // `23:59:59` (daily granularity; see `start_of_day`/`end_of_day`), and on read
    // only the date comes back (`date_part`). The field's normalized value is the full
    // datetime, the same one the update sends, so undo does not see a false conflict.
    // A null end means no end; restoring null clears the field.
    field!(
        "campaign.start_date_time",
        "campaign",
        "start_date_time",
        ["startDateTime"],
        Date
    ),
    field!(
        "campaign.end_date_time",
        "campaign",
        "end_date_time",
        ["endDateTime"],
        Date
    ),
    field!(
        "campaign.target_spend.cpc_bid_ceiling_micros",
        "campaign",
        "target_spend.cpc_bid_ceiling_micros",
        ["targetSpend", "cpcBidCeilingMicros"],
        Micros
    ),
    field!(
        "campaign.maximize_conversions.target_cpa_micros",
        "campaign",
        "maximize_conversions.target_cpa_micros",
        ["maximizeConversions", "targetCpaMicros"],
        Micros
    ),
    field!(
        "campaign.maximize_conversion_value.target_roas",
        "campaign",
        "maximize_conversion_value.target_roas",
        ["maximizeConversionValue", "targetRoas"],
        Double
    ),
    field!(
        "campaign_budget.amount_micros",
        "campaign_budget",
        "amount_micros",
        ["amountMicros"],
        Micros
    ),
    field!("ad_group.name", "ad_group", "name", ["name"], Text),
    field!("ad_group.status", "ad_group", "status", ["status"], Enum),
    field!(
        "ad_group.cpc_bid_micros",
        "ad_group",
        "cpc_bid_micros",
        ["cpcBidMicros"],
        Micros
    ),
    field!(
        "ad_group_criterion.status",
        "ad_group_criterion",
        "status",
        ["status"],
        Enum
    ),
    field!(
        "ad_group_criterion.cpc_bid_micros",
        "ad_group_criterion",
        "cpc_bid_micros",
        ["cpcBidMicros"],
        Micros
    ),
    field!(
        "campaign_criterion.bid_modifier",
        "campaign_criterion",
        "bid_modifier",
        ["bidModifier"],
        Double
    ),
    field!(
        "ad_group_criterion.bid_modifier",
        "ad_group_criterion",
        "bid_modifier",
        ["bidModifier"],
        Double
    ),
    field!(
        "ad_group_ad.status",
        "ad_group_ad",
        "status",
        ["status"],
        Enum
    ),
    // Confirmed against a real account (2026-10-18): an ad's texts and URLs are edited on the
    // `ad` resource (`customers/{c}/ads/{id}`, `AdOperation`); the status stays on `ad_group_ad`.
    field!("ad.final_urls", "ad", "final_urls", ["finalUrls"], Text),
    field!(
        "ad.responsive_search_ad.headlines",
        "ad",
        "responsive_search_ad.headlines",
        ["responsiveSearchAd", "headlines"],
        AdTexts
    ),
    field!(
        "ad.responsive_search_ad.descriptions",
        "ad",
        "responsive_search_ad.descriptions",
        ["responsiveSearchAd", "descriptions"],
        AdTexts
    ),
    field!("asset_group.name", "asset_group", "name", ["name"], Text),
    field!(
        "asset_group.status",
        "asset_group",
        "status",
        ["status"],
        Enum
    ),
    field!(
        "asset_group.final_urls",
        "asset_group",
        "final_urls",
        ["finalUrls"],
        Text
    ),
    // Read-only: has no update (see `is_read_only`).
    field!(
        "you_tube_video_upload.state",
        "you_tube_video_upload",
        "state",
        ["state"],
        Enum
    ),
];

pub fn find(gaql: &str) -> Option<&'static EditableField> {
    FIELDS.iter().find(|f| f.gaql == gaql)
}

impl EditableField {
    pub fn is_read_only(&self) -> bool {
        self.resource == "you_tube_video_upload"
    }
}

/// Start of day (`YYYY-MM-DD` -> `YYYY-MM-DD 00:00:00`), format of `campaign.start_date_time`.
pub fn start_of_day(date: &str) -> String {
    format!("{date} 00:00:00")
}

/// End of day (`YYYY-MM-DD` -> `YYYY-MM-DD 23:59:59`), format of `campaign.end_date_time`.
pub fn end_of_day(date: &str) -> String {
    format!("{date} 23:59:59")
}

/// Only the date (the first 10 characters) of a datetime "yyyy-MM-dd HH:mm:ss".
pub fn date_part(datetime: &str) -> &str {
    datetime.get(..10).unwrap_or(datetime)
}

/// `snake_case` -> `camelCase`.
pub fn camel(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut up = false;
    for c in snake.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn value_to_decimal(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => Decimal::from_str(s).ok(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Decimal::from(i))
            } else {
                n.as_f64().and_then(Decimal::from_f64)
            }
        }
        _ => None,
    }
}

fn decimal_to_json(d: Decimal) -> Value {
    let d = d.normalize();
    if d.scale() == 0 {
        return d.to_i64().map_or(Value::Null, |i| json!(i));
    }
    d.to_f64()
        .and_then(Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

/// Reads the field from the search row (camelCase) in canonical form.
pub fn normalize(field: &EditableField, row: &Value) -> Value {
    let mut cur = row.get(camel(field.resource));
    for p in field.json_path {
        cur = cur.and_then(|v| v.get(*p));
    }
    let Some(v) = cur else { return Value::Null };
    if v.is_null() {
        return Value::Null;
    }
    match field.kind {
        FieldKind::Micros => match value_to_decimal(v) {
            Some(d) => decimal_to_json(d / Decimal::from(1_000_000)),
            None => v.clone(),
        },
        FieldKind::Double => match v {
            Value::String(s) => s
                .parse::<f64>()
                .ok()
                .and_then(Number::from_f64)
                .map_or_else(|| v.clone(), Value::Number),
            _ => v.clone(),
        },
        FieldKind::AdTexts => match v {
            Value::Array(items) => Value::Array(items.iter().map(ad_text).collect()),
            _ => v.clone(),
        },
        _ => v.clone(),
    }
}

/// `AdTextAsset` with only `text` and `pinnedField` (when present).
fn ad_text(item: &Value) -> Value {
    let mut out = Map::new();
    for key in ["text", "pinnedField"] {
        if let Some(x) = item.get(key).filter(|x| !x.is_null()) {
            out.insert(key.to_string(), x.clone());
        }
    }
    Value::Object(out)
}

/// Normalized value -> API value (micros as a string).
fn to_api(field: &EditableField, v: &Value) -> Result<Value, String> {
    if field.kind != FieldKind::Micros || v.is_null() {
        return Ok(v.clone());
    }
    let units = match v {
        Value::Number(_) => value_to_decimal(v),
        _ => None,
    }
    .ok_or_else(|| format!("{}: expected a number, got {v}", field.gaql))?;
    let micros = units * Decimal::from(1_000_000);
    if micros != micros.trunc() {
        return Err(format!("{}: precision beyond micros: {v}", field.gaql));
    }
    Ok(Value::String(micros.trunc().to_string()))
}

fn set_path(obj: &mut Map<String, Value>, path: &[&str], v: Value) {
    match path {
        [last] => {
            obj.insert((*last).to_string(), v);
        }
        [head, rest @ ..] => {
            let child = obj.entry((*head).to_string()).or_insert_with(|| json!({}));
            if let Value::Object(m) = child {
                set_path(m, rest, v);
            }
        }
        [] => {}
    }
}

/// Builds `{"<op_key>": {"update": {...}, "updateMask": "a,b"}}`.
pub fn update_op(rn: &str, changes: &[(&EditableField, Value)]) -> Result<Value, String> {
    let key = op_key(rn)?;
    if changes.is_empty() {
        return Err("no changes".to_string());
    }
    let mut update = Map::new();
    update.insert("resourceName".to_string(), json!(rn));
    let mut masks = Vec::with_capacity(changes.len());
    for (f, v) in changes {
        if f.is_read_only() {
            return Err(format!("{} is read-only", f.gaql));
        }
        set_path(&mut update, f.json_path, to_api(f, v)?);
        masks.push(f.mask);
    }
    Ok(json!({ key: { "update": update, "updateMask": masks.join(",") } }))
}

pub fn read_query(resource: &str, rn: &str, fields: &[&str]) -> String {
    format!(
        "SELECT {} FROM {resource} WHERE {resource}.resource_name = {}",
        fields.join(", "),
        crate::gaql::quote(rn)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(g: &str) -> &'static EditableField {
        find(g).unwrap()
    }

    #[test]
    fn campaign_dates_are_datetime_fields() {
        assert!(find("campaign.start_date").is_none());
        assert!(find("campaign.end_date").is_none());
        assert_eq!(start_of_day("2026-11-01"), "2026-11-01 00:00:00");
        assert_eq!(end_of_day("2026-12-31"), "2026-12-31 23:59:59");
        assert_eq!(date_part("2026-12-31 23:59:59"), "2026-12-31");
        let v = update_op(
            "customers/1/campaigns/5",
            &[
                (f("campaign.start_date_time"), json!("2026-11-01 00:00:00")),
                (f("campaign.end_date_time"), json!(null)),
            ],
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"campaignOperation":{"update":{"resourceName":"customers/1/campaigns/5",
                "startDateTime":"2026-11-01 00:00:00","endDateTime":null},
                "updateMask":"start_date_time,end_date_time"}})
        );
        let row = json!({"campaign":{"startDateTime":"2026-11-01 00:00:00"}});
        assert_eq!(
            normalize(f("campaign.start_date_time"), &row),
            json!("2026-11-01 00:00:00")
        );
        assert_eq!(normalize(f("campaign.end_date_time"), &row), json!(null));
    }

    #[test]
    fn update_op_builds_mask() {
        let v = update_op(
            "customers/1/campaigns/5",
            &[
                (f("campaign.name"), json!("x")),
                (f("campaign.status"), json!("PAUSED")),
            ],
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"campaignOperation":{"update":{"resourceName":"customers/1/campaigns/5","name":"x","status":"PAUSED"},"updateMask":"name,status"}})
        );
        let v = update_op(
            "customers/1/campaignBudgets/9",
            &[(f("campaign_budget.amount_micros"), json!(25.4))],
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"campaignBudgetOperation":{"update":{"resourceName":"customers/1/campaignBudgets/9","amountMicros":"25400000"},"updateMask":"amount_micros"}})
        );
    }

    #[test]
    fn update_op_nested() {
        let v = update_op(
            "customers/1/campaigns/5",
            &[(
                f("campaign.target_spend.cpc_bid_ceiling_micros"),
                json!(1.5),
            )],
        )
        .unwrap();
        assert_eq!(
            v["campaignOperation"]["update"]["targetSpend"]["cpcBidCeilingMicros"],
            "1500000"
        );
        assert_eq!(
            v["campaignOperation"]["updateMask"],
            "target_spend.cpc_bid_ceiling_micros"
        );
        let v = update_op(
            "customers/1/ads/3",
            &[(f("ad.final_urls"), json!(["https://a.com"]))],
        )
        .unwrap();
        assert_eq!(
            v["adOperation"]["update"]["finalUrls"],
            json!(["https://a.com"])
        );
        assert_eq!(v["adOperation"]["updateMask"], "final_urls");
        let v = update_op(
            "customers/1/ads/3",
            &[(
                f("ad.responsive_search_ad.headlines"),
                json!([{"text": "a"}]),
            )],
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"adOperation":{"update":{"resourceName":"customers/1/ads/3","responsiveSearchAd":{"headlines":[{"text":"a"}]}},"updateMask":"responsive_search_ad.headlines"}})
        );
    }

    #[test]
    fn update_op_errors() {
        assert!(update_op("customers/1/campaigns/5", &[]).is_err());
        assert!(update_op("customers/1/zzz/5", &[(f("campaign.name"), json!("x"))]).is_err());
        assert!(
            update_op(
                "customers/1/campaignBudgets/9",
                &[(f("campaign_budget.amount_micros"), json!("abc"))]
            )
            .is_err()
        );
        assert!(
            update_op(
                "customers/1/campaignBudgets/9",
                &[(f("campaign_budget.amount_micros"), json!(0.0000001))]
            )
            .is_err()
        );
        assert!(
            update_op(
                "customers/1/campaigns/5",
                &[(f("you_tube_video_upload.state"), json!("x"))]
            )
            .is_err()
        );
    }

    #[test]
    fn normalize_reads_camel_rows() {
        let b = f("campaign_budget.amount_micros");
        assert_eq!(
            normalize(b, &json!({"campaignBudget":{"amountMicros":"25400000"}})),
            json!(25.4)
        );
        assert_eq!(
            normalize(b, &json!({"campaignBudget":{"amountMicros":"1000000000"}})),
            json!(1000)
        );
        assert_eq!(normalize(b, &json!({"campaignBudget":{}})), json!(null));
        assert_eq!(normalize(b, &json!({})), json!(null));
        assert_eq!(
            normalize(
                f("campaign.status"),
                &json!({"campaign":{"status":"PAUSED"}})
            ),
            json!("PAUSED")
        );
        assert_eq!(
            normalize(
                f("campaign.target_spend.cpc_bid_ceiling_micros"),
                &json!({"campaign":{"targetSpend":{"cpcBidCeilingMicros":"2500000"}}})
            ),
            json!(2.5)
        );
        let h = json!([{"text":"a","pinnedField":"HEADLINE_1"},{"text":"b"}]);
        assert_eq!(
            normalize(
                f("ad.responsive_search_ad.headlines"),
                &json!({"ad":{"responsiveSearchAd":{"headlines":h.clone()}}})
            ),
            h
        );
        // Output-only fields are dropped.
        let raw = json!([
            {"text":"a","pinnedField":"HEADLINE_1","assetPerformanceLabel":"BEST",
             "policySummaryInfo":{"reviewStatus":"REVIEWED"}},
            {"text":"b","assetPerformanceLabel":"LOW","pinnedField":null}
        ]);
        assert_eq!(
            normalize(
                f("ad.responsive_search_ad.descriptions"),
                &json!({"ad":{"responsiveSearchAd":{"descriptions":raw}}})
            ),
            h
        );
    }

    #[test]
    fn roundtrip_micros() {
        for (g, rn, x) in [
            (
                "campaign_budget.amount_micros",
                "customers/1/campaignBudgets/9",
                json!(25.4),
            ),
            (
                "campaign_budget.amount_micros",
                "customers/1/campaignBudgets/9",
                json!(0.01),
            ),
            (
                "campaign_budget.amount_micros",
                "customers/1/campaignBudgets/9",
                json!(1000),
            ),
            (
                "ad_group.cpc_bid_micros",
                "customers/1/adGroups/3",
                json!(0.35),
            ),
        ] {
            let fld = f(g);
            let op = update_op(rn, &[(fld, x.clone())]).unwrap();
            let upd = op.as_object().unwrap().values().next().unwrap()["update"].clone();
            // rebuild the search row from the update
            let row = json!({ camel(fld.resource): upd });
            assert_eq!(normalize(fld, &row), x, "{g} {x}");
        }
    }

    #[test]
    fn read_query_and_camel() {
        assert_eq!(
            read_query(
                "campaign",
                "customers/1/campaigns/5",
                &["campaign.name", "campaign.status"]
            ),
            "SELECT campaign.name, campaign.status FROM campaign WHERE campaign.resource_name = 'customers/1/campaigns/5'"
        );
        assert_eq!(camel("cpc_bid_ceiling_micros"), "cpcBidCeilingMicros");
        assert_eq!(camel("ad_group_ad"), "adGroupAd");
        assert_eq!(camel("you_tube_video_upload"), "youTubeVideoUpload");
    }

    #[test]
    fn table_consistent() {
        for fl in FIELDS {
            assert!(fl.gaql.starts_with(fl.resource), "{}", fl.gaql);
            let rest = &fl.gaql[fl.resource.len() + 1..];
            let expect: Vec<String> = rest.split('.').map(camel).collect();
            assert_eq!(fl.json_path, expect.as_slice(), "{}", fl.gaql);
            assert_eq!(fl.mask, rest);
        }
        assert!(find("nope.x").is_none());
    }
}
