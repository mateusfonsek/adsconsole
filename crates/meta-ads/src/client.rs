use std::sync::{Arc, Mutex};
use std::time::Duration;

use adsconsole_core::secrets::Secret;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use sha2::Sha256;

use crate::endpoints::MetaEndpoints;
use crate::error::{MetaError, net_text};
use crate::limiter::{AccountLimiter, COST_READ, COST_WRITE};
use crate::retry::{MAX_RETRIES, MAX_WAIT, RetryPolicy};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Nível de acesso da Marketing API (`ads_api_access_tier`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessTier {
    /// `development_access` (Limited Access).
    Limited,
    /// `standard_access` (Full Access).
    Full,
}

impl AccessTier {
    fn parse(s: &str) -> Option<AccessTier> {
        match s {
            "development_access" => Some(AccessTier::Limited),
            "standard_access" => Some(AccessTier::Full),
            _ => None,
        }
    }
}

/// Uso informado pela Meta nos headers de uma resposta.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub tier: Option<AccessTier>,
    /// Maior percentual de uso entre os headers (BUC, conta e insights).
    pub buc_max_pct: Option<f64>,
    /// Tempo até a Meta liberar de novo, quando bloqueado.
    pub regain_after: Option<Duration>,
}

fn header_json(headers: &HeaderMap, name: &str) -> Option<Value> {
    serde_json::from_str(headers.get(name)?.to_str().ok()?).ok()
}

fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// `reset_time_duration` do `X-Ad-Account-Usage`, se positivo.
fn account_reset(headers: &HeaderMap) -> Option<Duration> {
    header_json(headers, "x-ad-account-usage")?["reset_time_duration"]
        .as_u64()
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
}

impl Usage {
    /// Lê `X-Business-Use-Case-Usage`, `X-Ad-Account-Usage` e `X-FB-Ads-Insights-Throttle`.
    pub fn from_headers(headers: &HeaderMap) -> Option<Usage> {
        let mut u = Usage::default();
        let mut seen = false;
        let mut regain_secs: u64 = 0;
        if let Some(Value::Object(buc)) = header_json(headers, "x-business-use-case-usage") {
            seen = true;
            for entry in buc.values().filter_map(Value::as_array).flatten() {
                for k in ["call_count", "total_cputime", "total_time"] {
                    u.buc_max_pct = max_opt(u.buc_max_pct, entry[k].as_f64());
                }
                if let Some(min) = entry["estimated_time_to_regain_access"].as_u64() {
                    regain_secs = regain_secs.max(min * 60);
                }
                if let Some(t) = entry["ads_api_access_tier"]
                    .as_str()
                    .and_then(AccessTier::parse)
                {
                    u.tier = Some(t);
                }
            }
        }
        if let Some(acc) = header_json(headers, "x-ad-account-usage") {
            seen = true;
            let util = acc["acc_id_util_pct"].as_f64();
            u.buc_max_pct = max_opt(u.buc_max_pct, util);
            // `reset_time_duration` vem em toda resposta (a documentação traz 9,67% com
            // 100 s): só é bloqueio com a conta em 100%. Num erro de limite, vira o
            // `retry_after` (ver `account_reset`).
            if util.is_some_and(|p| p >= 100.0)
                && let Some(s) = account_reset(headers)
            {
                regain_secs = regain_secs.max(s.as_secs());
            }
            if let Some(t) = acc["ads_api_access_tier"]
                .as_str()
                .and_then(AccessTier::parse)
            {
                u.tier = Some(t);
            }
        }
        if let Some(ins) = header_json(headers, "x-fb-ads-insights-throttle") {
            seen = true;
            u.buc_max_pct = max_opt(u.buc_max_pct, ins["app_id_util_pct"].as_f64());
            u.buc_max_pct = max_opt(u.buc_max_pct, ins["acc_id_util_pct"].as_f64());
            if let Some(t) = ins["ads_api_access_tier"]
                .as_str()
                .and_then(AccessTier::parse)
            {
                u.tier = Some(t);
            }
        }
        u.regain_after = (regain_secs > 0).then(|| Duration::from_secs(regain_secs));
        seen.then_some(u)
    }
}

/// Tipo de uma escrita: `Validate` (`execution_options=["validate_only"]`) segue a
/// política de retry de leitura e nunca é "possivelmente aplicada".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Validate,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Validate,
    Write,
}

/// Onde continuar uma listagem: o cursor `after` da página (`None` = a primeira) e
/// quantos itens daquela página já foram entregues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCursor {
    pub after: Option<String>,
    pub skip: usize,
}

/// Itens de uma listagem; `next` diz onde continuar, ou `None` se acabou.
/// `starts[i]` é o cursor que começa em `data[i]` (para cortar a resposta no meio).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub data: Vec<Value>,
    pub starts: Vec<PageCursor>,
    pub next: Option<PageCursor>,
}

/// Cliente da Graph API de uma conexão.
#[derive(Clone)]
pub struct MetaClient {
    http: reqwest::Client,
    endpoints: MetaEndpoints,
    token: Secret,
    app_secret: Option<Secret>,
    proof: Option<String>,
    retry: RetryPolicy,
    timeout: Duration,
    usage: Arc<Mutex<Option<Usage>>>,
    limiter: Option<Arc<AccountLimiter>>,
    /// Conta das chamadas deste cliente, para o rate limiter (ver [`MetaClient::for_account`]).
    account: Option<String>,
    /// Não passa pelo `acquire` do limiter (ver [`MetaClient::bypass_limiter`]).
    bypass: bool,
}

fn appsecret_proof(secret: &Secret, token: &Secret) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes())
        .expect("HMAC aceita chave de qualquer tamanho");
    mac.update(token.expose().as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Valores de formulário: strings como estão, o resto como JSON.
fn form_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl MetaClient {
    pub fn new(endpoints: MetaEndpoints, token: Secret, app_secret: Option<Secret>) -> MetaClient {
        let proof = app_secret.as_ref().map(|s| appsecret_proof(s, &token));
        MetaClient {
            http: reqwest::Client::new(),
            endpoints,
            token,
            app_secret,
            proof,
            retry: RetryPolicy::default(),
            timeout: DEFAULT_TIMEOUT,
            usage: Arc::new(Mutex::new(None)),
            limiter: None,
            account: None,
            bypass: false,
        }
    }

    /// Usa `limiter` nas chamadas de clientes criados por [`for_account`](Self::for_account).
    pub fn with_limiter(mut self, limiter: Arc<AccountLimiter>) -> MetaClient {
        self.limiter = Some(limiter);
        self
    }

    /// Cópia deste cliente com outro token (ex.: o de uma Página), com o
    /// `appsecret_proof` recalculado.
    pub fn with_token(&self, token: Secret) -> MetaClient {
        let mut c = self.clone();
        c.proof = c.app_secret.as_ref().map(|s| appsecret_proof(s, &token));
        c.token = token;
        c.usage = Arc::new(Mutex::new(None));
        c
    }

    /// Cópia que não espera nem recusa pelo limiter local (só a Meta decide), mas ainda
    /// registra o uso informado. Para o rollback: apagar o que ficou não pode esperar.
    pub fn bypass_limiter(&self) -> MetaClient {
        let mut c = self.clone();
        c.bypass = true;
        c
    }

    /// Cópia deste cliente cujas chamadas contam (e esperam) no rate limiter da conta.
    pub fn for_account(&self, account: &str) -> MetaClient {
        let mut c = self.clone();
        c.account = Some(account.to_string());
        c.usage = Arc::new(Mutex::new(None));
        c
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> MetaClient {
        self.retry = retry;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> MetaClient {
        self.timeout = timeout;
        self
    }

    /// Uso informado na resposta mais recente deste cliente.
    pub fn last_usage(&self) -> Option<Usage> {
        *self.usage.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<url::Url, MetaError> {
        let base = format!(
            "{}/{}",
            self.endpoints.graph_base.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let mut url = url::Url::parse(&base)
            .map_err(|e| MetaError::Transport(format!("URL inválida: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
            if let Some(p) = &self.proof {
                q.append_pair("appsecret_proof", p);
            }
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
        Ok(url)
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, MetaError> {
        let url = self.url(path, query)?;
        self.send(Kind::Read, || self.http.get(url.clone())).await
    }

    /// Segue `paging.cursors.after` enquanto houver `paging.next`, até `max_items`.
    pub async fn get_all(
        &self,
        path: &str,
        query: &[(&str, String)],
        max_items: usize,
    ) -> Result<Page, MetaError> {
        let start = PageCursor {
            after: None,
            skip: 0,
        };
        self.get_from(path, query, &start, max_items).await
    }

    /// Como [`get_all`](Self::get_all), começando de `start`.
    pub async fn get_from(
        &self,
        path: &str,
        query: &[(&str, String)],
        start: &PageCursor,
        max_items: usize,
    ) -> Result<Page, MetaError> {
        let mut data = Vec::new();
        let mut starts = Vec::new();
        let mut after = start.after.clone();
        let mut skip = start.skip;
        loop {
            let mut q = query.to_vec();
            if let Some(a) = &after {
                q.push(("after", a.clone()));
            }
            let body = self.get(path, &q).await?;
            let items = body["data"].as_array().cloned().unwrap_or_default();
            let total = items.len();
            for (i, item) in items.into_iter().enumerate().skip(skip) {
                if data.len() == max_items {
                    return Ok(Page {
                        data,
                        starts,
                        next: Some(PageCursor { after, skip: i }),
                    });
                }
                data.push(item);
                starts.push(PageCursor {
                    after: after.clone(),
                    skip: i,
                });
            }
            let next_after = body["paging"]["cursors"]["after"]
                .as_str()
                .map(str::to_string);
            let has_next = body["paging"]["next"].is_string() && next_after.is_some();
            if !has_next || total == 0 {
                return Ok(Page {
                    data,
                    starts,
                    next: None,
                });
            }
            after = next_after;
            skip = 0;
            if data.len() == max_items {
                return Ok(Page {
                    data,
                    starts,
                    next: Some(PageCursor { after, skip: 0 }),
                });
            }
        }
    }

    /// POST form-encoded (strings como estão, o resto como JSON).
    pub async fn post(
        &self,
        path: &str,
        params: &Map<String, Value>,
        kind: WriteKind,
    ) -> Result<Value, MetaError> {
        let url = self.url(path, &[])?;
        let form: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.clone(), form_value(v)))
            .collect();
        let kind = match kind {
            WriteKind::Validate => Kind::Validate,
            WriteKind::Write => Kind::Write,
        };
        self.send(kind, || self.http.post(url.clone()).form(&form))
            .await
    }

    /// POST multipart (upload de vídeo). `make_form` monta o formulário de novo a cada
    /// tentativa, porque um `Form` não pode ser reenviado.
    pub async fn post_multipart(
        &self,
        path: &str,
        make_form: &(dyn Fn() -> reqwest::multipart::Form + Send + Sync),
    ) -> Result<Value, MetaError> {
        let url = self.url(path, &[])?;
        self.send(Kind::Write, || {
            self.http.post(url.clone()).multipart(make_form())
        })
        .await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, MetaError> {
        let url = self.url(path, &[])?;
        self.send(Kind::Write, || self.http.delete(url.clone()))
            .await
    }

    async fn send(
        &self,
        kind: Kind,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<Value, MetaError> {
        let mut attempt = 0;
        loop {
            let err = match self.once(kind, &build).await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            let retryable = match &err {
                e if e.is_rate_limit() => true,
                _ if kind == Kind::Write => false,
                MetaError::Timeout | MetaError::Transport(_) => true,
                MetaError::Api {
                    http_status,
                    is_transient,
                    ..
                } => *http_status == 429 || *http_status >= 500 || *is_transient,
                _ => false,
            };
            if !retryable || attempt >= MAX_RETRIES {
                return Err(err);
            }
            let wait = self
                .retry
                .delay(attempt)
                .max(err.retry_after().unwrap_or_default());
            if wait > MAX_WAIT {
                return Err(err);
            }
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    async fn once(
        &self,
        kind: Kind,
        build: &impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<Value, MetaError> {
        let limited = match (&self.limiter, &self.account) {
            (Some(l), Some(a)) => Some((l, a)),
            _ => None,
        };
        if let Some((l, a)) = limited.filter(|_| !self.bypass) {
            let cost = if kind == Kind::Read {
                COST_READ
            } else {
                COST_WRITE
            };
            l.acquire(a, cost).await?;
        }
        let resp = build()
            .bearer_auth(self.token.expose())
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| match (kind, e.is_timeout(), e.is_connect()) {
                // Sem conexão, o request nem saiu: dá para repetir.
                (_, _, true) => MetaError::Transport(net_text(e)),
                (Kind::Write, true, _) => MetaError::MaybeApplied {
                    cause: "tempo esgotado".into(),
                },
                (Kind::Write, false, _) => MetaError::MaybeApplied { cause: net_text(e) },
                (_, true, _) => MetaError::Timeout,
                _ => MetaError::Transport(net_text(e)),
            })?;
        let status = resp.status().as_u16();
        let usage = Usage::from_headers(resp.headers());
        let reset = account_reset(resp.headers());
        if let Some(u) = &usage {
            *self.usage.lock().unwrap_or_else(|e| e.into_inner()) = Some(*u);
            if let Some((l, a)) = limited {
                l.observe(a, u);
            }
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| match (kind, e.is_timeout()) {
                (Kind::Write, _) => MetaError::MaybeApplied { cause: net_text(e) },
                (_, true) => MetaError::Timeout,
                _ => MetaError::Transport(net_text(e)),
            })?;
        let body: Value = if bytes.is_empty() {
            Value::Object(Map::new())
        } else {
            match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) if (200..300).contains(&status) => {
                    return Err(MetaError::Decode("a resposta não é JSON".into()));
                }
                Err(_) => Value::Null,
            }
        };
        // A Meta às vezes responde 200 com `error` no corpo (visto na criação de anúncio).
        let error_in_body = body.get("error").is_some_and(Value::is_object);
        if (200..300).contains(&status) && !error_in_body {
            return Ok(body);
        }
        let mut err = MetaError::from_body(status, &body);
        if let MetaError::Api {
            maybe_applied,
            retry_after,
            ..
        } = &mut err
        {
            *retry_after = usage.and_then(|u| u.regain_after).max(reset);
            let limited = MetaError::from_body(status, &body).is_rate_limit();
            *maybe_applied = kind == Kind::Write && status >= 500 && !limited;
        }
        Err(err)
    }
}
