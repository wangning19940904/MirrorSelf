//! Per-recipe sync state and run history (`0004_sync.sql`). Not derived
//! from the vault: `process --rebuild` keeps it.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RecipeState {
    pub recipe: String,
    pub cursor: Option<String>,
    pub last_run_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
    pub paused: bool,
    pub paused_reason: Option<String>,
    pub paused_at: Option<String>,
    pub tool: Option<String>,
    pub schema_sha256: Option<String>,
    pub pinned_at: Option<String>,
}

impl RecipeState {
    pub fn cursor_time(&self) -> Option<DateTime<Utc>> {
        parse_utc(self.cursor.as_deref())
    }

    pub fn last_run_time(&self) -> Option<DateTime<Utc>> {
        parse_utc(self.last_run_at.as_deref())
    }
}

fn parse_utc(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc))
}

pub fn load(conn: &Connection, recipe: &str) -> Result<RecipeState> {
    let row = conn
        .query_row(
            "SELECT cursor, last_run_at, last_success_at, last_error, last_error_at, paused, paused_reason,
                    paused_at, tool, schema_sha256, pinned_at
             FROM sync_state WHERE recipe = ?1",
            [recipe],
            |r| {
                Ok(RecipeState {
                    recipe: recipe.to_string(),
                    cursor: r.get(0)?,
                    last_run_at: r.get(1)?,
                    last_success_at: r.get(2)?,
                    last_error: r.get(3)?,
                    last_error_at: r.get(4)?,
                    paused: r.get::<_, i64>(5)? != 0,
                    paused_reason: r.get(6)?,
                    paused_at: r.get(7)?,
                    tool: r.get(8)?,
                    schema_sha256: r.get(9)?,
                    pinned_at: r.get(10)?,
                })
            },
        )
        .optional()?;
    Ok(row.unwrap_or_else(|| RecipeState { recipe: recipe.to_string(), ..Default::default() }))
}

pub fn save(conn: &Connection, s: &RecipeState) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state (recipe, cursor, last_run_at, last_success_at, last_error, last_error_at, paused,
                                 paused_reason, paused_at, tool, schema_sha256, pinned_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT (recipe) DO UPDATE SET
           cursor = excluded.cursor, last_run_at = excluded.last_run_at,
           last_success_at = excluded.last_success_at, last_error = excluded.last_error,
           last_error_at = excluded.last_error_at, paused = excluded.paused,
           paused_reason = excluded.paused_reason, paused_at = excluded.paused_at,
           tool = excluded.tool, schema_sha256 = excluded.schema_sha256, pinned_at = excluded.pinned_at",
        params![
            s.recipe,
            s.cursor,
            s.last_run_at,
            s.last_success_at,
            s.last_error,
            s.last_error_at,
            s.paused as i64,
            s.paused_reason,
            s.paused_at,
            s.tool,
            s.schema_sha256,
            s.pinned_at
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub id: i64,
    pub recipe: String,
    pub started_at: String,
    pub finished_at: String,
    pub window_from: Option<String>,
    pub window_to: Option<String>,
    pub status: String,
    pub raw_id: Option<String>,
    pub created: Option<bool>,
    pub bytes: Option<i64>,
    pub error: Option<String>,
}

pub fn insert_run(conn: &Connection, r: &RunRecord) -> Result<i64> {
    conn.execute(
        "INSERT INTO sync_runs (recipe, started_at, finished_at, window_from, window_to, status, raw_id, created, bytes, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            r.recipe,
            r.started_at,
            r.finished_at,
            r.window_from,
            r.window_to,
            r.status,
            r.raw_id,
            r.created.map(i64::from),
            r.bytes,
            r.error
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Most recent runs first.
pub fn recent_runs(conn: &Connection, recipe: Option<&str>, limit: u32) -> Result<Vec<RunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, recipe, started_at, finished_at, window_from, window_to, status, raw_id, created, bytes, error
         FROM sync_runs WHERE ?1 IS NULL OR recipe = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![recipe, limit], |r| {
            Ok(RunRecord {
                id: r.get(0)?,
                recipe: r.get(1)?,
                started_at: r.get(2)?,
                finished_at: r.get(3)?,
                window_from: r.get(4)?,
                window_to: r.get(5)?,
                status: r.get(6)?,
                raw_id: r.get(7)?,
                created: r.get::<_, Option<i64>>(8)?.map(|c| c != 0),
                bytes: r.get(9)?,
                error: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
