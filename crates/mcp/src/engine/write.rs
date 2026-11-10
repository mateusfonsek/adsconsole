//! Write flow: checks, history and application under the account mutex.

use adsconsole_core::config::WriteMode;
use adsconsole_core::db::{ChangeRecord, ChangeStatus, Db, new_change_id};
use chrono::Utc;
use serde_json::{Value, json};

use super::{Engine, EngineEvent, internal};
use crate::adapter::{Call, Progress, ToolSpec, Validated};
use crate::error::{ToolError, codes};

const IRREVERSIBLE_WARNING: &str = "This change cannot be undone.";

/// Marks a write as the undo of another change.
pub(crate) struct UndoOf {
    pub change_id: String,
    /// Applies even if the object changed after the original.
    pub force: bool,
}

impl Engine {
    /// Checks the write mode, runs `precheck` and `validate` (recording nothing if they fail)
    /// and proceeds according to the mode. `undo` marks the change as the undo of another.
    pub(crate) async fn submit_write(
        &self,
        call: Call,
        spec: &ToolSpec,
        requested_by: Option<String>,
        undo: Option<UndoOf>,
    ) -> Result<Value, ToolError> {
        // The read-only check runs here too, before the precheck: undo calls submit_write directly.
        let mode = self.settings().write_mode;
        if mode == WriteMode::ReadOnly {
            return Err(self.read_only_error());
        }
        self.adapter.precheck(&call).await?;
        let validated = self.adapter.validate(&call).await?;
        match mode {
            WriteMode::Direct => {
                let change = self.new_change(
                    call,
                    spec,
                    &validated,
                    ChangeStatus::Approved,
                    requested_by,
                    undo,
                );
                self.db.insert_change(&change).map_err(internal)?;
                let change = self.apply_detached(change).await?;
                change_outcome(&change, validated.warning.as_deref())
            }
            WriteMode::Approval => {
                // Preview of the "before" for the on-screen diff; `apply` takes another at execution.
                let before = self.adapter.snapshot(&call).await?;
                let mut change = self.new_change(
                    call,
                    spec,
                    &validated,
                    ChangeStatus::Pending,
                    requested_by,
                    undo,
                );
                change.before = Some(before);
                let warning = match (validated.warning, spec.reversible) {
                    (w, true) => w,
                    (Some(w), false) => Some(format!("{w} {IRREVERSIBLE_WARNING}")),
                    (None, false) => Some(IRREVERSIBLE_WARNING.to_string()),
                };
                // Subscribe before recording, so a quick approval is not missed.
                let events = self.subscribe();
                self.queue_and_wait(change, warning, events).await
            }
            WriteMode::ReadOnly => unreachable!("handled above"),
        }
    }

    /// `READ_ONLY_MODE` when the platform is in read-only mode.
    pub(crate) fn ensure_writable(&self) -> Result<(), ToolError> {
        if self.settings().write_mode == WriteMode::ReadOnly {
            return Err(self.read_only_error());
        }
        Ok(())
    }

    fn read_only_error(&self) -> ToolError {
        ToolError::new(
            codes::READ_ONLY_MODE,
            format!(
                "The write mode of {} is read-only. Change it in AdsConsole → Tools.",
                self.platform().display_name()
            ),
        )
    }

    fn new_change(
        &self,
        call: Call,
        spec: &ToolSpec,
        validated: &Validated,
        status: ChangeStatus,
        requested_by: Option<String>,
        undo: Option<UndoOf>,
    ) -> ChangeRecord {
        let (undo_of, undo_force) = match undo {
            Some(u) => (Some(u.change_id), u.force),
            None => (None, false),
        };
        let now = Utc::now();
        ChangeRecord {
            change_id: new_change_id(),
            platform: self.platform(),
            account_id: call.account_id,
            connection_id: call.connection_id,
            tool: call.tool,
            input: call.input,
            summary: validated.summary.clone(),
            before: None,
            after: None,
            inverse: None,
            reversible: spec.reversible,
            status,
            requested_by,
            undo_of,
            undo_force,
            undone_by: None,
            error: None,
            progress: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Runs `apply` in its own task: if the client disconnects and the caller's future
    /// is dropped, the change still finishes and the history stays correct.
    pub(crate) async fn apply_detached(
        &self,
        change: ChangeRecord,
    ) -> Result<ChangeRecord, ToolError> {
        let engine = self.clone();
        tokio::spawn(async move { engine.apply(change).await })
            .await
            .map_err(internal)?
    }

    /// Applies an already-approved change, under the account mutex, and records the result.
    /// Returns the final record (`Applied` or `Failed`); `Err` only for an internal failure.
    pub(crate) async fn apply(&self, mut change: ChangeRecord) -> Result<ChangeRecord, ToolError> {
        let _guard = self.locks.lock(change.platform, &change.account_id).await;
        let call = Call {
            connection_id: change.connection_id.clone(),
            account_id: change.account_id.clone(),
            tool: change.tool.clone(),
            input: change.input.clone(),
        };
        // Undo: under the account mutex, re-checks what `undo` checked earlier
        // (another undo may have won, or the object may have changed while waiting).
        let outcome = match self.undo_blocked(&change).await? {
            Some(e) => Err(e),
            None => self.run(&call, &mut change).await,
        };
        match outcome {
            Ok(()) => {
                change.status = ChangeStatus::Applied;
                self.db.update_change(&change).map_err(internal)?;
                if let Some(orig) = &change.undo_of {
                    self.mark_undone(orig, &change.change_id)?;
                }
                self.emit(EngineEvent::ChangeApplied {
                    change_id: change.change_id.clone(),
                });
            }
            Err(e) => {
                change.status = ChangeStatus::Failed;
                change.reversible = false;
                change.error = Some(serde_json::to_value(&e).map_err(internal)?);
                self.db.update_change(&change).map_err(internal)?;
                self.emit(EngineEvent::ChangeFailed {
                    change_id: change.change_id.clone(),
                });
            }
        }
        Ok(change)
    }

    /// `precheck` again (the approval may have come hours later), snapshot and execute.
    async fn run(&self, call: &Call, change: &mut ChangeRecord) -> Result<(), ToolError> {
        self.adapter.precheck(call).await?;
        let before = self.adapter.snapshot(call).await?;
        change.before = Some(before.clone());
        let progress = DbProgress {
            db: &self.db,
            change_id: &change.change_id,
        };
        let applied = self
            .adapter
            .execute_tracked(call, &before, &progress)
            .await?;
        change.after = Some(applied.after);
        change.reversible = applied.inverse.is_some();
        change.inverse = applied.inverse;
        Ok(())
    }

    /// Reason not to apply `change`, if it undoes another: `INVALID_STATE` if the
    /// original is no longer `Applied`; `UNDO_CONFLICT` (without `force`) if the object
    /// changed after the original.
    async fn undo_blocked(&self, change: &ChangeRecord) -> Result<Option<ToolError>, ToolError> {
        let Some(orig_id) = &change.undo_of else {
            return Ok(None);
        };
        let Some(orig) = self.db.get_change(orig_id).map_err(internal)? else {
            return Ok(None);
        };
        if orig.status == ChangeStatus::Applied {
            return Ok(match (&orig.inverse, change.undo_force) {
                // A failure to read the current state also blocks execution (becomes `Failed`).
                (Some(inverse), false) => self
                    .undo_conflict(&orig, inverse)
                    .await
                    .unwrap_or_else(Some),
                _ => None,
            });
        }
        Ok(match orig.status {
            ChangeStatus::Applied => None,
            ChangeStatus::Undone => Some(ToolError::new(
                codes::INVALID_STATE,
                format!(
                    "Already undone by {}.",
                    orig.undone_by.as_deref().unwrap_or("another change")
                ),
            )),
            other => Some(ToolError::new(
                codes::INVALID_STATE,
                format!(
                    "Change {orig_id} can no longer be undone: status {}.",
                    other.as_str()
                ),
            )),
        }
        .map(|e| e.with_details(json!({ "change_id": orig_id, "status": orig.status.as_str() }))))
    }

    fn mark_undone(&self, original_id: &str, undo_id: &str) -> Result<(), ToolError> {
        self.db
            .mark_undone(original_id, undo_id)
            .map_err(internal)?;
        Ok(())
    }
}

/// Records the progress of a multi-step write on the change itself. A recording
/// error does not abort the write in progress.
struct DbProgress<'a> {
    db: &'a Db,
    change_id: &'a str,
}

impl Progress for DbProgress<'_> {
    fn created(&self, ids: &Value) {
        if let Err(e) = self
            .db
            .record_progress(self.change_id, &json!({ "created": ids }))
        {
            tracing::error!(change_id = self.change_id, "failed to record progress: {e}");
        }
    }
}

/// Tool response for a final record: a success body for `Applied` (with the
/// `validate` warnings in `warning`), the original error (with `details.change_id`)
/// for `Failed`.
pub(crate) fn change_outcome(
    change: &ChangeRecord,
    warning: Option<&str>,
) -> Result<Value, ToolError> {
    match change.status {
        ChangeStatus::Applied => {
            let mut body = json!({
                "status": "applied",
                "change_id": change.change_id,
                "summary": change.summary,
                "reversible": change.reversible,
                "result": change.after.clone().unwrap_or(Value::Null),
            });
            let mut warnings: Vec<&str> = warning.into_iter().collect();
            // The queue warning (approval mode) may already carry the irreversible one.
            if !change.reversible && !warnings.iter().any(|w| w.contains(IRREVERSIBLE_WARNING)) {
                warnings.push(IRREVERSIBLE_WARNING);
            }
            if !warnings.is_empty() {
                body["warning"] = Value::String(warnings.join(" "));
            }
            Ok(body)
        }
        ChangeStatus::Failed => {
            let mut err = change
                .error
                .clone()
                .and_then(|v| serde_json::from_value::<ToolError>(v).ok())
                .unwrap_or_else(|| {
                    ToolError::new(codes::EXECUTION_FAILED, "The change failed to apply.")
                });
            let mut details = match err.details.take() {
                Some(Value::Object(m)) => m,
                Some(other) => {
                    let mut m = serde_json::Map::new();
                    m.insert("details".into(), other);
                    m
                }
                None => serde_json::Map::new(),
            };
            details.insert("change_id".into(), Value::String(change.change_id.clone()));
            err.details = Some(Value::Object(details));
            Err(err)
        }
        other => Err(ToolError::new(
            codes::INVALID_STATE,
            format!("Change {} has status {}.", change.change_id, other.as_str()),
        )
        .with_details(json!({ "change_id": change.change_id }))),
    }
}
