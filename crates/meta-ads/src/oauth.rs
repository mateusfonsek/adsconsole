use std::collections::HashMap;
use std::time::Duration;

use adsconsole_core::secrets::Secret;
use rand::Rng;
use rand::distr::Alphanumeric;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::endpoints::MetaEndpoints;
use crate::error::MetaError;
use crate::token::{exchange, http_client};

const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: u64 = 8192;
const SUCCESS_PAGE: &str = "Connection complete. You can close this tab.";
const FAILURE_PAGE: &str = "The connection was not completed. Go back to AdsConsole.";

/// Credentials of the BYO app and the Facebook Login for Business configuration.
#[derive(Clone)]
pub struct AppCreds {
    pub app_id: String,
    pub app_secret: Secret,
    pub config_id: String,
    /// https page that receives the login redirect and forwards it to the local port
    /// (carried in `state` as `<port>.<random>`). Required once the app is published:
    /// Meta only accepts `localhost` in development mode. `None`: redirect straight to
    /// localhost.
    pub relay_url: Option<String>,
}

fn random_string(len: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

async fn respond(mut stream: TcpStream, status: &str, text: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>AdsConsole</title></head><body><p>{text}</p></body></html>"
    );
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(msg.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Reads the first line of the request; returns the parameters if it is `GET /callback`.
async fn read_callback(stream: &mut TcpStream) -> Option<HashMap<String, String>> {
    let mut line = String::new();
    let mut reader = BufReader::new((&mut *stream).take(MAX_REQUEST_BYTES));
    tokio::time::timeout(CONNECTION_READ_TIMEOUT, reader.read_line(&mut line))
        .await
        .ok()?
        .ok()?;
    let target = line.split_whitespace().nth(1)?;
    let url = url::Url::parse(&format!("http://localhost{target}")).ok()?;
    (url.path() == "/callback").then(|| url.query_pairs().into_owned().collect())
}

async fn wait_for_callback(
    listener: &TcpListener,
    state: &str,
) -> Result<(TcpStream, HashMap<String, String>), MetaError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| MetaError::Transport(e.to_string()))?;
        match read_callback(&mut stream).await {
            Some(params) if params.get("state").map(String::as_str) == Some(state) => {
                return Ok((stream, params));
            }
            // Wrong state: another local page must not cancel the login; keep waiting.
            Some(_) => respond(stream, "400 Bad Request", "Invalid request.").await,
            // Favicon and other paths: keep waiting.
            None => respond(stream, "404 Not Found", "Not found.").await,
        }
    }
}

/// Logs in through the system browser with a loopback on `localhost` (`port = 0` picks a
/// free port), directly or through the redirect page at `relay_url`. Exchanges the `code`
/// for the short-lived token and that one for the long-lived token (~60 days).
pub async fn login(
    endpoints: &MetaEndpoints,
    app: &AppCreds,
    port: u16,
    open: &(dyn Fn(&str) -> std::io::Result<()> + Send + Sync),
    wait: Duration,
) -> Result<Secret, MetaError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| MetaError::Transport(format!("could not open the local port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| MetaError::Transport(e.to_string()))?
        .port();
    let (redirect_uri, state) = match &app.relay_url {
        Some(relay) => {
            let ok = url::Url::parse(relay).is_ok_and(|u| u.scheme() == "https");
            if !ok {
                return Err(MetaError::Auth(format!(
                    "the redirect page must be https: {relay}"
                )));
            }
            (relay.clone(), format!("{port}.{}", random_string(32)))
        }
        None => (
            format!("http://localhost:{port}/callback"),
            random_string(32),
        ),
    };
    let dialog = url::Url::parse_with_params(
        &endpoints.dialog_url,
        &[
            ("client_id", app.app_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("config_id", app.config_id.as_str()),
            ("response_type", "code"),
            ("state", state.as_str()),
        ],
    )
    .map_err(|e| MetaError::Auth(format!("invalid login URL: {e}")))?;
    open(dialog.as_str())
        .map_err(|e| MetaError::Auth(format!("could not open the browser: {e}")))?;

    let (stream, params) = tokio::time::timeout(wait, wait_for_callback(&listener, &state))
        .await
        .map_err(|_| MetaError::Timeout)??;

    if let Some(error) = params.get("error") {
        respond(stream, "200 OK", FAILURE_PAGE).await;
        let reason = params.get("error_reason").unwrap_or(error);
        return Err(MetaError::Auth(format!("access denied: {reason}")));
    }
    let Some(code) = params.get("code") else {
        respond(stream, "400 Bad Request", FAILURE_PAGE).await;
        return Err(MetaError::Auth("callback without code".into()));
    };

    let result = async {
        let client = http_client()?;
        let short = exchange(
            &client,
            endpoints,
            &[
                ("client_id", app.app_id.as_str()),
                ("client_secret", app.app_secret.expose()),
                ("redirect_uri", redirect_uri.as_str()),
                ("code", code.as_str()),
            ],
        )
        .await
        .map_err(|e| match e {
            MetaError::ReauthRequired => MetaError::Auth("login code invalid or expired".into()),
            other => other,
        })?;
        exchange(
            &client,
            endpoints,
            &[
                ("grant_type", "fb_exchange_token"),
                ("client_id", app.app_id.as_str()),
                ("client_secret", app.app_secret.expose()),
                ("fb_exchange_token", short.expose()),
            ],
        )
        .await
    }
    .await;

    match &result {
        Ok(_) => respond(stream, "200 OK", SUCCESS_PAGE).await,
        Err(_) => respond(stream, "200 OK", FAILURE_PAGE).await,
    }
    result
}
