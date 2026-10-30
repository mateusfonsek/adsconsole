use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

/// Códigos de limite de chamadas: a Meta recusa sem aplicar.
const RATE_LIMIT_CODES: [i64; 4] = [4, 17, 32, 613];
const RATE_LIMIT_SUBCODES: [i64; 4] = [1487742, 5044001, 1487632, 1487225];

/// Erros da biblioteca da Meta.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum MetaError {
    #[error("falha de autenticação: {0}")]
    Auth(String),
    /// Token inválido, vencido ou revogado: é preciso logar de novo.
    #[error("o token da Meta não vale mais; é preciso reconectar")]
    ReauthRequired,
    #[error("falha de rede: {0}")]
    Transport(String),
    #[error("tempo esgotado")]
    Timeout,
    #[error("resposta inesperada da Meta: {0}")]
    Decode(String),
    /// Arquivo ou URL de mídia inválido.
    #[error("arquivo: {0}")]
    File(String),
    /// Uma escrita sem resposta (timeout ou queda de rede): pode ter sido aplicada.
    #[error("a Meta não confirmou a escrita: {cause}")]
    MaybeApplied { cause: String },
    /// O rate limiter local recusou: esperar `wait` antes de chamar de novo esta conta.
    #[error("limite de chamadas da conta {account}; tente de novo em {} s", wait.as_secs())]
    RateLimited { account: String, wait: Duration },
    #[error("erro da Meta {code}: {message}")]
    Api {
        http_status: u16,
        code: i64,
        subcode: Option<i64>,
        message: String,
        user_title: Option<String>,
        user_msg: Option<String>,
        is_transient: bool,
        fbtrace_id: Option<String>,
        /// `error_data.blame_field_specs`, com os caminhos juntados por `.`.
        blame_fields: Vec<String>,
        /// Escrita que recebeu 5xx: pode ter sido aplicada.
        maybe_applied: bool,
        /// `estimated_time_to_regain_access` (ou `reset_time_duration`) da resposta.
        retry_after: Option<Duration>,
    },
}

/// Texto de um erro do reqwest sem a URL: ela leva `appsecret_proof` e, no login e no
/// `debug_token`, tokens e o app secret.
pub(crate) fn net_text(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut out = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

impl MetaError {
    /// Lê `{"error": {...}}` de uma resposta do Graph.
    pub(crate) fn from_body(http_status: u16, body: &Value) -> MetaError {
        let e = &body["error"];
        let code = e["code"].as_i64().unwrap_or(0);
        if code == 190 {
            return MetaError::ReauthRequired;
        }
        let text = |k: &str| e[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        let blame_fields = e["error_data"]["blame_field_specs"]
            .as_array()
            .map(|specs| {
                specs
                    .iter()
                    .filter_map(|spec| match spec {
                        Value::Array(parts) => Some(
                            parts
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("."),
                        ),
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        MetaError::Api {
            http_status,
            code,
            subcode: e["error_subcode"].as_i64(),
            message: text("message").unwrap_or_else(|| format!("HTTP {http_status}")),
            user_title: text("error_user_title"),
            user_msg: text("error_user_msg"),
            is_transient: e["is_transient"].as_bool().unwrap_or(false),
            fbtrace_id: text("fbtrace_id"),
            blame_fields,
            maybe_applied: false,
            retry_after: None,
        }
    }

    /// Limite de chamadas (códigos 4, 17, 32, 613, 80000–80014 e os subcódigos de limite).
    pub fn is_rate_limit(&self) -> bool {
        match self {
            MetaError::Api { code, subcode, .. } => {
                RATE_LIMIT_CODES.contains(code)
                    || (80000..=80014).contains(code)
                    || subcode.is_some_and(|s| RATE_LIMIT_SUBCODES.contains(&s))
            }
            _ => false,
        }
    }

    /// Quanto esperar antes de tentar de novo, quando a Meta informa.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            MetaError::Api { retry_after, .. } => *retry_after,
            MetaError::RateLimited { wait, .. } => Some(*wait),
            _ => None,
        }
    }
}
