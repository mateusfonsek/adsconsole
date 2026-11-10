//! Checklist of Meta Ads on a real account: runs the tools through the Engine (direct mode),
//! undoes each write and prints an `OK | FAIL | SKIPPED` report with the evidence for
//! the `VERIFY(v26.0)` points in the code.
//!
//! `cargo run -p adsconsole-mcp --example meta_checklist -- <act_id> [--page <page_id>]
//!     [--image <file>] [--video <file>] [--only <steps>] [--yes]`
//!
//! - Steps: reads, image, validation, new_creative, creation, rollback, budget,
//!   duplicate, empty_field, video. Without `--only`, all run except `new_creative` and
//!   `video`.
//! - The ads' Page is only the one from `--page`; without it, the steps that need a Page
//!   are skipped.
//! - The ads use a post that already exists on the Page (`existing_post`), because an app
//!   in development mode cannot advertise posts it created; `new_creative` (only with
//!   `--only` and `--image`) records whether a new creative works in the app's current
//!   mode, and if it does, creates an unpublished post on the Page.
//! - `--no-ads`: for an account with no payment method (Meta rejects the ad), the edit,
//!   budget and duplication steps run on a paused campaign + ad set created through
//!   `graph_post` and deleted at the end of the step.
//! - Before any real write, prints the plan and asks for `yes` (or `--yes`).
//! - Every real write is undone right after, including the image; Ctrl+C undoes what has
//!   already been created. The video stays in the account (Meta does not allow deleting
//!   it through the API).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::Db;
use adsconsole_core::paths::{EnvOverrides, data_dir};
use adsconsole_core::routing::normalize_account_id;
use adsconsole_core::secrets::open_secret_store;
use adsconsole_mcp::ToolError;
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::codes;
use adsconsole_mcp::meta::MetaAdapter;
use adsconsole_meta_ads::endpoints::MetaEndpoints;
use chrono::Utc;
use serde_json::{Value, json};

const USAGE: &str = "usage: meta_checklist <act_id> [--page <page_id>] [--link <url>] [--no-ads] [--image <file>] [--video <file>] \
                     [--only <step,...>] [--yes] [--undo <change_id>]... [--list-media] [--delete-image <hash>]... \
                     [--delete-video <id>]... [--delete-creative <id>]...";
const STEPS: [&str; 10] = [
    "reads",
    "image",
    "validation",
    "new_creative",
    "creation",
    "rollback",
    "budget",
    "duplicate",
    "empty_field",
    "video",
];
const WRITE_STEPS: [&str; 8] = [
    "image",
    "new_creative",
    "creation",
    "rollback",
    "budget",
    "duplicate",
    "empty_field",
    "video",
];
/// Steps that only run when requested with `--only`: they leave traces outside undo control.
const OPT_IN_STEPS: [&str; 2] = ["new_creative", "video"];
const BY: &str = "meta_checklist";

/// Points marked `VERIFY(v26.0)` in the code and what the checklist observes for each.
const VERIFY: [(&str, &str); 10] = [
    (
        "V1",
        "browser login with a random port on localhost (app in development mode)",
    ),
    (
        "V2",
        "targeting_automation.advantage_audience required on the ad set",
    ),
    (
        "V3",
        "is_adset_budget_sharing_enabled without a campaign budget",
    ),
    ("V4", "image creative with link_data and call_to_action"),
    ("V5", "ad with creative {creative_id}"),
    (
        "V6",
        "undoing a creation deletes the campaign and its children",
    ),
    ("V7", "limit of 4 budget changes per hour on the ad set"),
    (
        "V8",
        "synchronous copy with /copies (deep_copy, status_option, rename_options)",
    ),
    (
        "V9",
        "new creative (object_story_spec) with the app in its current mode",
    ),
    ("V10", "creative from an existing post (object_story_id)"),
];

#[derive(Debug, PartialEq)]
struct Args {
    account: String,
    /// The ads' Page (never chosen automatically).
    page: Option<String>,
    /// Destination of the new creative (default https://example.com/).
    link: String,
    /// Edit, budget and duplication on a campaign without ads (account with no payment method).
    no_ads: bool,
    image: Option<PathBuf>,
    video: Option<PathBuf>,
    only: Option<Vec<String>>,
    yes: bool,
    /// Only undoes these changes (with force) and exits: cleanup of leftovers.
    undo: Vec<String>,
    /// Only lists the account's videos and images and exits.
    list_media: bool,
    /// Only says, for each account in the connections, whether there is a payment method (no details).
    funding: bool,
    /// Deletes these test images (hash) and videos (id) and exits.
    delete_images: Vec<String>,
    delete_videos: Vec<String>,
    /// Deletes these test creatives (id) and exits.
    delete_creatives: Vec<String>,
}

impl Args {
    fn runs(&self, step: &str) -> bool {
        match &self.only {
            Some(o) => o.iter().any(|s| s == step),
            None => !OPT_IN_STEPS.contains(&step),
        }
    }
}

fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut it = raw.into_iter();
    let account = it.next().filter(|a| !a.starts_with("--")).ok_or(USAGE)?;
    let mut args = Args {
        account: normalize_account_id(Platform::MetaAds, &account),
        page: None,
        link: "https://example.com/".into(),
        no_ads: false,
        image: None,
        video: None,
        only: None,
        yes: false,
        undo: Vec::new(),
        list_media: false,
        funding: false,
        delete_images: Vec::new(),
        delete_videos: Vec::new(),
        delete_creatives: Vec::new(),
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--page" => args.page = Some(it.next().ok_or(USAGE)?),
            "--no-ads" => args.no_ads = true,
            "--link" => args.link = it.next().ok_or(USAGE)?,
            "--image" => args.image = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--video" => args.video = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--yes" => args.yes = true,
            "--undo" => args.undo.push(it.next().ok_or(USAGE)?),
            "--list-media" => args.list_media = true,
            "--funding" => args.funding = true,
            "--delete-image" => args.delete_images.push(it.next().ok_or(USAGE)?),
            "--delete-video" => args.delete_videos.push(it.next().ok_or(USAGE)?),
            "--delete-creative" => args.delete_creatives.push(it.next().ok_or(USAGE)?),
            "--only" => {
                let list: Vec<String> = it
                    .next()
                    .ok_or(USAGE)?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if let Some(bad) = list.iter().find(|s| !STEPS.contains(&s.as_str())) {
                    return Err(format!("unknown step: {bad} (use {})", STEPS.join(", ")));
                }
                args.only = Some(list);
            }
            other => return Err(format!("unknown option: {other}\n{USAGE}")),
        }
    }
    Ok(args)
}

/// Proceeds with no planned writes or with `--yes`. Otherwise only on `yes` in the input.
fn confirmed(planned: &[&str], yes: bool, input: &mut dyn BufRead) -> bool {
    if planned.is_empty() || yes {
        return true;
    }
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        return false;
    }
    line.trim().eq_ignore_ascii_case("yes")
}

// ---------------------------------------------------------------- generated image

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Valid grayscale PNG, uncompressed, with a checkerboard that depends on `seed`.
fn png(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize + 1) * h as usize);
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            let cell = u64::from(x / 40 + y / 40);
            raw.push((30 + (cell * 37 + seed) % 200) as u8);
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(block) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &z);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

// ---------------------------------------------------------------- checklist

struct Line {
    outcome: &'static str,
    step: String,
    tool: String,
    code: String,
    detail: String,
}

struct Checklist {
    engine: Engine,
    account: String,
    stamp: String,
    lines: Vec<Line>,
    evidence: BTreeMap<&'static str, Vec<String>>,
    /// Changes to undo, in the order they were made: (change_id, label).
    undo: Vec<(String, String)>,
    left_behind: Vec<String>,
    irreversible: Vec<String>,
    page_id: Option<String>,
    image_hash: Option<String>,
    /// Page post for the ads (`existing_post`).
    post_id: Option<String>,
    no_ads: bool,
    link: String,
    /// Account creatives before the run: the cleanup deletes the ones that appeared afterwards.
    creatives_before: Option<Vec<String>>,
    /// Undo of the image upload: runs after deleting the creatives that use it.
    image_undo: Option<(String, String)>,
}

/// Entry on the undo stack that is not a change: a helper campaign to delete.
const FIXTURE: &str = "fixture:";

fn short(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s
    }
}

fn err_json(e: &ToolError) -> String {
    let s = serde_json::to_string(e).unwrap_or_default();
    if s.chars().count() > 600 {
        format!("{}…", s.chars().take(600).collect::<String>())
    } else {
        s
    }
}

/// "Name (id)" of each Page in a `get_pages` result.
fn pages_list(v: &Value) -> String {
    v["pages"]
        .as_array()
        .map(|l| {
            l.iter()
                .map(|x| {
                    format!(
                        "{} ({})",
                        x["name"].as_str().unwrap_or("?"),
                        x["page_id"].as_str().unwrap_or("?")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn len(v: &Value, key: &str) -> usize {
    v[key].as_array().map_or(0, Vec::len)
}

impl Checklist {
    fn line(
        &mut self,
        outcome: &'static str,
        step: &str,
        tool: &str,
        code: &str,
        detail: impl Into<String>,
    ) {
        let detail = short(&detail.into());
        println!("{outcome} {step} {tool} {code} {detail}");
        self.lines.push(Line {
            outcome,
            step: step.into(),
            tool: tool.into(),
            code: code.into(),
            detail,
        });
    }

    fn ok(&mut self, step: &str, tool: &str, detail: impl Into<String>) {
        self.line("OK", step, tool, "-", detail);
    }

    fn fail(&mut self, step: &str, tool: &str, e: &ToolError) {
        let detail = e.message.clone();
        self.line("FAIL", step, tool, &e.code, detail);
        self.note("E1", format!("{step} {tool}: {}", err_json(e)));
    }

    fn skip(&mut self, step: &str, tool: &str, why: &str) {
        self.line("SKIPPED", step, tool, "-", why);
    }

    fn note(&mut self, key: &'static str, text: impl Into<String>) {
        self.evidence.entry(key).or_default().push(text.into());
    }

    async fn call(&self, tool: &str, mut input: Value) -> Result<Value, ToolError> {
        if tool != "list_ad_accounts" && tool != "undo_change" {
            input["account_id"] = json!(self.account);
        }
        // On the Limited tier (60 points every 5 min), the whole checklist goes over the limit:
        // wait the time the error indicates and retry (up to 3 times).
        for _ in 0..3 {
            match self
                .engine
                .call_tool(tool, input.clone(), Some(BY.into()))
                .await
            {
                Err(e) if e.code == codes::RATE_LIMITED => {
                    let secs = e
                        .details
                        .as_ref()
                        .and_then(|d| d["retry_after_secs"].as_u64())
                        .unwrap_or(60)
                        .min(400);
                    println!("  Meta call limit: waiting {} s for {tool}...", secs + 2);
                    tokio::time::sleep(std::time::Duration::from_secs(secs + 2)).await;
                }
                other => return other,
            }
        }
        self.engine.call_tool(tool, input, Some(BY.into())).await
    }

    /// Write: on OK, goes onto the undo stack (if reversible).
    async fn write(&mut self, step: &str, tool: &str, label: &str, input: Value) -> Option<Value> {
        match self.call(tool, input).await {
            Ok(out) => {
                let id = out["change_id"].as_str().unwrap_or_default().to_string();
                if out["reversible"] == true {
                    self.undo.push((id.clone(), label.to_string()));
                }
                self.ok(step, tool, format!("{label}: {id}"));
                Some(out)
            }
            Err(e) => {
                if matches!(
                    e.code.as_str(),
                    codes::EXECUTION_FAILED | codes::PARTIAL_ROLLBACK
                ) {
                    self.left_behind
                        .push(format!("{label}: {} ({})", e.message, err_json(&e)));
                }
                self.fail(step, tool, &e);
                None
            }
        }
    }

    /// Undoes the last change on the stack.
    async fn undo_last(&mut self, step: &str) {
        let Some((id, label)) = self.undo.pop() else {
            return;
        };
        if let Some(obj) = id.strip_prefix(FIXTURE) {
            self.delete_fixture(step, obj, &label).await;
            return;
        }
        match self.call("undo_change", json!({ "change_id": id })).await {
            Ok(out) => self.ok(
                step,
                "undo_change",
                format!(
                    "{label} undone ({})",
                    out["change_id"].as_str().unwrap_or("?")
                ),
            ),
            Err(e) => {
                self.left_behind
                    .push(format!("{label} ({id}): undo failed: {}", e.message));
                self.fail(step, "undo_change", &e);
            }
        }
    }

    /// Undoes everything until the stack has `keep` items (with force, at the end or on Ctrl+C).
    async fn unwind_to(&mut self, keep: usize) {
        while self.undo.len() > keep {
            let (id, label) = self.undo.pop().unwrap_or_default();
            if let Some(obj) = id.strip_prefix(FIXTURE) {
                self.delete_fixture("cleanup", obj, &label).await;
                continue;
            }
            match self
                .call("undo_change", json!({ "change_id": id, "force": true }))
                .await
            {
                Ok(_) => println!("  undone: {label}"),
                Err(e) => self
                    .left_behind
                    .push(format!("{label} ({id}): {}", e.message)),
            }
        }
    }

    /// Deletes the helper campaign (its ad sets go with it).
    async fn delete_fixture(&mut self, step: &str, id: &str, label: &str) {
        let input = json!({ "path": id, "params": { "method": "delete" } });
        match self.call("graph_post", input).await {
            Ok(_) => self.ok(step, "graph_post", format!("{label} deleted ({id})")),
            Err(e) => {
                self.left_behind
                    .push(format!("{label} ({id}): delete failed: {}", e.message));
                self.fail(step, "graph_post", &e);
            }
        }
    }

    /// Paused campaign + ad set, without an ad, created directly on the Graph API: Meta only
    /// asks for a payment method on the ad. Goes on the stack as `fixture:` to be deleted.
    async fn fixture_campaign(&mut self, step: &str, what: &str) -> Option<(String, String)> {
        let campaign = json!({
            "name": self.name(what),
            "objective": "OUTCOME_ENGAGEMENT",
            "status": "PAUSED",
            "special_ad_categories": "[]",
            "is_adset_budget_sharing_enabled": "false",
        });
        let input = json!({ "path": format!("{}/campaigns", self.account), "params": campaign });
        let id = match self.call("graph_post", input).await {
            Ok(out) => out["result"]["id"].as_str()?.to_string(),
            Err(e) => {
                self.fail(step, "graph_post", &e);
                return None;
            }
        };
        self.undo
            .push((format!("{FIXTURE}{id}"), format!("helper campaign {what}")));
        self.ok(
            step,
            "graph_post",
            format!("helper campaign without an ad: {id}"),
        );
        let targeting = json!({
            "geo_locations": { "countries": ["BR"] },
            "age_min": 18,
            "targeting_automation": { "advantage_audience": 0 },
        });
        let adset = json!({
            "name": self.name(&format!("{what} ad set")),
            "campaign_id": id,
            "daily_budget": "2000",
            "billing_event": "IMPRESSIONS",
            "optimization_goal": "POST_ENGAGEMENT",
            "bid_strategy": "LOWEST_COST_WITHOUT_CAP",
            "targeting": targeting.to_string(),
            "status": "PAUSED",
        });
        let input = json!({ "path": format!("{}/adsets", self.account), "params": adset });
        match self.call("graph_post", input).await {
            Ok(out) => {
                let adset = out["result"]["id"].as_str()?.to_string();
                self.ok(step, "graph_post", format!("helper ad set: {adset}"));
                Some((id, adset))
            }
            Err(e) => {
                self.fail(step, "graph_post", &e);
                None
            }
        }
    }

    /// Ids of the account's creatives (up to 500), or `None` if listing failed.
    async fn creative_ids(&mut self) -> Option<Vec<String>> {
        let path = format!("{}/adcreatives", self.account);
        let input = json!({ "path": path, "params": { "fields": "id", "limit": "500" } });
        let v = self.call("graph_get", input).await.ok()?;
        Some(
            v["data"]
                .as_array()?
                .iter()
                .filter_map(|c| c["id"].as_str().map(str::to_string))
                .collect(),
        )
    }

    /// Undoing a creation leaves the creatives in the account; the checklist deletes the
    /// ones from this run: those that did not exist before it (the creative of an existing
    /// post carries the post text as its name) or, without that list, those with the run
    /// stamp in the name.
    async fn sweep_creatives(&mut self) {
        let path = format!("{}/adcreatives", self.account);
        let input = json!({ "path": path, "params": { "fields": "id,name", "limit": "500" } });
        let list = match self.call("graph_get", input).await {
            Ok(v) => v["data"].as_array().cloned().unwrap_or_default(),
            Err(e) => {
                self.left_behind
                    .push(format!("creatives: could not list: {}", e.message));
                return;
            }
        };
        for c in list {
            let (Some(id), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else {
                continue;
            };
            let new = match &self.creatives_before {
                Some(before) => !before.iter().any(|b| b == id),
                None => name.contains(&self.stamp),
            };
            if !new {
                continue;
            }
            let input = json!({ "path": id, "params": { "method": "delete" } });
            match self.call("graph_post", input).await {
                Ok(_) => println!("  deleted: creative {id}"),
                Err(e) => self
                    .left_behind
                    .push(format!("creative {id} ({name}): {}", e.message)),
            }
        }
    }

    fn name(&self, what: &str) -> String {
        format!("AdsConsole checklist {} {what}", self.stamp)
    }

    fn creative(&self, hash: &str) -> Value {
        json!({
            "type": "image",
            "page_id": self.page_id,
            "link": self.link,
            "message": "AdsConsole test (paused, deleted right after).",
            "headline": "AdsConsole test",
            "image_hash": hash,
            "call_to_action": "LEARN_MORE",
        })
    }

    /// Creative of the test ads: the existing post, if any; otherwise a new creative.
    fn ad_creative(&self) -> Option<Value> {
        match (&self.post_id, &self.image_hash, &self.page_id) {
            (Some(post), _, _) => Some(json!({ "type": "existing_post", "object_story_id": post })),
            (None, Some(hash), Some(_)) => Some(self.creative(hash)),
            _ => None,
        }
    }

    /// Campaign with 1 ad set. With an existing post, engagement (the post has no external
    /// link to optimize clicks for); with a new creative, traffic to the website.
    fn campaign_input(&self, what: &str, ads: Vec<Value>) -> Value {
        let post = ads
            .first()
            .is_some_and(|a| a["creative"]["type"] == "existing_post");
        let mut adset = json!({
            "name": self.name(&format!("{what} ad set")),
            "daily_budget": 20,
            "billing_event": "IMPRESSIONS",
            "targeting": { "geo_locations": { "countries": ["BR"] }, "age_min": 18 },
            "ads": ads,
        });
        let objective = if post {
            adset["optimization_goal"] = json!("POST_ENGAGEMENT");
            adset["promoted_object"] = json!({ "page_id": self.page_id });
            adset["destination_type"] = json!("ON_POST");
            "OUTCOME_ENGAGEMENT"
        } else {
            adset["optimization_goal"] = json!("LINK_CLICKS");
            adset["destination_type"] = json!("WEBSITE");
            "OUTCOME_TRAFFIC"
        };
        json!({
            "campaign": { "name": self.name(what), "objective": objective },
            "adsets": [adset],
        })
    }

    /// Id of the object in the `result` of a creation whose name is `name`.
    fn id_named(result: &Value, name: &str) -> Option<String> {
        result
            .as_object()?
            .iter()
            .find(|(_, v)| v["name"] == name)
            .map(|(k, _)| k.clone())
    }

    fn keep_creative_note(&mut self, what: &str) {
        if self.post_id.is_none() {
            self.irreversible
                .push(format!("creative of the test ad ({what})"));
        }
    }

    /// Simple campaign (1 ad set, 1 ad): returns (campaign, ad set).
    async fn simple_campaign(&mut self, step: &str, what: &str) -> Option<(String, String)> {
        if self.no_ads {
            return self.fixture_campaign(step, what).await;
        }
        let creative = self.ad_creative()?;
        let ad = json!({ "name": self.name(&format!("{what} ad")), "creative": creative });
        let input = self.campaign_input(what, vec![ad]);
        let out = self
            .write(
                step,
                "create_campaign",
                &format!("create_campaign {what}"),
                input,
            )
            .await?;
        self.keep_creative_note(what);
        let campaign = Self::id_named(&out["result"], &self.name(what))?;
        let adset = Self::id_named(&out["result"], &self.name(&format!("{what} ad set")))?;
        Some((campaign, adset))
    }

    async fn reads(&mut self) {
        let s = "reads";
        match self.call("list_ad_accounts", json!({})).await {
            Ok(v) => {
                let found = v["accounts"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x["account_id"] == self.account));
                if found {
                    self.ok(
                        s,
                        "list_ad_accounts",
                        format!("{} accounts", len(&v, "accounts")),
                    );
                } else {
                    self.line(
                        "FAIL",
                        s,
                        "list_ad_accounts",
                        "-",
                        "the account is not in the cache",
                    );
                }
            }
            Err(e) => self.fail(s, "list_ad_accounts", &e),
        }
        for (tool, input) in [
            ("get_campaigns", json!({})),
            ("get_insights", json!({ "level": "campaign" })),
            (
                "search_targeting",
                json!({ "type": "interest", "query": "soccer" }),
            ),
            ("search_targeting", json!({ "type": "behavior" })),
            ("search_targeting", json!({ "type": "demographic" })),
            (
                "search_targeting",
                json!({ "type": "geo", "query": "London", "geo_types": ["city"] }),
            ),
            (
                "estimate_audience",
                json!({ "targeting": { "geo_locations": { "countries": ["BR"] }, "age_min": 18 } }),
            ),
        ] {
            match self.call(tool, input).await {
                Ok(v) => {
                    let n = ["campaigns", "rows", "results"]
                        .iter()
                        .find_map(|k| v[*k].as_array().map(Vec::len));
                    let detail = n.map_or_else(|| short(&v.to_string()), |n| format!("{n} items"));
                    self.ok(s, tool, detail);
                }
                Err(e) => self.fail(s, tool, &e),
            }
        }
        match self.call("get_pages", json!({})).await {
            Ok(v) => {
                let n = len(&v, "pages");
                match self.page_id.clone() {
                    Some(p) => {
                        let found = v["pages"].as_array().and_then(|l| {
                            l.iter().find(|x| x["page_id"].as_str() == Some(p.as_str()))
                        });
                        match found {
                            Some(x) if x["can_advertise"] == true => {
                                self.ok(s, "get_pages", format!("{n} pages; using --page {p}"));
                                self.page_posts(&p).await;
                            }
                            Some(_) => {
                                self.line(
                                    "FAIL",
                                    s,
                                    "get_pages",
                                    "-",
                                    format!("Page {p} cannot advertise"),
                                );
                                self.page_id = None;
                            }
                            None => {
                                self.line(
                                    "FAIL",
                                    s,
                                    "get_pages",
                                    "-",
                                    format!(
                                        "Page {p} is not among the {n} accessible ones: {}",
                                        pages_list(&v)
                                    ),
                                );
                                self.page_id = None;
                            }
                        }
                    }
                    None => self.ok(
                        s,
                        "get_pages",
                        format!(
                            "{n} pages; without --page, the steps that need a Page are skipped"
                        ),
                    ),
                }
            }
            Err(e) => self.fail(s, "get_pages", &e),
        }
        match self
            .call(
                "search_ad_library",
                json!({ "query": "sneakers", "countries": ["BR"] }),
            )
            .await
        {
            Ok(v) => self.ok(s, "search_ad_library", format!("{} ads", len(&v, "ads"))),
            Err(e) if e.code == codes::ACCESS_DENIED => self.skip(
                s,
                "search_ad_library",
                "the Library requires identity confirmation",
            ),
            Err(e) => self.fail(s, "search_ad_library", &e),
        }
    }

    async fn page_posts(&mut self, page: &str) {
        let s = "reads";
        match self
            .call("get_page_posts", json!({ "page_id": page }))
            .await
        {
            Ok(v) => {
                let posts = v["posts"].as_array().cloned().unwrap_or_default();
                self.post_id = posts
                    .iter()
                    .find(|x| x["can_promote"] != false)
                    .and_then(|x| x["object_story_id"].as_str().map(str::to_string));
                let which = if self.post_id.is_some() {
                    "using one in the ads"
                } else {
                    "none promotable"
                };
                self.ok(
                    s,
                    "get_page_posts",
                    format!("{} posts; {which}", posts.len()),
                );
            }
            Err(e) => self.fail(s, "get_page_posts", &e),
        }
    }

    async fn validation(&mut self, adapter: &MetaAdapter, connection: &str) {
        let s = "validation";
        let Some(creative) = self.ad_creative() else {
            self.skip(s, "create_campaign", "no post or image for the creative");
            return;
        };
        let ad = json!({ "name": self.name("validation ad"), "creative": creative });
        let mut input = self.campaign_input("validation", vec![ad]);
        input["account_id"] = json!(self.account);
        let call = adsconsole_mcp::Call {
            connection_id: connection.to_string(),
            account_id: self.account.clone(),
            tool: "create_campaign".into(),
            input,
        };
        use adsconsole_mcp::PlatformAdapter;
        match adapter.validate(&call).await {
            Ok(v) => {
                self.ok(s, "create_campaign", short(&v.summary));
                self.note("V3", "validate_only of the campaign without a campaign budget accepted is_adset_budget_sharing_enabled=false");
            }
            Err(e) => {
                self.note(
                    "V3",
                    format!("validate_only of the campaign: {}", err_json(&e)),
                );
                self.fail(s, "create_campaign", &e);
            }
        }
    }

    async fn image(&mut self, file: Option<&PathBuf>, dir: &std::path::Path) {
        let s = "image";
        let path = match file {
            Some(p) => p.clone(),
            None => {
                let p = dir.join("checklist.png");
                let seed = Utc::now().timestamp_micros() as u64;
                if let Err(e) = std::fs::write(&p, png(1200, 628, seed)) {
                    self.line(
                        "FAIL",
                        s,
                        "upload_image",
                        "-",
                        format!("could not generate the image: {e}"),
                    );
                    return;
                }
                p
            }
        };
        if let Some(out) = self
            .write(
                s,
                "upload_image",
                "upload_image",
                json!({ "file_path": path.to_string_lossy() }),
            )
            .await
        {
            self.image_hash = out["result"]["image_hash"].as_str().map(str::to_string);
            // The image is only deleted after the creatives that use it (Meta refuses before).
            if out["reversible"] == true {
                self.image_undo = self.undo.pop();
            }
        }
    }

    /// Tries a campaign with a new creative (image + text), to record whether the app's
    /// mode allows it. Undoes it right after if it works.
    async fn new_creative(&mut self) {
        let s = "new_creative";
        let (Some(_), Some(hash)) = (self.page_id.clone(), self.image_hash.clone()) else {
            self.skip(s, "create_campaign", "no Page or image");
            return;
        };
        let ad = json!({ "name": self.name("new creative ad"), "creative": self.creative(&hash) });
        let base = self.undo.len();
        match self
            .call(
                "create_campaign",
                self.campaign_input("new creative", vec![ad]),
            )
            .await
        {
            Ok(out) => {
                self.undo.push((
                    out["change_id"].as_str().unwrap_or_default().into(),
                    "create_campaign new creative".into(),
                ));
                self.ok(s, "create_campaign", "new creative accepted");
                self.note(
                    "V9",
                    "new creative (object_story_spec) accepted in the app's current mode",
                );
                self.note("V4", "image creative with link_data and LEARN_MORE created");
                self.irreversible.push("new test creative".into());
                while self.undo.len() > base {
                    self.undo_last(s).await;
                }
            }
            Err(e) => {
                let lower = e.message.to_lowercase();
                self.note("V9", format!("new creative rejected: {}", err_json(&e)));
                if lower.contains("development") {
                    self.skip(
                        s,
                        "create_campaign",
                        "Meta rejects a new creative with the app in development mode",
                    );
                } else {
                    self.fail(s, "create_campaign", &e);
                }
            }
        }
    }

    async fn creation(&mut self) {
        let s = "creation";
        let base = self.undo.len();
        let Some((campaign, adset)) = self.simple_campaign(s, "creation").await else {
            if self.ad_creative().is_none() {
                self.skip(
                    s,
                    "create_campaign",
                    "no post or image (run reads and image)",
                );
            }
            self.unwind_to(base).await;
            return;
        };
        if self.no_ads {
            self.note(
                "V2",
                "helper ad set (graph_post) created with targeting_automation.advantage_audience=0",
            );
        } else {
            self.note(
                "V2",
                "ad set created with targeting_automation.advantage_audience=0",
            );
            self.note("V5", "ad created with creative {creative_id}");
        }
        if self.post_id.is_some() && !self.no_ads {
            self.note(
                "V10",
                "campaign created with a creative from an existing post (object_story_id)",
            );
        }
        let (ad, creative) = match self
            .call("get_campaign_details", json!({ "campaign_id": campaign }))
            .await
        {
            Ok(d) => {
                self.ok(
                    s,
                    "get_campaign_details",
                    format!("{} ad sets, {} ads", len(&d, "adsets"), len(&d, "ads")),
                );
                (
                    d["ads"][0]["ad_id"].as_str().map(str::to_string),
                    d["ads"][0]["creative"]["id"].as_str().map(str::to_string),
                )
            }
            Err(e) => {
                self.fail(s, "get_campaign_details", &e);
                (None, None)
            }
        };
        let input = json!({ "campaign_id": campaign, "name": self.name("creation edited") });
        self.write(s, "update_campaign", "update_campaign name", input)
            .await;
        let input = json!({
            "adset_id": adset, "name": self.name("ad set edited"), "daily_budget": 25,
            "targeting": { "geo_locations": { "countries": ["BR"] }, "age_min": 21 },
        });
        self.write(
            s,
            "update_adset",
            "update_adset name+budget+audience",
            input,
        )
        .await;
        if let Some(ad) = &ad {
            let input = json!({ "ad_id": ad, "name": self.name("ad edited") });
            self.write(s, "update_ad", "update_ad name", input).await;
        }
        if let Some(creative) = &creative {
            let input = json!({ "adset_id": adset, "name": self.name("extra ad"), "creative_id": creative });
            self.write(s, "create_ad", "create_ad with existing creative", input)
                .await;
        }
        // Conflict: two edits of the name; undoing the first must give UNDO_CONFLICT.
        let input = json!({ "campaign_id": campaign, "name": self.name("A") });
        let a = self
            .write(s, "update_campaign", "update_campaign conflict A", input)
            .await;
        let input = json!({ "campaign_id": campaign, "name": self.name("B") });
        let b = self
            .write(s, "update_campaign", "update_campaign conflict B", input)
            .await;
        if let (Some(a), Some(_)) = (a, b) {
            match self
                .call("undo_change", json!({ "change_id": a["change_id"] }))
                .await
            {
                Err(e) if e.code == codes::UNDO_CONFLICT => {
                    self.ok(s, "conflict", "UNDO_CONFLICT as expected")
                }
                Err(e) => self.fail(s, "conflict", &e),
                Ok(_) => self.line("FAIL", s, "conflict", "-", "undoing A gave no conflict"),
            }
        }
        while self.undo.len() > base {
            self.undo_last(s).await;
        }
        match self
            .call("get_campaign_details", json!({ "campaign_id": campaign }))
            .await
        {
            Ok(d)
                if d["campaign"]["status"] == "DELETED"
                    || d["campaign"]["effective_status"] == "DELETED" =>
            {
                self.ok(s, "campaign deleted", "status DELETED");
                self.note("V6", "after the undo, the campaign read is DELETED");
            }
            Err(e) if e.code == codes::NOT_FOUND || e.code == codes::API_ERROR => {
                self.ok(s, "campaign deleted", format!("read returns {}", e.code));
                self.note(
                    "V6",
                    format!(
                        "after the undo, reading the campaign gives {}",
                        err_json(&e)
                    ),
                );
            }
            Ok(d) => self.line(
                "FAIL",
                s,
                "campaign deleted",
                "-",
                format!("status {}", d["campaign"]["status"]),
            ),
            Err(e) => self.fail(s, "campaign deleted", &e),
        }
    }

    async fn rollback(&mut self) {
        let s = "rollback";
        let Some(creative) = self.ad_creative() else {
            self.skip(s, "create_campaign", "no post or image");
            return;
        };
        let ad = |c: &Self, n: u8| json!({ "name": c.name(&format!("rollback ad {n}")), "creative": creative.clone() });
        let mut input = self.campaign_input("rollback", vec![ad(self, 1)]);
        // The 2nd ad set depends on the campaign, so Meta only validates it at that point:
        // it fails after the campaign, the 1st ad set, the creative and the ad already exist.
        let mut bad = input["adsets"][0].clone();
        bad["name"] = json!(self.name("rollback invalid ad set"));
        bad["optimization_goal"] = json!("META_INVALID_ADSCONSOLE");
        bad["ads"] = json!([ad(self, 2)]);
        if let Some(list) = input["adsets"].as_array_mut() {
            list.push(bad);
        }
        match self.call("create_campaign", input).await {
            Ok(out) => {
                if out["reversible"] == true {
                    self.undo.push((
                        out["change_id"].as_str().unwrap_or_default().into(),
                        "create_campaign rollback".into(),
                    ));
                }
                self.line(
                    "FAIL",
                    s,
                    "create_campaign",
                    "-",
                    "the invalid ad set was accepted",
                );
                self.undo_last(s).await;
            }
            Err(e) if e.code == codes::PARTIAL_ROLLBACK => {
                self.left_behind.push(format!("rollback: {}", err_json(&e)));
                self.fail(s, "create_campaign", &e);
            }
            Err(e) => {
                let d = e.details.clone().unwrap_or(Value::Null);
                if d["rolled_back"].as_array().is_some_and(|a| !a.is_empty()) {
                    self.ok(
                        s,
                        "create_campaign",
                        format!(
                            "failed at {} and deleted {}",
                            d["failed_step"], d["rolled_back"]
                        ),
                    );
                    self.keep_creative_note("rollback");
                } else {
                    self.fail(s, "create_campaign", &e);
                }
                self.note("E1", format!("rollback: step error: {}", err_json(&e)));
            }
        }
    }

    async fn budget(&mut self) {
        let s = "budget";
        let base = self.undo.len();
        let Some((_, adset)) = self.simple_campaign(s, "budget").await else {
            if self.ad_creative().is_none() {
                self.skip(s, "update_adset", "no post or image");
            }
            return;
        };
        for (i, v) in [21, 22, 23, 24].into_iter().enumerate() {
            let input = json!({ "adset_id": adset, "daily_budget": v });
            if self
                .write(s, "update_adset", &format!("budget {}", i + 1), input)
                .await
                .is_none()
            {
                break;
            }
        }
        match self
            .call(
                "update_adset",
                json!({ "adset_id": adset, "daily_budget": 26 }),
            )
            .await
        {
            Err(e) if e.code == codes::BUDGET_CHANGE_LIMIT => {
                self.ok(
                    s,
                    "update_adset",
                    format!("5th change rejected: {}", e.message),
                );
                self.note("V7", "the 5th change in the same hour was rejected locally");
            }
            Err(e) => self.fail(s, "update_adset", &e),
            Ok(out) => {
                self.undo.push((
                    out["change_id"].as_str().unwrap_or_default().into(),
                    "budget 5".into(),
                ));
                self.line(
                    "FAIL",
                    s,
                    "update_adset",
                    "-",
                    "the 5th change went through",
                );
            }
        }
        // Undoing the budget changes would be another change: just delete the campaign.
        self.undo.truncate(base + 1);
        self.undo_last(s).await;
    }

    async fn duplicate(&mut self) {
        let s = "duplicate";
        let base = self.undo.len();
        let Some((campaign, _)) = self.simple_campaign(s, "duplication").await else {
            if self.ad_creative().is_none() {
                self.skip(s, "duplicate", "no post or image");
            }
            return;
        };
        let input = json!({ "object_type": "campaign", "object_id": campaign });
        match self
            .write(s, "duplicate", "duplicate campaign", input)
            .await
        {
            Some(out) => self.note(
                "V8",
                format!("synchronous copy: {}", short(&out["result"].to_string())),
            ),
            None => self.note("V8", "the copy failed (see E1)"),
        }
        while self.undo.len() > base {
            self.undo_last(s).await;
        }
    }

    /// Filling a field that was empty (spend_cap) has no undo: the change comes out
    /// irreversible, with a warning, and undo refuses. Runs on a helper campaign.
    async fn empty_field(&mut self) {
        let s = "empty_field";
        let base = self.undo.len();
        let Some((campaign, _)) = self.fixture_campaign(s, "empty field").await else {
            self.unwind_to(base).await;
            return;
        };
        let input = json!({ "campaign_id": campaign, "spend_cap": 300 });
        match self.call("update_campaign", input).await {
            Ok(out) => {
                let warned = out["warning"]
                    .as_str()
                    .is_some_and(|w| w.contains("spend_cap"));
                if out["reversible"] == false && warned {
                    self.ok(
                        s,
                        "update_campaign",
                        "spend_cap filled: no undo, with a warning",
                    );
                } else {
                    self.line("FAIL", s, "update_campaign", "-", short(&out.to_string()));
                }
                let id = out["change_id"].clone();
                match self.call("undo_change", json!({ "change_id": id })).await {
                    Err(e) if e.code == codes::NOT_REVERSIBLE => {
                        self.ok(s, "undo_change", "rejected with NOT_REVERSIBLE")
                    }
                    Err(e) => self.fail(s, "undo_change", &e),
                    Ok(_) => self.line("FAIL", s, "undo_change", "-", "the undo went through"),
                }
            }
            Err(e) => self.fail(s, "update_campaign", &e),
        }
        self.unwind_to(base).await;
    }

    async fn video(&mut self, file: Option<&PathBuf>) {
        let s = "video";
        let Some(file) = file else {
            self.skip(s, "upload_video", "no --video");
            return;
        };
        let input = json!({ "file_path": file.to_string_lossy(), "title": self.name("video") });
        if let Some(out) = self.write(s, "upload_video", "upload_video", input).await {
            // Meta does not allow deleting the video through the API: it stays in the account library.
            self.irreversible
                .push(format!("video {}", out["result"]["video_id"]));
        }
    }

    fn report(&self) {
        println!("\n## Results\n");
        println!("| Result | Step | Tool | Code | Detail |");
        println!("|---|---|---|---|---|");
        for l in &self.lines {
            println!(
                "| {} | {} | {} | {} | {} |",
                l.outcome,
                l.step,
                l.tool,
                l.code,
                l.detail.replace('|', "/")
            );
        }
        println!("\n## VERIFY(v26.0) evidence\n");
        for (k, what) in VERIFY {
            println!("{k} {what}");
            match self.evidence.get(k) {
                Some(list) => list.iter().for_each(|e| println!("  - {e}")),
                None => println!("  - not exercised in this run"),
            }
        }
        if let Some(list) = self.evidence.get("E1") {
            println!("E1 full errors");
            list.iter().for_each(|e| println!("  - {e}"));
        }
        println!("\n## What was left in the account\n");
        if self.left_behind.is_empty() {
            println!("Nothing reversible was left behind.");
        } else {
            println!("Check the account:");
            self.left_behind.iter().for_each(|l| println!("  - {l}"));
        }
        if !self.irreversible.is_empty() {
            println!("Irreversible (they stay in the account by nature):");
            self.irreversible.iter().for_each(|l| println!("  - {l}"));
        }
        let count = |o: &str| self.lines.iter().filter(|l| l.outcome == o).count();
        println!(
            "\nTotal: {} OK, {} FAIL, {} SKIPPED.",
            count("OK"),
            count("FAIL"),
            count("SKIPPED")
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(std::env::args().skip(1))?;
    let env = EnvOverrides::from_env()?;
    let dir = data_dir(&env)?;
    let db = Arc::new(Db::open(&dir.join("adsconsole.db"))?);
    let (secrets, _) = open_secret_store(&dir);
    let adapter = MetaAdapter::new(db.clone(), secrets, MetaEndpoints::default());
    let mut config = Config::default();
    config.servers.insert(
        Platform::MetaAds,
        ServerSettings {
            write_mode: WriteMode::Direct,
            ..ServerSettings::default()
        },
    );
    let media = args.funding
        || args.list_media
        || !args.delete_images.is_empty()
        || !args.delete_videos.is_empty()
        || !args.delete_creatives.is_empty();
    // graph_get/graph_post: list and delete media and the creatives that undo leaves behind.
    if let Some(s) = config.servers.get_mut(&Platform::MetaAds) {
        s.groups
            .insert(adsconsole_core::config::ToolGroup::Escape, true);
    }
    let engine = Engine::new(adapter.clone(), db.clone(), Arc::new(RwLock::new(config)))?;
    // The Graph API accepts `method=delete` on a POST: this way the cleanup goes through graph_post.
    let mut deletes: Vec<(String, Value)> = args
        .delete_images
        .iter()
        .map(|h| {
            (
                format!("{}/adimages", args.account),
                json!({ "hash": h, "method": "delete" }),
            )
        })
        .collect();
    deletes.extend(args.delete_videos.iter().map(|v| {
        (
            format!("{}/advideos", args.account),
            json!({ "video_id": v, "method": "delete" }),
        )
    }));
    deletes.extend(
        args.delete_creatives
            .iter()
            .map(|c| (c.clone(), json!({ "method": "delete" }))),
    );
    for (path, params) in deletes {
        let input = json!({ "account_id": args.account, "path": path, "params": params });
        match engine.call_tool("graph_post", input, Some(BY.into())).await {
            Ok(v) => println!("delete {params}: {}", v["result"]),
            Err(e) => println!("delete {params}: {}: {}", e.code, e.message),
        }
    }
    if args.funding {
        let accounts = db.list_accounts(Platform::MetaAds, None)?;
        for (_, a) in accounts {
            let input = json!({
                "account_id": a.account_id,
                "path": a.account_id,
                "params": { "fields": "funding_source_details{type},disable_reason,account_status" },
            });
            match engine.call_tool("graph_get", input, Some(BY.into())).await {
                // Only the type and whether it exists: the card details never leave here.
                Ok(v) => {
                    let f = &v["funding_source_details"];
                    let has = f.is_object() && !f["type"].is_null();
                    println!(
                        "{} | {} | payment: {} (type {}) | status {} | disable_reason {}",
                        a.account_id,
                        a.name,
                        if has { "yes" } else { "no" },
                        f["type"],
                        v["account_status"],
                        v["disable_reason"]
                    );
                }
                Err(e) => println!("{} | {} | {}: {}", a.account_id, a.name, e.code, e.message),
            }
        }
        return Ok(());
    }
    if media {
        for (path, fields) in [
            (
                format!("{}/advideos", args.account),
                "id,title,created_time",
            ),
            (
                format!("{}/adimages", args.account),
                "hash,name,created_time",
            ),
            (
                format!("{}/adcreatives", args.account),
                "id,name,object_story_id,effective_object_story_id",
            ),
            (format!("{}/campaigns", args.account), "id,name,status"),
        ] {
            let input = json!({ "account_id": args.account, "path": path, "params": { "fields": fields, "limit": "100" } });
            match engine.call_tool("graph_get", input, Some(BY.into())).await {
                Ok(v) => println!("{path}: {}", v["data"]),
                Err(e) => println!("{path}: {}: {}", e.code, e.message),
            }
        }
        return Ok(());
    }
    if !args.undo.is_empty() {
        for id in &args.undo {
            match engine.undo(id, true, Some(BY.into())).await {
                Ok(v) => println!("{id}: {}", v["status"]),
                Err(e) => println!("{id}: {}: {}", e.code, e.message),
            }
        }
        return Ok(());
    }
    if let Err(e) = engine.refresh_accounts().await {
        println!(
            "Warning: failed to refresh the accounts: {}: {}",
            e.code, e.message
        );
    }
    let Some((connection, account)) = db
        .accounts_for(Platform::MetaAds, &args.account)?
        .into_iter()
        .next()
    else {
        return Err(format!(
            "account {} is not in any Meta connection (run meta_connect)",
            args.account
        )
        .into());
    };
    println!(
        "Account {} ({}, {}), connection {connection}",
        account.account_id, account.name, account.currency
    );
    if account.status.as_deref() != Some("ACTIVE") {
        println!(
            "Warning: the account is {:?}; writes will be rejected.",
            account.status
        );
    }

    let planned: Vec<&str> = WRITE_STEPS
        .iter()
        .copied()
        .filter(|s| args.runs(s))
        .collect();
    let go = if planned.is_empty() {
        true
    } else {
        println!(
            "\nPlanned real writes (everything paused and undone right after; only the video stays):\n  {}",
            planned.join(", ")
        );
        if !args.yes {
            print!(
                "Type yes to make these writes (any other answer runs only reads and validation): "
            );
            use std::io::Write;
            std::io::stdout().flush()?;
        }
        confirmed(&planned, args.yes, &mut std::io::stdin().lock())
    };

    let tmp = tempfile::tempdir()?;
    let mut c = Checklist {
        engine,
        account: account.account_id.clone(),
        stamp: Utc::now().format("%Y%m%d-%H%M%S").to_string(),
        lines: Vec::new(),
        evidence: BTreeMap::new(),
        undo: Vec::new(),
        left_behind: Vec::new(),
        irreversible: Vec::new(),
        page_id: args.page.clone(),
        image_hash: None,
        post_id: None,
        no_ads: args.no_ads,
        link: args.link.clone(),
        creatives_before: None,
        image_undo: None,
    };
    c.creatives_before = c.creative_ids().await;
    let interrupted = tokio::select! {
        () = run_steps(&mut c, &args, &adapter, &connection, go, tmp.path()) => false,
        r = tokio::signal::ctrl_c() => r.is_ok(),
    };
    if interrupted {
        println!(
            "\nInterrupted (Ctrl+C): undoing what was created. Another Ctrl+C exits without cleaning up."
        );
        tokio::spawn(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                println!(
                    "\nExiting without finishing the cleanup: check the account in Ads Manager."
                );
                std::process::exit(130);
            }
        });
    }
    c.unwind_to(0).await;
    c.sweep_creatives().await;
    if let Some(entry) = c.image_undo.take() {
        c.undo.push(entry);
        c.unwind_to(0).await;
    }
    c.report();
    if interrupted {
        std::process::exit(130);
    }
    Ok(())
}

async fn run_steps(
    c: &mut Checklist,
    args: &Args,
    adapter: &MetaAdapter,
    connection: &str,
    go: bool,
    tmp: &std::path::Path,
) {
    c.note(
        "V1",
        "the connection used was created by meta_connect (browser login, random port)",
    );
    for step in STEPS.iter().filter(|s| args.runs(s)) {
        if WRITE_STEPS.contains(step) && !go {
            c.skip(step, "-", "real writes not confirmed");
            continue;
        }
        println!("\n# {step}");
        match *step {
            "reads" => c.reads().await,
            "image" => c.image(args.image.as_ref(), tmp).await,
            "validation" => c.validation(adapter, connection).await,
            "new_creative" if args.image.is_none() => c.skip(
                step,
                "create_campaign",
                "use --image with an image from the Page's advertiser",
            ),
            "new_creative" => c.new_creative().await,
            "creation" => c.creation().await,
            "rollback" => c.rollback().await,
            "budget" => c.budget().await,
            "duplicate" => c.duplicate().await,
            "empty_field" => c.empty_field().await,
            "video" => c.video(args.video.as_ref()).await,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Args, String> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_args() {
        let a = args(&[
            "123",
            "--only",
            "reads,creation",
            "--yes",
            "--image",
            "x.png",
        ])
        .unwrap();
        assert_eq!(a.account, "act_123");
        assert!(a.yes);
        assert!(a.runs("creation") && !a.runs("video"));
        assert_eq!(a.image, Some(PathBuf::from("x.png")));
        assert!(args(&["act_1", "--only", "nothing"]).is_err());
        assert!(args(&[]).is_err());
        assert!(args(&["act_1", "--zzz"]).is_err());
        // Video (irreversible) and new creative (creates a post on the Page) only run when requested.
        let all = args(&["act_1"]).unwrap();
        assert!(all.runs("creation") && !all.runs("video") && !all.runs("new_creative"));
        assert!(args(&["act_1", "--only", "video"]).unwrap().runs("video"));
        assert_eq!(
            args(&["act_1", "--page", "101"]).unwrap().page.as_deref(),
            Some("101")
        );
        assert!(all.page.is_none() && !all.no_ads);
        assert!(args(&["act_1", "--no-ads"]).unwrap().no_ads);
        assert_eq!(
            args(&["act_1", "--undo", "chg_1", "--undo", "chg_2"])
                .unwrap()
                .undo,
            ["chg_1", "chg_2"]
        );
    }

    #[test]
    fn writes_need_yes_unless_flag() {
        let ask = |input: &str, yes: bool| confirmed(&["creation"], yes, &mut input.as_bytes());
        assert!(ask("yes\n", false));
        assert!(ask("YES\n", false));
        assert!(!ask("no\n", false));
        assert!(!ask("", false));
        assert!(ask("", true));
        assert!(confirmed(&[], false, &mut "".as_bytes()));
    }

    #[test]
    fn generated_png_is_valid() {
        let p = png(1200, 628, 7);
        assert_eq!(&p[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(u32::from_be_bytes(p[16..20].try_into().unwrap()), 1200);
        assert_eq!(u32::from_be_bytes(p[20..24].try_into().unwrap()), 628);
        assert_ne!(png(10, 10, 1), png(10, 10, 2));
    }
}
