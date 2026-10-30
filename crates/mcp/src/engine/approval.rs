//! Fila de aprovação: espera da resposta, decisão única (aprovar/recusar) e expiração.
//!
//! Toda decisão passa por `transition_status` (compare-and-set) a partir de `Pending`,
//! então só uma delas vence, mesmo com chamadas concorrentes ou depois de um reinício.

use adsconsole_core::db::{ChangeRecord, ChangeStatus, PendingApproval};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::write::change_outcome;
use super::{Engine, EngineEvent, internal};
use crate::error::{ToolError, codes};

/// Quanto tempo uma pendência espera por uma decisão antes de expirar.
pub const PENDING_TTL: chrono::Duration = chrono::Duration::hours(24);

const PENDING_MESSAGE: &str =
    "Aguardando aprovação no AdsConsole. Consulte o resultado com get_change_status.";

impl Engine {
    /// Aprova e executa uma mudança pendente. Devolve o registro final; se a execução
    /// falhar, o registro vem com status `Failed` (a aprovação em si foi feita).
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

    /// Recusa uma mudança pendente; nada é executado.
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

    /// Pendências desta plataforma, mais novas primeiro.
    pub fn pending(&self) -> Result<Vec<PendingApproval>, ToolError> {
        self.db
            .list_pending(Some(self.platform()))
            .map_err(internal)
    }

    /// Expira as pendências desta plataforma vencidas antes de `now`; devolve os ids expirados.
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
                // Já decidida por outro caminho: a linha da fila não serve mais.
                self.db.delete_pending(&id).map_err(internal)?;
            }
        }
        Ok(expired)
    }

    /// `Pending → Expired`, remove da fila e avisa. `false` se já tinha sido decidida.
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

    /// Grava a mudança como pendente e espera a decisão por até `approval_wait_secs`.
    /// `events` precisa ter sido assinado antes da gravação, para não perder uma
    /// aprovação rápida. Se o future for descartado, a pendência continua na fila.
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
            // Sem linha na fila a pendência nunca seria decidida nem expiraria. Ninguém
            // mais conhece este id (nem evento saiu), então dá para sobrescrever direto.
            let err = internal(e);
            let mut failed = change;
            failed.status = ChangeStatus::Failed;
            failed.reversible = false;
            failed.error = serde_json::to_value(&err).ok();
            if let Err(update_err) = self.db.update_change(&failed) {
                tracing::error!(
                    change_id = change_id.as_str(),
                    error = %update_err,
                    "falha ao marcar como falha a mudança que não entrou na fila"
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
        // Tempo esgotado não é erro: cai na leitura do banco abaixo.
        if let Ok(result) =
            tokio::time::timeout(wait, self.wait_decision(&mut events, &change_id)).await
        {
            result?;
        }

        // Lê o banco mesmo no tempo esgotado: pode ter sido decidida no limite.
        let change = self.load_change(&change_id)?;
        match change.status {
            ChangeStatus::Applied | ChangeStatus::Failed => {
                change_outcome(&change, warning.as_deref())
            }
            ChangeStatus::Rejected => Err(ToolError::new(
                codes::APPROVAL_REJECTED,
                "O usuário recusou esta mudança no AdsConsole.",
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

    /// Espera um evento final desta mudança.
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
                // Eventos perdidos: o banco diz se já houve decisão.
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

    /// A mudança desta plataforma, ou `CHANGE_NOT_FOUND`.
    pub(super) fn load_change(&self, change_id: &str) -> Result<ChangeRecord, ToolError> {
        self.db
            .get_change(change_id)
            .map_err(internal)?
            .filter(|c| c.platform == self.platform())
            .ok_or_else(|| {
                ToolError::new(
                    codes::CHANGE_NOT_FOUND,
                    format!("Mudança {change_id} não encontrada."),
                )
                .with_details(json!({ "change_id": change_id }))
            })
    }

    fn already_decided(&self, change_id: &str) -> Result<ToolError, ToolError> {
        let status = self.load_change(change_id)?.status;
        Ok(ToolError::new(
            codes::INVALID_STATE,
            format!("Esta mudança já foi decidida: {}.", status.as_str()),
        )
        .with_details(json!({ "change_id": change_id, "status": status.as_str() })))
    }
}

fn expired_error(change_id: &str) -> ToolError {
    ToolError::new(
        codes::APPROVAL_EXPIRED,
        "A aprovação desta mudança expirou sem decisão.",
    )
    .with_details(json!({ "change_id": change_id }))
}
