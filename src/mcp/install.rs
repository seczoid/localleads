//! `localleads mcp` command line: run the stdio server, or print/write the
//! config snippets that register LocalLeads with coding agents.

use super::stdio::{DEFAULT_SERVER, StdioOptions, run_stdio};
use crate::auth::{self, API_KEY_ENV};
use crate::db::add_api_key_to_file;
use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Args)]
#[command(
    about = "Run the MCP server over stdio for coding agents (Claude Code, Codex, Cursor, ...)",
    long_about = "Run the MCP server over stdio for coding agents.\n\n\
If a LocalLeads dashboard is serving the same database, stdio is bridged to its /mcp endpoint so \
searches keep running in the dashboard. Otherwise the engine runs inside this process.\n\n\
Every connection needs an API key: create one on the dashboard's MCP page and pass it in \
LOCALLEADS_API_KEY (or --api-key).\n\n\
Run `localleads mcp install --write` to register LocalLeads with Claude Code, Codex or Cursor \
with a new key."
)]
pub struct McpArgs {
    #[command(subcommand)]
    pub action: Option<McpAction>,
    /// Bridge to the dashboard at this URL instead of looking for one.
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,
    /// API key from the dashboard's MCP page (or set LOCALLEADS_API_KEY).
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,
    /// Always run the engine in this process, even if a dashboard is running.
    #[arg(long)]
    pub embedded: bool,
}

#[derive(Debug, Subcommand)]
pub enum McpAction {
    /// Print (or with --write, apply) the setup for Claude Code, Codex and Cursor.
    Install(InstallArgs),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Client {
    Claude,
    Codex,
    Cursor,
    /// Any other MCP client (Claude Desktop, VS Code, Windsurf, ...): a JSON snippet.
    Json,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    /// The agent launches `localleads mcp`; it uses a running dashboard when there is one.
    Stdio,
    /// The agent connects to a running dashboard's /mcp endpoint.
    Http,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Which client to set up.
    #[arg(long, value_enum, default_value = "all")]
    pub client: Client,
    /// stdio (recommended: nothing else to run) or http (needs the dashboard running).
    #[arg(long, value_enum, default_value = "stdio")]
    pub transport: Transport,
    /// Dashboard URL for --transport http.
    #[arg(long, value_name = "URL", default_value = DEFAULT_SERVER)]
    pub url: String,
    /// Register it: runs `claude mcp add` / `codex mcp add`, or edits ~/.codex/config.toml and ~/.cursor/mcp.json.
    #[arg(long)]
    pub write: bool,
    /// Server name in the client's config.
    #[arg(long, default_value = "localleads")]
    pub name: String,
    /// API key to put in the config. With --write and no key, a new one is created.
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,
}

/// Shown in printed setups until a real key is filled in.
const KEY_PLACEHOLDER: &str = "YOUR_API_KEY";

pub async fn run(args: McpArgs, db_path: PathBuf, explicit_db: bool) -> Result<()> {
    match args.action {
        Some(McpAction::Install(install)) => run_install(install, &db_path, explicit_db),
        None => {
            run_stdio(StdioOptions {
                db_path,
                server: args.server,
                api_key: args
                    .api_key
                    .or_else(|| std::env::var(API_KEY_ENV).ok())
                    .map(|key| key.trim().to_owned())
                    .filter(|key| !key.is_empty()),
                embedded: args.embedded,
            })
            .await
        }
    }
}

struct Launch {
    command: String,
    args: Vec<String>,
    url: String,
    transport: Transport,
    key: String,
}

impl Launch {
    fn shell(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn claude_command(&self, name: &str) -> Vec<String> {
        let mut parts = vec!["claude".to_owned(), "mcp".to_owned(), "add".to_owned()];
        parts.extend(["--scope".to_owned(), "user".to_owned()]);
        match self.transport {
            Transport::Http => {
                parts.extend(["--transport".to_owned(), "http".to_owned(), name.to_owned(), self.url.clone()]);
                parts.extend(["--header".to_owned(), format!("Authorization: Bearer {}", self.key)]);
            }
            Transport::Stdio => {
                parts.extend([name.to_owned(), "--env".to_owned(), format!("{API_KEY_ENV}={}", self.key)]);
                parts.extend(["--".to_owned(), self.command.clone()]);
                parts.extend(self.args.iter().cloned());
            }
        }
        parts
    }

    fn codex_command(&self, name: &str) -> Vec<String> {
        let mut parts = vec!["codex".to_owned(), "mcp".to_owned(), "add".to_owned(), name.to_owned()];
        match self.transport {
            Transport::Http => parts.extend(["--url".to_owned(), self.url.clone()]),
            Transport::Stdio => {
                parts.extend(["--env".to_owned(), format!("{API_KEY_ENV}={}", self.key)]);
                parts.push("--".to_owned());
                parts.push(self.command.clone());
                parts.extend(self.args.iter().cloned());
            }
        }
        parts
    }

    fn codex_toml(&self, name: &str) -> String {
        match self.transport {
            Transport::Http => format!(
                "[mcp_servers.{name}]\nurl = {}\nhttp_headers = {{ Authorization = {} }}\n",
                toml_string(&self.url),
                toml_string(&format!("Bearer {}", self.key))
            ),
            Transport::Stdio => format!(
                "[mcp_servers.{name}]\ncommand = {}\nargs = [{}]\nenv = {{ {API_KEY_ENV} = {} }}\nstartup_timeout_sec = 30\ntool_timeout_sec = 300\n",
                toml_string(&self.command),
                self.args.iter().map(|arg| toml_string(arg)).collect::<Vec<_>>().join(", "),
                toml_string(&self.key)
            ),
        }
    }

    fn json_entry(&self) -> Value {
        match self.transport {
            Transport::Http => json!({
                "url": self.url,
                "headers": { "Authorization": format!("Bearer {}", self.key) }
            }),
            Transport::Stdio => json!({
                "command": self.command,
                "args": self.args,
                "env": { API_KEY_ENV: self.key }
            }),
        }
    }
}

pub(super) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@%+,".contains(ch))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub(super) fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run_install(args: InstallArgs, db_path: &Path, explicit_db: bool) -> Result<()> {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("find the localleads executable")?;
    let mut launch_args = vec!["mcp".to_owned()];
    if explicit_db {
        let db = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
        launch_args.insert(0, db.display().to_string());
        launch_args.insert(0, "--db".to_owned());
    }
    let given = args.api_key.as_deref().map(str::trim).filter(|key| !key.is_empty());
    let key = match given {
        Some(key) => key.to_owned(),
        None if args.write => {
            let (key, shown) = auth::new_api_key();
            add_api_key_to_file(db_path, "mcp install", &shown, &auth::digest(&key))
                .with_context(|| format!("create an API key in {}", db_path.display()))?;
            key
        }
        None => KEY_PLACEHOLDER.to_owned(),
    };
    let launch = Launch {
        command: exe.display().to_string(),
        args: launch_args,
        url: format!("{}/mcp", args.url.trim_end_matches('/')),
        transport: args.transport,
        key,
    };
    let name = args.name.as_str();
    let wants = |client: Client| args.client == Client::All || args.client == client;

    println!("LocalLeads MCP server setup ({} transport)", format!("{:?}", args.transport).to_lowercase());
    println!("Executable: {}", exe.display());
    println!("Database:   {}", db_path.display());
    if exe.components().any(|part| part.as_os_str() == "target") {
        println!(
            "note: this binary is inside a cargo target folder. For a stable path run `cargo install --path .` and rerun `localleads mcp install`."
        );
    }
    match args.transport {
        Transport::Stdio => println!(
            "The agent launches `{}`. It uses the dashboard when one is running for this database, and runs the engine itself otherwise.",
            launch.shell()
        ),
        Transport::Http => println!(
            "The agent connects to {}; start the dashboard first with `localleads`.",
            launch.url
        ),
    }
    if given.is_none() && args.write {
        println!("Created API key \"mcp install\" (revoke it on the dashboard's MCP page).");
    } else if given.is_none() {
        println!(
            "Replace {KEY_PLACEHOLDER} with a key from the dashboard's MCP page, or run with --write to create one."
        );
    }
    println!();

    if wants(Client::Claude) {
        let command = launch.claude_command(name);
        println!("## Claude Code\n");
        println!("{}\n", command.iter().map(|part| shell_quote(part)).collect::<Vec<_>>().join(" "));
        if args.write {
            write_with_cli(&command, &["claude", "mcp", "remove", "--scope", "user", name], "Claude Code")?;
        }
    }
    if wants(Client::Codex) {
        println!("## Codex\n");
        // `codex mcp add --url` can't set a header, so HTTP goes in the file.
        let http = args.transport == Transport::Http;
        if http {
            println!("add to ~/.codex/config.toml:\n\n{}", launch.codex_toml(name));
        } else {
            println!("{}\n", launch.codex_command(name).iter().map(|part| shell_quote(part)).collect::<Vec<_>>().join(" "));
            println!("or add to ~/.codex/config.toml:\n\n{}", launch.codex_toml(name));
        }
        if args.write {
            if on_path("codex") && !http {
                write_with_cli(&launch.codex_command(name), &["codex", "mcp", "remove", name], "Codex")?;
            } else {
                write_codex_toml(&launch, name)?;
            }
        }
    }
    if wants(Client::Cursor) {
        println!("## Cursor (~/.cursor/mcp.json, or .cursor/mcp.json in a project)\n");
        let snippet = json!({ "mcpServers": { name: launch.json_entry() } });
        println!("{}\n", serde_json::to_string_pretty(&snippet)?);
        if args.write {
            let path = home().context("no home directory")?.join(".cursor/mcp.json");
            merge_json_config(&path, "mcpServers", name, launch.json_entry())?;
        }
    }
    if wants(Client::Json) {
        println!("## Other MCP clients (Claude Desktop, Windsurf, ...: \"mcpServers\"; VS Code .vscode/mcp.json: \"servers\")\n");
        let mut entry = launch.json_entry();
        if args.transport == Transport::Http {
            entry["type"] = json!("http");
        }
        println!("{}\n", serde_json::to_string_pretty(&json!({ "mcpServers": { name: entry } }))?);
    }
    if !args.write {
        println!("Run `localleads mcp install --write` to apply this automatically (add --client claude|codex|cursor to pick one).");
    }
    Ok(())
}

/// Runs `command`; if the server is already registered, removes it with
/// `remove` and adds it again, so the new key takes effect.
fn write_with_cli(command: &[String], remove: &[&str], label: &str) -> Result<()> {
    let Some((program, rest)) = command.split_first() else {
        return Ok(());
    };
    if !on_path(program) {
        println!("skipped {label}: `{program}` is not on PATH; run the command above once it is installed.\n");
        return Ok(());
    }
    let run = || -> Result<(bool, String)> {
        let output = Command::new(program).args(rest).output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok((output.status.success(), text))
    };
    let (mut ok, mut text) = run()?;
    if !ok && text.contains("already exists") && !remove.is_empty() {
        let removed = Command::new(remove[0]).args(&remove[1..]).output()?;
        if removed.status.success() {
            (ok, text) = run()?;
        }
    }
    if ok {
        println!("registered with {label}: {}\n", text.trim());
    } else {
        println!("{label} registration failed: {}\n", text.trim());
    }
    Ok(())
}

fn write_codex_toml(launch: &Launch, name: &str) -> Result<()> {
    let path = home().context("no home directory")?.join(".codex/config.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(&format!("[mcp_servers.{name}]")) {
        println!("{} already has [mcp_servers.{name}]; left unchanged.\n", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&launch.codex_toml(name));
    std::fs::write(&path, text)?;
    println!("added [mcp_servers.{name}] to {}\n", path.display());
    Ok(())
}

/// Adds `section.name = entry` to a JSON config file, keeping everything else.
fn merge_json_config(path: &Path, section: &str, name: &str, entry: Value) -> Result<()> {
    let mut config = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.is_object() => value,
            _ => {
                println!("{} is not plain JSON (comments?); add the snippet above by hand.\n", path.display());
                return Ok(());
            }
        },
        _ => json!({}),
    };
    let servers = config
        .as_object_mut()
        .context("config is not an object")?
        .entry(section)
        .or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        println!("{}: \"{section}\" is not an object; add the snippet above by hand.\n", path.display());
        return Ok(());
    };
    if servers.get(name) == Some(&entry) {
        println!("{} already has this server.\n", path.display());
        return Ok(());
    }
    let replaced = servers.insert(name.to_owned(), entry).is_some();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::copy(path, path.with_extension("json.bak"))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&config)? + "\n")?;
    println!(
        "{} \"{name}\" in {}\n",
        if replaced { "updated" } else { "added" },
        path.display()
    );
    Ok(())
}

/// Quotes one shell word, for commands shown on the dashboard.
pub fn quote(value: &str) -> String {
    shell_quote(value)
}
