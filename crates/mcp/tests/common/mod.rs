//! Helpers compartilhados pelos testes do Google Ads (wiremock + Db em memória).
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

/// Conexão semeada pelo [`google_harness`].
pub const CONN: &str = "g_test";
/// Projeto do Cloud da conexão semeada (é a chave de `google_ops`).
pub const PROJECT: &str = "proj";

/// `SecretStore` em memória que deixa ver as chaves gravadas.
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
        name: format!("Conta {id}"),
        currency: currency.into(),
        timezone: Some("America/Sao_Paulo".into()),
        status: Some(status.into()),
        manager_id: manager.map(str::to_string),
    }
}

/// Grava uma conexão (segredos + `connections_meta`) e o cache de contas dela.
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
        &format!("Conexão {connection_id}"),
        &creds,
        &Secret::new(refresh),
        None,
        level,
    )
    .unwrap();
    h.db.replace_accounts(Platform::GoogleAds, connection_id, accounts)
        .unwrap();
}

/// Sobe o wiremock (token sempre ok), uma `Db` em memória, um [`TestStore`] e a
/// conexão [`CONN`] (Explorer, projeto [`PROJECT`]) com as contas 111 (BRL, ENABLED),
/// 999 (MCC, `MANAGER`) e 222 (BRL, ENABLED, filha da 999).
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

/// Faz o papel do navegador no OAuth: GET no redirect_uri com `code` e o `state` da URL.
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

/// `googleAds:search` da conta `customer` cujo corpo contém `needle` devolve `results`.
pub async fn mock_search(server: &MockServer, customer: &str, needle: &str, results: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/v26/customers/{customer}/googleAds:search")))
        .and(body_string_contains(needle))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": results })))
        .mount(server)
        .await;
}

/// Linha de `FROM customer`.
pub fn customer_row(id: &str, currency: &str, status: &str, manager: bool) -> Value {
    json!({ "customer": {
        "resourceName": format!("customers/{id}"), "id": id,
        "descriptiveName": format!("Cliente {id}"), "currencyCode": currency,
        "timeZone": "America/Sao_Paulo", "status": status, "manager": manager
    }})
}

/// Linha de `FROM customer_client`.
pub fn client_row(manager: &str, id: &str, currency: &str, status: &str) -> Value {
    json!({ "customerClient": {
        "resourceName": format!("customers/{manager}/customerClients/{id}"), "id": id,
        "descriptiveName": format!("Filha {id}"), "currencyCode": currency,
        "timeZone": "America/Sao_Paulo", "status": status, "manager": false, "level": "1"
    }})
}

/// Corpo que distingue a consulta `FROM customer` (termina na aspa do JSON) da de `customer_client`.
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

/// Mocks de uma conexão com 111 (BRL, ENABLED) direta e a MCC 999 com a filha 222.
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
