//! Budget changes per Meta object (at most 4 per hour per ad set).

use chrono::{DateTime, Duration, Utc};
use rusqlite::params;

use super::{Db, get_ts, ts};
use crate::error::CoreError;

impl Db {
    pub fn record_budget_change(
        &self,
        object_id: &str,
        at: DateTime<Utc>,
    ) -> Result<(), CoreError> {
        self.conn().execute(
            "INSERT INTO meta_budget_changes (object_id, at) VALUES (?1, ?2)",
            params![object_id, ts(&at)],
        )?;
        Ok(())
    }

    /// Timestamps of the changes to `object_id` since `since`, in ascending order.
    /// Deletes rows older than 2 h.
    pub fn budget_changes_since(
        &self,
        object_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<DateTime<Utc>>, CoreError> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM meta_budget_changes WHERE at < ?1",
            params![ts(&(Utc::now() - Duration::hours(2)))],
        )?;
        let mut stmt = conn.prepare(
            "SELECT at FROM meta_budget_changes WHERE object_id = ?1 AND at >= ?2 ORDER BY at",
        )?;
        let rows = stmt.query_map(params![object_id, ts(&since)], |r| get_ts(r, 0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM meta_budget_changes", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn budget_changes_window() {
        let db = Db::open_in_memory().unwrap();
        let now = Utc::now();
        let (a, b, c) = (
            now - Duration::hours(3),
            now - Duration::minutes(50),
            now - Duration::minutes(10),
        );
        db.record_budget_change("s1", c).unwrap();
        db.record_budget_change("s1", a).unwrap();
        db.record_budget_change("s1", b).unwrap();
        db.record_budget_change("outro", c).unwrap();
        let got = db
            .budget_changes_since("s1", now - Duration::hours(1))
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(ts(&got[0]), ts(&b));
        assert_eq!(ts(&got[1]), ts(&c));
        assert_eq!(rows(&db), 3, "the 3 h row was deleted");
    }
}
