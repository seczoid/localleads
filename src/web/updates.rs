//! The sidebar's update card, and installing an update from it.

use super::{AppError, WebResult, notify, render};
use crate::app::SharedState;
use crate::update::Sidebar;
use askama::Template;
use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;

#[derive(Template)]
#[template(path = "partials/update_card.html")]
struct UpdateCard {
    u: Sidebar,
}

/// The card, refreshed when the update's state changes.
pub async fn card() -> WebResult {
    render(UpdateCard {
        u: crate::update::sidebar(),
    })
}

/// Downloads, checks and installs the update, then restarts into it.
pub async fn start(State(state): State<SharedState>) -> WebResult {
    state
        .start_update()
        .map_err(|error| AppError::conflict(error.to_string()))?;
    Ok(notify("info", "Downloading the update…", &["update-changed"]))
}

/// Hides the note about an update that didn't finish.
pub async fn dismiss(State(state): State<SharedState>) -> WebResult {
    crate::update::dismiss_failure();
    state.update_changed();
    Ok((
        [
            ("HX-Trigger", axum::http::HeaderValue::from_static("update-changed")),
            ("HX-Reswap", axum::http::HeaderValue::from_static("none")),
        ],
        "",
    )
        .into_response())
}

/// Looks for a new version now.
pub async fn check(State(state): State<SharedState>) -> WebResult {
    if !crate::update::enabled() {
        return Ok(notify("info", "Update checks are turned off (LOCALLEADS_UPDATES=off)", &[]));
    }
    let result = crate::update::check_now(&state.db_path).await;
    state.update_changed();
    let sidebar = crate::update::sidebar();
    Ok(match result {
        Err(error) => notify("error", &format!("Couldn't reach GitHub: {error:#}"), &[]),
        Ok(()) if sidebar.update.is_empty() => {
            notify("ok", &format!("You're on the newest version (v{})", sidebar.version), &[])
        }
        Ok(()) => notify("ok", &format!("v{} is out", sidebar.update), &["update-changed"]),
    })
}

/// Which version is serving, so a restarting page knows when it's back.
pub async fn status() -> impl IntoResponse {
    let sidebar = crate::update::sidebar();
    Json(serde_json::json!({
        "version": sidebar.version,
        "restarting": sidebar.restarting,
        "busy": !sidebar.busy.is_empty(),
        "failed": !sidebar.failed.is_empty(),
    }))
}
