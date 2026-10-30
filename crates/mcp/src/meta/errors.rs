//! Tradução dos erros da Meta para [`ToolError`].

use adsconsole_core::db::{ConnectionStatus, Db};
use adsconsole_meta_ads::error::MetaError;
use serde_json::json;

use super::ctx::Ctx;
use crate::error::{ToolError, codes};

const RECONNECT_HINT: &str = "Reconecte a conexão em AdsConsole → Conexões.";

/// Marca a conexão como `needs_reconnect` (quando há banco) e devolve o erro.
fn needs_reconnect(scope: Option<(&Db, &str)>) -> ToolError {
    if let Some((db, connection_id)) = scope
        && let Err(e) = db.set_connection_status(connection_id, ConnectionStatus::NeedsReconnect)
    {
        tracing::warn!(connection_id, error = %e, "falha ao marcar a conexão para reconectar");
    }
    ToolError::new(
        codes::CONNECTION_NEEDS_RECONNECT,
        "O acesso desta conexão à Meta venceu ou foi revogado.",
    )
    .with_suggestion(RECONNECT_HINT)
}

pub fn translate(err: MetaError, ctx: Option<&Ctx>, tool: &str) -> ToolError {
    translate_in(
        err,
        ctx.map(|c| (c.db.as_ref(), c.connection_id.as_str())),
        tool,
    )
}

pub(crate) fn translate_in(err: MetaError, scope: Option<(&Db, &str)>, tool: &str) -> ToolError {
    match err {
        MetaError::ReauthRequired => needs_reconnect(scope),
        MetaError::RateLimited { account, wait } => {
            let mut e = ToolError::new(
                codes::RATE_LIMITED,
                format!(
                    "A conta {account} chegou ao limite de chamadas da Meta; tente de novo em {} s.",
                    wait.as_secs()
                ),
            )
            .with_details(json!({ "retry_after_secs": wait.as_secs() }))
            .with_suggestion("Espere o tempo indicado antes de repetir.");
            e.retryable = true;
            e
        }
        MetaError::MaybeApplied { cause } => maybe_applied(tool, &cause),
        ref api @ MetaError::Api {
            code,
            subcode,
            ref message,
            ref user_title,
            ref user_msg,
            ref fbtrace_id,
            ref blame_fields,
            maybe_applied: maybe,
            ..
        } => {
            if code == 190 {
                return needs_reconnect(scope);
            }
            let shown = user_msg.clone().unwrap_or_else(|| message.clone());
            if maybe {
                return self::maybe_applied(tool, message);
            }
            if api.is_rate_limit() {
                let secs = api.retry_after().map(|d| d.as_secs());
                let mut e = ToolError::new(
                    codes::RATE_LIMITED,
                    "A Meta recusou a chamada por limite de uso; tente de novo em alguns minutos.",
                )
                .with_details(
                    json!({ "retry_after_secs": secs, "code": code, "subcode": subcode }),
                );
                e.retryable = true;
                return e;
            }
            if subcode == Some(4841021) {
                return ToolError::new(
                    codes::ACCOUNT_NOT_ACTIVE,
                    "A Meta não deixa escrever nesta conta pelo status dela.",
                )
                .with_details(json!({ "code": code, "subcode": subcode }));
            }
            if code == 10 || (200..=299).contains(&code) {
                return ToolError::new(
                    codes::ACCESS_DENIED,
                    format!("A Meta negou o acesso: {shown}"),
                )
                .with_suggestion(
                    "Confira se o token tem as 6 permissões (ads_read, ads_management, \
                         business_management, pages_show_list, pages_read_engagement, \
                         pages_manage_ads) e se a pessoa tem função na conta de anúncios.",
                )
                .with_details(json!({ "code": code, "subcode": subcode }));
            }
            if subcode == Some(1885183) {
                return ToolError::new(
                    codes::API_ERROR,
                    "A Meta não deixa anunciar um criativo novo criado por um app em modo de \
                     desenvolvimento.",
                )
                .with_suggestion(
                    "Use um post que já existe na Página (criativo existing_post; veja \
                     get_page_posts) ou publique o app (modo Live) no Meta for Developers.",
                )
                .with_details(json!({ "code": code, "subcode": subcode }));
            }
            if code == 368 || subcode == Some(1346003) {
                return ToolError::new(codes::AD_POLICY, shown).with_details(
                    json!({ "code": code, "subcode": subcode, "title": user_title }),
                );
            }
            if code == 100
                && let Some(field) = blame_fields.first()
            {
                return ToolError::new(codes::INVALID_INPUT, shown)
                    .with_field(field.clone())
                    .with_details(
                        json!({ "code": code, "subcode": subcode, "fbtrace_id": fbtrace_id }),
                    );
            }
            let text = match user_title {
                Some(t) if user_msg.is_some() => format!("{t}: {shown}"),
                _ => shown,
            };
            ToolError::new(codes::API_ERROR, text)
                .with_details(json!({ "code": code, "subcode": subcode, "fbtrace_id": fbtrace_id }))
        }
        MetaError::Timeout => {
            let mut e = ToolError::new(codes::API_ERROR, "A Meta demorou demais para responder.");
            e.retryable = true;
            e
        }
        MetaError::Transport(m) => {
            let mut e = ToolError::new(
                codes::API_ERROR,
                format!("Falha de rede ao falar com a Meta: {m}"),
            );
            e.retryable = true;
            e
        }
        MetaError::Auth(m) => {
            ToolError::new(codes::API_ERROR, format!("Falha no login da Meta: {m}"))
        }
        MetaError::Decode(m) => ToolError::new(
            codes::API_ERROR,
            format!("Resposta inesperada da Meta: {m}"),
        ),
        MetaError::File(m) => ToolError::new(codes::INVALID_INPUT, m).with_field("file_path"),
    }
}

fn maybe_applied(tool: &str, cause: &str) -> ToolError {
    ToolError::new(
        codes::EXECUTION_FAILED,
        format!("A Meta não confirmou o resultado de {tool} ({cause})."),
    )
    .with_suggestion("A Meta não confirmou a mudança; confira na conta antes de repetir.")
}
