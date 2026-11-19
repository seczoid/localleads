#!/usr/bin/env bash
# LocalLeads setup: installs LocalLeads and starts it.
#
#   curl -fsSL https://raw.githubusercontent.com/seczoid/localleads/main/setup.sh | bash
#
# It downloads the latest release for this machine, asks a few questions
# (press Enter for the defaults), sets up HTTPS with Caddy (installing Caddy
# if it's missing), runs LocalLeads as a service that starts on boot, and
# prints the address and the setup code for creating your account. Run it
# again to update, change settings or uninstall.
#
# Linux servers (x86_64, ARM64; systemd) and macOS. Unattended:
#
#   curl -fsSL .../setup.sh | bash -s -- --domain leads.example.com --yes
#
# Run with --help for every option.

# Everything is inside main(), called on the last line, so a download cut
# short never runs half a script.

set -euo pipefail

REPO="seczoid/localleads"
SCRIPT_URL="https://raw.githubusercontent.com/$REPO/main/setup.sh"
RELEASES="https://github.com/$REPO/releases"
SERVICE="localleads"
LABEL="com.localleads.server"
# First line of a Caddyfile that setup.sh owns, and the line it adds to a
# Caddyfile that's yours.
CADDY_OWNED="# Managed by LocalLeads setup.sh"
CADDY_ADDED="# Added by LocalLeads setup.sh"
CADDY_SITE=/etc/caddy/localleads.caddy
CADDYFILE=/etc/caddy/Caddyfile
# Coolify's Traefik reads extra routes from here (see coolify_owns_443).
COOLIFY_DYNAMIC=/data/coolify/proxy/dynamic
COOLIFY_ROUTE=$COOLIFY_DYNAMIC/localleads-acme.yaml

# ------------------------------------------------------------ output

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
	BOLD=$'\033[1m' DIM=$'\033[2m' RED=$'\033[31m' GREEN=$'\033[32m'
	YELLOW=$'\033[33m' BLUE=$'\033[34m' CYAN=$'\033[36m' RESET=$'\033[0m'
else
	BOLD="" DIM="" RED="" GREEN="" YELLOW="" BLUE="" CYAN="" RESET=""
fi

say() { printf '%s\n' "$*"; }
step() { printf '\n%s==>%s %s%s%s\n' "$BLUE" "$RESET" "$BOLD" "$*" "$RESET"; }
ok() { printf '  %s✓%s %s\n' "$GREEN" "$RESET" "$*"; }
note() { printf '  %s%s%s\n' "$DIM" "$*" "$RESET"; }
warn() { printf '  %s!%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
	printf '\n%s✗ %s%s\n' "$RED" "$1" "$RESET" >&2
	shift
	local line
	for line in "$@"; do printf '  %s\n' "$line" >&2; done
	printf '\n' >&2
	exit 1
}

lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

# ------------------------------------------------------------ prompts

INTERACTIVE=0
ANSWER=""

# Questions are read from the terminal, not stdin, since stdin is the
# script itself under `curl | bash`.
open_tty() {
	if [ "$ASSUME_YES" = 1 ]; then return; fi
	if (exec 3</dev/tty) 2>/dev/null; then
		exec 3</dev/tty
		INTERACTIVE=1
	fi
}

# ask "Question" [default]: sets ANSWER.
ask() {
	local question=$1 default=${2-}
	if [ "$INTERACTIVE" != 1 ]; then
		ANSWER=$default
		return
	fi
	if [ -n "$default" ]; then
		printf '%s?%s %s %s[%s]%s ' "$CYAN" "$RESET" "$question" "$DIM" "$default" "$RESET"
	else
		printf '%s?%s %s ' "$CYAN" "$RESET" "$question"
	fi
	IFS= read -r ANSWER <&3 || ANSWER=""
	ANSWER=$(printf '%s' "$ANSWER" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
	[ -n "$ANSWER" ] || ANSWER=$default
}

# confirm "Question" y|n: succeeds on yes.
confirm() {
	local question=$1 default=$2 hint
	if [ "$INTERACTIVE" != 1 ]; then
		[ "$default" = y ]
		return
	fi
	if [ "$default" = y ]; then hint="Y/n"; else hint="y/N"; fi
	while :; do
		printf '%s?%s %s %s[%s]%s ' "$CYAN" "$RESET" "$question" "$DIM" "$hint" "$RESET"
		IFS= read -r ANSWER <&3 || ANSWER=""
		case $(lower "$ANSWER") in
		"") [ "$default" = y ]; return ;;
		y | yes) return 0 ;;
		n | no) return 1 ;;
		*) say "  Please answer y or n." ;;
		esac
	done
}

# choose default "Option one" "Option two" ...: sets ANSWER to the number.
choose() {
	local default=$1 i=1 option
	shift
	if [ "$INTERACTIVE" != 1 ]; then
		ANSWER=$default
		return
	fi
	for option in "$@"; do
		if [ "$i" = "$default" ]; then
			printf '  %s%s)%s %s\n' "$BOLD" "$i" "$RESET" "$option"
		else
			printf '  %s) %s\n' "$i" "$option"
		fi
		i=$((i + 1))
	done
	while :; do
		ask "Choose 1-$#" "$default"
		case $ANSWER in
		*[!0-9]* | "") ;;
		*) if [ "$ANSWER" -ge 1 ] && [ "$ANSWER" -le $# ]; then return; fi ;;
		esac
		say "  Type a number from 1 to $#."
	done
}

# ------------------------------------------------------------ helpers

OS=$(uname -s)
SUDO=""

# Runs a command as root (through sudo when needed).
as_root() {
	if [ -n "$SUDO" ]; then "$SUDO" "$@"; else "$@"; fi
}

is_ipv4() {
	local re='^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$'
	[[ $1 =~ $re ]] || return 1
	local part
	for part in "${BASH_REMATCH[@]:1}"; do [ "$part" -le 255 ] || return 1; done
}

is_domain() {
	local re='^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z][a-z0-9-]{0,61}[a-z0-9]$'
	[[ $1 =~ $re ]]
}

is_port() {
	case $1 in *[!0-9]* | "") return 1 ;; esac
	[ "$1" -ge 1 ] && [ "$1" -le 65535 ]
}

# "https://Leads.Example.com/path" -> "leads.example.com"
bare_host() {
	lower "$1" | sed -E 's#^[a-z]+://##; s#/.*$##; s#:[0-9]+$##; s#\.$##'
}

public_ipv4() {
	local url ip
	for url in https://api.ipify.org https://ipv4.icanhazip.com https://ifconfig.me/ip; do
		ip=$(curl -4 -fsS --max-time 5 "$url" 2>/dev/null | tr -d '[:space:]') || continue
		if is_ipv4 "$ip"; then
			printf '%s' "$ip"
			return 0
		fi
	done
	return 1
}

# The IPv4 addresses a name resolves to, one per line.
resolve_ipv4() {
	local out=""
	if command -v getent >/dev/null 2>&1; then
		out=$(getent ahostsv4 "$1" 2>/dev/null | awk '{print $1}') || true
	elif command -v dig >/dev/null 2>&1; then
		out=$(dig +short A "$1" 2>/dev/null) || true
	elif command -v host >/dev/null 2>&1; then
		out=$(host -t A "$1" 2>/dev/null | awk '/has address/{print $NF}') || true
	fi
	printf '%s\n' "$out" | while IFS= read -r ip; do
		if is_ipv4 "$ip"; then printf '%s\n' "$ip"; fi
	done | sort -u
}

port_busy() {
	local out
	if command -v ss >/dev/null 2>&1; then
		out=$(ss -Hltn "sport = :$1" 2>/dev/null) || out=$(ss -ltn 2>/dev/null | awk -v p=":$1" 'NR>1 && $4 ~ p"$"')
	elif command -v lsof >/dev/null 2>&1; then
		out=$(lsof -nP -iTCP:"$1" -sTCP:LISTEN 2>/dev/null) || true
	elif command -v netstat >/dev/null 2>&1; then
		out=$(netstat -an 2>/dev/null | awk -v p="[.:]$1\$" '/LISTEN/ && $4 ~ p') || true
	else
		return 1
	fi
	[ -n "$out" ]
}

# What's listening on a port: "nginx", "caddy", "docker-proxy", ...
port_owner() {
	local name="" out
	if command -v ss >/dev/null 2>&1; then
		out=$(as_root ss -Hltnp "sport = :$1" 2>/dev/null) || out=""
		name=$(printf '%s\n' "$out" | sed -n 's/.*users:(("\([^"]*\)".*/\1/p' | head -n 1)
	fi
	if [ -z "$name" ] && command -v lsof >/dev/null 2>&1; then
		name=$(as_root lsof -nP -iTCP:"$1" -sTCP:LISTEN 2>/dev/null | awk 'NR==2{print $1}') || name=""
	fi
	printf '%s' "${name:-another program}"
}

# The installed app's own port is free for it: it's restarted in place.
app_port_taken() {
	[ "$1" = "${INSTALLED_PORT:-}" ] && return 1
	port_busy "$1"
}

free_port_from() {
	local port=$1
	while app_port_taken "$port"; do port=$((port + 1)); done
	printf '%s' "$port"
}

new_setup_code() {
	local hex
	hex=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
	printf '%s-%s-%s-%s' "${hex:0:4}" "${hex:4:4}" "${hex:8:4}" "${hex:12:4}"
}

# Reads KEY=value from an env file without running it.
env_get() {
	as_root test -f "$2" || return 0
	as_root sed -n "s/^$1=//p" "$2" | tail -n 1
}

# Runs a command quietly, showing its output only if it fails.
quietly() {
	local out
	if ! out=$("$@" 2>&1); then
		printf '%s\n' "$out" | tail -n 20 >&2
		die "This failed: $*"
	fi
}

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		shasum -a 256 "$1" | awk '{print $1}'
	fi
}

# ------------------------------------------------------------ options

usage() {
	cat <<EOF
LocalLeads setup: installs LocalLeads and starts it.

  curl -fsSL $SCRIPT_URL | bash
  curl -fsSL $SCRIPT_URL | bash -s -- [options]

With no options it asks a few questions. Run it again to update, change
settings or uninstall.

Where it's served (pick one):
  --domain NAME     Your own domain, with HTTPS from Let's Encrypt (Linux).
                    Point its DNS A record at this server first.
  --ip [ADDRESS]    This server's IP address, with HTTPS from Let's Encrypt
                    for the IP itself (a self-signed certificate if it can't
                    get one) (Linux). Default on Linux.
  --local           Only this machine, at http://localhost:PORT.
                    Default on macOS.
  --proxy URL       Behind your own web server (nginx, Traefik, ...): URL
                    is the address people open; LocalLeads listens on
                    127.0.0.1:PORT and you get a config to paste in.

Other options:
  --port N          Port LocalLeads listens on, on 127.0.0.1 (default 7878).
  --edge            Install the newest build of main instead of the latest
                    release.
  --version vX.Y.Z  Install that release.
  --binary PATH     Install this localleads binary instead of downloading.
  --no-firewall     Don't open the HTTPS ports in ufw or firewalld.
  -y, --yes         Don't ask; use the defaults and the options given.
  --update          Update an existing install to the latest release.
  --uninstall       Remove LocalLeads. Its data is kept unless you also pass
                    --delete-data. Caddy stays installed.
  -h, --help        Show this help.
EOF
}

MODE=""
DOMAIN=""
SERVER_IP=""
IP_CHALLENGES=none
IP_TRUSTED=0
PROXY_URL=""
PORT=""
HTTP_PORT=""
HTTPS_PORT=""
CHANNEL=""
VERSION=""
BINARY="${LOCALLEADS_BINARY:-}"
FIREWALL=1
ASSUME_YES=0
ACTION=""
DELETE_DATA=0
MODE_FROM_FLAGS=""
SETUP_CODE=""
INSTALLED_PORT=""
PROXY_SOFTWARE=""

need_value() {
	[ $# -ge 2 ] && [ -n "$2" ] && [ "${2#-}" = "$2" ] || die "$1 needs a value." "Run with --help to see the options."
}

parse_args() {
	while [ $# -gt 0 ]; do
		case $1 in
		--domain) need_value "$@"; MODE=domain; DOMAIN=$(bare_host "$2"); shift ;;
		--domain=*) MODE=domain; DOMAIN=$(bare_host "${1#*=}") ;;
		--ip)
			MODE=ip
			if [ $# -ge 2 ] && is_ipv4 "$2"; then SERVER_IP=$2; shift; fi
			;;
		--ip=*) MODE=ip; SERVER_IP=${1#*=} ;;
		--local) MODE=local ;;
		--proxy) need_value "$@"; MODE=proxy; PROXY_URL=$2; shift ;;
		--proxy=*) MODE=proxy; PROXY_URL=${1#*=} ;;
		--port) need_value "$@"; PORT=$2; shift ;;
		--port=*) PORT=${1#*=} ;;
		--edge) CHANNEL=edge ;;
		--version) need_value "$@"; CHANNEL=version; VERSION=$2; shift ;;
		--version=*) CHANNEL=version; VERSION=${1#*=} ;;
		--binary) need_value "$@"; BINARY=$2; shift ;;
		--binary=*) BINARY=${1#*=} ;;
		--no-firewall) FIREWALL=0 ;;
		-y | --yes) ASSUME_YES=1 ;;
		--update) ACTION=update ;;
		--uninstall) ACTION=uninstall ;;
		--delete-data) DELETE_DATA=1 ;;
		-h | --help) usage; exit 0 ;;
		*) die "Unknown option: $1" "Run with --help to see the options." ;;
		esac
		shift
	done
	if [ -n "$SERVER_IP" ] && ! is_ipv4 "$SERVER_IP"; then die "--ip needs an IPv4 address like 203.0.113.7."; fi
	if [ "$MODE" = domain ] && ! is_domain "$DOMAIN"; then die "\"$DOMAIN\" doesn't look like a domain name."; fi
	if [ -n "$PORT" ] && ! is_port "$PORT"; then die "\"$PORT\" isn't a port number."; fi
	case $VERSION in "" | v*) ;; *) VERSION="v$VERSION" ;; esac
	if [ -n "$BINARY" ] && [ ! -f "$BINARY" ]; then die "--binary: $BINARY doesn't exist."; fi
	return 0
}

# ------------------------------------------------------------ system

TARGET=""
BIN=""
CONF_DIR=""
ENV_FILE=""
DATA_DIR=""
UNIT=""
PLIST=""
LOG_FILE=""

check_system() {
	command -v curl >/dev/null 2>&1 || die "curl is missing." "Install it with your package manager, then run this again."
	local arch
	arch=$(uname -m)
	case $OS in
	Linux)
		case $arch in
		x86_64 | amd64) TARGET=x86_64-unknown-linux-gnu ;;
		aarch64 | arm64) TARGET=aarch64-unknown-linux-gnu ;;
		*) die "There's no LocalLeads build for $arch yet." "Builds exist for x86_64 and ARM64. You can build from source: see github.com/$REPO." ;;
		esac
		check_glibc
		if [ ! -d /run/systemd/system ]; then
			die "LocalLeads runs as a systemd service, and systemd isn't running here." \
				"In a container or without systemd, use Docker instead: see" \
				"https://github.com/$REPO/blob/main/docs/SELF_HOSTING.md"
		fi
		if [ "$(id -u)" != 0 ]; then
			command -v sudo >/dev/null 2>&1 || die "Run this as root; it installs a service." "  curl -fsSL $SCRIPT_URL | su -c bash"
			SUDO=sudo
			if ! sudo -n true 2>/dev/null; then
				[ "$INTERACTIVE" = 1 ] || die "This needs root. Run it with sudo:" "  curl -fsSL $SCRIPT_URL | sudo bash -s -- --yes"
				note "Some steps need root, so sudo asks for your password."
				sudo -v <&3 || die "sudo didn't work." "Run this as root instead."
			fi
		fi
		BIN=/usr/local/bin/localleads
		CONF_DIR=/etc/localleads
		ENV_FILE=$CONF_DIR/localleads.env
		DATA_DIR=/var/lib/localleads
		UNIT=/etc/systemd/system/$SERVICE.service
		;;
	Darwin)
		case $arch in
		arm64) TARGET=aarch64-apple-darwin ;;
		x86_64)
			# An Intel shell on Apple silicon (Rosetta) still gets the native build.
			if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
				TARGET=aarch64-apple-darwin
			else
				TARGET=x86_64-apple-darwin
			fi
			;;
		*) die "There's no LocalLeads build for $arch." ;;
		esac
		CONF_DIR="$HOME/.localleads"
		BIN=$CONF_DIR/bin/localleads
		ENV_FILE=$CONF_DIR/localleads.env
		DATA_DIR=$CONF_DIR/data
		PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
		LOG_FILE=$CONF_DIR/localleads.log
		;;
	*)
		die "This script runs on Linux and macOS." \
			"On Windows, download localleads-x86_64-pc-windows-msvc.zip from" \
			"$RELEASES/latest and run localleads.exe --open."
		;;
	esac
	ok "$OS $arch"
}

check_glibc() {
	local out version major minor
	out=$(ldd --version 2>&1 | head -n 1) || out=""
	case $(lower "$out") in
	*musl*)
		die "This system uses musl (Alpine?), and LocalLeads builds need glibc." \
			"Use Docker instead: https://github.com/$REPO/blob/main/docs/SELF_HOSTING.md"
		;;
	esac
	version=$(printf '%s' "$out" | grep -oE '[0-9]+\.[0-9]+$' || true)
	[ -n "$version" ] || return 0
	major=${version%%.*} minor=${version#*.}
	if [ "$major" -lt 2 ] || { [ "$major" = 2 ] && [ "$minor" -lt 28 ]; }; then
		die "This system has glibc $version; LocalLeads needs 2.28 or newer." \
			"That's Ubuntu 20.04+, Debian 10+, RHEL/Rocky/Alma 8+ or newer."
	fi
}

# ------------------------------------------------------------ existing install

is_installed() { as_root test -f "$ENV_FILE" && [ -n "$(env_get LOCALLEADS_MODE "$ENV_FILE")" ]; }

load_install() {
	[ -n "$MODE" ] || MODE=$(env_get LOCALLEADS_MODE "$ENV_FILE")
	[ -n "$DOMAIN" ] || DOMAIN=$(env_get DOMAIN "$ENV_FILE")
	[ -n "$SERVER_IP" ] || SERVER_IP=$(env_get SERVER_IP "$ENV_FILE")
	[ -n "$HTTP_PORT" ] || HTTP_PORT=$(env_get HTTP_PORT "$ENV_FILE")
	[ -n "$HTTPS_PORT" ] || HTTPS_PORT=$(env_get HTTPS_PORT "$ENV_FILE")
	[ -n "$CHANNEL" ] || CHANNEL=$(env_get LOCALLEADS_CHANNEL "$ENV_FILE")
	if [ -z "$PROXY_URL" ] && [ "$MODE" = proxy ]; then PROXY_URL=$(env_get LOCALLEADS_PUBLIC_URL "$ENV_FILE"); fi
	local bind
	bind=$(env_get LOCALLEADS_BIND "$ENV_FILE")
	INSTALLED_PORT=${bind##*:}
	[ -n "$PORT" ] || PORT=$INSTALLED_PORT
	SETUP_CODE=$(env_get LOCALLEADS_SETUP_CODE "$ENV_FILE")
}

service_running() {
	if [ "$OS" = Linux ]; then
		systemctl is-active --quiet "$SERVICE" 2>/dev/null
	else
		launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1
	fi
}

existing_menu() {
	local version
	version=$("$BIN" --version 2>/dev/null | awk '{print $2}') || version=""
	step "LocalLeads ${version:+$version }is already installed"
	if service_running; then ok "It's running"; else note "It's stopped"; fi
	if [ -n "$ACTION" ]; then return; fi
	if [ -n "$MODE_FROM_FLAGS" ]; then
		ACTION=reconfigure
		return
	fi
	if [ "$INTERACTIVE" != 1 ]; then
		ACTION=update
		return
	fi
	choose 1 "Update to the latest version (keeps your data and settings)" \
		"Change settings (address, domain, port)" \
		"Uninstall" \
		"Quit"
	case $ANSWER in
	1) ACTION=update ;;
	2) ACTION=reconfigure ;;
	3) ACTION=uninstall ;;
	*) exit 0 ;;
	esac
}

# ------------------------------------------------------------ wizard

configure() {
	if [ -z "$MODE" ]; then
		step "Where will you open LocalLeads?"
		if [ "$OS" = Darwin ]; then
			choose 1 \
				"Only on this computer ${DIM}(http://localhost)${RESET}" \
				"Behind my own web server ${DIM}(nginx, Caddy, Cloudflare Tunnel, ...)${RESET}"
			case $ANSWER in
			1) MODE=local ;;
			2) MODE=proxy ;;
			esac
		else
			choose 1 \
				"From anywhere, at this server's IP address ${DIM}(HTTPS from Let's Encrypt, no domain needed; quickest)${RESET}" \
				"From anywhere, at my own domain ${DIM}(HTTPS from Let's Encrypt; needs a DNS record)${RESET}" \
				"Only on this machine ${DIM}(http://localhost; reach it over an SSH tunnel)${RESET}" \
				"Behind my own web server ${DIM}(nginx, Traefik, Cloudflare Tunnel, ...)${RESET}"
			case $ANSWER in
			1) MODE=ip ;;
			2) MODE=domain ;;
			3) MODE=local ;;
			4) MODE=proxy ;;
			esac
		fi
	fi
	if [ "$OS" = Darwin ] && uses_caddy; then
		die "Serving at a domain or IP address with HTTPS is for Linux servers." \
			"On a Mac, use --local, or --proxy behind your own web server."
	fi
	case $MODE in
	domain) configure_domain ;;
	ip) configure_ip ;;
	local) configure_port ;;
	proxy) configure_proxy ;;
	*) die "Unknown mode \"$MODE\" in $ENV_FILE." "Run with --local, --ip, --domain or --proxy to choose again." ;;
	esac
	if uses_caddy; then
		configure_port
		check_web_ports
	fi
}

configure_domain() {
	while ! is_domain "$DOMAIN"; do
		[ "$INTERACTIVE" = 1 ] || die "--domain is needed (for example --domain leads.example.com)."
		[ -z "$DOMAIN" ] || say "  \"$DOMAIN\" doesn't look like a domain name, like leads.example.com."
		ask "Domain name (like leads.example.com)" "$DOMAIN"
		DOMAIN=$(bare_host "$ANSWER")
	done
	HTTP_PORT=80 HTTPS_PORT=443
	local ip addresses
	ip=$(public_ipv4) || ip=""
	SERVER_IP=$ip
	addresses=$(resolve_ipv4 "$DOMAIN")
	if [ -z "$addresses" ]; then
		warn "$DOMAIN doesn't resolve yet."
		note "Add an A record at your DNS provider: $DOMAIN → ${ip:-the server IP}"
		note "Caddy keeps retrying, so HTTPS starts working once DNS has updated."
		confirm "Continue anyway?" y || exit 1
	elif [ -n "$ip" ] && ! printf '%s\n' "$addresses" | grep -qx "$ip"; then
		warn "$DOMAIN points to $(printf '%s' "$addresses" | tr '\n' ' ')but this server is $ip."
		note "That's fine behind Cloudflare's proxy (orange cloud); otherwise fix the A record."
		confirm "Continue anyway?" y || exit 1
	else
		ok "$DOMAIN points to this server${ip:+ ($ip)}"
	fi
}

configure_ip() {
	if [ -z "$SERVER_IP" ]; then
		SERVER_IP=$(public_ipv4) || SERVER_IP=""
	fi
	if [ "$INTERACTIVE" = 1 ]; then
		while :; do
			ask "This server's IP address" "$SERVER_IP"
			if is_ipv4 "$ANSWER"; then SERVER_IP=$ANSWER; break; fi
			say "  Type an IPv4 address, like 203.0.113.7."
		done
	elif [ -z "$SERVER_IP" ]; then
		die "Couldn't find this server's public IP address." "Pass it with --ip 203.0.113.7."
	fi
	[ -n "$HTTP_PORT" ] || HTTP_PORT=80
	[ -n "$HTTPS_PORT" ] || HTTPS_PORT=443
}

# The port LocalLeads itself listens on, on 127.0.0.1.
configure_port() {
	local default=${PORT:-7878}
	if app_port_taken "$default"; then
		local free
		free=$(free_port_from "$default")
		note "Port $default is in use, so LocalLeads will use $free."
		default=$free
	fi
	# Behind Caddy the port is internal; only ask when people use it.
	if [ "$INTERACTIVE" = 1 ] && ! uses_caddy; then
		while :; do
			ask "Port" "$default"
			if is_port "$ANSWER" && ! app_port_taken "$ANSWER"; then PORT=$ANSWER; break; fi
			if is_port "$ANSWER"; then say "  Port $ANSWER is in use."; else say "  Type a port number."; fi
		done
	else
		PORT=$default
	fi
}

configure_proxy() {
	while :; do
		if [ "$INTERACTIVE" = 1 ]; then
			ask "Address people open (like https://leads.example.com)" "$PROXY_URL"
			PROXY_URL=$ANSWER
		elif [ -z "$PROXY_URL" ]; then
			die "--proxy needs the address people open, like https://leads.example.com."
		fi
		case $PROXY_URL in
		http://* | https://*) PROXY_URL=${PROXY_URL%/}; break ;;
		*.*) PROXY_URL="https://${PROXY_URL%/}"; break ;;
		esac
		[ "$INTERACTIVE" = 1 ] || die "--proxy needs a URL like https://leads.example.com."
		say "  That should look like https://leads.example.com."
		PROXY_URL=""
	done
	configure_port
}

uses_caddy() { [ "$MODE" = domain ] || [ "$MODE" = ip ]; }

public_url() {
	local port_part=""
	if [ -n "$HTTPS_PORT" ] && [ "$HTTPS_PORT" != 443 ]; then port_part=":$HTTPS_PORT"; fi
	case $MODE in
	domain) printf 'https://%s%s' "$DOMAIN" "$port_part" ;;
	ip) printf 'https://%s%s' "$SERVER_IP" "$port_part" ;;
	local) printf 'http://localhost:%s' "$PORT" ;;
	proxy) printf '%s' "$PROXY_URL" ;;
	esac
}

# Whether Caddy serves only LocalLeads, so setup.sh may write its whole
# Caddyfile, global options (ports, default certificate) included.
caddy_is_ours() {
	! as_root test -f "$CADDYFILE" ||
		[ "$(as_root head -n 1 "$CADDYFILE")" = "$CADDY_OWNED" ] ||
		caddyfile_is_stock
}

# The Caddyfile the caddy package ships: a welcome page on :80.
caddyfile_is_stock() {
	as_root grep -q 'root \* /usr/share/caddy' "$CADDYFILE" 2>/dev/null &&
		[ "$(as_root grep -cvE '^[[:space:]]*(#|$)' "$CADDYFILE")" -le 6 ]
}

# Ports 80 and 443 must be free, or Caddy's.
check_web_ports() {
	local http_owner="" https_owner=""
	if port_busy "$HTTP_PORT"; then http_owner=$(port_owner "$HTTP_PORT"); fi
	if port_busy "$HTTPS_PORT"; then https_owner=$(port_owner "$HTTPS_PORT"); fi
	case $http_owner in caddy) http_owner="" ;; esac
	case $https_owner in caddy) https_owner="" ;; esac
	if [ -z "$http_owner" ] && [ -z "$https_owner" ]; then return; fi

	step "Ports in use"
	[ -z "$http_owner" ] || warn "Port $HTTP_PORT is used by $http_owner."
	[ -z "$https_owner" ] || warn "Port $HTTPS_PORT is used by $https_owner."
	local owner
	for owner in "$http_owner" "$https_owner"; do
		case $(lower "$owner") in
		*nginx*) PROXY_SOFTWARE=nginx ;;
		*apache* | *httpd*) PROXY_SOFTWARE=apache ;;
		esac
	done

	# An IP address can use any port, as long as Caddy serves only
	# LocalLeads: its certificate comes through Coolify's proxy when that's
	# what holds 443 (see ip_challenges), else Caddy signs its own. A domain
	# needs 80 or 443 for Let's Encrypt.
	local other_ports=0 new_http=$HTTP_PORT new_https=$HTTPS_PORT
	if [ "$MODE" = ip ] && caddy_is_ours; then
		other_ports=1
		if [ -n "$http_owner" ]; then
			new_http=8080
			while port_busy "$new_http"; do new_http=$((new_http + 1)); done
		fi
		if [ -n "$https_owner" ]; then
			new_https=8443
			while port_busy "$new_https" || [ "$new_https" = "$new_http" ]; do new_https=$((new_https + 1)); done
		fi
	fi

	if [ "$INTERACTIVE" != 1 ]; then
		if [ "$other_ports" = 1 ]; then
			HTTP_PORT=$new_http HTTPS_PORT=$new_https
			note "Using HTTP port $HTTP_PORT and HTTPS port $HTTPS_PORT instead."
			return
		fi
		die "Caddy needs ports 80 and 443 for HTTPS, and they're taken." \
			"Free them, or keep your web server in front and run:" \
			"  curl -fsSL $SCRIPT_URL | bash -s -- --proxy https://leads.example.com"
	fi

	if [ "$other_ports" = 1 ]; then
		choose 1 \
			"Use other ports: https://$SERVER_IP:$new_https" \
			"Put LocalLeads behind the web server that's already running" \
			"Quit, so I can free the ports first"
	else
		if [ "$MODE" = domain ]; then say "  Let's Encrypt checks your domain on ports 80 and 443."; fi
		choose 1 \
			"Put LocalLeads behind the web server that's already running" \
			"Quit, so I can free the ports first"
		ANSWER=$((ANSWER + 1))
	fi
	case $ANSWER in
	1) HTTP_PORT=$new_http HTTPS_PORT=$new_https ;;
	2)
		if [ "$MODE" = domain ]; then PROXY_URL="https://$DOMAIN"; else PROXY_URL=""; fi
		MODE=proxy HTTP_PORT="" HTTPS_PORT=""
		configure_proxy
		;;
	*) exit 0 ;;
	esac
}

summary() {
	step "Ready to install"
	printf '  %-10s %s\n' "Address" "$(public_url)"
	case $MODE in
	domain) printf '  %-10s %s\n' "HTTPS" "Let's Encrypt certificate, renewed by Caddy" ;;
	ip) printf '  %-10s %s\n' "HTTPS" "Let's Encrypt certificate for the IP (self-signed if it can't get one)" ;;
	local) printf '  %-10s %s\n' "Reachable" "Only from this machine" ;;
	proxy) printf '  %-10s %s\n' "Proxy to" "http://127.0.0.1:$PORT" ;;
	esac
	if uses_caddy && ! command -v caddy >/dev/null 2>&1; then
		printf '  %-10s %s\n' "Caddy" "Not installed yet; it'll be installed"
	fi
	printf '  %-10s %s\n' "Version" "$(channel_label)"
	printf '  %-10s %s\n' "Data" "$DATA_DIR"
	say ""
	confirm "Install now?" y || exit 0
}

channel_label() {
	if [ -n "$BINARY" ]; then printf '%s' "$BINARY"; return; fi
	case ${CHANNEL:-stable} in
	edge) printf 'edge (newest build of main)' ;;
	version) printf '%s' "$VERSION" ;;
	*) printf 'latest release' ;;
	esac
}

# ------------------------------------------------------------ download

install_binary() {
	step "Getting LocalLeads"
	local tmp
	tmp=$(mktemp -d)
	if [ -n "$BINARY" ]; then
		cp "$BINARY" "$tmp/localleads"
		ok "Using $BINARY"
	else
		download_release "$tmp"
	fi
	chmod 755 "$tmp/localleads"
	local version
	if ! version=$("$tmp/localleads" --version 2>&1); then
		rm -rf "$tmp"
		die "The downloaded program doesn't run on this machine." "$version"
	fi
	if [ "$OS" = Linux ]; then
		as_root install -m 755 "$tmp/localleads" "$BIN"
	else
		mkdir -p "$(dirname "$BIN")"
		install -m 755 "$tmp/localleads" "$BIN"
	fi
	rm -rf "$tmp"
	ok "Installed $version at $BIN"
}

download_release() {
	local dir=$1 asset="localleads-$TARGET.tar.gz" base
	case ${CHANNEL:-stable} in
	edge) base="$RELEASES/download/edge" ;;
	version) base="$RELEASES/download/$VERSION" ;;
	*) base="$RELEASES/latest/download" ;;
	esac
	if ! curl -fsSL --retry 3 -o "$dir/$asset" "$base/$asset" 2>/dev/null; then
		if [ "${CHANNEL:-stable}" = stable ]; then
			# Before the first release, main's newest build is all there is.
			# Updates still look for a release first.
			note "No release yet, so using the newest build of main."
			base="$RELEASES/download/edge"
			curl -fsSL --retry 3 -o "$dir/$asset" "$base/$asset" ||
				die "Couldn't download $asset." "Check $RELEASES for a build for $TARGET."
		else
			die "Couldn't download $base/$asset." "Check $RELEASES for that version."
		fi
	fi
	if curl -fsSL --retry 3 -o "$dir/SHA256SUMS" "$base/SHA256SUMS" 2>/dev/null; then
		local expected actual
		expected=$(awk -v a="$asset" '$2 == a || $2 == "*"a {print $1}' "$dir/SHA256SUMS")
		actual=$(sha256 "$dir/$asset")
		if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
			die "The download's checksum doesn't match SHA256SUMS." \
				"Try again; if it keeps happening, report it at github.com/$REPO/issues."
		fi
		ok "Downloaded $asset (checksum verified)"
	else
		warn "No SHA256SUMS in this release; skipped the checksum check."
	fi
	tar -xzf "$dir/$asset" -C "$dir"
	mv "$dir/localleads-$TARGET/localleads" "$dir/localleads"
}

# ------------------------------------------------------------ service

write_env() {
	[ -n "$SETUP_CODE" ] || SETUP_CODE=$(new_setup_code)
	local behind_proxy=false
	if [ "$MODE" != local ]; then behind_proxy=true; fi
	local content
	content="# Written by setup.sh. Run it again to change these:
#   curl -fsSL $SCRIPT_URL | bash
LOCALLEADS_BIND=127.0.0.1:$PORT
LOCALLEADS_DB=$DATA_DIR/localleads.sqlite
LOCALLEADS_PUBLIC_URL=$(public_url)
LOCALLEADS_BEHIND_PROXY=$behind_proxy
# Needed once, to create the account. Unused after that.
LOCALLEADS_SETUP_CODE=$SETUP_CODE
# For setup.sh:
LOCALLEADS_MODE=$MODE
LOCALLEADS_CHANNEL=${CHANNEL:-stable}
DOMAIN=$DOMAIN
SERVER_IP=$SERVER_IP
HTTP_PORT=$HTTP_PORT
HTTPS_PORT=$HTTPS_PORT"
	if [ "$OS" = Linux ]; then
		as_root mkdir -p "$CONF_DIR"
		printf '%s\n' "$content" | as_root tee "$ENV_FILE" >/dev/null
		as_root chown "root:$SERVICE" "$ENV_FILE"
		as_root chmod 640 "$ENV_FILE"
	else
		mkdir -p "$CONF_DIR"
		(umask 077 && printf '%s\n' "$content" >"$ENV_FILE")
	fi
}

install_service() {
	step "Setting up the service"
	if [ "$OS" = Linux ]; then
		install_systemd
	else
		install_launchd
	fi
}

install_systemd() {
	if ! id "$SERVICE" >/dev/null 2>&1; then
		as_root useradd --system --user-group --home-dir "$DATA_DIR" --shell "$(command -v nologin || echo /bin/false)" "$SERVICE"
		ok "Created the $SERVICE system user"
	fi
	as_root mkdir -p "$DATA_DIR"
	as_root chown "$SERVICE:$SERVICE" "$DATA_DIR"
	as_root chmod 750 "$DATA_DIR"
	write_env
	as_root tee "$UNIT" >/dev/null <<EOF
# Written by LocalLeads setup.sh.
[Unit]
Description=LocalLeads
Documentation=https://github.com/$REPO
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE
Group=$SERVICE
EnvironmentFile=$ENV_FILE
ExecStart=$BIN
WorkingDirectory=$DATA_DIR
Restart=on-failure
RestartSec=3
# Running searches carry on where they were after a restart.
TimeoutStopSec=30
LimitNOFILE=65536

NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=$DATA_DIR
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
ProtectClock=true
RestrictSUIDSGID=true
RestrictNamespaces=true
LockPersonality=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX

[Install]
WantedBy=multi-user.target
EOF
	as_root systemctl daemon-reload
	as_root systemctl enable "$SERVICE" >/dev/null 2>&1
	as_root systemctl restart "$SERVICE"
	ok "Started the $SERVICE service (starts on boot)"
}

install_launchd() {
	mkdir -p "$DATA_DIR" "$(dirname "$PLIST")"
	write_env
	local vars="" line key value
	while IFS= read -r line; do
		case $line in
		LOCALLEADS_BIND=* | LOCALLEADS_DB=* | LOCALLEADS_PUBLIC_URL=* | LOCALLEADS_BEHIND_PROXY=* | LOCALLEADS_SETUP_CODE=*) ;;
		*) continue ;;
		esac
		key=${line%%=*} value=${line#*=}
		vars="$vars		<key>$key</key><string>$value</string>
"
	done <"$ENV_FILE"
	cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by LocalLeads setup.sh. -->
<plist version="1.0">
<dict>
	<key>Label</key><string>$LABEL</string>
	<key>ProgramArguments</key><array><string>$BIN</string></array>
	<key>EnvironmentVariables</key>
	<dict>
$vars	</dict>
	<key>WorkingDirectory</key><string>$DATA_DIR</string>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><true/>
	<key>StandardOutPath</key><string>$LOG_FILE</string>
	<key>StandardErrorPath</key><string>$LOG_FILE</string>
	<key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
</dict>
</plist>
EOF
	stop_launchd
	# Loading can fail for a moment after unloading; try a few times.
	local i out=""
	for i in 1 2 3 4 5; do
		if out=$(launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>&1); then
			ok "Started LocalLeads (starts when you log in)"
			return
		fi
		sleep 1
	done
	die "Couldn't start LocalLeads with launchd." "$out"
}

# Unloads the agent and waits until it's gone (bootout returns early).
stop_launchd() {
	launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || return 0
	local i
	for i in $(seq 1 20); do
		launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || return 0
		sleep 0.5
	done
}

wait_healthy() {
	local i
	for i in $(seq 1 60); do
		if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
			ok "LocalLeads is up on 127.0.0.1:$PORT"
			return
		fi
		sleep 1
	done
	say ""
	if [ "$OS" = Linux ]; then
		as_root journalctl -u "$SERVICE" -n 40 --no-pager >&2 || true
	else
		tail -n 40 "$LOG_FILE" >&2 || true
	fi
	die "LocalLeads isn't answering on 127.0.0.1:$PORT." "Its log is above."
}

# ------------------------------------------------------------ Caddy

ensure_caddy() {
	if command -v caddy >/dev/null 2>&1; then
		ok "Caddy $(caddy version 2>/dev/null | awk '{print $1}') is installed"
		return
	fi
	step "Installing Caddy"
	if command -v apt-get >/dev/null 2>&1; then
		note "From Caddy's official apt repository"
		quietly as_root apt-get update -q
		quietly as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -q debian-keyring debian-archive-keyring apt-transport-https curl gnupg
		curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/gpg.key |
			as_root gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
		curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt |
			as_root tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
		as_root chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg /etc/apt/sources.list.d/caddy-stable.list
		quietly as_root apt-get update -q
		quietly as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -q caddy
	elif command -v dnf >/dev/null 2>&1; then
		note "From Caddy's official COPR repository"
		quietly as_root dnf install -y 'dnf-command(copr)'
		quietly as_root dnf copr enable -y @caddy/caddy
		quietly as_root dnf install -y caddy
	elif command -v pacman >/dev/null 2>&1; then
		quietly as_root pacman -Sy --noconfirm --needed caddy
	elif command -v zypper >/dev/null 2>&1; then
		quietly as_root zypper --non-interactive install caddy
	else
		install_caddy_binary
	fi
	command -v caddy >/dev/null 2>&1 || die "Caddy didn't install." "Install it yourself (https://caddyserver.com/docs/install), then run this again."
	ok "Installed Caddy $(caddy version 2>/dev/null | awk '{print $1}')"
}

# No supported package manager: Caddy's official static build and the
# systemd unit from its docs.
install_caddy_binary() {
	note "From Caddy's GitHub releases"
	local arch version tmp
	case $TARGET in x86_64*) arch=amd64 ;; *) arch=arm64 ;; esac
	version=$(curl -fsSL https://api.github.com/repos/caddyserver/caddy/releases/latest | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p' | head -n 1)
	[ -n "$version" ] || die "Couldn't find Caddy's latest version." "Install Caddy yourself (https://caddyserver.com/docs/install), then run this again."
	tmp=$(mktemp -d)
	curl -fsSL "https://github.com/caddyserver/caddy/releases/download/v$version/caddy_${version}_linux_$arch.tar.gz" | tar -xz -C "$tmp" caddy
	as_root install -m 755 "$tmp/caddy" /usr/bin/caddy
	rm -rf "$tmp"
	if ! id caddy >/dev/null 2>&1; then
		as_root groupadd --system caddy
		as_root useradd --system --gid caddy --create-home --home-dir /var/lib/caddy --shell "$(command -v nologin || echo /bin/false)" caddy
	fi
	as_root mkdir -p /etc/caddy
	as_root test -f "$CADDYFILE" || printf '%s\n' "$CADDY_OWNED" | as_root tee "$CADDYFILE" >/dev/null
	as_root tee /etc/systemd/system/caddy.service >/dev/null <<'EOF'
[Unit]
Description=Caddy
Documentation=https://caddyserver.com/docs/
After=network.target network-online.target
Requires=network-online.target

[Service]
Type=notify
User=caddy
Group=caddy
ExecStart=/usr/bin/caddy run --environ --config /etc/caddy/Caddyfile
ExecReload=/usr/bin/caddy reload --config /etc/caddy/Caddyfile --force
TimeoutStopSec=5s
LimitNOFILE=1048576
PrivateTmp=true
ProtectSystem=full
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
EOF
	as_root systemctl daemon-reload
	as_root systemctl enable caddy >/dev/null 2>&1
}

# Caddy gets Let's Encrypt certificates for IP addresses from 2.11 on.
caddy_ip_certs() {
	local version major minor
	version=$(caddy version 2>/dev/null | awk '{print $1}')
	version=${version#v}
	major=${version%%.*}
	minor=${version#*.}
	minor=${minor%%.*}
	case "$major$minor" in "" | *[!0-9]*) return 1 ;; esac
	[ "$major" -gt 2 ] || { [ "$major" = 2 ] && [ "$minor" -ge 11 ]; }
}

# Coolify's Traefik holds port 443, reads route files from a folder, and
# can reach this machine as host.docker.internal.
coolify_owns_443() {
	as_root test -d "$COOLIFY_DYNAMIC" || return 1
	command -v docker >/dev/null 2>&1 || return 1
	as_root docker ps --format '{{.Names}}' 2>/dev/null | grep -qx coolify-proxy || return 1
	as_root grep -q 'host.docker.internal:host-gateway' "$COOLIFY_DYNAMIC/../docker-compose.yml" 2>/dev/null || return 1
	[ "$(port_owner 443)" = docker-proxy ]
}

# The name Let's Encrypt asks for when it checks an IP address over TLS:
# 203.0.113.7 -> 7.113.0.203.in-addr.arpa
reverse_dns_name() {
	printf '%s' "$1" | awk -F. '{printf "%s.%s.%s.%s.in-addr.arpa", $4, $3, $2, $1}'
}

# How Let's Encrypt can check this server before it certifies its IP
# address: on port 80 (http), on 443 (tls), both, through Coolify's proxy
# on 443 (coolify), or not at all (none: Caddy signs its own).
ip_challenges() {
	IP_CHALLENGES=none
	caddy_ip_certs || return 0
	if [ "$HTTP_PORT" = 80 ] && [ "$HTTPS_PORT" = 443 ]; then
		IP_CHALLENGES=both
	elif [ "$HTTP_PORT" = 80 ]; then
		IP_CHALLENGES=http
	elif [ "$HTTPS_PORT" = 443 ]; then
		IP_CHALLENGES=tls
	elif coolify_owns_443; then
		IP_CHALLENGES=coolify
	fi
}

# Coolify's Traefik passes Let's Encrypt's TLS check for this IP straight
# through to Caddy, unopened. Nothing else is routed to LocalLeads.
write_coolify_route() {
	as_root tee "$COOLIFY_ROUTE" >/dev/null <<EOF
# LocalLeads, written by setup.sh; it's rewritten when you run it again.
# Caddy (port $HTTPS_PORT) gets a Let's Encrypt certificate for $SERVER_IP.
# Traefik owns port 443, so it passes the TLS-ALPN check for that IP (asked
# for by its reverse-DNS name) through to Caddy untouched.
tcp:
  routers:
    localleads-acme-tls:
      rule: "HostSNI(\`$(reverse_dns_name "$SERVER_IP")\`)"
      entryPoints:
        - https
      service: localleads-caddy-tls
      tls:
        passthrough: true
  services:
    localleads-caddy-tls:
      loadBalancer:
        servers:
          - address: "host.docker.internal:$HTTPS_PORT"
EOF
	note "Coolify's proxy passes Let's Encrypt's check on to Caddy ($COOLIFY_ROUTE)"
	# Traefik picks the file up within a second or so.
	sleep 2
}

configure_caddy() {
	step "Setting up HTTPS"
	local site tls="" disable=""
	IP_CHALLENGES=none
	if [ "$MODE" = domain ]; then
		site="https://$DOMAIN"
	else
		site="https://$SERVER_IP"
		ip_challenges
		case $IP_CHALLENGES in
		http) disable="
			disable_tlsalpn_challenge" ;;
		tls | coolify) disable="
			disable_http_challenge" ;;
		esac
		if [ "$IP_CHALLENGES" = none ]; then
			tls="	tls internal

"
		else
			# Let's Encrypt's 6-day certificates cover IP addresses; Caddy
			# renews them. If it can't get one, it signs its own.
			tls="	tls {
		issuer acme {
			profile shortlived$disable
		}
		issuer internal
	}

"
		fi
	fi
	if [ "$IP_CHALLENGES" = coolify ]; then
		write_coolify_route
	elif as_root test -f "$COOLIFY_ROUTE"; then
		as_root rm -f "$COOLIFY_ROUTE"
	fi
	local redirect=""
	if [ "$HTTPS_PORT" != 443 ]; then
		site="$site:$HTTPS_PORT"
		# Caddy's own redirect would drop the port; send plain HTTP to it.
		redirect="http:// {
	redir https://{host}:$HTTPS_PORT{uri} permanent
}

"
	fi
	as_root tee "$CADDY_SITE" >/dev/null <<EOF
# LocalLeads, written by setup.sh; it's rewritten when you run it again.
${redirect}$site {
${tls}	# Live updates (/events) and MCP (/mcp) stream; don't compress them.
	@compress not path /events /mcp
	encode @compress zstd gzip

	reverse_proxy 127.0.0.1:$PORT

	# LocalLeads sends HSTS and its other security headers itself.
	header -Server
}
EOF

	local backup="" ours=0
	if caddy_is_ours; then ours=1; fi
	if as_root test -f "$CADDYFILE" && [ "$(as_root head -n 1 "$CADDYFILE")" != "$CADDY_OWNED" ]; then
		backup="$CADDYFILE.before-localleads"
		as_root cp "$CADDYFILE" "$backup"
	fi
	if [ "$ours" = 1 ]; then
		# Caddy serves only LocalLeads: write its Caddyfile, with the global
		# options LocalLeads needs.
		local options=""
		if [ "$MODE" = ip ]; then
			# Browsers send no server name for an IP address, so say which
			# certificate to answer with.
			options="	default_sni $SERVER_IP
"
		fi
		if [ "$HTTP_PORT" != 80 ]; then options="$options	http_port $HTTP_PORT
"; fi
		if [ "$HTTPS_PORT" != 443 ]; then options="$options	https_port $HTTPS_PORT
	auto_https disable_redirects
"; fi
		{
			printf '%s\n' "$CADDY_OWNED"
			printf '# LocalLeads is in %s. You can add your own sites below.\n' "$CADDY_SITE"
			if [ -n "$options" ]; then printf '\n{\n%s}\n' "$options"; fi
			printf '\nimport %s\n' "$CADDY_SITE"
		} | as_root tee "$CADDYFILE" >/dev/null
	elif ! as_root grep -q "^import $CADDY_SITE\$" "$CADDYFILE"; then
		# Your own Caddyfile: add LocalLeads next to your sites.
		printf '\n%s\nimport %s\n' "$CADDY_ADDED" "$CADDY_SITE" | as_root tee -a "$CADDYFILE" >/dev/null
		note "Added LocalLeads to your Caddyfile (backup: $backup)"
	fi

	local check
	if ! check=$(as_root caddy validate --adapter caddyfile --config "$CADDYFILE" 2>&1); then
		if [ -n "$backup" ]; then as_root cp "$backup" "$CADDYFILE"; fi
		as_root rm -f "$CADDY_SITE"
		printf '%s\n' "$check" | tail -n 15 >&2
		die "Caddy didn't accept the config." "Your Caddyfile is back as it was. The error is above."
	fi
	as_root systemctl enable caddy >/dev/null 2>&1 || true
	if systemctl is-active --quiet caddy; then
		as_root systemctl reload caddy || as_root systemctl restart caddy
	else
		as_root systemctl restart caddy
	fi
	ok "Caddy serves $(public_url)"
	open_firewall
	wait_https
}

open_firewall() {
	[ "$FIREWALL" = 1 ] || return 0
	local port
	if command -v ufw >/dev/null 2>&1 && as_root ufw status 2>/dev/null | grep -q '^Status: active'; then
		for port in "$HTTP_PORT" "$HTTPS_PORT"; do as_root ufw allow "$port/tcp" >/dev/null; done
		as_root ufw allow "$HTTPS_PORT/udp" >/dev/null
		ok "Opened ports $HTTP_PORT and $HTTPS_PORT in ufw"
	elif command -v firewall-cmd >/dev/null 2>&1 && as_root firewall-cmd --state >/dev/null 2>&1; then
		for port in "$HTTP_PORT" "$HTTPS_PORT"; do as_root firewall-cmd --permanent --add-port="$port/tcp" >/dev/null; done
		as_root firewall-cmd --permanent --add-port="$HTTPS_PORT/udp" >/dev/null
		as_root firewall-cmd --reload >/dev/null
		ok "Opened ports $HTTP_PORT and $HTTPS_PORT in firewalld"
	fi
}

wait_https() {
	note "Waiting for the HTTPS certificate..."
	local i code
	for i in $(seq 1 45); do
		if [ "$MODE" = ip ]; then
			# No server name for an IP address; Caddy answers with its default.
			code=$(curl -ks -o /dev/null -w '%{http_code}' --max-time 5 -H "Host: $SERVER_IP" "https://127.0.0.1:$HTTPS_PORT/healthz") || code=""
		else
			code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 --resolve "$DOMAIN:$HTTPS_PORT:127.0.0.1" "https://$DOMAIN:$HTTPS_PORT/healthz") || code=""
		fi
		if [ "$code" = 200 ]; then
			ok "HTTPS works"
			if [ "$MODE" = ip ] && [ "$IP_CHALLENGES" != none ]; then wait_ip_certificate; fi
			return
		fi
		sleep 2
	done
	warn "HTTPS isn't working yet."
	if [ "$MODE" = domain ]; then
		note "Usually DNS hasn't updated yet, or a firewall blocks ports 80 and 443."
		note "Caddy keeps retrying. See what it says with:"
	else
		note "See what Caddy says with:"
	fi
	note "  journalctl -u caddy -n 50 --no-pager"
}

# Waits for Let's Encrypt's certificate for the IP, checked the way a
# browser would (Caddy serves its own until then).
wait_ip_certificate() {
	note "Waiting for Let's Encrypt's certificate for $SERVER_IP..."
	local i
	for i in $(seq 1 30); do
		if curl -s -o /dev/null --max-time 5 \
			--connect-to "$SERVER_IP:$HTTPS_PORT:127.0.0.1:$HTTPS_PORT" \
			"https://$SERVER_IP:$HTTPS_PORT/healthz"; then
			IP_TRUSTED=1
			ok "Trusted certificate from Let's Encrypt; Caddy renews it every few days"
			return
		fi
		sleep 2
	done
	warn "No Let's Encrypt certificate yet, so Caddy serves its own for now."
	note "Caddy keeps trying. See what it says with:"
	note "  journalctl -u caddy -n 50 --no-pager"
}

# Takes LocalLeads out of Caddy, leaving your own sites alone.
remove_from_caddy() {
	as_root rm -f "$COOLIFY_ROUTE" 2>/dev/null || true
	as_root test -f "$CADDY_SITE" || return 0
	as_root rm -f "$CADDY_SITE"
	if [ "$(as_root head -n 1 "$CADDYFILE" 2>/dev/null)" = "$CADDY_OWNED" ]; then
		if as_root test -f "$CADDYFILE.before-localleads"; then
			as_root mv "$CADDYFILE.before-localleads" "$CADDYFILE"
		else
			printf '%s\n' "$CADDY_OWNED" | as_root tee "$CADDYFILE" >/dev/null
		fi
	else
		as_root sed -i "\#^$CADDY_ADDED\$#d; \#^import $CADDY_SITE\$#d" "$CADDYFILE"
	fi
	as_root systemctl reload caddy 2>/dev/null || as_root systemctl restart caddy 2>/dev/null || true
	ok "Removed LocalLeads from Caddy (Caddy itself stays installed)"
}

# ------------------------------------------------------------ uninstall

uninstall() {
	step "Uninstalling LocalLeads"
	local delete=$DELETE_DATA
	if [ "$INTERACTIVE" = 1 ] && [ "$delete" != 1 ]; then
		say "  Your leads, proxies and account are in $DATA_DIR."
		if confirm "Delete them too? This can't be undone" n; then
			ask "Type delete to confirm"
			if [ "$ANSWER" = delete ]; then delete=1; else note "Keeping the data."; fi
		fi
	fi
	if [ "$OS" = Linux ]; then
		as_root systemctl disable --now "$SERVICE" >/dev/null 2>&1 || true
		as_root rm -f "$UNIT"
		as_root systemctl daemon-reload
		remove_from_caddy
		as_root rm -f "$BIN" "$ENV_FILE" "$CONF_DIR"/proxy-*
		as_root rmdir "$CONF_DIR" 2>/dev/null || true
		if [ "$delete" = 1 ]; then
			as_root rm -rf "$DATA_DIR"
			as_root userdel "$SERVICE" 2>/dev/null || true
		fi
	else
		stop_launchd
		rm -f "$PLIST" "$BIN" "$ENV_FILE" "$LOG_FILE" "$CONF_DIR"/proxy-*
		rmdir "$(dirname "$BIN")" 2>/dev/null || true
		if [ "$delete" = 1 ]; then rm -rf "$DATA_DIR"; fi
		rmdir "$CONF_DIR" 2>/dev/null || true
	fi
	ok "Removed the LocalLeads service and program"
	if [ "$delete" = 1 ]; then
		ok "Deleted the data"
	else
		note "Your data is still in $DATA_DIR; running this script again picks it up."
	fi
	say ""
}

# ------------------------------------------------------------ finish

# Prints, and saves next to the settings, the config that sends the public
# address to LocalLeads, for the web server in front.
proxy_example() {
	local host file example
	host=$(bare_host "$PROXY_URL")
	if [ -z "$PROXY_SOFTWARE" ]; then
		if command -v nginx >/dev/null 2>&1; then
			PROXY_SOFTWARE=nginx
		elif command -v caddy >/dev/null 2>&1; then
			PROXY_SOFTWARE=caddy
		elif command -v apache2 >/dev/null 2>&1 || command -v httpd >/dev/null 2>&1; then
			PROXY_SOFTWARE=apache
		fi
	fi
	case $PROXY_SOFTWARE in
	caddy)
		file="$CONF_DIR/proxy-caddy.txt"
		example="$host {
	reverse_proxy 127.0.0.1:$PORT
}"
		;;
	apache)
		file="$CONF_DIR/proxy-apache.conf"
		example="# Needs: a2enmod proxy proxy_http headers
<VirtualHost *:443>
    ServerName $host
    ProxyPreserveHost On
    ProxyPass / http://127.0.0.1:$PORT/ flushpackets=on
    ProxyPassReverse / http://127.0.0.1:$PORT/
    RequestHeader set X-Forwarded-Proto https
    # Your SSLEngine and certificate lines go here.
</VirtualHost>"
		;;
	*)
		file="$CONF_DIR/proxy-nginx.conf"
		example="server {
    server_name $host;
    listen 80;
    # Add HTTPS with: certbot --nginx -d $host

    location / {
        proxy_pass http://127.0.0.1:$PORT;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        # Live updates and MCP stream; don't buffer them.
        proxy_buffering off;
        proxy_read_timeout 1h;
        client_max_body_size 64m;
    }
}"
		;;
	esac
	if [ "$OS" = Linux ]; then
		printf '%s\n' "$example" | as_root tee "$file" >/dev/null
	else
		printf '%s\n' "$example" >"$file"
	fi
	say ""
	say "  ${BOLD}Last step: point your web server at LocalLeads${RESET}"
	note "Add this to your ${PROXY_SOFTWARE:-nginx} config (saved as $file), then reload it:"
	say ""
	printf '%s\n' "$example" | sed 's/^/    /'
}

finish() {
	local url
	url=$(public_url)
	say ""
	say "${GREEN}${BOLD}  LocalLeads is running.${RESET}"
	say ""
	say "  Open         ${BOLD}$url${RESET}"
	if [ "$MODE" = local ]; then
		say "               ${DIM}Create your account on the first visit.${RESET}"
		if [ "$OS" = Linux ]; then
			note "From your computer: ssh -L $PORT:127.0.0.1:$PORT you@this-server, then open it."
		fi
	else
		say "  Setup code   ${BOLD}$SETUP_CODE${RESET}  ${DIM}(to create your account, the first time)${RESET}"
	fi
	if [ "$MODE" = ip ] && [ "$IP_TRUSTED" != 1 ]; then
		say ""
		note "Your browser warns about the certificate because it's self-signed."
		if [ "$IP_CHALLENGES" = none ]; then
			if caddy_ip_certs; then
				note "Let's Encrypt checks an IP address on port 80 or 443, and both are taken."
			else
				note "Caddy 2.11 or newer can get a Let's Encrypt certificate for the IP."
			fi
		fi
		note "Choose Advanced → Proceed. For a trusted certificate, point a domain"
		note "at $SERVER_IP and run this again."
	fi
	if uses_caddy; then
		say ""
		note "If it doesn't load, open ports $HTTP_PORT and $HTTPS_PORT (TCP) in your cloud"
		note "provider's firewall or security group."
	fi
	if [ "$MODE" = proxy ]; then proxy_example; fi
	say ""
	say "  ${BOLD}Manage it${RESET}"
	if [ "$OS" = Linux ]; then
		say "    journalctl -u $SERVICE -f        ${DIM}# logs${RESET}"
		say "    systemctl restart $SERVICE       ${DIM}# restart${RESET}"
		say "    systemctl stop $SERVICE          ${DIM}# stop${RESET}"
	else
		say "    tail -f $LOG_FILE"
		say "    launchctl kickstart -k gui/\$(id -u)/$LABEL   ${DIM}# restart${RESET}"
	fi
	say "    curl -fsSL $SCRIPT_URL | bash"
	say "    ${DIM}# update, change settings or uninstall${RESET}"
	say ""
	say "  Settings: $ENV_FILE"
	say "  Data:     $DATA_DIR"
	say "  New versions show up in the app's sidebar, ready to install with one click."
	say "  Connect AI agents over MCP from the app: Agents, in the sidebar."
	say ""
}

# ------------------------------------------------------------ main

main() {
	parse_args "$@"
	MODE_FROM_FLAGS=$MODE
	open_tty

	say ""
	say "${BOLD}  LocalLeads setup${RESET}"
	note "Google Maps lead finder with email enrichment, proxies and MCP."
	say ""

	check_system

	if is_installed; then
		existing_menu
		case $ACTION in
		uninstall)
			uninstall
			return
			;;
		update)
			load_install
			;;
		reconfigure)
			# Keep the setup code and channel; ask the rest again.
			local keep_channel=$CHANNEL was_caddy=0
			case $(env_get LOCALLEADS_MODE "$ENV_FILE") in domain | ip) was_caddy=1 ;; esac
			load_install
			if [ -n "$keep_channel" ]; then CHANNEL=$keep_channel; fi
			if [ -z "$MODE_FROM_FLAGS" ]; then
				MODE="" DOMAIN="" PROXY_URL="" HTTP_PORT="" HTTPS_PORT=""
			fi
			;;
		esac
	else
		case $ACTION in
		update | uninstall) die "LocalLeads isn't installed here." ;;
		esac
		ACTION=install
	fi

	if [ "$ACTION" = update ]; then
		case $MODE in
		domain | ip | local | proxy) ;;
		*) die "$ENV_FILE is missing its settings." "Run this again without --update to set it up." ;;
		esac
	else
		configure
		summary
	fi

	install_binary
	if uses_caddy && [ "$ACTION" != update ]; then ensure_caddy; fi
	install_service
	wait_healthy
	if uses_caddy && [ "$ACTION" != update ]; then configure_caddy; fi
	if [ "$ACTION" = reconfigure ] && [ "${was_caddy:-0}" = 1 ] && ! uses_caddy; then remove_from_caddy; fi
	finish
}

main "$@"
