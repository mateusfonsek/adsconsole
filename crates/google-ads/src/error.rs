use serde_json::Value;

/// Um erro individual do `GoogleAdsFailure`.
#[derive(Debug, Clone, PartialEq)]
pub struct AdsError {
    /// Chave do `errorCode`, p.ex. `campaignBudgetError`.
    pub category: String,
    /// Valor do `errorCode`, p.ex. `BUDGET_BELOW_PER_DAY_MINIMUM`.
    pub code: String,
    /// Mensagem original do Google.
    pub message: String,
    /// Ex.: `operations[0].create.amount_micros`.
    pub field_path: Option<String>,
    /// Índice da primeira operação citada no caminho.
    pub operation_index: Option<usize>,
    pub details: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum GoogleError {
    #[error("Falha de autenticação no Google: {0}")]
    Auth(String),
    #[error("A autorização do Google expirou ou foi revogada. Reconecte a conta.")]
    ReauthRequired,
    #[error("Falha de comunicação com o Google: {0}")]
    Transport(String),
    #[error("O Google demorou demais para responder.")]
    Timeout,
    #[error("Resposta inesperada do Google: {0}")]
    Decode(String),
    /// Arquivo local a enviar (upload de vídeo): sumiu, mudou ou não pôde ser lido.
    #[error("Arquivo: {0}")]
    File(String),
    #[error(
        "Não foi possível confirmar se a mudança foi aplicada; confira na conta antes de repetir. ({cause})"
    )]
    MaybeApplied { cause: String },
    #[error("{}", api_display(*.http_status, status, message, errors, *.maybe_applied))]
    Api {
        http_status: u16,
        status: String,
        /// Mensagem original do Google (em inglês).
        message: String,
        errors: Vec<AdsError>,
        request_id: Option<String>,
        /// Um 5xx num mutate pode ter sido aplicado mesmo assim.
        maybe_applied: bool,
    },
}

fn api_display(
    http_status: u16,
    status: &str,
    message: &str,
    errors: &[AdsError],
    maybe_applied: bool,
) -> String {
    let mut out = format!("O Google Ads recusou a requisição (HTTP {http_status}");
    if !status.is_empty() {
        out.push_str(&format!(", {status}"));
    }
    out.push_str(&format!("): {message}"));
    for e in errors {
        out.push_str(&format!("\n- {}: {}", e.code, e.message));
        if let Some(p) = &e.field_path {
            out.push_str(&format!(" (em {p})"));
        }
    }
    if maybe_applied {
        out.push_str(
            "\nA operação pode ter sido aplicada apesar do erro. Confira na conta do Google Ads antes de tentar de novo.",
        );
    }
    out
}

impl GoogleError {
    /// Erro de cota/limite de taxa, repetível mesmo em escrita.
    pub(crate) fn is_quota(&self) -> bool {
        matches!(self, GoogleError::Api { http_status, status, errors, .. }
            if *http_status == 429
                || is_exhausted(status)
                || errors.iter().any(|e| is_exhausted(&e.code)))
    }
}

fn is_exhausted(s: &str) -> bool {
    s == "RESOURCE_EXHAUSTED" || s == "RESOURCE_TEMPORARILY_EXHAUSTED"
}

fn field_path(elements: &[Value]) -> (Option<String>, Option<usize>) {
    let mut parts = Vec::new();
    let mut op_index = None;
    for el in elements {
        let Some(name) = el.get("fieldName").and_then(Value::as_str) else {
            continue;
        };
        match el.get("index").and_then(Value::as_u64) {
            Some(i) => {
                op_index.get_or_insert(i as usize);
                parts.push(format!("{name}[{i}]"));
            }
            None => parts.push(name.to_string()),
        }
    }
    if parts.is_empty() {
        (None, op_index)
    } else {
        (Some(parts.join(".")), op_index)
    }
}

fn parse_one(e: &Value) -> AdsError {
    let (category, code) = e
        .get("errorCode")
        .and_then(Value::as_object)
        .and_then(|o| o.iter().next())
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()),
            )
        })
        .unwrap_or_default();
    let (field_path, operation_index) = e
        .pointer("/location/fieldPathElements")
        .and_then(Value::as_array)
        .map(|a| field_path(a))
        .unwrap_or((None, None));
    AdsError {
        category,
        code,
        message: e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        field_path,
        operation_index,
        details: e.get("details").cloned(),
    }
}

/// Monta `GoogleError::Api` a partir de uma resposta HTTP de erro.
pub(crate) fn parse_api_error(
    http_status: u16,
    body: &[u8],
    header_request_id: Option<String>,
    maybe_applied: bool,
) -> GoogleError {
    let json: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let err = json.get("error");
    let text = |k: &str| {
        err.and_then(|e| e.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let mut message = text("message").unwrap_or_default();
    if message.is_empty() {
        message = format!("HTTP {http_status}");
    }
    let mut errors = Vec::new();
    let mut request_id = None;
    let details = err.and_then(|e| e.get("details")).and_then(Value::as_array);
    for d in details.into_iter().flatten() {
        let is_failure = d
            .get("@type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.ends_with("GoogleAdsFailure"));
        if !is_failure {
            continue;
        }
        if request_id.is_none() {
            request_id = d
                .get("requestId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if let Some(list) = d.get("errors").and_then(Value::as_array) {
            errors.extend(list.iter().map(parse_one));
        }
    }
    GoogleError::Api {
        http_status,
        status: text("status").unwrap_or_default(),
        message,
        errors,
        request_id: request_id.or(header_request_id),
        maybe_applied,
    }
}
