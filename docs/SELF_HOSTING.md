# Self-hosting LocalLeads

LocalLeads runs as one Docker container with a SQLite database on a volume.
The included `compose.yaml` puts [Caddy](https://caddyserver.com) in front of
it, which gets and renews HTTPS certificates from Let's Encrypt on its own.

```
internet ──443──▶ caddy (HTTPS, HTTP/3) ──▶ localleads:7878 ──▶ /data/localleads.sqlite
```

## The quick way: setup.sh

On the server (or your Mac):

```bash
curl -fsSL https://raw.githubusercontent.com/seczoid/localleads/main/setup.sh | bash
```

It downloads the latest release for the machine, verifies its checksum, asks
where you'll open LocalLeads (your domain, the server's IP, only this
machine, or behind your own web server), and starts it. Nothing else to
install: if HTTPS needs Caddy and it's missing, the script installs it from
Caddy's official apt or COPR repository (or pacman, zypper, or the official
binary).

What it sets up on Linux:

| What | Where |
| --- | --- |
| Program | `/usr/local/bin/localleads` |
| Settings | `/etc/localleads/localleads.env` |
| Data (SQLite) | `/var/lib/localleads/` |
| Service | `localleads.service`, running as the `localleads` system user, sandboxed (`ProtectSystem=strict`, `NoNewPrivileges`, ...) |
| HTTPS | Caddy, with LocalLeads in `/etc/caddy/localleads.caddy`, imported from your Caddyfile |

On macOS everything lives in `~/.localleads/`, and a launchd agent starts it
when you log in.

If Caddy already serves other sites, LocalLeads is added next to them and
the old Caddyfile is kept as `Caddyfile.before-localleads`. If ports 80 and
443 belong to something else, the script names it and offers other ports (an
IP address can use any; a domain needs 80 or 443 for Let's Encrypt) or
putting LocalLeads behind that web server, with an nginx, Caddy or Apache
config to paste in.

Run the same command again to update, change settings or uninstall.
Unattended:

```bash
curl -fsSL https://raw.githubusercontent.com/seczoid/localleads/main/setup.sh | sudo bash -s -- --domain leads.example.com --yes
```

Day to day:

```bash
journalctl -u localleads -f       # logs (and the setup code)
systemctl restart localleads
sudo -u localleads env LOCALLEADS_API_KEY=ll_... localleads --db /var/lib/localleads/localleads.sqlite mcp   # stdio MCP on the server
```

`bash setup.sh --help` lists every option. The sections on settings,
security and connecting agents below apply to both setups; backups work the
same way from the dashboard.

The rest of this page runs it with Docker instead.

## What you need

- A Linux server with Docker Engine and the Compose plugin
  (`docker compose version`). 2 GB of RAM runs it comfortably. Building the
  image from source needs about 4 GB (or swap); on a smaller server, build
  elsewhere or use a prebuilt image.
- A domain or subdomain whose DNS `A` (and `AAAA`, if you have IPv6) record
  points at the server.
- Ports 80 and 443 open (TCP, plus 443/UDP for HTTP/3). Port 80 is only used
  to get certificates and redirect to HTTPS.

## Start it

```bash
git clone https://github.com/seczoid/localleads.git
cd localleads
cp .env.example .env
nano .env              # set DOMAIN=leads.example.com
docker compose up -d --build
```

The first build compiles the app and takes 10 to 20 minutes. Later builds
reuse the cache.

### Create your account

A fresh install has no account, and anyone who reaches it first could create
one. So the server prints a one-time **setup code** to its log:

```bash
docker compose logs localleads
```

```
  No account yet. Open the app and create one with this setup code:

      k7Pq-2mXa-9TzR-bW4e
```

Open `https://leads.example.com`, enter the code, your email and a password
(twice). From then on you sign in with the email and password. The code stops
working once the account exists. To choose the code yourself, set
`LOCALLEADS_SETUP_CODE` in `.env` before the first start.

There is one account per database, and no password reset. If you lose the
password, restore a backup or start over with a new volume.

## Connect AI agents (MCP)

Agents connect to `https://leads.example.com/mcp` with an API key:

1. Open **MCP** in the sidebar, name a key and create it. It's shown once.
2. Pick your agent. The page shows its setup with your URL and key filled in.

Every MCP request needs `Authorization: Bearer <key>` (or `X-API-Key: <key>`).
Revoke a key on the same page and it stops working immediately.

An agent on the server itself can also use stdio:

```bash
docker compose exec -T -e LOCALLEADS_API_KEY=ll_... localleads localleads mcp
```

## Update

```bash
git pull
docker compose up -d --build
```

The database schema upgrades itself on start. Searches that were running when
the container stopped are marked interrupted; resume them from the dashboard.

## Back up and restore

Everything lives in one SQLite file in the `localleads-data` volume:
searches, leads, proxies (with their passwords), your account and API key
hashes.

- **Back up:** Settings → **Download a backup** takes a consistent copy while
  the app runs. Agents can call the `backup_database` MCP tool.
- **Copy the volume** (with the app stopped):

  ```bash
  docker compose stop localleads
  docker run --rm -v localleads_localleads-data:/data -v "$PWD":/backup debian:trixie-slim \
    tar czf /backup/localleads-data.tgz -C /data .
  docker compose start localleads
  ```

- **Restore** a downloaded backup:

  ```bash
  docker compose stop localleads
  docker compose cp ./localleads-backup.sqlite localleads:/data/restore.sqlite
  docker compose run --rm --no-deps --user root --entrypoint sh localleads -c \
    'rm -f /data/localleads.sqlite-wal /data/localleads.sqlite-shm && mv /data/restore.sqlite /data/localleads.sqlite && chown localleads:localleads /data/localleads.sqlite'
  docker compose start localleads
  ```

Keep backups private: they contain proxy credentials and lead data.

## Settings

Set these in `.env` (Compose passes them on) or as container environment
variables.

| `.env` | Container variable | What it does |
| --- | --- | --- |
| `DOMAIN` | | Domain Caddy serves and gets a certificate for. Required. |
| `PUBLIC_URL` | `LOCALLEADS_PUBLIC_URL` | Address people open. Defaults to `https://$DOMAIN`. The app only answers requests for this host (and `localhost`, for health checks). |
| `HTTP_PORT`, `HTTPS_PORT` | | Host ports for Caddy. Default 80 and 443. With another HTTPS port, set `PUBLIC_URL` to include it. |
| `LOCALLEADS_SETUP_CODE` | `LOCALLEADS_SETUP_CODE` | Fixed setup code instead of a random one. |
| `LOCALLEADS_IMAGE` | | Image to run, e.g. a prebuilt `ghcr.io/seczoid/localleads:latest`. |
| | `LOCALLEADS_BEHIND_PROXY` | Trust `X-Forwarded-For`/`-Proto` from the proxy. Set to `true` in `compose.yaml`. |
| | `LOCALLEADS_DB` | Database path. `/data/localleads.sqlite` in the image. |
| | `LOCALLEADS_BIND` | Listen address. `0.0.0.0:7878` in the image. |

The same settings exist as flags for a native install: `--public-url`,
`--behind-proxy`, `--db`, `--bind`.

## Security

- **HTTPS everywhere:** Caddy redirects HTTP to HTTPS and sends HSTS. The app
  marks its sign-in cookie `Secure`, `HttpOnly` and `SameSite=Lax`.
- **Accounts:** Argon2id password hashes. Sign-in tokens and API keys are
  stored only as SHA-256 hashes, so a stolen database can't be used to sign in.
- **Brute force:** after 10 wrong passwords (or setup codes) from one address
  in 15 minutes, that address is locked out for the rest of the window; after
  100 failures from anywhere, sign-in pauses for everyone.
- **Requests:** unknown `Host` names are refused (DNS rebinding), cross-site
  form posts are blocked, and every page sends a Content-Security-Policy,
  `X-Frame-Options: DENY`, `nosniff` and `no-store` caching.
- **Container:** runs as an unprivileged user, publishes no ports itself
  (only Caddy does), and has a health check.

Docker publishes ports past `ufw`/firewalld rules. Here only Caddy's 80/443
are published, which is what you want open anyway.

## Things to know on a VPS

- **Google limits datacenter IPs.** A VPS address gets rate-limited much
  sooner than a home connection. Add proxies (Proxies page) for anything
  beyond small searches, and keep **Requests at once** modest.
- **Email verification uses outbound port 25,** which most cloud providers
  block by default. Searches still find emails without it; turn **Verify
  emails** off, or ask your provider to open port 25.
- **Disk:** leads add up. About 5 KB per lead, so a million leads is
  roughly 5 GB.

## Without Caddy

Already running nginx, Traefik or another proxy? Run only the app and point
your proxy at it.

```bash
docker build -t localleads .
docker run -d --name localleads --restart unless-stopped \
  -p 127.0.0.1:7878:7878 -v localleads-data:/data \
  -e LOCALLEADS_PUBLIC_URL=https://leads.example.com \
  -e LOCALLEADS_BEHIND_PROXY=true \
  localleads
```

Your proxy must pass the original `Host`, set `X-Forwarded-For` and
`X-Forwarded-Proto`, and not buffer `/events` or `/mcp` (they stream). For
nginx:

```nginx
location / {
    proxy_pass http://127.0.0.1:7878;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_http_version 1.1;
    proxy_buffering off;
    proxy_read_timeout 1h;
    client_max_body_size 64m;
}
```

## On your own computer

- **With HTTPS:** set `DOMAIN=localhost` and run `docker compose up -d --build`,
  then open `https://localhost`. Caddy uses its own local certificate
  authority, so the browser warns until you trust it
  (`docker compose exec caddy caddy trust` shows how).
- **Plain HTTP:** run only the app, published on localhost:

  ```bash
  docker build -t localleads .
  docker run -d -p 127.0.0.1:7878:7878 -v localleads-data:/data localleads
  ```

  Open `http://127.0.0.1:7878`. It still asks for the setup code from
  `docker logs`, since inside the container it listens on all interfaces.
- **No Docker:** `cargo run --release` serves on `http://127.0.0.1:7878`, with
  no setup code.
