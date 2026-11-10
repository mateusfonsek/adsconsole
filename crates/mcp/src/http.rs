//! Local HTTP server: one MCP endpoint per platform, behind the access middleware.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use adsconsole_core::Platform;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::{Engine, SharedConfig};
use crate::server::McpService;

/// Interval between sweeps for expired pending changes.
const EXPIRE_EVERY: Duration = Duration::from_secs(60);

pub struct ServerOptions {
    /// 0 picks a free port (used in tests).
    pub port: u16,
    pub token: String,
    /// Accepts `port: 0` (a free port chosen by the system); only tests use it.
    pub allow_ephemeral_port: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(
        "Port {0} is already in use. Close the other program or change the port in AdsConsole → MCPs (or ADSCONSOLE_PORT)."
    )]
    PortInUse(u16),
    #[error(
        "The MCP token is empty. Set a token in ADSCONSOLE_TOKEN or let AdsConsole generate one."
    )]
    EmptyToken,
    #[error("Invalid port: {0}. Use a number from 1 to 65535.")]
    InvalidPort(u16),
    #[error("Two {} MCP instances on the same server.", .0.display_name())]
    DuplicatePlatform(Platform),
    #[error("Failed to start the MCP server: {0}")]
    Io(#[from] std::io::Error),
}

pub struct RunningServer {
    pub addr: SocketAddr,
    last_request: Arc<AtomicI64>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningServer {
    /// Time of the last request with a valid token, on any endpoint.
    pub fn last_request_at(&self) -> Option<DateTime<Utc>> {
        match self.last_request.load(Ordering::Relaxed) {
            0 => None,
            ms => DateTime::from_timestamp_millis(ms),
        }
    }

    /// Closes the MCP sessions, stops accepting connections and waits for the server to finish.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for task in std::mem::take(&mut self.tasks) {
            if let Err(e) = task.await {
                tracing::error!(error = %e, "mcp server task ended with an error");
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
    last_request: Arc<AtomicI64>,
}

pub async fn start(
    engines: Vec<Engine>,
    config: SharedConfig,
    opts: ServerOptions,
) -> Result<RunningServer, StartError> {
    // An empty token would accept `Authorization: Bearer ` from any local process.
    if opts.token.trim().is_empty() {
        return Err(StartError::EmptyToken);
    }
    if opts.port == 0 && !opts.allow_ephemeral_port {
        return Err(StartError::InvalidPort(0));
    }
    for (i, e) in engines.iter().enumerate() {
        if engines[..i].iter().any(|o| o.platform() == e.platform()) {
            return Err(StartError::DuplicatePlatform(e.platform()));
        }
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
    let last_request = Arc::new(AtomicI64::new(0));
    let guard = Arc::new(Guard {
        token: opts.token,
        config,
        last_request: last_request.clone(),
    });
    let app = router.layer(middleware::from_fn_with_state(guard, check_access));

    let mut tasks = Vec::with_capacity(engines.len() + 1);
    let stop = cancel.clone();
    tasks.push(tokio::spawn(async move {
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await;
        if let Err(e) = served {
            tracing::error!(error = %e, "mcp server stopped with an error");
        }
    }));
    for engine in engines {
        tasks.push(tokio::spawn(expire_loop(engine, cancel.clone())));
    }
    tracing::info!(%addr, "mcp server listening");
    Ok(RunningServer {
        addr,
        last_request,
        cancel,
        tasks,
    })
}

/// Expires the stale pending changes now and every `EXPIRE_EVERY`, until cancelled.
async fn expire_loop(engine: Engine, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(EXPIRE_EVERY);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                if let Err(e) = engine.expire_stale(Utc::now()) {
                    tracing::warn!(platform = engine.platform().as_str(), error = %e, "failed to expire pending changes");
                }
            }
        }
    }
}

/// Order: browser (403) → token (401) → paused platform (503).
async fn check_access(State(guard): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    if headers.contains_key(header::ORIGIN) {
        return reject(
            StatusCode::FORBIDDEN,
            "Browser requests are not accepted.".into(),
        );
    }
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|t| ct_eq(t, guard.token.as_bytes()));
    if !authorized {
        let mut resp = reject(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid token. Copy the token from AdsConsole → MCPs.".into(),
        );
        resp.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return resp;
    }
    guard
        .last_request
        .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
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
                format!("The {} MCP is paused in AdsConsole.", p.display_name()),
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

/// Constant-time comparison for equal lengths (the length itself is not a secret).
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
                allow_ephemeral_port: true,
            };
            let err = start(Vec::new(), config.clone(), opts).await.err();
            assert!(matches!(err, Some(StartError::EmptyToken)), "{err:?}");
        }
        assert!(
            StartError::EmptyToken
                .to_string()
                .starts_with("The MCP token is empty.")
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
