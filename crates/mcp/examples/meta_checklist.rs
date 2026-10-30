//! Checklist da Meta Ads numa conta real: roda as ferramentas pelo Engine (modo direto),
//! desfaz cada escrita e imprime um relatório `OK | FALHA | PULADO` com as evidências
//! dos pontos `VERIFY(v26.0)` do código.
//!
//! `cargo run -p adsconsole-mcp --example meta_checklist -- <act_id> [--page <page_id>]
//!     [--image <arquivo>] [--video <arquivo>] [--only <etapas>] [--yes]`
//!
//! - Etapas: leituras, imagem, validacao, criativo_novo, criacao, rollback, orcamento,
//!   duplicar, video. Sem `--only`, rodam todas menos `criativo_novo` e `video`.
//! - A Página dos anúncios é só a de `--page`; sem ela, as etapas que precisam de Página
//!   são puladas.
//! - Os anúncios usam um post que já existe na Página (`existing_post`), porque um app em
//!   modo de desenvolvimento não pode anunciar posts criados por ele; `criativo_novo`
//!   (só com `--only` e `--image`) registra se um criativo novo funciona no modo atual do
//!   app, e se funcionar cria um post não publicado na Página.
//! - `--sem-anuncio`: para conta sem forma de pagamento (a Meta recusa o anúncio), edição,
//!   orçamento e duplicação rodam numa campanha + ad set pausados criados por `graph_post`
//!   e apagados no fim da etapa.
//! - Antes de qualquer escrita real, imprime o plano e pede `sim` (ou `--yes`).
//! - Toda escrita real é desfeita em seguida, inclusive a imagem; Ctrl+C desfaz o que já
//!   foi criado. O vídeo fica na conta (a Meta não deixa apagá-lo pela API).

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

const USAGE: &str = "uso: meta_checklist <act_id> [--page <page_id>] [--link <url>] [--sem-anuncio] [--image <arquivo>] [--video <arquivo>] \
                     [--only <etapa,...>] [--yes] [--undo <change_id>]... [--list-media] [--delete-image <hash>]... \
                     [--delete-video <id>]... [--delete-creative <id>]...";
const STEPS: [&str; 10] = [
    "leituras",
    "imagem",
    "validacao",
    "criativo_novo",
    "criacao",
    "rollback",
    "orcamento",
    "duplicar",
    "campo_vazio",
    "video",
];
const WRITE_STEPS: [&str; 8] = [
    "imagem",
    "criativo_novo",
    "criacao",
    "rollback",
    "orcamento",
    "duplicar",
    "campo_vazio",
    "video",
];
/// Etapas que só rodam pedidas em `--only`: deixam rastro fora do controle do desfazer.
const OPT_IN_STEPS: [&str; 2] = ["criativo_novo", "video"];
const BY: &str = "meta_checklist";

/// Pontos marcados `VERIFY(v26.0)` no código e o que o checklist observa de cada um.
const VERIFY: [(&str, &str); 10] = [
    (
        "V1",
        "login no navegador com porta aleatória em localhost (app em modo de desenvolvimento)",
    ),
    (
        "V2",
        "targeting_automation.advantage_audience exigido no ad set",
    ),
    (
        "V3",
        "is_adset_budget_sharing_enabled sem orçamento de campanha",
    ),
    ("V4", "criativo de imagem com link_data e call_to_action"),
    ("V5", "anúncio com creative {creative_id}"),
    ("V6", "desfazer de uma criação apaga a campanha e os filhos"),
    ("V7", "limite de 4 mudanças de orçamento por hora no ad set"),
    (
        "V8",
        "cópia síncrona com /copies (deep_copy, status_option, rename_options)",
    ),
    (
        "V9",
        "criativo novo (object_story_spec) com o app no modo atual",
    ),
    ("V10", "criativo de post existente (object_story_id)"),
];

#[derive(Debug, PartialEq)]
struct Args {
    account: String,
    /// Página dos anúncios (nunca escolhida sozinha).
    page: Option<String>,
    /// Destino do criativo novo (padrão https://example.com/).
    link: String,
    /// Edição, orçamento e duplicação numa campanha sem anúncio (conta sem pagamento).
    no_ads: bool,
    image: Option<PathBuf>,
    video: Option<PathBuf>,
    only: Option<Vec<String>>,
    yes: bool,
    /// Só desfaz estas mudanças (com force) e sai: limpeza de sobras.
    undo: Vec<String>,
    /// Só lista os vídeos e imagens da conta e sai.
    list_media: bool,
    /// Só diz, para cada conta das conexões, se há forma de pagamento (sem os dados).
    funding: bool,
    /// Apaga estas imagens (hash) e vídeos (id) de teste e sai.
    delete_images: Vec<String>,
    delete_videos: Vec<String>,
    /// Apaga estes criativos (id) de teste e sai.
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
            "--sem-anuncio" => args.no_ads = true,
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
                    return Err(format!(
                        "etapa desconhecida: {bad} (use {})",
                        STEPS.join(", ")
                    ));
                }
                args.only = Some(list);
            }
            other => return Err(format!("opção desconhecida: {other}\n{USAGE}")),
        }
    }
    Ok(args)
}

/// Sem escritas planejadas ou com `--yes`, segue. Senão só com `sim` na entrada.
fn confirmed(planned: &[&str], yes: bool, input: &mut dyn BufRead) -> bool {
    if planned.is_empty() || yes {
        return true;
    }
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        return false;
    }
    line.trim().eq_ignore_ascii_case("sim")
}

// ---------------------------------------------------------------- imagem gerada

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

/// PNG válido em cinza, sem compressão, com um xadrez que depende de `seed`.
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
    /// Mudanças a desfazer, na ordem em que foram feitas: (change_id, rótulo).
    undo: Vec<(String, String)>,
    left_behind: Vec<String>,
    irreversible: Vec<String>,
    page_id: Option<String>,
    image_hash: Option<String>,
    /// Post da Página para os anúncios (`existing_post`).
    post_id: Option<String>,
    no_ads: bool,
    link: String,
    /// Criativos da conta antes da rodada: a limpeza apaga os que surgiram depois.
    creatives_before: Option<Vec<String>>,
    /// Desfazer do envio de imagem: roda depois de apagar os criativos que a usam.
    image_undo: Option<(String, String)>,
}

/// Entrada da pilha de desfazer que não é mudança: campanha de apoio a apagar.
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

/// "Nome (id)" de cada Página de um `get_pages`.
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
        self.line("FALHA", step, tool, &e.code, detail);
        self.note("E1", format!("{step} {tool}: {}", err_json(e)));
    }

    fn skip(&mut self, step: &str, tool: &str, why: &str) {
        self.line("PULADO", step, tool, "-", why);
    }

    fn note(&mut self, key: &'static str, text: impl Into<String>) {
        self.evidence.entry(key).or_default().push(text.into());
    }

    async fn call(&self, tool: &str, mut input: Value) -> Result<Value, ToolError> {
        if tool != "list_ad_accounts" && tool != "undo_change" {
            input["account_id"] = json!(self.account);
        }
        // No tier Limited (60 pontos a cada 5 min), o checklist inteiro passa do limite:
        // espera o tempo que o erro indica e tenta de novo (até 3 vezes).
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
                    println!(
                        "  limite de chamadas da Meta: esperando {} s para {tool}...",
                        secs + 2
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(secs + 2)).await;
                }
                other => return other,
            }
        }
        self.engine.call_tool(tool, input, Some(BY.into())).await
    }

    /// Escrita: em OK, entra na pilha de desfazer (se reversível).
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

    /// Desfaz a última mudança da pilha.
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
                    "{label} desfeita ({})",
                    out["change_id"].as_str().unwrap_or("?")
                ),
            ),
            Err(e) => {
                self.left_behind
                    .push(format!("{label} ({id}): desfazer falhou: {}", e.message));
                self.fail(step, "undo_change", &e);
            }
        }
    }

    /// Desfaz tudo até a pilha ficar com `keep` itens (com force, no fim ou no Ctrl+C).
    async fn unwind_to(&mut self, keep: usize) {
        while self.undo.len() > keep {
            let (id, label) = self.undo.pop().unwrap_or_default();
            if let Some(obj) = id.strip_prefix(FIXTURE) {
                self.delete_fixture("limpeza", obj, &label).await;
                continue;
            }
            match self
                .call("undo_change", json!({ "change_id": id, "force": true }))
                .await
            {
                Ok(_) => println!("  desfeita: {label}"),
                Err(e) => self
                    .left_behind
                    .push(format!("{label} ({id}): {}", e.message)),
            }
        }
    }

    /// Apaga a campanha de apoio (os ad sets vão junto).
    async fn delete_fixture(&mut self, step: &str, id: &str, label: &str) {
        let input = json!({ "path": id, "params": { "method": "delete" } });
        match self.call("graph_post", input).await {
            Ok(_) => self.ok(step, "graph_post", format!("{label} apagada ({id})")),
            Err(e) => {
                self.left_behind
                    .push(format!("{label} ({id}): apagar falhou: {}", e.message));
                self.fail(step, "graph_post", &e);
            }
        }
    }

    /// Campanha + ad set pausados, sem anúncio, criados direto na Graph: a Meta só pede
    /// forma de pagamento no anúncio. Entra na pilha como `fixture:` para ser apagada.
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
        self.undo.push((
            format!("{FIXTURE}{id}"),
            format!("campanha de apoio {what}"),
        ));
        self.ok(
            step,
            "graph_post",
            format!("campanha de apoio sem anúncio: {id}"),
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
                self.ok(step, "graph_post", format!("ad set de apoio: {adset}"));
                Some((id, adset))
            }
            Err(e) => {
                self.fail(step, "graph_post", &e);
                None
            }
        }
    }

    /// Ids dos criativos da conta (até 500), ou `None` se não deu para listar.
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

    /// O desfazer de uma criação deixa os criativos na conta; o checklist apaga os da
    /// rodada: os que não existiam antes dela (o criativo de post existente leva o texto
    /// do post no nome) ou, sem essa lista, os com o carimbo da rodada no nome.
    async fn sweep_creatives(&mut self) {
        let path = format!("{}/adcreatives", self.account);
        let input = json!({ "path": path, "params": { "fields": "id,name", "limit": "500" } });
        let list = match self.call("graph_get", input).await {
            Ok(v) => v["data"].as_array().cloned().unwrap_or_default(),
            Err(e) => {
                self.left_behind
                    .push(format!("criativos: não deu para listar: {}", e.message));
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
                Ok(_) => println!("  apagado: criativo {id}"),
                Err(e) => self
                    .left_behind
                    .push(format!("criativo {id} ({name}): {}", e.message)),
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
            "message": "Teste do AdsConsole (pausado, apagado em seguida).",
            "headline": "Teste AdsConsole",
            "image_hash": hash,
            "call_to_action": "LEARN_MORE",
        })
    }

    /// Criativo dos anúncios de teste: o post existente, se houver; senão um criativo novo.
    fn ad_creative(&self) -> Option<Value> {
        match (&self.post_id, &self.image_hash, &self.page_id) {
            (Some(post), _, _) => Some(json!({ "type": "existing_post", "object_story_id": post })),
            (None, Some(hash), Some(_)) => Some(self.creative(hash)),
            _ => None,
        }
    }

    /// Campanha de 1 ad set. Com post existente, engajamento (o post não tem link externo
    /// para otimizar clique); com criativo novo, tráfego para o site.
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

    /// Id do objeto do `result` de uma criação cujo nome é `name`.
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
                .push(format!("criativo do anúncio de teste ({what})"));
        }
    }

    /// Campanha simples (1 ad set, 1 anúncio): devolve (campanha, ad set).
    async fn simple_campaign(&mut self, step: &str, what: &str) -> Option<(String, String)> {
        if self.no_ads {
            return self.fixture_campaign(step, what).await;
        }
        let creative = self.ad_creative()?;
        let ad = json!({ "name": self.name(&format!("{what} anúncio")), "creative": creative });
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
        let s = "leituras";
        match self.call("list_ad_accounts", json!({})).await {
            Ok(v) => {
                let found = v["accounts"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x["account_id"] == self.account));
                if found {
                    self.ok(
                        s,
                        "list_ad_accounts",
                        format!("{} contas", len(&v, "accounts")),
                    );
                } else {
                    self.line(
                        "FALHA",
                        s,
                        "list_ad_accounts",
                        "-",
                        "a conta não está no cache",
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
                json!({ "type": "interest", "query": "futebol" }),
            ),
            ("search_targeting", json!({ "type": "behavior" })),
            ("search_targeting", json!({ "type": "demographic" })),
            (
                "search_targeting",
                json!({ "type": "geo", "query": "São Paulo", "geo_types": ["city"] }),
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
                    let detail = n.map_or_else(|| short(&v.to_string()), |n| format!("{n} itens"));
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
                                self.ok(s, "get_pages", format!("{n} páginas; usando --page {p}"));
                                self.page_posts(&p).await;
                            }
                            Some(_) => {
                                self.line(
                                    "FALHA",
                                    s,
                                    "get_pages",
                                    "-",
                                    format!("a Página {p} não pode anunciar"),
                                );
                                self.page_id = None;
                            }
                            None => {
                                self.line(
                                    "FALHA",
                                    s,
                                    "get_pages",
                                    "-",
                                    format!(
                                        "a Página {p} não está entre as {n} acessíveis: {}",
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
                        format!("{n} páginas; sem --page, as etapas com Página são puladas"),
                    ),
                }
            }
            Err(e) => self.fail(s, "get_pages", &e),
        }
        match self
            .call(
                "search_ad_library",
                json!({ "query": "tênis", "countries": ["BR"] }),
            )
            .await
        {
            Ok(v) => self.ok(
                s,
                "search_ad_library",
                format!("{} anúncios", len(&v, "ads")),
            ),
            Err(e) if e.code == codes::ACCESS_DENIED => self.skip(
                s,
                "search_ad_library",
                "a Biblioteca exige confirmação de identidade",
            ),
            Err(e) => self.fail(s, "search_ad_library", &e),
        }
    }

    async fn page_posts(&mut self, page: &str) {
        let s = "leituras";
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
                    "usando um nos anúncios"
                } else {
                    "nenhum promovível"
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
        let s = "validacao";
        let Some(creative) = self.ad_creative() else {
            self.skip(s, "create_campaign", "sem post ou imagem para o criativo");
            return;
        };
        let ad = json!({ "name": self.name("validação anúncio"), "creative": creative });
        let mut input = self.campaign_input("validação", vec![ad]);
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
                self.note("V3", "validate_only da campanha sem orçamento de campanha aceitou is_adset_budget_sharing_enabled=false");
            }
            Err(e) => {
                self.note("V3", format!("validate_only da campanha: {}", err_json(&e)));
                self.fail(s, "create_campaign", &e);
            }
        }
    }

    async fn image(&mut self, file: Option<&PathBuf>, dir: &std::path::Path) {
        let s = "imagem";
        let path = match file {
            Some(p) => p.clone(),
            None => {
                let p = dir.join("checklist.png");
                let seed = Utc::now().timestamp_micros() as u64;
                if let Err(e) = std::fs::write(&p, png(1200, 628, seed)) {
                    self.line(
                        "FALHA",
                        s,
                        "upload_image",
                        "-",
                        format!("não deu para gerar a imagem: {e}"),
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
            // A imagem só sai depois dos criativos que a usam (a Meta recusa antes).
            if out["reversible"] == true {
                self.image_undo = self.undo.pop();
            }
        }
    }

    /// Tenta uma campanha com criativo novo (imagem + texto), para registrar se o modo do
    /// app permite. Desfaz em seguida se der certo.
    async fn new_creative(&mut self) {
        let s = "criativo_novo";
        let (Some(_), Some(hash)) = (self.page_id.clone(), self.image_hash.clone()) else {
            self.skip(s, "create_campaign", "sem Página ou imagem");
            return;
        };
        let ad =
            json!({ "name": self.name("criativo novo anúncio"), "creative": self.creative(&hash) });
        let base = self.undo.len();
        match self
            .call(
                "create_campaign",
                self.campaign_input("criativo novo", vec![ad]),
            )
            .await
        {
            Ok(out) => {
                self.undo.push((
                    out["change_id"].as_str().unwrap_or_default().into(),
                    "create_campaign criativo novo".into(),
                ));
                self.ok(s, "create_campaign", "criativo novo aceito");
                self.note(
                    "V9",
                    "criativo novo (object_story_spec) aceito no modo atual do app",
                );
                self.note("V4", "criativo de imagem com link_data e LEARN_MORE criado");
                self.irreversible.push("criativo novo de teste".into());
                while self.undo.len() > base {
                    self.undo_last(s).await;
                }
            }
            Err(e) => {
                let lower = e.message.to_lowercase();
                self.note("V9", format!("criativo novo recusado: {}", err_json(&e)));
                if lower.contains("desenvolvimento") || lower.contains("development") {
                    self.skip(
                        s,
                        "create_campaign",
                        "a Meta recusa criativo novo com o app em modo de desenvolvimento",
                    );
                } else {
                    self.fail(s, "create_campaign", &e);
                }
            }
        }
    }

    async fn creation(&mut self) {
        let s = "criacao";
        let base = self.undo.len();
        let Some((campaign, adset)) = self.simple_campaign(s, "criação").await else {
            if self.ad_creative().is_none() {
                self.skip(
                    s,
                    "create_campaign",
                    "sem post ou imagem (rode leituras e imagem)",
                );
            }
            self.unwind_to(base).await;
            return;
        };
        if self.no_ads {
            self.note(
                "V2",
                "ad set de apoio (graph_post) criado com targeting_automation.advantage_audience=0",
            );
        } else {
            self.note(
                "V2",
                "ad set criado com targeting_automation.advantage_audience=0",
            );
            self.note("V5", "anúncio criado com creative {creative_id}");
        }
        if self.post_id.is_some() && !self.no_ads {
            self.note(
                "V10",
                "campanha criada com criativo de post existente (object_story_id)",
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
                    format!("{} ad sets, {} anúncios", len(&d, "adsets"), len(&d, "ads")),
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
        let input = json!({ "campaign_id": campaign, "name": self.name("criação editada") });
        self.write(s, "update_campaign", "update_campaign nome", input)
            .await;
        let input = json!({
            "adset_id": adset, "name": self.name("ad set editado"), "daily_budget": 25,
            "targeting": { "geo_locations": { "countries": ["BR"] }, "age_min": 21 },
        });
        self.write(
            s,
            "update_adset",
            "update_adset nome+orçamento+público",
            input,
        )
        .await;
        if let Some(ad) = &ad {
            let input = json!({ "ad_id": ad, "name": self.name("anúncio editado") });
            self.write(s, "update_ad", "update_ad nome", input).await;
        }
        if let Some(creative) = &creative {
            let input = json!({ "adset_id": adset, "name": self.name("anúncio extra"), "creative_id": creative });
            self.write(s, "create_ad", "create_ad com criativo existente", input)
                .await;
        }
        // Conflito: duas edições do nome; desfazer a primeira tem que dar UNDO_CONFLICT.
        let input = json!({ "campaign_id": campaign, "name": self.name("A") });
        let a = self
            .write(s, "update_campaign", "update_campaign conflito A", input)
            .await;
        let input = json!({ "campaign_id": campaign, "name": self.name("B") });
        let b = self
            .write(s, "update_campaign", "update_campaign conflito B", input)
            .await;
        if let (Some(a), Some(_)) = (a, b) {
            match self
                .call("undo_change", json!({ "change_id": a["change_id"] }))
                .await
            {
                Err(e) if e.code == codes::UNDO_CONFLICT => {
                    self.ok(s, "conflito", "UNDO_CONFLICT como esperado")
                }
                Err(e) => self.fail(s, "conflito", &e),
                Ok(_) => self.line("FALHA", s, "conflito", "-", "desfazer A não deu conflito"),
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
                self.ok(s, "campanha apagada", "status DELETED");
                self.note("V6", "depois do desfazer, a campanha lida está DELETED");
            }
            Err(e) if e.code == codes::NOT_FOUND || e.code == codes::API_ERROR => {
                self.ok(s, "campanha apagada", format!("leitura devolve {}", e.code));
                self.note(
                    "V6",
                    format!("depois do desfazer, ler a campanha dá {}", err_json(&e)),
                );
            }
            Ok(d) => self.line(
                "FALHA",
                s,
                "campanha apagada",
                "-",
                format!("status {}", d["campaign"]["status"]),
            ),
            Err(e) => self.fail(s, "campanha apagada", &e),
        }
    }

    async fn rollback(&mut self) {
        let s = "rollback";
        let Some(creative) = self.ad_creative() else {
            self.skip(s, "create_campaign", "sem post ou imagem");
            return;
        };
        let ad = |c: &Self, n: u8| json!({ "name": c.name(&format!("rollback anúncio {n}")), "creative": creative.clone() });
        let mut input = self.campaign_input("rollback", vec![ad(self, 1)]);
        // O 2º ad set depende da campanha, então a Meta só o valida na hora: falha depois
        // de a campanha, o 1º ad set, o criativo e o anúncio já existirem.
        let mut bad = input["adsets"][0].clone();
        bad["name"] = json!(self.name("rollback ad set inválido"));
        bad["optimization_goal"] = json!("META_INVALIDO_ADSCONSOLE");
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
                    "FALHA",
                    s,
                    "create_campaign",
                    "-",
                    "o ad set inválido foi aceito",
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
                            "falhou em {} e apagou {}",
                            d["failed_step"], d["rolled_back"]
                        ),
                    );
                    self.keep_creative_note("rollback");
                } else {
                    self.fail(s, "create_campaign", &e);
                }
                self.note("E1", format!("rollback: erro do passo: {}", err_json(&e)));
            }
        }
    }

    async fn budget(&mut self) {
        let s = "orcamento";
        let base = self.undo.len();
        let Some((_, adset)) = self.simple_campaign(s, "orçamento").await else {
            if self.ad_creative().is_none() {
                self.skip(s, "update_adset", "sem post ou imagem");
            }
            return;
        };
        for (i, v) in [21, 22, 23, 24].into_iter().enumerate() {
            let input = json!({ "adset_id": adset, "daily_budget": v });
            if self
                .write(s, "update_adset", &format!("orçamento {}", i + 1), input)
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
                    format!("5ª mudança recusada: {}", e.message),
                );
                self.note("V7", "a 5ª mudança na mesma hora foi recusada localmente");
            }
            Err(e) => self.fail(s, "update_adset", &e),
            Ok(out) => {
                self.undo.push((
                    out["change_id"].as_str().unwrap_or_default().into(),
                    "orçamento 5".into(),
                ));
                self.line("FALHA", s, "update_adset", "-", "a 5ª mudança passou");
            }
        }
        // Desfazer as mudanças de orçamento seria outra mudança: só apaga a campanha.
        self.undo.truncate(base + 1);
        self.undo_last(s).await;
    }

    async fn duplicate(&mut self) {
        let s = "duplicar";
        let base = self.undo.len();
        let Some((campaign, _)) = self.simple_campaign(s, "duplicação").await else {
            if self.ad_creative().is_none() {
                self.skip(s, "duplicate", "sem post ou imagem");
            }
            return;
        };
        let input = json!({ "object_type": "campaign", "object_id": campaign });
        match self
            .write(s, "duplicate", "duplicate campanha", input)
            .await
        {
            Some(out) => self.note(
                "V8",
                format!("cópia síncrona: {}", short(&out["result"].to_string())),
            ),
            None => self.note("V8", "a cópia falhou (ver E1)"),
        }
        while self.undo.len() > base {
            self.undo_last(s).await;
        }
    }

    /// Preencher um campo que estava vazio (spend_cap) não tem desfazer: a mudança sai
    /// irreversível, com aviso, e o desfazer recusa. Roda numa campanha de apoio.
    async fn empty_field(&mut self) {
        let s = "campo_vazio";
        let base = self.undo.len();
        let Some((campaign, _)) = self.fixture_campaign(s, "campo vazio").await else {
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
                        "spend_cap preenchido: sem desfazer, com aviso",
                    );
                } else {
                    self.line("FALHA", s, "update_campaign", "-", short(&out.to_string()));
                }
                let id = out["change_id"].clone();
                match self.call("undo_change", json!({ "change_id": id })).await {
                    Err(e) if e.code == codes::NOT_REVERSIBLE => {
                        self.ok(s, "undo_change", "recusado com NOT_REVERSIBLE")
                    }
                    Err(e) => self.fail(s, "undo_change", &e),
                    Ok(_) => self.line("FALHA", s, "undo_change", "-", "o desfazer passou"),
                }
            }
            Err(e) => self.fail(s, "update_campaign", &e),
        }
        self.unwind_to(base).await;
    }

    async fn video(&mut self, file: Option<&PathBuf>) {
        let s = "video";
        let Some(file) = file else {
            self.skip(s, "upload_video", "sem --video");
            return;
        };
        let input = json!({ "file_path": file.to_string_lossy(), "title": self.name("vídeo") });
        if let Some(out) = self.write(s, "upload_video", "upload_video", input).await {
            // A Meta não deixa apagar o vídeo pela API: fica na biblioteca da conta.
            self.irreversible
                .push(format!("vídeo {}", out["result"]["video_id"]));
        }
    }

    fn report(&self) {
        println!("\n## Resultados\n");
        println!("| Resultado | Etapa | Ferramenta | Código | Detalhe |");
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
        println!("\n## Evidências VERIFY(v26.0)\n");
        for (k, what) in VERIFY {
            println!("{k} {what}");
            match self.evidence.get(k) {
                Some(list) => list.iter().for_each(|e| println!("  - {e}")),
                None => println!("  - não exercitado nesta execução"),
            }
        }
        if let Some(list) = self.evidence.get("E1") {
            println!("E1 erros completos");
            list.iter().for_each(|e| println!("  - {e}"));
        }
        println!("\n## O que ficou na conta\n");
        if self.left_behind.is_empty() {
            println!("Nada reversível ficou para trás.");
        } else {
            println!("Confira na conta:");
            self.left_behind.iter().for_each(|l| println!("  - {l}"));
        }
        if !self.irreversible.is_empty() {
            println!("Irreversíveis (ficam na conta por natureza):");
            self.irreversible.iter().for_each(|l| println!("  - {l}"));
        }
        let count = |o: &str| self.lines.iter().filter(|l| l.outcome == o).count();
        println!(
            "\nTotal: {} OK, {} FALHA, {} PULADO.",
            count("OK"),
            count("FALHA"),
            count("PULADO")
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
    // graph_get/graph_post: listar e apagar mídia e os criativos que o desfazer deixa.
    if let Some(s) = config.servers.get_mut(&Platform::MetaAds) {
        s.groups
            .insert(adsconsole_core::config::ToolGroup::Escape, true);
    }
    let engine = Engine::new(adapter.clone(), db.clone(), Arc::new(RwLock::new(config)))?;
    // A Graph aceita `method=delete` num POST: assim a limpeza passa pelo graph_post.
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
            Ok(v) => println!("apagar {params}: {}", v["result"]),
            Err(e) => println!("apagar {params}: {}: {}", e.code, e.message),
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
                // Só o tipo e se existe: os dados do cartão não saem daqui.
                Ok(v) => {
                    let f = &v["funding_source_details"];
                    let has = f.is_object() && !f["type"].is_null();
                    println!(
                        "{} | {} | pagamento: {} (tipo {}) | status {} | disable_reason {}",
                        a.account_id,
                        a.name,
                        if has { "sim" } else { "não" },
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
            "Aviso: falha ao atualizar as contas: {}: {}",
            e.code, e.message
        );
    }
    let Some((connection, account)) = db
        .accounts_for(Platform::MetaAds, &args.account)?
        .into_iter()
        .next()
    else {
        return Err(format!(
            "a conta {} não está em nenhuma conexão da Meta (rode meta_connect)",
            args.account
        )
        .into());
    };
    println!(
        "Conta {} ({}, {}), conexão {connection}",
        account.account_id, account.name, account.currency
    );
    if account.status.as_deref() != Some("ACTIVE") {
        println!(
            "Aviso: a conta está {:?}; as escritas serão recusadas.",
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
            "\nEscritas reais planejadas (tudo pausado e desfeito em seguida; só o vídeo fica):\n  {}",
            planned.join(", ")
        );
        if !args.yes {
            print!(
                "Digite sim para fazer essas escritas (outra resposta roda só leituras e validação): "
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
            "\nInterrompido (Ctrl+C): desfazendo o que foi criado. Outro Ctrl+C sai sem limpar."
        );
        tokio::spawn(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                println!(
                    "\nSaindo sem terminar a limpeza: confira a conta no Gerenciador de Anúncios."
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
        "a conexão usada foi criada pelo meta_connect (login no navegador, porta aleatória)",
    );
    for step in STEPS.iter().filter(|s| args.runs(s)) {
        if WRITE_STEPS.contains(step) && !go {
            c.skip(step, "-", "escritas reais não confirmadas");
            continue;
        }
        println!("\n# {step}");
        match *step {
            "leituras" => c.reads().await,
            "imagem" => c.image(args.image.as_ref(), tmp).await,
            "validacao" => c.validation(adapter, connection).await,
            "criativo_novo" if args.image.is_none() => c.skip(
                step,
                "create_campaign",
                "use --image com uma imagem do anunciante da Página",
            ),
            "criativo_novo" => c.new_creative().await,
            "criacao" => c.creation().await,
            "rollback" => c.rollback().await,
            "orcamento" => c.budget().await,
            "duplicar" => c.duplicate().await,
            "campo_vazio" => c.empty_field().await,
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
            "leituras,criacao",
            "--yes",
            "--image",
            "x.png",
        ])
        .unwrap();
        assert_eq!(a.account, "act_123");
        assert!(a.yes);
        assert!(a.runs("criacao") && !a.runs("video"));
        assert_eq!(a.image, Some(PathBuf::from("x.png")));
        assert!(args(&["act_1", "--only", "nada"]).is_err());
        assert!(args(&[]).is_err());
        assert!(args(&["act_1", "--zzz"]).is_err());
        // Vídeo (irreversível) e criativo novo (cria post na Página) só rodam pedidos.
        let all = args(&["act_1"]).unwrap();
        assert!(all.runs("criacao") && !all.runs("video") && !all.runs("criativo_novo"));
        assert!(args(&["act_1", "--only", "video"]).unwrap().runs("video"));
        assert_eq!(
            args(&["act_1", "--page", "101"]).unwrap().page.as_deref(),
            Some("101")
        );
        assert!(all.page.is_none() && !all.no_ads);
        assert!(args(&["act_1", "--sem-anuncio"]).unwrap().no_ads);
        assert_eq!(
            args(&["act_1", "--undo", "chg_1", "--undo", "chg_2"])
                .unwrap()
                .undo,
            ["chg_1", "chg_2"]
        );
    }

    #[test]
    fn writes_need_sim_unless_yes() {
        let ask = |input: &str, yes: bool| confirmed(&["criacao"], yes, &mut input.as_bytes());
        assert!(ask("sim\n", false));
        assert!(ask("SIM\n", false));
        assert!(!ask("não\n", false));
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
