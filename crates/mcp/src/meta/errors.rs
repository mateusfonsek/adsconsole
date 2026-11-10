//! Translation of Meta errors into [`ToolError`].

use adsconsole_core::db::{ConnectionStatus, Db};
use adsconsole_meta_ads::error::MetaError;
use serde_json::json;

use super::ctx::Ctx;
use crate::error::{ToolError, codes};

const RECONNECT_HINT: &str = "Reconnect this connection in AdsConsole → Connections.";

/// Marks the connection as `needs_reconnect` (when there is a database) and returns the error.
fn needs_reconnect(scope: Option<(&Db, &str)>) -> ToolError {
    if let Some((db, connection_id)) = scope
        && let Err(e) = db.set_connection_status(connection_id, ConnectionStatus::NeedsReconnect)
    {
        tracing::warn!(connection_id, error = %e, "failed to mark the connection for reconnect");
    }
    ToolError::new(
        codes::CONNECTION_NEEDS_RECONNECT,
        "This connection's Meta access expired or was revoked.",
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
                    "Account {account} reached the Meta call limit; try again in {} s.",
                    wait.as_secs()
                ),
            )
            .with_details(json!({ "retry_after_secs": wait.as_secs() }))
            .with_suggestion("Wait the indicated time before retrying.");
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
                    "Meta refused the call because of a usage limit; try again in a few minutes.",
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
                    "Meta does not allow writes to this account because of its status.",
                )
                .with_details(json!({ "code": code, "subcode": subcode }));
            }
            if code == 10 || (200..=299).contains(&code) {
                return ToolError::new(
                    codes::ACCESS_DENIED,
                    format!("Meta denied access: {shown}"),
                )
                .with_suggestion(
                    "Check that the token has the 6 permissions (ads_read, ads_management, \
                         business_management, pages_show_list, pages_read_engagement, \
                         pages_manage_ads) and that the person has a role on the ad account.",
                )
                .with_details(json!({ "code": code, "subcode": subcode }));
            }
            if subcode == Some(1885183) {
                return ToolError::new(
                    codes::API_ERROR,
                    "Meta does not allow running a new creative created by an app in development \
                     mode.",
                )
                .with_suggestion(
                    "Use a post that already exists on the Page (existing_post creative; see \
                     get_page_posts) or publish the app (Live mode) in Meta for Developers.",
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
            let mut e = ToolError::new(codes::API_ERROR, "Meta took too long to respond.");
            e.retryable = true;
            e
        }
        MetaError::Transport(m) => {
            let mut e = ToolError::new(
                codes::API_ERROR,
                format!("Failed to communicate with Meta: {m}"),
            );
            e.retryable = true;
            e
        }
        MetaError::Auth(m) => {
            ToolError::new(codes::API_ERROR, format!("Meta authentication failed: {m}"))
        }
        MetaError::Decode(m) => ToolError::new(
            codes::API_ERROR,
            format!("Unexpected response from Meta: {m}"),
        ),
        MetaError::File(m) => ToolError::new(codes::INVALID_INPUT, m).with_field("file_path"),
    }
}

fn maybe_applied(tool: &str, cause: &str) -> ToolError {
    ToolError::new(
        codes::EXECUTION_FAILED,
        format!("Meta did not confirm the result of {tool} ({cause})."),
    )
    .with_suggestion("Meta did not confirm the change; check the account before retrying.")
}
