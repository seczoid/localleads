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

pub fn csv_bytes(records: &[BusinessRecord]) -> Result<Vec<u8>> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    write_csv_records(&mut writer, records)?;
    writer
        .into_inner()
        .map_err(|error| anyhow::anyhow!("finish CSV export: {error}"))
}

pub fn json_bytes(records: &[BusinessRecord]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(records)?)
}

pub fn export_records_to_disk(records: &[BusinessRecord], stem: impl AsRef<Path>) -> Result<usize> {
    let stem = stem.as_ref();
    let csv_path = with_extension(stem, "csv");
    let json_path = with_extension(stem, "json");
    let csv_file = File::create(temporary_path(&csv_path))?;
    let mut writer = csv::Writer::from_writer(csv_file);
    write_csv_records(&mut writer, records)?;
    writer.flush()?;
    fs::rename(temporary_path(&csv_path), &csv_path)?;
    let json_path_tmp = temporary_path(&json_path);
    serde_json::to_writer_pretty(File::create(&json_path_tmp)?, records)?;
    fs::rename(json_path_tmp, json_path)?;
    Ok(records.len())
}

fn write_csv_records<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    records: &[BusinessRecord],
) -> Result<()> {
    writer.write_record(OUTPUT_COLUMNS.iter().copied())?;
    for record in records {
        // Like Outscraper: one row per email, or one row when there is none.
        let emails: Vec<&serde_json::Value> = extra_value(record, "emails")
            .and_then(serde_json::Value::as_array)
            .map(|emails| emails.iter().collect())
            .unwrap_or_default();
        let rows: Vec<Option<&serde_json::Value>> = if emails.is_empty() {
            vec![None]
        } else {
            emails.into_iter().map(Some).collect()
        };
        for email in rows {
            let values = OUTPUT_COLUMNS
                .iter()
                .map(|column| output_value(record, column, email))
                .collect::<Vec<_>>();
            writer.write_record(values)?;
        }
    }
    Ok(())
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
}
