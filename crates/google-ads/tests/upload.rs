use std::sync::{Arc, Mutex};
use std::time::Duration;

use adsconsole_core::secrets::Secret;
use adsconsole_google_ads::client::GoogleAdsClient;
use adsconsole_google_ads::client_json::ClientCreds;
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_google_ads::error::GoogleError;
use adsconsole_google_ads::meter::NoopMeter;
use adsconsole_google_ads::retry::RetryPolicy;
use adsconsole_google_ads::token::TokenManager;
use adsconsole_google_ads::upload::{self, UploadSession};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MIB: usize = 1024 * 1024;
const START: &str = "/upload/customers/1/youTubeVideoUploads:create";
const SESSION: &str = "/upload/session/abc";
const UPLOAD_RN: &str = "customers/1/youTubeVideoUploads/77";

async fn setup() -> (MockServer, GoogleAdsClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "tok", "expires_in": 3600})),
        )
        .mount(&server)
        .await;
    let endpoints = GoogleEndpoints {
        api_base: format!("{}/v26", server.uri()),
        upload_base: format!("{}/upload", server.uri()),
        auth_url: "https://accounts.example/auth".into(),
        token_url: format!("{}/token", server.uri()),
    };
    let creds = ClientCreds {
        client_id: "cid".into(),
        client_secret: Secret::new("csecret"),
        project_id: "proj".into(),
    };
    let tokens = Arc::new(TokenManager::new(
        endpoints.clone(),
        creds,
        Secret::new("refresh"),
    ));
    let client =
        GoogleAdsClient::new(endpoints, tokens, Arc::new(NoopMeter)).with_retry(RetryPolicy {
            base: Duration::from_millis(1),
        });
    (server, client)
}

async fn mock_start(server: &MockServer, granularity: Option<&str>) {
    let mut resp = ResponseTemplate::new(200)
        .insert_header("x-goog-upload-url", format!("{}{SESSION}", server.uri()));
    if let Some(g) = granularity {
        resp = resp.insert_header("x-goog-upload-chunk-granularity", g);
    }
    Mock::given(method("POST"))
        .and(path(START))
        .respond_with(resp)
        .mount(server)
        .await;
}

/// PUTs to the session: the last one (finalize) returns the upload name.
async fn mock_chunks(server: &MockServer) {
    Mock::given(method("PUT"))
        .and(path(SESSION))
        .respond_with(|req: &Request| {
            let cmd = req.headers.get("x-goog-upload-command").unwrap();
            if cmd.to_str().unwrap().contains("finalize") {
                ResponseTemplate::new(200).set_body_json(json!({ "resourceName": UPLOAD_RN }))
            } else {
                ResponseTemplate::new(200)
            }
        })
        .mount(server)
        .await;
}

fn video_file(len: usize) -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("video.mp4");
    let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    std::fs::write(&p, &bytes).unwrap();
    (dir, p, bytes)
}

async fn puts(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .collect()
}

fn hdr(r: &Request, k: &str) -> String {
    r.headers.get(k).unwrap().to_str().unwrap().to_string()
}

#[tokio::test]
async fn resumable_upload_protocol() {
    let (server, client) = setup().await;
    mock_start(&server, Some(&MIB.to_string())).await;
    mock_chunks(&server).await;
    let len = 5 * MIB / 2;
    let (_dir, file, bytes) = video_file(len);

    let mut session = upload::start_youtube_upload(
        &client,
        "1",
        Some("999"),
        len as u64,
        "Title",
        "Description",
        "UNLISTED",
        None,
    )
    .await
    .unwrap();
    assert_eq!(session.url, format!("{}{SESSION}", server.uri()));
    assert_eq!(session.granularity, MIB);
    // Default: ~8 MiB, a multiple of the granularity.
    assert_eq!(session.chunk_bytes, 8 * MIB);

    let reqs = server.received_requests().await.unwrap();
    let start = reqs.iter().find(|r| r.url.path() == START).unwrap();
    assert_eq!(hdr(start, "x-goog-upload-protocol"), "resumable");
    assert_eq!(hdr(start, "x-goog-upload-command"), "start");
    assert_eq!(
        hdr(start, "x-goog-upload-header-content-length"),
        len.to_string()
    );
    assert_eq!(hdr(start, "authorization"), "Bearer tok");
    assert_eq!(hdr(start, "login-customer-id"), "999");
    assert!(start.headers.get("developer-token").is_none());
    let body: Value = serde_json::from_slice(&start.body).unwrap();
    assert_eq!(
        body,
        json!({ "customer_id": "1", "you_tube_video_upload": {
            "video_title": "Title", "video_description": "Description",
            "video_privacy": "UNLISTED" } })
    );

    // 1 MiB chunks to get several PUTs.
    session.chunk_bytes = MIB;
    let progress = Arc::new(Mutex::new(Vec::new()));
    let p = progress.clone();
    let out = upload::upload_chunks(&client, &session, &file, move |n| p.lock().unwrap().push(n))
        .await
        .unwrap();
    assert_eq!(out, json!({ "resourceName": UPLOAD_RN }));
    assert_eq!(
        upload::upload_resource_name(&out).as_deref(),
        Some(UPLOAD_RN)
    );

    let puts = puts(&server).await;
    assert_eq!(puts.len(), 3);
    let offsets: Vec<String> = puts
        .iter()
        .map(|r| hdr(r, "x-goog-upload-offset"))
        .collect();
    assert_eq!(offsets, ["0", &MIB.to_string(), &(2 * MIB).to_string()]);
    let cmds: Vec<String> = puts
        .iter()
        .map(|r| hdr(r, "x-goog-upload-command"))
        .collect();
    assert_eq!(cmds, ["upload", "upload", "upload, finalize"]);
    assert!(puts.iter().all(|r| hdr(r, "authorization") == "Bearer tok"));
    let sent: Vec<u8> = puts.iter().flat_map(|r| r.body.clone()).collect();
    assert_eq!(sent, bytes);
    assert_eq!(puts[2].body.len(), MIB / 2);
    assert_eq!(
        *progress.lock().unwrap(),
        [MIB as u64, 2 * MIB as u64, len as u64]
    );
}

#[tokio::test]
async fn start_with_channel_and_default_granularity() {
    let (server, client) = setup().await;
    mock_start(&server, None).await;
    let s = upload::start_youtube_upload(&client, "1", None, 10, "T", "", "PUBLIC", Some("UCabc"))
        .await
        .unwrap();
    assert_eq!(s.granularity, 256 * 1024);
    assert_eq!(s.chunk_bytes, 8 * MIB);
    let reqs = server.received_requests().await.unwrap();
    let start = reqs.iter().find(|r| r.url.path() == START).unwrap();
    assert!(start.headers.get("login-customer-id").is_none());
    let body: Value = serde_json::from_slice(&start.body).unwrap();
    assert_eq!(body["you_tube_video_upload"]["channel_id"], "UCabc");
    assert_eq!(body["you_tube_video_upload"]["video_privacy"], "PUBLIC");
}

#[tokio::test]
async fn start_rejects_foreign_upload_url_and_errors() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(START))
        .respond_with(
            ResponseTemplate::new(200).insert_header("x-goog-upload-url", "https://evil.example/u"),
        )
        .mount(&server)
        .await;
    let e = upload::start_youtube_upload(&client, "1", None, 10, "T", "", "UNLISTED", None)
        .await
        .unwrap_err();
    assert!(matches!(e, GoogleError::Decode(_)), "{e:?}");

    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(START))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let e = upload::start_youtube_upload(&client, "1", None, 10, "T", "", "UNLISTED", None)
        .await
        .unwrap_err();
    assert!(matches!(e, GoogleError::Decode(_)), "{e:?}");

    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(START))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({ "error": { "code": 400, "message": "bad", "status": "INVALID_ARGUMENT" } }),
        ))
        .mount(&server)
        .await;
    let e = upload::start_youtube_upload(&client, "1", None, 10, "T", "", "UNLISTED", None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            GoogleError::Api {
                http_status: 400,
                ..
            }
        ),
        "{e:?}"
    );
}

#[tokio::test]
async fn chunk_failure_is_maybe_applied_and_not_retried() {
    let (server, client) = setup().await;
    mock_start(&server, Some(&MIB.to_string())).await;
    Mock::given(method("PUT"))
        .and(path(SESSION))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let (_dir, file, _) = video_file(2 * MIB);
    let mut session = upload::start_youtube_upload(
        &client,
        "1",
        None,
        (2 * MIB) as u64,
        "T",
        "",
        "UNLISTED",
        None,
    )
    .await
    .unwrap();
    session.chunk_bytes = MIB;
    let e = upload::upload_chunks(&client, &session, &file, |_| {})
        .await
        .unwrap_err();
    assert!(matches!(e, GoogleError::MaybeApplied { .. }), "{e:?}");
    assert!(e.to_string().contains("0 of 2097152"), "{e}");
    assert_eq!(puts(&server).await.len(), 1, "chunk is not retried");
}

#[tokio::test]
async fn file_that_changed_size_is_refused() {
    let (server, client) = setup().await;
    mock_start(&server, Some(&MIB.to_string())).await;
    mock_chunks(&server).await;
    let (_dir, file, _) = video_file(MIB);
    // The session was opened for 2 MiB, but the file is 1 MiB.
    let session = UploadSession {
        url: format!("{}{SESSION}", server.uri()),
        granularity: MIB,
        chunk_bytes: MIB,
        size: (2 * MIB) as u64,
        resource_name: None,
    };
    let e = upload::upload_chunks(&client, &session, &file, |_| {})
        .await
        .unwrap_err();
    assert!(matches!(e, GoogleError::File(_)), "{e:?}");
    assert!(puts(&server).await.is_empty());
}

fn state_row(state: &str, video: Option<&str>) -> Value {
    let mut u = json!({ "resourceName": UPLOAD_RN, "state": state });
    if let Some(v) = video {
        u["videoId"] = json!(v);
    }
    json!({ "results": [{ "youTubeVideoUpload": u }] })
}

#[tokio::test]
async fn poll_until_processed_or_timeout() {
    let (server, client) = setup().await;
    for (state, video) in [("PENDING", None), ("UPLOADED", Some("vid12345678"))] {
        Mock::given(method("POST"))
            .and(path("/v26/customers/1/googleAds:search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(state_row(state, video)))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v26/customers/1/googleAds:search"))
        .and(body_string_contains(UPLOAD_RN))
        .and(body_string_contains("you_tube_video_upload.video_id"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(state_row("PROCESSED", Some("vid12345678"))),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    let s = upload::poll_upload(
        &client,
        "1",
        None,
        UPLOAD_RN,
        Duration::from_millis(1),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(s.state, "PROCESSED");
    assert_eq!(s.video_id.as_deref(), Some("vid12345678"));
    let searches = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with(":search"))
        .count();
    assert_eq!(searches, 3);

    // Short deadline: returns the last state read.
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/v26/customers/1/googleAds:search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(state_row("UPLOADED", Some("vid12345678"))),
        )
        .mount(&server)
        .await;
    let s = upload::poll_upload(
        &client,
        "1",
        None,
        UPLOAD_RN,
        Duration::from_millis(5),
        Duration::from_millis(20),
    )
    .await
    .unwrap();
    assert_eq!(s.state, "UPLOADED");
    assert_eq!(s.video_id.as_deref(), Some("vid12345678"));

    // FAILED is final.
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/v26/customers/1/googleAds:search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(state_row("FAILED", None)))
        .expect(1)
        .mount(&server)
        .await;
    let s = upload::poll_upload(
        &client,
        "1",
        None,
        UPLOAD_RN,
        Duration::from_millis(1),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(s.state, "FAILED");
    assert!(s.video_id.is_none());
}

#[test]
fn remove_body_is_snake_case() {
    assert_eq!(
        upload::remove_body("1", UPLOAD_RN),
        json!({ "customer_id": "1", "resource_names": [UPLOAD_RN] })
    );
}
