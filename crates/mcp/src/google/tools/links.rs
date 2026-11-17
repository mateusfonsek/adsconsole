//! Links: resources whose id is made of the ids of the two things they join (a label
//! attached to a campaign, an asset linked to an ad group, ...).
//!
//! The resource name carries everything needed to create the link again, so removing a
//! link is reversible: the hidden `restore_links` tool recreates the links from their
//! names, and its own inverse removes them. The name never goes to the API on a create
//! (`wire_ops` strips it); the plan keeps it to resolve temporaries and to build the undo.

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::resources;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{GTool, WriteTool, own_resource, parse_input};
use crate::adapter::ToolSpec;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::plan::{InverseSpec, RESTORE_LINKS, Watch, WritePlan};

/// The most links one write may create or remove.
pub(crate) const MAX_LINKS: usize = 1000;

/// One kind of link: its collection, the GAQL resource, the field read after the write,
/// and how the id parts map to the create body.
struct LinkKind {
    collection: &'static str,
    resource: &'static str,
    watch_field: &'static str,
    /// For each `~`-separated part of the id: the body field it fills.
    parts: &'static [Part],
}

#[derive(Clone, Copy)]
enum Part {
    /// Starts a parent reference: `{field: customers/{c}/{collection}/{id}}`.
    Ref(&'static str, &'static str),
    /// Joins this part to the previous `Ref` (composite ids such as `adGroupAds/g~a`).
    Join,
    /// Plain value (`fieldType`).
    Value(&'static str),
}

const KINDS: &[LinkKind] = &[
    LinkKind {
        collection: "campaignLabels",
        resource: "campaign_label",
        watch_field: "campaign_label.label",
        parts: &[
            Part::Ref("campaign", "campaigns"),
            Part::Ref("label", "labels"),
        ],
    },
    LinkKind {
        collection: "adGroupLabels",
        resource: "ad_group_label",
        watch_field: "ad_group_label.label",
        parts: &[
            Part::Ref("adGroup", "adGroups"),
            Part::Ref("label", "labels"),
        ],
    },
    LinkKind {
        collection: "adGroupAdLabels",
        resource: "ad_group_ad_label",
        watch_field: "ad_group_ad_label.label",
        parts: &[
            Part::Ref("adGroupAd", "adGroupAds"),
            Part::Join,
            Part::Ref("label", "labels"),
        ],
    },
    LinkKind {
        collection: "adGroupCriterionLabels",
        resource: "ad_group_criterion_label",
        watch_field: "ad_group_criterion_label.label",
        parts: &[
            Part::Ref("adGroupCriterion", "adGroupCriteria"),
            Part::Join,
            Part::Ref("label", "labels"),
        ],
    },
    LinkKind {
        collection: "customerAssets",
        resource: "customer_asset",
        watch_field: "customer_asset.status",
        parts: &[Part::Ref("asset", "assets"), Part::Value("fieldType")],
    },
    LinkKind {
        collection: "campaignAssets",
        resource: "campaign_asset",
        watch_field: "campaign_asset.status",
        parts: &[
            Part::Ref("campaign", "campaigns"),
            Part::Ref("asset", "assets"),
            Part::Value("fieldType"),
        ],
    },
    LinkKind {
        collection: "adGroupAssets",
        resource: "ad_group_asset",
        watch_field: "ad_group_asset.status",
        parts: &[
            Part::Ref("adGroup", "adGroups"),
            Part::Ref("asset", "assets"),
            Part::Value("fieldType"),
        ],
    },
];

/// Collections whose create never sends `resourceName` (see `wire_ops`).
pub(crate) fn is_link_collection(collection: &str) -> bool {
    KINDS.iter().any(|k| k.collection == collection)
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, msg)
}

/// Kind, customer and id parts of a link name.
fn split(rn: &str) -> Result<(&'static LinkKind, &str, Vec<&str>), ToolError> {
    let mut it = rn.splitn(4, '/');
    let (Some("customers"), Some(customer), Some(collection), Some(id)) =
        (it.next(), it.next(), it.next(), it.next())
    else {
        return Err(bad(format!("Invalid link name: {rn}")));
    };
    let kind = KINDS
        .iter()
        .find(|k| k.collection == collection)
        .ok_or_else(|| bad(format!("Not a link: {rn}")))?;
    let parts: Vec<&str> = id.split('~').collect();
    let id_ok = |p: &str| {
        let digits = p.strip_prefix('-').unwrap_or(p);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    };
    let valid = parts.len() == kind.parts.len()
        && parts.iter().zip(kind.parts).all(|(p, k)| match k {
            Part::Value(_) => {
                !p.is_empty() && p.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
            }
            Part::Ref(..) | Part::Join => id_ok(p),
        });
    if !valid {
        return Err(bad(format!("Invalid link name: {rn}")));
    }
    Ok((kind, customer, parts))
}

/// `create` operation of the link named `rn`, with the body rebuilt from the name. The
/// name stays in the operation (for the plan); `wire_ops` removes it before sending.
pub(crate) fn create_op(rn: &str) -> Result<Value, ToolError> {
    let (kind, customer, parts) = split(rn)?;
    let mut body = Map::new();
    body.insert("resourceName".into(), json!(rn));
    let mut current: Option<(&str, &str, String)> = None;
    let flush = |cur: Option<(&str, &str, String)>, body: &mut Map<String, Value>| {
        if let Some((field, collection, id)) = cur {
            body.insert(
                field.to_string(),
                json!(resources::rn(customer, collection, &id)),
            );
        }
    };
    for (p, k) in parts.iter().zip(kind.parts) {
        match *k {
            Part::Ref(field, collection) => {
                flush(current.take(), &mut body);
                current = Some((field, collection, (*p).to_string()));
            }
            Part::Join => {
                if let Some((_, _, id)) = current.as_mut() {
                    id.push('~');
                    id.push_str(p);
                }
            }
            Part::Value(field) => {
                flush(current.take(), &mut body);
                body.insert(field.to_string(), json!(p));
            }
        }
    }
    flush(current.take(), &mut body);
    let key = resources::op_key(rn).map_err(bad)?;
    Ok(json!({ key: { "create": Value::Object(body) } }))
}

/// What to read after creating or removing the link `rn`.
pub(crate) fn watch(rn: &str) -> Result<Watch, ToolError> {
    let (kind, _, _) = split(rn)?;
    Ok(Watch {
        resource_name: rn.to_string(),
        resource: kind.resource,
        fields: vec![kind.watch_field],
    })
}

/// GAQL resource of a link collection (e.g. `campaign_label`).
pub(crate) fn gaql_resource(rn: &str) -> Result<&'static str, ToolError> {
    split(rn).map(|(k, _, _)| k.resource)
}

pub(crate) fn hidden() -> Vec<GTool> {
    vec![GTool::Write(Box::new(RestoreLinks))]
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreLinksInput {
    /// Names of the links to create again.
    resource_names: Vec<String>,
}

/// Inverse of removing links (detaching labels, unlinking assets).
struct RestoreLinks;

#[async_trait]
impl WriteTool for RestoreLinks {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<RestoreLinksInput>(
            RESTORE_LINKS,
            "Creates again removed links (labels and assets).",
            ToolGroup::Edit,
            true,
        )
        .hidden()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: RestoreLinksInput = parse_input(input)?;
        if input.resource_names.is_empty() || input.resource_names.len() > MAX_LINKS {
            return Err(
                bad(format!("Provide 1 to {MAX_LINKS} links.")).with_field("resource_names")
            );
        }
        let mut operations = Vec::new();
        let mut watches = Vec::new();
        for rn in &input.resource_names {
            own_resource(ctx, rn)?;
            if rn
                .split('/')
                .nth(3)
                .is_some_and(|id| id.split('~').any(|p| p.starts_with('-')))
            {
                return Err(bad(format!("Temporary link name: {rn}")));
            }
            operations.push(create_op(rn)?);
            watches.push(watch(rn)?);
        }
        Ok(WritePlan {
            summary: format!("Restore {} link(s).", operations.len()),
            warning: None,
            operations,
            field_changes: Vec::new(),
            watch: watches,
            inverse: InverseSpec::RemoveCreated {
                order: input.resource_names,
            },
            direct: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_bodies_from_names() {
        assert_eq!(
            create_op("customers/1/campaignLabels/5~9").unwrap(),
            json!({ "campaignLabelOperation": { "create": {
                "resourceName": "customers/1/campaignLabels/5~9",
                "campaign": "customers/1/campaigns/5",
                "label": "customers/1/labels/9",
            } } })
        );
        assert_eq!(
            create_op("customers/1/adGroupAdLabels/3~7~9").unwrap()["adGroupAdLabelOperation"]["create"],
            json!({
                "resourceName": "customers/1/adGroupAdLabels/3~7~9",
                "adGroupAd": "customers/1/adGroupAds/3~7",
                "label": "customers/1/labels/9",
            })
        );
        assert_eq!(
            create_op("customers/1/adGroupCriterionLabels/3~44~9").unwrap()["adGroupCriterionLabelOperation"]
                ["create"]["adGroupCriterion"],
            json!("customers/1/adGroupCriteria/3~44")
        );
        assert_eq!(
            create_op("customers/1/campaignAssets/5~-2~CALLOUT").unwrap()["campaignAssetOperation"]
                ["create"],
            json!({
                "resourceName": "customers/1/campaignAssets/5~-2~CALLOUT",
                "campaign": "customers/1/campaigns/5",
                "asset": "customers/1/assets/-2",
                "fieldType": "CALLOUT",
            })
        );
        assert_eq!(
            create_op("customers/1/customerAssets/8~STRUCTURED_SNIPPET").unwrap()["customerAssetOperation"]
                ["create"],
            json!({
                "resourceName": "customers/1/customerAssets/8~STRUCTURED_SNIPPET",
                "asset": "customers/1/assets/8",
                "fieldType": "STRUCTURED_SNIPPET",
            })
        );
    }

    #[test]
    fn rejects_malformed_names() {
        for rn in [
            "customers/1/campaignLabels/5",
            "customers/1/campaignLabels/5~x",
            "customers/1/campaigns/5",
            "customers/1/campaignAssets/5~2~callout",
            "customers/1/adGroupAdLabels/3~9",
            "garbage",
        ] {
            assert!(create_op(rn).is_err(), "{rn}");
        }
    }

    #[test]
    fn watch_fields() {
        let w = watch("customers/1/adGroupAssets/3~8~SITELINK").unwrap();
        assert_eq!(
            (w.resource, w.fields),
            ("ad_group_asset", vec!["ad_group_asset.status"])
        );
        assert!(is_link_collection("campaignLabels"));
        assert!(!is_link_collection("campaigns"));
    }
}
