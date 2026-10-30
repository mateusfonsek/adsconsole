//! Escrita genérica do Google Ads. Cada ferramenta de escrita monta um [`WritePlan`];
//! o adapter valida, tira o snapshot, aplica e lê o estado atual a partir dele.
//!
//! Formato do antes/depois/atual: `{rn: {campo_gaql: valor normalizado}}`. O `target`
//! da inversa é `{"watch": [WatchTarget]}`, com os nomes de recurso já reais.

use std::collections::{HashMap, HashSet};

use adsconsole_core::db::InverseOp;
use adsconsole_google_ads::client::CallKind;
use adsconsole_google_ads::fields;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::ctx::Ctx;
use super::errors::translate;
use super::quota_warning;
use super::tools::assets::VideoUpload;
use crate::adapter::{Applied, Validated};
use crate::error::{ToolError, codes};

/// Ferramentas ocultas usadas como inversa (ver [`super::tools::hidden`]).
pub(crate) const RESTORE_FIELDS: &str = "restore_fields";
pub(crate) const REMOVE_RESOURCES: &str = "remove_resources";
pub(crate) const REMOVE_YOUTUBE_UPLOAD: &str = "remove_youtube_upload";

/// O que uma escrita vai fazer, montado por [`super::tools::WriteTool::plan`].
pub(crate) struct WritePlan {
    pub summary: String,
    pub warning: Option<String>,
    /// `MutateOperation`s do `googleAds:mutate`, na ordem de envio.
    pub operations: Vec<Value>,
    /// Campos alterados em recursos que já existem: viram o snapshot ("antes").
    pub field_changes: Vec<FieldChange>,
    /// O que ler depois da escrita ("depois") e no estado atual do desfazer.
    pub watch: Vec<Watch>,
    pub inverse: InverseSpec,
    /// Quando `Some`, a escrita não usa o `googleAds:mutate`: o `validate` só faz as
    /// checagens locais e o `execute` roda isto.
    pub direct: Option<DirectExec>,
}

pub(crate) struct FieldChange {
    pub resource_name: String,
    /// Campo GAQL da tabela [`fields::FIELDS`].
    pub field: &'static str,
    /// Valor novo, já normalizado (o formato de [`fields::normalize`]).
    pub value: Value,
}

pub(crate) struct Watch {
    /// Pode ser temporário (criado nesta escrita); o execute troca pelo real.
    pub resource_name: String,
    /// Recurso GAQL, ex.: `campaign`.
    pub resource: &'static str,
    /// Campos GAQL completos, ex.: `campaign.status`.
    pub fields: Vec<&'static str>,
}

pub(crate) enum InverseSpec {
    /// Volta os campos de `field_changes` para o snapshot (`restore_fields`).
    RestoreFields,
    /// Remove os recursos criados, nesta ordem (`remove_resources`). Os nomes podem
    /// ser temporários.
    RemoveCreated {
        order: Vec<String>,
    },
    None,
}

pub(crate) enum DirectExec {
    /// POST em `customers/{c}{path_suffix}`, contado como uma operação de mutate.
    Post { path_suffix: String, body: Value },
    /// Upload de vídeo para o YouTube e, quando processado, o asset do vídeo. O nome
    /// do upload só existe depois do envio, então a execução
    /// ([`super::tools::assets::run_upload`]) monta o "depois", o alvo e a inversa
    /// sozinha: `watch` e `inverse` do plano ficam vazios.
    YoutubeUpload(Box<VideoUpload>),
}

/// [`Watch`] resolvido, como fica gravado no `target` da inversa.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WatchTarget {
    pub resource_name: String,
    pub resource: String,
    pub fields: Vec<String>,
}

fn internal_plan(msg: impl std::fmt::Display) -> ToolError {
    ToolError::new(codes::INTERNAL, format!("Plano de escrita inválido: {msg}"))
}

/// `customers/{c}/{colecao}/{id}` cujo id (ou parte de um id composto `a~b`) é negativo.
fn is_temp(rn: &str) -> bool {
    let parts: Vec<&str> = rn.splitn(4, '/').collect();
    matches!(parts.as_slice(), ["customers", _, _, id] if id.split('~').any(|p| p.starts_with('-')))
}

/// Nomes temporários criados pelas operações (`<x>Operation.create.resourceName`).
fn created_name(op: &Value) -> Option<&str> {
    op.as_object()?
        .values()
        .next()?
        .get("create")?
        .get("resourceName")?
        .as_str()
}

/// Operações como vão para a API. O plano guarda o nome temporário de todo recurso
/// criado (para o desfazer e a releitura), mas vão sem `resourceName`:
/// - `campaignSharedSet`: o `-N` do nome repete o do `sharedSet` (`DUPLICATE_TEMP_IDS`);
/// - critério de campanha ou grupo que não é keyword (local, idioma, horário, …): o id
///   vem do conteúdo e o `-N` do nome dá `FIELD_MISMATCH`;
/// - `assetGroupAsset` e `assetGroupSignal`: o id é derivado do asset (ou do conteúdo),
///   como na criação do PMax, que passou na conta real.
///
/// O nome real volta na resposta, que casa com a operação pela posição ([`temp_map`]).
fn wire_ops(operations: &[Value]) -> Vec<Value> {
    operations
        .iter()
        .map(|op| {
            let mut op = op.clone();
            let collection = created_name(&op)
                .filter(|n| is_temp(n))
                .and_then(|n| n.split('/').nth(2))
                .map(str::to_string);
            let Some(collection) = collection else {
                return op;
            };
            if let Some(create) = op
                .as_object_mut()
                .and_then(|m| m.values_mut().next())
                .and_then(|inner| inner.get_mut("create"))
                .and_then(Value::as_object_mut)
            {
                let derived = matches!(collection.as_str(), "campaignCriteria" | "adGroupCriteria")
                    && !create.contains_key("keyword");
                let derived_name = matches!(
                    collection.as_str(),
                    "campaignSharedSets" | "assetGroupAssets" | "assetGroupSignals"
                );
                if derived_name || derived {
                    create.remove("resourceName");
                }
            }
            op
        })
        .collect()
}

/// Resumo legível das operações, sem os bytes das imagens: índice, tipo, nome (ou
/// temporário) do que cria e cada referência a `customers/...`, dizendo se aponta para
/// um recurso existente, para um temporário criado antes (e em que operação) ou para um
/// que não existe. O Google trata operações seguidas do mesmo tipo como um lote e não
/// deixa um lote referenciar temporários criados nele mesmo; essas referências saem
/// marcadas com `!! MESMO LOTE`.
pub fn describe_operations(operations: &[Value]) -> Vec<String> {
    // temporário -> (índice da operação, lote)
    let mut created: HashMap<String, (usize, usize)> = HashMap::new();
    let mut batch = 0;
    let mut last_key = "";
    let mut out = Vec::new();
    for (i, op) in operations.iter().enumerate() {
        let Some((key, inner)) = op.as_object().and_then(|m| m.iter().next()) else {
            out.push(format!("#{i} operação inválida"));
            continue;
        };
        if key != last_key {
            batch += 1;
            last_key = key;
        }
        let kind = ["create", "update", "remove"]
            .into_iter()
            .find(|k| inner.get(k).is_some())
            .unwrap_or("?");
        let body = &inner[kind];
        let own = body
            .get("resourceName")
            .and_then(Value::as_str)
            .or_else(|| body.as_str())
            .unwrap_or("(sem resourceName)");
        let mut refs = Vec::new();
        if let Some(m) = body.as_object() {
            for (k, v) in m.iter().filter(|(k, _)| *k != "resourceName") {
                collect_refs(k, v, &created, batch, &mut refs);
            }
        }
        let mut line = format!("#{i} lote {batch} {key} {kind} {own}");
        if !refs.is_empty() {
            line.push_str(&format!(" | refs: {}", refs.join("; ")));
        }
        out.push(line);
        if kind == "create"
            && let Some(n) = created_name(op).filter(|n| is_temp(n))
        {
            created.insert(n.to_string(), (i, batch));
        }
    }
    out
}

fn collect_refs(
    path: &str,
    v: &Value,
    created: &HashMap<String, (usize, usize)>,
    batch: usize,
    out: &mut Vec<String>,
) {
    match v {
        Value::String(s) if s.starts_with("customers/") => {
            let what = if !is_temp(s) {
                "existente".to_string()
            } else {
                match created.get(s) {
                    Some((at, b)) if *b == batch => format!("criado em #{at} !! MESMO LOTE"),
                    Some((at, _)) => format!("criado em #{at}"),
                    None => "!! TEMPORÁRIO SEM CRIAÇÃO ANTERIOR".to_string(),
                }
            };
            out.push(format!("{path}={s} ({what})"));
        }
        Value::Object(m) => m
            .iter()
            .for_each(|(k, x)| collect_refs(&format!("{path}.{k}"), x, created, batch, out)),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .for_each(|(k, x)| collect_refs(&format!("{path}[{k}]"), x, created, batch, out)),
        _ => {}
    }
}

/// `mutateOperationResponses[i]` corresponde a `operations[i]`.
fn temp_map(operations: &[Value], responses: &[Value]) -> HashMap<String, String> {
    operations
        .iter()
        .zip(responses)
        .filter_map(|(op, resp)| {
            let temp = created_name(op).filter(|n| is_temp(n))?;
            let real = resp
                .as_object()?
                .iter()
                .find(|(k, _)| k.ends_with("Result"))?
                .1
                .get("resourceName")?
                .as_str()?;
            Some((temp.to_string(), real.to_string()))
        })
        .collect()
}

/// Recurso e campos GAQL entram na query: só `[a-z0-9_.]`, e o campo é do recurso.
fn check_watch_target(w: &WatchTarget) -> Result<(), ToolError> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
    };
    let prefix = format!("{}.", w.resource);
    if !ident(&w.resource)
        || w.resource.contains('.')
        || w.fields.is_empty()
        || !w.fields.iter().all(|f| ident(f) && f.starts_with(&prefix))
    {
        return Err(ToolError::new(
            codes::INTERNAL,
            format!("Alvo de leitura inválido: {} {:?}", w.resource, w.fields),
        ));
    }
    Ok(())
}

/// Checagens locais do plano, antes de qualquer chamada que escreve.
fn check(plan: &WritePlan) -> Result<(), ToolError> {
    if plan.direct.is_none() && plan.operations.is_empty() {
        return Err(ToolError::new(
            codes::INVALID_INPUT,
            "Nenhuma operação para enviar.",
        ));
    }
    for c in &plan.field_changes {
        if fields::find(c.field).is_none() {
            return Err(internal_plan(format!("campo fora da tabela: {}", c.field)));
        }
        // O snapshot lê o "antes": só existe para recursos que já existem.
        if is_temp(&c.resource_name) {
            return Err(internal_plan(format!(
                "campo alterado em recurso temporário: {}",
                c.resource_name
            )));
        }
    }
    let created: HashSet<&str> = if plan.direct.is_some() {
        HashSet::new()
    } else {
        plan.operations.iter().filter_map(created_name).collect()
    };
    let mut refs: Vec<&str> = Vec::new();
    for w in &plan.watch {
        check_watch_target(&WatchTarget {
            resource_name: w.resource_name.clone(),
            resource: w.resource.to_string(),
            fields: w.fields.iter().map(|f| f.to_string()).collect(),
        })?;
        refs.push(&w.resource_name);
    }
    match &plan.inverse {
        InverseSpec::RemoveCreated { order } => refs.extend(order.iter().map(String::as_str)),
        InverseSpec::RestoreFields | InverseSpec::None => {}
    }
    if let Some(t) = refs
        .into_iter()
        .find(|r| is_temp(r) && !created.contains(r))
    {
        return Err(internal_plan(format!("id temporário sem criação: {t}")));
    }
    Ok(())
}

fn join_warnings(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a} {b}")),
        (a, b) => a.or(b),
    }
}

/// Regras locais e `validateOnly: true` (exceto em [`DirectExec`]), com o aviso de cota.
pub(crate) async fn validate(
    ctx: &Ctx,
    tool: &str,
    plan: &WritePlan,
) -> Result<Validated, ToolError> {
    check(plan)?;
    if plan.direct.is_none() {
        ctx.client
            .mutate(
                &ctx.customer_id,
                ctx.login.as_deref(),
                &wire_ops(&plan.operations),
                true,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
    }
    Ok(Validated {
        summary: plan.summary.clone(),
        warning: join_warnings(plan.warning.clone(), quota_warning(ctx)),
    })
}

/// Valores atuais dos campos de `field_changes`, agrupados por recurso; `{}` sem campos.
pub(crate) async fn snapshot(ctx: &Ctx, tool: &str, plan: &WritePlan) -> Result<Value, ToolError> {
    let mut targets: Vec<WatchTarget> = Vec::new();
    for c in &plan.field_changes {
        let field =
            fields::find(c.field).ok_or_else(|| internal_plan(format!("campo {}", c.field)))?;
        match targets
            .iter_mut()
            .find(|t| t.resource_name == c.resource_name)
        {
            Some(t) if !t.fields.iter().any(|f| f == c.field) => t.fields.push(c.field.into()),
            Some(_) => {}
            None => targets.push(WatchTarget {
                resource_name: c.resource_name.clone(),
                resource: field.resource.into(),
                fields: vec![c.field.into()],
            }),
        }
    }
    read_targets(ctx, tool, &targets).await
}

/// Lê um campo de uma linha do search: pela tabela de campos editáveis quando ele
/// está lá; senão, seguindo o caminho GAQL em camelCase.
fn read_field(gaql: &str, row: &Value) -> Value {
    if let Some(f) = fields::find(gaql) {
        return fields::normalize(f, row);
    }
    gaql.split('.')
        .try_fold(row, |cur, seg| cur.get(fields::camel(seg)))
        .cloned()
        .unwrap_or(Value::Null)
}

/// `{rn: {campo: valor}}` dos alvos. Recurso que não existe mais lê tudo `null`.
pub(crate) async fn read_targets(
    ctx: &Ctx,
    tool: &str,
    targets: &[WatchTarget],
) -> Result<Value, ToolError> {
    let mut out = Map::new();
    for t in targets {
        check_watch_target(t)?;
        let names: Vec<&str> = t.fields.iter().map(String::as_str).collect();
        let query = fields::read_query(&t.resource, &t.resource_name, &names);
        let page = ctx
            .client
            .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
        let row = page.results.first();
        let entry = out
            .entry(t.resource_name.clone())
            .or_insert_with(|| json!({}));
        if let Value::Object(m) = entry {
            for f in &t.fields {
                m.insert(f.clone(), row.map_or(Value::Null, |r| read_field(f, r)));
            }
        }
    }
    Ok(Value::Object(out))
}

/// Nome real de `name`; `Err(name)` se é temporário e a resposta não trouxe o real.
fn resolve(name: &str, temps: &HashMap<String, String>) -> Result<String, String> {
    if !is_temp(name) {
        return Ok(name.to_string());
    }
    temps.get(name).cloned().ok_or_else(|| name.to_string())
}

/// Corpo do `create` da operação que cria `name`.
fn create_body<'a>(operations: &'a [Value], name: &str) -> Option<&'a Value> {
    operations
        .iter()
        .find(|op| created_name(op) == Some(name))?
        .as_object()?
        .values()
        .next()?
        .get("create")
}

/// "Depois" quando a releitura falha, com o que já se sabe de cada campo observado:
/// - recurso criado nesta escrita: o valor no corpo do `create` (ex.: `status: PAUSED`,
///   `name`), normalizado como na leitura;
/// - recurso existente: o valor novo de `field_changes`.
///
/// Campo sem valor conhecido fica de fora; o desfazer só compara as chaves presentes.
/// `watch[i]` corresponde a `targets[i]` (mesmo alvo, nome já resolvido).
fn expected_after(plan: &WritePlan, targets: &[WatchTarget]) -> Value {
    let mut out = Map::new();
    for (w, t) in plan.watch.iter().zip(targets) {
        let mut known = Map::new();
        if let Some(body) = create_body(&plan.operations, &w.resource_name) {
            let row = json!({ fields::camel(w.resource): body });
            for f in &t.fields {
                let v = read_field(f, &row);
                if !v.is_null() {
                    known.insert(f.clone(), v);
                }
            }
        }
        for c in &plan.field_changes {
            if c.resource_name == t.resource_name && t.fields.iter().any(|f| f == c.field) {
                known.insert(c.field.to_string(), c.value.clone());
            }
        }
        if !known.is_empty() {
            let entry = out
                .entry(t.resource_name.clone())
                .or_insert_with(|| json!({}));
            if let Value::Object(m) = entry {
                m.extend(known);
            }
        }
    }
    Value::Object(out)
}

/// Alvos e inversa com os nomes reais.
type Resolved = (Vec<WatchTarget>, Option<(String, Value)>);

fn resolve_plan(
    plan: &WritePlan,
    before: &Value,
    temps: &HashMap<String, String>,
) -> Result<Resolved, String> {
    let targets = plan
        .watch
        .iter()
        .map(|w| {
            Ok(WatchTarget {
                resource_name: resolve(&w.resource_name, temps)?,
                resource: w.resource.to_string(),
                fields: w.fields.iter().map(|f| f.to_string()).collect(),
            })
        })
        .collect::<Result<_, String>>()?;
    let inverse = match &plan.inverse {
        InverseSpec::RestoreFields => {
            Some((RESTORE_FIELDS.to_string(), json!({ "updates": before })))
        }
        InverseSpec::RemoveCreated { order } => {
            let names = order
                .iter()
                .map(|n| resolve(n, temps))
                .collect::<Result<Vec<_>, _>>()?;
            Some((
                REMOVE_RESOURCES.to_string(),
                json!({ "resource_names": names }),
            ))
        }
        InverseSpec::None => None,
    };
    Ok((targets, inverse))
}

/// Aplica (mutate atômico ou [`DirectExec`]), resolve os nomes temporários, relê o
/// `watch` para o "depois" e monta a inversa.
///
/// Depois que o Google aplicou, nunca devolve `Err` (o Engine gravaria `Failed`):
/// - releitura falhou: "depois" de [`expected_after`], inversa mantida;
/// - resposta sem o nome real de um temporário referenciado: `after = {}` e sem
///   inversa (não reversível), com `tracing::error`.
pub(crate) async fn execute(
    ctx: &Ctx,
    tool: &str,
    plan: &WritePlan,
    before: &Value,
) -> Result<Applied, ToolError> {
    check(plan)?;
    if matches!(plan.inverse, InverseSpec::RestoreFields)
        && before.as_object().is_none_or(|m| m.is_empty())
    {
        return Err(internal_plan(
            "RestoreFields sem snapshot (antes vazio ou inválido)",
        ));
    }
    let responses = match &plan.direct {
        Some(DirectExec::YoutubeUpload(v)) => {
            return super::tools::assets::run_upload(ctx, tool, v).await;
        }
        Some(DirectExec::Post { path_suffix, body }) => {
            ctx.client
                .post(
                    &ctx.customer_id,
                    ctx.login.as_deref(),
                    path_suffix,
                    body,
                    CallKind::Mutate { ops: 1 },
                )
                .await
                .map_err(|e| translate(e, Some(ctx), tool))?;
            Vec::new()
        }
        None => ctx
            .client
            .mutate(
                &ctx.customer_id,
                ctx.login.as_deref(),
                &wire_ops(&plan.operations),
                false,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?,
    };
    let temps = temp_map(&plan.operations, &responses);
    let (targets, inverse) = match resolve_plan(plan, before, &temps) {
        Ok(r) => r,
        Err(missing) => {
            tracing::error!(
                tool,
                temp = missing.as_str(),
                "escrita aplicada, mas a resposta não trouxe o nome real; registrada sem desfazer"
            );
            return Ok(Applied {
                after: json!({}),
                inverse: None,
            });
        }
    };
    let after = match read_targets(ctx, tool, &targets).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(tool, error = %e.message, "falha ao reler depois da escrita");
            expected_after(plan, &targets)
        }
    };
    let target = json!({ "watch": targets });
    Ok(Applied {
        after,
        inverse: inverse.map(|(tool, input)| InverseOp {
            tool,
            input,
            target,
        }),
    })
}

/// Chave do `target` com valores fixos do "depois" que não são relidos (ex.: o
/// `video_id` do upload); o estado atual os repete como estão.
pub(crate) const TARGET_EXTRA: &str = "extra";

/// Estado atual de `target.watch`, no mesmo formato do "depois", mais as chaves de
/// `target.extra`.
pub(crate) async fn current_state(
    ctx: &Ctx,
    tool: &str,
    target: &Value,
) -> Result<Value, ToolError> {
    let targets: Vec<WatchTarget> = match target.get("watch") {
        Some(w) => serde_json::from_value(w.clone()).map_err(|e| {
            ToolError::new(codes::INTERNAL, format!("Alvo do desfazer inválido: {e}"))
        })?,
        None => Vec::new(),
    };
    let mut out = read_targets(ctx, tool, &targets).await?;
    if let (Some(Value::Object(extra)), Value::Object(m)) = (target.get(TARGET_EXTRA), &mut out) {
        for (k, v) in extra {
            m.insert(k.clone(), v.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adsconsole_core::Platform;
    use adsconsole_core::config::ToolGroup;
    use adsconsole_core::db::{AccountInfo, Db};
    use adsconsole_core::secrets::{MemoryStore, Secret};
    use adsconsole_google_ads::client_json::ClientCreds;
    use adsconsole_google_ads::endpoints::GoogleEndpoints;
    use adsconsole_google_ads::resources::TempIds;
    use adsconsole_google_ads::retry::RetryPolicy;
    use async_trait::async_trait;
    use chrono::Utc;
    use schemars::JsonSchema;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::adapter::{Call, PlatformAdapter, ToolSpec};
    use crate::google::GoogleAdapter;
    use crate::google::connections::{AccessLevel, save_connection};
    use crate::google::tools::{GTool, WriteTool};

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct NoInput {}

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Ok,
        /// Observa um temporário que nenhuma operação cria.
        Dangling,
        /// `field_changes` num recurso temporário.
        TempFieldChange,
    }

    /// Cria orçamento (-1) e campanha (-2, com nome e PAUSED) e observa a campanha.
    struct FakeCreate(Mode);

    #[async_trait]
    impl WriteTool for FakeCreate {
        fn spec(&self) -> ToolSpec {
            ToolSpec::write::<NoInput>("fake_create", "Cria.", ToolGroup::Create, true)
        }
        async fn plan(&self, ctx: &Ctx, _input: &Value) -> Result<WritePlan, ToolError> {
            let mut ids = TempIds::new(&ctx.customer_id);
            let budget = ids.next("campaignBudgets");
            let campaign = ids.next("campaigns");
            let watched = if self.0 == Mode::Dangling {
                ids.next("campaigns")
            } else {
                campaign.clone()
            };
            let field_changes = if self.0 == Mode::TempFieldChange {
                vec![FieldChange {
                    resource_name: campaign.clone(),
                    field: "campaign.name",
                    value: json!("x"),
                }]
            } else {
                Vec::new()
            };
            Ok(WritePlan {
                summary: "Cria campanha.".into(),
                warning: None,
                operations: vec![
                    json!({ "campaignBudgetOperation": { "create": {
                        "resourceName": budget, "amountMicros": "1000000" } } }),
                    json!({ "campaignOperation": { "create": {
                        "resourceName": campaign, "campaignBudget": budget,
                        "name": "Nova", "status": "PAUSED" } } }),
                ],
                field_changes,
                watch: vec![Watch {
                    resource_name: watched,
                    resource: "campaign",
                    fields: vec!["campaign.status", "campaign.name", "campaign.end_date_time"],
                }],
                inverse: InverseSpec::RemoveCreated {
                    order: vec![campaign, budget],
                },
                direct: None,
            })
        }
    }

    /// Escrita fora do mutate.
    struct FakeDirect;

    #[async_trait]
    impl WriteTool for FakeDirect {
        fn spec(&self) -> ToolSpec {
            ToolSpec::write::<NoInput>("fake_direct", "Direto.", ToolGroup::Edit, true)
        }
        async fn plan(&self, _ctx: &Ctx, _input: &Value) -> Result<WritePlan, ToolError> {
            Ok(WritePlan {
                summary: "Direto.".into(),
                warning: Some("Aviso do plano.".into()),
                operations: Vec::new(),
                field_changes: Vec::new(),
                watch: Vec::new(),
                inverse: InverseSpec::None,
                direct: Some(DirectExec::Post {
                    path_suffix: "/thing:do".into(),
                    body: json!({ "a": 1 }),
                }),
            })
        }
    }

    async fn setup(tool: impl WriteTool + 'static) -> (MockServer, Arc<GoogleAdapter>) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "access_token": "tok", "expires_in": 3600 })),
            )
            .mount(&server)
            .await;
        let db = Arc::new(Db::open_in_memory().unwrap());
        let endpoints = GoogleEndpoints {
            api_base: format!("{}/v26", server.uri()),
            upload_base: format!("{}/upload", server.uri()),
            auth_url: "https://accounts.example/auth".into(),
            token_url: format!("{}/token", server.uri()),
        };
        let adapter = GoogleAdapter::with_tools(
            db.clone(),
            Arc::new(MemoryStore::default()),
            endpoints,
            RetryPolicy {
                base: std::time::Duration::from_millis(1),
            },
            vec![GTool::Write(Box::new(tool))],
        );
        let creds = ClientCreds {
            client_id: "cid".into(),
            client_secret: Secret::new("s"),
            project_id: "p".into(),
        };
        save_connection(
            &adapter,
            "g1",
            "G1",
            &creds,
            &Secret::new("r"),
            None,
            AccessLevel::Explorer,
        )
        .unwrap();
        let acct = AccountInfo {
            account_id: "111".into(),
            name: "Conta".into(),
            currency: "BRL".into(),
            timezone: None,
            status: Some("ENABLED".into()),
            manager_id: None,
        };
        db.replace_accounts(Platform::GoogleAds, "g1", &[acct])
            .unwrap();
        db.record_google_ops("p", 0, Utc::now()).unwrap();
        (server, adapter)
    }

    fn fake_call() -> Call {
        Call {
            connection_id: "g1".into(),
            account_id: "111".into(),
            tool: "fake_create".into(),
            input: json!({}),
        }
    }

    async fn mock_mutate(server: &MockServer, responses: Value) {
        Mock::given(method("POST"))
            .and(path("/v26/customers/111/googleAds:mutate"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "mutateOperationResponses": responses })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn remove_created_resolves_temp_ids() {
        let (server, adapter) = setup(FakeCreate(Mode::Ok)).await;
        mock_mutate(
            &server,
            json!([
                { "campaignBudgetResult": { "resourceName": "customers/111/campaignBudgets/77" } },
                { "campaignResult": { "resourceName": "customers/111/campaigns/88" } }
            ]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v26/customers/111/googleAds:search"))
            .and(body_string_contains("customers/111/campaigns/88"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "results": [
                    { "campaign": { "resourceName": "customers/111/campaigns/88",
                        "status": "PAUSED", "name": "Nova" } }
                ]})),
            )
            .mount(&server)
            .await;

        let c = fake_call();
        adapter.validate(&c).await.unwrap();
        let before = adapter.snapshot(&c).await.unwrap();
        assert_eq!(before, json!({}));
        let applied = adapter.execute(&c, &before).await.unwrap();

        let rn = "customers/111/campaigns/88";
        assert_eq!(
            applied.after,
            json!({ rn: {
                "campaign.status": "PAUSED", "campaign.name": "Nova", "campaign.end_date_time": null
            } })
        );
        let inverse = applied.inverse.unwrap();
        assert_eq!(inverse.tool, REMOVE_RESOURCES);
        assert_eq!(
            inverse.input,
            json!({ "resource_names": [rn, "customers/111/campaignBudgets/77"] })
        );
        assert_eq!(
            inverse.target,
            json!({ "watch": [{
                "resource_name": rn, "resource": "campaign",
                "fields": ["campaign.status", "campaign.name", "campaign.end_date_time"]
            }]})
        );
        let current = adapter
            .current_state("g1", "111", &inverse.target)
            .await
            .unwrap();
        assert_eq!(current, applied.after);
        // Nenhuma leitura com o id temporário.
        let reqs = server.received_requests().await.unwrap();
        assert!(
            reqs.iter()
                .filter(|r| r.url.path().ends_with(":search"))
                .all(|r| !String::from_utf8_lossy(&r.body).contains("/-"))
        );
    }

    #[tokio::test]
    async fn dangling_temp_fails_before_any_request() {
        let (server, adapter) = setup(FakeCreate(Mode::Dangling)).await;
        mock_mutate(&server, json!([])).await;
        let err = adapter.validate(&fake_call()).await.unwrap_err();
        assert_eq!(err.code, codes::INTERNAL);
        let err = adapter.execute(&fake_call(), &json!({})).await.unwrap_err();
        assert_eq!(err.code, codes::INTERNAL);
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.iter().all(|r| !r.url.path().ends_with(":mutate")));
    }

    #[tokio::test]
    async fn missing_real_name_is_applied_without_undo() {
        let (server, adapter) = setup(FakeCreate(Mode::Ok)).await;
        // Resposta sem os resultados: não há como saber o nome real.
        mock_mutate(&server, json!([])).await;
        let applied = adapter.execute(&fake_call(), &json!({})).await.unwrap();
        assert_eq!(applied.after, json!({}));
        assert!(applied.inverse.is_none());
        // Nenhuma leitura com o id temporário.
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.iter().all(|r| !r.url.path().ends_with(":search")));
    }

    #[tokio::test]
    async fn reread_failure_keeps_inverse_and_known_values() {
        let (server, adapter) = setup(FakeCreate(Mode::Ok)).await;
        mock_mutate(
            &server,
            json!([
                { "campaignBudgetResult": { "resourceName": "customers/111/campaignBudgets/77" } },
                { "campaignResult": { "resourceName": "customers/111/campaigns/88" } }
            ]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v26/customers/111/googleAds:search"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let applied = adapter.execute(&fake_call(), &json!({})).await.unwrap();
        let rn = "customers/111/campaigns/88";
        // end_date não está no create: fica de fora.
        assert_eq!(
            applied.after,
            json!({ rn: { "campaign.status": "PAUSED", "campaign.name": "Nova" } })
        );
        let inverse = applied.inverse.unwrap();
        assert_eq!(inverse.tool, REMOVE_RESOURCES);
        assert_eq!(
            inverse.input,
            json!({ "resource_names": [rn, "customers/111/campaignBudgets/77"] })
        );
    }

    #[tokio::test]
    async fn temp_field_change_is_rejected() {
        let (server, adapter) = setup(FakeCreate(Mode::TempFieldChange)).await;
        mock_mutate(&server, json!([])).await;
        let err = adapter.validate(&fake_call()).await.unwrap_err();
        assert_eq!(err.code, codes::INTERNAL);
        assert!(err.message.contains("temporário"), "{}", err.message);
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| !r.url.path().ends_with(":mutate"))
        );
    }

    #[tokio::test]
    async fn direct_post_skips_validate_only_and_counts_one_op() {
        let (server, adapter) = setup(FakeDirect).await;
        Mock::given(method("POST"))
            .and(path("/v26/customers/111/thing:do"))
            .and(body_string_contains("\"a\":1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let c = Call {
            tool: "fake_direct".into(),
            ..fake_call()
        };
        let v = adapter.validate(&c).await.unwrap();
        assert_eq!(v.warning.as_deref(), Some("Aviso do plano."));
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| !r.url.path().starts_with("/v26"))
        );
        let applied = adapter.execute(&c, &json!({})).await.unwrap();
        assert_eq!(applied.after, json!({}));
        assert!(applied.inverse.is_none());
        assert_eq!(
            adapter.db().google_ops_last_24h("p", Utc::now()).unwrap(),
            1
        );
    }

    #[test]
    fn temp_detection() {
        assert!(is_temp("customers/1/campaigns/-2"));
        assert!(is_temp("customers/1/adGroupCriteria/-3~44"));
        assert!(!is_temp("customers/1/campaigns/5"));
        assert!(!is_temp("customers/1/adGroupCriteria/3~44"));
        assert!(!is_temp("-1"));
    }
}
