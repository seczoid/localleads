use crate::db::{Database, ProxyCheckRecord};
use crate::error::LocalLeadsError;
use crate::events::AppEvent;
use crate::models::ProxyEntry;
use anyhow::Result;
use reqwest::StatusCode;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ImportSummary {
    pub imported: usize,
    pub duplicates: usize,
    pub invalid: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProxyTestConfig {
    pub endpoint: String,
    pub timeout: Duration,
    pub retries: u32,
    pub max_latency_ms: u64,
    pub expected_body: Option<String>,
    pub verify_exit_ip: bool,
}

impl Default for ProxyTestConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://www.google.com/maps".to_owned(),
            timeout: Duration::from_secs(15),
            retries: 2,
            max_latency_ms: 10_000,
            expected_body: Some("google".to_owned()),
            verify_exit_ip: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProxyCheckOutcome {
    pub proxy: ProxyEntry,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub exit_ip: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ProxyPool {
    entries: Arc<Mutex<Vec<ProxyEntry>>>,
    cursor: Arc<AtomicUsize>,
}

impl ProxyPool {
    pub fn new(entries: Vec<ProxyEntry>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(entries)),
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn select(&self) -> Option<ProxyEntry> {
        let entries = self.entries.lock().ok()?;
        if entries.is_empty() {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        (0..entries.len())
            .map(|offset| &entries[(start + offset) % entries.len()])
            .find(|proxy| {
                proxy.enabled && proxy.status != "failed" && proxy.consecutive_failures < 3
            })
            .cloned()
    }

    #[cfg(feature = "verify")]
    /// Every proxy `select` could still hand out.
    pub fn healthy(&self) -> Vec<ProxyEntry> {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|proxy| {
                        proxy.enabled && proxy.status != "failed" && proxy.consecutive_failures < 3
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn mark_failure(&self, id: &str) {
        if let Ok(mut entries) = self.entries.lock()
            && let Some(proxy) = entries.iter_mut().find(|proxy| proxy.id == id)
        {
            proxy.status = "failed".to_owned();
            proxy.consecutive_failures = proxy.consecutive_failures.saturating_add(1);
        }
    }

    pub fn replace_entries(&self, entries: Vec<ProxyEntry>) {
        if let Ok(mut current) = self.entries.lock() {
            *current = entries;
        }
    }
}

pub fn parse_proxy_line(raw: &str, default_protocol: &str) -> Result<ProxyEntry, LocalLeadsError> {
    let value = raw.trim();
    if value.is_empty() || value.starts_with('#') {
        return Err(LocalLeadsError::InvalidProxy(
            "empty or comment line".to_owned(),
        ));
    }
    let protocol = normalize_protocol(default_protocol)?;
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("{protocol}://{value}")
    };
    let mut parsed = url::Url::parse(&candidate)
        .map_err(|error| LocalLeadsError::InvalidProxy(error.to_string()))?;
    let actual_protocol = normalize_protocol(parsed.scheme())?;
    if parsed.host_str().is_none() {
        return Err(LocalLeadsError::InvalidProxy("host is missing".to_owned()));
    }
    let port = parsed
        .port()
        .ok_or_else(|| LocalLeadsError::InvalidProxy("port is missing".to_owned()))?;
    if port == 0 {
        return Err(LocalLeadsError::InvalidProxy(
            "port must be between 1 and 65535".to_owned(),
        ));
    }
    parsed.set_path("");
    parsed.set_query(None);
    parsed.set_fragment(None);
    let username = if parsed.username().is_empty() {
        None
    } else {
        Some(parsed.username().to_owned())
    };
    let password = parsed.password().map(ToOwned::to_owned);
    Ok(ProxyEntry {
        id: Uuid::new_v4().to_string(),
        url: parsed.to_string(),
        protocol: actual_protocol,
        username,
        password,
        enabled: true,
        status: "unknown".to_owned(),
        latency_ms: None,
        last_checked_at: None,
        consecutive_failures: 0,
        last_error: None,
    })
}

pub fn normalize_protocol(value: &str) -> Result<String, LocalLeadsError> {
    let protocol = value.trim().trim_end_matches(':').to_ascii_lowercase();
    if ["http", "https", "socks4", "socks5", "socks5h"].contains(&protocol.as_str()) {
        Ok(protocol)
    } else {
        Err(LocalLeadsError::InvalidProxy(format!(
            "unsupported protocol '{value}'; use http, https, socks4, socks5, or socks5h"
        )))
    }
}

pub fn import_proxy_text(database: &Database, text: &str, protocol: &str) -> Result<ImportSummary> {
    let mut duplicates = 0;
    let mut invalid = Vec::new();
    let existing = database.list_proxies()?;
    let mut known: std::collections::HashSet<String> =
        existing.into_iter().map(|proxy| proxy.url).collect();
    let mut pending = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        match parse_proxy_line(line, protocol) {
            Ok(proxy) => {
                if !known.insert(proxy.url.clone()) {
                    duplicates += 1;
                } else {
                    pending.push(proxy);
                }
            }
            Err(error) => invalid.push(format!("line {}: {}", line_number + 1, error)),
        }
    }
    database.save_proxies_batch(&pending)?;
    Ok(ImportSummary {
        imported: pending.len(),
        duplicates,
        invalid,
    })
}

/// The expected text must appear within this many bytes of the response.
const BODY_SCAN_LIMIT: usize = 256 * 1024;

/// One TLS configuration shared by every test client: the root store is built
/// once and the session cache is shared, so repeat handshakes with the test
/// endpoint resume instead of starting from scratch.
fn shared_tls_config() -> rustls::ClientConfig {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let mut config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("rustls default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            config
        })
        .clone()
}

/// Cancels a proxy check run, including checks already in flight.
#[derive(Clone)]
pub struct CancelToken(Arc<watch::Sender<bool>>);

impl Default for CancelToken {
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl CancelToken {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

struct AttemptError {
    message: String,
    /// The proxy could not be reached at all. If that happens before any
    /// attempt succeeded, more attempts will not help.
    unreachable: bool,
}

impl AttemptError {
    fn from_request(error: &reqwest::Error) -> Self {
        let mut cause: &dyn std::error::Error = error;
        while let Some(source) = cause.source() {
            cause = source;
        }
        let detail = cause.to_string();
        if error.is_timeout() {
            Self {
                message: "timed out".to_owned(),
                unreachable: true,
            }
        } else if error.is_connect() {
            Self {
                message: format!("could not connect through proxy: {detail}"),
                unreachable: true,
            }
        } else {
            Self {
                message: detail,
                unreachable: false,
            }
        }
    }
}

fn failed_outcome(proxy: ProxyEntry, error: String) -> ProxyCheckOutcome {
    ProxyCheckOutcome {
        proxy,
        status: "failed".to_owned(),
        latency_ms: None,
        exit_ip: None,
        error: Some(error),
    }
}

fn test_client(proxy: &ProxyEntry, config: &ProxyTestConfig) -> Result<reqwest::Client, String> {
    let route =
        reqwest::Proxy::all(&proxy.url).map_err(|error| format!("configure proxy: {error}"))?;
    reqwest::Client::builder()
        .proxy(route)
        .tls_backend_preconfigured(shared_tls_config())
        .timeout(config.timeout)
        .tcp_nodelay(true)
        .pool_max_idle_per_host(1)
        .build()
        .map_err(|error| format!("build proxy test client: {error}"))
}

/// Tests one proxy. Attempts stop as soon as the outcome is decided: once a
/// majority succeeded, once a majority can no longer succeed, or when the very
/// first attempt cannot reach the proxy at all (dead proxies cost one attempt).
pub async fn check_proxy(proxy: ProxyEntry, config: &ProxyTestConfig) -> ProxyCheckOutcome {
    let client = match test_client(&proxy, config) {
        Ok(client) => client,
        Err(error) => return failed_outcome(proxy, error),
    };
    let attempts = config.retries.saturating_add(1) as usize;
    let required = attempts.div_ceil(2);
    let mut successes = Vec::with_capacity(attempts);
    let mut failures = 0;
    let mut last_error = None;
    for _ in 0..attempts {
        match check_once(&client, config).await {
            Ok(result) => {
                successes.push(result);
                if successes.len() >= required {
                    break;
                }
            }
            Err(error) => {
                failures += 1;
                let dead = error.unreachable && successes.is_empty();
                let give_up = dead || attempts - failures < required;
                last_error = Some(error.message);
                if give_up {
                    break;
                }
            }
        }
    }
    if successes.len() < required {
        let detail = last_error.unwrap_or_else(|| "no successful responses".to_owned());
        return failed_outcome(
            proxy,
            format!("{}/{} checks succeeded; {detail}", successes.len(), attempts),
        );
    }

    successes.sort_unstable_by_key(|(latency_ms, _)| *latency_ms);
    let (latency_ms, exit_ip) = successes[successes.len() / 2].clone();
    ProxyCheckOutcome {
        proxy,
        status: if latency_ms as u64 <= config.max_latency_ms {
            "healthy"
        } else {
            "slow"
        }
        .to_owned(),
        latency_ms: Some(latency_ms),
        exit_ip,
        error: None,
    }
}

/// One request through the proxy. The body is streamed and abandoned as soon
/// as the expected text shows up, so a healthy proxy never has to download
/// the whole page.
async fn check_once(
    client: &reqwest::Client,
    config: &ProxyTestConfig,
) -> Result<(i64, Option<String>), AttemptError> {
    let started = Instant::now();
    let mut response = client
        .get(&config.endpoint)
        .send()
        .await
        .map_err(|error| AttemptError::from_request(&error))?;
    let status = response.status();
    if !status.is_success() && !status.is_redirection() {
        return Err(AttemptError {
            message: format!("endpoint returned HTTP {status}"),
            unreachable: status == StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        });
    }
    match config.expected_body.as_deref() {
        Some(expected) => {
            let needle = expected.to_ascii_lowercase().into_bytes();
            let mut window: Vec<u8> = Vec::with_capacity(16 * 1024);
            let mut scanned = 0usize;
            let mut found = false;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| AttemptError::from_request(&error))?
            {
                scanned += chunk.len();
                window.extend(chunk.iter().map(u8::to_ascii_lowercase));
                if window.windows(needle.len()).any(|part| part == needle) {
                    found = true;
                    break;
                }
                if scanned >= BODY_SCAN_LIMIT {
                    break;
                }
                // Keep only the tail that could still begin a match.
                let keep = needle.len().saturating_sub(1);
                if window.len() > keep {
                    window.drain(..window.len() - keep);
                }
            }
            if !found {
                return Err(AttemptError {
                    message: format!("response did not contain \"{expected}\""),
                    unreachable: false,
                });
            }
        }
        None => {
            // The first bytes of the body prove the tunnel carries data.
            response
                .chunk()
                .await
                .map_err(|error| AttemptError::from_request(&error))?;
        }
    }
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let exit_ip = if config.verify_exit_ip {
        let lookup = async {
            client
                .get("https://api.ipify.org")
                .send()
                .await?
                .text()
                .await
        };
        Some(
            lookup
                .await
                .map_err(|error| AttemptError::from_request(&error))?
                .trim()
                .to_owned(),
        )
    } else {
        None
    };
    Ok((latency_ms, exit_ip))
}

/// Checks many proxies at once. Every check is its own Tokio task, so checks
/// run in parallel on all CPU cores, bounded by `concurrency`. Results go to
/// a single writer that stores them in batched transactions, keeping SQLite
/// off the async worker threads.
pub async fn check_many_with_events(
    database: Database,
    proxies: Vec<ProxyEntry>,
    config: ProxyTestConfig,
    concurrency: usize,
    events: mpsc::UnboundedSender<AppEvent>,
    cancel: CancelToken,
) {
    let limit = concurrency.clamp(1, MAX_CONCURRENCY);
    let config = Arc::new(config);
    let permits = Arc::new(Semaphore::new(limit));
    let (results_tx, results_rx) = mpsc::channel::<ProxyCheckOutcome>(limit * 2);
    let writer = tokio::spawn(store_results(
        database,
        config.endpoint.clone(),
        results_rx,
        events.clone(),
    ));

    let mut tasks = JoinSet::new();
    for proxy in proxies {
        let permit = tokio::select! {
            permit = permits.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
            () = cancel.cancelled() => break,
        };
        let config = config.clone();
        let results = results_tx.clone();
        let cancel = cancel.clone();
        tasks.spawn(async move {
            let _permit = permit;
            tokio::select! {
                outcome = check_proxy(proxy, &config) => {
                    let _ = results.send(outcome).await;
                }
                () = cancel.cancelled() => {}
            }
        });
        while tasks.try_join_next().is_some() {}
    }
    drop(results_tx);
    while tasks.join_next().await.is_some() {}
    let total = writer.await.unwrap_or(0);
    let _ = events.send(AppEvent::ProxyCheckFinished {
        total,
        cancelled: cancel.is_cancelled(),
    });
}

pub const MAX_CONCURRENCY: usize = 2048;

async fn store_results(
    database: Database,
    endpoint: String,
    mut results: mpsc::Receiver<ProxyCheckOutcome>,
    events: mpsc::UnboundedSender<AppEvent>,
) -> usize {
    let mut stored = 0;
    let mut batch = Vec::with_capacity(512);
    while results.recv_many(&mut batch, 512).await > 0 {
        let records: Vec<ProxyCheckRecord> = batch
            .drain(..)
            .map(|outcome| ProxyCheckRecord {
                proxy_id: outcome.proxy.id,
                status: outcome.status,
                latency_ms: outcome.latency_ms,
                exit_ip: outcome.exit_ip,
                error: outcome.error,
            })
            .collect();
        let (mut healthy, mut slow, mut failed) = (0, 0, 0);
        for record in &records {
            match record.status.as_str() {
                "healthy" => healthy += 1,
                "slow" => slow += 1,
                _ => failed += 1,
            }
        }
        stored += records.len();
        let db = database.clone();
        let endpoint = endpoint.clone();
        let saved =
            tokio::task::spawn_blocking(move || db.record_proxy_checks(&endpoint, &records)).await;
        if let Ok(Err(error)) | Err(error) = saved.map_err(anyhow::Error::from) {
            crate::serverlog::error!("Couldn't store proxy check results: {error:#}");
        }
        let _ = events.send(AppEvent::ProxyChecksStored {
            healthy,
            slow,
            failed,
        });
    }
    stored
}

/// A proxy URL without its password, for logs.
pub fn redact(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) if parsed.password().is_some() => {
            let _ = parsed.set_password(Some("***"));
            parsed.to_string()
        }
        _ => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_normalizes_authenticated_proxy() {
        let proxy = parse_proxy_line("alice:secret@example.com:8080", "http").expect("valid proxy");
        assert_eq!(proxy.protocol, "http");
        assert_eq!(proxy.username.as_deref(), Some("alice"));
        assert_eq!(proxy.password.as_deref(), Some("secret"));
        assert!(proxy.masked_url().contains("******"));
        assert!(!proxy.masked_url().contains("secret"));
    }

    #[tokio::test]
    async fn unreachable_proxy_fails_fast_without_retrying() {
        // Nothing listens on port 1, so the connection is refused at once.
        let proxy = parse_proxy_line("127.0.0.1:1", "http").expect("valid proxy");
        let config = ProxyTestConfig {
            retries: 4,
            ..ProxyTestConfig::default()
        };
        let started = Instant::now();
        let outcome = check_proxy(proxy, &config).await;
        assert_eq!(outcome.status, "failed");
        let error = outcome.error.expect("failure reason");
        assert!(error.starts_with("0/5 checks succeeded"), "{error}");
        assert!(error.contains("could not connect"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn parser_rejects_missing_port() {
        assert!(parse_proxy_line("example.com", "http").is_err());
    }
}
