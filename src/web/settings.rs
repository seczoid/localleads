use super::views::{NoticeView, StatsView, fmt_bytes};
use super::{AppError, WebResult, notify, render};
use crate::app::SharedState;
use askama::Template;
use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum_extra::extract::Form;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    nav: &'static str,
    db_path: String,
    data_dir: String,
    db_size: String,
    default_backup: String,
    version: &'static str,
    stats: StatsView,
    notices: Vec<NoticeView>,
    email: String,
}

fn default_backup_path(database_path: &Path) -> PathBuf {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    database_path.with_file_name(format!("localleads-backup-{timestamp}.sqlite"))
}

pub async fn page(
    State(state): State<SharedState>,
    account: Option<axum::Extension<crate::db::Account>>,
) -> WebResult {
    render(SettingsPage {
        nav: "settings",
        email: account.map(|account| account.0.email).unwrap_or_default(),
        db_path: state.db_path.display().to_string(),
        data_dir: state
            .db_path
            .parent()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        db_size: state.db.file_size().map(fmt_bytes).unwrap_or_default(),
        default_backup: default_backup_path(&state.db_path).display().to_string(),
        version: env!("CARGO_PKG_VERSION"),
        stats: state.db.stats()?.into(),
        notices: state
            .notices(50)
            .into_iter()
            .map(NoticeView::from)
            .collect(),
    })
}

#[derive(Deserialize)]
pub struct BackupForm {
    #[serde(default)]
    path: String,
}

pub async fn backup(State(state): State<SharedState>, Form(form): Form<BackupForm>) -> WebResult {
    let destination = if form.path.trim().is_empty() {
        default_backup_path(&state.db_path)
    } else {
        PathBuf::from(form.path.trim())
    };
    if destination == state.db_path {
        return Err(AppError::bad_request(
            "Choose a path other than the live database",
        ));
    }
    let db = state.db.clone();
    let target = destination.clone();
    tokio::task::spawn_blocking(move || db.backup_to(&target))
        .await
        .map_err(anyhow::Error::from)??;
    let message = format!("Backup written to {}", destination.display());
    state.notice("ok", message.clone());
    Ok(notify("ok", &message, &[]))
}

/// Streams a consistent snapshot of the database to the browser.
pub async fn download_backup(State(state): State<SharedState>) -> WebResult {
    let db = state.db.clone();
    let snapshot = state.db_path.with_file_name(format!(
        ".localleads-download-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let target = snapshot.clone();
    let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        db.backup_to(&target)?;
        let bytes = std::fs::read(&target)?;
        let _ = std::fs::remove_file(&target);
        Ok(bytes)
    })
    .await
    .map_err(anyhow::Error::from)?;
    let _ = std::fs::remove_file(&snapshot);
    let bytes = bytes?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.sqlite3".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"localleads-backup-{}.sqlite\"",
                    chrono::Local::now().format("%Y%m%d-%H%M%S")
                ),
            ),
        ],
        Body::from(bytes),
    )
        .into_response())
}
