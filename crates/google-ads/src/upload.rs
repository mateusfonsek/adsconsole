//! Resumable video upload to YouTube (`YouTubeVideoUploadService`).
//!
//! 1. [`start_youtube_upload`]: POST to `{upload_base}/customers/{c}/youTubeVideoUploads:create`
//!    with a snake_case body; the session URL comes in `x-goog-upload-url`.
//! 2. [`upload_chunks`]: PUTs to the session, in chunks that are multiples of the
//!    granularity; the last one carries `upload, finalize` and the response has the
//!    upload name.
//! 3. [`poll_upload`]: reads `you_tube_video_upload.state` until `PROCESSED`, `FAILED`
//!    or `REJECTED` (or until the deadline).
//! 4. [`REMOVE_SUFFIX`] and [`remove_body`]: `youTubeVideoUploads:remove` (the video
//!    becomes `UNAVAILABLE`), sent by undo as a direct POST.
//!
//! The token goes only in the `Authorization` header, and only to the same host as
//! `upload_base`. No chunk is retried: after the first byte is sent, any failure is
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

/// Timeout of each chunk PUT.
pub const CHUNK_TIMEOUT: Duration = Duration::from_secs(300);
/// Target size of each chunk (rounded down to the granularity).
pub const TARGET_CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Granularity when Google does not provide one (the Google resumable protocol default).
pub const DEFAULT_GRANULARITY: usize = 256 * 1024;
/// Suffix of the `remove` call, relative to `customers/{c}`.
pub const REMOVE_SUFFIX: &str = "/youTubeVideoUploads:remove";
/// Final states of the upload.
pub const FINAL_STATES: [&str; 3] = ["PROCESSED", "FAILED", "REJECTED"];

/// Session opened by [`start_youtube_upload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSession {
    pub url: String,
    pub granularity: usize,
    /// Bytes per PUT: a multiple of `granularity`, about [`TARGET_CHUNK_BYTES`].
    pub chunk_bytes: usize,
    /// Size declared at start; the file must be exactly this size.
    pub size: u64,
    /// Upload name, if the start call already returned it in the body.
    pub resource_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadState {
    pub state: String,
    pub video_id: Option<String>,
}

/// Largest multiple of `granularity` up to [`TARGET_CHUNK_BYTES`] (at least one).
pub fn chunk_bytes(granularity: usize) -> usize {
    let g = granularity.max(1);
    (TARGET_CHUNK_BYTES / g).max(1) * g
}

/// Upload name in a response (`resourceName`, `resource_name` or inside
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

/// Same scheme, host and port: the token only goes to the `upload_base` host.
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

/// Opens the upload session for `size` bytes. `channel_id` only when the video goes to
/// the advertiser's channel; without it, it goes to a Google-managed channel. No retry.
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
        GoogleError::Decode("the upload start did not return the session URL".into())
    })?;
    if !same_origin(&session_url, base) {
        return Err(GoogleError::Decode(
            "the upload session URL is not on the Google Ads host".into(),
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

/// Failure after bytes were sent: the upload may exist in the account (pending or even
/// complete, if it was the last chunk).
fn interrupted(offset: u64, size: u64, why: impl std::fmt::Display) -> GoogleError {
    GoogleError::MaybeApplied {
        cause: format!("video upload interrupted at byte {offset} of {size}: {why}"),
    }
}

fn file_error(file: &Path, what: &str, e: std::io::Error) -> GoogleError {
    GoogleError::File(format!("could not {what} {}: {e}", file.display()))
}

/// Sends the file to the session, reading one chunk at a time (never the whole file).
/// `on_progress` receives the total of bytes already accepted. Returns the body of the
/// last chunk's response (with the upload name; see [`upload_resource_name`]).
///
/// Errors:
/// - [`GoogleError::File`] if the file does not open or is not `session.size` bytes,
///   before any PUT;
/// - [`GoogleError::MaybeApplied`] on any failure after the first PUT (including the
///   file changing midway). No chunk is retried.
pub async fn upload_chunks(
    client: &GoogleAdsClient,
    session: &UploadSession,
    file: &Path,
    on_progress: impl Fn(u64),
) -> Result<Value, GoogleError> {
    let size = session.size;
    let mut f = tokio::fs::File::open(file)
        .await
        .map_err(|e| file_error(file, "open", e))?;
    let meta = f
        .metadata()
        .await
        .map_err(|e| file_error(file, "read", e))?;
    if !meta.is_file() || meta.len() != size {
        return Err(GoogleError::File(format!(
            "{} changed: it had {size} bytes and now has {}.",
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
            let msg = format!("the file changed during the upload ({e})");
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
                .map_err(|_| interrupted(offset, size, "malformed final response"));
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

/// Reads the upload state every `every` until a final state ([`FINAL_STATES`]) or until
/// `max`; at the deadline, returns the last state read. An upload the search does not
/// find stays `UNSPECIFIED`.
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

/// Body of `youTubeVideoUploads:remove` (snake_case, validated against the real API).
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
