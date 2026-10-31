//! Erro único que a UI recebe: código estável, mensagem pronta e campo opcional.

use adsconsole_core::CoreError;
use adsconsole_core::secrets::SecretStoreError;
use adsconsole_mcp::ToolError;
use adsconsole_mcp::http::StartError;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub field: Option<String>,
    #[specta(type = Option<specta_typescript::Unknown>)]
    pub details: Option<Value>,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        AppError {
            code: code.into(),
            message: message.into(),
            field: None,
            details: None,
        }
    }

    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    pub fn internal(e: impl std::fmt::Display) -> Self {
        AppError::new("INTERNAL", format!("Erro interno: {e}"))
    }
}

impl From<ToolError> for AppError {
    fn from(e: ToolError) -> Self {
        AppError {
            code: e.code,
            message: e.message,
            field: e.field,
            details: e.details,
        }
    }
}

impl From<CoreError> for AppError {
    fn from(e: CoreError) -> Self {
        AppError::internal(e)
    }
}

impl From<StartError> for AppError {
    fn from(e: StartError) -> Self {
        let code = match e {
            StartError::PortInUse(_) => "PORT_IN_USE",
            StartError::InvalidPort(_) => "INVALID_PORT",
            _ => "INTERNAL",
        };
        AppError::new(code, e.to_string())
    }
}

impl From<SecretStoreError> for AppError {
    fn from(e: SecretStoreError) -> Self {
        AppError::new("KEYCHAIN_UNAVAILABLE", e.to_string())
    }
}
