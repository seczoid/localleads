use crate::models::JobStatus;

#[derive(Debug)]
pub enum AppEvent {
    JobProgress {
        job_id: String,
        message: String,
    },
    JobFinished {
        job_id: String,
        status: JobStatus,
        message: String,
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
