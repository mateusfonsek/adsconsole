use std::time::Duration;

use adsconsole_core::secrets::Secret;
use adsconsole_meta_ads::client::{AccessTier, MetaClient, PageCursor, WriteKind};
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::retry::RetryPolicy;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn client(server: &MockServer, secret: Option<&str>) -> MetaClient {
    let ep = MetaEndpoints {
        graph_base: server.uri(),
        dialog_url: "https://x.test/dialog".into(),
    };
    MetaClient::new(ep, Secret::new("T"), secret.map(Secret::new)).with_retry(RetryPolicy {
        base: Duration::from_millis(1),
    })
}

fn proof(secret: &str, token: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(token.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn params(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[tokio::test]
async fn bearer_and_appsecret_proof() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/act_1/campaigns"))
        .and(query_param("appsecret_proof", proof("S", "T")))
        .and(query_param_is_missing("access_token"))
        .and(|r: &Request| {
            r.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer T")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/act_2/campaigns"))
        .and(query_param_is_missing("appsecret_proof"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;
    client(&server, Some("S"))
        .get("act_1/campaigns", &[("fields", "id".into())])
        .await
        .unwrap();
    client(&server, None)
        .get("act_2/campaigns", &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn parses_graph_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("invalid_param.json")))
        .mount(&server)
        .await;
    let err = client(&server, None)
        .post(
            "act_1/adsets",
            &params(json!({"name": "x"})),
            WriteKind::Write,
        )
        .await
        .unwrap_err();
    let MetaError::Api {
        http_status,
        code,
        subcode,
        user_title,
        user_msg,
        blame_fields,
        fbtrace_id,
        maybe_applied,
        ..
    } = err
    else {
        panic!("esperava Api: {err:?}")
    };
    assert_eq!(http_status, 400);
    assert_eq!(code, 100);
    assert_eq!(subcode, Some(1487390));
    assert_eq!(user_title.as_deref(), Some("Público inválido"));
    assert!(user_msg.unwrap().contains("local"));
    assert_eq!(blame_fields, ["targeting.geo_locations"]);
    assert_eq!(fbtrace_id.as_deref(), Some("AbCdEf123"));
    assert!(!maybe_applied);
}

#[tokio::test]
async fn error_in_200_body_is_api_error() {
    // Visto na conta real: o POST do anúncio voltou 200 com `error` no corpo.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": {
            "message": "Bloqueado", "code": 31, "error_subcode": 3858385,
            "error_user_msg": "Acreditamos que alguém pode ter acessado sua conta."
        }})))
        .mount(&server)
        .await;
    let err = client(&server, None)
        .post("act_1/ads", &params(json!({"name": "x"})), WriteKind::Write)
        .await
        .unwrap_err();
    let MetaError::Api {
        code,
        subcode,
        user_msg,
        maybe_applied,
        ..
    } = err
    else {
        panic!("esperava Api: {err:?}")
    };
    assert_eq!(code, 31);
    assert_eq!(subcode, Some(3858385));
    assert!(user_msg.unwrap().contains("Acreditamos"));
    assert!(!maybe_applied);
}

#[tokio::test]
async fn token_error_is_reauth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "Error validating access token", "code": 190, "error_subcode": 463}})))
        .mount(&server)
        .await;
    let err = client(&server, None).get("me", &[]).await.unwrap_err();
    assert_eq!(err, MetaError::ReauthRequired);
}

#[tokio::test]
async fn read_retries_on_rate_limit_and_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("rate_limit.json")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "1"})))
        .mount(&server)
        .await;
    let v = client(&server, None).get("act_1", &[]).await.unwrap();
    assert_eq!(v["id"], "1");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn read_gives_up_after_three_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = client(&server, None).get("act_1", &[]).await.unwrap_err();
    assert!(matches!(
        err,
        MetaError::Api {
            http_status: 503,
            ..
        }
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn write_5xx_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "An unknown error occurred", "code": 1, "is_transient": true}})))
        .mount(&server)
        .await;
    let err = client(&server, None)
        .post(
            "act_1/campaigns",
            &params(json!({"name": "x"})),
            WriteKind::Write,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            MetaError::Api {
                maybe_applied: true,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn write_rate_limit_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"message": "limit", "code": 613}})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "c1"})))
        .mount(&server)
        .await;
    let v = client(&server, None)
        .post(
            "act_1/campaigns",
            &params(json!({"name": "x"})),
            WriteKind::Write,
        )
        .await
        .unwrap();
    assert_eq!(v["id"], "c1");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn write_timeout_is_maybe_applied() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "c1"}))
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&server)
        .await;
    let err = client(&server, None)
        .with_timeout(Duration::from_millis(50))
        .post(
            "act_1/campaigns",
            &params(json!({"name": "x"})),
            WriteKind::Write,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::MaybeApplied { .. }), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn validate_retries_like_read() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;
    let c = client(&server, None);
    let v = c
        .post(
            "act_1/campaigns",
            &params(json!({"name": "x"})),
            WriteKind::Validate,
        )
        .await
        .unwrap();
    assert_eq!(v["success"], true);

    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
        .mount(&slow)
        .await;
    let err = client(&slow, None)
        .with_timeout(Duration::from_millis(30))
        .post(
            "act_1/campaigns",
            &params(json!({"name": "x"})),
            WriteKind::Validate,
        )
        .await
        .unwrap_err();
    assert_eq!(err, MetaError::Timeout);
}

#[tokio::test]
async fn post_encodes_json_values() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/act_1/adsets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "s1"})))
        .mount(&server)
        .await;
    client(&server, None)
        .post(
            "act_1/adsets",
            &params(json!({
                "name": "a b",
                "daily_budget": "2540",
                "targeting": {"geo_locations": {"countries": ["BR"]}},
                "execution_options": ["validate_only"]
            })),
            WriteKind::Write,
        )
        .await
        .unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&req.body)
        .into_owned()
        .collect();
    assert_eq!(form["name"], "a b");
    assert_eq!(form["daily_budget"], "2540");
    assert_eq!(
        serde_json::from_str::<Value>(&form["targeting"]).unwrap(),
        json!({"geo_locations": {"countries": ["BR"]}})
    );
    assert_eq!(form["execution_options"], "[\"validate_only\"]");
}

#[tokio::test]
async fn delete_object() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .expect(1)
        .mount(&server)
        .await;
    let v = client(&server, None).delete("c1").await.unwrap();
    assert_eq!(v["success"], true);
}

#[tokio::test]
async fn get_all_follows_cursors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/act_1/campaigns"))
        .and(query_param("after", "P2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "4"}, {"id": "5"}],
            "paging": {"cursors": {"before": "x", "after": "P3"}}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/act_1/campaigns"))
        .and(query_param_is_missing("after"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "1"}, {"id": "2"}, {"id": "3"}],
            "paging": {"cursors": {"after": "P2"}, "next": "https://graph/next"}
        })))
        .mount(&server)
        .await;
    let c = client(&server, None);
    let all = c.get_all("act_1/campaigns", &[], 100).await.unwrap();
    assert_eq!(all.data.len(), 5);
    assert_eq!(all.next, None, "a 2ª página não tem paging.next");
    let four = c.get_all("act_1/campaigns", &[], 4).await.unwrap();
    assert_eq!(four.data.len(), 4);
    let next = four.next.unwrap();
    assert_eq!(
        next,
        PageCursor {
            after: Some("P2".into()),
            skip: 1
        }
    );
    let rest = c
        .get_from("act_1/campaigns", &[], &next, 100)
        .await
        .unwrap();
    let ids: Vec<&str> = rest
        .data
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["5"]);
    let two = c.get_all("act_1/campaigns", &[], 2).await.unwrap();
    assert_eq!(
        two.next,
        Some(PageCursor {
            after: None,
            skip: 2
        })
    );
    let three = c.get_all("act_1/campaigns", &[], 3).await.unwrap();
    assert_eq!(
        three.next,
        Some(PageCursor {
            after: Some("P2".into()),
            skip: 0
        })
    );
}

#[tokio::test]
async fn usage_headers_parsed() {
    let server = MockServer::start().await;
    let buc = json!({"999": [{"type": "ads_management", "call_count": 9, "total_cputime": 4, "total_time": 30,
        "estimated_time_to_regain_access": 2, "ads_api_access_tier": "development_access"}]});
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-business-use-case-usage", buc.to_string().as_str())
                .set_body_json(json!({"id": "1"})),
        )
        .mount(&server)
        .await;
    let c = client(&server, None);
    assert!(c.last_usage().is_none());
    c.get("act_1", &[]).await.unwrap();
    let u = c.last_usage().unwrap();
    assert_eq!(u.tier, Some(AccessTier::Limited));
    assert_eq!(u.buc_max_pct, Some(30.0));
    assert_eq!(u.regain_after, Some(Duration::from_secs(120)));

    let full = MockServer::start().await;
    let acc = json!({"acc_id_util_pct": 12.5, "reset_time_duration": 0, "ads_api_access_tier": "standard_access"});
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ad-account-usage", acc.to_string().as_str())
                .set_body_json(json!({"id": "1"})),
        )
        .mount(&full)
        .await;
    let c = client(&full, None);
    c.get("act_1", &[]).await.unwrap();
    let u = c.last_usage().unwrap();
    assert_eq!(u.tier, Some(AccessTier::Full));
    assert_eq!(u.buc_max_pct, Some(12.5));
    assert_eq!(u.regain_after, None);
}

#[tokio::test]
async fn rate_limit_error_carries_retry_after() {
    let server = MockServer::start().await;
    let buc = json!({"9": [{"type": "ads_management", "call_count": 100, "total_cputime": 10, "total_time": 10,
        "estimated_time_to_regain_access": 5, "ads_api_access_tier": "development_access"}]});
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("x-business-use-case-usage", buc.to_string().as_str())
                .set_body_json(json!({"error": {"message": "limit", "code": 80004}})),
        )
        .mount(&server)
        .await;
    let err = client(&server, None)
        .get("act_1/insights", &[])
        .await
        .unwrap_err();
    assert!(err.is_rate_limit());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(300)));
    // Espera maior que 60 s: não insiste.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn network_errors_hide_url() {
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
    let ep = MetaEndpoints {
        graph_base: format!("http://127.0.0.1:{port}"),
        dialog_url: "https://x.test/dialog".into(),
    };
    let c = MetaClient::new(ep, Secret::new("T"), Some(Secret::new("S"))).with_retry(RetryPolicy {
        base: Duration::from_millis(1),
    });
    let p = proof("S", "T");
    let err = c.get("act_1", &[]).await.unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(
        !text.contains(&p) && !text.contains("appsecret_proof"),
        "{text}"
    );
    let err = c
        .post(
            "act_1/campaigns",
            &params(json!({"name": "x"})),
            WriteKind::Write,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::MaybeApplied { .. }), "{err:?}");
    let text = format!("{err} {err:?}");
    assert!(
        !text.contains(&p) && !text.contains("appsecret_proof"),
        "{text}"
    );
}
