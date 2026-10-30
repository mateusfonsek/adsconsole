//! Ponto único de despacho das ferramentas: roteamento de conta, leitura e escrita.

mod approval;
mod history;
mod locks;
mod undo;
mod write;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings};
use adsconsole_core::db::{ChangeFilter, ChangeRecord, ChangeStatus, Db};
use adsconsole_core::routing::{RoutingError, normalize_account_id, resolve_connection};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::adapter::{AccountRef, Call, PlatformAdapter, ToolKind, ToolSpec};
use crate::error::{ToolError, codes};
use crate::registry::ToolRegistry;
use locks::AccountLocks;

pub use approval::PENDING_TTL;

pub type SharedConfig = Arc<RwLock<Config>>;

/// Intervalo mínimo entre dois refresh de contas disparados por `ACCESS_DENIED`
/// (no Google, cada refresh gasta operações da cota do projeto).
const ACCESS_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Um refresh por `ACCESS_DENIED` por vez, e nenhum a menos de
/// [`ACCESS_REFRESH_INTERVAL`] do início do anterior.
#[derive(Default)]
struct RefreshGate {
    running: AtomicBool,
    last_start: Mutex<Option<Instant>>,
}

/// Libera o [`RefreshGate`] ao terminar (inclusive em pânico).
struct RefreshPermit(Arc<RefreshGate>);

impl Drop for RefreshPermit {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
    }
}

impl RefreshGate {
    fn try_start(self: &Arc<Self>) -> Option<RefreshPermit> {
        let mut last = self.last_start.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < ACCESS_REFRESH_INTERVAL) {
            return None;
        }
        if self.running.swap(true, Ordering::AcqRel) {
            return None;
        }
        *last = Some(Instant::now());
        Some(RefreshPermit(self.clone()))
    }
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    ApprovalRequested {
        change_id: String,
        platform: Platform,
        summary: String,
        warning: Option<String>,
    },
    ChangeApplied {
        change_id: String,
    },
    ChangeFailed {
        change_id: String,
    },
    ChangeRejected {
        change_id: String,
    },
    ChangeExpired {
        change_id: String,
    },
}

#[derive(Clone)]
pub struct Engine {
    adapter: Arc<dyn PlatformAdapter>,
    db: Arc<Db>,
    config: SharedConfig,
    registry: Arc<ToolRegistry>,
    events: broadcast::Sender<EngineEvent>,
    locks: Arc<AccountLocks>,
    access_refresh: Arc<RefreshGate>,
}

/// Erro interno (banco, serialização) convertido para o cliente.
pub(crate) fn internal(e: impl std::fmt::Display) -> ToolError {
    ToolError::new(codes::INTERNAL, format!("Erro interno: {e}"))
}

impl Engine {
    pub fn new(
        adapter: Arc<dyn PlatformAdapter>,
        db: Arc<Db>,
        config: SharedConfig,
    ) -> Result<Engine, ToolError> {
        let mut specs = adapter.tools();
        specs.extend(history::history_tools());
        let registry = ToolRegistry::new(specs).map_err(internal)?;
        let (events, _) = broadcast::channel(256);
        let engine = Engine {
            adapter,
            db,
            config,
            registry: Arc::new(registry),
            events,
            locks: Arc::new(AccountLocks::default()),
            access_refresh: Arc::new(RefreshGate::default()),
        };
        engine.fail_interrupted()?;
        Ok(engine)
    }

    /// Mudanças desta plataforma que ficaram em `Approved` (execução interrompida por
    /// queda ou erro do banco depois da aprovação) viram `Failed`: não dá para saber
    /// se chegaram à plataforma.
    /// Se a escrita já tinha criado objetos (progresso gravado), eles vão no erro, em
    /// `details.orphaned_ids`.
    fn fail_interrupted(&self) -> Result<(), ToolError> {
        let filter = ChangeFilter {
            platform: Some(self.platform()),
            status: Some(ChangeStatus::Approved),
            limit: 100,
            ..ChangeFilter::default()
        };
        loop {
            let batch = self.db.list_changes(&filter).map_err(internal)?;
            if batch.is_empty() {
                return Ok(());
            }
            for mut change in batch {
                change.status = ChangeStatus::Failed;
                change.reversible = false;
                change.error =
                    Some(serde_json::to_value(interrupted_error(&change)).map_err(internal)?);
                self.db.update_change(&change).map_err(internal)?;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn adapter_for_tests(&self) -> Arc<dyn PlatformAdapter> {
        self.adapter.clone()
    }

    pub fn platform(&self) -> Platform {
        self.adapter.platform()
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    /// Configuração atual do servidor desta plataforma (cópia; nada fica travado).
    pub(crate) fn settings(&self) -> ServerSettings {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .server(self.platform())
    }

    /// Enviar sem receptor não é erro.
    pub(crate) fn emit(&self, event: EngineEvent) {
        let _ = self.events.send(event);
    }

    pub async fn call_tool(
        &self,
        name: &str,
        input: Value,
        requested_by: Option<String>,
    ) -> Result<Value, ToolError> {
        let spec = self
            .registry
            .get(name)
            .filter(|s| s.exposed)
            .ok_or_else(|| {
                ToolError::new(
                    codes::UNKNOWN_TOOL,
                    format!("Ferramenta desconhecida: {name}"),
                )
            })?;
        if !self.registry.is_enabled(name, &self.settings()) {
            return Err(ToolError::new(
                codes::TOOL_DISABLED,
                format!("A ferramenta {name} está desligada."),
            )
            .with_suggestion("Ligue em AdsConsole → Ferramentas e escrita"));
        }
        // Ruling C: as de histórico são do engine (sem conta); o undo_change passa
        // pela checagem de só leitura dentro de submit_write.
        if history::is_history_tool(name) {
            return self.call_history(name, input, requested_by).await;
        }
        if spec.kind == ToolKind::Write {
            // Antes do roteamento: em só leitura, nem o refresh de contas chama o adapter.
            self.ensure_writable()?;
        }
        let call = self.route(spec, input).await?;
        let result = match spec.kind {
            ToolKind::Read => self.adapter.read(&call).await,
            ToolKind::Write => self.submit_write(call, spec, requested_by, None).await,
        };
        // Acesso negado costuma ser a conta que saiu da conexão: atualiza o cache em
        // segundo plano, sem esperar e sem mexer no erro devolvido. Limitado pelo
        // RefreshGate, porque erros em sequência pediriam um refresh cada.
        if matches!(&result, Err(e) if e.code == codes::ACCESS_DENIED)
            && let Some(permit) = self.access_refresh.try_start()
        {
            let engine = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = engine.refresh_accounts().await;
            });
        }
        result
    }

    /// Normaliza o `account_id` e escolhe a conexão. Ferramentas sem conta vão com ids vazios.
    async fn route(&self, spec: &ToolSpec, mut input: Value) -> Result<Call, ToolError> {
        if !spec.takes_account {
            return Ok(Call {
                connection_id: String::new(),
                account_id: String::new(),
                tool: spec.name.clone(),
                input,
            });
        }
        let account: AccountRef = serde_json::from_value(input.clone()).map_err(|e| {
            ToolError::new(
                codes::INVALID_INPUT,
                format!("account_id ausente ou inválido: {e}"),
            )
            .with_field("account_id")
        })?;
        let platform = self.platform();
        let account_id = normalize_account_id(platform, &account.account_id);
        if let Some(obj) = input.as_object_mut() {
            obj.insert("account_id".into(), Value::String(account_id.clone()));
        }
        let explicit = account.connection.as_deref();
        let connection_id = match resolve_connection(&self.db, platform, &account_id, explicit) {
            Err(RoutingError::AccountNotFound(_) | RoutingError::AccountNotInConnection { .. }) => {
                // Falha no refresh não importa aqui: o erro de roteamento real é o que vale.
                let _ = self.refresh_accounts().await;
                resolve_connection(&self.db, platform, &account_id, explicit)?
            }
            other => other?,
        };
        Ok(Call {
            connection_id,
            account_id,
            tool: spec.name.clone(),
            input,
        })
    }

    /// Atualiza o cache de contas de cada conexão. Uma conexão que falha é pulada
    /// (mantém o cache antigo); só devolve erro se todas falharem.
    pub async fn refresh_accounts(&self) -> Result<usize, ToolError> {
        let connections = self.adapter.connections();
        refresh_connections(
            self.adapter.as_ref(),
            &self.db,
            self.platform(),
            &connections,
        )
        .await
    }
}

/// Atualiza o cache de contas de cada conexão. Uma conexão que falha é pulada
/// (mantém o cache antigo); só devolve erro se todas falharem. Devolve o total de
/// contas gravadas.
pub(crate) async fn refresh_connections(
    adapter: &dyn PlatformAdapter,
    db: &Db,
    platform: Platform,
    connections: &[String],
) -> Result<usize, ToolError> {
    let mut total = 0;
    let mut failures = 0;
    let mut last_err = None;
    for connection_id in connections {
        let result = match adapter.list_accounts(connection_id).await {
            Ok(accounts) => db
                .replace_accounts(platform, connection_id, &accounts)
                .map(|()| accounts.len())
                .map_err(internal),
            Err(e) => Err(e),
        };
        match result {
            Ok(n) => total += n,
            Err(e) => {
                tracing::warn!(
                    platform = platform.as_str(),
                    connection_id = connection_id.as_str(),
                    error = %e,
                    "falha ao atualizar contas da conexão"
                );
                failures += 1;
                last_err = Some(e);
            }
        }
    }
    match last_err {
        Some(e) if failures == connections.len() => Err(e),
        _ => Ok(total),
    }
}

/// Erro de uma mudança interrompida, com os ids que o progresso registrou.
fn interrupted_error(change: &ChangeRecord) -> ToolError {
    let orphans: Vec<String> = change
        .progress
        .as_ref()
        .and_then(|p| p.get("created"))
        .and_then(|c| c.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|c| c.get("id").and_then(|id| id.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if orphans.is_empty() {
        return ToolError::new(
            codes::EXECUTION_FAILED,
            "Execução interrompida; confira o estado na plataforma antes de repetir.",
        );
    }
    ToolError::new(
        codes::EXECUTION_FAILED,
        format!(
            "O AdsConsole foi fechado no meio desta mudança. Estes objetos podem ter ficado na conta: {}.",
            orphans.join(", ")
        ),
    )
    .with_details(serde_json::json!({ "orphaned_ids": orphans }))
    .with_suggestion("Confira esses objetos na plataforma e apague os que não quiser.")
}
