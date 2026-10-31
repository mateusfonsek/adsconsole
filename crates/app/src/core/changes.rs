//! Fila de aprovações e histórico de mudanças, sobre os mesmos `Engine` do servidor.

use std::sync::Arc;

use adsconsole_core::Platform;
use adsconsole_core::config::WriteMode;
use adsconsole_core::db::{ChangeFilter, ChangeRecord, ChangeStatus, PendingApproval};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::AppError;
use super::runtime::Runtime;

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ChangeView {
    pub id: String,
    pub platform: Platform,
    pub account_id: String,
    pub connection_name: Option<String>,
    pub tool: String,
    pub summary: String,
    /// `pending`, `approved`, `applied`, `failed`, `rejected`, `expired`...
    pub status: String,
    pub reversible: bool,
    pub created_at: DateTime<Utc>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub error: Option<Value>,
    /// Objetos criados por uma mudança que falhou no meio (para apagar à mão).
    pub orphaned_ids: Vec<String>,
    pub undone_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct PendingView {
    pub change: ChangeView,
    pub warning: Option<String>,
    pub expires_at: DateTime<Utc>,
    /// Até quando a IA ainda espera a resposta na mesma chamada.
    pub ai_waiting_until: DateTime<Utc>,
    /// `read_only`: o modo atual da plataforma não deixa aprovar.
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, specta::Type)]
pub struct HistoryFilter {
    pub platform: Option<Platform>,
    pub account_id: Option<String>,
    pub status: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: u32,
    pub offset: u32,
}

pub struct ChangesService {
    rt: Arc<Runtime>,
}

impl ChangesService {
    pub fn new(rt: Arc<Runtime>) -> Self {
        ChangesService { rt }
    }

    pub fn pending(&self) -> Result<Vec<PendingView>, AppError> {
        let config = self.rt.config();
        let list = self.rt.db().list_pending(None)?;
        list.into_iter()
            .map(|p: PendingApproval| {
                let settings = config.server(p.change.platform);
                let blocked =
                    (settings.write_mode == WriteMode::ReadOnly).then(|| "read_only".to_string());
                let wait = chrono::Duration::seconds(settings.approval_wait_secs as i64);
                Ok(PendingView {
                    ai_waiting_until: p.change.created_at + wait,
                    change: self.view(p.change)?,
                    warning: p.warning,
                    expires_at: p.expires_at,
                    blocked_reason: blocked,
                })
            })
            .collect()
    }

    pub fn history(&self, f: HistoryFilter) -> Result<Vec<ChangeView>, AppError> {
        let status = f
            .status
            .as_deref()
            .map(ChangeStatus::parse)
            .transpose()
            .map_err(|e| AppError::new("INVALID_INPUT", e.to_string()).with_field("status"))?;
        let filter = ChangeFilter {
            platform: f.platform,
            account_id: f.account_id,
            status,
            limit: f.limit.clamp(1, 200),
            offset: f.offset,
        };
        self.rt
            .db()
            .list_changes(&filter)?
            .into_iter()
            .filter(|c| f.since.is_none_or(|since| c.created_at >= since))
            .map(|c| self.view(c))
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<ChangeView, AppError> {
        let c = self.load(id)?;
        self.view(c)
    }

    pub async fn approve(&self, id: &str) -> Result<ChangeView, AppError> {
        let c = self.load(id)?;
        if self.rt.config().server(c.platform).write_mode == WriteMode::ReadOnly {
            return Err(AppError::new(
                "READ_ONLY_MODE",
                "O modo atual desta plataforma é só leitura. Recuse o pedido ou mude o modo em Ferramentas.",
            ));
        }
        let done = self.rt.engine(c.platform).approve(id).await?;
        self.view(done)
    }

    pub async fn reject(&self, id: &str) -> Result<ChangeView, AppError> {
        let c = self.load(id)?;
        let done = self.rt.engine(c.platform).reject(id).await?;
        self.view(done)
    }

    /// Recusa todas as pendências (ao sair do app). Devolve quantas recusou.
    pub async fn reject_all_pending(&self) -> Result<u32, AppError> {
        let mut n = 0;
        for p in self.rt.db().list_pending(None)? {
            if self
                .rt
                .engine(p.change.platform)
                .reject(&p.change.change_id)
                .await
                .is_ok()
            {
                n += 1;
            }
        }
        Ok(n)
    }

    pub async fn undo(&self, id: &str) -> Result<Value, AppError> {
        let c = self.load(id)?;
        Ok(self
            .rt
            .engine(c.platform)
            .undo(id, false, Some("app".into()))
            .await?)
    }

    fn load(&self, id: &str) -> Result<ChangeRecord, AppError> {
        self.rt.db().get_change(id)?.ok_or_else(|| {
            AppError::new("CHANGE_NOT_FOUND", format!("Mudança {id} não encontrada."))
        })
    }

    fn view(&self, c: ChangeRecord) -> Result<ChangeView, AppError> {
        let connection_name = self
            .rt
            .db()
            .get_connection(&c.connection_id)?
            .map(|m| m.name);
        let orphaned_ids = if c.status == ChangeStatus::Failed {
            orphans(&c)
        } else {
            Vec::new()
        };
        Ok(ChangeView {
            id: c.change_id,
            platform: c.platform,
            account_id: c.account_id,
            connection_name,
            tool: c.tool,
            summary: c.summary,
            status: c.status.as_str().to_string(),
            reversible: c.reversible,
            created_at: c.created_at,
            before: c.before,
            after: c.after,
            error: c.error,
            orphaned_ids,
            undone_by: c.undone_by,
        })
    }
}

/// Ids que a mudança chegou a criar: os do erro (`details.orphaned_ids`) ou, sem eles,
/// os do progresso gravado.
fn orphans(c: &ChangeRecord) -> Vec<String> {
    let ids = |v: Option<&Value>, key: Option<&str>| -> Vec<String> {
        v.and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|x| match key {
                        Some(k) => x.get(k).and_then(Value::as_str),
                        None => x.as_str(),
                    })
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let from_error = ids(
        c.error
            .as_ref()
            .and_then(|e| e.get("details"))
            .and_then(|d| d.get("orphaned_ids")),
        None,
    );
    if !from_error.is_empty() {
        return from_error;
    }
    ids(
        c.progress.as_ref().and_then(|p| p.get("created")),
        Some("id"),
    )
}
