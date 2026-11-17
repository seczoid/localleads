//! Finds email addresses in a web page without guessing: every address
//! comes from text that spells it out, after undoing the usual ways sites
//! hide them.
//!
//! Passes, each over the raw HTML of one page:
//! - `mailto:` links (percent-encoded, with `?subject=` and several
//!   addresses separated by commas);
//! - Cloudflare email protection (`data-cfemail` and
//!   `/cdn-cgi/l/email-protection#…`), XOR-decoded;
//! - the page text after decoding HTML entities (`&#64;`, `&commat;`, …),
//!   `@` / `\x40` escapes and percent-encoding, with tags that split an
//!   address (`info<span>@</span>site.com`) joined back;
//! - spelled-out forms: `info [at] site [dot] com`, `(at)`, `{at}`,
//!   `info AT site DOT com` and `info at site dot com`;
//! - reversed addresses written backwards for CSS `direction: rtl`.
//!
//! Then it drops what is not a mailbox: file names (`logo@2x.png`),
//! placeholders (`you@example.com`, `filler@godaddy.com`), tracking ids
//! (`a1b2…@sentry.io`) and the like.

use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// How an address was written on the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Found {
    Mailto,
    Cloudflare,
    StructuredData,
    Text,
    Obfuscated,
}

/// One address and where on the page it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageEmail {
    pub email: String,
    pub found: Found,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

/// An address: RFC 5322 dot-atom local part, a domain of LDH labels and an
/// alphabetic top-level domain.
fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        re(r"(?i)[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:xn--[a-z0-9-]{2,59}|[a-z]{2,24})")
    })
}

/// Every address on a page, best evidence first, each once.
pub fn extract(html: &str) -> Vec<PageEmail> {
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    let mut add = |raw: &str, how: Found| {
        if let Some(email) = clean(raw) {
            found
                .entry(email)
                .and_modify(|current| *current = (*current).min(how))
                .or_insert(how);
        }
    };

    for address in mailto_addresses(html) {
        add(&address, Found::Mailto);
    }
    for address in cloudflare_addresses(html) {
        add(&address, Found::Cloudflare);
    }
    for address in structured_addresses(html) {
        add(&address, Found::StructuredData);
    }
    let text = decode_text(html);
    for candidate in email_re().find_iter(&text) {
        add(candidate.as_str(), Found::Text);
    }
    let visible = visible_text(&text);
    for candidate in email_re().find_iter(&visible) {
        add(candidate.as_str(), Found::Text);
    }
    for address in spelled_out(&visible) {
        add(&address, Found::Obfuscated);
    }
    for address in reversed(&visible) {
        add(&address, Found::Obfuscated);
    }

    drop_clipped(&mut found);
    let mut emails: Vec<PageEmail> = found
        .into_iter()
        .map(|(email, found)| PageEmail { email, found })
        .collect();
    emails.sort_by(|a, b| a.found.cmp(&b.found).then_with(|| a.email.cmp(&b.email)));
    emails
}

// ------------------------------------------------------------- passes

fn mailto_addresses(html: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| re(r#"(?i)mailto:([^"'<>\s]*)"#));
    let mut out = Vec::new();
    for capture in re.captures_iter(html) {
        let target = decode_entities(&capture[1]);
        let target = percent_decode(&target);
        let target = target.split('?').next().unwrap_or_default();
        for part in target.split([',', ';']) {
            if let Some(found) = email_re().find(part.trim()) {
                out.push(found.as_str().to_owned());
            }
        }
    }
    out
}

/// Cloudflare replaces addresses with a hex string whose first byte is an
/// XOR key for the rest.
fn cloudflare_addresses(html: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        re(r#"(?i)(?:data-cfemail=["']?|/cdn-cgi/l/email-protection#)([0-9a-f]{4,})"#)
    });
    re.captures_iter(html)
        .filter_map(|capture| cloudflare_decode(&capture[1]))
        .collect()
}

pub fn cloudflare_decode(hex: &str) -> Option<String> {
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|index| u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let (key, rest) = bytes.split_first()?;
    let decoded: Vec<u8> = rest.iter().map(|byte| byte ^ key).collect();
    let text = String::from_utf8(decoded).ok()?;
    let text = percent_decode(&text);
    email_re().find(&text).map(|found| found.as_str().to_owned())
}

/// `"email": "…"` in JSON-LD and `itemprop="email"` microdata.
fn structured_addresses(html: &str) -> Vec<String> {
    static JSON: OnceLock<Regex> = OnceLock::new();
    static ITEMPROP: OnceLock<Regex> = OnceLock::new();
    let json = JSON.get_or_init(|| re(r#"(?i)"email"\s*:\s*"([^"]{3,254})""#));
    let itemprop = ITEMPROP.get_or_init(|| {
        re(r#"(?is)itemprop=["']?email["']?[^>]*>([^<]{3,254})<|itemprop=["']?email["']?[^>]*content=["']([^"']{3,254})["']"#)
    });
    let mut out = Vec::new();
    for capture in json.captures_iter(html) {
        let value = decode_text(&capture[1]);
        if let Some(found) = email_re().find(&value) {
            out.push(found.as_str().to_owned());
        }
    }
    for capture in itemprop.captures_iter(html) {
        let value = capture.get(1).or_else(|| capture.get(2)).map_or("", |m| m.as_str());
        let value = decode_text(value);
        if let Some(found) = email_re().find(&value) {
            out.push(found.as_str().to_owned());
        }
    }
    out
}

/// `info [at] site [dot] com` and friends.
fn spelled_out(text: &str) -> Vec<String> {
    static BRACKETED: OnceLock<(Regex, Regex)> = OnceLock::new();
    static WORDS: OnceLock<Regex> = OnceLock::new();
    let (at, dot) = BRACKETED.get_or_init(|| {
        (
            // Brackets in any case; bare words only in capitals ("INFO AT
            // ACME DOT COM"), since "contact us at acme.com" is a sentence.
            re(r"\s*[\[\(\{]\s*(?i:at|@)\s*[\]\)\}]\s*|\s+AT\s+"),
            re(r"\s*[\[\(\{]\s*(?i:dot|\.)\s*[\]\)\}]\s*|\s+DOT\s+"),
        )
    });
    let mut out = Vec::new();
    let with_at = at.replace_all(text, "@");
    let joined = dot.replace_all(&with_at, ".");
    if joined != text {
        out.extend(
            email_re()
                .find_iter(&joined)
                .map(|found| found.as_str().to_owned()),
        );
    }
    // Lower-case words only when both "at" and "dot" are there, so ordinary
    // sentences ("meet us at noon") don't turn into addresses.
    let words = WORDS.get_or_init(|| {
        re(r"(?i)\b([a-z0-9][a-z0-9._%+-]{0,63})\s+at\s+([a-z0-9][a-z0-9-]{0,62}(?:\s+dot\s+[a-z0-9][a-z0-9-]{0,62})+)\b")
    });
    for capture in words.captures_iter(text) {
        let domain = capture[2].split_whitespace().filter(|part| !part.eq_ignore_ascii_case("dot")).collect::<Vec<_>>().join(".");
        let address = format!("{}@{domain}", &capture[1]);
        if let Some(found) = email_re().find(&address)
            && found.as_str().len() == address.len()
        {
            out.push(address);
        }
    }
    out
}

/// `moc.etis@ofni` shown reversed with CSS. Only taken when the reversed
/// form ends in a known common top-level domain.
fn reversed(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        re(r"(?i)\b(?:moc|ten|gro|ude|vog|oc|su|ku\.oc|ac|ua\.moc|ofni|zib|oi|ed|rf|ln|ti|se)\.[a-z0-9.-]+@[a-z0-9._%+-]+\b")
    });
    re.find_iter(text)
        .map(|found| found.as_str().chars().rev().collect::<String>())
        .filter(|address| email_re().find(address).is_some_and(|m| m.as_str().len() == address.len()))
        .collect()
}

// ------------------------------------------------------------- decoding

/// Entities, `@` / `\x40` escapes and percent-encoding undone.
fn decode_text(text: &str) -> String {
    let text = decode_escapes(text);
    let text = decode_entities(&text);
    // `%40` only: decoding everything would glue `%20` spaces to words.
    text.replace("%40", "@").replace("%2E", ".").replace("%2e", ".")
}

fn decode_escapes(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| re(r"\\u([0-9a-fA-F]{4})|\\x([0-9a-fA-F]{2})|\\/"));
    re.replace_all(text, |caps: &regex::Captures| {
        let hex = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str());
        match hex.and_then(|hex| u32::from_str_radix(hex, 16).ok()).and_then(char::from_u32) {
            Some(character) => character.to_string(),
            None => "/".to_owned(),
        }
    })
    .into_owned()
}

pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| re(r"&(#[xX][0-9a-fA-F]{1,6}|#[0-9]{1,7}|[a-zA-Z]{2,8});?"));
    re.replace_all(text, |caps: &regex::Captures| {
        let entity = &caps[1];
        let decoded = if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(decimal) = entity.strip_prefix('#') {
            decimal.parse().ok().and_then(char::from_u32)
        } else {
            match entity {
                "commat" => Some('@'),
                "period" => Some('.'),
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "hyphen" | "dash" => Some('-'),
                "lowbar" => Some('_'),
                "plus" => Some('+'),
                _ => None,
            }
        };
        decoded.map_or_else(|| caps[0].to_owned(), |c| c.to_string())
    })
    .into_owned()
}

fn percent_decode(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", text.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| text.to_owned())
}

/// Page text: scripts and styles kept (addresses hide in them too), comments
/// dropped, tags that split an address around `@` or `.` joined, the rest of
/// the tags turned into spaces.
fn visible_text(html: &str) -> String {
    static COMMENTS: OnceLock<Regex> = OnceLock::new();
    static JOIN: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    let comments = COMMENTS.get_or_init(|| re(r"(?s)<!--.*?-->"));
    // "@" next to a tag on either side, "." only between two tags
    // (`acme<span>.</span>com`), so a sentence ending in ".</p>" isn't glued
    // to the next one.
    let join = JOIN.get_or_init(|| {
        re(r"(?:\s*<[^<>]{0,200}>\s*)+(@)(?:\s*<[^<>]{0,200}>\s*)*|(@)(?:\s*<[^<>]{0,200}>\s*)+|(?:<[^<>]{0,200}>)+\s*(\.)\s*(?:<[^<>]{0,200}>)+")
    });
    static INLINE: OnceLock<Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| re(r"<[^<>]{0,2000}>"));
    // Inline tags don't break a word on screen: `<a>d</a><span>elawarerg@`
    // reads "delawarerg@". Block tags (div, p, br, li, td, …) do.
    let inline = INLINE.get_or_init(|| {
        re(r"(?i)([\w.%+-]*)((?:<\s*/?\s*(?:a|abbr|b|bdi|bdo|cite|code|data|dfn|em|font|i|kbd|mark|q|s|samp|small|span|strong|sub|sup|time|u|var|wbr)\b[^<>]{0,500}>)+)(\w?)")
    });
    let text = comments.replace_all(html, "");
    let text = join.replace_all(&text, |caps: &regex::Captures| {
        (1..=3)
            .find_map(|group| caps.get(group))
            .map_or_else(String::new, |m| m.as_str().to_owned())
    });
    let text = inline.replace_all(&text, |caps: &regex::Captures| {
        let left = caps.get(1).map_or("", |m| m.as_str());
        let run = caps.get(2).map_or("", |m| m.as_str());
        let right = caps.get(3).map_or("", |m| m.as_str());
        // A label in its own element (`<span>Email</span><span>info@…`)
        // is laid out apart from the address, so it stays a separate word.
        // So does a number (`882-2712</a><a href="mailto:…">hello@`), and
        // anything in a different link: one link ending and the next one
        // starting are two things on screen.
        let glue = !left.is_empty()
            && !right.is_empty()
            && !is_label(left)
            && left.chars().any(|c| c.is_alphabetic())
            && !links_apart(run);
        format!("{left}{}{right}", if glue { "" } else { " " })
    });
    tags.replace_all(&text, " ").into_owned()
}

/// Whether a run of inline tags closes one link and opens another.
fn links_apart(run: &str) -> bool {
    static CLOSE: OnceLock<Regex> = OnceLock::new();
    static OPEN: OnceLock<Regex> = OnceLock::new();
    let close = CLOSE.get_or_init(|| re(r"(?i)<\s*/\s*a\s*>"));
    let open = OPEN.get_or_init(|| re(r"(?i)<\s*a\b"));
    close.find(run).is_some_and(|closed| open.is_match(&run[closed.end()..]))
}

/// Drops text-only addresses that are the tail of another address cut
/// mid-word (`elawarerg@aol.com` from `<a>d</a><span>elawarerg@aol.com`
/// when `delawarerg@aol.com` was found too). A tail after a separator
/// (`info@` next to `sales.info@`) is a different mailbox and stays.
fn drop_clipped(found: &mut BTreeMap<String, Found>) {
    let clipped: Vec<String> = found
        .iter()
        .filter(|(_, how)| matches!(how, Found::Text | Found::Obfuscated))
        .filter_map(|(email, _)| {
            let (local, domain) = email.split_once('@')?;
            found.keys().any(|other| {
                let Some((other_local, other_domain)) = other.split_once('@') else {
                    return false;
                };
                other_domain == domain
                    && other_local.len() > local.len()
                    && other_local.ends_with(local)
                    && other_local[..other_local.len() - local.len()]
                        .ends_with(|c: char| c.is_ascii_alphanumeric())
                    && local.starts_with(|c: char| c.is_ascii_alphanumeric())
            })
            .then(|| email.clone())
        })
        .collect();
    // And text-only addresses that are another found address with a number
    // stuck in front: a phone number before the link (`882-2712hello@`), or
    // a React key in the page's script data (`"0-info@…"`).
    let padded: Vec<String> = found
        .iter()
        .filter(|(_, how)| matches!(how, Found::Text | Found::Obfuscated))
        .filter_map(|(email, _)| {
            let (local, domain) = email.split_once('@')?;
            found.keys().any(|other| {
                let Some((other_local, other_domain)) = other.split_once('@') else {
                    return false;
                };
                other_domain == domain
                    && local.len() > other_local.len()
                    && local.ends_with(other_local)
                    && local[..local.len() - other_local.len()]
                        .chars()
                        .all(|c| c.is_ascii_digit() || "-_.+".contains(c))
            })
            .then(|| email.clone())
        })
        .collect();
    for email in clipped.into_iter().chain(padded) {
        found.remove(&email);
    }
}

/// Words sites put in front of an address in a separate element.
fn is_label(word: &str) -> bool {
    const LABELS: &[&str] = &[
        "email", "e-mail", "mail", "mailto", "contact", "contactus", "us", "at", "address", "write",
        "send", "reach", "message", "inquiries", "enquiries", "info", "sales", "support", "office",
        "phone", "tel", "fax", "call", "text", "or", "and", "to", "via", "by",
    ];
    let word = word.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
    LABELS.contains(&word.as_str())
}

// ------------------------------------------------------------- filtering

/// File types that look like top-level domains in `name@2x.png`.
const ASSET_SUFFIXES: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "avif", "ico", "bmp", "tif", "tiff", "css", "js",
    "mjs", "map", "json", "xml", "mp4", "webm", "mov", "mp3", "wav", "woff", "woff2", "ttf", "otf",
    "eot", "pdf", "zip", "php", "html", "htm", "aspx", "jsp", "txt", "heic",
];

/// Domains that only ever appear as examples, placeholders or tooling.
const JUNK_DOMAINS: &[&str] = &[
    "example.com", "example.org", "example.net", "example.edu", "domain.com", "domain.tld",
    "yourdomain.com", "yoursite.com", "yourcompany.com", "yourbusiness.com", "company.com",
    "website.com", "mysite.com", "email.tld", "address.com", "test.com", "sentry.io",
    "sentry-next.wixpress.com", "sentry.wixpress.com", "wixpress.com", "ingest.sentry.io",
    "o.sentry.io", "sentry.zendesk.com", "mailinator.com", "localhost.localdomain",
];

/// Exact addresses site builders put in their templates.
const JUNK_ADDRESSES: &[&str] = &[
    "filler@godaddy.com", "name@email.com", "email@email.com", "your@email.com",
    "youremail@email.com", "you@email.com", "user@email.com", "info@email.com", "test@test.com",
    "email@address.com", "name@company.com", "john@doe.com", "johndoe@gmail.com",
    "john.doe@gmail.com", "janedoe@gmail.com", "username@gmail.com", "yourname@gmail.com",
    "your.name@gmail.com", "someone@gmail.com", "example@gmail.com", "email@gmail.com",
    "abc@gmail.com", "xyz@gmail.com", "noreply@wordpress.com",
];

/// A word glued to the end of an address by the page layout:
/// `info@acme.com.Hours` or `info@acme.comCall` → `info@acme.com`.
fn unglue(raw: &str) -> &str {
    let Some(at) = raw.rfind('@') else { return raw };
    let domain = &raw[at + 1..];
    let lower_before = |end: usize| domain[..end].chars().any(|c| c.is_ascii_lowercase());
    // A capitalised last label after lower-case ones.
    if let Some(dot) = domain.rfind('.')
        && domain[dot + 1..].starts_with(|c: char| c.is_ascii_uppercase())
        && lower_before(dot)
        && domain[..dot].contains('.')
    {
        return &raw[..at + 1 + dot];
    }
    // A capital letter inside the top-level domain: "comCall".
    if let Some(dot) = domain.rfind('.') {
        let tld = &domain[dot + 1..];
        if let Some(hump) = tld.char_indices().skip(1).find(|(_, c)| c.is_ascii_uppercase()).map(|(i, _)| i)
            && tld[..hump].chars().all(|c| c.is_ascii_lowercase())
        {
            return &raw[..at + 1 + dot + 1 + hump];
        }
    }
    raw
}

/// Lower-cased, trimmed and checked; `None` when it isn't a real mailbox.
pub fn clean(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_matches(|c: char| ".-_'\"`".contains(c));
    let raw = raw.strip_prefix("mailto:").unwrap_or(raw);
    let raw = unglue(raw);
    let found = email_re().find(raw)?;
    if found.start() != 0 || found.end() != raw.len() {
        return None;
    }
    let email = raw.to_lowercase();
    let (local, domain) = email.rsplit_once('@')?;
    if local.is_empty() || local.len() > 64 || domain.len() > 253 || email.len() > 254 {
        return None;
    }
    let tld = domain.rsplit('.').next()?;
    if ASSET_SUFFIXES.contains(&tld) {
        return None;
    }
    if JUNK_ADDRESSES.contains(&email.as_str()) {
        return None;
    }
    if JUNK_DOMAINS
        .iter()
        .any(|junk| domain == *junk || domain.ends_with(&format!(".{junk}")))
    {
        return None;
    }
    // Retina image names: `icon@2x`, `logo@3x.webp`.
    if domain.starts_with(|c: char| c.is_ascii_digit()) && domain.split('.').next()?.ends_with('x') {
        return None;
    }
    // Tracking and message ids: long hex or hash-like local parts.
    let hexish = local.chars().filter(char::is_ascii_hexdigit).count();
    if local.len() >= 16 && hexish == local.len() {
        return None;
    }
    if local.len() >= 24 && local.chars().filter(char::is_ascii_digit).count() * 3 > local.len() {
        return None;
    }
    Some(email)
}

// ------------------------------------------------------------- roles

/// Shared inboxes (`info@`, `sales@`), not a person.
pub fn is_role(email: &str) -> bool {
    const ROLES: &[&str] = &[
        "info", "contact", "contactus", "contact-us", "hello", "hi", "sales", "support", "help",
        "office", "admin", "administrator", "billing", "accounts", "accounting", "service",
        "services", "customerservice", "customer.service", "team", "mail", "email", "enquiries",
        "enquiry", "inquiries", "inquiry", "booking", "bookings", "reservations", "orders",
        "marketing", "media", "press", "pr", "jobs", "careers", "hr", "recruiting", "webmaster",
        "noreply", "no-reply", "donotreply", "do-not-reply", "postmaster", "hostmaster", "abuse",
        "privacy", "legal", "estimates", "estimate", "quotes", "quote", "frontdesk", "reception",
        "general", "appointments", "scheduling", "dispatch", "operations", "ops", "feedback",
        "newsletter", "studio", "shop", "store",
    ];
    email
        .split('@')
        .next()
        .is_some_and(|local| ROLES.contains(&local))
}

/// The registrable part of a host, good enough to tell a company's own
/// domain from a web agency's: the last two labels, or three for
/// `co.uk`-style suffixes.
pub fn base_domain(host: &str) -> String {
    let host = host.trim_start_matches("www.").to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    let keep = match labels.as_slice() {
        [.., second, last] if last.len() == 2 && matches!(*second, "co" | "com" | "org" | "net" | "gov" | "edu" | "ac" | "or" | "ne") => 3,
        _ => 2,
    };
    labels[labels.len().saturating_sub(keep)..].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emails(html: &str) -> Vec<String> {
        extract(html).into_iter().map(|found| found.email).collect()
    }

    #[test]
    fn plain_and_mailto() {
        let html = r#"<a href="mailto:Info%40Acme.com?subject=Hi">Mail us</a> or sales@acme.com."#;
        assert_eq!(emails(html), vec!["info@acme.com", "sales@acme.com"]);
        assert_eq!(extract(html)[0].found, Found::Mailto);
        let several = r#"<a href="mailto:a@x.com,b@x.com">x</a>"#;
        assert_eq!(emails(several), vec!["a@x.com", "b@x.com"]);
    }

    #[test]
    fn entities_and_escapes() {
        assert_eq!(emails("bob&#64;shop&#46;com"), vec!["bob@shop.com"]);
        assert_eq!(emails("bob&#x40;shop.com"), vec!["bob@shop.com"]);
        assert_eq!(emails("bob&commat;shop&period;com"), vec!["bob@shop.com"]);
        assert_eq!(emails(r#"var e = "bob@shop.com";"#), vec!["bob@shop.com"]);
        assert_eq!(emails(r#"{"mail":"bob\x40shop.com"}"#), vec!["bob@shop.com"]);
    }

    #[test]
    fn cloudflare_protected() {
        // "info@acme.com" with key 0x42.
        let key = 0x42u8;
        let hex: String = std::iter::once(key)
            .chain("info@acme.com".bytes().map(|b| b ^ key))
            .map(|b| format!("{b:02x}"))
            .collect();
        let html = format!(r#"<a class="__cf_email__" data-cfemail="{hex}">[email&#160;protected]</a>"#);
        assert_eq!(emails(&html), vec!["info@acme.com"]);
        let link = format!(r#"<a href="/cdn-cgi/l/email-protection#{hex}">mail</a>"#);
        assert_eq!(emails(&link), vec!["info@acme.com"]);
    }

    #[test]
    fn split_by_tags() {
        assert_eq!(emails("info<span>@</span>acme<span>.</span>com"), vec!["info@acme.com"]);
        assert_eq!(emails("info<!-- x -->@acme.com"), vec!["info@acme.com"]);
        assert_eq!(emails("<b>Email:</b> <i>jo@acme.com</i>"), vec!["jo@acme.com"]);
    }

    #[test]
    fn spelled_out_forms() {
        assert_eq!(emails("info [at] acme [dot] com"), vec!["info@acme.com"]);
        assert_eq!(emails("info(at)acme(dot)co(dot)uk"), vec!["info@acme.co.uk"]);
        assert_eq!(emails("info {at} acme {dot} com"), vec!["info@acme.com"]);
        assert_eq!(emails("info AT acme DOT com"), vec!["info@acme.com"]);
        assert_eq!(emails("write to info at acme dot com today"), vec!["info@acme.com"]);
        assert!(emails("meet us at noon. Done").is_empty());
    }

    #[test]
    fn reversed_for_css() {
        assert_eq!(emails(r#"<span style="unicode-bidi:bidi-override;direction:rtl">moc.emca@ofni</span>"#), vec!["info@acme.com"]);
    }

    #[test]
    fn structured_data() {
        let html = r#"<script type="application/ld+json">{"@type":"LocalBusiness","email":"mailto:office@acme.com"}</script>"#;
        assert_eq!(emails(html), vec!["office@acme.com"]);
        let micro = r#"<span itemprop="email">desk@acme.com</span>"#;
        assert_eq!(emails(micro), vec!["desk@acme.com"]);
    }

    #[test]
    fn junk_is_dropped() {
        let html = r#"<img src="logo@2x.png"> <img src="/img/hero@3x.webp">
            you@example.com filler@godaddy.com
            https://8f2a3b4c5d6e7f8091a2b3c4d5e6f708@o12345.ingest.sentry.io/1
            <script src="lib@1.2.3/dist.js"></script> real@acme.com"#;
        assert_eq!(emails(html), vec!["real@acme.com"]);
    }

    #[test]
    fn glued_words_are_cut() {
        assert_eq!(emails("<p>Mail info@acme.com.</p><p>Hours: 9-5</p>"), vec!["info@acme.com"]);
        assert_eq!(clean("info@acme.com.Hours").as_deref(), Some("info@acme.com"));
        assert_eq!(clean("info@acme.comCall").as_deref(), Some("info@acme.com"));
        assert!(emails("contact us at acme.com for more").is_empty());
    }

    #[test]
    fn roles_and_domains() {
        assert!(is_role("info@acme.com"));
        assert!(!is_role("grady@acme.com"));
        assert_eq!(base_domain("www.shop.acme.co.uk"), "acme.co.uk");
        assert_eq!(base_domain("WWW.Acme.com"), "acme.com");
    }

    #[test]
    fn inline_tags_inside_a_word_join_it() {
        let html = r#"<a href="mailto:delawarerg@aol.com">d</a><span style="color:#fff">elawarerg@aol.com</span>"#;
        assert_eq!(emails(html), vec!["delawarerg@aol.com"]);
        let html = "<p><b>jo</b><b>hn@acme.com</b></p>";
        assert_eq!(emails(html), vec!["john@acme.com"]);
    }

    #[test]
    fn labels_in_their_own_element_stay_apart() {
        let html = r#"<span class="label">Email</span><span>info@acme.com</span>"#;
        assert_eq!(emails(html), vec!["info@acme.com"]);
        let html = "<div>Call us</div><div>sales@acme.com</div>";
        assert_eq!(emails(html), vec!["sales@acme.com"]);
    }


    #[test]
    fn separate_mailboxes_sharing_a_tail_both_stay() {
        let html = "<p>sales.info@acme.com</p><p>info@acme.com</p>";
        assert_eq!(emails(html), vec!["info@acme.com", "sales.info@acme.com"]);
    }

    #[test]
    fn a_phone_link_next_to_an_email_link_stays_apart() {
        // spekless.com: the phone link ends right where the email link starts.
        let html = r#"<a href="sms:+15718822712">Text <!-- -->(571) 882-2712</a><a href="mailto:hello@spekless.com">hello@spekless.com</a>"#;
        assert_eq!(emails(html), vec!["hello@spekless.com"]);
        // Without the mailto, the visible text alone still keeps them apart.
        let html = r#"<span>Call (571) 882-2712</span><span>hello@spekless.com</span>"#;
        assert_eq!(emails(html), vec!["hello@spekless.com"]);
        let html = r#"<a href="/a">Jo</a><a href="/b">hn@acme.com</a>"#;
        assert!(!emails(html).contains(&"john@acme.com".to_owned()));
    }

    #[test]
    fn script_keys_in_front_of_an_address_are_dropped() {
        // sunnybutler.com: a React key in the page's flight data.
        let html = r#"<a href="mailto:info@sunnybutler.com">info@sunnybutler.com</a><script>self.__next_f.push([1,"["$","$1","0-info@sunnybutler.com",{}]"])</script>"#;
        assert_eq!(emails(html), vec!["info@sunnybutler.com"]);
    }

}
