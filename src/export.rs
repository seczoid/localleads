use crate::models::BusinessRecord;
use anyhow::Result;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use url::Url;

pub const OUTPUT_COLUMNS: &[&str] = &[
    "query",
    "name",
    "name_for_emails",
    "subtypes",
    "category",
    "type",
    "phone",
    "phone.phones_enricher.carrier_name",
    "phone.phones_enricher.carrier_type",
    "phone.whitepages_phones.name",
    "phone.whitepages_phones.phone_type",
    "phone.whitepages_phones.address",
    "website",
    "address",
    "street",
    "city",
    "county",
    "state",
    "state_code",
    "postal_code",
    "country",
    "country_code",
    "domain",
    "company_name",
    "company_phone",
    "company_phone.phones_enricher.carrier_name",
    "company_phone.phones_enricher.carrier_type",
    "company_phone.whitepages_phones.name",
    "company_phone.whitepages_phones.phone_type",
    "company_phone.whitepages_phones.address",
    "company_phones",
    "company_linkedin",
    "company_facebook",
    "company_instagram",
    "company_x",
    "company_youtube",
    "full_name",
    "first_name",
    "last_name",
    "title",
    "email",
    "email.emails_validator.status",
    "email.emails_validator.status_details",
    "contact_phone",
    "contact_phone.phones_enricher.carrier_name",
    "contact_phone.phones_enricher.carrier_type",
    "contact_phone.whitepages_phones.name",
    "contact_phone.whitepages_phones.phone_type",
    "contact_phone.whitepages_phones.address",
    "contact_phones",
    "contact_linkedin",
    "contact_facebook",
    "contact_instagram",
    "contact_x",
    "website_title",
    "website_description",
    "website_generator",
    "website_has_gtm",
    "website_has_fb_pixel",
    "source",
    "latitude",
    "longitude",
    "h3",
    "time_zone",
    "plus_code",
    "area_service",
    "rating",
    "reviews",
    "reviews_link",
    "reviews_tags",
    "reviews_per_score",
    "reviews_per_score_1",
    "reviews_per_score_2",
    "reviews_per_score_3",
    "reviews_per_score_4",
    "reviews_per_score_5",
    "photos_count",
    "photo",
    "street_view",
    "logo",
    "located_in",
    "located_google_id",
    "business_status",
    "working_hours",
    "working_hours_csv_compatible",
    "other_hours",
    "popular_times",
    "typical_time_spent",
    "range",
    "prices",
    "reservation_links",
    "booking_appointment_link",
    "menu_link",
    "order_links",
    "about",
    "description",
    "posts",
    "verified",
    "owner_id",
    "owner_title",
    "owner_link",
    "location_link",
    "location_reviews_link",
    "place_id",
    "google_id",
    "cid",
    "kgmid",
    "reviews_id",
    "company_insights.employees",
    "company_insights.revenue",
    "company_insights.founded_year",
    "company_insights.industry",
    "company_insights.is_public",
    "company_insights.name",
    "company_insights.country",
    "company_insights.state",
    "company_insights.city",
    "company_insights.zip",
    "company_insights.address",
    "chain_info.chain",
];

#[cfg(test)]
pub fn export_csv(
    database: &crate::db::Database,
    search: &str,
    destination: impl AsRef<Path>,
) -> Result<usize> {
    let destination = destination.as_ref();
    let temporary = temporary_path(destination);
    let records = database.export_leads(search)?;
    let mut writer = csv::Writer::from_writer(File::create(&temporary)?);
    write_csv_records(&mut writer, &records)?;
    writer.flush()?;
    fs::rename(&temporary, destination)?;
    Ok(records.len())
}

#[cfg(test)]
pub fn csv_bytes(records: &[BusinessRecord]) -> Result<Vec<u8>> {
    csv_bytes_as(records, &CsvLayout::clean())
}

pub fn csv_bytes_as(records: &[BusinessRecord], layout: &CsvLayout) -> Result<Vec<u8>> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    layout.write_header(&mut writer)?;
    for record in records {
        layout.write_record(&mut writer, record)?;
    }
    writer
        .into_inner()
        .map_err(|error| anyhow::anyhow!("finish CSV export: {error}"))
}

pub fn json_bytes(records: &[BusinessRecord]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(records)?)
}

pub fn export_records_to_disk(
    records: &[BusinessRecord],
    stem: impl AsRef<Path>,
    layout: &CsvLayout,
) -> Result<usize> {
    let stem = stem.as_ref();
    let csv_path = with_extension(stem, "csv");
    let json_path = with_extension(stem, "json");
    let csv_file = File::create(temporary_path(&csv_path))?;
    let mut writer = csv::Writer::from_writer(csv_file);
    layout.write_header(&mut writer)?;
    for record in records {
        layout.write_record(&mut writer, record)?;
    }
    writer.flush()?;
    fs::rename(temporary_path(&csv_path), &csv_path)?;
    let json_path_tmp = temporary_path(&json_path);
    serde_json::to_writer_pretty(File::create(&json_path_tmp)?, records)?;
    fs::rename(json_path_tmp, json_path)?;
    Ok(records.len())
}

#[cfg(test)]
fn write_csv_records<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    records: &[BusinessRecord],
) -> Result<()> {
    write_csv_header(writer)?;
    for record in records {
        write_csv_record(writer, record)?;
    }
    Ok(())
}

/// The clean CSV's header (every clean column).
pub fn write_csv_header<W: std::io::Write>(writer: &mut csv::Writer<W>) -> Result<()> {
    CsvLayout::clean().write_header(writer)
}

/// One business in the clean CSV.
pub fn write_csv_record<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    record: &BusinessRecord,
) -> Result<()> {
    CsvLayout::clean().write_record(writer, record)
}

pub fn write_csv_header_for<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    columns: &[&str],
) -> Result<()> {
    writer.write_record(columns.iter().copied())?;
    Ok(())
}

/// Like Outscraper: one row per email when email columns are exported, or
/// one row when there is none. Without email columns, one row a business.
pub fn write_csv_record_for<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    record: &BusinessRecord,
    columns: &[&str],
) -> Result<()> {
    let emails: Vec<&serde_json::Value> = extra_value(record, "emails")
        .and_then(serde_json::Value::as_array)
        .map(|emails| emails.iter().collect())
        .unwrap_or_default();
    let per_email = columns.iter().any(|column| column.starts_with("email"));
    let rows: Vec<Option<&serde_json::Value>> = if emails.is_empty() || !per_email {
        vec![None]
    } else {
        emails.into_iter().map(Some).collect()
    };
    for email in rows {
        let values = columns
            .iter()
            .map(|column| output_value(record, column, email))
            .collect::<Vec<_>>();
        writer.write_record(values)?;
    }
    Ok(())
}

/// Columns picked by name (a comma list), in the export's usual order.
/// Unknown names are ignored; none picked means every column.
pub fn pick_columns(list: &str) -> Vec<&'static str> {
    let wanted: std::collections::HashSet<&str> =
        list.split(',').map(str::trim).filter(|name| !name.is_empty()).collect();
    let picked: Vec<&'static str> =
        OUTPUT_COLUMNS.iter().copied().filter(|column| wanted.contains(column)).collect();
    if picked.is_empty() { OUTPUT_COLUMNS.to_vec() } else { picked }
}

/// A short starting set for most uses.
pub const ESSENTIAL_COLUMNS: &[&str] = &[
    "name",
    "category",
    "phone",
    "website",
    "email",
    "email.emails_validator.status",
    "address",
    "city",
    "state",
    "postal_code",
    "rating",
    "reviews",
    "location_link",
];

/// The column chooser's sections, each column in the first that fits.
pub fn column_groups() -> Vec<(&'static str, Vec<&'static str>)> {
    let rules: [(&str, fn(&str) -> bool); 9] = [
        ("Company insights", |c| c.starts_with("company_insights.")),
        ("Email and people", |c| {
            c.starts_with("email")
                || c.starts_with("contact_")
                || matches!(c, "full_name" | "first_name" | "last_name" | "title")
        }),
        ("Phone", |c| c.starts_with("phone") || c.starts_with("company_phone")),
        ("Website and socials", |c| {
            c.starts_with("website") || c.starts_with("company_") || c == "domain"
        }),
        ("Location", |c| {
            matches!(
                c,
                "address" | "street" | "city" | "county" | "state" | "state_code" | "postal_code"
                    | "country" | "country_code" | "latitude" | "longitude" | "h3" | "time_zone"
                    | "plus_code" | "area_service" | "located_in" | "located_google_id"
            )
        }),
        ("Reviews and photos", |c| {
            c.starts_with("reviews") || c.starts_with("photo")
                || matches!(c, "rating" | "street_view" | "logo" | "posts")
        }),
        ("Links", |c| {
            c.ends_with("_link") || c.ends_with("_links") || c.starts_with("owner_")
        }),
        ("Google ids", |c| {
            matches!(c, "place_id" | "google_id" | "cid" | "kgmid" | "source" | "query")
        }),
        ("Business", |_| true),
    ];
    let mut groups: Vec<(&'static str, Vec<&'static str>)> =
        rules.iter().map(|(name, _)| (*name, Vec::new())).collect();
    for column in OUTPUT_COLUMNS {
        if let Some(index) = rules.iter().position(|(_, fits)| fits(column)) {
            groups[index].1.push(column);
        }
    }
    // Business first, the rarely wanted company data last.
    let order = [
        "Business", "Phone", "Website and socials", "Email and people", "Location",
        "Reviews and photos", "Links", "Google ids", "Company insights",
    ];
    groups.sort_by_key(|(name, _)| order.iter().position(|o| o == name));
    groups.retain(|(_, columns)| !columns.is_empty());
    groups
}

/// Writes a JSON array one record at a time, laid out exactly like
/// `serde_json::to_vec_pretty` lays out the whole list.
pub struct JsonArrayWriter<W: std::io::Write> {
    out: W,
    empty: bool,
}

impl<W: std::io::Write> JsonArrayWriter<W> {
    pub fn new(mut out: W) -> Result<Self> {
        out.write_all(b"[")?;
        Ok(Self { out, empty: true })
    }

    pub fn push(&mut self, record: &BusinessRecord) -> Result<()> {
        let text = serde_json::to_string_pretty(record)?;
        self.out.write_all(if self.empty { b"\n  " } else { b",\n  " })?;
        // JSON strings never hold a raw newline, so this only indents layout.
        self.out.write_all(text.replace('\n', "\n  ").as_bytes())?;
        self.empty = false;
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        self.out.write_all(if self.empty { b"]" } else { b"\n]" })?;
        self.out.flush()?;
        Ok(self.out)
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }
}

/// Writes CSV and JSON for a lead query in one pass over the database.
pub fn export_query_to_disk(
    database: &crate::db::Database,
    query: &crate::db::LeadQuery,
    stem: impl AsRef<Path>,
) -> Result<usize> {
    let stem = stem.as_ref();
    let csv_path = with_extension(stem, "csv");
    let json_path = with_extension(stem, "json");
    let mut csv_writer = csv::Writer::from_writer(std::io::BufWriter::new(File::create(
        temporary_path(&csv_path),
    )?));
    let mut json_writer = JsonArrayWriter::new(std::io::BufWriter::new(File::create(
        temporary_path(&json_path),
    )?))?;
    write_csv_header(&mut csv_writer)?;
    let count = database.for_each_export_lead(query, |record| {
        write_csv_record(&mut csv_writer, &record)?;
        json_writer.push(&record)?;
        Ok(true)
    })?;
    csv_writer.flush()?;
    drop(csv_writer);
    json_writer.finish()?;
    fs::rename(temporary_path(&csv_path), &csv_path)?;
    fs::rename(temporary_path(&json_path), &json_path)?;
    Ok(count)
}

// ------------------------------------------------------------ clean CSV

/// A column of the clean CSV: one row per business, plain headers, values
/// people read (hours as text, the best email first).
pub struct CleanColumn {
    pub id: &'static str,
    pub header: &'static str,
    pub group: &'static str,
}

const fn col(id: &'static str, header: &'static str, group: &'static str) -> CleanColumn {
    CleanColumn { id, header, group }
}

pub const CLEAN_COLUMNS: &[CleanColumn] = &[
    col("name", "Name", "Business"),
    col("category", "Category", "Business"),
    col("phone", "Phone", "Contact"),
    col("other_phones", "Other phones", "Contact"),
    col("email", "Email", "Contact"),
    col("email_status", "Email status", "Contact"),
    col("other_emails", "Other emails", "Contact"),
    col("website", "Website", "Contact"),
    col("maps_url", "Google Maps URL", "Contact"),
    col("address", "Address", "Location"),
    col("city", "City", "Location"),
    col("state", "State", "Location"),
    col("postal_code", "Postal code", "Location"),
    col("country", "Country", "Location"),
    col("rating", "Rating", "Business"),
    col("reviews", "Reviews", "Business"),
    col("hours", "Hours", "Business"),
    col("claimed", "Claimed", "Business"),
    col("facebook", "Facebook", "Socials"),
    col("instagram", "Instagram", "Socials"),
    col("linkedin", "LinkedIn", "Socials"),
    col("x", "X", "Socials"),
    col("youtube", "YouTube", "Socials"),
    col("description", "Description", "Business"),
    col("search_term", "Search term", "Google"),
    col("latitude", "Latitude", "Location"),
    col("longitude", "Longitude", "Location"),
    col("place_id", "Place ID", "Google"),
];

/// Clean columns picked by id (a comma list), in the usual order. Unknown
/// ids are ignored; none picked means every column.
pub fn pick_clean_columns(list: &str) -> Vec<&'static CleanColumn> {
    let wanted: std::collections::HashSet<&str> =
        list.split(',').map(str::trim).filter(|id| !id.is_empty()).collect();
    let picked: Vec<&'static CleanColumn> =
        CLEAN_COLUMNS.iter().filter(|column| wanted.contains(column.id)).collect();
    if picked.is_empty() { CLEAN_COLUMNS.iter().collect() } else { picked }
}

/// The clean columns by section, for the column chooser.
pub fn clean_groups() -> Vec<(&'static str, Vec<&'static CleanColumn>)> {
    let order = ["Business", "Contact", "Socials", "Location", "Google"];
    order
        .iter()
        .map(|group| (*group, CLEAN_COLUMNS.iter().filter(|c| c.group == *group).collect()))
        .collect()
}

/// Which CSV to write: the clean one, or every Outscraper-style column.
#[derive(Clone)]
pub enum CsvLayout {
    Clean(Vec<&'static CleanColumn>),
    Outscraper(Vec<&'static str>),
}

impl CsvLayout {
    pub fn clean() -> Self {
        Self::Clean(CLEAN_COLUMNS.iter().collect())
    }

    /// From a style ("clean" or "outscraper") and a comma list of columns.
    pub fn pick(style: &str, columns: &str) -> Self {
        if style.trim().eq_ignore_ascii_case("outscraper") {
            Self::Outscraper(pick_columns(columns))
        } else {
            Self::Clean(pick_clean_columns(columns))
        }
    }

    pub fn write_header<W: std::io::Write>(&self, writer: &mut csv::Writer<W>) -> Result<()> {
        match self {
            Self::Clean(columns) => writer.write_record(columns.iter().map(|c| c.header))?,
            Self::Outscraper(columns) => write_csv_header_for(writer, columns)?,
        }
        Ok(())
    }

    pub fn write_record<W: std::io::Write>(
        &self,
        writer: &mut csv::Writer<W>,
        record: &BusinessRecord,
    ) -> Result<()> {
        match self {
            Self::Clean(columns) => {
                let emails = ranked_emails(record);
                writer.write_record(columns.iter().map(|c| clean_value(record, c.id, &emails)))?;
            }
            Self::Outscraper(columns) => write_csv_record_for(writer, record, columns)?,
        }
        Ok(())
    }
}

/// Usable emails, best first: verified, then risky, then unverified or
/// unchecked. Invalid ones are left out.
fn ranked_emails(record: &BusinessRecord) -> Vec<&serde_json::Value> {
    let rank = |email: &serde_json::Value| match email.get("reachable").and_then(|v| v.as_str()) {
        Some("safe") => 0,
        Some("risky") => 1,
        Some("invalid") => 9,
        _ => 2,
    };
    let mut emails: Vec<&serde_json::Value> = extra_value(record, "emails")
        .and_then(serde_json::Value::as_array)
        .map(|list| list.iter().filter(|email| email_text(Some(email), "email").contains('@')).collect())
        .unwrap_or_default();
    emails.retain(|email| rank(email) < 9);
    emails.sort_by_key(|email| rank(email));
    emails
}

fn clean_value(record: &BusinessRecord, id: &str, emails: &[&serde_json::Value]) -> String {
    let value = match id {
        "name" => record.name.clone(),
        "category" => record.category.clone().unwrap_or_default(),
        "phone" => record.phone.clone().unwrap_or_default(),
        "other_phones" => {
            let main = digits(record.phone.as_deref().unwrap_or_default());
            let mut seen = vec![main];
            let mut others = Vec::new();
            for phone in extra_text(record, "company_phones").split(',').map(str::trim) {
                let key = digits(phone);
                // The same number written differently (+1 555… vs 555…).
                if key.len() < 6 || seen.iter().any(|s| !s.is_empty() && (s.ends_with(&key) || key.ends_with(s.as_str()))) {
                    continue;
                }
                seen.push(key);
                others.push(phone.to_owned());
            }
            others.join(", ")
        }
        "email" => emails.first().map(|e| email_text(Some(e), "email")).unwrap_or_default(),
        "email_status" => match emails.first().and_then(|e| e.get("reachable")).and_then(|v| v.as_str()) {
            Some("safe") => "Verified".to_owned(),
            Some("risky") => "Risky".to_owned(),
            Some("unknown") => "Unverified".to_owned(),
            _ if !emails.is_empty() => "Not checked".to_owned(),
            _ => String::new(),
        },
        "other_emails" => emails.iter().skip(1).map(|e| email_text(Some(e), "email")).collect::<Vec<_>>().join(", "),
        "website" => record.website.clone().unwrap_or_default(),
        "address" => record.address.clone().unwrap_or_default().replace('\n', ", "),
        "city" | "state" | "postal_code" | "country" => extra_text(record, id),
        "rating" => optional_number(record.rating),
        "reviews" => optional_number(record.review_count),
        "hours" => readable_hours(record),
        "claimed" => match extra_value(record, "verified").and_then(serde_json::Value::as_bool) {
            Some(true) => "Yes".to_owned(),
            Some(false) => "No".to_owned(),
            None => String::new(),
        },
        "facebook" | "instagram" | "linkedin" | "x" | "youtube" => extra_text(record, &format!("company_{id}")),
        "description" => {
            let google = extra_text(record, "description");
            if google.trim().is_empty() { extra_text(record, "website_description") } else { google }
        }
        "maps_url" => maps_url(record),
        "search_term" => extra_text(record, "query"),
        "latitude" => optional_number(record.latitude),
        "longitude" => optional_number(record.longitude),
        "place_id" => record.place_id.clone().unwrap_or_default(),
        _ => String::new(),
    };
    // Missing values sometimes arrive as the text "null".
    let value = value.trim();
    if value == "null" { String::new() } else { value.to_owned() }
}

/// A link that opens the listing itself: by its CID when known, otherwise
/// by name and place id (Google's own search link shows "Google" as the
/// search text).
fn maps_url(record: &BusinessRecord) -> String {
    let cid = extra_text(record, "cid");
    if !cid.is_empty() && cid.chars().all(|c| c.is_ascii_digit()) {
        return format!("https://maps.google.com/?cid={cid}");
    }
    match record.place_id.as_deref().filter(|id| !id.is_empty()) {
        Some(place_id) => {
            let mut url = Url::parse("https://www.google.com/maps/search/").expect("valid base");
            url.query_pairs_mut()
                .append_pair("api", "1")
                .append_pair("query", &record.name)
                .append_pair("query_place_id", place_id);
            url.to_string()
        }
        None => record.maps_url.clone().unwrap_or_default(),
    }
}

fn digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Opening hours as "Mon–Fri 9 AM–5 PM; Sat 10 AM–2 PM; Sun Closed", from
/// Google's hours table (or the simpler day,hours|day,hours text).
fn readable_hours(record: &BusinessRecord) -> String {
    let mut week: [Option<String>; 7] = Default::default();
    // Google's table: [[[day name, day number (1 = Monday), date, [[hours text, …], …]], …], …]
    let table = match extra_value(record, "working_hours") {
        Some(serde_json::Value::String(text)) => serde_json::from_str::<serde_json::Value>(text).ok(),
        Some(value @ serde_json::Value::Array(_)) => Some(value.clone()),
        _ => None,
    };
    if let Some(days) = table.as_ref().and_then(|t| t.get(0)).and_then(serde_json::Value::as_array) {
        for day in days {
            let Some(number) = day.get(1).and_then(serde_json::Value::as_u64) else { continue };
            let spans: Vec<String> = day
                .get(3)
                .and_then(serde_json::Value::as_array)
                .map(|spans| spans.iter().filter_map(|s| s.get(0).and_then(|t| t.as_str()).map(tidy)).collect())
                .unwrap_or_default();
            if (1..=7).contains(&number) && !spans.is_empty() {
                week[number as usize - 1] = Some(spans.join(", "));
            }
        }
    }
    if week.iter().all(Option::is_none) {
        // "Wednesday,9 AM-6 PM|Thursday,…"
        for part in extra_text(record, "working_hours_csv_compatible").split('|') {
            let Some((day, hours)) = part.split_once(',') else { continue };
            let day = day.trim().to_lowercase();
            if let Some(index) = DAYS.iter().position(|d| day.starts_with(&d.to_lowercase())) {
                week[index] = Some(tidy(hours));
            }
        }
    }
    if week.iter().all(Option::is_none) {
        return record.hours.as_deref().filter(|h| !h.trim_start().starts_with('[')).map(tidy).unwrap_or_default();
    }
    if week.iter().all(|day| day.as_deref() == Some("Open 24 hours")) {
        return "Open 24 hours".to_owned();
    }
    // Runs of days with the same hours.
    let mut parts = Vec::new();
    let mut start = 0;
    while start < 7 {
        let mut end = start;
        while end + 1 < 7 && week[end + 1] == week[start] {
            end += 1;
        }
        if let Some(hours) = &week[start] {
            let days = match end - start {
                0 => DAYS[start].to_owned(),
                1 => format!("{}, {}", DAYS[start], DAYS[end]),
                _ => format!("{}–{}", DAYS[start], DAYS[end]),
            };
            parts.push(format!("{days} {hours}"));
        }
        start = end + 1;
    }
    parts.join("; ")
}

/// Plain spaces and dashes in Google's hours text.
fn tidy(text: &str) -> String {
    text.replace(['\u{202f}', '\u{a0}'], " ").replace(" - ", "–").trim().to_owned()
}

fn email_text(email: Option<&serde_json::Value>, key: &str) -> String {
    email
        .and_then(|email| email.get(key))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn output_value(record: &BusinessRecord, column: &str, email: Option<&serde_json::Value>) -> String {
    match column {
        "query" => extra_text(record, "query"),
        "name" => record.name.clone(),
        "name_for_emails" => extra_or(record, column, Some(&record.name)),
        "email" => email_text(email, "email"),
        "email.emails_validator.status" => email_text(email, "status"),
        "email.emails_validator.status_details" => email_text(email, "status_details"),
        "full_name" => email_text(email, "full_name"),
        "first_name" | "last_name" => {
            let full = email_text(email, "full_name");
            let mut parts = full.split_whitespace();
            let first = parts.next().unwrap_or_default().to_owned();
            if column == "first_name" {
                first
            } else {
                parts.collect::<Vec<_>>().join(" ")
            }
        }
        "title" => email_text(email, "title"),
        // Where the contact was found, as Outscraper reports it.
        "source" => email_text(email, "source"),
        "company_name" => record.name.clone(),
        "company_phone" => extra_text(record, "company_phones")
            .split(", ")
            .next()
            .unwrap_or_default()
            .to_owned(),
        "company_phones" | "company_linkedin" | "company_facebook" | "company_instagram"
        | "company_x" | "company_youtube" | "website_title" | "website_description"
        | "website_generator" | "website_has_gtm" | "website_has_fb_pixel" => {
            extra_text(record, column)
        }
        "subtypes" => extra_joined(record, "subtypes"),
        "category" | "type" => record.category.clone().unwrap_or_default(),
        "phone" => record.phone.clone().unwrap_or_default(),
        "website" => record.website.clone().unwrap_or_default(),
        "address" => record.address.clone().unwrap_or_default(),
        "street" | "city" | "county" | "state" | "state_code" | "postal_code" | "country"
        | "country_code" => extra_text(record, column),
        "domain" => record
            .website
            .as_deref()
            .and_then(|value| Url::parse(value).ok())
            .and_then(|value| value.host_str().map(ToOwned::to_owned))
            .unwrap_or_default(),
        "latitude" => optional_number(record.latitude),
        "longitude" => optional_number(record.longitude),
        "time_zone" | "h3" | "plus_code" | "area_service" => extra_text(record, column),
        "rating" => optional_number(record.rating),
        "reviews" => optional_number(record.review_count),
        "reviews_link" | "reviews_tags" | "reviews_per_score" => extra_text(record, column),
        "reviews_per_score_1"
        | "reviews_per_score_2"
        | "reviews_per_score_3"
        | "reviews_per_score_4"
        | "reviews_per_score_5" => extra_text(record, column),
        "photos_count" | "photo" | "street_view" | "logo" | "located_in" | "located_google_id" => {
            extra_text(record, column)
        }
        "business_status" => extra_or(record, column, record.status.as_deref()),
        "working_hours" => extra_or(record, column, record.hours.as_deref()),
        "working_hours_csv_compatible" => extra_text(record, column),
        "other_hours"
        | "popular_times"
        | "typical_time_spent"
        | "range"
        | "prices"
        | "reservation_links"
        | "booking_appointment_link"
        | "menu_link"
        | "order_links"
        | "about"
        | "description"
        | "posts"
        | "verified"
        | "owner_id"
        | "owner_title"
        | "owner_link" => extra_text(record, column),
        "location_link" => record.maps_url.clone().unwrap_or_default(),
        "location_reviews_link" => extra_text(record, "reviews_link"),
        "place_id" => record.place_id.clone().unwrap_or_default(),
        "google_id" | "cid" | "kgmid" | "reviews_id" => extra_text(record, column),
        _ => String::new(),
    }
}

fn extra_value<'a>(record: &'a BusinessRecord, key: &str) -> Option<&'a serde_json::Value> {
    record.extra.get(key).filter(|value| !value.is_null())
}

fn extra_text(record: &BusinessRecord, key: &str) -> String {
    match extra_value(record, key) {
        Some(serde_json::Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn extra_or(record: &BusinessRecord, key: &str, fallback: Option<&str>) -> String {
    let value = extra_text(record, key);
    if value.is_empty() {
        fallback.unwrap_or_default().to_owned()
    } else {
        value
    }
}

fn extra_joined(record: &BusinessRecord, key: &str) -> String {
    match extra_value(record, key) {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => extra_text(record, key),
    }
}

fn optional_number<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn temporary_path(destination: &Path) -> PathBuf {
    destination.with_extension(format!(
        "{}.tmp",
        destination
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("export")
    ))
}

fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut output = path.to_path_buf();
    output.set_extension(extension);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::{BusinessRecord, SearchConfig};
    use tempfile::tempdir;

    #[test]
    fn csv_export_is_created_atomically() -> Result<()> {
        let database = Database::in_memory()?;
        database.create_job("job-1", &SearchConfig::default())?;
        database.insert_lead(
            "job-1",
            "test",
            "",
            &BusinessRecord {
                name: "Test Lead".to_owned(),
                category: None,
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
                place_id: Some("place-1".to_owned()),
                source: "test".to_owned(),
                extra: serde_json::json!({"query": "coffee"}),
            },
        )?;
        let directory = tempdir()?;
        let path = directory.path().join("leads.csv");
        assert_eq!(export_csv(&database, "", &path)?, 1);
        assert!(path.is_file());
        assert!(!path.with_extension("csv.tmp").exists());
        Ok(())
    }

    fn sample(name: &str) -> BusinessRecord {
        BusinessRecord {
            name: name.to_owned(),
            category: Some("Cafe".to_owned()),
            address: Some("1 Main St\nSuite 2".to_owned()),
            phone: None,
            website: None,
            latitude: Some(1.5),
            longitude: None,
            rating: None,
            review_count: Some(12),
            status: None,
            hours: None,
            maps_url: None,
            place_id: Some(format!("place-{name}")),
            source: "test".to_owned(),
            extra: serde_json::json!({"query": "coffee", "emails": [{"email": "a@b.test"}]}),
        }
    }

    #[test]
    fn streamed_exports_match_whole_file_exports() -> Result<()> {
        for records in [vec![], vec![sample("One")], vec![sample("One"), sample("Two")]] {
            let mut json = JsonArrayWriter::new(Vec::new())?;
            let mut csv_writer = csv::Writer::from_writer(Vec::new());
            write_csv_header(&mut csv_writer)?;
            for record in &records {
                json.push(record)?;
                write_csv_record(&mut csv_writer, record)?;
            }
            assert_eq!(json.finish()?, json_bytes(&records)?);
            assert_eq!(csv_writer.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?, csv_bytes(&records)?);
        }
        Ok(())
    }

    #[test]
    fn clean_csv_is_one_readable_row_per_business() -> Result<()> {
        let hours = r#"[[["Monday",1,[2026,11,16],[["7 AM–5 PM",[[7],[17]]]],0,1],["Tuesday",2,[2026,11,17],[["7 AM–5 PM",[[7],[17]]]],0,1],["Wednesday",3,[2026,11,18],[["7 AM–5 PM",[[7],[17]]]],0,1],["Thursday",4,[2026,11,19],[["7 AM–5 PM",[[7],[17]]]],0,1],["Friday",5,[2026,11,20],[["7 AM–5 PM",[[7],[17]]]],0,1],["Saturday",6,[2026,11,21],[["8 AM–12 PM",[[8],[12]]],["1–4 PM",[[13],[16]]]],0,1],["Sunday",7,[2026,11,22],[["Closed"]],0,2]]]"#;
        let mut record = sample("Joe's Plumbing");
        record.phone = Some("+1 555-010-2000".to_owned());
        record.extra = serde_json::json!({
            "query": "plumber",
            "working_hours": hours,
            "verified": false,
            "company_phones": "+15550102000, (555) 010-3000",
            "company_facebook": "https://facebook.com/joes",
            "emails": [
                {"email": "info@joes.test", "reachable": "unknown"},
                {"email": "bad@joes.test", "reachable": "invalid"},
                {"email": "joe@joes.test", "reachable": "safe"},
            ],
        });
        let text = String::from_utf8(csv_bytes(&[record])?)?;
        let mut reader = csv::Reader::from_reader(text.as_bytes());
        let headers = reader.headers()?.clone();
        let rows: Vec<csv::StringRecord> = reader.records().collect::<Result<_, _>>()?;
        assert_eq!(rows.len(), 1, "one row per business");
        let get = |name: &str| rows[0][headers.iter().position(|h| h == name).unwrap()].to_owned();
        assert_eq!(get("Email"), "joe@joes.test");
        assert_eq!(get("Email status"), "Verified");
        assert_eq!(get("Other emails"), "info@joes.test");
        assert_eq!(get("Other phones"), "(555) 010-3000");
        assert_eq!(get("Hours"), "Mon–Fri 7 AM–5 PM; Sat 8 AM–12 PM, 1–4 PM; Sun Closed");
        assert_eq!(get("Claimed"), "No");
        assert_eq!(get("Facebook"), "https://facebook.com/joes");
        assert_eq!(get("Address"), "1 Main St, Suite 2");
        assert_eq!(get("Search term"), "plumber");
        assert_eq!(get("Google Maps URL"), "https://www.google.com/maps/search/?api=1&query=Joe%27s+Plumbing&query_place_id=place-Joe%27s+Plumbing");
        assert_eq!(headers.len(), CLEAN_COLUMNS.len());
        Ok(())
    }

    #[test]
    fn picked_layouts_keep_their_columns() {
        let CsvLayout::Clean(columns) = CsvLayout::pick("", "phone,name,nope") else { panic!() };
        assert_eq!(columns.iter().map(|c| c.id).collect::<Vec<_>>(), ["name", "phone"]);
        let CsvLayout::Outscraper(columns) = CsvLayout::pick("outscraper", "") else { panic!() };
        assert_eq!(columns.len(), OUTPUT_COLUMNS.len());
    }
}
