<div align="center">

<img src="static/favicon.svg" width="72" alt="LocalLeads logo">

# LocalLeads

**Find every local business on Google Maps, get their emails, and let your AI agent do the rest.**

A self-hosted lead machine written in Rust. Search whole states ZIP code by ZIP code, read each business's website for emails, socials and phone numbers, verify the emails, rotate through hundreds of checked proxies, and drive all of it from Claude, Codex or Cursor over MCP. One binary, one SQLite file, no API keys, no monthly bill.

[![License: MIT](https://img.shields.io/badge/license-MIT-22c55e.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024-f74c00.svg?logo=rust)](https://www.rust-lang.org)
[![Docker](https://img.shields.io/badge/docker-ready-2496ed.svg?logo=docker&logoColor=white)](docs/SELF_HOSTING.md)
[![MCP](https://img.shields.io/badge/MCP-40_tools-8b5cf6.svg)](#-built-for-ai-agents-mcp)
[![Docker image](https://github.com/seczoid/localleads/actions/workflows/docker.yml/badge.svg)](https://github.com/seczoid/localleads/actions/workflows/docker.yml)

[Features](#-features) · [Quick start](#-quick-start) · [MCP](#-built-for-ai-agents-mcp) · [Self-hosting](#-self-hosting-with-https) · [Docs](docs/SELF_HOSTING.md)

<!-- SCREENSHOT: hero shot of the dashboard with a search running (1440x900 or wider) -->
<img src="docs/images/hero.png" alt="LocalLeads dashboard" width="900">

</div>

---

## ✨ Why LocalLeads

- **Whole regions, not 120 results.** Google Maps stops at about 120 results per lookup. LocalLeads splits a state, a city or a whole country into ZIP codes, busiest first, so you get what Maps actually has, not the first page.
- **Emails, not just listings.** Every lead with a website gets its site read: home, contact, about and team pages. Out come emails (even obfuscated and Cloudflare-protected ones), the people behind them, socials, extra phones and the tech the site runs.
- **Proxies that actually work.** A built-in proxy checker tests hundreds of proxies against Google Maps at once, groups them, and fails over mid-search when one dies.
- **Your agent can run it.** A full MCP server with 40 tools: "find 500 roofers in Austin with emails and export them" just works.
- **Yours.** Runs on your laptop or your VPS. Your data stays in one SQLite file you own. No API key, no per-lead pricing.
- **Fast and light.** Rust, Tokio and SQLite. Reads 64 websites at a time, keeps the leads table quick at millions of rows, and idles at under 5 MB of RAM in Docker.

## 🚀 Features

### 🔎 Google Maps search at scale

<!-- SCREENSHOT: the New search form with a state picked and the summary panel visible -->
<img src="docs/images/new-search.png" alt="Setting up a search" width="900">

- **Any business, anywhere:** one search can cover many business types ("plumbers", "hvac", "roofers"…), each searched in every area.
- **Location picker:** a country, whole states or provinces, single cities, ZIP codes, or your own list of places. 19 countries built in; the US ships with 40,000+ ZIP codes and city lookup.
- **ZIP-by-ZIP splitting:** big areas are broken into ZIP codes, busiest first, to get past Google's per-lookup cap.
- **Three search modes:** text search, a Google Maps URL, or a list of place IDs.
- **Filters** for rating, review count, has website, has phone and more, applied as results come in.
- **Global de-duplication:** each business is stored once and linked to every search that found it, so overlapping areas never create repeats.
- **Fine control:** maximum results, results per area, skip the first N, language and region, exact-match keywords, retries, and requests at once (10 without proxies, 100 to 300 with them).
- **Preview first:** the summary shows how many lookups a search will make before you start it.

### 📡 Live progress

<!-- SCREENSHOT: a running search page with the progress line and event log -->
<img src="docs/images/search-live.png" alt="A search running live" width="900">

- Watch areas searched, leads saved, leads enriched and emails found climb in real time, over server-sent events.
- Websites are read **while** Google is still being searched, so emails show up within seconds.
- Pause, resume, stop or run a search again. An event log records every step, retry and failure.

### 📧 Email and contact enrichment

<!-- SCREENSHOT: a lead's detail panel with emails, sources, socials and place details -->
<img src="docs/images/lead-detail.png" alt="Everything found for one lead" width="900">

For every lead with a website, LocalLeads reads up to 8 pages: the home page first, then links that look like contact, about or team pages, then common paths like `/contact` if nothing turned up. It picks up:

- `mailto:` links, plain addresses, and ones hidden as `name [at] domain (dot) com`
- Cloudflare-protected emails and emails in structured data (JSON-LD)
- **Names and job titles** next to an address, from team pages
- **Socials:** Facebook, Instagram, LinkedIn, X, YouTube and TikTok
- Extra phone numbers, the site title, and whether the site runs Google Tag Manager or a Facebook Pixel

Then it ranks them: emails on the company's own domain first, a real person's address ahead of `info@`. That one becomes the lead's **best email**.

**Google place details** (optional): reviews broken down by star, review topics, owner information, photos and opening hours.

**Email verification** (optional, per search): asks each address's mail server whether the mailbox exists, without sending anything. It checks the 5 best emails per lead and labels each one:

| Verdict | Meaning |
| --- | --- |
| ✅ Safe | The server confirmed the mailbox |
| ⚠️ Risky | Catch-all domain, role or disposable address |
| ❔ Unverified | The server wouldn't say |
| ❌ Invalid | No such mailbox; never picked as the best email |

### 📋 Leads browser and exports

<!-- SCREENSHOT: the leads table with the filter sidebar and facet counts -->
<img src="docs/images/leads-browse.png" alt="Browsing and filtering leads" width="900">

- **One list per search,** plus all leads together.
- **Faceted filters** with live counts: search, category, state, city, has phone, has website, email found, rating.
- **Instant search** across name, address, phone, website and email.
- **Server-side paging,** so the table stays fast with millions of rows.
- **Bulk actions** on selected rows or everything that matches: export, delete, or find emails again.
- **Exports:** CSV in a 120-column, Outscraper-style schema, or JSON with the raw Maps data. Download them, or write big exports straight to disk.
- **Lead detail:** every email and the page it came from, socials, phones, reviews, hours and coordinates.

### 🛡️ Proxy checker and manager

<!-- SCREENSHOT: the proxies page with the health table, groups and a check running -->
<img src="docs/images/proxies.png" alt="Proxy checker" width="900">

Google limits how fast one connection can search. LocalLeads has a complete proxy toolkit built in:

- **Paste or import** proxies in any common format: `http://user:pass@host:port`, `socks5://…`, `host:port`. Big files import in the background, de-duplicated.
- **Health checks** test every proxy against Google Maps at once and mark it healthy, slow or failed, with its latency and last error.
- **Tunable checker:** test URL, timeout, retries, slow threshold, expected page content, exit-IP check, and how many checks run at once.
- **History** of every check per proxy: when, status, latency, exit IP.
- **Groups** let a search use only some proxies, like a residential pool.
- **Failover:** a proxy that fails mid-search is set aside, and the request retries through another.
- **Bulk tools:** enable, disable, delete, or prune everything that keeps failing.

HTTP, HTTPS and SOCKS5 are supported. Only Google lookups go through proxies; websites are read directly.

### 🤖 Built for AI agents (MCP)

<!-- SCREENSHOT: the MCP page with the agent picker and an API key -->
<img src="docs/images/mcp.png" alt="Connecting an AI agent" width="900">

LocalLeads includes a **Model Context Protocol** server. Everything the dashboard does, an agent can do: **40 tools** covering searches, leads, exports, enrichment, proxies, groups and backups.

- **11 clients set up for you:** Claude Code, Codex, Cursor, Claude Desktop, VS Code (Copilot), Windsurf, Gemini CLI, Zed, Cline, opencode, and any other MCP client. The MCP page shows the exact command or config for each, with your paths and API key filled in.
- **Two ways to connect:** stdio (the agent launches `localleads mcp`) or Streamable HTTP at `/mcp`. Both share the dashboard's live state, so searches an agent starts show up on screen in real time.
- **API keys:** create one per agent, see when each was last used, revoke any of them instantly. Keys are stored only as hashes.
- **Safe by default:** delete tools are marked destructive, so agents ask first.

Things you can just ask:

> *"Find 500 roofers in Austin, TX with websites and emails, and tell me when it's done."*
>
> *"Show me the dentists from my last search rated 4.5+ that have an email, as a table."*
>
> *"Export every lead with a verified email from the plumbers search to CSV on my desktop."*
>
> *"Check all my proxies and remove the ones that keep failing."*

<details>
<summary><b>All 40 tools</b></summary>

| Area | Tools |
| --- | --- |
| Searches | `search_options` `find_cities` `preview_search` `create_search` `rerun_search` `list_searches` `get_search` `pause_search` `resume_search` `stop_search` `delete_search` |
| Leads | `browse_leads` `lead_facets` `lead_lists` `get_lead` `enrich_leads` `export_leads` `delete_leads` |
| Proxies | `list_proxies` `proxy_status` `add_proxies` `import_proxy_file` `check_proxies` `cancel_proxy_check` `set_proxies_enabled` `delete_proxies` `prune_proxies` `proxy_history` `get_checker_settings` `update_checker_settings` |
| Groups | `list_proxy_groups` `create_proxy_group` `rename_proxy_group` `delete_proxy_group` `assign_proxies_to_group` `remove_proxies_from_group` |
| App | `overview` `app_info` `list_notices` `backup_database` |

</details>

### 🔐 Self-hosting with HTTPS

- **Docker + Caddy:** one `docker compose up` gets you automatic HTTPS from Let's Encrypt, HTTP/3 and compression.
- **Accounts:** first-run setup creates your account (Argon2id password hash); sign out and change your password from Settings.
- **Hardened for the internet:** a one-time setup code so no stranger can claim a fresh install, sign-in lockout after repeated failures, `Secure` cookies, a Content-Security-Policy and other security headers, and a host allowlist.
- **Small footprint:** a 194 MB image that idles at under 5 MB of RAM, runs as an unprivileged user, has a health check and shuts down cleanly.

### 🧰 And also

- **Dashboard** with totals, running searches and recent leads at a glance.
- **Backups:** one-click consistent snapshots while searches run, and restore from the command line.
- **Built-in docs** page with a walkthrough of every screen.
- **Dark and light themes.**
- **JSON API** (`/api/stats`, `/api/jobs`, `/api/leads`) for scripts, using an API key or your session.

## ⚡ Quick start

### Option 1: Docker on a server, with HTTPS

You need a Linux server with Docker, and a domain whose DNS points at it.

```bash
git clone https://github.com/seczoid/localleads.git
cd localleads
cp .env.example .env
```

Set your domain in `.env`:

```bash
DOMAIN=leads.example.com
```

Start it (the first build compiles the app and takes 10 to 20 minutes):

```bash
docker compose up -d --build
```

Get the one-time setup code for creating your account:

```bash
docker compose logs localleads
```

Open `https://leads.example.com`, enter the code, and create your account. Caddy fetches and renews the certificate on its own.

Update later with:

```bash
git pull && docker compose up -d --build
```

> Backups, restores, all settings, and running behind your own nginx or Traefik are covered in **[docs/SELF_HOSTING.md](docs/SELF_HOSTING.md)**.

### Option 2: Docker on your own computer

```bash
git clone https://github.com/seczoid/localleads.git
cd localleads
docker build -t localleads .
docker run -d --name localleads -p 127.0.0.1:7878:7878 -v localleads-data:/data localleads
docker logs localleads
```

Open [http://127.0.0.1:7878](http://127.0.0.1:7878) and create your account with the setup code from the log.

### Option 3: Build from source

**1. Install the build tools.**

- macOS:

  ```bash
  xcode-select --install
  brew install cmake
  ```

- Debian / Ubuntu:

  ```bash
  sudo apt-get install -y build-essential cmake clang libclang-dev pkg-config
  ```

- Rust 1.88 or newer:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

**2. Build and install.**

```bash
git clone https://github.com/seczoid/localleads.git
cd localleads
cargo install --path . --features verify
```

`--features verify` adds email verification. Leave it off for a smaller, faster build.

**3. Run it.**

```bash
localleads --open
```

The dashboard opens at [http://127.0.0.1:7878](http://127.0.0.1:7878). Create your account on the first visit.

### Connect your AI agent

Register LocalLeads with Claude Code, Codex and Cursor in one go. This also creates an API key:

```bash
localleads mcp install --write
```

Or open **MCP** in the sidebar, create an API key, pick your agent, and paste its command or config. For Claude Code over HTTP, that looks like:

```bash
claude mcp add --scope user --transport http localleads https://leads.example.com/mcp \
  --header "Authorization: Bearer ll_your_api_key"
```

Full MCP details: **[docs/MCP.md](docs/MCP.md)**.

## ⚙️ Command line

```text
localleads [OPTIONS] [COMMAND]

Commands:
  mcp                 MCP server over stdio for AI agents
  mcp install         Print or (--write) apply the setup for Claude Code, Codex and Cursor

Options:
  --db <PATH>          SQLite database path               [env: LOCALLEADS_DB]
  --bind <ADDR>        Listen address (127.0.0.1:7878)     [env: LOCALLEADS_BIND]
  --public-url <URL>   Public address, e.g. https://leads.example.com
                                                           [env: LOCALLEADS_PUBLIC_URL]
  --behind-proxy       Trust X-Forwarded-* from a reverse proxy
                                                           [env: LOCALLEADS_BEHIND_PROXY]
  --restore <PATH>     Restore a SQLite backup before starting
  --open               Open the dashboard in your browser
```

Examples:

```bash
# Another database and port
localleads --db ~/leads/roofers.sqlite --bind 127.0.0.1:7879

# Restore a backup, then start
localleads --restore ~/Downloads/localleads-backup.sqlite

# MCP over stdio with an API key
LOCALLEADS_API_KEY=ll_your_api_key localleads mcp
```

## 🏗️ How it's built

| Layer | Tech |
| --- | --- |
| Server | Rust 2024, [axum](https://github.com/tokio-rs/axum), Tokio |
| UI | Server-rendered [Askama](https://github.com/askama-rs/askama) templates, [htmx](https://htmx.org), [Alpine.js](https://alpinejs.dev), DataTables |
| Storage | SQLite in WAL mode (one writer, a pool of readers) |
| Scraping | Google Maps' web search responses parsed directly; websites read with browser-like TLS ([webclaw](https://github.com/0xMassi/webclaw)) |
| Email checks | [check-if-email-exists](https://github.com/reacherhq/check-if-email-exists) (optional `verify` feature) |
| MCP | [rmcp](https://github.com/modelcontextprotocol/rust-sdk), stdio and Streamable HTTP |
| Deploy | Multi-stage Docker image, Caddy for HTTPS, multi-arch images built in GitHub Actions |

Everything (searches, leads, proxies, settings, your account) lives in a single SQLite file. Static assets are compiled into the binary, so there's nothing else to deploy.

## 🧪 Development

```bash
cargo run                      # debug build on http://127.0.0.1:7878
cargo test --features verify   # unit and integration tests
cargo clippy --all-targets --features verify
cargo fmt
```

Pull requests are welcome. For larger changes, please open an issue first to talk it through.

## ⚖️ Responsible use

LocalLeads reads public Google Maps listings and public business websites. Google can change its responses, show consent pages or rate-limit requests at any time; LocalLeads reports those as failures and doesn't try to bypass them. You're responsible for following the laws that apply to you (for example GDPR, CAN-SPAM and CASL) and the terms of the sites you access. Use the data to reach businesses respectfully.

## 📄 License

[MIT](LICENSE) © seczoid
