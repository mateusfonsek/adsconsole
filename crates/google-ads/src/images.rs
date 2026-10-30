//! Imagens para assets do Google Ads: a referência da entrada ([`ImageRef`]), a carga
//! de arquivo local ou URL ([`load`]), a conferência de proporção e tamanho mínimo
//! ([`check`]) e a operação de criação do asset de imagem ([`image_asset_op`]).
//!
//! Regras da carga:
//! - URL só `https` (também depois de redirecionamentos), sem host local ou privado
//!   (`localhost` e IPs de loopback, rede privada, link-local ou não especificados,
//!   v4 e v6), resposta `2xx` com
//!   `content-type` `image/*`, até [`MAX_IMAGE_BYTES`] (contados durante a leitura,
//!   sem confiar no `Content-Length`) e em até [`URL_TIMEOUT`];
//! - arquivo com caminho absoluto, até [`MAX_IMAGE_BYTES`] (pelo tamanho do arquivo
//!   já aberto, antes de ler);
//! - formatos png, jpeg e gif, detectados pelos bytes; dimensões via `imagesize`.

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

/// Tamanho máximo de uma imagem (5 MB).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Tempo máximo para baixar uma imagem por URL.
pub const URL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Tolerância da proporção (1%).
const RATIO_TOLERANCE: f64 = 0.01;

/// Imagem da entrada: arquivo local, URL `https` ou asset que já existe na conta.
/// Exatamente uma das chaves (`file_path`, `url` ou `asset`).
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum ImageRef {
    File {
        /// Caminho absoluto de um arquivo png, jpeg ou gif (até 5 MB).
        file_path: String,
    },
    Url {
        /// URL `https://` de uma imagem png, jpeg ou gif (até 5 MB).
        url: String,
    },
    Asset {
        /// Nome de um asset de imagem existente (`customers/123/assets/456`).
        asset: String,
    },
}

impl ImageRef {
    /// Leitura estrita: um objeto com exatamente uma das chaves `file_path`, `url` ou
    /// `asset`, com texto não vazio. O erro diz o que está errado (sem o caminho do
    /// campo, que fica com quem chama).
    pub fn from_value(v: &Value) -> Result<Self, String> {
        const KEYS: &str = "file_path, url ou asset";
        let obj = v
            .as_object()
            .ok_or_else(|| format!("esperado um objeto com {KEYS}"))?;
        if let Some(k) = obj
            .keys()
            .find(|k| !matches!(k.as_str(), "file_path" | "url" | "asset"))
        {
            return Err(format!("chave desconhecida '{k}'; use {KEYS}"));
        }
        let mut entries = obj.iter();
        let (key, value) = match (entries.next(), entries.next()) {
            (Some(e), None) => e,
            (None, _) => return Err(format!("informe {KEYS}")),
            (Some(_), Some(_)) => return Err(format!("informe só uma entre {KEYS}")),
        };
        let s = value
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{key} deve ser um texto não vazio"))?
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

/// Imagem carregada, pronta para virar asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// `image/png`, `image/jpeg` ou `image/gif` (pelos bytes, não pelo cabeçalho HTTP).
    pub mime: String,
}

impl LoadedImage {
    /// SHA-256 dos bytes, em hexadecimal.
    pub fn sha256_hex(&self) -> String {
        Sha256::digest(&self.bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Nome do asset pelo conteúdo: `img-<16 primeiros hex do SHA-256>`. Os mesmos
    /// bytes têm sempre o mesmo nome, em qualquer campanha: o Google deduplica imagens
    /// pelo conteúdo e recusa outro nome para os mesmos bytes.
    // Confirmado na conta real (2026-10-18, checklist T14.2): nome por conteúdo, `img-<16hex>`.
    pub fn content_name(&self) -> String {
        format!("img-{}", &self.sha256_hex()[..16])
    }
}

/// Resultado de [`load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// Imagem nova (de arquivo ou URL), a enviar como asset.
    New(LoadedImage),
    /// Asset que já existe; o nome vem da entrada como está (quem chama valida).
    Existing(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("a URL da imagem precisa ser https://")]
    NotHttps,
    #[error("a URL da imagem aponta para um endereço local ou privado ({0})")]
    ForbiddenHost(String),
    #[error("a imagem passa de 5 MB")]
    TooLarge,
    #[error("a URL não devolveu uma imagem (content-type '{0}')")]
    NotImage(String),
    #[error("formato de imagem não aceito; use png, jpeg ou gif")]
    UnsupportedFormat,
    #[error("não foi possível ler as dimensões da imagem")]
    Dimensions,
    #[error("a URL da imagem respondeu HTTP {0}")]
    Http(u16),
    #[error("o download da imagem passou de 30 s")]
    Timeout,
    #[error("falha ao baixar a imagem: {0}")]
    Fetch(String),
    #[error("{0}")]
    File(String),
}

/// Só para os testes, que servem a imagem por `http` local. Em produção, sempre [`load`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowHttp {
    /// Produção: só `https` e host público.
    No,
    /// Só testes: aceita `http` e qualquer host (o `wiremock` é local).
    ForTests,
}

/// Cliente HTTP das imagens: timeout de 30 s e redirecionamentos só para `https`
/// (até 5). Criado uma vez por processo.
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if url_ok(attempt.url(), AllowHttp::No).is_err() {
                attempt.error("redirecionamento para fora de https ou para host local")
            } else if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("redirecionamentos demais")
            } else {
                attempt.follow()
            }
        });
        // Falha só se o backend TLS não inicializar; sem isso nenhuma chamada funcionaria.
        reqwest::Client::builder()
            .timeout(URL_TIMEOUT)
            .redirect(policy)
            .build()
            .expect("cliente HTTP de imagens")
    })
}

/// Carrega a imagem de `r` (arquivo ou URL `https`) ou devolve o asset existente.
pub async fn load(r: &ImageRef, http: &reqwest::Client) -> Result<Loaded, ImageError> {
    load_with(http, r, AllowHttp::No).await
}

/// Como [`load`]; com [`AllowHttp::ForTests`] aceita também `http` (só nos testes).
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

/// IP de loopback, rede privada, link-local, não especificado ou broadcast (v4, e o
/// v4 embutido num v6 mapeado).
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

/// Host aceito: não é `localhost` (nem `*.localhost`) nem IP local ou privado.
/// Nomes que resolvem para IP privado não são conferidos (sem DNS aqui).
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

/// Esquema `https` e host público; com [`AllowHttp::ForTests`], qualquer host e
/// também `http` (o `wiremock` dos testes é local).
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
    // O cliente pode ter seguido redirecionamentos: o destino final também é conferido.
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
            "use o caminho absoluto do arquivo (veio '{raw}')"
        )));
    }
    let not_read =
        |e: std::io::Error| ImageError::File(format!("não foi possível ler '{raw}': {e}"));
    // Abre primeiro e confere o arquivo aberto (não o caminho, que pode mudar).
    let file = tokio::fs::File::open(path).await.map_err(not_read)?;
    let meta = file.metadata().await.map_err(not_read)?;
    if !meta.is_file() {
        return Err(ImageError::File(format!("'{raw}' não é um arquivo")));
    }
    if meta.len() > MAX_IMAGE_BYTES as u64 {
        return Err(ImageError::TooLarge);
    }
    // O arquivo pode crescer depois do metadata: lê no máximo o limite + 1.
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

/// Formato pelos bytes iniciais.
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

/// Formato e dimensões dos bytes de uma imagem.
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

/// Papel da imagem no Google Ads, com a proporção e o tamanho mínimo exigidos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// 1.91:1, mínimo 600x314 (`MARKETING_IMAGE`).
    Landscape,
    /// 1:1, mínimo 300x300 (`SQUARE_MARKETING_IMAGE`).
    Square,
    /// 4:5, mínimo 480x600 (`PORTRAIT_MARKETING_IMAGE`).
    Portrait,
    /// 1:1, mínimo 128x128 (`LOGO`).
    Logo,
    /// 4:1, mínimo 512x128 (`LANDSCAPE_LOGO`).
    LandscapeLogo,
}

impl ImageKind {
    /// (rótulo da proporção, proporção largura/altura, largura mínima, altura mínima).
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

/// Confere a proporção (com 1% de tolerância) e o tamanho mínimo de uma imagem
/// `w`x`h` para o papel `kind`. O erro é uma frase para o usuário.
pub fn check(kind: ImageKind, w: u32, h: u32) -> Result<(), String> {
    let (label, ratio, min_w, min_h) = kind.rule();
    let actual = if h == 0 { 0.0 } else { w as f64 / h as f64 };
    if ((actual - ratio) / ratio).abs() > RATIO_TOLERANCE {
        return Err(format!(
            "a imagem tem {w}x{h}; a proporção deve ser {label} (mínimo {min_w}x{min_h})"
        ));
    }
    if w < min_w || h < min_h {
        return Err(format!(
            "a imagem tem {w}x{h}; o mínimo é {min_w}x{min_h} (proporção {label})"
        ));
    }
    Ok(())
}

/// `MutateOperation` que cria o asset de imagem com o id temporário `temp`.
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
        assert!(ok("https://exemplo.com.br/a.png"));
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
