//! Meta tools. Each one is a read ([`ReadTool`]) or a write ([`WriteTool`]),
//! registered in [`all`].

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

/// A write: builds the [`MetaPlan`], rebuilt at every step (validate, snapshot,
/// execute), so it must be deterministic and free of side effects.
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

/// Tools with an account (the ones without, like `list_ad_accounts`, live in the adapter).
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

/// A Graph `path` must belong to the call's account (`act_…`) or be a numeric object id.
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
            "The path must start with the call's account ({}) or with an object id.",
            ctx.account_id
        ),
    )
    .with_field("path"))
}

pub(crate) fn parse_input<T: DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone())
        .map_err(|e| ToolError::new(codes::INVALID_INPUT, format!("Invalid input: {e}")))
}

/// Query parameters that never appear in a returned URL.
const SECRET_PARAMS: [&str; 2] = ["access_token", "appsecret_proof"];

/// Strips `access_token` and `appsecret_proof` from the query of an http(s) URL (Meta puts
/// them in `paging.next`, `ad_snapshot_url` etc.). Other text is left as is.
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

/// Strips every `access_token` key, at any depth (e.g. the Page tokens that `me/accounts`
/// returns), and the tokens in URL queries. Every read (and `graph_post`) passes its
/// result through here.
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

/// `next_cursor` of paginated reads: Meta's `after` and how many items to skip on the page.
pub(crate) fn encode_cursor(c: &PageCursor) -> String {
    let v = serde_json::json!({ "a": c.after, "s": c.skip });
    URL_SAFE_NO_PAD.encode(v.to_string())
}

pub(crate) fn decode_cursor(s: &str) -> Result<PageCursor, ToolError> {
    let bad = || ToolError::new(codes::INVALID_INPUT, "Invalid cursor.").with_field("cursor");
    let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| bad())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    Ok(PageCursor {
        after: v["a"].as_str().map(str::to_string),
        skip: v["s"].as_u64().ok_or_else(bad)? as usize,
    })
}
