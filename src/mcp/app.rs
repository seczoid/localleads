//! Dashboard overview, app settings/info, notices, and database backups.

use super::leads::shellexpand_home;
use super::{LocalLeadsMcp, ToolResult, blocking, fail, reply};
use crate::db::LeadQuery;
use crate::web::views::percent;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoticesParams {
    /// Notices to return, newest first. Default 20, max 50 (the server keeps
    /// the last 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct BackupParams {
    /// Destination .sqlite file on the machine running LocalLeads. Default:
    /// localleads-backup-<timestamp>.sqlite beside the database. "~/" is
    /// expanded.
    #[serde(default)]
    pub path: Option<String>,
}

#[tool_router(router = app_router, vis = "pub(super)")]
impl LocalLeadsMcp {
    #[tool(
        description = "The dashboard home page: totals (searches, active searches, leads, phone/website coverage, proxies, healthy proxies), the 8 newest searches with progress, the 6 newest leads, proxy health, the proxy check in progress, and recent notices.",
        annotations(read_only_hint = true)
    )]
    async fn overview(&self) -> ToolResult {
        let state = self.state.clone();
        let (stats, jobs, leads, health) = blocking(move || {
            Ok((
                state.db.stats()?,
                state.db.list_jobs()?,
                state.db.query_leads(&LeadQuery {
                    limit: 6,
                    ..LeadQuery::default()
                })?,
                state.db.proxy_status_counts(None)?,
            ))
        })
        .await?;
        let get = |key: &str| health.get(key).copied().unwrap_or(0);
        let proxies_total: i64 = health.values().sum();
        let recent: Vec<_> = jobs
            .iter()
            .take(8)
            .map(|job| {
                json!({
                    "id": job.id,
                    "name": job.name,
                    "status": job.status.to_string(),
                    "collected": job.collected,
                    "target": if job.total > 0 { json!(job.total) } else { json!("all") },
                    "started_at": job.started_at,
                })
            })
            .collect();
        reply(json!({
            "engine": self.mode,
            "stats": {
                "searches": stats.jobs,
                "active_searches": stats.active_jobs,
                "leads": stats.leads,
                "leads_with_phone": stats.leads_with_phone,
                "leads_with_website": stats.leads_with_website,
                "phone_pct": percent(stats.leads_with_phone, stats.leads),
                "website_pct": percent(stats.leads_with_website, stats.leads),
                "proxies": stats.proxies,
                "healthy_proxies": stats.healthy_proxies,
            },
            "recent_searches": recent,
            "recent_leads": leads,
            "proxy_health": {
                "total": proxies_total,
                "healthy": get("healthy"),
                "slow": get("slow"),
                "failed": get("failed"),
                "unknown": proxies_total - get("healthy") - get("slow") - get("failed"),
            },
            "proxy_check": self.state.proxy_check_progress(),
            "notices": self.state.notices(8),
        }))
    }

    #[tool(
        description = "The Settings page: app version, database path, data folder, database size, totals, how this MCP server reaches the engine (dashboard or embedded), and the dashboard URL hint.",
        annotations(read_only_hint = true)
    )]
    async fn app_info(&self) -> ToolResult {
        let stats = self.state.db.stats().map_err(fail)?;
        reply(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "engine": self.mode,
            "engine_note": match self.mode {
                super::EngineMode::Dashboard => "Tools run inside the dashboard server; searches keep running when the agent disconnects.",
                super::EngineMode::Embedded => "No dashboard was running for this database, so this MCP process runs the engine; searches stop when it exits. Start `localleads` for a dashboard.",
            },
            "db_path": self.state.db_path.display().to_string(),
            "data_dir": self.state.db_path.parent().map(|path| path.display().to_string()),
            "db_size_bytes": self.state.db.file_size(),
            "stats": stats,
        }))
    }

    #[tool(
        description = "Recent activity notices (search started/finished, imports, checks, deletions, exports), newest first. The Settings page's activity log.",
        annotations(read_only_hint = true)
    )]
    async fn list_notices(&self, Parameters(params): Parameters<NoticesParams>) -> ToolResult {
        reply(json!({ "notices": self.state.notices(params.limit.unwrap_or(20).clamp(1, 50)) }))
    }

    #[tool(
        description = "Write a consistent snapshot of the whole database (searches, leads, proxies, settings) to a .sqlite file and return its path. Safe while searches run. Restore with `localleads --restore <file>`.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn backup_database(&self, Parameters(params): Parameters<BackupParams>) -> ToolResult {
        let destination = match params.path.as_deref().map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) => PathBuf::from(shellexpand_home(path)),
            None => self.state.db_path.with_file_name(format!(
                "localleads-backup-{}.sqlite",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            )),
        };
        if super::canonical(&destination) == super::canonical(&self.state.db_path) {
            return Err("choose a path other than the live database".to_owned());
        }
        if let Some(parent) = destination.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create folder {}: {error}", parent.display()))?;
        }
        let db = self.state.db.clone();
        let target = destination.clone();
        blocking(move || db.backup_to(&target)).await?;
        let size = std::fs::metadata(&destination).map(|meta| meta.len()).ok();
        let message = format!("Backup written to {}", destination.display());
        self.state.notice("ok", message);
        reply(json!({ "path": destination.display().to_string(), "size_bytes": size }))
    }
}
