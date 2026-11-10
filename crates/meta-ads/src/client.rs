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

/// Marketing API access tier (`ads_api_access_tier`).
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

/// Usage reported by Meta in the headers of a response.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub tier: Option<AccessTier>,
    /// Highest usage percentage across the headers (BUC, account and insights).
    pub buc_max_pct: Option<f64>,
    /// Time until Meta allows calls again, when blocked.
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

/// `reset_time_duration` from `X-Ad-Account-Usage`, if positive.
fn account_reset(headers: &HeaderMap) -> Option<Duration> {
    header_json(headers, "x-ad-account-usage")?["reset_time_duration"]
        .as_u64()
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
}

impl Usage {
    /// Reads `X-Business-Use-Case-Usage`, `X-Ad-Account-Usage` and `X-FB-Ads-Insights-Throttle`.
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
            // `reset_time_duration` comes in every response (the documentation shows 9.67%
            // with 100 s): it only means a block when the account is at 100%. On a rate
            // limit error, it becomes the `retry_after` (see `account_reset`).
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

/// Kind of write: `Validate` (`execution_options=["validate_only"]`) follows the read
/// retry policy and is never "possibly applied".
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

/// Where to resume a listing: the page's `after` cursor (`None` = the first page) and
/// how many items of that page were already returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCursor {
    pub after: Option<String>,
    pub skip: usize,
}

/// Items of a listing; `next` says where to resume, or `None` when there are no more.
/// `starts[i]` is the cursor that begins at `data[i]` (to cut the response in the middle).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub data: Vec<Value>,
    pub starts: Vec<PageCursor>,
    pub next: Option<PageCursor>,
}

/// Graph API client for one connection.
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
    /// Account of this client's calls, for the rate limiter (see [`MetaClient::for_account`]).
    account: Option<String>,
    /// Skips the limiter's `acquire` (see [`MetaClient::bypass_limiter`]).
    bypass: bool,
}

fn appsecret_proof(secret: &Secret, token: &Secret) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes())
        .expect("HMAC accepts a key of any size");
    mac.update(token.expose().as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Form values: strings as they are, everything else as JSON.
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

    /// Uses `limiter` for calls from clients created by [`for_account`](Self::for_account).
    pub fn with_limiter(mut self, limiter: Arc<AccountLimiter>) -> MetaClient {
        self.limiter = Some(limiter);
        self
    }

    /// Copy of this client with another token (e.g. a Page's), with the
    /// `appsecret_proof` recomputed.
    pub fn with_token(&self, token: Secret) -> MetaClient {
        let mut c = self.clone();
        c.proof = c.app_secret.as_ref().map(|s| appsecret_proof(s, &token));
        c.token = token;
        c.usage = Arc::new(Mutex::new(None));
        c
    }

    /// Copy that neither waits nor refuses because of the local limiter (only Meta
    /// decides), but still records the reported usage. For rollback: deleting what was
    /// left behind cannot wait.
    pub fn bypass_limiter(&self) -> MetaClient {
        let mut c = self.clone();
        c.bypass = true;
        c
    }

    /// Copy of this client whose calls count against (and wait on) the account's rate limiter.
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

    /// Usage reported in this client's most recent response.
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
            .map_err(|e| MetaError::Transport(format!("invalid URL: {e}")))?;
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

    /// Follows `paging.cursors.after` while there is a `paging.next`, up to `max_items`.
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

    /// Like [`get_all`](Self::get_all), starting from `start`.
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

    /// Form-encoded POST (strings as they are, everything else as JSON).
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

    /// Multipart POST (video upload). `make_form` rebuilds the form on every attempt,
    /// because a `Form` cannot be resent.
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
                // No connection: the request never left, so it is safe to repeat.
                (_, _, true) => MetaError::Transport(net_text(e)),
                (Kind::Write, true, _) => MetaError::MaybeApplied {
                    cause: "timed out".into(),
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
                    return Err(MetaError::Decode("the response is not JSON".into()));
                }
                Err(_) => Value::Null,
            }
        };
        // Meta sometimes answers 200 with `error` in the body (seen when creating an ad).
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
