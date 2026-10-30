use std::sync::Arc;

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::endpoints::GoogleEndpoints;
use crate::error::{GoogleError, parse_api_error};
use crate::meter::OpsMeter;
use crate::retry::{MAX_RETRIES, RetryPolicy};
use crate::token::{TokenManager, http_client, map_reqwest};

/// Natureza da chamada: define o retry e a contagem de operações.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    Read,
    Mutate {
        ops: u32,
    },
    /// Mutate com `validateOnly`: nunca aplica, então segue o retry de leitura,
    /// mas conta as operações.
    Validate {
        ops: u32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub results: Vec<Value>,
    pub next_page_token: Option<String>,
}

#[derive(Clone)]
pub struct GoogleAdsClient {
    endpoints: GoogleEndpoints,
    tokens: Arc<TokenManager>,
    meter: Arc<dyn OpsMeter>,
    http: reqwest::Client,
    retry: RetryPolicy,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSearch {
    #[serde(default)]
    results: Vec<Value>,
    next_page_token: Option<String>,
}

impl GoogleAdsClient {
    pub fn new(
        endpoints: GoogleEndpoints,
        tokens: Arc<TokenManager>,
        meter: Arc<dyn OpsMeter>,
    ) -> Self {
        // Falha só se o backend TLS não inicializar; sem isso nenhuma chamada funcionaria.
        let http = http_client().expect("cliente HTTP");
        Self {
            endpoints,
            tokens,
            meter,
            http,
            retry: RetryPolicy::default(),
        }
    }

    /// Troca o timeout HTTP por request (padrão 60 s).
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("cliente HTTP");
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub async fn list_accessible_customers(&self) -> Result<Vec<String>, GoogleError> {
        let url = format!(
            "{}/customers:listAccessibleCustomers",
            self.endpoints.api_base
        );
        let v = self
            .send(Method::GET, &url, None, None, CallKind::Read)
            .await?;
        Ok(v.get("resourceNames")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|r| r.strip_prefix("customers/").unwrap_or(r).to_string())
            .collect())
    }

    pub async fn search(
        &self,
        customer: &str,
        login: Option<&str>,
        query: &str,
        page_token: Option<&str>,
    ) -> Result<SearchPage, GoogleError> {
        let mut body = json!({ "query": query });
        if let Some(t) = page_token {
            body["pageToken"] = json!(t);
        }
        let url = format!(
            "{}/customers/{customer}/googleAds:search",
            self.endpoints.api_base
        );
        let v = self
            .send(Method::POST, &url, login, Some(&body), CallKind::Read)
            .await?;
        let raw: RawSearch = serde_json::from_value(v)
            .map_err(|_| GoogleError::Decode("resposta de busca malformada".into()))?;
        Ok(SearchPage {
            results: raw.results,
            next_page_token: raw.next_page_token.filter(|t| !t.is_empty()),
        })
    }

    /// Segue as páginas até `max_rows` linhas (ou o fim dos resultados).
    pub async fn search_all(
        &self,
        customer: &str,
        login: Option<&str>,
        query: &str,
        max_rows: usize,
    ) -> Result<Vec<Value>, GoogleError> {
        let mut rows = Vec::new();
        let mut token: Option<String> = None;
        while rows.len() < max_rows {
            let page = self
                .search(customer, login, query, token.as_deref())
                .await?;
            rows.extend(page.results);
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        rows.truncate(max_rows);
        Ok(rows)
    }

    /// Mutate atômico (`partialFailure: false`). Em `validate_only` devolve lista vazia.
    pub async fn mutate(
        &self,
        customer: &str,
        login: Option<&str>,
        operations: &[Value],
        validate_only: bool,
    ) -> Result<Vec<Value>, GoogleError> {
        let body = json!({
            "mutateOperations": operations,
            "validateOnly": validate_only,
            "partialFailure": false,
        });
        let url = format!(
            "{}/customers/{customer}/googleAds:mutate",
            self.endpoints.api_base
        );
        let ops = u32::try_from(operations.len()).unwrap_or(u32::MAX);
        let kind = if validate_only {
            CallKind::Validate { ops }
        } else {
            CallKind::Mutate { ops }
        };
        let v = self
            .send(Method::POST, &url, login, Some(&body), kind)
            .await?;
        if validate_only {
            return Ok(Vec::new());
        }
        Ok(v.get("mutateOperationResponses")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// POST genérico em `customers/{c}{path_suffix}` (p.ex. `:generateKeywordIdeas`).
    pub async fn post(
        &self,
        customer: &str,
        login: Option<&str>,
        path_suffix: &str,
        body: &Value,
        kind: CallKind,
    ) -> Result<Value, GoogleError> {
        let sep = if path_suffix.starts_with([':', '/']) {
            ""
        } else {
            "/"
        };
        let url = format!(
            "{}/customers/{customer}{sep}{path_suffix}",
            self.endpoints.api_base
        );
        self.send(Method::POST, &url, login, Some(body), kind).await
    }

    /// Request com o token (só no header `Authorization`) e o `login-customer-id`,
    /// sem corpo nem retry. Usado pelo upload resumable ([`crate::upload`]).
    pub(crate) async fn authed(
        &self,
        method: Method,
        url: &str,
        login: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, GoogleError> {
        let token = self.tokens.access_token().await?;
        let mut req = self.http.request(method, url).bearer_auth(token.expose());
        if let Some(l) = login {
            req = req.header("login-customer-id", l);
        }
        Ok(req)
    }

    pub(crate) fn endpoints(&self) -> &GoogleEndpoints {
        &self.endpoints
    }

    /// Conta `ops` operações no medidor (uma resposta HTTP recebida).
    pub(crate) fn record_ops(&self, ops: u32) {
        self.meter.record(ops);
    }

    /// Envia com retry seguro. O medidor é acionado
    /// a cada resposta HTTP recebida (retries e páginas contam).
    async fn send(
        &self,
        method: Method,
        url: &str,
        login: Option<&str>,
        body: Option<&Value>,
        kind: CallKind,
    ) -> Result<Value, GoogleError> {
        let is_mutate = matches!(kind, CallKind::Mutate { .. });
        // Num mutate, qualquer falha depois do envio pode ter aplicado a mudança.
        let unsure = |e: GoogleError| match e {
            GoogleError::Timeout => GoogleError::MaybeApplied {
                cause: "tempo esgotado".into(),
            },
            GoogleError::Transport(c) => GoogleError::MaybeApplied { cause: c },
            other => other,
        };
        let mut retry = 0;
        loop {
            let token = self.tokens.access_token().await?;
            let mut req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(token.expose());
            if let Some(l) = login {
                req = req.header("login-customer-id", l);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if is_mutate => return Err(unsure(map_reqwest(e))),
                Err(e) => return Err(map_reqwest(e)),
            };
            self.meter.record(match kind {
                CallKind::Read => 1,
                CallKind::Mutate { ops } | CallKind::Validate { ops } => ops,
            });
            let status = resp.status();
            let request_id = resp
                .headers()
                .get("request-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) if is_mutate => return Err(unsure(map_reqwest(e))),
                Err(e) => return Err(map_reqwest(e)),
            };
            if status.is_success() {
                if bytes.is_empty() {
                    return Ok(Value::Null);
                }
                return serde_json::from_slice(&bytes).map_err(|_| {
                    if is_mutate {
                        GoogleError::MaybeApplied {
                            cause: "resposta JSON malformada".into(),
                        }
                    } else {
                        GoogleError::Decode("resposta JSON malformada".into())
                    }
                });
            }
            let err = parse_api_error(
                status.as_u16(),
                &bytes,
                request_id,
                is_mutate && status.is_server_error(),
            );
            let maybe_applied = matches!(
                err,
                GoogleError::Api {
                    maybe_applied: true,
                    ..
                }
            );
            let retryable =
                !maybe_applied && (err.is_quota() || (!is_mutate && status.is_server_error()));
            if !retryable || retry >= MAX_RETRIES {
                return Err(err);
            }
            tokio::time::sleep(self.retry.delay(retry)).await;
            retry += 1;
        }
    }
}
