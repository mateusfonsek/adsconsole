//! Google Ads edits: `update_campaign`, `update_ad_group` and `update_ad`, against a
//! stateful fake Google (search reads and mutate writes the same map).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::{ChangeFilter, ChangeStatus};
use adsconsole_google_ads::fields::camel;
use adsconsole_mcp::PlatformAdapter;
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use common::*;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

const CAMPAIGN: &str = "customers/111/campaigns/5";
const BUDGET: &str = "customers/111/campaignBudgets/7";
const AD_GROUP: &str = "customers/111/adGroups/3";
const AD_GROUP_AD: &str = "customers/111/adGroupAds/3~9";
const AD: &str = "customers/111/ads/9";
const MUTATE_PATH: &str = "/v26/customers/111/googleAds:mutate";
const SEARCH_PATH: &str = "/v26/customers/111/googleAds:search";

type State = Arc<Mutex<HashMap<String, Value>>>;

/// Fake Google: `{rn: resource in camelCase}`.
struct FakeAds {
    state: State,
}

impl FakeAds {
    fn get(&self, rn: &str) -> Value {
        self.state.lock().unwrap()[rn].clone()
    }
    fn set(&self, rn: &str, path: &[&str], v: Value) {
        let mut st = self.state.lock().unwrap();
        let mut cur = st.get_mut(rn).unwrap();
        for p in &path[..path.len() - 1] {
            cur = cur.get_mut(*p).unwrap();
        }
        cur[path[path.len() - 1]] = v;
    }
}

fn headlines(n: usize) -> Value {
    Value::Array(
        (0..n)
            .map(|i| json!({ "text": format!("Headline {i}") }))
            .collect(),
    )
}

fn seed(bidding: &str, shared: bool) -> HashMap<String, Value> {
    HashMap::from([
        (
            CAMPAIGN.into(),
            json!({ "resourceName": CAMPAIGN, "id": "5", "name": "A", "status": "ENABLED",
                    "biddingStrategyType": bidding, "campaignBudget": BUDGET,
                    "startDateTime": "2026-01-01 00:00:00",
                    "targetSpend": { "cpcBidCeilingMicros": "1000000" } }),
        ),
        (
            BUDGET.into(),
            json!({ "resourceName": BUDGET, "amountMicros": "30000000",
                    "explicitlyShared": shared }),
        ),
        (
            AD_GROUP.into(),
            json!({ "resourceName": AD_GROUP, "id": "3", "name": "G", "status": "ENABLED",
                    "cpcBidMicros": "500000" }),
        ),
        (
            AD_GROUP_AD.into(),
            json!({ "resourceName": AD_GROUP_AD, "status": "ENABLED" }),
        ),
        (
            AD.into(),
            json!({ "resourceName": AD, "id": "9", "type": "RESPONSIVE_SEARCH_AD",
                    "finalUrls": ["https://a.com"],
                    "responsiveSearchAd": { "headlines": headlines(3),
                        "descriptions": [{ "text": "D1" }, { "text": "D2" }] } }),
        ),
    ])
}

/// Number after `needle` in the query.
fn id_after(q: &str, needle: &str) -> Option<String> {
    let rest = &q[q.find(needle)? + needle.len()..];
    Some(rest.chars().take_while(char::is_ascii_digit).collect())
}

fn search_rows(st: &HashMap<String, Value>, q: &str) -> Vec<Value> {
    let resource: String = q[q.find("FROM ").unwrap() + 5..]
        .chars()
        .take_while(|c| *c != ' ')
        .collect();
    let get = |rn: &str| st.get(rn).cloned();
    if q.contains("criterion_id IN (") {
        let (collection, parent_key) = match resource.as_str() {
            "ad_group_criterion" => ("adGroupCriteria", "ad_group.id = "),
            "campaign_criterion" => ("campaignCriteria", "campaign.id = "),
            "shared_criterion" => ("sharedCriteria", "shared_set.id = "),
            other => panic!("unexpected query on {other}: {q}"),
        };
        let rest = &q[q.find("criterion_id IN (").unwrap() + 17..];
        let parent = id_after(q, parent_key).unwrap();
        let only_negative = q.contains("negative = TRUE");
        return rest[..rest.find(')').unwrap()]
            .split(", ")
            .filter_map(|id| get(&format!("customers/111/{collection}/{parent}~{id}")))
            .filter(|c| !only_negative || c["negative"] == json!(true))
            .map(|c| json!({ camel(&resource): c }))
            .collect();
    }
    let row = if let Some(i) = q.find("resource_name = '") {
        let rest = &q[i + 17..];
        let rn = &rest[..rest.find('\'').unwrap()];
        get(rn).map(|obj| json!({ camel(&resource): obj }))
    } else {
        match resource.as_str() {
            "campaign" => {
                let id = id_after(q, "campaign.id = ").unwrap();
                get(&format!("customers/111/campaigns/{id}")).map(|c| {
                    let budget = c["campaignBudget"].as_str().and_then(get);
                    json!({ "campaign": c, "campaignBudget": budget })
                })
            }
            "ad_group" => {
                let id = id_after(q, "ad_group.id = ").unwrap();
                get(&format!("customers/111/adGroups/{id}")).map(|g| json!({ "adGroup": g }))
            }
            "ad_group_ad" => {
                let g = id_after(q, "ad_group.id = ").unwrap();
                let a = id_after(q, "ad_group_ad.ad.id = ").unwrap();
                get(&format!("customers/111/adGroupAds/{g}~{a}")).map(|mut aga| {
                    aga["ad"] = get(&format!("customers/111/ads/{a}")).unwrap_or(json!({}));
                    json!({ "adGroupAd": aga })
                })
            }
            "asset_group" => {
                let id = id_after(q, "asset_group.id = ").unwrap();
                get(&format!("customers/111/assetGroups/{id}")).map(|g| json!({ "assetGroup": g }))
            }
            "asset_group_asset" | "asset_group_signal" => {
                let id = id_after(q, "asset_group.id = ").unwrap();
                let (collection, key) = if resource == "asset_group_asset" {
                    ("assetGroupAssets", "assetGroupAsset")
                } else {
                    ("assetGroupSignals", "assetGroupSignal")
                };
                let prefix = format!("customers/111/{collection}/{id}~");
                let mut keys: Vec<&String> = st.keys().filter(|k| k.starts_with(&prefix)).collect();
                keys.sort();
                return keys
                    .into_iter()
                    .map(|k| {
                        let obj = st[k].clone();
                        let asset = obj["asset"].as_str().and_then(get);
                        json!({ key: obj, "asset": asset })
                    })
                    .collect();
            }
            other => panic!("unexpected query on {other}: {q}"),
        }
    };
    row.into_iter().collect()
}

fn merge(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                merge(d.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (d, s) => *d = s.clone(),
    }
}

/// Real name of a temporary id (`-N` becomes `100 + N`, also in composite ids `a~-N`).
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

fn is_temp(rn: &str) -> bool {
    rn.rsplit('/')
        .next()
        .unwrap()
        .split('~')
        .any(|p| p.starts_with('-'))
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

fn last_id(rn: &str) -> &str {
    rn.rsplit('/').next().unwrap()
}

fn mutate(st: &mut HashMap<String, Value>, body: &Value) -> Value {
    let apply = body["validateOnly"] != json!(true);
    let ops = body["mutateOperations"].as_array().unwrap();
    let mut temps = HashMap::new();
    for op in ops {
        let (_, inner) = op.as_object().unwrap().iter().next().unwrap();
        if let Some(n) = inner["create"]["resourceName"]
            .as_str()
            .filter(|n| is_temp(n))
        {
            temps.insert(n.to_string(), real_name(n));
        }
    }
    let mut responses = Vec::new();
    let mut derived = 0;
    for op in ops {
        let (key, inner) = op.as_object().unwrap().iter().next().unwrap();
        let rn = if let Some(rn) = inner["remove"].as_str() {
            if apply {
                st.remove(rn)
                    .unwrap_or_else(|| panic!("remove of missing {rn}"));
            }
            rn.to_string()
        } else if inner["create"].is_object() {
            let mut obj = inner["create"].clone();
            subst(&mut obj, &temps);
            let rn = match obj["resourceName"].as_str() {
                Some(rn) => rn.to_string(),
                // Asset group asset without a name: Google derives it from asset group, asset
                // and field type.
                None if obj["fieldType"].is_string() => format!(
                    "customers/111/assetGroupAssets/{}~{}~{}",
                    last_id(obj["assetGroup"].as_str().unwrap()),
                    last_id(obj["asset"].as_str().unwrap()),
                    obj["fieldType"].as_str().unwrap()
                ),
                // Asset group signal without a name: Google derives the id from the content.
                None if obj["assetGroup"].is_string() => {
                    derived += 1;
                    format!(
                        "customers/111/assetGroupSignals/{}~{}",
                        last_id(obj["assetGroup"].as_str().unwrap()),
                        100 + derived
                    )
                }
                // Campaign or ad group criterion without a name: Google derives the id.
                None if obj["sharedSet"].is_null() => {
                    derived += 1;
                    let (collection, parent) = match obj["campaign"].as_str() {
                        Some(c) if obj["adGroup"].is_null() => ("campaignCriteria", c),
                        _ => ("adGroupCriteria", obj["adGroup"].as_str().unwrap()),
                    };
                    format!(
                        "customers/111/{collection}/{}~{}",
                        last_id(parent),
                        100 + derived
                    )
                }
                // `campaignSharedSet` without a name: Google derives it from campaign and list.
                None => format!(
                    "customers/111/campaignSharedSets/{}~{}",
                    last_id(obj["campaign"].as_str().unwrap()),
                    last_id(obj["sharedSet"].as_str().unwrap())
                ),
            };
            obj["resourceName"] = json!(rn);
            // Google returns ENABLED when the create has no status.
            let collection = rn.split('/').nth(2).unwrap();
            if [
                "adGroupCriteria",
                "campaignCriteria",
                "sharedSets",
                "campaignSharedSets",
                "assetGroupAssets",
            ]
            .contains(&collection)
                && obj.get("status").is_none()
            {
                obj["status"] = json!("ENABLED");
            }
            if apply {
                st.insert(rn.clone(), obj);
            }
            rn
        } else {
            let update = &inner["update"];
            let rn = update["resourceName"].as_str().unwrap();
            if apply {
                merge(st.get_mut(rn).expect(rn), update);
            }
            rn.to_string()
        };
        let result = format!("{}Result", key.trim_end_matches("Operation"));
        responses.push(json!({ result: { "resourceName": rn } }));
    }
    json!({ "mutateOperationResponses": responses })
}

async fn fake(h: &GoogleHarness, seed: HashMap<String, Value>) -> FakeAds {
    let state: State = Arc::new(Mutex::new(seed));
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
    FakeAds { state }
}

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
        .filter(|r| r.url.path() == MUTATE_PATH)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// GAQL queries sent to search.
async fn queries_of(h: &GoogleHarness) -> Vec<String> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == SEARCH_PATH)
        .map(|r| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            b["query"].as_str().unwrap().to_string()
        })
        .collect()
}

/// Operations of the last real mutate (without `validateOnly`).
async fn last_applied_ops(h: &GoogleHarness) -> Vec<Value> {
    let bodies = mutate_bodies(h).await;
    let last = bodies
        .iter()
        .rev()
        .find(|b| b["validateOnly"] == json!(false))
        .unwrap();
    last["mutateOperations"].as_array().unwrap().clone()
}

/// The `key` operation in `ops` (e.g. `campaignOperation`).
fn op<'a>(ops: &'a [Value], key: &str) -> &'a Value {
    ops.iter()
        .find_map(|o| o.get(key))
        .unwrap_or_else(|| panic!("no {key} in {ops:?}"))
}

fn with_account(mut input: Value) -> Value {
    input["account_id"] = json!("111");
    input
}

async fn validate_err(
    h: &GoogleHarness,
    tool: &str,
    input: Value,
) -> adsconsole_mcp::error::ToolError {
    h.adapter
        .validate(&call(CONN, "111", tool, with_account(input)))
        .await
        .unwrap_err()
}

fn assert_restored(g: &FakeAds) {
    assert_eq!(g.get(CAMPAIGN)["name"], "A");
    assert_eq!(g.get(BUDGET)["amountMicros"], "30000000");
}

#[tokio::test]
async fn update_campaign_name_and_budget() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(
            "update_campaign",
            with_account(json!({ "campaign_id": "5", "name": "B", "daily_budget": 50 })),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["reversible"], true);
    assert_eq!(
        out["summary"],
        "Campaign 5: name 'A' → 'B', budget 30 → 50 BRL"
    );

    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert_eq!(bodies[0]["validateOnly"], true);
    let ops = bodies[1]["mutateOperations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(
        ops[0],
        json!({ "campaignOperation": { "update": { "resourceName": CAMPAIGN, "name": "B" },
                                       "updateMask": "name" } })
    );
    assert_eq!(
        ops[1],
        json!({ "campaignBudgetOperation": {
            "update": { "resourceName": BUDGET, "amountMicros": "50000000" },
            "updateMask": "amount_micros" } })
    );

    let changes = h.db.list_changes(&ChangeFilter::default()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].before,
        Some(json!({
            CAMPAIGN: { "campaign.name": "A" },
            BUDGET: { "campaign_budget.amount_micros": 30 }
        }))
    );
    assert_eq!(
        changes[0].after,
        Some(json!({
            CAMPAIGN: { "campaign.name": "B" },
            BUDGET: { "campaign_budget.amount_micros": 50 }
        }))
    );

    // Undo: restore_fields sets both fields back.
    let change_id = out["change_id"].as_str().unwrap();
    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    let ops = last_applied_ops(&h).await;
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!(op(&ops, "campaignOperation")["update"]["name"], "A");
    assert_eq!(op(&ops, "campaignOperation")["updateMask"], "name");
    assert_eq!(
        op(&ops, "campaignBudgetOperation")["update"]["amountMicros"],
        "30000000"
    );
    assert_restored(&g);
    let orig = h.db.get_change(change_id).unwrap().unwrap();
    assert_eq!(orig.status, ChangeStatus::Undone);
}

#[tokio::test]
async fn approval_mode_roundtrip() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let engine = engine(&h, WriteMode::Approval);
    let out = engine
        .call_tool(
            "update_campaign",
            with_account(json!({ "campaign_id": "5", "name": "B", "daily_budget": 50 })),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "pending", "{out}");
    // Pending: only the validateOnly.
    assert_eq!(mutate_bodies(&h).await.len(), 1);
    assert_eq!(g.get(CAMPAIGN)["name"], "A");

    let id = out["change_id"].as_str().unwrap().to_string();
    let rec = engine.approve(&id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied, "{rec:?}");
    assert_eq!(g.get(CAMPAIGN)["name"], "B");
    assert_eq!(g.get(BUDGET)["amountMicros"], "50000000");

    let undo = engine
        .call_tool("undo_change", json!({ "change_id": id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "pending", "{undo}");
    let undo_id = undo["change_id"].as_str().unwrap().to_string();
    let rec = engine.approve(&undo_id).await.unwrap();
    assert_eq!(rec.status, ChangeStatus::Applied, "{rec:?}");
    let ops = last_applied_ops(&h).await;
    assert_eq!(op(&ops, "campaignOperation")["update"]["name"], "A");
    assert_eq!(
        op(&ops, "campaignBudgetOperation")["update"]["amountMicros"],
        "30000000"
    );
    assert_restored(&g);
}

#[tokio::test]
async fn shared_budget_warns() {
    let h = google_harness().await;
    fake(&h, seed("TARGET_SPEND", true)).await;
    let c = call(
        CONN,
        "111",
        "update_campaign",
        with_account(json!({ "campaign_id": "5", "daily_budget": 40 })),
    );
    let v = h.adapter.validate(&c).await.unwrap();
    let warning = v.warning.unwrap();
    assert!(warning.contains("shared"), "{warning}");
    assert!(
        warning.contains(
            "This budget is shared with other campaigns, and all of them will be affected."
        )
    );

    // Not shared: no warning.
    let h = google_harness().await;
    fake(&h, seed("TARGET_SPEND", false)).await;
    let v = h.adapter.validate(&c).await.unwrap();
    assert!(v.warning.is_none(), "{:?}", v.warning);
}

#[tokio::test]
async fn budget_precision_follows_currency() {
    let h = google_harness().await;
    fake(&h, seed("TARGET_SPEND", false)).await;
    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "5", "daily_budget": 10.555 }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("daily_budget"));
    assert!(mutate_bodies(&h).await.is_empty());
}

#[tokio::test]
async fn bidding_adjusts_current_target() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let out = engine(&h, WriteMode::Direct)
        .call_tool(
            "update_campaign",
            with_account(json!({ "campaign_id": "5",
                "bidding": { "maximize_clicks": { "cpc_cap": 2.5 } } })),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let ops = last_applied_ops(&h).await;
    let c = op(&ops, "campaignOperation");
    assert_eq!(c["updateMask"], "target_spend.cpc_bid_ceiling_micros");
    assert_eq!(c["update"]["targetSpend"]["cpcBidCeilingMicros"], "2500000");
    assert_eq!(
        g.get(CAMPAIGN)["targetSpend"]["cpcBidCeilingMicros"],
        "2500000"
    );

    let h = google_harness().await;
    fake(&h, seed("MAXIMIZE_CONVERSION_VALUE", false)).await;
    engine(&h, WriteMode::Direct)
        .call_tool(
            "update_campaign",
            with_account(json!({ "campaign_id": "5",
                "bidding": { "maximize_conversion_value": { "target_roas": 3.5 } } })),
            None,
        )
        .await
        .unwrap();
    let ops = last_applied_ops(&h).await;
    let c = op(&ops, "campaignOperation");
    assert_eq!(c["updateMask"], "maximize_conversion_value.target_roas");
    assert_eq!(c["update"]["maximizeConversionValue"]["targetRoas"], 3.5);
}

#[tokio::test]
async fn bidding_type_switch_refused() {
    let h = google_harness().await;
    fake(&h, seed("TARGET_SPEND", false)).await;
    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "5",
                "bidding": { "maximize_conversions": { "target_cpa": 20 } } }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(
        err.message,
        "Changing the bidding strategy type is not supported yet; adjust the target of the current strategy."
    );
    assert!(mutate_bodies(&h).await.is_empty());
}

#[tokio::test]
async fn update_campaign_status_and_dates() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let out = engine(&h, WriteMode::Direct)
        .call_tool(
            "update_campaign",
            with_account(json!({ "campaign_id": "5", "status": "PAUSED",
                                 "end_date": "2026-12-31" })),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let c = op(&last_applied_ops(&h).await, "campaignOperation").clone();
    assert_eq!(c["updateMask"], "status,end_date_time");
    assert_eq!(c["update"]["status"], "PAUSED");
    assert_eq!(c["update"]["endDateTime"], "2026-12-31 23:59:59");
    assert!(c["update"].get("endDate").is_none(), "{c}");
    assert_eq!(g.get(CAMPAIGN)["status"], "PAUSED");

    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "5", "end_date": "31/12/2026" }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("end_date"));
    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "5", "status": "REMOVED" }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn update_requires_some_field() {
    let h = google_harness().await;
    fake(&h, seed("TARGET_SPEND", false)).await;
    for (tool, input) in [
        ("update_campaign", json!({ "campaign_id": "5" })),
        ("update_ad_group", json!({ "ad_group_id": "3" })),
        ("update_ad", json!({ "ad_group_id": "3", "ad_id": "9" })),
    ] {
        let err = validate_err(&h, tool, input).await;
        assert_eq!(err.code, codes::INVALID_INPUT, "{tool}");
    }
    assert!(mutate_bodies(&h).await.is_empty());
}

#[tokio::test]
async fn missing_campaign_is_not_found() {
    let h = google_harness().await;
    fake(&h, seed("TARGET_SPEND", false)).await;
    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "6", "name": "x" }),
    )
    .await;
    assert_eq!(err.code, codes::NOT_FOUND, "{err:?}");
    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "5 OR 1=1", "name": "x" }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("campaign_id"));
}

#[tokio::test]
async fn update_ad_group_name_and_bid() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(
            "update_ad_group",
            with_account(json!({ "ad_group_id": "3", "name": "G2", "cpc_bid": 0.75 })),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let summary = out["summary"].as_str().unwrap();
    assert!(summary.contains("'G' → 'G2'"), "{summary}");
    assert!(summary.contains("0.5 → 0.75 BRL"), "{summary}");
    let c = op(&last_applied_ops(&h).await, "adGroupOperation").clone();
    assert_eq!(c["updateMask"], "name,cpc_bid_micros");
    assert_eq!(c["update"]["cpcBidMicros"], "750000");
    assert_eq!(g.get(AD_GROUP)["name"], "G2");

    engine
        .call_tool(
            "undo_change",
            json!({ "change_id": out["change_id"] }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(g.get(AD_GROUP)["name"], "G");
    assert_eq!(g.get(AD_GROUP)["cpcBidMicros"], "500000");
}

#[tokio::test]
async fn update_ad_status_and_texts() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let engine = engine(&h, WriteMode::Direct);
    let new_headlines = ["One", "Two", "Three", "Four"];
    let out = engine
        .call_tool(
            "update_ad",
            with_account(
                json!({ "ad_group_id": "3", "ad_id": "9", "status": "PAUSED",
                "headlines": new_headlines, "final_urls": ["https://b.com"] }),
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let ops = last_applied_ops(&h).await;
    assert_eq!(ops.len(), 2, "{ops:?}");
    // Verified on a real account: status on ad_group_ad; texts and URLs on the ad resource.
    let aga = op(&ops, "adGroupAdOperation");
    assert_eq!(aga["update"]["resourceName"], AD_GROUP_AD);
    assert_eq!(aga["updateMask"], "status");
    assert_eq!(aga["update"]["status"], "PAUSED");
    let ad = op(&ops, "adOperation");
    assert_eq!(ad["update"]["resourceName"], AD);
    assert_eq!(
        ad["updateMask"],
        "final_urls,responsive_search_ad.headlines"
    );
    assert_eq!(
        ad["update"]["responsiveSearchAd"]["headlines"],
        json!([{ "text": "One" }, { "text": "Two" }, { "text": "Three" }, { "text": "Four" }])
    );
    assert_eq!(ad["update"]["finalUrls"], json!(["https://b.com"]));
    assert_eq!(g.get(AD_GROUP_AD)["status"], "PAUSED");

    engine
        .call_tool(
            "undo_change",
            json!({ "change_id": out["change_id"] }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(g.get(AD_GROUP_AD)["status"], "ENABLED");
    assert_eq!(g.get(AD)["responsiveSearchAd"]["headlines"], headlines(3));
    assert_eq!(g.get(AD)["finalUrls"], json!(["https://a.com"]));
}

#[tokio::test]
async fn update_ad_text_limits() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let long = "x".repeat(31);
    let ok30 = "ç".repeat(30);
    for (input, field) in [
        (json!({ "headlines": ["a", "b", long] }), "headlines"),
        (json!({ "headlines": ["a", "b"] }), "headlines"),
        (json!({ "headlines": vec!["a"; 16] }), "headlines"),
        (json!({ "descriptions": ["just one"] }), "descriptions"),
        (json!({ "descriptions": vec!["d"; 5] }), "descriptions"),
        (
            json!({ "descriptions": ["d", "y".repeat(91)] }),
            "descriptions",
        ),
        (json!({ "final_urls": [] }), "final_urls"),
    ] {
        let mut input = input;
        input["ad_group_id"] = json!("3");
        input["ad_id"] = json!("9");
        let err = validate_err(&h, "update_ad", input.clone()).await;
        assert_eq!(err.code, codes::INVALID_INPUT, "{input}");
        assert_eq!(err.field.as_deref(), Some(field), "{input}");
    }
    assert!(mutate_bodies(&h).await.is_empty());

    // 30 characters (not bytes) pass.
    h.adapter
        .validate(&call(
            CONN,
            "111",
            "update_ad",
            with_account(
                json!({ "ad_group_id": "3", "ad_id": "9", "headlines": ["a", "b", ok30] }),
            ),
        ))
        .await
        .unwrap();

    // Texts only on RSAs.
    g.set(AD, &["type"], json!("EXPANDED_TEXT_AD"));
    let err = validate_err(
        &h,
        "update_ad",
        json!({ "ad_group_id": "3", "ad_id": "9", "headlines": ["a", "b", "c"] }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
    // Status works for any ad type.
    h.adapter
        .validate(&call(
            CONN,
            "111",
            "update_ad",
            with_account(json!({ "ad_group_id": "3", "ad_id": "9", "status": "PAUSED" })),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn edit_undo_conflict() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(
            "update_campaign",
            with_account(json!({ "campaign_id": "5", "name": "B" })),
            None,
        )
        .await
        .unwrap();
    g.set(CAMPAIGN, &["name"], json!("Z"));
    let err = engine
        .call_tool(
            "undo_change",
            json!({ "change_id": out["change_id"] }),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::UNDO_CONFLICT, "{err:?}");
    let diffs = err.details.as_ref().unwrap()["diffs"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(diffs.len(), 1, "{diffs:?}");
    assert_eq!(
        diffs[0]["path"],
        "/customers~1111~1campaigns~15/campaign.name"
    );
    assert_eq!(diffs[0]["expected"], "B");
    assert_eq!(diffs[0]["actual"], "Z");
    assert_eq!(g.get(CAMPAIGN)["name"], "Z");
}

#[tokio::test]
async fn edit_tools_are_exposed_reversible_idempotent() {
    let h = google_harness().await;
    for name in ["update_campaign", "update_ad_group", "update_ad"] {
        let spec = h
            .adapter
            .tools()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name}"));
        assert!(spec.exposed && spec.reversible && spec.idempotent, "{name}");
        assert!(!spec.destructive, "{name}");
    }
}

/// Headlines as the API returns them: with output-only fields and one pinned.
fn api_headlines(label: &str) -> Value {
    json!([
        { "text": "Headline 0", "pinnedField": "HEADLINE_1", "assetPerformanceLabel": label,
          "policySummaryInfo": { "reviewStatus": "REVIEWED", "approvalStatus": "APPROVED" } },
        { "text": "Headline 1", "assetPerformanceLabel": label },
        { "text": "Headline 2", "policySummaryInfo": { "reviewStatus": "REVIEW_IN_PROGRESS" } }
    ])
}

#[tokio::test]
async fn ad_text_output_only_keys_are_ignored_and_not_restored() {
    let h = google_harness().await;
    let mut seed = seed("TARGET_SPEND", false);
    seed.get_mut(AD).unwrap()["responsiveSearchAd"]["headlines"] = api_headlines("PENDING");
    let g = fake(&h, seed).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(
            "update_ad",
            with_account(json!({ "ad_group_id": "3", "ad_id": "9",
                                 "headlines": ["One", "Two", "Three"] })),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let change_id = out["change_id"].as_str().unwrap().to_string();
    let rec = h.db.get_change(&change_id).unwrap().unwrap();
    assert_eq!(
        rec.before.unwrap()[AD]["ad.responsive_search_ad.headlines"],
        json!([{ "text": "Headline 0", "pinnedField": "HEADLINE_1" },
               { "text": "Headline 1" }, { "text": "Headline 2" }])
    );

    // After review, the API adds output-only fields to the new headlines.
    g.set(
        AD,
        &["responsiveSearchAd", "headlines"],
        json!([
            { "text": "One", "assetPerformanceLabel": "LEARNING",
              "policySummaryInfo": { "reviewStatus": "REVIEWED" } },
            { "text": "Two", "assetPerformanceLabel": "GOOD" },
            { "text": "Three" }
        ]),
    );
    let undo = engine
        .call_tool("undo_change", json!({ "change_id": change_id }), None)
        .await
        .unwrap();
    assert_eq!(undo["status"], "applied", "{undo}");
    let ops = last_applied_ops(&h).await;
    let restored = &op(&ops, "adOperation")["update"]["responsiveSearchAd"]["headlines"];
    assert_eq!(
        *restored,
        json!([{ "text": "Headline 0", "pinnedField": "HEADLINE_1" },
               { "text": "Headline 1" }, { "text": "Headline 2" }])
    );
    for item in restored.as_array().unwrap() {
        for key in item.as_object().unwrap().keys() {
            assert!(key == "text" || key == "pinnedField", "{key}");
        }
    }
}

#[tokio::test]
async fn ad_text_edit_warns_when_pins_are_dropped() {
    let pins = "The current headline/description pins will be removed.";
    let h = google_harness().await;
    let mut seeded = seed("TARGET_SPEND", false);
    seeded.get_mut(AD).unwrap()["responsiveSearchAd"]["headlines"] = api_headlines("BEST");
    fake(&h, seeded).await;
    let validate = |input: Value| {
        let c = call(CONN, "111", "update_ad", with_account(input));
        let a = h.adapter.clone();
        async move { a.validate(&c).await.unwrap() }
    };
    let v =
        validate(json!({ "ad_group_id": "3", "ad_id": "9", "headlines": ["a", "b", "c"] })).await;
    assert!(
        v.warning.as_deref().is_some_and(|w| w.contains(pins)),
        "{:?}",
        v.warning
    );
    // Only status, or descriptions without pins: no warning.
    let v = validate(json!({ "ad_group_id": "3", "ad_id": "9", "status": "PAUSED" })).await;
    assert!(v.warning.is_none(), "{:?}", v.warning);
    let v =
        validate(json!({ "ad_group_id": "3", "ad_id": "9", "descriptions": ["d1", "d2"] })).await;
    assert!(v.warning.is_none(), "{:?}", v.warning);

    // Without current pins, editing headlines doesn't warn.
    let h = google_harness().await;
    fake(&h, seed("TARGET_SPEND", false)).await;
    let c = call(
        CONN,
        "111",
        "update_ad",
        with_account(json!({ "ad_group_id": "3", "ad_id": "9", "headlines": ["a", "b", "c"] })),
    );
    assert!(h.adapter.validate(&c).await.unwrap().warning.is_none());
}

#[tokio::test]
async fn single_date_is_checked_against_current_dates() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    // Current start: 2026-01-01.
    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "5", "end_date": "2025-12-31" }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("end_date"));
    h.adapter
        .validate(&call(
            CONN,
            "111",
            "update_campaign",
            with_account(json!({ "campaign_id": "5", "end_date": "2026-01-01" })),
        ))
        .await
        .unwrap();

    g.set(CAMPAIGN, &["endDateTime"], json!("2026-06-30 23:59:59"));
    let err = validate_err(
        &h,
        "update_campaign",
        json!({ "campaign_id": "5", "start_date": "2026-07-01" }),
    )
    .await;
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert_eq!(err.field.as_deref(), Some("start_date"));
    // Only the validateOnly of the valid date reached mutate.
    assert_eq!(mutate_bodies(&h).await.len(), 1);
}

#[tokio::test]
async fn campaign_dates_use_datetime_fields_and_undo_restores_them() {
    let h = google_harness().await;
    let g = fake(&h, seed("TARGET_SPEND", false)).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = engine
        .call_tool(
            "update_campaign",
            with_account(json!({ "campaign_id": "5", "start_date": "2026-02-01",
                                 "end_date": "2026-12-31" })),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    let c = op(&last_applied_ops(&h).await, "campaignOperation").clone();
    assert_eq!(c["updateMask"], "start_date_time,end_date_time");
    assert_eq!(c["update"]["startDateTime"], "2026-02-01 00:00:00");
    assert_eq!(c["update"]["endDateTime"], "2026-12-31 23:59:59");
    // The queries ask for the datetime fields, never start_date/end_date.
    for q in queries_of(&h).await {
        assert!(!q.contains("campaign.start_date,"), "{q}");
        assert!(!q.contains("campaign.end_date,"), "{q}");
        assert!(!q.contains("campaign.end_date "), "{q}");
    }
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        change.before,
        Some(
            json!({ CAMPAIGN: { "campaign.start_date_time": "2026-01-01 00:00:00",
                                 "campaign.end_date_time": null } })
        )
    );

    // Undo: the start goes back to the old datetime and the null end clears the field.
    undo(&engine, &out).await;
    let c = op(&last_applied_ops(&h).await, "campaignOperation").clone();
    let mut mask: Vec<&str> = c["updateMask"].as_str().unwrap().split(',').collect();
    mask.sort_unstable();
    assert_eq!(mask, ["end_date_time", "start_date_time"]);
    assert_eq!(c["update"]["startDateTime"], "2026-01-01 00:00:00");
    assert!(
        c["update"].get("endDateTime").is_none_or(Value::is_null),
        "{c}"
    );
    assert_eq!(g.get(CAMPAIGN)["startDateTime"], "2026-01-01 00:00:00");
}

// ------------------------------------------------- manage_keywords / negatives

const KW44: &str = "customers/111/adGroupCriteria/3~44";
const KW45: &str = "customers/111/adGroupCriteria/3~45";

fn kw_seed() -> HashMap<String, Value> {
    let mut s = seed("TARGET_SPEND", false);
    s.insert(
        KW44.into(),
        json!({ "resourceName": KW44, "criterionId": "44", "status": "ENABLED",
                "cpcBidMicros": "500000",
                "keyword": { "text": "shoe", "matchType": "BROAD" } }),
    );
    s.insert(
        KW45.into(),
        json!({ "resourceName": KW45, "criterionId": "45", "status": "PAUSED",
                "keyword": { "text": "leather boot", "matchType": "PHRASE" } }),
    );
    s
}

async fn applied(engine: &Engine, tool: &str, input: Value) -> Value {
    let out = engine
        .call_tool(tool, with_account(input), None)
        .await
        .unwrap();
    assert_eq!(out["status"], "applied", "{out}");
    out
}

async fn undo(engine: &Engine, out: &Value) -> Value {
    let u = engine
        .call_tool(
            "undo_change",
            json!({ "change_id": out["change_id"].as_str().unwrap() }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(u["status"], "applied", "{u}");
    u
}

fn removes(ops: &[Value]) -> Vec<String> {
    ops.iter()
        .map(|o| {
            o.as_object().unwrap().values().next().unwrap()["remove"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn add_keywords_and_undo() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "add", "keywords": [
            { "text": "leather shoe", "match_type": "EXACT", "cpc_bid": 1.5 },
            { "text": "boot", "match_type": "PHRASE" } ] }),
    )
    .await;
    assert_eq!(out["reversible"], true);

    let ops = last_applied_ops(&h).await;
    assert_eq!(ops.len(), 2);
    assert_eq!(
        ops[0],
        json!({ "adGroupCriterionOperation": { "create": {
            "resourceName": "customers/111/adGroupCriteria/3~-1",
            "adGroup": AD_GROUP, "status": "PAUSED", "cpcBidMicros": "1500000",
            "keyword": { "text": "leather shoe", "matchType": "EXACT" } } } })
    );
    let second = &ops[1]["adGroupCriterionOperation"]["create"];
    assert_eq!(second["resourceName"], "customers/111/adGroupCriteria/3~-2");
    assert_eq!(
        second["keyword"],
        json!({ "text": "boot", "matchType": "PHRASE" })
    );
    assert!(second.get("cpcBidMicros").is_none());

    let real1 = "customers/111/adGroupCriteria/3~101";
    let real2 = "customers/111/adGroupCriteria/3~102";
    assert_eq!(g.get(real1)["status"], "PAUSED");
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(change.before, Some(json!({})));
    assert_eq!(
        change.after,
        Some(json!({
            real1: { "ad_group_criterion.status": "PAUSED" },
            real2: { "ad_group_criterion.status": "PAUSED" } }))
    );

    undo(&engine, &out).await;
    assert_eq!(removes(&last_applied_ops(&h).await), vec![real1, real2]);
    assert!(!g.state.lock().unwrap().contains_key(real1));
    assert!(!g.state.lock().unwrap().contains_key(real2));
    assert_eq!(
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .status,
        ChangeStatus::Undone
    );
}

#[tokio::test]
async fn pause_keyword_restorable() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "pause", "keywords": [{ "criterion_id": "44" }] }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    assert_eq!(
        out["summary"],
        "Ad group 3: status of 'shoe' ACTIVE → PAUSED"
    );
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops[0],
        json!({ "adGroupCriterionOperation": {
            "update": { "resourceName": KW44, "status": "PAUSED" }, "updateMask": "status" } })
    );
    assert_eq!(g.get(KW44)["status"], "PAUSED");
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        change.before,
        Some(json!({ KW44: { "ad_group_criterion.status": "ENABLED" } }))
    );

    undo(&engine, &out).await;
    assert_eq!(g.get(KW44)["status"], "ENABLED");

    // enable
    let out = applied(
        &engine,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "enable", "keywords": [{ "criterion_id": "45" }] }),
    )
    .await;
    assert_eq!(g.get(KW45)["status"], "ENABLED");
    undo(&engine, &out).await;
    assert_eq!(g.get(KW45)["status"], "PAUSED");
}

#[tokio::test]
async fn set_bid_restorable() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "set_bid",
                "keywords": [{ "criterion_id": "44", "cpc_bid": 2.5 }] }),
    )
    .await;
    assert_eq!(g.get(KW44)["cpcBidMicros"], "2500000");
    assert_eq!(out["summary"], "Ad group 3: bid of 'shoe' 0.5 → 2.5 BRL");
    undo(&engine, &out).await;
    assert_eq!(g.get(KW44)["cpcBidMicros"], "500000");

    let e = validate_err(
        &h,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "set_bid", "keywords": [{ "criterion_id": "44" }] }),
    )
    .await;
    assert_eq!(
        (e.code.as_str(), e.field.as_deref()),
        (codes::INVALID_INPUT, Some("keywords[0].cpc_bid"))
    );
}

#[tokio::test]
async fn remove_is_irreversible() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let v = h
        .adapter
        .validate(&call(
            CONN,
            "111",
            "manage_keywords",
            with_account(json!({ "ad_group_id": "3", "action": "remove",
                "keywords": [{ "criterion_id": "44" }] })),
        ))
        .await
        .unwrap();
    assert_eq!(v.warning.as_deref(), Some("This change cannot be undone."));
    let out = applied(
        &engine,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "remove",
                "keywords": [{ "criterion_id": "44" }, { "criterion_id": "45" }] }),
    )
    .await;
    assert_eq!(out["reversible"], false);
    assert_eq!(out["warning"], "This change cannot be undone.");
    assert_eq!(removes(&last_applied_ops(&h).await), vec![KW44, KW45]);
    assert!(g.state.lock().unwrap().get(KW44).is_none());
    let changes = h.db.list_changes(&ChangeFilter::default()).unwrap();
    assert!(!changes[0].reversible);
    assert!(changes[0].inverse.is_none());
}

#[tokio::test]
async fn change_match_type_one_mutate() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "change_match_type",
                "keywords": [{ "criterion_id": "44", "match_type": "EXACT" }] }),
    )
    .await;
    assert_eq!(out["reversible"], false);
    let bodies = mutate_bodies(&h).await;
    assert_eq!(bodies.len(), 2, "validateOnly + apply");
    let ops = last_applied_ops(&h).await;
    assert_eq!(ops.len(), 2);
    assert_eq!(
        ops[0],
        json!({ "adGroupCriterionOperation": { "create": {
            "resourceName": "customers/111/adGroupCriteria/3~-1",
            "adGroup": AD_GROUP, "status": "ENABLED", "cpcBidMicros": "500000",
            "keyword": { "text": "shoe", "matchType": "EXACT" } } } })
    );
    assert_eq!(removes(&ops[1..]), vec![KW44]);
    let new = g.get("customers/111/adGroupCriteria/3~101");
    assert_eq!(new["keyword"]["matchType"], "EXACT");
    assert!(g.state.lock().unwrap().get(KW44).is_none());
    assert!(!h.db.list_changes(&ChangeFilter::default()).unwrap()[0].reversible);

    // The plan's warning shows in validation, approval and the applied response.
    assert_eq!(
        out["warning"],
        "Google does not allow changing the match type; the old keyword will be removed \
         and a new one created. This change cannot be undone."
    );
    let v = h
        .adapter
        .validate(&call(
            CONN,
            "111",
            "manage_keywords",
            with_account(json!({ "ad_group_id": "3", "action": "change_match_type",
                "keywords": [{ "criterion_id": "45", "match_type": "EXACT" }] })),
        ))
        .await
        .unwrap();
    assert!(v.warning.unwrap().starts_with(
        "Google does not allow changing the match type; \
             the old keyword will be removed and a new one created."
    ));

    // Same match type: nothing to do.
    let e = validate_err(
        &h,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "change_match_type",
                "keywords": [{ "criterion_id": "45", "match_type": "PHRASE" }] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn keyword_input_validation() {
    let h = google_harness().await;
    fake(&h, kw_seed()).await;
    let cases = [
        (json!({ "action": "add", "keywords": [] }), "keywords"),
        (
            json!({ "action": "add", "keywords": [{ "match_type": "EXACT" }] }),
            "keywords[0].text",
        ),
        (
            json!({ "action": "add", "keywords": [{ "text": "a" }] }),
            "keywords[0].match_type",
        ),
        (
            json!({ "action": "add", "keywords": [{ "text": "x".repeat(81), "match_type": "EXACT" }] }),
            "keywords[0].text",
        ),
        (
            json!({ "action": "add", "keywords": [{ "text": "a b c d e f g h i j k", "match_type": "EXACT" }] }),
            "keywords[0].text",
        ),
        (
            json!({ "action": "pause", "keywords": [{}] }),
            "keywords[0].criterion_id",
        ),
        (
            json!({ "action": "pause", "keywords": [{ "criterion_id": "4x" }] }),
            "keywords[0].criterion_id",
        ),
        (
            json!({ "action": "pause", "keywords": [{ "criterion_id": "44" }, { "criterion_id": "44" }] }),
            "keywords[1].criterion_id",
        ),
        (
            json!({ "action": "pause", "keywords": [{ "criterion_id": "99" }] }),
            "keywords[0].criterion_id",
        ),
    ];
    for (mut input, field) in cases {
        input["ad_group_id"] = json!("3");
        let e = validate_err(&h, "manage_keywords", input.clone()).await;
        assert_eq!(e.field.as_deref(), Some(field), "{input} -> {e:?}");
    }
    // Exact limits pass: 80 characters and 10 words.
    let ok = json!({ "ad_group_id": "3", "action": "add", "keywords": [
        { "text": "x".repeat(80), "match_type": "EXACT" },
        { "text": "a b c d e f g h i j", "match_type": "BROAD" }] });
    h.adapter
        .validate(&call(CONN, "111", "manage_keywords", with_account(ok)))
        .await
        .unwrap();
    // More than 200 per call.
    let many: Vec<Value> = (0..201)
        .map(|i| json!({ "text": format!("k{i}"), "match_type": "EXACT" }))
        .collect();
    let e = validate_err(
        &h,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "add", "keywords": many }),
    )
    .await;
    assert_eq!(e.field.as_deref(), Some("keywords"));
    let e = validate_err(
        &h,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "add", "keywords": [{ "text": "a", "match_type": "STRICT" }] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn negatives_levels() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);

    // campaign
    let out = applied(
        &engine,
        "manage_negatives",
        json!({ "level": "campaign", "campaign_id": "5", "action": "add",
                "keywords": [{ "text": "free", "match_type": "BROAD" }] }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops[0],
        json!({ "campaignCriterionOperation": { "create": {
            "resourceName": "customers/111/campaignCriteria/5~-1",
            "campaign": CAMPAIGN, "negative": true,
            "keyword": { "text": "free", "matchType": "BROAD" } } } })
    );
    let real = "customers/111/campaignCriteria/5~101";
    assert_eq!(g.get(real)["negative"], true);
    undo(&engine, &out).await;
    assert_eq!(removes(&last_applied_ops(&h).await), vec![real]);

    // ad group
    let out = applied(
        &engine,
        "manage_negatives",
        json!({ "level": "ad_group", "ad_group_id": "3", "action": "add",
                "keywords": [{ "text": "used", "match_type": "EXACT" }] }),
    )
    .await;
    let c = &last_applied_ops(&h).await[0]["adGroupCriterionOperation"]["create"];
    assert_eq!(c["negative"], true);
    assert_eq!(c["adGroup"], AD_GROUP);
    assert!(c.get("status").is_none());
    undo(&engine, &out).await;

    // shared list
    applied(
        &engine,
        "manage_negatives",
        json!({ "level": "shared_list", "shared_set_id": "9", "action": "add",
                "keywords": [{ "text": "cheap", "match_type": "PHRASE" }] }),
    )
    .await;
    let c = &last_applied_ops(&h).await[0]["sharedCriterionOperation"]["create"];
    assert_eq!(c["sharedSet"], "customers/111/sharedSets/9");
    assert_eq!(
        c["keyword"],
        json!({ "text": "cheap", "matchType": "PHRASE" })
    );
    assert!(c.get("negative").is_none());

    // ids required per level
    let kw = json!([{ "text": "a", "match_type": "EXACT" }]);
    for (input, field) in [
        (
            json!({ "level": "shared_list", "action": "add", "keywords": kw }),
            "shared_set_id",
        ),
        (
            json!({ "level": "campaign", "action": "add", "keywords": kw }),
            "campaign_id",
        ),
        (
            json!({ "level": "ad_group", "action": "add", "keywords": kw }),
            "ad_group_id",
        ),
        (json!({ "action": "add", "keywords": kw }), "level"),
        (
            json!({ "level": "campaign", "campaign_id": "5", "action": "add", "keywords": [] }),
            "keywords",
        ),
        (
            json!({ "level": "campaign", "campaign_id": "5", "action": "remove", "keywords": [{ "text": "a" }] }),
            "keywords[0].criterion_id",
        ),
        (
            json!({ "action": "create_list", "keywords": kw }),
            "list_name",
        ),
        (
            json!({ "action": "attach_list", "campaign_id": "5" }),
            "shared_set_id",
        ),
        (
            json!({ "action": "attach_list", "shared_set_id": "9" }),
            "campaign_id",
        ),
    ] {
        let e = validate_err(&h, "manage_negatives", input.clone()).await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{input}");
        assert_eq!(e.field.as_deref(), Some(field), "{input}");
    }
}

#[tokio::test]
async fn negatives_remove_is_irreversible() {
    let h = google_harness().await;
    let mut s = kw_seed();
    let neg = "customers/111/campaignCriteria/5~70";
    s.insert(
        neg.into(),
        json!({ "resourceName": neg, "criterionId": "70", "negative": true,
            "keyword": { "text": "free", "matchType": "BROAD" } }),
    );
    // A campaign location (not a keyword) and a positive ad group keyword.
    let loc = "customers/111/campaignCriteria/5~71";
    s.insert(
        loc.into(),
        json!({ "resourceName": loc, "criterionId": "71", "location": {} }),
    );
    let g = fake(&h, s).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_negatives",
        json!({ "level": "campaign", "campaign_id": "5", "action": "remove",
                "keywords": [{ "criterion_id": "70" }] }),
    )
    .await;
    assert_eq!(out["reversible"], false);
    assert_eq!(out["warning"], "This change cannot be undone.");
    assert_eq!(removes(&last_applied_ops(&h).await), vec![neg]);
    assert!(g.state.lock().unwrap().get(neg).is_none());
    assert_eq!(
        out["summary"],
        "Removes 1 negative keyword(s) from campaign 5: 'free' [BROAD]"
    );
    let applied_before = mutate_bodies(&h).await.len();

    // Campaign location and positive ad group keyword: NOT_FOUND, no mutate.
    for input in [
        json!({ "level": "campaign", "campaign_id": "5", "action": "remove",
                "keywords": [{ "criterion_id": "71" }] }),
        json!({ "level": "ad_group", "ad_group_id": "3", "action": "remove",
                "keywords": [{ "criterion_id": "44" }] }),
    ] {
        let e = validate_err(&h, "manage_negatives", input.clone()).await;
        assert_eq!(e.code, codes::NOT_FOUND, "{input}");
        assert_eq!(e.field.as_deref(), Some("keywords[0].criterion_id"));
    }
    assert_eq!(mutate_bodies(&h).await.len(), applied_before);
    assert!(g.state.lock().unwrap().contains_key(KW44));
}

#[tokio::test]
async fn negative_removed_elsewhere_is_undo_conflict() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_negatives",
        json!({ "level": "campaign", "campaign_id": "5", "action": "add",
                "keywords": [{ "text": "free", "match_type": "broad" }] }),
    )
    .await;
    let real = "customers/111/campaignCriteria/5~101";
    assert_eq!(g.get(real)["keyword"]["matchType"], "BROAD");
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        change.after.unwrap()[real]["campaign_criterion.status"],
        "ENABLED"
    );
    g.set(real, &["status"], json!("REMOVED"));
    let r = engine
        .call_tool(
            "undo_change",
            json!({ "change_id": out["change_id"] }),
            None,
        )
        .await;
    let err = r.unwrap_err();
    assert_eq!(err.code, codes::UNDO_CONFLICT, "{err:?}");
}

#[tokio::test]
async fn match_type_is_case_insensitive_with_field_error() {
    let h = google_harness().await;
    fake(&h, kw_seed()).await;
    let ok = json!({ "ad_group_id": "3", "action": "add",
                     "keywords": [{ "text": "a", "match_type": "Phrase" }] });
    h.adapter
        .validate(&call(CONN, "111", "manage_keywords", with_account(ok)))
        .await
        .unwrap();
    let e = validate_err(
        &h,
        "manage_keywords",
        json!({ "ad_group_id": "3", "action": "add",
                "keywords": [{ "text": "a", "match_type": "STRICT" }] }),
    )
    .await;
    assert_eq!(e.field.as_deref(), Some("keywords[0].match_type"));
}

#[tokio::test]
async fn create_and_attach_list() {
    let h = google_harness().await;
    let g = fake(&h, kw_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_negatives",
        json!({ "action": "create_list", "list_name": "General negatives", "campaign_id": "5",
                "keywords": [{ "text": "free", "match_type": "BROAD" },
                             { "text": "used", "match_type": "EXACT" }] }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    assert_eq!(ops.len(), 4);
    let set = op(&ops, "sharedSetOperation")["create"].clone();
    assert_eq!(set["name"], "General negatives");
    assert_eq!(set["type"], "NEGATIVE_KEYWORDS");
    let temp = set["resourceName"].as_str().unwrap();
    assert!(temp.starts_with("customers/111/sharedSets/-"));
    let crit: Vec<&Value> = ops
        .iter()
        .filter_map(|o| o.get("sharedCriterionOperation"))
        .collect();
    assert_eq!(crit.len(), 2);
    assert!(crit.iter().all(|c| c["create"]["sharedSet"] == temp));
    let attach = &op(&ops, "campaignSharedSetOperation")["create"];
    assert_eq!(attach["sharedSet"], temp);
    assert_eq!(attach["campaign"], CAMPAIGN);
    // The link has no name (its `-N` would repeat the sharedSet's: DUPLICATE_TEMP_IDS) and
    // no temporary id repeats across the operations.
    assert!(attach.get("resourceName").is_none(), "{attach}");
    let mut names: Vec<&str> = ops
        .iter()
        .filter_map(|o| o.as_object()?.values().next()?["create"]["resourceName"].as_str())
        .collect();
    let n = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!((n, names.len()), (3, 3));
    let own_ids: std::collections::HashSet<&str> = names
        .iter()
        .map(|n| n.rsplit(['/', '~']).next().unwrap())
        .collect();
    assert_eq!(own_ids.len(), 3, "{names:?}");

    // The temporary id becomes the real id everywhere; undo removes children before the list.
    let set_real = "customers/111/sharedSets/101";
    assert_eq!(g.get(set_real)["name"], "General negatives");
    assert_eq!(
        g.get("customers/111/sharedCriteria/101~102")["sharedSet"],
        set_real
    );
    assert_eq!(
        g.get("customers/111/campaignSharedSets/5~101")["sharedSet"],
        set_real
    );
    undo(&engine, &out).await;
    let rm = removes(&last_applied_ops(&h).await);
    assert_eq!(rm.last().unwrap(), set_real);
    assert_eq!(rm.len(), 4);
    assert!(rm[..3].iter().all(|r| !r.contains("/sharedSets/")));
    assert!(g.state.lock().unwrap().get(set_real).is_none());

    // Attach a list that already exists.
    let out = applied(
        &engine,
        "manage_negatives",
        json!({ "action": "attach_list", "campaign_id": "5", "shared_set_id": "9" }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops[0],
        json!({ "campaignSharedSetOperation": { "create": {
            "campaign": CAMPAIGN, "sharedSet": "customers/111/sharedSets/9" } } })
    );
    let attached = "customers/111/campaignSharedSets/5~9";
    assert!(g.state.lock().unwrap().contains_key(attached));
    undo(&engine, &out).await;
    assert_eq!(removes(&last_applied_ops(&h).await), vec![attached]);
}

#[tokio::test]
async fn keyword_tools_are_exposed_and_flagged() {
    let h = google_harness().await;
    for name in ["manage_keywords", "manage_negatives"] {
        let spec = h
            .adapter
            .tools()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name}"));
        assert!(
            spec.exposed && spec.reversible && spec.destructive,
            "{name}"
        );
    }
}

// ------------------------------------------------------- manage_targeting

const CC_LOC: &str = "customers/111/campaignCriteria/5~70";
const CC_DEV: &str = "customers/111/campaignCriteria/5~71";
const CC_LANG: &str = "customers/111/campaignCriteria/5~72";
const AC_AUD: &str = "customers/111/adGroupCriteria/3~80";

fn targeting_seed() -> HashMap<String, Value> {
    let mut s = kw_seed();
    s.insert(
        CC_LOC.into(),
        json!({ "resourceName": CC_LOC, "criterionId": "70", "type": "LOCATION",
                "bidModifier": 0.8, "status": "ENABLED",
                "location": { "geoTargetConstant": "geoTargetConstants/1001773" } }),
    );
    s.insert(
        CC_DEV.into(),
        json!({ "resourceName": CC_DEV, "criterionId": "71", "type": "DEVICE",
                "bidModifier": 1.0, "status": "ENABLED", "device": { "type": "MOBILE" } }),
    );
    s.insert(
        CC_LANG.into(),
        json!({ "resourceName": CC_LANG, "criterionId": "72", "type": "LANGUAGE",
                "status": "ENABLED",
                "language": { "languageConstant": "languageConstants/1014" } }),
    );
    s.insert(
        AC_AUD.into(),
        json!({ "resourceName": AC_AUD, "criterionId": "80", "type": "USER_LIST",
                "bidModifier": 1.5, "status": "ENABLED",
                "userList": { "userList": "customers/111/userLists/9" } }),
    );
    s
}

#[tokio::test]
async fn add_location_and_schedule() {
    let h = google_harness().await;
    let g = fake(&h, targeting_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "add", "criteria": [
            { "type": "location", "geo_id": "1001773", "negative": true },
            { "type": "ad_schedule", "day": "monday", "start_hour": 9, "end_hour": 18,
              "bid_modifier": 1.2 },
            { "type": "language", "language_id": "1014" },
            { "type": "device", "device": "TABLET", "bid_modifier": 0 } ] }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops[0],
        json!({ "campaignCriterionOperation": { "create": {
            "campaign": CAMPAIGN, "negative": true,
            "location": { "geoTargetConstant": "geoTargetConstants/1001773" } } } })
    );
    assert_eq!(
        ops[1]["campaignCriterionOperation"]["create"],
        json!({ "campaign": CAMPAIGN,
                "bidModifier": 1.2,
                "adSchedule": { "dayOfWeek": "MONDAY", "startHour": 9, "startMinute": "ZERO",
                                "endHour": 18, "endMinute": "ZERO" } })
    );
    assert_eq!(
        ops[2]["campaignCriterionOperation"]["create"]["language"],
        json!({ "languageConstant": "languageConstants/1014" })
    );
    let dev = &ops[3]["campaignCriterionOperation"]["create"];
    assert_eq!(dev["device"], json!({ "type": "TABLET" }));
    assert_eq!(dev["bidModifier"], 0.0);

    let reals: Vec<String> = (1..=4)
        .map(|n| format!("customers/111/campaignCriteria/5~{}", 100 + n))
        .collect();
    for r in &reals {
        assert!(g.state.lock().unwrap().contains_key(r), "{r}");
    }
    undo(&engine, &out).await;
    assert_eq!(removes(&last_applied_ops(&h).await), reals);
    for r in &reals {
        assert!(!g.state.lock().unwrap().contains_key(r));
    }
}

#[tokio::test]
async fn engine_roundtrip_location_add_undo_removes_real_name() {
    let h = google_harness().await;
    let g = fake(&h, targeting_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "add",
                "criteria": [{ "type": "location", "geo_id": "2076" }] }),
    )
    .await;
    let real = "customers/111/campaignCriteria/5~101";
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        change.after,
        Some(json!({ real: { "campaign_criterion.status": "ENABLED" } }))
    );
    undo(&engine, &out).await;
    assert_eq!(removes(&last_applied_ops(&h).await), vec![real]);
    assert!(!g.state.lock().unwrap().contains_key(real));
    assert_eq!(
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .status,
        ChangeStatus::Undone
    );
}

#[tokio::test]
async fn targeting_removed_elsewhere_is_undo_conflict() {
    let h = google_harness().await;
    let g = fake(&h, targeting_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "add",
                "criteria": [{ "type": "language", "language_id": "1000" }] }),
    )
    .await;
    g.set(
        "customers/111/campaignCriteria/5~101",
        &["status"],
        json!("REMOVED"),
    );
    let err = engine
        .call_tool(
            "undo_change",
            json!({ "change_id": out["change_id"] }),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::UNDO_CONFLICT, "{err:?}");
}

#[tokio::test]
async fn ad_group_level_rules() {
    let h = google_harness().await;
    let g = fake(&h, targeting_seed()).await;
    for c in [
        json!({ "type": "location", "geo_id": "1" }),
        json!({ "type": "language", "language_id": "1000" }),
        json!({ "type": "device", "device": "MOBILE", "bid_modifier": 1.1 }),
        json!({ "type": "ad_schedule", "day": "MONDAY", "start_hour": 1, "end_hour": 2 }),
    ]
    .into_iter()
    {
        let e = validate_err(
            &h,
            "manage_targeting",
            json!({ "campaign_id": "5", "ad_group_id": "3", "action": "add", "criteria": [c] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{c}");
        assert_eq!(e.field.as_deref(), Some("criteria[0].type"));
    }
    let engine = engine(&h, WriteMode::Direct);
    applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "ad_group_id": "3", "action": "add", "criteria": [
            { "type": "audience", "user_list_id": "9" },
            { "type": "placement", "url": "https://example.com/page" },
            { "type": "topic", "topic_id": "66" } ] }),
    )
    .await;
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops[0],
        json!({ "adGroupCriterionOperation": { "create": {
            "adGroup": AD_GROUP,
            "userList": { "userList": "customers/111/userLists/9" } } } })
    );
    assert_eq!(
        ops[1]["adGroupCriterionOperation"]["create"]["placement"],
        json!({ "url": "https://example.com/page" })
    );
    assert_eq!(
        ops[2]["adGroupCriterionOperation"]["create"]["topic"],
        json!({ "topicConstant": "topicConstants/66" })
    );
    assert!(
        g.state
            .lock()
            .unwrap()
            .contains_key("customers/111/adGroupCriteria/3~101")
    );

    // The campaign level also accepts all three.
    applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "add", "criteria": [
            { "type": "placement", "url": "example.com" } ] }),
    )
    .await;
    let ops = last_applied_ops(&h).await;
    assert!(ops[0]["campaignCriterionOperation"]["create"]["placement"].is_object());
}

#[tokio::test]
async fn bid_modifier_limits_and_restore() {
    let h = google_harness().await;
    let g = fake(&h, targeting_seed()).await;
    for bad in [11.0, 0.05, 0.0] {
        let e = validate_err(
            &h,
            "manage_targeting",
            json!({ "campaign_id": "5", "action": "set_bid_modifier",
                    "criteria": [{ "type": "location", "criterion_id": "70",
                                   "bid_modifier": bad }] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{bad}");
        assert_eq!(e.field.as_deref(), Some("criteria[0].bid_modifier"));
    }
    // A device accepts 0 (exclusion).
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "set_bid_modifier",
                "criteria": [{ "type": "device", "criterion_id": "71", "bid_modifier": 0 }] }),
    )
    .await;
    assert_eq!(g.get(CC_DEV)["bidModifier"], 0.0);
    undo(&engine, &out).await;
    assert_eq!(g.get(CC_DEV)["bidModifier"], 1.0);

    let out = applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "set_bid_modifier",
                "criteria": [{ "type": "location", "criterion_id": "70",
                               "bid_modifier": 1.2 }] }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops[0],
        json!({ "campaignCriterionOperation": {
            "update": { "resourceName": CC_LOC, "bidModifier": 1.2 },
            "updateMask": "bid_modifier" } })
    );
    assert_eq!(g.get(CC_LOC)["bidModifier"], 1.2);
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        change.before,
        Some(json!({ CC_LOC: { "campaign_criterion.bid_modifier": 0.8 } }))
    );
    undo(&engine, &out).await;
    assert_eq!(g.get(CC_LOC)["bidModifier"], 0.8);

    // Ad group level.
    applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "ad_group_id": "3", "action": "set_bid_modifier",
                "criteria": [{ "type": "audience", "criterion_id": "80", "bid_modifier": 2 }] }),
    )
    .await;
    assert_eq!(g.get(AC_AUD)["bidModifier"], 2.0);

    // Language has no bid modifier; a criterion of another type is not touched.
    for (kind, id) in [("language", "72"), ("device", "70")] {
        let e = validate_err(
            &h,
            "manage_targeting",
            json!({ "campaign_id": "5", "action": "set_bid_modifier",
                    "criteria": [{ "type": kind, "criterion_id": id, "bid_modifier": 1.1 }] }),
        )
        .await;
        assert!(
            e.code == codes::INVALID_INPUT || e.code == codes::NOT_FOUND,
            "{kind}: {e:?}"
        );
    }
}

#[tokio::test]
async fn schedule_hours_validated() {
    let h = google_harness().await;
    fake(&h, targeting_seed()).await;
    for (start, end, field) in [
        (10, 9, "criteria[0].end_hour"),
        (10, 10, "criteria[0].end_hour"),
        (24, 24, "criteria[0].start_hour"),
        (0, 25, "criteria[0].end_hour"),
        (0, 0, "criteria[0].end_hour"),
    ] {
        let e = validate_err(
            &h,
            "manage_targeting",
            json!({ "campaign_id": "5", "action": "add", "criteria": [
                { "type": "ad_schedule", "day": "FRIDAY", "start_hour": start,
                  "end_hour": end }] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{start}-{end}");
        assert_eq!(e.field.as_deref(), Some(field), "{start}-{end}");
    }
    let e = validate_err(
        &h,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "add", "criteria": [
            { "type": "ad_schedule", "day": "FUNDAY", "start_hour": 1, "end_hour": 2 }] }),
    )
    .await;
    assert_eq!(e.field.as_deref(), Some("criteria[0].day"));
    // 0-24 is valid.
    h.adapter
        .validate(&call(
            CONN,
            "111",
            "manage_targeting",
            with_account(json!({ "campaign_id": "5", "action": "add", "criteria": [
                { "type": "ad_schedule", "day": "SUNDAY", "start_hour": 0, "end_hour": 24 }] })),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn targeting_input_validation() {
    let h = google_harness().await;
    fake(&h, targeting_seed()).await;
    for (c, field) in [
        (
            json!({ "type": "location", "geo_id": "1; DROP" }),
            "criteria[0].geo_id",
        ),
        (json!({ "type": "location" }), "criteria[0].geo_id"),
        (
            json!({ "type": "placement", "url": "not a url" }),
            "criteria[0].url",
        ),
        (
            json!({ "type": "placement", "url": "ftp://x.com" }),
            "criteria[0].url",
        ),
        (
            json!({ "type": "topic", "topic_id": "x" }),
            "criteria[0].topic_id",
        ),
        (json!({ "type": "audience" }), "criteria[0].user_list_id"),
        (
            json!({ "type": "device", "device": "TV", "bid_modifier": 1 }),
            "criteria[0].device",
        ),
        (
            json!({ "type": "device", "device": "MOBILE" }),
            "criteria[0].bid_modifier",
        ),
        (
            json!({ "type": "device", "device": "MOBILE", "bid_modifier": 10.5 }),
            "criteria[0].bid_modifier",
        ),
    ] {
        let e = validate_err(
            &h,
            "manage_targeting",
            json!({ "campaign_id": "5", "action": "add", "criteria": [c] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{c}");
        assert_eq!(e.field.as_deref(), Some(field), "{c}");
    }
    let e = validate_err(
        &h,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "add", "criteria": [] }),
    )
    .await;
    assert_eq!(e.field.as_deref(), Some("criteria"));
    let e = validate_err(
        &h,
        "manage_targeting",
        json!({ "campaign_id": "x'", "action": "add",
                "criteria": [{ "type": "language", "language_id": "1" }] }),
    )
    .await;
    assert_eq!(e.field.as_deref(), Some("campaign_id"));
}

#[tokio::test]
async fn targeting_remove_checks_kind_and_is_irreversible() {
    let h = google_harness().await;
    let g = fake(&h, targeting_seed()).await;
    let engine = engine(&h, WriteMode::Direct);

    // A type other than the requested one (ad group keyword 44, location 70 as a
    // language) and an unknown id: NOT_FOUND, no mutate.
    let before = mutate_bodies(&h).await.len();
    for input in [
        json!({ "campaign_id": "5", "action": "remove",
                "criteria": [{ "type": "language", "criterion_id": "70" }] }),
        json!({ "campaign_id": "5", "action": "remove",
                "criteria": [{ "type": "location", "criterion_id": "999" }] }),
        json!({ "campaign_id": "5", "ad_group_id": "3", "action": "remove",
                "criteria": [{ "type": "audience", "criterion_id": "44" }] }),
    ] {
        let e = validate_err(&h, "manage_targeting", input.clone()).await;
        assert_eq!(e.code, codes::NOT_FOUND, "{input}");
        assert_eq!(e.field.as_deref(), Some("criteria[0].criterion_id"));
    }
    let e = validate_err(
        &h,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "remove",
                "criteria": [{ "type": "location" }] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert_eq!(e.field.as_deref(), Some("criteria[0].criterion_id"));
    assert_eq!(mutate_bodies(&h).await.len(), before);

    let out = applied(
        &engine,
        "manage_targeting",
        json!({ "campaign_id": "5", "action": "remove",
                "criteria": [{ "type": "location", "criterion_id": "70" },
                             { "type": "language", "criterion_id": "72" }] }),
    )
    .await;
    assert_eq!(out["reversible"], false);
    assert_eq!(out["warning"], "This change cannot be undone.");
    assert_eq!(removes(&last_applied_ops(&h).await), vec![CC_LOC, CC_LANG]);
    assert!(g.state.lock().unwrap().get(CC_LOC).is_none());
    assert!(g.state.lock().unwrap().contains_key(CC_DEV));
}

#[tokio::test]
async fn targeting_tool_is_exposed_and_flagged() {
    let h = google_harness().await;
    let spec = h
        .adapter
        .tools()
        .into_iter()
        .find(|t| t.name == "manage_targeting")
        .expect("manage_targeting is exposed");
    assert!(spec.description.contains("geo_target_constant"));
    assert!(spec.description.contains("language_constant"));
    assert!(spec.reversible);
}

// ------------------------------------------------ recommendations and asset group

const REC: &str = "customers/111/recommendations/abc";
const REC_DISMISSED: &str = "customers/111/recommendations/old";
const AG: &str = "customers/111/assetGroups/20";
const AG_OTHER_ASSET: &str = "customers/111/assetGroupAssets/21~50~HEADLINE";
const SIGNAL: &str = "customers/111/assetGroupSignals/20~900";

fn aga(asset: u32, field_type: &str) -> (String, Value) {
    let rn = format!("customers/111/assetGroupAssets/20~{asset}~{field_type}");
    (
        rn.clone(),
        json!({ "resourceName": rn, "assetGroup": AG, "fieldType": field_type,
                "asset": format!("customers/111/assets/{asset}"), "status": "ENABLED" }),
    )
}

/// Asset group at the minimum: 3 headlines, 1 long headline, 2 descriptions, 2 images and 1 logo.
fn pmax_seed() -> HashMap<String, Value> {
    let mut s = seed("TARGET_SPEND", false);
    s.insert(
        REC.into(),
        json!({ "resourceName": REC, "type": "KEYWORD", "dismissed": false }),
    );
    s.insert(
        REC_DISMISSED.into(),
        json!({ "resourceName": REC_DISMISSED, "type": "KEYWORD", "dismissed": true }),
    );
    s.insert(
        AG.into(),
        json!({ "resourceName": AG, "id": "20", "name": "Group A", "status": "ENABLED",
                "finalUrls": ["https://a.com"] }),
    );
    for (asset, ft) in [
        (31, "HEADLINE"),
        (32, "HEADLINE"),
        (33, "HEADLINE"),
        (34, "LONG_HEADLINE"),
        (35, "DESCRIPTION"),
        (36, "DESCRIPTION"),
        (37, "MARKETING_IMAGE"),
        (38, "SQUARE_MARKETING_IMAGE"),
        (39, "LOGO"),
    ] {
        let (rn, v) = aga(asset, ft);
        s.insert(rn, v);
        s.insert(
            format!("customers/111/assets/{asset}"),
            json!({ "resourceName": format!("customers/111/assets/{asset}"),
                    "textAsset": { "text": format!("Text {asset}") } }),
        );
    }
    s.insert(
        AG_OTHER_ASSET.into(),
        json!({ "resourceName": AG_OTHER_ASSET, "fieldType": "HEADLINE", "status": "ENABLED" }),
    );
    s.insert(
        SIGNAL.into(),
        json!({ "resourceName": SIGNAL, "assetGroup": AG, "searchTheme": { "text": "shoes" } }),
    );
    s
}

async fn mock_recommendation_post(h: &GoogleHarness, suffix: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/v26/customers/111/recommendations:{suffix}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "resourceName": REC }] })))
        .mount(&h.server)
        .await;
}

async fn posts_to(h: &GoogleHarness, suffix: &str) -> Vec<Value> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == format!("/v26/customers/111/recommendations:{suffix}"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn apply_recommendation_is_irreversible() {
    let h = google_harness().await;
    fake(&h, pmax_seed()).await;
    mock_recommendation_post(&h, "apply").await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "apply_recommendation",
        json!({ "recommendation": "abc" }),
    )
    .await;
    assert_eq!(out["reversible"], false);
    let by_rn = applied(
        &engine,
        "apply_recommendation",
        json!({ "recommendation": REC,
                "parameters": { "campaignBudget": { "newBudgetAmountMicros": "9000000" } } }),
    )
    .await;
    assert_eq!(by_rn["reversible"], false);
    assert_eq!(
        posts_to(&h, "apply").await,
        vec![
            json!({ "operations": [{ "resourceName": REC }] }),
            json!({ "operations": [{ "resourceName": REC,
                "campaignBudget": { "newBudgetAmountMicros": "9000000" } }] }),
        ]
    );
    // No mutate call: the write is direct.
    assert!(mutate_bodies(&h).await.is_empty());
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert!(!change.reversible);
}

#[tokio::test]
async fn dismiss_recommendation_body() {
    let h = google_harness().await;
    fake(&h, pmax_seed()).await;
    mock_recommendation_post(&h, "dismiss").await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "dismiss_recommendation",
        json!({ "recommendation": REC }),
    )
    .await;
    assert_eq!(out["reversible"], false);
    assert_eq!(
        posts_to(&h, "dismiss").await,
        vec![json!({ "operations": [{ "resourceName": REC }] })]
    );
}

#[tokio::test]
async fn recommendation_must_exist_and_be_open() {
    let h = google_harness().await;
    fake(&h, pmax_seed()).await;
    mock_recommendation_post(&h, "apply").await;
    mock_recommendation_post(&h, "dismiss").await;
    for tool in ["apply_recommendation", "dismiss_recommendation"] {
        let e = validate_err(&h, tool, json!({ "recommendation": "missing" })).await;
        assert_eq!(e.code, codes::NOT_FOUND, "{tool}");
        let e = validate_err(&h, tool, json!({ "recommendation": "old" })).await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{tool}");
        assert!(e.message.contains("dismissed"), "{}", e.message);
        for bad in [
            "customers/999/recommendations/abc",
            "customers/111/campaigns/5",
            "a/b",
            "a'b",
            "",
        ] {
            let e = validate_err(&h, tool, json!({ "recommendation": bad })).await;
            assert_eq!(e.code, codes::INVALID_INPUT, "{tool} {bad}");
            assert_eq!(e.field.as_deref(), Some("recommendation"));
        }
    }
    for p in [
        json!([1]),
        json!("x"),
        json!({ "resourceName": "customers/111/recommendations/zzz" }),
    ] {
        let e = validate_err(
            &h,
            "apply_recommendation",
            json!({ "recommendation": "abc", "parameters": p }),
        )
        .await;
        assert_eq!(e.field.as_deref(), Some("parameters"), "{p}");
    }
    assert!(posts_to(&h, "apply").await.is_empty());
    assert!(posts_to(&h, "dismiss").await.is_empty());
}

#[tokio::test]
async fn asset_group_add_headline() {
    let h = google_harness().await;
    let g = fake(&h, pmax_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "update_asset_group",
        json!({ "asset_group_id": "20",
                "add_assets": [{ "field_type": "headline", "text": "  Free shipping " }] }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops,
        vec![
            json!({ "assetOperation": { "create": {
                "resourceName": "customers/111/assets/-1",
                "textAsset": { "text": "Free shipping" } } } }),
            // No `resourceName`: the link id is derived from the asset (as in PMax).
            json!({ "assetGroupAssetOperation": { "create": {
                "assetGroup": AG, "asset": "customers/111/assets/-1",
                "fieldType": "HEADLINE" } } }),
        ]
    );
    let real = "customers/111/assetGroupAssets/20~101~HEADLINE";
    assert!(g.state.lock().unwrap().contains_key(real));
    let change =
        h.db.get_change(out["change_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(
        change.after,
        Some(json!({ real: { "asset_group_asset.status": "ENABLED" } }))
    );
    // Undo removes the real link (the asset itself stays).
    undo(&engine, &out).await;
    assert_eq!(removes(&last_applied_ops(&h).await), vec![real]);
    assert!(!g.state.lock().unwrap().contains_key(real));
}

#[tokio::test]
async fn asset_group_minimums_enforced() {
    let h = google_harness().await;
    fake(&h, pmax_seed()).await;
    for (rn, min) in [
        (
            "customers/111/assetGroupAssets/20~31~HEADLINE",
            "minimum is 3",
        ),
        (
            "customers/111/assetGroupAssets/20~34~LONG_HEADLINE",
            "minimum is 1",
        ),
        (
            "customers/111/assetGroupAssets/20~35~DESCRIPTION",
            "minimum is 2",
        ),
        (
            "customers/111/assetGroupAssets/20~37~MARKETING_IMAGE",
            "1.91:1",
        ),
        (
            "customers/111/assetGroupAssets/20~38~SQUARE_MARKETING_IMAGE",
            "1:1",
        ),
    ] {
        let e = validate_err(
            &h,
            "update_asset_group",
            json!({ "asset_group_id": "20", "remove_assets": [rn] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{rn}");
        assert!(e.message.contains(min), "{rn}: {}", e.message);
    }
    // Swapping one headline for another in the same call keeps the minimum.
    let v = h
        .adapter
        .validate(&call(
            CONN,
            "111",
            "update_asset_group",
            with_account(json!({ "asset_group_id": "20",
                "remove_assets": ["customers/111/assetGroupAssets/20~31~HEADLINE"],
                "add_assets": [{ "field_type": "HEADLINE", "text": "New" }] })),
        ))
        .await
        .unwrap();
    assert!(v.warning.unwrap().contains("cannot be undone"));
    assert!(
        mutate_bodies(&h)
            .await
            .iter()
            .all(|b| b["validateOnly"] == json!(true))
    );
}

#[tokio::test]
async fn asset_group_maximums_enforced() {
    let h = google_harness().await;
    fake(&h, pmax_seed()).await;
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "DESCRIPTION", "text": "a" },
            { "field_type": "DESCRIPTION", "text": "b" },
            { "field_type": "DESCRIPTION", "text": "c" },
            { "field_type": "DESCRIPTION", "text": "d" } ] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert!(e.message.contains("maximum is 5"), "{}", e.message);
    assert_eq!(e.field.as_deref(), Some("add_assets"));
}

#[tokio::test]
async fn asset_group_remove_is_irreversible_and_scoped_to_the_group() {
    let h = google_harness().await;
    let g = fake(&h, pmax_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    // A link from another asset group and one that doesn't exist (or was removed): NOT_FOUND,
    // nothing is sent.
    for rn in [AG_OTHER_ASSET, "customers/111/assetGroupAssets/20~999~LOGO"] {
        let e = validate_err(
            &h,
            "update_asset_group",
            json!({ "asset_group_id": "20", "remove_assets": [rn] }),
        )
        .await;
        assert_eq!(e.code, codes::NOT_FOUND, "{rn}");
        assert_eq!(e.field.as_deref(), Some("remove_assets[0]"));
    }
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "20", "remove_signals": ["customers/111/assetGroupSignals/21~1"] }),
    )
    .await;
    assert_eq!(e.code, codes::NOT_FOUND);
    assert_eq!(e.field.as_deref(), Some("remove_signals[0]"));
    assert!(mutate_bodies(&h).await.is_empty());

    let logo = "customers/111/assetGroupAssets/20~39~LOGO";
    let out = applied(
        &engine,
        "update_asset_group",
        json!({ "asset_group_id": "20", "remove_assets": [logo], "remove_signals": [SIGNAL] }),
    )
    .await;
    assert_eq!(out["reversible"], false);
    assert_eq!(removes(&last_applied_ops(&h).await), vec![logo, SIGNAL]);
    assert!(!g.state.lock().unwrap().contains_key(logo));
    assert!(!g.state.lock().unwrap().contains_key(SIGNAL));
}

#[tokio::test]
async fn asset_group_search_theme_signal() {
    let h = google_harness().await;
    let g = fake(&h, pmax_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_signals": {
            "search_themes": ["running shoes"],
            "audiences": ["customers/111/audiences/77"] } }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops,
        vec![
            json!({ "assetGroupSignalOperation": { "create": {
                "assetGroup": AG,
                "audience": { "audience": "customers/111/audiences/77" } } } }),
            json!({ "assetGroupSignalOperation": { "create": {
                "assetGroup": AG,
                "searchTheme": { "text": "running shoes" } } } }),
        ]
    );
    let reals = [
        "customers/111/assetGroupSignals/20~101",
        "customers/111/assetGroupSignals/20~102",
    ];
    for r in reals {
        assert!(g.state.lock().unwrap().contains_key(r), "{r}");
    }
    undo(&engine, &out).await;
    assert_eq!(removes(&last_applied_ops(&h).await), reals);
}

#[tokio::test]
async fn asset_group_field_edit_and_undo() {
    let h = google_harness().await;
    let g = fake(&h, pmax_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let out = applied(
        &engine,
        "update_asset_group",
        json!({ "asset_group_id": "20", "name": "Group B", "status": "PAUSED",
                "final_urls": ["https://b.com"] }),
    )
    .await;
    assert_eq!(out["reversible"], true);
    let ops = last_applied_ops(&h).await;
    let update = &op(&ops, "assetGroupOperation")["update"];
    assert_eq!(
        *update,
        json!({ "resourceName": AG, "name": "Group B", "status": "PAUSED",
                "finalUrls": ["https://b.com"] })
    );
    assert_eq!(g.get(AG)["name"], "Group B");
    undo(&engine, &out).await;
    let ag = g.get(AG);
    assert_eq!(ag["name"], "Group A");
    assert_eq!(ag["status"], "ENABLED");
    assert_eq!(ag["finalUrls"], json!(["https://a.com"]));
}

#[tokio::test]
async fn asset_group_existing_asset_and_input_rules() {
    let h = google_harness().await;
    let g = fake(&h, pmax_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let img = "customers/111/assets/88";
    let out = applied(
        &engine,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "PORTRAIT_MARKETING_IMAGE", "image": { "asset": img } },
            { "field_type": "YOUTUBE_VIDEO", "youtube_video_id": "dQw4w9WgXcQ" } ] }),
    )
    .await;
    let ops = last_applied_ops(&h).await;
    assert_eq!(
        ops[0],
        json!({ "assetGroupAssetOperation": { "create": {
            "assetGroup": AG, "asset": img, "fieldType": "PORTRAIT_MARKETING_IMAGE" } } })
    );
    assert_eq!(
        ops[1],
        json!({ "assetOperation": { "create": {
            "resourceName": "customers/111/assets/-1",
            "youtubeVideoAsset": { "youtubeVideoId": "dQw4w9WgXcQ" } } } })
    );
    let real = "customers/111/assetGroupAssets/20~88~PORTRAIT_MARKETING_IMAGE";
    assert!(g.state.lock().unwrap().contains_key(real));
    undo(&engine, &out).await;
    assert!(!g.state.lock().unwrap().contains_key(real));

    for (item, field) in [
        (json!({ "field_type": "HEADLINE" }), "add_assets[0]"),
        (
            json!({ "field_type": "HEADLINE", "text": "a", "asset": img }),
            "add_assets[0]",
        ),
        (
            json!({ "field_type": "NOTHING", "text": "a" }),
            "add_assets[0].field_type",
        ),
        (
            json!({ "field_type": "HEADLINE", "text": "x".repeat(31) }),
            "add_assets[0].text",
        ),
        (
            json!({ "field_type": "LOGO", "text": "a" }),
            "add_assets[0].text",
        ),
        (
            json!({ "field_type": "HEADLINE", "image": { "asset": img } }),
            "add_assets[0].image",
        ),
        (
            json!({ "field_type": "LOGO", "asset": "customers/222/assets/1" }),
            "add_assets[0].asset",
        ),
        (
            json!({ "field_type": "YOUTUBE_VIDEO", "youtube_video_id": "https://youtu.be/x" }),
            "add_assets[0].youtube_video_id",
        ),
    ] {
        let e = validate_err(
            &h,
            "update_asset_group",
            json!({ "asset_group_id": "20", "add_assets": [item] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{item}");
        assert_eq!(e.field.as_deref(), Some(field), "{item}");
    }
    // A local host URL is refused before any request, on the image field.
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "LOGO", "image": { "url": "https://127.0.0.1/x.png" } }] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert_eq!(e.field.as_deref(), Some("add_assets[0].image"), "{e:?}");
    // Malformed ImageRef: the error names the field.
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "LOGO", "image": { "asset": "customers/111/assets/1", "url": "x" } }] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert_eq!(e.field.as_deref(), Some("add_assets[0].image"), "{e:?}");
    // Repeated link, edit + add together, and an empty call.
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "LOGO", "asset": "customers/111/assets/39" }] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT);
    assert_eq!(
        e.message, "This asset is already in the asset group as LOGO.",
        "{e:?}"
    );
    // The same asset twice in the call (and not yet in the asset group).
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "LOGO", "asset": "customers/111/assets/77" },
            { "field_type": "LOGO", "asset": "customers/111/assets/77" }] }),
    )
    .await;
    assert!(e.message.contains("appears more than once"), "{e:?}");
    for input in [
        json!({ "asset_group_id": "20", "name": "x",
                "add_assets": [{ "field_type": "HEADLINE", "text": "a" }] }),
        json!({ "asset_group_id": "20" }),
        json!({ "asset_group_id": "20", "name": "  " }),
    ] {
        let e = validate_err(&h, "update_asset_group", input.clone()).await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{input}");
    }
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "99", "name": "x" }),
    )
    .await;
    assert_eq!(e.code, codes::NOT_FOUND);
}

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

#[tokio::test]
async fn asset_group_accepts_file_image() {
    let h = google_harness().await;
    let g = fake(&h, pmax_seed()).await;
    let engine = engine(&h, WriteMode::Direct);
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, bytes: Vec<u8>| {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    };
    let landscape = file("landscape.png", png(1200, 628));
    let copy = file("copy.png", png(1200, 628));
    let square = file("square.png", png(1200, 1200));
    let out = applied(
        &engine,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "MARKETING_IMAGE", "image": { "file_path": landscape } },
            { "field_type": "SQUARE_MARKETING_IMAGE", "image": { "file_path": square } } ] }),
    )
    .await;
    assert_eq!(out["reversible"], true, "{out}");
    let summary = out["summary"].as_str().unwrap();
    assert!(summary.contains(&landscape), "{summary}");
    let ops = last_applied_ops(&h).await;
    let keys: Vec<&str> = ops
        .iter()
        .map(|o| o.as_object().unwrap().keys().next().unwrap().as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "assetOperation",
            "assetGroupAssetOperation",
            "assetOperation",
            "assetGroupAssetOperation"
        ]
    );
    let image = &ops[0]["assetOperation"]["create"];
    assert_eq!(image["resourceName"], "customers/111/assets/-1");
    assert_eq!(image["type"], "IMAGE");
    assert!(
        image["name"].as_str().unwrap().starts_with("img-"),
        "{image}"
    );
    assert_eq!(
        ops[1],
        json!({ "assetGroupAssetOperation": { "create": {
            "assetGroup": AG, "asset": "customers/111/assets/-1",
            "fieldType": "MARKETING_IMAGE" } } })
    );
    let real = "customers/111/assetGroupAssets/20~101~MARKETING_IMAGE";
    assert!(g.state.lock().unwrap().contains_key(real));
    undo(&engine, &out).await;
    assert!(!g.state.lock().unwrap().contains_key(real));

    // The same bytes twice in the same type: refused.
    let e = validate_err(
        &h,
        "update_asset_group",
        json!({ "asset_group_id": "20", "add_assets": [
            { "field_type": "MARKETING_IMAGE", "image": { "file_path": landscape } },
            { "field_type": "MARKETING_IMAGE", "image": { "file_path": copy } } ] }),
    )
    .await;
    assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
    // Wrong aspect ratio for the type, and a missing file: on the image field.
    for image in [
        json!({ "file_path": square }),
        json!({ "file_path": "/does/not/exist.png" }),
    ] {
        let e = validate_err(
            &h,
            "update_asset_group",
            json!({ "asset_group_id": "20", "add_assets": [
                { "field_type": "MARKETING_IMAGE", "image": image }] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{e:?}");
        assert_eq!(e.field.as_deref(), Some("add_assets[0].image"), "{e:?}");
    }
}

#[tokio::test]
async fn asset_group_accepts_youtube_links() {
    let h = google_harness().await;
    fake(&h, pmax_seed()).await;
    for link in [
        "https://youtu.be/dQw4w9WgXcQ",
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10s",
        "youtube.com/shorts/dQw4w9WgXcQ",
    ] {
        h.adapter
            .validate(&call(
                CONN,
                "111",
                "update_asset_group",
                with_account(json!({ "asset_group_id": "20", "add_assets": [
                    { "field_type": "YOUTUBE_VIDEO", "youtube_video_id": link }] })),
            ))
            .await
            .unwrap_or_else(|e| panic!("{link}: {e:?}"));
        let ops = mutate_bodies(&h).await.last().unwrap()["mutateOperations"].clone();
        assert_eq!(
            ops[0]["assetOperation"]["create"]["youtubeVideoAsset"],
            json!({ "youtubeVideoId": "dQw4w9WgXcQ" }),
            "{link}"
        );
    }
    for bad in [
        "https://youtu.be/short",
        "https://www.youtube.com/watch?x=1",
        "https://vimeo.com/123",
    ] {
        let e = validate_err(
            &h,
            "update_asset_group",
            json!({ "asset_group_id": "20", "add_assets": [
                { "field_type": "YOUTUBE_VIDEO", "youtube_video_id": bad }] }),
        )
        .await;
        assert_eq!(e.code, codes::INVALID_INPUT, "{bad}");
        assert_eq!(
            e.field.as_deref(),
            Some("add_assets[0].youtube_video_id"),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn recommendation_and_asset_group_tools_are_exposed() {
    let h = google_harness().await;
    let tools = h.adapter.tools();
    for (name, reversible) in [
        ("apply_recommendation", false),
        ("dismiss_recommendation", false),
        ("update_asset_group", true),
    ] {
        let spec = tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name}"));
        assert_eq!(spec.reversible, reversible, "{name}");
    }
}
