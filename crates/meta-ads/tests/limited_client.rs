use std::time::Duration;

use adsconsole_core::secrets::Secret;
use adsconsole_meta_ads::client::MetaClient;
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::limiter::AccountLimiter;
use adsconsole_meta_ads::retry::RetryPolicy;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn account_client_observes_and_refuses() {
    let server = MockServer::start().await;
    let buc = json!({"9": [{"type": "ads_management", "call_count": 96, "total_cputime": 1, "total_time": 1,
        "estimated_time_to_regain_access": 10, "ads_api_access_tier": "development_access"}]});
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-business-use-case-usage", buc.to_string().as_str())
                .set_body_json(json!({"id": "1"})),
        )
        .mount(&server)
        .await;
    let ep = MetaEndpoints {
        graph_base: server.uri(),
        dialog_url: "https://x.test".into(),
    };
    let limiter = AccountLimiter::new();
    let base = MetaClient::new(ep, Secret::new("T"), None)
        .with_retry(RetryPolicy {
            base: Duration::from_millis(1),
        })
        .with_limiter(limiter.clone());
    let acct = base.for_account("act_1");
    acct.get("act_1", &[]).await.unwrap();
    // The response said 96% and 10 min to regain access: the next call never leaves.
    let err = acct.get("act_1", &[]).await.unwrap_err();
    assert!(matches!(err, MetaError::RateLimited { .. }), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    // A call without an account (e.g. me/adaccounts) skips the limiter.
    base.get("me/adaccounts", &[]).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

fn account_client(server: &MockServer) -> MetaClient {
    let ep = MetaEndpoints {
        graph_base: server.uri(),
        dialog_url: "https://x.test".into(),
    };
    MetaClient::new(ep, Secret::new("T"), None)
        .with_retry(RetryPolicy {
            base: Duration::from_millis(1),
        })
        .with_limiter(AccountLimiter::new())
        .for_account("act_1")
}

#[tokio::test]
async fn account_usage_reset_blocks_only_at_100() {
    // Example from the documentation: 9.67% usage with reset_time_duration=100 is a normal response.
    let server = MockServer::start().await;
    let acc = json!({"acc_id_util_pct": 9.67, "reset_time_duration": 100, "ads_api_access_tier": "standard_access"});
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ad-account-usage", acc.to_string().as_str())
                .set_body_json(json!({"id": "1"})),
        )
        .mount(&server)
        .await;
    let c = account_client(&server);
    c.get("act_1", &[]).await.unwrap();
    assert_eq!(c.last_usage().unwrap().regain_after, None);
    let t0 = std::time::Instant::now();
    c.get("act_1", &[]).await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1), "does not wait");

    // At 100%, the reset blocks the account.
    let full = MockServer::start().await;
    let acc = json!({"acc_id_util_pct": 100, "reset_time_duration": 100, "ads_api_access_tier": "standard_access"});
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ad-account-usage", acc.to_string().as_str())
                .set_body_json(json!({"id": "1"})),
        )
        .mount(&full)
        .await;
    let c = account_client(&full);
    c.get("act_1", &[]).await.unwrap();
    assert_eq!(
        c.last_usage().unwrap().regain_after,
        Some(Duration::from_secs(100))
    );
    let err = c.get("act_1", &[]).await.unwrap_err();
    assert!(matches!(err, MetaError::RateLimited { .. }), "{err:?}");

    // On a rate limit error, the reset becomes the retry_after even below 100%.
    let limited = MockServer::start().await;
    let acc = json!({"acc_id_util_pct": 9.67, "reset_time_duration": 100});
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("x-ad-account-usage", acc.to_string().as_str())
                .set_body_json(json!({"error": {"message": "limit", "code": 17}})),
        )
        .mount(&limited)
        .await;
    let ep = MetaEndpoints {
        graph_base: limited.uri(),
        dialog_url: "https://x.test".into(),
    };
    let err = MetaClient::new(ep, Secret::new("T"), None)
        .with_retry(RetryPolicy {
            base: Duration::from_millis(1),
        })
        .get("act_1", &[])
        .await
        .unwrap_err();
    assert!(err.is_rate_limit());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(100)));
}
