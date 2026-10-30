use std::time::Duration;

use adsconsole_core::secrets::Secret;
use adsconsole_meta_ads::client::MetaClient;
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::retry::RetryPolicy;
use adsconsole_meta_ads::upload::{VideoState, check_image_url, upload_video, video_status};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn client(server: &MockServer) -> MetaClient {
    MetaClient::new(
        MetaEndpoints {
            graph_base: server.uri(),
            dialog_url: "https://x.test".into(),
        },
        Secret::new("T"),
        None,
    )
    .with_retry(RetryPolicy {
        base: Duration::from_millis(1),
    })
}

fn has(r: &Request, needle: &str) -> bool {
    r.body.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn image_url_rules() {
    assert!(check_image_url("https://cdn.example.com/a.png").is_ok());
    for bad in [
        "http://cdn.example.com/a.png",
        "https://localhost/a.png",
        "https://127.0.0.1/a.png",
        "https://10.0.0.1/a.png",
        "https://192.168.0.5/a.png",
        "https://[::1]/a.png",
        "ftp://x/a.png",
        "nada",
    ] {
        assert!(check_image_url(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn video_phases_in_order() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v.mp4");
    std::fs::write(&file, b"ABCDEFGH").unwrap();
    Mock::given(method("POST"))
        .and(path("/act_1/advideos"))
        .and(|r: &Request| has(r, "upload_phase=start"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "upload_session_id": "S1", "video_id": "V1", "start_offset": "0", "end_offset": "4"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/act_1/advideos"))
        .and(|r: &Request| has(r, "ABCD") && has(r, "transfer"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"start_offset": "4", "end_offset": "8"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/act_1/advideos"))
        .and(|r: &Request| has(r, "EFGH") && has(r, "transfer"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"start_offset": "8", "end_offset": "8"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/act_1/advideos"))
        .and(|r: &Request| has(r, "upload_phase=finish") || (has(r, "finish") && has(r, "S1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;
    let id = upload_video(&client(&server), "act_1", &file, "Meu vídeo")
        .await
        .unwrap();
    assert_eq!(id, "V1");
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 4);
    assert!(has(&reqs[0], "file_size=8"));
    assert!(has(&reqs[1], "ABCD") && has(&reqs[1], "S1"));
    assert!(has(&reqs[2], "EFGH"));
    assert!(has(&reqs[3], "finish") && has(&reqs[3], "Meu"));
}

#[tokio::test]
async fn status_parsed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/V1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "V1", "status": {"video_status": "processing", "processing_progress": 40}
        })))
        .mount(&server)
        .await;
    assert_eq!(
        video_status(&client(&server), "V1").await.unwrap(),
        VideoState::Processing { progress: Some(40) }
    );
}
