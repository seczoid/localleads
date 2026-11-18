//! Searches, the home page: what's running now, then every search with its
//! lead counts, under an "All leads" row.

use super::views::{JobView, StagesView, fmt_ago, fmt_count, percent};
use super::{RenderHtml, WebResult, render};
use crate::app::SharedState;
use askama::Template;
use axum::extract::State;
use axum::response::{Html, IntoResponse};

struct Totals {
    leads: String,
    searches: String,
    proxies: String,
    healthy: String,
    has_proxies: bool,
}

struct RunningView {
    job: JobView,
    stages: StagesView,
}

struct SearchRow {
    job: JobView,
    leads: String,
    has_leads: bool,
    shared: String,
    phone_pct: u32,
    website_pct: u32,
    updated: String,
}

struct AllRow {
    leads: String,
    has_leads: bool,
    phone_pct: u32,
    website_pct: u32,
    places: String,
    categories: String,
}

#[derive(Template)]
#[template(path = "searches.html")]
struct SearchesPage {
    nav: &'static str,
    totals: Totals,
    oob: bool,
    body: String,
}

#[derive(Template)]
#[template(path = "partials/searches_live.html")]
struct SearchesLive {
    totals: Totals,
    running: Vec<RunningView>,
    rows: Vec<SearchRow>,
    all: AllRow,
    oob: bool,
}

fn load(state: &SharedState, oob: bool) -> WebResult<SearchesLive> {
    let stats = state.db.stats()?;
    let jobs = state.db.list_jobs()?;
    let list_stats = state.db.lead_list_stats()?;
    let (categories, cities) = state.db.lead_overview()?;
    let totals = Totals {
        leads: fmt_count(stats.leads),
        searches: fmt_count(stats.jobs),
        proxies: fmt_count(stats.proxies),
        healthy: fmt_count(stats.healthy_proxies),
        has_proxies: stats.proxies > 0,
    };
    let running = jobs
        .iter()
        .filter(|job| job.status.is_active())
        .map(|job| {
            Ok(RunningView {
                job: JobView::new(job, state.is_live(&job.id)),
                stages: super::jobs::job_stages(state, job)?,
            })
        })
        .collect::<WebResult<Vec<_>>>()?;
    let rows = jobs
        .iter()
        .map(|job| {
            let list = list_stats.get(&job.id).cloned().unwrap_or_default();
            SearchRow {
                job: JobView::new(job, state.is_live(&job.id)),
                leads: fmt_count(list.leads),
                has_leads: list.leads > 0,
                shared: if list.leads > list.first_found {
                    fmt_count(list.leads - list.first_found)
                } else {
                    String::new()
                },
                phone_pct: percent(list.with_phone, list.leads),
                website_pct: percent(list.with_website, list.leads),
                updated: fmt_ago(
                    list.last_found
                        .as_deref()
                        .or(job.finished_at.as_deref())
                        .or(job.started_at.as_deref()),
                ),
            }
        })
        .collect();
    let all = AllRow {
        leads: fmt_count(stats.leads),
        has_leads: stats.leads > 0,
        phone_pct: percent(stats.leads_with_phone, stats.leads),
        website_pct: percent(stats.leads_with_website, stats.leads),
        places: fmt_count(cities),
        categories: fmt_count(i64::try_from(categories.len()).unwrap_or(i64::MAX)),
    };
    Ok(SearchesLive {
        totals,
        running,
        rows,
        all,
        oob,
    })
}

pub async fn page(State(state): State<SharedState>) -> WebResult {
    let live = load(&state, false)?;
    let body = live.render_html()?;
    render(SearchesPage {
        nav: "jobs",
        totals: live.totals,
        oob: false,
        body,
    })
}

pub async fn live(State(state): State<SharedState>) -> WebResult {
    let html = state.shared_render("searches", || load(&state, true)?.render_html())?;
    Ok(Html(html).into_response())
}
