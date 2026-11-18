//! Shared server state: running job controls, proxy pools, the proxy checker,
//! notices, and the broadcast channel that drives live browser updates.

use crate::db::Database;
use crate::db::EnrichCounts;
use crate::events::{AppEvent, LiveProgress};
use crate::models::{JobStatus, ProxyEntry, SearchConfig};
use crate::proxy::{
    CancelToken, MAX_CONCURRENCY, ProxyPool, ProxyTestConfig, check_many_with_events,
    import_proxy_text,
};
use crate::scraper::{JobControl, run_job};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

const CHECKER_SETTINGS_KEY: &str = "proxy_checker";
const MAX_NOTICES: usize = 50;

/// The endpoint that restores a trash entry.
pub fn undo_url(trash_id: &str) -> String {
    format!("/trash/{trash_id}/restore")
}

#[derive(Clone, Debug, Serialize)]
pub struct Notice {
    pub level: &'static str,
    pub message: String,
    pub at: String,
    /// Where to POST to undo what the notice reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo: Option<String>,
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
    /// The update card in the sidebar changed.
    Update,
    Notice(Notice),
    /// A search finished, failed or was stopped: tabs that allow it raise a
    /// system notification.
    JobFinished(JobFinished),
}

#[derive(Clone, Debug, Serialize)]
pub struct JobFinished {
    pub id: String,
    pub name: String,
    pub status: String,
    pub message: String,
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
    /// Latest live counters per running job.
    live: HashMap<String, LiveProgress>,
    /// Website-reading counts per job, reused for a few seconds because
    /// counting a big search's leads is slow.
    enrich_counts: HashMap<String, (Instant, EnrichCounts)>,
    /// When each search started reading websites, and how many it had read.
    web_start: HashMap<String, (Instant, i64)>,
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
    /// Bumped by every change a live view shows, so a cached render is
    /// dropped the moment it could be out of date.
    changes: AtomicU64,
    /// Live partials rendered once and shared by every open tab:
    /// key -> (rendered at, `changes` then, html).
    live_pages: Mutex<HashMap<String, (Instant, u64, String)>>,
}

/// Longest a shared live render is reused, even with nothing marked changed
/// (counters that move without an event, like elapsed time).
const LIVE_PAGE_FOR: Duration = Duration::from_millis(1500);

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
            changes: AtomicU64::new(0),
            live_pages: Mutex::new(HashMap::new()),
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
        self.notice_with_undo(level, message, None);
    }

    /// A notice whose toast offers Undo, restoring the trash entry `undo`.
    pub fn notice_with_undo(&self, level: &'static str, message: impl Into<String>, undo: Option<&str>) {
        let notice = Notice {
            level,
            message: message.into(),
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            undo: undo.map(undo_url),
        };
        crate::serverlog::record(
            match level {
                "error" | "bad" => "error",
                "warn" => "warn",
                _ => "info",
            },
            notice.message.clone(),
        );
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
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.jobs_dirty.store(true, Ordering::Relaxed);
    }

    pub fn mark_proxies_dirty(&self) {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.proxies_dirty.store(true, Ordering::Relaxed);
    }

    pub fn mark_leads_dirty(&self) {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.leads_dirty.store(true, Ordering::Relaxed);
    }

    /// A live partial (search progress, the searches list) rendered once
    /// and shared: every tab refreshes on the same signal, so without this
    /// each would build the same page from the database again.
    pub fn shared_render<E>(
        &self,
        key: &str,
        render: impl FnOnce() -> Result<String, E>,
    ) -> Result<String, E> {
        let changes = self.changes.load(Ordering::Relaxed);
        if let Ok(pages) = self.live_pages.lock()
            && let Some((at, seen, html)) = pages.get(key)
            && *seen == changes
            && at.elapsed() < LIVE_PAGE_FOR
        {
            return Ok(html.clone());
        }
        let html = render()?;
        if let Ok(mut pages) = self.live_pages.lock() {
            pages.retain(|_, (at, _, _)| at.elapsed() < LIVE_PAGE_FOR);
            pages.insert(key.to_owned(), (Instant::now(), changes, html.clone()));
        }
        Ok(html)
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
                    let saved = state_ref
                        .db
                        .save_enrichment_async(target.id.clone(), patch, summary)
                        .await;
                    state_ref.mark_leads_dirty();
                    saved.is_ok() && has_email
                })
                .buffer_unordered(64)
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
        self.launch_job(&id, &config, false, false)?;
        self.notice("ok", format!("Started search \"{}\"", config.job_name));
        Ok(id)
    }

    fn launch_job(&self, id: &str, config: &SearchConfig, paused: bool, resumed: bool) -> Result<()> {
        if crate::update::restarting() {
            bail!("LocalLeads is restarting for an update; try again in a moment");
        }
        let pool = ProxyPool::new(
            self.db
                .list_proxies_for_group(config.proxy_group_id.as_deref())?,
        );
        let control = JobControl::new();
        if paused {
            control.toggle_pause();
        }
        // Claimed before the task starts, so another process starting up
        // now sees this search as taken.
        self.db.claim_job_runner(id)?;
        {
            let mut runtime = self.runtime();
            runtime.controls.insert(id.to_owned(), control.clone());
            runtime.pools.insert(id.to_owned(), pool.clone());
        }
        tokio::spawn(run_job(
            self.db.clone(),
            id.to_owned(),
            config.clone(),
            pool,
            control,
            self.events_tx.clone(),
            resumed,
        ));
        self.mark_jobs_dirty();
        Ok(())
    }

    /// Picks up the searches that were running when the app last stopped
    /// (a crash, a restart or a shutdown), from where each left off.
    pub fn resume_orphaned_jobs(&self) -> Result<usize> {
        let jobs = self.db.orphaned_jobs(&|id| self.is_live(id))?;
        for (id, config, paused) in &jobs {
            if let Err(error) = self.launch_job(id, config, *paused, true) {
                let message = format!("Couldn't pick this search up again: {error}");
                let _ = self.db.update_job_status(id, JobStatus::Interrupted, Some(&message));
            }
        }
        if !jobs.is_empty() {
            self.notice(
                "info",
                format!(
                    "Picked up {} {} where {} left off",
                    jobs.len(),
                    if jobs.len() == 1 { "search" } else { "searches" },
                    if jobs.len() == 1 { "it" } else { "they" }
                ),
            );
        }
        Ok(jobs.len())
    }

    /// Keeps looking for a while for searches whose earlier process was
    /// still winding down at start-up, and picks those up too.
    pub async fn adopt_late_orphans(self: Arc<Self>) {
        for _ in 0..24 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if crate::update::restarting() {
                return;
            }
            if let Err(error) = self.resume_orphaned_jobs() {
                crate::serverlog::error!("Couldn't pick up unfinished searches: {error:#}");
            }
        }
    }

    /// Winds every running search down for a restart, keeping each one's
    /// status so it carries on afterwards. Waits up to `wait` for them to
    /// save what they finished.
    pub async fn suspend_jobs(&self, wait: std::time::Duration) {
        let controls: Vec<JobControl> = self.runtime().controls.values().cloned().collect();
        for control in &controls {
            control.suspend();
        }
        let deadline = Instant::now() + wait;
        while !self.runtime().controls.is_empty() && Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    pub fn update_changed(&self) {
        let _ = self.ui_tx.send(UiEvent::Update);
    }

    /// Installs the update on offer and restarts into it, in the
    /// background. Errors only when it can't start at all.
    pub fn start_update(self: &Arc<Self>) -> Result<()> {
        let release = crate::update::begin()?;
        self.update_changed();
        let state = self.clone();
        tokio::spawn(async move {
            if let Err(error) = state.run_update(&release).await {
                crate::serverlog::error!("Update to {} failed: {error:#}", release.version);
                crate::update::set_phase(crate::update::Phase::Failed(format!("{error:#}")));
                state.update_changed();
                state.notice("error", format!("Couldn't update to {}: {error:#}", release.version));
            }
        });
        Ok(())
    }

    async fn run_update(self: &Arc<Self>, release: &crate::update::Release) -> Result<()> {
        use crate::update::{Phase, set_phase};
        let release = crate::update::refresh(&self.db_path, release.clone()).await;
        let staged = crate::update::prepare(&self.db_path, &release).await?;
        set_phase(Phase::Busy("Backing up your data…".to_owned()));
        self.update_changed();
        {
            let db_path = self.db_path.clone();
            let version = release.version.clone();
            match tokio::task::spawn_blocking(move || {
                crate::update::backup_database(&db_path, &version)
            })
            .await?
            {
                Ok(Some(path)) => crate::serverlog::info!("Backed the database up to {}", path.display()),
                Ok(None) => crate::serverlog::warning!("Not enough disk space to back the database up first; updating anyway"),
                Err(error) => bail!("couldn't back up the database: {error:#}"),
            }
        }
        {
            let db_path = self.db_path.clone();
            let staged = staged.clone();
            tokio::task::spawn_blocking(move || crate::update::verify_staged(&db_path, &staged))
                .await??;
        }
        set_phase(Phase::Restarting);
        self.update_changed();
        crate::serverlog::info!("Restarting into LocalLeads {}", release.version);
        self.suspend_jobs(std::time::Duration::from_secs(15)).await;
        // Lets the browser hear that the restart is underway.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let db_path = self.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::update::restart_into(&db_path, &staged)
        })
        .await?;
        // Only reached when the new version couldn't be started: carry on
        // here, searches included.
        let error = result.err().unwrap_or_else(|| anyhow!("couldn't start the new version"));
        set_phase(Phase::Failed(format!("{error:#}")));
        self.update_changed();
        if let Err(error) = self.resume_orphaned_jobs() {
            crate::serverlog::error!("Couldn't pick up unfinished searches: {error:#}");
        }
        Err(error)
    }

    /// Carries on a stopped, failed or interrupted search: finished areas
    /// are skipped and its leads stay in the same list.
    pub fn continue_job(&self, job_id: &str) -> Result<()> {
        let job = self
            .db
            .get_job(job_id)?
            .ok_or_else(|| anyhow!("Search not found"))?;
        if job.status.is_active() || self.is_live(job_id) {
            bail!("This search is still running");
        }
        if job.status == JobStatus::Completed {
            bail!("This search is already complete");
        }
        self.launch_job(job_id, &job.config, false, true)?;
        self.notice("ok", format!("Carrying on with \"{}\"", job.name));
        Ok(())
    }

    pub fn job_activity(&self, job_id: &str) -> Option<String> {
        self.runtime().activity.get(job_id).cloned()
    }

    pub fn job_live_progress(&self, job_id: &str) -> Option<LiveProgress> {
        self.runtime().live.get(job_id).copied()
    }

    /// How many of a search's leads have had their website read, counted
    /// at most every few seconds per search.
    pub fn job_enrich_counts(&self, job_id: &str) -> Result<EnrichCounts> {
        const FRESH_FOR: Duration = Duration::from_secs(4);
        if let Some((at, counts)) = self.runtime().enrich_counts.get(job_id)
            && at.elapsed() < FRESH_FOR
        {
            return Ok(*counts);
        }
        let counts = self.db.job_enrich_counts(job_id)?;
        self.runtime()
            .enrich_counts
            .insert(job_id.to_owned(), (Instant::now(), counts));
        Ok(counts)
    }

    /// Websites read a minute since the search started reading them, once
    /// there is half a minute to go on.
    pub fn job_web_pace(&self, job_id: &str, read: i64) -> Option<f64> {
        let reading = self
            .job_live_progress(job_id)
            .is_some_and(|live| live.reading_sites);
        if !reading {
            return None;
        }
        let mut runtime = self.runtime();
        let (at, first) = *runtime
            .web_start
            .entry(job_id.to_owned())
            .or_insert((Instant::now(), read));
        let seconds = at.elapsed().as_secs_f64();
        (seconds >= 30.0 && read > first).then(|| (read - first) as f64 * 60.0 / seconds)
    }

    /// Searches this process is running now.
    pub fn running_searches(&self) -> usize {
        self.runtime().controls.len()
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
            AppEvent::JobProgress { job_id, message, live } => {
                let mut runtime = state.runtime();
                runtime.live.insert(job_id.clone(), live);
                runtime.activity.insert(job_id, message);
                drop(runtime);
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
                    runtime.live.remove(&job_id);
                    runtime.enrich_counts.remove(&job_id);
                    runtime.web_start.remove(&job_id);
                    drop(runtime);
                    state
                        .db
                        .get_job(&job_id)
                        .ok()
                        .flatten()
                        .map(|job| job.name)
                        .unwrap_or_else(|| job_id.clone())
                };
                let level = match status {
                    JobStatus::Completed => "ok",
                    JobStatus::Failed => "error",
                    _ => "info",
                };
                state.notice(level, format!("{name}: {status} — {message}"));
                let _ = state.ui_tx.send(UiEvent::JobFinished(JobFinished {
                    id: job_id.clone(),
                    name: name.clone(),
                    status: status.to_string(),
                    message: message.clone(),
                }));
                state.mark_jobs_dirty();
            }
            AppEvent::JobSuspended { job_id } => {
                let mut runtime = state.runtime();
                runtime.controls.remove(&job_id);
                runtime.pools.remove(&job_id);
                runtime.activity.remove(&job_id);
                runtime.live.remove(&job_id);
                runtime.enrich_counts.remove(&job_id);
                runtime.web_start.remove(&job_id);
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
