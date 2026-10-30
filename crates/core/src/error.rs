use crate::platform::Platform;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Valor inválido: {0}")]
    InvalidAmount(String),
    #[error("Status \"{value}\" não é válido para {}", platform.display_name())]
    InvalidStatus { platform: Platform, value: String },
    #[error("{0}")]
    Config(String),
    #[error("Erro nos segredos: {0}")]
    Secret(String),
    #[error("Erro no banco de dados: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("Não encontrado: {0}")]
    NotFound(String),
    #[error("Erro de E/S: {0}")]
    Io(#[from] std::io::Error),
}
