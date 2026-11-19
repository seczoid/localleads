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
    /// Stopped only for a restart: the search keeps its status and is
    /// picked up again when the app is back.
    suspended: Arc<AtomicBool>,
}

impl JobControl {
    pub fn new() -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            suspended: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Winds the search down for a restart without ending it.
    pub fn suspend(&self) {
        self.suspended.store(true, Ordering::Relaxed);
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended.load(Ordering::Relaxed)
    }

    pub fn toggle_pause(&self) -> bool {
        let next = !self.paused.load(Ordering::Relaxed);
        self.paused.store(next, Ordering::Relaxed);
        next
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
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
            query.append_pair("gl", &region);
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
            query.append_pair("gl", &region);
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
pub(crate) fn region(config: &SearchConfig) -> Option<std::borrow::Cow<'_, str>> {
    let value = config.region.trim();
    if !value.is_empty() && value.len() <= 3 {
        return Some(value.into());
    }
    config
        .location_selection
        .as_ref()
        .and_then(|selection| crate::models::country_region(&selection.country))
        .map(Into::into)
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
    /// Results left out because they lay far from the area searched.
    out_of_area: AtomicI64,
    /// Results left out because they weren't in the category searched.
    off_category: AtomicI64,
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
    /// Areas finished since progress was last saved.
    finished: Mutex<Vec<String>>,
    /// Google Maps is done and websites are being read.
    reading_sites: AtomicBool,
}

impl Run<'_> {
    fn limit_reached(&self) -> bool {
        self.tally.collected.load(Ordering::Relaxed) >= self.budget.limit
    }

    /// Stopped by the user or a shutdown, or the lead limit is reached:
    /// Google Maps lookups end where they are.
    fn stopped(&self) -> bool {
        self.control.is_cancelled() || self.limit_reached()
    }

    fn save_progress(&self) -> Result<()> {
        let finished = std::mem::take(
            &mut *self
                .finished
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.database.mark_lookups_done(self.job_id, &finished)?;
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
        if self.enriching && self.reading_sites.load(Ordering::Relaxed) {
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

    fn live(&self) -> crate::events::LiveProgress {
        crate::events::LiveProgress {
            lookups_done: self.tally.lookups_done.load(Ordering::Relaxed),
            total_lookups: i64::try_from(self.total_lookups).unwrap_or(i64::MAX),
            checking: self.tally.checking.load(Ordering::Relaxed),
            reading_sites: self.reading_sites.load(Ordering::Relaxed),
        }
    }
}

/// Names one area of a search, so a resumed search knows which are done.
fn lookup_key(lookup: &Lookup) -> String {
    let center = lookup
        .center
        .map(|(latitude, longitude)| format!("{latitude:.5},{longitude:.5}"))
        .unwrap_or_default();
    format!("{}\u{1f}{}\u{1f}{center}", lookup.query, lookup.location)
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
    resumed: bool,
) {
    let result =
        run_job_inner(&database, &job_id, &config, &pool, &control, &events, resumed).await;
    if control.is_suspended() {
        // The status stays as it is, so the search carries on after the
        // restart from the areas it already finished.
        let _ = database.append_event(
            &job_id,
            "info",
            "Paused for a restart; carries on when LocalLeads is back",
        );
        let _ = events.send(AppEvent::JobSuspended { job_id });
        return;
    }
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
    resumed: bool,
) -> Result<()> {
    config.validate()?;
    database.claim_job_runner(job_id)?;
    database.update_job_status(
        job_id,
        if control.is_paused() { JobStatus::Paused } else { JobStatus::Running },
        None,
    )?;
    let lookups = config.lookups();
    let total_lookups = lookups.len();
    let concurrency = usize::try_from(config.concurrency).unwrap_or(1).clamp(
        1,
        usize::try_from(crate::models::MAX_CONCURRENCY).unwrap_or(500),
    );

    // A search picked up again keeps what it had: the areas it finished
    // are skipped and its counts carry on from where they were.
    let done = database.done_lookups(job_id)?;
    let lookups: Vec<Lookup> = lookups
        .into_iter()
        .filter(|lookup| !done.contains(&lookup_key(lookup)))
        .collect();
    let tally = Tally::default();
    let mut collected = 0;
    if resumed {
        collected = database.job_lead_count(job_id)?;
        let job = database.get_job(job_id)?;
        tally.collected.store(collected, Ordering::Relaxed);
        tally.failures.store(job.as_ref().map_or(0, |job| job.failures), Ordering::Relaxed);
        tally.duplicates.store(job.as_ref().map_or(0, |job| job.duplicates), Ordering::Relaxed);
        tally.lookups_done.store(
            i64::try_from(total_lookups - lookups.len()).unwrap_or(0),
            Ordering::Relaxed,
        );
        database.append_event(
            job_id,
            "info",
            &format!(
                "Picked up where it left off: {} of {} areas were already searched, {} leads kept",
                fmt_number(i64::try_from(total_lookups - lookups.len()).unwrap_or(0)),
                fmt_number(i64::try_from(total_lookups).unwrap_or(i64::MAX)),
                fmt_number(collected),
            ),
        )?;
    } else {
        database.append_event(
            job_id,
            "info",
            &format!(
                "Search started: {} areas, {} at a time",
                fmt_number(i64::try_from(total_lookups).unwrap_or(i64::MAX)),
                concurrency
            ),
        )?;
    }
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
            reserved: AtomicI64::new(collected),
        },
        tally,
        total_lookups,
        enriching: config.enriches(),
        finished: Mutex::new(Vec::new()),
        reading_sites: AtomicBool::new(false),
    };

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
                    live: run.live(),
                });
                last = activity;
            }
        }
    };
    // Google Maps first, then the websites: both at once fight over the
    // same connections and slow each other down.
    let work = async {
        // Once the lead limit is reached (or the search is stopped), no new
        // area starts; the ones under way stop at once (see wait_stopped).
        stream::iter(lookups)
            .take_while(|_| futures::future::ready(!run.stopped()))
            .map(|lookup| collect_lookup(&run, lookup))
            .buffer_unordered(concurrency)
            .try_for_each(|()| async { Ok(()) })
            .await?;
        run.save_progress()?;
        if run.limit_reached() {
            let done = run.tally.lookups_done.load(Ordering::Relaxed);
            let total = i64::try_from(run.total_lookups).unwrap_or(i64::MAX);
            if done < total {
                database.append_event(
                    job_id,
                    "info",
                    &format!(
                        "Reached the limit of {} leads after {} of {} areas; the rest weren't searched",
                        fmt_number(run.budget.limit),
                        fmt_number(done),
                        fmt_number(total)
                    ),
                )?;
            }
        }
        let out_of_area = run.tally.out_of_area.load(Ordering::Relaxed);
        if out_of_area > 0 {
            database.append_event(
                job_id,
                "info",
                &format!(
                    "Left out {} results that lay far from the areas searched",
                    fmt_number(out_of_area)
                ),
            )?;
        }
        let off_category = run.tally.off_category.load(Ordering::Relaxed);
        if off_category > 0 {
            database.append_event(
                job_id,
                "info",
                &format!(
                    "Left out {} results in other categories than the one searched",
                    fmt_number(off_category)
                ),
            )?;
        }
        if !run.enriching || control.is_cancelled() {
            return Ok(());
        }
        let enricher = match crate::enrich::run::Enricher::new(config, pool.clone()).await {
            Ok(enricher) => enricher,
            Err(error) => {
                database.append_event(job_id, "error", &format!("Enrichment is off: {error}"))?;
                return Ok(());
            }
        };
        database.append_event(
            job_id,
            "info",
            &format!(
                "Google Maps done with {} leads. {}, {} at a time",
                fmt_number(run.tally.collected.load(Ordering::Relaxed)),
                enricher.describe(),
                concurrency
            ),
        )?;
        run.reading_sites.store(true, Ordering::Relaxed);
        let result = enrich_leads(&run, &enricher, concurrency).boxed().await;
        let _ = database.release_job_enrichment(job_id);
        result
    };
    tokio::pin!(report);
    let result: Result<()> = tokio::select! {
        result = work => result,
        () = &mut report => Ok(()),
    };
    result?;
    run.save_progress()?;
    let _ = events.send(AppEvent::JobProgress {
        job_id: job_id.to_owned(),
        message: run.activity(),
        live: run.live(),
    });
    Ok(())
}

/// Enriches the search's leads once Google Maps is done, as many at once
/// as the search itself ran.
/// Mail-server checks run apart from reading, so slow mail servers never
/// hold up the websites; the enricher bounds the checks themselves.
const CHECK_LEADS_AT_ONCE: usize = 256;

async fn enrich_leads(
    run: &Run<'_>,
    enricher: &crate::enrich::run::Enricher,
    concurrency: usize,
) -> Result<()> {
    // Claims walk the list in id order from a cursor, so each claim reads
    // only new rows however long the list is. A last pass from the start
    // picks up leads handed back while the walk went on.
    let batch = (concurrency * 2).max(200);
    let targets = stream::unfold(
        (Vec::new(), String::new(), false),
        move |(mut queue, mut after, mut last_pass): (Vec<crate::db::EnrichTarget>, String, bool)| async move {
            loop {
                run.control.wait_if_paused().await;
                if run.control.is_cancelled() {
                    return None;
                }
                if let Some(target) = queue.pop() {
                    return Some((target, (queue, after, last_pass)));
                }
                let database = run.database.clone();
                let job_id = run.job_id.to_owned();
                let from = after.clone();
                let claimed = tokio::task::spawn_blocking(move || {
                    database.claim_enrich_targets(&job_id, &from, batch)
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
                if claimed.is_empty() {
                    if last_pass {
                        return None;
                    }
                    last_pass = true;
                    after.clear();
                    continue;
                }
                if let Some(last) = claimed.iter().map(|target| &target.id).max() {
                    after.clone_from(last);
                }
                queue = claimed;
                queue.reverse();
            }
        },
    )
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
                    run.database
                        .save_enrichment_async(target.id.clone(), patch, summary)
                        .await
                        .context("save enrichment")?;
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
            .buffer_unordered(concurrency)
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
                run.database
                    .save_enrichment_async(id, patch, summary)
                    .await
                    .context("save email checks")?;
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

async fn wait_stopped(run: &Run<'_>) {
    while !run.stopped() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn collect_lookup(run: &Run<'_>, lookup: Lookup) -> Result<()> {
    let result = collect_lookup_pages(run, &lookup).await;
    // An area cut short by a stop, a shutdown or the lead limit is searched
    // again when the search is continued; one that ran to its end (or
    // failed) is not.
    if matches!(result, Ok(true)) {
        run.tally.lookups_done.fetch_add(1, Ordering::Relaxed);
        run.finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(lookup_key(&lookup));
    }
    result.map(|_| ())
}

/// Searches one area page by page. Ok(false) when it was cut short (a stop,
/// a shutdown or the lead limit), so the area counts as not searched yet.
async fn collect_lookup_pages(run: &Run<'_>, lookup: &Lookup) -> Result<bool> {
    let config = run.config;
    let mut center = lookup.center.map(|(latitude, longitude)| MapCenter {
        latitude,
        longitude,
    });
    let mut skipped = 0_u32;
    let mut seen = 0_u32;
    let per_lookup =
        (!config.collect_all && config.per_query_limit > 0).then_some(config.per_query_limit);
    let full_page = usize::try_from(MAP_PAGE_SIZE).unwrap_or(20);
    let category = searched_category(config, &lookup.query);
    let mut page_index = 0;
    while page_index < MAX_PAGES {
        run.control.wait_if_paused().await;
        if run.stopped() {
            return Ok(false);
        }
        // Page 1 alone. A full page 1 means the area likely has more, so
        // pages 2 and 3 are asked for together; later pages one at a time.
        // Pages are still taken in order, and a short page 2 ends the
        // lookup as before.
        let room_for_two = per_lookup.is_none_or(|limit| seen + MAP_PAGE_SIZE < limit);
        let together = if page_index == 1 && room_for_two { 2 } else { 1 };
        let together = together.min(MAX_PAGES - page_index);
        let pages = futures::future::join_all(
            (page_index..page_index + together)
                .map(|index| fetch_with_retries(run, lookup, center, index * MAP_PAGE_SIZE)),
        )
        .await;
        for page in pages {
            page_index += 1;
            let page = match page {
                Ok(page) => page,
                Err(error) => {
                    if run.stopped() {
                        return Ok(false);
                    }
                    run.tally.failures.fetch_add(1, Ordering::Relaxed);
                    let message = format!("{} / {}: {error}", lookup.query, lookup.location);
                    let _ = run.database.append_event(run.job_id, "error", &message);
                    return Ok(true);
                }
            };
            center = Some(page.center);
            let rows = page.rows;
            let mut batch = Vec::with_capacity(page.places.len());
            let mut relevant = 0_usize;
            for record in page.places {
                // An area Google can't place (a PO-box ZIP, say) gets
                // results from anywhere; those belong to no area searched.
                if !in_reach(lookup, &record) {
                    run.tally.out_of_area.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                // Where an area has few businesses of the kind searched,
                // Google fills the list with whatever else is nearby.
                if category.is_some_and(|category| !in_category(&record, category)) {
                    run.tally.off_category.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                relevant += 1;
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
            // A full page with nothing near the area, or nothing of the
            // kind searched: later pages only go farther out.
            let strayed = rows > 0 && relevant == 0;
            if per_lookup.is_some_and(|limit| seen >= limit) || rows < full_page || strayed {
                return Ok(true);
            }
            if run.stopped() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// The Google Maps category a text search names, when the query is exactly
/// one (as the category picker fills it in). Results are then kept only if
/// Google lists that category for them. Category names are English, so
/// searches in other languages are left as they are.
fn searched_category(config: &SearchConfig, query: &str) -> Option<&'static str> {
    let english = language(config).to_ascii_lowercase().starts_with("en");
    (matches!(config.mode, SearchMode::Text) && english)
        .then(|| crate::categories::exact(query.trim().trim_matches('"')))
        .flatten()
}

/// Whether Google lists `category` (lower case) among the record's
/// categories, main or additional.
fn in_category(record: &BusinessRecord, category: &str) -> bool {
    let listed = record
        .extra
        .get("subtypes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .chain(record.category.as_deref());
    listed.into_iter().any(|name| name.trim().eq_ignore_ascii_case(category))
}

/// How far from an area's centre a result may lie and still count, in
/// metres: three times the area's span, and never under 25 km.
fn reach_m(span_m: u32) -> f64 {
    (f64::from(span_m) * 3.0).max(25_000.0)
}

/// Whether a result lies near the area the lookup aimed at. A lookup with
/// no centre, or a result with no coordinates, always counts.
fn in_reach(lookup: &Lookup, record: &BusinessRecord) -> bool {
    let (Some(center), Some(latitude), Some(longitude)) =
        (lookup.center, record.latitude, record.longitude)
    else {
        return true;
    };
    distance_m(center, (latitude, longitude)) <= reach_m(lookup.span_m)
}

/// Great-circle distance between two points, in metres.
fn distance_m((lat1, lon1): (f64, f64), (lat2, lon2): (f64, f64)) -> f64 {
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let half_dphi = (phi2 - phi1) / 2.0;
    let half_dlambda = (lon2 - lon1).to_radians() / 2.0;
    let a = half_dphi.sin().powi(2) + phi1.cos() * phi2.cos() * half_dlambda.sin().powi(2);
    2.0 * 6_371_000.0 * a.sqrt().min(1.0).asin()
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
    let results = run
        .database
        .insert_leads_async(run.job_id, &lookup.query, &lookup.location, &batch)
        .await
        .context("save leads")?;
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
        if run.stopped() {
            return Err(anyhow::anyhow!("stopped"));
        }
        let proxy = run.pool.select();
        if proxy.is_none() && config.require_proxy && !config.allow_direct {
            return Err(anyhow::anyhow!("no enabled healthy proxy is available"));
        }
        let http = run.clients.get(proxy.as_ref())?;
        // A request under way when the limit is reached (or the search is
        // stopped) is dropped instead of waited for.
        let request = client.search_page(
            &http,
            &lookup.query,
            &lookup.location,
            center,
            lookup.span_m,
            offset,
            config,
        );
        let outcome = tokio::select! {
            outcome = request => outcome,
            () = wait_stopped(run) => return Err(anyhow::anyhow!("stopped")),
        };
        match outcome {
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
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt + 1))) => {}
                        () = wait_stopped(run) => return Err(anyhow::anyhow!("stopped")),
                    }
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
    fn category_searches_keep_only_that_category() {
        let record = |category: &str, subtypes: &[&str]| BusinessRecord {
            name: "Example".to_owned(),
            category: Some(category.to_owned()),
            address: None,
            phone: None,
            website: None,
            latitude: None,
            longitude: None,
            rating: None,
            review_count: None,
            status: None,
            hours: None,
            maps_url: None,
            place_id: None,
            source: "test".to_owned(),
            extra: serde_json::json!({ "subtypes": subtypes }),
        };
        let config = SearchConfig::default();
        let category = searched_category(&config, "House Cleaning Service").expect("a category");
        assert!(in_category(&record("House cleaning service", &["House cleaning service"]), category));
        // Listed as an additional category still counts.
        assert!(in_category(
            &record("Janitorial service", &["Janitorial service", "House cleaning service"]),
            category
        ));
        assert!(!in_category(&record("Plumber", &["Plumber"]), category));
        assert!(!in_category(&record("Cleaning service", &["Cleaning service"]), category));

        // Free text and other languages keep everything Google returns.
        assert_eq!(searched_category(&config, "cheap cleaners near the beach"), None);
        let german = SearchConfig { language: "de".to_owned(), ..SearchConfig::default() };
        assert_eq!(searched_category(&german, "house cleaning service"), None);
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
                "category contains plumb".to_owned()
            ]
        ));
        assert!(!matches_filters(&record, &["category contains plumbing".to_owned()]));
    }

    #[test]
    fn results_far_from_the_area_are_left_out() {
        let record = |latitude, longitude| BusinessRecord {
            name: "Dentist".to_owned(),
            category: None,
            address: None,
            phone: None,
            website: None,
            latitude,
            longitude,
            rating: None,
            review_count: None,
            status: None,
            hours: None,
            maps_url: None,
            place_id: None,
            source: "test".to_owned(),
            extra: serde_json::json!({}),
        };
        // A ZIP in Austin, TX.
        let zip = Lookup {
            query: "dentist".to_owned(),
            location: "78709, TX".to_owned(),
            center: Some((30.2672, -97.7431)),
            span_m: 8_000,
        };
        assert!(in_reach(&zip, &record(Some(30.3072), Some(-97.7560))), "north Austin");
        assert!(in_reach(&zip, &record(Some(30.4394), Some(-97.6200))), "Pflugerville, 22 km");
        assert!(!in_reach(&zip, &record(Some(32.7767), Some(-96.7970))), "Dallas");
        assert!(!in_reach(&zip, &record(Some(29.7604), Some(-95.3698))), "Houston");
        assert!(in_reach(&zip, &record(None, None)), "no coordinates");
        let anywhere = Lookup { center: None, ..zip.clone() };
        assert!(in_reach(&anywhere, &record(Some(32.7767), Some(-96.7970))));
        // A region reaches farther than a ZIP.
        let region = Lookup { span_m: 50_000, ..zip };
        assert!(in_reach(&region, &record(Some(29.4241), Some(-98.4936))), "San Antonio, 118 km");
        let km = distance_m((30.2672, -97.7431), (32.7767, -96.7970)) / 1000.0;
        assert!((km - 295.0).abs() < 3.0, "Austin to Dallas is about 295 km, got {km}");
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


    #[tokio::test]
    async fn the_lead_limit_stops_areas_without_marking_them_searched() {
        let path = std::env::temp_dir().join(format!("localleads-limit-{}.sqlite", uuid::Uuid::new_v4()));
        let database = Database::open(&path).unwrap();
        let config = SearchConfig { total_limit: 500, max_retries: 3, ..SearchConfig::default() };
        let pool = ProxyPool::new(Vec::new());
        let control = JobControl::new();
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let run = Run {
            database: &database,
            job_id: "job",
            config: &config,
            pool: &pool,
            control: &control,
            events: &events,
            clients: HttpClients::default(),
            budget: Budget { limit: 500, reserved: AtomicI64::new(500) },
            tally: Tally::default(),
            total_lookups: 3,
            enriching: false,
            finished: Mutex::new(Vec::new()),
            reading_sites: AtomicBool::new(false),
        };
        run.tally.collected.store(500, Ordering::Relaxed);
        let lookup = |location: &str| Lookup {
            query: "roofing contractor".into(),
            location: location.into(),
            center: Some((27.9, -82.4)),
            span_m: 6000,
        };

        // Reached: no request is made and the area stays unsearched.
        let started = std::time::Instant::now();
        let error = fetch_with_retries(&run, &lookup("Tampa, FL"), None, 0).await.unwrap_err();
        assert_eq!(error.to_string(), "stopped");
        collect_lookup(&run, lookup("Tampa, FL")).await.unwrap();
        assert_eq!(run.tally.lookups_done.load(Ordering::Relaxed), 0);
        assert!(run.finished.lock().unwrap().is_empty());
        assert_eq!(run.tally.failures.load(Ordering::Relaxed), 0);

        // Reached while a retry is waiting: it ends at once.
        run.tally.collected.store(0, Ordering::Relaxed);
        let stopper = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            run.tally.collected.store(500, Ordering::Relaxed);
        };
        let started_wait = std::time::Instant::now();
        let ((), outcome) = tokio::join!(stopper, async {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(30)) => false,
                () = wait_stopped(&run) => true,
            }
        });
        assert!(outcome && started_wait.elapsed() < Duration::from_secs(2));

        // New areas aren't started once it's reached.
        let started_areas = std::sync::atomic::AtomicUsize::new(0);
        stream::iter([lookup("A"), lookup("B"), lookup("C")])
            .take_while(|_| futures::future::ready(!run.stopped()))
            .for_each(|_| async { started_areas.fetch_add(1, Ordering::Relaxed); })
            .await;
        assert_eq!(started_areas.load(Ordering::Relaxed), 0);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(database);
        let _ = std::fs::remove_file(&path);
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
