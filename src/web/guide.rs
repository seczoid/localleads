//! The MCP setup page and the user guide.

use super::{AppError, WebResult, redirect, render};
use crate::app::SharedState;
use crate::auth;
use crate::mcp::LocalLeadsMcp;
use crate::mcp::clients::{self, Launch};
use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum_extra::extract::Form;
use serde::Deserialize;

/// Stands in for a key in the setup snippets until one is created.
const KEY_PLACEHOLDER: &str = "YOUR_API_KEY";

pub struct KeyView {
    id: String,
    name: String,
    shown: String,
    created: String,
    last_used: String,
}

pub struct SnippetView {
    label: String,
    code: String,
}

pub struct ClientView {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    check: &'static str,
    stdio: Vec<SnippetView>,
    http: Vec<SnippetView>,
    http_note: &'static str,
}

pub struct ToolGroupView {
    name: &'static str,
    tools: Vec<(String, String)>,
}

#[derive(Template)]
#[template(path = "mcp.html")]
struct McpPage {
    nav: &'static str,
    endpoint: String,
    command: String,
    install: String,
    db_path: String,
    in_build_folder: bool,
    tool_count: usize,
    clients: Vec<ClientView>,
    groups: Vec<ToolGroupView>,
    keys: Vec<KeyView>,
    /// Served to other machines (e.g. from Docker): agents connect over
    /// HTTP, since the command only exists on the server.
    remote: bool,
    /// A key that was just created, shown this once.
    new_key: String,
    key_name: String,
}

#[derive(Template)]
#[template(path = "docs.html")]
struct DocsPage {
    nav: &'static str,
    install: String,
    can_verify: bool,
    tool_count: usize,
}

/// The dashboard's own address: the public URL when one is set, otherwise
/// as the browser reached it.
fn base_url(headers: &HeaderMap) -> String {
    let site = super::site::site();
    if let Some(url) = &site.public_url {
        return url.clone();
    }
    let scheme = if site.request_is_https(headers) { "https" } else { "http" };
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1:7878");
    format!("{scheme}://{host}")
}

/// What an agent should launch: this binary, plus `--db` when the
/// dashboard isn't using the default database.
fn launch_parts(state: &SharedState) -> (String, Vec<String>) {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "localleads".to_owned());
    let mut args = vec!["mcp".to_owned()];
    let canonical = |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let default = crate::default_database_path().ok().map(|path| canonical(&path));
    let current = canonical(&state.db_path);
    if default.as_ref() != Some(&current) {
        args.insert(0, current.display().to_string());
        args.insert(0, "--db".to_owned());
    }
    (exe, args)
}

fn install_command(exe: &str, args: &[String]) -> String {
    let mut parts: Vec<String> = vec![exe.to_owned()];
    // `--db` goes before the subcommand, like the launch command.
    parts.extend(args.iter().take_while(|arg| *arg != "mcp").cloned());
    parts.extend(["mcp".to_owned(), "install".to_owned(), "--write".to_owned()]);
    parts
        .iter()
        .map(|part| crate::mcp::install::quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn mcp(State(state): State<SharedState>, headers: HeaderMap) -> WebResult {
    mcp_page(&state, &headers, String::new(), String::new())
}

#[derive(Deserialize)]
pub struct KeyForm {
    #[serde(default)]
    name: String,
}

pub async fn create_key(State(state): State<SharedState>, headers: HeaderMap, Form(form): Form<KeyForm>) -> WebResult {
    let name = form.name.trim();
    if name.chars().count() > 80 {
        return Err(AppError::bad_request("Keep the key's name under 80 characters."));
    }
    let name = if name.is_empty() { "MCP client" } else { name };
    let (key, shown) = auth::new_api_key();
    state.db.create_api_key(name, &shown, &auth::digest(&key))?;
    state.notice("ok", format!("Created API key \"{name}\""));
    mcp_page(&state, &headers, key, name.to_owned())
}

pub async fn revoke_key(State(state): State<SharedState>, headers: HeaderMap, Path(id): Path<String>) -> WebResult {
    if !state.db.revoke_api_key(&id)? {
        return Err(AppError::not_found("That key was already revoked."));
    }
    state.notice("ok", "Revoked an API key");
    Ok(redirect(&headers, "/agents"))
}

fn when(at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|at| at.with_timezone(&chrono::Local).format("%b %-d, %Y %H:%M").to_string())
        .unwrap_or_else(|_| at.to_owned())
}

fn mcp_page(state: &SharedState, headers: &HeaderMap, new_key: String, key_name: String) -> WebResult {
    let (exe, args) = launch_parts(state);
    let endpoint = format!("{}/mcp", base_url(headers));
    let launch = Launch {
        command: &exe,
        args: &args,
        url: &endpoint,
        name: "localleads",
        key: if new_key.is_empty() { KEY_PLACEHOLDER } else { &new_key },
    };
    let view = |snippets: Vec<clients::Snippet>| {
        snippets
            .into_iter()
            .map(|snippet| SnippetView {
                label: snippet.label,
                code: snippet.code,
            })
            .collect()
    };
    let clients = clients::setups(&launch)
        .into_iter()
        .map(|setup| ClientView {
            id: setup.id,
            name: setup.name,
            kind: setup.kind,
            check: setup.check,
            stdio: view(setup.stdio),
            http: view(setup.http),
            http_note: setup.http_note,
        })
        .collect();
    let groups: Vec<ToolGroupView> = LocalLeadsMcp::tool_groups()
        .into_iter()
        .map(|(name, tools)| ToolGroupView { name, tools })
        .collect();
    let command = std::iter::once(exe.as_str())
        .chain(args.iter().map(String::as_str))
        .map(crate::mcp::install::quote)
        .collect::<Vec<_>>()
        .join(" ");
    render(McpPage {
        nav: "mcp",
        install: install_command(&exe, &args),
        in_build_folder: std::path::Path::new(&exe)
            .components()
            .any(|part| part.as_os_str() == "target"),
        command,
        endpoint,
        db_path: state.db_path.display().to_string(),
        tool_count: groups.iter().map(|group| group.tools.len()).sum(),
        clients,
        groups,
        keys: state
            .db
            .api_keys()?
            .into_iter()
            .map(|key| KeyView {
                created: when(&key.created_at),
                last_used: key.last_used_at.as_deref().map(when).unwrap_or_else(|| "Never".to_owned()),
                id: key.id,
                name: key.name,
                shown: key.shown,
            })
            .collect(),
        remote: super::site::site().public_url.is_some(),
        new_key,
        key_name,
    })
}

pub async fn docs(State(state): State<SharedState>) -> WebResult {
    let (exe, args) = launch_parts(&state);
    render(DocsPage {
        nav: "docs",
        install: install_command(&exe, &args),
        can_verify: cfg!(feature = "verify"),
        tool_count: crate::mcp::tool_count(),
    })
}
