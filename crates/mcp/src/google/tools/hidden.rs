//! Hidden tools used as the inverse of writes: `restore_fields`, `remove_resources`
//! and `remove_youtube_upload`. Never listed; undo calls them with the original
//! change's account.

use std::collections::BTreeMap;

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::{fields, resources, upload};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{GTool, WriteTool, own_resource, parse_input};
use crate::adapter::ToolSpec;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::plan::{
    DirectExec, FieldChange, InverseSpec, REMOVE_RESOURCES, REMOVE_YOUTUBE_UPLOAD, RESTORE_FIELDS,
    Watch, WritePlan,
};

pub(crate) fn hidden() -> Vec<GTool> {
    vec![
        GTool::Write(Box::new(RestoreFields)),
        GTool::Write(Box::new(RemoveResources)),
        GTool::Write(Box::new(RemoveYoutubeUpload)),
    ]
}

fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, msg)
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreFieldsInput {
    /// Values per resource: `{resource_name: {gaql_field: value}}`.
    updates: BTreeMap<String, BTreeMap<String, Value>>,
}

struct RestoreFields;

#[async_trait]
impl WriteTool for RestoreFields {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<RestoreFieldsInput>(
            RESTORE_FIELDS,
            "Sets fields of Google Ads resources back to the given values.",
            ToolGroup::Edit,
            true,
        )
        .idempotent()
        .hidden()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: RestoreFieldsInput = parse_input(input)?;
        if input.updates.is_empty() {
            return Err(invalid("No fields to restore.").with_field("updates"));
        }
        let mut operations = Vec::new();
        let mut field_changes = Vec::new();
        let mut watch = Vec::new();
        for (rn, values) in &input.updates {
            own_resource(ctx, rn)?;
            let op_key = resources::op_key(rn).map_err(invalid)?;
            let mut changes = Vec::new();
            for (gaql, value) in values {
                let field = fields::find(gaql)
                    .ok_or_else(|| invalid(format!("Field is not editable: {gaql}")))?;
                // The field must belong to the named resource (campaign.* on campaigns/…).
                if op_key != format!("{}Operation", fields::camel(field.resource)) {
                    return Err(invalid(format!(
                        "Field {gaql} does not belong to resource {rn}."
                    )));
                }
                changes.push((field, value.clone()));
                field_changes.push(FieldChange {
                    resource_name: rn.clone(),
                    field: field.gaql,
                    value: value.clone(),
                });
            }
            let Some((first, _)) = changes.first() else {
                return Err(invalid(format!("No fields to restore on {rn}.")));
            };
            watch.push(Watch {
                resource_name: rn.clone(),
                resource: first.resource,
                fields: changes.iter().map(|(f, _)| f.gaql).collect(),
            });
            operations.push(fields::update_op(rn, &changes).map_err(invalid)?);
        }
        Ok(WritePlan {
            summary: format!(
                "Restore {} field(s) on {} resource(s).",
                field_changes.len(),
                operations.len()
            ),
            warning: None,
            operations,
            field_changes,
            watch,
            inverse: InverseSpec::RestoreFields,
            direct: None,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveResourcesInput {
    /// Resource names to remove, in this order.
    resource_names: Vec<String>,
}

struct RemoveResources;

#[async_trait]
impl WriteTool for RemoveResources {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<RemoveResourcesInput>(
            REMOVE_RESOURCES,
            "Removes Google Ads resources (permanent).",
            ToolGroup::Edit,
            false,
        )
        .destructive()
        .hidden()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: RemoveResourcesInput = parse_input(input)?;
        if input.resource_names.is_empty() {
            return Err(invalid("No resources to remove.").with_field("resource_names"));
        }
        let operations = input
            .resource_names
            .iter()
            .map(|rn| {
                own_resource(ctx, rn)?;
                resources::remove_op(rn).map_err(invalid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WritePlan {
            summary: format!(
                "Remove {} resource(s): {}.",
                operations.len(),
                input.resource_names.join(", ")
            ),
            warning: None,
            operations,
            field_changes: Vec::new(),
            watch: Vec::new(),
            inverse: InverseSpec::None,
            direct: None,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveYoutubeUploadInput {
    /// `customers/{c}/youTubeVideoUploads/{id}`.
    resource_name: String,
}

/// Inverse of `upload_video`: `youTubeVideoUploads:remove` (the video becomes
/// `UNAVAILABLE`; this cannot be undone).
struct RemoveYoutubeUpload;

#[async_trait]
impl WriteTool for RemoveYoutubeUpload {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<RemoveYoutubeUploadInput>(
            REMOVE_YOUTUBE_UPLOAD,
            "Removes a video uploaded to YouTube through Google Ads (permanent).",
            ToolGroup::Media,
            false,
        )
        .destructive()
        .hidden()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: RemoveYoutubeUploadInput = parse_input(input)?;
        let rn = input.resource_name.trim();
        own_resource(ctx, rn).map_err(|e| e.with_field("resource_name"))?;
        let prefix = format!("customers/{}/youTubeVideoUploads/", ctx.customer_id);
        if !rn
            .strip_prefix(&prefix)
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
        {
            return Err(invalid(format!(
                "Expected a name in the format {prefix}ID (got '{rn}')."
            ))
            .with_field("resource_name"));
        }
        Ok(WritePlan {
            summary: format!("Remove uploaded video {rn}."),
            warning: None,
            operations: Vec::new(),
            field_changes: Vec::new(),
            watch: vec![Watch {
                resource_name: rn.to_string(),
                resource: "you_tube_video_upload",
                fields: vec!["you_tube_video_upload.state"],
            }],
            inverse: InverseSpec::None,
            direct: Some(DirectExec::Post {
                path_suffix: upload::REMOVE_SUFFIX.to_string(),
                body: upload::remove_body(&ctx.customer_id, rn),
            }),
        })
    }
}
