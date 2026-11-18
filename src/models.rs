use crate::error::LocalLeadsError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const DEFAULT_OUTPUT_FIELDS: &[&str] = &[
    "name",
    "category",
    "address",
    "phone",
    "website",
    "latitude",
    "longitude",
    "rating",
    "review_count",
    "status",
    "hours",
    "maps_url",
    "place_id",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum JobStatus {
    Queued,
    Running,
    Paused,
    Stopping,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running | Self::Paused | Self::Stopping
        )
    }
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        };
        f.write_str(label)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SearchMode {
    Text,
    MapsUrl,
    PlaceId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum LocationMode {
    #[default]
    All,
    States,
    PostalCodes,
}

impl fmt::Display for LocationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::All => "all states/provinces",
            Self::States => "specific states/provinces",
            Self::PostalCodes => "specific ZIP/postal codes",
        })
    }
}

/// How finely a state is split. Google Maps returns about 40 results for a
/// statewide query, 120–200 for a city, and the same again for each ZIP code,
/// so deeper splits find many more businesses at the cost of more lookups.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaDepth {
    /// One lookup per state (older searches).
    #[default]
    Region,
    /// One lookup per city or town.
    City,
    /// One lookup per ZIP code.
    Zip,
}

impl AreaDepth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::City => "city",
            Self::Zip => "zip",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "city" => Self::City,
            "zip" => Self::Zip,
            _ => Self::Region,
        }
    }

    /// Map viewport for a lookup at this depth, in meters.
    pub fn span_m(self) -> u32 {
        match self {
            Self::Region => 50_000,
            Self::City => 15_000,
            Self::Zip => 8_000,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LocationSelection {
    pub country: String,
    pub mode: LocationMode,
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub postal_codes: Vec<String>,
    /// Single US cities, as "Louisville, KY", picked without their whole
    /// state. Searched like states (every ZIP code at ZIP depth).
    #[serde(default)]
    pub cities: Vec<String>,
    /// Split states into cities or ZIP codes (US only).
    #[serde(default)]
    pub depth: AreaDepth,
}

/// One Google Maps lookup: a query in one place. `center` aims the map when
/// it's known, which saves a request.
#[derive(Clone, Debug, PartialEq)]
pub struct Lookup {
    pub query: String,
    pub location: String,
    pub center: Option<(f64, f64)>,
    pub span_m: u32,
}

/// A place to search, before it's paired with a query.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub label: String,
    pub center: Option<(f64, f64)>,
    pub span_m: u32,
}

impl Place {
    fn named(label: String) -> Self {
        Self {
            label,
            center: None,
            span_m: AreaDepth::Region.span_m(),
        }
    }
}

impl LocationSelection {
    pub fn validate(&self) -> Result<(), LocalLeadsError> {
        if self.country.trim().is_empty() {
            return Err(LocalLeadsError::Validation(
                "country cannot be empty".to_owned(),
            ));
        }
        match self.mode {
            LocationMode::All => Ok(()),
            LocationMode::States
                if self
                    .states
                    .iter()
                    .chain(&self.cities)
                    .any(|value| !value.trim().is_empty()) =>
            {
                Ok(())
            }
            LocationMode::PostalCodes
                if self
                    .postal_codes
                    .iter()
                    .any(|value| !value.trim().is_empty()) =>
            {
                Ok(())
            }
            LocationMode::States => Err(LocalLeadsError::Validation(
                "select at least one state or province".to_owned(),
            )),
            LocationMode::PostalCodes => Err(LocalLeadsError::Validation(
                "enter at least one ZIP or postal code".to_owned(),
            )),
        }
    }

    #[cfg(test)]
    pub fn locations(&self) -> Vec<String> {
        self.places().into_iter().map(|place| place.label).collect()
    }

    /// Whether states can be split into cities and postal codes.
    pub fn can_split(&self) -> bool {
        is_us(&self.country) || self.world().is_some()
    }

    fn world(&self) -> Option<&'static crate::world::Country> {
        if is_us(&self.country) {
            None
        } else {
            crate::world::country(&self.country)
        }
    }

    /// Whether the finest split is by postal code (the US and about a
    /// hundred other countries) rather than by town.
    pub fn has_postal_codes(&self) -> bool {
        is_us(&self.country) || self.world().is_some_and(crate::world::Country::has_postal)
    }

    fn picked_states(&self) -> Vec<&str> {
        match self.mode {
            LocationMode::All => country_subdivisions(&self.country).to_vec(),
            LocationMode::States => self
                .states
                .iter()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .collect(),
            LocationMode::PostalCodes => Vec::new(),
        }
    }

    /// The state a picked city is in.
    fn city_state(&self, city: &str) -> Option<String> {
        match self.world() {
            Some(country) => country.city_state(city).map(str::to_lowercase),
            None => city
                .rsplit_once(',')
                .and_then(|(_, state)| crate::geo::us_state_code(state))
                .map(str::to_lowercase),
        }
    }

    fn state_key(&self, state: &str) -> Option<String> {
        match self.world() {
            Some(country) => country
                .state(state)
                .and_then(|index| country.states.get(index))
                .map(|name| name.to_lowercase()),
            None => crate::geo::us_state_code(state).map(str::to_lowercase),
        }
    }

    /// Picked cities whose whole state isn't picked too.
    fn picked_cities(&self, states: &[&str]) -> Vec<&str> {
        if self.mode != LocationMode::States {
            return Vec::new();
        }
        let picked: Vec<String> = states.iter().filter_map(|state| self.state_key(state)).collect();
        self.cities
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .filter(|city| !self.city_state(city).is_some_and(|state| picked.contains(&state)))
            .collect()
    }

    fn splits(&self) -> bool {
        self.can_split() && self.depth != AreaDepth::Region
    }

    /// The depth a state is split to: ZIP depth means towns in countries
    /// without postal codes.
    fn by_postal_code(&self) -> bool {
        self.depth == AreaDepth::Zip && self.has_postal_codes()
    }

    fn state_area_count(&self, state: &str) -> usize {
        if !self.splits() {
            return 0;
        }
        match self.world() {
            Some(country) => country.area_count(Some(state), self.by_postal_code()),
            None => crate::geo::us_area_count(state, self.depth == AreaDepth::City),
        }
    }

    fn state_areas(&self, state: &str) -> Vec<crate::geo::Area> {
        if !self.splits() {
            return Vec::new();
        }
        match self.world() {
            Some(country) if self.by_postal_code() => country.postal_areas(Some(state)),
            Some(country) => country.town_areas(Some(state)),
            None if self.depth == AreaDepth::City => crate::geo::us_city_areas(state),
            None => crate::geo::us_zip_areas(state),
        }
    }

    fn city_area_count(&self, city: &str) -> usize {
        if !(self.splits() && self.by_postal_code()) {
            return 1;
        }
        match self.world() {
            Some(country) => country.city_postal_count(city).max(1),
            None => crate::geo::us_city_zip_count(city).max(1),
        }
    }

    fn city_areas(&self, city: &str) -> Vec<crate::geo::Area> {
        let world = self.world();
        if self.splits() && self.by_postal_code() {
            let areas = match world {
                Some(country) => country.city_postal_areas(city),
                None => crate::geo::us_city_zip_areas(city),
            };
            if !areas.is_empty() {
                return areas;
            }
        }
        world.and_then(|country| country.city_area(city)).into_iter().collect()
    }

    /// The place a state is searched as when it isn't split.
    fn state_place(&self, state: &str) -> Place {
        Place::named(format!("{state}, {}", self.country))
    }

    /// `places().len()`, without building the list.
    pub fn place_count(&self) -> usize {
        if self.mode == LocationMode::PostalCodes {
            return self
                .postal_codes
                .iter()
                .filter(|value| !value.trim().is_empty())
                .count();
        }
        let states = self.picked_states();
        let cities = self.picked_cities(&states);
        if states.is_empty() && cities.is_empty() {
            // A country without states: its towns or postal codes.
            return self
                .world()
                .filter(|_| self.splits())
                .map_or(1, |country| country.area_count(None, self.by_postal_code()).max(1));
        }
        let state_places: usize = states
            .iter()
            .map(|state| self.state_area_count(state).max(1))
            .sum();
        let city_places: usize = cities.iter().map(|city| self.city_area_count(city)).sum();
        state_places + city_places
    }

    /// Every place to search, busiest first: at ZIP or city depth, areas in
    /// cities with many ZIP codes come first, so a search with a lead limit
    /// reaches it in the fewest requests.
    pub fn places(&self) -> Vec<Place> {
        if self.mode == LocationMode::PostalCodes {
            let world = self.world();
            let us = is_us(&self.country);
            return self
                .postal_codes
                .iter()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|code| {
                    let known = match world {
                        Some(country) => country.postal_place(code),
                        None if us => crate::geo::us_zip_place(code),
                        None => None,
                    };
                    Place {
                        label: known
                            .as_ref()
                            .map(|(label, _)| label.clone())
                            .unwrap_or_else(|| format!("{code}, {}", self.country)),
                        center: known.map(|(_, center)| center),
                        span_m: AreaDepth::Zip.span_m(),
                    }
                })
                .collect();
        }
        let states = self.picked_states();
        let cities = self.picked_cities(&states);
        let mut weighted: Vec<(u32, Place)> = Vec::new();
        let push_areas = |areas: Vec<crate::geo::Area>, weighted: &mut Vec<(u32, Place)>| {
            weighted.extend(areas.into_iter().map(|area| {
                (
                    area.weight,
                    Place {
                        label: area.label,
                        center: Some((area.latitude, area.longitude)),
                        span_m: self.depth.span_m(),
                    },
                )
            }));
        };
        if states.is_empty() && cities.is_empty() {
            // A country without states: its towns or postal codes.
            let areas = match self.world() {
                Some(country) if self.splits() && self.by_postal_code() => country.postal_areas(None),
                Some(country) if self.splits() => country.town_areas(None),
                _ => Vec::new(),
            };
            if areas.is_empty() {
                return vec![Place::named(self.country.clone())];
            }
            push_areas(areas, &mut weighted);
        }
        for state in &states {
            let areas = self.state_areas(state);
            if areas.is_empty() {
                weighted.push((0, self.state_place(state)));
            } else {
                push_areas(areas, &mut weighted);
            }
        }
        for city in &cities {
            let areas = self.city_areas(city);
            if areas.is_empty() {
                weighted.push((0, Place::named((*city).to_owned())));
            } else {
                push_areas(areas, &mut weighted);
            }
        }
        weighted.sort_by_key(|item| std::cmp::Reverse(item.0));
        let mut seen = std::collections::HashSet::new();
        weighted
            .into_iter()
            .map(|(_, place)| place)
            .filter(|place| seen.insert(place.label.clone()))
            .collect()
    }
}

/// Google's `gl` region code for a country: its ISO code. Without it
/// Google Maps returns far fewer results (5 instead of 150 for a US ZIP).
pub fn country_region(country: &str) -> Option<String> {
    if is_us(country) {
        return Some("us".to_owned());
    }
    crate::world::iso(country).map(str::to_ascii_lowercase)
}

/// Most lookups a search may run at once.
pub const MAX_CONCURRENCY: u32 = 500;

const US: &str = "United States";

/// Whether a country names the United States (whose places are `geo`).
pub fn is_us(country: &str) -> bool {
    matches!(
        country.trim().to_lowercase().as_str(),
        "united states" | "us" | "usa" | "united states of america"
    )
}

/// Every country a search can cover: the United States first, then the
/// rest alphabetically.
pub fn country_options() -> Vec<&'static str> {
    std::iter::once(US)
        .chain(crate::world::countries().iter().map(|(name, _)| *name))
        .collect()
}

/// A country's states, provinces or regions.
pub fn country_subdivisions(country: &str) -> &'static [&'static str] {
    if is_us(country) {
        return US_STATES;
    }
    crate::world::country(country).map_or(&[], |country| country.states.as_slice())
}

const US_STATES: &[&str] = &[
    "Alabama",
    "Alaska",
    "Arizona",
    "Arkansas",
    "California",
    "Colorado",
    "Connecticut",
    "Delaware",
    "District of Columbia",
    "Florida",
    "Georgia",
    "Hawaii",
    "Idaho",
    "Illinois",
    "Indiana",
    "Iowa",
    "Kansas",
    "Kentucky",
    "Louisiana",
    "Maine",
    "Maryland",
    "Massachusetts",
    "Michigan",
    "Minnesota",
    "Mississippi",
    "Missouri",
    "Montana",
    "Nebraska",
    "Nevada",
    "New Hampshire",
    "New Jersey",
    "New Mexico",
    "New York",
    "North Carolina",
    "North Dakota",
    "Ohio",
    "Oklahoma",
    "Oregon",
    "Pennsylvania",
    "Puerto Rico",
    "Rhode Island",
    "South Carolina",
    "South Dakota",
    "Tennessee",
    "Texas",
    "Utah",
    "Vermont",
    "Virginia",
    "Washington",
    "West Virginia",
    "Wisconsin",
    "Wyoming",
];

impl fmt::Display for SearchMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::MapsUrl => "maps_url",
            Self::PlaceId => "place_id",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchConfig {
    pub job_name: String,
    pub queries: Vec<String>,
    pub locations: Vec<String>,
    #[serde(default)]
    pub location_selection: Option<LocationSelection>,
    pub mode: SearchMode,
    pub language: String,
    pub region: String,
    pub exact_match: bool,
    pub total_limit: u32,
    pub per_query_limit: u32,
    pub collect_all: bool,
    pub start_at: u32,
    pub concurrency: u32,
    pub max_retries: u32,
    pub require_proxy: bool,
    pub allow_direct: bool,
    #[serde(default)]
    pub proxy_group_id: Option<String>,
    pub filters: Vec<String>,
    pub output_fields: Vec<String>,
    pub enrichments: Vec<String>,
    pub tags: Vec<String>,
    /// Read each lead's website for emails, socials and site details.
    #[serde(default = "enabled")]
    pub find_emails: bool,
    /// Open each lead's Google place page for reviews per star, owner,
    /// review topics and photos.
    #[serde(default = "enabled")]
    pub place_details: bool,
    /// Ask each found email's mail server whether the address exists
    /// (builds with the `verify` feature only). Off unless chosen.
    #[serde(default)]
    pub verify_emails: bool,
}

fn enabled() -> bool {
    true
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            job_name: "New local search".to_owned(),
            queries: vec!["coffee shops".to_owned()],
            locations: vec!["New York, NY".to_owned()],
            location_selection: None,
            mode: SearchMode::Text,
            language: "en".to_owned(),
            region: "us".to_owned(),
            exact_match: false,
            total_limit: 1_000,
            per_query_limit: 0,
            collect_all: false,
            start_at: 0,
            concurrency: 10,
            max_retries: 2,
            require_proxy: false,
            allow_direct: true,
            proxy_group_id: None,
            filters: Vec::new(),
            output_fields: DEFAULT_OUTPUT_FIELDS
                .iter()
                .map(|v| (*v).to_owned())
                .collect(),
            enrichments: Vec::new(),
            tags: Vec::new(),
            find_emails: true,
            place_details: true,
            verify_emails: false,
        }
    }
}

impl SearchConfig {
    /// Whether this search does any enrichment after collecting leads.
    pub fn enriches(&self) -> bool {
        self.find_emails || self.place_details
    }

    pub fn validate(&self) -> Result<(), LocalLeadsError> {
        if self.job_name.trim().is_empty() {
            return Err(LocalLeadsError::Validation(
                "job name cannot be empty".to_owned(),
            ));
        }
        if self.queries.iter().all(|value| value.trim().is_empty()) {
            return Err(LocalLeadsError::Validation(
                "at least one query is required".to_owned(),
            ));
        }
        if let Some(selection) = &self.location_selection {
            selection.validate()?;
        } else if self.locations.iter().any(|value| value.trim().is_empty()) {
            return Err(LocalLeadsError::Validation(
                "locations cannot be empty".to_owned(),
            ));
        }
        if self.total_limit == 0 && !self.collect_all {
            return Err(LocalLeadsError::Validation(
                "total limit must be greater than zero".to_owned(),
            ));
        }
        if self.concurrency == 0 || self.concurrency > MAX_CONCURRENCY {
            return Err(LocalLeadsError::Validation(format!(
                "parallel lookups must be between 1 and {MAX_CONCURRENCY}"
            )));
        }
        if self.max_retries > 10 {
            return Err(LocalLeadsError::Validation(
                "retries must be at most 10".to_owned(),
            ));
        }
        if self.require_proxy && !self.allow_direct && self.concurrency == 0 {
            return Err(LocalLeadsError::Validation(
                "proxy settings are incompatible".to_owned(),
            ));
        }
        for filter in &self.filters {
            parse_filter(filter)?;
        }
        Ok(())
    }

    pub fn searches(&self) -> Vec<(String, String)> {
        self.lookups()
            .into_iter()
            .map(|lookup| (lookup.query, lookup.location))
            .collect()
    }

    /// `lookups().len()`, without building the list.
    pub fn lookup_count(&self) -> usize {
        let places = match &self.location_selection {
            Some(selection) => selection.place_count(),
            None => self.locations.len().max(1),
        };
        let queries = self
            .queries
            .iter()
            .filter(|query| !query.trim().is_empty())
            .count();
        places * queries
    }

    pub fn places(&self) -> Vec<Place> {
        if let Some(selection) = &self.location_selection {
            selection.places()
        } else if self.locations.is_empty() {
            vec![Place::named(String::new())]
        } else {
            self.locations
                .iter()
                .map(|location| {
                    // A bare US ZIP code gets its state and map position.
                    let zip = location.trim();
                    let known = (zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit()))
                        .then(|| crate::geo::us_zip_place(zip))
                        .flatten();
                    match known {
                        Some((label, center)) => Place {
                            label,
                            center: Some(center),
                            span_m: AreaDepth::Zip.span_m(),
                        },
                        None => Place::named(location.clone()),
                    }
                })
                .collect()
        }
    }

    /// Every query in every place. Places go round-robin across queries so a
    /// stopped search has covered each query about evenly.
    pub fn lookups(&self) -> Vec<Lookup> {
        let places = self.places();
        let queries: Vec<&String> = self
            .queries
            .iter()
            .filter(|query| !query.trim().is_empty())
            .collect();
        let mut lookups = Vec::with_capacity(places.len() * queries.len());
        for place in &places {
            for query in &queries {
                lookups.push(Lookup {
                    query: (*query).clone(),
                    location: place.label.clone(),
                    center: place.center,
                    span_m: place.span_m,
                });
            }
        }
        lookups
    }

    pub fn field_list(&self) -> String {
        if self.output_fields.is_empty() {
            "all".to_owned()
        } else {
            self.output_fields.join(",")
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BusinessRecord {
    pub name: String,
    pub category: Option<String>,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub website: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub rating: Option<f64>,
    pub review_count: Option<i64>,
    pub status: Option<String>,
    pub hours: Option<String>,
    pub maps_url: Option<String>,
    pub place_id: Option<String>,
    pub source: String,
    pub extra: serde_json::Value,
}

impl BusinessRecord {
    pub fn dedupe_key(&self) -> String {
        self.place_id.clone().unwrap_or_else(|| {
            format!(
                "{}|{}",
                self.name.to_ascii_lowercase().trim(),
                self.address
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .trim()
            )
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProxyEntry {
    pub id: String,
    pub url: String,
    pub protocol: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub enabled: bool,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub consecutive_failures: i64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProxyGroup {
    pub id: String,
    pub name: String,
    pub proxy_count: i64,
}

impl ProxyEntry {
    pub fn masked_url(&self) -> String {
        match url::Url::parse(&self.url) {
            Ok(mut parsed) => {
                if parsed.password().is_some() {
                    let _ = parsed.set_password(Some("******"));
                }
                // The URL parser adds a "/" path to `http://host:port`.
                let text = parsed.to_string();
                match text.strip_suffix('/') {
                    Some(bare) if parsed.path() == "/" && !self.url.ends_with('/') => bare.to_owned(),
                    _ => text,
                }
            }
            Err(_) => "[invalid proxy]".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct JobSummary {
    pub id: String,
    pub name: String,
    pub status: JobStatus,
    pub total: i64,
    pub collected: i64,
    pub failures: i64,
    pub duplicates: i64,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub config: SearchConfig,
}

#[derive(Clone, Debug, Serialize)]
pub struct LeadRow {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub website: Option<String>,
    pub rating: Option<f64>,
    pub review_count: Option<i64>,
    pub status: Option<String>,
    pub maps_url: Option<String>,
    pub place_id: Option<String>,
    pub job_id: String,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub created_at: String,
    /// The best email found on the website.
    pub email: Option<String>,
    /// RECEIVING, INVALID or UNKNOWN.
    pub email_status: Option<String>,
    /// safe, risky, unknown or invalid.
    pub email_reach: Option<String>,
    /// Usable addresses found.
    pub email_count: i64,
    /// NULL (not enriched), queued, done or failed.
    pub enrich_status: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ParsedFilter {
    pub field: String,
    pub operator: String,
    pub value: String,
}

pub fn parse_filter(raw: &str) -> Result<ParsedFilter, LocalLeadsError> {
    let raw = raw.trim();
    let operators = [
        "not_empty",
        "is_empty",
        "contains",
        "!=",
        ">=",
        "<=",
        "=",
        ">",
        "<",
        "in",
    ];
    let Some(operator) = operators.iter().find(|operator| raw.contains(**operator)) else {
        return Err(LocalLeadsError::Validation(format!(
            "filter '{raw}' needs an operator (=, !=, >, <, contains, in, is_empty, not_empty)"
        )));
    };
    let Some((field, value)) = raw.split_once(operator) else {
        return Err(LocalLeadsError::Validation(format!(
            "invalid filter '{raw}'"
        )));
    };
    let field = field.trim().to_owned();
    if ![
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
    ]
    .contains(&field.as_str())
    {
        return Err(LocalLeadsError::Validation(format!(
            "unsupported filter field '{field}'"
        )));
    }
    Ok(ParsedFilter {
        field,
        operator: (*operator).to_owned(),
        value: value.trim().to_owned(),
    })
}

pub fn now() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_selection(country: &str, depth: AreaDepth) -> LocationSelection {
        LocationSelection {
            country: country.to_owned(),
            mode: LocationMode::All,
            depth,
            ..LocationSelection::default()
        }
    }

    #[test]
    fn other_countries_split_like_the_us() {
        // France: postal codes, then towns, then regions.
        let zip = world_selection("France", AreaDepth::Zip);
        let city = world_selection("France", AreaDepth::City);
        let region = world_selection("France", AreaDepth::Region);
        assert!(zip.can_split() && zip.has_postal_codes());
        let places = zip.places();
        assert_eq!(places.len(), zip.place_count());
        assert!(places.len() > 3_000, "{}", places.len());
        assert!(places.iter().all(|place| place.center.is_some() && place.label.ends_with(", France")));
        assert!(city.place_count() > 1_000);
        assert_eq!(region.place_count(), country_subdivisions("France").len());
        assert_eq!(country_region("France").as_deref(), Some("fr"));

        // Nigeria has no postal codes: the finest split is by town.
        let nigeria = world_selection("Nigeria", AreaDepth::Zip);
        assert!(nigeria.can_split() && !nigeria.has_postal_codes());
        assert_eq!(nigeria.place_count(), world_selection("Nigeria", AreaDepth::City).place_count());

        // A picked city in a state that isn't picked is searched by its postal codes.
        let lyon = LocationSelection {
            country: "France".to_owned(),
            mode: LocationMode::States,
            states: vec!["Brittany".to_owned()],
            cities: vec!["Lyon, Auvergne-Rhone-Alpes".to_owned()],
            depth: AreaDepth::Zip,
            ..LocationSelection::default()
        };
        let places = lyon.places();
        assert_eq!(places.len(), lyon.place_count());
        assert!(places.iter().any(|place| place.label.starts_with("690")), "{:?}", &places[..3]);
        assert!(places.iter().any(|place| place.label.starts_with("29")));

        // Typed postal codes get their centre.
        let typed = LocationSelection {
            country: "Germany".to_owned(),
            mode: LocationMode::PostalCodes,
            postal_codes: vec!["10115".to_owned()],
            ..LocationSelection::default()
        };
        assert!(typed.places()[0].center.is_some());
    }


    #[test]
    fn search_config_expands_queries_and_locations() {
        let config = SearchConfig {
            queries: vec!["plumber".to_owned(), "roofer".to_owned()],
            locations: vec!["Austin".to_owned(), "Dallas".to_owned()],
            ..SearchConfig::default()
        };
        assert_eq!(config.searches().len(), 4);
    }

    #[test]
    fn filter_parser_rejects_unknown_fields() {
        let error = parse_filter("secret=1").expect_err("unknown fields must fail");
        assert!(error.to_string().contains("unsupported filter field"));
    }

    #[test]
    fn states_split_into_zip_lookups_with_centers() {
        let selection = |depth| LocationSelection {
            country: "United States".to_owned(),
            mode: LocationMode::States,
            states: vec!["Kentucky".to_owned(), "Delaware".to_owned()],
            cities: Vec::new(),
            postal_codes: Vec::new(),
            depth,
        };
        let config = SearchConfig {
            queries: vec!["plumber".to_owned(), "roofer".to_owned()],
            location_selection: Some(selection(AreaDepth::Zip)),
            ..SearchConfig::default()
        };
        let lookups = config.lookups();
        assert_eq!(lookups.len(), config.lookup_count());
        assert!(lookups.len() > 1_800, "{}", lookups.len());
        assert!(lookups.iter().all(|lookup| lookup.center.is_some()));
        let zip = lookups
            .iter()
            .find(|lookup| lookup.location == "40202, KY")
            .expect("Louisville ZIP");
        assert_eq!(zip.span_m, AreaDepth::Zip.span_m());

        let region = SearchConfig {
            location_selection: Some(selection(AreaDepth::Region)),
            ..config.clone()
        };
        assert_eq!(region.lookup_count(), 4);
        assert_eq!(region.lookups()[0].location, "Kentucky, United States");
        assert!(region.lookups()[0].center.is_none());

        let cities = SearchConfig {
            location_selection: Some(selection(AreaDepth::City)),
            ..config
        };
        assert_eq!(cities.lookups().len(), cities.lookup_count());
        assert!(cities.lookup_count() < lookups.len());
    }

    #[test]
    fn old_saved_searches_keep_one_lookup_per_state() {
        let json = r#"{"country":"United States","mode":"states","states":["Ohio"]}"#;
        let selection: LocationSelection = serde_json::from_str(json).expect("old selection");
        assert_eq!(selection.depth, AreaDepth::Region);
        assert_eq!(
            selection.locations(),
            vec!["Ohio, United States".to_owned()]
        );
    }
}
