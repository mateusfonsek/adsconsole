mod common;

use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::db::ConnectionStatus;
use adsconsole_core::secrets::Secret;
use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::error::codes;
use adsconsole_mcp::meta::connections::{
    AddMetaLogin, AddMetaToken, MetaConnKind, add_meta_login_connection, add_meta_token_connection,
    expiring_connections, reconnect_meta_connection,
};
use adsconsole_mcp::meta::errors::translate;
use adsconsole_mcp::meta::scrub;
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::oauth::AppCreds;
use chrono::Utc;
use common::meta::*;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SCOPES: [&str; 6] = [
    "ads_read",
    "ads_management",
    "business_management",
    "pages_show_list",
    "pages_read_engagement",
    "pages_manage_ads",
];

fn app() -> AppCreds {
    AppCreds {
        app_id: "123".into(),
        app_secret: Secret::new("secret"),
        config_id: "cfg9".into(),
        relay_url: None,
    }
}

/// The request body (form) has `key=value`.
fn body_has(key: &'static str, value: &'static str) -> impl wiremock::Match {
    move |r: &Request| url::form_urlencoded::parse(&r.body).any(|(k, v)| k == key && v == value)
}

async fn mock_login(server: &MockServer, scopes: &[&str], expires_in_days: i64) {
    Mock::given(method("POST"))
        .and(path("/oauth/access_token"))
        .and(body_has("grant_type", "fb_exchange_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "LONG"})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/access_token"))
        .and(body_has("code", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "SHORT"})))
        .mount(server)
        .await;
    let expires = Utc::now().timestamp() + expires_in_days * 86400;
    Mock::given(method("GET"))
        .and(path("/debug_token"))
        .and(query_param("input_token", "LONG"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "type": "USER", "is_valid": true, "expires_at": expires, "scopes": scopes, "user_id": "u9"
        }})))
        .mount(server)
        .await;
}

async fn mock_accounts(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/me/adaccounts"))
        .and(query_param("after", "C2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "act_222", "account_id": "222", "name": "Closed", "currency": "BRL",
             "timezone_name": "America/Sao_Paulo", "account_status": 101}
        ]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/adaccounts"))
        .and(|r: &Request| !r.url.query().unwrap_or("").contains("after="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "act_111", "account_id": "111", "name": "Mine", "currency": "BRL",
                      "timezone_name": "America/Sao_Paulo", "account_status": 1,
                      "business": {"id": "b1", "name": "Portfolio"}}],
            "paging": {"cursors": {"after": "C2"}, "next": "https://graph/next"}
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn add_login_connection_end_to_end() {
    let h = meta_harness().await;
    mock_login(&h.server, &SCOPES, 60).await;
    mock_accounts(&h.server).await;
    let open = meta_browser();
    let meta = add_meta_login_connection(
        &h.adapter,
        AddMetaLogin {
            name: "My Meta".into(),
            app: app(),
            port: 0,
        },
        &open,
    )
    .await
    .unwrap();
    assert!(meta.connection_id.starts_with("m_"));
    assert_eq!(meta.platform, Platform::MetaAds);
    let days = (meta.expires_at.unwrap() - Utc::now()).num_days();
    assert!((59..=60).contains(&days), "{days}");
    let keys = h.secrets.keys();
    assert!(keys.contains(&format!("conn/{}/access_token", meta.connection_id)));
    assert!(keys.contains(&format!("conn/{}/app_secret", meta.connection_id)));
    let closed = h.db.accounts_for(Platform::MetaAds, "act_222").unwrap();
    assert_eq!(closed[0].1.status.as_deref(), Some("CLOSED"));
    let active = h.db.accounts_for(Platform::MetaAds, "act_111").unwrap();
    assert!(active.iter().any(|(c, a)| c == &meta.connection_id
        && a.status.as_deref() == Some("ACTIVE")
        && a.timezone.as_deref() == Some("America/Sao_Paulo")));
}

#[tokio::test]
async fn relay_url_is_kept_for_reconnect() {
    let h = meta_harness().await;
    mock_login(&h.server, &SCOPES, 60).await;
    mock_accounts(&h.server).await;
    let open = meta_browser();
    let mut app = app();
    app.relay_url = Some("https://relay.test/oauth/meta/".into());
    let meta = add_meta_login_connection(
        &h.adapter,
        AddMetaLogin {
            name: "Published".into(),
            app,
            port: 0,
        },
        &open,
    )
    .await
    .unwrap();
    assert_eq!(meta.extra["relay_url"], "https://relay.test/oauth/meta/");
    // Reconnect uses the same return page.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let s2 = seen.clone();
    let inner = meta_browser();
    let spy = move |url: &str| {
        *s2.lock().unwrap() = url.to_string();
        inner(url)
    };
    reconnect_meta_connection(&h.adapter, &meta.connection_id, 0, &spy)
        .await
        .unwrap();
    let url = seen.lock().unwrap().clone();
    let q: std::collections::HashMap<String, String> = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(q["redirect_uri"], "https://relay.test/oauth/meta/");
}

#[tokio::test]
async fn missing_scopes_refused_and_cleaned() {
    let h = meta_harness().await;
    mock_login(&h.server, &SCOPES[..5], 60).await;
    let before = h.secrets.keys();
    let open = meta_browser();
    let err = add_meta_login_connection(
        &h.adapter,
        AddMetaLogin {
            name: "x".into(),
            app: app(),
            port: 0,
        },
        &open,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::ACCESS_DENIED);
    assert!(err.message.contains("pages_manage_ads"), "{}", err.message);
    assert_eq!(h.secrets.keys(), before);
    assert_eq!(h.db.list_connections(Platform::MetaAds).unwrap().len(), 1);
}

#[tokio::test]
async fn system_user_token_connection() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/debug_token"))
        .and(query_param("input_token", "SYS"))
        .and(header("authorization", "Bearer 123|secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "type": "SYSTEM_USER", "is_valid": true, "expires_at": 0, "scopes": SCOPES, "user_id": "77"
        }})))
        .mount(&h.server)
        .await;
    mock_accounts(&h.server).await;
    let meta = add_meta_token_connection(
        &h.adapter,
        AddMetaToken {
            name: "System".into(),
            access_token: Secret::new("SYS"),
            app_id: Some("123".into()),
            app_secret: Some(Secret::new("secret")),
        },
    )
    .await
    .unwrap();
    assert_eq!(meta.expires_at, None);
    assert_eq!(meta.extra["kind"], "system_user");
    let open = meta_browser();
    let err = reconnect_meta_connection(&h.adapter, &meta.connection_id, 0, &open)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_STATE);
    let _ = MetaConnKind::SystemUser;

    // An invalid token is refused.
    Mock::given(method("GET"))
        .and(path("/debug_token"))
        .and(query_param("input_token", "BAD"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "type": "USER", "is_valid": false, "expires_at": 0, "scopes": SCOPES
        }})))
        .mount(&h.server)
        .await;
    let err = add_meta_token_connection(
        &h.adapter,
        AddMetaToken {
            name: "Bad".into(),
            access_token: Secret::new("BAD"),
            app_id: None,
            app_secret: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn closed_account_refused_before_any_write() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/act_101"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "act_101", "account_status": 101})),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/act_111"))
        .and(query_param("fields", "account_status"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "act_111", "account_status": 1})),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    let err = h
        .adapter
        .precheck(&meta_call("act_101", "update_campaign", json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCOUNT_NOT_ACTIVE);
    assert!(err.message.contains("is closed;"), "{}", err.message);
    let posts = h
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() != "GET")
        .count();
    assert_eq!(posts, 0);

    let call = meta_call("act_111", "update_campaign", json!({}));
    h.adapter.precheck(&call).await.unwrap();
    h.adapter.precheck(&call).await.unwrap();
}

#[tokio::test]
async fn precheck_updates_cached_status() {
    let h = meta_harness().await;
    Mock::given(method("GET"))
        .and(path("/act_111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "act_111", "account_status": 2})),
        )
        .mount(&h.server)
        .await;
    let err = h
        .adapter
        .precheck(&meta_call("act_111", "update_campaign", json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCOUNT_NOT_ACTIVE);
    assert!(err.message.contains("is disabled;"), "{}", err.message);
    let cached = h.db.accounts_for(Platform::MetaAds, "act_111").unwrap();
    assert_eq!(cached[0].1.status.as_deref(), Some("DISABLED"));
}

#[tokio::test]
async fn code_190_marks_needs_reconnect() {
    let h = meta_harness().await;
    seed_meta_connection(
        &h,
        "m_two",
        "TOK2",
        None,
        &[meta_account("act_222", "BRL", "ACTIVE")],
    );
    Mock::given(method("GET"))
        .and(path("/me/adaccounts"))
        .and(|r: &Request| {
            r.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer TOK")
        })
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Error validating access token: Session has expired", "code": 190, "error_subcode": 463
        }})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/adaccounts"))
        .and(|r: &Request| {
            r.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer TOK2")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "act_222", "name": "Other", "currency": "BRL", "account_status": 1}
        ]})))
        .mount(&h.server)
        .await;
    let err = h.adapter.list_accounts(META_CONN).await.unwrap_err();
    assert_eq!(err.code, codes::CONNECTION_NEEDS_RECONNECT);
    let conn = h.db.get_connection(META_CONN).unwrap().unwrap();
    assert_eq!(conn.status, ConnectionStatus::NeedsReconnect);
    assert_eq!(h.adapter.connections(), ["m_two"]);
    let other = h.adapter.list_accounts("m_two").await.unwrap();
    assert_eq!(other[0].account_id, "act_222");
}

#[tokio::test]
async fn token_expiry_warning() {
    let h = meta_harness().await;
    let soon = Utc::now() + chrono::Duration::days(5);
    let later = Utc::now() + chrono::Duration::days(30);
    seed_meta_connection(
        &h,
        "m_soon",
        "T5",
        Some(soon),
        &[meta_account("act_5", "BRL", "ACTIVE")],
    );
    seed_meta_connection(
        &h,
        "m_later",
        "T30",
        Some(later),
        &[meta_account("act_30", "BRL", "ACTIVE")],
    );

    let mut call = meta_call("", "list_ad_accounts", json!({"connection": "m_soon"}));
    call.connection_id = "m_soon".into();
    let out = h.adapter.read(&call).await.unwrap();
    let warning = out["token_warning"].as_str().unwrap();
    assert!(warning.contains("5 days"), "{warning}");
    assert_eq!(out["accounts"][0]["account_id"], "act_5");

    let mut call = meta_call("", "list_ad_accounts", json!({"connection": "m_later"}));
    call.connection_id = "m_later".into();
    let out = h.adapter.read(&call).await.unwrap();
    assert!(out.get("token_warning").is_none());

    let expiring = expiring_connections(&h.db, Utc::now()).unwrap();
    let ids: Vec<&str> = expiring.iter().map(|c| c.connection_id.as_str()).collect();
    assert_eq!(ids, ["m_soon"]);
}

fn api(code: i64, subcode: Option<i64>) -> MetaError {
    MetaError::Api {
        http_status: 400,
        code,
        subcode,
        message: "meta message".into(),
        user_title: None,
        user_msg: None,
        is_transient: false,
        fbtrace_id: Some("tr".into()),
        blame_fields: vec![],
        maybe_applied: false,
        retry_after: None,
    }
}

#[tokio::test]
async fn translations_table() {
    let t = |e: MetaError| translate(e, None, "test");
    assert_eq!(
        t(MetaError::ReauthRequired).code,
        codes::CONNECTION_NEEDS_RECONNECT
    );
    assert_eq!(t(api(10, None)).code, codes::ACCESS_DENIED);
    assert_eq!(t(api(200, None)).code, codes::ACCESS_DENIED);
    assert_eq!(t(api(294, None)).code, codes::ACCESS_DENIED);
    assert_eq!(t(api(100, Some(4841021))).code, codes::ACCOUNT_NOT_ACTIVE);
    let rate = t(api(17, None));
    assert_eq!(rate.code, codes::RATE_LIMITED);
    assert!(rate.retryable);
    let local = t(MetaError::RateLimited {
        account: "act_1".into(),
        wait: Duration::from_secs(120),
    });
    assert_eq!(local.code, codes::RATE_LIMITED);
    assert_eq!(local.details.unwrap()["retry_after_secs"], 120);
    assert_eq!(t(api(368, None)).code, codes::AD_POLICY);
    assert_eq!(t(api(100, Some(1346003))).code, codes::AD_POLICY);
    let MetaError::Api { .. } = api(100, None) else {
        unreachable!()
    };
    let mut blamed = api(100, Some(1487390));
    if let MetaError::Api {
        blame_fields,
        user_msg,
        ..
    } = &mut blamed
    {
        *blame_fields = vec!["targeting.geo_locations".into()];
        *user_msg = Some("Pick a location.".into());
    }
    let e = t(blamed);
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert_eq!(e.field.as_deref(), Some("targeting.geo_locations"));
    assert_eq!(e.message, "Pick a location.");
    let mut maybe = api(1, None);
    if let MetaError::Api { maybe_applied, .. } = &mut maybe {
        *maybe_applied = true;
    }
    assert_eq!(t(maybe).code, codes::EXECUTION_FAILED);
    assert_eq!(
        t(MetaError::MaybeApplied {
            cause: "timed out".into()
        })
        .code,
        codes::EXECUTION_FAILED
    );
    let dev = t(api(100, Some(1885183)));
    assert_eq!(dev.code, codes::API_ERROR);
    assert!(dev.suggestion.unwrap().contains("existing_post"));
    let other = t(api(2, None));
    assert_eq!(other.code, codes::API_ERROR);
    assert_eq!(other.details.unwrap()["code"], 2);
    assert!(t(MetaError::Timeout).retryable);
}

#[test]
fn page_tokens_never_returned() {
    let v = json!({"data": [
        {"id": "p1", "access_token": "EAAB", "nested": {"access_token": "x", "keep": 1}},
        {"list": [{"access_token": "y"}]}
    ], "access_token": "z"});
    let clean = scrub(v);
    assert!(!clean.to_string().contains("access_token"), "{clean}");
    assert_eq!(clean["data"][0]["nested"]["keep"], 1);
    let _ = Value::Null;
}

#[test]
fn tokens_in_urls_removed() {
    let v = json!({
        "paging": {
            "next": "https://graph.facebook.com/v26.0/act_1/campaigns?access_token=EAAB&limit=25&after=QVF&appsecret_proof=abc",
            "previous": "https://graph.facebook.com/v26.0/act_1/campaigns?access_token=EAAB"
        },
        "list": ["http://x.test/a?appsecret_proof=p&k=1", "text with a loose access_token=EAAB"],
        "keep": "https://www.facebook.com/ads/library/?id=1"
    });
    let clean = scrub(v);
    let next = clean["paging"]["next"].as_str().unwrap();
    assert!(
        !next.contains("EAAB") && !next.contains("appsecret_proof"),
        "{next}"
    );
    assert!(
        next.contains("limit=25") && next.contains("after=QVF"),
        "{next}"
    );
    assert_eq!(
        clean["paging"]["previous"],
        "https://graph.facebook.com/v26.0/act_1/campaigns"
    );
    assert_eq!(clean["list"][0], "http://x.test/a?k=1");
    assert_eq!(clean["list"][1], "text with a loose access_token=EAAB");
    assert_eq!(clean["keep"], "https://www.facebook.com/ads/library/?id=1");
}
