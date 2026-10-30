//! Campos editáveis de campanhas, ad sets e anúncios, e a conversão entre o formato
//! da Meta e o das ferramentas (o mesmo em leitura, "antes" e "depois").

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
    /// Dinheiro em unidades mínimas (string de inteiro na Meta).
    Minor,
    Enum,
    Json,
    DateTime,
    /// Id do criativo de um anúncio (`creative{id}` na leitura, `{"creative_id"}` na escrita).
    CreativeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditableField {
    pub name: &'static str,
    pub kinds: &'static [ObjectKind],
    pub value: FieldKind,
    /// O que pedir em `?fields=` para ler o campo.
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

/// Valor do campo num objeto lido da Meta, no formato das ferramentas. Ausente vira `null`.
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

/// O inverso de [`normalize`]: o valor como a Meta espera no POST.
pub fn to_param(field: &EditableField, value: &Value, exponent: u32) -> Result<Value, String> {
    match field.value {
        FieldKind::CreativeId => match value.as_str() {
            Some(id) => Ok(json!({ "creative_id": id })),
            None => Err(format!("{}: informe o id do criativo", field.name)),
        },
        FieldKind::Minor if value.is_null() => Ok(Value::Null),
        FieldKind::Minor => {
            let amount = decimal_of(value)
                .filter(|_| value.is_number())
                .ok_or_else(|| format!("{}: informe um número", field.name))?;
            to_minor_units(amount, exponent)
                .map(|u| Value::String(u.to_string()))
                .map_err(|e| format!("{}: {e}", field.name))
        }
        _ => Ok(value.clone()),
    }
}
