//! Undo: applies the inverse of a change, with conflict detection.

use adsconsole_core::db::{ChangeRecord, ChangeStatus, InverseOp};
use serde_json::{Value, json};

use super::write::UndoOf;
use super::{Engine, internal};
use crate::adapter::Call;
use crate::diff::diff;
use crate::error::{ToolError, codes};

impl Engine {
    /// Undoes an applied change by sending the inverse through the normal write flow.
    /// Without `force`, refuses with `UNDO_CONFLICT` if the object changed after the change.
    /// Returns the same body as a write (`applied` or `pending`).
    pub async fn undo(
        &self,
        change_id: &str,
        force: bool,
        requested_by: Option<String>,
    ) -> Result<Value, ToolError> {
        // In read-only mode, not even the read of the current state reaches the adapter.
        self.ensure_writable()?;
        let change = self.load_change(change_id)?;
        let state_error = |message: String| {
            ToolError::new(codes::INVALID_STATE, message)
                .with_details(json!({ "change_id": change_id, "status": change.status.as_str() }))
        };
        match change.status {
            ChangeStatus::Applied => {}
            ChangeStatus::Pending => {
                return Err(state_error(
                    "The change is still pending. Reject it in AdsConsole instead of undoing it."
                        .into(),
                ));
            }
            ChangeStatus::Undone => {
                return Err(state_error(format!(
                    "Already undone by {}.",
                    change.undone_by.as_deref().unwrap_or("another change")
                )));
            }
            other => {
                return Err(state_error(format!(
                    "Only an applied change can be undone; this one has status {}.",
                    other.as_str()
                )));
            }
        }
        let Some(inverse) = change.inverse.clone() else {
            return Err(
                ToolError::new(codes::NOT_REVERSIBLE, "This change cannot be undone.")
                    .with_details(json!({ "change_id": change_id })),
            );
        };
        if let Some(in_flight) = self.db.find_undo_in_flight(change_id).map_err(internal)? {
            return Err(state_error(format!(
                "An undo is already pending: {}.",
                in_flight.change_id
            )));
        }
        // Early check, for an immediate answer; `apply` checks again under the account
        // mutex, because the approval may come hours later.
        if !force && let Some(e) = self.undo_conflict(&change, &inverse).await? {
            return Err(e);
        }
        // The inverse is valid even if hidden or turned off: the toggle that counts is undo_change's.
        let spec = self
            .registry
            .get(&inverse.tool)
            .ok_or_else(|| internal(format!("inverse {} not registered", inverse.tool)))?;
        let mut input = match inverse.input {
            Value::Object(m) => m,
            other => {
                return Err(internal(format!("inverse input is not an object: {other}")));
            }
        };
        input.insert("account_id".into(), json!(change.account_id));
        input.insert("connection".into(), json!(change.connection_id));
        let call = Call {
            connection_id: change.connection_id,
            account_id: change.account_id,
            tool: inverse.tool,
            input: Value::Object(input),
        };
        let undo = UndoOf {
            change_id: change_id.to_string(),
            force,
        };
        self.submit_write(call, spec, requested_by, Some(undo))
            .await
    }

    /// `UNDO_CONFLICT` if the current state of the target differs from the original's `after`.
    pub(crate) async fn undo_conflict(
        &self,
        original: &ChangeRecord,
        inverse: &InverseOp,
    ) -> Result<Option<ToolError>, ToolError> {
        let current = self
            .adapter
            .current_state(
                &original.connection_id,
                &original.account_id,
                &inverse.target,
            )
            .await?;
        let diffs = diff(original.after.as_ref().unwrap_or(&Value::Null), &current);
        if diffs.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            ToolError::new(
                codes::UNDO_CONFLICT,
                "The object was modified after this change.",
            )
            .with_details(json!({ "diffs": diffs }))
            .with_suggestion(
                "The object changed after this modification. Use force=true to undo anyway.",
            ),
        ))
    }
}
