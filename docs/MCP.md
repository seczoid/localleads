# LocalLeads MCP server

LocalLeads ships an MCP server, so coding agents (Claude Code, Codex, Cursor and any other MCP client) can do everything the dashboard can: run searches, browse and export leads, manage proxies and groups, and change settings.

## Install

Use one command. Claude Code:

```bash
claude mcp add --scope user localleads --env LOCALLEADS_API_KEY=ll_... -- "$(command -v localleads)" mcp
```

Or let LocalLeads print and apply the setup for every client it finds:

```bash
localleads mcp install            # print commands and snippets for Claude Code, Codex, Cursor and others
localleads mcp install --write    # register: runs `claude mcp add` / `codex mcp add`, edits ~/.cursor/mcp.json
localleads mcp install --client cursor --write
localleads --db ~/leads.sqlite mcp install --write   # pin a specific database
```

Every agent needs an API key. Create one on the dashboard's MCP page (`/agents`), or let `mcp install --write` create one (named "mcp install"). Keys look like `ll_` plus 40 characters, are shown once, and are stored only as SHA-256 hashes; revoke them on the same page. Without `--write`, the printed setup has `YOUR_API_KEY` where the key goes; pass `--api-key KEY` to fill it in.

`mcp install` puts the absolute path of the running binary into the config. After a `cargo build`, that path is inside `target/`. Run `cargo install --path .` first if you want a stable path.

### Codex (`~/.codex/config.toml`)

```toml
[mcp_servers.localleads]
command = "/absolute/path/to/localleads"
args = ["mcp"]
env = { LOCALLEADS_API_KEY = "ll_..." }
startup_timeout_sec = 30
tool_timeout_sec = 300
```

The equivalent command is `codex mcp add localleads --env LOCALLEADS_API_KEY=ll_... -- /absolute/path/to/localleads mcp`.

### Cursor (`~/.cursor/mcp.json` or `.cursor/mcp.json`)

```json
{ "mcpServers": { "localleads": { "command": "/absolute/path/to/localleads", "args": ["mcp"], "env": { "LOCALLEADS_API_KEY": "ll_..." } } } }
```

### Over HTTP, against a running dashboard

```bash
claude mcp add --scope user --transport http localleads http://127.0.0.1:7878/mcp --header "Authorization: Bearer ll_..."
```

For Cursor and other JSON configs, use `{ "url": "http://127.0.0.1:7878/mcp", "headers": { "Authorization": "Bearer ll_..." } }`. Codex takes `url = "http://127.0.0.1:7878/mcp"` with `http_headers = { Authorization = "Bearer ll_..." }`. `localleads mcp install --transport http [--url ...]` prints all of these.

## How it runs

There are two transports, and both serve the same tools:

- **Streamable HTTP** at `/mcp` on the dashboard server (`localleads`, default `http://127.0.0.1:7878/mcp`). Tools run inside the dashboard, searches keep running after the agent disconnects, and open dashboard tabs update live. Every request needs an API key (`Authorization: Bearer <key>` or `X-API-Key: <key>`), and the dashboard's request guard adds a loopback-only `Host` check and cross-site blocking.
- **stdio** (`localleads mcp [--db PATH]`):
  1. It first looks for a dashboard serving the same database. It checks the address that dashboard announces in `.<db-file>.server.json` beside the database, then `127.0.0.1:7878`, and verifies each through `/mcp/info`.
  2. If it finds one, it bridges stdio to that dashboard's `/mcp`, so there is one engine and searches stay durable. If the dashboard restarts, the bridge reconnects the session.
  3. Otherwise it runs the engine in-process. A search started this way stops when the agent closes the MCP server. `app_info` and `create_search` say so.

  Options: `--server URL` bridges to that dashboard explicitly. `--api-key KEY` (or the `LOCALLEADS_API_KEY` environment variable) is required: create a key on the dashboard's MCP page, or let `localleads mcp install --write` create one. The key is sent to the dashboard when bridging, and checked against the database when running embedded. `--embedded` never bridges.

Why stdio bridges instead of opening the database itself: opening the database marks searches left "active" as interrupted. A second process opening a database that a dashboard is using would therefore corrupt the status of that dashboard's live searches.

## Conventions

- Every tool returns JSON text. Failures come back as tool errors (`isError: true`) with a readable message.
- Large results are paged. `browse_leads` and `list_proxies` take `limit` (default 50, max 1000) and `offset`, and return `total` and `next_offset`. Paging is stable (ties are broken by id). `browse_leads.fields` trims rows to the columns you need.
- Lead tools share one filter set, including `ids` for a hand-picked selection. Bulk actions (`delete_leads`, `enrich_leads`) take `ids`, or `all_matching: true` to act on everything matching the filters, as the leads table does. `enrich_leads` runs in the background; `lead_facets` `email.pending` shows what is left.
- Long-running work runs in the background:
  - `create_search` and `rerun_search` return a search id. Poll `get_search` with it.
  - `check_proxies` starts the single global proxy check. Poll `proxy_status`, and stop it with `cancel_proxy_check`.
  - `import_proxy_file` waits for its import by default.
- Tool annotations mark read-only and destructive tools, so clients can ask before a delete.

## Search settings map onto `SearchConfig`

`create_search` and `preview_search` take the fields of `SearchConfig` by their serde names. They also accept `name` as an alias of `job_name` and friendlier enums: `mode: text|maps_url|place_id`, and `location_selection.mode: all|states|postal_codes`.

They also match the form's defaults: `location_selection.depth` defaults to `zip`, the form's only depth, and the selection stays the source of truth (`locations` is left empty and worked out when the search runs). An empty `region` defaults to the selected country's Google `gl` code.

The input is laid over `SearchConfig::default()` serialized as JSON, or over the copied search's config when `copy_from` is set, and is then deserialized and validated by `SearchConfig::validate`. As a result:

- A new `SearchConfig` (or `LocationSelection`) field works through MCP immediately as a pass-through key, with its default taken from `Default`. Unknown keys are rejected with the list of valid ones, so typos don't pass silently.
- The test `mcp::tests::search_input_documents_every_config_field` fails until the new field is also added to `SearchInput` in `src/mcp/searches.rs` with a doc comment. That doc comment is what agents see in the tool schema.

## Feature parity: dashboard → MCP tool

| Dashboard capability (route) | MCP tool |
|---|---|
| Dashboard home: stats, recent searches/leads, proxy health, notices (`/`, `/partials/dashboard`, `/api/stats`) | `overview` |
| New search form options: modes, countries, states (`/jobs/new`, `/locations/states`) | `search_options` |
| City picker: search US cities or list a state's cities (`/locations/cities`) | `find_cities` |
| Preview lookups and validate (`POST /jobs/preview`) | `preview_search` |
| Create search with every form option: name, queries, mode, language, region, exact match, location modes (whole country / states and single US cities / ZIP codes / custom places; ZIP depth by default, `depth` optional), total/per-lookup limits, collect all, start at, concurrency, retries, proxy group, require proxy, allow direct, filters, output fields, enrichments, tags (`POST /jobs`) | `create_search` |
| Duplicate a search into the form (`/jobs/new?from=`) | `create_search` with `copy_from` |
| Rerun (`POST /jobs/{id}/rerun`) | `rerun_search` |
| Searches list with status filter (`/jobs`, `/partials/jobs`, `/api/jobs`) | `list_searches` |
| Search detail, live progress, activity, events (`/jobs/{id}`, `/jobs/{id}/live`, `/api/jobs/{id}`) | `get_search` |
| Pause / resume toggle (`POST /jobs/{id}/pause`) | `pause_search`, `resume_search` |
| Stop (`POST /jobs/{id}/stop`) | `stop_search` |
| Delete, optionally with its leads (`POST /jobs/{id}/delete?leads=`) | `delete_search` (`delete_leads`) |
| Lead lists grid: all-leads card and per-search cards (`/leads`, `/partials/lists`) | `lead_lists` |
| Faceted browse: text, searches, categories, states, cities, has phone, website, email (has / safe / none / pending), min rating, sort, paging (`/leads/browse`, `/leads/data`, `/api/leads`) | `browse_leads` |
| Facet counts and filter chips (`/partials/leads/browse`) | `lead_facets` |
| Lead detail with every search that found it, plus what enrichment found: emails and verdicts, website, socials, people, reviews per star, owner (`/leads/{id}`) | `get_lead` (`enrichment`) |
| Delete lead (`POST /leads/{id}/delete`) | `delete_leads` (`ids`) |
| Bulk on ticked rows or all matching: delete, enrich, export CSV / JSON (`POST /leads/bulk` `delete`, `enrich`, `csv`, `json`) | `delete_leads`, `enrich_leads`, `export_leads` (`ids`, or `all_matching` + filters) |
| Export CSV / JSON download (`/leads/export.csv`, `/leads/export.json`) | `export_leads` (`format`, `path`, or `inline`) |
| Save CSV + JSON to disk beside the database (`POST /leads/export-disk`) | `export_leads` (`format: both`) |
| Proxies table: group, status, enabled and text filters, sort, paging (`/proxies`, `/proxies/data`) | `list_proxies` |
| Group tabs, health bar, check progress, import progress (`/partials/proxy-groups`, `/partials/proxy-progress`) | `proxy_status`, `list_proxy_groups` |
| Paste proxies, optionally into a group (`POST /proxies/add`) | `add_proxies` |
| Import a proxy file (`POST /proxies/import`) | `import_proxy_file` |
| Bulk: check selected / check all matching (`POST /proxies/bulk` `check`, `check_all`) | `check_proxies` (`ids` or `all_matching` + filters) |
| Cancel check (`POST /proxies/check/cancel`) | `cancel_proxy_check` |
| Bulk: enable / disable (`enable`, `disable`) | `set_proxies_enabled` |
| Bulk: delete selected / all matching (`delete`) | `delete_proxies` |
| Bulk: delete failed / keep only healthy (`delete_failed`, `keep_healthy`) | `prune_proxies` (`mode`) |
| Bulk: add to group / remove from group (`assign`, `unassign`) | `assign_proxies_to_group`, `remove_proxies_from_group` |
| Proxy check history (`/proxies/{id}/history`) | `proxy_history` |
| Create / rename / delete group (`POST /groups`, `/groups/{id}/rename`, `/groups/{id}/delete`) | `create_proxy_group`, `rename_proxy_group`, `delete_proxy_group` |
| Checker settings (`/proxies` settings dialog, `POST /proxies/settings`) | `get_checker_settings`, `update_checker_settings` |
| Settings page: version, database path, data folder, size, totals (`/settings`) | `app_info` |
| Activity log (`/settings` notices, live toasts over `/events`) | `list_notices` (also in `overview`, `proxy_status`) |
| Write a backup to a path (`POST /settings/backup`) | `backup_database` |
| Download a backup (`/settings/backup.sqlite`) | `backup_database` (writes the file; the agent reads it from disk) |

The guide pages `/agents` (MCP setup) and `/docs` explain the app; they have no feature of their own. The state codes from `/locations/regions` are covered by `search_options` and `find_cities`.

These are infrastructure routes, not features: `/events` (SSE refresh signals; `list_notices` covers its notices), `/static/*` and `/healthz`. Restoring a backup is a launch flag (`localleads --restore`), not a dashboard feature.

`mcp::tests::every_dashboard_capability_has_a_tool` pins the tool list above.
