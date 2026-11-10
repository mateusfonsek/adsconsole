//! The engine's own history tools: list_changes, get_change_status and undo_change.

use adsconsole_core::config::ToolGroup;
use adsconsole_core::db::{ChangeFilter, ChangeRecord, ChangeStatus};
use adsconsole_core::routing::normalize_account_id;
use chrono::{DateTime, SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Engine, internal};
use crate::adapter::ToolSpec;
use crate::error::{ToolError, codes};

pub(super) const LIST_CHANGES: &str = "list_changes";
pub(super) const GET_CHANGE_STATUS: &str = "get_change_status";
pub(super) const UNDO_CHANGE: &str = "undo_change";

const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 100;

#[derive(Deserialize, JsonSchema)]
struct ListChangesInput {
    /// Only the changes of this account.
    #[serde(default)]
    account_id: Option<String>,
    /// Only the changes with this status: pending, approved, applied, rejected, expired, failed or undone.
    #[serde(default)]
    status: Option<String>,
    /// How many changes to return (default 20, maximum 100).
    #[serde(default)]
    limit: Option<u32>,
    /// The `next_cursor` of the previous page.
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ChangeIdInput {
    /// Change id (`chg_…`).
    change_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct UndoChangeInput {
    /// Id of the change to undo (`chg_…`).
    change_id: String,
    /// Undoes even if the object changed after the modification.
    #[serde(default)]
    force: bool,
}

pub(super) fn is_history_tool(name: &str) -> bool {
    matches!(name, LIST_CHANGES | GET_CHANGE_STATUS | UNDO_CHANGE)
}

/// The three tools, registered together with the platform's.
pub(super) fn history_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::read::<ListChangesInput>(
            LIST_CHANGES,
            "Lists the changes made by this server, newest first.",
            ToolGroup::History,
        )
        .no_account(),
        ToolSpec::read::<ChangeIdInput>(
            GET_CHANGE_STATUS,
            "Shows one change in full: status, before, after and error.",
            ToolGroup::History,
        )
        .no_account(),
        ToolSpec::write::<UndoChangeInput>(
            UNDO_CHANGE,
            "Undoes an applied change. Refuses if the object changed after it, unless force=true.",
            ToolGroup::History,
            true,
        )
        .no_account(),
    ]
}

fn parse<T: DeserializeOwned>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input)
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Invalid input: {e}")))
}

impl Engine {
    /// Dispatches a history tool (see [`is_history_tool`]).
    pub(super) async fn call_history(
        &self,
        name: &str,
        input: Value,
        requested_by: Option<String>,
    ) -> Result<Value, ToolError> {
        match name {
            LIST_CHANGES => self.list_changes_tool(input),
            GET_CHANGE_STATUS => self.get_change_status_tool(input),
            UNDO_CHANGE => {
                let i: UndoChangeInput = parse(input)?;
                self.undo(&i.change_id, i.force, requested_by).await
            }
            other => Err(internal(format!("{other} is not a history tool"))),
        }
    }

    fn list_changes_tool(&self, input: Value) -> Result<Value, ToolError> {
        let i: ListChangesInput = parse(input)?;
        let status = i
            .status
            .as_deref()
            .map(ChangeStatus::parse)
            .transpose()
            .map_err(|_| {
                ToolError::new(
                    codes::INVALID_INPUT,
                    "Invalid status. Use pending, approved, applied, rejected, expired, failed or undone.",
                )
                .with_field("status")
            })?;
        let offset = match i.cursor.as_deref() {
            None => 0,
            Some(c) => c.parse::<u32>().map_err(|_| {
                ToolError::new(codes::INVALID_INPUT, "Invalid cursor.").with_field("cursor")
            })?,
        };
        let limit = i.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let platform = self.platform();
        let changes = self
            .db
            .list_changes(&ChangeFilter {
                platform: Some(platform),
                account_id: i.account_id.map(|a| normalize_account_id(platform, &a)),
                status,
                limit,
                offset,
            })
            .map_err(internal)?;
        let mut out = json!({ "changes": changes.iter().map(list_item).collect::<Vec<_>>() });
        if changes.len() == limit as usize {
            out["next_cursor"] = json!(offset.saturating_add(limit).to_string());
        }
        Ok(out)
    }

    fn get_change_status_tool(&self, input: Value) -> Result<Value, ToolError> {
        let i: ChangeIdInput = parse(input)?;
        let change = self.load_change(&i.change_id)?;
        let mut out = serde_json::to_value(&change).map_err(internal)?;
        out["created_at"] = json!(timestamp(&change.created_at));
        out["updated_at"] = json!(timestamp(&change.updated_at));
        Ok(out)
    }
}

fn list_item(c: &ChangeRecord) -> Value {
    json!({
        "change_id": c.change_id,
        "tool": c.tool,
        "account_id": c.account_id,
        "status": c.status.as_str(),
        "summary": c.summary,
        "reversible": c.reversible,
        "requested_by": c.requested_by,
        "undo_of": c.undo_of,
        "undone_by": c.undone_by,
        "created_at": timestamp(&c.created_at),
    })
}

/// RFC 3339 UTC with microseconds.
fn timestamp(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}
