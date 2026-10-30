//! Upload resumable de vídeo para o YouTube (`YouTubeVideoUploadService`).
//!
//! 1. [`start_youtube_upload`]: POST em `{upload_base}/customers/{c}/youTubeVideoUploads:create`
//!    com o corpo em snake_case; a URL da sessão vem em `x-goog-upload-url`.
//! 2. [`upload_chunks`]: PUTs na sessão, em pedaços múltiplos da granularidade; o último
//!    leva `upload, finalize` e a resposta traz o nome do upload.
//! 3. [`poll_upload`]: lê `you_tube_video_upload.state` até `PROCESSED`, `FAILED` ou
//!    `REJECTED` (ou até o prazo).
//! 4. [`REMOVE_SUFFIX`] e [`remove_body`]: `youTubeVideoUploads:remove` (o vídeo fica
//!    `UNAVAILABLE`), enviado pelo desfazer como POST direto.
//!
//! O token vai só no header `Authorization`, e só para o mesmo host do `upload_base`.
//! Nenhum pedaço é repetido: depois do primeiro byte enviado, qualquer falha é
//! [`GoogleError::MaybeApplied`].

use std::path::Path;
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::client::GoogleAdsClient;
use crate::error::{GoogleError, parse_api_error};
use crate::fields;
use crate::token::map_reqwest;

/// Timeout de cada PUT de pedaço.
pub const CHUNK_TIMEOUT: Duration = Duration::from_secs(300);
/// Tamanho alvo de cada pedaço (arredondado para baixo na granularidade).
pub const TARGET_CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Granularidade quando o Google não informa (a do protocolo resumable do Google).
pub const DEFAULT_GRANULARITY: usize = 256 * 1024;
/// Sufixo do `remove`, relativo a `customers/{c}`.
pub const REMOVE_SUFFIX: &str = "/youTubeVideoUploads:remove";
/// Estados finais do upload.
pub const FINAL_STATES: [&str; 3] = ["PROCESSED", "FAILED", "REJECTED"];

/// Sessão aberta por [`start_youtube_upload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSession {
    pub url: String,
    pub granularity: usize,
    /// Bytes por PUT: múltiplo de `granularity`, cerca de [`TARGET_CHUNK_BYTES`].
    pub chunk_bytes: usize,
    /// Tamanho declarado no start; o arquivo tem que ter exatamente isso.
    pub size: u64,
    /// Nome do upload, se o start já o devolveu no corpo.
    pub resource_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadState {
    pub state: String,
    pub video_id: Option<String>,
}

/// Maior múltiplo de `granularity` até [`TARGET_CHUNK_BYTES`] (no mínimo um).
pub fn chunk_bytes(granularity: usize) -> usize {
    let g = granularity.max(1);
    (TARGET_CHUNK_BYTES / g).max(1) * g
}

/// Nome do upload numa resposta (`resourceName`, `resource_name` ou dentro de
/// `youTubeVideoUpload`).
pub fn upload_resource_name(v: &Value) -> Option<String> {
    let direct = |o: &Value| {
        o.get("resourceName")
            .or_else(|| o.get("resource_name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    direct(v).or_else(|| v.get("youTubeVideoUpload").and_then(direct))
}

/// Mesmo esquema, host e porta: o token só vai para o host do `upload_base`.
fn same_origin(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => {
            a.scheme() == b.scheme()
                && a.host_str() == b.host_str()
                && a.port_or_known_default() == b.port_or_known_default()
        }
        _ => false,
    }
}

/// Abre a sessão de upload de `size` bytes. `channel_id` só quando o vídeo vai para o
/// canal do anunciante; sem ele, vai para um canal gerenciado pelo Google. Sem retry.
#[allow(clippy::too_many_arguments)]
pub async fn start_youtube_upload(
    client: &GoogleAdsClient,
    customer: &str,
    login: Option<&str>,
    size: u64,
    title: &str,
    description: &str,
    privacy: &str,
    channel_id: Option<&str>,
) -> Result<UploadSession, GoogleError> {
    let base = &client.endpoints().upload_base;
    let url = format!("{base}/customers/{customer}/youTubeVideoUploads:create");
    let mut video = json!({
        "video_title": title,
        "video_description": description,
        "video_privacy": privacy,
    });
    if let Some(c) = channel_id {
        video["channel_id"] = json!(c);
    }
    let body = json!({ "customer_id": customer, "you_tube_video_upload": video });
    let resp = client
        .authed(Method::POST, &url, login)
        .await?
        .header("X-Goog-Upload-Protocol", "resumable")
        .header("X-Goog-Upload-Command", "start")
        .header("X-Goog-Upload-Header-Content-Length", size.to_string())
        .json(&body)
        .send()
        .await
        .map_err(map_reqwest)?;
    client.record_ops(1);
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.map_err(map_reqwest)?;
    let hdr = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    if !status.is_success() {
        return Err(parse_api_error(
            status.as_u16(),
            &bytes,
            hdr("request-id"),
            false,
        ));
    }
    let session_url = hdr("x-goog-upload-url").ok_or_else(|| {
        GoogleError::Decode("o início do upload não trouxe a URL da sessão".into())
    })?;
    if !same_origin(&session_url, base) {
        return Err(GoogleError::Decode(
            "a URL da sessão de upload não é do host do Google Ads".into(),
        ));
    }
    let granularity = hdr("x-goog-upload-chunk-granularity")
        .and_then(|g| g.parse::<usize>().ok())
        .filter(|g| *g > 0)
        .unwrap_or(DEFAULT_GRANULARITY);
    let resource_name = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| upload_resource_name(&v));
    Ok(UploadSession {
        url: session_url,
        granularity,
        chunk_bytes: chunk_bytes(granularity),
        size,
        resource_name,
    })
}

/// Falha depois de bytes enviados: o upload pode existir na conta (pendente ou até
/// concluído, se foi o último pedaço).
fn interrupted(offset: u64, size: u64, why: impl std::fmt::Display) -> GoogleError {
    GoogleError::MaybeApplied {
        cause: format!("envio do vídeo interrompido no byte {offset} de {size}: {why}"),
    }
}

fn file_error(file: &Path, what: &str, e: std::io::Error) -> GoogleError {
    GoogleError::File(format!("não foi possível {what} {}: {e}", file.display()))
}

/// Envia o arquivo na sessão, lendo um pedaço por vez (nunca o arquivo inteiro).
/// `on_progress` recebe o total de bytes já aceitos. Devolve o corpo da resposta do
/// último pedaço (com o nome do upload; ver [`upload_resource_name`]).
///
/// Erros:
/// - [`GoogleError::File`] se o arquivo não abre ou não tem `session.size` bytes,
///   antes de qualquer PUT;
/// - [`GoogleError::MaybeApplied`] em qualquer falha depois do primeiro PUT (inclusive
///   o arquivo mudar no meio). Nenhum pedaço é repetido.
pub async fn upload_chunks(
    client: &GoogleAdsClient,
    session: &UploadSession,
    file: &Path,
    on_progress: impl Fn(u64),
) -> Result<Value, GoogleError> {
    let size = session.size;
    let mut f = tokio::fs::File::open(file)
        .await
        .map_err(|e| file_error(file, "abrir", e))?;
    let meta = f.metadata().await.map_err(|e| file_error(file, "ler", e))?;
    if !meta.is_file() || meta.len() != size {
        return Err(GoogleError::File(format!(
            "{} mudou: tinha {size} bytes e agora tem {}.",
            file.display(),
            meta.len()
        )));
    }
    let chunk = session.chunk_bytes.max(1) as u64;
    let mut offset: u64 = 0;
    loop {
        let len = chunk.min(size - offset);
        let last = offset + len == size;
        let mut buf = vec![0u8; len as usize];
        if let Err(e) = f.read_exact(&mut buf).await {
            let msg = format!("o arquivo mudou durante o envio ({e})");
            return Err(if offset == 0 {
                GoogleError::File(msg)
            } else {
                interrupted(offset, size, msg)
            });
        }
        let req = match client.authed(Method::PUT, &session.url, None).await {
            Ok(r) => r,
            Err(e) if offset == 0 => return Err(e),
            Err(e) => return Err(interrupted(offset, size, e)),
        };
        let resp = req
            .timeout(CHUNK_TIMEOUT)
            .header("X-Goog-Upload-Offset", offset.to_string())
            .header(
                "X-Goog-Upload-Command",
                if last { "upload, finalize" } else { "upload" },
            )
            .body(buf)
            .send()
            .await
            .map_err(|e| interrupted(offset, size, map_reqwest(e)))?;
        let status = resp.status();
        let request_id = resp
            .headers()
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| interrupted(offset, size, map_reqwest(e)))?;
        if !status.is_success() {
            let api = parse_api_error(status.as_u16(), &bytes, request_id, false);
            return Err(interrupted(offset, size, api));
        }
        offset += len;
        on_progress(offset);
        if last {
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_slice(&bytes)
                .map_err(|_| interrupted(offset, size, "resposta final malformada"));
        }
    }
}

fn state_of(row: &Value) -> UploadState {
    let u = row.get("youTubeVideoUpload");
    let s = |k: &str| {
        u.and_then(|u| u.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    UploadState {
        state: s("state").unwrap_or_else(|| "UNSPECIFIED".into()),
        video_id: s("videoId"),
    }
}

/// Lê o estado do upload a cada `every` até um estado final ([`FINAL_STATES`]) ou até
/// `max`; no prazo, devolve o último estado lido. Upload que a busca não encontra
/// fica `UNSPECIFIED`.
pub async fn poll_upload(
    client: &GoogleAdsClient,
    customer: &str,
    login: Option<&str>,
    upload_rn: &str,
    every: Duration,
    max: Duration,
) -> Result<UploadState, GoogleError> {
    let query = fields::read_query(
        "you_tube_video_upload",
        upload_rn,
        &[
            "you_tube_video_upload.resource_name",
            "you_tube_video_upload.state",
            "you_tube_video_upload.video_id",
        ],
    );
    let t0 = tokio::time::Instant::now();
    loop {
        let page = client.search(customer, login, &query, None).await?;
        let state = page.results.first().map_or_else(
            || UploadState {
                state: "UNSPECIFIED".into(),
                video_id: None,
            },
            state_of,
        );
        if FINAL_STATES.contains(&state.state.as_str()) || t0.elapsed() + every > max {
            return Ok(state);
        }
        tokio::time::sleep(every).await;
    }
}

/// Corpo do `youTubeVideoUploads:remove` (snake_case, validado na API real).
pub fn remove_body(customer: &str, rn: &str) -> Value {
    json!({ "customer_id": customer, "resource_names": [rn] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_sizes() {
        assert_eq!(chunk_bytes(1024 * 1024), 8 * 1024 * 1024);
        assert_eq!(chunk_bytes(3 * 1024 * 1024), 6 * 1024 * 1024);
        assert_eq!(chunk_bytes(16 * 1024 * 1024), 16 * 1024 * 1024);
        assert_eq!(chunk_bytes(0), TARGET_CHUNK_BYTES);
    }

    #[test]
    fn resource_name_shapes() {
        let rn = "customers/1/youTubeVideoUploads/2";
        for v in [
            json!({ "resourceName": rn }),
            json!({ "resource_name": rn }),
            json!({ "youTubeVideoUpload": { "resourceName": rn } }),
        ] {
            assert_eq!(upload_resource_name(&v).as_deref(), Some(rn), "{v}");
        }
        assert!(upload_resource_name(&json!({})).is_none());
        assert!(upload_resource_name(&json!({ "resourceName": "" })).is_none());
    }

    #[test]
    fn origin_check() {
        assert!(same_origin(
            "https://googleads.googleapis.com/resumable/x?upload_id=1",
            "https://googleads.googleapis.com/resumable/upload/v26"
        ));
        assert!(!same_origin(
            "https://evil.example/x",
            "https://googleads.googleapis.com/resumable/upload/v26"
        ));
        assert!(!same_origin(
            "http://googleads.googleapis.com/x",
            "https://googleads.googleapis.com/resumable/upload/v26"
        ));
    }
}
