use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Db, conv_err, get_ts, parse_platform, text_err, ts};
use crate::error::CoreError;
use crate::platform::Platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Pending,
    /// Transient: approved, executing.
    Approved,
    Applied,
    Rejected,
    Expired,
    Failed,
    Undone,
}

impl ChangeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeStatus::Pending => "pending",
            ChangeStatus::Approved => "approved",
            ChangeStatus::Applied => "applied",
            ChangeStatus::Rejected => "rejected",
            ChangeStatus::Expired => "expired",
            ChangeStatus::Failed => "failed",
            ChangeStatus::Undone => "undone",
        }
    }

    pub fn parse(s: &str) -> Result<ChangeStatus, CoreError> {
        Ok(match s {
            "pending" => ChangeStatus::Pending,
            "approved" => ChangeStatus::Approved,
            "applied" => ChangeStatus::Applied,
            "rejected" => ChangeStatus::Rejected,
            "expired" => ChangeStatus::Expired,
            "failed" => ChangeStatus::Failed,
            "undone" => ChangeStatus::Undone,
            other => {
                return Err(CoreError::Config(format!("Unknown change status: {other}")));
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InverseOp {
    pub tool: String,
    pub input: Value,
    pub target: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub change_id: String,
    pub platform: Platform,
    pub account_id: String,
    pub connection_id: String,
    pub tool: String,
    pub input: Value,
    pub summary: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub inverse: Option<InverseOp>,
    pub reversible: bool,
    pub status: ChangeStatus,
    pub requested_by: Option<String>,
    pub undo_of: Option<String>,
    /// Undo with `force=true`: applies even if the object changed after the original.
    pub undo_force: bool,
    pub undone_by: Option<String>,
    pub error: Option<Value>,
    /// Progress of a multi-step write (e.g. `{"created": [..]}` on Meta).
    /// Stored by [`Db::record_progress`]; `update_change` overwrites it only when it is `Some`.
    #[serde(default)]
    pub progress: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeFilter {
    pub platform: Option<Platform>,
    pub account_id: Option<String>,
    pub status: Option<ChangeStatus>,
    pub limit: u32,
    pub offset: u32,
}

impl Default for ChangeFilter {
    fn default() -> Self {
        ChangeFilter {
            platform: None,
            account_id: None,
            status: None,
            limit: 20,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub change: ChangeRecord,
    pub warning: Option<String>,
    pub expires_at: DateTime<Utc>,
}

/// `"chg_"` + UUID v7 (sorts by time).
pub fn new_change_id() -> String {
    format!("chg_{}", uuid::Uuid::now_v7())
}

fn get_json(row: &Row, idx: usize) -> rusqlite::Result<Value> {
    let s: String = row.get(idx)?;
    serde_json::from_str(&s).map_err(|e| conv_err(idx, e))
}

fn get_opt_json(row: &Row, idx: usize) -> rusqlite::Result<Option<Value>> {
    let s: Option<String> = row.get(idx)?;
    s.map(|s| serde_json::from_str(&s).map_err(|e| conv_err(idx, e)))
        .transpose()
}

const COLUMNS: &str = "change_id, platform, account_id, connection_id, tool, input, summary, \
    before, after, inverse, reversible, status, requested_by, undo_of, undone_by, error, \
    created_at, updated_at, undo_force, progress";

fn row_to_change(row: &Row) -> rusqlite::Result<ChangeRecord> {
    let platform: String = row.get(1)?;
    let status: String = row.get(11)?;
    let inverse = match get_opt_json(row, 9)? {
        Some(v) => Some(serde_json::from_value(v).map_err(|e| conv_err(9, e))?),
        None => None,
    };
    Ok(ChangeRecord {
        change_id: row.get(0)?,
        platform: parse_platform(&platform).map_err(|e| text_err(1, e))?,
        account_id: row.get(2)?,
        connection_id: row.get(3)?,
        tool: row.get(4)?,
        input: get_json(row, 5)?,
        summary: row.get(6)?,
        before: get_opt_json(row, 7)?,
        after: get_opt_json(row, 8)?,
        inverse,
        reversible: row.get(10)?,
        status: ChangeStatus::parse(&status).map_err(|e| text_err(11, e))?,
        requested_by: row.get(12)?,
        undo_of: row.get(13)?,
        undone_by: row.get(14)?,
        error: get_opt_json(row, 15)?,
        created_at: get_ts(row, 16)?,
        updated_at: get_ts(row, 17)?,
        undo_force: row.get(18)?,
        progress: get_opt_json(row, 19)?,
    })
}

fn json_text(v: &Option<Value>) -> Option<String> {
    v.as_ref().map(Value::to_string)
}

fn inverse_text(c: &ChangeRecord) -> Result<Option<String>, CoreError> {
    c.inverse
        .as_ref()
        .map(|i| serde_json::to_string(i).map_err(|e| CoreError::Config(e.to_string())))
        .transpose()
}

impl Db {
    pub fn insert_change(&self, c: &ChangeRecord) -> Result<(), CoreError> {
        let inverse = inverse_text(c)?;
        self.conn().execute(
            &format!(
                "INSERT INTO changes ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)"
            ),
            params![
                c.change_id,
                c.platform.as_str(),
                c.account_id,
                c.connection_id,
                c.tool,
                c.input.to_string(),
                c.summary,
                json_text(&c.before),
                json_text(&c.after),
                inverse,
                c.reversible,
                c.status.as_str(),
                c.requested_by,
                c.undo_of,
                c.undone_by,
                json_text(&c.error),
                ts(&c.created_at),
                ts(&c.updated_at),
                c.undo_force,
                json_text(&c.progress),
            ],
        )?;
        Ok(())
    }

    pub fn get_change(&self, id: &str) -> Result<Option<ChangeRecord>, CoreError> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM changes WHERE change_id = ?1"),
                [id],
                row_to_change,
            )
            .optional()?)
    }

    /// Overwrites the mutable fields and sets `updated_at = now`.
    pub fn update_change(&self, c: &ChangeRecord) -> Result<(), CoreError> {
        let inverse = inverse_text(c)?;
        let n = self.conn().execute(
            "UPDATE changes SET summary=?2, before=?3, after=?4, inverse=?5, reversible=?6, \
             status=?7, requested_by=?8, undo_of=?9, undone_by=?10, error=?11, updated_at=?12, \
             progress=COALESCE(?13, progress) WHERE change_id=?1",
            params![
                c.change_id,
                c.summary,
                json_text(&c.before),
                json_text(&c.after),
                inverse,
                c.reversible,
                c.status.as_str(),
                c.requested_by,
                c.undo_of,
                c.undone_by,
                json_text(&c.error),
                ts(&Utc::now()),
                json_text(&c.progress),
            ],
        )?;
        if n == 0 {
            return Err(CoreError::NotFound(format!(
                "Change {} not found",
                c.change_id
            )));
        }
        Ok(())
    }

    /// Stores only the progress (and `updated_at`) of a change in flight.
    pub fn record_progress(&self, change_id: &str, progress: &Value) -> Result<(), CoreError> {
        let n = self.conn().execute(
            "UPDATE changes SET progress=?2, updated_at=?3 WHERE change_id=?1",
            params![change_id, progress.to_string(), ts(&Utc::now())],
        )?;
        if n == 0 {
            return Err(CoreError::NotFound(format!("Change {change_id} not found")));
        }
        Ok(())
    }

    /// Compare-and-set on the status; `true` if the row changed.
    pub fn transition_status(
        &self,
        id: &str,
        from: ChangeStatus,
        to: ChangeStatus,
    ) -> Result<bool, CoreError> {
        let n = self.conn().execute(
            "UPDATE changes SET status=?3, updated_at=?4 WHERE change_id=?1 AND status=?2",
            params![id, from.as_str(), to.as_str(), ts(&Utc::now())],
        )?;
        Ok(n > 0)
    }

    /// Marks the original as undone by `undone_by`, in a single step, if it is still
    /// `applied`; `true` if the row changed.
    pub fn mark_undone(&self, id: &str, undone_by: &str) -> Result<bool, CoreError> {
        let n = self.conn().execute(
            "UPDATE changes SET status='undone', undone_by=?2, updated_at=?3 \
             WHERE change_id=?1 AND status='applied'",
            params![id, undone_by, ts(&Utc::now())],
        )?;
        Ok(n > 0)
    }

    /// Newest first.
    pub fn list_changes(&self, f: &ChangeFilter) -> Result<Vec<ChangeRecord>, CoreError> {
        let mut clauses = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if let Some(p) = f.platform {
            args.push(p.as_str().to_string());
            clauses.push(format!("platform = ?{}", args.len()));
        }
        if let Some(a) = &f.account_id {
            args.push(a.clone());
            clauses.push(format!("account_id = ?{}", args.len()));
        }
        if let Some(s) = f.status {
            args.push(s.as_str().to_string());
            clauses.push(format!("status = ?{}", args.len()));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT {COLUMNS} FROM changes {where_sql} \
             ORDER BY created_at DESC, change_id DESC LIMIT {} OFFSET {}",
            f.limit, f.offset
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), row_to_change)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Undo in flight (status `pending` or `approved`) for the original change.
    pub fn find_undo_in_flight(
        &self,
        original_id: &str,
    ) -> Result<Option<ChangeRecord>, CoreError> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM changes WHERE undo_of = ?1 \
                     AND status IN ('pending','approved') LIMIT 1"
                ),
                [original_id],
                row_to_change,
            )
            .optional()?)
    }

    pub fn insert_pending(
        &self,
        change_id: &str,
        expires_at: DateTime<Utc>,
        warning: Option<&str>,
    ) -> Result<(), CoreError> {
        self.conn().execute(
            "INSERT INTO pending_approvals (change_id, warning, expires_at) VALUES (?1,?2,?3)",
            params![change_id, warning, ts(&expires_at)],
        )?;
        Ok(())
    }

    pub fn delete_pending(&self, change_id: &str) -> Result<(), CoreError> {
        self.conn().execute(
            "DELETE FROM pending_approvals WHERE change_id = ?1",
            [change_id],
        )?;
        Ok(())
    }

    pub fn list_pending(
        &self,
        platform: Option<Platform>,
    ) -> Result<Vec<PendingApproval>, CoreError> {
        let cols = COLUMNS
            .split(", ")
            .map(|c| format!("c.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols}, p.warning, p.expires_at FROM pending_approvals p \
             JOIN changes c ON c.change_id = p.change_id \
             WHERE (?1 IS NULL OR c.platform = ?1) ORDER BY c.created_at DESC, c.change_id DESC"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([platform.map(|p| p.as_str())], |row| {
            Ok(PendingApproval {
                change: row_to_change(row)?,
                warning: row.get(20)?,
                expires_at: get_ts(row, 21)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// IDs of pending approvals with `expires_at` before `now`.
    pub fn pending_expired_before(&self, now: DateTime<Utc>) -> Result<Vec<String>, CoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT change_id FROM pending_approvals WHERE expires_at < ?1 ORDER BY expires_at",
        )?;
        let rows = stmt.query_map([ts(&now)], |r| r.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::migrations::MIGRATIONS;
    use crate::platform::Platform;
    use chrono::{Duration, SubsecRound, Utc};
    use serde_json::json;

    fn sample(id: &str, status: ChangeStatus) -> ChangeRecord {
        let now = Utc::now();
        ChangeRecord {
            change_id: id.to_string(),
            platform: Platform::GoogleAds,
            account_id: "111".into(),
            connection_id: "conn_1".into(),
            tool: "pause_campaign".into(),
            input: json!({"campaign_id": "9", "nested": {"a": [1, 2, {"b": null}]}}),
            summary: "Pause campaign 9".into(),
            before: Some(json!({"status": "enabled"})),
            after: Some(json!({"status": "paused"})),
            inverse: Some(InverseOp {
                tool: "enable_campaign".into(),
                input: json!({"campaign_id": "9"}),
                target: json!({"kind": "campaign", "id": "9"}),
            }),
            reversible: true,
            status,
            requested_by: Some("claude".into()),
            undo_of: None,
            undo_force: false,
            undone_by: None,
            error: Some(json!({"code": "x", "detail": {"k": [1]}})),
            progress: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        drop(Db::open(&path).unwrap());
        let db = Db::open(&path).unwrap();
        assert_eq!(db.user_version().unwrap() as usize, MIGRATIONS.len());
    }

    #[test]
    fn insert_get_roundtrip_with_json() {
        let db = Db::open_in_memory().unwrap();
        let mut c = sample("chg_a", ChangeStatus::Pending);
        c.created_at = c.created_at.trunc_subsecs(6);
        c.updated_at = c.created_at;
        db.insert_change(&c).unwrap();
        assert_eq!(db.get_change("chg_a").unwrap().unwrap(), c);
        assert!(db.get_change("nope").unwrap().is_none());
    }

    #[test]
    fn update_overwrites_and_touches_updated_at() {
        let db = Db::open_in_memory().unwrap();
        let mut c = sample("chg_a", ChangeStatus::Pending);
        c.updated_at = Utc::now() - Duration::hours(1);
        db.insert_change(&c).unwrap();
        c.status = ChangeStatus::Applied;
        c.undone_by = Some("chg_b".into());
        db.update_change(&c).unwrap();
        let got = db.get_change("chg_a").unwrap().unwrap();
        assert_eq!(got.status, ChangeStatus::Applied);
        assert_eq!(got.undone_by.as_deref(), Some("chg_b"));
        assert!(got.updated_at > c.updated_at);
        assert!(matches!(
            db.update_change(&sample("missing", ChangeStatus::Pending)),
            Err(crate::CoreError::NotFound(_))
        ));
    }

    #[test]
    fn transition_is_compare_and_set() {
        let db = Db::open_in_memory().unwrap();
        db.insert_change(&sample("chg_a", ChangeStatus::Pending))
            .unwrap();
        let (p, a) = (ChangeStatus::Pending, ChangeStatus::Approved);
        assert!(db.transition_status("chg_a", p, a).unwrap());
        assert!(!db.transition_status("chg_a", p, a).unwrap());
        assert_eq!(
            db.get_change("chg_a").unwrap().unwrap().status,
            ChangeStatus::Approved
        );
    }

    #[test]
    fn mark_undone_only_from_applied() {
        let db = Db::open_in_memory().unwrap();
        db.insert_change(&sample("chg_a", ChangeStatus::Applied))
            .unwrap();
        assert!(db.mark_undone("chg_a", "chg_u1").unwrap());
        let got = db.get_change("chg_a").unwrap().unwrap();
        assert_eq!(got.status, ChangeStatus::Undone);
        assert_eq!(got.undone_by.as_deref(), Some("chg_u1"));
        // Second call changes nothing: the first undo stays recorded.
        assert!(!db.mark_undone("chg_a", "chg_u2").unwrap());
        assert_eq!(
            db.get_change("chg_a")
                .unwrap()
                .unwrap()
                .undone_by
                .as_deref(),
            Some("chg_u1")
        );
        db.insert_change(&sample("chg_p", ChangeStatus::Pending))
            .unwrap();
        assert!(!db.mark_undone("chg_p", "chg_u3").unwrap());
        let p = db.get_change("chg_p").unwrap().unwrap();
        assert_eq!(p.status, ChangeStatus::Pending);
        assert!(p.undone_by.is_none());
        assert!(!db.mark_undone("nope", "chg_u4").unwrap());
    }

    #[test]
    fn progress_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let c = sample("chg_p", ChangeStatus::Approved);
        db.insert_change(&c).unwrap();
        assert_eq!(db.get_change("chg_p").unwrap().unwrap().progress, None);
        let p = json!({"created": [{"key": "campaign", "id": "c1", "kind": "campaign"}]});
        db.record_progress("chg_p", &p).unwrap();
        let mut got = db.get_change("chg_p").unwrap().unwrap();
        assert_eq!(got.progress, Some(p.clone()));
        got.status = ChangeStatus::Failed;
        got.progress = None;
        db.update_change(&got).unwrap();
        assert_eq!(
            db.get_change("chg_p").unwrap().unwrap().progress,
            Some(p),
            "update_change preserves progress"
        );
        assert!(db.record_progress("nope", &json!({})).is_err());
    }

    #[test]
    fn undo_force_roundtrips() {
        let db = Db::open_in_memory().unwrap();
        let mut c = sample("chg_f", ChangeStatus::Pending);
        c.undo_of = Some("chg_a".into());
        c.undo_force = true;
        db.insert_change(&c).unwrap();
        assert!(db.get_change("chg_f").unwrap().unwrap().undo_force);
        db.insert_change(&sample("chg_g", ChangeStatus::Pending))
            .unwrap();
        assert!(!db.get_change("chg_g").unwrap().unwrap().undo_force);
    }

    #[test]
    fn list_filters_and_orders_newest_first() {
        let db = Db::open_in_memory().unwrap();
        let base = Utc::now() - Duration::hours(1);
        for (i, (id, acc)) in [("chg_1", "111"), ("chg_2", "111"), ("chg_3", "222")]
            .into_iter()
            .enumerate()
        {
            let mut c = sample(id, ChangeStatus::Applied);
            c.account_id = acc.into();
            c.created_at = base + Duration::minutes(i as i64);
            db.insert_change(&c).unwrap();
        }
        let f = ChangeFilter {
            account_id: Some("111".into()),
            ..Default::default()
        };
        let ids: Vec<_> = db
            .list_changes(&f)
            .unwrap()
            .into_iter()
            .map(|c| c.change_id)
            .collect();
        assert_eq!(ids, ["chg_2", "chg_1"]);
        let f = ChangeFilter {
            account_id: Some("111".into()),
            limit: 1,
            offset: 1,
            ..Default::default()
        };
        let got = db.list_changes(&f).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].change_id, "chg_1");
        let f = ChangeFilter {
            status: Some(ChangeStatus::Failed),
            platform: Some(Platform::GoogleAds),
            ..Default::default()
        };
        assert!(db.list_changes(&f).unwrap().is_empty());
        assert_eq!(ChangeFilter::default().limit, 20);
    }

    #[test]
    fn undo_in_flight_finds_pending_or_approved_only() {
        let db = Db::open_in_memory().unwrap();
        let mut u = sample("chg_u", ChangeStatus::Pending);
        u.undo_of = Some("chg_a".into());
        db.insert_change(&u).unwrap();
        assert!(db.find_undo_in_flight("chg_a").unwrap().is_some());
        db.transition_status("chg_u", ChangeStatus::Pending, ChangeStatus::Approved)
            .unwrap();
        assert!(db.find_undo_in_flight("chg_a").unwrap().is_some());
        db.transition_status("chg_u", ChangeStatus::Approved, ChangeStatus::Applied)
            .unwrap();
        assert!(db.find_undo_in_flight("chg_a").unwrap().is_none());
    }

    #[test]
    fn pending_queue_and_expiry() {
        let db = Db::open_in_memory().unwrap();
        db.insert_change(&sample("chg_a", ChangeStatus::Pending))
            .unwrap();
        let mut b = sample("chg_b", ChangeStatus::Pending);
        b.platform = Platform::MetaAds;
        db.insert_change(&b).unwrap();
        let now = Utc::now();
        db.insert_pending("chg_a", now - Duration::minutes(1), Some("aviso"))
            .unwrap();
        db.insert_pending("chg_b", now + Duration::hours(1), None)
            .unwrap();
        assert_eq!(db.pending_expired_before(now).unwrap(), ["chg_a"]);
        let all = db.list_pending(None).unwrap();
        assert_eq!(all.len(), 2);
        let a = all.iter().find(|p| p.change.change_id == "chg_a").unwrap();
        assert_eq!(a.warning.as_deref(), Some("aviso"));
        assert_eq!(db.list_pending(Some(Platform::MetaAds)).unwrap().len(), 1);
        db.delete_pending("chg_a").unwrap();
        assert_eq!(db.list_pending(None).unwrap().len(), 1);
    }

    #[test]
    fn pending_row_deleted_with_change() {
        let db = Db::open_in_memory().unwrap();
        db.insert_change(&sample("chg_a", ChangeStatus::Pending))
            .unwrap();
        db.insert_pending("chg_a", Utc::now() + Duration::hours(1), None)
            .unwrap();
        db.execute_raw_for_test("DELETE FROM changes WHERE change_id = 'chg_a'");
        assert!(db.list_pending(None).unwrap().is_empty());
    }

    #[test]
    fn change_ids_sort_by_time() {
        let a = new_change_id();
        let b = new_change_id();
        assert!(a.starts_with("chg_"));
        assert!(a < b);
    }

    #[test]
    fn status_parse_roundtrip() {
        for s in [
            ChangeStatus::Pending,
            ChangeStatus::Approved,
            ChangeStatus::Applied,
            ChangeStatus::Rejected,
            ChangeStatus::Expired,
            ChangeStatus::Failed,
            ChangeStatus::Undone,
        ] {
            assert_eq!(ChangeStatus::parse(s.as_str()).unwrap(), s);
        }
        assert!(ChangeStatus::parse("xx").is_err());
    }
}
