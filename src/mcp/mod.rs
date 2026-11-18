//! Model Context Protocol server: every dashboard capability as an MCP tool.
//!
//! Two transports share one tool set:
//! - Streamable HTTP at `/mcp` on the dashboard server, sharing its engine;
//! - stdio (`localleads mcp`), which bridges to a running dashboard for the
//!   same database when there is one and otherwise runs the engine itself.
//!
//! Tools call the same `AppState`/`Database` layer the web handlers use, so
//! anything an agent does shows up live in open dashboard tabs.

mod app;
pub mod clients;
pub mod install;
mod key_auth;
mod leads;
mod proxies;
mod searches;
mod stdio;

pub use stdio::announce;

use crate::app::SharedState;
use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool_handler};
use serde::Serialize;
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

/// Successful tool output is JSON text; errors become `isError` results so
/// the agent sees the message.
pub(crate) type ToolResult = Result<String, String>;

/// How this MCP server reaches the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineMode {
    /// Mounted on the dashboard server: searches keep running after the
    /// agent disconnects and the dashboard shows the same live state.
    Dashboard,
    /// `localleads mcp` with no dashboard running for this database: the
    /// engine runs inside the MCP process and stops when the agent closes it.
    Embedded,
}

#[derive(Clone)]
pub struct LocalLeadsMcp {
    state: SharedState,
    mode: EngineMode,
}

const INSTRUCTIONS: &str = "LocalLeads finds local businesses on Google Maps and stores them as leads in a local SQLite database. \
Every dashboard feature is available here.\n\
Typical flow: search_options (valid countries, states, filters, proxy groups) -> preview_search (counts the query x location lookups and validates) -> create_search (returns a search id; runs in the background) -> get_search (poll progress and events) -> browse_leads / lead_facets / export_leads.\n\
Searches are called jobs in some ids and fields. Lead lists are per-search: filter browse_leads by search_ids. Leads are deduplicated globally; get_lead shows every search that found a lead.\n\
Large results are paged: browse_leads and list_proxies take limit (max 1000) and offset and return the total. Use fields to keep rows small, and export_leads to write everything to disk.\n\
Proxies: add_proxies or import_proxy_file, then check_proxies (runs in the background; poll proxy_status). Group proxies with create_proxy_group and assign_proxies_to_group, then pass proxy_group_id to create_search.\n\
Stopped or failed searches: continue_search carries them on from where they stopped; rerun_search starts a fresh copy.\n\
Destructive tools (delete_*, prune_proxies) return an undo_id; undo_delete restores it within 60 minutes, after which it is gone for good. Confirm with the user before deleting.";

/// How many tools agents get, for the docs.
pub fn tool_count() -> usize {
    LocalLeadsMcp::router().list_all().len()
}

impl LocalLeadsMcp {
    pub fn new(state: SharedState, mode: EngineMode) -> Self {
        Self { state, mode }
    }

    fn router() -> &'static ToolRouter<Self> {
        static ROUTER: OnceLock<ToolRouter<LocalLeadsMcp>> = OnceLock::new();
        ROUTER.get_or_init(|| {
            let all = Self::searches_router()
                + Self::leads_router()
                + Self::proxies_router()
                + Self::app_router();
            let mut router = ToolRouter::new();
            for mut route in all {
                let mut schema = serde_json::Value::Object((*route.attr.input_schema).clone());
                portable_schema(&mut schema);
                if let serde_json::Value::Object(schema) = schema {
                    route.attr.input_schema = Arc::new(schema);
                }
                router.add_route(route);
            }
            router
        })
    }

    /// Every tool by area, with the first sentence of its description,
    /// for the dashboard's MCP page.
    pub fn tool_groups() -> Vec<(&'static str, Vec<(String, String)>)> {
        let list = |router: ToolRouter<Self>| {
            let mut tools: Vec<(String, String)> = router
                .list_all()
                .into_iter()
                .map(|tool| {
                    let text = tool.description.as_deref().unwrap_or("").trim().to_owned();
                    let first = text
                        .split_once(". ")
                        .map_or(text.as_str(), |(first, _)| first)
                        .trim_end_matches('.')
                        .to_owned();
                    (tool.name.to_string(), first)
                })
                .collect();
            tools.sort();
            tools
        };
        vec![
            ("Searches", list(Self::searches_router())),
            ("Leads", list(Self::leads_router())),
            ("Proxies", list(Self::proxies_router())),
            ("App", list(Self::app_router())),
        ]
    }

    /// Names of every tool, for docs and tests.
    #[cfg(test)]
    pub fn tool_names() -> Vec<String> {
        Self::router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }
}

#[tool_handler(router = Self::router())]
impl ServerHandler for LocalLeadsMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("localleads", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_owned())
    }
}

// ------------------------------------------------------------------ HTTP

/// Cancelled on shutdown so open MCP streams don't hold the server open.
/// Cancelled when the web server shuts down.
pub(crate) fn shutdown_token() -> &'static CancellationToken {
    static TOKEN: OnceLock<CancellationToken> = OnceLock::new();
    TOKEN.get_or_init(Default::default)
}

/// Ends every open MCP session; call when the web server shuts down.
pub fn shutdown() {
    shutdown_token().cancel();
}

/// `/mcp` (Streamable HTTP) and `/mcp/info` (used by `localleads mcp` to find
/// a dashboard serving the same database). Every request needs an API key
/// ([`key_auth`]). Host and cross-site checks come from the web server's
/// request guard, so rmcp's own host allowlist is
/// turned off.
pub fn router(state: SharedState) -> Router<SharedState> {
    let config = StreamableHttpServerConfig::default()
        .disable_allowed_hosts()
        .with_cancellation_token(shutdown_token().child_token());
    let factory_state = state.clone();
    let service = StreamableHttpService::new(
        move || Ok(LocalLeadsMcp::new(factory_state.clone(), EngineMode::Dashboard)),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/mcp", service)
        .route("/mcp/info", get(info))
        .route_layer(key_auth::layer(state))
}

#[derive(Serialize, serde::Deserialize)]
pub struct ServerIdentity {
    pub app: String,
    pub version: String,
    pub db_path: String,
}

async fn info(State(state): State<SharedState>) -> impl IntoResponse {
    axum::Json(ServerIdentity {
        app: "localleads".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        db_path: canonical(&state.db_path),
    })
}

pub(crate) fn canonical(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

// --------------------------------------------------------------- helpers

/// Rewrites optional fields from `type: [T, "null"]` / `anyOf: [T, null]`
/// with `default: null` into plain `T`. The fields stay optional (they are
/// not `required`), and the schema maps onto every client's tool format;
/// some reject nullable unions.
pub(crate) fn portable_schema(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get("type") {
                let rest: Vec<Value> = types
                    .iter()
                    .filter(|kind| kind.as_str() != Some("null"))
                    .cloned()
                    .collect();
                if rest.len() < types.len() {
                    map.insert(
                        "type".to_owned(),
                        if rest.len() == 1 { rest[0].clone() } else { Value::Array(rest) },
                    );
                }
            }
            for key in ["anyOf", "oneOf"] {
                let Some(Value::Array(branches)) = map.get(key) else {
                    continue;
                };
                let is_null = |branch: &Value| {
                    branch.get("type").and_then(Value::as_str) == Some("null")
                        || branch.get("const") == Some(&Value::Null)
                };
                if !branches.iter().any(is_null) {
                    continue;
                }
                let rest: Vec<Value> = branches.iter().filter(|branch| !is_null(branch)).cloned().collect();
                map.remove(key);
                match <[Value; 1]>::try_from(rest) {
                    Ok([Value::Object(only)]) => {
                        for (inner_key, inner) in only {
                            map.entry(inner_key).or_insert(inner);
                        }
                    }
                    Ok([other]) => {
                        map.insert(key.to_owned(), Value::Array(vec![other]));
                    }
                    Err(rest) => {
                        map.insert(key.to_owned(), Value::Array(rest));
                    }
                }
            }
            if map.get("default") == Some(&Value::Null) {
                map.remove("default");
            }
            for child in map.values_mut() {
                portable_schema(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(portable_schema),
        _ => {}
    }
}

/// Pretty JSON for the agent.
/// A delete's undo id for its reply: null when nothing was deleted.
pub(super) fn undo_id(count: usize, undo: String) -> serde_json::Value {
    if count == 0 || undo.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(undo)
    }
}

pub(crate) fn reply(value: impl Serialize) -> ToolResult {
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

pub(crate) fn fail(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Runs blocking database work off the async runtime.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T, String>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(fail)?
        .map_err(fail)
}

/// Clamps a page size: default 50, at most 1,000.
pub(crate) fn page_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(50).clamp(1, 1_000)
}

#[cfg(test)]
mod tests;
