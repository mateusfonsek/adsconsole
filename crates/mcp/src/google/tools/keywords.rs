//! Keywords e negativas: `manage_keywords` e `manage_negatives`.
//!
//! As criações usam ids temporários (`{pai}~-N`) e têm como inversa `RemoveCreated`;
//! pausar, ativar e mudar lance são `FieldChange` com `RestoreFields`; remover e trocar
//! o tipo de correspondência são irreversíveis (`InverseSpec::None`).
//!
//! Os construtores de critério (`criterion_temp`, `create_op`, `keyword_body`,
//! `keyword_text`) são `pub(crate)` para a segmentação (Task 11) e as criações (Task 13).

use adsconsole_core::config::ToolGroup;
use adsconsole_core::money::Decimal;
use adsconsole_google_ads::fields;
use adsconsole_google_ads::resources::{self, TempIds};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::edit::{EditStatus, Edits, Show, positive_micros};
use super::reports::{at, invalid, text, valid_id};
use super::{GTool, WriteTool, parse_input};
use crate::adapter::{AccountRef, ToolSpec};
use crate::engine::internal;
use crate::error::{ToolError, codes};
use crate::google::ctx::Ctx;
use crate::google::errors::translate;
use crate::google::plan::{InverseSpec, Watch, WritePlan};

pub(crate) fn tools() -> Vec<GTool> {
    vec![
        GTool::Write(Box::new(ManageKeywords)),
        GTool::Write(Box::new(ManageNegatives)),
    ]
}

const MANAGE_KEYWORDS: &str = "manage_keywords";
const MANAGE_NEGATIVES: &str = "manage_negatives";

const MAX_KEYWORDS: usize = 200;
const MAX_TEXT_CHARS: usize = 80;
const MAX_TEXT_WORDS: usize = 10;
const MAX_LIST_NAME_CHARS: usize = 255;
/// Quantas keywords aparecem no resumo antes do "e mais N".
const SUMMARY_ITEMS: usize = 5;

pub(crate) const IRREVERSIBLE_WARNING: &str = "Esta mudança não pode ser desfeita.";
const MATCH_TYPE_WARNING: &str = "O Google não permite trocar o tipo de correspondência; \
                                  a keyword antiga será removida e uma nova criada.";

// ---------------------------------------------------------------- helpers

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum MatchType {
    Exact,
    Phrase,
    Broad,
}

impl MatchType {
    pub(crate) fn to_google(self) -> &'static str {
        match self {
            MatchType::Exact => "EXACT",
            MatchType::Phrase => "PHRASE",
            MatchType::Broad => "BROAD",
        }
    }
}

/// Tipo de correspondência obrigatório, sem diferenciar maiúsculas.
pub(crate) fn match_type(v: &Option<String>, field: &str) -> Result<MatchType, ToolError> {
    let raw = required(v, field)?;
    match raw.trim().to_uppercase().as_str() {
        "EXACT" => Ok(MatchType::Exact),
        "PHRASE" => Ok(MatchType::Phrase),
        "BROAD" => Ok(MatchType::Broad),
        _ => Err(invalid(format!(
            "{field}: tipo de correspondência desconhecido '{raw}'; use EXACT, PHRASE ou BROAD."
        ))
        .with_field(field)),
    }
}

/// Texto de keyword da entrada: 1 a 80 caracteres e até 10 palavras. `field` é o
/// campo apontado no erro (ex.: `keywords[0].text`).
pub(crate) fn keyword_text(raw: Option<&str>, field: &str) -> Result<String, ToolError> {
    let t = raw
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| invalid(format!("Informe {field}.")).with_field(field))?;
    if t.chars().count() > MAX_TEXT_CHARS {
        return Err(invalid(format!(
            "{field}: a keyword tem no máximo {MAX_TEXT_CHARS} caracteres ('{t}')."
        ))
        .with_field(field));
    }
    if t.split_whitespace().count() > MAX_TEXT_WORDS {
        return Err(invalid(format!(
            "{field}: a keyword tem no máximo {MAX_TEXT_WORDS} palavras ('{t}')."
        ))
        .with_field(field));
    }
    Ok(t.to_string())
}

/// Valor obrigatório da entrada; faltando, `INVALID_INPUT` apontando o campo.
pub(crate) fn required<'a, T>(v: &'a Option<T>, field: &str) -> Result<&'a T, ToolError> {
    v.as_ref()
        .ok_or_else(|| invalid(format!("Informe {field}.")).with_field(field))
}

/// Id numérico obrigatório.
fn required_id(v: &Option<String>, field: &str) -> Result<String, ToolError> {
    valid_id(required(v, field)?, field)
}

/// Nome temporário de um critério filho de `parent_id`: `customers/{c}/{collection}/{pai}~-N`.
pub(crate) fn criterion_temp(
    ctx: &Ctx,
    temps: &mut TempIds,
    collection: &str,
    parent_id: &str,
) -> String {
    let t = temps.next(collection);
    let n = t.rsplit('/').next().unwrap_or_default();
    resources::rn(&ctx.customer_id, collection, &format!("{parent_id}~{n}"))
}

/// `{"<x>Operation": {"create": {resourceName: rn, ..body}}}`.
pub(crate) fn create_op(rn: &str, mut body: Value) -> Result<Value, ToolError> {
    body["resourceName"] = json!(rn);
    let key = resources::op_key(rn).map_err(internal)?;
    Ok(json!({ key: { "create": body } }))
}

/// Corpo de um critério de keyword: pai (`adGroup`, `campaign` ou `sharedSet`) e
/// `keyword {text, matchType}`; `negative: true` quando for negativa.
pub(crate) fn keyword_body(
    parent_field: &str,
    parent_rn: &str,
    text: &str,
    match_type: &str,
    negative: bool,
) -> Value {
    let mut body = json!({
        parent_field: parent_rn,
        "keyword": { "text": text, "matchType": match_type },
    });
    if negative {
        body["negative"] = json!(true);
    }
    body
}

fn unique(ids: &mut Vec<String>, id: &str, field: &str) -> Result<(), ToolError> {
    if ids.iter().any(|i| i == id) {
        return Err(invalid(format!("O critério {id} aparece mais de uma vez.")).with_field(field));
    }
    ids.push(id.to_string());
    Ok(())
}

/// Lista curta para o resumo: até 5 itens e "e mais N".
fn list(items: &[String]) -> String {
    let mut out = items
        .iter()
        .take(SUMMARY_ITEMS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SUMMARY_ITEMS {
        out.push_str(&format!(", e mais {}", items.len() - SUMMARY_ITEMS));
    }
    out
}

fn described(text: &str, match_type: &str) -> String {
    format!("'{text}' [{match_type}]")
}

/// No máximo [`MAX_KEYWORDS`] por chamada.
fn at_most_max(n: usize) -> Result<(), ToolError> {
    if n > MAX_KEYWORDS {
        return Err(invalid(format!(
            "No máximo {MAX_KEYWORDS} keywords por chamada (vieram {n})."
        ))
        .with_field("keywords"));
    }
    Ok(())
}

/// Entre 1 e [`MAX_KEYWORDS`] por chamada.
fn limit_keywords(n: usize) -> Result<(), ToolError> {
    if n == 0 {
        return Err(invalid("Informe ao menos uma keyword.").with_field("keywords"));
    }
    at_most_max(n)
}

// --------------------------------------------------------- manage_keywords

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum KeywordAction {
    Add,
    Remove,
    Pause,
    Enable,
    SetBid,
    ChangeMatchType,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct KeywordIn {
    /// `add`: texto da keyword (até 80 caracteres e 10 palavras).
    #[serde(default)]
    text: Option<String>,
    /// `add`: tipo de correspondência. `change_match_type`: o novo tipo.
    /// `EXACT`, `PHRASE` ou `BROAD` (minúsculas também valem).
    #[serde(default)]
    #[schemars(with = "Option<MatchType>")]
    match_type: Option<String>,
    /// Todas as ações menos `add`: id do critério da keyword no grupo.
    #[serde(default)]
    criterion_id: Option<String>,
    /// `add` (opcional) e `set_bid`: lance de CPC, na moeda da conta.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schemars(with = "Option<f64>")]
    cpc_bid: Option<Decimal>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ManageKeywordsInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    ad_group_id: String,
    /// `add`, `remove`, `pause`, `enable`, `set_bid` ou `change_match_type`.
    action: KeywordAction,
    /// Até 200 por chamada.
    keywords: Vec<KeywordIn>,
}

struct ManageKeywords;

#[async_trait]
impl WriteTool for ManageKeywords {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ManageKeywordsInput>(
            MANAGE_KEYWORDS,
            "Gerencia as keywords de um grupo de anúncios do Google Ads: adicionar (nascem \
             pausadas), remover, pausar, ativar, mudar o lance ou trocar o tipo de \
             correspondência. Remover e trocar o tipo não podem ser desfeitos.",
            ToolGroup::Edit,
            true,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ManageKeywordsInput = parse_input(input)?;
        let ag = valid_id(&input.ad_group_id, "ad_group_id")?;
        limit_keywords(input.keywords.len())?;
        if input.action == KeywordAction::Add {
            return add_keywords(
                ctx,
                &ag,
                &input.keywords,
                &mut TempIds::new(&ctx.customer_id),
            );
        }
        let found = existing_keywords(ctx, &ag, &input.keywords).await?;
        let head = format!("Grupo de anúncios {ag}");
        match input.action {
            KeywordAction::Add => unreachable!("tratado acima"),
            KeywordAction::Pause | KeywordAction::Enable => {
                let status = if input.action == KeywordAction::Pause {
                    EditStatus::Paused
                } else {
                    EditStatus::Active
                };
                let google = status.to_google()?;
                let mut edits = Edits::new(&head, &ctx.currency);
                for (rn, row) in &found {
                    let label = format!("status de {}", quoted(row));
                    edits.set(
                        rn,
                        "ad_group_criterion.status",
                        row,
                        json!(google),
                        &label,
                        Show::Status,
                    )?;
                }
                edits.into_plan(None)
            }
            KeywordAction::SetBid => {
                let mut edits = Edits::new(&head, &ctx.currency);
                for (i, ((rn, row), kw)) in found.iter().zip(&input.keywords).enumerate() {
                    let field = format!("keywords[{i}].cpc_bid");
                    let bid = *required(&kw.cpc_bid, &field)?;
                    let label = format!("lance de {}", quoted(row));
                    edits.money(
                        rn,
                        "ad_group_criterion.cpc_bid_micros",
                        row,
                        bid,
                        &field,
                        &label,
                    )?;
                }
                edits.into_plan(None)
            }
            KeywordAction::Remove => {
                let mut operations = Vec::new();
                let mut names = Vec::new();
                for (rn, row) in &found {
                    operations.push(resources::remove_op(rn).map_err(internal)?);
                    names.push(quoted(row));
                }
                Ok(irreversible(
                    format!(
                        "Remover {} keyword(s) do grupo de anúncios {ag}: {}",
                        names.len(),
                        list(&names)
                    ),
                    IRREVERSIBLE_WARNING,
                    operations,
                    Vec::new(),
                ))
            }
            KeywordAction::ChangeMatchType => change_match_types(ctx, &ag, &input.keywords, &found),
        }
    }
}

/// Plano sem inversa, com aviso.
fn irreversible(
    summary: String,
    warning: &str,
    operations: Vec<Value>,
    watch: Vec<Watch>,
) -> WritePlan {
    WritePlan {
        summary,
        warning: Some(warning.to_string()),
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::None,
        direct: None,
    }
}

/// `'texto'` da linha lida de `ad_group_criterion`.
fn quoted(row: &Value) -> String {
    format!(
        "'{}'",
        text(row, "adGroupCriterion.keyword.text").unwrap_or_default()
    )
}

fn add_keywords(
    ctx: &Ctx,
    ag: &str,
    keywords: &[KeywordIn],
    temps: &mut TempIds,
) -> Result<WritePlan, ToolError> {
    let ag_rn = resources::rn(&ctx.customer_id, "adGroups", ag);
    let mut operations = Vec::new();
    let mut watch = Vec::new();
    let mut order = Vec::new();
    let mut names = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let text = keyword_text(kw.text.as_deref(), &format!("keywords[{i}].text"))?;
        let mt = match_type(&kw.match_type, &format!("keywords[{i}].match_type"))?;
        let mut body = keyword_body("adGroup", &ag_rn, &text, mt.to_google(), false);
        body["status"] = json!("PAUSED");
        if let Some(bid) = kw.cpc_bid {
            let micros = positive_micros(bid, &ctx.currency, &format!("keywords[{i}].cpc_bid"))?;
            body["cpcBidMicros"] = json!(micros.to_string());
        }
        let rn = criterion_temp(ctx, temps, "adGroupCriteria", ag);
        operations.push(create_op(&rn, body)?);
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: "ad_group_criterion",
            fields: vec!["ad_group_criterion.status"],
        });
        order.push(rn);
        names.push(described(&text, mt.to_google()));
    }
    Ok(WritePlan {
        summary: format!(
            "Adicionar {} keyword(s) ao grupo de anúncios {ag} (nascem pausadas): {}",
            names.len(),
            list(&names)
        ),
        warning: None,
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::RemoveCreated { order },
        direct: None,
    })
}

/// Linhas (`adGroupCriterion`) das keywords informadas, na ordem da entrada, com o nome
/// do recurso. Id repetido é `INVALID_INPUT`; id que não existe no grupo, `NOT_FOUND`.
async fn existing_keywords(
    ctx: &Ctx,
    ag: &str,
    keywords: &[KeywordIn],
) -> Result<Vec<(String, Value)>, ToolError> {
    let mut ids: Vec<String> = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let field = format!("keywords[{i}].criterion_id");
        let id = required_id(&kw.criterion_id, &field)?;
        unique(&mut ids, &id, &field)?;
    }
    let query = format!(
        "SELECT ad_group_criterion.resource_name, ad_group_criterion.criterion_id, \
         ad_group_criterion.status, ad_group_criterion.cpc_bid_micros, \
         ad_group_criterion.keyword.text, ad_group_criterion.keyword.match_type \
         FROM ad_group_criterion WHERE ad_group.id = {ag} \
         AND ad_group_criterion.criterion_id IN ({}) \
         AND ad_group_criterion.type = 'KEYWORD' AND ad_group_criterion.negative = false \
         AND ad_group_criterion.status != 'REMOVED'",
        ids.join(", ")
    );
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
        .await
        .map_err(|e| translate(e, Some(ctx), MANAGE_KEYWORDS))?;
    ids.iter()
        .enumerate()
        .map(|(i, id)| {
            let rn = resources::rn(&ctx.customer_id, "adGroupCriteria", &format!("{ag}~{id}"));
            page.results
                .iter()
                .find(|r| text(r, "adGroupCriterion.resourceName").as_deref() == Some(&rn))
                .map(|r| (rn, r.clone()))
                .ok_or_else(|| {
                    ToolError::new(
                        codes::NOT_FOUND,
                        format!("Keyword {id} não encontrada no grupo {ag}."),
                    )
                    .with_field(format!("keywords[{i}].criterion_id"))
                })
        })
        .collect()
}

/// Cria o critério com o novo tipo e remove o antigo, no mesmo mutate. A nova keyword
/// herda o status e o lance da antiga (é uma substituição, não uma keyword a mais).
fn change_match_types(
    ctx: &Ctx,
    ag: &str,
    keywords: &[KeywordIn],
    found: &[(String, Value)],
) -> Result<WritePlan, ToolError> {
    let ag_rn = resources::rn(&ctx.customer_id, "adGroups", ag);
    let mut temps = TempIds::new(&ctx.customer_id);
    let mut operations = Vec::new();
    let mut watch = Vec::new();
    let mut names = Vec::new();
    for (i, ((old_rn, row), kw)) in found.iter().zip(keywords).enumerate() {
        let new_type = match_type(&kw.match_type, &format!("keywords[{i}].match_type"))?;
        let old_type = text(row, "adGroupCriterion.keyword.matchType").unwrap_or_default();
        let kw_text = text(row, "adGroupCriterion.keyword.text").unwrap_or_default();
        if old_type == new_type.to_google() {
            return Err(
                invalid(format!("A keyword '{kw_text}' já é do tipo {old_type}."))
                    .with_field(format!("keywords[{i}].match_type")),
            );
        }
        let mut body = keyword_body("adGroup", &ag_rn, &kw_text, new_type.to_google(), false);
        body["status"] =
            json!(text(row, "adGroupCriterion.status").unwrap_or_else(|| "PAUSED".into()));
        if let Some(bid) = at(row, "adGroupCriterion.cpcBidMicros") {
            body["cpcBidMicros"] = bid.clone();
        }
        let rn = criterion_temp(ctx, &mut temps, "adGroupCriteria", ag);
        operations.push(create_op(&rn, body)?);
        operations.push(resources::remove_op(old_rn).map_err(internal)?);
        watch.push(Watch {
            resource_name: rn,
            resource: "ad_group_criterion",
            fields: vec!["ad_group_criterion.status"],
        });
        names.push(format!("'{kw_text}' {old_type} → {}", new_type.to_google()));
    }
    Ok(irreversible(
        format!(
            "Trocar o tipo de correspondência de {} keyword(s) no grupo de anúncios {ag}: {}",
            names.len(),
            list(&names)
        ),
        MATCH_TYPE_WARNING,
        operations,
        watch,
    ))
}

// ------------------------------------------------------- manage_negatives

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum NegativeLevel {
    Campaign,
    AdGroup,
    SharedList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum NegativeAction {
    Add,
    Remove,
    CreateList,
    AttachList,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NegativeIn {
    /// `add` e `create_list`: texto da negativa (até 80 caracteres e 10 palavras).
    #[serde(default)]
    text: Option<String>,
    /// `add` e `create_list`: `EXACT`, `PHRASE` ou `BROAD`.
    #[serde(default)]
    #[schemars(with = "Option<MatchType>")]
    match_type: Option<String>,
    /// `remove`: id do critério da negativa.
    #[serde(default)]
    criterion_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ManageNegativesInput {
    #[serde(flatten)]
    #[allow(dead_code)]
    account: AccountRef,
    /// `add` e `remove`: onde fica a negativa. `create_list` é sempre `shared_list` e
    /// `attach_list` é sempre `campaign`.
    #[serde(default)]
    level: Option<NegativeLevel>,
    /// `level=campaign` e `attach_list`; em `create_list`, anexa a lista nova à campanha.
    #[serde(default)]
    campaign_id: Option<String>,
    /// `level=ad_group`.
    #[serde(default)]
    ad_group_id: Option<String>,
    /// `level=shared_list` e `attach_list`.
    #[serde(default)]
    shared_set_id: Option<String>,
    /// `add`, `remove`, `create_list` ou `attach_list`.
    action: NegativeAction,
    /// Até 200 por chamada.
    #[serde(default)]
    keywords: Option<Vec<NegativeIn>>,
    /// `create_list`: nome da lista.
    #[serde(default)]
    list_name: Option<String>,
}

/// Onde as negativas de um nível ficam.
struct Level {
    collection: &'static str,
    parent_field: &'static str,
    parent_collection: &'static str,
    parent_id: String,
    negative: bool,
    resource: &'static str,
    watch_fields: Vec<&'static str>,
    /// Filtro GAQL do pai (`campaign.id`, …) e do tipo de recurso lido.
    parent_filter: &'static str,
    /// Condição extra do `remove`: só negativas de keyword ainda não removidas.
    remove_where: &'static str,
    label: String,
}

impl Level {
    fn parent_rn(&self, ctx: &Ctx) -> String {
        resources::rn(&ctx.customer_id, self.parent_collection, &self.parent_id)
    }
}

fn level_of(input: &ManageNegativesInput) -> Result<Level, ToolError> {
    Ok(match *required(&input.level, "level")? {
        NegativeLevel::Campaign => {
            let id = required_id(&input.campaign_id, "campaign_id")?;
            Level {
                collection: "campaignCriteria",
                parent_field: "campaign",
                parent_collection: "campaigns",
                label: format!("campanha {id}"),
                parent_id: id,
                negative: true,
                resource: "campaign_criterion",
                watch_fields: vec!["campaign_criterion.negative", "campaign_criterion.status"],
                parent_filter: "campaign.id",
                remove_where: "campaign_criterion.negative = TRUE AND campaign_criterion.type = 'KEYWORD' AND campaign_criterion.status != 'REMOVED'",
            }
        }
        NegativeLevel::AdGroup => {
            let id = required_id(&input.ad_group_id, "ad_group_id")?;
            Level {
                collection: "adGroupCriteria",
                parent_field: "adGroup",
                parent_collection: "adGroups",
                label: format!("grupo de anúncios {id}"),
                parent_id: id,
                negative: true,
                resource: "ad_group_criterion",
                watch_fields: vec!["ad_group_criterion.negative", "ad_group_criterion.status"],
                parent_filter: "ad_group.id",
                remove_where: "ad_group_criterion.negative = TRUE AND ad_group_criterion.type = 'KEYWORD' AND ad_group_criterion.status != 'REMOVED'",
            }
        }
        NegativeLevel::SharedList => {
            let id = required_id(&input.shared_set_id, "shared_set_id")?;
            Level {
                collection: "sharedCriteria",
                parent_field: "sharedSet",
                parent_collection: "sharedSets",
                label: format!("lista compartilhada {id}"),
                parent_id: id,
                negative: false,
                resource: "shared_criterion",
                watch_fields: vec!["shared_criterion.keyword.text"],
                parent_filter: "shared_set.id",
                remove_where: "shared_criterion.type = 'KEYWORD'",
            }
        }
    })
}

/// Cria os critérios de negativa de `lvl` sob `parent_rn` (que pode ser temporário).
/// Devolve os nomes temporários na ordem de criação.
#[allow(clippy::too_many_arguments)]
fn negative_creates(
    ctx: &Ctx,
    temps: &mut TempIds,
    lvl: &Level,
    parent_rn: &str,
    parent_key: &str,
    keywords: &[NegativeIn],
    operations: &mut Vec<Value>,
    watch: &mut Vec<Watch>,
    names: &mut Vec<String>,
) -> Result<Vec<String>, ToolError> {
    let mut created = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let text = keyword_text(kw.text.as_deref(), &format!("keywords[{i}].text"))?;
        let mt = match_type(&kw.match_type, &format!("keywords[{i}].match_type"))?;
        let body = keyword_body(
            lvl.parent_field,
            parent_rn,
            &text,
            mt.to_google(),
            lvl.negative,
        );
        let rn = criterion_temp(ctx, temps, lvl.collection, parent_key);
        operations.push(create_op(&rn, body)?);
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: lvl.resource,
            fields: lvl.watch_fields.clone(),
        });
        created.push(rn);
        names.push(described(&text, mt.to_google()));
    }
    Ok(created)
}

struct ManageNegatives;

#[async_trait]
impl WriteTool for ManageNegatives {
    fn spec(&self) -> ToolSpec {
        ToolSpec::write::<ManageNegativesInput>(
            MANAGE_NEGATIVES,
            "Gerencia palavras-chave negativas no Google Ads: adicionar ou remover na \
             campanha, no grupo de anúncios ou numa lista compartilhada, criar uma lista \
             compartilhada de negativas e anexá-la a uma campanha. Remover não pode ser \
             desfeito.",
            ToolGroup::Edit,
            true,
        )
        .destructive()
    }

    async fn plan(&self, ctx: &Ctx, input: &Value) -> Result<WritePlan, ToolError> {
        let input: ManageNegativesInput = parse_input(input)?;
        let keywords = input.keywords.as_deref().unwrap_or_default();
        match input.action {
            NegativeAction::Add => {
                let lvl = level_of(&input)?;
                limit_keywords(keywords.len())?;
                let parent_rn = lvl.parent_rn(ctx);
                let (mut ops, mut watch, mut names) = (Vec::new(), Vec::new(), Vec::new());
                let order = negative_creates(
                    ctx,
                    &mut TempIds::new(&ctx.customer_id),
                    &lvl,
                    &parent_rn,
                    &lvl.parent_id,
                    keywords,
                    &mut ops,
                    &mut watch,
                    &mut names,
                )?;
                Ok(WritePlan {
                    summary: format!(
                        "Adicionar {} negativa(s) na {}: {}",
                        names.len(),
                        lvl.label,
                        list(&names)
                    ),
                    warning: None,
                    operations: ops,
                    field_changes: Vec::new(),
                    watch,
                    inverse: InverseSpec::RemoveCreated { order },
                    direct: None,
                })
            }
            NegativeAction::Remove => {
                let lvl = level_of(&input)?;
                limit_keywords(keywords.len())?;
                let found = existing_negatives(ctx, &lvl, keywords).await?;
                let operations = found
                    .iter()
                    .map(|(rn, _)| resources::remove_op(rn).map_err(internal))
                    .collect::<Result<Vec<_>, _>>()?;
                let names: Vec<String> = found.iter().map(|(_, d)| d.clone()).collect();
                Ok(irreversible(
                    format!(
                        "Remover {} negativa(s) da {}: {}",
                        names.len(),
                        lvl.label,
                        list(&names)
                    ),
                    IRREVERSIBLE_WARNING,
                    operations,
                    Vec::new(),
                ))
            }
            NegativeAction::CreateList => {
                // A lista pode nascer vazia (sem keywords), por isso não exige ao menos uma.
                at_most_max(keywords.len())?;
                create_list(ctx, &input, keywords)
            }
            NegativeAction::AttachList => attach_list(ctx, &input),
        }
    }
}

/// Negativas de keyword de `lvl` com os ids informados, na ordem da entrada: nome do
/// recurso e `'texto' [TIPO]`. Id que não é de uma negativa de keyword (uma keyword
/// positiva, um local, um dispositivo, ou já removida) é `NOT_FOUND`, para o `remove`
/// nunca apagar outro tipo de critério.
async fn existing_negatives(
    ctx: &Ctx,
    lvl: &Level,
    keywords: &[NegativeIn],
) -> Result<Vec<(String, String)>, ToolError> {
    let mut ids: Vec<String> = Vec::new();
    for (i, kw) in keywords.iter().enumerate() {
        let field = format!("keywords[{i}].criterion_id");
        let id = required_id(&kw.criterion_id, &field)?;
        unique(&mut ids, &id, &field)?;
    }
    let r = lvl.resource;
    let query = format!(
        "SELECT {r}.resource_name, {r}.criterion_id, {r}.keyword.text, {r}.keyword.match_type \
         FROM {r} WHERE {} = {} AND {r}.criterion_id IN ({}) AND {}",
        lvl.parent_filter,
        lvl.parent_id,
        ids.join(", "),
        lvl.remove_where
    );
    let page = ctx
        .client
        .search(&ctx.customer_id, ctx.login.as_deref(), &query, None)
        .await
        .map_err(|e| translate(e, Some(ctx), MANAGE_NEGATIVES))?;
    let root = fields::camel(r);
    ids.iter()
        .enumerate()
        .map(|(i, id)| {
            let rn = resources::rn(
                &ctx.customer_id,
                lvl.collection,
                &format!("{}~{id}", lvl.parent_id),
            );
            page.results
                .iter()
                .find(|row| text(row, &format!("{root}.resourceName")).as_deref() == Some(&rn))
                .map(|row| {
                    let t = text(row, &format!("{root}.keyword.text")).unwrap_or_default();
                    let m = text(row, &format!("{root}.keyword.matchType")).unwrap_or_default();
                    (rn, described(&t, &m))
                })
                .ok_or_else(|| {
                    ToolError::new(
                        codes::NOT_FOUND,
                        format!("Negativa {id} não encontrada na {}.", lvl.label),
                    )
                    .with_field(format!("keywords[{i}].criterion_id"))
                })
        })
        .collect()
}

/// `level` informado tem que combinar com a ação (`create_list` e `attach_list` o fixam).
fn expect_level(input: &ManageNegativesInput, expected: NegativeLevel) -> Result<(), ToolError> {
    match input.level {
        Some(l) if l != expected => Err(invalid(format!(
            "Para {} o nível é sempre {}.",
            if expected == NegativeLevel::SharedList {
                "create_list"
            } else {
                "attach_list"
            },
            if expected == NegativeLevel::SharedList {
                "shared_list"
            } else {
                "campaign"
            }
        ))
        .with_field("level")),
        _ => Ok(()),
    }
}

fn create_list(
    ctx: &Ctx,
    input: &ManageNegativesInput,
    keywords: &[NegativeIn],
) -> Result<WritePlan, ToolError> {
    expect_level(input, NegativeLevel::SharedList)?;
    let name = required(&input.list_name, "list_name")?.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_LIST_NAME_CHARS {
        return Err(invalid(format!(
            "list_name deve ter de 1 a {MAX_LIST_NAME_CHARS} caracteres."
        ))
        .with_field("list_name"));
    }
    let campaign_id = match &input.campaign_id {
        Some(_) => Some(required_id(&input.campaign_id, "campaign_id")?),
        None => None,
    };
    let mut temps = TempIds::new(&ctx.customer_id);
    let set_rn = temps.next("sharedSets");
    let set_id = set_rn.rsplit('/').next().unwrap_or_default().to_string();
    let mut operations = vec![create_op(
        &set_rn,
        json!({ "name": name, "type": "NEGATIVE_KEYWORDS" }),
    )?];
    let mut watch = vec![Watch {
        resource_name: set_rn.clone(),
        resource: "shared_set",
        fields: vec!["shared_set.name", "shared_set.status"],
    }];
    let lvl = Level {
        collection: "sharedCriteria",
        parent_field: "sharedSet",
        parent_collection: "sharedSets",
        parent_id: set_id.clone(),
        negative: false,
        resource: "shared_criterion",
        watch_fields: vec!["shared_criterion.keyword.text"],
        parent_filter: "shared_set.id",
        remove_where: "shared_criterion.type = 'KEYWORD'",
        label: String::new(),
    };
    let mut names = Vec::new();
    let mut order = negative_creates(
        ctx,
        &mut temps,
        &lvl,
        &set_rn,
        &set_id,
        keywords,
        &mut operations,
        &mut watch,
        &mut names,
    )?;
    let mut summary = format!(
        "Criar a lista de negativas '{name}' com {} palavra(s)",
        names.len()
    );
    if let Some(cid) = campaign_id {
        let rn = resources::rn(
            &ctx.customer_id,
            "campaignSharedSets",
            &format!("{cid}~{set_id}"),
        );
        operations.push(create_op(
            &rn,
            json!({
                "campaign": resources::rn(&ctx.customer_id, "campaigns", &cid),
                "sharedSet": set_rn,
            }),
        )?);
        watch.push(Watch {
            resource_name: rn.clone(),
            resource: "campaign_shared_set",
            fields: vec!["campaign_shared_set.campaign", "campaign_shared_set.status"],
        });
        order.push(rn);
        summary.push_str(&format!(" e anexá-la à campanha {cid}"));
    }
    order.push(set_rn);
    if !names.is_empty() {
        summary.push_str(&format!(": {}", list(&names)));
    }
    Ok(WritePlan {
        summary,
        warning: None,
        operations,
        field_changes: Vec::new(),
        watch,
        inverse: InverseSpec::RemoveCreated { order },
        direct: None,
    })
}

fn attach_list(ctx: &Ctx, input: &ManageNegativesInput) -> Result<WritePlan, ToolError> {
    expect_level(input, NegativeLevel::Campaign)?;
    let cid = required_id(&input.campaign_id, "campaign_id")?;
    let sid = required_id(&input.shared_set_id, "shared_set_id")?;
    // O nome é determinístico (`{campanha}~{lista}`), então não precisa de temporário.
    let rn = resources::rn(
        &ctx.customer_id,
        "campaignSharedSets",
        &format!("{cid}~{sid}"),
    );
    let key = resources::op_key(&rn).map_err(internal)?;
    let operation = json!({ key: { "create": {
        "campaign": resources::rn(&ctx.customer_id, "campaigns", &cid),
        "sharedSet": resources::rn(&ctx.customer_id, "sharedSets", &sid),
    } } });
    Ok(WritePlan {
        summary: format!("Anexar a lista de negativas {sid} à campanha {cid}"),
        warning: None,
        operations: vec![operation],
        field_changes: Vec::new(),
        watch: vec![Watch {
            resource_name: rn.clone(),
            resource: "campaign_shared_set",
            fields: vec!["campaign_shared_set.campaign", "campaign_shared_set.status"],
        }],
        inverse: InverseSpec::RemoveCreated { order: vec![rn] },
        direct: None,
    })
}
