use std::time::{Duration, Instant};

use adsconsole_core::secrets::Secret;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::client_json::ClientCreds;
use crate::endpoints::GoogleEndpoints;
use crate::error::GoogleError;

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: Option<Secret>,
    pub refresh_token: Option<Secret>,
    #[serde(default)]
    pub expires_in: u64,
}

#[derive(Deserialize)]
struct TokenErrorBody {
    error: Option<String>,
    error_description: Option<String>,
}

pub(crate) fn http_client() -> Result<reqwest::Client, GoogleError> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| GoogleError::Transport(e.without_url().to_string()))
}

pub(crate) fn map_reqwest(e: reqwest::Error) -> GoogleError {
    if e.is_timeout() {
        GoogleError::Timeout
    } else {
        GoogleError::Transport(e.without_url().to_string())
    }
}

/// Form POST to `token_url`. `invalid_grant` becomes `ReauthRequired`.
pub(crate) async fn post_token(
    client: &reqwest::Client,
    token_url: &str,
    params: &[(&str, &str)],
) -> Result<TokenResponse, GoogleError> {
    let resp = client
        .post(token_url)
        .form(params)
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(map_reqwest)?;
    if !status.is_success() {
        let body: TokenErrorBody = serde_json::from_slice(&bytes).unwrap_or(TokenErrorBody {
            error: None,
            error_description: None,
        });
        return Err(match body.error.as_deref() {
            Some("invalid_grant") => GoogleError::ReauthRequired,
            Some(code) => GoogleError::Auth(match body.error_description {
                Some(d) => format!("{code}: {d}"),
                None => code.to_string(),
            }),
            None => GoogleError::Auth(format!("the token server responded {status}")),
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| GoogleError::Decode("malformed token response".into()))
}

/// Caches the access token and renews it (one refresh at a time) when less than 60 s remain.
pub struct TokenManager {
    endpoints: GoogleEndpoints,
    creds: ClientCreds,
    refresh: Secret,
    http: reqwest::Client,
    cache: Mutex<Option<(Secret, Instant)>>,
}

impl TokenManager {
    pub fn new(endpoints: GoogleEndpoints, creds: ClientCreds, refresh: Secret) -> Self {
        // Fails only if the TLS backend cannot initialize; no call would work without it.
        let http = http_client().expect("HTTP client");
        Self {
            http,
            endpoints,
            creds,
            refresh,
            cache: Mutex::new(None),
        }
    }

    pub async fn access_token(&self) -> Result<Secret, GoogleError> {
        let mut cache = self.cache.lock().await;
        if let Some((token, valid_until)) = cache.as_ref()
            && Instant::now() < *valid_until
        {
            return Ok(token.clone());
        }
        let resp = post_token(
            &self.http,
            &self.endpoints.token_url,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh.expose()),
                ("client_id", &self.creds.client_id),
                ("client_secret", self.creds.client_secret.expose()),
            ],
        )
        .await?;
        let token = resp
            .access_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| GoogleError::Decode("malformed token response".into()))?;
        let lifetime = Duration::from_secs(resp.expires_in).saturating_sub(EXPIRY_MARGIN);
        *cache = Some((token.clone(), Instant::now() + lifetime));
        Ok(token)
    }
}
