//! Start-up rules of the HTTP server: port, platforms and last request.

use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::Config;
use adsconsole_core::db::Db;
use adsconsole_mcp::engine::{Engine, SharedConfig};
use adsconsole_mcp::fake::FakePlatform;
use adsconsole_mcp::http::{ServerOptions, StartError, start};
use chrono::Utc;
use serde_json::json;

const TOKEN: &str = "t0k";

fn config() -> SharedConfig {
    Arc::new(RwLock::new(Config::default()))
}

fn engine(p: Platform, config: &SharedConfig) -> Engine {
    let db = Arc::new(Db::open_in_memory().unwrap());
    Engine::new(FakePlatform::new(p), db, config.clone()).unwrap()
}

fn opts(port: u16, allow_ephemeral_port: bool) -> ServerOptions {
    ServerOptions {
        port,
        token: TOKEN.into(),
        allow_ephemeral_port,
    }
}

#[tokio::test]
async fn duplicate_platform_is_error() {
    let c = config();
    let engines = vec![
        engine(Platform::GoogleAds, &c),
        engine(Platform::GoogleAds, &c),
    ];
    let err = start(engines, c, opts(0, true)).await.err().unwrap();
    assert!(
        matches!(err, StartError::DuplicatePlatform(Platform::GoogleAds)),
        "{err:?}"
    );
}

#[tokio::test]
async fn port_zero_rejected_without_flag() {
    let c = config();
    let err = start(vec![engine(Platform::GoogleAds, &c)], c, opts(0, false))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, StartError::InvalidPort(0)), "{err:?}");
}

#[tokio::test]
async fn last_request_at_set_after_authorized_call() {
    let c = config();
    let server = start(vec![engine(Platform::GoogleAds, &c)], c, opts(0, true))
        .await
        .unwrap();
    assert!(server.last_request_at().is_none());

    let url = format!("http://{}{}", server.addr, Platform::GoogleAds.endpoint());
    let http = reqwest::Client::new();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}});

    let unauthorized = http.post(&url).json(&init).send().await.unwrap();
    assert_eq!(unauthorized.status(), 401);
    assert!(server.last_request_at().is_none());

    let ok = http
        .post(&url)
        .bearer_auth(TOKEN)
        .header("Accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success(), "{}", ok.status());
    let at = server.last_request_at().expect("recorded the request");
    assert!(at <= Utc::now());
}
