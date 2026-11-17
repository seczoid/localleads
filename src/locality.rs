//! Pulls city, state and postal code out of a Google Maps address line.
//!
//! Addresses arrive as comma-separated parts, often with the business name
//! first: "Cleany, 167 Madison Ave Ste 202, New York, NY 10016". Only shapes
//! we can read with confidence are parsed; anything else is left empty so a
//! wrong guess never ends up in a filter.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Locality {
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
}

const US_STATES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
    "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM",
    "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA",
    "WV", "WI", "WY", "PR", "VI", "GU",
];
const CA_PROVINCES: &[&str] = &[
    "AB", "BC", "MB", "NB", "NL", "NS", "NT", "NU", "ON", "PE", "QC", "SK", "YT",
];
const AU_STATES: &[&str] = &["NSW", "VIC", "QLD", "WA", "SA", "TAS", "ACT", "NT"];
const COUNTRIES: &[&str] = &[
    "united states",
    "usa",
    "us",
    "united states of america",
    "canada",
    "australia",
    "germany",
    "deutschland",
    "france",
    "spain",
    "españa",
    "italy",
    "italia",
    "netherlands",
    "austria",
    "switzerland",
    "belgium",
    "united kingdom",
    "uk",
];

/// City, state and postal code from the address, or, when the listing hides
/// its address, from the nearest US ZIP code to its coordinates.
pub fn locate(address: &str, latitude: Option<f64>, longitude: Option<f64>) -> Locality {
    let parsed = parse(address);
    if parsed.city.is_some() || parsed.state.is_some() {
        return parsed;
    }
    let (Some(latitude), Some(longitude)) = (latitude, longitude) else {
        return parsed;
    };
    match crate::geo::us_nearest_zip(latitude, longitude) {
        Some((zip, city, state)) => Locality {
            city: Some(city.to_owned()),
            state: Some(state.to_owned()),
            postal_code: Some(zip.to_owned()),
        },
        None => parsed,
    }
}

pub fn parse(address: &str) -> Locality {
    let mut parts: Vec<&str> = address
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() >= 2
        && parts
            .last()
            .is_some_and(|last| COUNTRIES.contains(&last.to_lowercase().as_str()))
    {
        parts.pop();
    }
    let Some(&last) = parts.last() else {
        return Locality::default();
    };
    let previous = parts.len().checked_sub(2).map(|index| parts[index]);
    let tokens: Vec<&str> = last.split_whitespace().collect();

    // "NY 10016" or "New York NY 10016" (US), "ON M5V 2T6" (Canada),
    // "Sydney NSW 2000" (Australia).
    if let Some(found) = region_and_postal(&tokens) {
        let (region, postal, city_tokens) = found;
        let city = if city_tokens.is_empty() {
            previous.filter(|value| looks_like_city(value))
        } else {
            None
        };
        let city = city
            .map(ToOwned::to_owned)
            .or_else(|| (!city_tokens.is_empty()).then(|| city_tokens.join(" ")));
        return Locality {
            city: city.map(|value| tidy(&value)),
            state: Some(region.to_owned()),
            postal_code: Some(postal),
        };
    }

    // "10115 Berlin" (most of Europe).
    if tokens.len() >= 2 && is_numeric_postal(tokens[0]) {
        let city = tokens[1..].join(" ");
        if looks_like_city(&city) {
            return Locality {
                city: Some(tidy(&city)),
                state: None,
                postal_code: Some(tokens[0].to_owned()),
            };
        }
    }
    Locality::default()
}

/// Returns (region, postal code, leading city tokens).
fn region_and_postal<'a>(tokens: &[&'a str]) -> Option<(&'a str, String, Vec<&'a str>)> {
    let count = tokens.len();
    // Canada: two-part postal code "M5V 2T6".
    if count >= 3 {
        let region = tokens[count - 3];
        let postal = format!("{} {}", tokens[count - 2], tokens[count - 1]);
        if CA_PROVINCES.contains(&region) && is_canadian_postal(&postal) {
            return Some((region, postal.to_ascii_uppercase(), tokens[..count - 3].to_vec()));
        }
    }
    if count >= 2 {
        let region = tokens[count - 2];
        let postal = tokens[count - 1];
        if US_STATES.contains(&region) && is_us_zip(postal) {
            return Some((region, postal.to_owned(), tokens[..count - 2].to_vec()));
        }
        if CA_PROVINCES.contains(&region) && is_canadian_postal(postal) {
            return Some((region, postal.to_ascii_uppercase(), tokens[..count - 2].to_vec()));
        }
        if AU_STATES.contains(&region) && postal.len() == 4 && all_digits(postal) {
            return Some((region, postal.to_owned(), tokens[..count - 2].to_vec()));
        }
    }
    None
}

fn all_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_us_zip(value: &str) -> bool {
    match value.split_once('-') {
        Some((zip, plus)) => zip.len() == 5 && all_digits(zip) && plus.len() == 4 && all_digits(plus),
        None => value.len() == 5 && all_digits(value),
    }
}

fn is_numeric_postal(value: &str) -> bool {
    (4..=6).contains(&value.len()) && all_digits(value)
}

fn is_canadian_postal(value: &str) -> bool {
    let compact: Vec<u8> = value.bytes().filter(|byte| *byte != b' ').collect();
    compact.len() == 6
        && compact.iter().enumerate().all(|(index, byte)| {
            if index % 2 == 0 {
                byte.is_ascii_alphabetic()
            } else {
                byte.is_ascii_digit()
            }
        })
}

/// A city part has letters and no house number or suite marker.
fn looks_like_city(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 48
        && value.chars().any(char::is_alphabetic)
        && !value.chars().any(|character| character.is_ascii_digit() || character == '#')
}

fn tidy(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(city: Option<&str>, state: Option<&str>, postal: Option<&str>) -> Locality {
        Locality {
            city: city.map(ToOwned::to_owned),
            state: state.map(ToOwned::to_owned),
            postal_code: postal.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn parses_us_addresses_with_business_name_prefix() {
        assert_eq!(
            parse("Cleany, 167 Madison Ave Ste 202, New York, NY 10016"),
            loc(Some("New York"), Some("NY"), Some("10016"))
        );
        assert_eq!(
            parse("Cleanzilla, 39-08 24th St Suíte 4, Long Island City, NY 11101-1234, United States"),
            loc(Some("Long Island City"), Some("NY"), Some("11101-1234"))
        );
    }

    #[test]
    fn skips_city_when_previous_part_is_a_street() {
        assert_eq!(parse("123 Main St, TX 75001"), loc(None, Some("TX"), Some("75001")));
    }

    #[test]
    fn parses_canada_australia_and_europe() {
        assert_eq!(
            parse("1 King St W, Toronto, ON M5V 2T6, Canada"),
            loc(Some("Toronto"), Some("ON"), Some("M5V 2T6"))
        );
        assert_eq!(
            parse("Shop 3, 10 George St, Sydney NSW 2000"),
            loc(Some("Sydney"), Some("NSW"), Some("2000"))
        );
        assert_eq!(
            parse("Torstraße 1, 10119 Berlin, Germany"),
            loc(Some("Berlin"), None, Some("10119"))
        );
    }

    #[test]
    fn leaves_unknown_shapes_empty() {
        assert_eq!(parse(""), Locality::default());
        assert_eq!(parse("Somewhere nice"), Locality::default());
    }
}
