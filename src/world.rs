//! Built-in places for every country but the United States (which keeps
//! `geo`), used to split a country into many small lookups the way US
//! searches go ZIP code by ZIP code.
//!
//! Each country has its states or provinces, its towns of 1,000+ people,
//! and, for about a hundred countries, its postal codes. Built by
//! `scripts/build_world_places.py` from GeoNames (CC BY 4.0), see
//! `data/NOTICE.md`. The file is unpacked the first time a country other
//! than the US is used, and each country is read on first use.

use crate::geo::{Area, City};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Mutex, OnceLock};

const DATA: &[u8] = include_bytes!("../data/world_places.tsv.gz");

pub struct Country {
    pub iso: &'static str,
    pub name: &'static str,
    pub states: Vec<&'static str>,
    pub towns: Vec<Town>,
    pub postal: Vec<PostalArea>,
    /// Postal areas per town, by index into `towns`.
    town_postal: Vec<u32>,
    /// Folded picker label ("munich, bavaria") → its towns, biggest first.
    /// Several towns of one name in one state share a label and are
    /// picked together.
    by_label: HashMap<String, Vec<usize>>,
}

pub struct Town {
    pub name: &'static str,
    pub state: Option<usize>,
    pub latitude: f64,
    pub longitude: f64,
    pub population: u32,
    /// A part of a bigger place (a neighborhood or section): searched, but
    /// not listed as a city to pick.
    pub section: bool,
}

pub struct PostalArea {
    pub code: &'static str,
    pub place: &'static str,
    pub state: Option<usize>,
    pub latitude: f64,
    pub longitude: f64,
    pub weight: u32,
    /// The town this area belongs to, by index into `towns`.
    pub town: Option<usize>,
}

struct Index {
    /// ISO code → (name, the country's rows).
    rows: HashMap<&'static str, (&'static str, &'static str)>,
    /// Lower-case name or ISO code → ISO code.
    lookup: HashMap<String, &'static str>,
    /// (name, ISO code), alphabetically.
    names: Vec<(&'static str, &'static str)>,
}

fn index() -> &'static Index {
    static INDEX: OnceLock<Index> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut text = String::new();
        let unpacked = flate2::read::GzDecoder::new(DATA).read_to_string(&mut text);
        if unpacked.is_err() {
            text.clear();
        }
        let text: &'static str = Box::leak(text.into_boxed_str());
        let mut rows = HashMap::new();
        let mut lookup = HashMap::new();
        let mut names = Vec::new();
        // Rows are grouped by country, each group starting with its C row.
        let mut current: Option<(&'static str, &'static str, usize)> = None;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            if line.starts_with("C\t") {
                if let Some((iso, name, start)) = current {
                    rows.insert(iso, (name, &text[start..offset]));
                }
                let mut fields = line.trim_end().split('\t').skip(1);
                let (Some(iso), Some(name)) = (fields.next(), fields.next()) else {
                    current = None;
                    offset += line.len();
                    continue;
                };
                current = Some((iso, name, offset + line.len()));
                lookup.insert(name.to_lowercase(), iso);
                lookup.insert(iso.to_lowercase(), iso);
                names.push((name, iso));
            }
            offset += line.len();
        }
        if let Some((iso, name, start)) = current {
            rows.insert(iso, (name, &text[start..offset]));
        }
        // Names people also use.
        for (alias, iso) in [
            ("uk", "GB"),
            ("great britain", "GB"),
            ("england", "GB"),
            ("russia", "RU"),
            ("south korea", "KR"),
            ("korea", "KR"),
            ("czech republic", "CZ"),
            ("czechia", "CZ"),
            ("vietnam", "VN"),
            ("uae", "AE"),
            ("holland", "NL"),
            ("the netherlands", "NL"),
            ("ivory coast", "CI"),
            ("turkiye", "TR"),
            ("türkiye", "TR"),
        ] {
            if rows.contains_key(iso) {
                lookup.entry(alias.to_owned()).or_insert(iso);
            }
        }
        names.sort_by_key(|(name, _)| fold(name));
        Index { rows, lookup, names }
    })
}

/// Every country with places, as (name, ISO code), alphabetically. The
/// United States isn't here; it has `geo`.
pub fn countries() -> &'static [(&'static str, &'static str)] {
    &index().names
}

/// The ISO code of a country given by name, alias or code.
pub fn iso(country: &str) -> Option<&'static str> {
    index().lookup.get(&country.trim().to_lowercase()).copied()
}

/// A country's places, read the first time it's asked for.
pub fn country(name: &str) -> Option<&'static Country> {
    static PARSED: OnceLock<Mutex<HashMap<&'static str, &'static Country>>> = OnceLock::new();
    let iso = iso(name)?;
    let parsed = PARSED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut parsed = parsed.lock().ok()?;
    if let Some(country) = parsed.get(iso) {
        return Some(country);
    }
    let &(country_name, rows) = index().rows.get(iso)?;
    let country: &'static Country = Box::leak(Box::new(parse(iso, country_name, rows)));
    parsed.insert(iso, country);
    Some(country)
}

fn parse(iso: &'static str, name: &'static str, rows: &'static str) -> Country {
    let mut country = Country {
        iso,
        name,
        states: Vec::new(),
        towns: Vec::new(),
        postal: Vec::new(),
        town_postal: Vec::new(),
        by_label: HashMap::new(),
    };
    let number = |value: Option<&str>| value.and_then(|value| value.parse::<f64>().ok());
    let index = |value: Option<&str>| value.and_then(|value| value.parse::<usize>().ok());
    for line in rows.lines() {
        let mut fields = line.split('\t');
        let kind = fields.next();
        let _iso = fields.next();
        match kind {
            Some("S") => {
                let _index = fields.next();
                if let Some(state) = fields.next() {
                    country.states.push(state);
                }
            }
            Some("T") => {
                let name = fields.next().unwrap_or_default();
                let state = index(fields.next());
                let (Some(latitude), Some(longitude)) = (number(fields.next()), number(fields.next())) else {
                    continue;
                };
                let population = fields.next().and_then(|value| value.parse().ok()).unwrap_or(0);
                let section = fields.next() == Some("1");
                country.towns.push(Town {
                    name,
                    state,
                    latitude,
                    longitude,
                    population,
                    section,
                });
            }
            Some("P") => {
                let code = fields.next().unwrap_or_default();
                let place = fields.next().unwrap_or_default();
                let state = index(fields.next());
                let (Some(latitude), Some(longitude)) = (number(fields.next()), number(fields.next())) else {
                    continue;
                };
                let weight = fields.next().and_then(|value| value.parse().ok()).unwrap_or(1);
                let town = index(fields.next());
                country.postal.push(PostalArea {
                    code,
                    place,
                    state,
                    latitude,
                    longitude,
                    weight,
                    town,
                });
            }
            _ => {}
        }
    }
    country.town_postal = vec![0; country.towns.len()];
    for area in &country.postal {
        if let Some(count) = area.town.and_then(|town| country.town_postal.get_mut(town)) {
            *count += 1;
        }
    }
    let labels: Vec<(String, usize)> = country
        .towns
        .iter()
        .enumerate()
        .filter(|(_, town)| !town.section)
        .map(|(index, town)| (fold(&country.town_label(town)), index))
        .collect();
    for (label, index) in labels {
        country.by_label.entry(label).or_default().push(index);
    }
    let towns = &country.towns;
    for group in country.by_label.values_mut() {
        group.sort_by_key(|&index| std::cmp::Reverse(towns[index].population));
    }
    country
}

impl Country {
    pub fn has_postal(&self) -> bool {
        !self.postal.is_empty()
    }

    /// The index of a state given by name, ignoring case and accents.
    pub fn state(&self, name: &str) -> Option<usize> {
        let wanted = fold(name);
        self.states.iter().position(|state| fold(state) == wanted)
    }

    fn in_state(&self, state: Option<&str>) -> Option<Option<usize>> {
        match state {
            None => Some(None),
            Some(name) => self.state(name).map(Some),
        }
    }

    /// Postal areas in a state (or the whole country), as "75001 Paris, France".
    pub fn postal_areas(&self, state: Option<&str>) -> Vec<Area> {
        let Some(state) = self.in_state(state) else {
            return Vec::new();
        };
        self.postal
            .iter()
            .filter(|area| state.is_none() || area.state == state)
            .map(|area| self.postal_area(area))
            .collect()
    }

    fn postal_area(&self, area: &PostalArea) -> Area {
        Area {
            label: format!("{} {}, {}", area.code, short_place(area.place), self.name),
            latitude: area.latitude,
            longitude: area.longitude,
            weight: area.weight,
        }
    }

    /// Towns in a state (or the whole country), as "Lyon, Auvergne-Rhône-Alpes, France".
    pub fn town_areas(&self, state: Option<&str>) -> Vec<Area> {
        let Some(state) = self.in_state(state) else {
            return Vec::new();
        };
        self.towns
            .iter()
            .filter(|town| state.is_none() || town.state == state)
            .map(|town| Area {
                label: self.town_search_label(town),
                latitude: town.latitude,
                longitude: town.longitude,
                weight: town.population,
            })
            .collect()
    }

    /// How many places `postal_areas` / `town_areas` would return.
    pub fn area_count(&self, state: Option<&str>, postal: bool) -> usize {
        let Some(state) = self.in_state(state) else {
            return 0;
        };
        if postal {
            self.postal
                .iter()
                .filter(|area| state.is_none() || area.state == state)
                .count()
        } else {
            self.towns
                .iter()
                .filter(|town| state.is_none() || town.state == state)
                .count()
        }
    }

    fn state_name(&self, state: Option<usize>) -> Option<&'static str> {
        state.and_then(|index| self.states.get(index).copied())
    }

    /// "Lyon, Auvergne-Rhône-Alpes": what the picker stores for a city.
    fn town_label(&self, town: &Town) -> String {
        match self.state_name(town.state) {
            Some(state) => format!("{}, {state}", town.name),
            None => town.name.to_owned(),
        }
    }

    /// What Google is asked for: the city label plus the country.
    fn town_search_label(&self, town: &Town) -> String {
        format!("{}, {}", self.town_label(town), self.name)
    }

    /// The towns a picker label names, biggest first.
    fn find_towns(&self, label: &str) -> Vec<usize> {
        let label = fold(label);
        if let Some(group) = self.by_label.get(&label) {
            return group.clone();
        }
        // Older labels or typed names without the state.
        self.towns
            .iter()
            .enumerate()
            .filter(|(_, town)| !town.section && fold(town.name) == label)
            .max_by_key(|(_, town)| town.population)
            .map(|(index, _)| index)
            .into_iter()
            .collect()
    }

    /// The biggest town a picker label names.
    fn find_town(&self, label: &str) -> Option<usize> {
        self.find_towns(label).first().copied()
    }

    /// The postal areas of one city (every town of that name in its state).
    pub fn city_postal_areas(&self, label: &str) -> Vec<Area> {
        let towns = self.find_towns(label);
        self.postal
            .iter()
            .filter(|area| area.town.is_some_and(|town| towns.contains(&town)))
            .map(|area| self.postal_area(area))
            .collect()
    }

    fn towns_postal_count(&self, towns: &[usize]) -> u32 {
        towns
            .iter()
            .filter_map(|&town| self.town_postal.get(town))
            .sum()
    }

    pub fn city_postal_count(&self, label: &str) -> usize {
        self.towns_postal_count(&self.find_towns(label)) as usize
    }

    /// A city as one place to search, aimed at its middle.
    pub fn city_area(&self, label: &str) -> Option<Area> {
        let town = &self.towns[self.find_town(label)?];
        Some(Area {
            label: self.town_search_label(town),
            latitude: town.latitude,
            longitude: town.longitude,
            weight: town.population,
        })
    }

    /// The state a picked city is in, for leaving out cities whose whole
    /// state is picked too.
    pub fn city_state(&self, label: &str) -> Option<&'static str> {
        self.state_name(self.towns[self.find_town(label)?].state)
    }

    /// A town as a picker entry, covering every town sharing its label.
    fn city(&self, index: usize) -> City {
        let town = &self.towns[index];
        let label = self.town_label(town);
        let group = self.find_towns(&label);
        City {
            name: town.name.to_owned(),
            state: self.state_name(town.state).unwrap_or_default().to_owned(),
            zips: self.towns_postal_count(&group),
            population: group.iter().map(|&other| self.towns[other].population).sum(),
            label,
        }
    }

    /// Whether a town is the one listed for its label (the biggest).
    fn listed(&self, index: usize) -> bool {
        let town = &self.towns[index];
        !town.section
            && self
                .by_label
                .get(&fold(&self.town_label(town)))
                .is_some_and(|group| group.first() == Some(&index))
    }

    /// The cities of a state, alphabetically (sections left out).
    pub fn cities(&self, state: &str) -> Vec<City> {
        let Some(state) = self.state(state) else {
            return Vec::new();
        };
        let mut found: Vec<usize> = (0..self.towns.len())
            .filter(|&index| self.towns[index].state == Some(state) && self.listed(index))
            .collect();
        found.sort_by_key(|&index| fold(self.towns[index].name));
        found.into_iter().map(|index| self.city(index)).collect()
    }

    /// Cities whose name starts with `query` (then ones containing it),
    /// biggest first.
    pub fn city_search(&self, query: &str, limit: usize) -> Vec<City> {
        let query = fold(query);
        if query.chars().count() < 2 {
            return Vec::new();
        }
        let mut found: Vec<(bool, u32, usize)> = self
            .towns
            .iter()
            .enumerate()
            .filter(|(index, _)| self.listed(*index))
            .filter_map(|(index, town)| {
                let name = fold(town.name);
                if name.starts_with(&query) {
                    Some((true, town.population, index))
                } else if name.contains(&query) {
                    Some((false, town.population, index))
                } else {
                    None
                }
            })
            .collect();
        found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        found.into_iter().take(limit).map(|(_, _, index)| self.city(index)).collect()
    }

    /// A postal code as typed, with its centre if it's a known one.
    pub fn postal_place(&self, code: &str) -> Option<(String, (f64, f64))> {
        let wanted = fold(code);
        self.postal
            .iter()
            .find(|area| fold(area.code) == wanted)
            .map(|area| (self.postal_area(area).label, (area.latitude, area.longitude)))
    }
}

/// Lower case, without accents, spaces or punctuation, for matching names.
fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => Some('a'),
            'ç' | 'ć' | 'č' => Some('c'),
            'ď' | 'đ' => Some('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => Some('e'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ı' => Some('i'),
            'ł' | 'ľ' | 'ĺ' => Some('l'),
            'ñ' | 'ń' | 'ň' => Some('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => Some('o'),
            'ř' => Some('r'),
            'ś' | 'š' | 'ş' | 'ș' => Some('s'),
            'ť' | 'ţ' | 'ț' => Some('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => Some('u'),
            'ý' | 'ÿ' => Some('y'),
            'ź' | 'ż' | 'ž' => Some('z'),
            'ß' => Some('s'),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// A postal place name without long notes in brackets.
fn short_place(place: &str) -> &str {
    place.split(" (").next().unwrap_or(place).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countries_load_with_states_towns_and_postal_codes() {
        assert!(countries().len() > 200, "{}", countries().len());
        assert!(countries().iter().all(|(_, iso)| *iso != "US"));
        let france = country("France").expect("France");
        assert_eq!(france.iso, "FR");
        assert!(france.states.len() >= 13);
        assert!(france.has_postal());
        let paris = france.postal_areas(Some("Île-de-France"));
        assert!(paris.len() > 1000, "{}", paris.len());
        assert!(paris.iter().any(|area| area.label.starts_with("75001 ")));
        assert_eq!(france.area_count(Some("ile de france"), true), paris.len());
        // Countries are found by code and common names too.
        assert_eq!(iso("gb"), Some("GB"));
        assert_eq!(iso("UK"), Some("GB"));
        assert_eq!(iso("Narnia"), None);
    }

    #[test]
    fn countries_without_postal_codes_split_into_towns() {
        let nigeria = country("Nigeria").expect("Nigeria");
        assert!(!nigeria.has_postal());
        let lagos = nigeria.town_areas(Some("Lagos"));
        assert!(lagos.len() > 5, "{}", lagos.len());
        assert!(lagos.iter().all(|area| area.label.ends_with(", Lagos, Nigeria")));
    }

    #[test]
    fn cities_are_listed_found_and_expanded() {
        let germany = country("Germany").expect("Germany");
        let found = germany.city_search("munic", 5);
        assert_eq!(found[0].name, "Munich");
        let label = found[0].label.clone();
        assert!(label.starts_with("Munich, "), "{label}");
        let areas = germany.city_postal_areas(&label);
        assert!(areas.len() > 20, "{}", areas.len());
        assert_eq!(germany.city_postal_count(&label), areas.len());
        assert!(germany.city_area(&label).is_some());
        assert!(germany.cities("Bavaria").iter().any(|city| city.label == label));
    }

    #[test]
    fn towns_sharing_a_name_are_one_city() {
        // Maharashtra has two towns called Jalgaon; the picker needs each
        // label once, and picking it covers both towns' postal codes.
        let india = country("India").expect("India");
        for state in ["Maharashtra", "Kerala", "Karnataka"] {
            let cities = india.cities(state);
            let labels: std::collections::HashSet<_> = cities.iter().map(|city| &city.label).collect();
            assert_eq!(labels.len(), cities.len(), "{state}");
        }
        let germany = country("Germany").expect("Germany");
        let amberg = germany.cities("Bavaria").into_iter().find(|city| city.label == "Amberg, Bavaria").expect("Amberg");
        let towns = germany.find_towns(&amberg.label);
        assert!(towns.len() > 1);
        assert_eq!(germany.city_postal_count(&amberg.label), amberg.zips as usize);
        assert!(amberg.zips >= germany.town_postal[towns[0]]);
    }

    #[test]
    fn fine_codes_are_merged_to_useful_areas() {
        // Portugal lists ~200,000 street-level codes; a whole-country search
        // should be a few thousand lookups, like a US state.
        let portugal = country("Portugal").expect("Portugal");
        assert!(portugal.postal.len() < 10_000, "{}", portugal.postal.len());
        assert!(portugal.postal_place(&portugal.postal[0].code.to_owned()).is_some());
    }
}
