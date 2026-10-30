//! Ferramentas da Meta. Cada uma é uma leitura ([`ReadTool`]) ou uma escrita
//! ([`WriteTool`], a partir da Task 6), registrada em [`all`].

pub(crate) mod accounts;
pub(crate) mod create;
pub(crate) mod creative;
pub(crate) mod duplicate;
pub(crate) mod edit;
pub(crate) mod escape;
pub(crate) mod hidden;
pub(crate) mod lookup;
pub(crate) mod media;
pub(crate) mod reports;

use adsconsole_meta_ads::client::PageCursor;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::ctx::Ctx;
use super::plan::MetaPlan;
use crate::adapter::ToolSpec;
use crate::error::{ToolError, codes};

#[async_trait::async_trait]
pub(crate) trait ReadTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError>;
}

/// Uma escrita: monta o [`MetaPlan`], refeito em cada etapa (validate, snapshot,
/// execute), então precisa ser determinístico e sem efeitos colaterais.
#[async_trait::async_trait]
pub(crate) trait WriteTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<MetaPlan, ToolError>;
}

pub(crate) enum MTool {
    Read(Box<dyn ReadTool>),
    Write(Box<dyn WriteTool>),
}

impl MTool {
    pub(crate) fn spec(&self) -> ToolSpec {
        match self {
            MTool::Read(t) => t.spec(),
            MTool::Write(t) => t.spec(),
        }
    }
}

/// Ferramentas com conta (as sem conta, como `list_ad_accounts`, ficam no adapter).
pub(crate) fn all() -> Vec<MTool> {
    let mut tools = reports::tools();
    tools.extend(lookup::tools());
    tools.extend(edit::tools());
    tools.extend(media::tools());
    tools.extend(create::tools());
    tools.extend(duplicate::tools());
    tools.extend(hidden::hidden());
    tools.extend(escape::escape());
    tools
}

/// `path` do Graph precisa ser da conta da chamada (`act_…`) ou um id numérico de objeto.
pub(crate) fn own_path(ctx: &Ctx, path: &str) -> Result<(), ToolError> {
    let first = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    let numeric = !first.is_empty() && first.chars().all(|c| c.is_ascii_digit());
    if first == ctx.account_id || numeric {
        return Ok(());
    }
    Err(ToolError::new(
        codes::INVALID_INPUT,
        format!(
            "O caminho precisa começar pela conta da chamada ({}) ou por um id de objeto.",
            ctx.account_id
        ),
    )
    .with_field("path"))
}

pub(crate) fn parse_input<T: DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone())
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Entrada inválida: {e}")))
}

/// Parâmetros de query que nunca saem numa URL devolvida.
const SECRET_PARAMS: [&str; 2] = ["access_token", "appsecret_proof"];

/// Tira `access_token` e `appsecret_proof` da query de uma URL http(s) (a Meta os põe
/// em `paging.next`, `ad_snapshot_url` etc.). Outros textos ficam como estão.
pub(crate) fn without_token(url: &str) -> String {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return url.to_string();
    }
    let Ok(mut u) = url::Url::parse(url) else {
        return url.to_string();
    };
    if !u
        .query_pairs()
        .any(|(k, _)| SECRET_PARAMS.contains(&k.as_ref()))
    {
        return url.to_string();
    }
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !SECRET_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    u.to_string()
}

/// Tira toda chave `access_token`, em qualquer profundidade (ex.: tokens de Página que
/// `me/accounts` traz), e os tokens na query de URLs. Toda leitura (e o `graph_post`)
/// passa o resultado por aqui.
pub fn scrub(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(without_token(&s)),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| k != "access_token")
                .map(|(k, v)| (k, scrub(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(scrub).collect()),
        other => other,
    }
}

/// `next_cursor` das leituras paginadas: o `after` da Meta e quantos itens pular na página.
pub(crate) fn encode_cursor(c: &PageCursor) -> String {
    let v = serde_json::json!({ "a": c.after, "s": c.skip });
    URL_SAFE_NO_PAD.encode(v.to_string())
}

pub(crate) fn decode_cursor(s: &str) -> Result<PageCursor, ToolError> {
    let bad = || ToolError::new(codes::INVALID_INPUT, "cursor inválido").with_field("cursor");
    let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| bad())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    Ok(PageCursor {
        after: v["a"].as_str().map(str::to_string),
        skip: v["s"].as_u64().ok_or_else(bad)? as usize,
    })
}
