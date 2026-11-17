//! Leads: one list per search, and a faceted view across all of them.

use super::views::{
    JobView, LeadDetailView, LeadView, fmt_ago, fmt_count, percent, place_label,
};
use super::table::{TablePage, TableParams};
use super::{AppError, RenderHtml, WebResult, notify, render};
use crate::app::SharedState;
use crate::db::{FacetCount, LeadFacets, LeadQuery, WebsiteFilter};
use crate::export::{csv_bytes, export_records_to_disk, json_bytes};
use askama::Template;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect};
use axum_extra::extract::Query;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path as FsPath;

/// Facet options shown before "Show more".
const FACET_PREVIEW: usize = 8;

#[derive(Deserialize, Default, Clone, Debug)]
pub struct LeadFilters {
    #[serde(default)]
    pub q: String,
    /// Search (job) ids.
    #[serde(default)]
    pub source: Vec<String>,
    #[serde(default)]
    pub category: Vec<String>,
    #[serde(default)]
    pub state: Vec<String>,
    /// "city|state" keys.
    #[serde(default)]
    pub city: Vec<String>,
    #[serde(default)]
    pub has_phone: String,
    /// "", "has" or "none".
    #[serde(default)]
    pub website: String,
    /// "", has, safe, none or pending.
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub min_rating: String,
    #[serde(default)]
    pub sort: String,
    /// Older links used `job=` and `has_website=`.
    #[serde(default)]
    pub job: String,
    #[serde(default)]
    pub has_website: String,
}

fn on(value: &str) -> bool {
    matches!(value, "on" | "1" | "true")
}

impl LeadFilters {
    fn sources(&self) -> Vec<String> {
        let mut sources = self.source.clone();
        if !self.job.is_empty() && !sources.contains(&self.job) {
            sources.push(self.job.clone());
        }
        sources.retain(|value| !value.is_empty());
        sources
    }

    fn website_value(&self) -> &str {
        match self.website.as_str() {
            "has" | "none" => &self.website,
            _ if on(&self.has_website) => "has",
            _ => "",
        }
    }

    fn email_value(&self) -> &str {
        match self.email.as_str() {
            value @ ("has" | "safe" | "none" | "pending") => value,
            _ => "",
        }
    }

    pub fn query(&self, limit: usize, offset: usize) -> LeadQuery {
        LeadQuery {
            search: self.q.clone(),
            sources: self.sources(),
            categories: self.category.clone(),
            states: self.state.clone(),
            cities: self.city.clone(),
            ids: Vec::new(),
            has_phone: on(&self.has_phone),
            website: match self.website_value() {
                "has" => WebsiteFilter::Has,
                "none" => WebsiteFilter::Missing,
                _ => WebsiteFilter::Any,
            },
            email: self.email_value().to_owned(),
            min_rating: self.min_rating.trim().parse::<f64>().ok(),
            sort: self.sort.clone(),
            limit,
            offset,
        }
    }

    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if !self.q.trim().is_empty() {
            pairs.push(("q", self.q.clone()));
        }
        pairs.extend(self.sources().into_iter().map(|value| ("source", value)));
        pairs.extend(self.category.iter().map(|value| ("category", value.clone())));
        pairs.extend(self.state.iter().map(|value| ("state", value.clone())));
        pairs.extend(self.city.iter().map(|value| ("city", value.clone())));
        if on(&self.has_phone) {
            pairs.push(("has_phone", "1".to_owned()));
        }
        let website = self.website_value();
        if !website.is_empty() {
            pairs.push(("website", website.to_owned()));
        }
        if !self.email_value().is_empty() {
            pairs.push(("email", self.email_value().to_owned()));
        }
        if !self.min_rating.is_empty() {
            pairs.push(("min_rating", self.min_rating.clone()));
        }
        if !self.sort.is_empty() {
            pairs.push(("sort", self.sort.clone()));
        }
        pairs
    }

    /// Query string for these filters, without paging.
    pub fn qs(&self) -> String {
        encode(&self.pairs())
    }

    /// Query string with one filter value removed.
    fn qs_without(&self, key: &str, value: &str) -> String {
        let pairs: Vec<_> = self
            .pairs()
            .into_iter()
            .filter(|(k, v)| !(*k == key && v == value))
            .collect();
        encode(&pairs)
    }

    fn has_filters(&self) -> bool {
        self.pairs().iter().any(|(key, _)| *key != "sort")
    }
}

fn encode(pairs: &[(&str, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn with_qs(path: &str, qs: &str) -> String {
    if qs.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{qs}")
    }
}

// ---------------------------------------------------------------- table data

/// Sort keys for the browse table's columns, in column order.
const LEAD_SORT_KEYS: [&str; 7] = ["name", "city", "phone", "website", "email", "rating", "added"];

pub async fn data(
    State(state): State<SharedState>,
    Query(filters): Query<LeadFilters>,
    Query(params): Query<TableParams>,
) -> WebResult {
    let mut query = filters.query(params.limit(), params.start);
    query.sort = params.sort(&LEAD_SORT_KEYS);
    let page = tokio::task::spawn_blocking(move || -> anyhow::Result<TablePage<LeadView>> {
        let total = state.db.count_leads(&query)?;
        let data = state
            .db
            .query_leads(&query)?
            .into_iter()
            .map(LeadView::from)
            .collect();
        Ok(TablePage {
            draw: params.draw,
            records_total: total,
            records_filtered: total,
            data,
        })
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(axum::Json(page).into_response())
}

// ---------------------------------------------------------------- lists

struct CategoryChip {
    label: String,
    count: String,
    href: String,
}

struct ListCard {
    job: JobView,
    leads: String,
    has_leads: bool,
    shared: String,
    phone_pct: u32,
    website_pct: u32,
    categories: Vec<CategoryChip>,
    more_categories: usize,
    updated: String,
    href: String,
}

struct AllCard {
    leads: String,
    has_leads: bool,
    phone_pct: u32,
    website_pct: u32,
    no_website: String,
    categories: Vec<CategoryChip>,
    more_categories: usize,
    places: String,
}

#[derive(Template)]
#[template(path = "partials/lead_lists.html")]
struct LeadLists {
    all: AllCard,
    cards: Vec<ListCard>,
}

fn chips(source: Option<&str>, categories: &[(String, i64)]) -> (Vec<CategoryChip>, usize) {
    let chips = categories
        .iter()
        .take(3)
        .map(|(label, count)| {
            let mut pairs = Vec::new();
            if let Some(source) = source {
                pairs.push(("source", source.to_owned()));
            }
            let value = if label == "Uncategorized" { "" } else { label };
            pairs.push(("category", value.to_owned()));
            CategoryChip {
                label: label.clone(),
                count: fmt_count(*count),
                href: with_qs("/leads/browse", &encode(&pairs)),
            }
        })
        .collect();
    (chips, categories.len().saturating_sub(3))
}

impl LeadLists {
    fn load(state: &SharedState) -> WebResult<Self> {
        let stats = state.db.stats()?;
        let (all_categories, city_count) = state.db.lead_overview()?;
        let list_stats = state.db.lead_list_stats()?;
        let (all_chips, all_more) = chips(None, &all_categories);
        let all = AllCard {
            leads: fmt_count(stats.leads),
            has_leads: stats.leads > 0,
            phone_pct: percent(stats.leads_with_phone, stats.leads),
            website_pct: percent(stats.leads_with_website, stats.leads),
            no_website: fmt_count(stats.leads - stats.leads_with_website),
            categories: all_chips,
            more_categories: all_more,
            places: fmt_count(city_count),
        };
        let cards = state
            .db
            .list_jobs()?
            .iter()
            .map(|job| {
                let list = list_stats.get(&job.id).cloned().unwrap_or_default();
                let (categories, more_categories) = chips(Some(&job.id), &list.categories);
                ListCard {
                    job: JobView::new(job, state.is_live(&job.id)),
                    leads: fmt_count(list.leads),
                    has_leads: list.leads > 0,
                    shared: if list.leads > list.first_found {
                        fmt_count(list.leads - list.first_found)
                    } else {
                        String::new()
                    },
                    phone_pct: percent(list.with_phone, list.leads),
                    website_pct: percent(list.with_website, list.leads),
                    categories,
                    more_categories,
                    updated: fmt_ago(
                        list.last_found
                            .as_deref()
                            .or(job.finished_at.as_deref())
                            .or(job.started_at.as_deref()),
                    ),
                    href: with_qs("/leads/browse", &encode(&[("source", job.id.clone())])),
                }
            })
            .collect();
        Ok(Self { all, cards })
    }
}

#[derive(Template)]
#[template(path = "leads.html")]
struct ListsPage {
    nav: &'static str,
    lists: String,
}

/// `/leads`: the lead lists. Old filter links go on to the browse view.
pub async fn lists(State(state): State<SharedState>, RawQuery(raw): RawQuery) -> WebResult {
    if let Some(raw) = raw.filter(|value| !value.is_empty()) {
        return Ok(Redirect::to(&format!("/leads/browse?{raw}")).into_response());
    }
    let lists = LeadLists::load(&state)?.render_html()?;
    render(ListsPage { nav: "leads", lists })
}

pub async fn lists_partial(State(state): State<SharedState>) -> WebResult {
    render(LeadLists::load(&state)?)
}

// ---------------------------------------------------------------- browse

struct FacetOption {
    value: String,
    label: String,
    count: String,
    checked: bool,
    /// Past the preview and not selected: shown after "Show more".
    extra: bool,
}

struct FacetGroup {
    param: &'static str,
    title: &'static str,
    options: Vec<FacetOption>,
    extra: usize,
}

struct RadioOption {
    value: &'static str,
    label: &'static str,
    count: String,
    checked: bool,
}

struct Chip {
    label: String,
    href: String,
}

#[derive(Template)]
#[template(path = "partials/leads_browse.html")]
struct BrowseBody {
    total: String,
    groups: Vec<FacetGroup>,
    phone_count: String,
    has_phone: bool,
    website: Vec<RadioOption>,
    email: Vec<RadioOption>,
    rating: Vec<RadioOption>,
    chips: Vec<Chip>,
    filtered: bool,
    oob: bool,
}

fn category_label(value: &str) -> String {
    if value.is_empty() {
        "Uncategorized".to_owned()
    } else {
        value.to_owned()
    }
}

fn facet_group(
    param: &'static str,
    title: &'static str,
    counts: &[FacetCount],
    selected: &[String],
    label: impl Fn(&str) -> String,
) -> FacetGroup {
    let mut options: Vec<FacetOption> = counts
        .iter()
        .enumerate()
        .map(|(index, facet)| {
            let checked = selected.contains(&facet.value);
            FacetOption {
                label: label(&facet.value),
                count: fmt_count(facet.count),
                extra: index >= FACET_PREVIEW && !checked,
                checked,
                value: facet.value.clone(),
            }
        })
        .collect();
    // A selected value with no matches stays visible so it can be unticked.
    for value in selected {
        if !counts.iter().any(|facet| &facet.value == value) {
            options.insert(
                0,
                FacetOption {
                    value: value.clone(),
                    label: label(value),
                    count: "0".to_owned(),
                    checked: true,
                    extra: false,
                },
            );
        }
    }
    let extra = options.iter().filter(|option| option.extra).count();
    FacetGroup {
        param,
        title,
        options,
        extra,
    }
}

fn city_label(key: &str) -> String {
    let (city, state) = key.split_once('|').unwrap_or((key, ""));
    place_label(Some(city), Some(state))
}

impl BrowseBody {
    fn load(state: &SharedState, filters: &LeadFilters, oob: bool) -> WebResult<Self> {
        let query = filters.query(0, 0);
        let facets: LeadFacets = state.db.lead_facets(&query)?;
        let jobs = state.db.list_jobs()?;
        let names: HashMap<&str, &str> = jobs
            .iter()
            .map(|job| (job.id.as_str(), job.name.as_str()))
            .collect();
        // Searches in the order they were created, newest first.
        let order: HashMap<&str, usize> = jobs
            .iter()
            .enumerate()
            .map(|(index, job)| (job.id.as_str(), index))
            .collect();
        let mut sources = facets.sources.clone();
        sources.sort_by_key(|facet| order.get(facet.value.as_str()).copied().unwrap_or(usize::MAX));
        let source_label =
            |id: &str| names.get(id).map_or_else(|| "Deleted search".to_owned(), |name| (*name).to_owned());
        let selected_sources = filters.sources();
        let state_label = |value: &str| {
            if value.is_empty() {
                "Unknown".to_owned()
            } else {
                value.to_owned()
            }
        };
        let groups = vec![
            facet_group("source", "Search", &sources, &selected_sources, source_label),
            facet_group(
                "category",
                "Category",
                &facets.categories,
                &filters.category,
                category_label,
            ),
            facet_group("state", "State", &facets.states, &filters.state, state_label),
            facet_group("city", "City", &facets.cities, &filters.city, city_label),
        ];

        let website = filters.website_value();
        let website_options = vec![
            RadioOption {
                value: "",
                label: "Any",
                count: fmt_count(facets.website_total),
                checked: website.is_empty(),
            },
            RadioOption {
                value: "has",
                label: "Has website",
                count: fmt_count(facets.with_website),
                checked: website == "has",
            },
            RadioOption {
                value: "none",
                label: "No website",
                count: fmt_count(facets.website_total - facets.with_website),
                checked: website == "none",
            },
        ];
        let email = filters.email_value();
        let email_options = [
            ("", "Any"),
            ("has", "Has email"),
            ("safe", "Verified email"),
            ("none", "No email found"),
            ("pending", "Not checked yet"),
        ]
        .into_iter()
        .zip(facets.email)
        // "Verified" only means something when emails were checked.
        .filter(|((value, _), count)| *value != "safe" || *count > 0 || email == "safe")
        .map(|((value, label), count)| RadioOption {
            value,
            label,
            count: fmt_count(count),
            checked: email == value,
        })
        .collect();
        let rating = filters.min_rating.as_str();
        let rating_options = [
            ("", "Any", facets.rating_total),
            ("3", "3.0 and up", facets.rating_3),
            ("4", "4.0 and up", facets.rating_4),
            ("4.5", "4.5 and up", facets.rating_45),
        ]
        .into_iter()
        .map(|(value, label, count)| RadioOption {
            value,
            label,
            count: fmt_count(count),
            checked: rating == value,
        })
        .collect();

        let mut chips = Vec::new();
        let mut chip = |key: &str, value: &str, label: String| {
            chips.push(Chip {
                label,
                href: with_qs("/leads/browse", &filters.qs_without(key, value)),
            });
        };
        if !filters.q.trim().is_empty() {
            chip("q", &filters.q, format!("“{}”", filters.q.trim()));
        }
        for value in &selected_sources {
            chip("source", value, source_label(value));
        }
        for value in &filters.category {
            chip("category", value, category_label(value));
        }
        for value in &filters.state {
            chip("state", value, state_label(value));
        }
        for value in &filters.city {
            chip("city", value, city_label(value));
        }
        if on(&filters.has_phone) {
            chip("has_phone", "1", "Has phone".to_owned());
        }
        match website {
            "has" => chip("website", "has", "Has website".to_owned()),
            "none" => chip("website", "none", "No website".to_owned()),
            _ => {}
        }
        match email {
            "has" => chip("email", "has", "Has email".to_owned()),
            "safe" => chip("email", "safe", "Verified email".to_owned()),
            "none" => chip("email", "none", "No email found".to_owned()),
            "pending" => chip("email", "pending", "Email not checked".to_owned()),
            _ => {}
        }
        if !filters.min_rating.is_empty() {
            chip("min_rating", &filters.min_rating, format!("★ {}+", filters.min_rating));
        }

        Ok(Self {
            total: fmt_count(facets.total),
            groups,
            phone_count: fmt_count(facets.with_phone),
            has_phone: on(&filters.has_phone),
            website: website_options,
            email: email_options,
            rating: rating_options,
            filtered: filters.has_filters(),
            chips,
            oob,
        })
    }
}

#[derive(Template)]
#[template(path = "leads_browse.html")]
struct BrowsePage {
    nav: &'static str,
    title: String,
    subtitle: String,
    job_id: String,
    q: String,
    body: String,
}

pub async fn browse(
    State(state): State<SharedState>,
    Query(filters): Query<LeadFilters>,
) -> WebResult {
    let body = BrowseBody::load(&state, &filters, false)?.render_html()?;
    let sources = filters.sources();
    let single = match sources.as_slice() {
        [only] => state.db.get_job(only)?,
        _ => None,
    };
    let (title, subtitle, job_id) = match single {
        Some(job) => {
            let view = JobView::new(&job, false);
            let subtitle = [view.queries.clone(), view.locations.clone()]
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            (job.name, subtitle, job.id)
        }
        None if sources.len() > 1 => (
            format!("{} searches", sources.len()),
            String::new(),
            String::new(),
        ),
        None => ("All leads".to_owned(), String::new(), String::new()),
    };
    render(BrowsePage {
        nav: "leads",
        title,
        subtitle,
        job_id,
        q: filters.q.clone(),
        body,
    })
}

/// Facets plus the results header (out of band); the table reloads itself
/// after the swap. Keeps the address bar in step so reload and back return
/// to the same filters.
pub async fn browse_partial(
    State(state): State<SharedState>,
    Query(filters): Query<LeadFilters>,
) -> WebResult {
    let mut response = render(BrowseBody::load(&state, &filters, true)?)?;
    let url = with_qs("/leads/browse", &filters.qs());
    if let Ok(value) = HeaderValue::from_str(&url) {
        response.headers_mut().insert("HX-Replace-Url", value);
    }
    Ok(response)
}

// ---------------------------------------------------------------- detail

#[derive(Template)]
#[template(path = "partials/lead_detail.html")]
struct LeadDetailModal {
    detail: LeadDetailView,
}

pub async fn detail(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let detail = state
        .db
        .get_lead(&id)?
        .ok_or_else(|| AppError::not_found("Lead not found"))?;
    let sources = state.db.lead_sources(&id)?;
    render(LeadDetailModal {
        detail: LeadDetailView::new(detail, sources),
    })
}

pub async fn delete(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let removed = state.db.delete_leads(&[id])?;
    if removed == 0 {
        return Err(AppError::not_found("Lead not found"));
    }
    state.mark_jobs_dirty();
    Ok(notify(
        "ok",
        "Lead deleted",
        &["leads-changed", "close-modal"],
    ))
}

// ---------------------------------------------------------------- bulk

/// The browse table's selection: picked ids, or every lead matching the
/// filters.
#[derive(Deserialize)]
pub struct BulkLeads {
    #[serde(default)]
    action: String,
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    all_matching: String,
    /// The filter form as a query string.
    #[serde(default)]
    filters: String,
    /// The table's sort, as "key:asc|desc".
    #[serde(default)]
    sort: String,
}

impl BulkLeads {
    fn filters(&self) -> WebResult<LeadFilters> {
        let uri: axum::http::Uri = format!("/?{}", self.filters)
            .parse()
            .map_err(|_| AppError::bad_request("Bad filters"))?;
        let Query(filters) = Query::<LeadFilters>::try_from_uri(&uri)
            .map_err(|error| AppError::bad_request(error.to_string()))?;
        Ok(filters)
    }

    fn query(&self) -> WebResult<LeadQuery> {
        let filters = self.filters()?;
        let mut query = if on(&self.all_matching) {
            filters.query(0, 0)
        } else {
            let ids: Vec<String> = self
                .ids
                .iter()
                .filter(|id| !id.is_empty())
                .cloned()
                .collect();
            if ids.is_empty() {
                return Err(AppError::bad_request("Select some leads first"));
            }
            LeadQuery {
                ids,
                ..LeadQuery::default()
            }
        };
        if !self.sort.is_empty() {
            query.sort = self.sort.clone();
        }
        Ok(query)
    }
}

pub async fn bulk(
    State(state): State<SharedState>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<BulkLeads>,
) -> WebResult {
    let query = form.query()?;
    match form.action.as_str() {
        "csv" | "json" => {
            let stem = export_stem(&state, &form.filters()?);
            let records =
                tokio::task::spawn_blocking(move || state.db.export_leads_query(&query))
                    .await
                    .map_err(anyhow::Error::from)??;
            Ok(if form.action == "csv" {
                attachment("text/csv; charset=utf-8", &format!("{stem}.csv"), csv_bytes(&records)?)
            } else {
                attachment("application/json", &format!("{stem}.json"), json_bytes(&records)?)
            })
        }
        "delete" => {
            let task_state = state.clone();
            let removed = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
                let ids = task_state.db.lead_ids(&query)?;
                task_state.db.delete_leads(&ids)
            })
            .await
            .map_err(anyhow::Error::from)??;
            state.mark_jobs_dirty();
            Ok(notify(
                "ok",
                &format!("Deleted {} {}", fmt_count(removed as i64), if removed == 1 { "lead" } else { "leads" }),
                &["leads-changed"],
            ))
        }
        "enrich" => {
            let task_state = state.clone();
            let ids = tokio::task::spawn_blocking(move || task_state.db.lead_ids(&query))
                .await
                .map_err(anyhow::Error::from)??;
            let queued = crate::app::AppState::enrich_leads(&state, &ids)?;
            let message = if queued == 0 {
                "Those leads are already being enriched".to_owned()
            } else {
                format!(
                    "Finding emails and details for {} {}; the table fills in as each one finishes",
                    fmt_count(queued as i64),
                    if queued == 1 { "lead" } else { "leads" }
                )
            };
            Ok(notify("ok", &message, &[]))
        }
        _ => Err(AppError::bad_request("Unknown action")),
    }
}

// ---------------------------------------------------------------- export

/// File name stem: the search's name when exporting one list.
fn export_stem(state: &SharedState, filters: &LeadFilters) -> String {
    let name = match filters.sources().as_slice() {
        [only] => state.db.get_job(only).ok().flatten().map(|job| job.name),
        _ => None,
    };
    let slug: String = name
        .unwrap_or_else(|| "localleads".to_owned())
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!(
        "{}-{}",
        if slug.is_empty() { "localleads" } else { &slug },
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    )
}

async fn export_records(
    state: SharedState,
    filters: LeadFilters,
) -> WebResult<Vec<crate::models::BusinessRecord>> {
    let records =
        tokio::task::spawn_blocking(move || state.db.export_leads_query(&filters.query(0, 0)))
            .await
            .map_err(anyhow::Error::from)??;
    Ok(records)
}

fn attachment(content_type: &str, file_name: &str, body: Vec<u8>) -> axum::response::Response {
    (
        [
            (header::CONTENT_TYPE, content_type.to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{file_name}\""),
            ),
        ],
        Body::from(body),
    )
        .into_response()
}

pub async fn export_csv(
    State(state): State<SharedState>,
    Query(filters): Query<LeadFilters>,
) -> WebResult {
    let stem = export_stem(&state, &filters);
    let records = export_records(state, filters).await?;
    Ok(attachment(
        "text/csv; charset=utf-8",
        &format!("{stem}.csv"),
        csv_bytes(&records)?,
    ))
}

pub async fn export_json(
    State(state): State<SharedState>,
    Query(filters): Query<LeadFilters>,
) -> WebResult {
    let stem = export_stem(&state, &filters);
    let records = export_records(state, filters).await?;
    Ok(attachment(
        "application/json",
        &format!("{stem}.json"),
        json_bytes(&records)?,
    ))
}

/// Writes CSV and JSON beside the database, like the desktop app did.
pub async fn export_disk(
    State(state): State<SharedState>,
    Query(filters): Query<LeadFilters>,
) -> WebResult {
    let directory = state
        .db_path
        .parent()
        .unwrap_or_else(|| FsPath::new("."))
        .to_path_buf();
    let stem = directory.join(export_stem(&state, &filters));
    let records = export_records(state.clone(), filters).await?;
    let stem_for_task = stem.clone();
    let count =
        tokio::task::spawn_blocking(move || export_records_to_disk(&records, &stem_for_task))
            .await
            .map_err(anyhow::Error::from)??;
    let message = format!("Exported {count} leads to {}.csv and .json", stem.display());
    state.notice("ok", message.clone());
    Ok(notify("ok", &message, &[]))
}
