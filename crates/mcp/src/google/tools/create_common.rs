//! Peças comuns das ferramentas de criação de campanha (Search, Performance Max,
//! Demand Gen e Display): orçamento, campanha base, estratégia de lance, local e
//! idioma, e sitelinks. Tudo é validado localmente, com o erro apontando o campo da
//! entrada, antes de qualquer chamada.
//!
//! Toda campanha nasce `PAUSED`. As criações usam ids temporários só onde outra
//! operação referencia o recurso (orçamento, campanha, grupo, asset); critérios,
//! anúncios e vínculos vão sem `resourceName` e o Google gera o nome.

use adsconsole_core::money::Decimal;
use adsconsole_google_ads::resources::TempIds;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Number, Value, json};

use adsconsole_google_ads::fields;
use adsconsole_google_ads::images::{self, ImageKind, ImageRef, Loaded};

use super::asset_group::asset_rn;
use super::edit::{date, final_url, positive_micros};
use super::reports::{invalid, valid_id};
use super::targeting::{language_criterion, location_criterion, with_parent};
use crate::engine::internal;
use crate::error::ToolError;
use crate::google::ctx::Ctx;

/// Tamanho máximo, em bytes UTF-8, do nome de campanha, orçamento e grupo de anúncios.
pub(crate) const MAX_NAME_BYTES: usize = 255;
const MAX_SITELINKS: usize = 20;
const SITELINK_TEXT_CHARS: usize = 25;
const SITELINK_DESCRIPTION_CHARS: usize = 35;
const EU_POLITICAL: &str = "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING";

/// Estratégia de lance da campanha nova. Na entrada: `{"maximize_clicks": {"cpc_cap": 2.5}}`,
/// `{"maximize_conversions": {"target_cpa": 40}}`,
/// `{"maximize_conversion_value": {"target_roas": 3.5}}` ou `{"manual_cpc": {}}`. O nome
/// sozinho (`"manual_cpc"`, `"maximize_clicks"`) também vale em [`parse_bidding`].
#[derive(Debug, Clone, Deserialize, JsonSchema)]
// Os nomes das variantes são as chaves da entrada (`maximize_clicks`, …).
#[allow(clippy::enum_variant_names)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Bidding {
    /// Maximizar cliques (`targetSpend`).
    MaximizeClicks {
        /// Limite de CPC, na moeda da conta.
        #[serde(default, with = "rust_decimal::serde::float_option")]
        #[schemars(with = "Option<f64>")]
        cpc_cap: Option<Decimal>,
    },
    /// Maximizar conversões.
    MaximizeConversions {
        /// CPA desejado, na moeda da conta.
        #[serde(default, with = "rust_decimal::serde::float_option")]
        #[schemars(with = "Option<f64>")]
        target_cpa: Option<Decimal>,
    },
    /// Maximizar valor de conversão.
    MaximizeConversionValue {
        /// ROAS desejado (ex.: 3.5 = 350%).
        #[serde(default)]
        target_roas: Option<f64>,
    },
    /// CPC manual (lances por grupo de anúncios).
    ManualCpc {},
}

impl Default for Bidding {
    fn default() -> Self {
        Bidding::MaximizeClicks { cpc_cap: None }
    }
}

impl Bidding {
    pub(crate) fn is_manual(&self) -> bool {
        matches!(self, Bidding::ManualCpc {})
    }

    /// Campo da estratégia na campanha e o valor dele, com dinheiro em micros.
    /// Erros apontam `bidding.<variante>.<alvo>`.
    pub(crate) fn api_field(&self, currency: &str) -> Result<(&'static str, Value), ToolError> {
        let micros = |v: &Option<Decimal>, key: &str, field: &str| -> Result<Value, ToolError> {
            Ok(match v {
                Some(d) => json!({ key: positive_micros(*d, currency, field)?.to_string() }),
                None => json!({}),
            })
        };
        Ok(match self {
            Bidding::MaximizeClicks { cpc_cap } => (
                "targetSpend",
                micros(
                    cpc_cap,
                    "cpcBidCeilingMicros",
                    "bidding.maximize_clicks.cpc_cap",
                )?,
            ),
            Bidding::MaximizeConversions { target_cpa } => (
                "maximizeConversions",
                micros(
                    target_cpa,
                    "targetCpaMicros",
                    "bidding.maximize_conversions.target_cpa",
                )?,
            ),
            Bidding::MaximizeConversionValue { target_roas } => {
                let value = match target_roas {
                    Some(r) => {
                        let n = Number::from_f64(*r).filter(|_| *r > 0.0).ok_or_else(|| {
                            let f = "bidding.maximize_conversion_value.target_roas";
                            invalid(format!("{f} deve ser maior que zero.")).with_field(f)
                        })?;
                        json!({ "targetRoas": n })
                    }
                    None => json!({}),
                };
                ("maximizeConversionValue", value)
            }
            Bidding::ManualCpc {} => ("manualCpc", json!({})),
        })
    }
}

/// Estratégia de lance da entrada (o campo `bidding`, recebido como JSON cru); sem
/// valor, o padrão `maximize_clicks`. Aceita também só o nome da estratégia
/// (`"manual_cpc"`), como `{"manual_cpc": {}}`. Valor inválido: `INVALID_INPUT` em
/// `bidding`.
pub(crate) fn parse_bidding(v: Option<&Value>) -> Result<Bidding, ToolError> {
    let v = match v {
        None | Some(Value::Null) => return Ok(Bidding::default()),
        Some(Value::String(name)) => json!({ name: {} }),
        Some(other) => other.clone(),
    };
    serde_json::from_value(v).map_err(|e| {
        invalid(format!(
            "bidding inválido ({e}); use maximize_clicks, maximize_conversions, \
             maximize_conversion_value ou manual_cpc."
        ))
        .with_field("bidding")
    })
}

/// Nome de campanha ou grupo de anúncios: sem espaços nas pontas, não vazio e com
/// até [`MAX_NAME_BYTES`] bytes em UTF-8 (o limite do Google é em bytes).
pub(crate) fn name_text(raw: &str, field: &str) -> Result<String, ToolError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(invalid(format!("Informe {field}.")).with_field(field));
    }
    if t.len() > MAX_NAME_BYTES {
        return Err(invalid(format!(
            "{field} tem no máximo {MAX_NAME_BYTES} bytes em UTF-8 (veio com {}).",
            t.len()
        ))
        .with_field(field));
    }
    Ok(t.to_string())
}

/// Nome do orçamento de uma campanha nova: `<nome> (orçamento <UTC aaaammdd-HHMMSSmmm>)`,
/// único porque o Google não aceita dois orçamentos com o mesmo nome. A parte do nome
/// da campanha é cortada (em fronteira de caractere) para o total caber em
/// [`MAX_NAME_BYTES`] bytes.
///
/// O horário deixa o `plan` não determinístico só neste nome, o que é seguro: o nome
/// do orçamento não é lido no `watch` nem entra na inversa (que usa o nome de recurso).
pub(crate) fn budget_name(campaign_name: &str) -> String {
    budget_name_at(campaign_name, chrono::Utc::now())
}

fn budget_name_at(campaign_name: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let suffix = format!(" (orçamento {})", now.format("%Y%m%d-%H%M%S%3f"));
    let max = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut cut = campaign_name.len().min(max);
    while !campaign_name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}", &campaign_name[..cut])
}

/// Datas de início e término (AAAA-MM-DD, início ≤ término) no corpo da campanha.
/// Na v26 vão como `startDateTime` (00:00:00) e `endDateTime` (23:59:59); ver `fields.rs`.
pub(crate) fn campaign_dates(
    campaign: &mut Value,
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<(), ToolError> {
    let start = start_date.map(|s| date(s, "start_date")).transpose()?;
    let end = end_date.map(|e| date(e, "end_date")).transpose()?;
    if let (Some(s), Some(e)) = (start, end)
        && s > e
    {
        return Err(invalid("start_date deve ser anterior a end_date.").with_field("end_date"));
    }
    for (gaql, value) in [
        (
            "campaign.start_date_time",
            start_date.map(fields::start_of_day),
        ),
        ("campaign.end_date_time", end_date.map(fields::end_of_day)),
    ] {
        if let Some(v) = value {
            let f = fields::find(gaql).ok_or_else(|| internal(format!("campo {gaql}")))?;
            set_path(campaign, f.json_path, json!(v));
        }
    }
    Ok(())
}

/// Grava `v` em `obj` seguindo os segmentos de `path`, criando os objetos do meio.
pub(crate) fn set_path(obj: &mut Value, path: &[&str], v: Value) {
    let Some((last, head)) = path.split_last() else {
        return;
    };
    let mut cur = obj;
    for seg in head {
        if !cur[*seg].is_object() {
            cur[*seg] = json!({});
        }
        cur = &mut cur[*seg];
    }
    cur[*last] = v;
}

/// Quantidade entre `min` e `max`; fora disso, `INVALID_INPUT` em `field`.
pub(crate) fn count(
    n: usize,
    min: usize,
    max: usize,
    what: &str,
    field: &str,
) -> Result<(), ToolError> {
    if n < min || n > max {
        return Err(invalid(format!(
            "{field}: informe de {min} a {max} {what} (vieram {n})."
        ))
        .with_field(field));
    }
    Ok(())
}

/// Texto obrigatório da entrada, sem espaços nas pontas, de 1 a `max` caracteres.
pub(crate) fn bounded_text(raw: &str, max: usize, field: &str) -> Result<String, ToolError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(invalid(format!("Informe {field}.")).with_field(field));
    }
    if t.chars().count() > max {
        return Err(
            invalid(format!("{field} tem no máximo {max} caracteres ('{t}').")).with_field(field),
        );
    }
    Ok(t.to_string())
}

/// `campaignBudgetOperation` de criação: orçamento diário (`daily_budget`, na moeda
/// da conta) com `deliveryMethod: STANDARD`, chamado [`budget_name`] da campanha.
pub(crate) fn budget_op(
    temp: &str,
    campaign_name: &str,
    daily_budget: Decimal,
    currency: &str,
    shared: bool,
) -> Result<Value, ToolError> {
    let micros = positive_micros(daily_budget, currency, "daily_budget")?;
    Ok(json!({ "campaignBudgetOperation": { "create": {
        "resourceName": temp,
        "name": budget_name(campaign_name),
        "amountMicros": micros.to_string(),
        "deliveryMethod": "STANDARD",
        "explicitlyShared": shared,
    } } }))
}

/// Corpo (`create`) da campanha: pausada, com orçamento, tipo de canal, estratégia
/// de lance e a declaração de propaganda política da UE. Quem chama acrescenta o
/// resto (redes, datas) e embrulha em `campaignOperation`.
pub(crate) fn campaign_base(
    temp: &str,
    budget_rn: &str,
    name: &str,
    channel: &str,
    bidding: &Bidding,
    currency: &str,
) -> Result<Value, ToolError> {
    let (key, value) = bidding.api_field(currency)?;
    Ok(json!({
        "resourceName": temp,
        "name": name,
        "status": "PAUSED",
        "advertisingChannelType": channel,
        "campaignBudget": budget_rn,
        key: value,
        "containsEuPoliticalAdvertising": EU_POLITICAL,
    }))
}

/// Operação de criação sem `resourceName` (o Google gera o nome).
pub(crate) fn create_unnamed(op_key: &str, body: Value) -> Value {
    json!({ op_key: { "create": body } })
}

/// Ids da entrada (`locations`, `languages`): numéricos e sem repetição. Erros em
/// `field[i]`.
pub(crate) fn criterion_ids(items: &[String], field: &str) -> Result<Vec<String>, ToolError> {
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in items.iter().enumerate() {
        let f = format!("{field}[{i}]");
        let id = valid_id(raw.trim(), &f)?;
        if out.contains(&id) {
            return Err(invalid(format!("{f}: o id {id} aparece mais de uma vez.")).with_field(f));
        }
        out.push(id);
    }
    Ok(out)
}

/// Critérios de local (`locations`: ids de `geoTargetConstants`) e idioma
/// (`languages`: ids de `languageConstants`) de `parent_field` = `parent_rn`, em
/// operações `op_key` sem `resourceName`. Erros em `{prefix}locations[i]` /
/// `{prefix}languages[i]`.
pub(crate) fn geo_lang_criteria(
    op_key: &str,
    parent_field: &str,
    parent_rn: &str,
    locations: &[String],
    languages: &[String],
    prefix: &str,
) -> Result<Vec<Value>, ToolError> {
    let mut ops = Vec::new();
    let lf = format!("{prefix}locations");
    for (i, id) in criterion_ids(locations, &lf)?.iter().enumerate() {
        let body = location_criterion(Some(id), false, &format!("{lf}[{i}]"))?;
        ops.push(create_unnamed(
            op_key,
            with_parent(body, parent_field, parent_rn),
        ));
    }
    let gf = format!("{prefix}languages");
    for (i, id) in criterion_ids(languages, &gf)?.iter().enumerate() {
        let body = language_criterion(Some(id), &format!("{gf}[{i}]"))?;
        ops.push(create_unnamed(
            op_key,
            with_parent(body, parent_field, parent_rn),
        ));
    }
    Ok(ops)
}

/// Critérios de campanha de local e idioma (ver [`geo_lang_criteria`]).
pub(crate) fn geo_lang_ops(
    campaign_rn: &str,
    locations: &[String],
    languages: &[String],
) -> Result<Vec<Value>, ToolError> {
    geo_lang_criteria(
        "campaignCriterionOperation",
        "campaign",
        campaign_rn,
        locations,
        languages,
        "",
    )
}

/// Textos de um anúncio: de `min` a `max` itens de 1 a `len` caracteres, sem
/// repetição. Erros de quantidade em `field`, de item em `field[i]`.
pub(crate) fn text_list(
    items: &[String],
    (min, max, len): (usize, usize, usize),
    what: &str,
    field: &str,
) -> Result<Vec<String>, ToolError> {
    count(items.len(), min, max, what, field)?;
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in items.iter().enumerate() {
        let f = format!("{field}[{i}]");
        let t = bounded_text(raw, len, &f)?;
        if out.contains(&t) {
            return Err(invalid(format!("{f}: '{t}' aparece mais de uma vez.")).with_field(f));
        }
        out.push(t);
    }
    Ok(out)
}

/// `[{"text": …}]` (`AdTextAsset`) dos textos.
pub(crate) fn text_assets(items: &[String]) -> Value {
    Value::Array(items.iter().map(|t| json!({ "text": t })).collect())
}

/// Sitelink da entrada.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SitelinkIn {
    /// Texto do link (até 25 caracteres).
    pub link_text: String,
    /// URL final do link (`https://…`).
    pub final_url: String,
    /// Linha 1 da descrição (até 35 caracteres); informe as duas linhas ou nenhuma.
    #[serde(default)]
    pub description1: Option<String>,
    /// Linha 2 da descrição (até 35 caracteres).
    #[serde(default)]
    pub description2: Option<String>,
}

/// Para cada sitelink (até 20): o asset (`sitelinkAsset` + `finalUrls`, com id
/// temporário) e o vínculo `campaignAsset` com `fieldType: SITELINK`. Erros apontam
/// `sitelinks[i].<campo>`.
pub(crate) fn sitelink_ops(
    temps: &mut TempIds,
    campaign_rn: &str,
    sitelinks: &[SitelinkIn],
) -> Result<Vec<Value>, ToolError> {
    if sitelinks.len() > MAX_SITELINKS {
        return Err(invalid(format!(
            "No máximo {MAX_SITELINKS} sitelinks (vieram {}).",
            sitelinks.len()
        ))
        .with_field("sitelinks"));
    }
    let mut ops = Vec::new();
    for (i, s) in sitelinks.iter().enumerate() {
        let f = |name: &str| format!("sitelinks[{i}].{name}");
        let mut asset = json!({
            "linkText": bounded_text(&s.link_text, SITELINK_TEXT_CHARS, &f("link_text"))?
        });
        match (&s.description1, &s.description2) {
            (Some(d1), Some(d2)) => {
                asset["description1"] = json!(bounded_text(
                    d1,
                    SITELINK_DESCRIPTION_CHARS,
                    &f("description1")
                )?);
                asset["description2"] = json!(bounded_text(
                    d2,
                    SITELINK_DESCRIPTION_CHARS,
                    &f("description2")
                )?);
            }
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                let missing = f(if s.description1.is_none() {
                    "description1"
                } else {
                    "description2"
                });
                return Err(invalid(format!(
                    "sitelinks[{i}]: informe as duas linhas da descrição ou nenhuma."
                ))
                .with_field(missing));
            }
        }
        let url = s.final_url.trim();
        final_url(url, &f("final_url"))?;
        let asset_rn = temps.next("assets");
        ops.push(json!({ "assetOperation": { "create": {
            "resourceName": asset_rn,
            "finalUrls": [url],
            "sitelinkAsset": asset,
        } } }));
        ops.push(create_unnamed(
            "campaignAssetOperation",
            json!({ "campaign": campaign_rn, "asset": asset_rn, "fieldType": "SITELINK" }),
        ));
    }
    Ok(ops)
}

/// [`ImageRef`] da entrada (recebido como JSON cru, para o erro apontar o campo):
/// formato inválido é `INVALID_INPUT` em `field`.
pub(crate) fn image_ref(v: &Value, field: &str) -> Result<ImageRef, ToolError> {
    ImageRef::from_value(v).map_err(|e| invalid(format!("{field}: {e}.")).with_field(field))
}

/// Imagem pronta para a criação, no papel `kind`: arquivo ou URL carregados e
/// conferidos (proporção e tamanho mínimo), ou asset existente desta conta (sem
/// conferência local). Erros: `INVALID_INPUT` em `field` (ou `field.asset`).
pub(crate) async fn load_image(
    ctx: &Ctx,
    r: &ImageRef,
    kind: ImageKind,
    field: &str,
) -> Result<Loaded, ToolError> {
    let loaded = load_unchecked(ctx, r, field).await?;
    if let Loaded::New(img) = &loaded {
        check_kind(kind, img.width, img.height, field)?;
    }
    Ok(loaded)
}

/// Imagem carregada (ou asset existente desta conta), sem conferir proporção.
async fn load_unchecked(ctx: &Ctx, r: &ImageRef, field: &str) -> Result<Loaded, ToolError> {
    let loaded = images::load(r, images::http_client())
        .await
        .map_err(|e| invalid(format!("{field}: {e}.")).with_field(field))?;
    Ok(match loaded {
        Loaded::Existing(rn) => Loaded::Existing(asset_rn(ctx, &rn, &format!("{field}.asset"))?),
        new => new,
    })
}

fn check_kind(kind: ImageKind, w: u32, h: u32, field: &str) -> Result<(), ToolError> {
    images::check(kind, w, h).map_err(|e| invalid(format!("{field}: {e}.")).with_field(field))
}

/// O primeiro papel de `kinds` em que a imagem `w`x`h` cabe.
fn first_fit(kinds: &[ImageKind], w: u32, h: u32, field: &str) -> Result<ImageKind, ToolError> {
    let mut why = Vec::new();
    for k in kinds {
        match images::check(*k, w, h) {
            Ok(()) => return Ok(*k),
            Err(e) => why.push(e),
        }
    }
    Err(invalid(format!("{field}: {}.", why.join("; "))).with_field(field))
}

/// Referência de imagem da entrada, já lida, com o papel e o campo.
pub(crate) struct ImageIn {
    pub r: ImageRef,
    pub kind: ImageKind,
    pub field: String,
}

/// Referência de imagem lida e, se for asset existente, conferida (sem carregar nada).
pub(crate) fn image_in(
    ctx: &Ctx,
    v: &Value,
    kind: ImageKind,
    field: String,
) -> Result<ImageIn, ToolError> {
    let r = image_ref(v, &field)?;
    if let ImageRef::Asset { asset } = &r {
        asset_rn(ctx, asset, &format!("{field}.asset"))?;
    }
    Ok(ImageIn { r, kind, field })
}

/// Lista de imagens de um papel: de `min` a `max`, cada uma lida com [`image_in`]
/// (`field[i]`) e sem a mesma referência duas vezes.
pub(crate) fn image_list(
    ctx: &Ctx,
    items: &[Value],
    kind: ImageKind,
    (min, max): (usize, usize),
    what: &str,
    field: &str,
) -> Result<Vec<ImageIn>, ToolError> {
    count(items.len(), min, max, what, field)?;
    let mut out: Vec<ImageIn> = Vec::new();
    for (i, v) in items.iter().enumerate() {
        let img = image_in(ctx, v, kind, format!("{field}[{i}]"))?;
        if out.iter().any(|x| x.r == img.r) {
            return Err(
                invalid(format!("{}: a imagem aparece mais de uma vez.", img.field))
                    .with_field(img.field),
            );
        }
        out.push(img);
    }
    Ok(out)
}

/// Aviso das criações com assets novos.
pub(crate) const ASSETS_WARNING: &str = "Os assets novos (textos, imagens e vídeos) ficam na \
     conta: o Google não remove assets, então desfazer remove só a campanha e o orçamento.";

/// Origens das imagens, sem repetição, na ordem em que aparecem (para o resumo).
pub(crate) fn image_sources<'a>(imgs: impl IntoIterator<Item = &'a ImageIn>) -> Vec<String> {
    let mut sources: Vec<String> = Vec::new();
    for img in imgs {
        let s = image_source_label(&img.r);
        if !sources.contains(&s) {
            sources.push(s);
        }
    }
    sources
}

/// Total de bytes de imagens novas por chamada (30 MB).
pub(crate) const MAX_NEW_IMAGE_BYTES: usize = 30 * 1024 * 1024;

/// Origem de uma imagem, para o resumo: o caminho do arquivo, o host da URL ou o id
/// do asset existente.
pub(crate) fn image_source_label(r: &ImageRef) -> String {
    match r {
        ImageRef::File { file_path } => file_path.clone(),
        ImageRef::Url { url } => url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| url.clone()),
        ImageRef::Asset { asset } => {
            format!("asset {}", asset.rsplit('/').next().unwrap_or_default())
        }
    }
}

/// Referência, nome do asset e dimensões (só das imagens novas).
type PooledRef = (ImageRef, String, Option<(u32, u32)>);

/// Imagens de uma criação. A mesma referência é carregada uma vez; os mesmos bytes
/// (por SHA-256, venham de arquivo ou URL) viram um asset só, com o nome
/// [`LoadedImage::content_name`](adsconsole_google_ads::images::LoadedImage::content_name).
/// Os bytes novos somam no máximo [`MAX_NEW_IMAGE_BYTES`].
#[derive(Default)]
pub(crate) struct ImagePool {
    refs: Vec<PooledRef>,
    /// (SHA-256, nome do asset) das imagens novas.
    hashes: Vec<(String, String)>,
    new_bytes: usize,
}

impl ImagePool {
    /// Nome do asset da imagem `r` no papel `kind`. Imagem nova: carregada e
    /// conferida ([`load_image`]) e, se os bytes ainda não vieram, criada em `ops`.
    pub(crate) async fn resolve(
        &mut self,
        ctx: &Ctx,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        r: &ImageRef,
        kind: ImageKind,
        field: &str,
    ) -> Result<String, ToolError> {
        if let Some((_, rn, dims)) = self.refs.iter().find(|(x, ..)| x == r) {
            if let Some((w, h)) = dims {
                check_kind(kind, *w, *h, field)?;
            }
            return Ok(rn.clone());
        }
        let loaded = load_image(ctx, r, kind, field).await?;
        self.register(r.clone(), loaded, temps, ops, field)
    }

    /// Como [`Self::resolve`], com o papel escolhido pela imagem: o primeiro de `kinds`
    /// em que ela cabe. Asset existente: papel `None` (desconhecido sem carregar).
    pub(crate) async fn resolve_fit(
        &mut self,
        ctx: &Ctx,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        r: &ImageRef,
        kinds: &[ImageKind],
        field: &str,
    ) -> Result<(String, Option<ImageKind>), ToolError> {
        if let Some((_, rn, dims)) = self.refs.iter().find(|(x, ..)| x == r) {
            let kind = dims
                .map(|(w, h)| first_fit(kinds, w, h, field))
                .transpose()?;
            return Ok((rn.clone(), kind));
        }
        let loaded = load_unchecked(ctx, r, field).await?;
        let kind = match &loaded {
            Loaded::New(img) => Some(first_fit(kinds, img.width, img.height, field)?),
            Loaded::Existing(_) => None,
        };
        let rn = self.register(r.clone(), loaded, temps, ops, field)?;
        Ok((rn, kind))
    }

    /// `[{"asset": rn}]` (`AdImageAsset`) das imagens, cada uma no seu papel; os mesmos
    /// bytes duas vezes na lista são recusados.
    pub(crate) async fn resolve_list(
        &mut self,
        ctx: &Ctx,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        imgs: &[ImageIn],
    ) -> Result<Value, ToolError> {
        let mut rns: Vec<String> = Vec::new();
        for img in imgs {
            let rn = self
                .resolve(ctx, temps, ops, &img.r, img.kind, &img.field)
                .await?;
            if rns.contains(&rn) {
                return Err(invalid(format!(
                    "{}: a mesma imagem aparece mais de uma vez.",
                    img.field
                ))
                .with_field(img.field.clone()));
            }
            rns.push(rn);
        }
        Ok(Value::Array(
            rns.into_iter().map(|rn| json!({ "asset": rn })).collect(),
        ))
    }

    /// Registra uma imagem já carregada e conferida; devolve o nome do asset.
    pub(crate) fn register(
        &mut self,
        r: ImageRef,
        loaded: Loaded,
        temps: &mut TempIds,
        ops: &mut Vec<Value>,
        field: &str,
    ) -> Result<String, ToolError> {
        let (rn, dims) = match loaded {
            Loaded::Existing(rn) => (rn, None),
            Loaded::New(img) => {
                let hash = img.sha256_hex();
                let dims = Some((img.width, img.height));
                match self.hashes.iter().find(|(h, _)| *h == hash) {
                    Some((_, rn)) => (rn.clone(), dims),
                    None => {
                        self.new_bytes += img.bytes.len();
                        if self.new_bytes > MAX_NEW_IMAGE_BYTES {
                            let top = field.split(['.', '[']).next().unwrap_or(field);
                            return Err(invalid(format!(
                                "{field}: as imagens novas desta chamada passam de 30 MB \
                                 no total; use imagens menores ou assets já existentes."
                            ))
                            .with_field(top));
                        }
                        let rn = temps.next("assets");
                        ops.push(images::image_asset_op(&rn, &img.content_name(), &img));
                        self.hashes.push((hash, rn.clone()));
                        (rn, dims)
                    }
                }
            }
        };
        self.refs.push((r, rn.clone(), dims));
        Ok(rn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidding_parses_name_and_struct_forms() {
        let parse = |v: Value| parse_bidding(Some(&v));
        assert!(parse(json!("manual_cpc")).unwrap().is_manual());
        assert!(parse(json!({ "manual_cpc": {} })).unwrap().is_manual());
        assert!(matches!(
            parse(json!("maximize_clicks")).unwrap(),
            Bidding::MaximizeClicks { cpc_cap: None }
        ));
        assert!(matches!(
            parse_bidding(None).unwrap(),
            Bidding::MaximizeClicks { cpc_cap: None }
        ));
        assert!(parse(Value::Null).is_ok());
        for bad in [
            json!("lance_magico"),
            json!(3),
            json!({ "maximize_clicks": { "cpc_cap": "x" } }),
        ] {
            let e = parse(bad.clone()).unwrap_err();
            assert_eq!(e.code, crate::error::codes::INVALID_INPUT, "{bad}");
            assert_eq!(e.field.as_deref(), Some("bidding"), "{bad}");
        }
        let b: Bidding =
            serde_json::from_value(json!({ "maximize_clicks": { "cpc_cap": 1.5 } })).unwrap();
        assert_eq!(
            b.api_field("BRL").unwrap(),
            ("targetSpend", json!({ "cpcBidCeilingMicros": "1500000" }))
        );
        assert_eq!(
            Bidding::default().api_field("BRL").unwrap(),
            ("targetSpend", json!({}))
        );
    }

    #[test]
    fn campaign_base_is_paused() {
        let c = campaign_base(
            "customers/1/campaigns/-2",
            "customers/1/campaignBudgets/-1",
            "X",
            "PERFORMANCE_MAX",
            &Bidding::MaximizeConversions { target_cpa: None },
            "BRL",
        )
        .unwrap();
        assert_eq!(c["status"], "PAUSED");
        assert_eq!(c["containsEuPoliticalAdvertising"], EU_POLITICAL);
        assert_eq!(c["maximizeConversions"], json!({}));
    }

    #[test]
    fn budget_name_is_bounded_and_timestamped() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-17T13:04:05.678Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            budget_name_at("Busca", now),
            "Busca (orçamento 20261017-130405678)"
        );
        // Nome no limite, com caracteres de 2 e 3 bytes: corta em fronteira de caractere.
        for name in [
            "ã".repeat(127),
            format!("a{}", "€".repeat(84)),
            "x".repeat(255),
        ] {
            assert!(name.len() >= 253, "{}", name.len());
            let b = budget_name_at(&name, now);
            assert!(b.len() <= MAX_NAME_BYTES, "{} bytes", b.len());
            assert!(b.ends_with(" (orçamento 20261017-130405678)"), "{b}");
            assert!(name.starts_with(b.split(" (orçamento ").next().unwrap()));
            // String válida em UTF-8 (as bytes voltam a ser str).
            assert!(std::str::from_utf8(b.as_bytes()).is_ok());
        }
    }

    #[test]
    fn name_limit_is_in_bytes() {
        assert!(name_text(&"ã".repeat(127), "name").is_ok()); // 254 bytes
        let e = name_text(&"ã".repeat(128), "name").unwrap_err(); // 256 bytes, 128 chars
        assert_eq!(e.field.as_deref(), Some("name"));
        assert!(name_text("  ", "ad_groups[0].name").is_err());
    }

    #[test]
    fn dates_are_set_by_path_segments() {
        let mut c = json!({});
        campaign_dates(&mut c, Some("2026-11-01"), Some("2026-12-31")).unwrap();
        assert_eq!(
            c,
            json!({ "startDateTime": "2026-11-01 00:00:00", "endDateTime": "2026-12-31 23:59:59" })
        );
        let mut c = json!({});
        set_path(&mut c, &["a", "b"], json!(1));
        assert_eq!(c, json!({ "a": { "b": 1 } }));
        let e = campaign_dates(&mut c, Some("2026-12-01"), Some("2026-11-01")).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("end_date"));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    /// Os mesmos bytes por arquivo e por URL viram um asset só, nomeado pelo conteúdo.
    #[tokio::test]
    async fn same_bytes_from_file_and_url_are_one_asset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(png(300, 300), "image/png"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("logo.png");
        std::fs::write(&file, png(300, 300)).unwrap();
        let by_file = ImageRef::File {
            file_path: file.to_string_lossy().into_owned(),
        };
        let by_url = ImageRef::Url {
            url: format!("{}/logo.png", server.uri()),
        };
        let client = reqwest::Client::new();
        let mut pool = ImagePool::default();
        let mut temps = TempIds::new("1");
        let mut ops = Vec::new();
        let mut rns = Vec::new();
        for r in [&by_file, &by_url] {
            let loaded = images::load_with(&client, r, images::AllowHttp::ForTests)
                .await
                .unwrap();
            rns.push(
                pool.register(r.clone(), loaded, &mut temps, &mut ops, "brand.logo")
                    .unwrap(),
            );
        }
        assert_eq!(rns[0], rns[1]);
        assert_eq!(ops.len(), 1, "{ops:?}");
        let name = ops[0]["assetOperation"]["create"]["name"].as_str().unwrap();
        assert!(name.starts_with("img-") && name.len() == 20, "{name}");
    }

    #[test]
    fn new_image_bytes_are_capped_per_call() {
        let mut pool = ImagePool::default();
        let mut temps = TempIds::new("1");
        let mut ops = Vec::new();
        let five_mb = images::MAX_IMAGE_BYTES;
        for i in 0..7u8 {
            let img = images::LoadedImage {
                bytes: vec![i; five_mb],
                width: 1200,
                height: 1200,
                mime: "image/png".into(),
            };
            let r = ImageRef::File {
                file_path: format!("/img{i}.png"),
            };
            let field = format!("asset_groups[0].images.square[{i}]");
            let out = pool.register(r, Loaded::New(img), &mut temps, &mut ops, &field);
            if i < 6 {
                out.unwrap(); // 6 x 5 MB = 30 MB: ainda cabe
            } else {
                let e = out.unwrap_err();
                assert_eq!(e.field.as_deref(), Some("asset_groups"), "{e:?}");
                assert!(e.message.contains("30 MB"), "{}", e.message);
            }
        }
        assert_eq!(ops.len(), 6);
    }

    #[test]
    fn image_sources_for_the_summary() {
        let f = |v: Value| image_source_label(&ImageRef::from_value(&v).unwrap());
        assert_eq!(f(json!({ "file_path": "/a/b.png" })), "/a/b.png");
        assert_eq!(
            f(json!({ "url": "https://cdn.exemplo.com/x.png?y=1" })),
            "cdn.exemplo.com"
        );
        assert_eq!(f(json!({ "asset": "customers/1/assets/77" })), "asset 77");
    }

    #[test]
    fn duplicate_location_is_refused() {
        let e =
            geo_lang_ops("customers/1/campaigns/-2", &["1".into(), "1".into()], &[]).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("locations[1]"));
    }
}
