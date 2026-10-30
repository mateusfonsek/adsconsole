//! Ferramentas de mídia: `upload_asset` (imagem, logo, texto ou vídeo do YouTube que
//! já existe; irreversível) e `upload_video` (envia um arquivo de vídeo ao YouTube e
//! cria o asset; reversível com `remove_youtube_upload`).
//!
//! O `upload_video` não usa o `googleAds:mutate` no `validate`: o Google não tem
//! `validate_only` para upload. Ele só confere o arquivo e a entrada; a execução
//! ([`run_upload`]) faz o start, os pedaços, a espera e o asset.

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
const CHECK_UPLOADS: &str = "Confira com gaql_search: SELECT you_tube_video_upload.resource_name, \
     you_tube_video_upload.state FROM you_tube_video_upload.";

// ---------------------------------------------------------------- upload_asset

#[derive(Debug, Deserialize, JsonSchema)]
struct UploadAssetInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// `image` (1.91:1, 1:1 ou 4:5), `logo` (1:1), `text` ou `youtube`.
    kind: String,
    /// Imagem de `image` e `logo`: `{"file_path": "/caminho/absoluto.png"}` ou
    /// `{"url": "https://…"}`.
    #[serde(default)]
    #[schemars(with = "Option<ImageRef>")]
    image: Option<Value>,
    /// Texto de `text` (até 90 caracteres).
    #[serde(default)]
    text: Option<String>,
    /// Id (11 caracteres) ou link (`youtu.be/…`, `youtube.com/watch?v=…`,
    /// `youtube.com/shorts/…`) de um vídeo que já está no YouTube, para `youtube`.
    #[serde(default)]
    youtube_video_id: Option<String>,
    /// Nome do asset (`text` e `youtube`; imagens recebem o nome pelo conteúdo).
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
                "kind desconhecido '{raw}'; use image, logo, text ou youtube."
            ))
            .with_field("kind")),
        }
    }

    /// Campo da entrada que este tipo usa.
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
        Some(ImageKind::Landscape) => "imagem 1.91:1",
        Some(ImageKind::Square) => "imagem 1:1",
        Some(ImageKind::Portrait) => "imagem 4:5",
        Some(ImageKind::Logo) => "logo 1:1",
        Some(ImageKind::LandscapeLogo) => "logo 4:1",
        None => "imagem",
    }
}

struct UploadAsset;

#[async_trait]
impl WriteTool for UploadAsset {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UploadAssetInput>(
            UPLOAD_ASSET,
            "Cria um asset na conta do Google Ads: imagem (1.91:1, 1:1 ou 4:5) ou logo (1:1) \
             de um arquivo ou URL https, texto (até 90 caracteres) ou um vídeo que já está no \
             YouTube (pelo id). Devolve o asset_resource_name para usar em campanhas e grupos \
             de recursos. O Google não remove assets.",
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
                "kind={} usa só {} (e name); tire {other}.",
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
                    invalid("Informe image ({\"file_path\": …} ou {\"url\": …}).")
                        .with_field("image")
                })?;
                if input.name.is_some() {
                    return Err(invalid(
                        "Imagens recebem o nome pelo conteúdo (img-…); tire name.",
                    )
                    .with_field("name"));
                }
                let r = image_ref(raw, "image")?;
                if let ImageRef::Asset { asset } = &r {
                    return Err(invalid(format!(
                        "image: {asset} já é um asset; use-o direto nas campanhas."
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
                    "Cria um asset de {} a partir de {}.",
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
                        "text deve ter de 1 a {TEXT_MAX} caracteres (veio com {n})."
                    ))
                    .with_field("text"));
                }
                let mut body = json!({ "textAsset": { "text": t } });
                if let Some(name) = &input.name {
                    body["name"] = json!(name_text(name, "name")?);
                }
                operations.push(create_op(&temps.next("assets"), body)?);
                format!("Cria um asset de texto '{t}'.")
            }
            AssetKind::Youtube => {
                let raw = input.youtube_video_id.as_deref().ok_or_else(|| {
                    invalid("Informe youtube_video_id.").with_field("youtube_video_id")
                })?;
                let id = youtube_id_or_link(raw, "youtube_video_id")?;
                let mut body = json!({ "youtubeVideoAsset": { "youtubeVideoId": id } });
                if let Some(name) = &input.name {
                    body["name"] = json!(name_text(name, "name")?);
                }
                operations.push(create_op(&temps.next("assets"), body)?);
                format!("Cria um asset do vídeo do YouTube {id}.")
            }
        };
        let asset = operations
            .first()
            .and_then(|op| op["assetOperation"]["create"]["resourceName"].as_str())
            .map(str::to_string)
            .ok_or_else(|| ToolError::new(codes::INTERNAL, "upload_asset sem operação"))?;
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

    /// `{asset_resource_name}`: o nome real do asset (a chave do "depois").
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
    /// Caminho absoluto do vídeo (mp4, mov, avi, wmv, flv, webm, mkv ou mpeg; até 10 GB).
    file_path: String,
    /// Título no YouTube (até 100 caracteres).
    title: String,
    /// Descrição no YouTube (até 5000 caracteres).
    #[serde(default)]
    description: Option<String>,
    /// `UNLISTED` (padrão), `PUBLIC` ou `PRIVATE`.
    #[serde(default)]
    privacy: Option<String>,
    /// Canal do YouTube do anunciante. Sem ele, o vídeo vai para um canal gerenciado
    /// pelo Google.
    #[serde(default)]
    channel_id: Option<String>,
    /// Minutos de espera pelo processamento (padrão 10, máximo 30).
    #[serde(default)]
    wait_minutes: Option<u32>,
}

/// Tamanho e data de modificação do arquivo, para notar troca entre validar e enviar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Envio planejado por `upload_video` ([`DirectExec::YoutubeUpload`]).
pub(crate) struct VideoUpload {
    pub path: PathBuf,
    pub stamp: FileStamp,
    pub title: String,
    pub description: String,
    pub privacy: &'static str,
    pub channel_id: Option<String>,
    /// Espera máxima pelo processamento (`wait_minutes`).
    pub wait: Duration,
}

fn bad_file(msg: impl Into<String>) -> ToolError {
    invalid(msg).with_field("file_path")
}

/// O arquivo de vídeo: caminho absoluto, extensão aceita, arquivo comum, 1 byte a 10 GB.
async fn video_file(raw: &str) -> Result<(PathBuf, FileStamp), ToolError> {
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute() {
        return Err(bad_file(format!(
            "file_path deve ser um caminho absoluto (recebido '{raw}')."
        )));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    if !VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        return Err(bad_file(format!(
            "{}: extensão não aceita; use {}.",
            path.display(),
            VIDEO_EXTENSIONS.join(", ")
        )));
    }
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| bad_file(format!("{}: arquivo não encontrado ({e}).", path.display())))?;
    if !meta.is_file() {
        return Err(bad_file(format!("{} não é um arquivo.", path.display())));
    }
    let size = meta.len();
    if size == 0 || size > MAX_VIDEO_BYTES {
        return Err(bad_file(format!(
            "{}: o vídeo deve ter de 1 byte a 10 GB (tem {size} bytes).",
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
            "{field} deve ter de {min} a {max} caracteres (veio com {n})."
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
            "privacy desconhecida '{raw}'; use UNLISTED, PUBLIC ou PRIVATE."
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
            "channel_id inválido '{raw}'; use o id do canal (ex.: UC…)."
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
            "Envia um arquivo de vídeo ao YouTube pelo Google Ads (não listado por padrão, num \
             canal gerenciado pelo Google ou no channel_id informado), espera o processamento \
             (até wait_minutes) e cria o asset do vídeo. Devolve video_id e \
             asset_resource_name. Desfazer remove o vídeo do YouTube. Durante o upload e o \
             processamento, outras escritas nesta conta ficam esperando.",
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
                "wait_minutes deve ser de 1 a {WAIT_MAX} (veio {wait})."
            ))
            .with_field("wait_minutes"));
        }
        let (path, stamp) = video_file(&input.file_path).await?;
        let place = match &channel_id {
            Some(c) => format!("no canal {c}"),
            None => "num canal gerenciado pelo Google".to_string(),
        };
        let summary = format!(
            "Envia {} ({:.1} MB) ao YouTube como \"{title}\" ({privacy}, {place}) e, quando o \
             processamento terminar (espera até {wait} min), cria o asset do vídeo. O Google \
             não tem validação prévia (validate_only) para upload de vídeo: só as checagens \
             locais foram feitas.",
            path.display(),
            stamp.size as f64 / 1_000_000.0
        );
        Ok(WritePlan {
            summary,
            warning: Some(
                "Desfazer remove o vídeo do YouTube (fica indisponível); o asset do vídeo, se \
                 criado, fica na conta. Durante o upload e o processamento, outras escritas \
                 nesta conta ficam esperando."
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

/// O arquivo do `execute` tem que ser o mesmo do `validate` (a aprovação pode vir
/// horas depois). `seen` é o que o `validate` deste processo viu; sem ele (o processo
/// reiniciou), vale o arquivo atual.
pub(crate) fn check_unchanged(seen: Option<FileStamp>, v: &VideoUpload) -> Result<(), ToolError> {
    match seen {
        Some(s) if s != v.stamp => Err(bad_file(format!(
            "{} mudou desde a validação (tamanho ou data de modificação). Peça o envio de \
             novo para validar o arquivo atual.",
            v.path.display()
        ))),
        _ => Ok(()),
    }
}

/// O que já se sabe do upload `rn`. O "depois" é
/// `{rn: {"you_tube_video_upload.state": estado}, video_id, asset_resource_name, mensagem?}`.
///
/// Estado final ([`FINAL_STATES`]): o alvo observa o estado e repete o resto como fixo
/// ([`TARGET_EXTRA`]). Estado em andamento (prazo estourado, leitura que falhou, envio
/// interrompido): o estado ainda vai mudar sozinho, então nada é observado e o estado
/// também vai no fixo, para o desfazer não acusar conflito por isso.
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
        extra.insert("mensagem".into(), json!(m));
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
        Some(id) => format!("upload_asset kind=youtube e o video_id {id}"),
        None => format!(
            "upload_asset kind=youtube e o video_id (veja com gaql_search: SELECT \
             you_tube_video_upload.video_id FROM you_tube_video_upload WHERE \
             you_tube_video_upload.resource_name = '{rn}')"
        ),
    }
}

/// Executa o [`VideoUpload`]: start, pedaços, espera e asset.
///
/// Depois que o upload tem nome, nunca devolve `Err` (o Engine gravaria `Failed` e o
/// upload ficaria órfão): pedaço que falhou, `FAILED`/`REJECTED`, prazo estourado,
/// leitura do estado que falhou e asset que não foi criado viram `Applied` com a
/// inversa `remove_youtube_upload` e a explicação em `mensagem`. Quando o estado ficou
/// em andamento, ele não é observado (ver [`outcome`]).
///
/// Antes do nome existir (start ou pedaços sem nome), a falha é `Err`; se bytes já
/// tinham sido enviados, é `EXECUTION_FAILED` com a sugestão de conferir os uploads.
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
        tracing::info!(tool, sent, size, "enviando vídeo");
    })
    .await
    {
        Ok(b) => b,
        Err(e) => {
            if let Some(rn) = &session.resource_name {
                let msg = format!("O envio do vídeo falhou: {e} Desfaça para remover o upload.");
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
        tracing::error!(
            tool,
            "vídeo enviado, mas a resposta não trouxe o nome do upload"
        );
        return Ok(Applied {
            after: json!({ "mensagem": format!(
                "O vídeo foi enviado, mas o Google não devolveu o nome do upload. {CHECK_UPLOADS}"
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
                    "O vídeo foi enviado, mas não deu para ler o estado do processamento: {} \
                 Quando terminar, crie o asset com {}.",
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
                "O YouTube não aceitou o vídeo (estado {st}); o asset não foi criado. Desfaça \
                 para remover o upload."
            )
        } else {
            format!(
                "O vídeo ainda está processando. Quando terminar, crie o asset com {}.",
                create_hint(&rn, vid)
            )
        };
        return Ok(outcome(&rn, Some(st), vid, None, Some(msg)));
    }
    let Some(id) = vid else {
        let msg = format!(
            "O vídeo foi processado, mas o Google não informou o video_id. Crie o asset com {}.",
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
                    "O asset do vídeo foi criado, mas a resposta não trouxe o nome; procure o \
                     asset do vídeo {id} com gaql_search."
                )
            });
            Ok(outcome(&rn, Some(st), Some(id), real, msg))
        }
        Err(e) => {
            let e = translate(e, Some(ctx), tool);
            // Talvez aplicado (timeout, 5xx): o asset pode existir.
            let what = if e.code == codes::EXECUTION_FAILED {
                "pode não ter sido criado"
            } else {
                "não foi criado"
            };
            let msg = format!(
                "O vídeo foi processado, mas o asset {what}: {} Se ele não estiver na conta, \
                 crie com {}.",
                e.message,
                create_hint(&rn, Some(id))
            );
            Ok(outcome(&rn, Some(st), Some(id), None, Some(msg)))
        }
    }
}
