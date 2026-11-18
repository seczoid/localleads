use super::views::{EventView, JobView, Opt, StagesView, config_rows, fmt_count, setup_sentence};
use super::{AppError, RenderHtml, WebResult, flag, notify, redirect, render};
use crate::app::SharedState;
use crate::models::{
    AreaDepth, DEFAULT_OUTPUT_FIELDS, LocationMode, LocationSelection,
    MAX_CONCURRENCY, SearchConfig, SearchMode, country_region, country_subdivisions, parse_filter,
};
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse};
use axum_extra::extract::Form;
use serde::Deserialize;

pub const FILTER_FIELDS: &[&str] = &[
    "name",
    "category",
    "address",
    "phone",
    "website",
    "rating",
    "review_count",
    "status",
    "query",
    "location",
];
pub const FILTER_OPERATORS: &[&str] = &[
    "=",
    "!=",
    ">",
    ">=",
    "<",
    "<=",
    "contains",
    "in",
    "is_empty",
    "not_empty",
];

// ------------------------------------------------------------------ list

/// The searches list moved to the home page; old links land there.
pub async fn list() -> axum::response::Redirect {
    axum::response::Redirect::permanent("/")
}

// ---------------------------------------------------------------- detail

#[derive(Template)]
#[template(path = "job_detail.html")]
struct JobDetailPage {
    nav: &'static str,
    job: JobView,
    stages: StagesView,
    setup: String,
    config: Vec<(&'static str, String)>,
    events: Vec<EventView>,
}

#[derive(Template)]
#[template(path = "partials/job_live.html")]
struct JobLive {
    job: JobView,
    stages: StagesView,
    events: Vec<EventView>,
}

fn load_job(state: &SharedState, id: &str) -> WebResult<(JobView, crate::models::JobSummary)> {
    let job = state
        .db
        .get_job(id)?
        .ok_or_else(|| AppError::not_found("Search not found"))?;
    Ok((JobView::new(&job, state.is_live(&job.id)), job))
}

/// The Google Maps and website progress of one search.
pub(super) fn job_stages(state: &SharedState, job: &crate::models::JobSummary) -> WebResult<StagesView> {
    let counts = if job.config.enriches() {
        state.job_enrich_counts(&job.id)?
    } else {
        crate::db::EnrichCounts::default()
    };
    let pace = state.job_web_pace(&job.id, counts.read);
    Ok(StagesView::new(job, state.job_live_progress(&job.id), counts, pace))
}

pub async fn detail(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let (job, summary) = load_job(&state, &id)?;
    let events = state
        .db
        .job_event_rows(&id, 200)?
        .into_iter()
        .map(EventView::from)
        .collect();
    let group = match &summary.config.proxy_group_id {
        Some(group_id) => state
            .db
            .list_proxy_groups()?
            .into_iter()
            .find(|group| &group.id == group_id)
            .map(|group| group.name),
        None => None,
    };
    render(JobDetailPage {
        nav: "jobs",
        stages: job_stages(&state, &summary)?,
        setup: setup_sentence(&summary.config, group.as_deref()),
        job,
        config: config_rows(&summary.config),
        events,
    })
}

pub async fn live(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let html = state.shared_render(&format!("job:{id}"), || {
        let (job, summary) = load_job(&state, &id)?;
        let events = state
            .db
            .job_event_rows(&id, 200)?
            .into_iter()
            .map(EventView::from)
            .collect();
        JobLive {
            stages: job_stages(&state, &summary)?,
            job,
            events,
        }
        .render_html()
    })?;
    Ok(Html(html).into_response())
}

#[derive(Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    limit: usize,
}

/// The newest `limit` events, for reading further back than the live view.
pub async fn events(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> WebResult {
    load_job(&state, &id)?;
    let limit = query.limit.clamp(200, 20_000);
    let total = state.db.job_event_count(&id)?;
    let events: Vec<EventView> = state
        .db
        .job_event_rows(&id, limit)?
        .into_iter()
        .map(EventView::from)
        .collect();
    let html = JobEvents { events }.render_html()?;
    let mut response = Html(html).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&total.to_string()) {
        response.headers_mut().insert("x-event-total", value);
    }
    Ok(response)
}

#[derive(Template)]
#[template(path = "partials/job_events.html")]
struct JobEvents {
    events: Vec<EventView>,
}

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    level: String,
    #[serde(default)]
    q: String,
}

/// The whole log as plain text, oldest first, with the page's filters.
pub async fn log_txt(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> WebResult {
    let (job, _) = load_job(&state, &id)?;
    let level = query.level.trim().to_ascii_lowercase();
    let mut text = String::new();
    for event in state.db.job_events_matching(&id, &level, &query.q)? {
        let event = EventView::from(event);
        text.push_str(&format!("{}  {:<5}  {}\n", event.time, event.level.to_uppercase(), event.message));
    }
    let name: String = job
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let name = if name.is_empty() { "search".to_owned() } else { name };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}-log.txt\""),
            ),
        ],
        text,
    )
        .into_response())
}

// --------------------------------------------------------------- actions

pub async fn toggle_pause(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let status = state
        .toggle_pause(&id)
        .map_err(|error| AppError::conflict(error.to_string()))?;
    Ok(notify("ok", &format!("Search {status}"), &["jobs-changed"]))
}

pub async fn stop(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    state
        .stop_job(&id)
        .map_err(|error| AppError::conflict(error.to_string()))?;
    Ok(notify("info", "Stopping search…", &["jobs-changed"]))
}

pub async fn continue_job(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    state
        .continue_job(&id)
        .map_err(|error| AppError::conflict(error.to_string()))?;
    Ok(notify("ok", "Carrying on from where it stopped…", &["jobs-changed"]))
}

pub async fn rerun(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult {
    let (_, summary) = load_job(&state, &id)?;
    let new_id = state.start_job(summary.config)?;
    Ok(redirect(&headers, &format!("/jobs/{new_id}")))
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    leads: Option<String>,
}

pub async fn delete(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> WebResult {
    let (job, _) = load_job(&state, &id)?;
    if job.active {
        return Err(AppError::conflict("Stop the search before deleting it"));
    }
    let task_state = state.clone();
    let delete_leads = flag(&query.leads);
    let removed = tokio::task::spawn_blocking(move || task_state.db.delete_job(&id, delete_leads))
        .await
        .map_err(anyhow::Error::from)??;
    let count = removed.count;
    state.notice_with_undo(
        "ok",
        if count > 0 {
            format!("Deleted search \"{}\" and {count} leads", job.name)
        } else {
            format!("Deleted search \"{}\"", job.name)
        },
        Some(&removed.undo),
    );
    state.mark_jobs_dirty();
    Ok(redirect(&headers, "/"))
}

// ------------------------------------------------------------------ form

#[derive(Deserialize, Default, Clone)]
pub struct JobForm {
    #[serde(default)]
    job_name: String,
    #[serde(default)]
    queries: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    location_country: String,
    #[serde(default)]
    location_mode: String,
    #[serde(default)]
    states: Vec<String>,
    /// Single cities picked without their whole state, as "Louisville, KY".
    #[serde(default)]
    cities: Vec<String>,
    #[serde(default)]
    postal_codes: String,
    #[serde(default)]
    custom_locations: String,
    /// "region", "city" or "zip"; see `AreaDepth`. Empty means ZIP codes.
    #[serde(default)]
    depth: String,
    #[serde(default)]
    proxy_group_id: String,
    #[serde(default)]
    total_limit: String,
    #[serde(default)]
    per_query_limit: String,
    #[serde(default)]
    start_at: String,
    #[serde(default)]
    concurrency: String,
    #[serde(default)]
    max_retries: String,
    #[serde(default)]
    collect_all: Option<String>,
    #[serde(default)]
    exact_match: Option<String>,
    #[serde(default)]
    require_proxy: Option<String>,
    #[serde(default)]
    allow_direct: Option<String>,
    #[serde(default)]
    filters: Vec<String>,
    #[serde(default)]
    output_fields: Vec<String>,
    #[serde(default)]
    enrichments: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    find_emails: Option<String>,
    #[serde(default)]
    place_details: Option<String>,
    #[serde(default)]
    verify_emails: Option<String>,
}

fn split_list(value: &str, separators: &[char]) -> Vec<String> {
    value
        .split(|ch| separators.contains(&ch))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_number(value: &str, label: &str, default: u32) -> Result<u32, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(default);
    }
    value
        .replace(',', "")
        .parse::<u32>()
        .map_err(|_| format!("{label} must be a whole number"))
}

/// "Hvac in New York" for a search left unnamed.
fn auto_name(
    queries: &[String],
    selection: Option<&LocationSelection>,
    locations: &[String],
) -> String {
    let what = match queries {
        [] => "Search".to_owned(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} +{}", rest.len()),
    };
    let place = match selection {
        Some(selection) => super::views::location_summary(&SearchConfig {
            location_selection: Some(selection.clone()),
            ..SearchConfig::default()
        }),
        None => match locations {
            [] => String::new(),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} +{}", rest.len()),
        },
    };
    let mut name = if place.is_empty() {
        what
    } else {
        format!("{what} in {place}")
    };
    if let Some(first) = name.get(..1) {
        name = first.to_uppercase() + &name[1..];
    }
    name.chars().take(200).collect()
}

impl JobForm {
    pub fn to_config(&self) -> Result<SearchConfig, String> {
        let defaults = SearchConfig::default();
        let mode = match self.mode.as_str() {
            "maps_url" => SearchMode::MapsUrl,
            "place_id" => SearchMode::PlaceId,
            _ => SearchMode::Text,
        };
        let country = if self.location_country.trim().is_empty() {
            "United States".to_owned()
        } else {
            self.location_country.trim().to_owned()
        };
        let region = match self.region.trim() {
            "" => country_region(&country).unwrap_or_default().to_owned(),
            value => value.to_ascii_lowercase(),
        };
        let (locations, location_selection) = match self.location_mode.as_str() {
            "custom" => (split_list(&self.custom_locations, &['\n', '|']), None),
            mode => {
                let selection = LocationSelection {
                    country,
                    mode: match mode {
                        "states" => LocationMode::States,
                        "postal_codes" => LocationMode::PostalCodes,
                        _ => LocationMode::All,
                    },
                    states: self
                        .states
                        .iter()
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                        .collect(),
                    cities: self
                        .cities
                        .iter()
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                        .collect(),
                    postal_codes: split_list(&self.postal_codes, &[',', '|', '\n', ' ']),
                    depth: match self.depth.trim() {
                        "" => AreaDepth::Zip,
                        value => AreaDepth::parse(value),
                    },
                };
                // The selection is the source of truth; its places are worked
                // out when the search runs (a whole country is ~41,000 ZIPs).
                (Vec::new(), Some(selection))
            }
        };
        if location_selection.is_none() && locations.is_empty() {
            return Err("Enter at least one custom location".to_owned());
        }
        let output_fields = if self.output_fields.is_empty() {
            defaults.output_fields.clone()
        } else {
            self.output_fields.clone()
        };
        let queries = split_list(&self.queries, &['\n', '|']);
        let job_name = match self.job_name.trim() {
            "" => auto_name(&queries, location_selection.as_ref(), &locations),
            name => name.to_owned(),
        };
        Ok(SearchConfig {
            job_name,
            queries,
            locations,
            location_selection,
            mode,
            language: if self.language.trim().is_empty() {
                defaults.language.clone()
            } else {
                self.language.trim().to_owned()
            },
            region,
            exact_match: flag(&self.exact_match),
            total_limit: parse_number(&self.total_limit, "Total limit", defaults.total_limit)?,
            per_query_limit: parse_number(&self.per_query_limit, "Per-lookup limit", 0)?,
            collect_all: flag(&self.collect_all),
            start_at: parse_number(&self.start_at, "Start at", 0)?,
            concurrency: parse_number(&self.concurrency, "Concurrency", defaults.concurrency)?,
            max_retries: parse_number(&self.max_retries, "Retries", defaults.max_retries)?,
            require_proxy: flag(&self.require_proxy),
            allow_direct: flag(&self.allow_direct),
            proxy_group_id: Some(self.proxy_group_id.trim().to_owned())
                .filter(|value| !value.is_empty()),
            filters: self
                .filters
                .iter()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .collect(),
            output_fields,
            enrichments: split_list(&self.enrichments, &['|', ',', '\n']),
            tags: split_list(&self.tags, &['|', ',']),
            find_emails: flag(&self.find_emails),
            place_details: flag(&self.place_details),
            verify_emails: flag(&self.verify_emails),
        })
    }

    fn from_config(config: &SearchConfig) -> Self {
        let cities = config
            .location_selection
            .as_ref()
            .map(|selection| selection.cities.clone())
            .unwrap_or_default();
        let (location_country, location_mode, states, postal_codes, custom_locations, depth) =
            match &config.location_selection {
                Some(selection) => (
                    selection.country.clone(),
                    match selection.mode {
                        LocationMode::All => "all",
                        LocationMode::States => "states",
                        // The picker has no ZIP mode; ZIP codes are custom places.
                        LocationMode::PostalCodes => "custom",
                    }
                    .to_owned(),
                    selection.states.clone(),
                    String::new(),
                    selection.postal_codes.join("\n"),
                    selection.depth.as_str().to_owned(),
                ),
                None => (
                    "United States".to_owned(),
                    "custom".to_owned(),
                    Vec::new(),
                    String::new(),
                    config.locations.join("\n"),
                    AreaDepth::Zip.as_str().to_owned(),
                ),
            };
        Self {
            job_name: config.job_name.clone(),
            queries: config.queries.join("\n"),
            mode: config.mode.to_string(),
            language: config.language.clone(),
            region: config.region.clone(),
            location_country,
            location_mode,
            states,
            cities,
            postal_codes,
            custom_locations,
            depth,
            proxy_group_id: config.proxy_group_id.clone().unwrap_or_default(),
            total_limit: config.total_limit.to_string(),
            per_query_limit: if config.per_query_limit == 0 {
                String::new()
            } else {
                config.per_query_limit.to_string()
            },
            start_at: config.start_at.to_string(),
            concurrency: config.concurrency.to_string(),
            max_retries: config.max_retries.to_string(),
            collect_all: config.collect_all.then(|| "on".to_owned()),
            exact_match: config.exact_match.then(|| "on".to_owned()),
            require_proxy: config.require_proxy.then(|| "on".to_owned()),
            allow_direct: config.allow_direct.then(|| "on".to_owned()),
            filters: config.filters.clone(),
            output_fields: config.output_fields.clone(),
            enrichments: config.enrichments.join(" | "),
            tags: config.tags.join(", "),
            find_emails: config.find_emails.then(|| "on".to_owned()),
            place_details: config.place_details.then(|| "on".to_owned()),
            verify_emails: config.verify_emails.then(|| "on".to_owned()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub value: String,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "job_new.html")]
struct JobNewPage {
    nav: &'static str,
    form: JobForm,
    from_name: String,
    collect_all: bool,
    exact_match: bool,
    require_proxy: bool,
    allow_direct: bool,
    modes: Vec<Opt>,
    groups: Vec<Opt>,
    max_concurrency: u32,
    picker: String,
    output_fields: Vec<Check>,
    filter_fields: Vec<&'static str>,
    filter_ops: Vec<&'static str>,
    filters_json: String,
    /// This build can check emails with their mail servers.
    can_verify: bool,
}

/// What the location picker starts with (see `regionPicker` in app.js).
#[derive(serde::Serialize)]
struct PickerState {
    country: String,
    /// Every country as (name, ISO code), for the country search.
    countries: Vec<(&'static str, &'static str)>,
    /// "zip", "city" or "region".
    depth: String,
    #[serde(flatten)]
    regions: Regions,
    all: bool,
    picked_states: Vec<String>,
    picked_cities: Vec<String>,
}

/// A country's states and how finely it can be split.
#[derive(serde::Serialize)]
struct Regions {
    states: Vec<&'static str>,
    /// Whether states expand into cities and towns.
    expandable: bool,
    /// Whether the finest split is by postal code rather than by town.
    postal: bool,
    /// "ZIP code" in the US, "postal code" elsewhere.
    postal_noun: &'static str,
    /// US state codes, for matching "Louisville, KY" to Kentucky.
    codes: std::collections::HashMap<&'static str, &'static str>,
    /// Places a whole-country search covers, per depth.
    counts: RegionCounts,
    /// Places each state covers: [postal codes or towns at the finest
    /// split, towns].
    areas: std::collections::HashMap<&'static str, [usize; 2]>,
}

#[derive(serde::Serialize)]
struct RegionCounts {
    zip: usize,
    city: usize,
    region: usize,
}

impl Regions {
    fn of(country: &str) -> Self {
        let selection = |depth| LocationSelection {
            country: country.to_owned(),
            mode: LocationMode::All,
            depth,
            ..LocationSelection::default()
        };
        let whole = selection(AreaDepth::Zip);
        let us = crate::models::is_us(country);
        let states = country_subdivisions(country);
        let one = |state: &str, depth| {
            LocationSelection {
                country: country.to_owned(),
                mode: LocationMode::States,
                states: vec![state.to_owned()],
                depth,
                ..LocationSelection::default()
            }
            .place_count()
        };
        let areas = if whole.can_split() {
            states
                .iter()
                .map(|&state| (state, [one(state, AreaDepth::Zip), one(state, AreaDepth::City)]))
                .collect()
        } else {
            Default::default()
        };
        Self {
            areas,
            states: states.to_vec(),
            expandable: whole.can_split(),
            postal: whole.has_postal_codes(),
            postal_noun: if us { "ZIP code" } else { "postal code" },
            codes: if us { crate::geo::us_state_codes() } else { Default::default() },
            counts: RegionCounts {
                zip: whole.place_count(),
                city: selection(AreaDepth::City).place_count(),
                region: selection(AreaDepth::Region).place_count(),
            },
        }
    }
}

impl PickerState {
    fn new(form: &JobForm) -> Self {
        let country = form.location_country.clone();
        Self {
            regions: Regions::of(&country),
            countries: std::iter::once(("United States", "US"))
                .chain(crate::world::countries().iter().copied())
                .collect(),
            depth: if form.depth.trim().is_empty() { "zip".to_owned() } else { form.depth.trim().to_owned() },
            all: form.location_mode == "all",
            picked_states: form.states.clone(),
            picked_cities: form.cities.clone(),
            country,
        }
    }
}

#[derive(Deserialize, Default)]
pub struct NewQuery {
    #[serde(default)]
    from: String,
}

pub async fn new_form(
    State(state): State<SharedState>,
    Query(query): Query<NewQuery>,
) -> WebResult {
    let (config, from_name) = match state.db.get_job(&query.from)? {
        Some(job) => {
            let mut config = job.config;
            config.job_name = format!("{} (copy)", job.name);
            (config, job.name)
        }
        None => {
            let config = SearchConfig {
                job_name: String::new(),
                queries: Vec::new(),
                location_selection: Some(LocationSelection {
                    country: "United States".to_owned(),
                    mode: LocationMode::All,
                    depth: AreaDepth::Zip,
                    ..LocationSelection::default()
                }),
                ..SearchConfig::default()
            };
            (config, String::new())
        }
    };
    let form = JobForm::from_config(&config);
    let filters: Vec<serde_json::Value> = form
        .filters
        .iter()
        .filter_map(|raw| parse_filter(raw).ok())
        .map(|filter| {
            serde_json::json!({ "field": filter.field, "op": filter.operator, "value": filter.value })
        })
        .collect();
    let mut groups = vec![Opt::new("", "All enabled proxies", &form.proxy_group_id)];
    for group in state.db.list_proxy_groups()? {
        groups.push(Opt::new(
            group.id.clone(),
            format!("{} ({} proxies)", group.name, group.proxy_count),
            &form.proxy_group_id,
        ));
    }
    let page = JobNewPage {
        can_verify: cfg!(feature = "verify"),
        nav: "new",
        from_name,
        collect_all: flag(&form.collect_all),
        exact_match: flag(&form.exact_match),
        require_proxy: flag(&form.require_proxy),
        allow_direct: flag(&form.allow_direct),
        modes: [
            ("text", "Text query"),
            ("maps_url", "Google Maps URL"),
            ("place_id", "Place ID"),
        ]
        .iter()
        .map(|(value, label)| Opt::new(*value, *label, &form.mode))
        .collect(),
        groups,
        max_concurrency: MAX_CONCURRENCY,
        picker: serde_json::to_string(&PickerState::new(&form)).map_err(anyhow::Error::from)?,
        output_fields: DEFAULT_OUTPUT_FIELDS
            .iter()
            .map(|field| Check {
                value: (*field).to_owned(),
                checked: form.output_fields.iter().any(|value| value == field),
            })
            .collect(),
        filter_fields: FILTER_FIELDS.to_vec(),
        filter_ops: FILTER_OPERATORS.to_vec(),
        filters_json: serde_json::Value::Array(filters).to_string(),
        form,
    };
    render(page)
}

#[derive(Deserialize, Default)]
pub struct RegionsQuery {
    #[serde(default)]
    country: String,
}

/// States or provinces of a country, for the location picker.
pub async fn regions(Query(query): Query<RegionsQuery>) -> WebResult {
    let country = query.country;
    let regions = tokio::task::spawn_blocking(move || Regions::of(&country))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(axum::Json(regions).into_response())
}

/// Google Maps categories for the business type picker. Fixed per build, so
/// browsers can keep it; the page asks for it with the app version.
pub async fn categories() -> axum::response::Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=604800, immutable"),
        ],
        crate::categories::json(),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub struct CitiesQuery {
    #[serde(default)]
    country: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    q: String,
}

/// Cities of a state, or cities matching a search, for the picker.
pub async fn cities(Query(query): Query<CitiesQuery>) -> WebResult {
    let world = (!query.country.trim().is_empty() && !crate::models::is_us(&query.country))
        .then(|| crate::world::country(&query.country))
        .flatten();
    let searching = !query.q.trim().is_empty();
    let cities = match world {
        Some(country) if searching => country.city_search(&query.q, 40),
        Some(country) => country.cities(&query.state),
        None if searching => crate::geo::us_city_search(&query.q, 40),
        None => crate::geo::us_cities(&query.state),
    };
    Ok(axum::Json(cities).into_response())
}

#[derive(Template)]
#[template(path = "partials/job_preview.html")]
struct JobPreview {
    error: String,
    ready: bool,
    what: String,
    place: String,
    lookups: String,
    lookup_math: String,
    goal: String,
    time: String,
    collects: String,
    proxies: String,
    proxy_tone: &'static str,
}

impl JobPreview {
    fn empty(error: String) -> Self {
        Self {
            error,
            ready: false,
            what: String::new(),
            place: String::new(),
            lookups: String::new(),
            lookup_math: String::new(),
            goal: String::new(),
            time: String::new(),
            collects: String::new(),
            proxies: String::new(),
            proxy_tone: "",
        }
    }
}

/// Seconds one worker spends on a lookup, paging included. Rough on
/// purpose: the summary says "about".
const SECONDS_PER_LOOKUP: f64 = 4.0;

pub async fn preview(State(state): State<SharedState>, Form(form): Form<JobForm>) -> WebResult {
    let config = match form.to_config() {
        Ok(config) => config,
        Err(error) => return render(JobPreview::empty(error)),
    };
    // Nothing to complain about until something is typed.
    let typed = config.queries.iter().any(|query| !query.trim().is_empty());
    let error = config
        .validate()
        .err()
        .filter(|_| typed)
        .map(|error| capitalize(&error.to_string()))
        .unwrap_or_default();
    let queries: Vec<&String> = config
        .queries
        .iter()
        .filter(|query| !query.trim().is_empty())
        .collect();
    if !error.is_empty() || queries.is_empty() {
        return render(JobPreview::empty(error));
    }
    let what = match queries.as_slice() {
        [] => String::new(),
        [one] => (*one).clone(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
    };
    let places = config
        .location_selection
        .as_ref()
        .map_or(config.locations.len(), LocationSelection::place_count);
    let area = config
        .location_selection
        .as_ref()
        .filter(|selection| {
            selection.can_split()
                && selection.depth != AreaDepth::Region
                && selection.mode != LocationMode::PostalCodes
        })
        .map_or("place", |selection| match selection.depth {
            AreaDepth::Zip if crate::models::is_us(&selection.country) => "ZIP code",
            AreaDepth::Zip if selection.has_postal_codes() => "postal code",
            _ if crate::models::is_us(&selection.country) => "place",
            _ => "town",
        });
    let plural = |count: usize, noun: &str| {
        format!(
            "{} {noun}{}",
            fmt_count(i64::try_from(count).unwrap_or(i64::MAX)),
            if count == 1 { "" } else { "s" }
        )
    };
    let lookups = config.lookup_count();
    let lookup_math = if queries.len() > 1 {
        format!("· {} × {}", plural(queries.len(), "business type"), plural(places, area))
    } else {
        format!("· one per {area}")
    };
    let workers = f64::from(config.concurrency.max(1));
    let seconds = (lookups as f64 * SECONDS_PER_LOOKUP / workers) as i64;
    let time = if config.collect_all {
        format!("About {} to cover every area", super::views::fmt_duration(seconds))
    } else {
        format!(
            "Up to {}; it stops sooner once it has {} leads",
            super::views::fmt_duration(seconds),
            fmt_count(i64::from(config.total_limit))
        )
    };
    let mut collects = vec!["name, phone, website, address, rating and hours".to_owned()];
    if config.find_emails {
        collects.push("emails and socials from each website".to_owned());
    }
    if config.place_details {
        collects.push("reviews, owner and photos from Google".to_owned());
    }
    if config.verify_emails {
        collects.push("a mail-server check of each email".to_owned());
    }
    let group_id = config.proxy_group_id.as_deref().filter(|id| !id.is_empty());
    let healthy = state
        .db
        .proxy_status_counts(group_id)?
        .get("healthy")
        .copied()
        .unwrap_or(0);
    let group_name = match group_id {
        Some(id) => state
            .db
            .list_proxy_groups()?
            .into_iter()
            .find(|group| group.id == id)
            .map(|group| group.name),
        None => None,
    };
    let pool = group_name.map_or_else(|| "your proxies".to_owned(), |name| format!("the {name} group"));
    let (proxies, proxy_tone) = if healthy > 0 {
        (
            format!("{} healthy in {pool}, {} requests at a time", fmt_count(healthy), config.concurrency),
            "",
        )
    } else if config.require_proxy && !config.allow_direct {
        (
            format!("No healthy proxies in {pool}, and this search requires one. Check proxies first."),
            "warn-text",
        )
    } else {
        (
            format!("None healthy in {pool}, so requests go out directly, {} at a time", config.concurrency),
            if config.concurrency > 20 { "warn-text" } else { "" },
        )
    };
    render(JobPreview {
        ready: true,
        error,
        what,
        place: super::views::location_summary(&config),
        lookups: fmt_count(i64::try_from(lookups).unwrap_or(i64::MAX)),
        lookup_math,
        goal: if config.collect_all {
            "Everything Google Maps has".to_owned()
        } else {
            format!("{} leads", fmt_count(i64::from(config.total_limit)))
        },
        time,
        collects: {
            let mut text = collects.join("; ");
            if let Some(first) = text.get(0..1) {
                text = first.to_uppercase() + &text[1..];
            }
            text
        },
        proxies,
        proxy_tone,
    })
}

pub async fn create(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Form(form): Form<JobForm>,
) -> WebResult {
    let config = form.to_config().map_err(AppError::bad_request)?;
    config
        .validate()
        .map_err(|error| AppError::bad_request(capitalize(&error.to_string())))?;
    let id = state.start_job(config)?;
    Ok(redirect(&headers, &format!("/jobs/{id}")))
}

fn capitalize(value: &str) -> String {
    let value = value.strip_prefix("validation failed: ").unwrap_or(value);
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_round_trips_config() {
        let config = SearchConfig {
            queries: vec!["plumber".to_owned(), "roofer".to_owned()],
            location_selection: Some(LocationSelection {
                country: "United States".to_owned(),
                mode: LocationMode::States,
                states: vec!["Texas".to_owned(), "Ohio".to_owned()],
                cities: Vec::new(),
                postal_codes: Vec::new(),
                depth: AreaDepth::Region,
            }),
            filters: vec!["rating >= 4".to_owned()],
            ..SearchConfig::default()
        };
        let form = JobForm::from_config(&config);
        let parsed = form.to_config().expect("valid form");
        assert_eq!(parsed.queries, config.queries);
        assert_eq!(parsed.searches().len(), 4);
        assert_eq!(parsed.filters, config.filters);
        parsed.validate().expect("valid config");
    }

    #[test]
    fn custom_locations_are_split_by_line() {
        let form = JobForm {
            job_name: "x".to_owned(),
            queries: "cafe".to_owned(),
            location_mode: "custom".to_owned(),
            custom_locations: "Austin, TX\nDallas, TX".to_owned(),
            ..JobForm::default()
        };
        let config = form.to_config().expect("valid");
        assert!(config.location_selection.is_none());
        assert_eq!(config.locations, vec!["Austin, TX", "Dallas, TX"]);
    }

    #[test]
    fn bad_numbers_are_reported() {
        let form = JobForm {
            total_limit: "lots".to_owned(),
            ..JobForm::default()
        };
        assert!(form.to_config().unwrap_err().contains("Total limit"));
    }
}
