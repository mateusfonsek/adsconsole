mod accounts;
mod changes;
mod connections;
mod google_ops;
mod meta_budget;
mod migrations;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::types::Type;
use rusqlite::{Connection, Row};

use crate::platform::Platform;

use crate::error::CoreError;

pub use accounts::AccountInfo;
pub use changes::{
    ChangeFilter, ChangeRecord, ChangeStatus, InverseOp, PendingApproval, new_change_id,
};
pub use connections::{ConnectionMeta, ConnectionStatus};

pub(crate) fn parse_platform(s: &str) -> Result<Platform, CoreError> {
    Platform::ALL
        .into_iter()
        .find(|p| p.as_str() == s)
        .ok_or_else(|| CoreError::Config(format!("Unknown platform: {s}")))
}

pub(crate) fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub(crate) fn conv_err(
    idx: usize,
    e: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e))
}

pub(crate) fn text_err(idx: usize, e: CoreError) -> rusqlite::Error {
    conv_err(idx, std::io::Error::other(e.to_string()))
}

pub(crate) fn get_ts(row: &Row, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = row.get(idx)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| conv_err(idx, e))
}

/// Local SQLite database. Synchronous methods; the connection sits behind a mutex.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db, CoreError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Db, CoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Db, CoreError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations::run(&mut conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn user_version(&self) -> Result<i64, CoreError> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    #[cfg(test)]
    pub(crate) fn execute_raw_for_test(&self, sql: &str) {
        self.conn().execute_batch(sql).unwrap();
    }
}
