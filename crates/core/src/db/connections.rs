use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Db, conv_err, get_ts, parse_platform, text_err, ts};
use crate::error::CoreError;
use crate::platform::Platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Ok,
    NeedsReconnect,
}

impl ConnectionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConnectionStatus::Ok => "ok",
            ConnectionStatus::NeedsReconnect => "needs_reconnect",
        }
    }

    pub fn parse(s: &str) -> Result<ConnectionStatus, CoreError> {
        match s {
            "ok" => Ok(ConnectionStatus::Ok),
            "needs_reconnect" => Ok(ConnectionStatus::NeedsReconnect),
            other => Err(CoreError::Config(format!(
                "Unknown connection status: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionMeta {
    pub connection_id: String,
    pub platform: Platform,
    pub name: String,
    pub is_default: bool,
    pub status: ConnectionStatus,
    pub expires_at: Option<DateTime<Utc>>,
    pub extra: Value,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str =
    "connection_id, platform, name, is_default, status, expires_at, extra, created_at";

fn row_to_connection(row: &Row) -> rusqlite::Result<ConnectionMeta> {
    let platform: String = row.get(1)?;
    let status: String = row.get(4)?;
    let expires: Option<String> = row.get(5)?;
    let extra: Option<String> = row.get(6)?;
    Ok(ConnectionMeta {
        connection_id: row.get(0)?,
        platform: parse_platform(&platform).map_err(|e| text_err(1, e))?,
        name: row.get(2)?,
        is_default: row.get::<_, i64>(3)? != 0,
        status: ConnectionStatus::parse(&status).map_err(|e| text_err(4, e))?,
        expires_at: expires
            .map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(|e| conv_err(5, e))
            })
            .transpose()?,
        extra: match extra {
            Some(s) => serde_json::from_str(&s).map_err(|e| conv_err(6, e))?,
            None => Value::Null,
        },
        created_at: get_ts(row, 7)?,
    })
}

impl Db {
    /// Inserts or updates the connection without deleting its account cache.
    pub fn upsert_connection(&self, c: &ConnectionMeta) -> Result<(), CoreError> {
        let extra = serde_json::to_string(&c.extra)
            .map_err(|e| CoreError::Config(format!("invalid extra: {e}")))?;
        self.conn().execute(
            "INSERT INTO connections_meta (connection_id, platform, name, is_default, status, \
             expires_at, extra, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(connection_id) DO UPDATE SET platform = excluded.platform, \
             name = excluded.name, is_default = excluded.is_default, status = excluded.status, \
             expires_at = excluded.expires_at, extra = excluded.extra",
            params![
                c.connection_id,
                c.platform.as_str(),
                c.name,
                c.is_default as i64,
                c.status.as_str(),
                c.expires_at.as_ref().map(ts),
                extra,
                ts(&c.created_at),
            ],
        )?;
        Ok(())
    }

    pub fn get_connection(&self, id: &str) -> Result<Option<ConnectionMeta>, CoreError> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM connections_meta WHERE connection_id = ?1"),
                [id],
                row_to_connection,
            )
            .optional()?)
    }

    /// Ordered by `connection_id`.
    pub fn list_connections(&self, p: Platform) -> Result<Vec<ConnectionMeta>, CoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM connections_meta WHERE platform = ?1 ORDER BY connection_id"
        ))?;
        let rows = stmt.query_map([p.as_str()], row_to_connection)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Deletes the connection; its account cache goes with it (ON DELETE CASCADE).
    pub fn delete_connection(&self, id: &str) -> Result<(), CoreError> {
        self.conn().execute(
            "DELETE FROM connections_meta WHERE connection_id = ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn set_default_connection(&self, p: Platform, id: &str) -> Result<(), CoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE connections_meta SET is_default = 0 WHERE platform = ?1",
            [p.as_str()],
        )?;
        let n = tx.execute(
            "UPDATE connections_meta SET is_default = 1 WHERE platform = ?1 AND connection_id = ?2",
            params![p.as_str(), id],
        )?;
        if n == 0 {
            return Err(CoreError::NotFound(format!("Connection {id}")));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_connection_status(&self, id: &str, s: ConnectionStatus) -> Result<(), CoreError> {
        let n = self.conn().execute(
            "UPDATE connections_meta SET status = ?2 WHERE connection_id = ?1",
            params![id, s.as_str()],
        )?;
        if n == 0 {
            return Err(CoreError::NotFound(format!("Connection {id}")));
        }
        Ok(())
    }
}
