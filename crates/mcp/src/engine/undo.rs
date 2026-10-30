//! Desfazer: aplica a inversa de uma mudança, com detecção de conflito.

use adsconsole_core::db::{ChangeRecord, ChangeStatus, InverseOp};
use serde_json::{Value, json};

use super::write::UndoOf;
use super::{Engine, internal};
use crate::adapter::Call;
use crate::diff::diff;
use crate::error::{ToolError, codes};

impl Engine {
    /// Desfaz uma mudança aplicada passando a inversa pelo fluxo normal de escrita.
    /// Sem `force`, recusa com `UNDO_CONFLICT` se o objeto mudou depois da mudança.
    /// Devolve o mesmo corpo de uma escrita (`applied` ou `pending`).
    pub async fn undo(
        &self,
        change_id: &str,
        force: bool,
        requested_by: Option<String>,
    ) -> Result<Value, ToolError> {
        // Em só leitura, nem a leitura do estado atual chega ao adapter.
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
                    "A mudança ainda está pendente. Recuse-a no AdsConsole em vez de desfazer."
                        .into(),
                ));
            }
            ChangeStatus::Undone => {
                return Err(state_error(format!(
                    "Já desfeita por {}.",
                    change.undone_by.as_deref().unwrap_or("outra mudança")
                )));
            }
            other => {
                return Err(state_error(format!(
                    "Só uma mudança aplicada pode ser desfeita; esta está com status {}.",
                    other.as_str()
                )));
            }
        }
        let Some(inverse) = change.inverse.clone() else {
            return Err(ToolError::new(
                codes::NOT_REVERSIBLE,
                "Esta mudança não pode ser desfeita.",
            )
            .with_details(json!({ "change_id": change_id })));
        };
        if let Some(in_flight) = self.db.find_undo_in_flight(change_id).map_err(internal)? {
            return Err(state_error(format!(
                "Já existe um desfazer pendente: {}.",
                in_flight.change_id
            )));
        }
        // Checagem antecipada, para resposta imediata; `apply` confere de novo sob o
        // mutex da conta, porque a aprovação pode vir horas depois.
        if !force && let Some(e) = self.undo_conflict(&change, &inverse).await? {
            return Err(e);
        }
        // A inversa vale mesmo oculta ou desligada: o toggle que conta é o do undo_change.
        let spec = self
            .registry
            .get(&inverse.tool)
            .ok_or_else(|| internal(format!("inversa {} não registrada", inverse.tool)))?;
        let mut input = match inverse.input {
            Value::Object(m) => m,
            other => {
                return Err(internal(format!(
                    "entrada da inversa não é objeto: {other}"
                )));
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

    /// `UNDO_CONFLICT` se o estado atual do alvo difere do `after` da original.
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
                "O objeto foi alterado depois desta mudança.",
            )
            .with_details(json!({ "diffs": diffs }))
            .with_suggestion(
                "O objeto mudou depois desta alteração. Use force=true para desfazer mesmo assim.",
            ),
        ))
    }
}
