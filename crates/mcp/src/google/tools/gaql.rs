//! `gaql_search`: consulta GAQL livre (só `SELECT`) com paginação por cursor opaco.

use adsconsole_core::config::ToolGroup;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{GTool, ReadTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::truncate::{ITEMS_BUDGET, fit};

pub(crate) const NAME: &str = "gaql_search";

pub(crate) fn tool() -> GTool {
    GTool::Read(Box::new(GaqlSearch))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GaqlSearchInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// Consulta GAQL; precisa começar com SELECT.
    query: String,
    /// `next_cursor` da resposta anterior, com a mesma consulta, para continuar de onde parou.
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GaqlSearchOutput {
    /// Linhas como a API devolve (camelCase; int64 como texto; valores em micros).
    rows: Vec<Value>,
    /// Há mais linhas além das devolvidas; continue com `next_cursor`.
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Posição de leitura: a página `page_token` (ausente = primeira) e as `skip` linhas
/// dela que já foram entregues.
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    page_token: Option<String>,
    skip: usize,
}

impl Cursor {
    fn encode(&self) -> String {
        // Serializar dois campos simples não falha.
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    fn decode(s: &str) -> Result<Self, ToolError> {
        URL_SAFE_NO_PAD
            .decode(s)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| {
                ToolError::new(codes::INVALID_INPUT, "Cursor inválido.")
                    .with_field("cursor")
                    .with_suggestion(
                        "Use o next_cursor devolvido pela chamada anterior, sem alterar.",
                    )
            })
    }
}

fn is_select(query: &str) -> bool {
    let q = query.trim_start();
    q.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("select"))
        && q[6..].starts_with(|c: char| c.is_whitespace())
}

struct GaqlSearch;

#[async_trait]
impl ReadTool for GaqlSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec::read::<GaqlSearchInput>(
            NAME,
            "Executa uma consulta GAQL (só SELECT) na conta. Respostas grandes vêm cortadas, \
             com truncated: true e next_cursor; chame de novo com o cursor para continuar.",
            ToolGroup::Read,
        )
        .output::<GaqlSearchOutput>()
    }

    async fn run(&self, ctx: &Ctx, input: &Value) -> Result<Value, ToolError> {
        let input: GaqlSearchInput = parse_input(input)?;
        if !is_select(&input.query) {
            return Err(ToolError::new(
                codes::INVALID_INPUT,
                "A consulta GAQL precisa começar com SELECT.",
            )
            .with_field("query"));
        }
        let cursor = match input.cursor.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => Cursor::decode(c)?,
            None => Cursor {
                page_token: None,
                skip: 0,
            },
        };
        let mut token = cursor.page_token;
        let mut skip = cursor.skip;
        let (rows, next_cursor) = loop {
            let page = ctx
                .client
                .search(
                    &ctx.customer_id,
                    ctx.login.as_deref(),
                    &input.query,
                    token.as_deref(),
                )
                .await
                .map_err(|e| translate(e, Some(ctx), NAME))?;
            let remaining: Vec<Value> = page.results.into_iter().skip(skip).collect();
            if remaining.is_empty() {
                // Página esgotada (ou vazia): segue para a próxima, se houver.
                match page.next_page_token {
                    Some(next) => {
                        token = Some(next);
                        skip = 0;
                        continue;
                    }
                    None => break (Vec::new(), None),
                }
            }
            let total = remaining.len();
            let (rows, _) = fit(remaining, ITEMS_BUDGET);
            let next = if rows.len() < total {
                Some(Cursor {
                    page_token: token,
                    skip: skip + rows.len(),
                })
            } else {
                page.next_page_token.map(|t| Cursor {
                    page_token: Some(t),
                    skip: 0,
                })
            };
            break (rows, next);
        };
        let out = GaqlSearchOutput {
            rows,
            truncated: next_cursor.is_some(),
            next_cursor: next_cursor.map(|c| c.encode()),
        };
        serde_json::to_value(out).map_err(crate::engine::internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_detection() {
        assert!(is_select("SELECT a FROM b"));
        assert!(is_select("  select\n a FROM b"));
        assert!(!is_select("selectx a"));
        assert!(!is_select("DELETE x"));
        assert!(!is_select("sélect"));
        assert!(!is_select(""));
    }

    #[test]
    fn cursor_round_trip() {
        let c = Cursor {
            page_token: Some("p2".into()),
            skip: 3,
        };
        let back = Cursor::decode(&c.encode()).unwrap();
        assert_eq!((back.page_token.as_deref(), back.skip), (Some("p2"), 3));
        assert!(Cursor::decode("!!").is_err());
    }
}
