//! Media tools: `upload_asset` (image, logo, text or an existing YouTube video;
//! irreversible) and `upload_video` (uploads a video file to YouTube and creates the
//! asset; reversible with `remove_youtube_upload`).
//!
//! `upload_video` does not use `googleAds:mutate` in `validate`: Google has no
//! `validate_only` for uploads. It only checks the file and the input; the execution
//! ([`run_upload`]) does the start, the chunks, the wait and the asset.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use adsconsole_core::config::ToolGroup;
use adsconsole_core::db::InverseOp;
use adsconsole_google_ads::images::{ImageKind, ImageRef};
use adsconsole_google_ads::resources::TempIds;
use adsconsole_google_ads::upload::{self, FINAL_STATES};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::asset_group::youtube_id_or_link;
use super::create_common::{ImagePool, image_ref, image_source_label, name_text};
use super::keywords::create_op;
use super::reports::invalid;
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, Applied, ToolSpec};
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::google::plan::{
    DirectExec, InverseSpec, REMOVE_YOUTUBE_UPLOAD, TARGET_EXTRA, Watch, WatchTarget, WritePlan,
};

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Write(Box::new(UploadAsset)),
        GTool::Write(Box::new(UploadVideo)),
    ]
}

const UPLOAD_ASSET: &str = "upload_asset";
const UPLOAD_VIDEO: &str = "upload_video";
const TEXT_MAX: usize = 90;
const TITLE_MAX: usize = 100;
const DESCRIPTION_MAX: usize = 5000;
const MAX_VIDEO_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const VIDEO_EXTENSIONS: [&str; 8] = ["mp4", "mov", "avi", "wmv", "flv", "webm", "mkv", "mpeg"];
const WAIT_DEFAULT: u32 = 10;
const WAIT_MAX: u32 = 30;
const STATE_FIELD: &str = "you_tube_video_upload.state";
const CHECK_UPLOADS: &str = "Check with gaql_search: SELECT you_tube_video_upload.resource_name, \
     you_tube_video_upload.state FROM you_tube_video_upload.";

// ---------------------------------------------------------------- upload_asset

#[derive(Debug, Deserialize, JsonSchema)]
struct UploadAssetInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// `image` (1.91:1, 1:1 or 4:5), `logo` (1:1), `text` or `youtube`.
    kind: String,
    /// Image for `image` and `logo`: `{"file_path": "/absolute/path.png"}` or
    /// `{"url": "https://…"}`.
    #[serde(default)]
    #[schemars(with = "Option<ImageRef>")]
    image: Option<Value>,
    /// Text for `text` (up to 90 characters).
    #[serde(default)]
    text: Option<String>,
    /// For `youtube`: id (11 characters) or link (`youtu.be/…`, `youtube.com/watch?v=…`,
    /// `youtube.com/shorts/…`) of a video already on YouTube.
    #[serde(default)]
    youtube_video_id: Option<String>,
    /// Asset name (`text` and `youtube`; images are named after their content).
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Image,
    Logo,
    Text,
    Youtube,
}

impl AssetKind {
    fn parse(raw: &str) -> Result<Self, ToolError> {
        match raw.trim().to_lowercase().as_str() {
            "image" => Ok(AssetKind::Image),
            "logo" => Ok(AssetKind::Logo),
            "text" => Ok(AssetKind::Text),
            "youtube" => Ok(AssetKind::Youtube),
            _ => Err(invalid(format!(
                "unknown kind '{raw}'; use image, logo, text or youtube."
            ))
            .with_field("kind")),
        }
    }

    /// Input field this kind uses.
    fn field(self) -> &'static str {
        match self {
            AssetKind::Image | AssetKind::Logo => "image",
            AssetKind::Text => "text",
            AssetKind::Youtube => "youtube_video_id",
        }
    }
}

fn kind_label(k: Option<ImageKind>) -> &'static str {
    match k {
        Some(ImageKind::Landscape) => "1.91:1 image",
        Some(ImageKind::Square) => "1:1 image",
        Some(ImageKind::Portrait) => "4:5 image",
        Some(ImageKind::Logo) => "1:1 logo",
        Some(ImageKind::LandscapeLogo) => "4:1 logo",
        None => "image",
    }
}

struct UploadAsset;

#[async_trait]
impl WriteTool for UploadAsset {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UploadAssetInput>(
            UPLOAD_ASSET,
            "Creates an asset in the Google Ads account: an image (1.91:1, 1:1 or 4:5) or \
             logo (1:1) from a file or https URL, a text (up to 90 characters) or a video \
             already on YouTube (by id). Returns the asset_resource_name to use in campaigns \
             and asset groups. Google does not remove assets.",
            ToolGroup::Media,
            false,
        )
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: UploadAssetInput = parse_input(input)?;
        let kind = AssetKind::parse(&input.kind)?;
        let given = [
            ("image", input.image.is_some()),
            ("text", input.text.is_some()),
            ("youtube_video_id", input.youtube_video_id.is_some()),
        ];
        if let Some((other, _)) = given.iter().find(|(f, g)| *g && *f != kind.field()) {
            return Err(invalid(format!(
                "kind={} uses only {} (and name); remove {other}.",
                input.kind.trim(),
                kind.field()
            ))
            .with_field("kind"));
        }
        let mut temps = TempIds::new(&ctx.customer_id);
        let mut operations = Vec::new();
        let summary = match kind {
            AssetKind::Image | AssetKind::Logo => {
                let raw = input.image.as_ref().ok_or_else(|| {
                    invalid("Provide image ({\"file_path\": …} or {\"url\": …}).")
                        .with_field("image")
                })?;
                if input.name.is_some() {
                    return Err(invalid(
                        "Images are named after their content (img-…); remove name.",
                    )
                    .with_field("name"));
                }
                let r = image_ref(raw, "image")?;
                if let ImageRef::Asset { asset } = &r {
                    return Err(invalid(format!(
                        "image: {asset} is already an asset; use it directly in campaigns."
                    ))
                    .with_field("image"));
                }
                let mut pool = ImagePool::default();
                let role = if kind == AssetKind::Logo {
                    pool.resolve(
                        ctx,
                        &mut temps,
                        &mut operations,
                        &r,
                        ImageKind::Logo,
                        "image",
                    )
                    .await?;
                    Some(ImageKind::Logo)
                } else {
                    let kinds = [ImageKind::Landscape, ImageKind::Square, ImageKind::Portrait];
                    pool.resolve_fit(ctx, &mut temps, &mut operations, &r, &kinds, "image")
                        .await?
                        .1
                };
                format!(
                    "Create an asset ({}) from {}.",
                    kind_label(role),
                    image_source_label(&r)
                )
            }
            AssetKind::Text => {
                let raw = input.text.as_deref().unwrap_or_default();
                let t = raw.trim();
                let n = t.chars().count();
                if input.text.is_none() || n == 0 || n > TEXT_MAX {
                    return Err(invalid(format!(
                        "text must have 1 to {TEXT_MAX} characters (got {n})."
                    ))
                    .with_field("text"));
                }
                let mut body = json!({ "textAsset": { "text": t } });
                if let Some(name) = &input.name {
                    body["name"] = json!(name_text(name, "name")?);
                }
                operations.push(create_op(&temps.next("assets"), body)?);
                format!("Create text asset '{t}'.")
            }
            AssetKind::Youtube => {
                let raw = input.youtube_video_id.as_deref().ok_or_else(|| {
                    invalid("Provide youtube_video_id.").with_field("youtube_video_id")
                })?;
                let id = youtube_id_or_link(raw, "youtube_video_id")?;
                let mut body = json!({ "youtubeVideoAsset": { "youtubeVideoId": id } });
                if let Some(name) = &input.name {
                    body["name"] = json!(name_text(name, "name")?);
                }
                operations.push(create_op(&temps.next("assets"), body)?);
                format!("Create an asset for YouTube video {id}.")
            }
        };
        let asset = operations
            .first()
            .and_then(|op| op["assetOperation"]["create"]["resourceName"].as_str())
            .map(str::to_string)
            .ok_or_else(|| ToolError::new(codes::INTERNAL, "upload_asset without an operation"))?;
        Ok(WritePlan {
            summary,
            warning: None,
            operations,
            field_changes: Vec::new(),
            watch: vec![Watch {
                resource_name: asset,
                resource: "asset",
                fields: vec!["asset.type"],
            }],
            inverse: InverseSpec::None,
            direct: None,
        })
    }

    /// `{asset_resource_name}`: the real asset name (the key of the "after").
    fn map_applied(&self, a: Applied) -> Applied {
        let rn = a
            .after
            .as_object()
            .and_then(|m| m.keys().find(|k| k.starts_with("customers/")).cloned());
        Applied {
            after: json!({ "asset_resource_name": rn }),
            inverse: a.inverse,
        }
    }
}

// ---------------------------------------------------------------- upload_video

#[derive(Debug, Deserialize, JsonSchema)]
struct UploadVideoInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Absolute path of the video (mp4, mov, avi, wmv, flv, webm, mkv or mpeg; up to 10 GB).
    file_path: String,
    /// YouTube title (up to 100 characters).
    title: String,
    /// YouTube description (up to 5000 characters).
    #[serde(default)]
    description: Option<String>,
    /// `UNLISTED` (default), `PUBLIC` or `PRIVATE`.
    #[serde(default)]
    privacy: Option<String>,
    /// The advertiser's YouTube channel. Without it, the video goes to a channel managed
    /// by Google.
    #[serde(default)]
    channel_id: Option<String>,
    /// Minutes to wait for processing (default 10, maximum 30).
    #[serde(default)]
    wait_minutes: Option<u32>,
}

/// File size and modification time, to notice a swap between validation and upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Upload planned by `upload_video` ([`DirectExec::YoutubeUpload`]).
pub(crate) struct VideoUpload {
    pub path: PathBuf,
    pub stamp: FileStamp,
    pub title: String,
    pub description: String,
    pub privacy: &'static str,
    pub channel_id: Option<String>,
    /// Maximum wait for processing (`wait_minutes`).
    pub wait: Duration,
}

fn bad_file(msg: impl Into<String>) -> ToolError {
    invalid(msg).with_field("file_path")
}

/// The video file: absolute path, accepted extension, regular file, 1 byte to 10 GB.
async fn video_file(raw: &str) -> Result<(PathBuf, FileStamp), ToolError> {
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute() {
        return Err(bad_file(format!(
            "file_path must be an absolute path (got '{raw}')."
        )));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    if !VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        return Err(bad_file(format!(
            "{}: extension not accepted; use {}.",
            path.display(),
            VIDEO_EXTENSIONS.join(", ")
        )));
    }
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| bad_file(format!("{}: file not found ({e}).", path.display())))?;
    if !meta.is_file() {
        return Err(bad_file(format!("{} is not a file.", path.display())));
    }
    let size = meta.len();
    if size == 0 || size > MAX_VIDEO_BYTES {
        return Err(bad_file(format!(
            "{}: the video must be 1 byte to 10 GB (it is {size} bytes).",
            path.display()
        )));
    }
    Ok((
        path,
        FileStamp {
            size,
            modified: meta.modified().ok(),
        },
    ))
}

fn bounded(raw: &str, max: usize, field: &str, required: bool) -> Result<String, ToolError> {
    let t = raw.trim();
    let n = t.chars().count();
    if (required && n == 0) || n > max {
        let min = if required { 1 } else { 0 };
        return Err(invalid(format!(
            "{field} must have {min} to {max} characters (got {n})."
        ))
        .with_field(field));
    }
    Ok(t.to_string())
}

fn privacy(raw: Option<&str>) -> Result<&'static str, ToolError> {
    let Some(raw) = raw else {
        return Ok("UNLISTED");
    };
    match raw.trim().to_uppercase().as_str() {
        "UNLISTED" => Ok("UNLISTED"),
        "PUBLIC" => Ok("PUBLIC"),
        "PRIVATE" => Ok("PRIVATE"),
        _ => Err(invalid(format!(
            "unknown privacy '{raw}'; use UNLISTED, PUBLIC or PRIVATE."
        ))
        .with_field("privacy")),
    }
}

fn channel(raw: &str) -> Result<String, ToolError> {
    let c = raw.trim();
    if c.is_empty()
        || c.len() > 64
        || !c
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(invalid(format!(
            "invalid channel_id '{raw}'; use the channel id (e.g. UC…)."
        ))
        .with_field("channel_id"));
    }
    Ok(c.to_string())
}

struct UploadVideo;

#[async_trait]
impl WriteTool for UploadVideo {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UploadVideoInput>(
            UPLOAD_VIDEO,
            "Uploads a video file to YouTube through Google Ads (unlisted by default, to a \
             channel managed by Google or to the given channel_id), waits for processing (up \
             to wait_minutes) and creates the video asset. Returns video_id and \
             asset_resource_name. Undo removes the video from YouTube. During the upload and \
             processing, other writes to this account wait.",
            ToolGroup::Media,
            true,
        )
    }

    async fn plan(&self, _ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: UploadVideoInput = parse_input(input)?;
        let title = bounded(&input.title, TITLE_MAX, "title", true)?;
        let description = bounded(
            input.description.as_deref().unwrap_or_default(),
            DESCRIPTION_MAX,
            "description",
            false,
        )?;
        let privacy = privacy(input.privacy.as_deref())?;
        let channel_id = input.channel_id.as_deref().map(channel).transpose()?;
        let wait = input.wait_minutes.unwrap_or(WAIT_DEFAULT);
        if !(1..=WAIT_MAX).contains(&wait) {
            return Err(invalid(format!(
                "wait_minutes must be 1 to {WAIT_MAX} (got {wait})."
            ))
            .with_field("wait_minutes"));
        }
        let (path, stamp) = video_file(&input.file_path).await?;
        let place = match &channel_id {
            Some(c) => format!("to channel {c}"),
            None => "to a channel managed by Google".to_string(),
        };
        let summary = format!(
            "Upload {} ({:.1} MB) to YouTube as \"{title}\" ({privacy}, {place}) and, when \
             processing finishes (waiting up to {wait} min), create the video asset. Google \
             has no prior validation (validate_only) for video uploads: only local checks \
             were made.",
            path.display(),
            stamp.size as f64 / 1_000_000.0
        );
        Ok(WritePlan {
            summary,
            warning: Some(
                "Undo removes the video from YouTube (it becomes unavailable); the video \
                 asset, if created, stays in the account. During the upload and processing, \
                 other writes to this account wait."
                    .into(),
            ),
            operations: Vec::new(),
            field_changes: Vec::new(),
            watch: Vec::new(),
            inverse: InverseSpec::None,
            direct: Some(DirectExec::YoutubeUpload(Box::new(VideoUpload {
                path,
                stamp,
                title,
                description,
                privacy,
                channel_id,
                wait: Duration::from_secs(u64::from(wait) * 60),
            }))),
        })
    }
}

/// The `execute` file must be the same as the `validate` one (approval may come hours
/// later). `seen` is what this process's `validate` saw; without it (the process
/// restarted), the current file is used.
pub(crate) fn check_unchanged(seen: Option<FileStamp>, v: &VideoUpload) -> Result<(), ToolError> {
    match seen {
        Some(s) if s != v.stamp => Err(bad_file(format!(
            "{} changed since validation (size or modification time). Request the upload \
             again to validate the current file.",
            v.path.display()
        ))),
        _ => Ok(()),
    }
}

/// What is known so far about upload `rn`. The "after" is
/// `{rn: {"you_tube_video_upload.state": state}, video_id, asset_resource_name, message?}`.
///
/// Final state ([`FINAL_STATES`]): the target watches the state and repeats the rest as
/// fixed values ([`TARGET_EXTRA`]). In-progress state (deadline passed, failed read,
/// interrupted upload): the state will still change on its own, so nothing is watched
/// and the state also goes in the fixed values, so undo doesn't report a conflict for it.
fn outcome(
    rn: &str,
    state: Option<&str>,
    video_id: Option<&str>,
    asset: Option<&str>,
    msg: Option<String>,
) -> Applied {
    let mut extra = Map::new();
    extra.insert("video_id".into(), json!(video_id));
    extra.insert("asset_resource_name".into(), json!(asset));
    if let Some(m) = msg {
        extra.insert("message".into(), json!(m));
    }
    let entry = json!({ STATE_FIELD: state });
    let is_final = state.is_some_and(|s| FINAL_STATES.contains(&s));
    let watch = if is_final {
        vec![WatchTarget {
            resource_name: rn.to_string(),
            resource: "you_tube_video_upload".into(),
            fields: vec![STATE_FIELD.into()],
        }]
    } else {
        extra.insert(rn.to_string(), entry.clone());
        Vec::new()
    };
    let mut after = extra.clone();
    after.insert(rn.to_string(), entry);
    let target = json!({ "watch": watch, TARGET_EXTRA: extra });
    Applied {
        after: Value::Object(after),
        inverse: Some(InverseOp {
            tool: REMOVE_YOUTUBE_UPLOAD.into(),
            input: json!({ "resource_name": rn }),
            target,
        }),
    }
}

fn create_hint(rn: &str, video_id: Option<&str>) -> String {
    match video_id {
        Some(id) => format!("upload_asset kind=youtube and video_id {id}"),
        None => format!(
            "upload_asset kind=youtube and the video_id (find it with gaql_search: SELECT \
             you_tube_video_upload.video_id FROM you_tube_video_upload WHERE \
             you_tube_video_upload.resource_name = '{rn}')"
        ),
    }
}

/// Runs the [`VideoUpload`]: start, chunks, wait and asset.
///
/// Once the upload has a name, it never returns `Err` (the Engine would record `Failed`
/// and the upload would be orphaned): a failed chunk, `FAILED`/`REJECTED`, a passed
/// deadline, a failed state read and an asset that was not created become `Applied`
/// with the `remove_youtube_upload` inverse and the explanation in `message`. When the
/// state is still in progress, it is not watched (see [`outcome`]).
///
/// Before the name exists (start or chunks without a name), a failure is `Err`; if bytes
/// were already sent, it is `EXECUTION_FAILED` with the suggestion to check the uploads.
pub(crate) async fn run_upload(
    ctx: &Ctx,
    tool: &str,
    v: &VideoUpload,
) -> Result<Applied, ToolError> {
    let login = ctx.login.as_deref();
    let session = upload::start_youtube_upload(
        &ctx.client,
        &ctx.customer_id,
        login,
        v.stamp.size,
        &v.title,
        &v.description,
        v.privacy,
        v.channel_id.as_deref(),
    )
    .await
    .map_err(|e| translate(e, Some(ctx), tool))?;
    let size = v.stamp.size;
    let body = match upload::upload_chunks(&ctx.client, &session, &v.path, |sent| {
        tracing::info!(tool, sent, size, "uploading video");
    })
    .await
    {
        Ok(b) => b,
        Err(e) => {
            if let Some(rn) = &session.resource_name {
                let msg = format!("The video upload failed: {e} Undo to remove the upload.");
                return Ok(outcome(rn, None, None, None, Some(msg)));
            }
            let err = translate(e, Some(ctx), tool);
            return Err(if err.code == codes::EXECUTION_FAILED {
                err.with_suggestion(CHECK_UPLOADS)
            } else {
                err
            });
        }
    };
    let Some(rn) = session
        .resource_name
        .clone()
        .or_else(|| upload::upload_resource_name(&body))
    else {
        tracing::error!(tool, "video uploaded but the response has no upload name");
        return Ok(Applied {
            after: json!({ "message": format!(
                "The video was uploaded, but Google did not return the upload name. {CHECK_UPLOADS}"
            ) }),
            inverse: None,
        });
    };
    let every = ctx.upload_poll.every;
    let max = ctx.upload_poll.max.unwrap_or(v.wait);
    let state =
        match upload::poll_upload(&ctx.client, &ctx.customer_id, login, &rn, every, max).await {
            Ok(s) => s,
            Err(e) => {
                let e = translate(e, Some(ctx), tool);
                let msg = format!(
                    "The video was uploaded, but its processing state could not be read: {} \
                 When it finishes, create the asset with {}.",
                    e.message,
                    create_hint(&rn, None)
                );
                return Ok(outcome(&rn, None, None, None, Some(msg)));
            }
        };
    let st = state.state.as_str();
    let vid = state.video_id.as_deref();
    if st != "PROCESSED" {
        let msg = if FINAL_STATES.contains(&st) {
            format!(
                "YouTube did not accept the video (state {st}); the asset was not created. Undo \
                 to remove the upload."
            )
        } else {
            format!(
                "The video is still processing. When it finishes, create the asset with {}.",
                create_hint(&rn, vid)
            )
        };
        return Ok(outcome(&rn, Some(st), vid, None, Some(msg)));
    }
    let Some(id) = vid else {
        let msg = format!(
            "The video was processed, but Google did not return the video_id. Create the asset with {}.",
            create_hint(&rn, None)
        );
        return Ok(outcome(&rn, Some(st), None, None, Some(msg)));
    };
    let asset = TempIds::new(&ctx.customer_id).next("assets");
    let op = create_op(
        &asset,
        json!({ "youtubeVideoAsset": { "youtubeVideoId": id } }),
    )?;
    match ctx
        .client
        .mutate(&ctx.customer_id, login, &[op], false)
        .await
    {
        Ok(responses) => {
            let real = responses
                .first()
                .and_then(|r| r["assetResult"]["resourceName"].as_str());
            let msg = real.is_none().then(|| {
                format!(
                    "The video asset was created, but the response has no name; look for the \
                     asset of video {id} with gaql_search."
                )
            });
            Ok(outcome(&rn, Some(st), Some(id), real, msg))
        }
        Err(e) => {
            let e = translate(e, Some(ctx), tool);
            // Maybe applied (timeout, 5xx): the asset may exist.
            let what = if e.code == codes::EXECUTION_FAILED {
                "may not have been created"
            } else {
                "was not created"
            };
            let msg = format!(
                "The video was processed, but the asset {what}: {} If it is not in the \
                 account, create it with {}.",
                e.message,
                create_hint(&rn, Some(id))
            );
            Ok(outcome(&rn, Some(st), Some(id), None, Some(msg)))
        }
    }
}
