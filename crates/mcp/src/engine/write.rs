//! Fluxo de escrita: checagens, histórico e aplicação sob o mutex da conta.

use adsconsole_core::config::WriteMode;
use adsconsole_core::db::{ChangeRecord, ChangeStatus, Db, new_change_id};
use chrono::Utc;
use serde_json::{Value, json};

use super::{Engine, EngineEvent, internal};
use crate::adapter::{Call, Progress, ToolSpec, Validated};
use crate::error::{ToolError, codes};

const IRREVERSIBLE_WARNING: &str = "Esta mudança não pode ser desfeita.";

/// Marca uma escrita como desfazer de outra mudança.
pub(crate) struct UndoOf {
    pub change_id: String,
    /// Aplica mesmo se o objeto mudou depois da original.
    pub force: bool,
}

impl Engine {
    /// Checa o modo de escrita, faz `precheck` e `validate` (sem gravar nada se falharem)
    /// e segue conforme o modo. `undo` marca a mudança como desfazer de outra.
    pub(crate) async fn submit_write(
        &self,
        call: Call,
        spec: &ToolSpec,
        requested_by: Option<String>,
        undo: Option<UndoOf>,
    ) -> Result<Value, ToolError> {
        // Ruling B: também aqui (o desfazer da Task 10 entra direto), antes do precheck.
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
                // Prévia do "antes" para o diff na tela; `apply` tira outro na execução.
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
                // Assina antes de gravar, para não perder uma aprovação rápida.
                let events = self.subscribe();
                self.queue_and_wait(change, warning, events).await
            }
            WriteMode::ReadOnly => unreachable!("tratado acima"),
        }
    }

    /// `READ_ONLY_MODE` quando a plataforma está em só leitura.
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
                "O modo de escrita de {} está em só leitura. Mude em AdsConsole → Ferramentas e escrita.",
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

    /// Roda `apply` numa task própria: se o cliente desconectar e o future de quem chamou
    /// for descartado, a mudança termina e o histórico fica certo.
    pub(crate) async fn apply_detached(
        &self,
        change: ChangeRecord,
    ) -> Result<ChangeRecord, ToolError> {
        let engine = self.clone();
        tokio::spawn(async move { engine.apply(change).await })
            .await
            .map_err(internal)?
    }

    /// Aplica uma mudança já aprovada, sob o mutex da conta, e grava o resultado.
    /// Devolve o registro final (`Applied` ou `Failed`); `Err` só para falha interna.
    pub(crate) async fn apply(&self, mut change: ChangeRecord) -> Result<ChangeRecord, ToolError> {
        let _guard = self.locks.lock(change.platform, &change.account_id).await;
        let call = Call {
            connection_id: change.connection_id.clone(),
            account_id: change.account_id.clone(),
            tool: change.tool.clone(),
            input: change.input.clone(),
        };
        // Desfazer: sob o mutex da conta, confere de novo o que o `undo` checou antes
        // (outro desfazer pode ter vencido, ou o objeto pode ter mudado na espera).
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

    /// `precheck` de novo (a aprovação pode ter vindo horas depois), snapshot e execute.
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

    /// Motivo para não aplicar `change`, se ela desfaz outra: `INVALID_STATE` se a
    /// original não está mais `Applied`; `UNDO_CONFLICT` (sem `force`) se o objeto
    /// mudou depois da original.
    async fn undo_blocked(&self, change: &ChangeRecord) -> Result<Option<ToolError>, ToolError> {
        let Some(orig_id) = &change.undo_of else {
            return Ok(None);
        };
        let Some(orig) = self.db.get_change(orig_id).map_err(internal)? else {
            return Ok(None);
        };
        if orig.status == ChangeStatus::Applied {
            return Ok(match (&orig.inverse, change.undo_force) {
                // Falha ao ler o estado atual também impede a execução (vira `Failed`).
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
                    "Já desfeita por {}.",
                    orig.undone_by.as_deref().unwrap_or("outra mudança")
                ),
            )),
            other => Some(ToolError::new(
                codes::INVALID_STATE,
                format!(
                    "A mudança {orig_id} não pode mais ser desfeita: status {}.",
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

/// Grava o progresso de uma escrita em vários passos na própria mudança. Erro de
/// gravação não derruba a escrita em andamento.
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
            tracing::error!(
                change_id = self.change_id,
                "falha ao gravar o progresso: {e}"
            );
        }
    }
}

/// Resposta da ferramenta para um registro final: corpo de sucesso para `Applied`
/// (com os avisos do `validate` em `warning`), o erro original (com
/// `details.change_id`) para `Failed`.
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
            // O aviso da fila (modo aprovação) já pode trazer o de irreversível.
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
                    ToolError::new(codes::EXECUTION_FAILED, "A mudança falhou ao ser aplicada.")
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
            format!(
                "A mudança {} está com status {}.",
                change.change_id,
                other.as_str()
            ),
        )
        .with_details(json!({ "change_id": change.change_id }))),
    }
}
