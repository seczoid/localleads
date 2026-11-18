//! Built-in US places, used to split a state into many small lookups.
//!
//! Google Maps stops at roughly 40 results for a statewide query and about
//! 120–200 for a city, so deep searches ask once per city or ZIP code and
//! merge the results. Data: GeoNames postal codes (CC BY 4.0), see
//! `data/NOTICE.md`.

use std::collections::HashMap;
use std::sync::OnceLock;

const ZIPS: &str = include_str!("../data/us_zips.tsv");
const STATES: &str = include_str!("../data/us_states.tsv");

/// One place to search: the text appended to the query, plus where to aim
/// the map.
#[derive(Clone, Debug, PartialEq)]
pub struct Area {
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
    /// ZIP codes in the area's city: a rough measure of how busy it is, so
    /// searches can start where most businesses are.
    pub weight: u32,
}

/// A city in the location picker.
#[derive(Clone, Debug, serde::Serialize)]
pub struct City {
    pub name: String,
    /// "Louisville, KY": the value a search stores.
    pub label: String,
    pub state: String,
    pub zips: u32,
    /// People living there, when known (towns outside the US).
    pub population: u32,
}

struct Zip {
    code: &'static str,
    city: &'static str,
    latitude: f64,
    longitude: f64,
    /// Stands for every ZIP of its city that has no point of its own (PO
    /// boxes, single companies): those share one point, and Google Maps
    /// can't place them, so the city is searched there once by name.
    shared: bool,
}

struct Places {
    /// State code → its ZIP codes, sorted by city then ZIP. A city's ZIPs
    /// that share one point appear once (see `Zip::shared`).
    by_state: HashMap<&'static str, Vec<Zip>>,
    /// Lower-case state name or code → state code.
    state_codes: HashMap<String, &'static str>,
    /// ZIP code → (state code, latitude, longitude).
    zip_points: HashMap<&'static str, (&'static str, f64, f64)>,
    /// (state code, city) → areas a ZIP-level search makes of that city.
    city_sizes: HashMap<(&'static str, &'static str), u32>,
    /// State code → state name.
    state_names: HashMap<&'static str, &'static str>,
}

fn places() -> &'static Places {
    static PLACES: OnceLock<Places> = OnceLock::new();
    PLACES.get_or_init(|| {
        let mut by_state: HashMap<&'static str, Vec<Zip>> = HashMap::new();
        let mut zip_points = HashMap::new();
        let mut city_sizes: HashMap<(&'static str, &'static str), u32> = HashMap::new();
        let rows: Vec<(&'static str, &'static str, &'static str, f64, f64)> = ZIPS
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let (code, city, state) = (fields.next()?, fields.next()?, fields.next()?);
                let latitude = fields.next()?.parse().ok()?;
                let longitude = fields.next()?.parse().ok()?;
                Some((code, city, state, latitude, longitude))
            })
            .collect();
        let point = |state, city, latitude: f64, longitude: f64| {
            (state, city, latitude.to_bits(), longitude.to_bits())
        };
        let mut on_point: HashMap<_, u32> = HashMap::new();
        for &(_, city, state, latitude, longitude) in &rows {
            *on_point.entry(point(state, city, latitude, longitude)).or_default() += 1;
        }
        let mut cities_shared = std::collections::HashSet::new();
        for (code, city, state, latitude, longitude) in rows {
            zip_points.insert(code, (state, latitude, longitude));
            let shared = on_point[&point(state, city, latitude, longitude)] > 1;
            if shared && !cities_shared.insert((state, city)) {
                continue;
            }
            *city_sizes.entry((state, city)).or_default() += 1;
            by_state.entry(state).or_default().push(Zip {
                code,
                city,
                latitude,
                longitude,
                shared,
            });
        }
        let mut state_codes = HashMap::new();
        let mut state_names = HashMap::new();
        for line in STATES.lines() {
            if let Some((code, name)) = line.split_once('\t') {
                state_codes.insert(name.to_lowercase(), code);
                state_codes.insert(code.to_lowercase(), code);
                state_names.insert(code, name);
            }
        }
        Places {
            by_state,
            state_codes,
            zip_points,
            city_sizes,
            state_names,
        }
    })
}

/// US state name → two-letter code, for the location picker.
pub fn us_state_codes() -> HashMap<&'static str, &'static str> {
    places()
        .state_names
        .iter()
        .map(|(&code, &name)| (name, code))
        .collect()
}

/// The two-letter code for a US state name or code.
pub fn us_state_code(state: &str) -> Option<&'static str> {
    places()
        .state_codes
        .get(&state.trim().to_lowercase())
        .copied()
}

fn zip_area(code: &'static str, zip: &Zip) -> Area {
    let name = if zip.shared { zip.city } else { zip.code };
    Area {
        label: format!("{name}, {code}"),
        latitude: zip.latitude,
        longitude: zip.longitude,
        weight: places()
            .city_sizes
            .get(&(code, zip.city))
            .copied()
            .unwrap_or(1),
    }
}

/// Every ZIP code in a US state, as "40202, KY".
pub fn us_zip_areas(state: &str) -> Vec<Area> {
    let Some(code) = us_state_code(state) else {
        return Vec::new();
    };
    places()
        .by_state
        .get(code)
        .map(|zips| zips.iter().map(|zip| zip_area(code, zip)).collect())
        .unwrap_or_default()
}

/// Splits "Louisville, KY" into its state code and city name.
fn split_city_label(label: &str) -> Option<(&'static str, String)> {
    let (city, state) = label.rsplit_once(',')?;
    Some((us_state_code(state)?, city.trim().to_lowercase()))
}

/// Every ZIP code in a US city given as "Louisville, KY".
pub fn us_city_zip_areas(label: &str) -> Vec<Area> {
    let Some((code, city)) = split_city_label(label) else {
        return Vec::new();
    };
    places()
        .by_state
        .get(code)
        .map(|zips| {
            zips.iter()
                .filter(|zip| zip.city.to_lowercase() == city)
                .map(|zip| zip_area(code, zip))
                .collect()
        })
        .unwrap_or_default()
}

/// The cities of a US state, alphabetically.
pub fn us_cities(state: &str) -> Vec<City> {
    let Some(code) = us_state_code(state) else {
        return Vec::new();
    };
    let places = places();
    let state_name = places.state_names.get(code).copied().unwrap_or(code);
    let mut cities: Vec<City> = Vec::new();
    for zip in places.by_state.get(code).into_iter().flatten() {
        if cities.last().is_some_and(|city| city.name == zip.city) {
            continue;
        }
        cities.push(City {
            name: zip.city.to_owned(),
            label: format!("{}, {code}", zip.city),
            state: state_name.to_owned(),
            zips: places
                .city_sizes
                .get(&(code, zip.city))
                .copied()
                .unwrap_or(1),
            population: 0,
        });
    }
    cities
}

/// US cities whose name starts with `query` (then ones containing it),
/// biggest first.
pub fn us_city_search(query: &str, limit: usize) -> Vec<City> {
    let query = query.trim().to_lowercase();
    if query.len() < 2 {
        return Vec::new();
    }
    let places = places();
    let mut found: Vec<(bool, u32, &'static str, &'static str)> = places
        .city_sizes
        .iter()
        .filter_map(|(&(code, city), &zips)| {
            let lower = city.to_lowercase();
            if lower.starts_with(&query) {
                Some((true, zips, code, city))
            } else if lower.contains(&query) {
                Some((false, zips, code, city))
            } else {
                None
            }
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.3.cmp(b.3)));
    found
        .into_iter()
        .take(limit)
        .map(|(_, zips, code, city)| City {
            name: city.to_owned(),
            label: format!("{city}, {code}"),
            state: places
                .state_names
                .get(code)
                .copied()
                .unwrap_or(code)
                .to_owned(),
            zips,
            population: 0,
        })
        .collect()
}

/// ZIP codes in a US city given as "Louisville, KY".
pub fn us_city_zip_count(label: &str) -> usize {
    split_city_label(label)
        .and_then(|(code, city)| {
            places()
                .city_sizes
                .iter()
                .find(|((state, name), _)| *state == code && name.to_lowercase() == city)
                .map(|(_, &zips)| zips as usize)
        })
        .unwrap_or(0)
}

/// Every city or town in a US state, as "Louisville, KY", aimed at the
/// middle of its ZIP codes.
pub fn us_city_areas(state: &str) -> Vec<Area> {
    let Some(code) = us_state_code(state) else {
        return Vec::new();
    };
    let Some(zips) = places().by_state.get(code) else {
        return Vec::new();
    };
    let mut areas: Vec<Area> = Vec::new();
    let mut count = 0_u32;
    let mut current = "";
    for zip in zips {
        if zip.city != current {
            finish_city(&mut areas, count);
            current = zip.city;
            count = 0;
            areas.push(Area {
                label: format!("{}, {code}", zip.city),
                latitude: 0.0,
                longitude: 0.0,
                weight: places()
                    .city_sizes
                    .get(&(code, zip.city))
                    .copied()
                    .unwrap_or(1),
            });
        }
        if let Some(area) = areas.last_mut() {
            area.latitude += zip.latitude;
            area.longitude += zip.longitude;
        }
        count += 1;
    }
    finish_city(&mut areas, count);
    areas
}

fn finish_city(areas: &mut [Area], count: u32) {
    if let Some(area) = areas.last_mut()
        && count > 0
    {
        area.latitude /= f64::from(count);
        area.longitude /= f64::from(count);
    }
}

/// How many places `us_zip_areas` / `us_city_areas` would return, without
/// building them.
pub fn us_area_count(state: &str, cities: bool) -> usize {
    let Some(zips) = us_state_code(state).and_then(|code| places().by_state.get(code)) else {
        return 0;
    };
    if !cities {
        return zips.len();
    }
    let mut count = 0;
    let mut current = "";
    for zip in zips {
        if zip.city != current {
            count += 1;
            current = zip.city;
        }
    }
    count
}

/// The ZIP code whose centre is nearest to a point, as (ZIP, city, state),
/// if one is within 40 km. Used for listings that hide their street
/// address (service-area businesses): Google still gives their coordinates.
pub fn us_nearest_zip(latitude: f64, longitude: f64) -> Option<(&'static str, &'static str, &'static str)> {
    static GRID: OnceLock<HashMap<(i32, i32), Vec<(&'static str, &'static str, &'static str, f64, f64)>>> =
        OnceLock::new();
    let cell = |latitude: f64, longitude: f64| ((latitude * 2.0).floor() as i32, (longitude * 2.0).floor() as i32);
    let grid = GRID.get_or_init(|| {
        let mut grid: HashMap<_, Vec<_>> = HashMap::new();
        for (state, zips) in &places().by_state {
            for zip in zips {
                grid.entry(cell(zip.latitude, zip.longitude))
                    .or_default()
                    .push((zip.code, zip.city, *state, zip.latitude, zip.longitude));
            }
        }
        grid
    });
    if !latitude.is_finite() || !longitude.is_finite() {
        return None;
    }
    let (row, column) = cell(latitude, longitude);
    let scale = latitude.to_radians().cos();
    let mut best: Option<(f64, (&'static str, &'static str, &'static str))> = None;
    for dr in -1..=1 {
        for dc in -1..=1 {
            for &(code, city, state, lat, lon) in grid.get(&(row + dr, column + dc)).into_iter().flatten() {
                let (dy, dx) = (lat - latitude, (lon - longitude) * scale);
                let distance = (dy * dy + dx * dx).sqrt() * 111.0;
                if best.is_none_or(|(km, _)| distance < km) {
                    best = Some((distance, (code, city, state)));
                }
            }
        }
    }
    best.filter(|(km, _)| *km <= 40.0).map(|(_, place)| place)
}

/// A US ZIP code as "19801, DE" with its coordinates.
pub fn us_zip_place(zip: &str) -> Option<(String, (f64, f64))> {
    let digits: String = zip
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    places()
        .zip_points
        .get(digits.as_str())
        .map(|&(state, latitude, longitude)| (format!("{digits}, {state}"), (latitude, longitude)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_zip_names_the_town() {
        let (_, city, state) = us_nearest_zip(38.9271446, -77.216004).unwrap();
        assert_eq!(state, "VA");
        assert!(!city.is_empty());
        assert_eq!(us_nearest_zip(51.5, -0.12), None);
    }

    #[test]
    fn states_resolve_by_name_or_code() {
        assert_eq!(us_state_code("Kentucky"), Some("KY"));
        assert_eq!(us_state_code("ky"), Some("KY"));
        assert_eq!(us_state_code("District of Columbia"), Some("DC"));
        assert_eq!(us_state_code("Narnia"), None);
    }

    #[test]
    fn states_split_into_zips_and_cities() {
        let zips = us_zip_areas("Kentucky");
        assert!(zips.len() > 800, "{}", zips.len());
        assert!(zips.iter().any(|area| area.label == "40202, KY"));
        let cities = us_city_areas("Kentucky");
        assert!(cities.len() > 500 && cities.len() < zips.len());
        assert_eq!(us_area_count("Kentucky", false), zips.len());
        assert_eq!(us_area_count("Kentucky", true), cities.len());
        let louisville = cities
            .iter()
            .find(|area| area.label == "Louisville, KY")
            .expect("Louisville");
        assert!((louisville.latitude - 38.2).abs() < 0.3);
        assert!((louisville.longitude + 85.7).abs() < 0.3);
    }

    #[test]
    fn cities_expand_to_their_zips() {
        let zips = us_city_zip_areas("Louisville, KY");
        assert!(zips.len() > 30, "{}", zips.len());
        assert_eq!(us_city_zip_count("louisville, ky"), zips.len());
        assert!(zips.iter().all(|zip| zip.weight as usize == zips.len()));
        assert!(us_city_zip_areas("Nowhere, KY").is_empty());
        let cities = us_cities("Kentucky");
        assert!(cities.iter().any(|city| city.label == "Louisville, KY"));
        let found = us_city_search("louis", 5);
        assert_eq!(found[0].label, "Louisville, KY");
    }

    #[test]
    fn zips_without_a_point_of_their_own_are_searched_once() {
        // Austin has 74 ZIPs; 26 of them (PO boxes, the IRS) sit on two
        // shared points. Searching "78709, TX" gets results from all of
        // Texas, so those are one area, "Austin, TX".
        let zips = us_city_zip_areas("Austin, TX");
        assert_eq!(zips.len(), 49);
        assert_eq!(us_city_zip_count("Austin, TX"), 49);
        let labels: std::collections::HashSet<_> = zips.iter().map(|zip| zip.label.as_str()).collect();
        assert_eq!(labels.len(), zips.len(), "labels are unique");
        assert!(labels.contains("Austin, TX"));
        assert!(labels.contains("78701, TX"));
        assert!(!labels.contains("78709, TX"));
        // A ZIP typed in by hand still has its place.
        assert_eq!(us_zip_place("78709").map(|(label, _)| label).as_deref(), Some("78709, TX"));
        let texas = us_zip_areas("Texas");
        assert_eq!(us_area_count("Texas", false), texas.len());
        let labels: std::collections::HashSet<_> = texas.iter().map(|zip| zip.label.as_str()).collect();
        assert_eq!(labels.len(), texas.len(), "labels are unique across a state");
    }

    #[test]
    fn zip_points_are_found() {
        let (label, (latitude, _)) = us_zip_place("10001").expect("NYC ZIP");
        assert_eq!(label, "10001, NY");
        assert!((latitude - 40.75).abs() < 0.1);
        assert!(us_zip_place("00000").is_none());
    }
}
