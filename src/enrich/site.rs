//! Reads a business's website with webclaw: the home page, then the pages
//! most likely to list contacts (contact, about, team, imprint, …), then
//! common contact paths if no address turned up yet.

use super::emails::{self, Found};
use futures::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use url::Url;
use webclaw_fetch::{FetchClient, FetchConfig};

/// Pages read per site, the home page included.
const MAX_PAGES: usize = 8;
/// Pages read at once for one site.
const PAGES_AT_ONCE: usize = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SiteEmail {
    pub email: String,
    pub found: String,
    /// The page it was on.
    pub page: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Socials {
    pub linkedin: Option<String>,
    pub facebook: Option<String>,
    pub instagram: Option<String>,
    pub x: Option<String>,
    pub youtube: Option<String>,
    pub tiktok: Option<String>,
}

/// A person named in the site's structured data.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub title: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SiteReport {
    /// Where the home page ended up after redirects.
    pub url: String,
    pub pages: Vec<String>,
    pub emails: Vec<SiteEmail>,
    pub phones: Vec<String>,
    pub socials: Socials,
    pub contacts: Vec<Contact>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub generator: Option<String>,
    pub has_gtm: bool,
    pub has_fb_pixel: bool,
    pub error: Option<String>,
}

pub struct SiteReader {
    client: Arc<FetchClient>,
}

impl SiteReader {
    /// One client for a whole run. Websites are read directly, never
    /// through the search's proxies (those are for Google and SMTP).
    pub fn new() -> anyhow::Result<Self> {
        let config = FetchConfig {
            timeout: Duration::from_secs(15),
            total_timeout: Some(Duration::from_secs(30)),
            ..FetchConfig::default()
        };
        let client = FetchClient::new(config).map_err(|error| anyhow::anyhow!("set up website reader: {error}"))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    pub async fn read(&self, website: &str) -> SiteReport {
        let Some(start) = normalize(website) else {
            return SiteReport {
                error: Some("not a web address".to_owned()),
                ..SiteReport::default()
            };
        };
        let mut report = SiteReport::default();
        let mut collector = Collector::default();

        let home = match self.client.fetch(start.as_str()).await {
            Ok(page) if page.status < 400 => page,
            Ok(page) => {
                report.error = Some(format!("home page answered {}", page.status));
                return report;
            }
            Err(error) => {
                report.error = Some(error.to_string());
                return report;
            }
        };
        report.url = home.url.clone();
        let base = Url::parse(&home.url).unwrap_or(start);
        let site = emails::base_domain(base.host_str().unwrap_or_default());
        read_meta(&home.html, &mut report);
        collector.page(&home.url, &home.html);
        let mut seen: BTreeSet<String> = BTreeSet::from([canonical(&home.url)]);

        // Contact-ish pages linked from the home page, best first.
        let mut ranked: Vec<(u32, String)> = webclaw_core::page_anchors(&home.html, &home.url)
            .into_iter()
            .filter_map(|anchor| {
                let url = Url::parse(&anchor.href).ok()?;
                if emails::base_domain(url.host_str()?) != site || is_asset(&url) {
                    return None;
                }
                let score = page_score(url.path(), &anchor.text);
                (score > 0).then(|| (score, url.to_string()))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
        let mut queue: Vec<String> = Vec::new();
        for (_, url) in ranked {
            if queue.len() + 1 >= MAX_PAGES {
                break;
            }
            if seen.insert(canonical(&url)) {
                queue.push(url);
            }
        }
        self.read_pages(&queue, &mut collector).await;

        // Nothing yet: try the usual contact paths the menu may hide.
        if collector.emails.is_empty() {
            let guesses: Vec<String> = ["/contact", "/contact-us", "/about", "/about-us", "/impressum"]
                .iter()
                .filter_map(|path| base.join(path).ok())
                .map(|url| url.to_string())
                .filter(|url| seen.insert(canonical(url)))
                .take(MAX_PAGES.saturating_sub(1 + queue.len()).max(2))
                .collect();
            self.read_pages(&guesses, &mut collector).await;
        }

        report.pages = collector.pages;
        report.phones = collector.phones.into_iter().collect();
        report.socials = collector.socials;
        report.contacts = collector.contacts;
        report.emails = rank_emails(collector.emails, &site);
        report
    }

    async fn read_pages(&self, urls: &[String], collector: &mut Collector) {
        let pages: Vec<_> = futures::stream::iter(urls.iter().cloned())
            .map(|url| {
                let client = Arc::clone(&self.client);
                async move { client.fetch(&url).await.ok().filter(|page| page.status < 400) }
            })
            .buffer_unordered(PAGES_AT_ONCE)
            .collect()
            .await;
        for page in pages.into_iter().flatten() {
            collector.page(&page.url, &page.html);
        }
    }
}

#[derive(Default)]
struct Collector {
    pages: Vec<String>,
    /// email → (how, first page).
    emails: BTreeMap<String, (Found, String)>,
    phones: BTreeSet<String>,
    socials: Socials,
    contacts: Vec<Contact>,
}

impl Collector {
    fn page(&mut self, url: &str, html: &str) {
        self.pages.push(url.to_owned());
        for found in emails::extract(html) {
            self.emails
                .entry(found.email)
                .and_modify(|current| {
                    if found.found < current.0 {
                        current.0 = found.found;
                    }
                })
                .or_insert((found.found, url.to_owned()));
        }
        self.phones.extend(tel_links(html));
        social_links(html, &mut self.socials);
        for contact in structured_contacts(html) {
            if !self.contacts.iter().any(|known| known.name == contact.name) {
                self.contacts.push(contact);
            }
        }
    }
}

/// The company's own addresses first, then free mailboxes (gmail…), then
/// other domains (often the web agency); within each, people before shared
/// inboxes, then by how plainly the page wrote them.
fn rank_emails(found: BTreeMap<String, (Found, String)>, site: &str) -> Vec<SiteEmail> {
    let mut emails: Vec<(u8, bool, Found, SiteEmail)> = found
        .into_iter()
        .map(|(email, (how, page))| {
            let domain = email.rsplit('@').next().unwrap_or_default();
            let tier = if emails::base_domain(domain) == site {
                0
            } else if FREE_MAIL.contains(&domain) {
                1
            } else {
                2
            };
            let role = emails::is_role(&email);
            let found = serde_json::to_value(how)
                .ok()
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .unwrap_or_default();
            (tier, role, how, SiteEmail { email, found, page })
        })
        .collect();
    emails.sort_by(|a, b| (a.0, a.1, a.2, &a.3.email).cmp(&(b.0, b.1, b.2, &b.3.email)));
    emails.into_iter().map(|(_, _, _, email)| email).collect()
}

pub const FREE_MAIL: &[&str] = &[
    "gmail.com", "googlemail.com", "yahoo.com", "ymail.com", "hotmail.com", "outlook.com",
    "live.com", "msn.com", "aol.com", "icloud.com", "me.com", "mac.com", "comcast.net",
    "att.net", "sbcglobal.net", "verizon.net", "bellsouth.net", "charter.net", "cox.net",
    "protonmail.com", "proton.me", "gmx.com", "mail.com", "zoho.com", "yandex.com",
];

/// How likely a page is to list contacts, from its path and link text.
fn page_score(path: &str, text: &str) -> u32 {
    let haystack = format!("{} {}", path.to_lowercase(), text.to_lowercase());
    const WORDS: &[(&str, u32)] = &[
        ("contact", 100), ("kontakt", 100), ("contacto", 100), ("impressum", 90),
        ("imprint", 90), ("get-in-touch", 90), ("get in touch", 90), ("reach-us", 80),
        ("team", 70), ("staff", 70), ("our-people", 70), ("leadership", 60), ("about", 60),
        ("who-we-are", 60), ("meet", 50), ("location", 40), ("office", 40), ("quote", 35),
        ("estimate", 35), ("legal", 30), ("privacy", 25), ("support", 25), ("help", 15),
        ("careers", 10), ("jobs", 10),
    ];
    let mut score = WORDS
        .iter()
        .filter(|(word, _)| haystack.contains(word))
        .map(|(_, score)| *score)
        .max()
        .unwrap_or(0);
    // Prefer short paths: "/contact" over "/blog/how-to-contact-a-roofer".
    if score > 0 {
        let depth = path.trim_matches('/').split('/').count() as u32;
        score = score.saturating_sub(depth.saturating_sub(1) * 15);
    }
    score
}

fn is_asset(url: &Url) -> bool {
    let path = url.path().to_lowercase();
    [".pdf", ".jpg", ".jpeg", ".png", ".gif", ".svg", ".webp", ".zip", ".mp4", ".doc", ".docx", ".xls", ".xlsx"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
}

fn normalize(website: &str) -> Option<Url> {
    let website = website.trim();
    let with_scheme = if website.contains("://") {
        website.to_owned()
    } else {
        format!("https://{website}")
    };
    let url = Url::parse(&with_scheme).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

/// One key per page: no scheme, "www.", trailing slash or fragment.
fn canonical(url: &str) -> String {
    let Ok(mut url) = Url::parse(url) else {
        return url.to_owned();
    };
    url.set_fragment(None);
    let host = url.host_str().unwrap_or_default().trim_start_matches("www.").to_owned();
    format!("{host}{}{}", url.path().trim_end_matches('/'), url.query().map(|q| format!("?{q}")).unwrap_or_default())
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

fn read_meta(html: &str, report: &mut SiteReport) {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static META: OnceLock<Regex> = OnceLock::new();
    let title = TITLE.get_or_init(|| re(r"(?is)<title[^>]*>(.*?)</title>"));
    let meta = META.get_or_init(|| re(r#"(?is)<meta\s[^>]*>"#));
    report.title = title
        .captures(html)
        .map(|caps| tidy(&emails::decode_entities(&caps[1])))
        .filter(|title| !title.is_empty());
    let mut generators = Vec::new();
    for tag in meta.find_iter(html) {
        let tag = tag.as_str();
        let name = attribute(tag, "name").or_else(|| attribute(tag, "property")).unwrap_or_default().to_lowercase();
        let Some(content) = attribute(tag, "content").map(|value| tidy(&emails::decode_entities(&value))) else {
            continue;
        };
        match name.as_str() {
            "description" if report.description.is_none() => report.description = Some(content),
            "og:description" if report.description.is_none() => report.description = Some(content),
            "og:title" if report.title.is_none() => report.title = Some(content),
            "generator" => {
                // "Powered by Slider Revolution 6.8.1 - responsive, …" → the
                // name and version only.
                let short = content.split(" - ").next().unwrap_or(&content).trim().to_owned();
                if !short.is_empty() && !generators.contains(&short) {
                    generators.push(short);
                }
            }
            _ => {}
        }
    }
    if !generators.is_empty() {
        report.generator = Some(generators.join("; "));
    }
    let lower = html.to_lowercase();
    report.has_gtm = lower.contains("googletagmanager.com/gtm.js") || lower.contains("googletagmanager.com/ns.html");
    report.has_fb_pixel = lower.contains("connect.facebook.net") && lower.contains("fbevents.js")
        || lower.contains("fbq('init'")
        || lower.contains("fbq(\"init\"");
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    static CACHE: OnceLock<Regex> = OnceLock::new();
    let re = CACHE.get_or_init(|| re(r#"(?is)([a-z:_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#));
    re.captures_iter(tag)
        .find(|caps| caps[1].eq_ignore_ascii_case(name))
        .and_then(|caps| caps.get(2).or_else(|| caps.get(3)).or_else(|| caps.get(4)))
        .map(|m| m.as_str().to_owned())
}

fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Phone numbers from `tel:` links, digits and a leading + only.
fn tel_links(html: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| re(r#"(?i)href\s*=\s*["']tel:([^"']+)["']"#));
    re.captures_iter(html)
        .filter_map(|caps| {
            let raw = emails::decode_entities(&caps[1]);
            let raw = raw.replace("%20", " ").replace("%2B", "+").replace("%2b", "+");
            let mut digits: String = raw.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
            if digits.matches('+').count() > 1 || digits[1.min(digits.len())..].contains('+') {
                digits.retain(|c| c != '+');
            }
            let count = digits.chars().filter(char::is_ascii_digit).count();
            (7..=15).contains(&count).then_some(digits)
        })
        .collect()
}

/// Company profiles, not share buttons or the platforms' own pages.
fn social_links(html: &str, socials: &mut Socials) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        re(r#"(?i)https?://(?:[a-z]{2,3}\.)?(?:www\.)?(facebook\.com|fb\.com|instagram\.com|linkedin\.com|twitter\.com|x\.com|youtube\.com|tiktok\.com)/[^\s"'<>\\)]+"#)
    });
    for found in re.find_iter(html) {
        let url = found.as_str().trim_end_matches(['/', '.', ',']).to_owned();
        let Ok(parsed) = Url::parse(&url) else { continue };
        let host = parsed.host_str().unwrap_or_default().to_lowercase();
        let path = parsed.path().to_lowercase();
        let first = path.trim_start_matches('/').split('/').next().unwrap_or_default().to_owned();
        const NOT_PROFILES: &[&str] = &[
            "sharer", "sharer.php", "share", "share.php", "intent", "dialog", "plugins", "tr",
            "home.php", "login", "signup", "policies", "privacy", "legal", "help", "about",
            "terms", "hashtag", "search", "watch", "embed", "shareArticle", "sharearticle",
            "p", "reel", "explore", "i", "events", "groups", "permalink.php", "photo.php",
            "story.php", "v", "results", "feed", "oauth", "developers", "business", "2008",
            "fbml", "xfbml",
        ];
        if first.is_empty() || NOT_PROFILES.contains(&first.as_str()) || path.ends_with(".js") {
            continue;
        }
        let slot = if host.contains("facebook") || host == "fb.com" {
            &mut socials.facebook
        } else if host.contains("instagram") {
            &mut socials.instagram
        } else if host.contains("linkedin") {
            if !matches!(first.as_str(), "company" | "in" | "school" | "showcase") {
                continue;
            }
            &mut socials.linkedin
        } else if host.contains("twitter") || host == "x.com" || host == "www.x.com" {
            &mut socials.x
        } else if host.contains("youtube") {
            if first == "embed" {
                continue;
            }
            &mut socials.youtube
        } else {
            &mut socials.tiktok
        };
        if slot.is_none() {
            let clean = format!("https://{}{}", host.trim_start_matches("m."), parsed.path().trim_end_matches('/'));
            *slot = Some(clean);
        }
    }
}

/// People in JSON-LD (`Person`, `employee`, `founder`).
fn structured_contacts(html: &str) -> Vec<Contact> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| re(r#"(?is)<script[^>]*application/ld\+json[^>]*>(.*?)</script>"#));
    let mut contacts = Vec::new();
    for caps in re.captures_iter(html) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(caps[1].trim()) else {
            continue;
        };
        collect_people(&value, &mut contacts, 0);
    }
    contacts
}

fn collect_people(value: &serde_json::Value, out: &mut Vec<Contact>, depth: usize) {
    if depth > 6 || out.len() >= 20 {
        return;
    }
    match value {
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_people(item, out, depth + 1)),
        serde_json::Value::Object(map) => {
            let is_person = map.get("@type").is_some_and(|kind| {
                kind.as_str() == Some("Person")
                    || kind.as_array().is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("Person")))
            });
            let text = |key: &str| map.get(key).and_then(|v| v.as_str()).map(tidy).filter(|v| !v.is_empty());
            if is_person && let Some(name) = text("name") {
                out.push(Contact {
                    name,
                    title: text("jobTitle"),
                    email: text("email").and_then(|email| emails::clean(email.trim_start_matches("mailto:"))),
                    phone: text("telephone"),
                });
            }
            for child in map.values() {
                collect_people(child, out, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_pages_rank_first() {
        assert!(page_score("/contact-us", "Contact") > page_score("/about", "About us"));
        assert!(page_score("/about", "") > page_score("/privacy-policy", ""));
        assert_eq!(page_score("/services/patios", "Patios"), 0);
        assert!(page_score("/contact", "") > page_score("/blog/2024/how-to-contact", ""));
    }

    #[test]
    fn meta_socials_and_phones() {
        let html = r#"<html><head><title>Acme &amp; Sons | Roofing</title>
            <meta name="description" content="Roofs  done right.">
            <meta name="generator" content="WordPress 6.5">
            <script src="https://www.googletagmanager.com/gtm.js?id=GTM-1"></script>
            <script>fbq('init', '123');</script></head><body>
            <a href="https://www.facebook.com/sharer/sharer.php?u=x">share</a>
            <a href="https://www.facebook.com/acmeroofing/">fb</a>
            <a href="https://www.linkedin.com/company/acme-roofing">li</a>
            <a href="https://twitter.com/intent/tweet">t</a>
            <a href="https://x.com/acmeroof">x</a>
            <a href="tel:+1 (502) 555-0100">call</a></body></html>"#;
        let mut report = SiteReport::default();
        read_meta(html, &mut report);
        assert_eq!(report.title.as_deref(), Some("Acme & Sons | Roofing"));
        assert_eq!(report.description.as_deref(), Some("Roofs done right."));
        assert_eq!(report.generator.as_deref(), Some("WordPress 6.5"));
        assert!(report.has_gtm && report.has_fb_pixel);
        let mut socials = Socials::default();
        social_links(html, &mut socials);
        assert_eq!(socials.facebook.as_deref(), Some("https://www.facebook.com/acmeroofing"));
        assert_eq!(socials.linkedin.as_deref(), Some("https://www.linkedin.com/company/acme-roofing"));
        assert_eq!(socials.x.as_deref(), Some("https://x.com/acmeroof"));
        assert_eq!(tel_links(html), vec!["+15025550100"]);
    }

    #[test]
    fn own_domain_people_first() {
        let mut found = BTreeMap::new();
        for (email, how) in [
            ("info@acme.com", Found::Text),
            ("joe@acme.com", Found::Text),
            ("acmeroof@gmail.com", Found::Mailto),
            ("hello@agency.com", Found::Mailto),
        ] {
            found.insert(email.to_owned(), (how, "https://acme.com".to_owned()));
        }
        let ranked: Vec<String> = rank_emails(found, "acme.com").into_iter().map(|e| e.email).collect();
        assert_eq!(ranked, vec!["joe@acme.com", "info@acme.com", "acmeroof@gmail.com", "hello@agency.com"]);
    }

    #[test]
    fn people_from_json_ld() {
        let html = r#"<script type="application/ld+json">{"@type":"LocalBusiness","employee":[{"@type":"Person","name":"Jonathan Seawright","jobTitle":"Owner","email":"mailto:j.seawright@acme.com"}]}</script>"#;
        let people = structured_contacts(html);
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].title.as_deref(), Some("Owner"));
        assert_eq!(people[0].email.as_deref(), Some("j.seawright@acme.com"));
    }
}
