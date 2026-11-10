use std::time::Duration;

use adsconsole_app::core::demo;
use adsconsole_core::Platform;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn demo_boot_has_connections_and_pending() {
    let dir = tempfile::tempdir().unwrap();
    let s = demo::boot(
        dir.path().to_path_buf(),
        demo::DemoOptions {
            port: free_port(),
            pending_after: Duration::from_millis(100),
        },
    )
    .await
    .unwrap();
    let conns = s.connections.list().unwrap();
    assert!(
        conns
            .iter()
            .filter(|c| c.platform == Platform::GoogleAds)
            .count()
            >= 2,
        "{conns:?}"
    );

    let mut pending = 0;
    for _ in 0..50 {
        pending = s.changes.pending().unwrap().len();
        if pending > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(pending, 1);
    // Nothing leaves the computer: adding a connection in the demo is refused.
    let err = s
        .connections
        .add_meta_token(adsconsole_app::core::connections::MetaTokenRequest {
            name: "x".into(),
            access_token: "t".into(),
            app_id: None,
            app_secret: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, "DEMO_MODE");
}
