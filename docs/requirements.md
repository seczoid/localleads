# Requirements checklist

| Reference capability | LocalLeads status |
| --- | --- |
| Category, brand, arbitrary text queries | Implemented: a picker over Google's business categories (with ready-made groups) plus free text searched as typed |
| Multiple queries and locations | Implemented as a query/location cross product |
| Country and location coverage picker | Every country, with its states/provinces, cities and postal codes, plus splitting large areas into smaller ones |
| Maps URLs and place identifiers | Implemented with Maps web search and place ID extraction |
| Country, city, region, custom locations | Implemented as free-form location strings plus language/region |
| Exact match | Implemented by quoted text queries |
| Total and per-query limits | Implemented and validated |
| Collect all available pages | Implemented through map-result offsets |
| Starting position | Implemented as a first-result offset |
| Advanced filters | Implemented for name, category, address, phone, website, rating, review count, and status with comparison/text/empty operators |
| Output field catalog | Clean CSV with chosen columns (one row per business), or the full Outscraper-format CSV; raw Maps data is retained |
| Duplicate detection | Implemented with unique place ID or normalized name/address key |
| Business extraction | Implemented for name, categories, address, phone, website links, coordinates, rating, reviews when present, status, hours, time zone, photos, Maps IDs, and raw source metadata |
| Third-party enrichment | Websites are visited directly for emails and social profiles, with optional email checks; no outside enrichment service is called |
| Job name, tags, formats, exports | Implemented in wizard, durable jobs, and CSV/JSON exports |
| Partial result preservation | Implemented in SQLite before progress events |
| Proxy add/import/normalize/deduplicate | Implemented by paste or file upload in the browser |
| Proxy enable/disable/delete and masked credentials | Implemented as bulk table operations with row selection |
| Proxy groups | Implemented with persistent groups, membership operations, and job group selection |
| Google Maps-specific proxy health check | Implemented with configurable concurrency up to 1024, timeout, retries, latency, expected response, optional exit IP, and live row updates |
| Proxy health history | Implemented in proxy_checks |
| Proxy-aware scraping | Implemented with healthy selection, bounded retries, cooldown via failure exclusion, and direct fallback policy |
| Pause/resume/stop | Implemented; stopped, cancelled or failed searches can be continued from where they left off |
| Restart recovery | Searches that were running carry on by themselves after a restart, update or crash |
| Lead search/filter/detail | Implemented in the web app and over MCP |
| CSV and JSON exports | Implemented with temporary files and rename |
| Undo deletes | Deleted searches, leads and proxies can be restored for an hour |
| SQLite backup/restore | Backup from Settings or the `backup_database` MCP tool; restore is a pre-launch CLI operation (`--restore`) |
| CAPTCHA bypass/access-control evasion | Deliberately unavailable |
| Outscraper-only enrichment packages | Unavailable without a real supported integration |
| Google Maps web response scraping | Implemented through the `tbm=map` endpoint and internal map payload |
| Recurring proxy checks | Not included in the first executable |
