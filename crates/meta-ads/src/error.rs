use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

/// Rate limit codes: Meta refuses without applying.
const RATE_LIMIT_CODES: [i64; 4] = [4, 17, 32, 613];
const RATE_LIMIT_SUBCODES: [i64; 4] = [1487742, 5044001, 1487632, 1487225];

/// Errors of the Meta library.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum MetaError {
    #[error("Meta authentication failed: {0}")]
    Auth(String),
    /// Token invalid, expired or revoked: the user must log in again.
    #[error("The Meta token expired or was revoked. Reconnect the account.")]
    ReauthRequired,
    #[error("Failed to communicate with Meta: {0}")]
    Transport(String),
    #[error("Meta took too long to respond.")]
    Timeout,
    #[error("Unexpected response from Meta: {0}")]
    Decode(String),
    /// Invalid media file or URL.
    #[error("File: {0}")]
    File(String),
    /// A write with no response (timeout or network drop): it may have been applied.
    #[error("Meta did not confirm the write: {cause}")]
    MaybeApplied { cause: String },
    /// The local rate limiter refused: wait `wait` before calling this account again.
    #[error("Rate limit for account {account}; try again in {} s", wait.as_secs())]
    RateLimited { account: String, wait: Duration },
    #[error("Meta error {code}: {message}")]
    Api {
        http_status: u16,
        code: i64,
        subcode: Option<i64>,
        message: String,
        user_title: Option<String>,
        user_msg: Option<String>,
        is_transient: bool,
        fbtrace_id: Option<String>,
        /// `error_data.blame_field_specs`, with the paths joined by `.`.
        blame_fields: Vec<String>,
        /// A write that got a 5xx: it may have been applied.
        maybe_applied: bool,
        /// `estimated_time_to_regain_access` (or `reset_time_duration`) from the response.
        retry_after: Option<Duration>,
    },
}

/// Text of a reqwest error without the URL: it carries `appsecret_proof` and, in login
/// and `debug_token`, tokens and the app secret.
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
    /// Reads `{"error": {...}}` from a Graph response.
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

    /// Rate limit (codes 4, 17, 32, 613, 80000–80014 and the rate limit subcodes).
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

    /// How long to wait before retrying, when Meta provides it.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            MetaError::Api { retry_after, .. } => *retry_after,
            MetaError::RateLimited { wait, .. } => Some(*wait),
            _ => None,
        }
    }
}
