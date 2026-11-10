//! Google Ads creation: `create_search_campaign`, `create_pmax_campaign`, `create_demand_gen_campaign` and `create_display_campaign`, against a stateful fake Google
//! (mutate writes to the map and swaps the temporary ids, and search reads by resource name).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::{ChangeFilter, ChangeStatus};
use adsconsole_google_ads::fields::camel;
use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::{ToolError, codes};
use common::*;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

const TOOL: &str = "create_search_campaign";
const MUTATE_PATH: &str = "/v26/customers/111/googleAds:mutate";
const SEARCH_PATH: &str = "/v26/customers/111/googleAds:search";
const BUDGET_TEMP: &str = "customers/111/campaignBudgets/-1";
const CAMPAIGN_TEMP: &str = "customers/111/campaigns/-2";
const BUDGET_REAL: &str = "customers/111/campaignBudgets/101";
const CAMPAIGN_REAL: &str = "customers/111/campaigns/102";

type State = Arc<Mutex<HashMap<String, Value>>>;

// ------------------------------------------------------------- fake Google

/// Real name of a temporary id: `-N` becomes `100 + N` (also in composite ids).
fn real_name(rn: &str) -> String {
    let (head, id) = rn.rsplit_once('/').unwrap();
    let id: Vec<String> = id
        .split('~')
        .map(|p| match p.strip_prefix('-') {
            Some(n) => (100 + n.parse::<i64>().unwrap()).to_string(),
            None => p.to_string(),
        })
        .collect();
    format!("{head}/{}", id.join("~"))
}

fn subst(v: &mut Value, temps: &HashMap<String, String>) {
    match v {
        Value::String(s) => {
            if let Some(r) = temps.get(s.as_str()) {
                *s = r.clone();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| subst(x, temps)),
        Value::Object(m) => m.values_mut().for_each(|x| subst(x, temps)),
        _ => {}
    }
}

/// Collection of a create without a `resourceName` (Google generates the name).
fn collection(key: &str) -> &'static str {
    match key {
        "campaignCriterionOperation" => "campaignCriteria",
        "adGroupCriterionOperation" => "adGroupCriteria",
        "adGroupAdOperation" => "adGroupAds",
        "campaignAssetOperation" => "campaignAssets",
        "assetGroupAssetOperation" => "assetGroupAssets",
        "assetGroupSignalOperation" => "assetGroupSignals",
        other => panic!("unexpected create without resourceName: {other}"),
    }
}

fn mutate(st: &mut HashMap<String, Value>, body: &Value) -> Value {
    let apply = body["validateOnly"] != json!(true);
    let ops = body["mutateOperations"].as_array().unwrap();
    let mut temps = HashMap::new();
    for op in ops {
        let (_, inner) = op.as_object().unwrap().iter().next().unwrap();
        if let Some(n) = inner["create"]["resourceName"].as_str() {
            assert!(n.contains("/-"), "non-temporary name in create: {n}");
            temps.insert(n.to_string(), real_name(n));
        }
    }
    let mut responses = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let (key, inner) = op.as_object().unwrap().iter().next().unwrap();
        let rn = if let Some(rn) = inner["remove"].as_str() {
            if apply {
                st.remove(rn)
                    .unwrap_or_else(|| panic!("remove of missing {rn}"));
            }
            rn.to_string()
        } else {
            let mut obj = inner["create"].clone();
            assert!(obj.is_object(), "unexpected operation: {op}");
            subst(&mut obj, &temps);
            let rn = match obj["resourceName"].as_str() {
                Some(rn) => rn.to_string(),
                None => format!("customers/111/{}/9{i}", collection(key)),
            };
            obj["resourceName"] = json!(rn);
            if apply {
                st.insert(rn.clone(), obj);
            }
            rn
        };
        let result = format!("{}Result", key.trim_end_matches("Operation"));
        responses.push(json!({ result: { "resourceName": rn } }));
    }
    json!({ "mutateOperationResponses": responses })
}

fn search_rows(st: &HashMap<String, Value>, q: &str) -> Vec<Value> {
    let resource: String = q[q.find("FROM ").unwrap() + 5..]
        .chars()
        .take_while(|c| *c != ' ')
        .collect();
    let i = q
        .find("resource_name = '")
        .unwrap_or_else(|| panic!("unexpected query: {q}"));
    let rest = &q[i + 17..];
    let rn = &rest[..rest.find('\'').unwrap()];
    st.get(rn)
        .map(|obj| json!({ camel(&resource): obj }))
        .into_iter()
        .collect()
}

async fn fake(h: &GoogleHarness) -> State {
    let state: State = Arc::new(Mutex::new(HashMap::new()));
    let s = state.clone();
    Mock::given(method("POST"))
        .and(path(SEARCH_PATH))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let rows = search_rows(&s.lock().unwrap(), body["query"].as_str().unwrap());
            ResponseTemplate::new(200).set_body_json(json!({ "results": rows }))
        })
        .mount(&h.server)
        .await;
    let s = state.clone();
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(mutate(&mut s.lock().unwrap(), &body))
        })
        .mount(&h.server)
        .await;
    state
}

// ---------------------------------------------------------------- helpers

fn engine(h: &GoogleHarness, mode: WriteMode) -> Engine {
    let settings = ServerSettings {
        write_mode: mode,
        approval_wait_secs: 0,
        ..ServerSettings::default()
    };
    let mut config = Config::default();
    config.servers.insert(Platform::GoogleAds, settings);
    Engine::new(
        h.adapter.clone(),
        h.db.clone(),
        Arc::new(RwLock::new(config)),
    )
    .unwrap()
}

async fn mutate_bodies(h: &GoogleHarness) -> Vec<Value> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("googleAds:mutate"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn keys(ops: &[Value]) -> Vec<String> {
    ops.iter()
        .map(|o| o.as_object().unwrap().keys().next().unwrap().clone())
        .collect()
}

fn create(ops: &[Value], i: usize) -> &Value {
    &ops[i].as_object().unwrap().values().next().unwrap()["create"]
}

fn texts(prefix: &str, n: usize) -> Value {
    Value::Array((0..n).map(|i| json!(format!("{prefix} {i}"))).collect())
}

fn rsa() -> Value {
    json!({ "headlines": texts("Headline", 3), "descriptions": texts("Description", 2),
            "final_url": "https://example.com" })
}

/// Minimal input: 1 ad group, 2 keywords and 1 RSA.
fn minimal() -> Value {
    json!({
        "account_id": "111",
        "name": "Shoe Search",
        "daily_budget": 50,
        "ad_groups": [{
            "name": "Shoes",
            "keywords": [
                { "text": "dress shoes", "match_type": "PHRASE" },
                { "text": "leather shoes", "match_type": "exact" }
            ],
            "ads": [rsa()]
        }]
    })
}

async fn validate_err(h: &GoogleHarness, account: &str, input: Value) -> ToolError {
    h.adapter
        .validate(&call(CONN, account, TOOL, input))
        .await
        .unwrap_err()
}

async fn assert_invalid(h: &GoogleHarness, input: Value, field: &str) {
    let e = validate_err(h, "111", input).await;
    assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
    assert_eq!(e.field.as_deref(), Some(field), "{e:?}");
}

/// Ops of the single `validateOnly` sent by a passing `validate`.
async fn validated_ops(h: &GoogleHarness, input: Value) -> Vec<Value> {
    h.adapter
        .validate(&call(CONN, "111", TOOL, input))
        .await
        .unwrap();
    let bodies = mutate_bodies(h).await;
    let last = bodies.last().unwrap();
    assert_eq!(last["validateOnly"], true);
    last["mutateOperations"].as_array().unwrap().clone()
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn search_campaign_operations() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = engine.call_tool(TOOL, minimal(), None).await.unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], true);
    assert_eq!(
        out["summary"],
        "Creates Search campaign 'Shoe Search' (paused), 1 ad groups, 2 keywords, \
         budget 50 BRL/day"
    );

    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert_eq!(bodies[0]["validateOnly"], true);
    assert_eq!(bodies[1]["validateOnly"], false);
    assert_eq!(bodies[1]["partialFailure"], false);
    // Same operations in both calls, except the budget name (it has the timestamp).
    let without_budget_name = |b: &Value| {
        let mut ops = b["mutateOperations"].clone();
        ops[0]["campaignBudgetOperation"]["create"]["name"].take();
        ops
    };
    assert_eq!(
        without_budget_name(&bodies[0]),
        without_budget_name(&bodies[1])
    );
    let ops = bodies[1]["mutateOperations"].as_array().unwrap();
    assert_eq!(
        keys(ops),
        [
            "campaignBudgetOperation",
            "campaignOperation",
            "adGroupOperation",
            "adGroupCriterionOperation",
            "adGroupCriterionOperation",
            "adGroupAdOperation",
        ]
    );
    let mut budget = create(ops, 0).clone();
    let budget_name = budget["name"].take();
    assert_budget_name(budget_name.as_str().unwrap(), "Shoe Search");
    assert_eq!(
        budget,
        json!({ "resourceName": BUDGET_TEMP, "name": null,
                "amountMicros": "50000000", "deliveryMethod": "STANDARD",
                "explicitlyShared": false })
    );
    assert_eq!(
        *create(ops, 1),
        json!({ "resourceName": CAMPAIGN_TEMP, "name": "Shoe Search", "status": "PAUSED",
                "advertisingChannelType": "SEARCH", "campaignBudget": BUDGET_TEMP,
                "targetSpend": {},
                "containsEuPoliticalAdvertising": "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING",
                "networkSettings": { "targetGoogleSearch": true, "targetSearchNetwork": false,
                                     "targetContentNetwork": false } })
    );
    let ag = "customers/111/adGroups/-3";
    assert_eq!(
        *create(ops, 2),
        json!({ "resourceName": ag, "name": "Shoes", "campaign": CAMPAIGN_TEMP,
                "type": "SEARCH_STANDARD", "status": "ENABLED" })
    );
    assert_eq!(
        *create(ops, 3),
        json!({ "adGroup": ag, "keyword": { "text": "dress shoes", "matchType": "PHRASE" } })
    );
    assert_eq!(create(ops, 4)["keyword"]["matchType"], "EXACT");
    assert_eq!(
        *create(ops, 5),
        json!({ "adGroup": ag, "status": "ENABLED", "ad": {
            "finalUrls": ["https://example.com"],
            "responsiveSearchAd": {
                "headlines": [{ "text": "Headline 0" }, { "text": "Headline 1" }, { "text": "Headline 2" }],
                "descriptions": [{ "text": "Description 0" }, { "text": "Description 1" }] } } })
    );

    // After: the real campaign, paused.
    let changes = h.db.list_changes(&ChangeFilter::default()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].after,
        Some(json!({ CAMPAIGN_REAL: { "campaign.status": "PAUSED",
                                      "campaign.name": "Shoe Search" } }))
    );
    assert_eq!(changes[0].before, Some(json!({})));
    let st = state.lock().unwrap();
    assert_eq!(st[CAMPAIGN_REAL]["campaignBudget"], BUDGET_REAL);
    assert_eq!(
        st["customers/111/adGroups/103"]["campaign"],
        json!(CAMPAIGN_REAL)
    );
}

#[tokio::test]
async fn search_campaign_full_order_and_fields() {
    let h = google_harness().await;
    fake(&h).await;
    let input = json!({
        "account_id": "111",
        "name": "Full",
        "daily_budget": 30.5,
        "bidding": { "manual_cpc": {} },
        "search_partners": true,
        "display_network": true,
        "locations": ["2076"],
        "languages": ["1014"],
        "start_date": "2026-11-01",
        "end_date": "2026-12-31",
        "negatives": [{ "text": "free", "match_type": "BROAD" }],
        "sitelinks": [
            { "link_text": "Contact", "final_url": "https://example.com/contact",
              "description1": "Talk to us", "description2": "Fast support" },
            { "link_text": "Store", "final_url": "https://example.com/store" }
        ],
        "ad_groups": [
            { "name": "G1", "cpc_bid": 1.25,
              "keywords": [{ "text": "a b", "match_type": "EXACT" }],
              "negatives": [{ "text": "used", "match_type": "PHRASE" }],
              "ads": [{ "headlines": texts("H", 3), "descriptions": texts("D", 2),
                        "final_url": "https://example.com", "path1": "shoes",
                        "path2": "promo" }] },
            { "name": "G2",
              "keywords": [{ "text": "c", "match_type": "BROAD" }],
              "ads": [rsa(), rsa()] }
        ]
    });
    let ops = validated_ops(&h, input).await;
    assert_eq!(
        keys(&ops),
        [
            "campaignBudgetOperation",
            "campaignOperation",
            "campaignCriterionOperation",
            "campaignCriterionOperation",
            "campaignCriterionOperation",
            "assetOperation",
            "campaignAssetOperation",
            "assetOperation",
            "campaignAssetOperation",
            "adGroupOperation",
            "adGroupCriterionOperation",
            "adGroupCriterionOperation",
            "adGroupAdOperation",
            "adGroupOperation",
            "adGroupCriterionOperation",
            "adGroupAdOperation",
            "adGroupAdOperation",
        ]
    );
    assert_eq!(create(&ops, 0)["amountMicros"], "30500000");
    let campaign = create(&ops, 1);
    assert_eq!(campaign["manualCpc"], json!({}));
    assert!(campaign.get("targetSpend").is_none(), "{campaign}");
    assert_eq!(campaign["startDateTime"], "2026-11-01 00:00:00");
    assert_eq!(campaign["endDateTime"], "2026-12-31 23:59:59");
    assert!(campaign.get("startDate").is_none() && campaign.get("endDate").is_none());
    assert_eq!(
        campaign["networkSettings"],
        json!({ "targetGoogleSearch": true, "targetSearchNetwork": true,
                "targetContentNetwork": true })
    );
    assert_eq!(
        *create(&ops, 2),
        json!({ "campaign": CAMPAIGN_TEMP,
                "location": { "geoTargetConstant": "geoTargetConstants/2076" } })
    );
    assert_eq!(
        *create(&ops, 3),
        json!({ "campaign": CAMPAIGN_TEMP,
                "language": { "languageConstant": "languageConstants/1014" } })
    );
    assert_eq!(
        *create(&ops, 4),
        json!({ "campaign": CAMPAIGN_TEMP, "negative": true,
                "keyword": { "text": "free", "matchType": "BROAD" } })
    );
    assert_eq!(
        *create(&ops, 5),
        json!({ "resourceName": "customers/111/assets/-3",
                "finalUrls": ["https://example.com/contact"],
                "sitelinkAsset": { "linkText": "Contact", "description1": "Talk to us",
                                   "description2": "Fast support" } })
    );
    assert_eq!(
        *create(&ops, 6),
        json!({ "campaign": CAMPAIGN_TEMP, "asset": "customers/111/assets/-3",
                "fieldType": "SITELINK" })
    );
    assert_eq!(
        create(&ops, 7)["sitelinkAsset"],
        json!({ "linkText": "Store" })
    );
    assert_eq!(create(&ops, 8)["asset"], "customers/111/assets/-4");
    let g1 = create(&ops, 9);
    assert_eq!(g1["resourceName"], "customers/111/adGroups/-5");
    assert_eq!(g1["cpcBidMicros"], "1250000");
    assert_eq!(create(&ops, 10)["adGroup"], "customers/111/adGroups/-5");
    assert_eq!(
        *create(&ops, 11),
        json!({ "adGroup": "customers/111/adGroups/-5", "negative": true,
                "keyword": { "text": "used", "matchType": "PHRASE" } })
    );
    let rsa = &create(&ops, 12)["ad"]["responsiveSearchAd"];
    assert_eq!(rsa["path1"], "shoes");
    assert_eq!(rsa["path2"], "promo");
    assert_eq!(
        create(&ops, 13)["resourceName"],
        "customers/111/adGroups/-6"
    );
    assert!(create(&ops, 13).get("cpcBidMicros").is_none());
    assert_eq!(create(&ops, 14)["adGroup"], "customers/111/adGroups/-6");
    assert_eq!(create(&ops, 16)["adGroup"], "customers/111/adGroups/-6");
}

#[tokio::test]
async fn bidding_variants() {
    let h = google_harness().await;
    fake(&h).await;
    let cases = [
        (
            json!({ "maximize_clicks": { "cpc_cap": 2.5 } }),
            "targetSpend",
            json!({ "cpcBidCeilingMicros": "2500000" }),
        ),
        (
            json!({ "maximize_conversions": { "target_cpa": 40 } }),
            "maximizeConversions",
            json!({ "targetCpaMicros": "40000000" }),
        ),
        (
            json!({ "maximize_conversions": {} }),
            "maximizeConversions",
            json!({}),
        ),
        (
            json!({ "maximize_conversion_value": { "target_roas": 3.5 } }),
            "maximizeConversionValue",
            json!({ "targetRoas": 3.5 }),
        ),
        (json!("manual_cpc"), "manualCpc", json!({})),
    ];
    for (bidding, key, value) in cases {
        let mut input = minimal();
        input["bidding"] = bidding.clone();
        let ops = validated_ops(&h, input).await;
        let campaign = create(&ops, 1);
        assert_eq!(campaign[key], value, "{bidding}: {campaign}");
        for other in [
            "targetSpend",
            "maximizeConversions",
            "maximizeConversionValue",
            "manualCpc",
        ] {
            if other != key {
                assert!(campaign.get(other).is_none(), "{bidding}: {campaign}");
            }
        }
    }
    let mut input = minimal();
    input["bidding"] = json!({ "maximize_conversion_value": { "target_roas": 0 } });
    assert_invalid(&h, input, "bidding.maximize_conversion_value.target_roas").await;
    let mut input = minimal();
    input["bidding"] = json!({ "maximize_clicks": { "cpc_cap": 1.555 } });
    assert_invalid(&h, input, "bidding.maximize_clicks.cpc_cap").await;
}

#[tokio::test]
async fn search_campaign_limits() {
    let h = google_harness().await;
    fake(&h).await;
    type Edit = fn(&mut Value);
    let cases: Vec<(Edit, &str)> = vec![
        (
            |i| i["ad_groups"][0]["ads"][0]["headlines"] = texts("T", 2),
            "ad_groups[0].ads[0].headlines",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["headlines"] = texts("T", 16),
            "ad_groups[0].ads[0].headlines",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["headlines"][1] = json!("x".repeat(31)),
            "ad_groups[0].ads[0].headlines",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["descriptions"] = texts("D", 1),
            "ad_groups[0].ads[0].descriptions",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["descriptions"] = texts("D", 5),
            "ad_groups[0].ads[0].descriptions",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["descriptions"][0] = json!("x".repeat(91)),
            "ad_groups[0].ads[0].descriptions",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["path1"] = json!("x".repeat(16)),
            "ad_groups[0].ads[0].path1",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["path2"] = json!("x".repeat(16)),
            "ad_groups[0].ads[0].path2",
        ),
        (
            |i| i["ad_groups"][0]["ads"][0]["final_url"] = json!("example.com"),
            "ad_groups[0].ads[0].final_url",
        ),
        (|i| i["ad_groups"][0]["ads"] = json!([]), "ad_groups[0].ads"),
        (
            |i| i["ad_groups"][0]["ads"] = json!([rsa(), rsa(), rsa(), rsa()]),
            "ad_groups[0].ads",
        ),
        (
            |i| i["ad_groups"][0]["keywords"] = json!([]),
            "ad_groups[0].keywords",
        ),
        (
            |i| {
                i["ad_groups"][0]["keywords"] = Value::Array(
                    (0..201)
                        .map(|n| json!({ "text": format!("k{n}"), "match_type": "EXACT" }))
                        .collect(),
                )
            },
            "ad_groups[0].keywords",
        ),
        (
            |i| i["ad_groups"][0]["keywords"][1]["match_type"] = json!("FUZZY"),
            "ad_groups[0].keywords[1].match_type",
        ),
        (
            |i| i["ad_groups"][0]["keywords"][0]["text"] = json!(" "),
            "ad_groups[0].keywords[0].text",
        ),
        (
            |i| i["ad_groups"][0]["negatives"] = json!([{ "text": "x" }]),
            "ad_groups[0].negatives[0].match_type",
        ),
        (
            |i| i["ad_groups"][0]["name"] = json!(""),
            "ad_groups[0].name",
        ),
        (|i| i["ad_groups"] = json!([]), "ad_groups"),
        (
            |i| {
                let g = i["ad_groups"][0].clone();
                i["ad_groups"] = Value::Array(
                    (0..21)
                        .map(|n| {
                            let mut g = g.clone();
                            g["name"] = json!(format!("G{n}"));
                            g
                        })
                        .collect(),
                );
            },
            "ad_groups",
        ),
        (
            |i| {
                let g = i["ad_groups"][0].clone();
                i["ad_groups"] = json!([g.clone(), g]);
            },
            "ad_groups[1].name",
        ),
        (|i| i["name"] = json!("  "), "name"),
        // Name limit in UTF-8 bytes: 128 "ã" are 128 characters and 256 bytes.
        (|i| i["name"] = json!("ã".repeat(128)), "name"),
        (
            |i| i["ad_groups"][0]["name"] = json!("ã".repeat(128)),
            "ad_groups[0].name",
        ),
        (|i| i["bidding"] = json!("magic_bid"), "bidding"),
        (
            |i| i["bidding"] = json!({ "maximize_clicks": { "cpc_cap": "x" } }),
            "bidding",
        ),
        (|i| i["bidding"] = json!(7), "bidding"),
        (|i| i["daily_budget"] = json!(0), "daily_budget"),
        (|i| i["daily_budget"] = json!(10.555), "daily_budget"),
        (|i| i["locations"] = json!(["abc"]), "locations[0]"),
        (|i| i["languages"] = json!(["1014", "x"]), "languages[1]"),
        (
            |i| i["negatives"] = json!([{ "match_type": "EXACT" }]),
            "negatives[0].text",
        ),
        (|i| i["start_date"] = json!("01/11/2026"), "start_date"),
        (
            |i| {
                i["start_date"] = json!("2026-12-01");
                i["end_date"] = json!("2026-11-01");
            },
            "end_date",
        ),
        (
            |i| {
                i["sitelinks"] =
                    json!([{ "link_text": "x".repeat(26), "final_url": "https://a.com" }])
            },
            "sitelinks[0].link_text",
        ),
        (
            |i| {
                i["sitelinks"] = json!([{ "link_text": "ok", "final_url": "https://a.com",
                                       "description1": "only one" }])
            },
            "sitelinks[0].description2",
        ),
        (
            |i| {
                i["sitelinks"] = json!([{ "link_text": "ok", "final_url": "https://a.com",
                                       "description1": "x".repeat(36), "description2": "y" }])
            },
            "sitelinks[0].description1",
        ),
        (
            |i| i["sitelinks"] = json!([{ "link_text": "ok", "final_url": "ftp://a.com" }]),
            "sitelinks[0].final_url",
        ),
    ];
    for (edit, field) in cases {
        let mut input = minimal();
        edit(&mut input);
        assert_invalid(&h, input, field).await;
    }
    // All local: no call went out.
    assert!(mutate_bodies(&h).await.is_empty());

    // Limits counted in characters, not bytes.
    let mut input = minimal();
    input["ad_groups"][0]["ads"][0]["headlines"][0] = json!("ç".repeat(30));
    input["ad_groups"][0]["ads"][0]["descriptions"][0] = json!("ã".repeat(90));
    input["ad_groups"][0]["ads"][0]["path1"] = json!("é".repeat(15));
    input["sitelinks"] = json!([{ "link_text": "ç".repeat(25), "final_url": "https://a.com",
                                  "description1": "ã".repeat(35), "description2": "õ".repeat(35) }]);
    validated_ops(&h, input).await;
}

#[tokio::test]
async fn search_campaign_undo_removes_campaign_then_budget() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = engine.call_tool(TOOL, minimal(), None).await.unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let change_id = out["change_id"].as_str().unwrap().to_string();

    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    let bodies = mutate_bodies(&h).await;
    let last = bodies.last().unwrap();
    assert_eq!(last["validateOnly"], false);
    assert_eq!(
        last["mutateOperations"],
        json!([
            { "campaignOperation": { "remove": CAMPAIGN_REAL } },
            { "campaignBudgetOperation": { "remove": BUDGET_REAL } }
        ])
    );
    let st = state.lock().unwrap();
    assert!(!st.contains_key(CAMPAIGN_REAL));
    assert!(!st.contains_key(BUDGET_REAL));
    drop(st);
    let orig = h.db.get_change(&change_id).unwrap().unwrap();
    assert_eq!(orig.status, ChangeStatus::Undone);
}

#[tokio::test]
async fn search_campaign_approval_mode() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let engine = engine(&h, WriteMode::Approval);
    let out = engine.call_tool(TOOL, minimal(), None).await.unwrap();
    assert_eq!(out["status"], "pending", "{out}");
    // Pending: only the validateOnly, nothing created.
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["validateOnly"], true);
    assert!(state.lock().unwrap().is_empty());

    let id = out["change_id"].as_str().unwrap().to_string();
    let rec = engine.approve(&id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied, "{rec:?}");
    assert_eq!(state.lock().unwrap()[CAMPAIGN_REAL]["status"], "PAUSED");
    assert_eq!(
        rec.after,
        Some(json!({ CAMPAIGN_REAL: { "campaign.status": "PAUSED",
                                      "campaign.name": "Shoe Search" } }))
    );
}

#[tokio::test]
async fn search_campaign_budget_decimals() {
    let h = google_harness().await;
    h.db.replace_accounts(
        Platform::GoogleAds,
        CONN,
        &[
            account("111", "BRL", "ENABLED", None),
            account("333", "JPY", "ENABLED", None),
        ],
    )
    .unwrap();
    let mut input = minimal();
    input["account_id"] = json!("333");
    input["daily_budget"] = json!(10.5);
    let e = validate_err(&h, "333", input).await;
    assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
    assert_eq!(e.field.as_deref(), Some("daily_budget"));
}

/// `<name> (budget yyyymmdd-HHMMSSmmm)` with up to 255 bytes; `campaign` starts with the
/// part of the name that was kept.
fn assert_budget_name(budget: &str, campaign: &str) {
    assert!(budget.len() <= 255, "{} bytes", budget.len());
    let (head, stamp) = budget.rsplit_once(" (budget ").expect(budget);
    assert!(campaign.starts_with(head), "{budget}");
    let stamp = stamp.strip_suffix(')').expect(budget);
    assert_eq!(stamp.len(), 18, "{stamp}");
    assert!(
        stamp
            .chars()
            .enumerate()
            .all(|(i, c)| if i == 8 { c == '-' } else { c.is_ascii_digit() }),
        "{stamp}"
    );
}

#[tokio::test]
async fn budget_name_is_unique_and_bounded() {
    let h = google_harness().await;
    fake(&h).await;
    // Campaign name with 254 bytes (127 "ã"): accepted, and the budget name is cut.
    let long = "ã".repeat(127);
    let mut input = minimal();
    input["name"] = json!(long);
    input["ad_groups"][0]["name"] = json!("é".repeat(127));
    let ops = validated_ops(&h, input.clone()).await;
    assert_eq!(create(&ops, 1)["name"], json!(long));
    let first = create(&ops, 0)["name"].as_str().unwrap().to_string();
    assert_budget_name(&first, &long);
    assert!(first.starts_with("ããã"), "{first}");
    // The same request twice: different budget names.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let ops = validated_ops(&h, input).await;
    let second = create(&ops, 0)["name"].as_str().unwrap();
    assert_budget_name(second, &long);
    assert_ne!(first, second);
}

#[tokio::test]
async fn cpc_bid_requires_manual() {
    let h = google_harness().await;
    fake(&h).await;
    let mut input = minimal();
    input["ad_groups"][0]["cpc_bid"] = json!(1);
    assert_invalid(&h, input.clone(), "ad_groups[0].cpc_bid").await;
    input["bidding"] = json!({ "maximize_clicks": { "cpc_cap": 2 } });
    assert_invalid(&h, input.clone(), "ad_groups[0].cpc_bid").await;
    input["bidding"] = json!("manual_cpc");
    validated_ops(&h, input).await;
}

#[tokio::test]
async fn spec_is_reversible_create() {
    let h = google_harness().await;
    let spec = h
        .adapter
        .tools()
        .into_iter()
        .find(|t| t.name == TOOL)
        .expect("create_search_campaign registered");
    assert!(spec.reversible);
    assert!(spec.exposed);
    assert_eq!(spec.group, adsconsole_core::config::ToolGroup::Create);
    assert!(!spec.destructive);
}

// ------------------------------------------------------- Performance Max

const PMAX: &str = "create_pmax_campaign";

/// Minimal PNG: signature + `IHDR` with the dimensions.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    b.extend_from_slice(&13u32.to_be_bytes());
    b.extend_from_slice(b"IHDR");
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

struct Images {
    _dir: tempfile::TempDir,
    landscape: String,
    square: String,
    wide: String,
}

impl Images {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: Vec<u8>| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            p.to_string_lossy().into_owned()
        };
        Images {
            landscape: write("landscape.png", png(1200, 628)),
            square: write("square.png", png(1200, 1200)),
            wide: write("wide.png", png(1200, 600)),
            _dir: dir,
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Minimal PMax input: logo, 1 asset group with 3 headlines, 1 long headline,
/// 2 descriptions, 1 1.91:1 image and 1 1:1 image.
fn pmax_minimal(img: &Images) -> Value {
    json!({
        "account_id": "111",
        "name": "PMax Shoes",
        "daily_budget": 80,
        "brand": { "business_name": "Shoes Inc", "logo": { "file_path": img.square } },
        "asset_groups": [{
            "name": "Group 1",
            "final_urls": ["https://example.com"],
            "headlines": ["Shoes", "Genuine leather shoes", "Free shipping"],
            "long_headlines": ["Leather shoes with free shipping across the country"],
            "descriptions": ["Buy today.", "Dress and casual styles with fast delivery."],
            "images": {
                "landscape": [{ "file_path": img.landscape }],
                "square": [{ "file_path": img.square }]
            }
        }]
    })
}

async fn pmax_err(h: &GoogleHarness, input: Value) -> ToolError {
    h.adapter
        .validate(&call(CONN, "111", PMAX, input))
        .await
        .unwrap_err()
}

async fn assert_pmax_invalid(h: &GoogleHarness, input: Value, field: &str) {
    let e = pmax_err(h, input.clone()).await;
    assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
    assert_eq!(e.field.as_deref(), Some(field), "{e:?} {input}");
}

async fn pmax_ops(h: &GoogleHarness, input: Value) -> Vec<Value> {
    h.adapter
        .validate(&call(CONN, "111", PMAX, input))
        .await
        .unwrap();
    let bodies = mutate_bodies(h).await;
    let last = bodies.last().unwrap();
    assert_eq!(last["validateOnly"], true);
    last["mutateOperations"].as_array().unwrap().clone()
}

/// `create` bodies of the operations with key `key`.
fn creates<'a>(ops: &'a [Value], key: &str) -> Vec<&'a Value> {
    ops.iter()
        .filter_map(|o| o.get(key))
        .map(|o| &o["create"])
        .collect()
}

#[tokio::test]
async fn pmax_single_mutate_with_assets() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let img = Images::new();
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(PMAX, pmax_minimal(&img), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], true);
    assert_eq!(
        out["summary"],
        format!(
            "Creates Performance Max campaign 'PMax Shoes' (paused), 1 asset groups, \
             budget 80 BRL/day; images from {}, {}",
            img.square, img.landscape
        )
    );

    // One validateOnly and one real mutate, each with everything.
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert_eq!(bodies[0]["validateOnly"], true);
    assert_eq!(bodies[1]["validateOnly"], false);
    assert_eq!(bodies[1]["partialFailure"], false);
    let ops = bodies[1]["mutateOperations"].as_array().unwrap();
    assert_eq!(
        bodies[0]["mutateOperations"].as_array().unwrap().len(),
        ops.len()
    );

    let landscape = png(1200, 628);
    let square = png(1200, 1200);
    assert_eq!(
        keys(ops),
        [
            "campaignBudgetOperation",
            "campaignOperation",
            // Brand: business name and logo, with the link to the campaign.
            "assetOperation",
            "campaignAssetOperation",
            "assetOperation",
            "campaignAssetOperation",
            // Asset group: 3 headlines, 1 long, 2 descriptions and the 1.91:1 image (the
            // 1:1 is the same file as the logo and reuses its asset).
            "assetOperation",
            "assetOperation",
            "assetOperation",
            "assetOperation",
            "assetOperation",
            "assetOperation",
            "assetOperation",
            "assetGroupOperation",
            "assetGroupAssetOperation",
            "assetGroupAssetOperation",
            "assetGroupAssetOperation",
            "assetGroupAssetOperation",
            "assetGroupAssetOperation",
            "assetGroupAssetOperation",
            "assetGroupAssetOperation",
            "assetGroupAssetOperation",
        ]
    );
    let budget = create(ops, 0);
    assert_eq!(budget["resourceName"], BUDGET_TEMP);
    assert_eq!(budget["explicitlyShared"], false);
    assert_eq!(budget["amountMicros"], "80000000");
    let campaign = create(ops, 1);
    assert_eq!(campaign["resourceName"], CAMPAIGN_TEMP);
    assert_eq!(campaign["advertisingChannelType"], "PERFORMANCE_MAX");
    assert_eq!(campaign["status"], "PAUSED");
    assert_eq!(campaign["campaignBudget"], BUDGET_TEMP);
    assert_eq!(campaign["maximizeConversions"], json!({}));
    assert_eq!(campaign["brandGuidelinesEnabled"], true);
    assert_eq!(
        campaign["assetAutomationSettings"],
        json!([{ "assetAutomationType": "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION",
                 "assetAutomationStatus": "OPTED_OUT" }])
    );

    // Brand.
    assert_eq!(create(ops, 2)["textAsset"], json!({ "text": "Shoes Inc" }));
    let biz = create(ops, 2)["resourceName"].clone();
    assert_eq!(
        *create(ops, 3),
        json!({ "campaign": CAMPAIGN_TEMP, "asset": biz, "fieldType": "BUSINESS_NAME" })
    );
    let logo = create(ops, 4);
    assert_eq!(logo["type"], "IMAGE");
    let logo_name = logo["name"].as_str().unwrap();
    assert!(
        logo_name.starts_with("img-") && logo_name.len() == 20,
        "{logo_name}"
    );
    assert_eq!(logo["imageAsset"]["data"], b64(&square));
    let logo_rn = logo["resourceName"].clone();
    assert_eq!(
        *create(ops, 5),
        json!({ "campaign": CAMPAIGN_TEMP, "asset": logo_rn, "fieldType": "LOGO" })
    );
    let images = creates(ops, "assetOperation")
        .into_iter()
        .filter(|a| a.get("imageAsset").is_some())
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 2, "{images:?}");
    assert_eq!(images[1]["imageAsset"]["data"], b64(&landscape));

    // Asset group and links.
    let group = creates(ops, "assetGroupOperation")[0];
    assert_eq!(group["campaign"], CAMPAIGN_TEMP);
    assert_eq!(group["name"], "Group 1");
    assert_eq!(group["status"], "ENABLED");
    assert_eq!(group["finalUrls"], json!(["https://example.com"]));
    let links = creates(ops, "assetGroupAssetOperation");
    assert!(
        links
            .iter()
            .all(|l| l["assetGroup"] == group["resourceName"])
    );
    let count = |ft: &str| links.iter().filter(|l| l["fieldType"] == ft).count();
    assert_eq!(count("HEADLINE"), 3);
    assert_eq!(count("LONG_HEADLINE"), 1);
    assert_eq!(count("DESCRIPTION"), 2);
    assert_eq!(count("MARKETING_IMAGE"), 1);
    assert_eq!(count("SQUARE_MARKETING_IMAGE"), 1);
    let sq = links
        .iter()
        .find(|l| l["fieldType"] == "SQUARE_MARKETING_IMAGE")
        .unwrap();
    assert_eq!(sq["asset"], logo_rn);

    // Stored in the fake Google with the real names.
    let st = state.lock().unwrap();
    assert_eq!(st[CAMPAIGN_REAL]["status"], "PAUSED");
    assert_eq!(st[CAMPAIGN_REAL]["campaignBudget"], BUDGET_REAL);
    drop(st);
    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        rec.after,
        Some(json!({ CAMPAIGN_REAL: { "campaign.status": "PAUSED",
                                      "campaign.name": "PMax Shoes" } }))
    );
}

#[tokio::test]
async fn pmax_full_options() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, bytes: Vec<u8>| {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    };
    let portrait = write("portrait.png", png(960, 1200));
    let wide_logo = write("wide_logo.png", png(1200, 300));
    let mut input = pmax_minimal(&img);
    input["bidding"] = json!({ "maximize_conversion_value": { "target_roas": 3.5 } });
    input["final_url_expansion"] = json!(true);
    input["locations"] = json!(["2076"]);
    input["languages"] = json!(["1014"]);
    input["brand"]["landscape_logo"] = json!({ "file_path": wide_logo });
    input["brand"]["logo"] = json!({ "asset": "customers/111/assets/77" });
    let group = &mut input["asset_groups"][0];
    group["images"]["portrait"] = json!([{ "file_path": portrait }]);
    group["youtube_video_ids"] = json!(["dQw4w9WgXcQ"]);
    group["signals"] = json!({
        "audiences": ["customers/111/audiences/5"],
        "search_themes": ["dress shoes"]
    });
    let mut second = group.clone();
    second["name"] = json!("Group 2");
    // YouTube link: the extracted id is the same video as group 1 (a single asset).
    second["youtube_video_ids"] = json!(["https://www.youtube.com/watch?v=dQw4w9WgXcQ"]);
    input["asset_groups"].as_array_mut().unwrap().push(second);

    let ops = pmax_ops(&h, input).await;
    let campaign = create(&ops, 1);
    assert_eq!(
        campaign["maximizeConversionValue"],
        json!({ "targetRoas": 3.5 })
    );
    assert!(
        campaign.get("assetAutomationSettings").is_none(),
        "{campaign}"
    );
    assert_eq!(
        keys(&ops)[2..4],
        ["campaignCriterionOperation", "campaignCriterionOperation"]
    );
    let brand = creates(&ops, "campaignAssetOperation");
    assert_eq!(brand.len(), 3);
    assert_eq!(
        *brand[1],
        json!({ "campaign": CAMPAIGN_TEMP, "asset": "customers/111/assets/77",
                "fieldType": "LOGO" })
    );
    assert_eq!(brand[2]["fieldType"], "LANDSCAPE_LOGO");
    // Identical images in two groups become a single asset: wide logo, 1.91:1, 1:1, 4:5.
    let image_assets = creates(&ops, "assetOperation")
        .into_iter()
        .filter(|a| a.get("imageAsset").is_some())
        .count();
    assert_eq!(image_assets, 4);
    assert_eq!(creates(&ops, "assetGroupOperation").len(), 2);
    let videos: Vec<_> = creates(&ops, "assetOperation")
        .into_iter()
        .filter(|a| a.get("youtubeVideoAsset").is_some())
        .collect();
    // The same video in both groups: one asset, two links.
    assert_eq!(videos.len(), 1);
    assert_eq!(
        videos[0]["youtubeVideoAsset"],
        json!({ "youtubeVideoId": "dQw4w9WgXcQ" })
    );
    let links = creates(&ops, "assetGroupAssetOperation");
    assert_eq!(
        links
            .iter()
            .filter(|l| l["fieldType"] == "PORTRAIT_MARKETING_IMAGE")
            .count(),
        2
    );
    assert_eq!(
        links
            .iter()
            .filter(|l| l["fieldType"] == "YOUTUBE_VIDEO")
            .count(),
        2
    );
    let signals = creates(&ops, "assetGroupSignalOperation");
    assert_eq!(signals.len(), 4);
    let ag = creates(&ops, "assetGroupOperation")[0]["resourceName"].clone();
    assert_eq!(
        *signals[0],
        json!({ "assetGroup": ag, "audience": { "audience": "customers/111/audiences/5" } })
    );
    assert_eq!(
        *signals[1],
        json!({ "assetGroup": ag, "searchTheme": { "text": "dress shoes" } })
    );
    // Signals right after the group's own links, before the next group.
    let k = keys(&ops);
    let first_signal = k
        .iter()
        .position(|k| k == "assetGroupSignalOperation")
        .unwrap();
    let second_group = k.iter().rposition(|k| k == "assetGroupOperation").unwrap();
    assert!(first_signal < second_group);
}

#[tokio::test]
async fn pmax_minimums() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let base = pmax_minimal(&img);
    let with = |f: &dyn Fn(&mut Value)| {
        let mut v = base.clone();
        f(&mut v);
        v
    };
    let g = "asset_groups[0]";
    let cases: Vec<(Value, String)> = vec![
        (
            with(&|v| v["asset_groups"][0]["headlines"] = json!(["Shoes", "Free shipping"])),
            format!("{g}.headlines"),
        ),
        (
            with(&|v| {
                v["asset_groups"][0]["headlines"] = json!([
                    "Genuine leather shoes",
                    "Free shipping nationwide",
                    "Durable genuine leather"
                ])
            }),
            format!("{g}.headlines"),
        ),
        (
            with(&|v| v["asset_groups"][0]["headlines"][1] = json!("x".repeat(31))),
            format!("{g}.headlines[1]"),
        ),
        (
            with(&|v| v["asset_groups"][0]["headlines"] = texts("H", 16)),
            format!("{g}.headlines"),
        ),
        (
            with(&|v| v["asset_groups"][0]["long_headlines"] = json!([])),
            format!("{g}.long_headlines"),
        ),
        (
            with(&|v| v["asset_groups"][0]["long_headlines"] = texts("L", 6)),
            format!("{g}.long_headlines"),
        ),
        (
            with(&|v| v["asset_groups"][0]["descriptions"] = json!(["Buy today."])),
            format!("{g}.descriptions"),
        ),
        (
            with(&|v| v["asset_groups"][0]["descriptions"] = texts("D", 6)),
            format!("{g}.descriptions"),
        ),
        (
            with(&|v| {
                v["asset_groups"][0]["descriptions"] = json!([
                    "Dress and casual styles with fast delivery across the whole country.",
                    "Genuine leather, comfortable soles and a warranty for one full year."
                ])
            }),
            format!("{g}.descriptions"),
        ),
        (
            with(&|v| v["asset_groups"][0]["descriptions"][0] = json!("x".repeat(91))),
            format!("{g}.descriptions[0]"),
        ),
        (
            with(&|v| v["asset_groups"][0]["images"]["square"] = json!([])),
            format!("{g}.images.square"),
        ),
        (
            with(&|v| {
                v["asset_groups"][0]["images"]
                    .as_object_mut()
                    .unwrap()
                    .remove("square");
            }),
            format!("{g}.images.square"),
        ),
        (
            with(&|v| v["asset_groups"][0]["images"]["landscape"] = json!([])),
            format!("{g}.images.landscape"),
        ),
        (
            with(&|v| {
                v["asset_groups"][0]["images"]["landscape"] =
                    json!([{ "file_path": img.landscape }, { "file_path": img.wide }])
            }),
            format!("{g}.images.landscape[1]"),
        ),
        (
            with(&|v| v["asset_groups"][0]["images"]["square"] = json!([{ "path": "/a.png" }])),
            format!("{g}.images.square[0]"),
        ),
        (
            with(&|v| {
                v["asset_groups"][0]["images"]["square"] =
                    json!([{ "url": "http://example.com/a.png" }])
            }),
            format!("{g}.images.square[0]"),
        ),
        (
            with(&|v| v["asset_groups"][0]["final_urls"] = json!([])),
            format!("{g}.final_urls"),
        ),
        (
            with(&|v| v["asset_groups"][0]["final_urls"] = json!(["example.com"])),
            format!("{g}.final_urls[0]"),
        ),
        (
            with(&|v| v["asset_groups"][0]["name"] = json!(" ")),
            format!("{g}.name"),
        ),
        (
            with(&|v| v["asset_groups"][0]["youtube_video_ids"] = json!(["https://youtu.be/x"])),
            format!("{g}.youtube_video_ids[0]"),
        ),
        (
            with(&|v| {
                v["asset_groups"][0]["signals"] = json!({ "search_themes": texts("theme", 26) })
            }),
            format!("{g}.signals.search_themes"),
        ),
        (
            with(&|v| {
                v["asset_groups"][0]["signals"] =
                    json!({ "audiences": ["customers/222/audiences/1"] })
            }),
            format!("{g}.signals.audiences[0]"),
        ),
        (
            with(&|v| v["asset_groups"] = json!([])),
            "asset_groups".into(),
        ),
        (
            with(&|v| {
                let g0 = v["asset_groups"][0].clone();
                v["asset_groups"] = json!([
                    g0.clone(),
                    g0.clone(),
                    g0.clone(),
                    g0.clone(),
                    g0.clone(),
                    g0
                ]);
            }),
            "asset_groups".into(),
        ),
        (
            with(&|v| {
                let mut g1 = v["asset_groups"][0].clone();
                g1["name"] = json!("Group 1");
                v["asset_groups"].as_array_mut().unwrap().push(g1);
            }),
            "asset_groups[1].name".into(),
        ),
        (
            with(&|v| v["brand"]["business_name"] = json!("x".repeat(26))),
            "brand.business_name".into(),
        ),
        (
            with(&|v| v["brand"]["logo"] = json!({ "file_path": img.landscape })),
            "brand.logo".into(),
        ),
        (
            with(&|v| v["brand"]["logo"] = json!({ "asset": "customers/222/assets/1" })),
            "brand.logo.asset".into(),
        ),
        (
            with(&|v| v["brand"]["logo"] = json!("logo.png")),
            "brand.logo".into(),
        ),
        (
            with(&|v| v["brand"]["landscape_logo"] = json!({ "file_path": img.square })),
            "brand.landscape_logo".into(),
        ),
        (
            with(&|v| v["bidding"] = json!("maximize_clicks")),
            "bidding".into(),
        ),
        (
            with(&|v| v["bidding"] = json!({ "manual_cpc": {} })),
            "bidding".into(),
        ),
        (
            with(&|v| v["bidding"] = json!({ "maximize_conversions": { "target_cpa": -1 } })),
            "bidding.maximize_conversions.target_cpa".into(),
        ),
        (
            with(&|v| v["daily_budget"] = json!(0)),
            "daily_budget".into(),
        ),
        (with(&|v| v["name"] = json!("")), "name".into()),
        (
            with(&|v| v["locations"] = json!(["abc"])),
            "locations[0]".into(),
        ),
    ];
    for (input, field) in cases {
        assert_pmax_invalid(&h, input, &field).await;
    }
    // Nothing reached Google.
    assert!(mutate_bodies(&h).await.is_empty());
}

#[tokio::test]
async fn pmax_conversion_error_translated() {
    let h = google_harness().await;
    let img = Images::new();
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": {
            "code": 400, "message": "Request contains an invalid argument.",
            "status": "INVALID_ARGUMENT",
            "details": [{
                "@type": "type.googleapis.com/google.ads.googleads.v26.errors.GoogleAdsFailure",
                "errors": [{
                    "errorCode": { "biddingError": "CONVERSION_TRACKING_NOT_ENABLED" },
                    "message": "Conversion tracking is not enabled."
                }]
            }]
        }})))
        .mount(&h.server)
        .await;
    let e = pmax_err(&h, pmax_minimal(&img)).await;
    assert_eq!(e.code, codes::BIDDING_REQUIRES_CONVERSIONS, "{e:?}");
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["validateOnly"], true);
}

#[tokio::test]
async fn pmax_undo_removes_campaign_then_budget() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let img = Images::new();
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(PMAX, pmax_minimal(&img), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let change_id = out["change_id"].as_str().unwrap().to_string();
    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    let bodies = mutate_bodies(&h).await;
    assert_eq!(
        bodies.last().unwrap()["mutateOperations"],
        json!([
            { "campaignOperation": { "remove": CAMPAIGN_REAL } },
            { "campaignBudgetOperation": { "remove": BUDGET_REAL } }
        ])
    );
    let st = state.lock().unwrap();
    assert!(!st.contains_key(CAMPAIGN_REAL));
    assert!(!st.contains_key(BUDGET_REAL));
    drop(st);
    let orig = h.db.get_change(&change_id).unwrap().unwrap();
    assert_eq!(orig.status, ChangeStatus::Undone);
    let undone =
        h.db.list_changes(&ChangeFilter::default())
            .unwrap()
            .into_iter()
            .find(|c| c.tool == "remove_resources");
    assert!(undone.is_some());
}

#[tokio::test]
async fn pmax_spec_is_reversible_create() {
    let h = google_harness().await;
    let spec = h
        .adapter
        .tools()
        .into_iter()
        .find(|t| t.name == PMAX)
        .expect("create_pmax_campaign registered");
    assert!(spec.reversible);
    assert!(spec.exposed);
    assert_eq!(spec.group, adsconsole_core::config::ToolGroup::Create);
    assert!(!spec.destructive);
    assert!(
        spec.description.contains("conversion tracking"),
        "{}",
        spec.description
    );
}

#[tokio::test]
async fn pmax_warns_assets_stay_and_lists_sources() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let mut input = pmax_minimal(&img);
    input["brand"]["logo"] = json!({ "asset": "customers/111/assets/77" });
    let v = h
        .adapter
        .validate(&call(CONN, "111", PMAX, input))
        .await
        .unwrap();
    let warning = v.warning.unwrap();
    assert!(warning.contains("stay in the account"), "{warning}");
    assert!(
        v.summary.ends_with(&format!(
            "images from asset 77, {}, {}",
            img.landscape, img.square
        )),
        "{}",
        v.summary
    );
}

/// The same bytes get the same asset name in any campaign, and two files with the
/// same bytes become a single asset.
#[tokio::test]
async fn pmax_image_assets_are_named_by_content() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let copy = format!("{}.copy.png", img.square);
    std::fs::copy(&img.square, &copy).unwrap();
    let image_names = |ops: &[Value]| -> Vec<String> {
        creates(ops, "assetOperation")
            .into_iter()
            .filter(|a| a.get("imageAsset").is_some())
            .map(|a| a["name"].as_str().unwrap().to_string())
            .collect()
    };

    let mut a = pmax_minimal(&img);
    a["asset_groups"][0]["images"]["square"] = json!([{ "file_path": copy }]);
    let ops = pmax_ops(&h, a).await;
    let names_a = image_names(&ops);
    assert_eq!(names_a.len(), 2, "{names_a:?}"); // logo/square (same bytes) and 1.91:1
    let logo_rn = &creates(&ops, "campaignAssetOperation")[1]["asset"];
    let square = creates(&ops, "assetGroupAssetOperation")
        .into_iter()
        .find(|l| l["fieldType"] == "SQUARE_MARKETING_IMAGE")
        .unwrap();
    assert_eq!(square["asset"], *logo_rn);

    let mut b = pmax_minimal(&img);
    b["name"] = json!("Another campaign");
    let names_b = image_names(&pmax_ops(&h, b).await);
    assert_eq!(names_a, names_b);
}

/// Local validation of every group runs before any image is loaded.
#[tokio::test]
async fn pmax_validates_texts_before_loading_images() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let mut input = pmax_minimal(&img);
    input["brand"]["logo"] = json!({ "file_path": "/does/not/exist/logo.png" });
    let mut g1 = input["asset_groups"][0].clone();
    g1["name"] = json!("Group 2");
    g1["descriptions"] = json!(["Only one."]);
    input["asset_groups"].as_array_mut().unwrap().push(g1);
    assert_pmax_invalid(&h, input.clone(), "asset_groups[1].descriptions").await;
    // Once the text is fixed, the missing logo shows up.
    input["asset_groups"][1]["descriptions"] = json!(["Buy today.", "Fast delivery."]);
    assert_pmax_invalid(&h, input, "brand.logo").await;
}

#[tokio::test]
async fn pmax_repeated_video_in_a_group() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let mut input = pmax_minimal(&img);
    input["asset_groups"][0]["youtube_video_ids"] = json!(["dQw4w9WgXcQ", " dQw4w9WgXcQ "]);
    let e = pmax_err(&h, input).await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert_eq!(
        e.field.as_deref(),
        Some("asset_groups[0].youtube_video_ids[1]")
    );
    assert!(
        e.message.contains("appears more than once"),
        "{}",
        e.message
    );
}

// ------------------------------------------------------------ Demand Gen

const DG: &str = "create_demand_gen_campaign";
const DISPLAY: &str = "create_display_campaign";
const AD_GROUP_TEMP: &str = "customers/111/adGroups/-3";

/// Extra images: 400x400 logo (different bytes from the 1:1) and 4:5 portrait.
struct DgImages {
    base: Images,
    logo: String,
    portrait: String,
    /// Same dimensions as `base.landscape` / `base.square`, different bytes.
    landscape2: String,
    square2: String,
}

impl DgImages {
    fn new() -> Self {
        let base = Images::new();
        let dir = std::path::Path::new(&base.square).parent().unwrap();
        let write = |name: &str, bytes: Vec<u8>| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p.to_string_lossy().into_owned()
        };
        DgImages {
            logo: write("logo.png", png(400, 400)),
            portrait: write("portrait.png", png(960, 1200)),
            landscape2: write("landscape2.png", [png(1200, 628), vec![1]].concat()),
            square2: write("square2.png", [png(1200, 1200), vec![1]].concat()),
            base,
        }
    }
}

fn multi_asset(img: &DgImages) -> Value {
    json!({ "multi_asset": {
        "headlines": ["Leather shoes", "Free shipping"],
        "descriptions": ["Dress and casual styles with fast delivery."],
        "business_name": "Shoes Inc",
        "logo": { "file_path": img.logo },
        "images": {
            "landscape": [{ "file_path": img.base.landscape }],
            "square": [{ "file_path": img.base.square }]
        },
        "final_url": "https://example.com",
        "call_to_action": "Shop Now"
    } })
}

fn dg_input(ads: Value) -> Value {
    json!({
        "account_id": "111",
        "name": "DG Shoes",
        "daily_budget": 30,
        "ad_groups": [{
            "name": "Group 1",
            "locations": ["2076"],
            "languages": ["1014"],
            "ads": ads
        }]
    })
}

fn card(img: &str, n: usize) -> Value {
    json!({ "headline": format!("Card {n}"), "image": { "file_path": img },
            "final_url": format!("https://example.com/{n}") })
}

fn carousel(img: &DgImages, cards: Vec<Value>) -> Value {
    json!({ "carousel": {
        "headline": "New collection",
        "description": "See this season's styles.",
        "business_name": "Shoes Inc",
        "logo": { "file_path": img.logo },
        "cards": cards,
        "final_url": "https://example.com"
    } })
}

async fn tool_err(h: &GoogleHarness, tool: &str, input: Value) -> ToolError {
    h.adapter
        .validate(&call(CONN, "111", tool, input))
        .await
        .unwrap_err()
}

async fn assert_tool_invalid(h: &GoogleHarness, tool: &str, input: Value, field: &str) {
    let e = tool_err(h, tool, input.clone()).await;
    assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
    assert_eq!(e.field.as_deref(), Some(field), "{e:?} {input}");
}

async fn tool_ops(h: &GoogleHarness, tool: &str, input: Value) -> Vec<Value> {
    h.adapter
        .validate(&call(CONN, "111", tool, input))
        .await
        .unwrap();
    let bodies = mutate_bodies(h).await;
    let last = bodies.last().unwrap();
    assert_eq!(last["validateOnly"], true);
    last["mutateOperations"].as_array().unwrap().clone()
}

/// Writes `value` at JSON Pointer `ptr`, creating the final key if missing.
fn set_ptr(v: &mut Value, ptr: &str, value: Value) {
    let (parent, key) = ptr.rsplit_once('/').unwrap();
    let target = v.pointer_mut(parent).unwrap_or_else(|| panic!("{ptr}"));
    match target {
        Value::Array(a) => a[key.parse::<usize>().unwrap()] = value,
        other => other[key] = value,
    }
}

fn ad_of(ops: &[Value], i: usize) -> &Value {
    &creates(ops, "adGroupAdOperation")[i]["ad"]
}

#[tokio::test]
async fn demand_gen_multi_asset_ops() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let img = DgImages::new();
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(DG, dg_input(json!([multi_asset(&img)])), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], true);
    assert_eq!(
        out["summary"],
        format!(
            "Creates Demand Gen campaign 'DG Shoes' (paused), 1 ad groups, 1 ads, \
             budget 30 BRL/day; images from {}, {}, {}",
            img.logo, img.base.landscape, img.base.square
        )
    );

    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert_eq!(bodies[0]["validateOnly"], true);
    let ops = bodies[1]["mutateOperations"].as_array().unwrap();
    assert_eq!(
        keys(ops),
        [
            "assetOperation",
            "assetOperation",
            "assetOperation",
            "campaignBudgetOperation",
            "campaignOperation",
            "adGroupOperation",
            "adGroupCriterionOperation",
            "adGroupCriterionOperation",
            "adGroupAdOperation",
        ]
    );
    let budget = create(ops, 3);
    assert_eq!(budget["explicitlyShared"], false);
    assert_eq!(budget["amountMicros"], "30000000");
    let campaign = create(ops, 4);
    assert_eq!(campaign["advertisingChannelType"], "DEMAND_GEN");
    assert_eq!(campaign["status"], "PAUSED");
    assert_eq!(campaign["campaignBudget"], BUDGET_TEMP);
    assert_eq!(campaign["targetSpend"], json!({}));

    let group = create(ops, 5);
    assert_eq!(group["resourceName"], AD_GROUP_TEMP);
    assert_eq!(group["campaign"], CAMPAIGN_TEMP);
    assert_eq!(group["name"], "Group 1");
    assert_eq!(group["status"], "ENABLED");
    // Location and language on the ad group, not on the campaign.
    assert_eq!(
        *create(ops, 6),
        json!({ "adGroup": AD_GROUP_TEMP,
                "location": { "geoTargetConstant": "geoTargetConstants/2076" } })
    );
    assert_eq!(
        *create(ops, 7),
        json!({ "adGroup": AD_GROUP_TEMP,
                "language": { "languageConstant": "languageConstants/1014" } })
    );
    let asset_rn = |i: usize| create(ops, i)["resourceName"].clone();
    assert_eq!(create(ops, 0)["imageAsset"]["data"], b64(&png(400, 400)));
    assert_eq!(create(ops, 1)["imageAsset"]["data"], b64(&png(1200, 628)));
    assert_eq!(create(ops, 2)["imageAsset"]["data"], b64(&png(1200, 1200)));

    let ad_group_ad = create(ops, 8);
    assert_eq!(ad_group_ad["adGroup"], AD_GROUP_TEMP);
    assert_eq!(ad_group_ad["status"], "ENABLED");
    assert_eq!(
        ad_group_ad["ad"],
        json!({
            "finalUrls": ["https://example.com"],
            "demandGenMultiAssetAd": {
                "headlines": [{ "text": "Leather shoes" }, { "text": "Free shipping" }],
                "descriptions": [{ "text": "Dress and casual styles with fast delivery." }],
                "businessName": "Shoes Inc",
                "logoImages": [{ "asset": asset_rn(0) }],
                "marketingImages": [{ "asset": asset_rn(1) }],
                "squareMarketingImages": [{ "asset": asset_rn(2) }],
                "callToActionText": "Shop Now"
            }
        })
    );

    let st = state.lock().unwrap();
    assert_eq!(st[CAMPAIGN_REAL]["status"], "PAUSED");
    drop(st);
    let rec =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        rec.after,
        Some(json!({ CAMPAIGN_REAL: { "campaign.status": "PAUSED",
                                      "campaign.name": "DG Shoes" } }))
    );
}

#[tokio::test]
async fn demand_gen_video_by_id() {
    let h = google_harness().await;
    fake(&h).await;
    let img = DgImages::new();
    let video = |videos: Value| {
        json!({ "video_responsive": {
            "headlines": ["Leather shoes"],
            "long_headlines": ["Leather shoes with free shipping across the country"],
            "descriptions": ["Buy today."],
            "business_name": "Shoes Inc",
            "logo": { "file_path": img.logo },
            "videos": videos,
            "final_url": "https://example.com"
        } })
    };
    // The same id in two ads becomes a single asset.
    let input = dg_input(json!([
        video(json!(["dQw4w9WgXcQ", "customers/111/assets/55"])),
        video(json!([" dQw4w9WgXcQ "])),
    ]));
    let v = h
        .adapter
        .validate(&call(CONN, "111", DG, input.clone()))
        .await
        .unwrap();
    assert!(v.warning.unwrap().contains("stay in the account"));
    let ops = tool_ops(&h, DG, input).await;
    let videos: Vec<&Value> = creates(&ops, "assetOperation")
        .into_iter()
        .filter(|a| a.get("youtubeVideoAsset").is_some())
        .collect();
    assert_eq!(videos.len(), 1, "{videos:?}");
    assert_eq!(
        videos[0]["youtubeVideoAsset"],
        json!({ "youtubeVideoId": "dQw4w9WgXcQ" })
    );
    let video_rn = videos[0]["resourceName"].clone();
    let ad = &ad_of(&ops, 0)["demandGenVideoResponsiveAd"];
    assert_eq!(
        ad["videos"],
        json!([{ "asset": video_rn }, { "asset": "customers/111/assets/55" }])
    );
    assert_eq!(
        ad["longHeadlines"],
        json!([{ "text": "Leather shoes with free shipping across the country" }])
    );
    assert_eq!(ad["businessName"], json!({ "text": "Shoes Inc" }));
    assert_eq!(ad["logoImages"].as_array().unwrap().len(), 1);
    assert_eq!(
        ad_of(&ops, 1)["demandGenVideoResponsiveAd"]["videos"],
        json!([{ "asset": video_rn }])
    );
    // The video asset comes before the first ad that uses it.
    let pos = |pred: &dyn Fn(&Value) -> bool| ops.iter().position(pred).unwrap();
    assert!(
        pos(&|o| o["assetOperation"]["create"]["youtubeVideoAsset"].is_object())
            < pos(&|o| o.get("adGroupAdOperation").is_some())
    );

    // A YouTube link becomes the extracted id; a malformed link is refused on the field.
    let ops = tool_ops(
        &h,
        DG,
        dg_input(json!([video(json!(["https://youtu.be/dQw4w9WgXcQ"]))])),
    )
    .await;
    let linked: Vec<&Value> = creates(&ops, "assetOperation")
        .into_iter()
        .filter(|a| a.get("youtubeVideoAsset").is_some())
        .collect();
    assert_eq!(
        linked[0]["youtubeVideoAsset"],
        json!({ "youtubeVideoId": "dQw4w9WgXcQ" })
    );
    let mut bad = dg_input(json!([video(json!(["https://youtu.be/short"]))]));
    assert_tool_invalid(&h, DG, bad.clone(), "ad_groups[0].ads[0].videos[0]").await;
    bad["ad_groups"][0]["ads"] = json!([video(json!([
        "dQw4w9WgXcQ",
        "https://www.youtube.com/shorts/dQw4w9WgXcQ"
    ]))]);
    assert_tool_invalid(&h, DG, bad.clone(), "ad_groups[0].ads[0].videos[1]").await;
    bad["ad_groups"][0]["ads"] = json!([video(json!([]))]);
    assert_tool_invalid(&h, DG, bad.clone(), "ad_groups[0].ads[0].videos").await;
    bad["ad_groups"][0]["ads"] = json!([video(json!(["dQw4w9WgXcQ", "dQw4w9WgXcQ"]))]);
    assert_tool_invalid(&h, DG, bad.clone(), "ad_groups[0].ads[0].videos[1]").await;
    bad["ad_groups"][0]["ads"] = json!([video(json!(["customers/999/assets/1"]))]);
    assert_tool_invalid(&h, DG, bad, "ad_groups[0].ads[0].videos[0]").await;
}

#[tokio::test]
async fn demand_gen_carousel_card_limits() {
    let h = google_harness().await;
    fake(&h).await;
    let img = DgImages::new();
    let field = "ad_groups[0].ads[0].cards";
    let one = dg_input(json!([carousel(&img, vec![card(&img.base.landscape, 0)])]));
    assert_tool_invalid(&h, DG, one, field).await;
    let eleven = (0..11).map(|n| card(&img.base.landscape, n)).collect();
    assert_tool_invalid(&h, DG, dg_input(json!([carousel(&img, eleven)])), field).await;
    // Card with an image that is neither 1.91:1 nor 1:1.
    let wrong = vec![card(&img.base.landscape, 0), card(&img.base.wide, 1)];
    assert_tool_invalid(
        &h,
        DG,
        dg_input(json!([carousel(&img, wrong)])),
        "ad_groups[0].ads[0].cards[1].image",
    )
    .await;
    // Mixed aspect ratios across the cards.
    let mixed = vec![card(&img.base.landscape, 0), card(&img.base.square, 1)];
    assert_tool_invalid(
        &h,
        DG,
        dg_input(json!([carousel(&img, mixed)])),
        "ad_groups[0].ads[0].cards[1].image",
    )
    .await;

    // The same image on two cards (same file, or another file with the same content)
    // is refused before reaching Google: DUPLICATE_IMAGE_ACROSS_CAROUSEL_CARDS.
    let copy = img.square2.replace("square2", "square_copy");
    std::fs::copy(&img.base.square, &copy).unwrap();
    for second in [&img.base.square, &copy] {
        let cards = vec![card(&img.base.square, 0), card(second, 1)];
        let e = tool_err(&h, DG, dg_input(json!([carousel(&img, cards)]))).await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
        assert_eq!(
            e.field.as_deref(),
            Some("ad_groups[0].ads[0].cards[1].image")
        );
        assert!(
            e.message.contains("the image is repeated on another card"),
            "{}",
            e.message
        );
    }

    // Two 1:1 cards: card assets with the square image and each one's URL.
    let mut cards = vec![card(&img.base.square, 0), card(&img.square2, 1)];
    cards[1]["call_to_action"] = json!("See More");
    let ops = tool_ops(&h, DG, dg_input(json!([carousel(&img, cards)]))).await;
    let assets = creates(&ops, "assetOperation");
    let image_rn = assets
        .iter()
        .find(|a| a["imageAsset"]["data"] == b64(&png(1200, 1200)))
        .unwrap()["resourceName"]
        .clone();
    let image2_rn = assets
        .iter()
        .find(|a| a["imageAsset"]["data"] == b64(&[png(1200, 1200), vec![1]].concat()))
        .unwrap()["resourceName"]
        .clone();
    let card_assets: Vec<&&Value> = assets
        .iter()
        .filter(|a| a.get("demandGenCarouselCardAsset").is_some())
        .collect();
    assert_eq!(card_assets.len(), 2);
    assert_eq!(
        card_assets[0]["demandGenCarouselCardAsset"],
        json!({ "squareMarketingImageAsset": image_rn, "headline": "Card 0" })
    );
    assert_eq!(
        card_assets[1]["demandGenCarouselCardAsset"],
        json!({ "squareMarketingImageAsset": image2_rn, "headline": "Card 1",
                "callToActionText": "See More" })
    );
    assert_eq!(
        card_assets[1]["finalUrls"],
        json!(["https://example.com/1"])
    );
    let ad = ad_of(&ops, 0);
    assert_eq!(ad["finalUrls"], json!(["https://example.com"]));
    let c = &ad["demandGenCarouselAd"];
    assert_eq!(c["headline"], json!({ "text": "New collection" }));
    assert_eq!(
        c["description"],
        json!({ "text": "See this season's styles." })
    );
    assert_eq!(c["businessName"], "Shoes Inc");
    assert!(c["logoImage"]["asset"].is_string(), "{c}");
    assert_eq!(
        c["carouselCards"],
        json!([{ "asset": card_assets[0]["resourceName"] },
               { "asset": card_assets[1]["resourceName"] }])
    );

    // 1.91:1 cards use marketingImageAsset.
    let cards = vec![card(&img.base.landscape, 0), card(&img.landscape2, 1)];
    let ops = tool_ops(&h, DG, dg_input(json!([carousel(&img, cards)]))).await;
    let card_asset = creates(&ops, "assetOperation")
        .into_iter()
        .find(|a| a.get("demandGenCarouselCardAsset").is_some())
        .unwrap();
    assert!(
        card_asset["demandGenCarouselCardAsset"]["marketingImageAsset"].is_string(),
        "{card_asset}"
    );
}

#[tokio::test]
async fn demand_gen_carousel_new_images_are_created_before_the_cards_that_use_them() {
    let h = google_harness().await;
    fake(&h).await;
    let img = DgImages::new();
    // Multi-asset (new 1.91:1 and 1:1 images) and carousel (the same 1.91:1 on card 1,
    // another one only on card 2) in the same ad group.
    let cards = vec![card(&img.base.landscape, 1), card(&img.landscape2, 2)];
    let input = dg_input(json!([multi_asset(&img), carousel(&img, cards)]));
    let ops = tool_ops(&h, DG, input).await;
    let pos = |name: &Value| {
        ops.iter()
            .position(|o| o["assetOperation"]["create"]["resourceName"] == *name)
            .unwrap_or_else(|| panic!("no create for {name}"))
    };
    let card_ops: Vec<(usize, &Value)> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o["assetOperation"]["create"]["demandGenCarouselCardAsset"].is_object())
        .collect();
    assert_eq!(card_ops.len(), 2);
    for (i, o) in &card_ops {
        let image =
            &o["assetOperation"]["create"]["demandGenCarouselCardAsset"]["marketingImageAsset"];
        assert!(pos(image) < *i, "image {image} after card {i}");
    }
    // Every asset creation comes before any ad (the real validateOnly refused a card
    // image created after the multi-asset ad: RESOURCE_NOT_FOUND).
    let first_ad = ops
        .iter()
        .position(|o| o.get("adGroupAdOperation").is_some())
        .unwrap();
    let last_asset = ops
        .iter()
        .rposition(|o| o.get("assetOperation").is_some())
        .unwrap();
    assert!(
        last_asset < first_ad,
        "asset at {last_asset}, ad at {first_ad}"
    );
    let ad = ops
        .iter()
        .position(|o| o["adGroupAdOperation"]["create"]["ad"]["demandGenCarouselAd"].is_object())
        .unwrap();
    assert!(card_ops.iter().all(|(i, _)| *i < ad));
    // Distinct images, unique temporary names.
    let imgs: Vec<&Value> = card_ops
        .iter()
        .map(|(_, o)| &o["assetOperation"]["create"]["demandGenCarouselCardAsset"]["marketingImageAsset"])
        .collect();
    assert_ne!(imgs[0], imgs[1]);
    let mut names: Vec<&str> = ops
        .iter()
        .filter_map(|o| o.as_object()?.values().next()?["create"]["resourceName"].as_str())
        .collect();
    let n = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(n, names.len(), "repeated temporary id");
}

/// Google treats consecutive operations of the same type as one batch and does not
/// resolve, inside it, a temporary id created in that same batch (a card right after its
/// image: RESOURCE_NOT_FOUND). The description marks those references with `!!`; none
/// may remain, and every reference to a temporary id must point to an earlier create.
#[tokio::test]
async fn demand_gen_references_never_point_into_their_own_batch() {
    let h = google_harness().await;
    fake(&h).await;
    let img = DgImages::new();
    let two_cards = || vec![card(&img.base.landscape, 1), card(&img.landscape2, 2)];
    let cases = [
        // Multi-asset and carousel with a new image used only on the card.
        dg_input(json!([multi_asset(&img), carousel(&img, two_cards())])),
        // Carousel alone: nothing between the image and the card except what the order guarantees.
        dg_input(json!([carousel(&img, two_cards())])),
    ];
    for input in cases {
        let lines = h
            .adapter
            .describe_operations(&call(CONN, "111", DG, input))
            .await
            .unwrap();
        for l in &lines {
            eprintln!("{l}");
        }
        assert!(lines.iter().any(|l| l.contains("assetOperation create")));
        let bad: Vec<&String> = lines.iter().filter(|l| l.contains("!!")).collect();
        assert!(bad.is_empty(), "{bad:#?}");
        assert!(
            lines
                .iter()
                .all(|l| !l.contains("\"data\"") && l.len() < 600)
        );
    }
}

#[tokio::test]
async fn demand_gen_call_to_action_comes_from_googles_list() {
    let h = google_harness().await;
    fake(&h).await;
    let img = DgImages::new();
    let cta_of = |ops: &[Value]| ad_of(ops, 0)["demandGenMultiAssetAd"]["callToActionText"].clone();
    // Any locale (and English), ignoring case and surrounding spaces; the table's
    // canonical form is sent.
    for (typed, canonical) in [
        ("  comprar AGORA ", "Comprar agora"),
        ("Shop Now", "Shop Now"),
        ("shop now", "Shop Now"),
        ("Jetzt einkaufen", "Jetzt einkaufen"),
        ("saber mais", "Saber mais"),
    ] {
        let mut ad = multi_asset(&img);
        ad["multi_asset"]["call_to_action"] = json!(typed);
        let ops = tool_ops(&h, DG, dg_input(json!([ad]))).await;
        assert_eq!(cta_of(&ops), json!(canonical), "{typed}");
    }
    // Not in the list: error on the field, with the options in pt-BR and in English.
    let mut ad = multi_asset(&img);
    ad["multi_asset"]["call_to_action"] = json!("Buy it today");
    let e = tool_err(&h, DG, dg_input(json!([ad]))).await;
    assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
    assert_eq!(
        e.field.as_deref(),
        Some("ad_groups[0].ads[0].call_to_action")
    );
    for option in ["Comprar agora", "Saber mais", "Shop Now", "Learn More"] {
        assert!(e.message.contains(option), "{}", e.message);
    }
    // The same on the carousel cards.
    let mut cards = vec![card(&img.base.square, 0), card(&img.square2, 1)];
    cards[1]["call_to_action"] = json!("See it all");
    assert_tool_invalid(
        &h,
        DG,
        dg_input(json!([carousel(&img, cards)])),
        "ad_groups[0].ads[0].cards[1].call_to_action",
    )
    .await;
}

#[tokio::test]
async fn demand_gen_limits() {
    let h = google_harness().await;
    fake(&h).await;
    let img = DgImages::new();
    let ok = dg_input(json!([multi_asset(&img)]));
    let cases: Vec<(&str, Value, &str)> = vec![
        ("/ad_groups", json!([]), "ad_groups"),
        ("/ad_groups/0/ads", json!([]), "ad_groups[0].ads"),
        (
            "/ad_groups/0/locations",
            json!(["abc"]),
            "ad_groups[0].locations[0]",
        ),
        (
            "/ad_groups/0/languages",
            json!(["1014", "1014"]),
            "ad_groups[0].languages[1]",
        ),
        ("/ad_groups/0/name", json!(" "), "ad_groups[0].name"),
        ("/bidding", json!("manual_cpc"), "bidding"),
        (
            "/bidding",
            json!({ "maximize_conversion_value": {} }),
            "bidding",
        ),
        ("/name", json!(""), "name"),
        ("/daily_budget", json!(0), "daily_budget"),
        (
            "/ad_groups/0/ads/0/multi_asset/headlines",
            json!([]),
            "ad_groups[0].ads[0].headlines",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/headlines",
            texts("T", 6),
            "ad_groups[0].ads[0].headlines",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/headlines",
            json!(["x".repeat(41)]),
            "ad_groups[0].ads[0].headlines[0]",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/descriptions",
            json!(["x".repeat(91)]),
            "ad_groups[0].ads[0].descriptions[0]",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/business_name",
            json!("x".repeat(26)),
            "ad_groups[0].ads[0].business_name",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/final_url",
            json!("example"),
            "ad_groups[0].ads[0].final_url",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/images/square",
            json!([]),
            "ad_groups[0].ads[0].images.square",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/images/landscape",
            json!([{ "file_path": img.base.wide }]),
            "ad_groups[0].ads[0].images.landscape[0]",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/logo",
            json!({ "file_path": img.base.landscape }),
            "ad_groups[0].ads[0].logo",
        ),
        (
            "/ad_groups/0/ads/0/multi_asset/logo",
            json!({ "asset": "customers/999/assets/1" }),
            "ad_groups[0].ads[0].logo.asset",
        ),
    ];
    for (ptr, value, field) in cases {
        let mut input = ok.clone();
        set_ptr(&mut input, ptr, value);
        assert_tool_invalid(&h, DG, input, field).await;
    }
    let mut six = ok.clone();
    six["ad_groups"][0]["ads"] = json!(vec![multi_asset(&img); 6]);
    assert_tool_invalid(&h, DG, six, "ad_groups[0].ads").await;
    let mut eleven = ok.clone();
    let group = ok["ad_groups"][0].clone();
    eleven["ad_groups"] = Value::Array(
        (0..11)
            .map(|i| {
                let mut g = group.clone();
                g["name"] = json!(format!("G{i}"));
                g
            })
            .collect(),
    );
    assert_tool_invalid(&h, DG, eleven, "ad_groups").await;
    let mut dup = ok.clone();
    dup["ad_groups"] = json!([group.clone(), group]);
    assert_tool_invalid(&h, DG, dup, "ad_groups[1].name").await;

    // An invalid text in the second group shows up before the first group's missing logo.
    let mut order = ok.clone();
    order["ad_groups"][0]["ads"][0]["multi_asset"]["logo"] =
        json!({ "file_path": "/does/not/exist.png" });
    let mut g1 = order["ad_groups"][0].clone();
    g1["name"] = json!("Group 2");
    g1["ads"][0]["multi_asset"]["descriptions"] = json!([]);
    order["ad_groups"].as_array_mut().unwrap().push(g1);
    assert_tool_invalid(&h, DG, order, "ad_groups[1].ads[0].descriptions").await;

    // No mutate went out for the invalid cases.
    assert!(mutate_bodies(&h).await.is_empty());

    // maximize_conversions with a CPA is accepted.
    let mut conv = ok;
    conv["bidding"] = json!({ "maximize_conversions": { "target_cpa": 12.5 } });
    conv["ad_groups"][0]["ads"][0]["multi_asset"]["images"]["portrait"] =
        json!([{ "file_path": img.portrait }]);
    let ops = tool_ops(&h, DG, conv).await;
    assert_eq!(
        creates(&ops, "campaignOperation")[0]["maximizeConversions"],
        json!({ "targetCpaMicros": "12500000" })
    );
    let portrait = creates(&ops, "assetOperation")
        .into_iter()
        .find(|a| a["imageAsset"]["data"] == b64(&png(960, 1200)))
        .unwrap()["resourceName"]
        .clone();
    assert_eq!(
        ad_of(&ops, 0)["demandGenMultiAssetAd"]["portraitMarketingImages"],
        json!([{ "asset": portrait }])
    );
}

#[tokio::test]
async fn demand_gen_budget_minimum_from_api() {
    let h = google_harness().await;
    let img = DgImages::new();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../google-ads/tests/fixtures/budget_minimum.json"
    ))
    .unwrap();
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture))
        .mount(&h.server)
        .await;
    let mut input = dg_input(json!([multi_asset(&img)]));
    input["daily_budget"] = json!(1);
    let e = tool_err(&h, DG, input).await;
    assert_eq!(e.code, codes::BUDGET_TOO_LOW, "{e:?}");
    assert_eq!(e.field.as_deref(), Some("daily_budget"));
    assert!(e.message.contains("25.4 BRL"), "{}", e.message);
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["validateOnly"], true);
}

#[tokio::test]
async fn demand_gen_undo_removes_campaign_then_budget() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let img = DgImages::new();
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(DG, dg_input(json!([multi_asset(&img)])), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let change_id = out["change_id"].as_str().unwrap().to_string();
    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    assert_eq!(
        mutate_bodies(&h).await.last().unwrap()["mutateOperations"],
        json!([
            { "campaignOperation": { "remove": CAMPAIGN_REAL } },
            { "campaignBudgetOperation": { "remove": BUDGET_REAL } }
        ])
    );
    let st = state.lock().unwrap();
    assert!(!st.contains_key(CAMPAIGN_REAL));
    assert!(!st.contains_key(BUDGET_REAL));
    drop(st);
    assert_eq!(
        h.db.get_change(&change_id).unwrap().unwrap().status,
        ChangeStatus::Undone
    );
}

#[tokio::test]
async fn demand_gen_and_display_specs() {
    let h = google_harness().await;
    let tools = h.adapter.tools();
    for name in [DG, DISPLAY] {
        let spec = tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} registered"));
        assert!(spec.reversible);
        assert!(spec.exposed);
        assert_eq!(spec.group, adsconsole_core::config::ToolGroup::Create);
        assert!(!spec.destructive);
    }
    let dg = tools.iter().find(|t| t.name == DG).unwrap();
    assert!(
        dg.description.contains(
            "This is the official path for video, image and carousel ads; the API does \
             not create VIDEO channel campaigns."
        ),
        "{}",
        dg.description
    );
}

// --------------------------------------------------------------- Display

fn rda(img: &Images) -> Value {
    json!({
        "headlines": ["Shoes", "Free shipping"],
        "long_headline": "Leather shoes with free shipping across the country",
        "descriptions": ["Buy today."],
        "business_name": "Shoes Inc",
        "marketing_images": [{ "file_path": img.landscape }],
        "square_marketing_images": [{ "file_path": img.square }],
        "final_url": "https://example.com"
    })
}

fn display_input(img: &Images) -> Value {
    json!({
        "account_id": "111",
        "name": "Display Shoes",
        "daily_budget": 40,
        "locations": ["2076"],
        "ad_groups": [{
            "name": "Group 1",
            "targeting": {
                "audiences": ["777"],
                "placements": ["example.com"],
                "topics": ["13"]
            },
            "ads": [rda(img)]
        }]
    })
}

#[tokio::test]
async fn display_rda_ops() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(DISPLAY, display_input(&img), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(
        out["summary"],
        format!(
            "Creates Display campaign 'Display Shoes' (paused), 1 ad groups, 1 ads, \
             budget 40 BRL/day; images from {}, {}",
            img.landscape, img.square
        )
    );
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2);
    let ops = bodies[1]["mutateOperations"].as_array().unwrap();
    assert_eq!(
        keys(ops),
        [
            "campaignBudgetOperation",
            "campaignOperation",
            "campaignCriterionOperation",
            "adGroupOperation",
            "adGroupCriterionOperation",
            "adGroupCriterionOperation",
            "adGroupCriterionOperation",
            "assetOperation",
            "assetOperation",
            "adGroupAdOperation",
        ]
    );
    let campaign = create(ops, 1);
    assert_eq!(campaign["advertisingChannelType"], "DISPLAY");
    assert_eq!(campaign["status"], "PAUSED");
    assert_eq!(campaign["targetSpend"], json!({}));
    assert_eq!(create(ops, 0)["explicitlyShared"], false);
    assert_eq!(create(ops, 2)["campaign"], CAMPAIGN_TEMP);
    let group = create(ops, 3);
    assert_eq!(group["type"], "DISPLAY_STANDARD");
    assert_eq!(group["status"], "ENABLED");
    assert_eq!(group["resourceName"], AD_GROUP_TEMP);
    assert_eq!(
        *create(ops, 4),
        json!({ "adGroup": AD_GROUP_TEMP,
                "userList": { "userList": "customers/111/userLists/777" } })
    );
    assert_eq!(
        *create(ops, 5),
        json!({ "adGroup": AD_GROUP_TEMP, "placement": { "url": "example.com" } })
    );
    assert_eq!(
        *create(ops, 6),
        json!({ "adGroup": AD_GROUP_TEMP,
                "topic": { "topicConstant": "topicConstants/13" } })
    );
    let rn = |i: usize| create(ops, i)["resourceName"].clone();
    assert_eq!(create(ops, 7)["imageAsset"]["data"], b64(&png(1200, 628)));
    let ad = create(ops, 9);
    assert_eq!(ad["adGroup"], AD_GROUP_TEMP);
    assert_eq!(
        ad["ad"],
        json!({
            "finalUrls": ["https://example.com"],
            "responsiveDisplayAd": {
                "headlines": [{ "text": "Shoes" }, { "text": "Free shipping" }],
                "longHeadline": { "text": "Leather shoes with free shipping across the country" },
                "descriptions": [{ "text": "Buy today." }],
                "businessName": "Shoes Inc",
                "marketingImages": [{ "asset": rn(7) }],
                "squareMarketingImages": [{ "asset": rn(8) }]
            }
        })
    );

    // Undo: campaign and then budget.
    let change_id = out["change_id"].as_str().unwrap().to_string();
    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    assert_eq!(
        mutate_bodies(&h).await.last().unwrap()["mutateOperations"],
        json!([
            { "campaignOperation": { "remove": CAMPAIGN_REAL } },
            { "campaignBudgetOperation": { "remove": BUDGET_REAL } }
        ])
    );
}

#[tokio::test]
async fn display_options_and_limits() {
    let h = google_harness().await;
    fake(&h).await;
    let img = Images::new();
    let mut input = display_input(&img);
    input["bidding"] = json!("manual_cpc");
    input["ad_groups"][0]["cpc_bid"] = json!(0.8);
    input["ad_groups"][0]["ads"][0]["logo_images"] =
        json!([{ "asset": "customers/111/assets/31" }]);
    input["ad_groups"][0]["ads"][0]["square_logo_images"] = json!([{ "file_path": img.square }]);
    let ops = tool_ops(&h, DISPLAY, input.clone()).await;
    assert_eq!(create(&ops, 1)["manualCpc"], json!({}));
    assert_eq!(create(&ops, 3)["cpcBidMicros"], "800000");
    let ad = &ad_of(&ops, 0)["responsiveDisplayAd"];
    assert_eq!(
        ad["logoImages"],
        json!([{ "asset": "customers/111/assets/31" }])
    );
    // The square logo is the same file as the 1:1 image: the same asset.
    assert_eq!(ad["squareLogoImages"], ad["squareMarketingImages"]);

    let ok = display_input(&img);
    let cases: Vec<(&str, Value, &str)> = vec![
        ("/bidding", json!("maximize_conversion_value"), "bidding"),
        ("/ad_groups/0/cpc_bid", json!(1), "ad_groups[0].cpc_bid"),
        ("/ad_groups", json!([]), "ad_groups"),
        ("/ad_groups/0/ads", json!([]), "ad_groups[0].ads"),
        (
            "/ad_groups/0/ads/0/headlines",
            json!(["x".repeat(31)]),
            "ad_groups[0].ads[0].headlines[0]",
        ),
        (
            "/ad_groups/0/ads/0/headlines",
            texts("T", 6),
            "ad_groups[0].ads[0].headlines",
        ),
        (
            "/ad_groups/0/ads/0/long_headline",
            json!("x".repeat(91)),
            "ad_groups[0].ads[0].long_headline",
        ),
        (
            "/ad_groups/0/ads/0/business_name",
            json!("x".repeat(26)),
            "ad_groups[0].ads[0].business_name",
        ),
        (
            "/ad_groups/0/ads/0/marketing_images",
            json!([]),
            "ad_groups[0].ads[0].marketing_images",
        ),
        (
            "/ad_groups/0/ads/0/marketing_images",
            json!([{ "file_path": img.square }]),
            "ad_groups[0].ads[0].marketing_images[0]",
        ),
        (
            "/ad_groups/0/ads/0/square_marketing_images",
            json!([]),
            "ad_groups[0].ads[0].square_marketing_images",
        ),
        (
            "/ad_groups/0/targeting/audiences",
            json!(["x"]),
            "ad_groups[0].targeting.audiences[0]",
        ),
        (
            "/ad_groups/0/targeting/placements",
            json!(["not a url"]),
            "ad_groups[0].targeting.placements[0]",
        ),
        (
            "/ad_groups/0/targeting/topics",
            json!([""]),
            "ad_groups[0].targeting.topics[0]",
        ),
        ("/locations", json!(["abc"]), "locations[0]"),
    ];
    for (ptr, value, field) in cases {
        let mut input = ok.clone();
        set_ptr(&mut input, ptr, value);
        assert_tool_invalid(&h, DISPLAY, input, field).await;
    }
}

fn refusal(error_code: Value, field_path: Value) -> Value {
    json!({ "error": {
        "code": 400, "message": "Request contains an invalid argument.",
        "status": "INVALID_ARGUMENT",
        "details": [{
            "@type": "type.googleapis.com/google.ads.googleads.v26.errors.GoogleAdsFailure",
            "errors": [{
                "errorCode": error_code,
                "message": "Not allowed.",
                "location": { "fieldPathElements": field_path }
            }]
        }]
    }})
}

#[tokio::test]
async fn display_refusal_points_to_demand_gen() {
    let channel_path = json!([
        { "fieldName": "mutate_operations", "index": 1 },
        { "fieldName": "campaign_operation" },
        { "fieldName": "create" },
        { "fieldName": "advertising_channel_type" }
    ]);
    let cases = [
        refusal(
            json!({ "campaignError": "CANNOT_SET_FIELD" }),
            channel_path.clone(),
        ),
        refusal(
            json!({ "operationAccessDeniedError": "OPERATION_NOT_PERMITTED_FOR_CONTEXT" }),
            json!([]),
        ),
        refusal(
            json!({ "campaignError": "CANNOT_CREATE_DISPLAY_CAMPAIGN" }),
            json!([]),
        ),
    ];
    for body in cases {
        let h = google_harness().await;
        let img = Images::new();
        Mock::given(method("POST"))
            .and(path(MUTATE_PATH))
            .respond_with(ResponseTemplate::new(400).set_body_json(body.clone()))
            .mount(&h.server)
            .await;
        let e = tool_err(&h, DISPLAY, display_input(&img)).await;
        assert_eq!(e.code, codes::DISPLAY_NOT_AVAILABLE, "{e:?} {body}");
        assert_eq!(
            e.message,
            "Google no longer allows creating Display campaigns through the API. Use \
             create_demand_gen_campaign."
        );
    }

    // Other tools do not change: the same error on Search stays API_ERROR.
    let h = google_harness().await;
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(refusal(
            json!({ "campaignError": "CANNOT_SET_FIELD" }),
            channel_path,
        )))
        .mount(&h.server)
        .await;
    let e = validate_err(&h, "111", minimal()).await;
    assert_eq!(e.code, codes::API_ERROR, "{e:?}");

    // Any other Google error on Display follows the normal translation.
    let h = google_harness().await;
    let img = Images::new();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../google-ads/tests/fixtures/budget_minimum.json"
    ))
    .unwrap();
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture))
        .mount(&h.server)
        .await;
    let e = tool_err(&h, DISPLAY, display_input(&img)).await;
    assert_eq!(e.code, codes::BUDGET_TOO_LOW, "{e:?}");
}

/// The `validateOnly` passes and the real mutate answers `status`/`body`.
async fn display_execute_err(status: u16, body: Value) -> ToolError {
    let h = google_harness().await;
    let img = Images::new();
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(move |req: &Request| {
            let sent: Value = serde_json::from_slice(&req.body).unwrap();
            if sent["validateOnly"] == json!(true) {
                ResponseTemplate::new(200).set_body_json(json!({ "mutateOperationResponses": [] }))
            } else {
                ResponseTemplate::new(status).set_body_json(body.clone())
            }
        })
        .mount(&h.server)
        .await;
    let c = call(CONN, "111", DISPLAY, display_input(&img));
    h.adapter.validate(&c).await.unwrap();
    let e = h.adapter.execute(&c, &json!({})).await.unwrap_err();
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["validateOnly"], false);
    e
}

#[tokio::test]
async fn display_refusal_on_real_mutate() {
    let path = json!([
        { "fieldName": "mutate_operations", "index": 1 },
        { "fieldName": "campaign_operation" },
        { "fieldName": "create" },
        { "fieldName": "advertising_channel_type" }
    ]);
    let e = display_execute_err(
        400,
        refusal(json!({ "campaignError": "CANNOT_SET_FIELD" }), path.clone()),
    )
    .await;
    assert_eq!(e.code, codes::DISPLAY_NOT_AVAILABLE, "{e:?}");

    // Maybe applied (5xx on the mutate): stays EXECUTION_FAILED, with the suggestion to
    // check the account, even when it mentions the channel.
    let mut body = refusal(json!({ "internalError": "INTERNAL_ERROR" }), path);
    body["error"]["code"] = json!(500);
    body["error"]["status"] = json!("INTERNAL");
    let e = display_execute_err(500, body).await;
    assert_eq!(e.code, codes::EXECUTION_FAILED, "{e:?}");
    assert!(
        e.suggestion
            .as_deref()
            .unwrap_or_default()
            .contains("check in the account"),
        "{e:?}"
    );
}

// ------------------------------------------------------- upload_video / upload_asset

const VIDEO: &str = "upload_video";
const UPLOAD_RN: &str = "customers/111/youTubeVideoUploads/55";
const VIDEO_ID: &str = "abcdefghijk";
const YT_START: &str = "/upload/customers/111/youTubeVideoUploads:create";
const YT_SESSION: &str = "/upload/session/1";
const YT_REMOVE: &str = "/v26/customers/111/youTubeVideoUploads:remove";

struct VideoFile {
    _dir: tempfile::TempDir,
    path: String,
}

fn video_file(name: &str, len: usize) -> VideoFile {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::fs::write(&p, vec![7u8; len]).unwrap();
    VideoFile {
        path: p.to_string_lossy().into_owned(),
        _dir: dir,
    }
}

/// Start, PUTs (finalize returns the upload name), the upload state in `state`, and
/// the remove.
async fn mock_youtube(h: &GoogleHarness, state: &str, chunk_status: u16) {
    Mock::given(method("POST"))
        .and(path(YT_START))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "x-goog-upload-url",
                    format!("{}{YT_SESSION}", h.server.uri()),
                )
                .insert_header("x-goog-upload-chunk-granularity", "1024"),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("PUT"))
        .and(path(YT_SESSION))
        .respond_with(move |req: &Request| {
            let cmd = req.headers.get("x-goog-upload-command").unwrap();
            if chunk_status != 200 {
                ResponseTemplate::new(chunk_status)
            } else if cmd.to_str().unwrap().contains("finalize") {
                ResponseTemplate::new(200).set_body_json(json!({ "resourceName": UPLOAD_RN }))
            } else {
                ResponseTemplate::new(200)
            }
        })
        .mount(&h.server)
        .await;
    let mut row = json!({ "resourceName": UPLOAD_RN, "state": state });
    if state != "PENDING" && state != "FAILED" {
        row["videoId"] = json!(VIDEO_ID);
    }
    Mock::given(method("POST"))
        .and(path(SEARCH_PATH))
        .and(wiremock::matchers::body_string_contains(
            "FROM you_tube_video_upload",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "results": [{ "youTubeVideoUpload": row }] })),
        )
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path(YT_REMOVE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&h.server)
        .await;
}

async fn requests_to(h: &GoogleHarness, p: &str) -> Vec<Value> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == p)
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        .collect()
}

fn video_input(file: &VideoFile) -> Value {
    json!({ "account_id": "111", "file_path": file.path, "title": "Test video" })
}

#[tokio::test]
async fn upload_video_end_to_end_and_undo() {
    let h = google_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(1), Duration::from_secs(5));
    let state = fake(&h).await;
    mock_youtube(&h, "PROCESSED", 200).await;
    let file = video_file("video.mp4", 3000);
    let engine = engine(&h, WriteMode::Direct);

    let out = engine
        .call_tool(VIDEO, video_input(&file), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], true, "{out}");
    let asset = "customers/111/assets/101";
    assert_eq!(
        out["result"],
        json!({
            UPLOAD_RN: { "you_tube_video_upload.state": "PROCESSED" },
            "video_id": VIDEO_ID,
            "asset_resource_name": asset
        })
    );
    assert!(
        out["summary"].as_str().unwrap().contains("validate_only"),
        "{out}"
    );
    // Start with the snake_case body and the whole file in the PUTs.
    let start = &requests_to(&h, YT_START).await[0];
    assert_eq!(
        start,
        &json!({ "customer_id": "111", "you_tube_video_upload": {
            "video_title": "Test video", "video_description": "",
            "video_privacy": "UNLISTED" } })
    );
    let puts: Vec<_> = h
        .server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .collect();
    assert_eq!(puts.iter().map(|r| r.body.len()).sum::<usize>(), 3000);
    // Only the real asset mutate (there is no validate_only for the upload).
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["validateOnly"], false);
    assert_eq!(
        bodies[0]["mutateOperations"],
        json!([{ "assetOperation": { "create": {
            "resourceName": "customers/111/assets/-1",
            "youtubeVideoAsset": { "youtubeVideoId": VIDEO_ID } } } }])
    );
    assert!(state.lock().unwrap().contains_key(asset));

    let change_id = out["change_id"].as_str().unwrap().to_string();
    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    assert_eq!(
        requests_to(&h, YT_REMOVE).await,
        vec![json!({ "customer_id": "111", "resource_names": [UPLOAD_RN] })]
    );
    let orig = h.db.get_change(&change_id).unwrap().unwrap();
    assert_eq!(orig.status, ChangeStatus::Undone);
    let inverse = orig.inverse.unwrap();
    assert_eq!(inverse.tool, "remove_youtube_upload");
    assert_eq!(inverse.input, json!({ "resource_name": UPLOAD_RN }));
}

/// validate, snapshot and execute directly on the adapter.
async fn video_applied(h: &GoogleHarness, input: Value) -> adsconsole_mcp::adapter::Applied {
    let c = call(CONN, "111", VIDEO, input);
    h.adapter.validate(&c).await.unwrap();
    let before = h.adapter.snapshot(&c).await.unwrap();
    h.adapter.execute(&c, &before).await.unwrap()
}

#[tokio::test]
async fn upload_video_partial_failures_are_recorded_with_undo() {
    // YouTube refused: no asset, with the inverse to clean up the upload.
    let h = google_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(1), Duration::from_secs(5));
    mock_youtube(&h, "FAILED", 200).await;
    let file = video_file("video.mov", 2000);
    let a = video_applied(&h, video_input(&file)).await;
    assert_eq!(a.after[UPLOAD_RN]["you_tube_video_upload.state"], "FAILED");
    assert_eq!(a.after["asset_resource_name"], Value::Null);
    assert!(
        a.after["message"].as_str().unwrap().contains("FAILED"),
        "{}",
        a.after
    );
    let inv = a.inverse.unwrap();
    assert_eq!(inv.tool, "remove_youtube_upload");
    assert_eq!(inv.input, json!({ "resource_name": UPLOAD_RN }));
    assert!(mutate_bodies(&h).await.is_empty());
    // The current state repeats the "after" (undo sees no conflict).
    let current = h
        .adapter
        .current_state(CONN, "111", &inv.target)
        .await
        .unwrap();
    assert_eq!(current, a.after);

    // Deadline exceeded: the current state and the message with the video_id.
    let h = google_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(1), Duration::from_millis(3));
    mock_youtube(&h, "UPLOADED", 200).await;
    let a = video_applied(&h, video_input(&file)).await;
    assert_eq!(
        a.after[UPLOAD_RN]["you_tube_video_upload.state"],
        "UPLOADED"
    );
    assert_eq!(a.after["video_id"], VIDEO_ID);
    assert_eq!(
        a.after["message"],
        format!(
            "The video is still processing. When it finishes, create the asset with \
             upload_asset kind=youtube and video_id {VIDEO_ID}."
        )
    );
    assert!(a.inverse.is_some());
    assert!(mutate_bodies(&h).await.is_empty());

    // The asset failed: the upload stays recorded, with the inverse.
    let h = google_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(1), Duration::from_secs(5));
    mock_youtube(&h, "PROCESSED", 200).await;
    Mock::given(method("POST"))
        .and(path(MUTATE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({ "error": { "code": 400, "message": "bad", "status": "INVALID_ARGUMENT" } }),
        ))
        .mount(&h.server)
        .await;
    let a = video_applied(&h, video_input(&file)).await;
    assert_eq!(
        a.after[UPLOAD_RN]["you_tube_video_upload.state"],
        "PROCESSED"
    );
    assert_eq!(a.after["asset_resource_name"], Value::Null);
    let msg = a.after["message"].as_str().unwrap();
    assert!(msg.contains("upload_asset kind=youtube"), "{msg}");
    assert!(a.inverse.is_some());

    // A chunk failed before the upload name existed: error, no retry.
    let h = google_harness().await;
    mock_youtube(&h, "PROCESSED", 500).await;
    let c = call(CONN, "111", VIDEO, video_input(&file));
    h.adapter.validate(&c).await.unwrap();
    let e = h.adapter.execute(&c, &json!({})).await.unwrap_err();
    assert_eq!(e.code, codes::EXECUTION_FAILED, "{e:?}");
    assert!(
        e.suggestion
            .as_deref()
            .unwrap_or_default()
            .contains("you_tube_video_upload"),
        "{e:?}"
    );
    assert_eq!(requests_to(&h, YT_SESSION).await.len(), 1);
}

#[tokio::test]
async fn upload_video_validates_file() {
    let h = google_harness().await;
    let ok = video_file("video.webm", 10);
    let txt = video_file("notes.txt", 10);
    let empty = video_file("empty.mp4", 0);
    let missing = format!("{}/missing.mp4", ok._dir.path().display());
    let base = video_input(&ok);
    let with = |k: &str, v: Value| {
        let mut x = base.clone();
        x[k] = v;
        x
    };
    for (input, field) in [
        (with("file_path", json!(missing)), "file_path"),
        (with("file_path", json!(txt.path)), "file_path"),
        (with("file_path", json!(empty.path)), "file_path"),
        (with("file_path", json!("video.mp4")), "file_path"),
        (
            with("file_path", json!(ok._dir.path().to_string_lossy())),
            "file_path",
        ),
        (with("title", json!("x".repeat(101))), "title"),
        (with("title", json!("  ")), "title"),
        (with("description", json!("x".repeat(5001))), "description"),
        (with("privacy", json!("SECRET")), "privacy"),
        (with("wait_minutes", json!(31)), "wait_minutes"),
        (with("wait_minutes", json!(0)), "wait_minutes"),
        (
            with("channel_id", json!("channel with space")),
            "channel_id",
        ),
    ] {
        let e = h
            .adapter
            .validate(&call(CONN, "111", VIDEO, input.clone()))
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::INVALID_INPUT, "{input}");
        assert_eq!(e.field.as_deref(), Some(field), "{e:?} {input}");
    }
    // Valid: local checks only, no call to Google.
    let v = h
        .adapter
        .validate(&call(CONN, "111", VIDEO, with("privacy", json!("public"))))
        .await
        .unwrap();
    assert!(v.summary.contains("validate_only"), "{}", v.summary);
    let w = v.warning.as_deref().unwrap_or_default();
    assert!(
        w.contains("During the upload and processing, other writes to this account wait."),
        "{w}"
    );
    assert!(w.contains("Undo removes the video"), "{w}");
    assert!(v.summary.contains("PUBLIC"), "{}", v.summary);
    let reqs = h.server.received_requests().await.unwrap();
    assert!(reqs.iter().all(|r| r.url.path() == "/token"), "{reqs:?}");
}

#[tokio::test]
async fn upload_video_refuses_file_changed_since_validate() {
    let h = google_harness().await;
    mock_youtube(&h, "PROCESSED", 200).await;
    let file = video_file("video.mkv", 100);
    let c = call(CONN, "111", VIDEO, video_input(&file));
    h.adapter.validate(&c).await.unwrap();
    std::fs::write(&file.path, vec![1u8; 150]).unwrap();
    let e = h.adapter.execute(&c, &json!({})).await.unwrap_err();
    assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
    assert_eq!(e.field.as_deref(), Some("file_path"), "{e:?}");
    assert!(requests_to(&h, YT_START).await.is_empty());
}

#[tokio::test]
async fn upload_tools_specs() {
    let h = google_harness().await;
    let tools = h.adapter.tools();
    let spec = |n: &str| tools.iter().find(|t| t.name == n).unwrap().clone();
    let video = spec(VIDEO);
    assert!(video.reversible && video.exposed);
    assert_eq!(video.group, adsconsole_core::config::ToolGroup::Media);
    assert!(
        video
            .description
            .contains("During the upload and processing, other writes to this account wait."),
        "{}",
        video.description
    );
    let asset = spec("upload_asset");
    assert!(!asset.reversible && asset.exposed);
    assert_eq!(asset.group, adsconsole_core::config::ToolGroup::Media);
    let remove = spec("remove_youtube_upload");
    assert!(!remove.reversible && !remove.exposed);
}

const ASSET: &str = "upload_asset";

async fn asset_err(h: &GoogleHarness, input: Value) -> ToolError {
    h.adapter
        .validate(&call(CONN, "111", ASSET, input))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn upload_asset_text_and_image() {
    let h = google_harness().await;
    let state = fake(&h).await;
    let img = Images::new();
    let engine = engine(&h, WriteMode::Direct);

    // Text: up to 90 characters.
    let e = asset_err(
        &h,
        json!({ "account_id": "111", "kind": "text", "text": "x".repeat(91) }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert_eq!(e.field.as_deref(), Some("text"), "{e:?}");
    let out = engine
        .call_tool(
            ASSET,
            json!({ "account_id": "111", "kind": "text", "text": " Free shipping ", "name": "Shipping" }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], false);
    assert_eq!(
        out["result"],
        json!({ "asset_resource_name": "customers/111/assets/101" })
    );
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["validateOnly"], true);
    assert_eq!(
        bodies[1]["mutateOperations"],
        json!([{ "assetOperation": { "create": {
            "resourceName": "customers/111/assets/-1", "name": "Shipping",
            "textAsset": { "text": "Free shipping" } } } }])
    );

    // 1200x628 image: IMAGE asset named by content.
    let out = engine
        .call_tool(
            ASSET,
            json!({ "account_id": "111", "kind": "image", "image": { "file_path": img.landscape } }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], false);
    let ops = mutate_bodies(&h).await.last().unwrap()["mutateOperations"].clone();
    let c = &ops[0]["assetOperation"]["create"];
    assert_eq!(c["type"], "IMAGE");
    assert_eq!(
        c["imageAsset"]["data"],
        b64(&std::fs::read(&img.landscape).unwrap())
    );
    assert!(c["name"].as_str().unwrap().starts_with("img-"), "{c}");
    assert!(
        state
            .lock()
            .unwrap()
            .contains_key("customers/111/assets/101")
    );

    // YouTube.
    let out = engine
        .call_tool(
            ASSET,
            json!({ "account_id": "111", "kind": "youtube", "youtube_video_id": VIDEO_ID }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let ops = mutate_bodies(&h).await.last().unwrap()["mutateOperations"].clone();
    assert_eq!(
        ops,
        json!([{ "assetOperation": { "create": {
            "resourceName": "customers/111/assets/-1",
            "youtubeVideoAsset": { "youtubeVideoId": VIDEO_ID } } } }])
    );

    // YouTube link: the id is taken from the link.
    for link in [
        format!("https://youtu.be/{VIDEO_ID}"),
        format!("https://www.youtube.com/watch?v={VIDEO_ID}"),
        format!("https://youtube.com/shorts/{VIDEO_ID}?feature=share"),
    ] {
        h.adapter
            .validate(&call(
                CONN,
                "111",
                ASSET,
                json!({ "account_id": "111", "kind": "youtube", "youtube_video_id": link }),
            ))
            .await
            .unwrap_or_else(|e| panic!("{link}: {e:?}"));
        let ops = mutate_bodies(&h).await.last().unwrap()["mutateOperations"].clone();
        assert_eq!(
            ops[0]["assetOperation"]["create"]["youtubeVideoAsset"]["youtubeVideoId"], VIDEO_ID,
            "{link}"
        );
    }

    // Local errors, with the field.
    let logo_wide = json!({ "file_path": img.landscape });
    for (input, field) in [
        (json!({ "kind": "logo", "image": logo_wide }), "image"),
        (
            json!({ "kind": "image", "image": { "file_path": img.wide } }),
            "image",
        ),
        (
            json!({ "kind": "image", "image": { "asset": "customers/111/assets/5" } }),
            "image",
        ),
        (json!({ "kind": "image", "image": "x.png" }), "image"),
        (json!({ "kind": "image" }), "image"),
        (
            json!({ "kind": "image", "image": { "file_path": img.square }, "name": "x" }),
            "name",
        ),
        (
            json!({ "kind": "text", "text": "a", "youtube_video_id": VIDEO_ID }),
            "kind",
        ),
        (json!({ "kind": "text", "text": "  " }), "text"),
        (
            json!({ "kind": "youtube", "youtube_video_id": "https://youtu.be/x" }),
            "youtube_video_id",
        ),
        (json!({ "kind": "video" }), "kind"),
    ] {
        let mut input = input;
        input["account_id"] = json!("111");
        let e = asset_err(&h, input.clone()).await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{input}");
        assert_eq!(e.field.as_deref(), Some(field), "{e:?} {input}");
    }
    // A 1:1 logo passes.
    h.adapter
        .validate(&call(
            CONN,
            "111",
            ASSET,
            json!({ "account_id": "111", "kind": "logo", "image": { "file_path": img.square } }),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn upload_video_undo_after_timeout_needs_no_force() {
    let h = google_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(1), Duration::from_millis(3));
    // The state starts UPLOADED and then becomes PROCESSED (mounted first: it takes precedence).
    let current = Arc::new(Mutex::new("UPLOADED".to_string()));
    let c = current.clone();
    Mock::given(method("POST"))
        .and(path(SEARCH_PATH))
        .and(wiremock::matchers::body_string_contains(
            "FROM you_tube_video_upload",
        ))
        .respond_with(move |_: &Request| {
            let row = json!({ "resourceName": UPLOAD_RN, "state": *c.lock().unwrap(),
                              "videoId": VIDEO_ID });
            ResponseTemplate::new(200)
                .set_body_json(json!({ "results": [{ "youTubeVideoUpload": row }] }))
        })
        .with_priority(1)
        .mount(&h.server)
        .await;
    mock_youtube(&h, "UPLOADED", 200).await;
    let file = video_file("video.mp4", 500);
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(VIDEO, video_input(&file), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(
        out["result"][UPLOAD_RN]["you_tube_video_upload.state"],
        "UPLOADED"
    );
    let change_id = out["change_id"].as_str().unwrap().to_string();
    let orig = h.db.get_change(&change_id).unwrap().unwrap();
    // In-progress state: nothing watched, the state goes in the fixed part.
    assert_eq!(orig.inverse.as_ref().unwrap().target["watch"], json!([]));

    *current.lock().unwrap() = "PROCESSED".into();
    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    assert_eq!(requests_to(&h, YT_REMOVE).await.len(), 1);
    assert_eq!(
        h.db.get_change(&change_id).unwrap().unwrap().status,
        ChangeStatus::Undone
    );
}

#[tokio::test]
async fn upload_video_final_state_stays_watched() {
    let h = google_harness().await;
    h.adapter
        .set_poll(Duration::from_millis(1), Duration::from_secs(5));
    mock_youtube(&h, "REJECTED", 200).await;
    let file = video_file("video.mp4", 500);
    let a = video_applied(&h, video_input(&file)).await;
    let target = &a.inverse.unwrap().target;
    assert_eq!(
        target["watch"],
        json!([{ "resource_name": UPLOAD_RN, "resource": "you_tube_video_upload",
                 "fields": ["you_tube_video_upload.state"] }])
    );
    assert!(target["extra"].get(UPLOAD_RN).is_none(), "{target}");
}
