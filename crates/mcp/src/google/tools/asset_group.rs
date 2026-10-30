//! `update_asset_group`: edita um grupo de recursos (asset group) da Performance Max.
//!
//! - nome, status e URLs finais viram `FieldChange` (via [`Edits`]);
//! - ativos e sinais novos são criados no mesmo mutate (texto novo = `asset` +
//!   `asset_group_asset`) e a inversa é `RemoveCreated` dos vínculos e sinais;
//! - remover ativo ou sinal é irreversível. Cada nome precisa existir neste grupo (e
//!   não estar removido), senão `NOT_FOUND` e nada é enviado;
//! - os mínimos e máximos de cada tipo são conferidos contra as contagens atuais.
//!
//! Editar campos e criar itens na mesma chamada não é aceito: a inversa seria dupla.
//! Com remoções junto, vale a regra do irreversível.
//!
//! Imagens: asset existente, arquivo ou URL https. Arquivo e URL são lidos só depois
//! de todas as checagens locais e das leituras do grupo, num [`ImagePool`] (os mesmos
//! bytes viram um asset só, com o nome pelo conteúdo).

use std::collections::HashSet;

use adsconsole_core::config::ToolGroup;
use adsconsole_google_ads::images::{ImageKind, ImageRef};
use adsconsole_google_ads::resources::{self, TempIds};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::create_common::{ImageIn, ImagePool, image_in, image_source_label};
use super::edit::{EditStatus, Edits, Show, final_urls, read_one};
use super::keywords::{IRREVERSIBLE_WARNING, create_op, criterion_temp};
use super::reports::{invalid, text, valid_id};
use super::{GTool, WriteTool, own_resource, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![GTool::Write(Box::new(UpdateAssetGroup))]
}

const UPDATE_ASSET_GROUP: &str = "update_asset_group";
const SUMMARY_ITEMS: usize = 5;
pub(crate) const SEARCH_THEME_MAX: usize = 80;

// ------------------------------------------------------------------ tipos

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldType {
    Headline,
    LongHeadline,
    Description,
    MarketingImage,
    SquareMarketingImage,
    PortraitMarketingImage,
    Logo,
    LandscapeLogo,
    YoutubeVideo,
}

const ALL_TYPES: [FieldType; 9] = [
    FieldType::Headline,
    FieldType::LongHeadline,
    FieldType::Description,
    FieldType::MarketingImage,
    FieldType::SquareMarketingImage,
    FieldType::PortraitMarketingImage,
    FieldType::Logo,
    FieldType::LandscapeLogo,
    FieldType::YoutubeVideo,
];

impl FieldType {
    fn parse(raw: &str, field: &str) -> Result<Self, ToolError> {
        let up = raw.trim().to_uppercase();
        ALL_TYPES
            .into_iter()
            .find(|t| t.api() == up)
            .ok_or_else(|| {
                invalid(format!(
                    "{field}: tipo de ativo desconhecido '{raw}'; use HEADLINE, LONG_HEADLINE, \
                     DESCRIPTION, MARKETING_IMAGE, SQUARE_MARKETING_IMAGE, \
                     PORTRAIT_MARKETING_IMAGE, LOGO, LANDSCAPE_LOGO ou YOUTUBE_VIDEO."
                ))
                .with_field(field)
            })
    }

    pub(crate) fn api(self) -> &'static str {
        match self {
            FieldType::Headline => "HEADLINE",
            FieldType::LongHeadline => "LONG_HEADLINE",
            FieldType::Description => "DESCRIPTION",
            FieldType::MarketingImage => "MARKETING_IMAGE",
            FieldType::SquareMarketingImage => "SQUARE_MARKETING_IMAGE",
            FieldType::PortraitMarketingImage => "PORTRAIT_MARKETING_IMAGE",
            FieldType::Logo => "LOGO",
            FieldType::LandscapeLogo => "LANDSCAPE_LOGO",
            FieldType::YoutubeVideo => "YOUTUBE_VIDEO",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            FieldType::Headline => "título(s)",
            FieldType::LongHeadline => "título(s) longo(s)",
            FieldType::Description => "descrição(ões)",
            FieldType::MarketingImage => "imagem(ns) 1.91:1",
            FieldType::SquareMarketingImage => "imagem(ns) 1:1",
            FieldType::PortraitMarketingImage => "imagem(ns) 4:5",
            FieldType::Logo => "logo(s) 1:1",
            FieldType::LandscapeLogo => "logo(s) 4:1",
            FieldType::YoutubeVideo => "vídeo(s)",
        }
    }

    /// Texto máximo (em caracteres) dos tipos de texto.
    pub(crate) fn text_max(self) -> Option<usize> {
        match self {
            FieldType::Headline => Some(30),
            FieldType::LongHeadline | FieldType::Description => Some(90),
            _ => None,
        }
    }

    /// Papel da imagem nos tipos de imagem (proporção e tamanho mínimo).
    fn image_kind(self) -> Option<ImageKind> {
        match self {
            FieldType::MarketingImage => Some(ImageKind::Landscape),
            FieldType::SquareMarketingImage => Some(ImageKind::Square),
            FieldType::PortraitMarketingImage => Some(ImageKind::Portrait),
            FieldType::Logo => Some(ImageKind::Logo),
            FieldType::LandscapeLogo => Some(ImageKind::LandscapeLogo),
            _ => None,
        }
    }

    pub(crate) fn min(self) -> usize {
        match self {
            FieldType::Headline => 3,
            FieldType::Description => 2,
            FieldType::LongHeadline
            | FieldType::MarketingImage
            | FieldType::SquareMarketingImage => 1,
            _ => 0,
        }
    }

    pub(crate) fn max(self) -> Option<usize> {
        match self {
            FieldType::Headline => Some(15),
            FieldType::LongHeadline | FieldType::Description => Some(5),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AssetIn {
    /// `HEADLINE`, `LONG_HEADLINE`, `DESCRIPTION`, `MARKETING_IMAGE` (1.91:1),
    /// `SQUARE_MARKETING_IMAGE` (1:1), `PORTRAIT_MARKETING_IMAGE` (4:5), `LOGO`,
    /// `LANDSCAPE_LOGO` ou `YOUTUBE_VIDEO`.
    field_type: String,
    /// Texto novo (só `HEADLINE`, `LONG_HEADLINE` e `DESCRIPTION`).
    #[serde(default)]
    text: Option<String>,
    /// Imagem (só tipos de imagem): `{"asset": "customers/…/assets/…"}`,
    /// `{"file_path": "/caminho/absoluto.png"}` ou `{"url": "https://…"}`.
    #[serde(default)]
    #[schemars(with = "Option<ImageRef>")]
    image: Option<Value>,
    /// Id do vídeo do YouTube (só `YOUTUBE_VIDEO`) ou um link (`youtu.be/…`,
    /// `youtube.com/watch?v=…`, `youtube.com/shorts/…`).
    #[serde(default)]
    youtube_video_id: Option<String>,
    /// Nome de um asset que já existe na conta, de qualquer tipo.
    #[serde(default)]
    asset: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct SignalsIn {
    /// Nomes de público (`customers/123/audiences/456`).
    #[serde(default)]
    audiences: Option<Vec<String>>,
    /// Temas de busca (texto livre).
    #[serde(default)]
    search_themes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateAssetGroupInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    asset_group_id: String,
    #[serde(default)]
    name: Option<String>,
    /// `ACTIVE` ou `PAUSED`.
    #[serde(default)]
    status: Option<EditStatus>,
    #[serde(default)]
    final_urls: Option<Vec<String>>,
    /// Ativos a incluir (textos novos viram assets criados junto).
    #[serde(default)]
    add_assets: Option<Vec<AssetIn>>,
    /// Nomes de `asset_group_asset` deste grupo (`gaql_search` em `asset_group_asset`).
    #[serde(default)]
    remove_assets: Option<Vec<String>>,
    /// Sinais de público e temas de busca a incluir.
    #[serde(default)]
    add_signals: Option<SignalsIn>,
    /// Nomes de `asset_group_signal` deste grupo.
    #[serde(default)]
    remove_signals: Option<Vec<String>>,
}

/// De onde vem o ativo a incluir.
enum Source {
    NewText(String),
    NewVideo(String),
    /// Arquivo ou URL, carregado só na montagem das operações.
    NewImage(ImageIn),
    Existing(String),
}

struct AddAsset {
    field_type: FieldType,
    source: Source,
}

/// Vínculo atual do grupo.
struct GroupAsset {
    rn: String,
    field_type: Option<FieldType>,
    text: Option<String>,
}

// -------------------------------------------------------------- validação

/// Imagem da entrada (`field` = `add_assets[i].image`), lida sem carregar nada: asset
/// existente (desta conta) ou arquivo/URL para carregar depois, no papel do tipo.
fn image_source(img: &Value, ctx: &Ctx, kind: ImageKind, field: &str) -> Result<Source, ToolError> {
    let parsed = image_in(ctx, img, kind, field.to_string())?;
    Ok(match parsed.r {
        ImageRef::Asset { asset } => {
            Source::Existing(asset_rn(ctx, &asset, &format!("{field}.asset"))?)
        }
        _ => Source::NewImage(parsed),
    })
}

pub(crate) fn collection_rn(
    ctx: &Ctx,
    raw: &str,
    collection: &str,
    field: &str,
) -> Result<String, ToolError> {
    let raw = raw.trim();
    own_resource(ctx, raw).map_err(|e| e.with_field(field))?;
    let prefix = format!("customers/{}/{collection}/", ctx.customer_id);
    match raw.strip_prefix(&prefix) {
        Some(id) if !id.is_empty() && !id.contains('/') => Ok(raw.to_string()),
        _ => Err(invalid(format!(
            "{field}: esperado um nome no formato {prefix}ID (recebido '{raw}')."
        ))
        .with_field(field)),
    }
}

pub(crate) fn asset_rn(ctx: &Ctx, raw: &str, field: &str) -> Result<String, ToolError> {
    let rn = collection_rn(ctx, raw, "assets", field)?;
    valid_id(rn.rsplit('/').next().unwrap_or_default(), field)?;
    Ok(rn)
}

fn parse_add(ctx: &Ctx, a: &AssetIn, i: usize) -> Result<AddAsset, ToolError> {
    let f = format!("add_assets[{i}]");
    let field_type = FieldType::parse(&a.field_type, &format!("{f}.field_type"))?;
    let given = [
        a.text.is_some(),
        a.image.is_some(),
        a.youtube_video_id.is_some(),
        a.asset.is_some(),
    ]
    .iter()
    .filter(|g| **g)
    .count();
    if given != 1 {
        return Err(invalid(format!(
            "{f}: informe exatamente um entre text, image, youtube_video_id e asset."
        ))
        .with_field(f));
    }
    let source = if let Some(t) = &a.text {
        let max = field_type.text_max().ok_or_else(|| {
            invalid(format!("{f}.text: {} não aceita texto.", field_type.api()))
                .with_field(format!("{f}.text"))
        })?;
        let t = t.trim();
        let n = t.chars().count();
        if n == 0 || n > max {
            return Err(invalid(format!(
                "{f}.text deve ter de 1 a {max} caracteres (veio com {n})."
            ))
            .with_field(format!("{f}.text")));
        }
        Source::NewText(t.to_string())
    } else if let Some(img) = &a.image {
        let Some(kind) = field_type.image_kind() else {
            return Err(invalid(format!(
                "{f}.image: {} não é um tipo de imagem.",
                field_type.api()
            ))
            .with_field(format!("{f}.image")));
        };
        image_source(img, ctx, kind, &format!("{f}.image"))?
    } else if let Some(v) = &a.youtube_video_id {
        if field_type != FieldType::YoutubeVideo {
            return Err(
                invalid(format!("{f}.youtube_video_id só vale para YOUTUBE_VIDEO."))
                    .with_field(format!("{f}.youtube_video_id")),
            );
        }
        Source::NewVideo(youtube_id_or_link(v, &format!("{f}.youtube_video_id"))?)
    } else {
        let raw = a.asset.as_deref().unwrap_or_default();
        Source::Existing(asset_rn(ctx, raw, &format!("{f}.asset"))?)
    };
    Ok(AddAsset { field_type, source })
}

/// Id de vídeo do YouTube: 11 caracteres (letras, dígitos, `_` e `-`).
pub(crate) fn youtube_id(raw: &str, field: &str) -> Result<String, ToolError> {
    let v = raw.trim();
    if v.len() != 11
        || !v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(invalid(format!(
            "{field} deve ter 11 caracteres (o id do vídeo, não a URL)."
        ))
        .with_field(field));
    }
    Ok(v.to_string())
}

/// Como [`youtube_id`], aceitando também um link do YouTube (`youtu.be/<id>`,
/// `youtube.com/watch?v=<id>` ou `youtube.com/shorts/<id>`), de onde o id é tirado.
pub(crate) fn youtube_id_or_link(raw: &str, field: &str) -> Result<String, ToolError> {
    let v = raw.trim();
    let Some(id) = youtube_link_id(v) else {
        return youtube_id(v, field);
    };
    youtube_id(&id, field).map_err(|_| {
        invalid(format!(
            "{field}: o link não traz um id de vídeo de 11 caracteres."
        ))
        .with_field(field)
    })
}

/// Id do vídeo num link do YouTube; `None` se não é um link do YouTube.
fn youtube_link_id(raw: &str) -> Option<String> {
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else if raw.starts_with("youtu") || raw.starts_with("www.") || raw.starts_with("m.") {
        format!("https://{raw}")
    } else {
        return None;
    };
    let url = url::Url::parse(&with_scheme).ok()?;
    let host = url
        .host_str()?
        .trim_start_matches("www.")
        .trim_start_matches("m.");
    let mut segs = url.path_segments()?.filter(|s| !s.is_empty());
    match host {
        "youtu.be" => Some(segs.next().unwrap_or_default().to_string()),
        "youtube.com" => match segs.next() {
            Some("watch") => Some(
                url.query_pairs()
                    .find(|(k, _)| k == "v")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default(),
            ),
            Some("shorts") => Some(segs.next().unwrap_or_default().to_string()),
            _ => Some(String::new()),
        },
        _ => None,
    }
}

/// Vínculo `rn` (`{grupo}~{asset}~{tipo}`) que já existe no grupo ou já veio nesta
/// chamada: a mensagem diz qual dos dois.
fn link_taken(rn: &str, current: &[GroupAsset], ft: &str, repeated: String) -> ToolError {
    let msg = if current.iter().any(|a| a.rn == rn) {
        format!("Este asset já está no asset group como {ft}.")
    } else {
        repeated
    };
    invalid(msg).with_field("add_assets")
}

/// Lista sem repetição de nomes de uma coleção.
fn parse_rns(
    ctx: &Ctx,
    raw: &[String],
    collection: &str,
    field: &str,
) -> Result<Vec<String>, ToolError> {
    let mut out: Vec<String> = Vec::new();
    for (i, r) in raw.iter().enumerate() {
        let f = format!("{field}[{i}]");
        let rn = collection_rn(ctx, r, collection, &f)?;
        if out.contains(&rn) {
            return Err(invalid(format!("{rn} aparece mais de uma vez.")).with_field(f));
        }
        out.push(rn);
    }
    Ok(out)
}

pub(crate) fn list(items: &[String]) -> String {
    let mut out = items
        .iter()
        .take(SUMMARY_ITEMS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SUMMARY_ITEMS {
        out.push_str(&format!(", e mais {}", items.len() - SUMMARY_ITEMS));
    }
    out
}

fn search(ctx: &Ctx) -> impl Fn(adsconsole_google_ads::error::GoogleError) -> ToolError + '_ {
    move |e| translate(e, Some(ctx), UPDATE_ASSET_GROUP)
}

// ------------------------------------------------------------------ leituras

async fn group_assets(ctx: &Ctx, id: &str) -> Result<Vec<GroupAsset>, ToolError> {
    let query = format!(
        "SELECT asset_group_asset.resource_name, asset_group_asset.field_type, \
         asset.text_asset.text FROM asset_group_asset \
         WHERE asset_group.id = {id} AND asset_group_asset.status != 'REMOVED'"
    );
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
        .await
        .map_err(search(ctx))?;
    Ok(page
        .results
        .iter()
        .filter_map(|r| {
            Some(GroupAsset {
                rn: text(r, "assetGroupAsset.resourceName")?,
                field_type: text(r, "assetGroupAsset.fieldType")
                    .and_then(|t| FieldType::parse(&t, "").ok()),
                text: text(r, "asset.textAsset.text"),
            })
        })
        .collect())
}

async fn group_signals(ctx: &Ctx, id: &str) -> Result<Vec<String>, ToolError> {
    let query = format!(
        "SELECT asset_group_signal.resource_name FROM asset_group_signal \
         WHERE asset_group.id = {id}"
    );
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
        .await
        .map_err(search(ctx))?;
    Ok(page
        .results
        .iter()
        .filter_map(|r| text(r, "assetGroupSignal.resourceName"))
        .collect())
}

/// Recusa o que deixa o grupo abaixo do mínimo (nos tipos com remoção) ou acima do
/// máximo (nos tipos com inclusão), contando o que já existe.
fn check_counts(
    current: &[GroupAsset],
    adds: &[AddAsset],
    removed: &[&GroupAsset],
) -> Result<(), ToolError> {
    for t in ALL_TYPES {
        let now = current.iter().filter(|a| a.field_type == Some(t)).count();
        let add = adds.iter().filter(|a| a.field_type == t).count();
        let rem = removed.iter().filter(|a| a.field_type == Some(t)).count();
        let after = now + add - rem;
        if rem > 0 && after < t.min() {
            return Err(invalid(format!(
                "O grupo de recursos ficaria com {after} {}; o mínimo é {}.",
                t.label(),
                t.min()
            ))
            .with_field("remove_assets"));
        }
        if let Some(max) = t.max()
            && add > 0
            && after > max
        {
            return Err(invalid(format!(
                "O grupo de recursos ficaria com {after} {}; o máximo é {max}.",
                t.label()
            ))
            .with_field("add_assets"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- ferramenta

struct UpdateAssetGroup;

#[async_trait]
impl WriteTool for UpdateAssetGroup {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<UpdateAssetGroupInput>(
            UPDATE_ASSET_GROUP,
            "Edita um grupo de recursos (asset group) de uma campanha Performance Max: nome, \
             status, URLs finais, ativos (títulos, descrições, imagens, logos, vídeos) e sinais \
             (públicos e temas de busca). Os mínimos do grupo são respeitados (3 títulos, \
             1 título longo, 2 descrições, 1 imagem 1.91:1 e 1 imagem 1:1). Remover ativos \
             ou sinais não pode ser desfeito; editar campos e incluir itens na mesma chamada \
             não é aceito.",
            ToolGroup::Edit,
            true,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: UpdateAssetGroupInput = parse_input(input)?;
        let id = valid_id(&input.asset_group_id, "asset_group_id")?;
        let ag_rn = resources::rn(&ctx.customer_id, "assetGroups", &id);

        let adds: Vec<AddAsset> = input
            .add_assets
            .iter()
            .flatten()
            .enumerate()
            .map(|(i, a)| parse_add(ctx, a, i))
            .collect::<Result<_, _>>()?;
        let remove_assets = parse_rns(
            ctx,
            input.remove_assets.as_deref().unwrap_or_default(),
            "assetGroupAssets",
            "remove_assets",
        )?;
        let remove_signals = parse_rns(
            ctx,
            input.remove_signals.as_deref().unwrap_or_default(),
            "assetGroupSignals",
            "remove_signals",
        )?;
        let signals = input.add_signals.unwrap_or_default();
        let audiences: Vec<String> = signals
            .audiences
            .iter()
            .flatten()
            .enumerate()
            .map(|(i, a)| {
                collection_rn(ctx, a, "audiences", &format!("add_signals.audiences[{i}]"))
            })
            .collect::<Result<_, _>>()?;
        let mut themes: Vec<String> = Vec::new();
        for (i, t) in signals.search_themes.iter().flatten().enumerate() {
            let f = format!("add_signals.search_themes[{i}]");
            let t = t.trim();
            if t.is_empty() || t.chars().count() > SEARCH_THEME_MAX {
                return Err(invalid(format!(
                    "{f} deve ter de 1 a {SEARCH_THEME_MAX} caracteres."
                ))
                .with_field(f));
            }
            themes.push(t.to_string());
        }
        if let Some(n) = &input.name
            && n.trim().is_empty()
        {
            return Err(invalid("O nome não pode ficar vazio.").with_field("name"));
        }
        let urls = input.final_urls.as_deref().map(final_urls).transpose()?;
        let has_edit = input.name.is_some() || input.status.is_some() || urls.is_some();
        let creating = !adds.is_empty() || !audiences.is_empty() || !themes.is_empty();
        let removing = !remove_assets.is_empty() || !remove_signals.is_empty();
        if !has_edit && !creating && !removing {
            return Err(invalid("Informe ao menos uma alteração."));
        }
        if has_edit && creating && !removing {
            return Err(invalid(
                "Altere nome, status ou URLs finais em uma chamada e inclua ativos ou sinais \
                 em outra, para cada uma poder ser desfeita.",
            ));
        }

        // Leituras: o grupo, os vínculos e os sinais atuais.
        let group = read_one(
            ctx,
            UPDATE_ASSET_GROUP,
            &format!(
                "SELECT asset_group.resource_name, asset_group.name, asset_group.status, \
                 asset_group.final_urls FROM asset_group \
                 WHERE asset_group.id = {id} AND asset_group.status != 'REMOVED' LIMIT 1"
            ),
        )
        .await?
        .ok_or_else(|| {
            ToolError::new(
                codes::NOT_FOUND,
                format!(
                    "Grupo de recursos {id} não encontrado na conta {}.",
                    ctx.customer_id
                ),
            )
        })?;
        let current = if adds.is_empty() && remove_assets.is_empty() {
            Vec::new()
        } else {
            group_assets(ctx, &id).await?
        };
        let signal_rows = if remove_signals.is_empty() {
            Vec::new()
        } else {
            group_signals(ctx, &id).await?
        };

        let mut removed: Vec<&GroupAsset> = Vec::new();
        for (i, rn) in remove_assets.iter().enumerate() {
            removed.push(current.iter().find(|a| &a.rn == rn).ok_or_else(|| {
                ToolError::new(
                    codes::NOT_FOUND,
                    format!("O ativo {rn} não está no grupo de recursos {id}."),
                )
                .with_field(format!("remove_assets[{i}]"))
            })?);
        }
        for (i, rn) in remove_signals.iter().enumerate() {
            if !signal_rows.contains(rn) {
                return Err(ToolError::new(
                    codes::NOT_FOUND,
                    format!("O sinal {rn} não está no grupo de recursos {id}."),
                )
                .with_field(format!("remove_signals[{i}]")));
            }
        }
        check_counts(&current, &adds, &removed)?;

        // Edições de campos.
        let head = format!("Grupo de recursos {id}");
        let mut plan = if has_edit {
            let mut edits = Edits::new(head.clone(), &ctx.currency);
            if let Some(name) = &input.name {
                edits.set(
                    &ag_rn,
                    "asset_group.name",
                    &group,
                    json!(name),
                    "nome",
                    Show::Quoted,
                )?;
            }
            if let Some(status) = input.status {
                edits.set(
                    &ag_rn,
                    "asset_group.status",
                    &group,
                    json!(status.to_google()?),
                    "status",
                    Show::Status,
                )?;
            }
            if let Some(u) = urls {
                edits.set(
                    &ag_rn,
                    "asset_group.final_urls",
                    &group,
                    u,
                    "URLs finais",
                    Show::List,
                )?;
            }
            edits.into_plan(None)?
        } else {
            WritePlan {
                summary: String::new(),
                warning: None,
                operations: Vec::new(),
                field_changes: Vec::new(),
                watch: Vec::new(),
                inverse: InverseSpec::None,
                direct: None,
            }
        };

        // Criações (as imagens novas são carregadas só aqui).
        let mut temps = TempIds::new(&ctx.customer_id);
        let mut pool = ImagePool::default();
        let mut order: Vec<String> = Vec::new();
        let mut parts: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = current.iter().map(|a| a.rn.clone()).collect();
        let mut added: Vec<String> = Vec::new();
        for a in &adds {
            let ft = a.field_type.api();
            let aga_rn = match &a.source {
                Source::Existing(asset) => {
                    let asset_id = asset.rsplit('/').next().unwrap_or_default();
                    let rn = resources::rn(
                        &ctx.customer_id,
                        "assetGroupAssets",
                        &format!("{id}~{asset_id}~{ft}"),
                    );
                    if !seen.insert(rn.clone()) {
                        return Err(link_taken(
                            &rn,
                            &current,
                            ft,
                            format!("O ativo {asset} aparece mais de uma vez como {ft}."),
                        ));
                    }
                    plan.operations
                        .push(json!({ "assetGroupAssetOperation": { "create": {
                        "assetGroup": ag_rn, "asset": asset, "fieldType": ft } } }));
                    added.push(format!("{ft} {asset_id}"));
                    rn
                }
                Source::NewText(_) | Source::NewVideo(_) | Source::NewImage(_) => {
                    let (asset, label) = match &a.source {
                        Source::NewText(t) => {
                            let asset = temps.next("assets");
                            let body = json!({ "textAsset": { "text": t } });
                            plan.operations.push(create_op(&asset, body)?);
                            (asset, format!("{ft} '{t}'"))
                        }
                        Source::NewVideo(v) => {
                            let asset = temps.next("assets");
                            let body = json!({ "youtubeVideoAsset": { "youtubeVideoId": v } });
                            plan.operations.push(create_op(&asset, body)?);
                            (asset, format!("{ft} {v}"))
                        }
                        Source::NewImage(img) => {
                            let asset = pool
                                .resolve(
                                    ctx,
                                    &mut temps,
                                    &mut plan.operations,
                                    &img.r,
                                    img.kind,
                                    &img.field,
                                )
                                .await?;
                            (asset, format!("{ft} {}", image_source_label(&img.r)))
                        }
                        Source::Existing(_) => return Err(internal("fonte inesperada")),
                    };
                    let n = asset.rsplit('/').next().unwrap_or_default();
                    let rn = resources::rn(
                        &ctx.customer_id,
                        "assetGroupAssets",
                        &format!("{id}~{n}~{ft}"),
                    );
                    // Os mesmos bytes duas vezes no mesmo tipo dariam o mesmo vínculo.
                    if !seen.insert(rn.clone()) {
                        return Err(link_taken(
                            &rn,
                            &current,
                            ft,
                            format!("A mesma imagem aparece mais de uma vez como {ft}."),
                        ));
                    }
                    plan.operations.push(create_op(
                        &rn,
                        json!({ "assetGroup": ag_rn, "asset": asset, "fieldType": ft }),
                    )?);
                    added.push(label);
                    rn
                }
            };
            plan.watch.push(Watch {
                resource_name: aga_rn.clone(),
                resource: "asset_group_asset",
                fields: vec!["asset_group_asset.status"],
            });
            order.push(aga_rn);
        }
        if !added.is_empty() {
            parts.push(format!("incluir {}", list(&added)));
        }
        let mut signal_names: Vec<String> = Vec::new();
        for aud in &audiences {
            let rn = criterion_temp(ctx, &mut temps, "assetGroupSignals", &id);
            plan.operations.push(create_op(
                &rn,
                json!({ "assetGroup": ag_rn, "audience": { "audience": aud } }),
            )?);
            plan.watch.push(Watch {
                resource_name: rn.clone(),
                resource: "asset_group_signal",
                fields: vec!["asset_group_signal.audience.audience"],
            });
            order.push(rn);
            signal_names.push(format!(
                "público {}",
                aud.rsplit('/').next().unwrap_or_default()
            ));
        }
        for t in &themes {
            let rn = criterion_temp(ctx, &mut temps, "assetGroupSignals", &id);
            plan.operations.push(create_op(
                &rn,
                json!({ "assetGroup": ag_rn, "searchTheme": { "text": t } }),
            )?);
            plan.watch.push(Watch {
                resource_name: rn.clone(),
                resource: "asset_group_signal",
                fields: vec!["asset_group_signal.search_theme.text"],
            });
            order.push(rn);
            signal_names.push(format!("tema '{t}'"));
        }
        if !signal_names.is_empty() {
            parts.push(format!("incluir sinal(is) {}", list(&signal_names)));
        }

        // Remoções.
        if !removed.is_empty() {
            let names: Vec<String> = removed
                .iter()
                .map(|a| {
                    let ft = a.field_type.map_or("ativo", FieldType::api);
                    match &a.text {
                        Some(t) => format!("{ft} '{t}'"),
                        None => format!("{ft} {}", a.rn.rsplit('/').next().unwrap_or_default()),
                    }
                })
                .collect();
            parts.push(format!("remover {}", list(&names)));
        }
        for rn in remove_assets.iter().chain(&remove_signals) {
            plan.operations
                .push(resources::remove_op(rn).map_err(internal)?);
        }
        if !remove_signals.is_empty() {
            parts.push(format!("remover {} sinal(is)", remove_signals.len()));
        }

        // Resumo e inversa.
        let tail = parts.join("; ");
        plan.summary = match (has_edit, tail.is_empty()) {
            (true, true) => plan.summary,
            (true, false) => format!("{}; {tail}", plan.summary),
            (false, _) => format!("{head}: {tail}"),
        };
        if removing {
            plan.inverse = InverseSpec::None;
            plan.warning = Some(IRREVERSIBLE_WARNING.to_string());
        } else if creating {
            plan.inverse = InverseSpec::RemoveCreated { order };
        }
        Ok(plan)
    }
}
