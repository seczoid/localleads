use super::views::{HealthBar, JobView, LeadView, NoticeView, StatsView};
use super::{WebResult, render};
use crate::app::SharedState;
use crate::db::LeadQuery;
use askama::Template;
use axum::extract::State;

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    nav: &'static str,
    stats: StatsView,
    jobs: Vec<JobView>,
    leads: Vec<LeadView>,
    health: HealthBar,
    notices: Vec<NoticeView>,
}

#[derive(Template)]
#[template(path = "partials/dashboard_live.html")]
struct DashboardLive {
    stats: StatsView,
    jobs: Vec<JobView>,
    leads: Vec<LeadView>,
    health: HealthBar,
    notices: Vec<NoticeView>,
}

fn load(state: &SharedState) -> WebResult<DashboardLive> {
    let stats = StatsView::from(state.db.stats()?);
    let jobs = state
        .db
        .list_jobs()?
        .iter()
        .take(8)
        .map(|job| JobView::new(job, state.is_live(&job.id)))
        .collect();
    let leads = state
        .db
        .query_leads(&LeadQuery {
            limit: 6,
            ..LeadQuery::default()
        })?
        .into_iter()
        .map(LeadView::from)
        .collect();
    let health = HealthBar::new(&state.db.proxy_status_counts(None)?);
    let notices = state.notices(8).into_iter().map(NoticeView::from).collect();
    Ok(DashboardLive {
        stats,
        jobs,
        leads,
        health,
        notices,
    })
}

pub async fn page(State(state): State<SharedState>) -> WebResult {
    let live = load(&state)?;
    render(DashboardPage {
        nav: "dashboard",
        stats: live.stats,
        jobs: live.jobs,
        leads: live.leads,
        health: live.health,
        notices: live.notices,
    })
}

pub async fn live(State(state): State<SharedState>) -> WebResult {
    render(load(&state)?)
}
