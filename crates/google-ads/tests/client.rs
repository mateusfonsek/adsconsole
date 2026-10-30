use std::sync::{Arc, Mutex};
use std::time::Duration;

use adsconsole_core::secrets::Secret;
use adsconsole_google_ads::client::{CallKind, GoogleAdsClient};
use adsconsole_google_ads::client_json::ClientCreds;
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_google_ads::error::GoogleError;
use adsconsole_google_ads::meter::{NoopMeter, OpsMeter};
use adsconsole_google_ads::retry::RetryPolicy;
use adsconsole_google_ads::token::TokenManager;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[derive(Default)]
struct Recorder(Mutex<Vec<u32>>);
impl OpsMeter for Recorder {
    fn record(&self, ops: u32) {
        self.0.lock().unwrap().push(ops);
    }
}

async fn setup_with(meter: Arc<dyn OpsMeter>) -> (MockServer, GoogleAdsClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "tok", "expires_in": 3600, "token_type": "Bearer"}),
        ))
        .mount(&server)
        .await;
    let endpoints = GoogleEndpoints {
        api_base: format!("{}/v26", server.uri()),
        upload_base: format!("{}/upload", server.uri()),
        auth_url: "https://accounts.example/auth".into(),
        token_url: format!("{}/token", server.uri()),
    };
    let creds = ClientCreds {
        client_id: "cid".into(),
        client_secret: Secret::new("csecret"),
        project_id: "proj".into(),
    };
    let tokens = Arc::new(TokenManager::new(
        endpoints.clone(),
        creds,
        Secret::new("refresh"),
    ));
    let client = GoogleAdsClient::new(endpoints, tokens, meter).with_retry(RetryPolicy {
        base: Duration::from_millis(1),
    });
    (server, client)
}

async fn setup() -> (MockServer, GoogleAdsClient) {
    setup_with(Arc::new(NoopMeter)).await
}

const SEARCH: &str = "/v26/customers/1/googleAds:search";
const MUTATE: &str = "/v26/customers/1/googleAds:mutate";

async fn count(server: &MockServer, p: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == p)
        .count()
}

#[tokio::test]
async fn headers_and_no_developer_token() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .and(|r: &Request| r.headers.contains_key("developer-token"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .and(header("authorization", "Bearer tok"))
        .and(header("login-customer-id", "999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("search_page2.json")))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .and(header("authorization", "Bearer tok"))
        .and(|r: &Request| !r.headers.contains_key("login-customer-id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .with_priority(3)
        .mount(&server)
        .await;

    let page = client
        .search("1", Some("999"), "SELECT campaign.id FROM campaign", None)
        .await
        .unwrap();
    assert_eq!(page.results.len(), 2);
    let page = client.search("1", None, "SELECT 1", None).await.unwrap();
    assert!(page.results.is_empty());
}

#[tokio::test]
async fn parses_google_ads_failure() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("budget_minimum.json")))
        .mount(&server)
        .await;
    let err = client
        .mutate("1", None, &[json!({"campaignBudgetOperation": {}})], true)
        .await
        .unwrap_err();
    let GoogleError::Api {
        http_status,
        status,
        message,
        errors,
        request_id,
        maybe_applied,
    } = err
    else {
        panic!("esperava Api");
    };
    assert_eq!(http_status, 400);
    assert_eq!(status, "INVALID_ARGUMENT");
    assert_eq!(message, "Request contains an invalid argument.");
    assert_eq!(request_id.as_deref(), Some("req-budget-1"));
    assert!(!maybe_applied);
    assert_eq!(errors.len(), 1);
    let e = &errors[0];
    assert_eq!(e.category, "campaignBudgetError");
    assert_eq!(e.code, "BUDGET_BELOW_PER_DAY_MINIMUM");
    assert_eq!(
        e.field_path.as_deref(),
        Some("operations[0].create.amount_micros")
    );
    assert_eq!(e.operation_index, Some(0));
    assert!(
        e.details
            .as_ref()
            .unwrap()
            .to_string()
            .contains("budgetPerDayMinimumMicros")
    );
}

#[tokio::test]
async fn error_without_details_keeps_message() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(403).set_body_json(
            json!({"error": {"code": 403, "message": "nope", "status": "PERMISSION_DENIED"}}),
        ))
        .mount(&server)
        .await;
    let err = client.search("1", None, "q", None).await.unwrap_err();
    let GoogleError::Api {
        errors, message, ..
    } = &err
    else {
        panic!("esperava Api");
    };
    assert!(errors.is_empty());
    assert_eq!(message, "nope");
    let text = err.to_string();
    assert!(text.contains("nope") && !text.contains("tok"), "{text}");
}

#[tokio::test]
async fn read_retries_then_succeeds() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("search_page2.json")))
        .mount(&server)
        .await;
    let page = client.search("1", None, "q", None).await.unwrap();
    assert_eq!(page.results.len(), 2);
    assert_eq!(count(&server, SEARCH).await, 3);
}

#[tokio::test]
async fn read_gives_up_after_three_retries() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = client.search("1", None, "q", None).await.unwrap_err();
    assert!(matches!(
        err,
        GoogleError::Api {
            http_status: 503,
            maybe_applied: false,
            ..
        }
    ));
    assert_eq!(count(&server, SEARCH).await, 4);
}

#[tokio::test]
async fn mutate_5xx_not_retried() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(500).set_body_json(fixture("server_error.json")))
        .mount(&server)
        .await;
    let err = client
        .mutate("1", None, &[json!({})], false)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        GoogleError::Api {
            http_status: 500,
            maybe_applied: true,
            ..
        }
    ));
    assert!(err.to_string().contains("conta"), "{err}");
    assert_eq!(count(&server, MUTATE).await, 1);
}

#[tokio::test]
async fn mutate_quota_retried() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(429).set_body_json(fixture("quota_exhausted.json")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .and(body_partial_json(
            json!({"partialFailure": false, "validateOnly": false}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mutate_ok.json")))
        .mount(&server)
        .await;
    let out = client
        .mutate("1", None, &[json!({"a": 1}), json!({"b": 2})], false)
        .await
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(count(&server, MUTATE).await, 2);
}

#[tokio::test]
async fn mutate_temporarily_exhausted_code_retried() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(fixture("temporarily_exhausted.json")),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mutate_ok.json")))
        .mount(&server)
        .await;
    client.mutate("1", None, &[json!({})], false).await.unwrap();
    assert_eq!(count(&server, MUTATE).await, 2);
}

#[tokio::test]
async fn validate_only_returns_empty() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .and(body_partial_json(json!({"validateOnly": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    let out = client.mutate("1", None, &[json!({})], true).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn validate_only_5xx_is_retried_like_a_read() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .and(body_partial_json(json!({"validateOnly": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    let out = client.mutate("1", None, &[json!({})], true).await.unwrap();
    assert!(out.is_empty());
    assert_eq!(count(&server, MUTATE).await, 2);
}

#[tokio::test]
async fn validate_only_persistent_5xx_is_never_maybe_applied() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = client
        .mutate("1", None, &[json!({})], true)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            GoogleError::Api {
                maybe_applied: false,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn validate_only_timeout_is_not_maybe_applied() {
    let (server, client) = setup().await;
    let client = client.with_timeout(Duration::from_millis(100));
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&server)
        .await;
    let err = client
        .mutate("1", None, &[json!({})], true)
        .await
        .unwrap_err();
    assert!(matches!(err, GoogleError::Timeout), "{err:?}");
}

#[tokio::test]
async fn search_all_follows_pages() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .and(body_partial_json(json!({"pageToken": "tok2"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("search_page2.json")))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("search_page1.json")))
        .with_priority(2)
        .mount(&server)
        .await;
    assert_eq!(
        client.search_all("1", None, "q", 100).await.unwrap().len(),
        5
    );
    assert_eq!(client.search_all("1", None, "q", 4).await.unwrap().len(), 4);
    assert_eq!(client.search_all("1", None, "q", 2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn meter_counts_ops() {
    let rec = Arc::new(Recorder::default());
    let (server, client) = setup_with(rec.clone()).await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .and(body_partial_json(json!({"pageToken": "tok2"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("search_page2.json")))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("search_page1.json")))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    client.search_all("1", None, "q", 100).await.unwrap();
    client
        .mutate("1", None, &[json!({}), json!({}), json!({})], true)
        .await
        .unwrap();
    assert_eq!(*rec.0.lock().unwrap(), vec![1, 1, 3]);
}

#[tokio::test]
async fn list_accessible_customers_returns_ids() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/v26/customers:listAccessibleCustomers"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("customers.json")))
        .mount(&server)
        .await;
    let ids = client.list_accessible_customers().await.unwrap();
    assert_eq!(ids, vec!["1234567890", "9876543210"]);
}

#[tokio::test]
async fn post_uses_suffix_and_kind() {
    let rec = Arc::new(Recorder::default());
    let (server, client) = setup_with(rec.clone()).await;
    Mock::given(method("POST"))
        .and(path("/v26/customers/1:generateKeywordIdeas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .mount(&server)
        .await;
    let v = client
        .post(
            "1",
            Some("5"),
            ":generateKeywordIdeas",
            &json!({"x": 1}),
            CallKind::Mutate { ops: 2 },
        )
        .await
        .unwrap();
    assert_eq!(v, json!({"results": []}));
    assert_eq!(*rec.0.lock().unwrap(), vec![2]);
}

#[tokio::test]
async fn mutate_timeout_is_maybe_applied_and_not_retried() {
    let (server, client) = setup().await;
    let client = client.with_timeout(Duration::from_millis(100));
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&server)
        .await;
    let err = client
        .mutate("1", None, &[json!({})], false)
        .await
        .unwrap_err();
    assert!(matches!(err, GoogleError::MaybeApplied { .. }), "{err:?}");
    assert!(err.to_string().contains("confira na conta"));
    assert_eq!(count(&server, MUTATE).await, 1);
}

#[tokio::test]
async fn mutate_2xx_malformed_body_is_maybe_applied() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(200).set_body_string("{nao e json"))
        .mount(&server)
        .await;
    let err = client
        .mutate("1", None, &[json!({})], false)
        .await
        .unwrap_err();
    assert!(matches!(err, GoogleError::MaybeApplied { .. }), "{err:?}");
    assert_eq!(count(&server, MUTATE).await, 1);
}

#[tokio::test]
async fn read_timeout_stays_timeout() {
    let (server, client) = setup().await;
    let client = client.with_timeout(Duration::from_millis(100));
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&server)
        .await;
    let err = client.search("1", None, "q", None).await.unwrap_err();
    assert!(matches!(err, GoogleError::Timeout), "{err:?}");
}

#[tokio::test]
async fn meter_counts_every_response_including_retries() {
    let rec = Arc::new(Recorder::default());
    let (server, client) = setup_with(rec.clone()).await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(SEARCH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .mount(&server)
        .await;
    client.search("1", None, "q", None).await.unwrap();
    assert_eq!(*rec.0.lock().unwrap(), vec![1, 1, 1]);
}

#[tokio::test]
async fn mutate_5xx_with_exhausted_body_not_retried() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path(MUTATE))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": {
            "code": 500, "message": "x", "status": "RESOURCE_EXHAUSTED"}})))
        .mount(&server)
        .await;
    let err = client
        .mutate("1", None, &[json!({})], false)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        GoogleError::Api {
            maybe_applied: true,
            ..
        }
    ));
    assert_eq!(count(&server, MUTATE).await, 1);
}
