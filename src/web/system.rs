//! The System page: how the server is doing and what it has logged since
//! it started.

use super::views::{fmt_bytes, fmt_count, fmt_duration};
use super::{WebResult, render};
use crate::app::SharedState;
use crate::serverlog;
use askama::Template;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use serde::Deserialize;

#[derive(Template)]
#[template(path = "system.html")]
struct SystemPage {
    nav: &'static str,
    version: &'static str,
    started: String,
    uptime: String,
    pid: u32,
    db_path: String,
    db_size: String,
    leads: String,
    searches: String,
    running: usize,
}

pub async fn page(State(state): State<SharedState>) -> WebResult {
    let started = serverlog::started();
    let stats = state.db.stats()?;
    render(SystemPage {
        nav: "system",
        version: env!("CARGO_PKG_VERSION"),
        started: started.format("%Y-%m-%d %H:%M").to_string(),
        uptime: fmt_duration((chrono::Local::now() - started).num_seconds().max(0)),
        pid: std::process::id(),
        db_path: state.db_path.display().to_string(),
        db_size: state.db.file_size().map(fmt_bytes).unwrap_or_default(),
        leads: fmt_count(stats.leads),
        searches: fmt_count(stats.jobs),
        running: state.running_searches(),
    })
}

#[derive(Deserialize)]
pub struct LinesQuery {
    #[serde(default)]
    after: u64,
}

/// Log lines after `after`, oldest first, for the page to append.
pub async fn lines(Query(query): Query<LinesQuery>) -> impl IntoResponse {
    Json(serde_json::json!({
        "lines": serverlog::lines_after(query.after, 3000),
        "dropped": serverlog::dropped(),
    }))
}

#[derive(Deserialize)]
pub struct TextQuery {
    #[serde(default)]
    level: String,
    #[serde(default)]
    q: String,
}

pub async fn log_txt(Query(query): Query<TextQuery>) -> impl IntoResponse {
    let level = query.level.trim().to_ascii_lowercase();
    let find = query.q.trim().to_lowercase();
    let mut text = String::new();
    for line in serverlog::lines_after(0, usize::MAX) {
        if (!level.is_empty() && line.level != level)
            || (!find.is_empty() && !line.message.to_lowercase().contains(&find))
        {
            continue;
        }
        text.push_str(&format!("{}  {:<5}  {}\n", line.time, line.level.to_uppercase(), line.message));
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"localleads-server-{stamp}.txt\""),
            ),
        ],
        text,
    )
}
