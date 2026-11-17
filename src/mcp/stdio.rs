//! `localleads mcp`: MCP over stdio.
//!
//! Opening the database marks searches left active as interrupted, so a
//! second engine must never open a database a dashboard is serving. When a
//! dashboard serves this database, stdio is bridged to its `/mcp` endpoint
//! and every tool runs in the dashboard (searches outlive the agent, and the
//! dashboard shows them live). Otherwise this process runs the engine itself.

use super::{EngineMode, LocalLeadsMcp, ServerIdentity, canonical};
use crate::app::AppState;
use crate::auth::API_KEY_ENV;
use crate::db::Database;
use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:7878";

pub struct StdioOptions {
    pub db_path: PathBuf,
    /// Dashboard to bridge to; when set, no database is opened here.
    pub server: Option<String>,
    /// API key made on the dashboard's MCP page.
    pub api_key: Option<String>,
    /// Never bridge; always run the engine in this process.
    pub embedded: bool,
}

pub async fn run_stdio(options: StdioOptions) -> Result<()> {
    let Some(key) = options.api_key.clone() else {
        bail!(
            "localleads mcp needs an API key. Create one on the dashboard's MCP page (or run \
             `localleads mcp install --write`), then set {API_KEY_ENV} in the agent's MCP config"
        );
    };
    if let Some(server) = &options.server {
        let url = server.trim_end_matches('/').to_owned();
        eprintln!("localleads mcp: bridging stdio to {url}/mcp");
        return bridge(url, key).await;
    }
    // Opening the database marks its active searches interrupted, so never
    // open one that a running dashboard serves, even with --embedded.
    match find_dashboard(&options.db_path, &key).await {
        Discovery::Serving(url) if options.embedded => bail!(
            "the dashboard at {url} serves {}; opening it here too would mark its running searches \
             interrupted. Drop --embedded to bridge to it, or stop the dashboard first",
            options.db_path.display()
        ),
        Discovery::Serving(url) => {
            eprintln!("localleads mcp: dashboard at {url} serves this database; bridging to {url}/mcp");
            return bridge(url, key).await;
        }
        Discovery::Locked(url) => bail!(
            "the dashboard at {url} serves {} but rejected the API key in {API_KEY_ENV}: it was \
             revoked or mistyped. Create a new one on the dashboard's MCP page",
            options.db_path.display()
        ),
        Discovery::None => {}
    }
    eprintln!(
        "localleads mcp: running the engine in-process on {}",
        options.db_path.display()
    );
    let database = Database::open(&options.db_path)?;
    if !database.use_api_key(&crate::auth::digest(&key))? {
        bail!(
            "the API key in {API_KEY_ENV} doesn't work with {}: it was revoked, mistyped, or made \
             for another database. Create a new one on the dashboard's MCP page",
            options.db_path.display()
        );
    }
    let state = AppState::new(database, options.db_path.clone());
    let service = LocalLeadsMcp::new(state, EngineMode::Embedded)
        .serve(rmcp::transport::stdio())
        .await
        .context("start MCP stdio server")?;
    service.waiting().await?;
    Ok(())
}

// ------------------------------------------------------------ discovery

/// File beside the database naming the dashboard that serves it.
fn announce_path(db_path: &Path) -> PathBuf {
    let name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "localleads".to_owned());
    db_path.with_file_name(format!(".{name}.server.json"))
}

/// Removes the announcement when the dashboard stops.
pub struct Announcement(PathBuf);

impl Drop for Announcement {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Records that the dashboard at `url` serves `db_path`, so `localleads mcp`
/// can find it on any port.
pub fn announce(db_path: &Path, url: &str) -> Option<Announcement> {
    let path = announce_path(db_path);
    let body = json!({ "url": url, "pid": std::process::id() }).to_string();
    std::fs::write(&path, body).ok()?;
    Some(Announcement(path))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_default()
}

enum Discovery {
    /// A dashboard serves this database at the URL.
    Serving(String),
    /// The dashboard announced for this database refused our API key.
    Locked(String),
    None,
}

async fn find_dashboard(db_path: &Path, key: &str) -> Discovery {
    let mut announced = None;
    if let Ok(text) = std::fs::read_to_string(announce_path(db_path))
        && let Ok(value) = serde_json::from_str::<Value>(&text)
        && let Some(url) = value.get("url").and_then(Value::as_str)
    {
        announced = Some(url.trim_end_matches('/').to_owned());
    }
    let mut candidates: Vec<String> = announced.iter().cloned().collect();
    if !candidates.iter().any(|url| url == DEFAULT_SERVER) {
        candidates.push(DEFAULT_SERVER.to_owned());
    }
    let ours = canonical(db_path);
    let client = client();
    for url in candidates {
        let request = client
            .get(format!("{url}/mcp/info"))
            .bearer_auth(key)
            .timeout(Duration::from_millis(1500));
        let Ok(response) = request.send().await else {
            continue;
        };
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Only the announced address is known to serve this database.
            if announced.as_deref() == Some(url.as_str()) {
                return Discovery::Locked(url);
            }
            continue;
        }
        if let Ok(identity) = response.json::<ServerIdentity>().await
            && identity.app == "localleads"
            && identity.db_path == ours
        {
            return Discovery::Serving(url);
        }
    }
    Discovery::None
}

// --------------------------------------------------------------- bridge

#[derive(Default)]
struct Session {
    id: Option<String>,
    protocol: Option<String>,
    /// Replayed when the dashboard restarts and forgets the session.
    initialize: Option<Value>,
}

struct Bridge {
    client: reqwest::Client,
    endpoint: String,
    key: String,
    session: Mutex<Session>,
    out: mpsc::UnboundedSender<String>,
}

/// Forwards newline-delimited JSON-RPC between stdio and the dashboard's
/// Streamable HTTP endpoint.
async fn bridge(url: String, key: String) -> Result<()> {
    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    let bridge = Arc::new(Bridge {
        client: client(),
        endpoint: format!("{url}/mcp"),
        key,
        session: Mutex::new(Session::default()),
        out,
    });
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            bridge.emit(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": "parse error" }
            }));
            continue;
        };
        if message.get("method").and_then(Value::as_str) == Some("initialize") {
            // Initialize must finish before anything else uses the session.
            bridge.forward(message).await;
        } else {
            let bridge = bridge.clone();
            tasks.spawn(async move { bridge.forward(message).await });
        }
    }
    while tasks.join_next().await.is_some() {}
    let session = bridge.session.lock().await.id.clone();
    if let Some(id) = session {
        let _ = bridge
            .request(reqwest::Method::DELETE, None, Some(&id), None)
            .send()
            .await;
    }
    drop(bridge);
    let _ = writer.await;
    Ok(())
}

impl Bridge {
    fn emit(&self, message: Value) {
        let _ = self.out.send(message.to_string());
    }

    fn request(
        &self,
        method: reqwest::Method,
        body: Option<&Value>,
        session: Option<&str>,
        protocol: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, &self.endpoint)
            .header("Accept", "application/json, text/event-stream");
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(session) = session {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(protocol) = protocol {
            request = request.header("MCP-Protocol-Version", protocol);
        }
        request.bearer_auth(&self.key)
    }

    async fn forward(&self, message: Value) {
        let id = message.get("id").cloned();
        let is_request = message.get("method").is_some() && id.is_some();
        let is_initialize = message.get("method").and_then(Value::as_str) == Some("initialize");
        if is_initialize {
            self.session.lock().await.initialize = Some(message.clone());
        }
        let mut retried = false;
        loop {
            let (session, protocol) = {
                let session = self.session.lock().await;
                if is_initialize {
                    (None, None)
                } else {
                    (session.id.clone(), session.protocol.clone())
                }
            };
            match self.post(&message, session.as_deref(), protocol.as_deref()).await {
                Ok(Outcome::Messages(messages, new_session)) => {
                    let mut state = self.session.lock().await;
                    if new_session.is_some() {
                        state.id = new_session;
                    }
                    for reply in messages {
                        if is_initialize
                            && let Some(version) = reply
                                .pointer("/result/protocolVersion")
                                .and_then(Value::as_str)
                        {
                            state.protocol = Some(version.to_owned());
                        }
                        self.emit(reply);
                    }
                    return;
                }
                Ok(Outcome::SessionExpired) if !retried && !is_initialize => {
                    retried = true;
                    if self.reinitialize().await.is_ok() {
                        continue;
                    }
                    self.fail(is_request, id, "LocalLeads dashboard restarted and the MCP session could not be re-established");
                    return;
                }
                Ok(Outcome::SessionExpired) => {
                    self.fail(is_request, id, "LocalLeads dashboard forgot this MCP session; reconnect the MCP server");
                    return;
                }
                Err(error) => {
                    self.fail(
                        is_request,
                        id,
                        &format!(
                            "LocalLeads dashboard at {} is not reachable ({error}); start it with `localleads`",
                            self.endpoint
                        ),
                    );
                    return;
                }
            }
        }
    }

    fn fail(&self, is_request: bool, id: Option<Value>, message: &str) {
        if is_request {
            self.emit(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": message },
            }));
        } else {
            eprintln!("localleads mcp: {message}");
        }
    }

    /// Replays the client's handshake after the dashboard restarted.
    async fn reinitialize(&self) -> Result<()> {
        let initialize = self
            .session
            .lock()
            .await
            .initialize
            .clone()
            .context("no initialize request to replay")?;
        let Outcome::Messages(_, session) = self.post(&initialize, None, None).await? else {
            anyhow::bail!("dashboard rejected initialize");
        };
        let protocol = self.session.lock().await.protocol.clone();
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let _ = self
            .post(&initialized, session.as_deref(), protocol.as_deref())
            .await?;
        self.session.lock().await.id = session;
        Ok(())
    }

    async fn post(&self, message: &Value, session: Option<&str>, protocol: Option<&str>) -> Result<Outcome> {
        let mut response = self
            .request(reqwest::Method::POST, Some(message), session, protocol)
            .send()
            .await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && session.is_some() {
            return Ok(Outcome::SessionExpired);
        }
        let new_session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Outcome::Messages(Vec::new(), new_session));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("HTTP {status}: {}", body.trim());
        }
        let wanted = message.get("id").cloned();
        if content_type.starts_with("text/event-stream") {
            // Read events until the response to this request arrives; the
            // server may keep the stream open briefly afterwards.
            let mut buffer = String::new();
            let mut messages = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let mut done = false;
                while let Some(end) = buffer.find("\n\n") {
                    let event: String = buffer.drain(..end + 2).collect();
                    let data: Vec<&str> = event
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(|data| data.strip_prefix(' ').unwrap_or(data))
                        .collect();
                    let data = data.join("\n");
                    if data.trim().is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(&data) {
                        let answers = wanted.is_some()
                            && value.get("method").is_none()
                            && value.get("id") == wanted.as_ref();
                        messages.push(value);
                        if answers {
                            done = true;
                        }
                    }
                }
                if done {
                    break;
                }
            }
            return Ok(Outcome::Messages(messages, new_session));
        }
        let body = response.text().await?;
        if body.trim().is_empty() {
            return Ok(Outcome::Messages(Vec::new(), new_session));
        }
        let value: Value = serde_json::from_str(&body).context("dashboard sent invalid JSON")?;
        let messages = match value {
            Value::Array(items) => items,
            single => vec![single],
        };
        Ok(Outcome::Messages(messages, new_session))
    }
}

enum Outcome {
    Messages(Vec<Value>, Option<String>),
    SessionExpired,
}
