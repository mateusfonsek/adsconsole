//! Servidor HTTP local: um endpoint MCP por plataforma, atrás do middleware de acesso.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use adsconsole_core::Platform;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::{Engine, SharedConfig};
use crate::server::McpService;

/// Intervalo entre as varreduras de pendências vencidas.
const EXPIRE_EVERY: Duration = Duration::from_secs(60);

pub struct ServerOptions {
    /// 0 escolhe uma porta livre (usado nos testes).
    pub port: u16,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(
        "A porta {0} já está em uso. Feche o outro programa ou mude a porta em AdsConsole → MCPs (ou ADSCONSOLE_PORT)."
    )]
    PortInUse(u16),
    #[error(
        "O token do MCP está vazio. Defina um token em ADSCONSOLE_TOKEN ou deixe o AdsConsole gerar um."
    )]
    EmptyToken,
    #[error("Falha ao iniciar o servidor MCP: {0}")]
    Io(#[from] std::io::Error),
}

pub struct RunningServer {
    pub addr: SocketAddr,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningServer {
    /// Fecha as sessões MCP, para de aceitar conexões e espera o servidor terminar.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for task in std::mem::take(&mut self.tasks) {
            if let Err(e) = task.await {
                tracing::error!(error = %e, "tarefa do servidor MCP terminou com erro");
            }
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Guard {
    token: String,
    config: SharedConfig,
}

pub async fn start(
    engines: Vec<Engine>,
    config: SharedConfig,
    opts: ServerOptions,
) -> Result<RunningServer, StartError> {
    // Token vazio aceitaria `Authorization: Bearer ` de qualquer processo local.
    if opts.token.trim().is_empty() {
        return Err(StartError::EmptyToken);
    }
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, opts.port))
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse => StartError::PortInUse(opts.port),
            _ => StartError::Io(e),
        })?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();

    let mut router = Router::new();
    for engine in &engines {
        let e = engine.clone();
        let svc: StreamableHttpService<McpService, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(McpService { engine: e.clone() }),
                Default::default(),
                StreamableHttpServerConfig::default().with_cancellation_token(cancel.child_token()),
            );
        router = router.nest_service(engine.platform().endpoint(), svc);
    }
    let guard = Arc::new(Guard {
        token: opts.token,
        config,
    });
    let app = router.layer(middleware::from_fn_with_state(guard, check_access));

    let mut tasks = Vec::with_capacity(engines.len() + 1);
    let stop = cancel.clone();
    tasks.push(tokio::spawn(async move {
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await;
        if let Err(e) = served {
            tracing::error!(error = %e, "servidor MCP parou com erro");
        }
    }));
    for engine in engines {
        tasks.push(tokio::spawn(expire_loop(engine, cancel.clone())));
    }
    tracing::info!(%addr, "servidor MCP ouvindo");
    Ok(RunningServer {
        addr,
        cancel,
        tasks,
    })
}

/// Expira as pendências vencidas agora e a cada `EXPIRE_EVERY`, até o cancelamento.
async fn expire_loop(engine: Engine, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(EXPIRE_EVERY);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                if let Err(e) = engine.expire_stale(Utc::now()) {
                    tracing::warn!(platform = engine.platform().as_str(), error = %e, "falha ao expirar pendências");
                }
            }
        }
    }
}

/// Ordem: navegador (403) → token (401) → plataforma pausada (503).
async fn check_access(State(guard): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    if headers.contains_key(header::ORIGIN) {
        return reject(
            StatusCode::FORBIDDEN,
            "Requests de navegador não são aceitos.".into(),
        );
    }
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|t| ct_eq(t, guard.token.as_bytes()));
    if !authorized {
        let mut resp = reject(
            StatusCode::UNAUTHORIZED,
            "Token ausente ou inválido. Copie o token em AdsConsole → MCPs.".into(),
        );
        resp.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return resp;
    }
    if let Some(p) = platform_of(req.uri().path()) {
        let enabled = guard
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .server(p)
            .enabled;
        if !enabled {
            return reject(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("O MCP de {} está pausado no AdsConsole.", p.display_name()),
            );
        }
    }
    next.run(req).await
}

fn platform_of(path: &str) -> Option<Platform> {
    Platform::ALL.into_iter().find(|p| {
        path.strip_prefix(p.endpoint())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

fn reject(status: StatusCode, message: String) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

/// Comparação em tempo constante para o mesmo tamanho (o tamanho em si não é segredo).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_compares_bytes() {
        assert!(ct_eq(b"t0k", b"t0k"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"t0k", b"t0K"));
        assert!(!ct_eq(b"t0k", b"t0kk"));
    }

    #[tokio::test]
    async fn refuses_empty_token() {
        let config: SharedConfig = Arc::new(std::sync::RwLock::new(Default::default()));
        for token in ["", "   ", "\t\n"] {
            let opts = ServerOptions {
                port: 0,
                token: token.into(),
            };
            let err = start(Vec::new(), config.clone(), opts).await.err();
            assert!(matches!(err, Some(StartError::EmptyToken)), "{err:?}");
        }
        assert!(
            StartError::EmptyToken
                .to_string()
                .starts_with("O token do MCP está vazio.")
        );
    }

    #[test]
    fn platform_from_path_prefix() {
        assert_eq!(platform_of("/mcp/google-ads"), Some(Platform::GoogleAds));
        assert_eq!(platform_of("/mcp/meta-ads/x"), Some(Platform::MetaAds));
        assert_eq!(platform_of("/mcp/google-adsx"), None);
        assert_eq!(platform_of("/"), None);
    }
}
