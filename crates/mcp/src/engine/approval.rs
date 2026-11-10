//! Approval queue: waiting for the answer, a single decision (approve/reject) and expiry.
//!
//! Every decision goes through `transition_status` (compare-and-set) from `Pending`,
//! so only one of them wins, even with concurrent calls or after a restart.

use adsconsole_core::db::{ChangeRecord, ChangeStatus, PendingApproval};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::write::change_outcome;
use super::{Engine, EngineEvent, internal};
use crate::error::{ToolError, codes};

/// How long a pending change waits for a decision before expiring.
pub const PENDING_TTL: chrono::Duration = chrono::Duration::hours(24);

const PENDING_MESSAGE: &str =
    "Waiting for approval in AdsConsole. Check the result with get_change_status.";

impl Engine {
    /// Approves and executes a pending change. Returns the final record; if execution
    /// fails, the record comes back with status `Failed` (the approval itself was done).
    pub async fn approve(&self, change_id: &str) -> Result<ChangeRecord, ToolError> {
        self.load_change(change_id)?;
        let now = Utc::now();
        let expires_at = self
            .db
            .list_pending(Some(self.platform()))
            .map_err(internal)?
            .into_iter()
            .find(|p| p.change.change_id == change_id)
            .map(|p| p.expires_at);
        if let Some(expires_at) = expires_at
            && expires_at < now
            && self.expire(change_id)?
        {
            return Err(expired_error(change_id));
        }
        if !self
            .db
            .transition_status(change_id, ChangeStatus::Pending, ChangeStatus::Approved)
            .map_err(internal)?
        {
            return Err(self.already_decided(change_id)?);
        }
        self.db.delete_pending(change_id).map_err(internal)?;
        let change = self.load_change(change_id)?;
        self.apply_detached(change).await
    }

    /// Rejects a pending change; nothing is executed.
    pub async fn reject(&self, change_id: &str) -> Result<ChangeRecord, ToolError> {
        self.load_change(change_id)?;
        if !self
            .db
            .transition_status(change_id, ChangeStatus::Pending, ChangeStatus::Rejected)
            .map_err(internal)?
        {
            return Err(self.already_decided(change_id)?);
        }
        self.db.delete_pending(change_id).map_err(internal)?;
        self.emit(EngineEvent::ChangeRejected {
            change_id: change_id.to_string(),
        });
        self.load_change(change_id)
    }

    /// Pending changes of this platform, newest first.
    pub fn pending(&self) -> Result<Vec<PendingApproval>, ToolError> {
        self.db
            .list_pending(Some(self.platform()))
            .map_err(internal)
    }

    /// Expires the pending changes of this platform that are overdue before `now`; returns the expired ids.
    pub fn expire_stale(&self, now: DateTime<Utc>) -> Result<Vec<String>, ToolError> {
        let mut expired = Vec::new();
        for id in self.db.pending_expired_before(now).map_err(internal)? {
            let Some(change) = self.db.get_change(&id).map_err(internal)? else {
                continue;
            };
            if change.platform != self.platform() {
                continue;
            }
            if self.expire(&id)? {
                expired.push(id);
            } else if change.status != ChangeStatus::Pending {
                // Already decided by another path: the queue row is no longer needed.
                self.db.delete_pending(&id).map_err(internal)?;
            }
        }
        Ok(expired)
    }

    /// `Pending → Expired`, removes it from the queue and notifies. `false` if it had already been decided.
    fn expire(&self, change_id: &str) -> Result<bool, ToolError> {
        if !self
            .db
            .transition_status(change_id, ChangeStatus::Pending, ChangeStatus::Expired)
            .map_err(internal)?
        {
            return Ok(false);
        }
        self.db.delete_pending(change_id).map_err(internal)?;
        self.emit(EngineEvent::ChangeExpired {
            change_id: change_id.to_string(),
        });
        Ok(true)
    }

    /// Records the change as pending and waits for the decision for up to `approval_wait_secs`.
    /// `events` must have been subscribed before the write, so a quick approval is not
    /// missed. If the future is dropped, the pending change stays in the queue.
    pub(super) async fn queue_and_wait(
        &self,
        change: ChangeRecord,
        warning: Option<String>,
        mut events: broadcast::Receiver<EngineEvent>,
    ) -> Result<Value, ToolError> {
        let change_id = change.change_id.clone();
        self.db.insert_change(&change).map_err(internal)?;
        if let Err(e) = self.db.insert_pending(
            &change_id,
            change.created_at + PENDING_TTL,
            warning.as_deref(),
        ) {
            // Without a queue row the pending change would never be decided or expire. Nobody
            // else knows this id (no event went out either), so it can be overwritten directly.
            let err = internal(e);
            let mut failed = change;
            failed.status = ChangeStatus::Failed;
            failed.reversible = false;
            failed.error = serde_json::to_value(&err).ok();
            if let Err(update_err) = self.db.update_change(&failed) {
                tracing::error!(
                    change_id = change_id.as_str(),
                    error = %update_err,
                    "failed to mark as failed the change that did not enter the queue"
                );
            }
            return Err(err);
        }
        self.emit(EngineEvent::ApprovalRequested {
            change_id: change_id.clone(),
            platform: change.platform,
            summary: change.summary.clone(),
            warning: warning.clone(),
        });

        let wait = std::time::Duration::from_secs(self.settings().approval_wait_secs);
        // A timeout is not an error: falls through to the database read below.
        if let Ok(result) =
            tokio::time::timeout(wait, self.wait_decision(&mut events, &change_id)).await
        {
            result?;
        }

        // Read the database even on timeout: it may have been decided right at the limit.
        let change = self.load_change(&change_id)?;
        match change.status {
            ChangeStatus::Applied | ChangeStatus::Failed => {
                change_outcome(&change, warning.as_deref())
            }
            ChangeStatus::Rejected => Err(ToolError::new(
                codes::APPROVAL_REJECTED,
                "The user rejected this change in AdsConsole.",
            )
            .with_details(json!({ "change_id": change_id }))),
            ChangeStatus::Expired => Err(expired_error(&change_id)),
            _ => Ok(json!({
                "status": "pending",
                "change_id": change_id,
                "summary": change.summary,
                "message": PENDING_MESSAGE,
            })),
        }
    }

    /// Waits for a final event of this change.
    async fn wait_decision(
        &self,
        events: &mut broadcast::Receiver<EngineEvent>,
        change_id: &str,
    ) -> Result<(), ToolError> {
        loop {
            match events.recv().await {
                Ok(
                    EngineEvent::ChangeApplied { change_id: id }
                    | EngineEvent::ChangeFailed { change_id: id }
                    | EngineEvent::ChangeRejected { change_id: id }
                    | EngineEvent::ChangeExpired { change_id: id },
                ) if id == change_id => return Ok(()),
                Ok(_) => {}
                // Lost events: the database says whether a decision has been made.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if self.is_decided(change_id)? {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }

    fn is_decided(&self, change_id: &str) -> Result<bool, ToolError> {
        Ok(!matches!(
            self.load_change(change_id)?.status,
            ChangeStatus::Pending | ChangeStatus::Approved
        ))
    }

    /// The change of this platform, or `CHANGE_NOT_FOUND`.
    pub(super) fn load_change(&self, change_id: &str) -> Result<ChangeRecord, ToolError> {
        self.db
            .get_change(change_id)
            .map_err(internal)?
            .filter(|c| c.platform == self.platform())
            .ok_or_else(|| {
                ToolError::new(
                    codes::CHANGE_NOT_FOUND,
                    format!("Change {change_id} not found."),
                )
                .with_details(json!({ "change_id": change_id }))
            })
    }

    fn already_decided(&self, change_id: &str) -> Result<ToolError, ToolError> {
        let status = self.load_change(change_id)?.status;
        Ok(ToolError::new(
            codes::INVALID_STATE,
            format!("This change has already been decided: {}.", status.as_str()),
        )
        .with_details(json!({ "change_id": change_id, "status": status.as_str() })))
    }
}

fn expired_error(change_id: &str) -> ToolError {
    ToolError::new(
        codes::APPROVAL_EXPIRED,
        "The approval of this change expired without a decision.",
    )
    .with_details(json!({ "change_id": change_id }))
}
