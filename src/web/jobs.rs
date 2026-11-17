use super::views::{EventView, JobView, Opt, config_rows, fmt_count};
use super::{AppError, WebResult, flag, notify, redirect, render};
use crate::app::SharedState;
use crate::models::{
    AreaDepth, COUNTRY_OPTIONS, DEFAULT_OUTPUT_FIELDS, LocationMode, LocationSelection,
    MAX_CONCURRENCY, SearchConfig, SearchMode, country_region, country_subdivisions, parse_filter,
};
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
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

#[derive(Deserialize, Default)]
pub struct JobsQuery {
    #[serde(default)]
    status: String,
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsPage {
    nav: &'static str,
    status_options: Vec<Opt>,
    jobs: Vec<JobView>,
}

#[derive(Template)]
#[template(path = "partials/jobs_table.html")]
struct JobsTable {
    jobs: Vec<JobView>,
}

fn load_jobs(state: &SharedState, status: &str) -> WebResult<Vec<JobView>> {
    Ok(state
        .db
        .list_jobs()?
        .iter()
        .filter(|job| match status {
            "" => true,
            "active" => job.status.is_active(),
            other => job.status.to_string() == other,
        })
        .map(|job| JobView::new(job, state.is_live(&job.id)))
        .collect())
}

pub async fn list(State(state): State<SharedState>, Query(query): Query<JobsQuery>) -> WebResult {
    let jobs = load_jobs(&state, &query.status)?;
    let status_options = [
        ("", "All statuses"),
        ("active", "Active"),
        ("completed", "Completed"),
        ("failed", "Failed"),
        ("cancelled", "Cancelled"),
        ("interrupted", "Interrupted"),
    ]
    .iter()
    .map(|(value, label)| Opt::new(*value, *label, &query.status))
    .collect();
    render(JobsPage {
        nav: "jobs",
        status_options,
        jobs,
    })
}

pub async fn table(State(state): State<SharedState>, Query(query): Query<JobsQuery>) -> WebResult {
    render(JobsTable {
        jobs: load_jobs(&state, &query.status)?,
    })
}

// ---------------------------------------------------------------- detail

#[derive(Template)]
#[template(path = "job_detail.html")]
struct JobDetailPage {
    nav: &'static str,
    job: JobView,
    activity: String,
    config: Vec<(&'static str, String)>,
    events: Vec<EventView>,
}

#[derive(Template)]
#[template(path = "partials/job_live.html")]
struct JobLive {
    job: JobView,
    activity: String,
    events: Vec<EventView>,
}

fn load_job(state: &SharedState, id: &str) -> WebResult<(JobView, crate::models::JobSummary)> {
    let job = state
        .db
        .get_job(id)?
        .ok_or_else(|| AppError::not_found("Search not found"))?;
    Ok((JobView::new(&job, state.is_live(&job.id)), job))
}

pub async fn detail(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let (job, summary) = load_job(&state, &id)?;
    let events = state
        .db
        .job_event_rows(&id, 200)?
        .into_iter()
        .map(EventView::from)
        .collect();
    render(JobDetailPage {
        nav: "jobs",
        activity: state.job_activity(&id).unwrap_or_default(),
        job,
        config: config_rows(&summary.config),
        events,
    })
}

pub async fn live(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let (job, _) = load_job(&state, &id)?;
    let events = state
        .db
        .job_event_rows(&id, 200)?
        .into_iter()
        .map(EventView::from)
        .collect();
    render(JobLive {
        activity: state.job_activity(&id).unwrap_or_default(),
        job,
        events,
    })
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
    let removed = state.db.delete_job(&id, flag(&query.leads))?;
    state.notice(
        "ok",
        if removed > 0 {
            format!("Deleted search \"{}\" and {removed} leads", job.name)
        } else {
            format!("Deleted search \"{}\"", job.name)
        },
    );
    state.mark_jobs_dirty();
    Ok(redirect(&headers, "/jobs"))
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
    countries: Vec<Opt>,
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
    states: Vec<&'static str>,
    /// Whether states expand into cities (US only).
    expandable: bool,
    codes: std::collections::HashMap<&'static str, &'static str>,
    all: bool,
    picked_states: Vec<String>,
    picked_cities: Vec<String>,
}

impl PickerState {
    fn new(form: &JobForm) -> Self {
        let country = form.location_country.clone();
        let states = country_subdivisions(&country).to_vec();
        let expandable = country == "United States";
        Self {
            expandable,
            codes: if expandable {
                crate::geo::us_state_codes()
            } else {
                Default::default()
            },
            all: form.location_mode == "all",
            picked_states: form.states.clone(),
            picked_cities: form.cities.clone(),
            states,
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
        countries: COUNTRY_OPTIONS
            .iter()
            .map(|country| Opt::new(*country, *country, &form.location_country))
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
    let states = country_subdivisions(&query.country);
    let expandable = query.country == "United States";
    Ok(axum::Json(serde_json::json!({
        "states": states,
        "expandable": expandable,
        "codes": if expandable { crate::geo::us_state_codes() } else { Default::default() },
    }))
    .into_response())
}

#[derive(Deserialize, Default)]
pub struct CitiesQuery {
    #[serde(default)]
    state: String,
    #[serde(default)]
    q: String,
}

/// Cities of a US state, or US cities matching a search, for the picker.
pub async fn cities(Query(query): Query<CitiesQuery>) -> WebResult {
    let cities = if query.q.trim().is_empty() {
        crate::geo::us_cities(&query.state)
    } else {
        crate::geo::us_city_search(&query.q, 40)
    };
    Ok(axum::Json(cities).into_response())
}

#[derive(Template)]
#[template(path = "partials/job_preview.html")]
struct JobPreview {
    error: String,
    ready: bool,
    goal: String,
    goal_label: &'static str,
    what: String,
    place: String,
    coverage: String,
}

pub async fn preview(Form(form): Form<JobForm>) -> WebResult {
    let config = match form.to_config() {
        Ok(config) => config,
        Err(error) => {
            return render(JobPreview {
                error,
                ready: false,
                goal: String::new(),
                goal_label: "",
                what: String::new(),
                place: String::new(),
                coverage: String::new(),
            });
        }
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
    let what = match queries.as_slice() {
        [] => String::new(),
        [one] => (*one).clone(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
    };
    let places = config
        .location_selection
        .as_ref()
        .map_or(config.locations.len(), LocationSelection::place_count);
    let zips = config.location_selection.as_ref().is_some_and(|selection| {
        selection.can_split()
            && selection.depth == AreaDepth::Zip
            && selection.mode != LocationMode::PostalCodes
    });
    let area = if zips { "ZIP code" } else { "place" };
    let areas = format!(
        "{} {area}{}",
        fmt_count(i64::try_from(places).unwrap_or(i64::MAX)),
        if places == 1 { "" } else { "s" }
    );
    let coverage = if config.collect_all {
        format!("Searches all {areas} and keeps everything Google Maps has.")
    } else if zips {
        format!(
            "Searches {areas}, busiest areas first, and stops at {} leads.",
            fmt_count(i64::from(config.total_limit))
        )
    } else {
        format!(
            "Searches {areas} and stops at {} leads.",
            fmt_count(i64::from(config.total_limit))
        )
    };
    render(JobPreview {
        ready: error.is_empty() && !queries.is_empty(),
        error,
        goal: if config.collect_all {
            "All".to_owned()
        } else {
            fmt_count(i64::from(config.total_limit))
        },
        goal_label: if config.collect_all {
            "leads available"
        } else {
            "leads max"
        },
        what,
        place: super::views::location_summary(&config),
        coverage,
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
