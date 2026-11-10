//! Editable fields of campaigns, ad sets and ads, and the conversion between Meta's
//! format and the tools' format (the same for reads, "before" and "after").

use adsconsole_core::money::{from_minor_units, to_minor_units};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Campaign,
    AdSet,
    Ad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Status,
    /// Money in minor units (an integer string in Meta).
    Minor,
    Enum,
    Json,
    DateTime,
    /// Creative id of an ad (`creative{id}` on read, `{"creative_id"}` on write).
    CreativeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditableField {
    pub name: &'static str,
    pub kinds: &'static [ObjectKind],
    pub value: FieldKind,
    /// What to request in `?fields=` to read the field.
    pub read: &'static str,
}

use ObjectKind::{Ad, AdSet, Campaign};

const fn field(
    name: &'static str,
    kinds: &'static [ObjectKind],
    value: FieldKind,
    read: &'static str,
) -> EditableField {
    EditableField {
        name,
        kinds,
        value,
        read,
    }
}

pub const FIELDS: &[EditableField] = &[
    field("name", &[Campaign, AdSet, Ad], FieldKind::Text, "name"),
    field(
        "status",
        &[Campaign, AdSet, Ad],
        FieldKind::Status,
        "status",
    ),
    field(
        "daily_budget",
        &[Campaign, AdSet],
        FieldKind::Minor,
        "daily_budget",
    ),
    field(
        "lifetime_budget",
        &[Campaign, AdSet],
        FieldKind::Minor,
        "lifetime_budget",
    ),
    field(
        "bid_strategy",
        &[Campaign, AdSet],
        FieldKind::Enum,
        "bid_strategy",
    ),
    field("bid_amount", &[AdSet], FieldKind::Minor, "bid_amount"),
    field("spend_cap", &[Campaign], FieldKind::Minor, "spend_cap"),
    field("targeting", &[AdSet], FieldKind::Json, "targeting"),
    field("start_time", &[AdSet], FieldKind::DateTime, "start_time"),
    field("end_time", &[AdSet], FieldKind::DateTime, "end_time"),
    field("creative", &[Ad], FieldKind::CreativeId, "creative{id}"),
];

pub fn find(name: &str, kind: ObjectKind) -> Option<&'static EditableField> {
    FIELDS
        .iter()
        .find(|f| f.name == name && f.kinds.contains(&kind))
}

fn decimal_of(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// Value of the field in an object read from Meta, in the tools' format. Missing becomes `null`.
pub fn normalize(field: &EditableField, object: &Value, exponent: u32) -> Value {
    match field.value {
        FieldKind::CreativeId => object["creative"]["id"].clone(),
        FieldKind::Minor => {
            let raw = &object[field.name];
            match raw
                .as_i64()
                .or_else(|| raw.as_str().and_then(|s| s.parse().ok()))
            {
                Some(units) => {
                    let d = from_minor_units(units, exponent).normalize();
                    if d.scale() == 0 {
                        json!(d.to_i64())
                    } else {
                        json!(d.to_f64())
                    }
                }
                None => Value::Null,
            }
        }
        _ => object[field.name].clone(),
    }
}

/// The inverse of [`normalize`]: the value as Meta expects it in the POST.
pub fn to_param(field: &EditableField, value: &Value, exponent: u32) -> Result<Value, String> {
    match field.value {
        FieldKind::CreativeId => match value.as_str() {
            Some(id) => Ok(json!({ "creative_id": id })),
            None => Err(format!("{}: provide the creative id", field.name)),
        },
        FieldKind::Minor if value.is_null() => Ok(Value::Null),
        FieldKind::Minor => {
            let amount = decimal_of(value)
                .filter(|_| value.is_number())
                .ok_or_else(|| format!("{}: provide a number", field.name))?;
            to_minor_units(amount, exponent)
                .map(|u| Value::String(u.to_string()))
                .map_err(|e| format!("{}: {e}", field.name))
        }
        _ => Ok(value.clone()),
    }
}
