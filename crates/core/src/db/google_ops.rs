//! Google Ads operation counts per Cloud project, over a sliding 24 h window.

use chrono::{DateTime, Duration, Utc};
use rusqlite::params;

use super::{Db, ts};
use crate::error::CoreError;

impl Db {
    pub fn record_google_ops(
        &self,
        project_id: &str,
        ops: u32,
        at: DateTime<Utc>,
    ) -> Result<(), CoreError> {
        self.conn().execute(
            "INSERT INTO google_ops (project_id, at, ops) VALUES (?1, ?2, ?3)",
            params![project_id, ts(&at), ops],
        )?;
        Ok(())
    }

    /// Sums the operations with `at > now - 24h` and deletes rows older than 48 h.
    pub fn google_ops_last_24h(
        &self,
        project_id: &str,
        now: DateTime<Utc>,
    ) -> Result<u64, CoreError> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM google_ops WHERE at < ?1",
            params![ts(&(now - Duration::hours(48)))],
        )?;
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(ops), 0) FROM google_ops WHERE project_id = ?1 AND at > ?2",
            params![project_id, ts(&(now - Duration::hours(24)))],
            |r| r.get(0),
        )?;
        Ok(total.max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM google_ops", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn ops_window_is_24h() {
        let db = Db::open_in_memory().unwrap();
        let now = Utc::now();
        db.record_google_ops("p", 10, now - Duration::hours(25))
            .unwrap();
        db.record_google_ops("p", 5, now - Duration::hours(1))
            .unwrap();
        db.record_google_ops("p", 3, now).unwrap();
        db.record_google_ops("other", 99, now).unwrap();
        assert_eq!(db.google_ops_last_24h("p", now).unwrap(), 8);
        assert_eq!(rows(&db), 4);
        db.record_google_ops("p", 7, now - Duration::hours(49))
            .unwrap();
        assert_eq!(rows(&db), 5);
        assert_eq!(db.google_ops_last_24h("p", now).unwrap(), 8);
        assert_eq!(rows(&db), 4);
    }
}
