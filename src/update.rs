//! Updates LocalLeads in place from its GitHub releases.
//!
//! The installed program is never overwritten: it often belongs to root
//! (`/usr/local/bin`) or sits inside a container image. A new version is
//! downloaded into an `updates` folder beside the database, checked against
//! the release's SHA256SUMS, test-run, and then the running server restarts
//! into it in place (same process id, so systemd, launchd and Docker carry
//! on as if nothing happened).
//!
//! Every later start goes through [`hand_off`]: the installed program
//! starts the newest downloaded version that works. A new version counts
//! as working once it has served for [`CONFIRM_AFTER`]; one that fails to
//! get there [`MAX_TRIES`] times is set aside and the last working version
//! runs instead. A newer installed program (from setup.sh or a new Docker
//! image) always wins over an older download.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const REPO_URL: &str = "https://github.com/seczoid/localleads";
const API: &str = "https://api.github.com/repos/seczoid/localleads";
/// Points update checks at a stand-in for GitHub's API (for testing).
const API_ENV: &str = "LOCALLEADS_UPDATE_API";

fn api() -> String {
    std::env::var(API_ENV)
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| API.to_owned())
}
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the app looks for a new release (and the star count).
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// First look, once the server has settled after starting.
const FIRST_CHECK: Duration = Duration::from_secs(20);
/// Downloads bigger than this are refused.
const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;
/// Starts a new version gets before it counts as broken.
const MAX_TRIES: u32 = 3;
/// How long a new version must serve before it counts as working.
pub const CONFIRM_AFTER: Duration = Duration::from_secs(30);
/// Set for a process started by [`hand_off`] or by an update, so it never
/// hands off again.
const LAUNCHED_ENV: &str = "LOCALLEADS_LAUNCHED";
/// The version an update came from, for the "Updated to …" notice.
const UPDATED_FROM_ENV: &str = "LOCALLEADS_UPDATED_FROM";
/// A version that didn't start, for the notice that says so.
const SET_ASIDE_ENV: &str = "LOCALLEADS_UPDATE_SET_ASIDE";
/// `LOCALLEADS_UPDATES=off` turns update checks and the button off.
const SWITCH_ENV: &str = "LOCALLEADS_UPDATES";

pub fn enabled() -> bool {
    !matches!(
        std::env::var(SWITCH_ENV).ok().as_deref().map(str::trim),
        Some("off" | "0" | "false" | "no")
    )
}

// ------------------------------------------------------------ the download folder

fn updates_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("updates")
}

/// A downloaded version.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Staged {
    version: String,
    /// File name inside the updates folder.
    file: String,
    sha256: String,
    /// Starts so far; only counted while the version is still on trial.
    #[serde(default)]
    tries: u32,
}

/// `updates/state.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Ledger {
    /// The newest version known to work.
    working: Option<Staged>,
    /// A version on trial: it hasn't yet served for [`CONFIRM_AFTER`].
    trial: Option<Staged>,
    /// Versions that never got there; never offered again.
    #[serde(default)]
    set_aside: Vec<String>,
}

impl Ledger {
    fn path(dir: &Path) -> PathBuf {
        dir.join("state.json")
    }

    fn load(dir: &Path) -> Self {
        std::fs::read(Self::path(dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Written to a temporary file and renamed, so a crash mid-write never
    /// leaves half a file.
    fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let temporary = dir.join("state.json.tmp");
        {
            let mut file = std::fs::File::create(&temporary)?;
            std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, Self::path(dir))?;
        Ok(())
    }
}

fn newer_than_running(version: &str) -> bool {
    match (semver::Version::parse(version), semver::Version::parse(VERSION)) {
        (Ok(theirs), Ok(ours)) => theirs > ours,
        _ => false,
    }
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `staged` is a newer version whose file is intact.
fn usable(dir: &Path, staged: &Staged) -> bool {
    newer_than_running(&staged.version)
        && file_sha256(&dir.join(&staged.file)).is_ok_and(|sum| sum == staged.sha256)
}

// ------------------------------------------------------------ starting

/// Called once at server start, before anything else: starts the newest
/// downloaded version that works instead of this one, if there is one.
/// Returns only when this program should run itself.
///
/// This runs even with update checks turned off, so turning them off never
/// quietly goes back to an older version.
pub fn hand_off(db_path: &Path) {
    if std::env::var_os(LAUNCHED_ENV).is_some() {
        return;
    }
    let dir = updates_dir(db_path);
    let mut ledger = Ledger::load(&dir);
    let mut set_aside = None;
    if let Some(mut trial) = ledger.trial.clone() {
        if !usable(&dir, &trial) {
            // Older than this program, or the file is gone or damaged.
            ledger.trial = None;
            let _ = ledger.save(&dir);
        } else if trial.tries >= MAX_TRIES {
            eprintln!(
                "LocalLeads {} didn't start properly after {} tries, so it was set aside",
                trial.version, trial.tries
            );
            ledger.set_aside.push(trial.version.clone());
            set_aside = Some(trial.version);
            ledger.trial = None;
            let _ = ledger.save(&dir);
        } else {
            trial.tries += 1;
            ledger.trial = Some(trial.clone());
            if ledger.save(&dir).is_ok() {
                start(&dir, &trial, None, None);
            }
        }
    }
    if let Some(working) = ledger.working.clone().filter(|working| usable(&dir, working)) {
        start(&dir, &working, None, set_aside.as_deref());
    }
    if let Some(version) = set_aside {
        let _ = SET_ASIDE.set(version);
    }
}

static SET_ASIDE: OnceLock<String> = OnceLock::new();

/// Replaces this process with `staged` (Unix), or starts it and exits
/// (Windows). Returns only if that failed.
fn start(dir: &Path, staged: &Staged, updated_from: Option<&str>, set_aside: Option<&str>) {
    let mut command = std::process::Command::new(dir.join(&staged.file));
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // At start-up the new version does exactly what was asked; restarting
    // for an update must not restore a backup or open a browser again.
    let args = if updated_from.is_some() { restart_args(args) } else { args };
    command.args(args).env(LAUNCHED_ENV, "1");
    if let Some(version) = updated_from {
        command.env(UPDATED_FROM_ENV, version);
    }
    if let Some(version) = set_aside {
        command.env(SET_ASIDE_ENV, version);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("Couldn't start LocalLeads {}: {error}", staged.version);
    }
    #[cfg(not(unix))]
    {
        match command.spawn() {
            Ok(_) => std::process::exit(0),
            Err(error) => eprintln!("Couldn't start LocalLeads {}: {error}", staged.version),
        }
    }
}

/// The arguments for restarting a running server: without `--restore`
/// (already done) and `--open` (the browser is open).
fn restart_args(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut kept = Vec::with_capacity(args.len());
    let mut skip_value = false;
    for arg in args {
        if std::mem::take(&mut skip_value) {
            continue;
        }
        let text = arg.to_string_lossy();
        if text == "--restore" {
            skip_value = true;
        } else if text == "--open" || text.starts_with("--restore=") {
        } else {
            kept.push(arg);
        }
    }
    kept
}

/// What to tell the user once the server is up: an update that just
/// finished, or one that was set aside.
pub fn startup_notice() -> Option<(&'static str, String)> {
    if let Some(version) = std::env::var(SET_ASIDE_ENV)
        .ok()
        .or_else(|| SET_ASIDE.get().cloned())
    {
        set_phase(Phase::Failed(format!("LocalLeads v{version} didn't start properly")));
        return Some((
            "error",
            format!("LocalLeads {version} didn't start properly, so you're still on {VERSION}"),
        ));
    }
    let from = std::env::var(UPDATED_FROM_ENV).ok()?;
    Some(("ok", format!("Updated from {from} to {VERSION}")))
}

/// Marks this version as working once it has served for a while, and
/// clears out downloads nothing needs any more.
pub async fn confirm_when_settled(db_path: PathBuf) {
    tokio::time::sleep(CONFIRM_AFTER).await;
    let _ = tokio::task::spawn_blocking(move || {
        let dir = updates_dir(&db_path);
        let mut ledger = Ledger::load(&dir);
        if let Some(trial) = ledger.trial.clone().filter(|trial| trial.version == VERSION) {
            ledger.working = Some(Staged { tries: 0, ..trial });
            ledger.trial = None;
            if ledger.save(&dir).is_err() {
                return;
            }
        }
        clean(&dir, &ledger);
    })
    .await;
}

/// Removes downloaded programs the ledger no longer names, and leftovers
/// of downloads that didn't finish. The newest backup stays.
fn clean(dir: &Path, ledger: &Ledger) {
    // An update underway owns its partial files.
    if !matches!(status().phase, Phase::Idle | Phase::Failed(_)) {
        return;
    }
    let keep: Vec<&str> = [&ledger.working, &ledger.trial]
        .into_iter()
        .flatten()
        .map(|staged| staged.file.as_str())
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let program = name.starts_with("localleads-") && !keep.contains(&name.as_str());
        if program || name.ends_with(".part") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ------------------------------------------------------------ checking

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// The release's page, for "What's new".
    pub page: String,
    asset: Option<(String, String)>,
    sums: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub enum Phase {
    #[default]
    Idle,
    /// Working through the update; the text says what's happening.
    Busy(String),
    /// About to restart into the new version.
    Restarting,
    Failed(String),
}

#[derive(Default)]
struct Status {
    latest: Option<Release>,
    stars: Option<u64>,
    checked: Option<Instant>,
    phase: Phase,
}

/// Everything the sidebar shows.
#[derive(Clone, Debug, Default)]
pub struct Sidebar {
    pub enabled: bool,
    pub version: &'static str,
    /// "1.2k", or empty while unknown.
    pub stars: String,
    pub repo_url: &'static str,
    /// The newer version on offer; empty when up to date.
    pub update: String,
    pub notes_url: String,
    /// Shown while an update runs; empty otherwise.
    pub busy: String,
    pub restarting: bool,
    pub failed: String,
    /// The update can't be installed here (no build for this system).
    pub manual_only: bool,
}

static STATUS: OnceLock<Mutex<Status>> = OnceLock::new();

fn status() -> std::sync::MutexGuard<'static, Status> {
    STATUS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn sidebar() -> Sidebar {
    let status = status();
    let (busy, restarting, failed) = match &status.phase {
        Phase::Idle => (String::new(), false, String::new()),
        Phase::Busy(text) => (text.clone(), false, String::new()),
        Phase::Restarting => ("Restarting…".to_owned(), true, String::new()),
        Phase::Failed(error) => (String::new(), false, capitalize(error)),
    };
    Sidebar {
        enabled: enabled(),
        version: VERSION,
        stars: status.stars.map(fmt_stars).unwrap_or_default(),
        repo_url: REPO_URL,
        update: status
            .latest
            .as_ref()
            .map(|release| release.version.clone())
            .unwrap_or_default(),
        notes_url: status
            .latest
            .as_ref()
            .map(|release| release.page.clone())
            .unwrap_or_default(),
        busy,
        restarting,
        failed,
        manual_only: status
            .latest
            .as_ref()
            .is_some_and(|release| release.asset.is_none() || release.sums.is_none()),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn fmt_stars(count: u64) -> String {
    if count >= 10_000 {
        format!("{}k", count / 1000)
    } else if count >= 1000 {
        let tenths = count / 100;
        if tenths % 10 == 0 {
            format!("{}k", tenths / 10)
        } else {
            format!("{}.{}k", tenths / 10, tenths % 10)
        }
    } else {
        count.to_string()
    }
}

/// The release asset built for this machine.
fn asset_name() -> Option<String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => return None,
    };
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    Some(format!("localleads-{target}.{extension}"))
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("LocalLeads/{VERSION} (+{REPO_URL})"))
        .connect_timeout(Duration::from_secs(20))
        .build()?)
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GithubRepo {
    stargazers_count: u64,
}

/// Looks for a newer stable release and the star count. A failed look
/// keeps what an earlier one found.
pub async fn check(db_path: &Path) -> Result<()> {
    let client = http()?;
    let stars = async {
        let repo: GithubRepo = client
            .get(api())
            .timeout(Duration::from_secs(20))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::Ok(repo.stargazers_count)
    };
    let latest = async {
        // "latest" is the newest release that is neither a draft nor a
        // pre-release, so the edge build is never offered.
        let release: GithubRelease = client
            .get(format!("{}/releases/latest", api()))
            .timeout(Duration::from_secs(20))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::Ok(release)
    };
    let (stars, latest) = tokio::join!(stars, latest);
    let set_aside = {
        let dir = updates_dir(db_path);
        tokio::task::spawn_blocking(move || Ledger::load(&dir).set_aside)
            .await
            .unwrap_or_default()
    };
    let mut status = status();
    if let Ok(stars) = stars {
        status.stars = Some(stars);
    }
    let release = latest?;
    status.checked = Some(Instant::now());
    let version = release.tag_name.trim_start_matches('v').to_owned();
    if release.draft
        || release.prerelease
        || !newer_than_running(&version)
        || set_aside.contains(&version)
    {
        status.latest = None;
        return Ok(());
    }
    let wanted = asset_name();
    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.clone())
    };
    status.latest = Some(Release {
        asset: wanted
            .as_deref()
            .and_then(|name| find(name).map(|url| (name.to_owned(), url))),
        sums: find("SHA256SUMS"),
        page: release.html_url,
        version,
    });
    Ok(())
}

/// Looks for updates now and every few hours.
pub async fn check_loop(db_path: PathBuf, changed: impl Fn() + Send + 'static) {
    if !enabled() {
        return;
    }
    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        let before = sidebar();
        if let Err(error) = check(&db_path).await {
            crate::serverlog::warning!("Update check failed: {error:#}");
        }
        let after = sidebar();
        if before.update != after.update || before.stars != after.stars {
            changed();
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// Runs a check now unless one ran in the last minute.
pub async fn check_now(db_path: &Path) -> Result<()> {
    let recent = status()
        .checked
        .is_some_and(|at| at.elapsed() < Duration::from_secs(60));
    if recent {
        return Ok(());
    }
    check(db_path).await
}

// ------------------------------------------------------------ installing

/// Whether the app is about to restart into an update.
pub fn restarting() -> bool {
    matches!(status().phase, Phase::Restarting)
}

/// Clears a failed update's note.
pub fn dismiss_failure() {
    let mut status = status();
    if matches!(status.phase, Phase::Failed(_)) {
        status.phase = Phase::Idle;
    }
}

pub fn set_phase(phase: Phase) {
    status().phase = phase;
}

/// Takes the update on offer for this run, or says why it can't start.
pub fn begin() -> Result<Release> {
    let mut status = status();
    if matches!(status.phase, Phase::Busy(_) | Phase::Restarting) {
        bail!("An update is already running");
    }
    let release = status
        .latest
        .clone()
        .ok_or_else(|| anyhow!("LocalLeads is up to date"))?;
    if release.asset.is_none() {
        bail!("There's no download of {} for this system", release.version);
    }
    if release.sums.is_none() {
        bail!(
            "{} has no checksums to check the download against, so it can't be installed from here",
            release.version
        );
    }
    status.phase = Phase::Busy(format!("Downloading v{}…", release.version));
    Ok(release)
}

/// The newest details of `release` (its downloads and checksums), so a
/// retry never reuses ones from an earlier look. Falls back to `release`
/// if GitHub can't be reached or now offers another version.
pub async fn refresh(db_path: &Path, release: Release) -> Release {
    if check(db_path).await.is_err() {
        return release;
    }
    let latest = status().latest.clone();
    match latest {
        Some(latest) if latest.version == release.version => latest,
        _ => release,
    }
}

/// Downloads, checks and test-runs `release`, and records it for trial.
/// Returns what to start. Nothing about the running install changes until
/// this has fully succeeded.
pub async fn prepare(db_path: &Path, release: &Release) -> Result<Staged> {
    let (asset_name, asset_url) = release.asset.clone().context("no download for this system")?;
    let sums_url = release.sums.clone().context("no checksums")?;
    let dir = updates_dir(db_path);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("create {}", dir.display()))?;
    let client = http()?;

    let sums = client
        .get(&sums_url)
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()
        .context("download SHA256SUMS")?
        .text()
        .await?;
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let sum = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            (name == asset_name).then(|| sum.to_ascii_lowercase())
        })
        .with_context(|| format!("SHA256SUMS doesn't list {asset_name}"))?;

    let archive = dir.join(format!("{asset_name}.part"));
    let actual = match download(&client, &asset_url, &archive).await {
        Ok(sum) => sum,
        Err(error) => {
            let _ = tokio::fs::remove_file(&archive).await;
            return Err(error);
        }
    };
    if actual != expected {
        let _ = tokio::fs::remove_file(&archive).await;
        bail!("the download doesn't match its checksum; nothing was changed");
    }

    set_phase(Phase::Busy(format!("Checking v{}…", release.version)));
    let file = if cfg!(windows) {
        format!("localleads-{}.exe", release.version)
    } else {
        format!("localleads-{}", release.version)
    };
    let program = dir.join(&file);
    {
        let archive = archive.clone();
        let program = program.clone();
        let unpacked = tokio::task::spawn_blocking(move || unpack(&archive, &program))
            .await
            .context("unpack the download");
        if !matches!(unpacked, Ok(Ok(()))) {
            let _ = tokio::fs::remove_file(&dir.join(format!("{asset_name}.part"))).await;
        }
        unpacked??;
    }
    let _ = tokio::fs::remove_file(&archive).await;
    let sha256 = {
        let program = program.clone();
        tokio::task::spawn_blocking(move || file_sha256(&program)).await??
    };
    smoke_test(&program, &release.version).await?;
    Ok(Staged {
        version: release.version.clone(),
        file,
        sha256,
        tries: 0,
    })
}

async fn download(client: &reqwest::Client, url: &str, path: &Path) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(15 * 60))
        .send()
        .await?
        .error_for_status()
        .context("download the new version")?;
    if response.content_length().is_some_and(|length| length > MAX_DOWNLOAD) {
        bail!("the download is unexpectedly large");
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .with_context(|| format!("create {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    while let Some(chunk) = response.chunk().await.context("download the new version")? {
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD {
            bail!("the download is unexpectedly large");
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok(hex(&hasher.finalize()))
}

/// Takes the program out of the release archive.
fn unpack(archive: &Path, program: &Path) -> Result<()> {
    let temporary = program.with_file_name(format!(
        "{}.part",
        program.file_name().unwrap_or_default().to_string_lossy()
    ));
    let file = std::fs::File::open(archive)?;
    #[cfg(not(windows))]
    {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let mut found = false;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if entry.header().entry_type().is_file()
                && path.file_name().is_some_and(|name| name == "localleads")
            {
                let mut out = std::fs::File::create(&temporary)?;
                std::io::copy(&mut entry, &mut out)?;
                out.sync_all()?;
                found = true;
                break;
            }
        }
        if !found {
            bail!("the download doesn't contain the program");
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        let mut zip = zip::ZipArchive::new(file)?;
        let index = (0..zip.len())
            .find(|index| {
                zip.by_index(*index).is_ok_and(|entry| {
                    entry.is_file() && entry.name().rsplit('/').next() == Some("localleads.exe")
                })
            })
            .context("the download doesn't contain the program")?;
        let mut entry = zip.by_index(index)?;
        let mut out = std::fs::File::create(&temporary)?;
        std::io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
    }
    std::fs::rename(&temporary, program)?;
    Ok(())
}

/// Runs `program --version` and checks it answers with `version`.
async fn smoke_test(program: &Path, version: &str) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(program)
            .arg("--version")
            .env(LAUNCHED_ENV, "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("the new version didn't answer in time")?
    .context("the new version doesn't run on this machine")?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !text.contains(version) {
        bail!(
            "the new version doesn't run on this machine ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Checks the downloaded program is still there and intact.
pub fn verify_staged(db_path: &Path, staged: &Staged) -> Result<()> {
    let path = updates_dir(db_path).join(&staged.file);
    let sum = file_sha256(&path).with_context(|| format!("read {}", path.display()))?;
    if sum != staged.sha256 {
        bail!("the downloaded program changed on disk; nothing was changed");
    }
    Ok(())
}

/// Records `staged` for trial and restarts into it. Returns only if the
/// restart couldn't happen; the trial is withdrawn then.
pub fn restart_into(db_path: &Path, staged: &Staged) -> Result<()> {
    let dir = updates_dir(db_path);
    let mut ledger = Ledger::load(&dir);
    // This start is the first try.
    let trial = Staged {
        tries: 1,
        ..staged.clone()
    };
    ledger.trial = Some(trial.clone());
    ledger.set_aside.retain(|version| *version != trial.version);
    ledger.save(&dir).context("record the new version")?;
    start(&dir, &trial, Some(VERSION), None);
    // Still here: the restart failed.
    let mut ledger = Ledger::load(&dir);
    if ledger.trial.as_ref() == Some(&trial) {
        ledger.trial = None;
        let _ = ledger.save(&dir);
    }
    bail!("couldn't start the new version")
}

/// Backs the database up beside the downloads before the first start of a
/// new version, when there's room. Earlier backups are removed first.
///
/// `VACUUM INTO` on a connection of its own takes a consistent copy while
/// searches keep writing.
pub fn backup_database(db_path: &Path, version: &str) -> Result<Option<PathBuf>> {
    let dir = updates_dir(db_path);
    std::fs::create_dir_all(&dir)?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("before-") && name.ends_with(".sqlite") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let size = std::fs::metadata(db_path).map(|meta| meta.len()).unwrap_or(0);
    // Room for the copy and as much again for the app to keep working.
    if free_space(&dir).is_some_and(|free| free < size.saturating_mul(2) + 256 * 1024 * 1024) {
        return Ok(None);
    }
    let path = dir.join(format!("before-{version}.sqlite"));
    let partial = dir.join(format!("before-{version}.sqlite.part"));
    let _ = std::fs::remove_file(&partial);
    let connection = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(30))?;
    connection
        .execute("VACUUM INTO ?1", [partial.to_string_lossy().as_ref()])
        .context("copy the database")?;
    drop(connection);
    std::fs::rename(&partial, &path)?;
    Ok(Some(path))
}

#[cfg(unix)]
fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs only writes the struct we pass.
    unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stats) != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
    }
}

#[cfg(not(unix))]
fn free_space(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(version: &str, dir: &Path) -> Staged {
        let file = format!("localleads-{version}");
        std::fs::write(dir.join(&file), version.as_bytes()).unwrap();
        Staged {
            version: version.to_owned(),
            sha256: file_sha256(&dir.join(&file)).unwrap(),
            file,
            tries: 0,
        }
    }

    #[test]
    fn only_intact_newer_versions_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let newer = staged("999.0.0", dir.path());
        assert!(usable(dir.path(), &newer));
        let older = staged("0.0.1", dir.path());
        assert!(!usable(dir.path(), &older));
        let mut damaged = newer.clone();
        damaged.sha256 = "0".repeat(64);
        assert!(!usable(dir.path(), &damaged));
        std::fs::remove_file(dir.path().join(&newer.file)).unwrap();
        assert!(!usable(dir.path(), &newer));
    }

    #[test]
    fn ledger_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::default();
        ledger.trial = Some(staged("999.0.0", dir.path()));
        ledger.set_aside.push("998.0.0".to_owned());
        ledger.save(dir.path()).unwrap();
        let loaded = Ledger::load(dir.path());
        assert_eq!(loaded.trial, ledger.trial);
        assert_eq!(loaded.set_aside, ledger.set_aside);
        // A damaged ledger reads as empty rather than failing a start.
        std::fs::write(Ledger::path(dir.path()), b"{not json").unwrap();
        assert!(Ledger::load(dir.path()).trial.is_none());
    }

    #[test]
    fn cleaning_keeps_named_programs() {
        let dir = tempfile::tempdir().unwrap();
        let working = staged("999.0.0", dir.path());
        let old = staged("997.0.0", dir.path());
        std::fs::write(dir.path().join("x.tar.gz.part"), b"").unwrap();
        let ledger = Ledger {
            working: Some(working.clone()),
            ..Ledger::default()
        };
        clean(dir.path(), &ledger);
        assert!(dir.path().join(&working.file).exists());
        assert!(!dir.path().join(&old.file).exists());
        assert!(!dir.path().join("x.tar.gz.part").exists());
    }

    #[test]
    fn star_counts_read_short() {
        assert_eq!(fmt_stars(987), "987");
        assert_eq!(fmt_stars(1000), "1k");
        assert_eq!(fmt_stars(1234), "1.2k");
        assert_eq!(fmt_stars(15_400), "15k");
    }

    #[test]
    fn restarts_skip_restore_and_open() {
        let args = |list: &[&str]| list.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(
            restart_args(args(&["--db", "x.sqlite", "--restore", "b.sqlite", "--open", "--bind", "0.0.0.0:1"])),
            args(&["--db", "x.sqlite", "--bind", "0.0.0.0:1"])
        );
        assert_eq!(restart_args(args(&["--restore=b.sqlite", "--behind-proxy"])), args(&["--behind-proxy"]));
    }

    #[test]
    fn backups_are_complete_copies() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("leads.sqlite");
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES (1),(2),(3);")
            .unwrap();
        let backup = backup_database(&db_path, "9.9.9").unwrap().unwrap();
        let copy = rusqlite::Connection::open(&backup).unwrap();
        let count: i64 = copy.query_row("SELECT count(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 3);
        drop(connection);
    }

    #[test]
    fn this_machine_has_a_release_build() {
        assert!(asset_name().is_some());
    }
}
