//! Media: `upload_image` (undone by deleting the image by hash, if it was not in the
//! library before) and `upload_video` (irreversible: Meta does not allow deleting the video via the API).

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

/// The file exists and fits within the limit.
fn check_file(path: &str, max: u64, label: &str) -> Result<PathBuf, ToolError> {
    let p = PathBuf::from(path);
    let meta = std::fs::metadata(&p)
        .map_err(|_| invalid("file_path", format!("File not found: {path}")))?;
    if !meta.is_file() {
        return Err(invalid("file_path", format!("{path} is not a file.")));
    }
    if meta.len() > max {
        return Err(invalid(
            "file_path",
            format!("The file exceeds the {label} limit."),
        ));
    }
    Ok(p)
}

/// Local file name, so the image does not show up in the library as "bytes".
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
    /// Path of a local file.
    #[serde(default)]
    file_path: Option<String>,
    /// https URL of the image (instead of file_path).
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
            "Uploads an image (file or https URL, up to 30 MB) to the account library and \
             returns the image_hash to use in creatives.",
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
                    "Provide file_path or url (exactly one).",
                ));
            }
        };
        let what = match &source {
            ImageSource::File(p) => p.display().to_string(),
            ImageSource::Url(u) => u.clone(),
        };
        Ok(MetaPlan {
            summary: format!("Upload image {what}"),
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
    /// Path of the video file (up to 4 GB).
    file_path: String,
    title: String,
    /// Waits for Meta to finish processing (default true, up to 10 min).
    #[serde(default)]
    wait_ready: Option<bool>,
}

struct UploadVideo;

#[async_trait::async_trait]
impl WriteTool for UploadVideo {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<VideoInput>(
            "upload_video",
            "Uploads a video to the account (in chunks) and waits for Meta to process it; returns \
             the video_id to use in creatives. To delete the video later, use the Ads Manager \
             media library.",
            ToolGroup::Media,
            false,
        )
    }

    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError> {
        let i: VideoInput = parse_input(input)?;
        let path = check_file(&i.file_path, MAX_VIDEO_BYTES, "4 GB")?;
        if i.title.trim().is_empty() {
            return Err(invalid("title", "Give the video a title."));
        }
        Ok(MetaPlan {
            summary: format!("Upload video {} ('{}')", path.display(), i.title.trim()),
            warning: Some("Other writes on this account wait for the upload to finish.".into()),
            direct: Some(DirectExec::VideoUpload {
                path,
                title: i.title.trim().to_string(),
                wait_ready: i.wait_ready.unwrap_or(true),
            }),
            ..MetaPlan::default()
        })
    }
}
