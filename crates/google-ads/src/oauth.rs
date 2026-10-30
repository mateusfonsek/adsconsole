use std::collections::HashMap;
use std::time::Duration;

use adsconsole_core::secrets::Secret;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use rand::distr::Alphanumeric;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::client_json::ClientCreds;
use crate::endpoints::GoogleEndpoints;
use crate::error::GoogleError;
use crate::token::{http_client, post_token};

const SCOPE: &str = "https://www.googleapis.com/auth/adwords";
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: u64 = 8192;
const FAILURE_PAGE: &str = "A conexão não foi autorizada. Pode fechar esta aba.";

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

/// Lê a primeira linha do request e devolve os parâmetros da query, ou `None` se não for um callback.
async fn read_callback(stream: &mut TcpStream) -> Option<HashMap<String, String>> {
    let mut line = String::new();
    let mut reader = BufReader::new((&mut *stream).take(MAX_REQUEST_BYTES));
    tokio::time::timeout(CONNECTION_READ_TIMEOUT, reader.read_line(&mut line))
        .await
        .ok()?
        .ok()?;
    let target = line.split_whitespace().nth(1)?;
    let url = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    (params.contains_key("code") || params.contains_key("error")).then_some(params)
}

async fn wait_for_callback(
    listener: &TcpListener,
    state: &str,
) -> Result<(TcpStream, HashMap<String, String>), GoogleError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| GoogleError::Transport(e.to_string()))?;
        match read_callback(&mut stream).await {
            Some(params) if params.get("state").map(String::as_str) == Some(state) => {
                return Ok((stream, params));
            }
            // State errado: outra página local não pode cancelar o login; segue esperando.
            Some(_) => respond(stream, "400 Bad Request", "Requisição inválida.").await,
            // Favicon, pré-conexão do navegador etc.: ignora e segue esperando.
            None => respond(stream, "404 Not Found", "Não encontrado.").await,
        }
    }
}

/// Fluxo OAuth de app para computador: loopback em 127.0.0.1 com PKCE S256.
/// Devolve o refresh token.
pub async fn authorize(
    endpoints: &GoogleEndpoints,
    creds: &ClientCreds,
    open: &(dyn Fn(&str) -> std::io::Result<()> + Send + Sync),
    wait: Duration,
) -> Result<Secret, GoogleError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
        GoogleError::Transport(format!("não foi possível abrir a porta local: {e}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| GoogleError::Transport(e.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_string(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string(32);

    let auth_url = url::Url::parse_with_params(
        &endpoints.auth_url,
        &[
            ("client_id", creds.client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", state.as_str()),
        ],
    )
    .map_err(|e| GoogleError::Auth(format!("URL de autorização inválida: {e}")))?;

    open(auth_url.as_str())
        .map_err(|e| GoogleError::Auth(format!("não foi possível abrir o navegador: {e}")))?;

    let (stream, params) = tokio::time::timeout(wait, wait_for_callback(&listener, &state))
        .await
        .map_err(|_| GoogleError::Timeout)??;

    if let Some(error) = params.get("error") {
        respond(stream, "200 OK", FAILURE_PAGE).await;
        return Err(GoogleError::Auth(format!("acesso negado: {error}")));
    }
    let Some(code) = params.get("code") else {
        respond(stream, "400 Bad Request", FAILURE_PAGE).await;
        return Err(GoogleError::Auth("callback sem code".into()));
    };

    let client = http_client()?;
    let exchanged = post_token(
        &client,
        &endpoints.token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.expose()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
        ],
    )
    .await
    .map_err(|e| match e {
        GoogleError::ReauthRequired => {
            GoogleError::Auth("código de autorização inválido ou expirado".into())
        }
        other => other,
    })
    .and_then(|resp| {
        resp.refresh_token.filter(|t| !t.is_empty()).ok_or_else(|| {
            GoogleError::Auth(
                "o Google não devolveu refresh token. Remova o acesso do app em myaccount.google.com (Segurança > Conexões de terceiros) e tente de novo.".into(),
            )
        })
    });

    match &exchanged {
        Ok(_) => respond(stream, "200 OK", "Conexão concluída. Pode fechar esta aba.").await,
        Err(_) => respond(stream, "200 OK", FAILURE_PAGE).await,
    }
    exchanged
}
