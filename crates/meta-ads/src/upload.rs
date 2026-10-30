//! Mídia: imagens (arquivo ou URL) e vídeo em fases (start, transfer, finish).

use std::io::{Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::client::{MetaClient, WriteKind};
use crate::error::{MetaError, net_text};

/// Teto de uma imagem.
pub const MAX_IMAGE_BYTES: u64 = 30 * 1024 * 1024;
/// Teto de um vídeo.
pub const MAX_VIDEO_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const URL_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    File(std::path::PathBuf),
    Url(String),
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
        }
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Só `https`, sem localhost nem IP privado.
pub fn check_image_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw).map_err(|_| "URL inválida.".to_string())?;
    if url.scheme() != "https" {
        return Err("Use uma URL https.".into());
    }
    let local = match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local")
        }
        Some(url::Host::Ipv4(ip)) => private_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => private_ip(IpAddr::V6(ip)),
        None => true,
    };
    if local {
        return Err("Endereços locais ou de rede privada não são aceitos.".into());
    }
    Ok(url)
}

/// Lê a imagem (até 30 MB).
pub async fn load_image(source: &ImageSource) -> Result<Vec<u8>, MetaError> {
    match source {
        ImageSource::File(p) => {
            let meta = std::fs::metadata(p)
                .map_err(|e| MetaError::File(format!("{}: {e}", p.display())))?;
            if meta.len() > MAX_IMAGE_BYTES {
                return Err(MetaError::File(format!("{} passa de 30 MB", p.display())));
            }
            std::fs::read(p).map_err(|e| MetaError::File(format!("{}: {e}", p.display())))
        }
        ImageSource::Url(u) => {
            let url = check_image_url(u).map_err(MetaError::File)?;
            let client = reqwest::Client::builder()
                .timeout(URL_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| MetaError::Transport(net_text(e)))?;
            let resp = client.get(url).send().await.map_err(|e| {
                MetaError::Transport(format!("não deu para baixar a imagem: {}", net_text(e)))
            })?;
            if !resp.status().is_success() {
                return Err(MetaError::File(format!(
                    "a URL respondeu {}",
                    resp.status()
                )));
            }
            if resp.content_length().is_some_and(|n| n > MAX_IMAGE_BYTES) {
                return Err(MetaError::File("a imagem passa de 30 MB".into()));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| MetaError::Transport(net_text(e)))?;
            if bytes.len() as u64 > MAX_IMAGE_BYTES {
                return Err(MetaError::File("a imagem passa de 30 MB".into()));
            }
            Ok(bytes.to_vec())
        }
    }
}

/// Quando a imagem `hash` foi criada na biblioteca da conta, ou `None` se ela não está
/// lá (`GET {act}/adimages?hashes=[...]`). O hash é o que a Meta devolve no envio: ele
/// não é o MD5 dos bytes, então não dá para calculá-lo antes.
pub async fn image_created_time(
    client: &MetaClient,
    account: &str,
    hash: &str,
) -> Result<Option<DateTime<Utc>>, MetaError> {
    let out = client
        .get(
            &format!("{account}/adimages"),
            &[
                ("hashes", json!([hash]).to_string()),
                ("fields", "hash,created_time".into()),
            ],
        )
        .await?;
    Ok(out["data"]
        .as_array()
        .and_then(|d| d.iter().find(|i| i["hash"] == hash))
        .and_then(|i| i["created_time"].as_str())
        .and_then(|t| DateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%z").ok())
        .map(|t| t.with_timezone(&Utc)))
}

/// `POST {act}/adimages` com os bytes em base64; devolve `{image_hash, url, name}`.
pub async fn upload_image(
    client: &MetaClient,
    account: &str,
    bytes: &[u8],
    name: Option<&str>,
) -> Result<Value, MetaError> {
    let mut params = Map::new();
    params.insert("bytes".into(), json!(STANDARD.encode(bytes)));
    if let Some(n) = name {
        params.insert("name".into(), json!(n));
    }
    let out = client
        .post(&format!("{account}/adimages"), &params, WriteKind::Write)
        .await?;
    let (key, img) = out["images"]
        .as_object()
        .and_then(|m| m.iter().next())
        .ok_or_else(|| MetaError::Decode("adimages sem images".into()))?;
    Ok(json!({
        "image_hash": img["hash"],
        "url": img["url"],
        "name": img.get("name").cloned().unwrap_or_else(|| json!(key)),
    }))
}

fn offset(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn read_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>, MetaError> {
    let mut f = std::fs::File::open(path).map_err(|e| MetaError::File(e.to_string()))?;
    f.seek(SeekFrom::Start(start))
        .map_err(|e| MetaError::File(e.to_string()))?;
    let mut buf = vec![0; (end - start) as usize];
    f.read_exact(&mut buf)
        .map_err(|e| MetaError::File(e.to_string()))?;
    Ok(buf)
}

/// Envia o vídeo em fases (`start`, `transfer` nos offsets que a Meta pede, `finish`).
/// Devolve o `video_id`.
pub async fn upload_video(
    client: &MetaClient,
    account: &str,
    file: &Path,
    title: &str,
) -> Result<String, MetaError> {
    let size = std::fs::metadata(file)
        .map_err(|e| MetaError::File(format!("{}: {e}", file.display())))?
        .len();
    let path = format!("{account}/advideos");
    let mut start = Map::new();
    start.insert("upload_phase".into(), json!("start"));
    start.insert("file_size".into(), json!(size.to_string()));
    let started = client.post(&path, &start, WriteKind::Write).await?;
    let session = started["upload_session_id"]
        .as_str()
        .ok_or_else(|| MetaError::Decode("advideos start sem upload_session_id".into()))?
        .to_string();
    let video_id = match &started["video_id"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(MetaError::Decode("advideos start sem video_id".into())),
    };
    let chunk_client = client.clone().with_timeout(CHUNK_TIMEOUT);
    let (mut from, mut to) = (
        offset(&started["start_offset"]),
        offset(&started["end_offset"]),
    );
    while let (Some(s), Some(e)) = (from, to) {
        if s >= e {
            break;
        }
        let bytes = read_range(file, s, e.min(size))?;
        let session = session.clone();
        let make = move || {
            reqwest::multipart::Form::new()
                .text("upload_phase", "transfer")
                .text("upload_session_id", session.clone())
                .text("start_offset", s.to_string())
                .part(
                    "video_file_chunk",
                    reqwest::multipart::Part::bytes(bytes.clone()).file_name("chunk"),
                )
        };
        let out = chunk_client.post_multipart(&path, &make).await?;
        from = offset(&out["start_offset"]);
        to = offset(&out["end_offset"]);
    }
    let mut finish = Map::new();
    finish.insert("upload_phase".into(), json!("finish"));
    finish.insert("upload_session_id".into(), json!(session));
    finish.insert("title".into(), json!(title));
    client.post(&path, &finish, WriteKind::Write).await?;
    Ok(video_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoState {
    Ready,
    Processing { progress: Option<u64> },
    Error(String),
}

/// `GET {video_id}?fields=status`.
pub async fn video_status(client: &MetaClient, video_id: &str) -> Result<VideoState, MetaError> {
    let v = client.get(video_id, &[("fields", "status".into())]).await?;
    let st = &v["status"];
    Ok(match st["video_status"].as_str().unwrap_or_default() {
        "ready" => VideoState::Ready,
        "error" => VideoState::Error(
            st["processing_phase"]["errors"][0]["message"]
                .as_str()
                .unwrap_or("a Meta não conseguiu processar o vídeo")
                .to_string(),
        ),
        _ => VideoState::Processing {
            progress: st["processing_progress"].as_u64(),
        },
    })
}
