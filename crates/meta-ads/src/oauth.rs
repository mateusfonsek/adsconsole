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
const SUCCESS_PAGE: &str = "Conexão concluída. Pode fechar esta aba.";
const FAILURE_PAGE: &str = "A conexão não foi concluída. Volte ao AdsConsole.";

/// Credenciais do app BYO e a configuração do Login do Facebook para Empresas.
#[derive(Clone)]
pub struct AppCreds {
    pub app_id: String,
    pub app_secret: Secret,
    pub config_id: String,
    /// Página https que recebe o retorno do login e o repassa à porta local (que vai no
    /// `state` como `<porta>.<aleatório>`). Necessária com o app publicado: a Meta só
    /// aceita `localhost` no modo de desenvolvimento. `None`: retorno direto ao localhost.
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

/// Lê a primeira linha do request; devolve os parâmetros se for `GET /callback`.
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
            // State errado: outra página local não pode cancelar o login; segue esperando.
            Some(_) => respond(stream, "400 Bad Request", "Requisição inválida.").await,
            // Favicon e outros caminhos: segue esperando.
            None => respond(stream, "404 Not Found", "Não encontrado.").await,
        }
    }
}

/// Login no navegador do sistema com loopback em `localhost` (`port = 0` escolhe uma
/// porta livre), direto ou pela página de retorno de `relay_url`. Troca o `code` pelo token curto e este pelo de longa duração (~60 dias).
pub async fn login(
    endpoints: &MetaEndpoints,
    app: &AppCreds,
    port: u16,
    open: &(dyn Fn(&str) -> std::io::Result<()> + Send + Sync),
    wait: Duration,
) -> Result<Secret, MetaError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| MetaError::Transport(format!("não foi possível abrir a porta local: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| MetaError::Transport(e.to_string()))?
        .port();
    let (redirect_uri, state) = match &app.relay_url {
        Some(relay) => {
            let ok = url::Url::parse(relay).is_ok_and(|u| u.scheme() == "https");
            if !ok {
                return Err(MetaError::Auth(format!(
                    "a página de retorno precisa ser https: {relay}"
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
    .map_err(|e| MetaError::Auth(format!("URL de login inválida: {e}")))?;
    open(dialog.as_str())
        .map_err(|e| MetaError::Auth(format!("não foi possível abrir o navegador: {e}")))?;

    let (stream, params) = tokio::time::timeout(wait, wait_for_callback(&listener, &state))
        .await
        .map_err(|_| MetaError::Timeout)??;

    if let Some(error) = params.get("error") {
        respond(stream, "200 OK", FAILURE_PAGE).await;
        let reason = params.get("error_reason").unwrap_or(error);
        return Err(MetaError::Auth(format!("acesso negado: {reason}")));
    }
    let Some(code) = params.get("code") else {
        respond(stream, "400 Bad Request", FAILURE_PAGE).await;
        return Err(MetaError::Auth("callback sem code".into()));
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
            MetaError::ReauthRequired => {
                MetaError::Auth("código de login inválido ou expirado".into())
            }
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
