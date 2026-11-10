//! Helpers for the Meta tests (wiremock + in-memory Db).

use std::sync::Arc;
use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::db::{AccountInfo, Db};
use adsconsole_core::secrets::{Secret, SecretStore};
use adsconsole_mcp::Call;
use adsconsole_mcp::meta::MetaAdapter;
use adsconsole_mcp::meta::connections::{ConnectionExtra, MetaConnKind, save_connection};
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::retry::RetryPolicy;
use chrono::{DateTime, Utc};
use serde_json::Value;
use wiremock::MockServer;

use super::TestStore;

/// Connection seeded by [`meta_harness`].
pub const META_CONN: &str = "m_test";
/// Token of the seeded connection.
pub const META_TOKEN: &str = "TOK";

pub struct MetaHarness {
    pub server: MockServer,
    pub db: Arc<Db>,
    pub secrets: Arc<TestStore>,
    pub adapter: Arc<MetaAdapter>,
}

pub fn meta_endpoints(server: &MockServer) -> MetaEndpoints {
    MetaEndpoints {
        graph_base: server.uri(),
        dialog_url: "https://www.facebook.test/v26.0/dialog/oauth".into(),
    }
}

pub fn meta_account(id: &str, currency: &str, status: &str) -> AccountInfo {
    AccountInfo {
        account_id: id.into(),
        name: format!("Account {id}"),
        currency: currency.into(),
        timezone: Some("America/Sao_Paulo".into()),
        status: Some(status.into()),
        manager_id: None,
    }
}

/// Stores a login connection (token, app secret and `connections_meta`) and its accounts.
pub fn seed_meta_connection(
    h: &MetaHarness,
    connection_id: &str,
    token: &str,
    expires_at: Option<DateTime<Utc>>,
    accounts: &[AccountInfo],
) {
    let extra = ConnectionExtra {
        kind: MetaConnKind::Login,
        app_id: Some("123".into()),
        config_id: Some("cfg".into()),
        user_id: Some("u1".into()),
        relay_url: None,
    };
    save_connection(
        &h.adapter,
        connection_id,
        &format!("Connection {connection_id}"),
        &Secret::new(token),
        None,
        &extra,
        expires_at,
    )
    .unwrap();
    h.db.replace_accounts(Platform::MetaAds, connection_id, accounts)
        .unwrap();
}

/// Wiremock, in-memory `Db`, [`TestStore`] and the [`META_CONN`] connection (no app secret,
/// token [`META_TOKEN`], no expiry) with `act_111` (ACTIVE, BRL) and `act_101` (CLOSED, BRL).
pub async fn meta_harness() -> MetaHarness {
    let server = MockServer::start().await;
    let db = Arc::new(Db::open_in_memory().unwrap());
    let secrets = Arc::new(TestStore::default());
    let store: Arc<dyn SecretStore> = secrets.clone();
    let adapter = MetaAdapter::with_retry(
        db.clone(),
        store,
        meta_endpoints(&server),
        RetryPolicy {
            base: Duration::from_millis(1),
        },
    );
    let h = MetaHarness {
        server,
        db,
        secrets,
        adapter,
    };
    seed_meta_connection(
        &h,
        META_CONN,
        META_TOKEN,
        None,
        &[
            meta_account("act_111", "BRL", "ACTIVE"),
            meta_account("act_101", "BRL", "CLOSED"),
        ],
    );
    h
}

pub fn meta_call(account_id: &str, tool: &str, input: Value) -> Call {
    let mut input = input;
    if let Some(obj) = input.as_object_mut()
        && !account_id.is_empty()
    {
        obj.insert("account_id".into(), Value::String(account_id.into()));
    }
    Call {
        connection_id: META_CONN.into(),
        account_id: account_id.into(),
        tool: tool.into(),
        input,
    }
}

/// Fake browser for the Meta login: reads the `redirect_uri` and the `state` from the
/// dialog URL and does `GET {redirect_uri}?code=abc&state=...`.
pub fn meta_browser() -> impl Fn(&str) -> std::io::Result<()> + Send + Sync {
    |url: &str| {
        let q: std::collections::HashMap<String, String> = url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        // With a return page (https), the browser goes straight back to the state's port.
        let base = match q["state"].split_once('.') {
            Some((port, _)) if q["redirect_uri"].starts_with("https://") => {
                format!("http://localhost:{port}/callback")
            }
            _ => q["redirect_uri"].clone(),
        };
        let target = format!("{base}?code=abc&state={}", q["state"]);
        tokio::spawn(async move {
            let _ = reqwest::get(target).await;
        });
        Ok(())
    }
}
