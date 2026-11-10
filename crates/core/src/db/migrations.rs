use rusqlite::Connection;

use crate::error::CoreError;

/// Migrations applied in order; position + 1 is the `PRAGMA user_version` value.
pub const MIGRATIONS: &[&str] = &[
    // 1: change history, approval queue, connections and account cache
    r#"
    CREATE TABLE changes (
        change_id     TEXT PRIMARY KEY,
        platform      TEXT NOT NULL,
        account_id    TEXT NOT NULL,
        connection_id TEXT NOT NULL,
        tool          TEXT NOT NULL,
        input         TEXT NOT NULL,
        summary       TEXT NOT NULL,
        before        TEXT,
        after         TEXT,
        inverse       TEXT,
        reversible    INTEGER NOT NULL,
        status        TEXT NOT NULL,
        requested_by  TEXT,
        undo_of       TEXT,
        undone_by     TEXT,
        error         TEXT,
        created_at    TEXT NOT NULL,
        updated_at    TEXT NOT NULL
    );
    CREATE INDEX idx_changes_account ON changes(platform, account_id, created_at);
    CREATE INDEX idx_changes_status ON changes(status);
    CREATE INDEX idx_changes_undo_of ON changes(undo_of);

    CREATE TABLE pending_approvals (
        change_id  TEXT PRIMARY KEY REFERENCES changes(change_id) ON DELETE CASCADE,
        warning    TEXT,
        expires_at TEXT NOT NULL
    );
    CREATE INDEX idx_pending_expires ON pending_approvals(expires_at);

    CREATE TABLE connections_meta (
        connection_id TEXT PRIMARY KEY,
        platform      TEXT NOT NULL,
        name          TEXT NOT NULL,
        is_default    INTEGER NOT NULL DEFAULT 0,
        status        TEXT NOT NULL DEFAULT 'ok',
        expires_at    TEXT,
        extra         TEXT,
        created_at    TEXT NOT NULL
    );

    CREATE TABLE account_cache (
        platform      TEXT NOT NULL,
        account_id    TEXT NOT NULL,
        connection_id TEXT NOT NULL REFERENCES connections_meta(connection_id) ON DELETE CASCADE,
        name          TEXT NOT NULL,
        currency      TEXT,
        timezone      TEXT,
        status        TEXT,
        manager_id    TEXT,
        updated_at    TEXT NOT NULL,
        PRIMARY KEY (platform, account_id, connection_id)
    );
    "#,
    // 2: undo `force`, for the conflict check at apply time
    r#"
    ALTER TABLE changes ADD COLUMN undo_force INTEGER NOT NULL DEFAULT 0;
    "#,
    // 3: Google Ads operations per Cloud project (sliding 24 h window)
    r#"
    CREATE TABLE google_ops (
        project_id TEXT NOT NULL,
        at         TEXT NOT NULL,
        ops        INTEGER NOT NULL
    );
    CREATE INDEX idx_google_ops ON google_ops(project_id, at);
    "#,
    // 4: progress of multi-step writes (Meta) and budget changes per object
    r#"
    ALTER TABLE changes ADD COLUMN progress TEXT;
    CREATE TABLE meta_budget_changes (
        object_id TEXT NOT NULL,
        at        TEXT NOT NULL
    );
    CREATE INDEX idx_meta_budget_changes ON meta_budget_changes(object_id, at);
    "#,
];

pub fn run(conn: &mut Connection) -> Result<(), CoreError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
