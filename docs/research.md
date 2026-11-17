# LocalLeads research notes

Research was performed against the current documentation available on 2026-10-11.

## Desktop UI

The application uses eframe/egui for the native desktop window and an unbounded event channel from background Tokio tasks. Rendering reads application state only. Scraping, proxy checks, exports, and backups run outside the UI callback.

## SQLite

- SQLite client documentation: <https://www.sqlite.org/docs.html>
- Rust wrapper: <https://github.com/rusqlite/rusqlite>

SQLite is embedded through rusqlite with WAL mode, normal synchronous writes, and a busy timeout. The app owns one connection protected by a mutex, which keeps transactions and schema setup coordinated while avoiding DuckDB's write contention for this transaction-heavy workload. Leads and proxy checks use append-oriented tables and indexes suited to operational filtering.

The database marks jobs that were active at startup as interrupted. Successful lead writes happen before progress is reported, so a stopped or failed job keeps its collected records. Backups checkpoint the WAL before copying the database; restore is an explicit pre-launch operation.

## Google Maps web reference

- Outscraper reference page: <https://outscraper.com/google-maps-scraper/>
- `gosom/google-maps-scraper`: <https://github.com/gosom/google-maps-scraper>
- `worldscraping/google-maps-scraper`: <https://github.com/worldscraping/google-maps-scraper>
- `jinef-john/google-maps-scraper`: <https://github.com/jinef-john/google-maps-scraper>

The collector uses the public Google web search endpoint with `tbm=map`, the query, and the internal `pb` map payload used by the open-source projects above. It first requests a map bootstrap response to obtain the search center, then requests result pages with `!8i<offset>` pagination. Responses begin with the XSSI prefix `)]}'`; result rows are currently read from `data[0][1]`, with business payloads at row `[14]`.

The parser currently maps name, categories, address, website links, phone, coordinates, rating, review count when present, status, hours, time zone, photos, Google identifiers, and raw source data. The response is undocumented and can change. Consent pages, rate limits, and access restrictions are reported as errors; LocalLeads does not attempt CAPTCHA or access-control bypasses.

## Proxy decisions

Proxy tests use https://www.google.com/maps as the required primary endpoint. A generic IP endpoint is only used as optional exit-IP enrichment after the Maps request succeeds. HTTP, HTTPS, SOCKS4, and SOCKS5 are supported by reqwest features. The checker runs selected proxies through bounded high concurrency, persists every result and check history, and emits row-level progress as checks complete. Proxy groups are stored in SQLite and running jobs refresh their selected group pool when membership changes. Credentials are masked in the app and exports; the local SQLite file still contains the credentials needed to use the proxy and should be protected with normal filesystem permissions.

## Open-source comparison

The eframe/egui and rusqlite repositories were inspected for maintained API patterns. The open-source Maps projects were compared for request construction and nested response parsing. LocalLeads uses their direct HTTP map-search approach while keeping the parser optional-field based, so a missing field becomes blank instead of stopping the whole job.
