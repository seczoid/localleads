//! Shared server state: running job controls, proxy pools, the proxy checker,
//! notices, and the broadcast channel that drives live browser updates.

use crate::db::Database;
use crate::events::AppEvent;
use crate::models::{JobStatus, ProxyEntry, SearchConfig};
use crate::proxy::{
    CancelToken, MAX_CONCURRENCY, ProxyPool, ProxyTestConfig, check_many_with_events,
    import_proxy_text,
};
use crate::scraper::{JobControl, run_job};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

const CHECKER_SETTINGS_KEY: &str = "proxy_checker";
const MAX_NOTICES: usize = 50;

#[derive(Clone, Debug, Serialize)]
pub struct Notice {
    pub level: &'static str,
    pub message: String,
    pub at: String,
}

/// Signals pushed to every open browser tab over server-sent events.
#[derive(Clone, Debug)]
pub enum UiEvent {
    Jobs,
    Proxies,
    /// Stored leads changed outside a search (bulk enrichment).
    Leads,
    /// Only the proxy check progress card changed.
    ProxyProgress,
    Notice(Notice),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProxyCheckProgress {
    pub running: bool,
    pub total: usize,
    pub completed: usize,
    pub healthy: usize,
    pub slow: usize,
    pub failed: usize,
    pub cancelling: bool,
    #[serde(skip)]
    pub started: Option<Instant>,
}

impl ProxyCheckProgress {
    /// Proxies finished per second since the check started.
    pub fn rate(&self) -> f64 {
        let Some(started) = self.started else {
            return 0.0;
        };
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed < 0.5 {
            return 0.0;
        }
        self.completed as f64 / elapsed
    }

    /// Estimated time left at the current rate.
    pub fn eta(&self) -> Option<Duration> {
        let rate = self.rate();
        if rate <= 0.0 || self.completed == 0 {
            return None;
        }
        let left = self.total.saturating_sub(self.completed) as f64;
        Some(Duration::from_secs_f64(left / rate))
    }

    pub fn percent(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        ((self.completed as f64 / self.total as f64) * 100.0).round() as u32
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CheckerSettings {
    pub endpoint: String,
    pub timeout_secs: u64,
    pub retries: u32,
    pub max_latency_ms: u64,
    pub expected_body: String,
    pub verify_exit_ip: bool,
    pub concurrency: usize,
}

impl Default for CheckerSettings {
    fn default() -> Self {
        Self {
            endpoint: ProxyTestConfig::default().endpoint,
            timeout_secs: 15,
            retries: 2,
            max_latency_ms: 10_000,
            expected_body: "google".to_owned(),
            verify_exit_ip: false,
            concurrency: 512,
        }
    }
}

impl CheckerSettings {
    pub fn normalized(mut self) -> Self {
        if self.endpoint.trim().is_empty() {
            self.endpoint = ProxyTestConfig::default().endpoint;
        }
        self.endpoint = self.endpoint.trim().to_owned();
        self.timeout_secs = self.timeout_secs.clamp(1, 120);
        self.retries = self.retries.min(5);
        self.max_latency_ms = self.max_latency_ms.clamp(1, 120_000);
        self.expected_body = self.expected_body.trim().to_owned();
        self.concurrency = self.concurrency.clamp(1, MAX_CONCURRENCY);
        self
    }

    fn test_config(&self) -> ProxyTestConfig {
        ProxyTestConfig {
            endpoint: self.endpoint.clone(),
            timeout: Duration::from_secs(self.timeout_secs),
            retries: self.retries,
            max_latency_ms: self.max_latency_ms,
            expected_body: (!self.expected_body.is_empty()).then(|| self.expected_body.clone()),
            verify_exit_ip: self.verify_exit_ip,
        }
    }
}

#[derive(Default)]
struct Runtime {
    controls: HashMap<String, JobControl>,
    pools: HashMap<String, ProxyPool>,
    proxy_check: ProxyCheckProgress,
    proxy_check_cancel: Option<CancelToken>,
    import_running: bool,
    notices: VecDeque<Notice>,
    /// Latest progress message per running job.
    activity: HashMap<String, String>,
}

pub struct AppState {
    pub db: Database,
    pub db_path: PathBuf,
    runtime: Mutex<Runtime>,
    ui_tx: broadcast::Sender<UiEvent>,
    events_tx: mpsc::UnboundedSender<AppEvent>,
    jobs_dirty: AtomicBool,
    proxies_dirty: AtomicBool,
    progress_dirty: AtomicBool,
    leads_dirty: AtomicBool,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    /// Builds the state and spawns the background event loop. Must be called
    /// inside a Tokio runtime.
    pub fn new(db: Database, db_path: PathBuf) -> SharedState {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (ui_tx, _) = broadcast::channel(256);
        let state = Arc::new(Self {
            db,
            db_path,
            runtime: Mutex::new(Runtime::default()),
            ui_tx,
            events_tx,
            jobs_dirty: AtomicBool::new(false),
            leads_dirty: AtomicBool::new(false),
            proxies_dirty: AtomicBool::new(false),
            progress_dirty: AtomicBool::new(false),
        });
        tokio::spawn(event_loop(state.clone(), events_rx));
        tokio::spawn(flush_loop(state.clone()));
        state
    }

    fn runtime(&self) -> MutexGuard<'_, Runtime> {
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.ui_tx.subscribe()
    }

    pub fn notice(&self, level: &'static str, message: impl Into<String>) {
        let notice = Notice {
            level,
            message: message.into(),
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
        };
        {
            let mut runtime = self.runtime();
            runtime.notices.push_front(notice.clone());
            runtime.notices.truncate(MAX_NOTICES);
        }
        let _ = self.ui_tx.send(UiEvent::Notice(notice));
    }

    pub fn notices(&self, limit: usize) -> Vec<Notice> {
        self.runtime().notices.iter().take(limit).cloned().collect()
    }

    pub fn mark_jobs_dirty(&self) {
        self.jobs_dirty.store(true, Ordering::Relaxed);
    }

    pub fn mark_proxies_dirty(&self) {
        self.proxies_dirty.store(true, Ordering::Relaxed);
    }

    fn mark_progress_dirty(&self) {
        self.progress_dirty.store(true, Ordering::Relaxed);
    }

    // ---------------------------------------------------------- enrichment

    /// Finds emails, website details and Google place details for stored
    /// leads in the background, with every enabled proxy. Returns how many
    /// leads were queued (ones already queued are skipped).
    pub fn enrich_leads(state: &SharedState, ids: &[String]) -> Result<usize> {
        let targets = state.db.claim_enrich_ids(ids)?;
        let count = targets.len();
        if count == 0 {
            return Ok(0);
        }
        let pool = ProxyPool::new(state.db.list_proxies_for_group(None)?);
        let state = state.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            let config = SearchConfig::default();
            let ids: Vec<String> = targets.iter().map(|target| target.id.clone()).collect();
            let enricher = match crate::enrich::run::Enricher::new(&config, pool).await {
                Ok(enricher) => enricher,
                Err(error) => {
                    let _ = state.db.release_enrich_ids(&ids);
                    state.notice("bad", format!("Couldn't start finding emails: {error}"));
                    return;
                }
            };
            let enricher = &enricher;
            let state_ref = &state;
            let with_email = futures::stream::iter(targets)
                .map(|target| async move {
                    let (patch, summary) = enricher.enrich(&target).await;
                    let has_email = summary.email_count > 0;
                    let db = state_ref.db.clone();
                    let saved = tokio::task::spawn_blocking(move || {
                        db.save_enrichment(&target.id, &patch, &summary)
                    })
                    .await;
                    state_ref.leads_dirty.store(true, Ordering::Relaxed);
                    matches!(saved, Ok(Ok(()))) && has_email
                })
                .buffer_unordered(12)
                .filter(|found| std::future::ready(*found))
                .count()
                .await;
            let _ = state.db.release_enrich_ids(&ids);
            state.notice(
                "ok",
                format!("Enriched {count} leads: {with_email} with an email"),
            );
        });
        Ok(count)
    }

    // ---------------------------------------------------------------- jobs

    pub fn start_job(&self, config: SearchConfig) -> Result<String> {
        config.validate()?;
        let id = uuid::Uuid::new_v4().to_string();
        self.db.create_job(&id, &config)?;
        let pool = ProxyPool::new(
            self.db
                .list_proxies_for_group(config.proxy_group_id.as_deref())?,
        );
        let control = JobControl::new();
        {
            let mut runtime = self.runtime();
            runtime.controls.insert(id.clone(), control.clone());
            runtime.pools.insert(id.clone(), pool.clone());
        }
        tokio::spawn(run_job(
            self.db.clone(),
            id.clone(),
            config.clone(),
            pool,
            control,
            self.events_tx.clone(),
        ));
        self.notice("ok", format!("Started search \"{}\"", config.job_name));
        self.mark_jobs_dirty();
        Ok(id)
    }

    pub fn job_activity(&self, job_id: &str) -> Option<String> {
        self.runtime().activity.get(job_id).cloned()
    }

    pub fn is_live(&self, job_id: &str) -> bool {
        self.runtime().controls.contains_key(job_id)
    }

    pub fn toggle_pause(&self, job_id: &str) -> Result<JobStatus> {
        let control = self
            .runtime()
            .controls
            .get(job_id)
            .cloned()
            .ok_or_else(|| anyhow!("Only jobs started since the server launched can be paused"))?;
        let paused = control.toggle_pause();
        let status = if paused {
            JobStatus::Paused
        } else {
            JobStatus::Running
        };
        self.db.update_job_status(job_id, status, None)?;
        self.mark_jobs_dirty();
        Ok(status)
    }

    pub fn stop_job(&self, job_id: &str) -> Result<()> {
        let control = self
            .runtime()
            .controls
            .get(job_id)
            .cloned()
            .ok_or_else(|| anyhow!("This search is not running in this server process"))?;
        control.cancel();
        self.db
            .update_job_status(job_id, JobStatus::Stopping, None)?;
        self.notice("info", "Stopping search; collected records are kept");
        self.mark_jobs_dirty();
        Ok(())
    }

    pub fn group_in_use(&self, group_id: &str) -> Result<bool> {
        Ok(self.db.list_jobs()?.iter().any(|job| {
            job.status.is_active() && job.config.proxy_group_id.as_deref() == Some(group_id)
        }))
    }

    /// Running jobs pick up proxy membership and health changes immediately.
    pub fn refresh_running_pools(&self) -> Result<()> {
        let pools: Vec<(String, ProxyPool)> = self
            .runtime()
            .pools
            .iter()
            .map(|(id, pool)| (id.clone(), pool.clone()))
            .collect();
        if pools.is_empty() {
            return Ok(());
        }
        let jobs = self.db.list_jobs()?;
        for (job_id, pool) in pools {
            if let Some(job) = jobs.iter().find(|job| job.id == job_id)
                && job.status.is_active()
            {
                pool.replace_entries(
                    self.db
                        .list_proxies_for_group(job.config.proxy_group_id.as_deref())?,
                );
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- proxies

    pub fn checker_settings(&self) -> CheckerSettings {
        self.db
            .get_setting(CHECKER_SETTINGS_KEY)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_str::<CheckerSettings>(&value).ok())
            .unwrap_or_default()
            .normalized()
    }

    pub fn save_checker_settings(&self, settings: CheckerSettings) -> Result<CheckerSettings> {
        let settings = settings.normalized();
        self.db
            .set_setting(CHECKER_SETTINGS_KEY, &serde_json::to_string(&settings)?)?;
        Ok(settings)
    }

    pub fn proxy_check_progress(&self) -> ProxyCheckProgress {
        self.runtime().proxy_check.clone()
    }

    pub fn import_running(&self) -> bool {
        self.runtime().import_running
    }

    pub fn start_proxy_check(&self, proxies: Vec<ProxyEntry>) -> Result<usize> {
        if proxies.is_empty() {
            return Err(anyhow!("Select at least one proxy to check"));
        }
        let settings = self.checker_settings();
        let cancel = CancelToken::default();
        let total = proxies.len();
        {
            let mut runtime = self.runtime();
            if runtime.proxy_check.running {
                return Err(anyhow!("A proxy check is already running"));
            }
            runtime.proxy_check = ProxyCheckProgress {
                running: true,
                total,
                started: Some(Instant::now()),
                ..ProxyCheckProgress::default()
            };
            runtime.proxy_check_cancel = Some(cancel.clone());
        }
        let database = self.db.clone();
        let events = self.events_tx.clone();
        let config = settings.test_config();
        let concurrency = settings.concurrency;
        tokio::spawn(async move {
            check_many_with_events(database, proxies, config, concurrency, events, cancel).await;
        });
        self.notice(
            "info",
            format!("Checking {total} proxies at concurrency {concurrency}"),
        );
        self.mark_proxies_dirty();
        Ok(total)
    }

    pub fn cancel_proxy_check(&self) {
        let mut runtime = self.runtime();
        if let Some(cancel) = &runtime.proxy_check_cancel {
            cancel.cancel();
            runtime.proxy_check.cancelling = true;
        }
        drop(runtime);
        self.mark_progress_dirty();
    }

    pub fn import_proxies(&self, label: String, text: String, protocol: String) -> Result<()> {
        {
            let mut runtime = self.runtime();
            if runtime.import_running {
                return Err(anyhow!("A proxy import is already running"));
            }
            runtime.import_running = true;
        }
        let database = self.db.clone();
        let events = self.events_tx.clone();
        tokio::task::spawn_blocking(move || {
            let event = match import_proxy_text(&database, &text, &protocol) {
                Ok(summary) => AppEvent::ProxyImportFinished {
                    path: label,
                    imported: summary.imported,
                    duplicates: summary.duplicates,
                    invalid: summary.invalid.len(),
                    error: None,
                },
                Err(error) => AppEvent::ProxyImportFinished {
                    path: label,
                    imported: 0,
                    duplicates: 0,
                    invalid: 0,
                    error: Some(error.to_string()),
                },
            };
            let _ = events.send(event);
        });
        self.mark_proxies_dirty();
        Ok(())
    }
}

async fn event_loop(state: SharedState, mut events: mpsc::UnboundedReceiver<AppEvent>) {
    while let Some(event) = events.recv().await {
        match event {
            AppEvent::JobProgress { job_id, message } => {
                state.runtime().activity.insert(job_id, message);
                state.mark_jobs_dirty();
            }
            AppEvent::JobFinished {
                job_id,
                status,
                message,
            } => {
                let name = {
                    let mut runtime = state.runtime();
                    runtime.controls.remove(&job_id);
                    runtime.pools.remove(&job_id);
                    runtime.activity.remove(&job_id);
                    drop(runtime);
                    state
                        .db
                        .get_job(&job_id)
                        .ok()
                        .flatten()
                        .map(|job| job.name)
                        .unwrap_or(job_id)
                };
                let level = match status {
                    JobStatus::Completed => "ok",
                    JobStatus::Failed => "error",
                    _ => "info",
                };
                state.notice(level, format!("{name}: {status} — {message}"));
                state.mark_jobs_dirty();
            }
            AppEvent::ProxyChecksStored {
                healthy,
                slow,
                failed,
            } => {
                {
                    let mut runtime = state.runtime();
                    let progress = &mut runtime.proxy_check;
                    progress.completed = progress.completed.saturating_add(healthy + slow + failed);
                    progress.healthy += healthy;
                    progress.slow += slow;
                    progress.failed += failed;
                }
                state.mark_progress_dirty();
                state.mark_proxies_dirty();
            }
            AppEvent::ProxyCheckFinished { total, cancelled } => {
                let summary = {
                    let mut runtime = state.runtime();
                    runtime.proxy_check.running = false;
                    runtime.proxy_check.cancelling = false;
                    runtime.proxy_check_cancel = None;
                    runtime.proxy_check.clone()
                };
                let _ = state.refresh_running_pools();
                state.notice(
                    if cancelled { "info" } else { "ok" },
                    format!(
                        "{} {total} proxies: {} healthy, {} slow, {} failed",
                        if cancelled {
                            "Check cancelled after"
                        } else {
                            "Checked"
                        },
                        summary.healthy,
                        summary.slow,
                        summary.failed
                    ),
                );
                state.mark_proxies_dirty();
            }
            AppEvent::ProxyImportFinished {
                path,
                imported,
                duplicates,
                invalid,
                error,
            } => {
                state.runtime().import_running = false;
                match error {
                    Some(error) => state.notice("error", format!("Import failed: {error}")),
                    None => {
                        let _ = state.refresh_running_pools();
                        state.notice(
                            "ok",
                            format!(
                                "{path}: imported {imported}, skipped {duplicates} duplicates and {invalid} invalid lines"
                            ),
                        );
                    }
                }
                state.mark_proxies_dirty();
            }
        }
    }
}

/// Coalesces bursts of progress events into browser refresh signals. The
/// small progress card refreshes often so a running check feels live; the
/// proxy table and counts refresh at most every few seconds while a check is
/// writing results, then right away once it finishes.
async fn flush_loop(state: SharedState) {
    const TICK: Duration = Duration::from_millis(400);
    const TABLE_GAP_WHILE_CHECKING: Duration = Duration::from_secs(3);
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tick: u64 = 0;
    let mut last_table = Instant::now();
    loop {
        interval.tick().await;
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(2) && state.jobs_dirty.swap(false, Ordering::Relaxed) {
            let _ = state.ui_tx.send(UiEvent::Jobs);
        }
        if state.progress_dirty.swap(false, Ordering::Relaxed) {
            let _ = state.ui_tx.send(UiEvent::ProxyProgress);
        }
        if tick.is_multiple_of(5) && state.leads_dirty.swap(false, Ordering::Relaxed) {
            let _ = state.ui_tx.send(UiEvent::Leads);
        }
        let checking = state.runtime().proxy_check.running;
        let due = !checking || last_table.elapsed() >= TABLE_GAP_WHILE_CHECKING;
        if due && state.proxies_dirty.swap(false, Ordering::Relaxed) {
            last_table = Instant::now();
            let _ = state.ui_tx.send(UiEvent::Proxies);
        }
    }
}
