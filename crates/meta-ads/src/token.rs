use std::time::Duration;

use adsconsole_core::secrets::Secret;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::endpoints::MetaEndpoints;
use crate::error::{MetaError, net_text};

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Permissions every connection must have.
pub const REQUIRED_SCOPES: [&str; 6] = [
    "ads_read",
    "ads_management",
    "business_management",
    "pages_show_list",
    "pages_read_engagement",
    "pages_manage_ads",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    User,
    SystemUser,
    Page,
    Other(String),
}

impl TokenKind {
    fn parse(s: &str) -> TokenKind {
        match s {
            "USER" => TokenKind::User,
            "SYSTEM_USER" => TokenKind::SystemUser,
            "PAGE" => TokenKind::Page,
            other => TokenKind::Other(other.to_string()),
        }
    }
}

/// What `debug_token` says about a token.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenInfo {
    pub kind: TokenKind,
    pub is_valid: bool,
    /// `None`: does not expire.
    pub expires_at: Option<DateTime<Utc>>,
    pub data_access_expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
    pub user_id: Option<String>,
}

pub(crate) fn http_client() -> Result<reqwest::Client, MetaError> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| MetaError::Transport(net_text(e)))
}

/// Error from a Graph response outside the main client (login and debug_token).
fn graph_error(body: &Value) -> MetaError {
    let err = &body["error"];
    if err["code"].as_i64() == Some(190) {
        return MetaError::ReauthRequired;
    }
    let msg = err["message"].as_str().unwrap_or("unknown error");
    MetaError::Auth(msg.to_string())
}

/// Sends and reads the JSON response. Network errors come without the URL (see [`net_text`]).
async fn send_json(req: reqwest::RequestBuilder) -> Result<Value, MetaError> {
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() {
            MetaError::Timeout
        } else {
            MetaError::Transport(net_text(e))
        }
    })?;
    let ok = resp.status().is_success();
    let body: Value = resp
        .json()
        .await
        .map_err(|_| MetaError::Decode("the response is not JSON".into()))?;
    if ok {
        Ok(body)
    } else {
        Err(graph_error(&body))
    }
}

/// `POST /oauth/access_token` with the parameters in the body (the app secret and the
/// codes stay out of the URL); returns the `access_token`.
pub(crate) async fn exchange(
    client: &reqwest::Client,
    endpoints: &MetaEndpoints,
    params: &[(&str, &str)],
) -> Result<Secret, MetaError> {
    let url = format!("{}/oauth/access_token", endpoints.graph_base);
    let body = send_json(client.post(url).form(params)).await?;
    body["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(Secret::new)
        .ok_or_else(|| MetaError::Decode("the token exchange did not return access_token".into()))
}

#[derive(Deserialize)]
struct DebugData {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    is_valid: bool,
    #[serde(default)]
    expires_at: i64,
    #[serde(default)]
    data_access_expires_at: i64,
    #[serde(default)]
    scopes: Vec<String>,
    user_id: Option<String>,
}

fn timestamp(secs: i64) -> Option<DateTime<Utc>> {
    (secs > 0)
        .then(|| DateTime::from_timestamp(secs, 0))
        .flatten()
}

/// `GET /debug_token`. With an app, authenticates with the app token (`app_id|app_secret`);
/// without one, with the token itself. Authentication goes in the header; only
/// `input_token` stays in the URL (Meta does not accept it elsewhere).
pub async fn debug_token(
    endpoints: &MetaEndpoints,
    token: &Secret,
    app: Option<(&str, &Secret)>,
) -> Result<TokenInfo, MetaError> {
    let auth = match app {
        Some((id, secret)) => format!("{id}|{}", secret.expose()),
        None => token.expose().to_string(),
    };
    let url = format!("{}/debug_token", endpoints.graph_base);
    let url = url::Url::parse_with_params(&url, &[("input_token", token.expose())])
        .map_err(|e| MetaError::Transport(format!("invalid URL: {e}")))?;
    let req = http_client()?.get(url).bearer_auth(auth);
    let body = send_json(req).await?;
    let data: DebugData = serde_json::from_value(body["data"].clone())
        .map_err(|e| MetaError::Decode(format!("debug_token: {e}")))?;
    Ok(TokenInfo {
        kind: TokenKind::parse(&data.kind),
        is_valid: data.is_valid,
        expires_at: timestamp(data.expires_at),
        data_access_expires_at: timestamp(data.data_access_expires_at),
        scopes: data.scopes,
        user_id: data.user_id,
    })
}

/// Permissions from [`REQUIRED_SCOPES`] that the token lacks, in list order.
pub fn missing_scopes(info: &TokenInfo) -> Vec<&'static str> {
    REQUIRED_SCOPES
        .into_iter()
        .filter(|s| !info.scopes.iter().any(|have| have == s))
        .collect()
}
