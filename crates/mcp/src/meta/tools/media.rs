//! Mídia: `upload_image` (desfeito apagando a imagem pelo hash, se ela não estava na
//! biblioteca antes) e `upload_video` (irreversível: a Meta não deixa apagar o vídeo pela API).

use std::path::PathBuf;

use adsconsole_core::config::ToolGroup;
use adsconsole_meta_ads::upload::{ImageSource, MAX_IMAGE_BYTES, MAX_VIDEO_BYTES, check_image_url};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{MTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::meta::ctx::Ctx;
use crate::meta::plan::{DirectExec, MetaPlan};

pub(crate) fn tools() -> Vec<MTool> {
    vec![
        MTool::Write(Box::new(UploadImage)),
        MTool::Write(Box::new(UploadVideo)),
    ]
}

fn invalid(field: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

/// Arquivo existe e cabe no limite.
fn check_file(path: &str, max: u64, label: &str) -> Result<PathBuf, ToolError> {
    let p = PathBuf::from(path);
    let meta = std::fs::metadata(&p)
        .map_err(|_| invalid("file_path", format!("Arquivo não encontrado: {path}")))?;
    if !meta.is_file() {
        return Err(invalid("file_path", format!("{path} não é um arquivo.")));
    }
    if meta.len() > max {
        return Err(invalid(
            "file_path",
            format!("O arquivo passa do limite de {label}."),
        ));
    }
    Ok(p)
}

/// Nome do arquivo local, para a imagem não aparecer na biblioteca como "bytes".
fn file_name(source: &ImageSource) -> Option<String> {
    match source {
        ImageSource::File(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
        ImageSource::Url(_) => None,
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImageInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Caminho de um arquivo local.
    #[serde(default)]
    file_path: Option<String>,
    /// URL https da imagem (no lugar de file_path).
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

struct UploadImage;

#[async_trait::async_trait]
impl WriteTool for UploadImage {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ImageInput>(
            "upload_image",
            "Sobe uma imagem (arquivo ou URL https, até 30 MB) para a biblioteca da conta e \
             devolve o image_hash para usar nos criativos.",
            ToolGroup::Media,
            true,
        )
    }

    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: ImageInput = parse_input(input)?;
        let source = match (i.file_path.as_deref(), i.url.as_deref()) {
            (Some(p), None) => ImageSource::File(check_file(p, MAX_IMAGE_BYTES, "30 MB")?),
            (None, Some(u)) => {
                check_image_url(u).map_err(|e| invalid("url", e))?;
                ImageSource::Url(u.to_string())
            }
            _ => {
                return Err(invalid(
                    "file_path",
                    "Informe file_path ou url (um dos dois).",
                ));
            }
        };
        let what = match &source {
            ImageSource::File(p) => p.display().to_string(),
            ImageSource::Url(u) => u.clone(),
        };
        Ok(MetaPlan {
            summary: format!("Subir a imagem {what}"),
            direct: Some(DirectExec::ImageUpload {
                name: i.name.or_else(|| file_name(&source)),
                source,
            }),
            ..MetaPlan::default()
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct VideoInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Caminho do arquivo de vídeo (até 4 GB).
    file_path: String,
    title: String,
    /// Espera a Meta terminar de processar (padrão true, até 10 min).
    #[serde(default)]
    wait_ready: Option<bool>,
}

struct UploadVideo;

#[async_trait::async_trait]
impl WriteTool for UploadVideo {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<VideoInput>(
            "upload_video",
            "Sobe um vídeo para a conta (em partes) e espera a Meta processar; devolve o \
             video_id para usar nos criativos. Para apagar o vídeo depois, use a biblioteca de \
             mídia do Gerenciador de Anúncios.",
            ToolGroup::Media,
            false,
        )
    }

    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: VideoInput = parse_input(input)?;
        let path = check_file(&i.file_path, MAX_VIDEO_BYTES, "4 GB")?;
        if i.title.trim().is_empty() {
            return Err(invalid("title", "Dê um título ao vídeo."));
        }
        Ok(MetaPlan {
            summary: format!("Subir o vídeo {} ('{}')", path.display(), i.title.trim()),
            warning: Some("Outras escritas nesta conta esperam o envio terminar.".into()),
            direct: Some(DirectExec::VideoUpload {
                path,
                title: i.title.trim().to_string(),
                wait_ready: i.wait_ready.unwrap_or(true),
            }),
            ..MetaPlan::default()
        })
    }
}
