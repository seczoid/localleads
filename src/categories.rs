//! Google Maps business categories for the "what are you looking for" picker,
//! from data/categories.tsv (most common first within each group).

use std::sync::LazyLock;

use serde::Serialize;

const SOURCE: &str = include_str!("../data/categories.tsv");

#[derive(Serialize)]
pub struct Group {
    pub key: &'static str,
    pub label: &'static str,
    pub items: Vec<&'static str>,
}

/// Every group, alphabetically by label.
pub fn groups() -> &'static [Group] {
    static GROUPS: LazyLock<Vec<Group>> = LazyLock::new(|| {
        let mut groups: Vec<Group> = Vec::new();
        for line in SOURCE.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let Some((key, category)) = line.split_once('\t') else {
                continue;
            };
            let category = category.trim();
            if category.is_empty() {
                continue;
            }
            match groups.iter_mut().find(|group| group.key == key) {
                Some(group) => group.items.push(category),
                None => groups.push(Group {
                    key,
                    label: label(key),
                    items: vec![category],
                }),
            }
        }
        groups.sort_by_key(|group| group.label);
        groups
    });
    &GROUPS
}

/// The picker's data as JSON, built once.
pub fn json() -> &'static str {
    static JSON: LazyLock<String> =
        LazyLock::new(|| serde_json::json!({ "groups": groups() }).to_string());
    &JSON
}

/// Where the page loads the list from; the address changes with the list,
/// so a long browser cache never serves an old one.
pub fn url() -> &'static str {
    static URL: LazyLock<String> = LazyLock::new(|| {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        SOURCE.hash(&mut hasher);
        format!("/search/categories.json?v={:x}", hasher.finish() & 0xffff_ffff)
    });
    &URL
}

/// Categories matching `query`: whole name first, then names starting with
/// it, then a word starting with it, then containing it; common ones first.
pub fn search(query: &str, limit: usize) -> Vec<&'static str> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<(u8, usize, &'static str)> = Vec::new();
    for group in groups() {
        for (rank, &item) in group.items.iter().enumerate() {
            let name = item.to_lowercase();
            let score = if name == query {
                0
            } else if name.starts_with(&query) {
                1
            } else if name.split([' ', '-', '/']).any(|word| word.starts_with(&query)) {
                2
            } else if name.contains(&query) {
                3
            } else {
                continue;
            };
            found.push((score, rank, item));
        }
    }
    found.sort();
    found.into_iter().take(limit).map(|(_, _, item)| item).collect()
}

fn label(key: &str) -> &'static str {
    match key {
        "agriculture" => "Agriculture",
        "artsEntertainment" => "Arts & entertainment",
        "automotive" => "Automotive",
        "beautyWellness" => "Beauty & wellness",
        "communityReligion" => "Community & religion",
        "construction" => "Construction",
        "education" => "Education",
        "eventsWeddings" => "Events & weddings",
        "financeInsurance" => "Finance & insurance",
        "fitnessSports" => "Fitness & sports",
        "foodDrink" => "Food & drink",
        "healthcare" => "Healthcare",
        "homeGarden" => "Home & garden",
        "legal" => "Legal",
        "manufacturingIndustry" => "Manufacturing & industry",
        "natureLandmarks" => "Nature & landmarks",
        "petsAnimals" => "Pets & animals",
        "professionalServices" => "Professional services",
        "publicServices" => "Public services",
        "realEstate" => "Real estate",
        "shoppingRetail" => "Shopping & retail",
        "technology" => "Technology",
        "transportLogistics" => "Transport & logistics",
        "travelHospitality" => "Travel & hospitality",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_has_a_label_and_categories() {
        let groups = groups();
        assert_eq!(groups.len(), 24);
        assert!(groups.iter().all(|group| group.label != "Other" && !group.items.is_empty()));
        let total: usize = groups.iter().map(|group| group.items.len()).sum();
        assert!(total > 4000, "{total}");
    }

    #[test]
    fn search_puts_close_matches_first() {
        assert_eq!(search("dentist", 1), ["dentist"]);
        let found = search("med", 50);
        assert!(found.iter().any(|item| *item == "medical laboratory"));
        assert!(found.iter().all(|item| item.contains("med")));
        assert!(search("zzzz", 5).is_empty());
    }
}
