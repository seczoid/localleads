use crate::models::{
    BusinessRecord, JobStatus, JobSummary, LeadRow, ProxyEntry, ProxyGroup, SearchConfig, now,
};
use anyhow::{Context, Result};

mod accounts;
pub use accounts::{Account, add_api_key_to_file};
use chrono::{DateTime, Utc};
use rusqlite::{
    Connection, OptionalExtension, Row, params, params_from_iter,
    types::{Type, Value as SqlValue},
};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Read-only connections for pages, so they read while a search writes.
const READERS: usize = 4;
/// How long the Leads page's filter counts are reused.
const FACET_TTL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Database {
    /// The one connection that writes (SQLite allows a single writer).
    connection: Arc<Mutex<Connection>>,
    /// Read-only connections; WAL lets them read alongside the writer.
    /// Empty for an in-memory database, which can't be shared.
    readers: Arc<Vec<Mutex<Connection>>>,
    next_reader: Arc<AtomicUsize>,
    /// Recent filter counts by query, so a busy page doesn't recount
    /// every lead on each load.
    facets: Arc<Mutex<Vec<(String, Instant, LeadFacets)>>>,
    path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct DashboardStats {
    pub jobs: i64,
    pub active_jobs: i64,
    pub leads: i64,
    pub leads_with_phone: i64,
    pub leads_with_website: i64,
    pub proxies: i64,
    pub healthy_proxies: i64,
}

/// Website filter on the leads view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebsiteFilter {
    #[default]
    Any,
    Has,
    Missing,
}

/// A facet whose own selection is left out when counting its options, so
/// picking one category still shows the counts for the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Facet {
    Source,
    Category,
    State,
    City,
    Phone,
    Website,
    Email,
    Rating,
}

#[derive(Clone, Debug, Default)]
pub struct LeadQuery {
    pub search: String,
    /// Search (job) ids; a lead matches if any of them found it.
    pub sources: Vec<String>,
    /// Category names; "" means uncategorized.
    pub categories: Vec<String>,
    /// State codes; "" means unknown.
    pub states: Vec<String>,
    /// "city|state" keys.
    pub cities: Vec<String>,
    /// Only these leads (a selection in the table); empty means any.
    pub ids: Vec<String>,
    pub has_phone: bool,
    pub website: WebsiteFilter,
    /// "", has, safe (verified), none (checked, nothing found), pending.
    pub email: String,
    pub min_rating: Option<f64>,
    pub sort: String,
    pub limit: usize,
    pub offset: usize,
}

const CITY_KEY: &str = "(coalesce(city, '') || '|' || coalesce(state, ''))";

fn push_in(
    clauses: &mut Vec<String>,
    values: &mut Vec<SqlValue>,
    template: &str,
    items: &[String],
) {
    if items.is_empty() {
        return;
    }
    let marks = vec!["?"; items.len()].join(", ");
    clauses.push(template.replace("{}", &marks));
    values.extend(items.iter().map(|item| SqlValue::Text(item.clone())));
}

impl LeadQuery {
    fn conditions(&self, skip: Option<Facet>) -> (Vec<String>, Vec<SqlValue>) {
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        let keep = |facet: Facet| skip != Some(facet);
        let search = self.search.trim().to_lowercase();
        if !search.is_empty() {
            let pattern = format!("%{search}%");
            clauses.push(
                "(lower(name) LIKE ? OR lower(coalesce(category, '')) LIKE ? OR lower(coalesce(address, '')) LIKE ? OR lower(coalesce(phone, '')) LIKE ? OR lower(coalesce(website, '')) LIKE ?)"
                    .to_owned(),
            );
            for _ in 0..5 {
                values.push(SqlValue::Text(pattern.clone()));
            }
        }
        push_in(&mut clauses, &mut values, "id IN ({})", &self.ids);
        if keep(Facet::Source) {
            push_in(
                &mut clauses,
                &mut values,
                "id IN (SELECT lead_id FROM lead_sources WHERE job_id IN ({}))",
                &self.sources,
            );
        }
        if keep(Facet::Category) {
            push_in(
                &mut clauses,
                &mut values,
                "coalesce(category, '') IN ({})",
                &self.categories,
            );
        }
        if keep(Facet::State) {
            push_in(
                &mut clauses,
                &mut values,
                "coalesce(state, '') IN ({})",
                &self.states,
            );
        }
        if keep(Facet::City) {
            push_in(
                &mut clauses,
                &mut values,
                &format!("{CITY_KEY} IN ({{}})"),
                &self.cities,
            );
        }
        if keep(Facet::Phone) && self.has_phone {
            clauses.push("coalesce(phone, '') <> ''".to_owned());
        }
        if keep(Facet::Website) {
            match self.website {
                WebsiteFilter::Any => {}
                WebsiteFilter::Has => clauses.push("coalesce(website, '') <> ''".to_owned()),
                WebsiteFilter::Missing => clauses.push("coalesce(website, '') = ''".to_owned()),
            }
        }
        if keep(Facet::Email)
            && let Some(clause) = email_clause(&self.email)
        {
            clauses.push(clause.to_owned());
        }
        if keep(Facet::Rating)
            && let Some(rating) = self.min_rating
        {
            clauses.push("rating >= ?".to_owned());
            values.push(SqlValue::Real(rating));
        }
        (clauses, values)
    }

    fn where_clause(&self) -> (String, Vec<SqlValue>) {
        self.where_except(None, &[])
    }

    fn where_except(&self, skip: Option<Facet>, extra: &[&str]) -> (String, Vec<SqlValue>) {
        let (mut clauses, values) = self.conditions(skip);
        clauses.extend(extra.iter().map(|clause| (*clause).to_owned()));
        let clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        (clause, values)
    }

    /// `sort` is "key" or "key:asc|desc". Empty values sort last either way,
    /// and `id` breaks ties so paging is stable.
    fn order_clause(&self) -> String {
        let (key, dir) = match self.sort.split_once(':') {
            Some((key, dir)) => (key, Some(dir.eq_ignore_ascii_case("desc"))),
            None => (self.sort.as_str(), None),
        };
        let desc = |default: bool| {
            if dir.unwrap_or(default) {
                "DESC"
            } else {
                "ASC"
            }
        };
        let order = match key {
            "oldest" => "created_at ASC".to_owned(),
            "name" => format!("lower(name) {}", desc(false)),
            "city" => {
                let d = desc(false);
                format!(
                    "coalesce(state, '') = '', state {d}, coalesce(city, '') = '', city {d}, lower(name)"
                )
            }
            "phone" => format!("coalesce(phone, '') = '', phone {}", desc(false)),
            "website" => format!("coalesce(website, '') = '', lower(website) {}", desc(false)),
            "email" => format!(
                "coalesce(email, '') = '', CASE email_reach WHEN 'safe' THEN 0 WHEN 'risky' THEN 1 WHEN 'unknown' THEN 2 ELSE 3 END {}, lower(email)",
                desc(false)
            ),
            "rating" => format!(
                "rating IS NULL, rating {d}, review_count {d}",
                d = desc(true)
            ),
            "reviews" => format!("review_count IS NULL, review_count {}", desc(true)),
            "added" => format!("created_at {}", desc(true)),
            _ => "created_at DESC".to_owned(),
        };
        format!("ORDER BY {order}, id")
    }
}

fn email_clause(value: &str) -> Option<&'static str> {
    Some(match value {
        "has" => "email_count > 0",
        "safe" => "email_count > 0 AND email_reach = 'safe'",
        "none" => "email_count = 0 AND enrich_status IN ('done', 'failed')",
        "pending" => "(enrich_status IS NULL OR enrich_status = 'queued')",
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct FacetCount {
    pub value: String,
    pub count: i64,
}

/// Option counts for every facet, each counted with the other facets applied.
#[derive(Clone, Debug, Default)]
pub struct LeadFacets {
    pub total: i64,
    pub sources: Vec<FacetCount>,
    pub categories: Vec<FacetCount>,
    pub states: Vec<FacetCount>,
    /// `value` is a "city|state" key.
    pub cities: Vec<FacetCount>,
    pub phone_total: i64,
    pub with_phone: i64,
    pub website_total: i64,
    pub with_website: i64,
    /// Any, has, safe, none, pending.
    pub email: [i64; 5],
    pub rating_total: i64,
    pub rating_3: i64,
    pub rating_4: i64,
    pub rating_45: i64,
}

/// Totals for one search's lead list.
#[derive(Clone, Debug, Default)]
pub struct LeadListStats {
    pub leads: i64,
    /// Leads no earlier search had found.
    pub first_found: i64,
    pub with_phone: i64,
    pub with_website: i64,
    pub last_found: Option<String>,
    pub categories: Vec<(String, i64)>,
}

#[derive(Clone, Debug)]
pub struct LeadSource {
    pub job_id: String,
    pub job_name: Option<String>,
    pub query: Option<String>,
    pub location: Option<String>,
    pub found_at: String,
}

/// A lead waiting for enrichment, with what the enrichment needs.
#[derive(Clone, Debug)]
pub struct EnrichTarget {
    pub id: String,
    pub name: String,
    pub address: Option<String>,
    pub website: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Google's "0x…:0x…" feature id.
    pub google_id: Option<String>,
}

/// The headline of an enrichment, kept in columns so leads can be
/// filtered and sorted by it.
#[derive(Clone, Debug, Default)]
pub struct EnrichSummary {
    pub email: Option<String>,
    pub email_status: Option<String>,
    pub email_reach: Option<String>,
    pub email_count: i64,
    pub status: String,
}

/// What `insert_lead` did with a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadInsert {
    /// First time any search found this business.
    New,
    /// Known business, newly added to this search's list.
    Linked,
    /// This search already had it.
    Repeat,
}

/// One proxy check result, ready to store.
#[derive(Clone, Debug)]
pub struct ProxyCheckRecord {
    pub proxy_id: String,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub exit_ip: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ProxyQuery {
    pub group_id: Option<String>,
    pub status: Option<String>,
    pub enabled: Option<bool>,
    pub search: String,
    /// "key" or "key:asc|desc"; empty means newest first.
    pub sort: String,
    pub limit: usize,
    pub offset: usize,
}

impl ProxyQuery {
    fn order_clause(&self) -> String {
        let (key, desc) = match self.sort.split_once(':') {
            Some((key, dir)) => (key, dir.eq_ignore_ascii_case("desc")),
            None => (self.sort.as_str(), false),
        };
        let dir = if desc { "DESC" } else { "ASC" };
        let order = match key {
            "proxy" => format!("url {dir}"),
            // Best first when ascending: healthy, slow, failed, unchecked.
            "status" => format!(
                "CASE status WHEN 'healthy' THEN 0 WHEN 'slow' THEN 1 WHEN 'failed' THEN 2 ELSE 3 END {dir}, latency_ms IS NULL, latency_ms"
            ),
            "latency" => format!("latency_ms IS NULL, latency_ms {dir}"),
            "checked" => format!("last_checked_at IS NULL, last_checked_at {dir}"),
            "error" => format!("coalesce(last_error, '') = '', last_error {dir}"),
            _ => "created_at DESC".to_owned(),
        };
        format!("ORDER BY {order}, id")
    }

    fn where_clause(&self) -> (String, Vec<SqlValue>) {
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        if let Some(group_id) = self.group_id.as_deref().filter(|value| !value.is_empty()) {
            clauses.push(
                "id IN (SELECT proxy_id FROM proxy_group_members WHERE group_id = ?)".to_owned(),
            );
            values.push(SqlValue::Text(group_id.to_owned()));
        }
        if let Some(status) = self.status.as_deref().filter(|value| !value.is_empty()) {
            clauses.push("status = ?".to_owned());
            values.push(SqlValue::Text(status.to_owned()));
        }
        if let Some(enabled) = self.enabled {
            clauses.push("enabled = ?".to_owned());
            values.push(SqlValue::Integer(i64::from(enabled)));
        }
        let search = self.search.trim().to_ascii_lowercase();
        if !search.is_empty() {
            clauses
                .push("(lower(url) LIKE ? OR lower(coalesce(last_error, '')) LIKE ?)".to_owned());
            let pattern = format!("%{search}%");
            values.push(SqlValue::Text(pattern.clone()));
            values.push(SqlValue::Text(pattern));
        }
        let clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        (clause, values)
    }
}

#[derive(Clone, Debug)]
pub struct JobEvent {
    pub created_at: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ProxyCheckRow {
    pub checked_at: String,
    pub endpoint: String,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub exit_ip: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LeadDetail {
    pub row: LeadRow,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub hours: Option<String>,
    pub source: String,
    pub extra_json: String,
    pub created_at: String,
    pub enriched_at: Option<String>,
}

const PROXY_COLUMNS: &str = "id, url, protocol, username, password, enabled, status, latency_ms, last_checked_at, consecutive_failures, last_error";
const LEAD_COLUMNS: &str = "id, name, category, address, phone, website, rating, review_count, status, maps_url, place_id, job_id, city, state, postal_code, created_at, email, email_status, email_reach, email_count, enrich_status";

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        if path.is_file() && fs::metadata(&path)?.len() > 0 && !is_sqlite_file(&path)? {
            anyhow::bail!("{} is not a LocalLeads SQLite database", path.display());
        }
        let connection = Connection::open(&path)
            .with_context(|| format!("open SQLite database at {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA cache_size = -65536;
             PRAGMA journal_size_limit = 67108864;",
        )?;
        // Fold the write-ahead log into the database and shrink it. A big
        // write (a backfill, a bulk delete) can leave it gigabytes long,
        // and without a size limit SQLite never gives the space back.
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        let mut database = Self {
            connection: Arc::new(Mutex::new(connection)),
            readers: Arc::new(Vec::new()),
            next_reader: Arc::new(AtomicUsize::new(0)),
            facets: Arc::new(Mutex::new(Vec::new())),
            path: Some(path.clone()),
        };
        database.initialize()?;
        let readers = (0..READERS)
            .map(|_| open_reader(&path).map(Mutex::new))
            .collect::<Result<Vec<_>>>()?;
        database.readers = Arc::new(readers);
        Ok(database)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let database = Self {
            connection: Arc::new(Mutex::new(Connection::open_in_memory()?)),
            readers: Arc::new(Vec::new()),
            next_reader: Arc::new(AtomicUsize::new(0)),
            facets: Arc::new(Mutex::new(Vec::new())),
            path: None,
        };
        database.initialize()?;
        Ok(database)
    }

    fn initialize(&self) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
            INSERT INTO schema_version
            SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_version);

            CREATE TABLE IF NOT EXISTS settings (
                key VARCHAR PRIMARY KEY,
                value VARCHAR NOT NULL,
                updated_at VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS jobs (
                id VARCHAR PRIMARY KEY,
                name VARCHAR NOT NULL,
                status VARCHAR NOT NULL,
                config_json VARCHAR NOT NULL,
                total BIGINT NOT NULL DEFAULT 0,
                collected BIGINT NOT NULL DEFAULT 0,
                failures BIGINT NOT NULL DEFAULT 0,
                duplicates BIGINT NOT NULL DEFAULT 0,
                started_at VARCHAR,
                finished_at VARCHAR,
                error VARCHAR,
                created_at VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS job_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id VARCHAR NOT NULL,
                created_at VARCHAR NOT NULL,
                level VARCHAR NOT NULL,
                message VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS leads (
                id VARCHAR PRIMARY KEY,
                job_id VARCHAR NOT NULL,
                dedupe_key VARCHAR NOT NULL UNIQUE,
                name VARCHAR NOT NULL,
                category VARCHAR,
                address VARCHAR,
                phone VARCHAR,
                website VARCHAR,
                latitude DOUBLE,
                longitude DOUBLE,
                rating DOUBLE,
                review_count BIGINT,
                status VARCHAR,
                hours VARCHAR,
                maps_url VARCHAR,
                place_id VARCHAR,
                source VARCHAR NOT NULL,
                extra_json VARCHAR NOT NULL,
                created_at VARCHAR NOT NULL
            );
            CREATE INDEX IF NOT EXISTS leads_name_idx ON leads(name);
            CREATE INDEX IF NOT EXISTS leads_job_idx ON leads(job_id);

            CREATE TABLE IF NOT EXISTS proxies (
                id VARCHAR PRIMARY KEY,
                url VARCHAR NOT NULL UNIQUE,
                protocol VARCHAR NOT NULL,
                username VARCHAR,
                password VARCHAR,
                enabled BOOLEAN NOT NULL DEFAULT TRUE,
                status VARCHAR NOT NULL DEFAULT 'unknown',
                latency_ms BIGINT,
                last_checked_at VARCHAR,
                consecutive_failures BIGINT NOT NULL DEFAULT 0,
                last_error VARCHAR,
                created_at VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS proxy_checks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                proxy_id VARCHAR NOT NULL,
                checked_at VARCHAR NOT NULL,
                endpoint VARCHAR NOT NULL,
                status VARCHAR NOT NULL,
                latency_ms BIGINT,
                exit_ip VARCHAR,
                error VARCHAR
            );
            CREATE TABLE IF NOT EXISTS proxy_groups (
                id VARCHAR PRIMARY KEY,
                name VARCHAR NOT NULL UNIQUE,
                created_at VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS proxy_group_members (
                group_id VARCHAR NOT NULL,
                proxy_id VARCHAR NOT NULL,
                created_at VARCHAR NOT NULL,
                PRIMARY KEY (group_id, proxy_id)
            );
            CREATE INDEX IF NOT EXISTS proxy_checks_proxy_idx ON proxy_checks(proxy_id, id);
            CREATE INDEX IF NOT EXISTS proxies_status_idx ON proxies(status);
            CREATE INDEX IF NOT EXISTS proxies_created_idx ON proxies(created_at DESC, id);
            CREATE INDEX IF NOT EXISTS proxy_group_members_proxy_idx ON proxy_group_members(proxy_id);

            -- Every search that found a lead. A business is stored once,
            -- but belongs to the list of each search that found it.
            CREATE TABLE IF NOT EXISTS lead_sources (
                lead_id VARCHAR NOT NULL,
                job_id VARCHAR NOT NULL,
                query VARCHAR,
                location VARCHAR,
                found_at VARCHAR NOT NULL,
                PRIMARY KEY (lead_id, job_id)
            );
            CREATE INDEX IF NOT EXISTS lead_sources_job_idx ON lead_sources(job_id, lead_id);
            "#,
        )?;
        for (column, kind) in [
            ("city", "VARCHAR"),
            ("state", "VARCHAR"),
            ("postal_code", "VARCHAR"),
            ("geo_parsed", "INTEGER NOT NULL DEFAULT 0"),
            // Enrichment: the best email and its check, how many usable
            // addresses were found, and where the lead is in the queue
            // (NULL = not yet, queued, done, failed).
            ("email", "VARCHAR"),
            ("email_status", "VARCHAR"),
            ("email_reach", "VARCHAR"),
            ("email_count", "INTEGER NOT NULL DEFAULT 0"),
            ("enrich_status", "VARCHAR"),
            ("enriched_at", "VARCHAR"),
        ] {
            ensure_column(&connection, "leads", column, kind)?;
        }
        connection.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS leads_category_idx ON leads(category);
            CREATE INDEX IF NOT EXISTS leads_state_city_idx ON leads(state, city);
            CREATE INDEX IF NOT EXISTS leads_created_idx ON leads(created_at);
            CREATE INDEX IF NOT EXISTS leads_geo_pending_idx ON leads(id) WHERE geo_parsed = 0;
            CREATE INDEX IF NOT EXISTS leads_enrich_idx ON leads(enrich_status);
            -- Covering indexes for the Leads page's filter counts, so they
            -- read a small index instead of every lead.
            DROP INDEX IF EXISTS leads_email_idx;
            CREATE INDEX IF NOT EXISTS leads_email_facet_idx ON leads(email_count, email_reach, enrich_status);
            CREATE INDEX IF NOT EXISTS leads_contact_idx ON leads((coalesce(phone, '') <> ''), (coalesce(website, '') <> ''));
            CREATE INDEX IF NOT EXISTS leads_rating_idx ON leads(rating);
            -- What the lead lists summary needs per lead, found by id.
            CREATE INDEX IF NOT EXISTS leads_list_idx ON leads(id, job_id, category, (coalesce(phone, '') <> ''), (coalesce(website, '') <> ''));
            -- Leads claimed by a run the app didn't finish go back in the queue.
            UPDATE leads SET enrich_status = NULL WHERE enrich_status = 'queued';
            "#,
        )?;
        accounts::create_tables(&connection)?;
        backfill_leads(&connection)?;
        shrink_leads(&connection)?;
        connection.execute("UPDATE schema_version SET version = 3 WHERE version < 3", [])?;
        connection.execute(
            "UPDATE jobs SET status = 'interrupted', finished_at = ?, error = 'Application exited while job was active' WHERE status IN ('running', 'paused', 'stopping')",
            params![now()],
        )?;
        Ok(())
    }

    pub fn create_job(&self, id: &str, config: &SearchConfig) -> Result<()> {
        let connection = self.lock()?;
        let config_json = serde_json::to_string(config)?;
        let total = if config.collect_all {
            0
        } else {
            i64::from(config.total_limit)
        };
        connection.execute(
            "INSERT INTO jobs (id, name, status, config_json, total, created_at) VALUES (?, ?, 'queued', ?, ?, ?)",
            params![id, config.job_name, config_json, total, now()],
        )?;
        Ok(())
    }

    pub fn update_job_status(
        &self,
        id: &str,
        status: JobStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let connection = self.lock()?;
        let finished_at = matches!(
            status,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
        );
        if finished_at {
            connection.execute(
                "UPDATE jobs SET status = ?, finished_at = ?, error = ? WHERE id = ?",
                params![status.to_string(), now(), error, id],
            )?;
        } else {
            connection.execute(
                "UPDATE jobs SET status = ?, error = ? WHERE id = ?",
                params![status.to_string(), error, id],
            )?;
        }
        Ok(())
    }

    pub fn update_job_progress(
        &self,
        id: &str,
        collected: i64,
        failures: i64,
        duplicates: i64,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE jobs SET collected = ?, failures = ?, duplicates = ?, status = CASE WHEN status = 'queued' THEN 'running' ELSE status END, started_at = COALESCE(started_at, ?) WHERE id = ?",
            params![collected, failures, duplicates, now(), id],
        )?;
        Ok(())
    }

    pub fn append_event(&self, job_id: &str, level: &str, message: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO job_events (job_id, created_at, level, message) VALUES (?, ?, ?, ?)",
            params![job_id, now(), level, message],
        )?;
        Ok(())
    }

    /// Stores a business once and adds it to `job_id`'s list. A business an
    /// earlier search already found is linked, and its missing fields are
    /// filled from the fresh record.
    #[cfg(test)]
    pub fn insert_lead(
        &self,
        job_id: &str,
        query: &str,
        location: &str,
        record: &BusinessRecord,
    ) -> Result<LeadInsert> {
        let mut results =
            self.insert_leads(job_id, query, location, std::slice::from_ref(record))?;
        results
            .pop()
            .ok_or_else(|| anyhow::anyhow!("lead insert returned no result"))
    }

    /// `insert_lead` for a page of results in one transaction, which keeps
    /// the single database connection free for other work during big searches.
    pub fn insert_leads(
        &self,
        job_id: &str,
        query: &str,
        location: &str,
        records: &[BusinessRecord],
    ) -> Result<Vec<LeadInsert>> {
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        let found_at = now();
        let mut results = Vec::with_capacity(records.len());
        for record in records {
            results.push(insert_lead_row(
                &transaction,
                job_id,
                query,
                location,
                record,
                &found_at,
            )?);
        }
        transaction.commit()?;
        Ok(results)
    }

    /// Takes up to `limit` leads of `job_id` that were never enriched and
    /// marks them queued so no other worker picks them up.
    pub fn claim_enrich_targets(&self, job_id: &str, limit: usize) -> Result<Vec<EnrichTarget>> {
        self.claim_targets(
            "SELECT l.id FROM lead_sources s JOIN leads l ON l.id = s.lead_id
             WHERE s.job_id = ?1 AND l.enrich_status IS NULL
             ORDER BY s.found_at, l.id LIMIT ?2",
            params![job_id, i64::try_from(limit).unwrap_or(i64::MAX)],
        )
    }

    /// Queues the given leads again, whatever their state, and returns them.
    pub fn claim_enrich_ids(&self, ids: &[String]) -> Result<Vec<EnrichTarget>> {
        let mut targets = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            targets.extend(self.claim_targets(
                &format!(
                    "SELECT id FROM leads WHERE id IN ({marks}) AND coalesce(enrich_status, '') <> 'queued'"
                ),
                rusqlite::params_from_iter(chunk.iter()),
            )?);
        }
        Ok(targets)
    }

    fn claim_targets(&self, select_ids: &str, args: impl rusqlite::Params) -> Result<Vec<EnrichTarget>> {
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        let ids: Vec<String> = transaction
            .prepare(select_ids)?
            .query_map(args, |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut targets = Vec::with_capacity(ids.len());
        {
            let mut read = transaction.prepare_cached(
                r#"SELECT id, name, address, website, latitude, longitude,
                          coalesce(json_extract(extra_json, '$.google_id'), json_extract(extra_json, '$.cid'))
                   FROM leads WHERE id = ?"#,
            )?;
            let mut claim =
                transaction.prepare_cached("UPDATE leads SET enrich_status = 'queued' WHERE id = ?")?;
            for id in &ids {
                let target = read.query_row(params![id], |row| {
                    let google_id: Option<String> = row.get(6)?;
                    Ok(EnrichTarget {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        address: row.get(2)?,
                        website: row.get(3)?,
                        latitude: row.get(4)?,
                        longitude: row.get(5)?,
                        google_id: google_id.filter(|value| value.starts_with("0x") && value.contains(':')),
                    })
                })?;
                claim.execute(params![id])?;
                targets.push(target);
            }
        }
        transaction.commit()?;
        Ok(targets)
    }

    /// Puts leads that are still queued (an enrichment that stopped) back.
    pub fn release_enrich_ids(&self, ids: &[String]) -> Result<()> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "UPDATE leads SET enrich_status = NULL WHERE id = ? AND enrich_status = 'queued'",
        )?;
        for id in ids {
            statement.execute(params![id])?;
        }
        Ok(())
    }

    /// Puts a search's claimed but unfinished leads (a stopped search) back.
    pub fn release_job_enrichment(&self, job_id: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE leads SET enrich_status = NULL WHERE enrich_status = 'queued'
             AND id IN (SELECT lead_id FROM lead_sources WHERE job_id = ?)",
            params![job_id],
        )?;
        Ok(())
    }

    /// Stores an enrichment: `patch` is merged into the lead's extra fields
    /// (keys set to null are removed) and the summary fills the columns.
    pub fn save_enrichment(
        &self,
        id: &str,
        patch: &serde_json::Map<String, serde_json::Value>,
        summary: &EnrichSummary,
    ) -> Result<()> {
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        let extra: Option<String> = transaction
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
        transaction.execute(
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
        transaction.commit()?;
        Ok(())
    }

    /// Per-search list totals, keyed by job id.
    pub fn lead_list_stats(&self) -> Result<HashMap<String, LeadListStats>> {
        let connection = self.read()?;
        let mut stats: HashMap<String, LeadListStats> = HashMap::new();
        {
            let mut statement = connection.prepare(
                r#"SELECT s.job_id, count(*),
                          sum(l.job_id = s.job_id),
                          sum(coalesce(l.phone, '') <> ''),
                          sum(coalesce(l.website, '') <> ''),
                          max(s.found_at)
                   FROM lead_sources s JOIN leads l INDEXED BY leads_list_idx ON l.id = s.lead_id
                   GROUP BY s.job_id"#,
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    LeadListStats {
                        leads: row.get(1)?,
                        first_found: row.get(2)?,
                        with_phone: row.get(3)?,
                        with_website: row.get(4)?,
                        last_found: row.get(5)?,
                        categories: Vec::new(),
                    },
                ))
            })?;
            for row in rows {
                let (job_id, value) = row?;
                stats.insert(job_id, value);
            }
        }
        let mut statement = connection.prepare(
            r#"SELECT s.job_id, coalesce(nullif(l.category, ''), 'Uncategorized'), count(*) AS n
               FROM lead_sources s JOIN leads l INDEXED BY leads_list_idx ON l.id = s.lead_id
               GROUP BY 1, 2 ORDER BY 1, n DESC"#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (job_id, category, count) = row?;
            if let Some(entry) = stats.get_mut(&job_id) {
                entry.categories.push((category, count));
            }
        }
        Ok(stats)
    }

    /// Categories by size and the number of distinct cities, across all leads.
    pub fn lead_overview(&self) -> Result<(Vec<(String, i64)>, i64)> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            "SELECT coalesce(nullif(category, ''), 'Uncategorized') AS c, count(*) AS n FROM leads GROUP BY c ORDER BY n DESC, c",
        )?;
        let categories = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let cities = connection.query_row(
            "SELECT count(*) FROM (SELECT 1 FROM leads WHERE coalesce(city, '') <> '' GROUP BY city, state)",
            [],
            |row| row.get(0),
        )?;
        Ok((categories, cities))
    }

    /// Counts for the Leads page's filters. The same query within
    /// `FACET_TTL` reuses its counts: they take a pass over every lead each.
    pub fn lead_facets(&self, query: &LeadQuery) -> Result<LeadFacets> {
        let key = {
            let (clause, values) = query.where_clause();
            format!("{clause}|{values:?}")
        };
        if let Ok(cache) = self.facets.lock()
            && let Some((_, at, facets)) = cache.iter().find(|(cached, _, _)| *cached == key)
            && at.elapsed() < FACET_TTL
        {
            return Ok(facets.clone());
        }
        let facets = self.count_facets(query)?;
        if let Ok(mut cache) = self.facets.lock() {
            cache.retain(|(cached, at, _)| *cached != key && at.elapsed() < FACET_TTL);
            cache.truncate(31);
            cache.push((key, Instant::now(), facets.clone()));
        }
        Ok(facets)
    }

    fn count_facets(&self, query: &LeadQuery) -> Result<LeadFacets> {
        let connection = self.read()?;
        let count_rows = |sql: String, values: Vec<SqlValue>| -> Result<Vec<FacetCount>> {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(values), |row| {
                Ok(FacetCount {
                    value: row.get(0)?,
                    count: row.get(1)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        };
        let mut facets = LeadFacets::default();

        let (clause, values) = query.where_clause();
        facets.total = connection.query_row(
            &format!("SELECT count(*) FROM leads {clause}"),
            params_from_iter(values),
            |row| row.get(0),
        )?;

        let (clause, values) = query.where_except(Some(Facet::Source), &[]);
        facets.sources = if clause.trim().is_empty() {
            count_rows("SELECT job_id, count(*) FROM lead_sources GROUP BY job_id".to_owned(), values)?
        } else {
            count_rows(
                format!(
                    "SELECT job_id, count(*) FROM lead_sources WHERE lead_id IN (SELECT id FROM leads {clause}) GROUP BY job_id"
                ),
                values,
            )?
        };

        let (clause, values) = query.where_except(Some(Facet::Category), &[]);
        facets.categories = count_rows(
            format!(
                "SELECT coalesce(category, '') AS v, count(*) AS n FROM leads {clause} GROUP BY v ORDER BY n DESC, v LIMIT 300"
            ),
            values,
        )?;

        let (clause, values) = query.where_except(Some(Facet::State), &[]);
        facets.states = count_rows(
            format!(
                "SELECT coalesce(state, '') AS v, count(*) AS n FROM leads {clause} GROUP BY v ORDER BY n DESC, v LIMIT 100"
            ),
            values,
        )?;

        let (clause, values) = query.where_except(Some(Facet::City), &["coalesce(city, '') <> ''"]);
        facets.cities = count_rows(
            format!(
                "SELECT {CITY_KEY} AS v, count(*) AS n FROM leads {clause} GROUP BY v ORDER BY n DESC, v LIMIT 300"
            ),
            values,
        )?;

        let (clause, values) = query.where_except(Some(Facet::Phone), &[]);
        (facets.phone_total, facets.with_phone) = connection.query_row(
            &format!(
                "SELECT count(*), coalesce(sum(coalesce(phone, '') <> ''), 0) FROM leads {clause}"
            ),
            params_from_iter(values),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let (clause, values) = query.where_except(Some(Facet::Website), &[]);
        (facets.website_total, facets.with_website) = connection.query_row(
            &format!(
                "SELECT count(*), coalesce(sum(coalesce(website, '') <> ''), 0) FROM leads {clause}"
            ),
            params_from_iter(values),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let (clause, values) = query.where_except(Some(Facet::Email), &[]);
        facets.email = connection.query_row(
            &format!(
                "SELECT count(*),
                        coalesce(sum(email_count > 0), 0),
                        coalesce(sum(email_count > 0 AND email_reach = 'safe'), 0),
                        coalesce(sum(email_count = 0 AND enrich_status IN ('done', 'failed')), 0),
                        coalesce(sum(enrich_status IS NULL OR enrich_status = 'queued'), 0)
                 FROM leads {clause}"
            ),
            params_from_iter(values),
            |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?]),
        )?;

        let (clause, values) = query.where_except(Some(Facet::Rating), &[]);
        (
            facets.rating_total,
            facets.rating_3,
            facets.rating_4,
            facets.rating_45,
        ) = connection.query_row(
            &format!(
                "SELECT count(*), coalesce(sum(rating >= 3), 0), coalesce(sum(rating >= 4), 0), coalesce(sum(rating >= 4.5), 0) FROM leads {clause}"
            ),
            params_from_iter(values),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(facets)
    }

    pub fn lead_sources(&self, lead_id: &str) -> Result<Vec<LeadSource>> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            r#"SELECT s.job_id, j.name, s.query, s.location, s.found_at
               FROM lead_sources s LEFT JOIN jobs j ON j.id = s.job_id
               WHERE s.lead_id = ? ORDER BY s.found_at"#,
        )?;
        let rows = statement.query_map(params![lead_id], |row| {
            Ok(LeadSource {
                job_id: row.get(0)?,
                job_name: row.get(1)?,
                query: row.get(2)?,
                location: row.get(3)?,
                found_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_jobs(&self) -> Result<Vec<JobSummary>> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            "SELECT id, name, status, total, collected, failures, duplicates, started_at, finished_at, error, config_json FROM jobs ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            let config_json: String = row.get(10)?;
            let config = serde_json::from_str(&config_json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(10, Type::Text, Box::new(error))
            })?;
            let status: String = row.get(2)?;
            Ok(JobSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                status: parse_status(&status),
                total: row.get(3)?,
                collected: row.get(4)?,
                failures: row.get(5)?,
                duplicates: row.get(6)?,
                started_at: row.get(7)?,
                finished_at: row.get(8)?,
                error: row.get(9)?,
                config,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_proxies(&self) -> Result<Vec<ProxyEntry>> {
        self.list_proxies_for_group(None)
    }

    pub fn list_proxies_for_group(&self, group_id: Option<&str>) -> Result<Vec<ProxyEntry>> {
        let connection = self.read()?;
        if let Some(group_id) = group_id {
            let mut statement = connection.prepare(
                "SELECT p.id, p.url, p.protocol, p.username, p.password, p.enabled, p.status, p.latency_ms, p.last_checked_at, p.consecutive_failures, p.last_error FROM proxies p JOIN proxy_group_members m ON m.proxy_id = p.id WHERE m.group_id = ? ORDER BY p.created_at DESC",
            )?;
            let rows = statement.query_map(params![group_id], proxy_from_row)?;
            return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        let mut statement = connection.prepare(
            "SELECT id, url, protocol, username, password, enabled, status, latency_ms, last_checked_at, consecutive_failures, last_error FROM proxies ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([], proxy_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_proxy_groups(&self) -> Result<Vec<ProxyGroup>> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            "SELECT g.id, g.name, count(m.proxy_id) FROM proxy_groups g LEFT JOIN proxy_group_members m ON m.group_id = g.id GROUP BY g.id, g.name, g.created_at ORDER BY g.created_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ProxyGroup {
                id: row.get(0)?,
                name: row.get(1)?,
                proxy_count: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_proxy_group(&self, id: &str, name: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO proxy_groups (id, name, created_at) VALUES (?, ?, ?)",
            params![id, name.trim(), now()],
        )?;
        Ok(())
    }

    pub fn delete_proxy_group(&self, id: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "DELETE FROM proxy_group_members WHERE group_id = ?",
            params![id],
        )?;
        connection.execute("DELETE FROM proxy_groups WHERE id = ?", params![id])?;
        Ok(())
    }

    pub fn add_proxies_to_group(&self, group_id: &str, proxy_ids: &[String]) -> Result<()> {
        let connection = self.lock()?;
        for proxy_id in proxy_ids {
            connection.execute(
                "INSERT INTO proxy_group_members (group_id, proxy_id, created_at) VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
                params![group_id, proxy_id, now()],
            )?;
        }
        Ok(())
    }

    pub fn remove_proxies_from_group(&self, group_id: &str, proxy_ids: &[String]) -> Result<()> {
        let connection = self.lock()?;
        for proxy_id in proxy_ids {
            connection.execute(
                "DELETE FROM proxy_group_members WHERE group_id = ? AND proxy_id = ?",
                params![group_id, proxy_id],
            )?;
        }
        Ok(())
    }

    pub fn save_proxies_batch(&self, proxies: &[ProxyEntry]) -> Result<()> {
        if proxies.is_empty() {
            return Ok(());
        }
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        let mut statement = transaction.prepare(
            r#"INSERT INTO proxies (id, url, protocol, username, password, enabled, status, latency_ms,
                last_checked_at, consecutive_failures, last_error, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT (id) DO UPDATE SET url = excluded.url, protocol = excluded.protocol,
                 username = excluded.username, password = excluded.password, enabled = excluded.enabled,
                 status = excluded.status, latency_ms = excluded.latency_ms, last_checked_at = excluded.last_checked_at,
                 consecutive_failures = excluded.consecutive_failures, last_error = excluded.last_error"#,
        )?;
        let created_at = now();
        for proxy in proxies {
            statement.execute(params![
                proxy.id,
                proxy.url,
                proxy.protocol,
                proxy.username,
                proxy.password,
                proxy.enabled,
                proxy.status,
                proxy.latency_ms,
                proxy.last_checked_at.map(|value| value.to_rfc3339()),
                proxy.consecutive_failures,
                proxy.last_error,
                created_at,
            ])?;
        }
        drop(statement);
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_proxies(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        let mut members =
            transaction.prepare("DELETE FROM proxy_group_members WHERE proxy_id = ?")?;
        let mut history = transaction.prepare("DELETE FROM proxy_checks WHERE proxy_id = ?")?;
        let mut proxies = transaction.prepare("DELETE FROM proxies WHERE id = ?")?;
        for id in ids {
            members.execute(params![id])?;
            history.execute(params![id])?;
            proxies.execute(params![id])?;
        }
        drop(proxies);
        drop(history);
        drop(members);
        transaction.commit()?;
        Ok(())
    }

    /// Stores a batch of check results in one transaction: each proxy's
    /// current state plus a row in its check history.
    pub fn record_proxy_checks(&self, endpoint: &str, records: &[ProxyCheckRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        {
            let checked_at = now();
            let mut update = transaction.prepare_cached(
                "UPDATE proxies SET status = ?1, latency_ms = ?2, last_checked_at = ?3, \
                 consecutive_failures = CASE WHEN ?1 IN ('healthy', 'slow') THEN 0 ELSE consecutive_failures + 1 END, \
                 last_error = ?4 WHERE id = ?5",
            )?;
            let mut history = transaction.prepare_cached(
                "INSERT INTO proxy_checks (proxy_id, checked_at, endpoint, status, latency_ms, exit_ip, error) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )?;
            for record in records {
                update.execute(params![
                    record.status,
                    record.latency_ms,
                    checked_at,
                    record.error,
                    record.proxy_id
                ])?;
                history.execute(params![
                    record.proxy_id,
                    checked_at,
                    endpoint,
                    record.status,
                    record.latency_ms,
                    record.exit_ip,
                    record.error
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn set_proxy_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE proxies SET enabled = ? WHERE id = ?",
            params![enabled, id],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<DashboardStats> {
        let connection = self.read()?;
        let jobs: i64 = connection.query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))?;
        let active_jobs: i64 = connection.query_row(
            "SELECT count(*) FROM jobs WHERE status IN ('queued', 'running', 'paused', 'stopping')",
            [],
            |row| row.get(0),
        )?;
        let leads: i64 =
            connection.query_row("SELECT count(*) FROM leads", [], |row| row.get(0))?;
        let proxies: i64 =
            connection.query_row("SELECT count(*) FROM proxies", [], |row| row.get(0))?;
        let healthy_proxies: i64 = connection.query_row(
            "SELECT count(*) FROM proxies WHERE enabled AND status = 'healthy'",
            [],
            |row| row.get(0),
        )?;
        let (leads_with_phone, leads_with_website): (i64, i64) = connection.query_row(
            "SELECT coalesce(sum(coalesce(phone, '') <> ''), 0), coalesce(sum(coalesce(website, '') <> ''), 0) FROM leads",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(DashboardStats {
            jobs,
            active_jobs,
            leads,
            leads_with_phone,
            leads_with_website,
            proxies,
            healthy_proxies,
        })
    }

    pub fn get_job(&self, id: &str) -> Result<Option<JobSummary>> {
        Ok(self.list_jobs()?.into_iter().find(|job| job.id == id))
    }

    pub fn delete_job(&self, id: &str, delete_leads: bool) -> Result<usize> {
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        // Leads another search also found stay; they only leave this list.
        let removed_leads = if delete_leads {
            transaction.execute(
                r#"DELETE FROM leads
                   WHERE id IN (SELECT lead_id FROM lead_sources WHERE job_id = ?1)
                   AND NOT EXISTS (SELECT 1 FROM lead_sources s WHERE s.lead_id = leads.id AND s.job_id <> ?1)"#,
                params![id],
            )?
        } else {
            0
        };
        transaction.execute("DELETE FROM lead_sources WHERE job_id = ?", params![id])?;
        transaction.execute("DELETE FROM job_events WHERE job_id = ?", params![id])?;
        transaction.execute("DELETE FROM jobs WHERE id = ?", params![id])?;
        transaction.commit()?;
        Ok(removed_leads)
    }

    pub fn job_event_rows(&self, job_id: &str, limit: usize) -> Result<Vec<JobEvent>> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            "SELECT created_at, level, message FROM job_events WHERE job_id = ? ORDER BY id DESC LIMIT ?",
        )?;
        let rows = statement.query_map(params![job_id, i64::try_from(limit)?], |row| {
            Ok(JobEvent {
                created_at: row.get(0)?,
                level: row.get(1)?,
                message: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_leads(&self, query: &LeadQuery) -> Result<Vec<LeadRow>> {
        let connection = self.read()?;
        let (clause, mut values) = query.where_clause();
        values.push(SqlValue::Integer(i64::try_from(query.limit.max(1))?));
        values.push(SqlValue::Integer(i64::try_from(query.offset)?));
        let sql = format!(
            "SELECT {LEAD_COLUMNS} FROM leads {clause} {} LIMIT ? OFFSET ?",
            query.order_clause()
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), lead_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Ids of every lead matching `query`, ignoring paging.
    pub fn lead_ids(&self, query: &LeadQuery) -> Result<Vec<String>> {
        let connection = self.read()?;
        let (clause, values) = query.where_clause();
        let mut statement = connection.prepare(&format!("SELECT id FROM leads {clause}"))?;
        let rows = statement.query_map(params_from_iter(values), |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_leads(&self, query: &LeadQuery) -> Result<i64> {
        let connection = self.read()?;
        let (clause, values) = query.where_clause();
        let sql = format!("SELECT count(*) FROM leads {clause}");
        Ok(connection.query_row(&sql, params_from_iter(values), |row| row.get(0))?)
    }

    pub fn export_leads_query(&self, query: &LeadQuery) -> Result<Vec<BusinessRecord>> {
        let connection = self.read()?;
        let (clause, values) = query.where_clause();
        let sql = format!(
            "SELECT name, category, address, phone, website, latitude, longitude, rating, review_count, status, hours, maps_url, place_id, source, extra_json FROM leads {clause} {}",
            query.order_clause()
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), record_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_lead(&self, id: &str) -> Result<Option<LeadDetail>> {
        let connection = self.read()?;
        let sql = format!(
            "SELECT {LEAD_COLUMNS}, latitude, longitude, hours, source, extra_json, enriched_at FROM leads WHERE id = ?"
        );
        Ok(connection
            .query_row(&sql, params![id], |row| {
                let lead = lead_from_row(row)?;
                Ok(LeadDetail {
                    created_at: lead.created_at.clone(),
                    row: lead,
                    latitude: row.get(21)?,
                    longitude: row.get(22)?,
                    hours: row.get(23)?,
                    source: row.get(24)?,
                    extra_json: row.get(25)?,
                    enriched_at: row.get(26)?,
                })
            })
            .optional()?)
    }

    pub fn delete_leads(&self, ids: &[String]) -> Result<usize> {
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        let mut removed = 0;
        {
            let mut statement = transaction.prepare("DELETE FROM leads WHERE id = ?")?;
            let mut sources = transaction.prepare("DELETE FROM lead_sources WHERE lead_id = ?")?;
            for id in ids {
                removed += statement.execute(params![id])?;
                sources.execute(params![id])?;
            }
        }
        transaction.commit()?;
        Ok(removed)
    }

    pub fn query_proxies(&self, query: &ProxyQuery) -> Result<Vec<ProxyEntry>> {
        let connection = self.read()?;
        let (clause, mut values) = query.where_clause();
        values.push(SqlValue::Integer(i64::try_from(query.limit.max(1))?));
        values.push(SqlValue::Integer(i64::try_from(query.offset)?));
        let sql = format!(
            "SELECT {PROXY_COLUMNS} FROM proxies {clause} {} LIMIT ? OFFSET ?",
            query.order_clause()
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), proxy_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_all_proxies(&self, query: &ProxyQuery) -> Result<Vec<ProxyEntry>> {
        let connection = self.read()?;
        let (clause, values) = query.where_clause();
        let sql =
            format!("SELECT {PROXY_COLUMNS} FROM proxies {clause} ORDER BY created_at DESC, id");
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), proxy_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_proxies(&self, query: &ProxyQuery) -> Result<i64> {
        let connection = self.read()?;
        let (clause, values) = query.where_clause();
        let sql = format!("SELECT count(*) FROM proxies {clause}");
        Ok(connection.query_row(&sql, params_from_iter(values), |row| row.get(0))?)
    }

    pub fn proxies_by_ids(&self, ids: &[String]) -> Result<Vec<ProxyEntry>> {
        let connection = self.read()?;
        let sql = format!("SELECT {PROXY_COLUMNS} FROM proxies WHERE id = ?");
        let mut statement = connection.prepare(&sql)?;
        let mut proxies = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(proxy) = statement
                .query_row(params![id], proxy_from_row)
                .optional()?
            {
                proxies.push(proxy);
            }
        }
        Ok(proxies)
    }

    pub fn proxy_status_counts(&self, group_id: Option<&str>) -> Result<HashMap<String, i64>> {
        let connection = self.read()?;
        let query = ProxyQuery {
            group_id: group_id.map(ToOwned::to_owned),
            ..ProxyQuery::default()
        };
        let (clause, values) = query.where_clause();
        let sql = format!("SELECT status, count(*) FROM proxies {clause} GROUP BY status");
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    pub fn proxy_checks(&self, proxy_id: &str, limit: usize) -> Result<Vec<ProxyCheckRow>> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            "SELECT checked_at, endpoint, status, latency_ms, exit_ip, error FROM proxy_checks WHERE proxy_id = ? ORDER BY id DESC LIMIT ?",
        )?;
        let rows = statement.query_map(params![proxy_id, i64::try_from(limit)?], |row| {
            Ok(ProxyCheckRow {
                checked_at: row.get(0)?,
                endpoint: row.get(1)?,
                status: row.get(2)?,
                latency_ms: row.get(3)?,
                exit_ip: row.get(4)?,
                error: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn proxy_group_names(&self) -> Result<HashMap<String, Vec<String>>> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            "SELECT m.proxy_id, g.name FROM proxy_group_members m JOIN proxy_groups g ON g.id = m.group_id ORDER BY g.created_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut names: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (proxy_id, name) = row?;
            names.entry(proxy_id).or_default().push(name);
        }
        Ok(names)
    }

    pub fn rename_proxy_group(&self, id: &str, name: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE proxy_groups SET name = ? WHERE id = ?",
            params![name.trim(), id],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let connection = self.read()?;
        Ok(connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now()],
        )?;
        Ok(())
    }

    pub fn file_size(&self) -> Option<u64> {
        let path = self.path.as_ref()?;
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let main = fs::metadata(path).ok()?.len();
        Some(main + fs::metadata(wal).map(|meta| meta.len()).unwrap_or(0))
    }

    #[cfg(test)]
    pub fn export_leads(&self, search: &str) -> Result<Vec<BusinessRecord>> {
        let connection = self.read()?;
        let pattern = format!("%{}%", search.to_ascii_lowercase());
        let mut statement = connection.prepare(
            r#"SELECT name, category, address, phone, website, latitude, longitude, rating, review_count,
               status, hours, maps_url, place_id, source, extra_json FROM leads
               WHERE (? = '' OR lower(name) LIKE ? OR lower(coalesce(category, '')) LIKE ?
               OR lower(coalesce(address, '')) LIKE ? OR lower(coalesce(phone, '')) LIKE ?
               OR lower(coalesce(website, '')) LIKE ?) ORDER BY created_at"#,
        )?;
        let rows = statement.query_map(
            params![search, pattern, pattern, pattern, pattern, pattern],
            |row| {
                let extra_json: String = row.get(14)?;
                let extra = serde_json::from_str(&extra_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(14, Type::Text, Box::new(error))
                })?;
                Ok(BusinessRecord {
                    name: row.get(0)?,
                    category: row.get(1)?,
                    address: row.get(2)?,
                    phone: row.get(3)?,
                    website: row.get(4)?,
                    latitude: row.get(5)?,
                    longitude: row.get(6)?,
                    rating: row.get(7)?,
                    review_count: row.get(8)?,
                    status: row.get(9)?,
                    hours: row.get(10)?,
                    maps_url: row.get(11)?,
                    place_id: row.get(12)?,
                    source: row.get(13)?,
                    extra,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<()> {
        let Some(source) = self.path.as_ref() else {
            return Err(anyhow::anyhow!("in-memory databases cannot be backed up"));
        };
        let destination = destination.as_ref();
        let connection = self.lock()?;
        connection.execute_batch("PRAGMA wal_checkpoint(FULL)")?;
        drop(connection);
        fs::copy(source, destination)
            .with_context(|| format!("copy database to {}", destination.display()))?;
        Ok(())
    }

    pub fn restore_to(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
        let source = source.as_ref();
        let destination = destination.as_ref();
        if !source.is_file() {
            return Err(anyhow::anyhow!(
                "backup does not exist: {}",
                source.display()
            ));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = destination.with_extension("restore.tmp");
        remove_sqlite_sidecars(destination);
        remove_sqlite_sidecars(&temporary);
        let _ = fs::remove_file(&temporary);
        if !is_sqlite_file(source)? {
            anyhow::bail!("{} is not a LocalLeads SQLite backup", source.display());
        }
        fs::copy(source, &temporary)?;
        let _ = fs::remove_file(destination);
        fs::rename(&temporary, destination)?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))
    }

    /// A read-only connection: a free one if there is one, else the next in
    /// turn. Reads never wait for the writer.
    fn read(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        if self.readers.is_empty() {
            return self.lock();
        }
        let start = self.next_reader.fetch_add(1, Ordering::Relaxed);
        for offset in 0..self.readers.len() {
            if let Ok(guard) = self.readers[(start + offset) % self.readers.len()].try_lock() {
                return Ok(guard);
            }
        }
        self.readers[start % self.readers.len()]
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))
    }
}

/// One lead inside an open transaction; see `Database::insert_lead`.
fn insert_lead_row(
    transaction: &rusqlite::Transaction<'_>,
    job_id: &str,
    query: &str,
    location: &str,
    record: &BusinessRecord,
    found_at: &str,
) -> Result<LeadInsert> {
    let dedupe_key = record.dedupe_key();
    let existing: Option<String> = transaction
        .prepare_cached("SELECT id FROM leads WHERE dedupe_key = ?")?
        .query_row(params![dedupe_key], |row| row.get(0))
        .optional()?;
    let (lead_id, is_new) = match existing {
        Some(id) => {
            transaction
                .prepare_cached(
                    r#"UPDATE leads SET
                           category = coalesce(nullif(category, ''), ?),
                           phone = coalesce(nullif(phone, ''), ?),
                           website = coalesce(nullif(website, ''), ?),
                           rating = coalesce(?, rating),
                           review_count = coalesce(?, review_count),
                           status = coalesce(?, status),
                           hours = coalesce(nullif(hours, ''), ?)
                           WHERE id = ?"#,
                )?
                .execute(params![
                    record.category,
                    record.phone,
                    record.website,
                    record.rating,
                    record.review_count,
                    record.status,
                    record.hours,
                    id,
                ])?;
            (id, false)
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let locality = crate::locality::locate(
                record.address.as_deref().unwrap_or(""),
                record.latitude,
                record.longitude,
            );
            transaction
                    .prepare_cached(
                        r#"INSERT INTO leads
                           (id, job_id, dedupe_key, name, category, address, phone, website, latitude, longitude,
                            rating, review_count, status, hours, maps_url, place_id, source, extra_json, created_at,
                            city, state, postal_code, geo_parsed)
                           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)"#,
                    )?
                    .execute(params![
                        id,
                        job_id,
                        dedupe_key,
                        record.name,
                        record.category,
                        record.address,
                        record.phone,
                        record.website,
                        record.latitude,
                        record.longitude,
                        record.rating,
                        record.review_count,
                        record.status,
                        record.hours,
                        record.maps_url,
                        record.place_id,
                        record.source,
                        serde_json::to_string(&record.extra)?,
                        found_at,
                        locality.city,
                        locality.state,
                        locality.postal_code,
                    ])?;
            (id, true)
        }
    };
    let linked = transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO lead_sources (lead_id, job_id, query, location, found_at) VALUES (?, ?, ?, ?, ?)",
            )?
            .execute(params![
                lead_id,
                job_id,
                Some(query).filter(|value| !value.is_empty()),
                Some(location).filter(|value| !value.is_empty()),
                found_at,
            ])?
            > 0;
    Ok(if is_new {
        LeadInsert::New
    } else if linked {
        LeadInsert::Linked
    } else {
        LeadInsert::Repeat
    })
}

fn is_sqlite_file(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut header = [0; 16];
    let bytes = file.read(&mut header)?;
    Ok(bytes == header.len() && &header == b"SQLite format 3\0")
}

fn remove_sqlite_sidecars(path: &Path) {
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        let _ = fs::remove_file(sidecar);
    }
}

fn parse_status(value: &str) -> JobStatus {
    match value {
        "queued" => JobStatus::Queued,
        "running" => JobStatus::Running,
        "paused" => JobStatus::Paused,
        "stopping" => JobStatus::Stopping,
        "completed" => JobStatus::Completed,
        "failed" => JobStatus::Failed,
        "cancelled" => JobStatus::Cancelled,
        _ => JobStatus::Interrupted,
    }
}

fn proxy_from_row(row: &Row<'_>) -> rusqlite::Result<ProxyEntry> {
    let checked: Option<String> = row.get(8)?;
    Ok(ProxyEntry {
        id: row.get(0)?,
        url: row.get(1)?,
        protocol: row.get(2)?,
        username: row.get(3)?,
        password: row.get(4)?,
        enabled: row.get(5)?,
        status: row.get(6)?,
        latency_ms: row.get(7)?,
        last_checked_at: checked.and_then(|value| {
            DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        }),
        consecutive_failures: row.get(9)?,
        last_error: row.get(10)?,
    })
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<BusinessRecord> {
    let extra_json: String = row.get(14)?;
    let extra = serde_json::from_str(&extra_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(14, Type::Text, Box::new(error))
    })?;
    Ok(BusinessRecord {
        name: row.get(0)?,
        category: row.get(1)?,
        address: row.get(2)?,
        phone: row.get(3)?,
        website: row.get(4)?,
        latitude: row.get(5)?,
        longitude: row.get(6)?,
        rating: row.get(7)?,
        review_count: row.get(8)?,
        status: row.get(9)?,
        hours: row.get(10)?,
        maps_url: row.get(11)?,
        place_id: row.get(12)?,
        source: row.get(13)?,
        extra,
    })
}

fn lead_from_row(row: &Row<'_>) -> rusqlite::Result<LeadRow> {
    Ok(LeadRow {
        id: row.get(0)?,
        name: row.get(1)?,
        category: row.get(2)?,
        address: row.get(3)?,
        phone: row.get(4)?,
        website: row.get(5)?,
        rating: row.get(6)?,
        review_count: row.get(7)?,
        status: row.get(8)?,
        maps_url: row.get(9)?,
        place_id: row.get(10)?,
        job_id: row.get(11)?,
        city: row.get(12)?,
        state: row.get(13)?,
        postal_code: row.get(14)?,
        created_at: row.get(15)?,
        email: row.get(16)?,
        email_status: row.get(17)?,
        email_reach: row.get(18)?,
        email_count: row.get(19)?,
        enrich_status: row.get(20)?,
    })
}

fn ensure_column(connection: &Connection, table: &str, column: &str, kind: &str) -> Result<()> {
    let exists: bool = connection.query_row(
        &format!("SELECT count(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?"),
        params![column],
        |row| row.get(0),
    )?;
    if !exists {
        connection.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

/// Links leads saved before lead sources existed to the search that found
/// them, and parses city, state and postal code for leads that lack them.
/// Both steps only touch rows that still need it, so running at every start
/// is cheap.
fn backfill_leads(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        r#"INSERT OR IGNORE INTO lead_sources (lead_id, job_id, query, location, found_at)
           SELECT id, job_id, NULL, NULL, created_at FROM leads
           WHERE NOT EXISTS (SELECT 1 FROM lead_sources s WHERE s.lead_id = leads.id)"#,
        [],
    )?;
    let version: i64 =
        transaction.query_row("SELECT max(version) FROM schema_version", [], |row| row.get(0))?;
    if version < 3 {
        // Listings without a street address get a town from their coordinates.
        transaction.execute(
            "UPDATE leads SET geo_parsed = 0 WHERE coalesce(city, '') = '' AND coalesce(state, '') = '' AND latitude IS NOT NULL",
            [],
        )?;
    }
    let pending: Vec<(String, Option<String>, Option<f64>, Option<f64>)> = {
        let mut statement = transaction
            .prepare("SELECT id, address, latitude, longitude FROM leads WHERE geo_parsed = 0")?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if !pending.is_empty() {
        let mut update = transaction.prepare(
            "UPDATE leads SET city = ?, state = ?, postal_code = ?, geo_parsed = 1 WHERE id = ?",
        )?;
        for (id, address, latitude, longitude) in pending {
            let locality = crate::locality::locate(address.as_deref().unwrap_or(""), latitude, longitude);
            update.execute(params![
                locality.city,
                locality.state,
                locality.postal_code,
                id
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

/// A read-only connection to the database file.
fn open_reader(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open a reader for {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    // 64 MB of page cache each, so repeat counts come from memory.
    connection.execute_batch("PRAGMA cache_size = -65536; PRAGMA temp_store = MEMORY;")?;
    Ok(connection)
}

/// Version 2 stops keeping a copy of Google's raw listing in every lead
/// (about 70% of a lead's size; every field used is already taken out of
/// it). Older databases drop the copies once, then shrink the file.
fn shrink_leads(connection: &Connection) -> Result<()> {
    let version: i64 =
        connection.query_row("SELECT max(version) FROM schema_version", [], |row| row.get(0))?;
    if version >= 2 {
        return Ok(());
    }
    let carrying: i64 = connection.query_row(
        "SELECT count(*) FROM leads WHERE json_type(extra_json, '$.raw') IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if carrying > 0 {
        eprintln!("Shrinking the database: dropping stored raw Maps listings from {carrying} leads (one time, may take a few minutes)…");
        connection.execute(
            "UPDATE leads SET extra_json = json_remove(extra_json, '$.raw') WHERE json_type(extra_json, '$.raw') IS NOT NULL",
            [],
        )?;
    }
    connection.execute("UPDATE schema_version SET version = 2", [])?;
    if carrying > 0 {
        connection.execute_batch("VACUUM;")?;
        eprintln!("Database shrunk.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lead_insert_is_deduplicated() -> Result<()> {
        let database = Database::in_memory()?;
        let config = SearchConfig::default();
        database.create_job("job-1", &config)?;
        let lead = BusinessRecord {
            name: "Example Cafe".to_owned(),
            category: Some("Cafe".to_owned()),
            address: Some("1 Main St".to_owned()),
            phone: None,
            website: None,
            latitude: None,
            longitude: None,
            rating: Some(4.5),
            review_count: Some(10),
            status: Some("OPERATIONAL".to_owned()),
            hours: None,
            maps_url: None,
            place_id: Some("place-1".to_owned()),
            source: "test".to_owned(),
            extra: serde_json::json!({}),
        };
        assert_eq!(
            database.insert_lead("job-1", "cafe", "Austin", &lead)?,
            LeadInsert::New
        );
        assert_eq!(
            database.insert_lead("job-1", "coffee", "Austin", &lead)?,
            LeadInsert::Repeat
        );
        assert_eq!(database.stats()?.leads, 1);
        Ok(())
    }

    fn record(name: &str, place: &str, category: &str, address: &str) -> BusinessRecord {
        BusinessRecord {
            name: name.to_owned(),
            category: Some(category.to_owned()),
            address: Some(address.to_owned()),
            phone: None,
            website: None,
            latitude: None,
            longitude: None,
            rating: None,
            review_count: None,
            status: None,
            hours: None,
            maps_url: None,
            place_id: Some(place.to_owned()),
            source: "test".to_owned(),
            extra: serde_json::json!({}),
        }
    }

    #[test]
    fn later_searches_link_known_leads_and_facets_count_them() -> Result<()> {
        let database = Database::in_memory()?;
        let config = SearchConfig::default();
        database.create_job("a", &config)?;
        database.create_job("b", &config)?;
        let shared = record(
            "Shared Plumbing",
            "p1",
            "Plumber",
            "1 Main St, Austin, TX 78701",
        );
        let mut with_phone = shared.clone();
        with_phone.phone = Some("+1 512 555 0100".to_owned());
        let other = record(
            "Bright Electric",
            "p2",
            "Electrician",
            "2 Oak St, Dallas, TX 75201",
        );

        assert_eq!(
            database.insert_lead("a", "plumber", "Austin", &shared)?,
            LeadInsert::New
        );
        assert_eq!(
            database.insert_lead("b", "plumber", "Texas", &with_phone)?,
            LeadInsert::Linked
        );
        assert_eq!(
            database.insert_lead("b", "electrician", "Texas", &other)?,
            LeadInsert::New
        );

        let stats = database.lead_list_stats()?;
        assert_eq!(stats["a"].leads, 1);
        assert_eq!(stats["b"].leads, 2);
        assert_eq!(stats["b"].first_found, 1);
        // The later sighting filled in the missing phone.
        assert_eq!(stats["a"].with_phone, 1);

        let query = LeadQuery {
            sources: vec!["b".to_owned()],
            categories: vec!["Plumber".to_owned()],
            ..LeadQuery::default()
        };
        let facets = database.lead_facets(&query)?;
        assert_eq!(facets.total, 1);
        // Category counts ignore the category selection itself.
        assert_eq!(facets.categories.len(), 2);
        let cities: Vec<_> = facets.cities.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(cities, ["Austin|TX"]);
        let sources: HashMap<_, _> = facets
            .sources
            .iter()
            .map(|source| (source.value.as_str(), source.count))
            .collect();
        assert_eq!(sources.get("a"), Some(&1));
        assert_eq!(sources.get("b"), Some(&1));

        // Deleting search b with its leads keeps the lead search a also found.
        assert_eq!(database.delete_job("b", true)?, 1);
        assert_eq!(database.stats()?.leads, 1);
        Ok(())
    }
}
