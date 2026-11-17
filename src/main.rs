mod app;
mod auth;
mod db;
mod enrich;
mod error;
mod events;
mod geo;
mod export;
mod locality;
mod mcp;
mod models;
mod proxy;
mod scraper;
mod web;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "localleads",
    version,
    about = "SQLite-backed Google Maps lead discovery web app"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, global = true, value_name = "PATH", env = "LOCALLEADS_DB", help = "SQLite database path")]
    db: Option<PathBuf>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Restore a SQLite backup before launch"
    )]
    restore: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ADDR",
        default_value = "127.0.0.1:7878",
        env = "LOCALLEADS_BIND",
        help = "Address the web server listens on"
    )]
    bind: SocketAddr,
    #[arg(
        long,
        value_name = "URL",
        env = "LOCALLEADS_PUBLIC_URL",
        help = "Address people open, e.g. https://leads.example.com (only that host is answered)"
    )]
    public_url: Option<String>,
    #[arg(
        long,
        env = "LOCALLEADS_BEHIND_PROXY",
        help = "Trust X-Forwarded-For/-Proto from a reverse proxy in front (Caddy, nginx, ...)"
    )]
    behind_proxy: bool,
    #[arg(long, help = "Open the app in the default browser after starting")]
    open: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// MCP server for coding agents (stdio); `mcp install` sets up Claude Code, Codex and Cursor.
    Mcp(mcp::install::McpArgs),
    /// Exit 0 if the server on --bind answers /healthz (for container health checks).
    #[command(hide = true)]
    Healthcheck,
}

#[tokio::main]
async fn main() -> Result<()> {
    // One rustls crypto provider for the whole process. The email checker
    // installs `ring` and panics if another was installed first.
    #[cfg(feature = "verify")]
    check_if_email_exists::initialize_crypto_provider();
    #[cfg(not(feature = "verify"))]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let args = Args::parse();
    raise_open_file_limit();
    let use_default_database = args.db.is_none();
    let database_path = args.db.unwrap_or(default_database_path()?);
    match args.command {
        Some(Command::Mcp(mcp_args)) => {
            return mcp::install::run(mcp_args, database_path, !use_default_database).await;
        }
        Some(Command::Healthcheck) => return healthcheck(args.bind).await,
        None => {}
    }
    let public_url = args.public_url.as_deref().map(parse_public_url).transpose()?;
    if let Some(backup) = args.restore {
        db::Database::restore_to(backup, &database_path).context("restore database backup")?;
    }
    let database = db::Database::open(&database_path)?;
    // Anyone who can reach a fresh install could create its account, so
    // when other machines can reach it, setup needs a code from the log.
    let exposed = !args.bind.ip().is_loopback();
    let setup_code = if database.has_account()? || !exposed {
        None
    } else {
        Some(
            std::env::var("LOCALLEADS_SETUP_CODE")
                .ok()
                .filter(|code| !code.trim().is_empty())
                .unwrap_or_else(auth::new_setup_code),
        )
    };
    web::site::configure(web::site::Site {
        public_host: public_url.as_ref().and_then(|url| url.host_str()).map(str::to_ascii_lowercase),
        public_url: public_url
            .as_ref()
            .map(|url| url.as_str().trim_end_matches('/').to_owned()),
        behind_proxy: args.behind_proxy,
        setup_code: setup_code.clone(),
    });
    let state = app::AppState::new(database, database_path.clone());

    let url = if args.bind.ip().is_unspecified() {
        format!("http://127.0.0.1:{}", args.bind.port())
    } else {
        format!("http://{}", args.bind)
    };
    println!(
        "LocalLeads {} — database {}",
        env!("CARGO_PKG_VERSION"),
        database_path.display()
    );
    println!("Listening on {url}  (Ctrl+C to stop)");
    if let Some(public) = &public_url {
        println!("Public address: {}", public.as_str().trim_end_matches('/'));
    }
    if exposed && public_url.as_ref().is_none_or(|url| url.scheme() != "https") {
        eprintln!(
            "warning: listening beyond localhost without an https:// --public-url; sign-in cookies and API keys travel in plain text unless an HTTPS proxy sits in front"
        );
    }
    if let Some(code) = &setup_code {
        println!();
        println!("  No account yet. Open the app and create one with this setup code:");
        println!();
        println!("      {code}");
        println!();
    }
    if args.open {
        open_browser(&url);
    }
    web::serve(
        state,
        web::ServerOptions {
            addr: args.bind,
        },
    )
    .await
}

fn parse_public_url(text: &str) -> Result<url::Url> {
    let url = url::Url::parse(text.trim()).with_context(|| format!("--public-url {text:?} is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        anyhow::bail!("--public-url must look like https://leads.example.com");
    }
    if url.path() != "/" {
        anyhow::bail!("--public-url must be the site's root (serving under a path isn't supported)");
    }
    Ok(url)
}

async fn healthcheck(bind: SocketAddr) -> Result<()> {
    let host = if bind.ip().is_unspecified() {
        std::net::IpAddr::from([127, 0, 0, 1])
    } else {
        bind.ip()
    };
    let url = format!("http://{}/healthz", SocketAddr::new(host, bind.port()));
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(3))
        .build()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("reach {url}"))?;
    anyhow::ensure!(response.status().is_success(), "{url} answered {}", response.status());
    Ok(())
}

/// Every in-flight proxy check holds a socket, and macOS starts processes
/// with a soft limit of 256 open files. Raise the soft limit toward the hard
/// limit so a high-concurrency check never fails with "too many open files".
#[cfg(unix)]
fn raise_open_file_limit() {
    // SAFETY: getrlimit/setrlimit only read and write the struct we pass.
    unsafe {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        // 10,240 is macOS's per-process OPEN_MAX; try higher first elsewhere.
        for target in [65_536 as libc::rlim_t, 10_240] {
            let wanted = target.min(limit.rlim_max);
            if limit.rlim_cur >= wanted {
                return;
            }
            let next = libc::rlimit {
                rlim_cur: wanted,
                rlim_max: limit.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &next) == 0 {
                return;
            }
        }
    }
}

#[cfg(not(unix))]
fn raise_open_file_limit() {}

pub(crate) fn default_database_path() -> Result<PathBuf> {
    let project_dirs = ProjectDirs::from("com", "LocalLeads", "LocalLeads")
        .context("could not determine a platform data directory; pass --db explicitly")?;
    Ok(project_dirs.data_local_dir().join("localleads.sqlite"))
}

fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(error) = result {
        eprintln!("could not open a browser: {error}");
    }
}
