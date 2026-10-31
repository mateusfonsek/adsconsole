mod common;

use adsconsole_app::core::runtime::{BootIssue, Runtime, ServerState};
use adsconsole_core::Platform;
use adsconsole_core::config::Config;
use adsconsole_core::secrets::SecretBackend;
use common::{boot_on, free_port, options, write_config};

fn running_port(s: &ServerState) -> u16 {
    match s {
        ServerState::Running { port } => *port,
        other => panic!("esperava Running, veio {other:?}"),
    }
}

#[tokio::test]
async fn boot_starts_server_and_persists_backend() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let (rt, _) = boot_on(dir.path(), port).await;
    assert_eq!(running_port(&rt.server_state()), port);
    let saved = Config::load(&dir.path().join("config.json"))
        .unwrap()
        .config;
    assert_eq!(saved.secret_backend, Some(SecretBackend::Keychain));
    assert_eq!(rt.config().secret_backend, Some(SecretBackend::Keychain));
}

#[tokio::test]
async fn boot_with_busy_port_reports_error() {
    let dir = tempfile::tempdir().unwrap();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let (rt, _) = boot_on(dir.path(), port).await;
    match rt.server_state() {
        ServerState::Error { port: p, error } => {
            assert_eq!(p, port);
            assert_eq!(error.code, "PORT_IN_USE");
        }
        other => panic!("esperava Error, veio {other:?}"),
    }
    // Os engines funcionam mesmo sem servidor.
    assert_eq!(
        rt.engine(Platform::GoogleAds).platform(),
        Platform::GoogleAds
    );
}

#[tokio::test]
async fn set_port_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (rt, _) = boot_on(dir.path(), busy.local_addr().unwrap().port()).await;
    let free = free_port();
    let state = rt.set_port(free).await.unwrap();
    assert_eq!(running_port(&state), free);
    assert_eq!(rt.config().port, free);
    assert_eq!(
        Config::load(&dir.path().join("config.json"))
            .unwrap()
            .config
            .port,
        free
    );
}

#[tokio::test]
async fn set_port_failure_keeps_old_port() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let (rt, _) = boot_on(dir.path(), port).await;
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let state = rt
        .set_port(busy.local_addr().unwrap().port())
        .await
        .unwrap();
    assert!(matches!(state, ServerState::Error { .. }), "{state:?}");
    assert_eq!(rt.config().port, port);
}

#[tokio::test]
async fn set_port_zero_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, _) = boot_on(dir.path(), free_port()).await;
    let err = rt.set_port(0).await.unwrap_err();
    assert_eq!(err.code, "INVALID_PORT");
}

#[tokio::test]
async fn pause_rejects_calls() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let (rt, _) = boot_on(dir.path(), port).await;
    rt.set_server_enabled(Platform::MetaAds, false).unwrap();
    let r = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}{}",
            Platform::MetaAds.endpoint()
        ))
        .bearer_auth(rt.token())
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert!(!rt.config().server(Platform::MetaAds).enabled);
}

#[tokio::test]
async fn keychain_failure_after_setup_is_boot_issue() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), |c| {
        c.port = free_port();
        c.secret_backend = Some(SecretBackend::Keychain);
    });
    let (opts, _) = options(dir.path(), false);
    let r = Runtime::boot(opts).await;
    assert!(matches!(r, Err(BootIssue::KeychainUnavailable)));
}

#[tokio::test]
async fn use_file_backend_then_boot_works() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), |c| {
        c.port = free_port();
        c.secret_backend = Some(SecretBackend::Keychain);
    });
    Runtime::use_file_backend(dir.path()).unwrap();
    let (opts, _) = options(dir.path(), false);
    let rt = Runtime::boot(opts).await.unwrap();
    assert_eq!(rt.config().secret_backend, Some(SecretBackend::File));
}

#[tokio::test]
async fn corrupt_config_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), "{lixo").unwrap();
    let (opts, _) = options(dir.path(), true);
    let rt = Runtime::boot(opts).await.unwrap();
    assert!(
        rt.notices()
            .iter()
            .any(|n| matches!(n, BootIssue::ConfigReset { .. }))
    );
}
