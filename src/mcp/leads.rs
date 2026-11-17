//! Leads: faceted browse with paging, facet counts, per-search lists,
//! detail with sources and enrichment, the leads table's bulk actions
//! (delete, enrich, export a selection or everything matching), and CSV/JSON
//! export.

use super::{LocalLeadsMcp, ToolResult, blocking, fail, page_limit, reply};
use crate::db::{FacetCount, LeadQuery, WebsiteFilter};
use crate::export::{csv_bytes, export_records_to_disk, json_bytes};
use crate::web::views::{EnrichView, percent, place_label};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Most lead ids one call takes; larger sets go through filters.
const MAX_IDS: usize = 1_000;
/// Inline exports stop here; larger ones must go to disk.
const INLINE_EXPORT_MAX: usize = 1_000;
/// Keeps inline replies within what agents accept from one tool call.
const INLINE_EXPORT_MAX_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebsiteInput {
    #[default]
    Any,
    /// Only leads with a website.
    Has,
    /// Only leads without a website.
    None,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EmailInput {
    #[default]
    Any,
    /// Leads with at least one usable email.
    Has,
    /// Leads whose best email verified as safe to send to.
    Safe,
    /// Leads whose website was read and had no email.
    None,
    /// Leads still waiting for enrichment.
    Pending,
}

/// The browse page's filters. All are optional and combine with AND;
/// values inside one list combine with OR.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct LeadFilterInput {
    /// Case-insensitive text matched against name, category, address, phone
    /// and website.
    #[serde(default)]
    pub q: Option<String>,
    /// Search ids: leads found by any of these searches (a search's lead
    /// list). From list_searches.
    #[serde(default, alias = "sources", alias = "search_id")]
    pub search_ids: Vec<String>,
    /// Categories exactly as lead_facets lists them; "" means uncategorized.
    #[serde(default)]
    pub categories: Vec<String>,
    /// State codes as lead_facets lists them, e.g. "TX"; "" means unknown.
    #[serde(default)]
    pub states: Vec<String>,
    /// City keys "City|ST" exactly as lead_facets returns them, e.g.
    /// "Austin|TX".
    #[serde(default)]
    pub cities: Vec<String>,
    /// Only leads with a phone number.
    #[serde(default)]
    pub has_phone: bool,
    /// any (default), has, or none.
    #[serde(default)]
    pub website: WebsiteInput,
    /// Minimum Google rating, e.g. 4 or 4.5.
    #[serde(default)]
    pub min_rating: Option<f64>,
    /// any (default), has, safe, none or pending.
    #[serde(default)]
    pub email: EmailInput,
    /// Only these lead ids: a hand-picked selection, like ticking rows in
    /// the leads table. Combines with the other filters. At most 1000.
    #[serde(default)]
    pub ids: Vec<String>,
}

impl LeadFilterInput {
    pub fn query(&self, sort: &str, limit: usize, offset: usize) -> LeadQuery {
        let trim = |values: &[String]| -> Vec<String> {
            values.iter().map(|value| value.trim().to_owned()).collect()
        };
        LeadQuery {
            search: self.q.clone().unwrap_or_default(),
            sources: trim(&self.search_ids)
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
            categories: trim(&self.categories),
            states: trim(&self.states),
            cities: trim(&self.cities),
            ids: trim(&self.ids)
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
            has_phone: self.has_phone,
            website: match self.website {
                WebsiteInput::Any => WebsiteFilter::Any,
                WebsiteInput::Has => WebsiteFilter::Has,
                WebsiteInput::None => WebsiteFilter::Missing,
            },
            email: match self.email {
                EmailInput::Any => "",
                EmailInput::Has => "has",
                EmailInput::Safe => "safe",
                EmailInput::None => "none",
                EmailInput::Pending => "pending",
            }
            .to_owned(),
            min_rating: self.min_rating,
            sort: sort.to_owned(),
            limit,
            offset,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct BrowseLeadsParams {
    #[serde(flatten)]
    pub filters: LeadFilterInput,
    /// Sort key, optionally with ":asc" or ":desc": added (default, newest
    /// first), oldest, name, city, phone, website, email (safe first), rating,
    /// reviews. Empty
    /// values sort last.
    #[serde(default)]
    pub sort: Option<String>,
    /// Rows per page, default 50, max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Rows to skip. Use next_offset from the previous page.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Only return these columns, e.g. ["id","name","phone","website"]. All
    /// columns: id, name, category, address, phone, website, rating,
    /// review_count, status, maps_url, place_id, job_id (first search to
    /// find it), city, state, postal_code, created_at, email (best found),
    /// email_status, email_reach (safe/risky/unknown/invalid), email_count,
    /// enrich_status (null, queued, done, failed).
    #[serde(default)]
    pub fields: Vec<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct LeadFacetsParams {
    #[serde(flatten)]
    pub filters: LeadFilterInput,
    /// Options per facet, biggest first. Default 25; 0 returns every option.
    #[serde(default)]
    pub facet_limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LeadIdParams {
    /// Lead id (from browse_leads).
    pub id: String,
}

/// Which leads a bulk action works on, like the leads table: ticked rows
/// (`ids`) or every lead matching the filters (`all_matching`).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct LeadSelectionParams {
    #[serde(flatten)]
    pub filters: LeadFilterInput,
    /// Act on every lead matching the filters instead of only `ids`. With
    /// no filters at all, that is every lead in the database. Check the
    /// count with browse_leads (limit 1) first.
    #[serde(default)]
    pub all_matching: bool,
}

impl LeadSelectionParams {
    pub(super) fn query(&self) -> Result<LeadQuery, String> {
        if self.filters.ids.len() > MAX_IDS {
            return Err(format!(
                "pass at most {MAX_IDS} ids per call; for more, use all_matching=true with filters"
            ));
        }
        let query = self.filters.query("", 0, 0);
        if !self.all_matching && query.ids.is_empty() {
            return Err(
                "pass ids, or all_matching=true to act on every lead matching the filters".to_owned(),
            );
        }
        Ok(query)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Csv,
    Json,
    /// CSV and JSON side by side (the dashboard's "Save to disk").
    Both,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ExportLeadsParams {
    #[serde(flatten)]
    pub filters: LeadFilterInput,
    /// csv (default; the full 120-column schema), json, or both.
    #[serde(default)]
    pub format: ExportFormat,
    /// Row order, same keys as browse_leads. Default: newest first.
    #[serde(default)]
    pub sort: Option<String>,
    /// Where to write. A file path, a directory, or (for "both") a path
    /// without extension. Relative paths are relative to the database's
    /// folder. Default: the database's folder, named after the search (or
    /// "localleads") plus a timestamp. Ignored when inline is true.
    #[serde(default)]
    pub path: Option<String>,
    /// Return the content in the reply instead of writing a file. Only for
    /// up to 1000 leads and a single format.
    #[serde(default)]
    pub inline: bool,
}

fn facet_list(
    counts: &[FacetCount],
    limit: usize,
    label: impl Fn(&str) -> String,
    key_name: &str,
) -> Value {
    let shown: Vec<Value> = counts
        .iter()
        .take(if limit == 0 { usize::MAX } else { limit })
        .map(|facet| {
            json!({ key_name: facet.value, "label": label(&facet.value), "count": facet.count })
        })
        .collect();
    json!({
        "options": shown,
        "more": counts.len().saturating_sub(shown.len()),
    })
}

/// Opening hours as ["Monday: 9 AM–5 PM", ...]. Google sends nested arrays
/// with one `[day, n, [y, m, d], [[text, ...], ...], ...]` entry per day;
/// plain text is split on `|`, `;` and newlines.
fn hours_lines(raw: &str) -> Value {
    fn day_line(day: &Value) -> Option<String> {
        let name = day.get(0)?.as_str()?;
        let slots = day.get(3)?.as_array()?;
        let times: Vec<&str> = slots
            .iter()
            .filter_map(|slot| slot.get(0)?.as_str())
            .collect();
        Some(if times.is_empty() {
            format!("{name}: closed")
        } else {
            format!("{name}: {}", times.join(", "))
        })
    }
    fn find(value: &Value) -> Option<Vec<String>> {
        let items = value.as_array()?;
        let lines: Vec<String> = items.iter().filter_map(day_line).collect();
        if lines.len() >= items.len().min(7) && !lines.is_empty() {
            return Some(lines);
        }
        items.iter().find_map(find)
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => find(&value).map_or(value, |lines| json!(lines)),
        Err(_) => json!(
            raw.split(['|', ';', '\n'])
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
        ),
    }
}

fn city_label(key: &str) -> String {
    let (city, state) = key.split_once('|').unwrap_or((key, ""));
    place_label(Some(city), Some(state))
}

fn slug(name: &str) -> String {
    let raw: String = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let joined = raw
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if joined.is_empty() {
        "localleads".to_owned()
    } else {
        joined
    }
}

impl LocalLeadsMcp {
    fn job_names(&self) -> Result<HashMap<String, String>, String> {
        Ok(self
            .state
            .db
            .list_jobs()
            .map_err(fail)?
            .into_iter()
            .map(|job| (job.id, job.name))
            .collect())
    }

    /// Default export file stem, like the dashboard: the search's name when
    /// exporting one list.
    fn export_stem(&self, filters: &LeadFilterInput) -> String {
        let name = match filters.search_ids.as_slice() {
            [only] => self
                .state
                .db
                .get_job(only.trim())
                .ok()
                .flatten()
                .map(|job| job.name),
            _ => None,
        };
        format!(
            "{}-{}",
            slug(&name.unwrap_or_default()),
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        )
    }

    fn data_dir(&self) -> PathBuf {
        self.state
            .db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    }
}

#[tool_router(router = leads_router, vis = "pub(super)")]
impl LocalLeadsMcp {
    #[tool(
        description = "Page through leads with the browse page's filters (text, searches, categories, states, cities, phone, website, email, minimum rating) and sorting. Returns total matches, one page of rows and next_offset. Paging is stable, so walk large lists (200k+ rows) with offset. Get filter values from lead_facets.",
        annotations(read_only_hint = true)
    )]
    async fn browse_leads(&self, Parameters(params): Parameters<BrowseLeadsParams>) -> ToolResult {
        let limit = page_limit(params.limit);
        let offset = params.offset.unwrap_or(0);
        let query = params
            .filters
            .query(params.sort.as_deref().unwrap_or("").trim(), limit, offset);
        let state = self.state.clone();
        let (total, rows) = blocking(move || {
            let total = state.db.count_leads(&query)?;
            let rows = state.db.query_leads(&query)?;
            Ok((total, rows))
        })
        .await?;
        let fields: Vec<String> = params
            .fields
            .iter()
            .map(|field| field.trim().to_owned())
            .filter(|field| !field.is_empty())
            .collect();
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let mut value = serde_json::to_value(row).unwrap_or_default();
                if !fields.is_empty()
                    && let Value::Object(map) = &mut value
                {
                    map.retain(|key, _| fields.contains(key));
                }
                value
            })
            .collect();
        let end = offset + rows.len();
        reply(json!({
            "total": total,
            "offset": offset,
            "limit": limit,
            "returned": rows.len(),
            "next_offset": (i64::try_from(end).unwrap_or(i64::MAX) < total).then_some(end),
            "leads": rows,
        }))
    }

    #[tool(
        description = "Facet counts for the browse page: how many leads match per search, category, state and city, plus has-phone, website and rating buckets. Each facet is counted with the other filters applied (its own selection left out), exactly like the dashboard. Use the returned values as browse_leads filters.",
        annotations(read_only_hint = true)
    )]
    async fn lead_facets(&self, Parameters(params): Parameters<LeadFacetsParams>) -> ToolResult {
        let limit = params.facet_limit.unwrap_or(25);
        let query = params.filters.query("", 0, 0);
        let state = self.state.clone();
        let mut facets = blocking(move || state.db.lead_facets(&query)).await?;
        let names = self.job_names()?;
        // Searches newest first, like the dashboard.
        let order: HashMap<String, usize> = self
            .state
            .db
            .list_jobs()
            .map_err(fail)?
            .into_iter()
            .enumerate()
            .map(|(index, job)| (job.id, index))
            .collect();
        facets
            .sources
            .sort_by_key(|facet| order.get(&facet.value).copied().unwrap_or(usize::MAX));
        let source_label = |id: &str| {
            names
                .get(id)
                .cloned()
                .unwrap_or_else(|| "Deleted search".to_owned())
        };
        let category_label = |value: &str| {
            if value.is_empty() {
                "Uncategorized".to_owned()
            } else {
                value.to_owned()
            }
        };
        let state_label = |value: &str| {
            if value.is_empty() {
                "Unknown".to_owned()
            } else {
                value.to_owned()
            }
        };
        reply(json!({
            "total": facets.total,
            "searches": facet_list(&facets.sources, limit, source_label, "search_id"),
            "categories": facet_list(&facets.categories, limit, category_label, "category"),
            "states": facet_list(&facets.states, limit, state_label, "state"),
            "cities": facet_list(&facets.cities, limit, city_label, "city"),
            "phone": { "any": facets.phone_total, "has_phone": facets.with_phone },
            "website": {
                "any": facets.website_total,
                "has": facets.with_website,
                "none": facets.website_total - facets.with_website,
            },
            "email": {
                "any": facets.email[0],
                "has": facets.email[1],
                "safe": facets.email[2],
                "none": facets.email[3],
                "pending": facets.email[4],
            },
            "min_rating": {
                "any": facets.rating_total,
                "3": facets.rating_3,
                "4": facets.rating_4,
                "4.5": facets.rating_45,
            },
        }))
    }

    #[tool(
        description = "The Leads page's lists: totals across all leads (phone/website coverage, top categories, distinct cities) and one list per search with its lead count, how many were already found by an earlier search, phone/website coverage, top categories and when it last found a lead.",
        annotations(read_only_hint = true)
    )]
    async fn lead_lists(&self) -> ToolResult {
        let state = self.state.clone();
        let (stats, (categories, cities), lists, jobs) = blocking(move || {
            Ok((
                state.db.stats()?,
                state.db.lead_overview()?,
                state.db.lead_list_stats()?,
                state.db.list_jobs()?,
            ))
        })
        .await?;
        let top = |categories: &[(String, i64)]| -> Value {
            json!(
                categories
                    .iter()
                    .take(10)
                    .map(|(name, count)| json!({ "category": name, "count": count }))
                    .collect::<Vec<_>>()
            )
        };
        let per_search: Vec<Value> = jobs
            .iter()
            .map(|job| {
                let list = lists.get(&job.id).cloned().unwrap_or_default();
                json!({
                    "search_id": job.id,
                    "name": job.name,
                    "status": job.status.to_string(),
                    "leads": list.leads,
                    "first_found_here": list.first_found,
                    "also_found_earlier": (list.leads - list.first_found).max(0),
                    "with_phone_pct": percent(list.with_phone, list.leads),
                    "with_website_pct": percent(list.with_website, list.leads),
                    "top_categories": top(&list.categories),
                    "categories": list.categories.len(),
                    "last_found_at": list.last_found.or_else(|| job.finished_at.clone()).or_else(|| job.started_at.clone()),
                })
            })
            .collect();
        reply(json!({
            "all_leads": {
                "leads": stats.leads,
                "with_phone_pct": percent(stats.leads_with_phone, stats.leads),
                "with_website_pct": percent(stats.leads_with_website, stats.leads),
                "without_website": stats.leads - stats.leads_with_website,
                "top_categories": top(&categories),
                "categories": categories.len(),
                "cities": cities,
            },
            "searches": per_search,
        }))
    }

    #[tool(
        description = "Everything stored for one lead: contact fields, location, coordinates, opening hours, what enrichment found (emails with their mail-server verdicts, website details, social links, people, Google reviews per star, owner), the raw data, and every search that found it (with the query and location of that lookup).",
        annotations(read_only_hint = true)
    )]
    async fn get_lead(&self, Parameters(params): Parameters<LeadIdParams>) -> ToolResult {
        let id = params.id.trim().to_owned();
        let state = self.state.clone();
        let (detail, sources) = blocking(move || {
            let detail = state.db.get_lead(&id)?;
            let sources = state.db.lead_sources(&id)?;
            Ok((detail, sources))
        })
        .await?;
        let detail = detail.ok_or_else(|| format!("lead '{}' not found", params.id))?;
        let enriched_at = detail.enriched_at.clone();
        let view = crate::web::views::LeadDetailView::new(detail.clone(), sources.clone());
        let mut value = serde_json::to_value(&detail.row).map_err(fail)?;
        value["enrichment"] = enrichment_json(&view.enrich, enriched_at);
        value["latitude"] = json!(detail.latitude);
        value["longitude"] = json!(detail.longitude);
        value["hours"] = detail.hours.as_deref().map(hours_lines).unwrap_or(Value::Null);
        value["source"] = json!(detail.source);
        value["raw"] =
            serde_json::from_str::<Value>(&detail.extra_json).unwrap_or(json!(detail.extra_json));
        value["found_by"] = json!(
            sources
                .into_iter()
                .map(|source| json!({
                    "search_id": source.job_id,
                    "search_name": source.job_name.unwrap_or_else(|| "Deleted search".to_owned()),
                    "query": source.query,
                    "location": source.location,
                    "found_at": source.found_at,
                }))
                .collect::<Vec<_>>()
        );
        reply(value)
    }

    #[tool(
        description = "Permanently delete leads from every search's list, like the leads table's bulk Delete: pass ids (up to 1000), or all_matching=true with browse filters to delete every matching lead (every lead when no filter is set). Cannot be undone; confirm with the user and check the count with browse_leads first.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true)
    )]
    async fn delete_leads(&self, Parameters(params): Parameters<LeadSelectionParams>) -> ToolResult {
        let query = params.query()?;
        let requested = query.ids.len();
        let state = self.state.clone();
        let (matched, removed) = blocking(move || {
            let ids = state.db.lead_ids(&query)?;
            let removed = state.db.delete_leads(&ids)?;
            Ok((ids.len(), removed))
        })
        .await?;
        self.state.mark_jobs_dirty();
        if removed > 0 {
            self.state.notice(
                "ok",
                format!("Deleted {removed} {}", if removed == 1 { "lead" } else { "leads" }),
            );
        }
        let mut value = json!({ "matched": matched, "deleted": removed });
        if !params.all_matching {
            value["requested"] = json!(requested);
        }
        reply(value)
    }

    #[tool(
        description = "Find emails and details for existing leads, like the leads table's bulk Enrich: reads each lead's website (emails, social links, phones, people), checks each email with its mail server, and opens the Google place page (reviews per star, owner, description, photos). Pass ids (up to 1000), or all_matching=true with browse filters. Runs in the background through every enabled proxy. Leads already queued are skipped; finished ones are checked again. Track progress with lead_facets (email.pending) or browse_leads email=pending, and read results with get_lead.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn enrich_leads(&self, Parameters(params): Parameters<LeadSelectionParams>) -> ToolResult {
        let query = params.query()?;
        let state = self.state.clone();
        let ids = blocking(move || state.db.lead_ids(&query)).await?;
        if ids.is_empty() {
            return Err("no leads match; check the ids or filters with browse_leads".to_owned());
        }
        let queued = crate::app::AppState::enrich_leads(&self.state, &ids).map_err(fail)?;
        let mut note = if queued == 0 {
            "Those leads are already being enriched.".to_owned()
        } else {
            "Runs in the background; leads fill in as each one finishes. lead_facets email.pending counts what is left.".to_owned()
        };
        if self.mode == super::EngineMode::Embedded && queued > 0 {
            note.push_str(" No dashboard is running, so this MCP process does the work and stops it when it exits.");
        }
        reply(json!({
            "matched": ids.len(),
            "queued": queued,
            "already_queued": ids.len() - queued,
            "note": note,
        }))
    }

    #[tool(
        description = "Export leads as CSV (the full 120-column schema), JSON, or both: everything matching the browse filters, or a hand-picked selection with ids (the leads table's bulk Export). Writes files to disk (default: beside the database, named after the search) and returns their paths, or returns small exports (up to 1000 leads) inline.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn export_leads(&self, Parameters(params): Parameters<ExportLeadsParams>) -> ToolResult {
        if params.filters.ids.len() > MAX_IDS {
            return Err(format!("pass at most {MAX_IDS} ids per call; for more, use filters"));
        }
        let query = params
            .filters
            .query(params.sort.as_deref().unwrap_or("").trim(), 0, 0);
        if params.inline {
            if matches!(params.format, ExportFormat::Both) {
                return Err("inline exports take one format: csv or json".to_owned());
            }
            let state = self.state.clone();
            let count_query = query.clone();
            let total = blocking(move || state.db.count_leads(&count_query)).await?;
            if total > INLINE_EXPORT_MAX as i64 {
                return Err(format!(
                    "{total} leads match; inline exports hold at most {INLINE_EXPORT_MAX}. Write to disk (inline=false) or narrow the filters."
                ));
            }
        }
        let state = self.state.clone();
        let records = blocking(move || state.db.export_leads_query(&query)).await?;
        let count = records.len();
        if params.inline {
            let bytes = match params.format {
                ExportFormat::Json => json_bytes(&records),
                _ => csv_bytes(&records),
            }
            .map_err(fail)?;
            if bytes.len() > INLINE_EXPORT_MAX_BYTES {
                return Err(format!(
                    "this export is {} KB, too big to return inline (limit {} KB); write it to disk (inline=false) or narrow the filters",
                    bytes.len() / 1024,
                    INLINE_EXPORT_MAX_BYTES / 1024
                ));
            }
            return reply(json!({
                "count": count,
                "format": if matches!(params.format, ExportFormat::Json) { "json" } else { "csv" },
                "content": String::from_utf8_lossy(&bytes),
            }));
        }
        let data_dir = self.data_dir();
        let default_stem = self.export_stem(&params.filters);
        let target = match params.path.as_deref().map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) => {
                let path = PathBuf::from(shellexpand_home(path));
                let path = if path.is_relative() { data_dir.join(path) } else { path };
                if path.is_dir() { path.join(&default_stem) } else { path }
            }
            None => data_dir.join(&default_stem),
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("create folder {}: {error}", parent.display())
            })?;
        }
        let format = params.format;
        let written = blocking(move || -> anyhow::Result<Vec<PathBuf>> {
            match format {
                ExportFormat::Both => {
                    let stem = target.with_extension("");
                    export_records_to_disk(&records, &stem)?;
                    Ok(vec![stem.with_extension("csv"), stem.with_extension("json")])
                }
                ExportFormat::Csv | ExportFormat::Json => {
                    let (extension, bytes) = if matches!(format, ExportFormat::Json) {
                        ("json", json_bytes(&records)?)
                    } else {
                        ("csv", csv_bytes(&records)?)
                    };
                    let path = if target.extension().is_some() {
                        target
                    } else {
                        target.with_extension(extension)
                    };
                    let temporary = path.with_extension(format!("{extension}.tmp"));
                    std::fs::write(&temporary, bytes)?;
                    std::fs::rename(&temporary, &path)?;
                    Ok(vec![path])
                }
            }
        })
        .await?;
        let paths: Vec<String> = written.iter().map(|path| path.display().to_string()).collect();
        self.state
            .notice("ok", format!("Exported {count} leads to {}", paths.join(" and ")));
        reply(json!({ "count": count, "files": paths }))
    }
}

/// What enrichment found, from the same view the lead popup shows. Empty
/// parts are left out. `raw` still holds every stored field.
pub(super) fn enrichment_json(view: &EnrichView, enriched_at: Option<String>) -> Value {
    fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
        let empty = match &value {
            Value::Null => true,
            Value::String(text) => text.is_empty(),
            Value::Array(items) => items.is_empty(),
            Value::Object(items) => items.is_empty(),
            _ => false,
        };
        if !empty {
            map.insert(key.to_owned(), value);
        }
    }
    let mut out = Map::new();
    put(
        &mut out,
        "status",
        json!(if view.status.is_empty() { "not enriched" } else { view.status.as_str() }),
    );
    put(&mut out, "enriched_at", json!(enriched_at));
    let emails: Vec<Value> = view
        .emails
        .iter()
        .map(|email| {
            let mut entry = Map::new();
            put(&mut entry, "email", json!(email.email));
            put(&mut entry, "verdict", json!(if email.verdict.is_empty() { "not checked" } else { email.verdict }));
            put(&mut entry, "details", json!(email.details));
            put(&mut entry, "person", json!(email.person));
            put(&mut entry, "found_as", json!(email.found));
            put(&mut entry, "found_on", json!(email.source));
            put(&mut entry, "checks", json!(email.checks));
            put(&mut entry, "gravatar", json!(email.gravatar));
            Value::Object(entry)
        })
        .collect();
    put(&mut out, "emails", json!(emails));
    let mut website = Map::new();
    put(&mut website, "title", json!(view.website_title));
    put(&mut website, "description", json!(view.website_description));
    put(&mut website, "built_with", json!(view.website_generator));
    put(&mut website, "tracking", json!(view.website_tags));
    if view.pages > 0 {
        website.insert("pages_read".to_owned(), json!(view.pages));
    }
    put(&mut website, "phones", json!(view.phones));
    put(&mut out, "website", Value::Object(website));
    let socials: Map<String, Value> = view
        .socials
        .iter()
        .map(|(label, url)| ((*label).to_owned(), json!(url)))
        .collect();
    put(&mut out, "socials", Value::Object(socials));
    put(&mut out, "people", json!(view.people));
    let mut google = Map::new();
    let per_star: Map<String, Value> = view
        .per_score
        .iter()
        .map(|(star, count, _)| (star.to_string(), json!(count)))
        .collect();
    put(&mut google, "reviews_per_star", Value::Object(per_star));
    put(&mut google, "review_topics", json!(view.review_tags));
    if let Some(verified) = view.verified {
        google.insert("claimed_by_owner".to_owned(), json!(verified));
    }
    put(&mut google, "owner", json!(view.owner));
    put(&mut google, "owner_link", json!(view.owner_link));
    put(&mut google, "description", json!(view.description));
    put(&mut google, "about", json!(view.about));
    put(&mut google, "photo", json!(view.photo));
    put(&mut google, "street_view", json!(view.street_view));
    let ids: Map<String, Value> = view
        .ids
        .iter()
        .map(|(label, value)| ((*label).to_owned(), json!(value)))
        .collect();
    put(&mut google, "ids", Value::Object(ids));
    put(&mut out, "google_place", Value::Object(google));
    put(&mut out, "errors", json!(view.errors));
    Value::Object(out)
}

/// Expands a leading "~/" to the home directory.
pub(crate) fn shellexpand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest).display().to_string(),
        _ => path.to_owned(),
    }
}
