mod common;

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use common::meta::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

fn engine(h: &MetaHarness) -> Engine {
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Direct,
            ..ServerSettings::default()
        },
    );
    Engine::new(
        h.adapter.clone(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap()
}

async fn active(h: &MetaHarness) {
    Mock::given(method("GET"))
        .and(path("/act_111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "act_111", "account_status": 1})),
        )
        .mount(&h.server)
        .await;
}

const IMAGE: &[u8] = b"\x89PNG fake";
/// O hash que a Meta dá a `IMAGE` (não é o MD5 dos bytes, como se viu na conta real).
const META_HASH: &str = "b8c704229b6dd1351b024c35acbafa4c";

/// Quando a biblioteca diz que a imagem `META_HASH` foi criada.
#[derive(Clone, Copy)]
enum Created {
    /// Agora: o envio criou a imagem.
    Now,
    /// Bem antes do envio: a imagem já estava lá.
    Old,
    /// A biblioteca não acha a imagem.
    Missing,
}

/// `GET {act}/adimages?hashes=[...]`: responde `created_time` conforme `created`.
async fn library(h: &MetaHarness, created: Created) {
    Mock::given(method("GET"))
        .and(path("/act_111/adimages"))
        .respond_with(move |r: &Request| {
            let asked: Vec<String> = r
                .url
                .query_pairs()
                .find(|(k, _)| k == "hashes")
                .and_then(|(_, v)| serde_json::from_str(&v).ok())
                .unwrap_or_default();
            let when = match created {
                Created::Now => Some(chrono::Utc::now()),
                Created::Old => Some(chrono::Utc::now() - chrono::Duration::days(30)),
                Created::Missing => None,
            };
            let data: Vec<_> = when
                .filter(|_| asked.iter().any(|a| a == META_HASH))
                .map(|t| {
                    json!({ "hash": META_HASH, "created_time": t.format("%Y-%m-%dT%H:%M:%S%z").to_string() })
                })
                .into_iter()
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
        })
        .mount(&h.server)
        .await;
}

fn has(r: &Request, needle: &str) -> bool {
    r.body.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn image_upload_returns_hash() {
    let h = meta_harness().await;
    active(&h).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("logo.png");
    std::fs::write(&file, IMAGE).unwrap();
    library(&h, Created::Now).await;
    Mock::given(method("POST"))
        .and(path("/act_111/adimages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"images": {"logo.png": {
                "hash": META_HASH, "url": "https://scontent.example/abc.png", "name": "logo.png"
            }}})),
        )
        .mount(&h.server)
        .await;
    let out = engine(&h)
        .call_tool(
            "upload_image",
            json!({"account_id": "act_111", "file_path": file.to_str().unwrap()}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(out["reversible"], true);
    assert_eq!(out["result"]["image_hash"], META_HASH);
    assert!(out["result"].get("aviso").is_none(), "{out}");
    let reqs = h.server.received_requests().await.unwrap();
    let lookup = reqs
        .iter()
        .find(|r| r.url.path() == "/act_111/adimages" && r.method.as_str() == "GET")
        .unwrap();
    assert!(
        lookup
            .url
            .query_pairs()
            .any(|(k, v)| k == "hashes" && v == format!("[\"{META_HASH}\"]")),
        "{}",
        lookup.url
    );
    let req = reqs
        .into_iter()
        .find(|r| r.url.path() == "/act_111/adimages" && r.method.as_str() == "POST")
        .unwrap();
    let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&req.body)
        .into_owned()
        .collect();
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&form["bytes"])
            .unwrap(),
        b"\x89PNG fake"
    );
    assert_eq!(form["name"], "logo.png");
}

#[tokio::test]
async fn image_undo_deletes_by_hash() {
    let h = meta_harness().await;
    active(&h).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("logo.png");
    std::fs::write(&file, IMAGE).unwrap();
    library(&h, Created::Now).await;
    let deletes = Arc::new(Mutex::new(Vec::new()));
    let seen = deletes.clone();
    Mock::given(method("POST"))
        .and(path("/act_111/adimages"))
        .respond_with(move |r: &Request| {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(&r.body).into_owned().collect();
            if form.get("method").map(String::as_str) == Some("delete") {
                seen.lock().unwrap().push(form["hash"].clone());
                ResponseTemplate::new(200).set_body_json(json!({"success": true}))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"images": {"logo.png": {
                    "hash": META_HASH, "url": "https://scontent.example/abc.png", "name": "logo.png"
                }}}))
            }
        })
        .mount(&h.server)
        .await;
    let e = engine(&h);
    let out = e
        .call_tool(
            "upload_image",
            json!({"account_id": "act_111", "file_path": file.to_str().unwrap()}),
            None,
        )
        .await
        .unwrap();
    e.call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap();
    assert_eq!(*deletes.lock().unwrap(), vec![META_HASH.to_string()]);
}

/// Envia `IMAGE` com a Meta devolvendo `hash`; desfazer não pode apagar nada.
async fn upload_not_undoable(created: Created) {
    let h = meta_harness().await;
    active(&h).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("logo.png");
    std::fs::write(&file, IMAGE).unwrap();
    library(&h, created).await;
    let deletes = Arc::new(Mutex::new(0));
    let seen = deletes.clone();
    Mock::given(method("POST"))
        .and(path("/act_111/adimages"))
        .respond_with(move |r: &Request| {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(&r.body).into_owned().collect();
            if form.contains_key("method") {
                *seen.lock().unwrap() += 1;
            }
            ResponseTemplate::new(200).set_body_json(json!({"images": {"logo.png": {
                "hash": META_HASH, "url": "https://scontent.example/abc.png", "name": "logo.png"
            }}}))
        })
        .mount(&h.server)
        .await;
    let e = engine(&h);
    let out = e
        .call_tool(
            "upload_image",
            json!({"account_id": "act_111", "file_path": file.to_str().unwrap()}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied");
    assert_eq!(out["reversible"], false, "{out}");
    assert_eq!(out["result"]["image_hash"], META_HASH);
    assert!(out["result"]["aviso"].as_str().is_some(), "{out}");
    let err = e
        .call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_REVERSIBLE);
    assert_eq!(*deletes.lock().unwrap(), 0);
}

#[tokio::test]
async fn image_already_in_library_not_undoable() {
    upload_not_undoable(Created::Old).await;
}

#[tokio::test]
async fn image_hash_unconfirmed_not_undoable() {
    // A biblioteca não acha a imagem: não dá para saber se ela já existia.
    upload_not_undoable(Created::Missing).await;
}

#[tokio::test]
async fn image_url_rules() {
    let h = meta_harness().await;
    active(&h).await;
    let e = engine(&h);
    for url in [
        "http://cdn.example/a.png",
        "https://localhost/a.png",
        "https://10.0.0.1/a.png",
    ] {
        let err = e
            .call_tool(
                "upload_image",
                json!({"account_id": "act_111", "url": url}),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_INPUT, "{url}");
        assert_eq!(err.field.as_deref(), Some("url"));
    }
    let err = e
        .call_tool("upload_image", json!({"account_id": "act_111"}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    let err = e
        .call_tool(
            "upload_image",
            json!({"account_id": "act_111", "file_path": "/nao/existe.png"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("file_path"));
}

async fn mock_video_upload(h: &MetaHarness) {
    Mock::given(method("POST"))
        .and(path("/act_111/advideos"))
        .and(|r: &Request| has(r, "upload_phase=start"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "upload_session_id": "S1", "video_id": "V1", "start_offset": "0", "end_offset": "4"
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/act_111/advideos"))
        .and(|r: &Request| has(r, "transfer"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"start_offset": "4", "end_offset": "4"})),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/act_111/advideos"))
        .and(|r: &Request| has(r, "finish"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
}

/// GET do vídeo devolve os estados em sequência (o último se repete).
async fn mock_states(h: &MetaHarness, states: Vec<&'static str>) {
    let n = Arc::new(Mutex::new(0usize));
    Mock::given(method("GET"))
        .and(path("/V1"))
        .respond_with(move |_: &Request| {
            let mut i = n.lock().unwrap();
            let s = states[(*i).min(states.len() - 1)];
            *i += 1;
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "V1", "status": {"video_status": s}}))
        })
        .mount(&h.server)
        .await;
}

fn video_file() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v.mp4");
    std::fs::write(&file, b"ABCD").unwrap();
    let p = file.to_str().unwrap().to_string();
    (dir, p)
}

#[tokio::test]
async fn video_waits_until_ready() {
    let h = meta_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(5), Duration::from_secs(5));
    active(&h).await;
    mock_video_upload(&h).await;
    mock_states(&h, vec!["processing", "processing", "ready"]).await;
    let (_dir, file) = video_file();
    let out = engine(&h)
        .call_tool(
            "upload_video",
            json!({"account_id": "act_111", "file_path": file, "title": "Vídeo"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["result"], json!({"video_id": "V1", "status": "ready"}));
    assert_eq!(out["reversible"], false);
}

#[tokio::test]
async fn video_warning_and_validation() {
    let h = meta_harness().await;
    active(&h).await;
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Approval,
            approval_wait_secs: 0,
            ..ServerSettings::default()
        },
    );
    let e = Engine::new(
        h.adapter.clone(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap();
    let (_dir, file) = video_file();
    e.call_tool(
        "upload_video",
        json!({"account_id": "act_111", "file_path": file, "title": "x"}),
        None,
    )
    .await
    .unwrap();
    let w = e.pending().unwrap()[0].warning.clone().unwrap();
    assert!(w.contains("esperam o envio terminar"), "{w}");
    let err = e
        .call_tool(
            "upload_video",
            json!({"account_id": "act_111", "file_path": "/nao/existe.mp4", "title": "x"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.field.as_deref(), Some("file_path"));
}

#[tokio::test]
async fn video_error_deletes_and_fails() {
    let h = meta_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(5), Duration::from_secs(5));
    active(&h).await;
    mock_video_upload(&h).await;
    mock_states(&h, vec!["processing", "error"]).await;
    Mock::given(method("DELETE"))
        .and(path("/act_111/advideos"))
        .and(wiremock::matchers::query_param("video_id", "V1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .expect(1)
        .mount(&h.server)
        .await;
    let (_dir, file) = video_file();
    let err = engine(&h)
        .call_tool(
            "upload_video",
            json!({"account_id": "act_111", "file_path": file, "title": "x"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::EXECUTION_FAILED);
}

#[tokio::test]
async fn video_upload_is_not_reversible() {
    // A Meta não deixa apagar vídeos da conta com as permissões do caso de uso
    // (#10 em DELETE /act_…/advideos e pages_manage_posts em DELETE /{video_id}).
    let h = meta_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(5), Duration::from_secs(5));
    active(&h).await;
    mock_video_upload(&h).await;
    mock_states(&h, vec!["ready"]).await;
    let (_dir, file) = video_file();
    let e = engine(&h);
    let out = e
        .call_tool(
            "upload_video",
            json!({"account_id": "act_111", "file_path": file, "title": "x"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["reversible"], false);
    let err = e
        .call_tool("undo_change", json!({"change_id": out["change_id"]}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_REVERSIBLE);
}

#[tokio::test]
async fn video_timeout_reports_processing() {
    let h = meta_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(5), Duration::from_millis(30));
    active(&h).await;
    mock_video_upload(&h).await;
    mock_states(&h, vec!["processing"]).await;
    let (_dir, file) = video_file();
    let out = engine(&h)
        .call_tool(
            "upload_video",
            json!({"account_id": "act_111", "file_path": file, "title": "x"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["result"]["status"], "processing");
    assert_eq!(out["reversible"], false);
}
