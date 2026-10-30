use chrono::Utc;
use rusqlite::{Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Db, ts};
use crate::error::CoreError;
use crate::platform::Platform;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AccountInfo {
    pub account_id: String,
    pub name: String,
    pub currency: String,
    pub timezone: Option<String>,
    pub status: Option<String>,
    pub manager_id: Option<String>,
}

const COLUMNS: &str = "connection_id, account_id, name, currency, timezone, status, manager_id";

fn row_to_pair(row: &Row) -> rusqlite::Result<(String, AccountInfo)> {
    Ok((
        row.get(0)?,
        AccountInfo {
            account_id: row.get(1)?,
            name: row.get(2)?,
            currency: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            timezone: row.get(4)?,
            status: row.get(5)?,
            manager_id: row.get(6)?,
        },
    ))
}

impl Db {
    /// Troca, numa transação, todas as contas em cache da conexão.
    pub fn replace_accounts(
        &self,
        p: Platform,
        connection_id: &str,
        accounts: &[AccountInfo],
    ) -> Result<(), CoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM account_cache WHERE platform = ?1 AND connection_id = ?2",
            params![p.as_str(), connection_id],
        )?;
        let now = ts(&Utc::now());
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO account_cache (platform, account_id, connection_id, name, \
                 currency, timezone, status, manager_id, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for a in accounts {
                stmt.execute(params![
                    p.as_str(),
                    a.account_id,
                    connection_id,
                    a.name,
                    a.currency,
                    a.timezone,
                    a.status,
                    a.manager_id,
                    now,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Pares (connection_id, conta) que têm esse `account_id`, ordenados por conexão.
    pub fn accounts_for(
        &self,
        p: Platform,
        account_id: &str,
    ) -> Result<Vec<(String, AccountInfo)>, CoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM account_cache WHERE platform = ?1 AND account_id = ?2 \
             ORDER BY connection_id"
        ))?;
        let rows = stmt.query_map(params![p.as_str(), account_id], row_to_pair)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn list_accounts(
        &self,
        p: Platform,
        connection_id: Option<&str>,
    ) -> Result<Vec<(String, AccountInfo)>, CoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM account_cache WHERE platform = ?1 \
             AND (?2 IS NULL OR connection_id = ?2) ORDER BY connection_id, account_id"
        ))?;
        let rows = stmt.query_map(params![p.as_str(), connection_id], row_to_pair)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}
