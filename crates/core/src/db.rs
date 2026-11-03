//! The index database: SQLCipher, keyed from the vault key, migrated forward only.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::codes;
use crate::crypto::SubKeys;

const MIGRATIONS: &[&str] = &[include_str!("schema/0001_init.sql")];

pub fn open(path: &Path, keys: &SubKeys) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    pragma(&conn, &format!("PRAGMA key = \"x'{}'\"", keys.index_key_hex()))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
        .context("cannot read the index: wrong vault key, or not a MirrorSelf index")?;
    pragma(&conn, "PRAGMA journal_mode = WAL")?;
    pragma(&conn, "PRAGMA foreign_keys = ON")?;
    pragma(&conn, "PRAGMA synchronous = NORMAL")?;
    migrate(&conn)?;
    codes::seed(&conn)?;
    Ok(conn)
}

/// Runs a PRAGMA and discards any rows it returns (SQLCipher's `key` and
/// `journal_mode` both answer with a row).
fn pragma(conn: &Connection, sql: &str) -> Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: usize = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {}", i + 1))?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}

pub fn schema_version() -> usize {
    MIGRATIONS.len()
}
