use crate::db::{Database, LeadInsert};
use crate::events::AppEvent;
use crate::models::{BusinessRecord, JobStatus, Lookup, ProxyEntry, SearchConfig, SearchMode};
use crate::proxy::ProxyPool;
use anyhow::{Context, Result};
use futures::FutureExt;
use futures::stream::{self, StreamExt, TryStreamExt};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI64, Ordering},
};
use std::time::Duration;
use url::Url;

const MAP_SEARCH_ENDPOINT: &str = "https://www.google.com/search";
/// Google Maps serves 20 results per page whatever a request asks for.
const MAP_PAGE_SIZE: u32 = 20;
/// Pages to read per lookup at most. Dense areas run out around 10 pages
/// (about 200 results).
const MAX_PAGES: u32 = 15;

#[derive(Clone)]
pub struct JobControl {
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl JobControl {
    pub fn new() -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn toggle_pause(&self) -> bool {
        let next = !self.paused.load(Ordering::Relaxed);
        self.paused.store(next, Ordering::Relaxed);
        next
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub async fn wait_if_paused(&self) {
        while self.paused.load(Ordering::Relaxed) && !self.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MapCenter {
    latitude: f64,
    longitude: f64,
}

#[derive(Clone, Copy)]
pub struct GoogleMapsClient;

impl GoogleMapsClient {
    #[allow(clippy::too_many_arguments)]
    async fn search_page(
        &self,
        http: &Client,
        query: &str,
        location: &str,
        center: Option<MapCenter>,
        span_m: u32,
        offset: u32,
        config: &SearchConfig,
    ) -> Result<MapPage> {
        let search_text = search_text(query, location, config);
        let center = center
            .or_else(|| coordinates_from_text(query))
            .or_else(|| coordinates_from_text(location));
        let center = match center {
            Some(value) => value,
            None => self.discover_center(http, &search_text, config).await?,
        };
        let url = map_url(&search_text, center, span_m, offset, config)?;
        let body = request_text(http, url).await?;
        parse_map_response(&body, center, query, location)
    }

    async fn discover_center(
        &self,
        http: &Client,
        search_text: &str,
        config: &SearchConfig,
    ) -> Result<MapCenter> {
        let url = bootstrap_url(search_text, config)?;
        let body = request_text(http, url).await?;
        let value = parse_json_response(&body)?;
        find_map_center(&value).ok_or_else(|| {
            anyhow::anyhow!(
                "Google Maps did not return a map center for '{}'; the response may require consent or access is temporarily restricted",
                truncate(search_text, 120)
            )
        })
    }
}

#[derive(Debug)]
struct MapPage {
    center: MapCenter,
    /// Result rows Google sent, including any that couldn't be parsed; a
    /// short page is the last one.
    rows: usize,
    places: Vec<BusinessRecord>,
}

pub(crate) async fn request_text(http: &Client, url: Url) -> Result<String> {
    let response = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json,text/plain,*/*")
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/131 Safari/537.36",
        )
        .send()
        .await
        .context("request Google Maps web search")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read Google Maps web response")?;
    if !status.is_success() {
        // Google's "Sorry…" page: this IP sent too many requests.
        if body.contains("<title>Sorry...</title>") || body.contains("/sorry/") {
            return Err(anyhow::anyhow!(
                "Google Maps HTTP {status}: Google is limiting requests from this IP; use proxies or try again later"
            ));
        }
        let text = if body.trim_start().starts_with('<') {
            "(HTML error page)".to_owned()
        } else {
            truncate(&body, 300)
        };
        return Err(anyhow::anyhow!("Google Maps web HTTP {status}: {text}"));
    }
    Ok(body)
}

fn bootstrap_url(search_text: &str, config: &SearchConfig) -> Result<Url> {
    let mut url = Url::parse(MAP_SEARCH_ENDPOINT)?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("tbm", "map");
        query.append_pair("authuser", "0");
        query.append_pair("hl", language(config));
        if let Some(region) = region(config) {
            query.append_pair("gl", region);
        }
        query.append_pair("q", search_text);
    }
    Ok(url)
}

fn map_url(
    search_text: &str,
    center: MapCenter,
    span_m: u32,
    offset: u32,
    config: &SearchConfig,
) -> Result<Url> {
    let payload = format!(
        "!1m3!1d{span_m}!2d{}!3d{}!2i1024!3i768!4f13.1!7i{}!8i{}!10b1",
        center.longitude, center.latitude, MAP_PAGE_SIZE, offset
    );
    let mut url = Url::parse(MAP_SEARCH_ENDPOINT)?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("tbm", "map");
        query.append_pair("authuser", "0");
        query.append_pair("hl", language(config));
        if let Some(region) = region(config) {
            query.append_pair("gl", region);
        }
        query.append_pair("q", search_text);
        query.append_pair("pb", &payload);
    }
    Ok(url)
}

pub(crate) fn language(config: &SearchConfig) -> &str {
    if config.language.trim().is_empty() {
        "en"
    } else {
        config.language.trim()
    }
}

/// Google's `gl` region. Searches saved without one use their country's,
/// since Google returns far fewer results without it.
pub(crate) fn region(config: &SearchConfig) -> Option<&str> {
    let value = config.region.trim();
    if !value.is_empty() && value.len() <= 3 {
        return Some(value);
    }
    config
        .location_selection
        .as_ref()
        .and_then(|selection| crate::models::country_region(&selection.country))
}

fn search_text(query: &str, location: &str, config: &SearchConfig) -> String {
    let query = match config.mode {
        SearchMode::MapsUrl => extract_place_name(query).unwrap_or_else(|| query.trim().to_owned()),
        SearchMode::PlaceId => extract_place_id(query).unwrap_or_else(|| query.trim().to_owned()),
        SearchMode::Text => query.trim().to_owned(),
    };
    let query = if config.exact_match {
        format!("\"{query}\"")
    } else {
        query
    };
    if matches!(config.mode, SearchMode::PlaceId) || location.trim().is_empty() {
        query
    } else {
        format!("{query} {}", location.trim())
    }
}

pub(crate) fn parse_json_response(body: &str) -> Result<Value> {
    let body = body.trim_start_matches("\u{feff}").trim_start();
    let body = body.strip_prefix(")]}'").unwrap_or(body).trim_start();
    serde_json::from_str(body).context("decode Google Maps web response")
}

fn parse_map_response(
    body: &str,
    center: MapCenter,
    query: &str,
    location: &str,
) -> Result<MapPage> {
    let value = parse_json_response(body)?;
    let listings = value
        .get(0)
        .and_then(|value| value.get(1))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Google Maps response did not contain result rows"))?;
    let places = listings
        .iter()
        .filter_map(|listing| MapPlace::from_listing(listing, query, location))
        .map(|place| place.record)
        .collect();
    Ok(MapPage {
        center,
        rows: listings.len(),
        places,
    })
}

fn find_map_center(value: &Value) -> Option<MapCenter> {
    let values = value.as_array()?;
    if values.len() >= 3 {
        let longitude = values.get(1).and_then(Value::as_f64);
        let latitude = values.get(2).and_then(Value::as_f64);
        if let (Some(longitude), Some(latitude)) = (longitude, latitude)
            && (-180.0..=180.0).contains(&longitude)
            && (-90.0..=90.0).contains(&latitude)
        {
            return Some(MapCenter {
                latitude,
                longitude,
            });
        }
    }
    values.iter().find_map(find_map_center)
}

fn coordinates_from_text(value: &str) -> Option<MapCenter> {
    if let Some(after_at) = value.split('@').nth(1) {
        let mut parts = after_at.split(',');
        let latitude = parts.next()?.parse().ok()?;
        let longitude = parts.next()?.parse().ok()?;
        return Some(MapCenter {
            latitude,
            longitude,
        });
    }
    let latitude = value.split("!3d").nth(1)?.split('!').next()?.parse().ok()?;
    let longitude = value.split("!4d").nth(1)?.split('!').next()?.parse().ok()?;
    Some(MapCenter {
        latitude,
        longitude,
    })
}

struct MapPlace {
    record: BusinessRecord,
}

impl MapPlace {
    fn from_listing(listing: &Value, query: &str, location: &str) -> Option<Self> {
        let business = listing.get(14)?;
        let name = value_string(value_at(business, &[11]))?;
        let categories = value_strings(value_at(business, &[13]));
        let address = value_string(value_at(business, &[18]));
        let latitude = value_f64(value_at(business, &[9, 2]));
        let longitude = value_f64(value_at(business, &[9, 3]));
        let place_id = value_string(value_at(business, &[78]));
        // Outscraper's ids: google_id is the "0x…:0x…" feature id, cid its
        // second half in decimal (the ludocid).
        let google_id = value_string(value_at(business, &[10]));
        let cid = google_id.as_deref().and_then(cid_from_feature_id);
        let kgmid = value_string(value_at(business, &[89]))
            .or_else(|| value_string(value_at(business, &[227, 0, 3])));
        let website = website_from_value(value_at(business, &[7]));
        let reviews_link = value_string(value_at(business, &[4, 3, 0])).or_else(|| {
            place_id
                .as_deref()
                .map(|id| format!("https://search.google.com/local/reviews?placeid={id}&q=*&hl=en"))
        });
        let hours_value = value_at(business, &[203]);
        let hours = hours_value.and_then(|value| serde_json::to_string(value).ok());
        let hours_csv_compatible = hours_value.map(hours_csv_compatible);
        let categories_value = value_at(business, &[13]).cloned().unwrap_or(Value::Null);
        // The small profile picture; Outscraper calls it the logo.
        let logo = value_string(value_at(business, &[157]));
        let time_zone = value_string(value_at(business, &[30]));
        let status = value_string(value_at(business, &[34, 4, 4]));
        let description = value_string(value_at(business, &[32, 0, 1]))
            .or_else(|| value_string(value_at(business, &[32, 1, 1])));
        let photos_count = value_i64(value_at(business, &[146, 0]));
        // [null, street, street, city, postal_code, state, country_code]
        let parts = value_at(business, &[183, 1]);
        let part = |index: usize| parts.and_then(|parts| value_string(parts.get(index)));
        let street = part(1).or_else(|| value_string(value_at(business, &[82, 1])));
        let city = part(3).or_else(|| value_string(value_at(business, &[82, 3])));
        let postal_code = part(4);
        let state = part(5);
        let country_code = value_string(value_at(business, &[243])).or_else(|| part(6));
        let state_code = state
            .as_deref()
            .filter(|_| country_code.as_deref() == Some("US"))
            .and_then(crate::geo::us_state_code);
        let plus_code = value_string(value_at(business, &[183, 2, 2, 0]));
        let owner_id = value_string(value_at(business, &[181, 6]))
            .or_else(|| value_string(value_at(business, &[227, 0, 6])));
        let h3 = match (latitude, longitude) {
            (Some(latitude), Some(longitude)) => h3_cell(latitude, longitude),
            _ => None,
        };
        let category = categories.first().cloned();
        let extra = serde_json::json!({
            "query": query,
            "location": location,
            "name_for_emails": name_for_emails(&name),
            "subtypes": categories.clone(),
            "street": street,
            "city": city,
            "postal_code": postal_code,
            "state": state,
            "state_code": state_code,
            "country": country_code.as_deref().map(country_name),
            "country_code": country_code,
            "plus_code": plus_code,
            "h3": h3,
            "time_zone": time_zone,
            "reviews_link": reviews_link,
            "working_hours": hours.clone(),
            "working_hours_csv_compatible": hours_csv_compatible,
            "business_status": status.clone(),
            "description": description,
            "about": about_attributes(value_at(business, &[100, 1])),
            "photos_count": photos_count,
            "logo": logo,
            "owner_id": owner_id,
            "owner_link": owner_id.as_deref().map(|id| format!("https://www.google.com/maps/contrib/{id}")),
            "google_id": google_id,
            "cid": cid,
            "kgmid": kgmid,
            "categories": categories_value,
        });
        let maps_url = place_id.as_deref().map(|id| {
            format!("https://www.google.com/maps/search/?api=1&query=Google&query_place_id={id}")
        });
        Some(Self {
            record: BusinessRecord {
                name,
                category,
                address,
                phone: value_string(value_at(business, &[178, 0, 0])),
                website,
                latitude,
                longitude,
                rating: value_f64(value_at(business, &[4, 7])),
                review_count: value_i64(value_at(business, &[4, 8])),
                status,
                hours,
                maps_url,
                place_id,
                source: "google_maps_web".to_owned(),
                extra,
            },
        })
    }
}

/// The decimal ludocid from a "0x…:0x…" feature id.
fn cid_from_feature_id(feature_id: &str) -> Option<String> {
    let hex = feature_id.split(':').nth(1)?.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).ok().map(|value| value.to_string())
}

/// H3 cell at resolution 9, the one Outscraper reports.
fn h3_cell(latitude: f64, longitude: f64) -> Option<String> {
    let point = h3o::LatLng::new(latitude, longitude).ok()?;
    Some(point.to_cell(h3o::Resolution::Nine).to_string())
}

/// Google's "About" tab as {"Accessibility": {"Wheelchair accessible
/// parking lot": true}}.
fn about_attributes(groups: Option<&Value>) -> Value {
    let mut about = serde_json::Map::new();
    for group in groups.and_then(Value::as_array).into_iter().flatten() {
        let Some(title) = value_string(group.get(1)) else {
            continue;
        };
        let mut items = serde_json::Map::new();
        for item in group.get(2).and_then(Value::as_array).into_iter().flatten() {
            if let Some(label) = value_string(item.get(1)) {
                let on = value_i64(value_at(item, &[2, 0])).is_none_or(|flag| flag == 1);
                items.insert(label, Value::Bool(on));
            }
        }
        if !items.is_empty() {
            about.insert(title, Value::Object(items));
        }
    }
    if about.is_empty() {
        Value::Null
    } else {
        Value::Object(about)
    }
}

/// The business name as you'd write it in an email: no legal suffix, no
/// tagline after a separator, and not shouted in capitals.
pub fn name_for_emails(name: &str) -> String {
    let mut name = name
        .split(['|', '–', '—'])
        .next()
        .unwrap_or(name)
        .split(" - ")
        .next()
        .unwrap_or(name)
        .trim()
        .to_owned();
    const SUFFIXES: &[&str] = &[
        "llc", "l.l.c.", "inc", "inc.", "incorporated", "ltd", "ltd.", "limited", "corp", "corp.",
        "corporation", "co", "co.", "pllc", "pc", "p.c.", "lp", "llp", "plc", "gmbh", "company",
    ];
    loop {
        let trimmed = name.trim_end_matches([',', ' ', '.']);
        let Some((head, last)) = trimmed.rsplit_once([' ', ',']) else {
            break;
        };
        let last = last.trim().to_ascii_lowercase();
        if !SUFFIXES.contains(&last.as_str())
            && !SUFFIXES.contains(&format!("{last}.").as_str())
        {
            break;
        }
        let head = head.trim_end_matches([',', ' ']);
        // "Smith & Co." is a name, not a legal form.
        if head.is_empty() || head.ends_with('&') || head.to_ascii_lowercase().ends_with(" and") {
            break;
        }
        name = head.to_owned();
    }
    let letters: Vec<char> = name.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() > 3 && letters.iter().all(|c| c.is_uppercase()) {
        name = name
            .split(' ')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    name
}

/// Outscraper's country names for the codes Google returns.
fn country_name(code: &str) -> String {
    match code {
        "US" => "United States of America",
        "CA" => "Canada",
        "GB" => "United Kingdom",
        "AU" => "Australia",
        "NZ" => "New Zealand",
        "IE" => "Ireland",
        "DE" => "Germany",
        "FR" => "France",
        "ES" => "Spain",
        "IT" => "Italy",
        "NL" => "Netherlands",
        "BE" => "Belgium",
        "AT" => "Austria",
        "CH" => "Switzerland",
        "MX" => "Mexico",
        "BR" => "Brazil",
        "IN" => "India",
        "ZA" => "South Africa",
        "AE" => "United Arab Emirates",
        other => other,
    }
    .to_owned()
}

pub(crate) fn value_at<'a>(value: &'a Value, path: &[usize]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, index| current.get(*index))
}

pub(crate) fn value_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn value_f64(value: Option<&Value>) -> Option<f64> {
    value?.as_f64()
}

pub(crate) fn value_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|value| value as i64))
}

fn value_strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect()
}

fn website_from_value(value: Option<&Value>) -> Option<String> {
    let values = value?.as_array()?;
    values.iter().find_map(|value| {
        let value = value.as_str()?;
        if value.starts_with("/url?q=") {
            let encoded = value.strip_prefix("/url?q=")?.split('&').next()?;
            let parsed = Url::parse(&format!("https://www.google.com/url?q={encoded}")).ok()?;
            return parsed
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned());
        }
        (value.starts_with("http://") || value.starts_with("https://")).then_some(value.to_owned())
    })
}

fn hours_csv_compatible(value: &Value) -> String {
    let mut entries = Vec::new();
    collect_hours(value, &mut entries);
    entries.join("|")
}

fn collect_hours(value: &Value, entries: &mut Vec<String>) {
    let Some(values) = value.as_array() else {
        return;
    };
    if let Some(day) = values.first().and_then(Value::as_str)
        && let Some(interval) = values.get(3).and_then(first_time_text)
        && !entries
            .iter()
            .any(|entry| entry.starts_with(&format!("{day},")))
    {
        entries.push(format!("{day},{interval}"));
    }
    for value in values {
        collect_hours(value, entries);
    }
}

fn first_time_text(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str()
        && (value.contains('-') || value.contains('–') || value.contains('—'))
    {
        return Some(value.replace(['–', '—'], "-"));
    }
    value.as_array()?.iter().find_map(first_time_text)
}

/// One HTTP client per proxy (and one direct), shared by every lookup, so
/// connections and TLS sessions are reused instead of rebuilt per request.
/// The TLS setup reqwest uses by default (aws-lc-rs, the platform's
/// certificate store, HTTP/1.1), built explicitly. The email checker
/// installs `ring` as the process-wide provider, which reqwest would
/// otherwise pick up, and Google answers that handshake with 403.
fn google_tls() -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .context("load the system's certificates")?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("set TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tls)
}

#[derive(Clone, Default)]
pub(crate) struct HttpClients(Arc<Mutex<HashMap<String, Client>>>);

impl HttpClients {
    pub(crate) fn get(&self, proxy: Option<&ProxyEntry>) -> Result<Client> {
        let key = proxy.map(|proxy| proxy.id.clone()).unwrap_or_default();
        let mut clients = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("HTTP client cache poisoned"))?;
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(google_tls()?)
            .timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30));
        if let Some(proxy) = proxy {
            builder =
                builder.proxy(reqwest::Proxy::all(&proxy.url).context("configure scraping proxy")?);
        }
        let client = builder.build().context("build Google Maps web client")?;
        clients.insert(key, client.clone());
        Ok(client)
    }
}

/// Leads still allowed under the search's total limit. Lookups reserve room
/// before saving a page, so parallel lookups never overshoot the limit.
struct Budget {
    limit: i64,
    reserved: AtomicI64,
}

impl Budget {
    fn reserve(&self, wanted: usize) -> usize {
        let wanted = i64::try_from(wanted).unwrap_or(i64::MAX);
        let mut current = self.reserved.load(Ordering::Relaxed);
        loop {
            let take = wanted.min(self.limit.saturating_sub(current)).max(0);
            if take == 0 {
                return 0;
            }
            match self.reserved.compare_exchange_weak(
                current,
                current + take,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return usize::try_from(take).unwrap_or(0),
                Err(actual) => current = actual,
            }
        }
    }

    /// Hands back room that a page didn't use (repeats within the search).
    fn release(&self, unused: usize) {
        self.reserved
            .fetch_sub(i64::try_from(unused).unwrap_or(0), Ordering::Relaxed);
    }
}

/// Counters shared by every lookup of a running search.
#[derive(Default)]
struct Tally {
    collected: AtomicI64,
    duplicates: AtomicI64,
    failures: AtomicI64,
    lookups_done: AtomicI64,
    enriched: AtomicI64,
    with_email: AtomicI64,
    /// Read leads waiting for their emails to be checked.
    checking: AtomicI64,
}

/// Everything a lookup needs, shared across the search.
struct Run<'a> {
    database: &'a Database,
    job_id: &'a str,
    config: &'a SearchConfig,
    pool: &'a ProxyPool,
    control: &'a JobControl,
    events: &'a tokio::sync::mpsc::UnboundedSender<AppEvent>,
    clients: HttpClients,
    budget: Budget,
    tally: Tally,
    total_lookups: usize,
    enriching: bool,
}

impl Run<'_> {
    fn limit_reached(&self) -> bool {
        self.tally.collected.load(Ordering::Relaxed) >= self.budget.limit
    }

    fn save_progress(&self) -> Result<()> {
        self.database.update_job_progress(
            self.job_id,
            self.tally.collected.load(Ordering::Relaxed),
            self.tally.failures.load(Ordering::Relaxed),
            self.tally.duplicates.load(Ordering::Relaxed),
        )
    }

    fn activity(&self) -> String {
        let mut text = format!(
            "{} of {} areas searched · {} leads",
            fmt_number(self.tally.lookups_done.load(Ordering::Relaxed)),
            fmt_number(i64::try_from(self.total_lookups).unwrap_or(i64::MAX)),
            fmt_number(self.tally.collected.load(Ordering::Relaxed)),
        );
        if self.enriching {
            text.push_str(&format!(
                " · {} enriched · {} with email",
                fmt_number(self.tally.enriched.load(Ordering::Relaxed)),
                fmt_number(self.tally.with_email.load(Ordering::Relaxed)),
            ));
            let checking = self.tally.checking.load(Ordering::Relaxed);
            if checking > 0 {
                text.push_str(&format!(" · checking emails of {}", fmt_number(checking)));
            }
        }
        text
    }
}

fn fmt_number(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 { format!("-{out}") } else { out }
}

pub async fn run_job(
    database: Database,
    job_id: String,
    config: SearchConfig,
    pool: ProxyPool,
    control: JobControl,
    events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) {
    let result = run_job_inner(&database, &job_id, &config, &pool, &control, &events).await;
    match result {
        Ok(()) if control.is_cancelled() => {
            let _ =
                database.update_job_status(&job_id, JobStatus::Cancelled, Some("Stopped by user"));
            let _ = events.send(AppEvent::JobFinished {
                job_id,
                status: JobStatus::Cancelled,
                message: "Search stopped; collected leads were kept".to_owned(),
            });
        }
        Ok(()) => {
            let _ = database.update_job_status(&job_id, JobStatus::Completed, None);
            let _ = events.send(AppEvent::JobFinished {
                job_id,
                status: JobStatus::Completed,
                message: "Search completed".to_owned(),
            });
        }
        Err(error) => {
            let message = error.to_string();
            let _ = database.append_event(&job_id, "error", &message);
            let _ = database.update_job_status(&job_id, JobStatus::Failed, Some(&message));
            let _ = events.send(AppEvent::JobFinished {
                job_id,
                status: JobStatus::Failed,
                message,
            });
        }
    }
}

async fn run_job_inner(
    database: &Database,
    job_id: &str,
    config: &SearchConfig,
    pool: &ProxyPool,
    control: &JobControl,
    events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    config.validate()?;
    database.update_job_status(job_id, JobStatus::Running, None)?;
    let lookups = config.lookups();
    let concurrency = usize::try_from(config.concurrency).unwrap_or(1).clamp(
        1,
        usize::try_from(crate::models::MAX_CONCURRENCY).unwrap_or(500),
    );
    database.append_event(
        job_id,
        "info",
        &format!(
            "Search started: {} areas, {} at a time",
            fmt_number(i64::try_from(lookups.len()).unwrap_or(i64::MAX)),
            concurrency
        ),
    )?;
    let run = Run {
        database,
        job_id,
        config,
        pool,
        control,
        events,
        clients: HttpClients::default(),
        budget: Budget {
            limit: if config.collect_all {
                i64::MAX
            } else {
                i64::from(config.total_limit)
            },
            reserved: AtomicI64::new(0),
        },
        tally: Tally::default(),
        total_lookups: lookups.len(),
        enriching: config.enriches(),
    };
    let enricher = if config.enriches() {
        match crate::enrich::run::Enricher::new(config, pool.clone()).await {
            Ok(enricher) => {
                database.append_event(job_id, "info", &enricher.describe())?;
                Some(enricher)
            }
            Err(error) => {
                database.append_event(job_id, "error", &format!("Enrichment is off: {error}"))?;
                None
            }
        }
    } else {
        None
    };
    let scraping_done = AtomicBool::new(false);

    // Progress is saved and pushed to the UI once a second rather than per
    // lead, however many lookups run at once.
    let report = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = String::new();
        loop {
            tick.tick().await;
            let activity = run.activity();
            if activity != last {
                let _ = run.save_progress();
                let _ = run.events.send(AppEvent::JobProgress {
                    job_id: run.job_id.to_owned(),
                    message: activity.clone(),
                });
                last = activity;
            }
        }
    };
    let scrape = async {
        let result = stream::iter(lookups)
            .map(|lookup| collect_lookup(&run, lookup))
            .buffer_unordered(concurrency)
            .try_for_each(|()| async { Ok(()) })
            .await;
        scraping_done.store(true, Ordering::Relaxed);
        result
    };
    let enrich = async {
        match &enricher {
            Some(enricher) => enrich_leads(&run, enricher, &scraping_done, concurrency).boxed().await,
            None => Ok(()),
        }
    };
    tokio::pin!(report);
    let result = tokio::select! {
        result = async { tokio::try_join!(scrape, enrich) } => result.map(|_| ()),
        () = &mut report => Ok(()),
    };
    if enricher.is_some() {
        let _ = database.release_job_enrichment(job_id);
    }
    result?;
    run.save_progress()?;
    let _ = events.send(AppEvent::JobProgress {
        job_id: job_id.to_owned(),
        message: run.activity(),
    });
    Ok(())
}

/// Reads every page of one lookup and saves the matching leads.
/// Enriches the search's leads as they come in, until the search is done
/// and none are left.
/// Leads read at once: each fetches a few pages of its website and maybe
/// Google's place page (those are held to the search's concurrency).
const READS_AT_ONCE: usize = 64;
/// Leads whose emails are checked at once; the enricher bounds the
/// mail-server checks themselves.
const CHECK_LEADS_AT_ONCE: usize = 64;

async fn enrich_leads(
    run: &Run<'_>,
    enricher: &crate::enrich::run::Enricher,
    scraping_done: &AtomicBool,
    concurrency: usize,
) -> Result<()> {
    let targets = stream::unfold(Vec::new(), |mut queue: Vec<crate::db::EnrichTarget>| async move {
        loop {
            run.control.wait_if_paused().await;
            if run.control.is_cancelled() {
                return None;
            }
            if let Some(target) = queue.pop() {
                return Some((target, queue));
            }
            let finished = scraping_done.load(Ordering::Relaxed);
            let database = run.database.clone();
            let job_id = run.job_id.to_owned();
            let claimed = tokio::task::spawn_blocking(move || database.claim_enrich_targets(&job_id, 100))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            if claimed.is_empty() {
                if finished {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
                continue;
            }
            queue = claimed;
            queue.reverse();
        }
    })
    .boxed();

    // Two stages, so slow mail servers never hold up website reading:
    // leads are read (place page + website) many at once and saved with
    // their emails right away; leads with emails then queue for the
    // mail-server checks, which fill in the verdicts as they finish.
    let (to_check, checks) = futures::channel::mpsc::unbounded::<(String, crate::enrich::run::Reading)>();
    let read = async move {
        let to_check = &to_check;
        targets
            .map(|target| {
                async move {
                    let outcome = tokio::select! {
                        reading = enricher.read(&target) => Some(reading),
                        () = wait_cancelled(run.control) => None,
                    };
                    let Some(reading) = outcome else {
                        return Ok(());
                    };
                    let (patch, summary) = reading.result();
                    let has_email = summary.email_count > 0;
                    let check = enricher.needs_check(&reading);
                    let database = run.database.clone();
                    let id = target.id.clone();
                    tokio::task::spawn_blocking(move || database.save_enrichment(&id, &patch, &summary))
                        .await
                        .context("save enrichment")??;
                    run.tally.enriched.fetch_add(1, Ordering::Relaxed);
                    if check {
                        run.tally.checking.fetch_add(1, Ordering::Relaxed);
                        let _ = to_check.unbounded_send((target.id, reading));
                    } else if has_email {
                        run.tally.with_email.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok::<(), anyhow::Error>(())
                }
                .boxed()
            })
            .buffer_unordered(READS_AT_ONCE.max(concurrency))
            .try_collect::<Vec<()>>()
            .await
            .map(|_| ())
        // `to_check` drops here, which ends the check stage once it drains.
    };
    let check = checks
        .map(|(id, reading)| {
            async move {
                run.control.wait_if_paused().await;
                let outcome = tokio::select! {
                    outcome = enricher.check(reading) => Some(outcome),
                    () = wait_cancelled(run.control) => None,
                };
                run.tally.checking.fetch_sub(1, Ordering::Relaxed);
                let Some((patch, summary)) = outcome else {
                    return Ok(());
                };
                let has_email = summary.email_count > 0;
                let database = run.database.clone();
                tokio::task::spawn_blocking(move || database.save_enrichment(&id, &patch, &summary))
                    .await
                    .context("save email checks")??;
                if has_email {
                    run.tally.with_email.fetch_add(1, Ordering::Relaxed);
                }
                Ok::<(), anyhow::Error>(())
            }
            .boxed()
        })
        .buffer_unordered(CHECK_LEADS_AT_ONCE)
        .try_collect::<Vec<()>>();
    tokio::try_join!(read, check).map(|_| ())
}

async fn wait_cancelled(control: &JobControl) {
    while !control.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn collect_lookup(run: &Run<'_>, lookup: Lookup) -> Result<()> {
    let result = collect_lookup_pages(run, &lookup).await;
    run.tally.lookups_done.fetch_add(1, Ordering::Relaxed);
    result
}

async fn collect_lookup_pages(run: &Run<'_>, lookup: &Lookup) -> Result<()> {
    let config = run.config;
    let mut center = lookup.center.map(|(latitude, longitude)| MapCenter {
        latitude,
        longitude,
    });
    let mut skipped = 0_u32;
    let mut seen = 0_u32;
    let per_lookup =
        (!config.collect_all && config.per_query_limit > 0).then_some(config.per_query_limit);
    for page_index in 0..MAX_PAGES {
        run.control.wait_if_paused().await;
        if run.control.is_cancelled() || run.limit_reached() {
            return Ok(());
        }
        let offset = page_index * MAP_PAGE_SIZE;
        let page = match fetch_with_retries(run, lookup, center, offset).await {
            Ok(page) => page,
            Err(error) => {
                if run.control.is_cancelled() {
                    return Ok(());
                }
                run.tally.failures.fetch_add(1, Ordering::Relaxed);
                let message = format!("{} / {}: {error}", lookup.query, lookup.location);
                let _ = run.database.append_event(run.job_id, "error", &message);
                return Ok(());
            }
        };
        center = Some(page.center);
        let rows = page.rows;
        let mut batch = Vec::with_capacity(page.places.len());
        for record in page.places {
            if skipped < config.start_at {
                skipped += 1;
                continue;
            }
            if per_lookup.is_some_and(|limit| seen >= limit) {
                break;
            }
            seen += 1;
            if matches_filters(&record, &config.filters) {
                batch.push(record);
            }
        }
        save_batch(run, lookup, batch).await?;
        if per_lookup.is_some_and(|limit| seen >= limit)
            || rows < usize::try_from(MAP_PAGE_SIZE).unwrap_or(20)
        {
            return Ok(());
        }
    }
    Ok(())
}

async fn save_batch(run: &Run<'_>, lookup: &Lookup, mut batch: Vec<BusinessRecord>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let room = run.budget.reserve(batch.len());
    batch.truncate(room);
    if batch.is_empty() {
        return Ok(());
    }
    let reserved = batch.len();
    let database = run.database.clone();
    let job_id = run.job_id.to_owned();
    let query = lookup.query.clone();
    let location = lookup.location.clone();
    let results = tokio::task::spawn_blocking(move || {
        database.insert_leads(&job_id, &query, &location, &batch)
    })
    .await
    .context("save leads")??;
    // A business an earlier search found still joins this search's list;
    // only a repeat within this search counts as a duplicate.
    let added = results
        .iter()
        .filter(|result| matches!(result, LeadInsert::New | LeadInsert::Linked))
        .count();
    let repeats = results.len() - added;
    run.tally
        .collected
        .fetch_add(i64::try_from(added).unwrap_or(0), Ordering::Relaxed);
    run.tally
        .duplicates
        .fetch_add(i64::try_from(repeats).unwrap_or(0), Ordering::Relaxed);
    run.budget.release(reserved - added);
    Ok(())
}

async fn fetch_with_retries(
    run: &Run<'_>,
    lookup: &Lookup,
    center: Option<MapCenter>,
    offset: u32,
) -> Result<MapPage> {
    let config = run.config;
    let client = GoogleMapsClient;
    let mut last_error = anyhow::anyhow!("no request attempt was made");
    for attempt in 0..=config.max_retries {
        run.control.wait_if_paused().await;
        if run.control.is_cancelled() {
            return Err(anyhow::anyhow!("cancelled"));
        }
        let proxy = run.pool.select();
        if proxy.is_none() && config.require_proxy && !config.allow_direct {
            return Err(anyhow::anyhow!("no enabled healthy proxy is available"));
        }
        let http = run.clients.get(proxy.as_ref())?;
        match client
            .search_page(
                &http,
                &lookup.query,
                &lookup.location,
                center,
                lookup.span_m,
                offset,
                config,
            )
            .await
        {
            Ok(value) => return Ok(value),
            Err(error) => {
                let route = proxy.as_ref().map_or_else(
                    || "direct".to_owned(),
                    |proxy| format!("via {}", crate::proxy::redact(&proxy.url)),
                );
                last_error = anyhow::anyhow!("{error:#} ({route})");
                if let Some(proxy) = proxy {
                    run.pool.mark_failure(&proxy.id);
                }
                if attempt < config.max_retries {
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt + 1))).await;
                }
            }
        }
    }
    Err(last_error)
}

fn matches_filters(record: &BusinessRecord, filters: &[String]) -> bool {
    filters.iter().all(|raw| {
        let Ok(filter) = crate::models::parse_filter(raw) else {
            return false;
        };
        let actual = match filter.field.as_str() {
            "name" => Some(record.name.clone()),
            "category" => record.category.clone(),
            "address" => record.address.clone(),
            "phone" => record.phone.clone(),
            "website" => record.website.clone(),
            "rating" => record.rating.map(|value| value.to_string()),
            "review_count" => record.review_count.map(|value| value.to_string()),
            "status" => record.status.clone(),
            _ => None,
        };
        match filter.operator.as_str() {
            "is_empty" => actual.as_deref().unwrap_or_default().is_empty(),
            "not_empty" => actual.as_deref().is_some_and(|value| !value.is_empty()),
            "contains" => actual
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&filter.value.to_ascii_lowercase()),
            "=" => actual
                .as_deref()
                .unwrap_or_default()
                .eq_ignore_ascii_case(&filter.value),
            "!=" => !actual
                .as_deref()
                .unwrap_or_default()
                .eq_ignore_ascii_case(&filter.value),
            ">" | ">=" | "<" | "<=" => {
                let left = actual
                    .as_deref()
                    .unwrap_or_default()
                    .parse::<f64>()
                    .unwrap_or(f64::NAN);
                let right = filter.value.parse::<f64>().unwrap_or(f64::NAN);
                match filter.operator.as_str() {
                    ">" => left > right,
                    ">=" => left >= right,
                    "<" => left < right,
                    "<=" => left <= right,
                    _ => false,
                }
            }
            "in" => filter.value.split('|').any(|value| {
                actual
                    .as_deref()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(value.trim())
            }),
            _ => false,
        }
    })
}

fn extract_place_id(value: &str) -> Option<String> {
    if value.starts_with("ChIJ") || value.starts_with("GhIJ") {
        return Some(value.split(['?', '&', '#']).next()?.to_owned());
    }
    value
        .split("!1s")
        .nth(1)
        .and_then(|part| part.split('!').next())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .split("query_place_id=")
                .nth(1)
                .and_then(|part| part.split('&').next())
                .map(ToOwned::to_owned)
        })
}

fn extract_place_name(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    let mut segments = url.path_segments()?;
    let _ = segments.find(|segment| *segment == "place")?;
    let segment = segments.next()?;
    let name = segment.replace('+', " ");
    (!name.is_empty()).then_some(name)
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_response_listing_becomes_business_record() {
        let mut business = vec![Value::Null; 79];
        business[4] = serde_json::json!([null, null, null, null, null, null, null, 4.8, 42]);
        business[9] = serde_json::json!([null, null, 18.52, 73.85]);
        business[10] = Value::String("0xabc:0xdef".to_owned());
        business[11] = Value::String("Example Cafe".to_owned());
        business[13] = serde_json::json!(["Cafe", "Coffee shop"]);
        business[18] = Value::String("1 Main St, Pune".to_owned());
        business[78] = Value::String("ChIJexample".to_owned());
        let mut listing = vec![Value::Null; 15];
        listing[14] = Value::Array(business);
        let record = MapPlace::from_listing(&Value::Array(listing), "coffee", "Pune")
            .expect("listing should parse")
            .record;
        assert_eq!(record.name, "Example Cafe");
        assert_eq!(record.review_count, Some(42));
        assert_eq!(record.source, "google_maps_web");
    }

    #[test]
    fn maps_urls_can_yield_place_ids() {
        assert_eq!(
            extract_place_id("https://maps.google.com/?data=!1sChIJ123!2e0"),
            Some("ChIJ123".to_owned())
        );
    }

    #[test]
    fn filters_match_business_attributes() {
        let record = BusinessRecord {
            name: "Blue Plumbing".to_owned(),
            category: Some("Plumber".to_owned()),
            address: None,
            phone: None,
            website: None,
            latitude: None,
            longitude: None,
            rating: Some(4.8),
            review_count: Some(12),
            status: Some("OPERATIONAL".to_owned()),
            hours: None,
            maps_url: None,
            place_id: None,
            source: "test".to_owned(),
            extra: serde_json::json!({}),
        };
        assert!(matches_filters(
            &record,
            &[
                "rating>=4".to_owned(),
                "category contains plumbing".to_owned()
            ]
        ));
    }

    #[test]
    fn listing_has_outscraper_fields() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/listing.json");
        let business: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut listing = vec![Value::Null; 15];
        listing[14] = business;
        let place = MapPlace::from_listing(&Value::Array(listing), "landscaping", "Lexington").unwrap();
        let extra = &place.record.extra;
        assert_eq!(extra["google_id"], "0x88424311b25ab3f7:0xd2ab990515c3bd27");
        assert_eq!(extra["cid"], "15180395216101424423");
        assert_eq!(extra["kgmid"], "/g/11h4lk0tjw");
        assert_eq!(extra["street"], "766 Westland Dr");
        assert_eq!(extra["postal_code"], "40504");
        assert_eq!(extra["state"], "Kentucky");
        assert_eq!(extra["state_code"], "KY");
        assert_eq!(extra["country"], "United States of America");
        assert_eq!(extra["plus_code"], "3F53+PG Lexington, Kentucky");
        assert_eq!(extra["owner_id"], "116904751837651203062");
        assert!(extra["h3"].as_str().unwrap().starts_with("89266d"), "{}", extra["h3"]);
        assert_eq!(extra["about"]["Accessibility"]["Wheelchair accessible parking lot"], true);
        assert!(extra["logo"].as_str().unwrap().contains("googleusercontent"));
    }

    #[test]
    fn names_for_emails() {
        assert_eq!(name_for_emails("Custom Lawn Care, LLC"), "Custom Lawn Care");
        assert_eq!(name_for_emails("ACME LANDSCAPING INC."), "Acme Landscaping");
        assert_eq!(name_for_emails("Stone Blue Hardscape | Patios & Walls"), "Stone Blue Hardscape");
        assert_eq!(name_for_emails("Smith & Co."), "Smith & Co.");
    }


    /// `PROXY=socks5h://… cargo test proxy_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network"]
    async fn proxy_probe() {
        let url = std::env::var("PROXY").expect("PROXY");
        let proxy = ProxyEntry {
            id: "probe".into(),
            url: url.clone(),
            protocol: url.split(':').next().unwrap().into(),
            username: None,
            password: None,
            enabled: true,
            status: "healthy".into(),
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 0,
            last_error: None,
        };
        let http = HttpClients::default().get(Some(&proxy)).unwrap();
        let config = SearchConfig::default();
        let target = bootstrap_url("landscaping Lexington, KY", &config).unwrap();
        println!("{target}");
        match request_text(&http, target).await {
            Ok(body) => println!("ok {} bytes", body.len()),
            Err(error) => println!("error: {error:#}"),
        }
        let center = MapCenter { latitude: 38.0406, longitude: -84.5037 };
        let target = map_url("landscaping Lexington, KY", center, 6000, 0, &config).unwrap();
        println!("{target}");
        match request_text(&http, target).await {
            Ok(body) => println!("ok {} bytes", body.len()),
            Err(error) => println!("error: {error:#}"),
        }
    }

}
