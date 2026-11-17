use super::table::{TablePage, TableParams};
use super::views::{
    CheckerView, GroupView, HealthBar, ProgressView, ProxyCheckView, ProxyView, fmt_count,
};
use super::{AppError, RenderHtml, WebResult, flag, notify, redirect, render};
use crate::app::{CheckerSettings, SharedState};
use crate::db::ProxyQuery;
use crate::proxy::{import_proxy_text, normalize_protocol};
use askama::Template;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum_extra::extract::Form;
use serde::Deserialize;

#[derive(Deserialize, Default, Clone, Debug)]
pub struct ProxyFilters {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub enabled: String,
    #[serde(default)]
    pub q: String,
}

impl ProxyFilters {
    fn query(&self, limit: usize, offset: usize) -> ProxyQuery {
        ProxyQuery {
            group_id: Some(self.group.clone()).filter(|value| !value.is_empty()),
            status: Some(self.status.clone()).filter(|value| !value.is_empty()),
            enabled: match self.enabled.as_str() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            },
            search: self.q.clone(),
            sort: String::new(),
            limit,
            offset,
        }
    }

    fn is_filtered(&self) -> bool {
        !(self.q.trim().is_empty() && self.status.is_empty() && self.enabled.is_empty())
    }
}

/// Sort keys for the proxies table's columns, in column order; the
/// checkbox and groups columns don't sort.
const PROXY_SORT_KEYS: [&str; 7] = ["", "proxy", "status", "latency", "", "checked", "error"];

/// One page of the proxies table (DataTables server-side protocol).
pub async fn data(
    State(state): State<SharedState>,
    Query(filters): Query<ProxyFilters>,
    Query(params): Query<TableParams>,
) -> WebResult {
    let page = tokio::task::spawn_blocking(move || -> anyhow::Result<TablePage<ProxyView>> {
        let mut query = filters.query(params.limit(), params.start);
        query.sort = params.sort(&PROXY_SORT_KEYS);
        let filtered = state.db.count_proxies(&query)?;
        let in_group = ProxyFilters {
            group: filters.group.clone(),
            ..ProxyFilters::default()
        };
        let total = if filters.is_filtered() {
            state.db.count_proxies(&in_group.query(0, 0))?
        } else {
            filtered
        };
        let names = state.db.proxy_group_names()?;
        let data = state
            .db
            .query_proxies(&query)?
            .iter()
            .map(|proxy| ProxyView::new(proxy, &names))
            .collect();
        Ok(TablePage {
            draw: params.draw,
            records_total: total,
            records_filtered: filtered,
            data,
        })
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(axum::Json(page).into_response())
}

#[derive(Template)]
#[template(path = "partials/proxy_groups.html")]
struct GroupTabs {
    groups: Vec<GroupView>,
    all_count: String,
    all_active: bool,
    current_name: String,
    current_id: String,
    health: HealthBar,
}

impl GroupTabs {
    fn load(state: &SharedState, filters: &ProxyFilters) -> WebResult<Self> {
        let current = Some(filters.group.as_str()).filter(|value| !value.is_empty());
        let groups = state.db.list_proxy_groups()?;
        let current_name = current
            .and_then(|id| groups.iter().find(|group| group.id == id))
            .map(|group| group.name.clone())
            .unwrap_or_default();
        Ok(Self {
            all_count: fmt_count(state.db.count_proxies(&ProxyQuery::default())?),
            all_active: current.is_none(),
            groups: groups
                .iter()
                .map(|group| GroupView::new(group, current))
                .collect(),
            current_id: current.unwrap_or_default().to_owned(),
            current_name,
            health: HealthBar::new(&state.db.proxy_status_counts(current)?),
        })
    }
}

#[derive(Template)]
#[template(path = "partials/proxy_progress.html")]
struct ProgressPartial {
    progress: ProgressView,
}

#[derive(Template)]
#[template(path = "proxies.html")]
struct ProxiesPage {
    nav: &'static str,
    filters: ProxyFilters,
    tabs: String,
    tabs_current_id: String,
    tabs_current_name: String,
    progress: String,
    checker: CheckerView,
    all_groups: Vec<GroupView>,
}

pub async fn page(
    State(state): State<SharedState>,
    Query(filters): Query<ProxyFilters>,
) -> WebResult {
    let tabs = GroupTabs::load(&state, &filters)?;
    let all_groups = state
        .db
        .list_proxy_groups()?
        .iter()
        .map(|group| GroupView::new(group, None))
        .collect();
    let progress = ProgressPartial {
        progress: ProgressView::new(&state.proxy_check_progress(), state.import_running()),
    }
    .render_html()?;
    render(ProxiesPage {
        nav: "proxies",
        progress,
        checker: state.checker_settings().into(),
        tabs_current_id: tabs.current_id.clone(),
        tabs_current_name: tabs.current_name.clone(),
        tabs: tabs.render_html()?,
        filters,
        all_groups,
    })
}

pub async fn groups(
    State(state): State<SharedState>,
    Query(filters): Query<ProxyFilters>,
) -> WebResult {
    render(GroupTabs::load(&state, &filters)?)
}

pub async fn progress(State(state): State<SharedState>) -> WebResult {
    render(ProgressPartial {
        progress: ProgressView::new(&state.proxy_check_progress(), state.import_running()),
    })
}

#[derive(Template)]
#[template(path = "partials/proxy_history.html")]
struct HistoryModal {
    proxy: ProxyView,
    checks: Vec<ProxyCheckView>,
}

pub async fn history(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let proxy = state
        .db
        .proxies_by_ids(std::slice::from_ref(&id))?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::not_found("Proxy not found"))?;
    let names = state.db.proxy_group_names()?;
    render(HistoryModal {
        proxy: ProxyView::new(&proxy, &names),
        checks: state
            .db
            .proxy_checks(&id, 25)?
            .into_iter()
            .map(ProxyCheckView::from)
            .collect(),
    })
}

// --------------------------------------------------------------- imports

#[derive(Deserialize)]
pub struct AddForm {
    #[serde(default)]
    text: String,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    group: String,
}

fn protocol_or_default(value: &str) -> WebResult<String> {
    if value.trim().is_empty() {
        return Ok("http".to_owned());
    }
    normalize_protocol(value).map_err(|error| AppError::bad_request(error.to_string()))
}

pub async fn add(State(state): State<SharedState>, Form(form): Form<AddForm>) -> WebResult {
    let protocol = protocol_or_default(&form.protocol)?;
    if form.text.trim().is_empty() {
        return Err(AppError::bad_request("Paste at least one proxy"));
    }
    let before: std::collections::HashSet<String> = state
        .db
        .list_proxies()?
        .into_iter()
        .map(|proxy| proxy.id)
        .collect();
    let summary = import_proxy_text(&state.db, &form.text, &protocol)?;
    if !form.group.is_empty() && summary.imported > 0 {
        let new_ids: Vec<String> = state
            .db
            .list_proxies()?
            .into_iter()
            .map(|proxy| proxy.id)
            .filter(|id| !before.contains(id))
            .collect();
        state.db.add_proxies_to_group(&form.group, &new_ids)?;
    }
    let _ = state.refresh_running_pools();
    state.mark_proxies_dirty();
    let mut message = format!(
        "Added {} proxies, skipped {} duplicates",
        summary.imported, summary.duplicates
    );
    if !summary.invalid.is_empty() {
        message.push_str(&format!(
            "; {} invalid ({})",
            summary.invalid.len(),
            summary
                .invalid
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let level = if summary.imported == 0 && !summary.invalid.is_empty() {
        "error"
    } else {
        "ok"
    };
    Ok(notify(
        level,
        &message,
        &["proxies-changed", "close-dialog"],
    ))
}

pub async fn import(State(state): State<SharedState>, mut multipart: Multipart) -> WebResult {
    let mut protocol = "http".to_owned();
    let mut text = String::new();
    let mut label = "proxy file".to_owned();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| AppError::bad_request(error.to_string()))?
    {
        match field.name().unwrap_or_default() {
            "protocol" => {
                protocol = protocol_or_default(
                    &field
                        .text()
                        .await
                        .map_err(|error| AppError::bad_request(error.to_string()))?,
                )?;
            }
            "file" => {
                if let Some(name) = field.file_name() {
                    label = name.to_owned();
                }
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|error| AppError::bad_request(error.to_string()))?;
                text = String::from_utf8_lossy(&bytes).into_owned();
            }
            _ => {}
        }
    }
    if text.trim().is_empty() {
        return Err(AppError::bad_request("Choose a non-empty proxy file"));
    }
    state
        .import_proxies(label.clone(), text, protocol)
        .map_err(|error| AppError::conflict(error.to_string()))?;
    Ok(notify(
        "info",
        &format!("Importing {label}…"),
        &["proxies-changed", "close-dialog"],
    ))
}

// ------------------------------------------------------------ bulk tools

#[derive(Deserialize, Default)]
pub struct BulkForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    all_matching: Option<String>,
    #[serde(default)]
    target_group: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    enabled: String,
    #[serde(default)]
    q: String,
}

impl BulkForm {
    fn filters(&self) -> ProxyFilters {
        ProxyFilters {
            group: self.group.clone(),
            status: self.status.clone(),
            enabled: self.enabled.clone(),
            q: self.q.clone(),
        }
    }
}

pub async fn bulk(State(state): State<SharedState>, Form(form): Form<BulkForm>) -> WebResult {
    let filters = form.filters();
    let everything =
        flag(&form.all_matching) || matches!(
            form.action.as_str(),
            "check_all" | "keep_healthy" | "delete_failed"
        );
    let proxies = if everything {
        state.db.query_all_proxies(&filters.query(0, 0))?
    } else {
        state.db.proxies_by_ids(&form.ids)?
    };
    let ids: Vec<String> = proxies.iter().map(|proxy| proxy.id.clone()).collect();
    if ids.is_empty() {
        return Err(AppError::bad_request("No proxies match this selection"));
    }
    let count = ids.len();
    let message = match form.action.as_str() {
        "check" | "check_all" => {
            state
                .start_proxy_check(proxies)
                .map_err(|error| AppError::conflict(error.to_string()))?;
            return Ok(notify(
                "info",
                &format!("Checking {} proxies", fmt_count(count as i64)),
                &["proxies-changed", "selection-clear"],
            ));
        }
        "enable" | "disable" => {
            let enabled = form.action == "enable";
            for id in &ids {
                state.db.set_proxy_enabled(id, enabled)?;
            }
            format!(
                "{} {count} proxies",
                if enabled { "Enabled" } else { "Disabled" }
            )
        }
        "delete" => {
            if state.proxy_check_progress().running {
                return Err(AppError::conflict("Wait for the running check to finish"));
            }
            state.db.delete_proxies(&ids)?;
            format!("Deleted {count} proxies")
        }
        "keep_healthy" | "delete_failed" => {
            if state.proxy_check_progress().running {
                return Err(AppError::conflict("Wait for the running check to finish"));
            }
            let keep_healthy = form.action == "keep_healthy";
            let doomed: Vec<String> = proxies
                .iter()
                .filter(|proxy| {
                    if keep_healthy {
                        proxy.status != "healthy"
                    } else {
                        proxy.status == "failed"
                    }
                })
                .map(|proxy| proxy.id.clone())
                .collect();
            if doomed.is_empty() {
                return Ok(notify(
                    "info",
                    if keep_healthy {
                        "Every proxy in this list is healthy"
                    } else {
                        "No failed proxies in this list"
                    },
                    &[],
                ));
            }
            state.db.delete_proxies(&doomed)?;
            format!(
                "Deleted {} {} proxies",
                fmt_count(doomed.len() as i64),
                if keep_healthy { "unhealthy" } else { "failed" }
            )
        }
        "assign" => {
            if form.target_group.is_empty() {
                return Err(AppError::bad_request("Choose a group to add proxies to"));
            }
            state.db.add_proxies_to_group(&form.target_group, &ids)?;
            format!("Added {count} proxies to the group")
        }
        "unassign" => {
            if form.group.is_empty() {
                return Err(AppError::bad_request(
                    "Open a group before removing proxies from it",
                ));
            }
            state.db.remove_proxies_from_group(&form.group, &ids)?;
            format!("Removed {count} proxies from this group")
        }
        other => return Err(AppError::bad_request(format!("Unknown action '{other}'"))),
    };
    let _ = state.refresh_running_pools();
    state.mark_proxies_dirty();
    Ok(notify(
        "ok",
        &message,
        &["proxies-changed", "selection-clear"],
    ))
}

pub async fn cancel_check(State(state): State<SharedState>) -> WebResult {
    state.cancel_proxy_check();
    Ok(notify(
        "info",
        "Cancelling proxy check…",
        &["proxies-changed"],
    ))
}

#[derive(Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    timeout_secs: String,
    #[serde(default)]
    retries: String,
    #[serde(default)]
    max_latency_ms: String,
    #[serde(default)]
    expected_body: String,
    #[serde(default)]
    verify_exit_ip: Option<String>,
    #[serde(default)]
    concurrency: String,
}

pub async fn save_settings(
    State(state): State<SharedState>,
    Form(form): Form<SettingsForm>,
) -> WebResult {
    fn number<T: std::str::FromStr>(value: &str, label: &str, default: T) -> WebResult<T> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(default);
        }
        value
            .parse()
            .map_err(|_| AppError::bad_request(format!("{label} must be a whole number")))
    }
    let defaults = CheckerSettings::default();
    let settings = state.save_checker_settings(CheckerSettings {
        endpoint: form.endpoint,
        timeout_secs: number(&form.timeout_secs, "Timeout", defaults.timeout_secs)?,
        retries: number(&form.retries, "Retries", defaults.retries)?,
        max_latency_ms: number(
            &form.max_latency_ms,
            "Slow threshold",
            defaults.max_latency_ms,
        )?,
        expected_body: form.expected_body,
        verify_exit_ip: flag(&form.verify_exit_ip),
        concurrency: number(&form.concurrency, "Concurrency", defaults.concurrency)?,
    })?;
    Ok(notify(
        "ok",
        &format!(
            "Checker settings saved (concurrency {}, timeout {}s)",
            settings.concurrency, settings.timeout_secs
        ),
        &[],
    ))
}

// ---------------------------------------------------------------- groups

#[derive(Deserialize)]
pub struct GroupForm {
    #[serde(default)]
    name: String,
}

pub async fn create_group(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Form(form): Form<GroupForm>,
) -> WebResult {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("Group name cannot be empty"));
    }
    if state
        .db
        .list_proxy_groups()?
        .iter()
        .any(|group| group.name.eq_ignore_ascii_case(name))
    {
        return Err(AppError::conflict(format!(
            "A group named \"{name}\" already exists"
        )));
    }
    let id = uuid::Uuid::new_v4().to_string();
    state.db.create_proxy_group(&id, name)?;
    state.notice("ok", format!("Created proxy group \"{name}\""));
    Ok(redirect(&headers, &format!("/proxies?group={id}")))
}

pub async fn rename_group(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Form(form): Form<GroupForm>,
) -> WebResult {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("Group name cannot be empty"));
    }
    state.db.rename_proxy_group(&id, name)?;
    Ok(notify(
        "ok",
        &format!("Renamed group to \"{name}\""),
        &["proxies-changed", "close-dialog"],
    ))
}

pub async fn delete_group(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult {
    if state.group_in_use(&id)? {
        return Err(AppError::conflict(
            "This group is attached to an active search",
        ));
    }
    state.db.delete_proxy_group(&id)?;
    state.notice("ok", "Proxy group deleted; its proxies were kept");
    Ok(redirect(&headers, "/proxies"))
}
