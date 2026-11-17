//! Setup snippets for each MCP client, shown on the dashboard's MCP page.
//! Built from the running binary's path so they can be pasted as they are.

use super::install::{shell_quote, toml_string};
use crate::auth::API_KEY_ENV;
use serde_json::{Value, json};

pub struct Snippet {
    /// What to do with it: "Run in a terminal", "Add to ~/.cursor/mcp.json", …
    pub label: String,
    pub code: String,
}

pub struct ClientSetup {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: &'static str,
    /// How to tell it worked.
    pub check: &'static str,
    pub stdio: Vec<Snippet>,
    /// Empty when the client can't connect over HTTP.
    pub http: Vec<Snippet>,
    pub http_note: &'static str,
}

/// How an agent reaches LocalLeads: it launches `command args…` (stdio),
/// or connects to `url` (the dashboard's `/mcp`). Either way it sends `key`:
/// as `LOCALLEADS_API_KEY` to the command, or as a Bearer header over HTTP.
pub struct Launch<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub url: &'a str,
    pub name: &'a str,
    pub key: &'a str,
}

impl Launch<'_> {
    fn shell(&self) -> String {
        std::iter::once(self.command)
            .chain(self.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn env(&self) -> Value {
        json!({ API_KEY_ENV: self.key })
    }

    fn headers(&self) -> Value {
        json!({ "Authorization": self.bearer() })
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.key)
    }

    fn stdio_entry(&self) -> Value {
        json!({ "command": self.command, "args": self.args, "env": self.env() })
    }

    fn http_entry(&self, url_field: &str) -> Value {
        let mut entry = serde_json::Map::new();
        entry.insert(url_field.to_owned(), json!(self.url));
        entry.insert("headers".to_owned(), self.headers());
        Value::Object(entry)
    }

    fn command_array(&self) -> Value {
        let mut parts = vec![self.command.to_owned()];
        parts.extend(self.args.iter().cloned());
        json!(parts)
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn snippet(label: impl Into<String>, code: impl Into<String>) -> Snippet {
    Snippet {
        label: label.into(),
        code: code.into(),
    }
}

pub fn setups(launch: &Launch<'_>) -> Vec<ClientSetup> {
    let name = launch.name;
    let shell = launch.shell();
    let url = launch.url;
    let servers = |entry: Value| pretty(&json!({ "mcpServers": { name: entry } }));
    let key = launch.key;
    let env_flag = shell_quote(&format!("{API_KEY_ENV}={key}"));
    let header_flag = shell_quote(&format!("Authorization: {}", launch.bearer()));
    let codex_stdio = format!(
        "[mcp_servers.{name}]\ncommand = {}\nargs = [{}]\nenv = {{ {API_KEY_ENV} = {} }}\nstartup_timeout_sec = 30\ntool_timeout_sec = 300",
        toml_string(launch.command),
        launch.args.iter().map(|arg| toml_string(arg)).collect::<Vec<_>>().join(", "),
        toml_string(key)
    );
    let vscode_add = serde_json::to_string(
        &json!({ "name": name, "command": launch.command, "args": launch.args, "env": launch.env() }),
    )
    .unwrap_or_default();
    vec![
        ClientSetup {
            id: "claude-code",
            name: "Claude Code",
            kind: "CLI",
            check: "Run `claude mcp list`, or type /mcp inside Claude Code; localleads should show as connected.",
            stdio: vec![snippet(
                "Run in a terminal",
                format!("claude mcp add --scope user {name} --env {env_flag} -- {shell}"),
            )],
            http: vec![snippet(
                "Run in a terminal",
                format!("claude mcp add --scope user --transport http {name} {url} --header {header_flag}"),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "codex",
            name: "Codex",
            kind: "CLI and IDE",
            check: "Run `codex mcp list`, or /mcp inside Codex.",
            stdio: vec![
                snippet("Run in a terminal", format!("codex mcp add {name} --env {env_flag} -- {shell}")),
                snippet("Or add to ~/.codex/config.toml", codex_stdio),
            ],
            http: vec![snippet(
                "Add to ~/.codex/config.toml",
                format!(
                    "[mcp_servers.{name}]\nurl = {}\nhttp_headers = {{ Authorization = {} }}",
                    toml_string(url),
                    toml_string(&launch.bearer())
                ),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "cursor",
            name: "Cursor",
            kind: "Editor",
            check: "Open Cursor Settings → MCP; localleads should show a green dot and its tools.",
            stdio: vec![snippet(
                "Add to ~/.cursor/mcp.json (or .cursor/mcp.json in one project)",
                servers(launch.stdio_entry()),
            )],
            http: vec![snippet(
                "Add to ~/.cursor/mcp.json (or .cursor/mcp.json in one project)",
                servers(launch.http_entry("url")),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "claude-desktop",
            name: "Claude Desktop",
            kind: "Desktop app",
            check: "Quit and reopen Claude Desktop; localleads appears under the tools (sliders) button in a chat.",
            stdio: vec![snippet(
                "Settings → Developer → Edit Config, then add to claude_desktop_config.json",
                servers(launch.stdio_entry()),
            )],
            http: Vec::new(),
            http_note: "Claude Desktop starts local servers itself, so use the stdio setup. It still shares the dashboard's live state while the dashboard runs.",
        },
        ClientSetup {
            id: "vscode",
            name: "VS Code",
            kind: "GitHub Copilot agent mode",
            check: "Open the Command Palette → MCP: List Servers, start localleads, then pick its tools in Copilot Chat's agent mode.",
            stdio: vec![
                snippet("Run in a terminal", format!("code --add-mcp {}", shell_quote(&vscode_add))),
                snippet(
                    "Or add to .vscode/mcp.json (or MCP: Open User Configuration)",
                    pretty(&json!({ "servers": { name: { "type": "stdio", "command": launch.command, "args": launch.args, "env": launch.env() } } })),
                ),
            ],
            http: vec![snippet(
                "Add to .vscode/mcp.json (or MCP: Open User Configuration)",
                pretty(&json!({ "servers": { name: { "type": "http", "url": url, "headers": launch.headers() } } })),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "windsurf",
            name: "Windsurf",
            kind: "Editor",
            check: "Open Cascade → MCP servers (hammer icon) and refresh; localleads lists its tools.",
            stdio: vec![snippet("Add to ~/.codeium/windsurf/mcp_config.json", servers(launch.stdio_entry()))],
            http: vec![snippet(
                "Add to ~/.codeium/windsurf/mcp_config.json",
                servers(launch.http_entry("serverUrl")),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "gemini",
            name: "Gemini CLI",
            kind: "CLI",
            check: "Run /mcp inside Gemini CLI.",
            stdio: vec![
                snippet("Run in a terminal", format!("gemini mcp add --scope user -e {env_flag} {name} {shell}")),
                snippet("Or add to ~/.gemini/settings.json", servers(launch.stdio_entry())),
            ],
            http: vec![
                snippet(
                    "Run in a terminal",
                    format!("gemini mcp add --scope user --transport http -H {header_flag} {name} {url}"),
                ),
                snippet("Or add to ~/.gemini/settings.json", servers(launch.http_entry("httpUrl"))),
            ],
            http_note: "",
        },
        ClientSetup {
            id: "zed",
            name: "Zed",
            kind: "Editor",
            check: "Open the Agent panel → settings; localleads shows a green dot under Model Context Protocol servers.",
            stdio: vec![snippet(
                "Add to Zed's settings.json (zed: open settings)",
                pretty(&json!({ "context_servers": { name: launch.stdio_entry() } })),
            )],
            http: vec![snippet(
                "Add to Zed's settings.json (zed: open settings)",
                pretty(&json!({ "context_servers": { name: launch.http_entry("url") } })),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "cline",
            name: "Cline",
            kind: "VS Code extension",
            check: "Open Cline → MCP Servers → Installed; localleads should be green.",
            stdio: vec![snippet(
                "MCP Servers → Configure → cline_mcp_settings.json",
                servers(json!({ "command": launch.command, "args": launch.args, "env": launch.env(), "disabled": false })),
            )],
            http: vec![snippet(
                "MCP Servers → Configure → cline_mcp_settings.json",
                servers(json!({ "type": "streamableHttp", "url": url, "headers": launch.headers(), "disabled": false })),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "opencode",
            name: "opencode",
            kind: "CLI",
            check: "Run `opencode mcp list`.",
            stdio: vec![snippet(
                "Add to ~/.config/opencode/opencode.json (or opencode.json in a project)",
                pretty(&json!({
                    "$schema": "https://opencode.ai/config.json",
                    "mcp": { name: { "type": "local", "command": launch.command_array(), "environment": launch.env(), "enabled": true } }
                })),
            )],
            http: vec![snippet(
                "Add to ~/.config/opencode/opencode.json (or opencode.json in a project)",
                pretty(&json!({
                    "$schema": "https://opencode.ai/config.json",
                    "mcp": { name: { "type": "remote", "url": url, "headers": launch.headers(), "enabled": true } }
                })),
            )],
            http_note: "",
        },
        ClientSetup {
            id: "other",
            name: "Any other client",
            kind: "Generic",
            check: "Most clients list connected servers and their tools in their settings.",
            stdio: vec![
                snippet(
                    "The command the client should launch, with this in its environment",
                    format!("{API_KEY_ENV}={key} {shell}"),
                ),
                snippet("Most clients take an \"mcpServers\" block like this", servers(launch.stdio_entry())),
            ],
            http: vec![
                snippet(
                    "Streamable HTTP endpoint, and the header to send",
                    format!("{url}\nAuthorization: {}", launch.bearer()),
                ),
                snippet(
                    "Most clients take an \"mcpServers\" block like this",
                    servers(json!({ "type": "http", "url": url, "headers": launch.headers() })),
                ),
            ],
            http_note: "",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_carry_the_binary_and_endpoint() {
        let args = vec!["mcp".to_owned()];
        let launch = Launch {
            command: "/Applications/Local Leads/localleads",
            args: &args,
            url: "http://127.0.0.1:7878/mcp",
            name: "localleads",
            key: "ll_test",
        };
        let all = setups(&launch);
        let claude = all.iter().find(|setup| setup.id == "claude-code").unwrap();
        assert_eq!(
            claude.stdio[0].code,
            "claude mcp add --scope user localleads --env LOCALLEADS_API_KEY=ll_test -- '/Applications/Local Leads/localleads' mcp"
        );
        let cursor = all.iter().find(|setup| setup.id == "cursor").unwrap();
        let parsed: Value = serde_json::from_str(&cursor.http[0].code).unwrap();
        assert_eq!(parsed["mcpServers"]["localleads"]["url"], "http://127.0.0.1:7878/mcp");
        assert_eq!(parsed["mcpServers"]["localleads"]["headers"]["Authorization"], "Bearer ll_test");
        let cursor_stdio: Value = serde_json::from_str(&cursor.stdio[0].code).unwrap();
        assert_eq!(cursor_stdio["mcpServers"]["localleads"]["env"]["LOCALLEADS_API_KEY"], "ll_test");
        // Every snippet carries the key.
        assert!(all.iter().flat_map(|setup| setup.stdio.iter().chain(&setup.http)).all(|snippet| snippet.code.contains("ll_test")));
        assert!(all.iter().all(|setup| !setup.stdio.is_empty()));
        assert!(all.iter().all(|setup| !setup.http.is_empty() || !setup.http_note.is_empty()));
    }
}
