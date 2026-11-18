//! Searches ("jobs" in code): options, preview, create, list, detail,
//! pause/resume/stop, rerun and delete.
//!
//! Search input maps onto `SearchConfig` through its serde shape: the input
//! is laid over `SearchConfig::default()` serialized as JSON, then
//! deserialized back. A field added to `SearchConfig` is accepted right away
//! (as a pass-through key, defaulted from `Default`); add it to
//! `SearchInput` as well to document it in the tool schema. The
//! `search_input_documents_every_config_field` test fails until you do.

use super::{LocalLeadsMcp, ToolResult, blocking, fail, reply};
use crate::models::{
    DEFAULT_OUTPUT_FIELDS, JobStatus, JobSummary, LocationMode,
    LocationSelection, SearchConfig, SearchMode, country_options, country_region, country_subdivisions,
};
use crate::web::views::{location_summary, percent};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Same lists as the dashboard's filter builder.
const FILTER_FIELDS: &[&str] = &[
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
const FILTER_OPERATORS: &[&str] = &[
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
/// The dashboard warns above this many lookups.
const LARGE_SEARCH: usize = 50_000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModeInput {
    /// Free-text queries such as "plumber".
    Text,
    /// Each query is a Google Maps search or place URL.
    MapsUrl,
    /// Each query is a Google place ID.
    PlaceId,
}

impl From<ModeInput> for SearchMode {
    fn from(mode: ModeInput) -> Self {
        match mode {
            ModeInput::Text => Self::Text,
            ModeInput::MapsUrl => Self::MapsUrl,
            ModeInput::PlaceId => Self::PlaceId,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocationModeInput {
    /// Every built-in state/province of the country (or the country itself
    /// when it has no built-in list).
    All,
    /// Only the listed `states`.
    States,
    /// Only the listed `postal_codes`.
    PostalCodes,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DepthInput {
    /// One lookup per state/province (about 40 results each).
    Region,
    /// One lookup per city or town (US only).
    City,
    /// One lookup per ZIP code (US only): the most results and the most
    /// lookups. The dashboard's default.
    Zip,
}

/// The dashboard's location picker (Whole country / States / ZIP codes).
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct LocationSelectionInput {
    /// Country name exactly as listed by search_options, e.g. "United States".
    /// Defaults to "United States".
    #[serde(default)]
    pub country: Option<String>,
    pub mode: LocationModeInput,
    /// For mode "states": full state/province names from search_options,
    /// e.g. ["Texas", "New York"].
    #[serde(default)]
    pub states: Vec<String>,
    /// For mode "states" (US only): single cities searched without their
    /// whole state, as "Louisville, KY" (the `label` from find_cities). Each
    /// city covers all of its ZIP codes. Can be combined with `states` or
    /// used alone.
    #[serde(default)]
    pub cities: Vec<String>,
    /// For mode "postal_codes": ZIP/postal codes, e.g. ["10001", "11201"].
    #[serde(default)]
    pub postal_codes: Vec<String>,
    /// US only: how finely states and cities are split into lookups.
    /// Default "zip", like the dashboard (the most leads; areas are searched
    /// busiest first, so a lead limit is reached quickly). "region" is one
    /// lookup per state (about 40 results each).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<DepthInput>,
    /// Any other location setting the engine supports, by field name.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Search settings. Every field is optional; omitted fields keep the
/// default (or, for rerun_search/copy_from, the copied search's value).
/// Each query runs once per location.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct SearchInput {
    /// Search name shown in the dashboard and used for export file names.
    #[serde(default, alias = "name")]
    pub job_name: Option<String>,
    /// Queries, one per entry, e.g. ["plumber", "emergency plumber"]. In
    /// maps_url mode these are Google Maps URLs; in place_id mode, place IDs.
    #[serde(default)]
    pub queries: Option<Vec<String>>,
    /// How queries are read. Default "text".
    #[serde(default)]
    pub mode: Option<ModeInput>,
    /// Pick locations by country, states or postal codes. Use either this
    /// or `locations`, not both.
    #[serde(default)]
    pub location_selection: Option<LocationSelectionInput>,
    /// Custom places, one per entry: cities, regions, neighborhoods or
    /// coordinates, e.g. ["Austin, TX", "Brooklyn, New York"]. The
    /// dashboard's "Custom places" mode.
    #[serde(default)]
    pub locations: Option<Vec<String>>,
    /// Google interface language code. Default "en".
    #[serde(default)]
    pub language: Option<String>,
    /// Google region code (gl), e.g. "us", "gb". Default: the
    /// location_selection country's code (Google returns far fewer results
    /// without it), else "us".
    #[serde(default)]
    pub region: Option<String>,
    /// Wrap each query in quotes for exact matching. Default false.
    #[serde(default)]
    pub exact_match: Option<bool>,
    /// Stop after this many leads in total. Ignored when collect_all is
    /// true. Default: see search_options.defaults.
    #[serde(default)]
    pub total_limit: Option<u32>,
    /// At most this many leads per query x location lookup; 0 means no
    /// per-lookup limit. Ignored when collect_all is true.
    #[serde(default)]
    pub per_query_limit: Option<u32>,
    /// Collect every available result page and ignore both limits.
    #[serde(default)]
    pub collect_all: Option<bool>,
    /// Skip the first N results of each lookup. Default 0.
    #[serde(default)]
    pub start_at: Option<u32>,
    /// Parallel lookups. Higher is faster but needs more proxies. Default:
    /// see search_options.defaults.
    #[serde(default)]
    pub concurrency: Option<u32>,
    /// Retries per request, 0-10. Default 2.
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Require a healthy proxy for every request. Default false.
    #[serde(default)]
    pub require_proxy: Option<bool>,
    /// Fall back to direct requests when no proxy is available. Default true.
    #[serde(default)]
    pub allow_direct: Option<bool>,
    /// Use only proxies in this group (id from list_proxy_groups). Omit or
    /// pass "" for all enabled proxies.
    #[serde(default)]
    pub proxy_group_id: Option<String>,
    /// Keep only leads matching every filter. Each is "field operator value",
    /// e.g. "rating >= 4", "website not_empty", "category contains dent",
    /// "status in OPERATIONAL,CLOSED_TEMPORARILY". Fields and operators are
    /// listed by search_options.
    #[serde(default)]
    pub filters: Option<Vec<String>>,
    /// Output fields stored with the search (see search_options). Empty
    /// means the default set. CSV exports always use the full schema.
    #[serde(default)]
    pub output_fields: Option<Vec<String>>,
    /// Enrichment labels stored with the search; no external service is
    /// called.
    #[serde(default)]
    pub enrichments: Option<Vec<String>>,
    /// Tags, e.g. ["q4-outreach", "hvac"].
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Read each lead's website for emails, socials, phones and site
    /// details (no AI; fixed extraction rules). Default true.
    #[serde(default)]
    pub find_emails: Option<bool>,
    /// Open each lead's Google place page for reviews per star, review
    /// topics, owner (claimed listing), description and photos. Default true.
    #[serde(default)]
    pub place_details: Option<bool>,
    /// Ask each found email's mail server whether the address exists.
    /// Slower; only in builds with email verification. Default false.
    #[serde(default)]
    pub verify_emails: Option<bool>,
    /// Any other search setting the engine supports, by its config field
    /// name (see `defaults` in search_options). Unknown names are rejected.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn clean_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindCategoriesParams {
    /// Business type or its start, e.g. "dent". Leave out to list the groups.
    #[serde(default)]
    pub q: Option<String>,
    /// A group key from the group list, e.g. "healthcare", to list all of
    /// its categories. Ignored when q is given.
    #[serde(default)]
    pub group: Option<String>,
    /// Most categories to return. Default 40, max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindCitiesParams {
    /// City name or its start, e.g. "louis". At least 2 letters.
    #[serde(default)]
    pub q: Option<String>,
    /// A state name (or US code), e.g. "Kentucky" or "KY", to list all its
    /// cities. Ignored when q is given.
    #[serde(default)]
    pub state: Option<String>,
    /// Country as listed by search_options. Defaults to "United States".
    #[serde(default)]
    pub country: Option<String>,
    /// Most cities to return. Default 40, max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Lays `input` over `base` (or the defaults) and validates the result.
pub fn build_config(input: SearchInput, base: Option<SearchConfig>) -> Result<SearchConfig, String> {
    let copying = base.is_some();
    let base = base.unwrap_or_default();
    let defaults = SearchConfig::default();
    let Value::Object(mut map) = serde_json::to_value(&base).map_err(fail)? else {
        return Err("search config did not serialize to an object".to_owned());
    };
    let known: Vec<String> = map.keys().cloned().collect();
    let extra_sets_locations = input.extra.contains_key("locations")
        || input.extra.contains_key("location_selection");
    for (key, value) in input.extra {
        if !known.contains(&key) {
            return Err(format!(
                "unknown search setting '{key}'; known settings: {}",
                known.join(", ")
            ));
        }
        map.insert(key, value);
    }
    let mut set = |key: &str, value: Value| {
        map.insert(key.to_owned(), value);
    };
    if let Some(name) = input.job_name {
        set("job_name", json!(name.trim()));
    }
    if let Some(queries) = input.queries {
        set("queries", json!(clean_list(queries)));
    }
    if let Some(mode) = input.mode {
        set("mode", serde_json::to_value(SearchMode::from(mode)).map_err(fail)?);
    }
    match (input.location_selection, input.locations) {
        (Some(_), Some(_)) => {
            return Err(
                "pass either location_selection or locations (custom places), not both".to_owned(),
            );
        }
        (Some(selection), None) => {
            let mut value = serde_json::to_value(LocationSelectionInput {
                country: Some(
                    selection
                        .country
                        .as_deref()
                        .map(str::trim)
                        .filter(|country| !country.is_empty())
                        .unwrap_or("United States")
                        .to_owned(),
                ),
                states: clean_list(selection.states),
                cities: clean_list(selection.cities),
                postal_codes: clean_list(selection.postal_codes),
                depth: Some(selection.depth.unwrap_or(DepthInput::Zip)),
                ..selection
            })
            .map_err(fail)?;
            value["mode"] = serde_json::to_value(match selection.mode {
                LocationModeInput::All => LocationMode::All,
                LocationModeInput::States => LocationMode::States,
                LocationModeInput::PostalCodes => LocationMode::PostalCodes,
            })
            .map_err(fail)?;
            let selection: LocationSelection = serde_json::from_value(value)
                .map_err(|error| format!("invalid location_selection: {error}"))?;
            // Like the dashboard: the selection is the source of truth and
            // its places are worked out when the search runs (a whole
            // country is ~41,000 ZIP codes).
            set("locations", json!([]));
            if input.region.as_deref().is_none_or(|region| region.trim().is_empty()) {
                set("region", json!(country_region(&selection.country).unwrap_or_default()));
            }
            set(
                "location_selection",
                serde_json::to_value(&selection).map_err(fail)?,
            );
        }
        (None, Some(locations)) => {
            let locations = clean_list(locations);
            if locations.is_empty() {
                return Err("enter at least one custom location".to_owned());
            }
            set("locations", json!(locations));
            set("location_selection", Value::Null);
        }
        (None, None) => {
            if !copying && !extra_sets_locations {
                return Err("choose where to search: pass location_selection (country, states or postal codes) or locations (custom places)".to_owned());
            }
        }
    }
    if let Some(language) = input.language {
        let language = language.trim();
        set(
            "language",
            json!(if language.is_empty() { defaults.language.as_str() } else { language }),
        );
    }
    if let Some(region) = input.region.filter(|region| !region.trim().is_empty()) {
        set("region", json!(region.trim().to_ascii_lowercase()));
    }
    let numbers = [
        ("total_limit", input.total_limit),
        ("per_query_limit", input.per_query_limit),
        ("start_at", input.start_at),
        ("concurrency", input.concurrency),
        ("max_retries", input.max_retries),
    ];
    for (key, value) in numbers {
        if let Some(value) = value {
            set(key, json!(value));
        }
    }
    let flags = [
        ("exact_match", input.exact_match),
        ("collect_all", input.collect_all),
        ("require_proxy", input.require_proxy),
        ("allow_direct", input.allow_direct),
    ];
    for (key, value) in flags {
        if let Some(value) = value {
            set(key, json!(value));
        }
    }
    if let Some(group) = input.proxy_group_id {
        let group = group.trim();
        set(
            "proxy_group_id",
            if group.is_empty() { Value::Null } else { json!(group) },
        );
    }
    if let Some(filters) = input.filters {
        set("filters", json!(clean_list(filters)));
    }
    if let Some(fields) = input.output_fields {
        let fields = clean_list(fields);
        set(
            "output_fields",
            json!(if fields.is_empty() { defaults.output_fields.clone() } else { fields }),
        );
    }
    if let Some(enrichments) = input.enrichments {
        set("enrichments", json!(clean_list(enrichments)));
    }
    if let Some(tags) = input.tags {
        set("tags", json!(clean_list(tags)));
    }
    for (key, value) in [
        ("find_emails", input.find_emails),
        ("place_details", input.place_details),
        ("verify_emails", input.verify_emails),
    ] {
        if let Some(value) = value {
            set(key, json!(value));
        }
    }
    let config: SearchConfig = serde_json::from_value(Value::Object(map))
        .map_err(|error| format!("invalid search settings: {error}"))?;
    config.validate().map_err(|error| {
        let message = error.to_string();
        message
            .strip_prefix("validation failed: ")
            .unwrap_or(&message)
            .to_owned()
    })?;
    Ok(config)
}

// ------------------------------------------------------------- params

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CreateSearchParams {
    /// Copy every setting from this existing search id first (the
    /// dashboard's "Duplicate"), then apply the settings given here.
    #[serde(default)]
    pub copy_from: Option<String>,
    #[serde(flatten)]
    pub search: SearchInput,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListSearchesParams {
    /// "" or omitted for all, "active" (queued, running, paused, stopping),
    /// or one status: queued, running, paused, stopping, completed, failed,
    /// cancelled, interrupted.
    #[serde(default)]
    pub status: Option<String>,
    /// Include each search's full settings. Default false.
    #[serde(default)]
    pub include_config: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchIdParams {
    /// Search id (from create_search or list_searches).
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSearchParams {
    /// Search id (from create_search or list_searches).
    pub id: String,
    /// Recent events to include, newest first. Default 50, max 500.
    #[serde(default)]
    pub events: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSearchParams {
    /// Search id. Active searches must be stopped first.
    pub id: String,
    /// Also delete the leads this search found first. Leads other searches
    /// also found are kept. Default false: keep every lead.
    #[serde(default)]
    pub delete_leads: bool,
}

// ------------------------------------------------------------- output

/// How far a search got, like the dashboard's Google Maps stage: leads
/// against the target, or for a "collect all" search (no target) map
/// lookups done against planned. `None` where that wasn't recorded (a
/// stopped "collect all" search from before lookups were tracked).
fn progress(state: &crate::app::AppState, job: &JobSummary) -> (Option<u32>, Option<i64>) {
    let planned = i64::try_from(job.config.lookup_count()).unwrap_or(i64::MAX);
    let recorded = || {
        state
            .db
            .done_lookups(&job.id)
            .ok()
            .map(|keys| i64::try_from(keys.len()).unwrap_or(i64::MAX))
            .filter(|&done| done > 0 || job.collected == 0)
    };
    let done = match state.job_live_progress(&job.id) {
        Some(live) => Some(live.lookups_done),
        None if job.status == JobStatus::Completed => recorded().or(Some(planned)),
        None => recorded(),
    };
    let percent_done = if job.status == JobStatus::Completed {
        Some(100)
    } else if job.total > 0 {
        Some(percent(job.collected, job.total))
    } else {
        done.map(|done| percent(done, planned))
    };
    (percent_done, done.map(|done| done.min(planned)))
}

fn search_json(state: &crate::app::AppState, job: &JobSummary, include_config: bool) -> Value {
    let live = state.is_live(&job.id);
    let (percent_done, lookups_done) = progress(state, job);
    let mut value = json!({
        "id": job.id,
        "name": job.name,
        "status": job.status.to_string(),
        "active": job.status.is_active(),
        "collected": job.collected,
        "target": if job.total > 0 { json!(job.total) } else { json!("all") },
        "percent": percent_done,
        "failures": job.failures,
        "duplicates": job.duplicates,
        "error": job.error,
        "started_at": job.started_at,
        "finished_at": job.finished_at,
        "lookups": job.config.lookup_count(),
        "lookups_done": lookups_done,
        "queries": job.config.queries,
        "locations": location_summary(&job.config),
        "tags": job.config.tags,
        "controllable": live,
        "can_pause": live && job.status == JobStatus::Running,
        "can_resume": live && job.status == JobStatus::Paused,
        "can_stop": live && matches!(job.status, JobStatus::Queued | JobStatus::Running | JobStatus::Paused),
    });
    if percent_done.is_none() {
        value["progress_note"] = json!(
            "Collects every result and stopped before finishing; it ran before LocalLeads recorded finished lookups, so how far it got is unknown."
        );
    }
    if include_config {
        value["config"] = config_json(&job.config);
    }
    value
}

/// Settings as JSON for the agent. With a location picker, `locations` is
/// worked out from `location_selection` when the search runs; older searches
/// stored the derived list (up to tens of thousands of entries), which is
/// cut to a sample.
fn config_json(config: &SearchConfig) -> Value {
    let mut value = serde_json::to_value(config).unwrap_or_default();
    if let Some(selection) = &config.location_selection {
        value["areas"] = json!(selection.place_count());
        if config.locations.len() > 10 {
            value["locations"] = json!(config.locations.iter().take(10).collect::<Vec<_>>());
            value["locations_note"] = json!("derived from location_selection; first 10 shown");
        }
    }
    value
}

fn preview_json(config: &SearchConfig) -> Value {
    let searches = config.searches();
    let samples: Vec<String> = searches
        .iter()
        .take(10)
        .map(|(query, location)| {
            if location.is_empty() {
                query.clone()
            } else {
                format!("{query} — {location}")
            }
        })
        .collect();
    let locations = config
        .location_selection
        .as_ref()
        .map_or(config.locations.len(), LocationSelection::place_count);
    json!({
        "lookups": searches.len(),
        "queries": config.queries.len(),
        "locations": locations,
        "parallel_lookups": config.concurrency,
        "sample_lookups": samples,
        "more_lookups": searches.len().saturating_sub(samples.len()),
        "limit": if config.collect_all {
            "all available results".to_owned()
        } else {
            let per_lookup = if config.per_query_limit > 0 {
                format!(", {} per lookup", config.per_query_limit)
            } else {
                String::new()
            };
            format!("up to {} leads{per_lookup}", config.total_limit)
        },
        "warning": (searches.len() > LARGE_SEARCH).then(|| format!(
            "{} lookups is a very large search; it may take hours without many proxies", searches.len()
        )),
    })
}

impl LocalLeadsMcp {
    fn load_job(&self, id: &str) -> Result<JobSummary, String> {
        self.state
            .db
            .get_job(id.trim())
            .map_err(fail)?
            .ok_or_else(|| format!("search '{id}' not found; list_searches shows valid ids"))
    }

    fn base_config(&self, copy_from: Option<&str>) -> Result<Option<SearchConfig>, String> {
        match copy_from.map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => Ok(Some(self.load_job(id)?.config)),
            None => Ok(None),
        }
    }

    fn set_paused(&self, id: &str, pause: bool) -> ToolResult {
        let job = self.load_job(id)?;
        if !self.state.is_live(&job.id) {
            return Err(format!(
                "search '{}' is {} and not running in this server process, so it can't be {}",
                job.name,
                job.status,
                if pause { "paused" } else { "resumed" }
            ));
        }
        let paused = job.status == JobStatus::Paused;
        if paused == pause {
            return reply(json!({ "id": job.id, "status": job.status.to_string(), "changed": false }));
        }
        if pause && job.status != JobStatus::Running {
            return Err(format!("only running searches can be paused; this one is {}", job.status));
        }
        let status = self.state.toggle_pause(&job.id).map_err(fail)?;
        reply(json!({ "id": job.id, "status": status.to_string(), "changed": true }))
    }

    fn start(&self, config: SearchConfig) -> ToolResult {
        let preview = preview_json(&config);
        let name = config.job_name.clone();
        let id = self.state.start_job(config).map_err(fail)?;
        let mut note = "Runs in the background. Poll get_search for progress; browse_leads with search_ids=[id] shows its leads as they arrive.".to_owned();
        if self.mode == super::EngineMode::Embedded {
            note.push_str(" This MCP server runs the engine itself (no dashboard is running for this database), so the search stops if the agent closes this MCP connection. Start the dashboard (`localleads`) for searches that outlive the session.");
        }
        reply(json!({ "id": id, "name": name, "status": "queued", "plan": preview, "note": note }))
    }
}

#[tool_router(router = searches_router, vis = "pub(super)")]
impl LocalLeadsMcp {
    #[tool(
        description = "Everything needed to fill in create_search: search modes, countries with their states/provinces, location modes, filter fields and operators, output fields, proxy groups, and the default value of every search setting. Same choices as the dashboard's New search form.",
        annotations(read_only_hint = true)
    )]
    async fn search_options(&self) -> ToolResult {
        let groups: Vec<Value> = self
            .state
            .db
            .list_proxy_groups()
            .map_err(fail)?
            .into_iter()
            .map(|group| json!({ "id": group.id, "name": group.name, "proxies": group.proxy_count }))
            .collect();
        // Only the US states inline: every country's would be thousands of
        // names. find_cities with just a country lists its states.
        let countries: Vec<Value> = country_options()
            .into_iter()
            .map(|country| {
                if crate::models::is_us(country) {
                    json!({ "name": country, "states": country_subdivisions(country) })
                } else {
                    json!({ "name": country })
                }
            })
            .collect();
        reply(json!({
            "modes": {
                "text": "Text query, e.g. \"plumber\"",
                "maps_url": "Google Maps URL per query",
                "place_id": "Google place ID per query",
            },
            "location_modes": {
                "location_selection.mode=all": "Whole country: every built-in state/province",
                "location_selection.mode=states": "Only the listed states/provinces and/or cities (location_selection.cities, from find_cities)",
                "location_selection.mode=postal_codes": "Only the listed ZIP/postal codes",
                "locations": "Custom places: any free-form place names or coordinates",
            },
            "countries": countries,
            "note_countries": "Every listed country splits into states/provinces, towns and (where the country has them) postal codes, like the US. Call find_cities with only country to get its states.",
            "filter_fields": FILTER_FIELDS,
            "filter_operators": FILTER_OPERATORS,
            "filter_examples": ["rating >= 4", "review_count > 20", "website not_empty", "phone is_empty", "category contains dentist", "status = OPERATIONAL", "name != Starbucks"],
            "output_fields": DEFAULT_OUTPUT_FIELDS,
            "proxy_groups": groups,
            "depths": {
                "region": "one lookup per state/province",
                "city": "one lookup per city or town",
                "zip": "one lookup per ZIP/postal code (per town in countries without postal codes; most results, most lookups; the default)",
            },
            "defaults": SearchConfig::default(),
        }))
    }

    #[tool(
        description = "Find Google Maps business categories to use as queries (the dashboard's business type picker). Pass q to search by name (closest and most common first), group to list one group's categories, or nothing to list the groups. Queries don't have to be categories: any search text works, but categories match Google's own labels.",
        annotations(read_only_hint = true)
    )]
    async fn find_categories(&self, Parameters(params): Parameters<FindCategoriesParams>) -> ToolResult {
        let query = params.q.as_deref().map(str::trim).unwrap_or_default();
        let group = params.group.as_deref().map(str::trim).unwrap_or_default();
        let limit = params.limit.unwrap_or(40).clamp(1, 1000);
        let groups = crate::categories::groups();
        if !query.is_empty() {
            return reply(json!({ "categories": crate::categories::search(query, limit) }));
        }
        if !group.is_empty() {
            let found = groups
                .iter()
                .find(|g| g.key.eq_ignore_ascii_case(group) || g.label.eq_ignore_ascii_case(group))
                .ok_or_else(|| format!("unknown group {group:?}; call find_categories with nothing to list them"))?;
            let items: Vec<&str> = found.items.iter().take(limit).copied().collect();
            return reply(json!({
                "group": found.label,
                "returned": items.len(),
                "more": found.items.len().saturating_sub(items.len()),
                "categories": items,
            }));
        }
        let list: Vec<Value> = groups
            .iter()
            .map(|g| json!({ "key": g.key, "label": g.label, "categories": g.items.len() }))
            .collect();
        reply(json!({ "groups": list }))
    }

    #[tool(
        description = "Find cities for location_selection.cities (the dashboard's city picker). country defaults to the United States. Pass q to search by name across the country (biggest first), state to list every city of a state alphabetically, or only country to list its states. Each result's label, e.g. \"Louisville, KY\" or \"Lyon, Auvergne-Rhone-Alpes\", is the value to put in cities; zips is how many ZIP/postal codes it covers.",
        annotations(read_only_hint = true)
    )]
    async fn find_cities(&self, Parameters(params): Parameters<FindCitiesParams>) -> ToolResult {
        let query = params.q.as_deref().map(str::trim).unwrap_or_default().to_owned();
        let state = params.state.as_deref().map(str::trim).unwrap_or_default().to_owned();
        let country = params.country.as_deref().map(str::trim).unwrap_or_default().to_owned();
        let us = country.is_empty() || crate::models::is_us(&country);
        let world = if us {
            None
        } else {
            Some(crate::world::country(&country).ok_or_else(|| {
                format!("unknown country {country:?}; search_options lists them")
            })?)
        };
        if query.is_empty() && state.is_empty() {
            if let Some(world) = world {
                return reply(json!({ "country": world.name, "iso": world.iso, "states": world.states, "postal_codes": world.has_postal() }));
            }
            return Err("pass q (a city name, at least 2 letters), state (e.g. \"Kentucky\" or \"KY\"), or a country to list its states".to_owned());
        }
        let limit = params.limit.unwrap_or(40).clamp(1, 1000);
        let (total, cities) = blocking(move || {
            let mut cities = match world {
                Some(world) if query.is_empty() => world.cities(&state),
                Some(world) => world.city_search(&query, limit),
                None if query.is_empty() => crate::geo::us_cities(&state),
                None => crate::geo::us_city_search(&query, limit),
            };
            let total = cities.len();
            cities.truncate(limit);
            Ok((total, cities))
        })
        .await?;
        reply(json!({
            "returned": cities.len(),
            "more": total.saturating_sub(cities.len()),
            "cities": cities,
        }))
    }

    #[tool(
        description = "Validate search settings and show what create_search would run: number of query x location lookups, sample lookups, limits, and any error or size warning. Nothing is started. Takes the same settings as create_search.",
        annotations(read_only_hint = true)
    )]
    async fn preview_search(&self, Parameters(params): Parameters<CreateSearchParams>) -> ToolResult {
        let base = self.base_config(params.copy_from.as_deref())?;
        match build_config(params.search, base) {
            Ok(config) => {
                let mut preview = preview_json(&config);
                preview["valid"] = json!(true);
                preview["config"] = config_json(&config);
                reply(preview)
            }
            Err(error) => reply(json!({ "valid": false, "error": error })),
        }
    }

    #[tool(
        description = "Start a Google Maps lead search in the background and return its id. Each query runs in each location; leads are saved as they arrive and deduplicated across all searches. Required: job_name, queries, and where to search (location_selection or locations), unless copy_from supplies them. Run preview_search first for big searches. Poll get_search with the returned id.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn create_search(&self, Parameters(params): Parameters<CreateSearchParams>) -> ToolResult {
        let base = self.base_config(params.copy_from.as_deref())?;
        let config = build_config(params.search, base)?;
        self.start(config)
    }

    #[tool(
        description = "Run an existing search again with exactly the same settings, as a new search. Returns the new search id.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn rerun_search(&self, Parameters(params): Parameters<SearchIdParams>) -> ToolResult {
        let job = self.load_job(&params.id)?;
        self.start(job.config)
    }

    #[tool(
        description = "List searches, newest first, with status and progress (collected, target, percent, lookups and lookups_done, failures, duplicates; percent counts lookups when the target is \"all\") and whether this server can pause/resume/stop them. Filter by status.",
        annotations(read_only_hint = true)
    )]
    async fn list_searches(&self, Parameters(params): Parameters<ListSearchesParams>) -> ToolResult {
        let status = params.status.unwrap_or_default().trim().to_ascii_lowercase();
        let jobs = self.state.db.list_jobs().map_err(fail)?;
        let searches: Vec<Value> = jobs
            .iter()
            .filter(|job| match status.as_str() {
                "" | "all" => true,
                "active" => job.status.is_active(),
                other => job.status.to_string() == other,
            })
            .map(|job| search_json(&self.state, job, params.include_config))
            .collect();
        reply(json!({ "count": searches.len(), "searches": searches }))
    }

    #[tool(
        description = "One search's detail: status, progress counters, the live activity line, full settings, and recent events (newest first). Poll this while a search runs.",
        annotations(read_only_hint = true)
    )]
    async fn get_search(&self, Parameters(params): Parameters<GetSearchParams>) -> ToolResult {
        let job = self.load_job(&params.id)?;
        let limit = params.events.unwrap_or(50).clamp(1, 500);
        let events: Vec<Value> = self
            .state
            .db
            .job_event_rows(&job.id, limit)
            .map_err(fail)?
            .into_iter()
            .map(|event| json!({ "at": event.created_at, "level": event.level, "message": event.message }))
            .collect();
        let mut value = search_json(&self.state, &job, true);
        value["activity"] = json!(self.state.job_activity(&job.id));
        value["events"] = json!(events);
        reply(value)
    }

    #[tool(
        description = "Pause a running search. Collected leads are kept; resume_search continues it. Only works for searches started by this server process.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn pause_search(&self, Parameters(params): Parameters<SearchIdParams>) -> ToolResult {
        self.set_paused(&params.id, true)
    }

    #[tool(
        description = "Resume a paused search.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn resume_search(&self, Parameters(params): Parameters<SearchIdParams>) -> ToolResult {
        self.set_paused(&params.id, false)
    }

    #[tool(
        description = "Stop a queued, running or paused search. Leads collected so far are kept. It moves to stopping, then cancelled; continue_search carries it on later from the areas it had not finished.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn stop_search(&self, Parameters(params): Parameters<SearchIdParams>) -> ToolResult {
        let job = self.load_job(&params.id)?;
        if !job.status.is_active() {
            return reply(json!({ "id": job.id, "status": job.status.to_string(), "changed": false }));
        }
        self.state.stop_job(&job.id).map_err(fail)?;
        reply(json!({ "id": job.id, "status": "stopping", "changed": true }))
    }

    #[tool(
        description = "Carry on a stopped, cancelled or failed search from where it stopped: areas it already finished are skipped, the rest are searched with the same settings, in the same search (unlike rerun_search, which starts over as a new one). Not for running or completed searches.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn continue_search(&self, Parameters(params): Parameters<SearchIdParams>) -> ToolResult {
        let job = self.load_job(&params.id)?;
        self.state.continue_job(&job.id).map_err(fail)?;
        self.state.mark_jobs_dirty();
        reply(json!({ "id": job.id, "name": job.name, "status": "running" }))
    }

    #[tool(
        description = "Delete a finished search and its event log. With delete_leads=true, also delete the leads it found first (leads other searches also found are kept). Stop active searches first. Returns an undo_id: undo_delete brings it all back within 60 minutes.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn delete_search(&self, Parameters(params): Parameters<DeleteSearchParams>) -> ToolResult {
        let job = self.load_job(&params.id)?;
        if job.status.is_active() || self.state.is_live(&job.id) {
            return Err("stop the search before deleting it (stop_search)".to_owned());
        }
        let state = self.state.clone();
        let id = job.id.clone();
        let removed = blocking(move || state.db.delete_job(&id, params.delete_leads)).await?;
        let (undo_id, removed) = (removed.undo, removed.count);
        self.state.notice(
            "ok",
            if removed > 0 {
                format!("Deleted search \"{}\" and {removed} leads", job.name)
            } else {
                format!("Deleted search \"{}\"", job.name)
            },
        );
        self.state.mark_jobs_dirty();
        reply(json!({ "deleted": job.id, "name": job.name, "leads_deleted": removed, "undo_id": super::undo_id(1, undo_id) }))
    }
}
