use std::sync::Arc;

use adsconsole_core::Platform;
use adsconsole_core::config::ToolGroup;
use adsconsole_core::db::{AccountInfo, InverseOp};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolError;

const IRREVERSIBLE_WARNING: &str = " Warning: this change cannot be undone.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Write,
}

/// Description of a tool exposed (or hidden) by a platform.
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

    /// Declares the schema of the structured output (`structuredContent`).
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

    /// Internal tool (e.g. the inverse of another): never listed and never directly callable.
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

/// Generates the JSON Schema of `T` as an object. schemars 1.x already puts the
/// fields of `#[serde(flatten)]` (e.g. [`AccountRef`]) in the top-level
/// `properties`/`required`.
pub fn schema_for<T: JsonSchema>() -> Arc<JsonObject> {
    let schema = schemars::schema_for!(T);
    let value = serde_json::to_value(schema).unwrap_or(Value::Null);
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

// Target account (and optional connection) of a tool. Included in the inputs with
// `#[serde(flatten)]`. Plain comment, not a doc comment: a doc comment would become
// the schema description of every tool that includes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AccountRef {
    /// Ad account id.
    pub account_id: String,
    /// Id of the connection to use when the account exists in more than one.
    #[serde(default)]
    pub connection: Option<String>,
}

/// Already-routed call. `input` still contains `account_id`/`connection`.
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

/// Result of an applied write. `inverse == None` means "not reversible".
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub after: Value,
    pub inverse: Option<InverseOp>,
}

/// Receives the progress of a multi-step write, so that a crash in the middle
/// does not leave created objects unrecorded.
pub trait Progress: Send + Sync {
    /// Complete list of what has been created so far (`[{"key", "id", "kind"}]`),
    /// not just the last one.
    fn created(&self, ids: &Value);
}

/// Contract that each platform (Google Ads, Meta Ads) implements.
///
/// For a tool with `takes_account = false`, the `Call` arrives with `account_id = ""`
/// and `connection_id` equal to the given `connection`, or `""` (all).
///
/// Invariants the Engine assumes:
/// - `current_state(target)` returns the same shape and the same keys as
///   [`Applied::after`]. The undo conflict detection compares only the keys of
///   `after`; a key missing from the current state counts as `null` and as a conflict.
/// - [`InverseOp::input`] contains neither `account_id` nor `connection`: undo
///   injects those of the original change.
/// - `execute` is all-or-nothing from the Engine's point of view: `Err` becomes `Failed`,
///   with `reversible = false` and no `after` recorded. An adapter with a multi-step write
///   (Meta) internally rolls back the steps already done; if that rollback fails, it reports
///   the ids of the orphaned objects in `ToolError.details`, under the key `"orphaned_ids"`.
/// - `precheck` may run twice per write: before entering the queue and again
///   under the account mutex, at execution.
#[async_trait::async_trait]
pub trait PlatformAdapter: Send + Sync + 'static {
    fn platform(&self) -> Platform;
    /// Includes the hidden tools (inverses).
    fn tools(&self) -> Vec<ToolSpec>;
    async fn list_accounts(&self, connection_id: &str) -> Result<Vec<AccountInfo>, ToolError>;
    async fn read(&self, call: &Call) -> Result<Value, ToolError>;
    /// Account checks before writing (e.g. account_status on Meta).
    async fn precheck(&self, call: &Call) -> Result<(), ToolError>;
    /// Local rules + the API validate_only.
    async fn validate(&self, call: &Call) -> Result<Validated, ToolError>;
    /// The "before" state, with only the affected fields.
    async fn snapshot(&self, call: &Call) -> Result<Value, ToolError>;
    async fn execute(&self, call: &Call, before: &Value) -> Result<Applied, ToolError>;
    /// What the Engine calls: `execute` with progress tracking. An adapter with a
    /// multi-step write (Meta) overrides it and calls `progress.created` on each creation.
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
    /// Ids of the active connections of this platform.
    fn connections(&self) -> Vec<String>;
}
