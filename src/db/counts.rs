//! Running totals per search, kept by triggers so every write path (new
//! leads, website reads, deletes, re-queues) updates them in its own
//! transaction. Pages read a few rows instead of joining every lead.
//!
//! A lead counts for each search in `lead_sources` that found it, like the
//! join the totals replace. Older versions of the app ignore these tables
//! and keep working; the triggers still keep them right meanwhile.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

// What each total counts, for a lead row `x`.
const READ: &str = "(x.enrich_status IS NOT NULL AND x.enrich_status <> 'queued')";
const EMAIL: &str = "(coalesce(x.email_count, 0) > 0)";
const PHONE: &str = "(coalesce(x.phone, '') <> '')";
const WEBSITE: &str = "(coalesce(x.website, '') <> '')";
const CATEGORY: &str = "coalesce(nullif(x.category, ''), 'Uncategorized')";

fn of(expression: &str, row: &str) -> String {
    expression.replace("x.", &format!("{row}."))
}

/// Creates the totals the first time (counting what's already there) and
/// the triggers that keep them.
pub(super) fn ensure(connection: &Connection) -> Result<()> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'job_counts'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let transaction = connection.unchecked_transaction()?;
    if !exists {
        transaction.execute_batch(&format!(
            r#"
            CREATE TABLE job_counts (
                job_id VARCHAR PRIMARY KEY,
                leads INTEGER NOT NULL DEFAULT 0,
                own INTEGER NOT NULL DEFAULT 0,
                with_phone INTEGER NOT NULL DEFAULT 0,
                with_website INTEGER NOT NULL DEFAULT 0,
                read INTEGER NOT NULL DEFAULT 0,
                with_email INTEGER NOT NULL DEFAULT 0,
                last_found VARCHAR
            ) WITHOUT ROWID;
            CREATE TABLE job_categories (
                job_id VARCHAR NOT NULL,
                category VARCHAR NOT NULL,
                n INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (job_id, category)
            ) WITHOUT ROWID;
            INSERT INTO job_counts
            SELECT s.job_id, count(*), coalesce(sum(coalesce(l.job_id = s.job_id, 0)), 0),
                   sum({phone}), sum({website}), sum({read}), sum({email}), max(s.found_at)
            FROM lead_sources s JOIN leads l ON l.id = s.lead_id
            GROUP BY s.job_id;
            INSERT INTO job_categories
            SELECT s.job_id, {category}, count(*)
            FROM lead_sources s JOIN leads l ON l.id = s.lead_id
            GROUP BY 1, 2;
            "#,
            phone = of(PHONE, "l"),
            website = of(WEBSITE, "l"),
            read = of(READ, "l"),
            email = of(EMAIL, "l"),
            category = of(CATEGORY, "l"),
        ))?;
    }
    transaction.execute_batch(&triggers())?;
    transaction.commit()?;
    Ok(())
}

/// Adds `sign` (1 or -1) times lead `row`'s totals to the searches chosen
/// by `which` (a condition on `job_counts.job_id`).
fn add_lead(row: &str, sign: &str, which: &str) -> String {
    format!(
        r#"UPDATE job_counts SET
               leads = leads {sign} 1,
               own = own {sign} coalesce({row}.job_id = job_counts.job_id, 0),
               with_phone = with_phone {sign} {phone},
               with_website = with_website {sign} {website},
               read = read {sign} {read},
               with_email = with_email {sign} {email}
           WHERE {which};"#,
        phone = of(PHONE, row),
        website = of(WEBSITE, row),
        read = of(READ, row),
        email = of(EMAIL, row),
    )
}

fn triggers() -> String {
    let category_new = of(CATEGORY, "NEW");
    let category_old = of(CATEGORY, "OLD");
    let sources_of_new = "job_id IN (SELECT job_id FROM lead_sources WHERE lead_id = NEW.id)";
    let sources_of_old = "job_id IN (SELECT job_id FROM lead_sources WHERE lead_id = OLD.id)";
    let changed = [READ, EMAIL, PHONE, WEBSITE, CATEGORY]
        .iter()
        .map(|expression| format!("{} <> {}", of(expression, "OLD"), of(expression, "NEW")))
        .chain(["OLD.job_id IS NOT NEW.job_id".to_owned()])
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        r#"
        -- A search found a lead (the lead row is always written first).
        CREATE TRIGGER IF NOT EXISTS job_counts_source_added AFTER INSERT ON lead_sources
        BEGIN
            INSERT INTO job_counts (job_id) SELECT NEW.job_id WHERE EXISTS (SELECT 1 FROM leads WHERE id = NEW.lead_id)
                ON CONFLICT(job_id) DO NOTHING;
            UPDATE job_counts SET last_found = max(coalesce(last_found, ''), NEW.found_at) WHERE job_id = NEW.job_id;
            {add_new_source}
            INSERT INTO job_categories (job_id, category, n)
                SELECT NEW.job_id, {category_l}, 1 FROM leads l WHERE l.id = NEW.lead_id
                ON CONFLICT(job_id, category) DO UPDATE SET n = n + 1;
        END;

        -- A lead left a search's list but still exists. (When the lead
        -- itself is deleted, the trigger below has counted it already.)
        CREATE TRIGGER IF NOT EXISTS job_counts_source_removed AFTER DELETE ON lead_sources
        WHEN EXISTS (SELECT 1 FROM leads WHERE id = OLD.lead_id)
        BEGIN
            {remove_old_source}
            UPDATE job_categories SET n = n - 1
                WHERE job_id = OLD.job_id AND category = (SELECT {category_l} FROM leads l WHERE l.id = OLD.lead_id);
        END;

        CREATE TRIGGER IF NOT EXISTS job_counts_lead_removed AFTER DELETE ON leads
        BEGIN
            {remove_old_lead}
            UPDATE job_categories SET n = n - 1 WHERE category = {category_old} AND {sources_of_old};
        END;

        -- Only for sources listed before their lead, which the app doesn't do.
        CREATE TRIGGER IF NOT EXISTS job_counts_lead_added AFTER INSERT ON leads
        WHEN EXISTS (SELECT 1 FROM lead_sources WHERE lead_id = NEW.id)
        BEGIN
            INSERT INTO job_counts (job_id) SELECT job_id FROM lead_sources WHERE lead_id = NEW.id
                ON CONFLICT(job_id) DO NOTHING;
            {add_new_lead}
            INSERT INTO job_categories (job_id, category, n)
                SELECT job_id, {category_new}, 1 FROM lead_sources WHERE lead_id = NEW.id
                ON CONFLICT(job_id, category) DO UPDATE SET n = n + 1;
        END;

        CREATE TRIGGER IF NOT EXISTS job_counts_lead_changed
        AFTER UPDATE OF enrich_status, email_count, phone, website, category, job_id ON leads
        WHEN {changed}
        BEGIN
            {remove_old_lead}
            {add_new_lead}
            UPDATE job_categories SET n = n - 1
                WHERE {category_old} <> {category_new} AND category = {category_old} AND {sources_of_old};
            INSERT INTO job_categories (job_id, category, n)
                SELECT job_id, {category_new}, 1 FROM lead_sources
                WHERE lead_id = NEW.id AND {category_old} <> {category_new}
                ON CONFLICT(job_id, category) DO UPDATE SET n = n + 1;
        END;
        "#,
        add_new_source = add_lead_from_leads("NEW", "+"),
        remove_old_source = add_lead_from_leads("OLD", "-"),
        remove_old_lead = add_lead("OLD", "-", sources_of_old),
        add_new_lead = add_lead("NEW", "+", sources_of_new),
        category_l = of(CATEGORY, "l"),
    )
}

/// The lead of source row `source` (NEW or OLD), added to or taken from
/// that source's search.
fn add_lead_from_leads(source: &str, sign: &str) -> String {
    let field = |expression: &str| {
        format!(
            "(SELECT {} FROM leads x WHERE x.id = {source}.lead_id)",
            expression
        )
    };
    format!(
        r#"UPDATE job_counts SET
               leads = leads {sign} 1,
               own = own {sign} {own},
               with_phone = with_phone {sign} {phone},
               with_website = with_website {sign} {website},
               read = read {sign} {read},
               with_email = with_email {sign} {email}
           WHERE job_id = {source}.job_id AND EXISTS (SELECT 1 FROM leads WHERE id = {source}.lead_id);"#,
        own = field(&format!("coalesce(x.job_id = {source}.job_id, 0)")),
        phone = field(PHONE),
        website = field(WEBSITE),
        read = field(READ),
        email = field(EMAIL),
    )
}

/// Forgets a deleted search's totals.
pub(super) fn forget(connection: &Connection, job_id: &str) -> Result<()> {
    connection.execute("DELETE FROM job_counts WHERE job_id = ?", [job_id])?;
    connection.execute("DELETE FROM job_categories WHERE job_id = ?", [job_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(connection: &Connection) -> Vec<String> {
        let mut rows = Vec::new();
        let mut statement = connection
            .prepare(
                "SELECT s.job_id, count(*), coalesce(sum(coalesce(l.job_id = s.job_id, 0)), 0),
                        sum(coalesce(l.phone, '') <> ''), sum(coalesce(l.website, '') <> ''),
                        sum(l.enrich_status IS NOT NULL AND l.enrich_status <> 'queued'),
                        sum(coalesce(l.email_count, 0) > 0)
                 FROM lead_sources s JOIN leads l ON l.id = s.lead_id GROUP BY 1 ORDER BY 1",
            )
            .unwrap();
        let mut query = statement.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let values: Vec<i64> = (1..7).map(|index| row.get(index).unwrap()).collect();
            rows.push(format!("{}:{values:?}", row.get::<_, String>(0).unwrap()));
        }
        let mut statement = connection
            .prepare(
                "SELECT s.job_id, coalesce(nullif(l.category, ''), 'Uncategorized'), count(*)
                 FROM lead_sources s JOIN leads l ON l.id = s.lead_id GROUP BY 1, 2 ORDER BY 1, 2",
            )
            .unwrap();
        let mut query = statement.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            rows.push(format!(
                "{}/{}={}",
                row.get::<_, String>(0).unwrap(),
                row.get::<_, String>(1).unwrap(),
                row.get::<_, i64>(2).unwrap()
            ));
        }
        rows
    }

    fn kept(connection: &Connection) -> Vec<String> {
        let mut rows = Vec::new();
        let mut statement = connection
            .prepare(
                "SELECT job_id, leads, own, with_phone, with_website, read, with_email
                 FROM job_counts WHERE leads > 0 ORDER BY 1",
            )
            .unwrap();
        let mut query = statement.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let values: Vec<i64> = (1..7).map(|index| row.get(index).unwrap()).collect();
            rows.push(format!("{}:{values:?}", row.get::<_, String>(0).unwrap()));
        }
        let mut statement = connection
            .prepare("SELECT job_id, category, n FROM job_categories WHERE n > 0 ORDER BY 1, 2")
            .unwrap();
        let mut query = statement.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            rows.push(format!(
                "{}/{}={}",
                row.get::<_, String>(0).unwrap(),
                row.get::<_, String>(1).unwrap(),
                row.get::<_, i64>(2).unwrap()
            ));
        }
        rows
    }

    #[test]
    fn totals_match_a_full_count_through_every_kind_of_write() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE leads (id TEXT PRIMARY KEY, job_id TEXT, category TEXT, phone TEXT,
                     website TEXT, enrich_status TEXT, email_count INTEGER);
                 CREATE TABLE lead_sources (lead_id TEXT NOT NULL, job_id TEXT NOT NULL,
                     found_at TEXT NOT NULL, PRIMARY KEY (lead_id, job_id));
                 INSERT INTO leads VALUES ('a', 'j1', 'Dentist', '1', '', NULL, 0);
                 INSERT INTO lead_sources VALUES ('a', 'j1', 't1');",
            )
            .unwrap();
        // Existing rows are counted when the totals are first made.
        ensure(&connection).unwrap();
        assert_eq!(kept(&connection), scan(&connection));
        // Running ensure again changes nothing.
        ensure(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO leads VALUES ('b', 'j1', '', NULL, 'b.com', NULL, NULL);
                 INSERT INTO lead_sources VALUES ('b', 'j1', 't2');
                 INSERT INTO leads VALUES ('c', 'j2', 'Plumber', '3', 'c.com', NULL, 0);
                 INSERT INTO lead_sources VALUES ('c', 'j2', 't3');
                 INSERT OR IGNORE INTO lead_sources VALUES ('c', 'j2', 't9');
                 -- Found again by another search.
                 INSERT INTO lead_sources VALUES ('a', 'j2', 't4');
                 -- Website reads: queued, then read with and without emails.
                 UPDATE leads SET enrich_status = 'queued' WHERE id IN ('a', 'b', 'c');
                 UPDATE leads SET enrich_status = 'done', email_count = 2 WHERE id = 'a';
                 UPDATE leads SET enrich_status = 'failed' WHERE id = 'b';
                 UPDATE leads SET enrich_status = NULL WHERE enrich_status = 'queued';
                 -- Details change on a later find.
                 UPDATE leads SET category = 'Orthodontist', phone = '' WHERE id = 'a';
                 UPDATE leads SET website = 'c2.com' WHERE id = 'c';",
            )
            .unwrap();
        assert_eq!(kept(&connection), scan(&connection));
        connection
            .execute_batch(
                // A lead leaves one list only, then a lead is deleted outright.
                "DELETE FROM lead_sources WHERE lead_id = 'a' AND job_id = 'j2';
                 DELETE FROM leads WHERE id = 'b';
                 DELETE FROM lead_sources WHERE lead_id = 'b';
                 -- Bulk re-read: back in the queue.
                 UPDATE leads SET enrich_status = NULL, email_count = 0 WHERE id = 'a';",
            )
            .unwrap();
        assert_eq!(kept(&connection), scan(&connection));
        // Deleting a search with its leads, the way delete_job does.
        connection
            .execute_batch(
                "DELETE FROM leads WHERE id IN (SELECT lead_id FROM lead_sources WHERE job_id = 'j2');
                 DELETE FROM lead_sources WHERE job_id = 'j2';",
            )
            .unwrap();
        forget(&connection, "j2").unwrap();
        assert_eq!(kept(&connection), scan(&connection));
    }
}
