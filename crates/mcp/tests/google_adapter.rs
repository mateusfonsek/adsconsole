mod common;

use adsconsole_core::Platform;
use adsconsole_core::db::ConnectionStatus;
use adsconsole_core::secrets::{SecretStore, connection_secret_key};
use adsconsole_google_ads::error::{AdsError, GoogleError};
use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::error::codes;
use adsconsole_mcp::google::connections::{
    AccessLevel, AddGoogleConnection, add_google_connection, reconnect_google_connection,
    remove_google_connection,
};
use adsconsole_mcp::google::errors::translate;
use adsconsole_mcp::google::quota_warning;
use common::*;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const CLIENT_JSON: &[u8] =
    br#"{"installed":{"client_id":"new.apps","client_secret":"new-secret","project_id":"novo-proj"}}"#;

fn add_req() -> AddGoogleConnection {
    AddGoogleConnection {
        name: "Minha conexão".into(),
        client_json: CLIENT_JSON.to_vec(),
        login_customer_id: None,
        access_level: AccessLevel::Explorer,
    }
}

fn secret(h: &GoogleHarness, id: &str, name: &str) -> Option<String> {
    h.secrets.get(&connection_secret_key(id, name)).unwrap()
}

fn api(category: &str, code: &str) -> GoogleError {
    api_with(category, code, None)
}

fn api_with(category: &str, code: &str, details: Option<Value>) -> GoogleError {
    GoogleError::Api {
        http_status: 400,
        status: "INVALID_ARGUMENT".into(),
        message: "Request contains an invalid argument.".into(),
        errors: vec![AdsError {
            category: category.into(),
            code: code.into(),
            message: format!("{code} message"),
            field_path: None,
            operation_index: None,
            details,
        }],
        request_id: Some("req-1".into()),
        maybe_applied: false,
    }
}

async fn login_headers(h: &GoogleHarness, customer: &str) -> Vec<Option<String>> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == format!("/v26/customers/{customer}/googleAds:search"))
        .map(|r| {
            r.headers
                .get("login-customer-id")
                .map(|v| v.to_str().unwrap().to_string())
        })
        .collect()
}

#[tokio::test]
async fn add_connection_end_to_end() {
    let h = google_harness().await;
    mock_account_tree(&h.server).await;
    let open = browser();
    let meta = add_google_connection(&h.adapter, add_req(), &open)
        .await
        .unwrap();
    let id = meta.connection_id.clone();
    assert!(id.starts_with("g_"), "{id}");
    assert_eq!(meta.platform, Platform::GoogleAds);
    assert_eq!(meta.status, ConnectionStatus::Ok);
    assert_eq!(meta.extra["project_id"], "novo-proj");
    assert_eq!(meta.extra["access_level"], "explorer");
    assert_eq!(meta.extra["login_customer_id"], Value::Null);
    assert_eq!(secret(&h, &id, "client_id").as_deref(), Some("new.apps"));
    assert_eq!(
        secret(&h, &id, "client_secret").as_deref(),
        Some("new-secret")
    );
    assert_eq!(secret(&h, &id, "refresh_token").as_deref(), Some("r_new"));

    let child = h.db.accounts_for(Platform::GoogleAds, "222").unwrap();
    let (_, child) = child.iter().find(|(c, _)| *c == id).unwrap();
    assert_eq!(child.manager_id.as_deref(), Some("999"));
    assert_eq!(child.status.as_deref(), Some("ENABLED"));
    let mcc = h.db.accounts_for(Platform::GoogleAds, "999").unwrap();
    let (_, mcc) = mcc.iter().find(|(c, _)| *c == id).unwrap();
    assert_eq!(mcc.status.as_deref(), Some("MANAGER"));
    let direct = h.db.accounts_for(Platform::GoogleAds, "111").unwrap();
    let (_, direct) = direct.iter().find(|(c, _)| *c == id).unwrap();
    assert_eq!(direct.manager_id, None);
    assert_eq!(direct.currency, "BRL");
    assert!(h.adapter.connections().contains(&id));
}

#[tokio::test]
async fn add_connection_rejects_web_client() {
    let h = google_harness().await;
    let mut req = add_req();
    req.client_json = br#"{"web":{"client_id":"a","client_secret":"s"}}"#.to_vec();
    let open = |_: &str| -> std::io::Result<()> { panic!("não deveria abrir o navegador") };
    let err = add_google_connection(&h.adapter, req, &open)
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(
        err.message.contains("App para computador"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn add_connection_rolls_back_on_failure() {
    let h = google_harness().await;
    Mock::given(method("GET"))
        .and(path("/v26/customers:listAccessibleCustomers"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"code": 403, "message": "denied", "status": "PERMISSION_DENIED"}
        })))
        .mount(&h.server)
        .await;
    let open = browser();
    let err = add_google_connection(&h.adapter, add_req(), &open)
        .await
        .unwrap_err();
    assert_ne!(err.code, codes::INTERNAL);
    let conns = h.db.list_connections(Platform::GoogleAds).unwrap();
    assert_eq!(conns.len(), 1, "só a conexão semeada sobra");
    assert_eq!(conns[0].connection_id, CONN);
    let expected: Vec<String> = ["client_id", "client_secret", "refresh_token"]
        .iter()
        .map(|n| connection_secret_key(CONN, n))
        .collect();
    assert_eq!(
        h.secrets.keys(),
        expected,
        "só os segredos da conexão semeada"
    );
    assert_eq!(h.adapter.connections(), vec![CONN.to_string()]);
}

#[tokio::test]
async fn mcc_children_dedup_first_manager_wins() {
    let h = google_harness().await;
    mock_accessible(&h.server, &["111", "998", "999"]).await;
    mock_search(
        &h.server,
        "111",
        CUSTOMER_QUERY,
        json!([customer_row("111", "BRL", "ENABLED", false)]),
    )
    .await;
    for mcc in ["998", "999"] {
        mock_search(
            &h.server,
            mcc,
            CUSTOMER_QUERY,
            json!([customer_row(mcc, "BRL", "ENABLED", true)]),
        )
        .await;
        mock_search(
            &h.server,
            mcc,
            CLIENT_QUERY,
            json!([
                client_row(mcc, "222", "BRL", "ENABLED"),
                client_row(mcc, "111", "BRL", "ENABLED")
            ]),
        )
        .await;
    }
    let accounts = h.adapter.list_accounts(CONN).await.unwrap();
    let ids: Vec<&str> = accounts.iter().map(|a| a.account_id.as_str()).collect();
    assert_eq!(ids.iter().filter(|i| **i == "222").count(), 1, "{ids:?}");
    assert_eq!(ids.iter().filter(|i| **i == "111").count(), 1, "{ids:?}");
    let child = accounts.iter().find(|a| a.account_id == "222").unwrap();
    assert_eq!(child.manager_id.as_deref(), Some("998"));
    let direct = accounts.iter().find(|a| a.account_id == "111").unwrap();
    assert_eq!(direct.manager_id, None);
    // A consulta de customer vai sem login; a de customer_client, com login = MCC.
    for r in h.server.received_requests().await.unwrap() {
        let body = String::from_utf8_lossy(&r.body).to_string();
        let login = r
            .headers
            .get("login-customer-id")
            .map(|v| v.to_str().unwrap());
        if body.contains(CLIENT_QUERY) {
            assert!(body.contains("customer_client.level = 1"), "{body}");
            let mcc = r.url.path().split('/').nth(3).unwrap();
            assert_eq!(login, Some(mcc));
        } else if body.contains(CUSTOMER_QUERY) {
            assert_eq!(login, None);
        }
    }
}

#[tokio::test]
async fn mcc_child_uses_manager_header() {
    let h = google_harness().await;
    for c in ["111", "222"] {
        mock_search(&h.server, c, "FROM campaign", json!([])).await;
    }
    let ctx = h.adapter.ctx(&call(CONN, "222", "x", json!({}))).unwrap();
    assert_eq!(ctx.customer_id, "222");
    assert_eq!(ctx.login.as_deref(), Some("999"));
    assert_eq!(ctx.currency, "BRL");
    assert_eq!(ctx.project_id, PROJECT);
    assert_eq!(ctx.access_level, AccessLevel::Explorer);
    assert_eq!(ctx.connection_id, CONN);
    ctx.client
        .search(
            &ctx.customer_id,
            ctx.login.as_deref(),
            "SELECT campaign.id FROM campaign",
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        login_headers(&h, "222").await,
        vec![Some("999".to_string())]
    );

    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    assert_eq!(ctx.login, None);
    ctx.client
        .search(
            &ctx.customer_id,
            ctx.login.as_deref(),
            "SELECT campaign.id FROM campaign",
            None,
        )
        .await
        .unwrap();
    assert_eq!(login_headers(&h, "111").await, vec![None]);

    let err = h
        .adapter
        .ctx(&call(CONN, "555", "x", json!({})))
        .err()
        .unwrap();
    assert_eq!(err.code, codes::ACCOUNT_NOT_IN_CONNECTION);
}

#[tokio::test]
async fn connection_login_customer_id_is_not_attached_to_direct_accounts() {
    let h = google_harness().await;
    let creds = adsconsole_google_ads::client_json::ClientCreds {
        client_id: "cid".into(),
        client_secret: adsconsole_core::secrets::Secret::new("csecret"),
        project_id: PROJECT.into(),
    };
    adsconsole_mcp::google::connections::save_connection(
        &h.adapter,
        "g_login",
        "Com login",
        &creds,
        &adsconsole_core::secrets::Secret::new("r_test"),
        Some("123-456-7890"),
        AccessLevel::Basic,
    )
    .unwrap();
    h.db.replace_accounts(
        Platform::GoogleAds,
        "g_login",
        &[account("333", "USD", "ENABLED", None)],
    )
    .unwrap();
    let ctx = h
        .adapter
        .ctx(&call("g_login", "333", "x", json!({})))
        .unwrap();
    // A conta é acessível direto (sem MCC no cache): o login da conexão, que pode ser
    // de uma MCC sem relação com ela, não vai no header.
    assert_eq!(ctx.login, None);
}

#[tokio::test]
async fn invalid_grant_marks_needs_reconnect() {
    let h = google_harness().await;
    seed_connection(
        &h,
        "g_bad",
        "r_bad",
        AccessLevel::Explorer,
        &[account("555", "BRL", "ENABLED", None)],
    );
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("refresh_token=r_bad"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .with_priority(1)
        .mount(&h.server)
        .await;
    mock_search(&h.server, "111", "FROM campaign", json!([])).await;

    // Pelo Ctx: translate marca a conexão.
    let ctx = h
        .adapter
        .ctx(&call("g_bad", "555", "x", json!({})))
        .unwrap();
    let e = ctx
        .client
        .search("555", None, "SELECT campaign.id FROM campaign", None)
        .await
        .unwrap_err();
    let err = translate(e, Some(&ctx), "get_campaigns");
    assert_eq!(err.code, codes::CONNECTION_NEEDS_RECONNECT);
    assert!(err.suggestion.unwrap().contains("Conexões"));
    let status = |id: &str| h.db.get_connection(id).unwrap().unwrap().status;
    assert_eq!(status("g_bad"), ConnectionStatus::NeedsReconnect);
    assert_eq!(h.adapter.connections(), vec![CONN.to_string()]);
    let err = h
        .adapter
        .ctx(&call("g_bad", "555", "x", json!({})))
        .err()
        .unwrap();
    assert_eq!(err.code, codes::CONNECTION_NEEDS_RECONNECT);

    // Sem Ctx (list_accounts): o adapter marca também.
    h.db.set_connection_status("g_bad", ConnectionStatus::Ok)
        .unwrap();
    let err = h.adapter.list_accounts("g_bad").await.unwrap_err();
    assert_eq!(err.code, codes::CONNECTION_NEEDS_RECONNECT);
    assert_eq!(status("g_bad"), ConnectionStatus::NeedsReconnect);

    // A conexão saudável continua respondendo.
    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    ctx.client
        .search("111", None, "SELECT campaign.id FROM campaign", None)
        .await
        .unwrap();
    assert_eq!(status(CONN), ConnectionStatus::Ok);
}

#[tokio::test]
async fn reconnect_swaps_refresh_token_and_clears_cache() {
    let h = google_harness().await;
    mock_search(&h.server, "111", "FROM campaign", json!([])).await;
    mock_account_tree(&h.server).await;
    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    ctx.client
        .search("111", None, "SELECT campaign.id FROM campaign", None)
        .await
        .unwrap();
    h.db.set_connection_status(CONN, ConnectionStatus::NeedsReconnect)
        .unwrap();
    let open = browser();
    let meta = reconnect_google_connection(&h.adapter, CONN, &open)
        .await
        .unwrap();
    assert_eq!(meta.status, ConnectionStatus::Ok);
    assert_eq!(secret(&h, CONN, "refresh_token").as_deref(), Some("r_new"));
    // O cache de contas foi relido depois do login.
    let cached = h.db.list_accounts(Platform::GoogleAds, Some(CONN)).unwrap();
    let direct = cached.iter().find(|(_, a)| a.account_id == "111").unwrap();
    assert_eq!(direct.1.name, "Cliente 111");
    // O OAuth usou o client guardado.
    let reqs = h.server.received_requests().await.unwrap();
    let exchange = reqs
        .iter()
        .find(|r| String::from_utf8_lossy(&r.body).contains("grant_type=authorization_code"))
        .unwrap();
    assert!(String::from_utf8_lossy(&exchange.body).contains("client_id=cid"));

    // Cliente novo: o próximo token usa o refresh novo.
    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    ctx.client
        .search("111", None, "SELECT campaign.id FROM campaign", None)
        .await
        .unwrap();
    let reqs = h.server.received_requests().await.unwrap();
    assert!(
        reqs.iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains("refresh_token=r_new"))
    );
}

#[tokio::test]
async fn remove_connection_deletes_secrets_and_meta() {
    let h = google_harness().await;
    remove_google_connection(&h.adapter, CONN).unwrap();
    assert!(h.db.get_connection(CONN).unwrap().is_none());
    assert!(
        h.db.list_accounts(Platform::GoogleAds, Some(CONN))
            .unwrap()
            .is_empty()
    );
    for name in ["client_id", "client_secret", "refresh_token"] {
        assert_eq!(secret(&h, CONN, name), None);
    }
    assert!(h.adapter.connections().is_empty());
}

#[tokio::test]
async fn budget_minimum_message() {
    let h = google_harness().await;
    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    let budget = |micros: &str| {
        api_with(
            "campaignBudgetError",
            "BUDGET_BELOW_PER_DAY_MINIMUM",
            Some(json!({"budgetPerDayMinimumErrorDetails": {
                "minimumBudgetAmountMicros": micros, "budgetPerDayMinimumMicros": micros
            }})),
        )
    };
    let err = translate(budget("25400000"), Some(&ctx), "create_search_campaign");
    assert_eq!(err.code, codes::BUDGET_TOO_LOW);
    assert_eq!(err.field.as_deref(), Some("daily_budget"));
    assert_eq!(
        err.message,
        "O orçamento diário mínimo nesta conta é 25.4 BRL."
    );

    let mut jpy = ctx.clone();
    jpy.currency = "JPY".into();
    let err = translate(budget("1000000000"), Some(&jpy), "create_search_campaign");
    assert_eq!(err.code, codes::BUDGET_TOO_LOW);
    assert!(
        err.message.contains("1000") && err.message.contains("JPY"),
        "{}",
        err.message
    );
    assert!(!err.message.contains("1000."), "{}", err.message);
}

#[tokio::test]
async fn translations_table() {
    let h = google_harness().await;
    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    let t = |e: GoogleError| translate(e, Some(&ctx), "tool");

    let e = t(api(
        "authorizationError",
        "CLOUD_PROJECT_NOT_APPROVED_FOR_PRODUCTION",
    ));
    assert_eq!(e.code, codes::ACCESS_LEVEL_INSUFFICIENT);
    assert_eq!(
        e.message,
        "O projeto do Google Cloud desta conexão está no nível Test e não acessa contas reais."
    );
    assert!(e.suggestion.unwrap().contains(
        "https://console.cloud.google.com/apis/api/googleads.googleapis.com/overview?project=proj"
    ));

    for code in ["USER_PERMISSION_DENIED", "CUSTOMER_NOT_ENABLED"] {
        assert_eq!(
            t(api("authorizationError", code)).code,
            codes::ACCESS_DENIED
        );
    }
    assert_eq!(
        t(api("authenticationError", "NOT_ADS_USER")).code,
        codes::ACCESS_DENIED
    );

    let e = t(api("biddingError", "CONVERSION_TRACKING_NOT_ENABLED"));
    assert_eq!(e.code, codes::BIDDING_REQUIRES_CONVERSIONS);
    assert_eq!(
        e.suggestion.as_deref(),
        Some("Configure o acompanhamento de conversões ou use bidding maximize_clicks.")
    );

    for code in ["RESOURCE_EXHAUSTED", "RESOURCE_TEMPORARILY_EXHAUSTED"] {
        let e = t(api("quotaError", code));
        assert_eq!(e.code, codes::QUOTA_EXHAUSTED);
        assert!(e.retryable);
    }
    let e = t(GoogleError::Api {
        http_status: 429,
        status: "RESOURCE_EXHAUSTED".into(),
        message: "Resource has been exhausted".into(),
        errors: vec![],
        request_id: None,
        maybe_applied: false,
    });
    assert_eq!(e.code, codes::QUOTA_EXHAUSTED);
    assert!(e.retryable);

    let finding = api_with(
        "policyFindingError",
        "POLICY_FINDING",
        Some(json!({"policyFindingDetails": {"policyTopicEntries": [
            {"topic": "ALCOHOL", "type": "LIMITED"}
        ]}})),
    );
    let e = t(finding);
    assert_eq!(e.code, codes::AD_POLICY);
    assert_eq!(e.details.as_ref().unwrap()["topics"], json!(["ALCOHOL"]));
    let violation = api_with(
        "policyViolationError",
        "POLICY_ERROR",
        Some(json!({"policyViolationDetails": {
            "externalPolicyName": "Trademarks", "key": {"policyName": "trademark"}
        }})),
    );
    let e = t(violation);
    assert_eq!(e.code, codes::AD_POLICY);
    assert_eq!(e.details.as_ref().unwrap()["topics"], json!(["Trademarks"]));
    // Duplicados não adjacentes saem também, na ordem da primeira aparição.
    let mut repeated = api_with(
        "policyFindingError",
        "POLICY_FINDING",
        Some(json!({"policyFindingDetails": {"policyTopicEntries": [
            {"topic": "ALCOHOL"}, {"topic": "GAMBLING"}, {"topic": "ALCOHOL"}
        ]}})),
    );
    if let GoogleError::Api { errors, .. } = &mut repeated {
        let again = errors[0].clone();
        errors.push(again);
    }
    let e = t(repeated);
    assert_eq!(
        e.details.as_ref().unwrap()["topics"],
        json!(["ALCOHOL", "GAMBLING"])
    );

    let e = t(api(
        "mutateError",
        "OPERATION_NOT_PERMITTED_FOR_REMOVED_RESOURCE",
    ));
    assert_eq!(e.code, codes::INVALID_STATE);
    assert_eq!(
        e.message,
        "O objeto já está removido no Google Ads, e objetos removidos não podem ser alterados nem reativados."
    );

    const CONFIRA: &str = "O Google respondeu com erro interno; confira na conta se a mudança foi aplicada antes de repetir.";
    let e = t(GoogleError::Api {
        http_status: 500,
        status: "INTERNAL".into(),
        message: "Internal error encountered.".into(),
        errors: vec![],
        request_id: None,
        maybe_applied: true,
    });
    assert_eq!(e.code, codes::EXECUTION_FAILED);
    assert_eq!(e.suggestion.as_deref(), Some(CONFIRA));
    assert!(!e.retryable);
    let e = t(GoogleError::MaybeApplied {
        cause: "tempo esgotado".into(),
    });
    assert_eq!(e.code, codes::EXECUTION_FAILED);
    assert_eq!(e.suggestion.as_deref(), Some(CONFIRA));

    let e = t(api("fieldError", "REQUIRED"));
    assert_eq!(e.code, codes::API_ERROR);
    assert_eq!(e.message, "REQUIRED message");
    let d = e.details.unwrap();
    assert_eq!(d["request_id"], "req-1");
    assert_eq!(d["errors"][0]["code"], "REQUIRED");

    assert_eq!(
        t(GoogleError::ReauthRequired).code,
        codes::CONNECTION_NEEDS_RECONNECT
    );
    assert_eq!(t(GoogleError::Timeout).code, codes::API_ERROR);
    assert!(t(GoogleError::Timeout).retryable);
}

#[tokio::test]
async fn manager_account_precheck() {
    let h = google_harness().await;
    let err = h
        .adapter
        .precheck(&call(CONN, "999", "update_campaign", json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCOUNT_NOT_ACTIVE);
    assert_eq!(
        err.message,
        "Contas de administrador (MCC) não têm campanhas; use uma conta filha."
    );
    h.adapter
        .precheck(&call(CONN, "111", "update_campaign", json!({})))
        .await
        .unwrap();
    h.db.replace_accounts(
        Platform::GoogleAds,
        CONN,
        &[account("111", "BRL", "CANCELED", None)],
    )
    .unwrap();
    let err = h
        .adapter
        .precheck(&call(CONN, "111", "update_campaign", json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::ACCOUNT_NOT_ACTIVE);
}

#[tokio::test]
async fn quota_warning_at_80_percent() {
    let h = google_harness().await;
    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    h.db.record_google_ops(PROJECT, 2000, chrono::Utc::now())
        .unwrap();
    assert_eq!(quota_warning(&ctx), None);
    h.db.record_google_ops(PROJECT, 304, chrono::Utc::now())
        .unwrap();
    assert_eq!(
        quota_warning(&ctx).as_deref(),
        Some("Esta conexão já usou 2304 de 2880 operações nas últimas 24 h (nível Explorer).")
    );
    let mut standard = ctx.clone();
    standard.access_level = AccessLevel::Standard;
    assert_eq!(quota_warning(&standard), None);
}

#[tokio::test]
async fn ops_meter_writes_google_ops_for_project() {
    let h = google_harness().await;
    mock_search(&h.server, "111", "FROM campaign", json!([])).await;
    let ctx = h.adapter.ctx(&call(CONN, "111", "x", json!({}))).unwrap();
    ctx.client
        .search("111", None, "SELECT campaign.id FROM campaign", None)
        .await
        .unwrap();
    assert_eq!(
        h.db.google_ops_last_24h(PROJECT, chrono::Utc::now())
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn list_accounts_tool_reads_cache_and_refreshes() {
    let h = google_harness().await;
    let out = h
        .adapter
        .read(&call("", "", "list_accounts", json!({})))
        .await
        .unwrap();
    let accounts = out["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 3);
    let child = accounts.iter().find(|a| a["account_id"] == "222").unwrap();
    assert_eq!(
        child,
        &json!({"account_id": "222", "name": "Conta 222", "currency": "BRL",
            "timezone": "America/Sao_Paulo", "status": "ENABLED", "manager_id": "999",
            "connection": CONN})
    );
    assert!(h.server.received_requests().await.unwrap().is_empty());

    // refresh: true consulta o Google e regrava o cache.
    mock_accessible(&h.server, &["111"]).await;
    mock_search(
        &h.server,
        "111",
        CUSTOMER_QUERY,
        json!([customer_row("111", "USD", "ENABLED", false)]),
    )
    .await;
    let out = h
        .adapter
        .read(&call(
            "",
            "",
            "list_accounts",
            json!({"refresh": true, "connection": CONN}),
        ))
        .await
        .unwrap();
    assert_eq!(
        out["accounts"],
        json!([{"account_id": "111", "name": "Cliente 111", "currency": "USD",
            "timezone": "America/Sao_Paulo", "status": "ENABLED", "manager_id": null,
            "connection": CONN}])
    );
    let err = h
        .adapter
        .read(&call(
            "",
            "",
            "list_accounts",
            json!({"connection": "g_nada"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::UNKNOWN_CONNECTION);
}

#[tokio::test]
async fn reconnect_after_removal_writes_nothing() {
    let h = google_harness().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Navegador que só devolve o callback quando o teste mandar.
    let open = move |url: &str| -> std::io::Result<()> {
        tx.send(url.to_string()).unwrap();
        Ok(())
    };
    let user = async {
        let url = rx.recv().await.unwrap();
        remove_google_connection(&h.adapter, CONN).unwrap();
        let q: std::collections::HashMap<String, String> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let target = format!("{}/?code=abc&state={}", q["redirect_uri"], q["state"]);
        let _ = reqwest::get(target).await;
    };
    let (result, ()) = tokio::join!(reconnect_google_connection(&h.adapter, CONN, &open), user);
    let err = result.unwrap_err();
    assert_eq!(err.code, codes::UNKNOWN_CONNECTION);
    assert!(h.secrets.keys().is_empty(), "{:?}", h.secrets.keys());
    assert!(h.db.get_connection(CONN).unwrap().is_none());
}
