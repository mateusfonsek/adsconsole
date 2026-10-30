//! Tradução de [`GoogleError`] para [`ToolError`].

use adsconsole_core::db::{ConnectionStatus, Db};
use adsconsole_core::money;
use adsconsole_google_ads::endpoints::console_links;
use adsconsole_google_ads::error::{AdsError, GoogleError};
use serde_json::{Value, json};

use super::ctx::Ctx;
use crate::error::{ToolError, codes};

const RECONNECT_SUGGESTION: &str = "Reconecte esta conexão no AdsConsole → Conexões.";
const MAYBE_APPLIED_SUGGESTION: &str = "O Google respondeu com erro interno; confira na conta se a mudança foi aplicada antes de repetir.";

/// De onde veio o erro: a conexão (para marcar `NeedsReconnect` e montar links do
/// Console) e a moeda da conta, quando há uma.
pub(crate) struct Scope<'a> {
    pub db: &'a Db,
    pub connection_id: &'a str,
    pub project_id: &'a str,
    pub currency: Option<&'a str>,
}

/// Traduz um erro do Google. Com `ctx`, um `ReauthRequired` marca a conexão como
/// `NeedsReconnect`.
pub fn translate(err: GoogleError, ctx: Option<&Ctx>, tool: &str) -> ToolError {
    let scope = ctx.map(|c| Scope {
        db: &c.db,
        connection_id: &c.connection_id,
        project_id: &c.project_id,
        currency: Some(&c.currency),
    });
    translate_in(err, scope.as_ref(), tool)
}

pub(crate) fn translate_in(err: GoogleError, scope: Option<&Scope>, tool: &str) -> ToolError {
    let out = match err {
        GoogleError::ReauthRequired => {
            if let Some(s) = scope {
                mark_needs_reconnect(s.db, s.connection_id);
            }
            ToolError::new(
                codes::CONNECTION_NEEDS_RECONNECT,
                "A autorização do Google desta conexão expirou ou foi revogada.",
            )
            .with_suggestion(RECONNECT_SUGGESTION)
        }
        GoogleError::MaybeApplied { .. } => {
            ToolError::new(codes::EXECUTION_FAILED, err.to_string())
                .with_suggestion(MAYBE_APPLIED_SUGGESTION)
        }
        GoogleError::Api {
            http_status,
            ref status,
            ref message,
            ref errors,
            ref request_id,
            maybe_applied,
        } => api_error(
            http_status,
            status,
            message,
            errors,
            request_id.as_deref(),
            maybe_applied,
            scope,
        ),
        GoogleError::Auth(_) => {
            let mut e = ToolError::new(codes::API_ERROR, err.to_string());
            if let Some(s) = scope {
                e = e.with_suggestion(format!(
                    "Confira o client OAuth desta conexão no Console: {}",
                    console_links(s.project_id).clients
                ));
            }
            e
        }
        GoogleError::Transport(_) | GoogleError::Timeout => {
            let mut e = ToolError::new(codes::API_ERROR, err.to_string());
            e.retryable = true;
            e
        }
        GoogleError::Decode(_) => ToolError::new(codes::API_ERROR, err.to_string()),
        GoogleError::File(_) => {
            ToolError::new(codes::INVALID_INPUT, err.to_string()).with_field("file_path")
        }
    };
    tracing::warn!(tool, code = out.code.as_str(), "erro do Google Ads");
    out
}

fn mark_needs_reconnect(db: &Db, connection_id: &str) {
    if let Err(e) = db.set_connection_status(connection_id, ConnectionStatus::NeedsReconnect) {
        tracing::warn!(connection_id, error = %e, "falha ao marcar a conexão para reconectar");
    }
}

fn is_exhausted(s: &str) -> bool {
    s == "RESOURCE_EXHAUSTED" || s == "RESOURCE_TEMPORARILY_EXHAUSTED"
}

fn errors_json(errors: &[AdsError]) -> Value {
    Value::Array(
        errors
            .iter()
            .map(|e| {
                json!({
                    "category": e.category,
                    "code": e.code,
                    "message": e.message,
                    "field_path": e.field_path,
                    "operation_index": e.operation_index,
                    "details": e.details,
                })
            })
            .collect(),
    )
}

/// Mínimo diário em unidades da moeda, a partir dos detalhes do erro.
fn budget_minimum(e: &AdsError) -> Option<String> {
    let d = e.details.as_ref()?.get("budgetPerDayMinimumErrorDetails")?;
    let raw = d
        .get("budgetPerDayMinimumMicros")
        .or_else(|| d.get("minimumBudgetAmountMicros"))?;
    let micros = match raw {
        Value::String(s) => s.parse::<i64>().ok()?,
        other => other.as_i64()?,
    };
    Some(money::from_micros(micros).normalize().to_string())
}

/// Tópicos de política citados pelos erros de política.
fn policy_topics(errors: &[&AdsError]) -> Vec<String> {
    let mut topics = Vec::new();
    for e in errors {
        let Some(d) = &e.details else { continue };
        if let Some(entries) = d
            .pointer("/policyFindingDetails/policyTopicEntries")
            .and_then(Value::as_array)
        {
            topics.extend(
                entries
                    .iter()
                    .filter_map(|t| t.get("topic").and_then(Value::as_str))
                    .map(str::to_string),
            );
        }
        if let Some(v) = d.get("policyViolationDetails") {
            let name = v
                .get("externalPolicyName")
                .or_else(|| v.pointer("/key/policyName"))
                .and_then(Value::as_str);
            topics.extend(name.map(str::to_string));
        }
    }
    let mut seen = std::collections::HashSet::new();
    topics.retain(|t| seen.insert(t.clone()));
    topics
}

fn api_error(
    http_status: u16,
    status: &str,
    message: &str,
    errors: &[AdsError],
    request_id: Option<&str>,
    maybe_applied: bool,
    scope: Option<&Scope>,
) -> ToolError {
    let details = json!({ "errors": errors_json(errors), "request_id": request_id });
    let find = |codes: &[&str]| errors.iter().find(|e| codes.contains(&e.code.as_str()));

    if maybe_applied {
        return ToolError::new(
            codes::EXECUTION_FAILED,
            format!("O Google respondeu com erro interno (HTTP {http_status}): {message}"),
        )
        .with_suggestion(MAYBE_APPLIED_SUGGESTION)
        .with_details(details);
    }
    if find(&["CLOUD_PROJECT_NOT_APPROVED_FOR_PRODUCTION"]).is_some() {
        let mut e = ToolError::new(
            codes::ACCESS_LEVEL_INSUFFICIENT,
            "O projeto do Google Cloud desta conexão está no nível Test e não acessa contas reais.",
        )
        .with_details(details);
        if let Some(s) = scope {
            e = e.with_suggestion(format!(
                "Peça o nível Explorer da Google Ads API em {}",
                console_links(s.project_id).api
            ));
        }
        return e;
    }
    if let Some(e) = find(&[
        "USER_PERMISSION_DENIED",
        "CUSTOMER_NOT_ENABLED",
        "NOT_ADS_USER",
    ]) {
        let (msg, suggestion) = match e.code.as_str() {
            "USER_PERMISSION_DENIED" => (
                "O usuário desta conexão não tem permissão nesta conta do Google Ads.",
                "Se a conta é filha de uma MCC, confira o login_customer_id da conexão; \
                 use list_accounts com refresh: true para atualizar as contas.",
            ),
            "CUSTOMER_NOT_ENABLED" => (
                "A conta do Google Ads não está ativa (cancelada ou ainda não configurada).",
                "Use list_accounts para ver as contas ativas.",
            ),
            _ => (
                "A conta Google usada no login desta conexão não tem acesso ao Google Ads.",
                "Reconecte com uma conta Google que tenha acesso no AdsConsole → Conexões.",
            ),
        };
        return ToolError::new(codes::ACCESS_DENIED, msg)
            .with_suggestion(suggestion)
            .with_details(details);
    }
    if let Some(e) = find(&["BUDGET_BELOW_PER_DAY_MINIMUM"]) {
        let msg = match budget_minimum(e) {
            Some(min) => match scope.and_then(|s| s.currency) {
                Some(cur) => format!("O orçamento diário mínimo nesta conta é {min} {cur}."),
                None => format!("O orçamento diário mínimo nesta conta é {min}."),
            },
            None => "O orçamento diário está abaixo do mínimo desta conta.".to_string(),
        };
        return ToolError::new(codes::BUDGET_TOO_LOW, msg)
            .with_field("daily_budget")
            .with_details(details);
    }
    if find(&["CONVERSION_TRACKING_NOT_ENABLED"]).is_some() {
        return ToolError::new(
            codes::BIDDING_REQUIRES_CONVERSIONS,
            "Esta estratégia de lance exige o acompanhamento de conversões ativo na conta.",
        )
        .with_suggestion("Configure o acompanhamento de conversões ou use bidding maximize_clicks.")
        .with_details(details);
    }
    if http_status == 429 || is_exhausted(status) || errors.iter().any(|e| is_exhausted(&e.code)) {
        let mut e = ToolError::new(
            codes::QUOTA_EXHAUSTED,
            "O Google Ads limitou as requisições desta conexão (cota esgotada).",
        )
        .with_suggestion("Espere alguns minutos e tente de novo.")
        .with_details(details);
        e.retryable = true;
        return e;
    }
    let policy: Vec<&AdsError> = errors
        .iter()
        .filter(|e| e.category == "policyFindingError" || e.category == "policyViolationError")
        .collect();
    if !policy.is_empty() {
        let topics = policy_topics(&policy);
        let msg = if topics.is_empty() {
            "O Google Ads recusou o conteúdo por violar políticas de anúncios.".to_string()
        } else {
            format!(
                "O Google Ads recusou o conteúdo por violar políticas de anúncios: {}.",
                topics.join(", ")
            )
        };
        return ToolError::new(codes::AD_POLICY, msg).with_details(json!({
            "topics": topics,
            "errors": errors_json(errors),
            "request_id": request_id,
        }));
    }
    if find(&["OPERATION_NOT_PERMITTED_FOR_REMOVED_RESOURCE"]).is_some() {
        return ToolError::new(
            codes::INVALID_STATE,
            "O objeto já está removido no Google Ads, e objetos removidos não podem ser alterados nem reativados.",
        )
        .with_details(details);
    }
    let first = errors
        .first()
        .map(|e| e.message.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or(message);
    let mut e = ToolError::new(codes::API_ERROR, first).with_details(details);
    e.retryable = http_status >= 500;
    e
}
