//! Display-ready view models. Templates only print strings and flags, so all
//! formatting and status logic lives here.

use crate::app::{CheckerSettings, Notice, ProxyCheckProgress};
use crate::db::{DashboardStats, JobEvent, LeadDetail, LeadSource, ProxyCheckRow};
use crate::models::{JobStatus, JobSummary, LeadRow, LocationMode, ProxyEntry, ProxyGroup};
use std::collections::HashMap;

pub fn fmt_time(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%b %-d, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

pub fn fmt_time_full(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

/// "just now", "12m ago", "3h ago", "4d ago", then a date.
pub fn fmt_ago(value: Option<&str>) -> String {
    let Some(time) = value.and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
    else {
        return String::new();
    };
    let seconds = (chrono::Utc::now() - time.with_timezone(&chrono::Utc)).num_seconds();
    match seconds {
        ..60 => "just now".to_owned(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..604_800 => format!("{}d ago", seconds / 86_400),
        _ => time
            .with_timezone(&chrono::Local)
            .format("%b %-d")
            .to_string(),
    }
}

pub fn fmt_count(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if value < 0 { format!("-{out}") } else { out }
}

pub fn fmt_bytes(value: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", units[unit])
    }
}

pub fn percent(part: i64, whole: i64) -> u32 {
    if whole <= 0 {
        return 0;
    }
    ((part as f64 / whole as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u32
}

pub fn job_status_class(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Completed => "ok",
        JobStatus::Failed => "bad",
        JobStatus::Cancelled | JobStatus::Interrupted => "muted",
        JobStatus::Paused | JobStatus::Stopping => "warn",
        JobStatus::Running | JobStatus::Queued => "live",
    }
}

pub fn proxy_status_class(status: &str) -> &'static str {
    match status {
        "healthy" => "ok",
        "slow" => "warn",
        "failed" => "bad",
        _ => "muted",
    }
}

#[derive(Clone, Debug)]
pub struct JobView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub status_class: &'static str,
    pub collected: String,
    pub total_label: String,
    pub has_target: bool,
    pub pct: u32,
    pub failures: i64,
    pub duplicates: i64,
    pub error: String,
    pub started: String,
    pub finished: String,
    pub active: bool,
    pub live: bool,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_stop: bool,
    pub searches: usize,
    pub queries: String,
    pub locations: String,
    pub tags: Vec<String>,
}

impl JobView {
    pub fn new(job: &JobSummary, live: bool) -> Self {
        let active = job.status.is_active();
        let has_target = job.total > 0;
        let pct = if job.status == JobStatus::Completed {
            100
        } else {
            percent(job.collected, job.total)
        };
        let locations = location_summary(&job.config);
        Self {
            id: job.id.clone(),
            name: job.name.clone(),
            status: job.status.to_string(),
            status_class: job_status_class(job.status),
            collected: fmt_count(job.collected),
            total_label: if has_target {
                fmt_count(job.total)
            } else {
                "all".to_owned()
            },
            has_target,
            pct,
            failures: job.failures,
            duplicates: job.duplicates,
            error: job.error.clone().unwrap_or_default(),
            started: fmt_time(job.started_at.as_deref()),
            finished: fmt_time(job.finished_at.as_deref()),
            active,
            live,
            can_pause: live && job.status == JobStatus::Running,
            can_resume: live && job.status == JobStatus::Paused,
            can_stop: live
                && matches!(
                    job.status,
                    JobStatus::Queued | JobStatus::Running | JobStatus::Paused
                ),
            searches: job.config.lookup_count(),
            queries: job.config.queries.join(", "),
            locations,
            tags: job.config.tags.clone(),
        }
    }
}

/// Where a search looks, in plain words: "the United States",
/// "New York, Texas and Louisville, KY", "Austin, TX and 2 more".
pub fn location_summary(config: &crate::models::SearchConfig) -> String {
    let Some(selection) = &config.location_selection else {
        return name_list(&config.locations);
    };
    match selection.mode {
        LocationMode::All => {
            if selection.country.starts_with("United ") {
                format!("the {}", selection.country)
            } else {
                selection.country.clone()
            }
        }
        LocationMode::States => {
            let names: Vec<String> = selection
                .states
                .iter()
                .chain(&selection.cities)
                .cloned()
                .collect();
            name_list(&names)
        }
        LocationMode::PostalCodes => name_list(&selection.postal_codes),
    }
}

fn name_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [first, second, third] => format!("{first}, {second} and {third}"),
        [first, second, rest @ ..] => format!("{first}, {second} and {} more", rest.len()),
    }
}

/// Key/value pairs describing a job's configuration on its detail page.
pub fn config_rows(config: &crate::models::SearchConfig) -> Vec<(&'static str, String)> {
    let yes_no = |value: bool| if value { "yes" } else { "no" }.to_owned();
    vec![
        ("Queries", config.queries.join(" | ")),
        ("Locations", location_summary(config)),
        ("Mode", config.mode.to_string()),
        (
            "Language / region",
            format!("{} / {}", config.language, config.region),
        ),
        (
            "Limits",
            if config.collect_all {
                "collect all pages".to_owned()
            } else {
                let per_lookup = if config.per_query_limit == 0 {
                    "no limit per lookup".to_owned()
                } else {
                    format!(
                        "{} per lookup",
                        fmt_count(i64::from(config.per_query_limit))
                    )
                };
                format!(
                    "{} total, {per_lookup}",
                    fmt_count(i64::from(config.total_limit))
                )
            },
        ),
        ("Start at", config.start_at.to_string()),
        (
            "Concurrency / retries",
            format!("{} / {}", config.concurrency, config.max_retries),
        ),
        ("Exact match", yes_no(config.exact_match)),
        (
            "Proxies",
            format!(
                "require: {}, direct fallback: {}",
                yes_no(config.require_proxy),
                yes_no(config.allow_direct)
            ),
        ),
        (
            "Filters",
            if config.filters.is_empty() {
                "none".to_owned()
            } else {
                config.filters.join("; ")
            },
        ),
        ("Output fields", config.field_list()),
        (
            "Enrichments",
            if config.enrichments.is_empty() {
                "none".to_owned()
            } else {
                config.enrichments.join(", ")
            },
        ),
        (
            "Tags",
            if config.tags.is_empty() {
                "none".to_owned()
            } else {
                config.tags.join(", ")
            },
        ),
    ]
}

#[derive(Clone, Debug)]
pub struct EventView {
    pub time: String,
    pub level: String,
    pub message: String,
}

impl From<JobEvent> for EventView {
    fn from(event: JobEvent) -> Self {
        Self {
            time: fmt_time_full(&event.created_at),
            level: event.level,
            message: event.message,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct LeadView {
    pub id: String,
    pub name: String,
    pub category: String,
    pub address: String,
    pub phone: String,
    pub website: String,
    pub website_host: String,
    pub rating: String,
    pub reviews: String,
    pub status: String,
    pub maps_url: String,
    pub place_id: String,
    /// "Austin, TX"
    pub location: String,
    pub postal_code: String,
    pub added: String,
    pub email: String,
    pub email_reach: String,
    /// More usable addresses besides `email`.
    pub more_emails: i64,
    /// "", queued, done or failed.
    pub enrich: String,
}

pub fn place_label(city: Option<&str>, state: Option<&str>) -> String {
    match (
        city.filter(|value| !value.is_empty()),
        state.filter(|value| !value.is_empty()),
    ) {
        (Some(city), Some(state)) => format!("{city}, {state}"),
        (Some(city), None) => city.to_owned(),
        (None, Some(state)) => state.to_owned(),
        (None, None) => String::new(),
    }
}

impl From<LeadRow> for LeadView {
    fn from(lead: LeadRow) -> Self {
        let website = lead.website.unwrap_or_default();
        let website_host = url::Url::parse(&website)
            .ok()
            .and_then(|url| {
                url.host_str()
                    .map(|host| host.trim_start_matches("www.").to_owned())
            })
            .unwrap_or_else(|| website.clone());
        Self {
            id: lead.id,
            name: lead.name,
            category: lead.category.unwrap_or_default(),
            address: lead.address.unwrap_or_default(),
            phone: lead.phone.unwrap_or_default(),
            website_host,
            website,
            rating: lead
                .rating
                .map(|value| format!("{value:.1}"))
                .unwrap_or_default(),
            reviews: lead.review_count.map(fmt_count).unwrap_or_default(),
            status: lead.status.unwrap_or_default(),
            maps_url: lead.maps_url.unwrap_or_default(),
            place_id: lead.place_id.unwrap_or_default(),
            location: place_label(lead.city.as_deref(), lead.state.as_deref()),
            postal_code: lead.postal_code.unwrap_or_default(),
            added: fmt_ago(Some(&lead.created_at)),
            email: lead.email.unwrap_or_default(),
            email_reach: lead.email_reach.unwrap_or_default(),
            more_emails: (lead.email_count - 1).max(0),
            enrich: lead.enrich_status.unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LeadSourceView {
    pub job_id: String,
    pub name: String,
    pub search: String,
    pub found: String,
}

#[derive(Clone, Debug)]
pub struct LeadDetailView {
    pub lead: LeadView,
    pub sources: Vec<LeadSourceView>,
    pub coordinates: String,
    pub hours: Vec<String>,
    pub source: String,
    pub created: String,
    pub raw_json: String,
    pub enrich: EnrichView,
}

/// One email in the lead popup.
#[derive(Clone, Debug, Default)]
pub struct EmailView {
    pub email: String,
    /// ok, warn, bad or "" for the badge colour.
    pub tone: &'static str,
    /// Verified, Risky, Unverified, Invalid.
    pub verdict: &'static str,
    /// "SMTP validated", "Catch-all", …
    pub details: String,
    /// "mailto link on /contact"
    pub found: String,
    pub source: String,
    pub person: String,
    /// What the mail-server check found, as short labels.
    pub checks: Vec<String>,
    pub gravatar: String,
}

/// Everything enrichment added, ready for the popup.
#[derive(Clone, Debug, Default)]
pub struct EnrichView {
    /// "", queued, done or failed.
    pub status: String,
    pub enriched: String,
    pub emails: Vec<EmailView>,
    pub website_title: String,
    pub website_description: String,
    pub website_generator: String,
    pub website_tags: Vec<&'static str>,
    pub pages: usize,
    pub socials: Vec<(&'static str, String)>,
    pub phones: String,
    pub people: Vec<String>,
    pub per_score: Vec<(u8, i64, u32)>,
    pub review_tags: String,
    pub owner: String,
    pub owner_link: String,
    pub verified: Option<bool>,
    pub description: String,
    pub photo: String,
    pub street_view: String,
    pub about: Vec<String>,
    pub ids: Vec<(&'static str, String)>,
    pub errors: Vec<String>,
}

impl EnrichView {
    fn new(extra: &serde_json::Value, status: Option<String>, enriched_at: Option<String>) -> Self {
        use serde_json::Value;
        let text = |key: &str| -> String {
            match extra.get(key) {
                Some(Value::String(value)) => value.trim().to_owned(),
                Some(Value::Number(value)) => value.to_string(),
                _ => String::new(),
            }
        };
        let flag = |key: &str| extra.get(key).and_then(Value::as_bool);
        let emails = extra
            .get("emails")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(email_view)
            .collect();
        let mut website_tags = Vec::new();
        if flag("website_has_gtm") == Some(true) {
            website_tags.push("Google Tag Manager");
        }
        if flag("website_has_fb_pixel") == Some(true) {
            website_tags.push("Facebook pixel");
        }
        let socials = [
            ("LinkedIn", "company_linkedin"),
            ("Facebook", "company_facebook"),
            ("Instagram", "company_instagram"),
            ("X", "company_x"),
            ("YouTube", "company_youtube"),
            ("TikTok", "company_tiktok"),
        ]
        .into_iter()
        .filter_map(|(label, key)| Some((label, text(key))).filter(|(_, url)| url.starts_with("http")))
        .collect();
        let people = extra
            .get("contacts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|contact| {
                let name = contact.get("name")?.as_str()?;
                let mut line = name.to_owned();
                for key in ["title", "email", "phone"] {
                    if let Some(value) = contact.get(key).and_then(Value::as_str) {
                        line.push_str(" · ");
                        line.push_str(value);
                    }
                }
                Some(line)
            })
            .collect();
        let scores: Vec<i64> = (1..=5)
            .map(|star| extra.get(format!("reviews_per_score_{star}")).and_then(Value::as_i64).unwrap_or(0))
            .collect();
        let most = scores.iter().copied().max().unwrap_or(0);
        let per_score = if extra.get("reviews_per_score_5").is_some() {
            (1..=5u8)
                .rev()
                .map(|star| {
                    let count = scores[usize::from(star - 1)];
                    let width = if most > 0 { u32::try_from(count * 100 / most).unwrap_or(0) } else { 0 };
                    (star, count, width)
                })
                .collect()
        } else {
            Vec::new()
        };
        let about = extra
            .get("about")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .flat_map(|(group, items)| {
                items
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(move |(item, on)| {
                        if on.as_bool() == Some(false) {
                            format!("{group}: no {}", item.to_lowercase())
                        } else {
                            format!("{group}: {item}")
                        }
                    })
            })
            .collect();
        let ids = [
            ("Google ID", "google_id"),
            ("CID", "cid"),
            ("KG MID", "kgmid"),
            ("Plus code", "plus_code"),
            ("H3", "h3"),
            ("Time zone", "time_zone"),
        ]
        .into_iter()
        .filter_map(|(label, key)| Some((label, text(key))).filter(|(_, value)| !value.is_empty()))
        .collect();
        let errors = extra
            .get("enrich_errors")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(step, error)| Some(format!("{step}: {}", error.as_str()?)))
            .collect();
        Self {
            status: status.unwrap_or_default(),
            enriched: enriched_at.as_deref().map(|at| fmt_ago(Some(at))).unwrap_or_default(),
            emails,
            website_title: text("website_title"),
            website_description: text("website_description"),
            website_generator: text("website_generator"),
            website_tags,
            pages: extra.get("website_pages").and_then(Value::as_array).map_or(0, Vec::len),
            socials,
            phones: text("company_phones"),
            people,
            per_score,
            review_tags: text("reviews_tags"),
            owner: text("owner_title"),
            owner_link: text("owner_link"),
            verified: flag("verified"),
            description: text("description"),
            photo: text("photo"),
            street_view: text("street_view"),
            about,
            ids,
            errors,
        }
    }
}

fn email_view(entry: &serde_json::Value) -> EmailView {
    let text = |key: &str| entry.get(key).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let (tone, verdict) = match entry.get("reachable").and_then(serde_json::Value::as_str) {
        Some("safe") => ("ok", "Verified"),
        Some("risky") => ("warn", "Risky"),
        Some("invalid") => ("bad", "Invalid"),
        Some("unknown") => ("", "Unverified"),
        // Not checked with the mail server.
        _ => ("", ""),
    };
    let found = match text("found").as_str() {
        "mailto" => "mailto link",
        "cloudflare" => "Cloudflare-protected address",
        "structured_data" => "structured data",
        "text" => "page text",
        "obfuscated" => "obfuscated text",
        _ => "page",
    }
    .to_owned();
    let mut person = text("full_name");
    let title = text("title");
    if !title.is_empty() {
        person = if person.is_empty() { title } else { format!("{person}, {title}") };
    }
    let mut checks = Vec::new();
    let mut gravatar = String::new();
    if let Some(check) = entry.get("check").filter(|check| check.is_object()) {
        let on = |key: &str| check.get(key).and_then(serde_json::Value::as_bool) == Some(true);
        if !on("valid_syntax") {
            checks.push("bad syntax".to_owned());
        } else if !on("accepts_mail") {
            checks.push("domain takes no mail".to_owned());
        } else {
            checks.push("domain accepts mail".to_owned());
        }
        if on("smtp_checked") {
            if on("is_deliverable") {
                checks.push("mailbox exists".to_owned());
            } else if on("can_connect_smtp") && !on("is_catch_all") {
                checks.push("mailbox not found".to_owned());
            }
        } else if on("accepts_mail") {
            checks.push("mailbox not checked".to_owned());
        }
        for (key, label) in [
            ("is_catch_all", "catch-all domain"),
            ("has_full_inbox", "inbox full"),
            ("is_disabled", "mailbox disabled"),
            ("is_disposable", "disposable"),
            ("is_role", "role address"),
            ("is_free", "free provider"),
        ] {
            if on(key) {
                checks.push(label.to_owned());
            }
        }
        if let Some(suggestion) = check.get("suggestion").and_then(serde_json::Value::as_str) {
            checks.push(format!("did you mean {suggestion}?"));
        }
        gravatar = check
            .get("gravatar_url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
    }
    EmailView {
        email: text("email"),
        tone,
        verdict,
        details: text("status_details"),
        found,
        source: text("source"),
        person,
        checks,
        gravatar,
    }
}

/// Opening hours as "Monday: 9 AM–5 PM" lines. Maps sends a nested array
/// (`[["Monday", 1, [y, m, d], [["9 AM–5 PM", …]], …], …]`); plain text
/// separated by `|`, `;` or newlines is passed through.
fn hours_lines(value: &str) -> Vec<String> {
    if let Ok(serde_json::Value::Array(days)) = serde_json::from_str(value) {
        let lines: Vec<String> = days
            .iter()
            .filter_map(|day| {
                let name = day.get(0)?.as_str()?;
                let times: Vec<&str> = day
                    .get(3)?
                    .as_array()?
                    .iter()
                    .filter_map(|slot| slot.get(0)?.as_str())
                    .collect();
                Some(if times.is_empty() {
                    format!("{name}: closed")
                } else {
                    format!("{name}: {}", times.join(", "))
                })
            })
            .collect();
        if !lines.is_empty() {
            return lines;
        }
    }
    value
        .split(['|', ';', '\n'])
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.starts_with('['))
        .map(ToOwned::to_owned)
        .collect()
}

impl LeadDetailView {
    pub fn new(detail: LeadDetail, sources: Vec<LeadSource>) -> Self {
        let coordinates = match (detail.latitude, detail.longitude) {
            (Some(lat), Some(lng)) => format!("{lat:.6}, {lng:.6}"),
            _ => String::new(),
        };
        let hours = detail.hours.as_deref().map(hours_lines).unwrap_or_default();
        let extra = serde_json::from_str::<serde_json::Value>(&detail.extra_json).unwrap_or_default();
        let enrich = EnrichView::new(&extra, detail.row.enrich_status.clone(), detail.enriched_at.clone());
        let raw_json = serde_json::to_string_pretty(&extra).unwrap_or(detail.extra_json);
        Self {
            lead: detail.row.into(),
            sources: sources
                .into_iter()
                .map(|source| {
                    let search = match (source.query, source.location) {
                        (Some(query), Some(location)) => format!("“{query}” in {location}"),
                        (Some(query), None) => format!("“{query}”"),
                        (None, Some(location)) => location,
                        (None, None) => String::new(),
                    };
                    LeadSourceView {
                        name: source
                            .job_name
                            .unwrap_or_else(|| "Deleted search".to_owned()),
                        job_id: source.job_id,
                        search,
                        found: fmt_time(Some(&source.found_at)),
                    }
                })
                .collect(),
            coordinates,
            hours,
            source: detail.source,
            created: fmt_time_full(&detail.created_at),
            raw_json,
            enrich,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProxyView {
    pub id: String,
    pub masked: String,
    pub protocol: String,
    pub enabled: bool,
    pub status: String,
    pub status_class: &'static str,
    pub latency: String,
    pub groups: String,
    pub last_error: String,
    pub last_checked: String,
    pub failures: i64,
}

impl ProxyView {
    pub fn new(proxy: &ProxyEntry, groups: &HashMap<String, Vec<String>>) -> Self {
        Self {
            id: proxy.id.clone(),
            masked: proxy.masked_url(),
            protocol: proxy.protocol.clone(),
            enabled: proxy.enabled,
            status: proxy.status.clone(),
            status_class: proxy_status_class(&proxy.status),
            latency: proxy
                .latency_ms
                .map(|value| format!("{} ms", fmt_count(value)))
                .unwrap_or_default(),
            groups: groups
                .get(&proxy.id)
                .map(|names| names.join(", "))
                .unwrap_or_default(),
            last_error: proxy.last_error.clone().unwrap_or_default(),
            last_checked: proxy
                .last_checked_at
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%b %-d, %H:%M")
                        .to_string()
                })
                .unwrap_or_default(),
            failures: proxy.consecutive_failures,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProxyCheckView {
    pub time: String,
    pub status: String,
    pub status_class: &'static str,
    pub latency: String,
    pub exit_ip: String,
    pub error: String,
    pub endpoint: String,
}

impl From<ProxyCheckRow> for ProxyCheckView {
    fn from(row: ProxyCheckRow) -> Self {
        Self {
            time: fmt_time_full(&row.checked_at),
            status_class: proxy_status_class(&row.status),
            status: row.status,
            latency: row
                .latency_ms
                .map(|value| format!("{} ms", fmt_count(value)))
                .unwrap_or_default(),
            exit_ip: row.exit_ip.unwrap_or_default(),
            error: row.error.unwrap_or_default(),
            endpoint: row.endpoint,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub count: String,
    pub active: bool,
}

impl GroupView {
    pub fn new(group: &ProxyGroup, active: Option<&str>) -> Self {
        Self {
            id: group.id.clone(),
            name: group.name.clone(),
            count: fmt_count(group.proxy_count),
            active: active == Some(group.id.as_str()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StatsView {
    pub jobs: String,
    pub active_jobs: String,
    pub leads: String,
    pub phone_pct: u32,
    pub website_pct: u32,
    pub proxies: String,
    pub healthy_proxies: String,
    pub healthy_pct: u32,
}

impl From<DashboardStats> for StatsView {
    fn from(stats: DashboardStats) -> Self {
        Self {
            jobs: fmt_count(stats.jobs),
            active_jobs: fmt_count(stats.active_jobs),
            leads: fmt_count(stats.leads),
            phone_pct: percent(stats.leads_with_phone, stats.leads),
            website_pct: percent(stats.leads_with_website, stats.leads),
            proxies: fmt_count(stats.proxies),
            healthy_proxies: fmt_count(stats.healthy_proxies),
            healthy_pct: percent(stats.healthy_proxies, stats.proxies),
        }
    }
}

/// Proxy health distribution as percentages for a stacked bar.
#[derive(Clone, Debug, Default)]
pub struct HealthBar {
    pub healthy: i64,
    pub slow: i64,
    pub failed: i64,
    pub unknown: i64,
    pub healthy_pct: u32,
    pub slow_pct: u32,
    pub failed_pct: u32,
    pub unknown_pct: u32,
}

impl HealthBar {
    pub fn new(counts: &HashMap<String, i64>) -> Self {
        let healthy = counts.get("healthy").copied().unwrap_or(0);
        let slow = counts.get("slow").copied().unwrap_or(0);
        let failed = counts.get("failed").copied().unwrap_or(0);
        let total: i64 = counts.values().sum();
        let unknown = total - healthy - slow - failed;
        Self {
            healthy,
            slow,
            failed,
            unknown,
            healthy_pct: percent(healthy, total),
            slow_pct: percent(slow, total),
            failed_pct: percent(failed, total),
            unknown_pct: percent(unknown, total),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProgressView {
    pub running: bool,
    pub cancelling: bool,
    pub completed: String,
    pub total: String,
    pub pct: u32,
    pub healthy: usize,
    pub slow: usize,
    pub failed: usize,
    pub rate: String,
    pub eta: String,
    pub import_running: bool,
}

fn fmt_eta(left: std::time::Duration) -> String {
    let secs = left.as_secs();
    match secs {
        0..=4 => "a few seconds left".to_owned(),
        5..=59 => format!("{secs}s left"),
        60..=3599 => format!("{}m {:02}s left", secs / 60, secs % 60),
        _ => format!("{}h {:02}m left", secs / 3600, (secs % 3600) / 60),
    }
}

impl ProgressView {
    pub fn new(progress: &ProxyCheckProgress, import_running: bool) -> Self {
        Self {
            running: progress.running,
            cancelling: progress.cancelling,
            completed: fmt_count(progress.completed as i64),
            total: fmt_count(progress.total as i64),
            pct: progress.percent(),
            healthy: progress.healthy,
            slow: progress.slow,
            failed: progress.failed,
            rate: match progress.rate() {
                rate if rate >= 10.0 => format!("{} / s", fmt_count(rate.round() as i64)),
                rate if rate > 0.0 => format!("{rate:.1} / s"),
                _ => String::new(),
            },
            eta: progress.eta().map(fmt_eta).unwrap_or_default(),
            import_running,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NoticeView {
    pub level: &'static str,
    pub message: String,
    pub at: String,
}

impl From<Notice> for NoticeView {
    fn from(notice: Notice) -> Self {
        Self {
            level: notice.level,
            message: notice.message,
            at: notice.at,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CheckerView {
    pub endpoint: String,
    pub timeout_secs: u64,
    pub retries: u32,
    pub max_latency_ms: u64,
    pub expected_body: String,
    pub verify_exit_ip: bool,
    pub concurrency: usize,
}

impl From<CheckerSettings> for CheckerView {
    fn from(settings: CheckerSettings) -> Self {
        Self {
            endpoint: settings.endpoint,
            timeout_secs: settings.timeout_secs,
            retries: settings.retries,
            max_latency_ms: settings.max_latency_ms,
            expected_body: settings.expected_body,
            verify_exit_ip: settings.verify_exit_ip,
            concurrency: settings.concurrency,
        }
    }
}

/// A `<select>` option.
#[derive(Clone, Debug)]
pub struct Opt {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

impl Opt {
    pub fn new(value: impl Into<String>, label: impl Into<String>, current: &str) -> Self {
        let value = value.into();
        Self {
            selected: value == current,
            label: label.into(),
            value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }
}

#[cfg(test)]
mod hours_tests {
    use super::hours_lines;

    #[test]
    fn reads_maps_hours_array() {
        let raw = r#"[["Monday",1,[2026,11,16],[["Open 24 hours",[[0,0],[24,0]]]],0,1],["Sunday",7,[2026,11,22],[],0,1]]"#;
        assert_eq!(
            hours_lines(raw),
            ["Monday: Open 24 hours", "Sunday: closed"]
        );
        assert_eq!(hours_lines("Mon 9-5 | Tue 9-5"), ["Mon 9-5", "Tue 9-5"]);
    }
}
