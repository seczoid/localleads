//! Enriches one lead: Google's place page, then the website (emails,
//! socials, phones, site details). Builds with the `verify` feature also
//! check each email with its mail server.
//! Everything found goes into the lead's extra fields under Outscraper's
//! column names; the best email and its verdict also go into columns.

use super::place::{self, PlaceRef};
use super::site::{SiteReader, SiteReport};
#[cfg(feature = "verify")]
use super::verify::Verifier;
use crate::db::{EnrichSummary, EnrichTarget};
use crate::models::SearchConfig;
use crate::proxy::ProxyPool;
use crate::scraper::HttpClients;
#[cfg(feature = "verify")]
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Emails checked per lead at most (the site's best first); the rest keep
/// their page details.
#[cfg(feature = "verify")]
const MAX_CHECKED: usize = 5;
/// Mail-server checks at once for one lead.
#[cfg(feature = "verify")]
const CHECKS_AT_ONCE: usize = 3;
/// Mail-server checks at once across all leads of a run.
#[cfg(feature = "verify")]
const CHECKS_IN_ALL: usize = 48;

/// How sure we are that mail to this address arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    Safe,
    Risky,
    #[default]
    Unknown,
    Invalid,
}

impl Reach {
    #[cfg(feature = "verify")]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Risky => "risky",
            Self::Unknown => "unknown",
            Self::Invalid => "invalid",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "safe" => Self::Safe,
            "risky" => Self::Risky,
            "invalid" => Self::Invalid,
            _ => Self::Unknown,
        }
    }
}


pub struct Enricher {
    site: Option<SiteReader>,
    #[cfg(feature = "verify")]
    verifier: Option<Verifier>,
    /// Bounds mail-server checks across every lead being checked.
    #[cfg(feature = "verify")]
    check_slots: Semaphore,
    /// Bounds Google place pages to the search's own concurrency, so many
    /// leads read at once don't flood its proxies.
    place_slots: Semaphore,
    place_details: bool,
    pool: ProxyPool,
    clients: HttpClients,
    require_proxy: bool,
    allow_direct: bool,
    max_retries: u32,
    language: String,
    region: String,
}

/// One address in the lead's `emails` list.
#[derive(Clone, Debug, Serialize)]
struct EmailEntry {
    email: String,
    /// How it was written on the page: mailto, cloudflare, structured_data, …
    found: String,
    /// The page it was on.
    source: String,
    full_name: Option<String>,
    title: Option<String>,
    /// The mail-server check's verdict; empty when the email wasn't checked.
    #[serde(skip_serializing_if = "String::is_empty")]
    reachable: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    status_details: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    check: Option<Value>,
}

/// What reading a lead found (place page and website), before its emails
/// are checked with their mail servers.
pub struct Reading {
    patch: Map<String, Value>,
    /// `None` when there was no website to read.
    entries: Option<Vec<EmailEntry>>,
    status: String,
}

impl Reading {
    /// Everything to save now; emails carry no verdict yet.
    pub fn result(&self) -> (Map<String, Value>, EnrichSummary) {
        let mut patch = self.patch.clone();
        let mut summary = EnrichSummary::default();
        if let Some(entries) = &self.entries {
            patch.insert("emails".into(), json!(entries));
            summary = summarize(entries);
        }
        summary.status.clone_from(&self.status);
        (patch, summary)
    }
}

impl Enricher {
    pub async fn new(config: &SearchConfig, pool: ProxyPool) -> anyhow::Result<Self> {
        // Websites are read directly; proxies are for Google (and SMTP).
        let site = if config.find_emails { Some(SiteReader::new()?) } else { None };
        #[cfg(feature = "verify")]
        let verifier = if config.find_emails && config.verify_emails {
            Some(Verifier::new(&pool.healthy(), true).await)
        } else {
            None
        };
        let place_slots = usize::try_from(config.concurrency).unwrap_or(1).clamp(1, 64);
        Ok(Self {
            site,
            #[cfg(feature = "verify")]
            verifier,
            #[cfg(feature = "verify")]
            check_slots: Semaphore::new(CHECKS_IN_ALL),
            place_slots: Semaphore::new(place_slots),
            place_details: config.place_details,
            pool,
            clients: HttpClients::default(),
            require_proxy: config.require_proxy,
            allow_direct: config.allow_direct,
            max_retries: config.max_retries.min(4),
            language: crate::scraper::language(config).to_owned(),
            region: crate::scraper::region(config).unwrap_or("us").to_owned(),
        })
    }

    /// A line for the search log on how emails will be checked.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.site.is_some() {
            parts.push("reading websites for emails".to_owned());
        }
        #[cfg(feature = "verify")]
        match &self.verifier {
            Some(verifier) if verifier.via_proxies() => {
                parts.push(format!(
                    "verifying emails through {} SOCKS5 {} that reach mail servers",
                    verifier.proxy_count(),
                    if verifier.proxy_count() == 1 { "proxy" } else { "proxies" }
                ));
            }
            Some(verifier) if verifier.smtp_available() => {
                parts.push("verifying emails directly (no SOCKS5 proxy reaches mail servers on port 25)".to_owned());
            }
            Some(_) => parts.push(
                "checking email domains only: port 25 is blocked here and there are no SOCKS5 proxies"
                    .to_owned(),
            ),
            None => {}
        }
        if self.place_details {
            parts.push("opening Google place pages".to_owned());
        }
        format!("Enrichment: {}", parts.join(", "))
    }

    /// Reads and checks one lead in one go.
    pub async fn enrich(&self, target: &EnrichTarget) -> (Map<String, Value>, EnrichSummary) {
        let reading = self.read(target).await;
        let (mut patch, summary) = reading.result();
        if !self.needs_check(&reading) {
            return (patch, summary);
        }
        let (checked, summary) = self.check(reading).await;
        patch.extend(checked);
        (patch, summary)
    }

    /// Reads the Google place page and the website at the same time. The
    /// emails found are not checked yet; see [`Self::check`].
    pub async fn read(&self, target: &EnrichTarget) -> Reading {
        let place = async {
            if !self.place_details {
                return None;
            }
            let (Some(google_id), Some(latitude), Some(longitude)) =
                (target.google_id.as_deref(), target.latitude, target.longitude)
            else {
                return None;
            };
            let place = PlaceRef {
                google_id,
                name: &target.name,
                address: target.address.as_deref().unwrap_or(""),
                latitude,
                longitude,
                language: &self.language,
                region: &self.region,
            };
            let _slot = self.place_slots.acquire().await.ok()?;
            Some(self.place(&place).await)
        };
        let site = async {
            match (&self.site, target.website.as_deref()) {
                (Some(reader), Some(website)) => Some(reader.read(website).await),
                _ => None,
            }
        };
        let (place, site) = tokio::join!(place, site);

        let mut patch = Map::new();
        let mut errors = Map::new();
        let mut attempted = 0;
        let mut failed = 0;
        if let Some(place) = place {
            attempted += 1;
            match place {
                Ok(fields) => patch.extend(fields),
                Err(error) => {
                    failed += 1;
                    errors.insert("place".into(), json!(error.to_string()));
                }
            }
        }
        let mut entries = None;
        if let Some(report) = site {
            attempted += 1;
            if report.pages.is_empty() {
                failed += 1;
            }
            if let Some(error) = &report.error {
                errors.insert("website".into(), json!(error));
            }
            site_fields(&mut patch, &report);
            entries = Some(found_emails(&report));
        }
        patch.insert(
            "enrich_errors".into(),
            if errors.is_empty() { Value::Null } else { Value::Object(errors) },
        );
        let status = if attempted > 0 && failed == attempted { "failed" } else { "done" };
        Reading {
            patch,
            entries,
            status: status.to_owned(),
        }
    }

    /// Whether the lead's emails still need their mail servers asked.
    pub fn needs_check(&self, reading: &Reading) -> bool {
        #[cfg(feature = "verify")]
        if self.verifier.is_some() {
            return reading.entries.as_ref().is_some_and(|entries| !entries.is_empty());
        }
        let _ = reading;
        false
    }

    /// Checks a read lead's emails; returns just the `emails` field and the
    /// email columns.
    pub async fn check(&self, reading: Reading) -> (Map<String, Value>, EnrichSummary) {
        let Reading { entries, status, .. } = reading;
        let mut patch = Map::new();
        let mut summary = EnrichSummary::default();
        if let Some(entries) = entries {
            #[cfg(feature = "verify")]
            let entries = self.verify(entries).await;
            summary = summarize(&entries);
            patch.insert("emails".into(), json!(entries));
        }
        summary.status = status;
        (patch, summary)
    }

    async fn place(&self, place: &PlaceRef<'_>) -> anyhow::Result<Map<String, Value>> {
        let mut last = anyhow::anyhow!("no request was made");
        for attempt in 0..=self.max_retries {
            let proxy = self.pool.select();
            if proxy.is_none() && self.require_proxy && !self.allow_direct {
                anyhow::bail!("no healthy proxy for the Google place page");
            }
            let http = self.clients.get(proxy.as_ref())?;
            match place::fetch(&http, place).await {
                Ok(fields) => return Ok(fields),
                Err(error) => {
                    last = error;
                    if let Some(proxy) = proxy {
                        self.pool.mark_failure(&proxy.id);
                    }
                    tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt + 1))).await;
                }
            }
        }
        Err(last)
    }

}

/// The website's emails in the site's order, with the contact each belongs to.
fn found_emails(report: &SiteReport) -> Vec<EmailEntry> {
    let entries = report.emails.iter().map(|found| {
        let contact = report
            .contacts
            .iter()
            .find(|contact| contact.email.as_deref() == Some(found.email.as_str()));
        EmailEntry {
            email: found.email.clone(),
            found: found.found.clone(),
            source: found.page.clone(),
            full_name: contact.map(|contact| contact.name.clone()),
            title: contact.and_then(|contact| contact.title.clone()),
            reachable: String::new(),
            status: String::new(),
            status_details: String::new(),
            check: None,
        }
    });
    entries.collect()
}

impl Enricher {
    #[cfg(feature = "verify")]
    async fn verify(&self, entries: Vec<EmailEntry>) -> Vec<EmailEntry> {
        let Some(verifier) = &self.verifier else {
            return entries;
        };
        let checked = futures::stream::iter(entries.into_iter().enumerate())
            .map(|(index, mut entry)| async move {
                if index < MAX_CHECKED
                    && let Ok(_slot) = self.check_slots.acquire().await
                {
                    let check = verifier.check(&entry.email).await;
                    entry.reachable = check.reachable.clone();
                    entry.status = check.status.clone();
                    entry.status_details = check.status_details.clone();
                    entry.check = serde_json::to_value(&check).ok();
                }
                (index, entry)
            })
            .buffer_unordered(CHECKS_AT_ONCE)
            .collect::<Vec<_>>()
            .await;
        let mut checked = checked;
        checked.sort_by_key(|(index, _)| *index);
        checked.into_iter().map(|(_, entry)| entry).collect()
    }
}

/// The best usable address: confirmed first, then by the site's own order
/// (company domain, people before roles). Invalid ones never win.
fn summarize(entries: &[EmailEntry]) -> EnrichSummary {
    let usable: Vec<&EmailEntry> = entries
        .iter()
        .filter(|entry| Reach::parse(&entry.reachable) != Reach::Invalid)
        .collect();
    let best = usable
        .iter()
        .enumerate()
        .min_by_key(|(index, entry)| {
            let tier = match Reach::parse(&entry.reachable) {
                Reach::Safe => 0,
                _ => 1,
            };
            (tier, *index)
        })
        .map(|(_, entry)| *entry);
    EnrichSummary {
        email: best.map(|entry| entry.email.clone()),
        email_status: best.map(|entry| entry.status.clone()).filter(|value| !value.is_empty()),
        email_reach: best.map(|entry| entry.reachable.clone()).filter(|value| !value.is_empty()),
        email_count: i64::try_from(usable.len()).unwrap_or(i64::MAX),
        status: String::new(),
    }
}

/// The website's fields under Outscraper's names. Every key is written, so
/// a re-run clears what the site no longer shows.
fn site_fields(patch: &mut Map<String, Value>, report: &SiteReport) {
    let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::String);
    let read = !report.pages.is_empty();
    patch.insert("website_title".into(), text(&report.title));
    patch.insert("website_description".into(), text(&report.description));
    patch.insert("website_generator".into(), text(&report.generator));
    patch.insert("website_has_gtm".into(), if read { json!(report.has_gtm) } else { Value::Null });
    patch.insert("website_has_fb_pixel".into(), if read { json!(report.has_fb_pixel) } else { Value::Null });
    patch.insert("company_linkedin".into(), text(&report.socials.linkedin));
    patch.insert("company_facebook".into(), text(&report.socials.facebook));
    patch.insert("company_instagram".into(), text(&report.socials.instagram));
    patch.insert("company_x".into(), text(&report.socials.x));
    patch.insert("company_youtube".into(), text(&report.socials.youtube));
    patch.insert("company_tiktok".into(), text(&report.socials.tiktok));
    patch.insert(
        "company_phones".into(),
        if report.phones.is_empty() { Value::Null } else { json!(report.phones.join(", ")) },
    );
    patch.insert(
        "contacts".into(),
        if report.contacts.is_empty() { Value::Null } else { json!(report.contacts) },
    );
    patch.insert(
        "website_pages".into(),
        if read { json!(report.pages) } else { Value::Null },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(email: &str, reachable: &str) -> EmailEntry {
        EmailEntry {
            email: email.to_owned(),
            found: "mailto".to_owned(),
            source: String::new(),
            full_name: None,
            title: None,
            reachable: reachable.to_owned(),
            status: "RECEIVING".to_owned(),
            status_details: String::new(),
            check: None,
        }
    }

    #[test]
    fn best_email_prefers_confirmed_then_site_order() {
        let entries = [
            entry("info@acme.com", "risky"),
            entry("bad@acme.com", "invalid"),
            entry("acme@gmail.com", "safe"),
        ];
        let summary = summarize(&entries);
        assert_eq!(summary.email.as_deref(), Some("acme@gmail.com"));
        assert_eq!(summary.email_count, 2);
        let unchecked = [entry("info@acme.com", "unknown"), entry("jo@acme.com", "unknown")];
        assert_eq!(summarize(&unchecked).email.as_deref(), Some("info@acme.com"));
        assert_eq!(summarize(&[entry("x@acme.com", "invalid")]).email, None);
    }
}
