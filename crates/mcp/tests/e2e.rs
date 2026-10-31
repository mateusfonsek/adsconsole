//! Ponta a ponta: servidor HTTP real em porta 0, cliente `rmcp` e `reqwest`.

use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::Db;
use adsconsole_mcp::engine::{Engine, EngineEvent, SharedConfig};
use adsconsole_mcp::fake::FakePlatform;
use adsconsole_mcp::http::{RunningServer, ServerOptions, StartError, start};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

const TOKEN: &str = "t0k";

struct Harness {
    server: RunningServer,
    google: Engine,
    meta: Engine,
    google_fake: Arc<FakePlatform>,
    config: SharedConfig,
}

impl Harness {
    fn url(&self, p: Platform) -> String {
        format!("http://{}{}", self.server.addr, p.endpoint())
    }
}

/// Uma fake por plataforma, cada uma com o próprio banco (as duas usam conn_a/conn_b);
/// a configuração é compartilhada.
async fn spawn(mode: WriteMode) -> Harness {
    let mut config = Config::default();
    for p in Platform::ALL {
        config.servers.insert(
            p,
            ServerSettings {
                write_mode: mode,
                approval_wait_secs: 5,
                ..ServerSettings::default()
            },
        );
    }
    let config: SharedConfig = Arc::new(RwLock::new(config));

    let engine_for = |p: Platform| {
        let config = config.clone();
        async move {
            let db = Arc::new(Db::open_in_memory().unwrap());
            let fake = FakePlatform::new(p);
            fake.seed_connections(&db).unwrap();
            let engine = Engine::new(fake.clone(), db, config).unwrap();
            engine.refresh_accounts().await.unwrap();
            (engine, fake)
        }
    };
    let (google, google_fake) = engine_for(Platform::GoogleAds).await;
    let (meta, _) = engine_for(Platform::MetaAds).await;

    let server = start(
        vec![google.clone(), meta.clone()],
        config.clone(),
        ServerOptions {
            port: 0,
            token: TOKEN.into(),
            allow_ephemeral_port: true,
        },
    )
    .await
    .unwrap();
    Harness {
        server,
        google,
        meta,
        google_fake,
        config,
    }
}

async fn client(url: String) -> RunningService<RoleClient, ()> {
    ().serve(StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(TOKEN),
    ))
    .await
    .unwrap()
}

async fn call(c: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().cloned().unwrap();
    c.call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .unwrap()
}

fn structured(r: &CallToolResult) -> &Value {
    r.structured_content.as_ref().expect("structured_content")
}

async fn tool_names(c: &RunningService<RoleClient, ()>) -> Vec<String> {
    c.list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect()
}

fn initialize_body() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "curl", "version": "0"}
        }
    })
    .to_string()
}

fn post(url: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(initialize_body())
}

async fn error_message(r: reqwest::Response) -> String {
    let body: Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    body["error"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn rejects_missing_token_and_browser_origin() {
    let h = spawn(WriteMode::Direct).await;
    let url = h.url(Platform::GoogleAds);

    let r = post(&url).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let www = r.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(www.starts_with("Bearer"), "{www}");

    let r = post(&url)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(
        error_message(r).await,
        "Requests de navegador não são aceitos."
    );

    let r = post(&url)
        .header("authorization", "Bearer errado")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.headers().contains_key("www-authenticate"));
    assert!(!error_message(r).await.is_empty());
}

#[tokio::test]
async fn lists_tools_per_endpoint_with_toggles() {
    let h = spawn(WriteMode::Direct).await;
    let g = client(h.url(Platform::GoogleAds)).await;
    let names = tool_names(&g).await;
    for t in [
        "get_campaigns",
        "remove_campaign",
        "list_changes",
        "undo_change",
    ] {
        assert!(names.iter().any(|n| n == t), "{t} ausente em {names:?}");
    }
    assert!(!names.iter().any(|n| n == "raw_mutate"));

    {
        let mut c = h.config.write().unwrap();
        let mut s = c.server(Platform::GoogleAds);
        s.tools.insert("get_campaigns".into(), false);
        c.servers.insert(Platform::GoogleAds, s);
    }
    let names = tool_names(&g).await;
    assert!(!names.iter().any(|n| n == "get_campaigns"));

    let m = client(h.url(Platform::MetaAds)).await;
    let meta_names = tool_names(&m).await;
    let expected: Vec<String> = h
        .meta
        .registry()
        .enabled(&ServerSettings::default())
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(meta_names, expected);
    assert!(meta_names.iter().any(|n| n == "get_campaigns"));
}

#[tokio::test]
async fn write_and_undo_over_mcp() {
    let h = spawn(WriteMode::Direct).await;
    let g = client(h.url(Platform::GoogleAds)).await;

    let r = call(
        &g,
        "create_campaign",
        json!({"account_id": "111", "name": "x", "daily_budget": 30}),
    )
    .await;
    assert_eq!(r.is_error, Some(false));
    let out = structured(&r);
    assert_eq!(out["status"], "applied", "{out}");
    let change_id = out["change_id"].as_str().unwrap().to_string();
    let campaign = out["result"]["campaign_id"].as_str().unwrap().to_string();
    assert!(h.google_fake.campaign("111", &campaign).is_some());

    let r = call(&g, "get_change_status", json!({"change_id": change_id})).await;
    let by = structured(&r)["requested_by"].as_str().unwrap().to_string();
    assert!(by.starts_with("rmcp"), "{by}");

    let r = call(&g, "undo_change", json!({"change_id": change_id})).await;
    assert_eq!(r.is_error, Some(false));
    assert_eq!(structured(&r)["status"], "applied", "{:?}", r);
    assert!(h.google_fake.campaign("111", &campaign).is_none());
}

#[tokio::test]
async fn tool_errors_are_results_not_protocol_errors() {
    let h = spawn(WriteMode::Direct).await;
    let g = client(h.url(Platform::GoogleAds)).await;
    let r = call(
        &g,
        "create_campaign",
        json!({"account_id": "111", "name": "x", "daily_budget": 10}),
    )
    .await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(structured(&r)["error"]["code"], "BUDGET_TOO_LOW");
}

#[tokio::test]
async fn approval_over_mcp() {
    let h = spawn(WriteMode::Approval).await;
    let mut rx = h.google.subscribe();
    let engine = h.google.clone();
    let approver = tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let EngineEvent::ApprovalRequested { change_id, .. } = ev {
                engine.approve(&change_id).await.unwrap();
                return;
            }
        }
    });
    let g = client(h.url(Platform::GoogleAds)).await;
    let r = call(
        &g,
        "create_campaign",
        json!({"account_id": "111", "name": "x", "daily_budget": 30}),
    )
    .await;
    assert_eq!(structured(&r)["status"], "applied", "{:?}", r);
    approver.await.unwrap();
}

#[tokio::test]
async fn paused_platform_returns_503() {
    let h = spawn(WriteMode::Direct).await;
    {
        let mut c = h.config.write().unwrap();
        let mut s = c.server(Platform::MetaAds);
        s.enabled = false;
        c.servers.insert(Platform::MetaAds, s);
    }
    let r = post(&h.url(Platform::MetaAds))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(
        error_message(r).await,
        "O MCP de Meta Ads está pausado no AdsConsole."
    );

    let r = post(&h.url(Platform::GoogleAds))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn port_in_use_is_reported() {
    let h = spawn(WriteMode::Direct).await;
    let port = h.server.addr.port();
    let err = start(
        vec![h.google.clone()],
        h.config.clone(),
        ServerOptions {
            port,
            token: TOKEN.into(),
            allow_ephemeral_port: true,
        },
    )
    .await
    .err()
    .expect("a porta está ocupada");
    assert!(
        matches!(err, StartError::PortInUse(p) if p == port),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "A porta {port} já está em uso. Feche o outro programa ou mude a porta em AdsConsole → MCPs (ou ADSCONSOLE_PORT)."
        )
    );
}

#[tokio::test]
async fn shutdown_stops_listening() {
    let h = spawn(WriteMode::Direct).await;
    let addr = h.server.addr;
    assert!(tokio::net::TcpStream::connect(addr).await.is_ok());
    // Uma sessão MCP aberta não pode segurar o desligamento.
    let g = client(h.url(Platform::GoogleAds)).await;
    assert!(!tool_names(&g).await.is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(5), h.server.shutdown())
        .await
        .expect("shutdown terminou");
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}
