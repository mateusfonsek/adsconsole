//! Shared helpers for the Google Ads tests (wiremock + in-memory Db).
#![allow(dead_code)]

pub mod meta;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adsconsole_core::CoreError;
use adsconsole_core::Platform;
use adsconsole_core::db::{AccountInfo, Db};
use adsconsole_core::secrets::{Secret, SecretStore};
use adsconsole_google_ads::client_json::ClientCreds;
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_google_ads::retry::RetryPolicy;
use adsconsole_mcp::Call;
use adsconsole_mcp::google::GoogleAdapter;
use adsconsole_mcp::google::connections::{AccessLevel, save_connection};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Connection seeded by [`google_harness`].
pub const CONN: &str = "g_test";
/// Cloud project of the seeded connection (it is the key of `google_ops`).
pub const PROJECT: &str = "proj";

/// In-memory `SecretStore` that exposes the stored keys.
#[derive(Default)]
pub struct TestStore(Mutex<BTreeMap<String, String>>);

impl TestStore {
    pub fn keys(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

impl SecretStore for TestStore {
    fn get(&self, key: &str) -> Result<Option<String>, CoreError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), CoreError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

pub struct GoogleHarness {
    pub server: MockServer,
    pub db: Arc<Db>,
    pub secrets: Arc<TestStore>,
    pub adapter: Arc<GoogleAdapter>,
    pub connection_id: String,
}

pub fn endpoints(server: &MockServer) -> GoogleEndpoints {
    GoogleEndpoints {
        api_base: format!("{}/v26", server.uri()),
        upload_base: format!("{}/upload", server.uri()),
        auth_url: "https://accounts.example/auth".into(),
        token_url: format!("{}/token", server.uri()),
    }
}

pub fn account(id: &str, currency: &str, status: &str, manager: Option<&str>) -> AccountInfo {
    AccountInfo {
        account_id: id.into(),
        name: format!("Account {id}"),
        currency: currency.into(),
        timezone: Some("America/Sao_Paulo".into()),
        status: Some(status.into()),
        manager_id: manager.map(str::to_string),
    }
}

/// Stores a connection (secrets + `connections_meta`) and its account cache.
pub fn seed_connection(
    h: &GoogleHarness,
    connection_id: &str,
    refresh: &str,
    level: AccessLevel,
    accounts: &[AccountInfo],
) {
    let creds = ClientCreds {
        client_id: "cid".into(),
        client_secret: Secret::new("csecret"),
        project_id: PROJECT.into(),
    };
    save_connection(
        &h.adapter,
        connection_id,
        &format!("Connection {connection_id}"),
        &creds,
        &Secret::new(refresh),
        None,
        level,
    )
    .unwrap();
    h.db.replace_accounts(Platform::GoogleAds, connection_id, accounts)
        .unwrap();
}

/// Starts wiremock (token always ok), an in-memory `Db`, a [`TestStore`] and the
/// [`CONN`] connection (Explorer, project [`PROJECT`]) with accounts 111 (BRL, ENABLED),
/// 999 (MCC, `MANAGER`) and 222 (BRL, ENABLED, child of 999).
pub async fn google_harness() -> GoogleHarness {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "tok", "expires_in": 3600, "refresh_token": "r_new"
        })))
        .mount(&server)
        .await;
    let db = Arc::new(Db::open_in_memory().unwrap());
    let secrets = Arc::new(TestStore::default());
    let store: Arc<dyn SecretStore> = secrets.clone();
    let adapter = GoogleAdapter::with_retry(
        db.clone(),
        store,
        endpoints(&server),
        RetryPolicy {
            base: Duration::from_millis(1),
        },
    );
    let h = GoogleHarness {
        server,
        db,
        secrets,
        adapter,
        connection_id: CONN.into(),
    };
    seed_connection(
        &h,
        CONN,
        "r_test",
        AccessLevel::Explorer,
        &[
            account("111", "BRL", "ENABLED", None),
            account("999", "BRL", "MANAGER", None),
            account("222", "BRL", "ENABLED", Some("999")),
        ],
    );
    h
}

pub fn call(connection_id: &str, account_id: &str, tool: &str, input: Value) -> Call {
    Call {
        connection_id: connection_id.into(),
        account_id: account_id.into(),
        tool: tool.into(),
        input,
    }
}

fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// Plays the browser in OAuth: GET on the redirect_uri with `code` and the URL's `state`.
pub fn browser() -> impl Fn(&str) -> std::io::Result<()> + Send + Sync {
    |url: &str| {
        let q = query(url);
        let target = format!("{}/?code=abc&state={}", q["redirect_uri"], q["state"]);
        tokio::spawn(async move {
            let _ = reqwest::get(target).await;
        });
        Ok(())
    }
}

/// `googleAds:search` for account `customer` whose body contains `needle` returns `results`.
pub async fn mock_search(server: &MockServer, customer: &str, needle: &str, results: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/v26/customers/{customer}/googleAds:search")))
        .and(body_string_contains(needle))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": results })))
        .mount(server)
        .await;
}

/// Row of `FROM customer`.
pub fn customer_row(id: &str, currency: &str, status: &str, manager: bool) -> Value {
    json!({ "customer": {
        "resourceName": format!("customers/{id}"), "id": id,
        "descriptiveName": format!("Customer {id}"), "currencyCode": currency,
        "timeZone": "America/Sao_Paulo", "status": status, "manager": manager
    }})
}

/// Row of `FROM customer_client`.
pub fn client_row(manager: &str, id: &str, currency: &str, status: &str) -> Value {
    json!({ "customerClient": {
        "resourceName": format!("customers/{manager}/customerClients/{id}"), "id": id,
        "descriptiveName": format!("Child {id}"), "currencyCode": currency,
        "timeZone": "America/Sao_Paulo", "status": status, "manager": false, "level": "1"
    }})
}

/// Body that tells the `FROM customer` query (ends at the JSON quote) from the `customer_client` one.
pub const CUSTOMER_QUERY: &str = "FROM customer\"";
pub const CLIENT_QUERY: &str = "FROM customer_client";

pub async fn mock_accessible(server: &MockServer, ids: &[&str]) {
    let names: Vec<String> = ids.iter().map(|i| format!("customers/{i}")).collect();
    Mock::given(method("GET"))
        .and(path("/v26/customers:listAccessibleCustomers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "resourceNames": names })))
        .mount(server)
        .await;
}

/// Mocks of a connection with 111 (BRL, ENABLED) directly and MCC 999 with child 222.
pub async fn mock_account_tree(server: &MockServer) {
    mock_accessible(server, &["111", "999"]).await;
    mock_search(
        server,
        "111",
        CUSTOMER_QUERY,
        json!([customer_row("111", "BRL", "ENABLED", false)]),
    )
    .await;
    mock_search(
        server,
        "999",
        CUSTOMER_QUERY,
        json!([customer_row("999", "BRL", "ENABLED", true)]),
    )
    .await;
    mock_search(
        server,
        "999",
        CLIENT_QUERY,
        json!([client_row("999", "222", "BRL", "ENABLED")]),
    )
    .await;
}
