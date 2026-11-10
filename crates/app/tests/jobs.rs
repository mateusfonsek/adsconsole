mod common;

use std::sync::Arc;

use adsconsole_app::core::connections::{ConnStatusView, ConnectionView};
use adsconsole_app::core::jobs::{
    check_update, first_call_tick, newer_release, newly_needing_reconnect,
};
use adsconsole_core::Platform;
use common::{boot_on, free_port};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn newer_release_compares_semver() {
    assert_eq!(newer_release("0.1.0", "v0.2.0"), Some("0.2.0".to_string()));
    assert_eq!(newer_release("0.2.0", "v0.2.0"), None);
    assert_eq!(newer_release("0.10.0", "v0.9.9"), None);
    assert_eq!(newer_release("0.1.0", "v0.2.0-rc.1"), None);
    assert_eq!(newer_release("0.1.0", "garbage"), None);
}

#[tokio::test]
async fn check_update_handles_errors_and_success() {
    let server = MockServer::start().await;
    let http = reqwest::Client::new();
    assert!(
        check_update(&http, &server.uri(), "o/r", "0.1.0")
            .await
            .is_none()
    );

    Mock::given(method("GET"))
        .and(path("/repos/o/r/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "tag_name": "v9.9.9",
            "html_url": "https://example.test/r"
        })))
        .mount(&server)
        .await;
    let info = check_update(&http, &server.uri(), "o/r", "0.1.0")
        .await
        .unwrap();
    assert_eq!(info.version, "9.9.9");
    assert_eq!(info.url, "https://example.test/r");
}

#[tokio::test]
async fn first_call_sets_client_seen() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let (rt, _) = boot_on(dir.path(), port).await;
    let rt = Arc::new(rt);
    assert!(!first_call_tick(&rt).await.unwrap());

    reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}{}",
            Platform::GoogleAds.endpoint()
        ))
        .bearer_auth(rt.token())
        .body("{}")
        .send()
        .await
        .unwrap();
    assert!(first_call_tick(&rt).await.unwrap());
    assert!(rt.config().client_seen);
    // It only reports once.
    assert!(!first_call_tick(&rt).await.unwrap());
}

fn conn(id: &str, status: ConnStatusView) -> ConnectionView {
    ConnectionView {
        id: id.into(),
        platform: Platform::MetaAds,
        name: format!("Connection {id}"),
        is_default: false,
        status,
        expires_at: None,
        accounts: 0,
        detail: None,
    }
}

#[test]
fn reconnect_only_reported_on_change() {
    let before = vec![
        conn("a", ConnStatusView::Ok),
        conn("b", ConnStatusView::NeedsReconnect),
    ];
    let after = vec![
        conn("a", ConnStatusView::NeedsReconnect),
        conn("b", ConnStatusView::NeedsReconnect),
    ];
    assert_eq!(
        newly_needing_reconnect(&before, &after),
        vec!["Connection a".to_string()]
    );
    assert!(newly_needing_reconnect(&after, &after).is_empty());
}
