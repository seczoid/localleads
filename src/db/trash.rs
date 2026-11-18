//! Deleted rows, kept for a while so a delete can be undone. Each table
//! that deletes touch has a twin `trash_<table>` with the same columns plus
//! `trash_id`; a delete copies its rows there in the same transaction.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, ToSql, params};

/// How long deleted rows can be brought back.
pub const KEEP_FOR_MINUTES: i64 = 60;

/// Tables in the order a restore puts rows back: rows that others point to
/// first. Each has the condition a restored row needs to make sense (the
/// rows it belongs to exist), so nothing comes back orphaned.
const TABLES: &[(&str, &str)] = &[
    ("jobs", ""),
    ("leads", ""),
    ("lead_sources", "lead_id IN (SELECT id FROM leads) AND job_id IN (SELECT id FROM jobs)"),
    ("job_events", "job_id IN (SELECT id FROM jobs)"),
    ("job_lookups", "job_id IN (SELECT id FROM jobs)"),
    ("proxies", ""),
    ("proxy_checks", "proxy_id IN (SELECT id FROM proxies)"),
    ("proxy_groups", ""),
    (
        "proxy_group_members",
        "proxy_id IN (SELECT id FROM proxies) AND group_id IN (SELECT id FROM proxy_groups)",
    ),
];

fn columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let names = statement.query_map([], |row| row.get(0))?;
    Ok(names.collect::<rusqlite::Result<_>>()?)
}

fn column_list(names: &[String]) -> String {
    names.iter().map(|name| format!("\"{name}\"")).collect::<Vec<_>>().join(", ")
}

/// Makes the trash tables, adds columns the live tables gained since, and
/// clears what is past keeping.
pub fn ensure(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS trash (
             id TEXT PRIMARY KEY,
             kind TEXT NOT NULL,
             label TEXT NOT NULL,
             deleted_at TEXT NOT NULL
         );",
    )?;
    for (table, _) in TABLES {
        let live = columns(connection, table)?;
        if live.is_empty() {
            continue;
        }
        let twin = format!("trash_{table}");
        let kept = columns(connection, &twin)?;
        if kept.is_empty() {
            connection.execute_batch(&format!(
                "CREATE TABLE {twin} AS SELECT '' AS trash_id, * FROM {table} WHERE 0;
                 CREATE INDEX IF NOT EXISTS {twin}_idx ON {twin}(trash_id);"
            ))?;
        } else {
            for name in live.iter().filter(|name| !kept.contains(name)) {
                connection.execute_batch(&format!("ALTER TABLE {twin} ADD COLUMN \"{name}\""))?;
            }
        }
    }
    purge(connection)?;
    Ok(())
}

/// Starts a trash entry; the rows go in with `keep`.
pub fn open(connection: &Connection, kind: &str, label: &str) -> Result<String> {
    purge(connection)?;
    let id = uuid::Uuid::new_v4().to_string();
    connection.execute(
        "INSERT INTO trash (id, kind, label, deleted_at) VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        params![id, kind, label],
    )?;
    Ok(id)
}

/// Copies the rows of `table` matching `condition` into the trash entry.
pub fn keep(
    connection: &Connection,
    trash_id: &str,
    table: &str,
    condition: &str,
    values: &[&dyn ToSql],
) -> Result<usize> {
    let names = columns(connection, table)?;
    let list = column_list(&names);
    // The rows go in unlabelled and are labelled after, so `condition`
    // keeps its own parameter numbering.
    let kept = connection.execute(
        &format!("INSERT INTO trash_{table} ({list}) SELECT {list} FROM {table} WHERE {condition}"),
        values,
    )?;
    connection.execute(
        &format!("UPDATE trash_{table} SET trash_id = ?1 WHERE trash_id IS NULL"),
        [trash_id],
    )?;
    Ok(kept)
}

/// What a restore brought back.
pub struct Restored {
    pub kind: String,
    pub label: String,
}

/// Puts a trash entry's rows back and forgets it. Rows whose place was
/// taken since (a business found again) stay as they are now.
pub fn restore(connection: &Connection, trash_id: &str) -> Result<Option<Restored>> {
    let entry: Option<(String, String)> = connection
        .query_row("SELECT kind, label FROM trash WHERE id = ?1", [trash_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    let Some((kind, label)) = entry else {
        return Ok(None);
    };
    for (table, belongs) in TABLES {
        let names = columns(connection, table)?;
        if names.is_empty() {
            continue;
        }
        let list = column_list(&names);
        let extra = if belongs.is_empty() { String::new() } else { format!(" AND {belongs}") };
        connection.execute(
            &format!("INSERT OR IGNORE INTO {table} ({list}) SELECT {list} FROM trash_{table} WHERE trash_id = ?1{extra}"),
            [trash_id],
        )?;
    }
    forget(connection, trash_id)?;
    Ok(Some(Restored { kind, label }))
}

fn forget(connection: &Connection, trash_id: &str) -> Result<()> {
    for (table, _) in TABLES {
        if !columns(connection, &format!("trash_{table}"))?.is_empty() {
            connection.execute(&format!("DELETE FROM trash_{table} WHERE trash_id = ?1"), [trash_id])?;
        }
    }
    connection.execute("DELETE FROM trash WHERE id = ?1", [trash_id])?;
    Ok(())
}

/// Drops entries older than [`KEEP_FOR_MINUTES`].
pub fn purge(connection: &Connection) -> Result<()> {
    let old: Vec<String> = {
        let mut statement = connection.prepare(&format!(
            "SELECT id FROM trash WHERE deleted_at < strftime('%Y-%m-%dT%H:%M:%fZ','now','-{KEEP_FOR_MINUTES} minutes')"
        ))?;
        let ids = statement.query_map([], |row| row.get(0))?;
        ids.collect::<rusqlite::Result<_>>()?
    };
    for id in old {
        forget(connection, &id)?;
    }
    Ok(())
}
