use std::sync::Arc;

use adsconsole_core::Platform;
use adsconsole_core::config::ToolGroup;
use adsconsole_core::db::{AccountInfo, InverseOp};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolError;

const IRREVERSIBLE_WARNING: &str = " Atenção: esta mudança não pode ser desfeita.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Write,
}

/// Descrição de uma ferramenta exposta (ou oculta) por uma plataforma.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub group: ToolGroup,
    pub kind: ToolKind,
    pub input_schema: Arc<JsonObject>,
    pub output_schema: Option<Arc<JsonObject>>,
    pub destructive: bool,
    pub idempotent: bool,
    pub reversible: bool,
    pub takes_account: bool,
    pub exposed: bool,
}

impl ToolSpec {
    fn base<I: JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
        group: ToolGroup,
        kind: ToolKind,
        reversible: bool,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            group,
            kind,
            input_schema: schema_for::<I>(),
            output_schema: None,
            destructive: false,
            idempotent: false,
            reversible,
            takes_account: true,
            exposed: true,
        }
    }

    pub fn read<I: JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
        group: ToolGroup,
    ) -> Self {
        Self::base::<I>(name, description, group, ToolKind::Read, false)
    }

    pub fn write<I: JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
        group: ToolGroup,
        reversible: bool,
    ) -> Self {
        Self::base::<I>(name, description, group, ToolKind::Write, reversible)
    }

    /// Declara o schema da saída estruturada (`structuredContent`).
    pub fn output<O: JsonSchema>(mut self) -> Self {
        self.output_schema = Some(schema_for::<O>());
        self
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    pub fn idempotent(mut self) -> Self {
        self.idempotent = true;
        self
    }

    pub fn no_account(mut self) -> Self {
        self.takes_account = false;
        self
    }

    /// Ferramenta interna (ex.: inversa de outra): nunca listada nem chamável direto.
    pub fn hidden(mut self) -> Self {
        self.exposed = false;
        self
    }

    pub fn to_mcp_tool(&self) -> Tool {
        let mut description = self.description.clone();
        if self.kind == ToolKind::Write && !self.reversible {
            description.push_str(IRREVERSIBLE_WARNING);
        }
        let annotations = ToolAnnotations::new()
            .read_only(self.kind == ToolKind::Read)
            .destructive(self.destructive)
            .idempotent(self.idempotent);
        let mut tool = Tool::new(self.name.clone(), description, self.input_schema.clone())
            .with_annotations(annotations);
        if let Some(out) = &self.output_schema {
            tool = tool.with_raw_output_schema(out.clone());
        }
        tool
    }
}

/// Gera o JSON Schema de `T` como objeto. O schemars 1.x já põe os campos de
/// `#[serde(flatten)]` (ex.: [`AccountRef`]) em `properties`/`required` do topo.
pub fn schema_for<T: JsonSchema>() -> Arc<JsonObject> {
    let schema = schemars::schema_for!(T);
    let value = serde_json::to_value(schema).unwrap_or(Value::Null);
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

// Conta (e conexão opcional) alvo de uma ferramenta. Entra nas entradas com
// `#[serde(flatten)]`. Comentário comum, não doc: doc viraria a descrição do
// schema de toda ferramenta que o inclui.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AccountRef {
    /// Id da conta de anúncios.
    pub account_id: String,
    /// Id da conexão a usar quando a conta existe em mais de uma.
    #[serde(default)]
    pub connection: Option<String>,
}

/// Chamada já roteada. `input` ainda contém `account_id`/`connection`.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub connection_id: String,
    pub account_id: String,
    pub tool: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub summary: String,
    pub warning: Option<String>,
}

/// Resultado de uma escrita aplicada. `inverse == None` significa "não reversível".
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub after: Value,
    pub inverse: Option<InverseOp>,
}

/// Recebe o andamento de uma escrita em vários passos, para que uma queda no meio
/// não deixe objetos criados sem registro.
pub trait Progress: Send + Sync {
    /// Lista completa do que já foi criado até agora (`[{"key", "id", "kind"}]`),
    /// não só o último.
    fn created(&self, ids: &Value);
}

/// Contrato que cada plataforma (Google Ads, Meta Ads) implementa.
///
/// Em ferramenta com `takes_account = false`, o `Call` chega com `account_id = ""`
/// e `connection_id` igual ao `connection` informado, ou `""` (todas).
///
/// Invariantes que o Engine assume:
/// - `current_state(target)` devolve o mesmo formato e as mesmas chaves de
///   [`Applied::after`]. A detecção de conflito do desfazer compara só as chaves de
///   `after`; chave ausente no estado atual vale `null` e conta como conflito.
/// - [`InverseOp::input`] não contém `account_id` nem `connection`: o desfazer
///   injeta os da mudança original.
/// - `execute` é tudo ou nada do ponto de vista do Engine: `Err` vira `Failed`, com
///   `reversible = false` e sem `after` gravado. Adapter com escrita em vários passos
///   (Meta) desfaz internamente os passos já feitos; se esse rollback falhar, informa
///   os ids dos objetos órfãos em `ToolError.details`, na chave `"orphaned_ids"`.
/// - `precheck` pode rodar duas vezes por escrita: antes de entrar na fila e de novo
///   sob o mutex da conta, na execução.
#[async_trait::async_trait]
pub trait PlatformAdapter: Send + Sync + 'static {
    fn platform(&self) -> Platform;
    /// Inclui as ferramentas ocultas (inversas).
    fn tools(&self) -> Vec<ToolSpec>;
    async fn list_accounts(&self, connection_id: &str) -> Result<Vec<AccountInfo>, ToolError>;
    async fn read(&self, call: &Call) -> Result<Value, ToolError>;
    /// Checagens da conta antes de escrever (ex.: account_status na Meta).
    async fn precheck(&self, call: &Call) -> Result<(), ToolError>;
    /// Regras locais + validate_only da API.
    async fn validate(&self, call: &Call) -> Result<Validated, ToolError>;
    /// Estado "antes", só com os campos afetados.
    async fn snapshot(&self, call: &Call) -> Result<Value, ToolError>;
    async fn execute(&self, call: &Call, before: &Value) -> Result<Applied, ToolError>;
    /// O que o Engine chama: `execute` com o registro de progresso. Adapter com escrita
    /// em vários passos (Meta) sobrescreve e chama `progress.created` a cada criação.
    async fn execute_tracked(
        &self,
        call: &Call,
        before: &Value,
        progress: &dyn Progress,
    ) -> Result<Applied, ToolError> {
        let _ = progress;
        self.execute(call, before).await
    }
    async fn current_state(
        &self,
        connection_id: &str,
        account_id: &str,
        target: &Value,
    ) -> Result<Value, ToolError>;
    /// Ids das conexões ativas desta plataforma.
    fn connections(&self) -> Vec<String>;
}
