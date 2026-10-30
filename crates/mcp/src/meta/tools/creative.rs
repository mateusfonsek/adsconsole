//! Criativos: imagem, vídeo e carrossel (montados como `object_story_spec`) ou um post
//! que já existe na Página (`object_story_id`).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::{ToolError, codes};

/// Tipos de botão aceitos.
pub(crate) const CALLS_TO_ACTION: [&str; 13] = [
    "LEARN_MORE",
    "SHOP_NOW",
    "SIGN_UP",
    "CONTACT_US",
    "BOOK_TRAVEL",
    "DOWNLOAD",
    "GET_OFFER",
    "GET_QUOTE",
    "SUBSCRIBE",
    "WHATSAPP_MESSAGE",
    "APPLY_NOW",
    "ORDER_NOW",
    "SEE_MORE",
];
const MESSAGE_MAX: usize = 2200;
const TITLE_MAX: usize = 255;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct Card {
    /// https.
    pub link: String,
    pub image_hash: String,
    #[serde(default)]
    pub headline: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub call_to_action: Option<String>,
}

/// O criativo de um anúncio. `type`: image, video ou carousel.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CreativeInput {
    Image {
        page_id: String,
        #[serde(default)]
        instagram_user_id: Option<String>,
        /// Destino (https).
        link: String,
        /// Texto principal (até 2 200 caracteres).
        message: String,
        #[serde(default)]
        headline: Option<String>,
        #[serde(default)]
        description: Option<String>,
        /// De `upload_image`.
        image_hash: String,
        /// LEARN_MORE, SHOP_NOW, SIGN_UP, CONTACT_US...
        #[serde(default)]
        call_to_action: Option<String>,
    },
    Video {
        page_id: String,
        #[serde(default)]
        instagram_user_id: Option<String>,
        link: String,
        message: String,
        #[serde(default)]
        headline: Option<String>,
        /// De `upload_video`.
        video_id: String,
        /// Capa: image_hash de `upload_image` ou uma URL https (um dos dois).
        #[serde(default)]
        thumbnail_image_hash: Option<String>,
        #[serde(default)]
        thumbnail_url: Option<String>,
        #[serde(default)]
        call_to_action: Option<String>,
    },
    /// Post já publicado na Página. Funciona com o app em modo de desenvolvimento, que
    /// não pode anunciar posts criados por ele mesmo.
    ExistingPost {
        /// `<page_id>_<post_id>`, de `get_page_posts`.
        object_story_id: String,
    },
    Carousel {
        page_id: String,
        #[serde(default)]
        instagram_user_id: Option<String>,
        link: String,
        message: String,
        /// 2 a 10 cards.
        cards: Vec<Card>,
    },
}

fn invalid(field: String, message: impl Into<String>) -> ToolError {
    ToolError::new(codes::INVALID_INPUT, message).with_field(field)
}

fn check_link(field: String, link: &str) -> Result<(), ToolError> {
    match url::Url::parse(link) {
        Ok(u) if u.scheme() == "https" => Ok(()),
        _ => Err(invalid(field, format!("O link precisa ser https: {link}"))),
    }
}

fn check_len(field: String, text: &str, max: usize) -> Result<(), ToolError> {
    if text.chars().count() > max {
        return Err(invalid(field, format!("Passa de {max} caracteres.")));
    }
    Ok(())
}

fn cta(
    field: String,
    kind: &Option<String>,
    default: Option<&str>,
    link: &str,
) -> Result<Option<Value>, ToolError> {
    let Some(t) = kind.as_deref().or(default) else {
        return Ok(None);
    };
    let t = t.trim().to_uppercase();
    if !CALLS_TO_ACTION.contains(&t.as_str()) {
        return Err(invalid(
            field,
            format!(
                "Botão desconhecido: {t}. Use: {}.",
                CALLS_TO_ACTION.join(", ")
            ),
        ));
    }
    Ok(Some(json!({ "type": t, "value": { "link": link } })))
}

fn put(m: &mut Map<String, Value>, k: &str, v: Option<Value>) {
    if let Some(v) = v {
        m.insert(k.into(), v);
    }
}

/// Parâmetros do POST em `adcreatives` (`object_story_spec` ou `object_story_id`).
pub(crate) fn creative_params(
    c: &CreativeInput,
    at: &str,
) -> Result<Map<String, Value>, ToolError> {
    if let CreativeInput::ExistingPost { object_story_id } = c {
        let ok = object_story_id.split_once('_').is_some_and(|(p, s)| {
            !p.is_empty() && !s.is_empty() && (p.to_owned() + s).chars().all(|x| x.is_ascii_digit())
        });
        if !ok {
            return Err(invalid(
                format!("{at}.object_story_id"),
                "Use o id do post no formato <page_id>_<post_id> (veja get_page_posts).",
            ));
        }
        let mut m = Map::new();
        m.insert("object_story_id".into(), json!(object_story_id));
        return Ok(m);
    }
    let mut m = Map::new();
    m.insert("object_story_spec".into(), story_spec(c, at)?);
    Ok(m)
}

/// `object_story_spec` do criativo; `at` é o caminho do campo na entrada, para os erros.
fn story_spec(c: &CreativeInput, at: &str) -> Result<Value, ToolError> {
    let f = |k: &str| format!("{at}.{k}");
    let mut spec = Map::new();
    match c {
        CreativeInput::ExistingPost { .. } => unreachable!("tratado em creative_params"),
        CreativeInput::Image {
            page_id,
            instagram_user_id,
            link,
            message,
            headline,
            description,
            image_hash,
            call_to_action,
        } => {
            check_link(f("link"), link)?;
            check_len(f("message"), message, MESSAGE_MAX)?;
            check_len(f("headline"), headline.as_deref().unwrap_or(""), TITLE_MAX)?;
            check_len(
                f("description"),
                description.as_deref().unwrap_or(""),
                TITLE_MAX,
            )?;
            spec.insert("page_id".into(), json!(page_id));
            // VERIFY(v26.0): identidade do Instagram como instagram_user_id (não instagram_actor_id).
            put(
                &mut spec,
                "instagram_user_id",
                instagram_user_id.as_ref().map(|v| json!(v)),
            );
            let mut data = Map::new();
            data.insert("link".into(), json!(link));
            data.insert("message".into(), json!(message));
            data.insert("image_hash".into(), json!(image_hash));
            put(&mut data, "name", headline.as_ref().map(|v| json!(v)));
            put(
                &mut data,
                "description",
                description.as_ref().map(|v| json!(v)),
            );
            put(
                &mut data,
                "call_to_action",
                cta(f("call_to_action"), call_to_action, None, link)?,
            );
            spec.insert("link_data".into(), Value::Object(data));
        }
        CreativeInput::Video {
            page_id,
            instagram_user_id,
            link,
            message,
            headline,
            video_id,
            thumbnail_image_hash,
            thumbnail_url,
            call_to_action,
        } => {
            check_link(f("link"), link)?;
            check_len(f("message"), message, MESSAGE_MAX)?;
            check_len(f("headline"), headline.as_deref().unwrap_or(""), TITLE_MAX)?;
            spec.insert("page_id".into(), json!(page_id));
            put(
                &mut spec,
                "instagram_user_id",
                instagram_user_id.as_ref().map(|v| json!(v)),
            );
            let mut data = Map::new();
            data.insert("video_id".into(), json!(video_id));
            data.insert("message".into(), json!(message));
            put(&mut data, "title", headline.as_ref().map(|v| json!(v)));
            // VERIFY(v26.0): a capa é obrigatória (image_hash ou image_url).
            match (thumbnail_image_hash, thumbnail_url) {
                (Some(h), None) => {
                    data.insert("image_hash".into(), json!(h));
                }
                (None, Some(u)) => {
                    check_link(f("thumbnail_url"), u)?;
                    data.insert("image_url".into(), json!(u));
                }
                _ => {
                    return Err(invalid(
                        f("thumbnail_image_hash"),
                        "Informe a capa do vídeo: thumbnail_image_hash ou thumbnail_url (um dos dois).",
                    ));
                }
            }
            put(
                &mut data,
                "call_to_action",
                cta(
                    f("call_to_action"),
                    call_to_action,
                    Some("LEARN_MORE"),
                    link,
                )?,
            );
            spec.insert("video_data".into(), Value::Object(data));
        }
        CreativeInput::Carousel {
            page_id,
            instagram_user_id,
            link,
            message,
            cards,
        } => {
            check_link(f("link"), link)?;
            check_len(f("message"), message, MESSAGE_MAX)?;
            if !(2..=10).contains(&cards.len()) {
                return Err(invalid(f("cards"), "O carrossel precisa de 2 a 10 cards."));
            }
            let mut children = Vec::new();
            for (i, card) in cards.iter().enumerate() {
                let cf = |k: &str| format!("{at}.cards[{i}].{k}");
                check_link(cf("link"), &card.link)?;
                check_len(
                    cf("headline"),
                    card.headline.as_deref().unwrap_or(""),
                    TITLE_MAX,
                )?;
                check_len(
                    cf("description"),
                    card.description.as_deref().unwrap_or(""),
                    TITLE_MAX,
                )?;
                let mut c = Map::new();
                c.insert("link".into(), json!(card.link));
                c.insert("image_hash".into(), json!(card.image_hash));
                put(&mut c, "name", card.headline.as_ref().map(|v| json!(v)));
                put(
                    &mut c,
                    "description",
                    card.description.as_ref().map(|v| json!(v)),
                );
                put(
                    &mut c,
                    "call_to_action",
                    cta(cf("call_to_action"), &card.call_to_action, None, &card.link)?,
                );
                children.push(Value::Object(c));
            }
            spec.insert("page_id".into(), json!(page_id));
            put(
                &mut spec,
                "instagram_user_id",
                instagram_user_id.as_ref().map(|v| json!(v)),
            );
            spec.insert(
                "link_data".into(),
                json!({ "link": link, "message": message, "child_attachments": children }),
            );
        }
    }
    Ok(Value::Object(spec))
}
