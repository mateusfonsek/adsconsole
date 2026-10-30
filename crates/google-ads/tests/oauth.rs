use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adsconsole_core::secrets::Secret;
use adsconsole_google_ads::client_json::{ClientCreds, ClientJsonError};
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_google_ads::error::GoogleError;
use adsconsole_google_ads::oauth;
use adsconsole_google_ads::token::TokenManager;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type SeenUrl = Arc<Mutex<Option<String>>>;

fn creds() -> ClientCreds {
    ClientCreds {
        client_id: "cid".into(),
        client_secret: Secret::new("csecret"),
        project_id: "proj".into(),
    }
}

fn endpoints(server: &MockServer) -> GoogleEndpoints {
    GoogleEndpoints {
        api_base: format!("{}/v26", server.uri()),
        upload_base: format!("{}/upload", server.uri()),
        auth_url: "https://accounts.example/auth".into(),
        token_url: format!("{}/token", server.uri()),
    }
}

fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// Faz o papel do navegador: GET no redirect_uri com os parâmetros extras.
fn browser(
    extra: impl Fn(&HashMap<String, String>) -> String + Send + Sync + 'static,
) -> (SeenUrl, impl Fn(&str) -> std::io::Result<()> + Send + Sync) {
    let seen = Arc::new(Mutex::new(None));
    let seen2 = seen.clone();
    let open = move |url: &str| {
        *seen2.lock().unwrap() = Some(url.to_string());
        let q = query(url);
        let target = format!("{}{}", q["redirect_uri"], extra(&q));
        tokio::spawn(async move {
            let _ = reqwest::get(target).await;
        });
        Ok(())
    };
    (seen, open)
}

#[test]
fn client_json_kinds() {
    let ok = br#"{"installed":{"client_id":"a.apps","client_secret":"s","project_id":"p1"}}"#;
    let c = ClientCreds::from_json(ok).unwrap();
    assert_eq!(c.client_id, "a.apps");
    assert_eq!(c.client_secret.expose(), "s");
    assert_eq!(c.project_id, "p1");
    assert!(!format!("{c:?}").contains("\"s\""));

    let web = br#"{"web":{"client_id":"a","client_secret":"s"}}"#;
    let e = ClientCreds::from_json(web).unwrap_err();
    assert!(matches!(e, ClientJsonError::WebClient));
    assert_eq!(
        e.to_string(),
        "Este JSON é de um client do tipo Aplicativo da Web. Crie um client do tipo App para computador."
    );

    assert!(matches!(
        ClientCreds::from_json(b"{}").unwrap_err(),
        ClientJsonError::NotOAuthClient
    ));
    assert!(matches!(
        ClientCreds::from_json(br#"{"installed":{"client_id":"a","project_id":"p"}}"#).unwrap_err(),
        ClientJsonError::Missing("client_secret")
    ));
    assert!(matches!(
        ClientCreds::from_json(b"nao e json").unwrap_err(),
        ClientJsonError::Invalid(_)
    ));
}

#[tokio::test]
async fn authorize_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29", "expires_in": 3600, "refresh_token": "1//r"
        })))
        .mount(&server)
        .await;

    let (seen, open) = browser(|q| format!("/?code=abc&state={}", q["state"]));
    let refresh = oauth::authorize(
        &endpoints(&server),
        &creds(),
        &open,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(refresh.expose(), "1//r");

    let url = seen.lock().unwrap().clone().unwrap();
    assert!(url.starts_with("https://accounts.example/auth?"));
    let q = query(&url);
    assert_eq!(q["scope"], "https://www.googleapis.com/auth/adwords");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["access_type"], "offline");
    assert_eq!(q["prompt"], "consent");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "cid");
    assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: HashMap<String, String> = url::form_urlencoded::parse(&reqs[0].body)
        .into_owned()
        .collect();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "abc");
    assert_eq!(body["redirect_uri"], q["redirect_uri"]);
    assert_eq!(body["client_secret"], "csecret");
    assert_eq!(body["code_verifier"].len(), 64);
    let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(body["code_verifier"].as_bytes()));
    assert_eq!(q["code_challenge"], expected);
}

#[tokio::test]
async fn authorize_without_refresh_token_hints_revoke() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29", "expires_in": 3600
        })))
        .mount(&server)
        .await;
    let (_, open) = browser(|q| format!("/?code=abc&state={}", q["state"]));
    let err = oauth::authorize(
        &endpoints(&server),
        &creds(),
        &open,
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    match err {
        GoogleError::Auth(m) => assert!(m.contains("myaccount.google.com"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn authorize_wrong_state_is_ignored_then_valid_callback_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29", "expires_in": 3600, "refresh_token": "1//r"
        })))
        .mount(&server)
        .await;
    let wrong_status = Arc::new(Mutex::new(None));
    let ws = wrong_status.clone();
    let open = move |url: &str| {
        let q = query(url);
        let base = q["redirect_uri"].clone();
        let state = q["state"].clone();
        let ws = ws.clone();
        tokio::spawn(async move {
            let r = reqwest::get(format!("{base}/?code=evil&state=errado"))
                .await
                .unwrap();
            *ws.lock().unwrap() = Some(r.status().as_u16());
            let _ = reqwest::get(format!("{base}/?code=abc&state={state}")).await;
        });
        Ok(())
    };
    let refresh = oauth::authorize(
        &endpoints(&server),
        &creds(),
        &open,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(refresh.expose(), "1//r");
    assert_eq!(*wrong_status.lock().unwrap(), Some(400));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn authorize_lone_wrong_state_times_out_and_denied_is_auth() {
    let server = MockServer::start().await;
    let (_, open) = browser(|_| "/?code=abc&state=errado".into());
    let err = oauth::authorize(
        &endpoints(&server),
        &creds(),
        &open,
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, GoogleError::Timeout), "{err:?}");

    let (_, open) = browser(|q| format!("/?error=access_denied&state={}", q["state"]));
    let err = oauth::authorize(
        &endpoints(&server),
        &creds(),
        &open,
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, GoogleError::Auth(ref m) if m == "acesso negado: access_denied"),
        "{err:?}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn authorize_times_out() {
    let server = MockServer::start().await;
    let open = |_: &str| Ok(());
    let err = oauth::authorize(
        &endpoints(&server),
        &creds(),
        &open,
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, GoogleError::Timeout), "{err:?}");
}

async fn token_server(expires_in: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.tok", "expires_in": expires_in
        })))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn token_cached_and_refreshed() {
    let server = token_server(3600).await;
    let tm = TokenManager::new(endpoints(&server), creds(), Secret::new("1//r"));
    assert_eq!(tm.access_token().await.unwrap().expose(), "ya29.tok");
    tm.access_token().await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let body: HashMap<String, String> =
        url::form_urlencoded::parse(&server.received_requests().await.unwrap()[0].body)
            .into_owned()
            .collect();
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "1//r");

    let server = token_server(30).await;
    let tm = TokenManager::new(endpoints(&server), creds(), Secret::new("1//r"));
    tm.access_token().await.unwrap();
    tm.access_token().await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn token_refresh_is_single_flight() {
    let server = token_server(3600).await;
    let tm = Arc::new(TokenManager::new(
        endpoints(&server),
        creds(),
        Secret::new("r"),
    ));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let tm = tm.clone();
            tokio::spawn(async move { tm.access_token().await.unwrap() })
        })
        .collect();
    for h in hs {
        h.await.unwrap();
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_grant_is_reauth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant", "error_description": "Token has been expired or revoked."
        })))
        .mount(&server)
        .await;
    let tm = TokenManager::new(endpoints(&server), creds(), Secret::new("r"));
    assert!(matches!(
        tm.access_token().await.unwrap_err(),
        GoogleError::ReauthRequired
    ));
}
