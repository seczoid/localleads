//! Proxies: list, add/import, health checks, enable/disable, delete and
//! prune, groups, history, and checker settings.

use super::leads::shellexpand_home;
use super::{LocalLeadsMcp, ToolResult, blocking, fail, page_limit, reply};
use crate::app::CheckerSettings;
use crate::db::ProxyQuery;
use crate::models::ProxyEntry;
use crate::proxy::{import_proxy_text, normalize_protocol};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProxyStatusInput {
    Healthy,
    Slow,
    Failed,
    /// Never checked.
    Unknown,
}

impl ProxyStatusInput {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Slow => "slow",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// The proxies table's filters.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ProxyFilterInput {
    /// Only proxies in this group (id from list_proxy_groups).
    #[serde(default)]
    pub group_id: Option<String>,
    /// Only proxies with this health status.
    #[serde(default)]
    pub status: Option<ProxyStatusInput>,
    /// true: only enabled proxies; false: only disabled ones.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Case-insensitive text matched against the proxy URL and last error.
    #[serde(default)]
    pub q: Option<String>,
}

impl ProxyFilterInput {
    fn query(&self, sort: &str, limit: usize, offset: usize) -> ProxyQuery {
        ProxyQuery {
            group_id: self
                .group_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
            status: self.status.map(|status| status.as_str().to_owned()),
            enabled: self.enabled,
            search: self.q.clone().unwrap_or_default(),
            sort: sort.to_owned(),
            limit,
            offset,
        }
    }
}

/// Which proxies a bulk action applies to: explicit ids, or every proxy
/// matching the filters (the dashboard's "select all matching").
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ProxySelection {
    /// Proxy ids from list_proxies.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Apply to every proxy matching the filters instead of ids. With no
    /// filters, that is every proxy.
    #[serde(default)]
    pub all_matching: bool,
    #[serde(flatten)]
    pub filters: ProxyFilterInput,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListProxiesParams {
    #[serde(flatten)]
    pub filters: ProxyFilterInput,
    /// Sort key, optionally with ":asc" or ":desc": proxy (URL), status
    /// (healthy first when ascending), latency, checked, error. Default:
    /// newest first.
    #[serde(default)]
    pub sort: Option<String>,
    /// Rows per page, default 50, max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Rows to skip. Use next_offset from the previous page.
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct AddProxiesParams {
    /// Proxies, one per line (or pass `proxies`). Accepted forms include
    /// host:port, host:port:user:pass, user:pass@host:port and full URLs like
    /// socks5://user:pass@host:port. Blank lines and # comments are skipped.
    #[serde(default)]
    pub text: Option<String>,
    /// Proxies as a list, same forms as `text`.
    #[serde(default)]
    pub proxies: Vec<String>,
    /// Protocol for lines without a scheme: http (default), https, socks4,
    /// socks5, socks5h.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Also add the new proxies to this group.
    #[serde(default)]
    pub group_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportProxyFileParams {
    /// Path to a text file of proxies on the machine running LocalLeads,
    /// one per line (same forms as add_proxies). "~/" is expanded.
    pub path: String,
    /// Protocol for lines without a scheme: http (default), https, socks4,
    /// socks5, socks5h.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Wait for the import to finish (up to 120 s) and report the result.
    /// Default true. With false, poll proxy_status.
    #[serde(default)]
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetEnabledParams {
    #[serde(flatten)]
    pub selection: ProxySelection,
    /// true to enable, false to disable. Searches only use enabled proxies.
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PruneMode {
    /// Delete proxies whose last check failed.
    DeleteFailed,
    /// Delete everything that is not healthy (slow, failed, never checked).
    KeepHealthy,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PruneParams {
    pub mode: PruneMode,
    /// Limit pruning to proxies matching these filters (e.g. one group).
    #[serde(flatten)]
    pub filters: ProxyFilterInput,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroupMembershipParams {
    /// Target group id (from list_proxy_groups).
    pub group: String,
    #[serde(flatten)]
    pub selection: ProxySelection,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProxyHistoryParams {
    /// Proxy id (from list_proxies).
    pub id: String,
    /// Checks to return, newest first. Default 25, max 500.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroupNameParams {
    /// Group name; must be unique (case-insensitive).
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameGroupParams {
    /// Group id.
    pub id: String,
    /// New name.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroupIdParams {
    /// Group id.
    pub id: String,
}

/// Fields to change; omitted ones keep their current value. Values are
/// clamped to the allowed ranges.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CheckerSettingsParams {
    /// URL each proxy fetches. Default https://www.google.com/maps.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Seconds before a check attempt times out, 1-120.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Retries after a failed attempt, 0-5.
    #[serde(default)]
    pub retries: Option<u32>,
    /// Slower responses than this count as "slow", 1-120000 ms.
    #[serde(default)]
    pub max_latency_ms: Option<u64>,
    /// Text the response must contain; "" disables the check.
    #[serde(default)]
    pub expected_body: Option<String>,
    /// Also record each proxy's exit IP.
    #[serde(default)]
    pub verify_exit_ip: Option<bool>,
    /// Proxies checked at once, 1-2048.
    #[serde(default)]
    pub concurrency: Option<usize>,
}

fn proxy_json(proxy: &ProxyEntry, groups: &HashMap<String, Vec<String>>) -> Value {
    json!({
        "id": proxy.id,
        "proxy": proxy.masked_url(),
        "protocol": proxy.protocol,
        "has_credentials": proxy.username.is_some() || proxy.password.is_some(),
        "enabled": proxy.enabled,
        "status": proxy.status,
        "latency_ms": proxy.latency_ms,
        "last_checked_at": proxy.last_checked_at,
        "consecutive_failures": proxy.consecutive_failures,
        "last_error": proxy.last_error,
        "groups": groups.get(&proxy.id).cloned().unwrap_or_default(),
    })
}

fn protocol_or_default(value: Option<&str>) -> Result<String, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok("http".to_owned()),
        Some(value) => normalize_protocol(value).map_err(fail),
    }
}

fn status_counts(counts: &HashMap<String, i64>) -> Value {
    let get = |key: &str| counts.get(key).copied().unwrap_or(0);
    let total: i64 = counts.values().sum();
    json!({
        "total": total,
        "healthy": get("healthy"),
        "slow": get("slow"),
        "failed": get("failed"),
        "unknown": total - get("healthy") - get("slow") - get("failed"),
    })
}

impl LocalLeadsMcp {
    fn select_proxies(&self, selection: &ProxySelection) -> Result<Vec<ProxyEntry>, String> {
        let proxies = if selection.all_matching {
            self.state
                .db
                .query_all_proxies(&selection.filters.query("", 0, 0))
                .map_err(fail)?
        } else {
            let ids: Vec<String> = selection
                .ids
                .iter()
                .map(|id| id.trim().to_owned())
                .filter(|id| !id.is_empty())
                .collect();
            if ids.is_empty() {
                return Err("pass proxy ids, or all_matching=true with optional filters".to_owned());
            }
            self.state.db.proxies_by_ids(&ids).map_err(fail)?
        };
        if proxies.is_empty() {
            return Err("no proxies match this selection".to_owned());
        }
        Ok(proxies)
    }

    fn require_group(&self, id: &str) -> Result<crate::models::ProxyGroup, String> {
        self.state
            .db
            .list_proxy_groups()
            .map_err(fail)?
            .into_iter()
            .find(|group| group.id == id.trim())
            .ok_or_else(|| format!("proxy group '{id}' not found; list_proxy_groups shows valid ids"))
    }

    fn proxies_changed(&self) {
        let _ = self.state.refresh_running_pools();
        self.state.mark_proxies_dirty();
    }

    fn check_status_json(&self) -> Value {
        let progress = self.state.proxy_check_progress();
        json!({
            "check": {
                "running": progress.running,
                "cancelling": progress.cancelling,
                "total": progress.total,
                "completed": progress.completed,
                "healthy": progress.healthy,
                "slow": progress.slow,
                "failed": progress.failed,
                "percent": progress.percent(),
                "per_second": (progress.rate() * 10.0).round() / 10.0,
                "eta_seconds": progress.eta().map(|eta| eta.as_secs()),
            },
            "import_running": self.state.import_running(),
        })
    }
}

#[tool_router(router = proxies_router, vis = "pub(super)")]
impl LocalLeadsMcp {
    #[tool(
        description = "Page through proxies with the proxies table's filters (group, status, enabled, text) and sorting. Returns total proxies in the group, matching count, one page of rows (credentials masked) and next_offset.",
        annotations(read_only_hint = true)
    )]
    async fn list_proxies(&self, Parameters(params): Parameters<ListProxiesParams>) -> ToolResult {
        let limit = page_limit(params.limit);
        let offset = params.offset.unwrap_or(0);
        let query = params
            .filters
            .query(params.sort.as_deref().unwrap_or("").trim(), limit, offset);
        let group_only = ProxyFilterInput {
            group_id: params.filters.group_id.clone(),
            ..ProxyFilterInput::default()
        }
        .query("", 0, 0);
        let state = self.state.clone();
        let (in_group, matching, rows, names) = blocking(move || {
            Ok((
                state.db.count_proxies(&group_only)?,
                state.db.count_proxies(&query)?,
                state.db.query_proxies(&query)?,
                state.db.proxy_group_names()?,
            ))
        })
        .await?;
        let end = offset + rows.len();
        reply(json!({
            "total": in_group,
            "matching": matching,
            "offset": offset,
            "limit": limit,
            "returned": rows.len(),
            "next_offset": (i64::try_from(end).unwrap_or(i64::MAX) < matching).then_some(end),
            "proxies": rows.iter().map(|proxy| proxy_json(proxy, &names)).collect::<Vec<_>>(),
        }))
    }

    #[tool(
        description = "Proxy health at a glance and background work in progress: status counts (overall and per group), groups, the running health check's progress (completed, healthy/slow/failed, rate, ETA), whether a file import is running, and recent notices. Poll this after check_proxies or import_proxy_file.",
        annotations(read_only_hint = true)
    )]
    async fn proxy_status(&self) -> ToolResult {
        let state = self.state.clone();
        let (overall, groups) = blocking(move || {
            let overall = state.db.proxy_status_counts(None)?;
            let mut groups = Vec::new();
            for group in state.db.list_proxy_groups()? {
                let counts = state.db.proxy_status_counts(Some(&group.id))?;
                groups.push((group, counts));
            }
            Ok((overall, groups))
        })
        .await?;
        let mut value = self.check_status_json();
        value["health"] = status_counts(&overall);
        value["groups"] = json!(
            groups
                .iter()
                .map(|(group, counts)| json!({
                    "id": group.id,
                    "name": group.name,
                    "proxies": group.proxy_count,
                    "health": status_counts(counts),
                }))
                .collect::<Vec<_>>()
        );
        value["recent_notices"] = json!(self.state.notices(5));
        reply(value)
    }

    #[tool(
        description = "Add pasted proxies (deduplicated against existing ones) and optionally put the new ones in a group. Returns how many were added, skipped as duplicates, or invalid (with reasons). New proxies start unchecked; run check_proxies next.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn add_proxies(&self, Parameters(params): Parameters<AddProxiesParams>) -> ToolResult {
        let protocol = protocol_or_default(params.protocol.as_deref())?;
        let mut text = params.text.unwrap_or_default();
        for line in params.proxies {
            text.push('\n');
            text.push_str(&line);
        }
        if text.trim().is_empty() {
            return Err("pass at least one proxy in text or proxies".to_owned());
        }
        let group = match params.group_id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => Some(self.require_group(id)?),
            None => None,
        };
        let state = self.state.clone();
        let group_id = group.as_ref().map(|group| group.id.clone());
        let summary = blocking(move || {
            let before: HashSet<String> =
                state.db.list_proxies()?.into_iter().map(|proxy| proxy.id).collect();
            let summary = import_proxy_text(&state.db, &text, &protocol)?;
            if let Some(group_id) = group_id
                && summary.imported > 0
            {
                let new_ids: Vec<String> = state
                    .db
                    .list_proxies()?
                    .into_iter()
                    .map(|proxy| proxy.id)
                    .filter(|id| !before.contains(id))
                    .collect();
                state.db.add_proxies_to_group(&group_id, &new_ids)?;
            }
            Ok(summary)
        })
        .await?;
        self.proxies_changed();
        self.state.notice(
            "ok",
            format!(
                "Added {} proxies, skipped {} duplicates",
                summary.imported, summary.duplicates
            ),
        );
        reply(json!({
            "added": summary.imported,
            "duplicates": summary.duplicates,
            "invalid": summary.invalid.len(),
            "invalid_lines": summary.invalid.iter().take(20).collect::<Vec<_>>(),
            "group": group.map(|group| group.name),
        }))
    }

    #[tool(
        description = "Import proxies from a text file on the machine running LocalLeads (the dashboard's file import). Runs in the background like the dashboard; by default waits for it to finish and reports the counts.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn import_proxy_file(&self, Parameters(params): Parameters<ImportProxyFileParams>) -> ToolResult {
        let protocol = protocol_or_default(params.protocol.as_deref())?;
        let path = std::path::PathBuf::from(shellexpand_home(params.path.trim()));
        let bytes = std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.trim().is_empty() {
            return Err(format!("{} is empty", path.display()));
        }
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "proxy file".to_owned());
        self.state
            .import_proxies(label.clone(), text, protocol)
            .map_err(fail)?;
        if !params.wait.unwrap_or(true) {
            return reply(json!({ "started": true, "file": label, "poll": "proxy_status (import_running)" }));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while self.state.import_running() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if self.state.import_running() {
            return reply(json!({ "started": true, "finished": false, "file": label, "poll": "proxy_status (import_running)" }));
        }
        let result = self
            .state
            .notices(10)
            .into_iter()
            .find(|notice| notice.message.starts_with(&format!("{label}:")) || notice.message.starts_with("Import failed"))
            .map(|notice| notice.message);
        reply(json!({ "finished": true, "file": label, "result": result }))
    }

    #[tool(
        description = "Start a background health check of the selected proxies (ids, or all_matching with filters; all_matching with no filters checks every proxy) using the checker settings. Only one check runs at a time. Poll proxy_status for progress; cancel_proxy_check stops it.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn check_proxies(&self, Parameters(selection): Parameters<ProxySelection>) -> ToolResult {
        let proxies = self.select_proxies(&selection)?;
        let total = self.state.start_proxy_check(proxies).map_err(fail)?;
        let settings = self.state.checker_settings();
        reply(json!({
            "started": true,
            "proxies": total,
            "concurrency": settings.concurrency,
            "poll": "proxy_status",
        }))
    }

    #[tool(
        description = "Cancel the running proxy health check. Results already recorded are kept.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn cancel_proxy_check(&self) -> ToolResult {
        let running = self.state.proxy_check_progress().running;
        self.state.cancel_proxy_check();
        reply(json!({ "was_running": running, "cancelling": running }))
    }

    #[tool(
        description = "Enable or disable the selected proxies. Searches only use enabled proxies; running searches pick up the change immediately.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn set_proxies_enabled(&self, Parameters(params): Parameters<SetEnabledParams>) -> ToolResult {
        let proxies = self.select_proxies(&params.selection)?;
        let state = self.state.clone();
        let enabled = params.enabled;
        let count = proxies.len();
        blocking(move || {
            for proxy in &proxies {
                state.db.set_proxy_enabled(&proxy.id, enabled)?;
            }
            Ok(())
        })
        .await?;
        self.proxies_changed();
        reply(json!({ "updated": count, "enabled": enabled }))
    }

    #[tool(
        description = "Permanently delete the selected proxies (ids, or all_matching with filters). Cannot be undone. Not allowed while a health check runs.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true)
    )]
    async fn delete_proxies(&self, Parameters(selection): Parameters<ProxySelection>) -> ToolResult {
        if self.state.proxy_check_progress().running {
            return Err("wait for the running check to finish (or cancel_proxy_check)".to_owned());
        }
        let ids: Vec<String> = self
            .select_proxies(&selection)?
            .into_iter()
            .map(|proxy| proxy.id)
            .collect();
        let count = ids.len();
        let state = self.state.clone();
        blocking(move || state.db.delete_proxies(&ids)).await?;
        self.proxies_changed();
        self.state.notice("ok", format!("Deleted {count} proxies"));
        reply(json!({ "deleted": count }))
    }

    #[tool(
        description = "Clean up proxies matching the filters (default: all): delete_failed removes proxies whose last check failed; keep_healthy removes everything not healthy (slow, failed, never checked). Cannot be undone. Not allowed while a health check runs.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true)
    )]
    async fn prune_proxies(&self, Parameters(params): Parameters<PruneParams>) -> ToolResult {
        if self.state.proxy_check_progress().running {
            return Err("wait for the running check to finish (or cancel_proxy_check)".to_owned());
        }
        let proxies = self
            .state
            .db
            .query_all_proxies(&params.filters.query("", 0, 0))
            .map_err(fail)?;
        let keep_healthy = matches!(params.mode, PruneMode::KeepHealthy);
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
        let count = doomed.len();
        if count > 0 {
            let state = self.state.clone();
            blocking(move || state.db.delete_proxies(&doomed)).await?;
            self.proxies_changed();
            self.state.notice(
                "ok",
                format!(
                    "Deleted {count} {} proxies",
                    if keep_healthy { "unhealthy" } else { "failed" }
                ),
            );
        }
        reply(json!({ "considered": proxies.len(), "deleted": count, "kept": proxies.len() - count }))
    }

    #[tool(
        description = "Recent health checks of one proxy, newest first: time, endpoint, status, latency, exit IP and error.",
        annotations(read_only_hint = true)
    )]
    async fn proxy_history(&self, Parameters(params): Parameters<ProxyHistoryParams>) -> ToolResult {
        let id = params.id.trim().to_owned();
        let proxy = self
            .state
            .db
            .proxies_by_ids(std::slice::from_ref(&id))
            .map_err(fail)?
            .into_iter()
            .next()
            .ok_or_else(|| format!("proxy '{id}' not found"))?;
        let names = self.state.db.proxy_group_names().map_err(fail)?;
        let checks: Vec<Value> = self
            .state
            .db
            .proxy_checks(&id, params.limit.unwrap_or(25).clamp(1, 500))
            .map_err(fail)?
            .into_iter()
            .map(|check| {
                json!({
                    "checked_at": check.checked_at,
                    "endpoint": check.endpoint,
                    "status": check.status,
                    "latency_ms": check.latency_ms,
                    "exit_ip": check.exit_ip,
                    "error": check.error,
                })
            })
            .collect();
        reply(json!({ "proxy": proxy_json(&proxy, &names), "checks": checks }))
    }

    // ------------------------------------------------------------ groups

    #[tool(
        description = "List proxy groups with their ids and proxy counts. Pass a group id to create_search (proxy_group_id) to limit a search to that group.",
        annotations(read_only_hint = true)
    )]
    async fn list_proxy_groups(&self) -> ToolResult {
        let groups: Vec<Value> = self
            .state
            .db
            .list_proxy_groups()
            .map_err(fail)?
            .into_iter()
            .map(|group| json!({ "id": group.id, "name": group.name, "proxies": group.proxy_count }))
            .collect();
        reply(json!({ "groups": groups }))
    }

    #[tool(
        description = "Create an empty proxy group and return its id. Add proxies with assign_proxies_to_group or add_proxies(group_id).",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn create_proxy_group(&self, Parameters(params): Parameters<GroupNameParams>) -> ToolResult {
        let name = params.name.trim();
        if name.is_empty() {
            return Err("group name cannot be empty".to_owned());
        }
        if self
            .state
            .db
            .list_proxy_groups()
            .map_err(fail)?
            .iter()
            .any(|group| group.name.eq_ignore_ascii_case(name))
        {
            return Err(format!("a group named \"{name}\" already exists"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.state.db.create_proxy_group(&id, name).map_err(fail)?;
        self.state.notice("ok", format!("Created proxy group \"{name}\""));
        self.state.mark_proxies_dirty();
        reply(json!({ "id": id, "name": name }))
    }

    #[tool(
        description = "Rename a proxy group.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn rename_proxy_group(&self, Parameters(params): Parameters<RenameGroupParams>) -> ToolResult {
        let name = params.name.trim();
        if name.is_empty() {
            return Err("group name cannot be empty".to_owned());
        }
        let group = self.require_group(&params.id)?;
        self.state.db.rename_proxy_group(&group.id, name).map_err(fail)?;
        self.state.mark_proxies_dirty();
        reply(json!({ "id": group.id, "name": name, "previous_name": group.name }))
    }

    #[tool(
        description = "Delete a proxy group. Its proxies are kept. Not allowed while an active search uses the group.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true)
    )]
    async fn delete_proxy_group(&self, Parameters(params): Parameters<GroupIdParams>) -> ToolResult {
        let group = self.require_group(&params.id)?;
        if self.state.group_in_use(&group.id).map_err(fail)? {
            return Err("this group is attached to an active search".to_owned());
        }
        self.state.db.delete_proxy_group(&group.id).map_err(fail)?;
        self.state.notice("ok", "Proxy group deleted; its proxies were kept");
        self.proxies_changed();
        reply(json!({ "deleted": group.id, "name": group.name, "proxies_kept": group.proxy_count }))
    }

    #[tool(
        description = "Add the selected proxies (ids, or all_matching with filters) to a group. A proxy can be in several groups. Running searches using the group pick them up immediately.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn assign_proxies_to_group(&self, Parameters(params): Parameters<GroupMembershipParams>) -> ToolResult {
        let group = self.require_group(&params.group)?;
        let ids: Vec<String> = self
            .select_proxies(&params.selection)?
            .into_iter()
            .map(|proxy| proxy.id)
            .collect();
        let count = ids.len();
        let state = self.state.clone();
        let group_id = group.id.clone();
        blocking(move || state.db.add_proxies_to_group(&group_id, &ids)).await?;
        self.proxies_changed();
        reply(json!({ "group": group.name, "assigned": count }))
    }

    #[tool(
        description = "Remove the selected proxies from a group (the proxies themselves are kept). With all_matching, the selection is limited to that group's members.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn remove_proxies_from_group(&self, Parameters(params): Parameters<GroupMembershipParams>) -> ToolResult {
        let group = self.require_group(&params.group)?;
        let mut selection = params.selection;
        if selection.all_matching {
            selection.filters.group_id = Some(group.id.clone());
        }
        let ids: Vec<String> = self
            .select_proxies(&selection)?
            .into_iter()
            .map(|proxy| proxy.id)
            .collect();
        let count = ids.len();
        let state = self.state.clone();
        let group_id = group.id.clone();
        blocking(move || state.db.remove_proxies_from_group(&group_id, &ids)).await?;
        self.proxies_changed();
        reply(json!({ "group": group.name, "removed": count }))
    }

    // ---------------------------------------------------------- settings

    #[tool(
        description = "Current proxy health-check settings: test endpoint, timeout, retries, slow threshold, expected body text, exit-IP check, and concurrency.",
        annotations(read_only_hint = true)
    )]
    async fn get_checker_settings(&self) -> ToolResult {
        reply(self.state.checker_settings())
    }

    #[tool(
        description = "Change proxy health-check settings. Omitted fields keep their value; values are clamped to allowed ranges. Returns the saved settings. Applies to the next check.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true)
    )]
    async fn update_checker_settings(&self, Parameters(params): Parameters<CheckerSettingsParams>) -> ToolResult {
        let current = self.state.checker_settings();
        let settings = CheckerSettings {
            endpoint: params.endpoint.unwrap_or(current.endpoint),
            timeout_secs: params.timeout_secs.unwrap_or(current.timeout_secs),
            retries: params.retries.unwrap_or(current.retries),
            max_latency_ms: params.max_latency_ms.unwrap_or(current.max_latency_ms),
            expected_body: params.expected_body.unwrap_or(current.expected_body),
            verify_exit_ip: params.verify_exit_ip.unwrap_or(current.verify_exit_ip),
            concurrency: params.concurrency.unwrap_or(current.concurrency),
        };
        let saved = self.state.save_checker_settings(settings).map_err(fail)?;
        reply(saved)
    }
}
