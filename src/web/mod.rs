//! axum web server: routing, request guard, live updates, and static assets.

mod api;
mod account;
pub mod site;
pub use account::api_key;
mod dashboard;
mod guide;
mod jobs;
mod leads;
mod proxies;
mod table;
mod settings;
mod system;
mod updates;
pub mod views;

/// ProxyScrape affiliate link, shown in the sidebar and on the Proxies page.
/// LocalLeads earns a commission on purchases made through it.
pub const PROXYSCRAPE_URL: &str = "https://fas.st/t/R1CJJibL";

use crate::app::{SharedState, UiEvent};
use askama::Template;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

pub struct ServerOptions {
    pub addr: SocketAddr,
}

/// Which Host headers the server answers to.
#[derive(Clone)]
enum Guard {
    /// Bound to localhost: only loopback names (blocks DNS rebinding).
    Loopback,
    /// Served at a public URL: that host, plus loopback for health checks.
    Public(String),
    /// Bound to all interfaces with no public URL set: any host.
    Any,
}

fn router(state: SharedState, guard: Guard) -> Router {
    let guard = Arc::new(guard);
    // Open to anyone: first-run setup, sign-in, and what those pages load.
    let public = Router::new()
        .route("/setup", get(account::setup_page).post(account::setup))
        .route("/login", get(account::login_page).post(account::login))
        .route("/logout", post(account::logout))
        .route("/static/{*path}", get(static_file))
        .route("/healthz", get(|| async { "ok" }));
    // Everything else needs a signed-in session (the JSON API also takes an
    // API key). MCP has its own API-key check (see `crate::mcp::router`).
    let signed_in = Router::new()
        .route("/", get(dashboard::page))
        .route("/partials/searches", get(dashboard::live))
        .route("/jobs", get(jobs::list).post(jobs::create))
        .route("/jobs/new", get(jobs::new_form))
        .route("/jobs/preview", post(jobs::preview))
        .route("/jobs/{id}", get(jobs::detail))
        .route("/jobs/{id}/live", get(jobs::live))
        .route("/jobs/{id}/events", get(jobs::events))
        .route("/jobs/{id}/log.txt", get(jobs::log_txt))
        .route("/jobs/{id}/pause", post(jobs::toggle_pause))
        .route("/jobs/{id}/stop", post(jobs::stop))
        .route("/jobs/{id}/rerun", post(jobs::rerun))
        .route("/jobs/{id}/continue", post(jobs::continue_job))
        .route("/update", post(updates::start))
        .route("/update/check", post(updates::check))
        .route("/update/dismiss", post(updates::dismiss))
        .route("/update/card", get(updates::card))
        .route("/update/status", get(updates::status))
        .route("/jobs/{id}/delete", post(jobs::delete))
        .route("/locations/regions", get(jobs::regions))
        .route("/locations/cities", get(jobs::cities))
        .route("/search/categories.json", get(jobs::categories))
        .route("/leads", get(leads::browse))
        .route("/leads/browse", get(leads::old_browse))
        .route("/partials/leads/browse", get(leads::browse_partial))
        .route("/leads/data", get(leads::data))
        .route("/leads/export.csv", get(leads::export_csv))
        .route("/leads/export/columns", get(leads::export_columns))
        .route("/leads/export.json", get(leads::export_json))
        .route("/leads/export-disk", post(leads::export_disk))
        .route("/leads/bulk", post(leads::bulk))
        .route("/leads/{id}", get(leads::detail))
        .route("/leads/{id}/delete", post(leads::delete))
        .route("/proxies", get(proxies::page))
        .route("/proxies/data", get(proxies::data))
        .route("/partials/proxy-progress", get(proxies::progress))
        .route("/partials/proxy-groups", get(proxies::groups))
        .route("/proxies/add", post(proxies::add))
        .route("/proxies/import", post(proxies::import))
        .route("/proxies/bulk", post(proxies::bulk))
        .route("/proxies/check/cancel", post(proxies::cancel_check))
        .route("/proxies/settings", post(proxies::save_settings))
        .route("/proxies/{id}/history", get(proxies::history))
        .route("/groups", post(proxies::create_group))
        .route("/groups/{id}/rename", post(proxies::rename_group))
        .route("/groups/{id}/delete", post(proxies::delete_group))
        .route("/agents", get(guide::mcp))
        .route("/docs", get(guide::docs))
        .route("/system", get(system::page))
        .route("/system/log.json", get(system::lines))
        .route("/system/log.txt", get(system::log_txt))
        .route("/settings", get(settings::page))
        .route("/settings/backup", post(settings::backup))
        .route("/settings/backup.sqlite", get(settings::download_backup))
        .route("/account/password", post(account::change_password))
        .route("/agents/keys", post(guide::create_key))
        .route("/agents/keys/{id}/revoke", post(guide::revoke_key))
        .route("/trash/{id}/restore", post(restore))
        // JSON API
        .route("/api/stats", get(api::stats))
        .route("/api/jobs", get(api::jobs))
        .route("/api/jobs/{id}", get(api::job))
        .route("/api/leads", get(api::leads))
        // infrastructure
        .route("/events", get(events))
        .route_layer(middleware::from_fn_with_state(state.clone(), account::require_sign_in));
    Router::new()
        .merge(public)
        .merge(signed_in)
        .merge(crate::mcp::router(state.clone()))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(guard, guard_requests))
        .layer(middleware::from_fn(site::security_headers))
        .with_state(state)
}

pub async fn serve(state: SharedState, options: ServerOptions) -> anyhow::Result<()> {
    let guard = if let Some(host) = &site::site().public_host {
        Guard::Public(host.clone())
    } else if options.addr.ip().is_loopback() {
        Guard::Loopback
    } else {
        Guard::Any
    };
    let db_path = state.db_path.clone();
    let jobs = state.clone();
    let app = router(state, guard);
    let listener = bind(options.addr).await?;
    let _announcement = crate::mcp::announce(&db_path, &local_url(options.addr));
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            crate::serverlog::info!("Shutting down");
            // Running searches save what they finished and keep their
            // status, so the next start carries them on.
            jobs.suspend_jobs(std::time::Duration::from_secs(10)).await;
            crate::mcp::shutdown();
        })
        .await?;
    Ok(())
}

/// Binds `addr`. Right after a restart the old process may still hold the
/// port for a moment, so a busy port is tried again for a while.
async fn bind(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!("listen on {addr}")));
            }
        }
    }
}

/// Ctrl+C, or SIGTERM from `docker stop` and systemd.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match terminate.as_mut() {
                    Some(signal) => { signal.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// URL a local client uses to reach `addr`.
fn local_url(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        format!("http://127.0.0.1:{}", addr.port())
    } else {
        format!("http://{addr}")
    }
}

/// Blocks unknown Host names (DNS rebinding) and cross-site form posts.
async fn guard_requests(State(guard): State<Arc<Guard>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let allowed = match guard.as_ref() {
        Guard::Loopback => is_loopback_host(&host),
        Guard::Public(public) => site::host_name(&host) == *public || is_loopback_host(&host),
        Guard::Any => true,
    };
    if !allowed {
        return (StatusCode::MISDIRECTED_REQUEST, "Host not allowed").into_response();
    }
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        if headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            == Some("cross-site")
        {
            return (StatusCode::FORBIDDEN, "Cross-site request blocked").into_response();
        }
        if let Some(origin) = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
        {
            let matches = origin
                .split_once("://")
                .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(&host));
            if !matches {
                return (StatusCode::FORBIDDEN, "Cross-origin request blocked").into_response();
            }
        }
    }
    next.run(request).await
}

fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1") || name.ends_with(".localhost")
}

async fn events(State(state): State<SharedState>) -> impl IntoResponse {
    let stream = BroadcastStream::new(state.subscribe()).map(|message| {
        let event = match message {
            Ok(UiEvent::Jobs) => Event::default().event("jobs").data("1"),
            Ok(UiEvent::Proxies) => Event::default().event("proxies").data("1"),
            Ok(UiEvent::Leads) => Event::default().event("leads").data("1"),
            Ok(UiEvent::ProxyProgress) => Event::default().event("proxy-progress").data("1"),
            Ok(UiEvent::Update) => Event::default().event("update").data("1"),
            Ok(UiEvent::Notice(notice)) => Event::default()
                .event("notice")
                .json_data(notice)
                .unwrap_or_else(|_| Event::default().comment("bad notice")),
            Ok(UiEvent::JobFinished(finished)) => Event::default()
                .event("job-finished")
                .json_data(finished)
                .unwrap_or_else(|_| Event::default().comment("bad event")),
            // The client fell behind; ask it to refresh everything.
            Err(_) => Event::default().event("resync").data("1"),
        };
        Ok::<_, Infallible>(event)
    });
    // Ends with the server, so a shutdown doesn't wait on open browser tabs
    // (they reconnect to the next process on their own).
    let token = crate::mcp::shutdown_token().clone();
    let stream = futures::StreamExt::take_until(stream, async move { token.cancelled().await });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn static_file(Path(path): Path<String>) -> Response {
    let (body, content_type): (&'static [u8], &str) = match path.as_str() {
        "app.css" => (
            include_bytes!("../../static/app.css"),
            "text/css; charset=utf-8",
        ),
        "app.js" => (
            include_bytes!("../../static/app.js"),
            "text/javascript; charset=utf-8",
        ),
        "favicon.svg" => (include_bytes!("../../static/favicon.svg"), "image/svg+xml"),
        "vendor/htmx.min.js" => (
            include_bytes!("../../static/vendor/htmx.min.js"),
            "text/javascript; charset=utf-8",
        ),
        "vendor/jquery.min.js" => (
            include_bytes!("../../static/vendor/jquery.min.js"),
            "text/javascript; charset=utf-8",
        ),
        "vendor/datatables.min.js" => (
            include_bytes!("../../static/vendor/datatables.min.js"),
            "text/javascript; charset=utf-8",
        ),
        "vendor/alpine.min.js" => (
            include_bytes!("../../static/vendor/alpine.min.js"),
            "text/javascript; charset=utf-8",
        ),
        "fonts/plus-jakarta-sans.woff2" => (
            include_bytes!("../../static/fonts/plus-jakarta-sans.woff2"),
            "font/woff2",
        ),
        "fonts/ibm-plex-mono-400.woff2" => (
            include_bytes!("../../static/fonts/ibm-plex-mono-400.woff2"),
            "font/woff2",
        ),
        "fonts/ibm-plex-mono-500.woff2" => (
            include_bytes!("../../static/fonts/ibm-plex-mono-500.woff2"),
            "font/woff2",
        ),
        "docs/dashboard.png" => (include_bytes!("../../static/docs/dashboard.png"), "image/png"),
        "docs/new-search.png" => (include_bytes!("../../static/docs/new-search.png"), "image/png"),
        "docs/search-live.png" => (include_bytes!("../../static/docs/search-live.png"), "image/png"),
        "docs/leads-browse.png" => (include_bytes!("../../static/docs/leads-browse.png"), "image/png"),
        "docs/lead-detail.png" => (include_bytes!("../../static/docs/lead-detail.png"), "image/png"),
        "docs/proxies.png" => (include_bytes!("../../static/docs/proxies.png"), "image/png"),
        "docs/mcp.png" => (include_bytes!("../../static/docs/mcp.png"), "image/png"),
        "docs/settings.png" => (include_bytes!("../../static/docs/settings.png"), "image/png"),
        _ => return (StatusCode::NOT_FOUND, "Not found").into_response(),
    };
    let cache = if path.starts_with("vendor/") || path.starts_with("fonts/") || path.starts_with("docs/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
        ],
        Body::from(body),
    )
        .into_response()
}

// ------------------------------------------------------------ responses

pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(error: E) -> Self {
        let error = error.into();
        let status = if error
            .downcast_ref::<crate::error::LocalLeadsError>()
            .is_some()
        {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            crate::serverlog::error!("{} {}", self.status.as_u16(), self.message);
        }
        (
            self.status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            self.message,
        )
            .into_response()
    }
}

pub type WebResult<T = Response> = Result<T, AppError>;

pub fn render(template: impl Template) -> WebResult {
    let html = template
        .render()
        .map_err(|error| anyhow::anyhow!("render template: {error}"))?;
    Ok(Html(html).into_response())
}

pub trait RenderHtml {
    fn render_html(&self) -> Result<String, AppError>;
}

impl<T: Template> RenderHtml for T {
    fn render_html(&self) -> Result<String, AppError> {
        self.render()
            .map_err(|error| anyhow::anyhow!("render template: {error}").into())
    }
}

pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// Builds an `HX-Trigger` header that shows a toast and fires extra events.
/// HX-Trigger header showing a toast and firing `events`. With `undo`, the
/// toast offers Undo, restoring that trash entry.
pub fn trigger_with_undo(
    level: &str,
    message: &str,
    undo: Option<&str>,
    events: &[&str],
) -> (&'static str, HeaderValue) {
    let mut notice = serde_json::json!({ "level": level, "message": message });
    if let Some(undo) = undo {
        notice["undo"] = serde_json::Value::String(crate::app::undo_url(undo));
    }
    // htmx fires these in order on the element that made the request, and
    // events like close-modal remove that element: once it is gone, later
    // events never reach the page. So the notice goes first; a JSON map
    // would sort it after them.
    let mut json = format!("{{\"notice\":{notice}");
    for event in events {
        json.push_str(&format!(",{}:null", serde_json::Value::String((*event).to_owned())));
    }
    json.push('}');
    let json = ascii_json(&json);
    (
        "HX-Trigger",
        HeaderValue::from_str(&json).unwrap_or_else(|_| HeaderValue::from_static("{}")),
    )
}

pub fn notify(level: &str, message: &str, events: &[&str]) -> Response {
    notify_with_undo(level, message, None, events)
}

pub fn notify_with_undo(level: &str, message: &str, undo: Option<&str>, events: &[&str]) -> Response {
    (
        [
            trigger_with_undo(level, message, undo, events),
            ("HX-Reswap", HeaderValue::from_static("none")),
        ],
        "",
    )
        .into_response()
}

/// Undoes a delete: puts the trash entry's rows back.
async fn restore(State(state): State<SharedState>, Path(id): Path<String>) -> WebResult {
    let task_state = state.clone();
    let restored = tokio::task::spawn_blocking(move || task_state.db.restore(&id))
        .await
        .map_err(anyhow::Error::from)??
        .ok_or_else(|| AppError::not_found("That can no longer be undone"))?;
    let events: &[&str] = match restored.kind.as_str() {
        "proxies" | "proxy_group" => {
            let _ = state.refresh_running_pools();
            state.mark_proxies_dirty();
            &["proxies-changed"]
        }
        _ => {
            state.mark_jobs_dirty();
            state.mark_leads_dirty();
            &["jobs-changed", "leads-changed"]
        }
    };
    Ok(notify("ok", &format!("Restored {}", restored.label), events))
}

pub fn redirect(headers: &HeaderMap, location: &str) -> Response {
    let value = HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/"));
    if is_htmx(headers) {
        ([("HX-Location", value)], "").into_response()
    } else {
        (StatusCode::SEE_OTHER, [(header::LOCATION, value)], "").into_response()
    }
}

fn ascii_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for ch in json.chars() {
        if ch.is_ascii() && !ch.is_ascii_control() {
            out.push(ch);
        } else {
            let mut buffer = [0u16; 2];
            for unit in ch.encode_utf16(&mut buffer) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// Parses a checkbox-style form value.
pub fn flag(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| matches!(value, "on" | "1" | "true" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_recognized() {
        assert!(is_loopback_host("localhost:7878"));
        assert!(is_loopback_host("127.0.0.1:7878"));
        assert!(is_loopback_host("[::1]:7878"));
        assert!(!is_loopback_host("evil.example:7878"));
        assert!(!is_loopback_host(""));
    }

    #[test]
    fn header_json_is_ascii() {
        assert_eq!(ascii_json("{\"m\":\"a—b\"}"), "{\"m\":\"a\\u2014b\"}");
    }
}
