//! Generic Meta write: a [`MetaPlan`] describes sequential steps (create, update,
//! delete), with `$ref:<key>` references to objects created earlier, and the fields to
//! watch. From it come validate_only, snapshot, execution with progress and
//! compensating rollback, current state and the inverse operation.

use std::collections::BTreeMap;

use adsconsole_core::db::InverseOp;
use adsconsole_meta_ads::client::WriteKind;
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::fields::{self, ObjectKind};
use adsconsole_meta_ads::upload::{
    ImageSource, VideoState, image_created_time, load_image, upload_image, upload_video,
    video_status,
};
use serde_json::{Map, Value, json};

use super::ctx::Ctx;
use super::errors::translate;
use crate::adapter::{Applied, Progress, Validated};
use crate::engine::internal;
use crate::error::{ToolError, codes};

pub(crate) const REF_PREFIX: &str = "$ref:";
pub(crate) const RESTORE_FIELDS: &str = "restore_fields";
pub(crate) const DELETE_OBJECTS: &str = "delete_objects";
pub(crate) const DELETE_IMAGE: &str = "delete_image";

/// Who deletes an object created by the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Owner {
    /// Top of a tree: deleted on rollback and on undo.
    Root,
    /// Deleted along with its parent (`key` of another `Create` in the plan).
    Under(String),
    /// Stays in the account on undo (e.g. creatives); only rollback deletes it.
    Keep,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    Create {
        key: String,
        /// Relative to the Graph root, e.g. `act_1/campaigns`.
        path: String,
        params: Map<String, Value>,
        owner: Owner,
    },
    Update {
        id: String,
        params: Map<String, Value>,
    },
    Delete {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldChange {
    pub object_id: String,
    pub kind: ObjectKind,
    pub field: &'static str,
    /// Normalized (tool format).
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Watch {
    /// Real id or `$ref:<key>`.
    pub object_id: String,
    pub kind: ObjectKind,
    pub fields: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum InverseSpec {
    RestoreFields,
    DeleteCreated,
    #[default]
    None,
}

/// Writes that are not a sequence of plain POSTs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DirectExec {
    GraphPost {
        path: String,
        params: Map<String, Value>,
    },
    ImageUpload {
        source: ImageSource,
        name: Option<String>,
    },
    VideoUpload {
        path: std::path::PathBuf,
        title: String,
        wait_ready: bool,
    },
    Copy {
        object_id: String,
        kind: ObjectKind,
        params: Map<String, Value>,
        /// More than 3 ads: async request with polling.
        asynchronous: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MetaPlan {
    pub summary: String,
    pub warning: Option<String>,
    pub steps: Vec<Step>,
    pub field_changes: Vec<FieldChange>,
    pub watch: Vec<Watch>,
    pub inverse: InverseSpec,
    pub direct: Option<DirectExec>,
    /// Ids of ad sets whose budget changes (limit of 4 per hour).
    pub budget_objects: Vec<String>,
}

pub(crate) fn kind_str(k: ObjectKind) -> &'static str {
    match k {
        ObjectKind::Campaign => "campaign",
        ObjectKind::AdSet => "adset",
        ObjectKind::Ad => "ad",
    }
}

pub(crate) fn parse_kind(s: &str) -> Option<ObjectKind> {
    match s {
        "campaign" => Some(ObjectKind::Campaign),
        "adset" => Some(ObjectKind::AdSet),
        "ad" => Some(ObjectKind::Ad),
        _ => None,
    }
}

fn has_ref(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains(REF_PREFIX),
        Value::Array(a) => a.iter().any(has_ref),
        Value::Object(m) => m.values().any(has_ref),
        _ => false,
    }
}

fn step_has_ref(step: &Step) -> bool {
    match step {
        Step::Create { path, params, .. } => {
            path.contains(REF_PREFIX) || params.values().any(has_ref)
        }
        Step::Update { id, params } => id.contains(REF_PREFIX) || params.values().any(has_ref),
        Step::Delete { id } => id.contains(REF_PREFIX),
    }
}

/// Replaces `$ref:<key>` with the real ids in any string.
fn resolve_str(s: &str, refs: &BTreeMap<String, String>) -> String {
    if !s.contains(REF_PREFIX) {
        return s.to_string();
    }
    let mut out = s.to_string();
    // Longest keys first: `$ref:ad_1_1` must not become `<id of ad_1>_1`.
    let mut keys: Vec<&String> = refs.keys().collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    for k in keys {
        out = out.replace(&format!("{REF_PREFIX}{k}"), &refs[k]);
    }
    out
}

fn resolve(v: &Value, refs: &BTreeMap<String, String>) -> Value {
    match v {
        Value::String(s) => Value::String(resolve_str(s, refs)),
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve(x, refs)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), resolve(x, refs)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn resolve_map(m: &Map<String, Value>, refs: &BTreeMap<String, String>) -> Map<String, Value> {
    m.iter()
        .map(|(k, v)| (k.clone(), resolve(v, refs)))
        .collect()
}

/// Local rules + validate_only of the steps that do not depend on objects of the plan.
pub(crate) async fn validate(
    ctx: &Ctx,
    tool: &str,
    plan: &MetaPlan,
) -> Result<Validated, ToolError> {
    check_budget_limit(ctx, plan)?;
    let mut deferred = false;
    if plan.direct.is_none() {
        for step in &plan.steps {
            if step_has_ref(step) {
                deferred = true;
                continue;
            }
            let res = match step {
                Step::Create { path, params, .. } => {
                    let mut p = params.clone();
                    p.insert("execution_options".into(), json!(["validate_only"]));
                    ctx.client.post(path, &p, WriteKind::Validate).await
                }
                Step::Update { id, params } => {
                    let mut p = params.clone();
                    p.insert("execution_options".into(), json!(["validate_only"]));
                    ctx.client.post(id, &p, WriteKind::Validate).await
                }
                // Only a plain id can be checked with GET (the video is deleted via `advideos?video_id=`).
                Step::Delete { id } if id.contains(['/', '?']) => continue,
                Step::Delete { id } => ctx.client.get(id, &[("fields", "id".into())]).await,
            };
            res.map_err(|e| translate(e, Some(ctx), tool))?;
        }
    }
    let mut warnings: Vec<String> = Vec::new();
    if let Some(w) = &plan.warning {
        warnings.push(w.clone());
    }
    if deferred {
        warnings.push(
            "Meta validates the next steps only at creation time; if one fails, what was already \
             created is deleted."
                .into(),
        );
    }
    if let Some(w) = &ctx.token_warning {
        warnings.push(w.clone());
    }
    Ok(Validated {
        summary: plan.summary.clone(),
        warning: (!warnings.is_empty()).then(|| warnings.join(" ")),
    })
}

/// Budget changes per ad set per hour (Meta limit).
const BUDGET_CHANGES_PER_HOUR: usize = 4;

/// Rejects the 5th budget change of an ad set within one hour, before calling the API.
fn check_budget_limit(ctx: &Ctx, plan: &MetaPlan) -> Result<(), ToolError> {
    let now = chrono::Utc::now();
    for id in &plan.budget_objects {
        let recent = ctx
            .db
            .budget_changes_since(id, now - chrono::Duration::hours(1))
            .map_err(internal)?;
        if recent.len() >= BUDGET_CHANGES_PER_HOUR {
            let free_at = (recent[recent.len() - BUDGET_CHANGES_PER_HOUR]
                + chrono::Duration::hours(1))
            .with_timezone(&chrono::Local)
            .format("%H:%M");
            let field = plan
                .field_changes
                .iter()
                .find(|fc| &fc.object_id == id && fc.field.ends_with("_budget"))
                .map_or("daily_budget", |fc| fc.field);
            return Err(ToolError::new(
                codes::BUDGET_CHANGE_LIMIT,
                format!(
                    "Meta allows only 4 budget changes per hour on this ad set. The next one is allowed at {free_at}."
                ),
            )
            .with_field(field)
            .with_details(json!({ "adset_id": id })));
        }
    }
    Ok(())
}

/// Reads `fields` of `id` and normalizes them (`{field: value}`).
async fn read_fields(
    ctx: &Ctx,
    tool: &str,
    id: &str,
    kind: ObjectKind,
    names: &[&'static str],
) -> Result<Map<String, Value>, ToolError> {
    let defs: Vec<&fields::EditableField> = names
        .iter()
        .map(|n| fields::find(n, kind).ok_or_else(|| internal(format!("field {n} in {kind:?}"))))
        .collect::<Result<_, _>>()?;
    let wanted = defs.iter().map(|f| f.read).collect::<Vec<_>>().join(",");
    let obj = ctx
        .client
        .get(id, &[("fields", wanted)])
        .await
        .map_err(|e| translate(e, Some(ctx), tool))?;
    Ok(defs
        .iter()
        .map(|f| (f.name.to_string(), fields::normalize(f, &obj, ctx.exponent)))
        .collect())
}

/// `{id: {field: value}}` of the fields the plan changes, before the change.
pub(crate) async fn snapshot(ctx: &Ctx, tool: &str, plan: &MetaPlan) -> Result<Value, ToolError> {
    let mut by_object: BTreeMap<(String, &'static str), (ObjectKind, Vec<&'static str>)> =
        BTreeMap::new();
    for fc in &plan.field_changes {
        let entry = by_object
            .entry((fc.object_id.clone(), kind_str(fc.kind)))
            .or_insert((fc.kind, Vec::new()));
        if !entry.1.contains(&fc.field) {
            entry.1.push(fc.field);
        }
    }
    let mut out = Map::new();
    for ((id, _), (kind, names)) in by_object {
        out.insert(
            id.clone(),
            Value::Object(read_fields(ctx, tool, &id, kind, &names).await?),
        );
    }
    Ok(Value::Object(out))
}

/// Watch with the ids already resolved, in the format stored in `InverseOp.target`.
fn watch_target(watch: &[Watch], refs: &BTreeMap<String, String>) -> Value {
    Value::Array(
        watch
            .iter()
            .map(|w| {
                json!({
                    "object_id": resolve_str(&w.object_id, refs),
                    "kind": kind_str(w.kind),
                    "fields": w.fields,
                })
            })
            .collect(),
    )
}

/// Reads the objects of a target (`{"watch": [...]}`) in the `after` format.
pub(crate) async fn current_state(
    ctx: &Ctx,
    tool: &str,
    target: &Value,
) -> Result<Value, ToolError> {
    // An image does not change after upload: its state is what was stored at upload.
    if let Some(image) = target.get("image") {
        return Ok(image.clone());
    }
    let mut out = Map::new();
    for w in target["watch"].as_array().cloned().unwrap_or_default() {
        let id = w["object_id"]
            .as_str()
            .ok_or_else(|| internal("watch without object_id"))?;
        let kind = parse_kind(w["kind"].as_str().unwrap_or_default())
            .ok_or_else(|| internal("watch without kind"))?;
        let names: Vec<&'static str> = w["fields"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|n| fields::find(n, kind).map(|f| f.name))
                    .collect()
            })
            .unwrap_or_default();
        out.insert(
            id.to_string(),
            Value::Object(read_fields(ctx, tool, id, kind, &names).await?),
        );
    }
    Ok(Value::Object(out))
}

struct Created {
    key: String,
    id: String,
    owner: Owner,
    kind: String,
}

fn progress_value(created: &[Created]) -> Value {
    Value::Array(
        created
            .iter()
            .map(|c| json!({ "key": c.key, "id": c.id, "kind": c.kind }))
            .collect(),
    )
}

/// Object type from the creation path (`act_1/adsets` → `adset`).
fn kind_of_path(path: &str) -> String {
    let edge = path.rsplit('/').next().unwrap_or(path);
    edge.strip_suffix('s').unwrap_or(edge).to_string()
}

/// Ids to delete, in reverse creation order. On rollback (`all`), everything that was
/// created, child by child; on undo, only the top of each tree (deleting the campaign
/// deletes its ad sets and ads) and nothing that is `Keep`.
fn deletable(created: &[Created], all: bool) -> Vec<String> {
    let keys: Vec<&str> = created.iter().map(|c| c.key.as_str()).collect();
    created
        .iter()
        .rev()
        .filter(|c| {
            all || match &c.owner {
                Owner::Root => true,
                Owner::Keep => false,
                Owner::Under(parent) => !keys.contains(&parent.as_str()),
            }
        })
        .map(|c| c.id.clone())
        .collect()
}

/// Name the step would give the object, for messages.
fn step_name(step: &Step) -> Option<String> {
    let params = match step {
        Step::Create { params, .. } | Step::Update { params, .. } => params,
        Step::Delete { .. } => return None,
    };
    params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn step_label(step: &Step, refs: &BTreeMap<String, String>) -> String {
    match step {
        Step::Create { key, .. } => key.clone(),
        Step::Update { id, .. } | Step::Delete { id } => resolve_str(id, refs),
    }
}

/// Deletes what was created; returns (deleted, orphans).
async fn rollback(ctx: &Ctx, created: &[Created]) -> (Vec<String>, Vec<String>) {
    let mut done = Vec::new();
    let mut orphans = Vec::new();
    let client = ctx.client.bypass_limiter();
    for id in deletable(created, true) {
        match client.delete(&id).await {
            Ok(_) => done.push(id),
            Err(e) => {
                tracing::error!(object_id = id.as_str(), error = %e, "rollback failed");
                orphans.push(id);
            }
        }
    }
    (done, orphans)
}

fn merge_details(mut err: ToolError, extra: Value) -> ToolError {
    let mut details = match err.details.take() {
        Some(Value::Object(m)) => m,
        Some(other) => {
            let mut m = Map::new();
            m.insert("details".into(), other);
            m
        }
        None => Map::new(),
    };
    if let Value::Object(e) = extra {
        details.extend(e);
    }
    err.details = Some(Value::Object(details));
    err
}

/// Error of a step, after the rollback.
async fn fail_step(
    ctx: &Ctx,
    tool: &str,
    step: &Step,
    refs: &BTreeMap<String, String>,
    created: &[Created],
    cause: MetaError,
) -> ToolError {
    let maybe = matches!(
        cause,
        MetaError::MaybeApplied { .. }
            | MetaError::Api {
                maybe_applied: true,
                ..
            }
    );
    let label = step_label(step, refs);
    let mut err = translate(cause, Some(ctx), tool);
    if maybe {
        let name = step_name(step).unwrap_or_else(|| label.clone());
        err.message = format!(
            "Meta did not confirm step {label} ('{name}'); check in the account whether this object was created."
        );
    }
    let (rolled_back, orphans) = rollback(ctx, created).await;
    if !orphans.is_empty() {
        let cause = serde_json::to_value(&err).unwrap_or(Value::Null);
        return ToolError::new(
            codes::PARTIAL_ROLLBACK,
            "The creation failed and not everything already created could be deleted.",
        )
        .with_details(json!({
            "orphaned_ids": orphans,
            "rolled_back": rolled_back,
            "failed_step": label,
            "cause": cause,
        }))
        .with_suggestion("Delete these objects in Ads Manager.");
    }
    merge_details(
        err,
        json!({ "failed_step": label, "rolled_back": rolled_back }),
    )
}

/// `after` as read; if the read fails, the value expected from the step parameters.
async fn read_after(
    ctx: &Ctx,
    tool: &str,
    plan: &MetaPlan,
    target: &Value,
    refs: &BTreeMap<String, String>,
) -> Value {
    match current_state(ctx, tool, target).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e.message, "failed to read the state after; using the expected one");
            expected_after(plan, refs)
        }
    }
}

fn expected_after(plan: &MetaPlan, refs: &BTreeMap<String, String>) -> Value {
    let mut out = Map::new();
    let created: BTreeMap<String, &Map<String, Value>> = plan
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::Create { key, params, .. } => {
                Some((resolve_str(&format!("{REF_PREFIX}{key}"), refs), params))
            }
            _ => None,
        })
        .collect();
    for w in &plan.watch {
        let id = resolve_str(&w.object_id, refs);
        let mut obj = Map::new();
        for f in &w.fields {
            let v = plan
                .field_changes
                .iter()
                .find(|fc| fc.object_id == w.object_id && fc.field == *f)
                .map(|fc| fc.value.clone())
                .or_else(|| created.get(&id).and_then(|p| p.get(*f).cloned()))
                .unwrap_or(Value::Null);
            obj.insert(f.to_string(), v);
        }
        out.insert(id, Value::Object(obj));
    }
    Value::Object(out)
}

/// Types of the `before` objects, so the inverse can convert the values.
fn kinds_of(plan: &MetaPlan) -> Value {
    let mut m = Map::new();
    for fc in &plan.field_changes {
        m.insert(fc.object_id.clone(), json!(kind_str(fc.kind)));
    }
    Value::Object(m)
}

/// Does the plan fill any field that was empty in "before"?
fn filled_empty(plan: &MetaPlan, before: &Value) -> bool {
    plan.field_changes
        .iter()
        .any(|fc| before[&fc.object_id][fc.field].is_null())
}

pub(crate) async fn execute(
    ctx: &Ctx,
    tool: &str,
    plan: &MetaPlan,
    before: &Value,
    progress: &dyn Progress,
) -> Result<Applied, ToolError> {
    // Again here, under the account mutex: in approval mode (and in concurrent calls)
    // `validate` counted before the other changes went in.
    check_budget_limit(ctx, plan)?;
    match &plan.direct {
        Some(DirectExec::GraphPost { path, params }) => {
            let out = ctx
                .client
                .post(path, params, WriteKind::Write)
                .await
                .map_err(|e| translate(e, Some(ctx), tool))?;
            return Ok(Applied {
                after: super::tools::scrub(out),
                inverse: None,
            });
        }
        Some(DirectExec::ImageUpload { source, name }) => {
            let bytes = load_image(source)
                .await
                .map_err(|e| translate(e, Some(ctx), tool))?;
            // Meta reuses identical images: if the image was already in the library, the
            // upload returns that one, and undo would delete the user's image. The image is
            // new only if the library says it was created just now (with clock slack).
            let started = chrono::Utc::now() - chrono::Duration::minutes(2);
            let mut out = upload_image(&ctx.client, &ctx.account_id, &bytes, name.as_deref())
                .await
                .map_err(|e| translate(e, Some(ctx), tool))?;
            let hash = out["image_hash"].as_str().map(str::to_string);
            let created = match hash.as_deref() {
                Some(h) => image_created_time(&ctx.client, &ctx.account_id, h)
                    .await
                    .unwrap_or(None),
                None => None,
            };
            let notice = match created {
                Some(t) if t >= started => None,
                Some(_) => Some(
                    "The image was already in the account library (Meta reuses identical \
                     images); undo does not delete it.",
                ),
                None => Some(
                    "Could not confirm that the image is new in the account library; undo \
                     does not delete it.",
                ),
            };
            if let Some(a) = notice {
                out["warning"] = json!(a);
            }
            let inverse = hash.filter(|_| notice.is_none()).map(|hash| InverseOp {
                tool: DELETE_IMAGE.into(),
                input: json!({ "hash": hash }),
                target: json!({ "image": out }),
            });
            return Ok(Applied {
                after: out,
                inverse,
            });
        }
        Some(DirectExec::VideoUpload {
            path,
            title,
            wait_ready,
        }) => return execute_video(ctx, tool, path, title, *wait_ready, progress).await,
        Some(DirectExec::Copy {
            object_id,
            kind,
            params,
            asynchronous,
        }) => {
            return execute_copy(ctx, tool, object_id, *kind, params, *asynchronous, progress)
                .await;
        }
        None => {}
    }
    let mut refs: BTreeMap<String, String> = BTreeMap::new();
    let mut created: Vec<Created> = Vec::new();
    for step in &plan.steps {
        let res = match step {
            Step::Create { path, params, .. } => {
                ctx.client
                    .post(
                        &resolve_str(path, &refs),
                        &resolve_map(params, &refs),
                        WriteKind::Write,
                    )
                    .await
            }
            Step::Update { id, params } => {
                ctx.client
                    .post(
                        &resolve_str(id, &refs),
                        &resolve_map(params, &refs),
                        WriteKind::Write,
                    )
                    .await
            }
            Step::Delete { id } => ctx.client.delete(&resolve_str(id, &refs)).await,
        };
        let out = match res {
            Ok(v) => v,
            Err(e) => return Err(fail_step(ctx, tool, step, &refs, &created, e).await),
        };
        if let Step::Create {
            key, path, owner, ..
        } = step
        {
            let Some(id) = out["id"].as_str().map(str::to_string) else {
                let body = super::scrub(out.clone()).to_string();
                let body: String = body.chars().take(300).collect();
                let e =
                    MetaError::Decode(format!("creating {key} returned no id (response: {body})"));
                return Err(fail_step(ctx, tool, step, &refs, &created, e).await);
            };
            refs.insert(key.clone(), id.clone());
            created.push(Created {
                key: key.clone(),
                id,
                owner: owner.clone(),
                kind: kind_of_path(path),
            });
            progress.created(&progress_value(&created));
        }
    }
    for id in &plan.budget_objects {
        let id = resolve_str(id, &refs);
        if let Err(e) = ctx.db.record_budget_change(&id, chrono::Utc::now()) {
            tracing::error!(object_id = id.as_str(), error = %e, "failed to record the budget change");
        }
    }
    let target = json!({ "watch": watch_target(&plan.watch, &refs) });
    let after = read_after(ctx, tool, plan, &target, &refs).await;
    let inverse = match &plan.inverse {
        // A field that was empty cannot go back to empty: no undo (the plan warning says which).
        InverseSpec::RestoreFields if filled_empty(plan, before) => None,
        InverseSpec::RestoreFields => Some(InverseOp {
            tool: RESTORE_FIELDS.into(),
            input: json!({ "updates": before, "kinds": kinds_of(plan) }),
            target,
        }),
        InverseSpec::DeleteCreated => {
            let ids = deletable(&created, false);
            (!ids.is_empty()).then(|| InverseOp {
                tool: DELETE_OBJECTS.into(),
                input: json!({ "ids": ids }),
                target,
            })
        }
        InverseSpec::None => None,
    };
    Ok(Applied { after, inverse })
}

async fn execute_video(
    ctx: &Ctx,
    tool: &str,
    path: &std::path::Path,
    title: &str,
    wait_ready: bool,
    progress: &dyn Progress,
) -> Result<Applied, ToolError> {
    let id = upload_video(&ctx.client, &ctx.account_id, path, title)
        .await
        .map_err(|e| translate(e, Some(ctx), tool))?;
    progress.created(&json!([{ "key": "video", "id": id, "kind": "video" }]));
    // Cleanup attempt if processing fails (Meta may refuse; see the end).
    let delete_path = format!("{}/advideos?video_id={id}", ctx.account_id);
    let mut status = "processing";
    if wait_ready {
        let deadline = tokio::time::Instant::now() + ctx.upload_poll.max;
        loop {
            match video_status(&ctx.client, &id).await {
                Ok(VideoState::Ready) => {
                    status = "ready";
                    break;
                }
                Ok(VideoState::Error(m)) => {
                    if let Err(e) = ctx.client.bypass_limiter().delete(&delete_path).await {
                        tracing::warn!(video_id = id.as_str(), error = %e, "failed to delete the failed video");
                    }
                    return Err(ToolError::new(
                        codes::EXECUTION_FAILED,
                        format!("Meta could not process the video: {m}"),
                    )
                    .with_details(json!({ "video_id": id })));
                }
                Ok(VideoState::Processing { .. }) => {}
                Err(e) => {
                    tracing::warn!(video_id = id.as_str(), error = %e, "failed to read the video status")
                }
            }
            if tokio::time::Instant::now() + ctx.upload_poll.every > deadline {
                break;
            }
            tokio::time::sleep(ctx.upload_poll.every).await;
        }
    }
    // No undo: Meta does not allow deleting account videos with the use case's
    // permissions (#10 on DELETE /act_…/advideos; DELETE /{video_id} requires pages_manage_posts).
    Ok(Applied {
        after: json!({ "video_id": id, "status": status }),
        inverse: None,
    })
}

/// Key of the copied id in the `/copies` response.
fn copied_key(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Campaign => "copied_campaign_id",
        ObjectKind::AdSet => "copied_adset_id",
        ObjectKind::Ad => "copied_ad_id",
    }
}

fn form_body(params: &Map<String, Value>) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        let v = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        ser.append_pair(k, &v);
    }
    ser.finish()
}

/// Waits for the async request to finish; returns the `/copies` result.
// VERIFY(v26.0): format of async_batch_requests and of each request's `result`.
async fn wait_async_copy(ctx: &Ctx, tool: &str, set_id: &str) -> Result<Value, ToolError> {
    let deadline = tokio::time::Instant::now() + ctx.upload_poll.max;
    loop {
        let v = ctx
            .client
            .get(
                set_id,
                &[(
                    "fields",
                    "status,success_count,error_count,requests{status,result}".into(),
                )],
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
        let status = v["status"].as_str().unwrap_or_default();
        if matches!(status, "COMPLETED" | "SUCCESS" | "FAILED" | "ERROR") {
            let req = &v["requests"]["data"][0];
            let result = match &req["result"] {
                Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                other => other.clone(),
            };
            let ok = req["status"]
                .as_str()
                .is_none_or(|s| s == "SUCCESS" || s == "COMPLETED");
            if ok && result.is_object() {
                return Ok(result);
            }
            return Err(ToolError::new(
                codes::EXECUTION_FAILED,
                "The async copy finished with an error at Meta.",
            )
            .with_details(json!({ "async_request_set_id": set_id, "result": result })));
        }
        if tokio::time::Instant::now() + ctx.upload_poll.every > deadline {
            return Err(ToolError::new(
                codes::EXECUTION_FAILED,
                "Meta is still making the copy; check the account in a few minutes.",
            )
            .with_details(json!({ "async_request_set_id": set_id })));
        }
        tokio::time::sleep(ctx.upload_poll.every).await;
    }
}

async fn execute_copy(
    ctx: &Ctx,
    tool: &str,
    object_id: &str,
    kind: ObjectKind,
    params: &Map<String, Value>,
    asynchronous: bool,
    progress: &dyn Progress,
) -> Result<Applied, ToolError> {
    let result = if asynchronous {
        let mut p = Map::new();
        p.insert(
            "name".into(),
            json!(format!("AdsConsole copy of {object_id}")),
        );
        p.insert(
            "adbatch".into(),
            json!([{ "name": "copy", "relative_url": format!("{object_id}/copies"), "body": form_body(params) }]),
        );
        let out = ctx
            .client
            .post(
                &format!("{}/async_batch_requests", ctx.account_id),
                &p,
                WriteKind::Write,
            )
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?;
        let set_id = out["id"]
            .as_str()
            .ok_or_else(|| internal("async_batch_requests without id"))?
            .to_string();
        wait_async_copy(ctx, tool, &set_id).await?
    } else {
        ctx.client
            .post(&format!("{object_id}/copies"), params, WriteKind::Write)
            .await
            .map_err(|e| translate(e, Some(ctx), tool))?
    };
    let copy = match &result[copied_key(kind)] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => {
            return Err(ToolError::new(
                codes::EXECUTION_FAILED,
                "Meta did not return the copy id; check the account.",
            )
            .with_details(json!({ "result": result })));
        }
    };
    progress.created(&json!([{ "key": "copy", "id": copy, "kind": kind_str(kind) }]));
    let watch = vec![Watch {
        object_id: copy.clone(),
        kind,
        fields: vec!["name", "status"],
    }];
    let target = json!({ "watch": watch_target(&watch, &BTreeMap::new()) });
    let after = match current_state(ctx, tool, &target).await {
        Ok(v) => v,
        Err(_) => json!({ copy.clone(): { "name": Value::Null, "status": "PAUSED" } }),
    };
    Ok(Applied {
        after,
        inverse: Some(InverseOp {
            tool: DELETE_OBJECTS.into(),
            input: json!({ "ids": [copy] }),
            target,
        }),
    })
}
