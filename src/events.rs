use crate::models::JobStatus;

/// Live counters of a running search, for its progress bars.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveProgress {
    /// Map lookups (one per query and area) finished so far.
    pub lookups_done: i64,
    pub total_lookups: i64,
    /// Leads whose emails are waiting on mail-server checks.
    pub checking: i64,
    /// Google Maps is done and the websites are being read.
    pub reading_sites: bool,
}

#[derive(Debug)]
pub enum AppEvent {
    JobProgress {
        job_id: String,
        message: String,
        live: LiveProgress,
    },
    JobFinished {
        job_id: String,
        status: JobStatus,
        message: String,
    },
    /// A search wound down for a restart; it carries on afterwards.
    JobSuspended {
        job_id: String,
    },
    /// A batch of proxy check results was stored.
    ProxyChecksStored {
        healthy: usize,
        slow: usize,
        failed: usize,
    },
    ProxyCheckFinished {
        total: usize,
        cancelled: bool,
    },
    ProxyImportFinished {
        path: String,
        imported: usize,
        duplicates: usize,
        invalid: usize,
        error: Option<String>,
    },
}
