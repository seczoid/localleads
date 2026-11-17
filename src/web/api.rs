//! Read-only JSON endpoints for scripts and integrations.

use super::leads::LeadFilters;
use super::{AppError, WebResult};
use crate::app::SharedState;
use axum::Json;
use axum::extract::{Path, State};
use axum_extra::extract::Query as FormQuery;
use axum::response::IntoResponse;
use serde::Deserialize;

pub async fn stats(State(state): State<SharedState>) -> WebResult {
    let stats = state.db.stats()?;
    let progress = state.proxy_check_progress();
    Ok(Json(serde_json::json!({ "stats": stats, "proxy_check": progress })).into_response())
}

pub async fn jobs(State(state): State<SharedState>) -> WebResult {
    let jobs: Vec<_> = state
        .db
        .list_jobs()?
        .into_iter()
        .map(|job| {
            let live = state.is_live(&job.id);
            serde_json::json!({ "job": job, "live": live })
        })
        .collect();
    Ok(Json(jobs).into_response())
}

pub async fn job(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let job = state
        .db
        .get_job(&id)?
        .ok_or_else(|| AppError::not_found("Search not found"))?;
    let events: Vec<_> = state
        .db
        .job_event_rows(&id, 100)?
        .into_iter()
        .map(|event| {
            serde_json::json!({ "at": event.created_at, "level": event.level, "message": event.message })
        })
        .collect();
    Ok(
        Json(serde_json::json!({ "job": job, "live": state.is_live(&id), "events": events }))
            .into_response(),
    )
}

#[derive(Deserialize, Default)]
pub struct Paging {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

pub async fn leads(
    State(state): State<SharedState>,
    FormQuery(filters): FormQuery<LeadFilters>,
    FormQuery(paging): FormQuery<Paging>,
) -> WebResult {
    let limit = paging.limit.unwrap_or(100).clamp(1, 1000);
    let offset = paging.offset.unwrap_or(0);
    let total = state.db.count_leads(&filters.query(0, 0))?;
    let leads = state.db.query_leads(&filters.query(limit, offset))?;
    Ok(Json(serde_json::json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "leads": leads,
    }))
    .into_response())
}
