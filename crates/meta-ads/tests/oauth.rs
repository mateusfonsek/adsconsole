use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adsconsole_core::secrets::Secret;
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::oauth::{AppCreds, login};
use adsconsole_meta_ads::token::{TokenKind, debug_token, missing_scopes};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn endpoints(server: &MockServer) -> MetaEndpoints {
    MetaEndpoints {
        graph_base: server.uri(),
        dialog_url: "https://www.facebook.test/v26.0/dialog/oauth".into(),
    }
}

fn app() -> AppCreds {
    AppCreds {
        app_id: "123".into(),
        app_secret: Secret::new("S3CR3T-xyz"),
        config_id: "cfg9".into(),
        relay_url: None,
    }
}

fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// Fake browser: receives the dialog URL and makes the requested GETs to the callback.
/// Each item of `visits` is the path+query to request; `{state}` becomes the URL's state.
fn browser(
    visits: Vec<&'static str>,
    seen: Arc<Mutex<Vec<(String, u16)>>>,
) -> impl Fn(&str) -> std::io::Result<()> + Send + Sync {
    move |url: &str| {
        let q = query(url);
        let redirect = q["redirect_uri"].clone();
        let base = redirect.trim_end_matches("/callback").to_string();
        let state = q["state"].clone();
        let visits: Vec<String> = visits
            .iter()
            .map(|v| v.replace("{state}", &state))
            .collect();
        let seen = seen.clone();
        seen.lock().unwrap().push((url.to_string(), 0));
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            for v in visits {
                let status = client
                    .get(format!("{base}{v}"))
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
                    .unwrap_or(0);
                seen.lock().unwrap().push((v, status));
            }
        });
        Ok(())
    }
}

fn form(r: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(&r.body).into_owned().collect()
}

/// The request's (form) body has `key=value`.
fn body_has(key: &'static str, value: &'static str) -> impl wiremock::Match {
    move |r: &Request| form(r).get(key).is_some_and(|v| v == value)
}

async fn mock_exchanges(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/oauth/access_token"))
        .and(query_param_is_missing("client_secret"))
        .and(body_has("grant_type", "fb_exchange_token"))
        .and(body_has("fb_exchange_token", "s"))
        .and(body_has("client_secret", "S3CR3T-xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "L", "token_type": "bearer", "expires_in": 5183944}),
        ))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/access_token"))
        .and(query_param_is_missing("code"))
        .and(body_has("code", "abc"))
        .and(body_has("client_id", "123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "s", "token_type": "bearer"})),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn login_happy_path() {
    let server = MockServer::start().await;
    mock_exchanges(&server).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let open = browser(
        vec!["/favicon.ico", "/callback?code=abc&state={state}"],
        seen.clone(),
    );
    let token = login(
        &endpoints(&server),
        &app(),
        0,
        &open,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(token.expose(), "L");

    let seen = seen.lock().unwrap().clone();
    let q = query(&seen[0].0);
    assert!(
        seen[0]
            .0
            .starts_with("https://www.facebook.test/v26.0/dialog/oauth?")
    );
    assert_eq!(q["client_id"], "123");
    assert_eq!(q["config_id"], "cfg9");
    assert_eq!(q["response_type"], "code");
    assert!(q["redirect_uri"].starts_with("http://localhost:"));
    assert!(q["redirect_uri"].ends_with("/callback"));
    assert!(q["state"].len() >= 16);
    assert!(!q.contains_key("client_secret"));
    assert_eq!(seen[1], ("/favicon.ico".to_string(), 404));
    // The 2nd exchange uses the redirect_uri of the 1st.
    let requests = server.received_requests().await.unwrap();
    let first = requests
        .iter()
        .find(|r| form(r).get("code").is_some_and(|c| c == "abc"))
        .unwrap();
    let first_q = form(first);
    assert_eq!(first_q["redirect_uri"], q["redirect_uri"]);
}

#[tokio::test]
async fn login_rejects_wrong_state_then_accepts() {
    let server = MockServer::start().await;
    mock_exchanges(&server).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let open = browser(
        vec![
            "/callback?code=abc&state=other",
            "/callback?code=abc&state={state}",
        ],
        seen.clone(),
    );
    let token = login(
        &endpoints(&server),
        &app(),
        0,
        &open,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(token.expose(), "L");
    assert_eq!(seen.lock().unwrap()[1].1, 400);
}

#[tokio::test]
async fn login_denied_and_timeout() {
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let open = browser(
        vec!["/callback?error=access_denied&error_reason=user_denied&state={state}"],
        seen,
    );
    let err = login(
        &endpoints(&server),
        &app(),
        0,
        &open,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(err, MetaError::Auth("access denied: user_denied".into()));

    let idle = |_: &str| Ok(());
    let err = login(
        &endpoints(&server),
        &app(),
        0,
        &idle,
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();
    assert_eq!(err, MetaError::Timeout);
}

#[tokio::test]
async fn login_through_relay() {
    // Published app: Meta does not accept localhost, so it returns the login to an https
    // page that forwards the code to the local port carried in the state.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/access_token"))
        .and(body_has("code", "abc"))
        .and(body_has("redirect_uri", "https://relay.test/oauth/meta/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "s", "token_type": "bearer"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/access_token"))
        .and(body_has("grant_type", "fb_exchange_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "L", "token_type": "bearer", "expires_in": 5183944}),
        ))
        .mount(&server)
        .await;
    let seen = Arc::new(Mutex::new(None));
    let s2 = seen.clone();
    let open = move |url: &str| {
        let q = query(url);
        *s2.lock().unwrap() = Some(q.clone());
        // The redirect page: reads the port from the state and forwards code and state.
        let state = q["state"].clone();
        let port: u16 = state.split('.').next().unwrap().parse().unwrap();
        tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .get(format!(
                    "http://localhost:{port}/callback?code=abc&state={state}"
                ))
                .send()
                .await;
        });
        Ok(())
    };
    let mut app = app();
    app.relay_url = Some("https://relay.test/oauth/meta/".into());
    let token = login(&endpoints(&server), &app, 0, &open, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(token.expose(), "L");
    let q = seen.lock().unwrap().clone().unwrap();
    assert_eq!(q["redirect_uri"], "https://relay.test/oauth/meta/");
    let (port, rest) = q["state"].split_once('.').unwrap();
    assert!(port.parse::<u16>().unwrap() > 0 && rest.len() >= 16);
}

#[tokio::test]
async fn relay_must_be_https() {
    let server = MockServer::start().await;
    let mut app = app();
    app.relay_url = Some("http://relay.test/oauth/meta/".into());
    let open = |_: &str| Ok(());
    let err = login(&endpoints(&server), &app, 0, &open, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn login_on_fixed_port() {
    let server = MockServer::start().await;
    mock_exchanges(&server).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let open = browser(vec!["/callback?code=abc&state={state}"], seen.clone());
    login(
        &endpoints(&server),
        &app(),
        port,
        &open,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let q = query(&seen.lock().unwrap()[0].0);
    assert_eq!(
        q["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );
}

fn all_scopes() -> serde_json::Value {
    json!([
        "ads_read",
        "ads_management",
        "business_management",
        "pages_show_list",
        "pages_read_engagement",
        "pages_manage_ads",
        "public_profile"
    ])
}

#[tokio::test]
async fn debug_token_parses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/debug_token"))
        .and(query_param("input_token", "SYS"))
        .and(query_param_is_missing("access_token"))
        .and(header("authorization", "Bearer 123|secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "app_id": "123", "type": "SYSTEM_USER", "is_valid": true, "expires_at": 0,
            "data_access_expires_at": 0, "scopes": all_scopes(), "user_id": "77"
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/debug_token"))
        .and(query_param("input_token", "USR"))
        .and(query_param_is_missing("access_token"))
        .and(header("authorization", "Bearer USR"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "type": "USER", "is_valid": true, "expires_at": 1797292800,
            "data_access_expires_at": 1800000000, "scopes": ["ads_read"]
        }})))
        .mount(&server)
        .await;
    let ep = endpoints(&server);
    let secret = Secret::new("secret");

    let sys = debug_token(&ep, &Secret::new("SYS"), Some(("123", &secret)))
        .await
        .unwrap();
    assert_eq!(sys.kind, TokenKind::SystemUser);
    assert!(sys.is_valid);
    assert_eq!(sys.expires_at, None);
    assert_eq!(sys.data_access_expires_at, None);
    assert_eq!(sys.user_id.as_deref(), Some("77"));
    assert!(missing_scopes(&sys).is_empty());

    let usr = debug_token(&ep, &Secret::new("USR"), None).await.unwrap();
    assert_eq!(usr.kind, TokenKind::User);
    assert_eq!(usr.expires_at.unwrap().timestamp(), 1797292800);
    assert_eq!(usr.data_access_expires_at.unwrap().timestamp(), 1800000000);
    assert_eq!(
        missing_scopes(&usr),
        [
            "ads_management",
            "business_management",
            "pages_show_list",
            "pages_read_engagement",
            "pages_manage_ads"
        ]
    );
}

#[tokio::test]
async fn debug_token_graph_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/debug_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Invalid OAuth access token.", "type": "OAuthException", "code": 190
        }})))
        .mount(&server)
        .await;
    let err = debug_token(&endpoints(&server), &Secret::new("x"), None)
        .await
        .unwrap_err();
    assert_eq!(err, MetaError::ReauthRequired);
}

/// Server that accepts the connection, reads the request and closes without responding.
async fn dropping_server() -> MetaEndpoints {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            drop(s);
        }
    });
    MetaEndpoints {
        graph_base: format!("http://127.0.0.1:{port}"),
        dialog_url: "https://www.facebook.test/v26.0/dialog/oauth".into(),
    }
}

#[tokio::test]
async fn network_errors_hide_secrets() {
    let ep = dropping_server().await;
    let secret = Secret::new("S3CR3T-xyz");
    let err = debug_token(&ep, &Secret::new("EAAUSR"), Some(("123", &secret)))
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("EAAUSR"), "{text}");
    assert!(!text.contains("S3CR3T-xyz"), "{text}");
    let err = debug_token(&ep, &Secret::new("EAAUSR"), None)
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("EAAUSR"), "{text}");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let open = browser(vec!["/callback?code=abc&state={state}"], seen);
    let err = login(&ep, &app(), 0, &open, Duration::from_secs(5))
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("S3CR3T-xyz"), "{text}");
    assert!(!text.contains("abc"), "{text}");
}
