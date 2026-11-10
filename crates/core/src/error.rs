use crate::platform::Platform;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Invalid value: {0}")]
    InvalidAmount(String),
    #[error("Status \"{value}\" is not valid for {}", platform.display_name())]
    InvalidStatus { platform: Platform, value: String },
    #[error("{0}")]
    Config(String),
    #[error("Secrets error: {0}")]
    Secret(String),
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
