//! Google's place page: what the search listing leaves out. Reviews per
//! star, review topics, the owner (a claimed, verified listing), the
//! business's own description and its photos.

use crate::scraper::{parse_json_response, request_text, value_at, value_i64, value_string};
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Map, Value, json};
use url::Url;

/// The fixed tail of the place request: which sections Google should send.
const SECTIONS: &str = "!12m4!2m3!1i360!2i120!4i8!13m57!2m2!1i203!2i100!3m2!2i4!5b1!6m6!1m2!1i86!2i86!1m2!1i408!2i240!7m33!1m3!1e1!2b0!3e3!1m3!1e2!2b1!3e2!1m3!1e2!2b0!3e3!1m3!1e8!2b0!3e3!1m3!1e10!2b0!3e3!1m3!1e10!2b1!3e2!1m3!1e10!2b0!3e4!1m3!1e9!2b1!3e2!2b1!9b0!15m8!1m7!1m2!1m1!1e2!2m2!1i195!2i195!3i20!14m3!1sRAD7auebN-CahvcPrbTO6A4!7e81!15i10112!15m108!1m26!13m9!2b1!3b1!4b1!6i1!8b1!9b1!14b1!20b1!25b1!18m15!3b1!4b1!5b1!6b1!13b1!14b1!17b1!21b1!22b1!30b1!32b1!33m1!1b1!34b1!36e2!10m1!8e3!11m1!3e1!17b1!20m2!1e3!1e6!24b1!25b1!26b1!27b1!29b1!30m1!2b1!36b1!37b1!39m3!2m2!2i1!3i1!43b1!52b1!54m1!1b1!55b1!56m1!1b1!61m2!1m1!1e1!65m5!3m4!1m3!1m2!1i224!2i298!72m22!1m8!2b1!5b1!7b1!12m4!1b1!2b1!4m1!1e1!4b1!8m10!1m6!4m1!1e1!4m1!1e3!4m1!1e4!3sother_user_google_review_posts__and__hotel_and_vr_partner_review_posts!6m1!1e1!9b1!89b1!90m2!1m1!1e2!98m3!1b1!2b1!3b1!103b1!113b1!114m3!1b1!2m1!1b1!117b1!122m1!1b1!126b1!127b1!128m1!1b1!21m0!22m1!1e81!30m8!3b1!6m2!1b1!2b1!7m2!1e3!2b1!9b1!34m5!7b1!10b1!14b1!15m1!1b0!37i803";

/// Which place to look up: the listing's feature id ("0x…:0x…"), its name
/// and address, and where it is.
pub struct PlaceRef<'a> {
    pub google_id: &'a str,
    pub name: &'a str,
    pub address: &'a str,
    pub latitude: f64,
    pub longitude: f64,
    pub language: &'a str,
    pub region: &'a str,
}

pub fn details_url(place: &PlaceRef<'_>) -> Result<Url> {
    let q = if place.address.is_empty() {
        place.name.to_owned()
    } else {
        format!("{}, {}", place.name, place.address)
    };
    // `!` separates fields, so it can't appear inside the query text.
    let q_field = q.replace('!', " ");
    let pb = format!(
        "!1m11!1s{}!2s{}!3m8!1m3!1d3141.0!2d{}!3d{}!3m2!1i1024!2i768!4f13.1{SECTIONS}",
        place.google_id, q_field, place.longitude, place.latitude
    );
    let mut url = Url::parse("https://www.google.com/maps/preview/place")?;
    url.query_pairs_mut()
        .append_pair("authuser", "0")
        .append_pair("hl", place.language)
        .append_pair("gl", place.region)
        .append_pair("q", &q)
        .append_pair("pb", &pb);
    Ok(url)
}

/// Fetches and parses the place page.
pub async fn fetch(http: &Client, place: &PlaceRef<'_>) -> Result<Map<String, Value>> {
    let body = request_text(http, details_url(place)?).await?;
    let value = parse_json_response(&body)?;
    let business = value
        .get(6)
        .filter(|value| value.is_array())
        .context("Google place page had no business")?;
    Ok(parse(business))
}

/// The Outscraper fields found in the place page's business array.
pub fn parse(business: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(scores) = value_at(business, &[175, 3]).and_then(Value::as_array)
        && scores.len() == 5
    {
        let mut per_score = Map::new();
        for (index, count) in scores.iter().enumerate() {
            let count = value_i64(Some(count)).unwrap_or(0);
            per_score.insert((index + 1).to_string(), json!(count));
            out.insert(format!("reviews_per_score_{}", index + 1), json!(count));
        }
        out.insert("reviews_per_score".into(), Value::Object(per_score));
    }
    let tags: Vec<String> = value_at(business, &[153, 0])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| value_string(tag.get(1)))
        .collect();
    if !tags.is_empty() {
        out.insert("reviews_tags".into(), json!(tags.join(", ")));
    }
    // A listing with an owner has been claimed and verified by the business.
    let owner_name = value_string(value_at(business, &[57, 1]));
    let owner_id = value_string(value_at(business, &[57, 2]));
    out.insert("verified".into(), json!(owner_id.is_some()));
    if let Some(name) = owner_name {
        let title = name.strip_suffix(" (Owner)").unwrap_or(&name).trim().to_owned();
        out.insert("owner_title".into(), json!(title));
    }
    if let Some(id) = owner_id {
        out.insert(
            "owner_link".into(),
            json!(format!("https://www.google.com/maps/contrib/{id}")),
        );
        out.insert("owner_id".into(), json!(id));
    }
    if let Some(description) = value_string(value_at(business, &[154, 0, 0])) {
        out.insert("description".into(), json!(description));
    }
    if let Some(photo) = value_string(value_at(business, &[37, 0, 0, 6, 0])) {
        out.insert("photo".into(), json!(resize(&photo, 800, 500)));
    }
    if let Some(caption) = value_string(value_at(business, &[37, 0, 0, 6, 1]))
        && let Some(count) = caption.split_whitespace().next().and_then(|n| n.replace(',', "").parse::<i64>().ok())
    {
        out.insert("photos_count".into(), json!(count));
    }
    // The street-level picture of the storefront.
    let street = [51, 72].into_iter().find_map(|index| {
        let picture = value_at(business, &[index, 0, 0])?;
        // Street View pictures carry a camera position ([3, lon, lat]).
        (value_i64(value_at(picture, &[8, 0, 0])) == Some(3))
            .then(|| value_string(value_at(picture, &[6, 0])))
            .flatten()
    });
    if let Some(street) = street {
        out.insert("street_view".into(), json!(resize(&street, 800, 500)));
    }
    out
}

/// Swaps a googleusercontent size suffix ("=w360-h270-k-no") for a bigger one.
fn resize(url: &str, width: u32, height: u32) -> String {
    match url.rsplit_once('=') {
        Some((base, size)) if size.starts_with('w') || size.starts_with('s') => {
            format!("{base}=w{width}-h{height}-k-no")
        }
        _ => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_place_page() {
        let mut business = vec![Value::Null; 180];
        business[175] = json!([null, [null, 1], 0, [2, 3, 0, 1, 109]]);
        business[153] = json!([[[["a"], "patio installation"], [["b"], "hardscaping"]]]);
        business[57] = json!([null, "Outdoor Spaces (Owner)", "1169"]);
        business[154] = json!([["We build patios."]]);
        business[37] = json!([[["id", 10, 12, null, null, null, ["https://lh3.googleusercontent.com/x=w360-h270-k-no", "241 Photos"]]]]);
        business[51] = json!([[["id", 10, 12, null, null, null, ["https://lh3.googleusercontent.com/sv=w203-h114-k-no", ""], null, [[3, -84.5, 38.0]]]]]);
        let out = parse(&Value::Array(business));
        assert_eq!(out["reviews_per_score"], json!({"1": 2, "2": 3, "3": 0, "4": 1, "5": 109}));
        assert_eq!(out["reviews_per_score_5"], json!(109));
        assert_eq!(out["reviews_tags"], json!("patio installation, hardscaping"));
        assert_eq!(out["owner_title"], json!("Outdoor Spaces"));
        assert_eq!(out["owner_link"], json!("https://www.google.com/maps/contrib/1169"));
        assert_eq!(out["verified"], json!(true));
        assert_eq!(out["description"], json!("We build patios."));
        assert_eq!(out["photo"], json!("https://lh3.googleusercontent.com/x=w800-h500-k-no"));
        assert_eq!(out["photos_count"], json!(241));
        assert_eq!(out["street_view"], json!("https://lh3.googleusercontent.com/sv=w800-h500-k-no"));
    }

    #[test]
    fn real_place_page() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/place.json");
        let Ok(body) = std::fs::read_to_string(path) else {
            return;
        };
        let value = parse_json_response(&body).expect("json");
        let out = parse(&value[6]);
        assert_eq!(out["reviews_per_score_5"], json!(109));
        assert_eq!(out["owner_title"], json!("Outdoor Spaces"));
        assert!(out["reviews_tags"].as_str().unwrap().starts_with("patio installation"));
        assert!(out.contains_key("street_view"), "{out:?}");
    }
}
