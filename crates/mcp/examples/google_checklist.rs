//! Checklist das ferramentas do Google Ads numa conta real, pelo `Engine` em modo Direct.
//!
//! `cargo run -p adsconsole-mcp --example google_checklist -- <account_id> [--video <arquivo>]
//!     [--image <arquivo>] [--only <etapa,...>] [--yes]`
//!
//! Usa a conexão do diretório de dados (`ADSCONSOLE_DATA_DIR` ou o padrão), criada com
//! `google_connect`. O `config.json` não é lido nem alterado: o modo Direct vale só aqui.
//!
//! Etapas, nesta ordem: `leituras`, `validacao` (só `validateOnly`), `search`,
//! `demand_gen`, `display`, `asset_texto`, `imagem` (com `--image`) e `video` (com
//! `--video`). As quatro primeiras de escrita criam objetos pausados e desfazem logo em
//! seguida; `asset_texto` e `imagem` são irreversíveis. Antes de qualquer escrita real o
//! plano é impresso e é preciso digitar `sim` (ou passar `--yes`).
//!
//! Cada passo imprime `OK|FALHA|PULADO <etapa> <ferramenta> <código?> <detalhe>`. No fim
//! saem a tabela de resultados, as evidências de cada item VERIFY(v26) e o que ficou na
//! conta. Nenhum token ou segredo é impresso. O log (com as chaves da resposta do
//! upload de vídeo) fica em `<dados>/logs`.

use std::collections::BTreeMap;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use adsconsole_core::Platform;
use adsconsole_core::config::{Config, ServerSettings, WriteMode};
use adsconsole_core::db::{ChangeFilter, ChangeStatus, Db};
use adsconsole_core::paths::{EnvOverrides, data_dir};
use adsconsole_core::routing::{normalize_account_id, resolve_connection};
use adsconsole_core::secrets::open_secret_store;
use adsconsole_google_ads::endpoints::GoogleEndpoints;
use adsconsole_mcp::engine::Engine;
use adsconsole_mcp::error::{ToolError, codes};
use adsconsole_mcp::google::GoogleAdapter;
use adsconsole_mcp::{Call, PlatformAdapter, Validated};
use chrono::{Days, Utc};
use serde_json::{Value, json};

const USAGE: &str = "uso: google_checklist <account_id> [--video <arquivo>] [--image <arquivo>] \
                     [--only <etapa,...>] [--yes]";
const REQUESTED_BY: &str = "google_checklist";
/// Rótulo da edição de anúncio (o desfazer dela é evidência de T9.2).
const UPDATE_AD_LABEL: &str = "update_ad títulos";

/// Etapas na ordem em que rodam.
const STEPS: [&str; 8] = [
    "leituras",
    "validacao",
    "search",
    "demand_gen",
    "display",
    "asset_texto",
    "imagem",
    "video",
];
/// Etapas que fazem escritas reais (pedem confirmação).
const WRITE_STEPS: [&str; 6] = [
    "search",
    "demand_gen",
    "display",
    "asset_texto",
    "imagem",
    "video",
];

/// Itens VERIFY(v26) das Tasks 9-16, na ordem do relatório.
const VERIFY: &[(&str, &str)] = &[
    (
        "T9.1",
        "update_ad: AdOperation update em customers/{c}/ads/{id} e leitura FROM ad",
    ),
    (
        "T9.2",
        "RSA: chaves só de saída (assetPerformanceLabel, policySummaryInfo) não dão conflito no desfazer",
    ),
    (
        "T9.3",
        "campaign.start_date_time/end_date_time (startDateTime/endDateTime); start_date/end_date não existem na v26",
    ),
    (
        "T9.4",
        "desfazer volta um valor que era nulo (end_date_time ausente)",
    ),
    (
        "T10.1",
        "temporários compostos sharedCriteria/-1~-2 e campaignSharedSets/<campanha>~-1 (create_list)",
    ),
    ("T10.2", "attach_list com o resourceName omitido"),
    (
        "T11.1",
        "desfazer set_bid_modifier quando o bid_modifier era nulo",
    ),
    (
        "T12.1",
        "update_asset_group: assetGroupAssets/{ag}~-N~FIELD e asset existente sem resourceName",
    ),
    (
        "T12.2",
        "recommendation.dismissed (leitura do plano de apply/dismiss)",
    ),
    (
        "T13.1",
        "filhos criados sem resourceName (critérios, adGroupAd, campaignAsset)",
    ),
    ("T13.2", "startDateTime/endDateTime na criação da campanha"),
    (
        "E1",
        "falhas API_ERROR de qualquer passo, com o erro completo (field_path)",
    ),
    (
        "E2",
        "operações (tipo, temporários e referências, sem bytes) das validações que falharam",
    ),
    (
        "T14.1",
        "PMax: assetAutomationSettings e brandGuidelinesEnabled",
    ),
    (
        "T14.2",
        "assets de imagem nomeados pelo conteúdo (img-<sha>) e deduplicação/DUPLICATE_ASSET_NAME",
    ),
    ("T14.3", "PMax: quantos sinais de público por asset group"),
    ("T15.1", "Demand Gen: local e idioma no grupo de anúncios"),
    ("T15.2", "Demand Gen: grupo de anúncios sem type"),
    (
        "T15.3",
        "businessName: string (multi-asset e carrossel); AdTextAsset (vídeo)",
    ),
    ("T15.4", "callToActionText livre (limite local 30)"),
    ("T15.5", "carrossel: papel da imagem do card"),
    (
        "T15.6",
        "Display sem networkSettings; RDA businessName string e longHeadline {text}",
    ),
    (
        "T16.1",
        "upload_video: onde vem o nome do upload (início ou resposta do finalize)",
    ),
    (
        "T16.2",
        "upload_video: URL da sessão com a mesma origem de upload_base",
    ),
];

// ---------------------------------------------------------------- argumentos

#[derive(Debug, PartialEq)]
struct Args {
    account: String,
    video: Option<PathBuf>,
    image: Option<PathBuf>,
    only: Option<Vec<String>>,
    yes: bool,
}

impl Args {
    /// A etapa roda nesta execução? `imagem` e `video` precisam do arquivo.
    fn runs(&self, step: &str) -> bool {
        match step {
            "imagem" if self.image.is_none() => false,
            "video" if self.video.is_none() => false,
            _ => self
                .only
                .as_ref()
                .is_none_or(|l| l.iter().any(|s| s == step)),
        }
    }
}

fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut account = None;
    let mut video = None;
    let mut image = None;
    let mut only = None;
    let mut yes = false;
    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--video" => video = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--image" => image = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
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
                if list.is_empty() {
                    return Err(USAGE.into());
                }
                only = Some(list);
            }
            "--yes" => yes = true,
            s if s.starts_with("--") => return Err(format!("opção desconhecida: {s}\n{USAGE}")),
            _ if account.is_none() => account = Some(a),
            _ => return Err(USAGE.into()),
        }
    }
    let account = account.ok_or(USAGE)?;
    if let Some(l) = &only {
        if l.iter().any(|s| s == "video") && video.is_none() {
            return Err("--only video precisa de --video <arquivo>".into());
        }
        if l.iter().any(|s| s == "imagem") && image.is_none() {
            return Err("--only imagem precisa de --image <arquivo>".into());
        }
    }
    Ok(Args {
        account,
        video,
        image,
        only,
        yes,
    })
}

/// Escritas reais que esta execução vai fazer, uma por linha.
fn planned_writes(args: &Args) -> Vec<String> {
    let mut out = Vec::new();
    for step in WRITE_STEPS.iter().filter(|s| args.runs(s)) {
        out.push(match *step {
            "search" => {
                "search: create_search_campaign pausada; update_campaign (nome, orçamento, \
                 end_date, start_date; v26: *_date_time), update_ad_group, update_ad, manage_keywords (add, pause, \
                 set_bid), manage_negatives (add, create_list x2, attach_list), manage_targeting \
                 (add local e horário, set_bid_modifier); conflito com dois update_campaign; \
                 desfaz tudo em ordem inversa e a campanha fica REMOVED"
                    .to_string()
            }
            "demand_gen" => "demand_gen: create_demand_gen_campaign pausada e desfazer (fica \
                 REMOVED; os assets de imagem e texto criados ficam na conta)"
                .to_string(),
            "display" => "display: create_display_campaign pausada e desfazer (fica REMOVED; os \
                 assets de imagem e texto ficam na conta)"
                .to_string(),
            "asset_texto" => "asset_texto: upload_asset texto (irreversível)".to_string(),
            "imagem" => format!(
                "imagem: upload_asset imagem {} (irreversível)",
                args.image.as_deref().unwrap_or(Path::new("")).display()
            ),
            "video" => format!(
                "video: upload_video {} (não listado) e desfazer (remove o vídeo do YouTube; o \
                 asset do vídeo fica na conta)",
                args.video.as_deref().unwrap_or(Path::new("")).display()
            ),
            other => other.to_string(),
        });
    }
    out
}

/// Sem escritas planejadas ou com `--yes`, segue. Senão só com `sim` na entrada.
fn confirmed(planned: &[String], yes: bool, input: &mut dyn BufRead) -> bool {
    if planned.is_empty() || yes {
        return true;
    }
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        return false;
    }
    line.trim().eq_ignore_ascii_case("sim")
}

// ---------------------------------------------------------------- imagens

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
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// PNG válido em tons de cinza, sem compressão (deflate "stored"), com um xadrez que
/// depende de `seed`: cada execução gera bytes novos.
fn png(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize + 1) * h as usize);
    for y in 0..h {
        raw.push(0); // filtro None
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
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &z);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

/// Imagens geradas num diretório temporário: 1.91:1, 1:1 e logo 1:1.
struct Images {
    _dir: tempfile::TempDir,
    landscape: String,
    /// Outra 1.91:1 (o carrossel exige uma imagem diferente por card).
    landscape2: String,
    square: String,
    logo: String,
    /// (arquivo, nome do asset pelo conteúdo).
    names: Vec<(String, String)>,
}

fn make_images(seed: u64) -> std::io::Result<Images> {
    let dir = tempfile::tempdir()?;
    let mut names = Vec::new();
    let mut write = |name: &str, bytes: Vec<u8>| -> std::io::Result<String> {
        let p = dir.path().join(name);
        std::fs::write(&p, &bytes)?;
        if let Ok(img) = adsconsole_google_ads::images::decode(bytes) {
            names.push((name.to_string(), img.content_name()));
        }
        Ok(p.to_string_lossy().into_owned())
    };
    let landscape = write("landscape.png", png(1200, 628, seed))?;
    let square = write("square.png", png(1200, 1200, seed + 1))?;
    let logo = write("logo.png", png(400, 400, seed + 2))?;
    let landscape2 = write("landscape2.png", png(1200, 628, seed + 3))?;
    Ok(Images {
        _dir: dir,
        landscape,
        landscape2,
        square,
        logo,
        names,
    })
}

// ---------------------------------------------------------------- utilidades

fn short(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &one_line[..i]),
        None => one_line,
    }
}

fn err_detail(e: &ToolError) -> String {
    let mut s = e.message.clone();
    if let Some(f) = &e.field {
        s.push_str(&format!(" [campo {f}]"));
    }
    short(&s, 240)
}

/// Erro inteiro (código, mensagem, campo e detalhes do Google), sem segredos.
fn err_evidence(e: &ToolError) -> String {
    short(
        &json!({ "code": e.code, "message": e.message, "field": e.field, "details": e.details })
            .to_string(),
        3000,
    )
}

/// Nomes de recurso (`customers/...`) entre as chaves de `result` de uma escrita.
fn resource_keys(body: &Value) -> Vec<String> {
    body["result"]
        .as_object()
        .map(|m| {
            m.keys()
                .filter(|k| k.starts_with("customers/"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn find_rn(body: &Value, collection: &str) -> Option<String> {
    let needle = format!("/{collection}/");
    resource_keys(body)
        .into_iter()
        .find(|k| k.contains(&needle))
}

/// Último id de um nome de recurso (`.../campaigns/12` → 12, `.../x/5~7` → 7).
fn last_id(rn: &str) -> String {
    rn.rsplit(['/', '~']).next().unwrap_or_default().to_string()
}

fn rows(v: &Value) -> Vec<Value> {
    v["rows"].as_array().cloned().unwrap_or_default()
}

/// Valor num caminho com pontos (`adGroup.id`), como texto.
fn at(row: &Value, path: &str) -> Option<String> {
    let v = path.split('.').try_fold(row, |v, k| v.get(k))?;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn day(offset: u64) -> String {
    (Utc::now().date_naive() + Days::new(offset))
        .format("%Y-%m-%d")
        .to_string()
}

// ---------------------------------------------------------------- execução

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mark {
    Ok,
    Falha,
    Pulado,
}

impl Mark {
    fn label(self) -> &'static str {
        match self {
            Mark::Ok => "OK",
            Mark::Falha => "FALHA",
            Mark::Pulado => "PULADO",
        }
    }
}

struct Line {
    mark: Mark,
    step: String,
    tool: String,
    code: Option<String>,
    detail: String,
}

/// Conferência depois de um desfazer.
enum AfterUndo {
    Nothing,
    /// Lê start_date_time/end_date_time da campanha (T9.3/T9.4).
    CampaignDates(String),
    /// Confere `campaign.status = REMOVED`.
    CampaignRemoved(String),
    /// Lê o bid_modifier do critério de campanha (VERIFY T11.1).
    CriterionBid(String),
    /// Lê o estado do upload de vídeo.
    VideoUpload(String),
}

struct Undoable {
    change_id: String,
    step: String,
    label: String,
    resources: Vec<String>,
    after: AfterUndo,
}

struct Checklist {
    engine: Engine,
    adapter: Arc<GoogleAdapter>,
    account: String,
    connection: String,
    stamp: String,
    lines: Vec<Line>,
    evidence: BTreeMap<&'static str, Vec<String>>,
    undo: Vec<Undoable>,
    left_behind: Vec<String>,
    irreversible: Vec<String>,
}

impl Checklist {
    fn line(&mut self, mark: Mark, step: &str, tool: &str, code: Option<&str>, detail: String) {
        println!(
            "{} {step} {tool} {}{}",
            mark.label(),
            code.map(|c| format!("{c} ")).unwrap_or_default(),
            detail
        );
        self.lines.push(Line {
            mark,
            step: step.into(),
            tool: tool.into(),
            code: code.map(str::to_string),
            detail,
        });
    }

    fn ok(&mut self, step: &str, tool: &str, detail: impl Into<String>) {
        self.line(Mark::Ok, step, tool, None, detail.into());
    }

    fn fail(&mut self, step: &str, tool: &str, e: &ToolError) {
        if e.code == "API_ERROR" {
            self.evidence("E1", format!("{step} {tool}: {}", err_evidence(e)));
        }
        self.line(Mark::Falha, step, tool, Some(&e.code), err_detail(e));
    }

    fn fail_msg(&mut self, step: &str, tool: &str, detail: impl Into<String>) {
        self.line(Mark::Falha, step, tool, None, detail.into());
    }

    fn skip(&mut self, step: &str, tool: &str, detail: impl Into<String>) {
        self.line(Mark::Pulado, step, tool, None, detail.into());
    }

    fn evidence(&mut self, item: &'static str, text: impl Into<String>) {
        let text = text.into();
        println!("  evidência {item}: {}", short(&text, 400));
        self.evidence.entry(item).or_default().push(text);
    }

    fn with_account(&self, input: Value) -> Value {
        let mut input = input;
        if let Some(m) = input.as_object_mut() {
            m.insert("account_id".into(), json!(self.account));
        }
        input
    }

    async fn call(&self, tool: &str, input: Value) -> Result<Value, ToolError> {
        let input = self.with_account(input);
        self.engine
            .call_tool(tool, input, Some(REQUESTED_BY.into()))
            .await
    }

    /// GAQL de apoio (busca de ids e evidências), sem linha no relatório.
    async fn gaql(&self, query: &str) -> Result<Vec<Value>, ToolError> {
        self.call("gaql_search", json!({ "query": query }))
            .await
            .map(|v| rows(&v))
    }

    /// GAQL de evidência: grava as linhas (ou o erro) no item.
    async fn gaql_evidence(&mut self, item: &'static str, what: &str, query: &str) {
        let text = match self.gaql(query).await {
            Ok(r) => format!("{what}: {}", Value::Array(r)),
            Err(e) => format!("{what}: erro {}", err_evidence(&e)),
        };
        self.evidence(item, text);
    }

    /// Leitura registrada: OK com um resumo das chaves, ou FALHA com o código.
    async fn read(&mut self, step: &str, tool: &str, input: Value) -> Option<Value> {
        match self.call(tool, input).await {
            Ok(v) => {
                let summary = match &v {
                    Value::Object(m) => m
                        .iter()
                        .map(|(k, x)| match x {
                            Value::Array(a) => format!("{k}[{}]", a.len()),
                            _ => k.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    Value::Array(a) => format!("{} itens", a.len()),
                    other => short(&other.to_string(), 80),
                };
                self.ok(step, tool, short(&summary, 200));
                Some(v)
            }
            // Token Explorer não acessa o Keyword Planner: é o comportamento correto.
            Err(e)
                if e.code == "ACCESS_LEVEL_INSUFFICIENT"
                    && matches!(tool, "keyword_ideas" | "keyword_metrics") =>
            {
                self.skip(step, tool, "exige nível Basic; conexão é Explorer");
                None
            }
            Err(e) => {
                self.fail(step, tool, &e);
                None
            }
        }
    }

    /// `precheck` + `validate` do adapter (regras locais e `validateOnly`), sem gravar
    /// mudança e sem aplicar.
    async fn validate(&self, tool: &str, input: Value) -> Result<Validated, ToolError> {
        let call = Call {
            connection_id: self.connection.clone(),
            account_id: self.account.clone(),
            tool: tool.into(),
            input: self.with_account(input),
        };
        self.adapter.precheck(&call).await?;
        self.adapter.validate(&call).await
    }

    /// Evidência E2: resumo das operações que a ferramenta enviaria (sem bytes), com as
    /// referências a temporários; serve para diagnosticar falhas de validateOnly.
    async fn operations_evidence(&mut self, tool: &str, input: Value) {
        let call = Call {
            connection_id: self.connection.clone(),
            account_id: self.account.clone(),
            tool: tool.into(),
            input: self.with_account(input),
        };
        match self.adapter.describe_operations(&call).await {
            Ok(lines) => self.evidence(
                "E2",
                format!("{tool}: operações enviadas\n    {}", lines.join("\n    ")),
            ),
            Err(e) => self.evidence(
                "E2",
                format!("{tool}: plano sem envio falhou: {}", err_evidence(&e)),
            ),
        }
    }

    /// Validação registrada; `accept` lista códigos de erro que contam como sucesso.
    async fn check_validate(
        &mut self,
        step: &str,
        tool: &str,
        input: Value,
        accept: &[&str],
    ) -> Result<Validated, ToolError> {
        let r = self.validate(tool, input).await;
        match &r {
            Ok(v) => {
                let warn = v
                    .warning
                    .as_deref()
                    .map(|w| format!(" (aviso: {w})"))
                    .unwrap_or_default();
                self.ok(
                    step,
                    tool,
                    short(&format!("validateOnly aceito: {}{warn}", v.summary), 240),
                );
            }
            Err(e) if accept.contains(&e.code.as_str()) => {
                let d = format!("{} (esperado): {}", e.code, err_detail(e));
                self.ok(step, tool, d);
            }
            Err(e) => self.fail(step, tool, e),
        }
        r
    }

    /// Escrita real registrada. Reversível: entra na pilha de desfazer.
    async fn write(
        &mut self,
        step: &str,
        tool: &str,
        label: &str,
        input: Value,
        after: AfterUndo,
    ) -> Result<Value, ToolError> {
        let r = self.call(tool, input).await;
        match &r {
            Ok(v) => {
                let change_id = v["change_id"].as_str().unwrap_or_default().to_string();
                let resources = resource_keys(v);
                let reversible = v["reversible"].as_bool().unwrap_or(false);
                self.ok(
                    step,
                    tool,
                    short(
                        &format!(
                            "{label}: {change_id} {}{}",
                            resources.join(" "),
                            if reversible { "" } else { " (irreversível)" }
                        ),
                        300,
                    ),
                );
                if reversible {
                    self.undo.push(Undoable {
                        change_id,
                        step: step.into(),
                        label: label.into(),
                        resources,
                        after,
                    });
                }
            }
            Err(e) => {
                self.fail(step, tool, e);
                if maybe_applied(tool, e) {
                    let id = e
                        .details
                        .as_ref()
                        .and_then(|d| d["change_id"].as_str())
                        .unwrap_or("-");
                    self.left_behind.push(format!(
                        "possivelmente aplicado: {label} (change_id {id}); procure \
                         'AdsConsole checklist {}' no Google Ads",
                        self.stamp
                    ));
                }
            }
        }
        r
    }

    /// Desfaz o último item da pilha. `UNDO_CONFLICT` fora do teste de conflito é uma
    /// falha (com os diffs na evidência); tenta de novo com `force` para limpar a conta.
    async fn undo_last(&mut self) -> bool {
        let Some(u) = self.undo.pop() else {
            return false;
        };
        let step = u.step.clone();
        let mut forced = false;
        let mut r = self
            .engine
            .undo(&u.change_id, false, Some(REQUESTED_BY.into()))
            .await;
        if let Err(e) = &r
            && e.code == codes::UNDO_CONFLICT
        {
            self.fail(&step, "undo_change", e);
            println!("  diffs: {}", short(&err_evidence(e), 1500));
            if u.label == UPDATE_AD_LABEL {
                self.evidence(
                    "T9.2",
                    format!("desfazer do update_ad: {}", err_evidence(e)),
                );
            }
            forced = true;
            r = self
                .engine
                .undo(&u.change_id, true, Some(REQUESTED_BY.into()))
                .await;
        }
        match r {
            Ok(v) => {
                let id = v["change_id"].as_str().unwrap_or_default();
                let how = if forced { " com force" } else { "" };
                self.ok(
                    &step,
                    "undo_change",
                    format!("{} desfeita{how} ({id})", u.label),
                );
                if u.label == UPDATE_AD_LABEL && !forced {
                    self.evidence(
                        "T9.2",
                        "desfazer do update_ad sem UNDO_CONFLICT (sem chaves de saída no diff)",
                    );
                }
                self.after_undo(&step, &u).await;
                true
            }
            Err(e) => {
                self.fail(&step, "undo_change", &e);
                self.left_behind.push(format!(
                    "{} ({}): {}",
                    u.label,
                    u.change_id,
                    u.resources.join(", ")
                ));
                false
            }
        }
    }

    async fn after_undo(&mut self, step: &str, u: &Undoable) {
        match &u.after {
            AfterUndo::Nothing => {}
            AfterUndo::CampaignDates(id) => {
                let q = format!(
                    "SELECT campaign.start_date_time, campaign.end_date_time FROM campaign WHERE campaign.id = {id}"
                );
                self.gaql_evidence("T9.4", &format!("datas depois de desfazer {}", u.label), &q)
                    .await;
            }
            AfterUndo::CampaignRemoved(id) => {
                let q = format!("SELECT campaign.status FROM campaign WHERE campaign.id = {id}");
                match self.gaql(&q).await {
                    Ok(r) => {
                        let st = r.first().and_then(|x| at(x, "campaign.status"));
                        if st.as_deref() == Some("REMOVED") {
                            self.ok(step, "remove_resources", format!("campanha {id} REMOVED"));
                        } else {
                            self.fail_msg(
                                step,
                                "remove_resources",
                                format!("campanha {id} com status {st:?}"),
                            );
                            self.left_behind
                                .push(format!("campanha {id} (status {st:?})"));
                        }
                    }
                    Err(e) => self.fail(step, "remove_resources", &e),
                }
            }
            AfterUndo::CriterionBid(rn) => {
                let q = format!(
                    "SELECT campaign_criterion.criterion_id, campaign_criterion.bid_modifier \
                     FROM campaign_criterion WHERE campaign_criterion.resource_name = '{rn}'"
                );
                self.gaql_evidence("T11.1", "bid_modifier depois de desfazer", &q)
                    .await;
            }
            AfterUndo::VideoUpload(rn) => {
                let q = format!(
                    "SELECT you_tube_video_upload.resource_name, you_tube_video_upload.state \
                     FROM you_tube_video_upload WHERE you_tube_video_upload.resource_name = '{rn}'"
                );
                self.gaql_evidence("T16.1", "upload depois de desfazer", &q)
                    .await;
            }
        }
    }

    /// Desfaz até sobrarem `keep` itens na pilha.
    async fn unwind_to(&mut self, keep: usize) {
        while self.undo.len() > keep {
            self.undo_last().await;
        }
    }

    // ------------------------------------------------------------ 1. leituras

    async fn reads(&mut self) {
        let s = "leituras";
        let accounts = self
            .engine
            .call_tool("list_accounts", json!({}), Some(REQUESTED_BY.into()))
            .await;
        match accounts {
            Ok(v) => {
                let n = v["accounts"].as_array().map_or(0, Vec::len);
                self.ok(s, "list_accounts", format!("{n} contas"));
            }
            Err(e) => self.fail(s, "list_accounts", &e),
        }
        let campaigns = self
            .read(
                s,
                "get_campaigns",
                json!({ "date_range": { "preset": "LAST_30_DAYS" } }),
            )
            .await;
        let first = campaigns.as_ref().and_then(|v| {
            v["campaigns"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|c| at(c, "campaign_id"))
        });
        match first {
            Some(id) => {
                self.read(s, "get_campaign_details", json!({ "campaign_id": id }))
                    .await;
            }
            None => self.skip(s, "get_campaign_details", "a conta não tem campanhas"),
        }
        self.read(s, "get_search_terms", json!({ "limit": 20 }))
            .await;
        self.read(s, "get_recommendations", json!({})).await;
        self.read(s, "get_conversion_actions", json!({})).await;
        self.read(s, "get_change_history", json!({ "days": 7, "limit": 20 }))
            .await;
        self.read(
            s,
            "keyword_ideas",
            json!({ "keywords": ["sapato social"], "language_id": "1014",
                    "geo_ids": ["2076"], "limit": 10 }),
        )
        .await;
        self.read(
            s,
            "keyword_metrics",
            json!({ "keywords": ["sapato social"], "language_id": "1014", "geo_ids": ["2076"],
                    "forecast": { "daily_budget": 10, "max_cpc": 1 } }),
        )
        .await;
        self.gaql_with_cursor().await;
        self.read(s, "list_changes", json!({ "limit": 5 })).await;
    }

    /// `gaql_search` com resposta grande (cortada) e a segunda página pelo cursor.
    async fn gaql_with_cursor(&mut self) {
        let s = "leituras";
        let q = "SELECT geo_target_constant.id, geo_target_constant.name, \
                 geo_target_constant.canonical_name, geo_target_constant.target_type \
                 FROM geo_target_constant LIMIT 5000";
        let first = match self.call("gaql_search", json!({ "query": q })).await {
            Ok(v) => v,
            Err(e) => return self.fail(s, "gaql_search", &e),
        };
        let n1 = rows(&first).len();
        let Some(cursor) = first["next_cursor"].as_str().map(str::to_string) else {
            return self.fail_msg(
                s,
                "gaql_search",
                format!(
                    "{n1} linhas sem next_cursor (truncated={})",
                    first["truncated"]
                ),
            );
        };
        self.ok(
            s,
            "gaql_search",
            format!("página 1: {n1} linhas, truncated=true"),
        );
        match self
            .call("gaql_search", json!({ "query": q, "cursor": cursor }))
            .await
        {
            Ok(v) => {
                let n2 = rows(&v).len();
                let a = rows(&first)
                    .last()
                    .and_then(|r| at(r, "geoTargetConstant.id"));
                let b = rows(&v).first().and_then(|r| at(r, "geoTargetConstant.id"));
                if n2 > 0 && a != b {
                    self.ok(
                        s,
                        "gaql_search",
                        format!("cursor: {n2} linhas, truncated={}", v["truncated"]),
                    );
                } else {
                    self.fail_msg(
                        s,
                        "gaql_search",
                        format!("cursor: {n2} linhas, repetiu {a:?}"),
                    );
                }
            }
            Err(e) => self.fail(s, "gaql_search", &e),
        }
    }

    // ------------------------------------------------------------ 2. validação

    fn search_input(&self, name: &str, full: bool) -> Value {
        let mut v = json!({
            "name": name,
            "daily_budget": 10,
            "bidding": "manual_cpc",
            "languages": ["1014"],
            "ad_groups": [{
                "name": "Grupo checklist",
                "cpc_bid": 1,
                "keywords": [
                    { "text": "sapato social checklist", "match_type": "PHRASE" },
                    { "text": "sapato couro checklist", "match_type": "EXACT" }
                ],
                "ads": [{
                    "headlines": ["Sapatos sociais", "Sapatos de couro", "Frete grátis"],
                    "descriptions": ["Modelos sociais e casuais.", "Entrega rápida para todo o país."],
                    "final_url": "https://example.com",
                    "path1": "sapatos"
                }]
            }]
        });
        if full {
            v["locations"] = json!(["2076"]);
            v["start_date"] = json!(day(7));
            v["end_date"] = json!(day(60));
            v["search_partners"] = json!(true);
            v["negatives"] = json!([{ "text": "grátis", "match_type": "BROAD" }]);
            v["sitelinks"] = json!([{ "link_text": "Contato", "final_url": "https://example.com/contato",
                                     "description1": "Fale com a gente", "description2": "Atendimento rápido" }]);
            v["ad_groups"][0]["negatives"] = json!([{ "text": "usado", "match_type": "EXACT" }]);
        }
        v
    }

    fn pmax_input(&self, img: &Images, audiences: &[String]) -> Value {
        let mut signals = json!({ "search_themes": ["sapato social", "sapato de couro"] });
        if !audiences.is_empty() {
            signals["audiences"] = json!(audiences);
        }
        json!({
            "name": format!("AdsConsole checklist {} pmax", self.stamp),
            "daily_budget": 20,
            "locations": ["2076"],
            "languages": ["1014"],
            "brand": { "business_name": "Checklist Ltda", "logo": { "file_path": img.logo } },
            "asset_groups": [{
                "name": "Grupo 1",
                "final_urls": ["https://example.com"],
                "headlines": ["Sapatos", "Sapatos de couro legítimo", "Frete grátis"],
                "long_headlines": ["Sapatos de couro com frete grátis para todo o Brasil"],
                "descriptions": ["Compre hoje.", "Modelos sociais e casuais com entrega rápida."],
                "images": {
                    "landscape": [{ "file_path": img.landscape }],
                    "square": [{ "file_path": img.square }]
                },
                "signals": signals
            }]
        })
    }

    fn multi_asset_ad(img: &Images) -> Value {
        json!({ "multi_asset": {
            "headlines": ["Sapatos de couro", "Frete grátis"],
            "descriptions": ["Modelos sociais e casuais com entrega rápida."],
            "business_name": "Checklist Ltda",
            "logo": { "file_path": img.logo },
            "images": {
                "landscape": [{ "file_path": img.landscape }],
                "square": [{ "file_path": img.square }]
            },
            "final_url": "https://example.com",
            "call_to_action": "Comprar agora"
        } })
    }

    fn carousel_ad(img: &Images) -> Value {
        let card = |n: usize| {
            let file = if n == 1 {
                &img.landscape
            } else {
                &img.landscape2
            };
            json!({ "headline": format!("Card {n}"), "image": { "file_path": file },
                    "final_url": format!("https://example.com/{n}"), "call_to_action": "Saber mais" })
        };
        json!({ "carousel": {
            "headline": "Coleção nova",
            "description": "Veja os modelos da estação.",
            "business_name": "Checklist Ltda",
            "logo": { "file_path": img.logo },
            "cards": [card(1), card(2)],
            "final_url": "https://example.com"
        } })
    }

    fn demand_gen_input(&self, img: &Images, ads: Vec<Value>) -> Value {
        json!({
            "name": format!("AdsConsole checklist {} demand gen", self.stamp),
            "daily_budget": 50,
            "ad_groups": [{
                "name": "Grupo 1",
                "locations": ["2076"],
                "languages": ["1014"],
                "ads": if ads.is_empty() { vec![Self::multi_asset_ad(img)] } else { ads }
            }]
        })
    }

    fn display_input(&self, img: &Images) -> Value {
        json!({
            "name": format!("AdsConsole checklist {} display", self.stamp),
            "daily_budget": 10,
            "locations": ["2076"],
            "languages": ["1014"],
            "ad_groups": [{
                "name": "Grupo 1",
                "ads": [{
                    "headlines": ["Sapatos", "Frete grátis"],
                    "long_headline": "Sapatos de couro com frete grátis para todo o Brasil",
                    "descriptions": ["Compre hoje."],
                    "business_name": "Checklist Ltda",
                    "marketing_images": [{ "file_path": img.landscape }],
                    "square_marketing_images": [{ "file_path": img.square }],
                    "final_url": "https://example.com"
                }]
            }]
        })
    }

    async fn validations(&mut self, img: &Images) {
        let s = "validacao";
        let name = format!("AdsConsole checklist {} busca validacao", self.stamp);
        let input = self.search_input(&name, true);
        let r = self
            .check_validate(s, "create_search_campaign", input, &[])
            .await;
        let text = match &r {
            Ok(_) => "validateOnly aceitou startDateTime/endDateTime na criação (com sitelinks, \
                      negativas e critérios sem resourceName)"
                .to_string(),
            Err(e) => err_evidence(e),
        };
        self.evidence("T13.2", text.clone());
        self.evidence("T9.3", format!("criação: {text}"));

        let audiences: Vec<String> = match self
            .gaql("SELECT audience.resource_name FROM audience LIMIT 2")
            .await
        {
            Ok(r) => r
                .iter()
                .filter_map(|x| at(x, "audience.resourceName"))
                .collect(),
            Err(_) => Vec::new(),
        };
        let input = self.pmax_input(img, &audiences);
        let r = self
            .check_validate(
                s,
                "create_pmax_campaign",
                input,
                &[codes::BIDDING_REQUIRES_CONVERSIONS],
            )
            .await;
        let text = match &r {
            Ok(_) => "validateOnly aceitou assetAutomationSettings e brandGuidelinesEnabled".into(),
            Err(e) if e.code == codes::BIDDING_REQUIRES_CONVERSIONS => format!(
                "recusado só por falta de conversões (o JSON foi aceito: nome de campo \
                 desconhecido seria INVALID_ARGUMENT antes): {}",
                err_evidence(e)
            ),
            Err(e) => err_evidence(e),
        };
        self.evidence("T14.1", text.clone());
        self.evidence(
            "T14.3",
            if audiences.is_empty() {
                "não exercitado: a conta não tem públicos (audience)".to_string()
            } else {
                format!(
                    "{} sinais de público no asset group: {text}",
                    audiences.len()
                )
            },
        );

        let input =
            self.demand_gen_input(img, vec![Self::multi_asset_ad(img), Self::carousel_ad(img)]);
        let r = self
            .check_validate(s, "create_demand_gen_campaign", input.clone(), &[])
            .await;
        if r.is_err() {
            self.operations_evidence("create_demand_gen_campaign", input)
                .await;
        }
        let text = match &r {
            Ok(_) => "validateOnly aceitou (multi-asset + carrossel)".to_string(),
            Err(e) => err_evidence(e),
        };
        for item in ["T15.1", "T15.2", "T15.3", "T15.4", "T15.5"] {
            self.evidence(item, format!("validação Demand Gen: {text}"));
        }

        let input = self.display_input(img);
        let r = self
            .check_validate(
                s,
                "create_display_campaign",
                input,
                &[codes::DISPLAY_NOT_AVAILABLE],
            )
            .await;
        let text = match &r {
            Ok(_) => "validateOnly aceitou".to_string(),
            Err(e) => err_evidence(e),
        };
        self.evidence("T15.6", format!("validação Display: {text}"));

        self.validate_asset_group().await;
        self.validate_recommendations().await;
    }

    /// `update_asset_group` (validateOnly) num asset group que já exista na conta.
    async fn validate_asset_group(&mut self) {
        let s = "validacao";
        let tool = "update_asset_group";
        let groups = match self
            .gaql(
                "SELECT asset_group.id, asset_group.resource_name FROM asset_group \
                 WHERE asset_group.status != 'REMOVED' LIMIT 1",
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return self.fail(s, tool, &e),
        };
        let Some(ag) = groups.first().and_then(|r| at(r, "assetGroup.id")) else {
            self.skip(s, tool, "a conta não tem asset group (PMax)");
            self.evidence("T12.1", "não exercitado: a conta não tem asset group");
            return;
        };
        let linked: Vec<String> = self
            .gaql(&format!(
                "SELECT asset_group_asset.asset FROM asset_group_asset WHERE asset_group.id = {ag}"
            ))
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| at(r, "assetGroupAsset.asset"))
            .collect();
        let existing = self
            .gaql("SELECT asset.resource_name, asset.text_asset.text FROM asset WHERE asset.type = 'TEXT' LIMIT 200")
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| at(r, "asset.textAsset.text").is_some_and(|t| t.chars().count() <= 30))
            .filter_map(|r| at(r, "asset.resourceName"))
            .find(|rn| !linked.contains(rn));
        let mut add = vec![
            json!({ "field_type": "HEADLINE", "text": format!("Checklist {}", &self.stamp[..8]) }),
        ];
        if let Some(rn) = &existing {
            add.push(json!({ "field_type": "HEADLINE", "asset": rn }));
        }
        let r = self
            .check_validate(
                s,
                tool,
                json!({ "asset_group_id": ag, "add_assets": add }),
                &[],
            )
            .await;
        let text = match &r {
            Ok(_) => format!(
                "validateOnly aceitou o texto novo (temporário composto){}",
                if existing.is_some() {
                    " e o asset existente sem resourceName"
                } else {
                    "; sem asset de texto existente para testar o link sem resourceName"
                }
            ),
            Err(e) => err_evidence(e),
        };
        self.evidence("T12.1", text);
    }

    /// Planos de apply/dismiss leem `recommendation.dismissed` (sem aplicar nada).
    async fn validate_recommendations(&mut self) {
        let s = "validacao";
        let recs = match self
            .gaql("SELECT recommendation.resource_name FROM recommendation LIMIT 1")
            .await
        {
            Ok(r) => r,
            Err(e) => {
                self.evidence("T12.2", err_evidence(&e));
                return self.fail(s, "dismiss_recommendation", &e);
            }
        };
        let Some(rn) = recs
            .first()
            .and_then(|r| at(r, "recommendation.resourceName"))
        else {
            self.skip(s, "dismiss_recommendation", "a conta não tem recomendações");
            self.skip(s, "apply_recommendation", "a conta não tem recomendações");
            self.evidence("T12.2", "não exercitado: a conta não tem recomendações");
            return;
        };
        for tool in ["dismiss_recommendation", "apply_recommendation"] {
            let r = self
                .check_validate(s, tool, json!({ "recommendation": rn }), &[])
                .await;
            let text = match &r {
                Ok(_) => format!("{tool}: o plano leu recommendation.dismissed sem erro"),
                Err(e) => format!("{tool}: {}", err_evidence(e)),
            };
            self.evidence("T12.2", text);
        }
    }

    // ------------------------------------------------------------ 3-5. Search

    async fn search(&mut self) {
        self.search_steps().await;
        // Falhou no meio: desfaz o que ficou, em ordem inversa.
        self.unwind_to(0).await;
    }

    async fn search_steps(&mut self) {
        let s = "search";
        let name = format!("AdsConsole checklist {} busca", self.stamp);
        let mut input = self.search_input(&name, false);
        // Início no futuro: a edição de start_date depois é válida (e cobre T13.2).
        input["start_date"] = json!(day(7));
        let created = match self
            .write(
                s,
                "create_search_campaign",
                "create_search_campaign",
                input,
                AfterUndo::Nothing,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                self.evidence("T13.1", format!("criação real: {}", err_evidence(&e)));
                self.evidence(
                    "T13.2",
                    format!("criação real com start_date: {}", err_evidence(&e)),
                );
                return;
            }
        };
        let Some(campaign_rn) = find_rn(&created, "campaigns") else {
            return self.fail_msg(
                s,
                "create_search_campaign",
                "sem o nome da campanha no resultado",
            );
        };
        let campaign = last_id(&campaign_rn);
        if let Some(u) = self.undo.last_mut() {
            u.after = AfterUndo::CampaignRemoved(campaign.clone());
        }
        let change_id = created["change_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        self.read(s, "get_change_status", json!({ "change_id": change_id }))
            .await;
        self.read(
            s,
            "get_campaign_details",
            json!({ "campaign_id": campaign }),
        )
        .await;

        let ag = self
            .gaql(&format!(
                "SELECT ad_group.id FROM ad_group WHERE campaign.id = {campaign}"
            ))
            .await
            .unwrap_or_default()
            .first()
            .and_then(|r| at(r, "adGroup.id"));
        let keywords: Vec<String> = match &ag {
            Some(ag) => self
                .gaql(&format!(
                    "SELECT ad_group_criterion.criterion_id FROM ad_group_criterion \
                     WHERE ad_group.id = {ag} AND ad_group_criterion.type = 'KEYWORD' \
                     AND ad_group_criterion.negative = FALSE"
                ))
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(|r| at(r, "adGroupCriterion.criterionId"))
                .collect(),
            None => Vec::new(),
        };
        let ad = match &ag {
            Some(ag) => self
                .gaql(&format!(
                    "SELECT ad_group_ad.ad.id FROM ad_group_ad WHERE ad_group.id = {ag}"
                ))
                .await
                .unwrap_or_default()
                .first()
                .and_then(|r| at(r, "adGroupAd.ad.id")),
            None => None,
        };
        self.gaql_evidence(
            "T13.2",
            "datas da campanha criada",
            &format!("SELECT campaign.start_date_time, campaign.end_date_time FROM campaign WHERE campaign.id = {campaign}"),
        )
        .await;
        self.evidence(
            "T13.1",
            format!(
                "criação real aplicada (com start_date); filhos encontrados: ad_group {ag:?}, {} keywords, anúncio {ad:?}",
                keywords.len()
            ),
        );

        // Edições, cada uma desfeita depois em ordem inversa.
        self.write(
            s,
            "update_campaign",
            "update_campaign nome+orçamento",
            json!({ "campaign_id": campaign, "name": format!("{name} (editada)"), "daily_budget": 12 }),
            AfterUndo::Nothing,
        )
        .await
        .ok();
        let r = self
            .write(
                s,
                "update_campaign",
                "update_campaign end_date",
                json!({ "campaign_id": campaign, "end_date": day(60) }),
                AfterUndo::CampaignDates(campaign.clone()),
            )
            .await;
        if let Err(e) = &r {
            self.evidence("T9.3", format!("update end_date: {}", err_evidence(e)));
        }
        self.gaql_evidence(
            "T9.3",
            "datas depois do update",
            &format!("SELECT campaign.start_date_time, campaign.end_date_time FROM campaign WHERE campaign.id = {campaign}"),
        )
        .await;
        let r = self
            .write(
                s,
                "update_campaign",
                "update_campaign start_date",
                json!({ "campaign_id": campaign, "start_date": day(3) }),
                AfterUndo::CampaignDates(campaign.clone()),
            )
            .await;
        if let Err(e) = &r {
            self.evidence("T9.3", format!("update start_date: {}", err_evidence(e)));
        }

        match &ag {
            Some(ag) => {
                self.write(
                    s,
                    "update_ad_group",
                    "update_ad_group nome+cpc",
                    json!({ "ad_group_id": ag, "name": "Grupo checklist (editado)", "cpc_bid": 2 }),
                    AfterUndo::Nothing,
                )
                .await
                .ok();
            }
            None => self.skip(s, "update_ad_group", "grupo de anúncios não encontrado"),
        }

        match (&ag, &ad) {
            (Some(ag), Some(ad)) => {
                let r = self
                    .write(
                        s,
                        "update_ad",
                        UPDATE_AD_LABEL,
                        json!({ "ad_group_id": ag, "ad_id": ad,
                                "headlines": ["Sapatos sociais", "Sapatos de couro", "Frete grátis", "Checklist"] }),
                        AfterUndo::Nothing,
                    )
                    .await;
                let text = match &r {
                    Ok(_) => {
                        "AdOperation update aplicado; o snapshot e o depois leram FROM ad".into()
                    }
                    Err(e) => err_evidence(e),
                };
                self.evidence("T9.1", text);
                self.gaql_evidence(
                    "T9.2",
                    "títulos lidos FROM ad depois do update",
                    &format!("SELECT ad.responsive_search_ad.headlines FROM ad WHERE ad.id = {ad}"),
                )
                .await;
            }
            _ => {
                self.skip(s, "update_ad", "anúncio não encontrado");
                self.evidence("T9.1", "não exercitado: anúncio não encontrado");
            }
        }

        match &ag {
            Some(ag) => self.keywords(ag, &keywords).await,
            None => self.skip(s, "manage_keywords", "grupo de anúncios não encontrado"),
        }
        self.negatives(&campaign).await;
        self.targeting(&campaign).await;

        // Desfaz as edições (fica só a criação).
        self.unwind_to(1).await;

        self.conflict(&campaign, &name).await;

        // Desfaz a criação: a campanha fica REMOVED.
        self.unwind_to(0).await;
    }

    async fn keywords(&mut self, ag: &str, keywords: &[String]) {
        let s = "search";
        let t = "manage_keywords";
        self.write(
            s,
            t,
            "manage_keywords add",
            json!({ "ad_group_id": ag, "action": "add",
                    "keywords": [{ "text": "sapato checklist novo", "match_type": "BROAD", "cpc_bid": 1 }] }),
            AfterUndo::Nothing,
        )
        .await
        .ok();
        match keywords {
            [first, second, ..] => {
                self.write(
                    s,
                    t,
                    "manage_keywords pause",
                    json!({ "ad_group_id": ag, "action": "pause", "keywords": [{ "criterion_id": first }] }),
                    AfterUndo::Nothing,
                )
                .await
                .ok();
                self.write(
                    s,
                    t,
                    "manage_keywords set_bid",
                    json!({ "ad_group_id": ag, "action": "set_bid",
                            "keywords": [{ "criterion_id": second, "cpc_bid": 3 }] }),
                    AfterUndo::Nothing,
                )
                .await
                .ok();
            }
            _ => self.skip(s, t, "pause/set_bid: keywords da campanha não encontradas"),
        }
    }

    async fn negatives(&mut self, campaign: &str) {
        let s = "search";
        let t = "manage_negatives";
        self.write(
            s,
            t,
            "manage_negatives add",
            json!({ "level": "campaign", "campaign_id": campaign, "action": "add",
                    "keywords": [{ "text": "gratis checklist", "match_type": "PHRASE" }] }),
            AfterUndo::Nothing,
        )
        .await
        .ok();
        let r = self
            .write(
                s,
                t,
                "manage_negatives create_list anexada",
                json!({ "action": "create_list", "campaign_id": campaign,
                        "list_name": format!("checklist {} A", self.stamp),
                        "keywords": [{ "text": "barato checklist", "match_type": "EXACT" }] }),
                AfterUndo::Nothing,
            )
            .await;
        let text = match &r {
            Ok(v) => format!(
                "create_list com campaign_id aplicado: {}",
                resource_keys(v).join(" ")
            ),
            Err(e) => err_evidence(e),
        };
        self.evidence("T10.1", text);
        let r = self
            .write(
                s,
                t,
                "manage_negatives create_list solta",
                json!({ "action": "create_list", "list_name": format!("checklist {} B", self.stamp),
                        "keywords": [{ "text": "usado checklist", "match_type": "PHRASE" }] }),
                AfterUndo::Nothing,
            )
            .await;
        let Some(set) = r.as_ref().ok().and_then(|v| find_rn(v, "sharedSets")) else {
            self.skip(s, t, "attach_list: lista nova não encontrada");
            self.evidence("T10.2", "não exercitado: create_list sem campanha falhou");
            return;
        };
        let r = self
            .write(
                s,
                t,
                "manage_negatives attach_list",
                json!({ "action": "attach_list", "campaign_id": campaign, "shared_set_id": last_id(&set) }),
                AfterUndo::Nothing,
            )
            .await;
        let text = match &r {
            Ok(v) => format!("attach_list aplicado: {}", resource_keys(v).join(" ")),
            Err(e) => err_evidence(e),
        };
        self.evidence("T10.2", text);
    }

    async fn targeting(&mut self, campaign: &str) {
        let s = "search";
        let t = "manage_targeting";
        let r = self
            .write(
                s,
                t,
                "manage_targeting add local+horário",
                json!({ "campaign_id": campaign, "action": "add", "criteria": [
                    { "type": "location", "geo_id": "2076" },
                    { "type": "ad_schedule", "day": "MONDAY", "start_hour": 9, "end_hour": 18 }
                ] }),
                AfterUndo::Nothing,
            )
            .await;
        let location = match &r {
            Ok(_) => self
                .gaql(&format!(
                    "SELECT campaign_criterion.resource_name, campaign_criterion.criterion_id \
                     FROM campaign_criterion WHERE campaign.id = {campaign} \
                     AND campaign_criterion.type = 'LOCATION'"
                ))
                .await
                .unwrap_or_default()
                .first()
                .and_then(|r| at(r, "campaignCriterion.resourceName")),
            Err(_) => None,
        };
        let Some(rn) = location else {
            self.skip(s, t, "set_bid_modifier: critério de local não encontrado");
            self.evidence("T11.1", "não exercitado: critério de local não encontrado");
            return;
        };
        let r = self
            .write(
                s,
                t,
                "manage_targeting set_bid_modifier",
                json!({ "campaign_id": campaign, "action": "set_bid_modifier", "criteria": [
                    { "type": "location", "criterion_id": last_id(&rn), "bid_modifier": 1.2 }
                ] }),
                AfterUndo::CriterionBid(rn.clone()),
            )
            .await;
        if let Err(e) = &r {
            self.evidence("T11.1", format!("set_bid_modifier: {}", err_evidence(e)));
        }
    }

    /// Duas renomeações seguidas; desfazer a primeira tem que dar `UNDO_CONFLICT`.
    async fn conflict(&mut self, campaign: &str, name: &str) {
        let s = "search";
        let base = self.undo.len();
        let a = self
            .write(
                s,
                "update_campaign",
                "update_campaign conflito A",
                json!({ "campaign_id": campaign, "name": format!("{name} conflito A") }),
                AfterUndo::Nothing,
            )
            .await;
        let b = self
            .write(
                s,
                "update_campaign",
                "update_campaign conflito B",
                json!({ "campaign_id": campaign, "name": format!("{name} conflito B") }),
                AfterUndo::Nothing,
            )
            .await;
        let (Ok(a), Ok(_)) = (a, b) else {
            self.skip(s, "conflito", "as renomeações falharam");
            return self.unwind_to(base).await;
        };
        let first = a["change_id"].as_str().unwrap_or_default().to_string();
        match self
            .engine
            .undo(&first, false, Some(REQUESTED_BY.into()))
            .await
        {
            Err(e) if e.code == codes::UNDO_CONFLICT => {
                self.ok(
                    s,
                    "conflito",
                    format!("UNDO_CONFLICT como esperado ({first})"),
                );
            }
            Err(e) => self.fail(s, "conflito", &e),
            Ok(v) => {
                self.fail_msg(
                    s,
                    "conflito",
                    format!("desfez sem conflito: {}", v["change_id"]),
                );
                // A primeira já foi desfeita: sai da pilha.
                self.undo.retain(|u| u.change_id != first);
            }
        }
        self.unwind_to(base).await;
    }

    // ------------------------------------------------------------ 6. Demand Gen e Display

    fn note_images(&mut self, img: &Images, what: &str) {
        for (file, name) in &img.names {
            if !self.irreversible.iter().any(|x| x.contains(name.as_str())) {
                self.irreversible.push(format!(
                    "asset de imagem {name} ({file}, {what}), se criado"
                ));
            }
        }
    }

    async fn image_names_evidence(&mut self, img: &Images, what: &str) {
        let names: Vec<String> = img.names.iter().map(|(_, n)| format!("'{n}'")).collect();
        let q = format!(
            "SELECT asset.resource_name, asset.name, asset.type FROM asset WHERE asset.name IN ({})",
            names.join(", ")
        );
        self.gaql_evidence("T14.2", &format!("assets de imagem depois de {what}"), &q)
            .await;
    }

    async fn demand_gen(&mut self, img: &Images) {
        let s = "demand_gen";
        let input = self.demand_gen_input(img, Vec::new());
        let r = self
            .write(
                s,
                "create_demand_gen_campaign",
                "create_demand_gen_campaign",
                input,
                AfterUndo::Nothing,
            )
            .await;
        self.note_images(img, "Demand Gen");
        let text = match &r {
            Ok(v) => format!("criação real aplicada: {}", resource_keys(v).join(" ")),
            Err(e) => err_evidence(e),
        };
        for item in ["T15.1", "T15.2", "T15.3", "T15.4"] {
            self.evidence(
                item,
                format!("criação real Demand Gen (multi-asset): {text}"),
            );
        }
        if let Ok(v) = &r {
            if let (Some(rn), Some(u)) = (find_rn(v, "campaigns"), self.undo.last_mut()) {
                u.after = AfterUndo::CampaignRemoved(last_id(&rn));
            }
            self.image_names_evidence(img, "Demand Gen").await;
        }
        self.unwind_to(0).await;
    }

    async fn display(&mut self, img: &Images, fresh_seed: u64) {
        let s = "display";
        // Mesmas imagens da Demand Gen: mostra se o Google deduplica pelo conteúdo.
        let input = self.display_input(img);
        let mut r = self
            .write(
                s,
                "create_display_campaign",
                "create_display_campaign",
                input,
                AfterUndo::Nothing,
            )
            .await;
        self.note_images(img, "Display");
        let mut fresh_images: Option<Images> = None;
        if let Err(e) = &r {
            self.evidence(
                "T14.2",
                format!("Display com as mesmas imagens: {}", err_evidence(e)),
            );
            if error_text(e).contains("DUPLICATE") {
                match make_images(fresh_seed) {
                    Ok(fresh) => {
                        let input = self.display_input(&fresh);
                        r = self
                            .write(
                                s,
                                "create_display_campaign",
                                "create_display_campaign (imagens novas)",
                                input,
                                AfterUndo::Nothing,
                            )
                            .await;
                        self.note_images(&fresh, "Display");
                        fresh_images = Some(fresh);
                    }
                    Err(e) => self.fail_msg(s, "create_display_campaign", format!("imagens: {e}")),
                }
            }
        }
        let text = match &r {
            Ok(v) => format!("criação real aplicada: {}", resource_keys(v).join(" ")),
            Err(e) => err_evidence(e),
        };
        self.evidence("T15.6", format!("criação real Display: {text}"));
        if let Ok(v) = &r {
            if let (Some(rn), Some(u)) = (find_rn(v, "campaigns"), self.undo.last_mut()) {
                u.after = AfterUndo::CampaignRemoved(last_id(&rn));
            }
            let used = fresh_images.as_ref().unwrap_or(img);
            self.image_names_evidence(used, "Display").await;
        }
        self.unwind_to(0).await;
    }

    // ------------------------------------------------------------ 7. assets e vídeo

    async fn text_asset(&mut self) {
        let text = format!("Checklist {}", &self.stamp[..8]);
        if let Ok(v) = self
            .write(
                "asset_texto",
                "upload_asset",
                "upload_asset texto",
                json!({ "kind": "text", "text": text }),
                AfterUndo::Nothing,
            )
            .await
        {
            self.irreversible.push(format!(
                "asset de texto {}",
                v["result"]["asset_resource_name"]
            ));
        }
    }

    async fn image_asset(&mut self, file: &Path) {
        if let Ok(v) = self
            .write(
                "imagem",
                "upload_asset",
                "upload_asset imagem",
                json!({ "kind": "image", "image": { "file_path": file.to_string_lossy() } }),
                AfterUndo::Nothing,
            )
            .await
        {
            self.irreversible.push(format!(
                "asset de imagem {}",
                v["result"]["asset_resource_name"]
            ));
        }
    }

    async fn video(&mut self, file: &Path) {
        let s = "video";
        let r = self
            .write(
                s,
                "upload_video",
                "upload_video",
                json!({ "file_path": file.to_string_lossy(),
                        "title": format!("AdsConsole checklist {}", self.stamp),
                        "privacy": "UNLISTED", "wait_minutes": 20 }),
                AfterUndo::Nothing,
            )
            .await;
        match &r {
            Ok(v) => {
                let result = &v["result"];
                println!(
                    "  resultado do upload: {}",
                    short(&result.to_string(), 1500)
                );
                self.evidence(
                    "T16.1",
                    format!(
                        "resultado: {}; reversível {}",
                        short(&result.to_string(), 1500),
                        v["reversible"]
                    ),
                );
                self.evidence(
                    "T16.2",
                    "o início foi aceito: a URL da sessão tinha a mesma origem",
                );
                if let Some(asset) = result["asset_resource_name"].as_str() {
                    self.irreversible.push(format!("asset do vídeo {asset}"));
                }
                let upload = result
                    .as_object()
                    .and_then(|m| m.keys().find(|k| k.starts_with("customers/")).cloned());
                match (upload, self.undo.last_mut()) {
                    (Some(rn), Some(u)) if u.label == "upload_video" => {
                        u.after = AfterUndo::VideoUpload(rn.clone());
                        self.gaql_evidence(
                            "T16.1",
                            "upload lido por GAQL",
                            &format!(
                                "SELECT you_tube_video_upload.resource_name, you_tube_video_upload.state, \
                                 you_tube_video_upload.video_id FROM you_tube_video_upload \
                                 WHERE you_tube_video_upload.resource_name = '{rn}'"
                            ),
                        )
                        .await;
                    }
                    _ => self.left_behind.push(format!(
                        "upload de vídeo sem desfazer: {}",
                        short(&result.to_string(), 300)
                    )),
                }
            }
            Err(e) => {
                self.evidence("T16.1", err_evidence(e));
                self.evidence("T16.2", err_evidence(e));
            }
        }
        self.unwind_to(0).await;
    }

    /// Depois de um Ctrl+C: a escrita em andamento roda numa task do Engine e pode ter
    /// terminado sem entrar na pilha. Põe na pilha as mudanças desta execução aplicadas,
    /// reversíveis e ainda não desfeitas; as que ainda estão em execução vão para a
    /// lista do que ficou.
    fn recover_interrupted(&mut self, db: &Db, started: chrono::DateTime<Utc>) {
        let mut mine = Vec::new();
        let mut offset = 0;
        loop {
            let filter = ChangeFilter {
                platform: Some(Platform::GoogleAds),
                account_id: Some(self.account.clone()),
                limit: 100,
                offset,
                ..ChangeFilter::default()
            };
            let batch = match db.list_changes(&filter) {
                Ok(b) => b,
                Err(e) => {
                    println!("  falha ao ler o histórico: {e}");
                    break;
                }
            };
            if batch.is_empty() {
                break;
            }
            offset += 100;
            let older = batch.iter().any(|ch| ch.created_at < started);
            mine.extend(batch.into_iter().filter(|ch| {
                ch.created_at >= started
                    && ch.requested_by.as_deref() == Some(REQUESTED_BY)
                    && ch.undo_of.is_none()
            }));
            if older {
                break;
            }
        }
        mine.sort_by_key(|ch| ch.created_at);
        for ch in mine {
            if self.undo.iter().any(|u| u.change_id == ch.change_id) {
                continue;
            }
            match ch.status {
                ChangeStatus::Applied if ch.reversible && ch.undone_by.is_none() => {
                    let resources = ch
                        .after
                        .as_ref()
                        .and_then(Value::as_object)
                        .map(|m| {
                            m.keys()
                                .filter(|k| k.starts_with("customers/"))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    self.undo.push(Undoable {
                        change_id: ch.change_id,
                        step: "interrompido".into(),
                        label: ch.tool,
                        resources,
                        after: AfterUndo::Nothing,
                    });
                }
                ChangeStatus::Pending | ChangeStatus::Approved => {
                    self.left_behind.push(format!(
                        "possivelmente aplicado: {} (change_id {}, em execução ao interromper); \
                         procure 'AdsConsole checklist {}' no Google Ads",
                        ch.tool, ch.change_id, self.stamp
                    ));
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ 8. relatório

    fn report(&self, log_dir: &Path) {
        println!("\n## Resultados\n");
        println!("| Resultado | Etapa | Ferramenta | Código | Detalhe |");
        println!("|---|---|---|---|---|");
        for l in &self.lines {
            println!(
                "| {} | {} | {} | {} | {} |",
                l.mark.label(),
                l.step,
                l.tool,
                l.code.as_deref().unwrap_or("-"),
                short(&l.detail, 120).replace('|', "/")
            );
        }

        println!("\n## Por ferramenta\n");
        println!("| Ferramenta | OK | FALHA | PULADO | Códigos |");
        println!("|---|---|---|---|---|");
        let mut by_tool: BTreeMap<&str, (usize, usize, usize, Vec<&str>)> = BTreeMap::new();
        for l in &self.lines {
            let e = by_tool.entry(l.tool.as_str()).or_default();
            match l.mark {
                Mark::Ok => e.0 += 1,
                Mark::Falha => e.1 += 1,
                Mark::Pulado => e.2 += 1,
            }
            if let Some(c) = l.code.as_deref()
                && !e.3.contains(&c)
            {
                e.3.push(c);
            }
        }
        for (tool, (ok, fail, skip, codes)) in by_tool {
            let codes = if codes.is_empty() {
                "-".to_string()
            } else {
                codes.join(", ")
            };
            println!("| {tool} | {ok} | {fail} | {skip} | {codes} |");
        }

        println!("\n## Evidências VERIFY(v26)\n");
        for (item, what) in VERIFY {
            println!("{item} {what}");
            match self.evidence.get(item) {
                Some(list) => {
                    for e in list {
                        println!("  - {e}");
                    }
                }
                None => println!("  - não exercitado nesta execução"),
            }
        }

        println!("\n## O que ficou na conta\n");
        if self.left_behind.is_empty() {
            println!("Nada reversível ficou para trás.");
        } else {
            println!("Não desfeito ou possivelmente aplicado (confira e remova no Google Ads):");
            for l in &self.left_behind {
                println!("  - {l}");
            }
        }
        if !self.irreversible.is_empty() {
            println!("Irreversíveis (ficam na conta por natureza):");
            for l in &self.irreversible {
                println!("  - {l}");
            }
        }
        let (ok, fail) = self.lines.iter().fold((0, 0), |(o, f), l| match l.mark {
            Mark::Ok => (o + 1, f),
            Mark::Falha => (o, f + 1),
            Mark::Pulado => (o, f),
        });
        println!("\nTotal: {ok} OK, {fail} FALHA. Log: {}", log_dir.display());
    }
}

/// A escrita que falhou pode ter chegado ao Google: `EXECUTION_FAILED` (timeout, 5xx
/// no mutate) ou qualquer falha do `upload_video`, que pode ter enviado bytes.
fn maybe_applied(tool: &str, e: &ToolError) -> bool {
    e.code == codes::EXECUTION_FAILED || tool == "upload_video"
}

/// Texto do erro para procurar códigos do Google (mensagem e detalhes).
fn error_text(e: &ToolError) -> String {
    format!(
        "{} {}",
        e.message,
        e.details.as_ref().map(Value::to_string).unwrap_or_default()
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(std::env::args().skip(1))?;
    let env = EnvOverrides::from_env()?;
    let dir = data_dir(&env)?;
    let log_dir = dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let _log = adsconsole_core::logging::init(&log_dir)?;

    let db = Arc::new(Db::open(&dir.join("adsconsole.db"))?);
    let (secrets, _) = open_secret_store(&dir);
    let adapter = GoogleAdapter::new(db.clone(), secrets, GoogleEndpoints::default());
    let mut config = Config::default();
    config.servers.insert(
        Platform::GoogleAds,
        ServerSettings {
            write_mode: WriteMode::Direct,
            ..ServerSettings::default()
        },
    );
    let dyn_adapter: Arc<dyn PlatformAdapter> = adapter.clone();
    let engine = Engine::new(dyn_adapter, db.clone(), Arc::new(RwLock::new(config)))?;
    engine
        .refresh_accounts()
        .await
        .map_err(|e| format!("falha ao listar as contas: {}: {}", e.code, e.message))?;

    let account = normalize_account_id(Platform::GoogleAds, &args.account);
    let connection = resolve_connection(&db, Platform::GoogleAds, &account, None)
        .map_err(|e| format!("conta {account}: {e}"))?;
    let currency = db
        .accounts_for(Platform::GoogleAds, &account)?
        .into_iter()
        .map(|(_, a)| a.currency)
        .next()
        .unwrap_or_default();
    println!("Conta {account} (moeda {currency}), conexão {connection}");
    println!("Diretório de dados: {}", dir.display());

    let planned = planned_writes(&args);
    let go = if planned.is_empty() {
        true
    } else {
        println!("\nEscritas reais planejadas:");
        for p in &planned {
            println!("  - {p}");
        }
        if !args.yes {
            print!(
                "Digite sim para fazer essas escritas (outra resposta roda só leituras e validação): "
            );
            std::io::stdout().flush()?;
        }
        confirmed(&planned, args.yes, &mut std::io::stdin().lock())
    };
    println!();

    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let seed = Utc::now().timestamp().unsigned_abs();
    let needs_images = ["validacao", "demand_gen", "display"]
        .iter()
        .any(|s| args.runs(s));
    let images = if needs_images {
        Some(make_images(seed)?)
    } else {
        None
    };

    let mut c = Checklist {
        engine,
        adapter,
        account,
        connection,
        stamp,
        lines: Vec::new(),
        evidence: BTreeMap::new(),
        undo: Vec::new(),
        left_behind: Vec::new(),
        irreversible: Vec::new(),
    };
    let started = Utc::now();
    let interrupted = tokio::select! {
        () = run_steps(&mut c, &args, images.as_ref(), go, seed) => false,
        r = tokio::signal::ctrl_c() => r.is_ok(),
    };
    if interrupted {
        println!(
            "\nInterrompido (Ctrl+C): desfazendo o que foi criado. Outro Ctrl+C sai sem limpar."
        );
        tokio::spawn(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                println!("\nSaindo sem terminar a limpeza: confira a conta no Google Ads.");
                std::process::exit(130);
            }
        });
        c.recover_interrupted(&db, started);
        c.unwind_to(0).await;
    }
    c.report(&log_dir);
    if interrupted {
        drop(_log);
        std::process::exit(130);
    }
    Ok(())
}

/// As etapas pedidas, na ordem. Corre contra o Ctrl+C em `main`.
async fn run_steps(c: &mut Checklist, args: &Args, images: Option<&Images>, go: bool, seed: u64) {
    for step in STEPS.iter().filter(|s| args.runs(s)) {
        if WRITE_STEPS.contains(step) && !go {
            c.skip(step, "-", "escritas reais não confirmadas");
            continue;
        }
        println!("\n# {step}");
        match *step {
            "leituras" => c.reads().await,
            "validacao" => {
                if let Some(img) = images {
                    c.validations(img).await;
                }
            }
            "search" => c.search().await,
            "demand_gen" => {
                if let Some(img) = images {
                    c.demand_gen(img).await;
                }
            }
            "display" => {
                if let Some(img) = images {
                    c.display(img, seed + 100).await;
                }
            }
            "asset_texto" => c.text_asset().await,
            "imagem" => {
                if let Some(f) = args.image.clone() {
                    c.image_asset(&f).await;
                }
            }
            "video" => {
                if let Some(f) = args.video.clone() {
                    c.video(&f).await;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Result<Args, String> {
        parse_args(s.split_whitespace().map(str::to_string))
    }

    #[test]
    fn parses_account_and_options() {
        let a = args("123-456-7890 --video v.mp4 --only search,video --yes").unwrap();
        assert_eq!(a.account, "123-456-7890");
        assert_eq!(a.video, Some(PathBuf::from("v.mp4")));
        assert_eq!(
            a.only,
            Some(vec!["search".to_string(), "video".to_string()])
        );
        assert!(a.yes);
        assert!(a.runs("search") && a.runs("video"));
        assert!(!a.runs("leituras") && !a.runs("imagem"));
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(args("").is_err());
        assert!(args("1 2").is_err());
        assert!(args("1 --only nada").is_err());
        assert!(args("1 --only video").is_err());
        assert!(args("1 --only imagem").is_err());
        assert!(args("1 --video").is_err());
        assert!(args("1 --force").is_err());
    }

    #[test]
    fn file_steps_need_their_file() {
        let a = args("1").unwrap();
        assert!(!a.runs("video") && !a.runs("imagem"));
        assert!(STEPS[..6].iter().all(|s| a.runs(s)));
        let planned = planned_writes(&a);
        assert_eq!(planned.len(), 4);
        let b = args("1 --image i.png --video v.mp4").unwrap();
        assert_eq!(planned_writes(&b).len(), 6);
        let reads = args("1 --only leituras,validacao").unwrap();
        assert!(planned_writes(&reads).is_empty());
    }

    #[test]
    fn writes_need_sim_unless_yes() {
        let planned = vec!["search".to_string()];
        let ask = |answer: &str, yes: bool| confirmed(&planned, yes, &mut answer.as_bytes());
        assert!(ask("sim\n", false));
        assert!(ask("  SIM  \n", false));
        assert!(!ask("s\n", false));
        assert!(!ask("não\n", false));
        assert!(!ask("", false));
        assert!(ask("", true));
        assert!(confirmed(&[], false, &mut "".as_bytes()));
    }

    #[test]
    fn generated_png_has_dimensions_and_valid_checksums() {
        let bytes = png(1200, 628, 7);
        let img = adsconsole_google_ads::images::decode(bytes.clone()).unwrap();
        assert_eq!(
            (img.width, img.height, img.mime.as_str()),
            (1200, 628, "image/png")
        );
        assert_ne!(png(1200, 628, 8), bytes);
        // Valores de referência conhecidos.
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn possibly_applied_errors() {
        let e = |code: &str| ToolError::new(code, "x");
        assert!(maybe_applied(
            "update_campaign",
            &e(codes::EXECUTION_FAILED)
        ));
        assert!(maybe_applied("upload_video", &e(codes::API_ERROR)));
        assert!(maybe_applied("upload_video", &e(codes::INVALID_INPUT)));
        assert!(!maybe_applied(
            "create_search_campaign",
            &e(codes::API_ERROR)
        ));
        assert!(!maybe_applied("update_campaign", &e(codes::INVALID_INPUT)));
    }

    #[test]
    fn helpers_extract_ids() {
        let body = json!({ "result": {
            "customers/1/campaigns/22": {}, "customers/1/campaignCriteria/22~33": {}, "x": 1 } });
        assert_eq!(
            find_rn(&body, "campaigns").as_deref(),
            Some("customers/1/campaigns/22")
        );
        assert_eq!(last_id("customers/1/campaignCriteria/22~33"), "33");
        assert_eq!(resource_keys(&body).len(), 2);
        assert_eq!(at(&json!({ "a": { "b": 5 } }), "a.b").as_deref(), Some("5"));
        assert_eq!(short("a  b\nc", 10), "a b c");
        assert_eq!(short("abcdef", 3), "abc…");
    }
}
