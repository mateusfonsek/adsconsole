use adsconsole_core::secrets::Secret;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ClientJsonError {
    #[error(
        "Este JSON é de um client do tipo Aplicativo da Web. Crie um client do tipo App para computador."
    )]
    WebClient,
    #[error(
        "Este JSON não parece ser de um client OAuth do Google (faltou a chave \"installed\")."
    )]
    NotOAuthClient,
    #[error("O JSON do client está sem o campo \"{0}\".")]
    Missing(&'static str),
    #[error("JSON do client inválido: {0}")]
    Invalid(String),
}

/// Credenciais de um client OAuth do tipo App para computador (chave `installed`).
#[derive(Debug, Clone)]
pub struct ClientCreds {
    pub client_id: String,
    pub client_secret: Secret,
    pub project_id: String,
}

impl ClientCreds {
    pub fn from_json(bytes: &[u8]) -> Result<ClientCreds, ClientJsonError> {
        let v: Value =
            serde_json::from_slice(bytes).map_err(|e| ClientJsonError::Invalid(e.to_string()))?;
        if v.get("web").is_some() {
            return Err(ClientJsonError::WebClient);
        }
        let inst = v.get("installed").ok_or(ClientJsonError::NotOAuthClient)?;
        let get = |key: &'static str| {
            inst.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or(ClientJsonError::Missing(key))
        };
        Ok(ClientCreds {
            client_id: get("client_id")?,
            client_secret: Secret::new(get("client_secret")?),
            project_id: get("project_id")?,
        })
    }
}
