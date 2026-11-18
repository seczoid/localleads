//! One thread that commits the busy writes (website reads and new leads)
//! together. While one batch commits, the next fills up, so a crawl of 200
//! sites at once takes one transaction for many saves instead of one each.

use super::{EnrichSummary, LeadInsert, insert_lead_row, now};
use crate::models::BusinessRecord;
use anyhow::Result;
use rusqlite::{Connection, params, params_from_iter, types::Value as SqlValue};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// The most writes one transaction takes.
const MOST_AT_ONCE: usize = 500;

type Done<T> = Box<dyn FnOnce(Result<T>) + Send>;

pub(super) enum Write {
    Enrichment {
        id: String,
        patch: serde_json::Map<String, serde_json::Value>,
        summary: EnrichSummary,
        done: Done<()>,
    },
    Leads {
        job_id: String,
        query: String,
        location: String,
        records: Vec<BusinessRecord>,
        done: Done<Vec<LeadInsert>>,
    },
}

enum Outcome {
    Enrichment(Done<()>, Result<()>),
    Leads(Done<Vec<LeadInsert>>, Result<Vec<LeadInsert>>),
}

impl Outcome {
    /// Tells the waiting caller; a failed commit fails every write in it.
    fn finish(self, failed_commit: Option<&str>) {
        let failed = |error: &str| anyhow::anyhow!("save failed: {error}");
        match (self, failed_commit) {
            (Self::Enrichment(done, _), Some(error)) => done(Err(failed(error))),
            (Self::Enrichment(done, result), None) => done(result),
            (Self::Leads(done, _), Some(error)) => done(Err(failed(error))),
            (Self::Leads(done, result), None) => done(result),
        }
    }
}

pub(super) fn start(connection: Arc<Mutex<Connection>>) -> Sender<Write> {
    let (queue, writes) = channel();
    std::thread::Builder::new()
        .name("db-writer".into())
        .spawn(move || run(&connection, &writes))
        .expect("start the database writer");
    queue
}

fn run(connection: &Mutex<Connection>, writes: &Receiver<Write>) {
    // Ends when every Database clone is gone.
    while let Ok(first) = writes.recv() {
        let mut batch = vec![first];
        while batch.len() < MOST_AT_ONCE {
            match writes.try_recv() {
                Ok(write) => batch.push(write),
                Err(_) => break,
            }
        }
        commit(connection, batch);
    }
}

fn commit(connection: &Mutex<Connection>, batch: Vec<Write>) {
    let fail_all = |batch: Vec<Write>, error: &str| {
        for write in batch {
            match write {
                Write::Enrichment { done, .. } => Outcome::Enrichment(done, Ok(())),
                Write::Leads { done, .. } => Outcome::Leads(done, Ok(Vec::new())),
            }
            .finish(Some(error));
        }
    };
    let Ok(connection) = connection.lock() else {
        return fail_all(batch, "database lock poisoned");
    };
    let mut transaction = match connection.unchecked_transaction() {
        Ok(transaction) => transaction,
        Err(error) => return fail_all(batch, &error.to_string()),
    };
    let mut outcomes = Vec::with_capacity(batch.len());
    for write in batch {
        outcomes.push(match write {
            Write::Enrichment { id, patch, summary, done } => {
                let result = in_savepoint(&mut transaction, |connection| {
                    save_enrichment(connection, &id, &patch, &summary)
                });
                Outcome::Enrichment(done, result)
            }
            Write::Leads { job_id, query, location, records, done } => {
                let found_at = now();
                let result = in_savepoint(&mut transaction, |connection| {
                    records
                        .iter()
                        .map(|record| {
                            insert_lead_row(connection, &job_id, &query, &location, record, &found_at)
                        })
                        .collect()
                });
                Outcome::Leads(done, result)
            }
        });
    }
    let failed = transaction.commit().err().map(|error| error.to_string());
    for outcome in outcomes {
        outcome.finish(failed.as_deref());
    }
}

/// Runs one write in its own savepoint, so a bad row undoes only itself and
/// not the rest of the batch.
fn in_savepoint<T>(
    transaction: &mut rusqlite::Transaction<'_>,
    work: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let savepoint = transaction.savepoint()?;
    // Dropping the savepoint on an error rolls it back.
    let value = work(&savepoint)?;
    savepoint.commit()?;
    Ok(value)
}

/// Stores an enrichment: `patch` is merged into the lead's extra fields
/// (keys set to null are removed), inside SQLite so the row is written
/// once, and the summary fills the columns.
pub(super) fn save_enrichment(
    connection: &Connection,
    id: &str,
    patch: &serde_json::Map<String, serde_json::Value>,
    summary: &EnrichSummary,
) -> Result<()> {
    let plain = |key: &str| !key.is_empty() && !key.contains(['"', '\\']);
    if !patch.keys().all(|key| plain(key)) {
        return save_enrichment_in_rust(connection, id, patch, summary);
    }
    let mut extra = "CASE WHEN json_valid(extra_json) AND json_type(extra_json) = 'object' \
                     THEN extra_json ELSE '{}' END"
        .to_owned();
    let mut values = Vec::new();
    let (removed, set): (Vec<_>, Vec<_>) = patch.iter().partition(|(_, value)| value.is_null());
    if !set.is_empty() {
        let mut call = format!("json_set({extra}");
        for (key, value) in &set {
            call.push_str(", ?, json(?)");
            values.push(SqlValue::Text(format!("$.\"{key}\"")));
            values.push(SqlValue::Text(serde_json::to_string(value)?));
        }
        call.push(')');
        extra = call;
    }
    if !removed.is_empty() {
        let mut call = format!("json_remove({extra}");
        for (key, _) in &removed {
            call.push_str(", ?");
            values.push(SqlValue::Text(format!("$.\"{key}\"")));
        }
        call.push(')');
        extra = call;
    }
    let sql = format!(
        "UPDATE leads SET extra_json = {extra}, email = ?, email_status = ?, email_reach = ?,
             email_count = ?, enrich_status = ?, enriched_at = ? WHERE id = ?"
    );
    values.extend([
        summary.email.clone().map_or(SqlValue::Null, SqlValue::Text),
        summary.email_status.clone().map_or(SqlValue::Null, SqlValue::Text),
        summary.email_reach.clone().map_or(SqlValue::Null, SqlValue::Text),
        SqlValue::Integer(summary.email_count),
        SqlValue::Text(summary.status.clone()),
        SqlValue::Text(now()),
        SqlValue::Text(id.to_owned()),
    ]);
    connection.prepare_cached(&sql)?.execute(params_from_iter(values))?;
    Ok(())
}

/// The same merge done in Rust, for keys a JSON path can't name plainly.
fn save_enrichment_in_rust(
    connection: &Connection,
    id: &str,
    patch: &serde_json::Map<String, serde_json::Value>,
    summary: &EnrichSummary,
) -> Result<()> {
    use rusqlite::OptionalExtension;
    let extra: Option<String> = connection
        .query_row("SELECT extra_json FROM leads WHERE id = ?", params![id], |row| row.get(0))
        .optional()?;
    let Some(extra) = extra else {
        return Ok(());
    };
    let mut extra: serde_json::Value =
        serde_json::from_str(&extra).unwrap_or_else(|_| serde_json::json!({}));
    if !extra.is_object() {
        extra = serde_json::json!({});
    }
    if let Some(object) = extra.as_object_mut() {
        for (key, value) in patch {
            if value.is_null() {
                object.remove(key);
            } else {
                object.insert(key.clone(), value.clone());
            }
        }
    }
    connection.execute(
        r#"UPDATE leads SET extra_json = ?, email = ?, email_status = ?, email_reach = ?,
               email_count = ?, enrich_status = ?, enriched_at = ? WHERE id = ?"#,
        params![
            serde_json::to_string(&extra)?,
            summary.email,
            summary.email_status,
            summary.email_reach,
            summary.email_count,
            summary.status,
            now(),
            id,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merged(start: &str, patch: serde_json::Value, in_sql: bool) -> String {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE leads (id TEXT PRIMARY KEY, extra_json TEXT, email TEXT, email_status TEXT,
                     email_reach TEXT, email_count INTEGER, enrich_status TEXT, enriched_at TEXT)",
            )
            .unwrap();
        connection.execute("INSERT INTO leads (id, extra_json) VALUES ('a', ?)", [start]).unwrap();
        let patch = patch.as_object().unwrap().clone();
        let summary = EnrichSummary { email_count: 1, status: "done".into(), ..Default::default() };
        if in_sql {
            save_enrichment(&connection, "a", &patch, &summary).unwrap();
            save_enrichment(&connection, "missing", &patch, &summary).unwrap();
        } else {
            save_enrichment_in_rust(&connection, "a", &patch, &summary).unwrap();
        }
        let (extra, status): (String, String) = connection
            .query_row("SELECT extra_json, enrich_status FROM leads WHERE id = 'a'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        let extra: serde_json::Value = serde_json::from_str(&extra).unwrap();
        format!("{extra} {status}")
    }

    #[test]
    fn sql_merge_matches_the_rust_merge() {
        let cases = [
            (r#"{"a":1,"socials":{"x":1,"y":2},"gone":true}"#,
             json!({"socials": {"z": 3}, "gone": null, "emails": ["e@x.com"], "n": 2.5, "s": "it's \"q\""})),
            ("not json", json!({"a": 1})),
            ("[1,2]", json!({"a": 1, "b": null})),
            ("{}", json!({"only_removed": null})),
            (r#"{"keep":1}"#, json!({})),
            (r#"{"k":1}"#, json!({"odd\"key": 1, "k": null})),
        ];
        for (start, patch) in cases {
            assert_eq!(merged(start, patch.clone(), true), merged(start, patch.clone(), false), "{start} {patch}");
        }
    }
}
