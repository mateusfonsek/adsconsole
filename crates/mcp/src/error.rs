use adsconsole_core::routing::RoutingError;
use rmcp::model::CallToolResult;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Códigos de erro estáveis devolvidos ao cliente MCP.
pub mod codes {
    pub const UNKNOWN_TOOL: &str = "UNKNOWN_TOOL";
    pub const TOOL_DISABLED: &str = "TOOL_DISABLED";
    pub const INVALID_INPUT: &str = "INVALID_INPUT";
    pub const READ_ONLY_MODE: &str = "READ_ONLY_MODE";
    pub const ACCOUNT_NOT_FOUND: &str = "ACCOUNT_NOT_FOUND";
    pub const ACCOUNT_NOT_IN_CONNECTION: &str = "ACCOUNT_NOT_IN_CONNECTION";
    pub const UNKNOWN_CONNECTION: &str = "UNKNOWN_CONNECTION";
    pub const AMBIGUOUS_ACCOUNT: &str = "AMBIGUOUS_ACCOUNT";
    pub const CONNECTION_NEEDS_RECONNECT: &str = "CONNECTION_NEEDS_RECONNECT";
    pub const ACCOUNT_NOT_ACTIVE: &str = "ACCOUNT_NOT_ACTIVE";
    pub const CHANGE_NOT_FOUND: &str = "CHANGE_NOT_FOUND";
    pub const NOT_REVERSIBLE: &str = "NOT_REVERSIBLE";
    pub const UNDO_CONFLICT: &str = "UNDO_CONFLICT";
    pub const INVALID_STATE: &str = "INVALID_STATE";
    pub const APPROVAL_REJECTED: &str = "APPROVAL_REJECTED";
    pub const APPROVAL_EXPIRED: &str = "APPROVAL_EXPIRED";
    pub const EXECUTION_FAILED: &str = "EXECUTION_FAILED";
    pub const INTERNAL: &str = "INTERNAL";
    pub const BUDGET_TOO_LOW: &str = "BUDGET_TOO_LOW";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const ACCESS_DENIED: &str = "ACCESS_DENIED";
    pub const ACCESS_LEVEL_INSUFFICIENT: &str = "ACCESS_LEVEL_INSUFFICIENT";
    pub const BIDDING_REQUIRES_CONVERSIONS: &str = "BIDDING_REQUIRES_CONVERSIONS";
    pub const QUOTA_EXHAUSTED: &str = "QUOTA_EXHAUSTED";
    pub const AD_POLICY: &str = "AD_POLICY";
    pub const API_ERROR: &str = "API_ERROR";
    pub const DISPLAY_NOT_AVAILABLE: &str = "DISPLAY_NOT_AVAILABLE";
    pub const PARTIAL_ROLLBACK: &str = "PARTIAL_ROLLBACK";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const BUDGET_CHANGE_LIMIT: &str = "BUDGET_CHANGE_LIMIT";
}

/// Erro de ferramenta: volta ao cliente como resultado com `isError: true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub field: Option<String>,
    pub suggestion: Option<String>,
    pub details: Option<Value>,
    pub retryable: bool,
}

impl ToolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            field: None,
            suggestion: None,
            details: None,
            retryable: false,
        }
    }

    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn to_call_result(&self) -> CallToolResult {
        CallToolResult::structured_error(json!({ "error": self }))
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<RoutingError> for ToolError {
    fn from(e: RoutingError) -> Self {
        let message = e.to_string();
        match e {
            RoutingError::UnknownConnection(_) => {
                ToolError::new(codes::UNKNOWN_CONNECTION, message)
                    .with_field("connection")
                    .with_suggestion("Use list_accounts para ver as conexões e contas disponíveis.")
            }
            RoutingError::AccountNotInConnection { .. } => {
                ToolError::new(codes::ACCOUNT_NOT_IN_CONNECTION, message)
                    .with_field("connection")
                    .with_suggestion("Use list_accounts para ver em qual conexão a conta está.")
            }
            RoutingError::AccountNotFound(_) => ToolError::new(codes::ACCOUNT_NOT_FOUND, message)
                .with_suggestion("Use list_accounts para ver as contas disponíveis"),
            RoutingError::Ambiguous { connections, .. } => {
                let ids: Vec<&str> = connections.iter().map(|(id, _)| id.as_str()).collect();
                let suggestion =
                    format!("Informe connection com um destes ids: {}.", ids.join(", "));
                let list: Vec<Value> = connections
                    .iter()
                    .map(|(id, name)| json!({ "connection_id": id, "name": name }))
                    .collect();
                ToolError::new(codes::AMBIGUOUS_ACCOUNT, message)
                    .with_field("connection")
                    .with_details(json!({ "connections": list }))
                    .with_suggestion(suggestion)
            }
            RoutingError::NeedsReconnect(_) => {
                ToolError::new(codes::CONNECTION_NEEDS_RECONNECT, message)
                    .with_suggestion("Reconecte esta conexão no AdsConsole → Conexões.")
            }
            RoutingError::Db(_) => ToolError::new(codes::INTERNAL, message),
        }
    }
}
