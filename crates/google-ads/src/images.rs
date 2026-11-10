//! Images for Google Ads assets: the input reference ([`ImageRef`]), loading from a
//! local file or URL ([`load`]), the aspect ratio and minimum size check
//! ([`check`]) and the operation that creates the image asset ([`image_asset_op`]).
//!
//! Loading rules:
//! - URL must be `https` (also after redirects), with no local or private host
//!   (`localhost` and loopback, private network, link-local or unspecified IPs,
//!   v4 and v6), a `2xx` response with
//!   `content-type` `image/*`, up to [`MAX_IMAGE_BYTES`] (counted while reading,
//!   without trusting `Content-Length`) and within [`URL_TIMEOUT`];
//! - file with an absolute path, up to [`MAX_IMAGE_BYTES`] (by the size of the
//!   already opened file, before reading);
//! - png, jpeg and gif formats, detected from the bytes; dimensions via `imagesize`.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine as _;
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// Maximum size of an image (5 MB).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Maximum time to download an image from a URL.
pub const URL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Aspect ratio tolerance (1%).
const RATIO_TOLERANCE: f64 = 0.01;

/// Input image: local file, `https` URL or an asset that already exists in the account.
/// Exactly one of the keys (`file_path`, `url` or `asset`).
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum ImageRef {
    File {
        /// Absolute path of a png, jpeg or gif file (up to 5 MB).
        file_path: String,
    },
    Url {
        /// `https://` URL of a png, jpeg or gif image (up to 5 MB).
        url: String,
    },
    Asset {
        /// Name of an existing image asset (`customers/123/assets/456`).
        asset: String,
    },
}

impl ImageRef {
    /// Strict parsing: an object with exactly one of the keys `file_path`, `url` or
    /// `asset`, with non-empty text. The error says what is wrong (without the field
    /// path, which is left to the caller).
    pub fn from_value(v: &Value) -> Result<Self, String> {
        const KEYS: &str = "file_path, url or asset";
        let obj = v
            .as_object()
            .ok_or_else(|| format!("expected an object with {KEYS}"))?;
        if let Some(k) = obj
            .keys()
            .find(|k| !matches!(k.as_str(), "file_path" | "url" | "asset"))
        {
            return Err(format!("unknown key '{k}'; use {KEYS}"));
        }
        let mut entries = obj.iter();
        let (key, value) = match (entries.next(), entries.next()) {
            (Some(e), None) => e,
            (None, _) => return Err(format!("provide {KEYS}")),
            (Some(_), Some(_)) => return Err(format!("provide only one of {KEYS}")),
        };
        let s = value
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{key} must be non-empty text"))?
            .to_string();
        Ok(match key.as_str() {
            "file_path" => ImageRef::File { file_path: s },
            "url" => ImageRef::Url { url: s },
            _ => ImageRef::Asset { asset: s },
        })
    }
}

impl<'de> Deserialize<'de> for ImageRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        ImageRef::from_value(&v).map_err(D::Error::custom)
    }
}

/// Loaded image, ready to become an asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// `image/png`, `image/jpeg` or `image/gif` (from the bytes, not the HTTP header).
    pub mime: String,
}

impl LoadedImage {
    /// SHA-256 of the bytes, in hexadecimal.
    pub fn sha256_hex(&self) -> String {
        Sha256::digest(&self.bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Asset name from the content: `img-<first 16 hex of the SHA-256>`. The same
    /// bytes always get the same name, in any campaign: Google deduplicates images
    /// by content and rejects another name for the same bytes.
    // Confirmed against a real account (2026-10-18): content-based name, `img-<16hex>`.
    pub fn content_name(&self) -> String {
        format!("img-{}", &self.sha256_hex()[..16])
    }
}

/// Result of [`load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// New image (from a file or URL), to upload as an asset.
    New(LoadedImage),
    /// Asset that already exists; the name comes from the input as is (the caller validates it).
    Existing(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("the image URL must be https://")]
    NotHttps,
    #[error("the image URL points to a local or private address ({0})")]
    ForbiddenHost(String),
    #[error("the image is larger than 5 MB")]
    TooLarge,
    #[error("the URL did not return an image (content-type '{0}')")]
    NotImage(String),
    #[error("unsupported image format; use png, jpeg or gif")]
    UnsupportedFormat,
    #[error("could not read the image dimensions")]
    Dimensions,
    #[error("the image URL responded with HTTP {0}")]
    Http(u16),
    #[error("the image download took longer than 30 s")]
    Timeout,
    #[error("failed to download the image: {0}")]
    Fetch(String),
    #[error("{0}")]
    File(String),
}

/// Only for tests, which serve the image over local `http`. In production, always use [`load`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowHttp {
    /// Production: only `https` and a public host.
    No,
    /// Tests only: accepts `http` and any host (`wiremock` is local).
    ForTests,
}

/// HTTP client for images: 30 s timeout and redirects only to `https`
/// (up to 5). Created once per process.
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if url_ok(attempt.url(), AllowHttp::No).is_err() {
                attempt.error("redirect away from https or to a local host")
            } else if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        });
        // Fails only if the TLS backend cannot initialize; no call would work without it.
        reqwest::Client::builder()
            .timeout(URL_TIMEOUT)
            .redirect(policy)
            .build()
            .expect("image HTTP client")
    })
}

/// Loads the image from `r` (file or `https` URL) or returns the existing asset.
pub async fn load(r: &ImageRef, http: &reqwest::Client) -> Result<Loaded, ImageError> {
    load_with(http, r, AllowHttp::No).await
}

/// Like [`load`]; with [`AllowHttp::ForTests`] it also accepts `http` (tests only).
pub async fn load_with(
    http: &reqwest::Client,
    r: &ImageRef,
    allow: AllowHttp,
) -> Result<Loaded, ImageError> {
    match r {
        ImageRef::Asset { asset } => Ok(Loaded::Existing(asset.clone())),
        ImageRef::File { file_path } => Ok(Loaded::New(decode(read_file(file_path).await?)?)),
        ImageRef::Url { url } => Ok(Loaded::New(decode(fetch(http, url, allow).await?)?)),
    }
}

/// Loopback, private-network, link-local, unspecified or broadcast IP (v4, and the
/// v4 embedded in a mapped v6).
fn forbidden_ip(ip: IpAddr) -> bool {
    let v4 = |a: Ipv4Addr| {
        a.is_loopback()
            || a.is_private()
            || a.is_link_local()
            || a.is_unspecified()
            || a.is_broadcast()
    };
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => {
            a.is_loopback()
                || a.is_unspecified()
                || a.is_unique_local()
                || a.is_unicast_link_local()
                || a.to_ipv4_mapped().is_some_and(v4)
        }
    }
}

/// Accepted host: not `localhost` (nor `*.localhost`) and not a local or private IP.
/// Names that resolve to a private IP are not checked (no DNS here).
fn host_ok(url: &url::Url) -> Result<(), ImageError> {
    let forbidden = match url.host() {
        None => true,
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(a)) => forbidden_ip(IpAddr::V4(a)),
        Some(url::Host::Ipv6(a)) => forbidden_ip(IpAddr::V6(a)),
    };
    if forbidden {
        return Err(ImageError::ForbiddenHost(
            url.host_str().unwrap_or_default().to_string(),
        ));
    }
    Ok(())
}

/// `https` scheme and public host; with [`AllowHttp::ForTests`], any host and
/// also `http` (the tests' `wiremock` is local).
fn url_ok(url: &url::Url, allow: AllowHttp) -> Result<(), ImageError> {
    match allow {
        AllowHttp::No if url.scheme() == "https" => host_ok(url),
        AllowHttp::ForTests if matches!(url.scheme(), "https" | "http") => Ok(()),
        _ => Err(ImageError::NotHttps),
    }
}

async fn fetch(http: &reqwest::Client, raw: &str, allow: AllowHttp) -> Result<Vec<u8>, ImageError> {
    let url = url::Url::parse(raw).map_err(|_| ImageError::NotHttps)?;
    url_ok(&url, allow)?;
    let map = |e: reqwest::Error| {
        if e.is_timeout() {
            ImageError::Timeout
        } else {
            ImageError::Fetch(e.without_url().to_string())
        }
    };
    let mut resp = http
        .get(url)
        .timeout(URL_TIMEOUT)
        .send()
        .await
        .map_err(map)?;
    // The client may have followed redirects: the final destination is checked too.
    url_ok(resp.url(), allow)?;
    if !resp.status().is_success() {
        return Err(ImageError::Http(resp.status().as_u16()));
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !ct.trim().to_ascii_lowercase().starts_with("image/") {
        return Err(ImageError::NotImage(ct));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(map)? {
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_file(raw: &str) -> Result<Vec<u8>, ImageError> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(ImageError::File(format!(
            "use the absolute path of the file (got '{raw}')"
        )));
    }
    let not_read = |e: std::io::Error| ImageError::File(format!("could not read '{raw}': {e}"));
    // Open first and check the opened file (not the path, which can change).
    let file = tokio::fs::File::open(path).await.map_err(not_read)?;
    let meta = file.metadata().await.map_err(not_read)?;
    if !meta.is_file() {
        return Err(ImageError::File(format!("'{raw}' is not a file")));
    }
    if meta.len() > MAX_IMAGE_BYTES as u64 {
        return Err(ImageError::TooLarge);
    }
    // The file can grow after the metadata call: read at most the limit + 1.
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(not_read)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge);
    }
    Ok(bytes)
}

/// Format from the leading bytes.
fn mime_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

/// Format and dimensions of an image's bytes.
pub fn decode(bytes: Vec<u8>) -> Result<LoadedImage, ImageError> {
    let mime = mime_of(&bytes).ok_or(ImageError::UnsupportedFormat)?;
    let size = imagesize::blob_size(&bytes).map_err(|_| ImageError::Dimensions)?;
    let width = u32::try_from(size.width).map_err(|_| ImageError::Dimensions)?;
    let height = u32::try_from(size.height).map_err(|_| ImageError::Dimensions)?;
    Ok(LoadedImage {
        bytes,
        width,
        height,
        mime: mime.to_string(),
    })
}

/// Role of the image in Google Ads, with the required aspect ratio and minimum size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// 1.91:1, minimum 600x314 (`MARKETING_IMAGE`).
    Landscape,
    /// 1:1, minimum 300x300 (`SQUARE_MARKETING_IMAGE`).
    Square,
    /// 4:5, minimum 480x600 (`PORTRAIT_MARKETING_IMAGE`).
    Portrait,
    /// 1:1, minimum 128x128 (`LOGO`).
    Logo,
    /// 4:1, minimum 512x128 (`LANDSCAPE_LOGO`).
    LandscapeLogo,
}

impl ImageKind {
    /// (ratio label, width/height ratio, minimum width, minimum height).
    fn rule(self) -> (&'static str, f64, u32, u32) {
        match self {
            ImageKind::Landscape => ("1.91:1", 1.91, 600, 314),
            ImageKind::Square => ("1:1", 1.0, 300, 300),
            ImageKind::Portrait => ("4:5", 0.8, 480, 600),
            ImageKind::Logo => ("1:1", 1.0, 128, 128),
            ImageKind::LandscapeLogo => ("4:1", 4.0, 512, 128),
        }
    }
}

/// Checks the aspect ratio (with 1% tolerance) and the minimum size of a `w`x`h`
/// image for the role `kind`. The error is a sentence meant for the user.
pub fn check(kind: ImageKind, w: u32, h: u32) -> Result<(), String> {
    let (label, ratio, min_w, min_h) = kind.rule();
    let actual = if h == 0 { 0.0 } else { w as f64 / h as f64 };
    if ((actual - ratio) / ratio).abs() > RATIO_TOLERANCE {
        return Err(format!(
            "the image is {w}x{h}; the aspect ratio must be {label} (minimum {min_w}x{min_h})"
        ));
    }
    if w < min_w || h < min_h {
        return Err(format!(
            "the image is {w}x{h}; the minimum is {min_w}x{min_h} (aspect ratio {label})"
        ));
    }
    Ok(())
}

/// `MutateOperation` that creates the image asset with the temporary id `temp`.
pub fn image_asset_op(temp: &str, name: &str, img: &LoadedImage) -> Value {
    json!({ "assetOperation": { "create": {
        "resourceName": temp,
        "name": name,
        "type": "IMAGE",
        "imageAsset": { "data": base64::engine::general_purpose::STANDARD.encode(&img.bytes) },
    } } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_hosts_pass_and_local_ones_do_not() {
        let ok = |u: &str| host_ok(&url::Url::parse(u).unwrap()).is_ok();
        assert!(ok("https://example.com/a.png"));
        assert!(ok("https://8.8.8.8/a.png"));
        assert!(ok("https://[2001:4860:4860::8888]/a.png"));
        for bad in [
            "https://localhost/a.png",
            "https://LOCALHOST./a.png",
            "https://api.localhost/a.png",
            "https://127.0.0.1/a.png",
            "https://10.1.2.3/a.png",
            "https://172.16.0.1/a.png",
            "https://192.168.0.10/a.png",
            "https://169.254.169.254/a.png",
            "https://0.0.0.0/a.png",
            "https://255.255.255.255/a.png",
            "https://[::1]/a.png",
            "https://[::]/a.png",
            "https://[fe80::1]/a.png",
            "https://[fd00::1]/a.png",
            "https://[::ffff:10.0.0.1]/a.png",
        ] {
            assert!(!ok(bad), "{bad}");
        }
    }
}
