//! DataTables server-side protocol, shared by the leads and proxies tables.
//! The browser holds one page; filtering, sorting and paging run in SQLite.

use serde::{Deserialize, Serialize};

/// Largest page a client may ask for.
const MAX_PAGE: usize = 1_000;

#[derive(Deserialize, Default)]
pub struct TableParams {
    #[serde(default)]
    pub draw: u64,
    #[serde(default)]
    pub start: usize,
    #[serde(default)]
    length: Option<i64>,
    /// Index of the column to sort by.
    #[serde(default)]
    order_col: Option<usize>,
    #[serde(default)]
    order_dir: String,
    /// The sorted column's key, when the table names its columns; wins
    /// over `order_col` so extra columns (checkboxes, row numbers) don't
    /// shift the mapping.
    #[serde(default)]
    order_key: String,
}

impl TableParams {
    pub fn limit(&self) -> usize {
        usize::try_from(self.length.unwrap_or(50).max(1))
            .unwrap_or(50)
            .min(MAX_PAGE)
    }

    /// "key:asc|desc" for the sorted column, using `keys[column]`; empty when
    /// the column is unknown or not sortable.
    pub fn sort(&self, keys: &[&str]) -> String {
        let named = keys
            .iter()
            .find(|key| !key.is_empty() && **key == self.order_key);
        let Some(key) = named.or_else(|| {
            self.order_col
                .and_then(|column| keys.get(column))
                .filter(|key| !key.is_empty())
        }) else {
            return String::new();
        };
        let dir = if self.order_dir.eq_ignore_ascii_case("desc") {
            "desc"
        } else {
            "asc"
        };
        format!("{key}:{dir}")
    }
}

#[derive(Serialize)]
pub struct TablePage<T: Serialize> {
    pub draw: u64,
    /// Rows before the user's filters; DataTables uses it to tell "nothing
    /// here yet" from "nothing matches".
    #[serde(rename = "recordsTotal")]
    pub records_total: i64,
    #[serde(rename = "recordsFiltered")]
    pub records_filtered: i64,
    pub data: Vec<T>,
}
