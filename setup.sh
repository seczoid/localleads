#!/usr/bin/env bash
# LocalLeads setup: installs LocalLeads with Docker and starts it.
#
#   curl -fsSL https://raw.githubusercontent.com/seczoid/localleads/main/setup.sh | bash
#
# It asks a few questions (press Enter to take the default), then pulls the
# image, writes the config, starts everything and prints the address and the
# setup code for creating your account. Run it again to update, change
# settings or uninstall.
#
# Unattended, with flags after `bash -s --`:
#
#   curl -fsSL .../setup.sh | bash -s -- --domain leads.example.com --yes
#
# Run with --help for every option.

# Everything is inside main(), called on the last line, so a download cut
# short never runs half a script.

set -euo pipefail

REPO="seczoid/localleads"
SCRIPT_URL="https://raw.githubusercontent.com/$REPO/main/setup.sh"
SOURCE_URL="https://codeload.github.com/$REPO/tar.gz/refs/heads/main"
REGISTRY_IMAGE="ghcr.io/$REPO"
PROJECT="localleads"

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
	for line in "$@"; do
		if [ -n "$line" ]; then printf '  %s\n' "$line" >&2; else printf '\n' >&2; fi
	done
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

is_ipv4() {
	local re='^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$'
	[[ $1 =~ $re ]] || return 1
	local part
	for part in "${BASH_REMATCH[@]:1}"; do [ "$part" -le 255 ] || return 1; done
}

is_domain() {
	local re='^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z][a-z0-9-]{0,61}[a-z0-9]$'
	[[ $1 =~ $re ]] || [ "$1" = localhost ]
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
	elif command -v dscacheutil >/dev/null 2>&1; then
		out=$(dscacheutil -q host -a name "$1" 2>/dev/null | awk '/^ip_address:/{print $2}') || true
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

free_port_from() {
	local port=$1
	while port_busy "$port"; do port=$((port + 1)); done
	printf '%s' "$port"
}

new_setup_code() {
	local hex
	hex=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
	printf '%s-%s-%s-%s' "${hex:0:4}" "${hex:4:4}" "${hex:8:4}" "${hex:12:4}"
}

# Reads KEY=value from an env file without running it.
env_get() {
	[ -f "$2" ] || return 0
	sed -n "s/^$1=//p" "$2" | tail -n 1
}

dc() { (cd "$DIR" && docker compose "$@"); }

memory_mb() {
	if [ -r /proc/meminfo ]; then
		awk '/^MemTotal:/{print int($2/1024)}' /proc/meminfo
	elif [ "$OS" = Darwin ]; then
		echo $(($(sysctl -n hw.memsize) / 1024 / 1024))
	else
		echo 0
	fi
}

# ------------------------------------------------------------ options

usage() {
	cat <<EOF
LocalLeads setup: installs LocalLeads with Docker and starts it.

  curl -fsSL $SCRIPT_URL | bash
  curl -fsSL $SCRIPT_URL | bash -s -- [options]

With no options it asks a few questions. Run it again to update, change
settings or uninstall.

Where it's served (pick one):
  --domain NAME     Your own domain, with HTTPS from Let's Encrypt.
                    Point its DNS A record at this server first.
  --ip [ADDRESS]    This server's IP address, with a self-signed
                    certificate (the browser warns once). Default on Linux.
  --local           Only this computer, at http://localhost:PORT.
                    Default on macOS.
  --proxy URL       Behind your own reverse proxy (nginx, Traefik, ...):
                    URL is the address people open; the app listens on
                    127.0.0.1:PORT for the proxy.

Other options:
  --port N          Port for --local and --proxy (default 7878).
  --http-port N     Host ports Caddy uses (default 80 and 443).
  --https-port N
  --dir PATH        Install folder (default /opt/localleads as root,
                    otherwise ~/localleads).
  --image REF       Docker image to run (default $REGISTRY_IMAGE:latest).
  --source PATH     Build the image from this source checkout.
  -y, --yes         Don't ask; use the defaults and the options given.
  --update          Update an existing install to the latest image.
  --uninstall       Stop and remove LocalLeads. Data is kept unless you
                    also pass --delete-data.
  -h, --help        Show this help.
EOF
}

MODE=""
DOMAIN=""
SERVER_IP=""
PROXY_URL=""
PORT=""
HTTP_PORT=""
HTTPS_PORT=""
DIR="${LOCALLEADS_DIR:-}"
IMAGE="${LOCALLEADS_IMAGE:-}"
SOURCE="${LOCALLEADS_SOURCE:-}"
ASSUME_YES=0
ACTION=""
DELETE_DATA=0

need_value() {
	if [ $# -lt 2 ] || [ -z "$2" ] || [ "${2#-}" != "$2" ]; then
		die "$1 needs a value." "Run with --help to see the options."
	fi
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
		--http-port) need_value "$@"; HTTP_PORT=$2; shift ;;
		--http-port=*) HTTP_PORT=${1#*=} ;;
		--https-port) need_value "$@"; HTTPS_PORT=$2; shift ;;
		--https-port=*) HTTPS_PORT=${1#*=} ;;
		--dir) need_value "$@"; DIR=$2; shift ;;
		--dir=*) DIR=${1#*=} ;;
		--image) need_value "$@"; IMAGE=$2; shift ;;
		--image=*) IMAGE=${1#*=} ;;
		--source) need_value "$@"; SOURCE=$2; shift ;;
		--source=*) SOURCE=${1#*=} ;;
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
	local port
	for port in "$PORT" "$HTTP_PORT" "$HTTPS_PORT"; do
		if [ -n "$port" ] && ! is_port "$port"; then die "\"$port\" isn't a port number."; fi
	done
	return 0
}

# ------------------------------------------------------------ checks

check_system() {
	case $OS in
	Linux | Darwin) ;;
	*) die "This script runs on Linux and macOS." "On Windows, use WSL 2 with Docker Desktop and run it there." ;;
	esac
	case $(uname -m) in
	x86_64 | amd64 | aarch64 | arm64) ;;
	*) warn "Prebuilt images are for x86_64 and arm64; on $(uname -m) it'll build from source." ;;
	esac
	command -v curl >/dev/null 2>&1 || die "curl is missing." "Install it with your package manager, then run this again."
}

check_docker() {
	step "Checking Docker"
	if ! command -v docker >/dev/null 2>&1; then
		if [ "$OS" = Darwin ]; then
			die "Docker isn't installed." \
				"Install one of these, start it, then run this again:" \
				"  Docker Desktop  https://www.docker.com/products/docker-desktop/" \
				"  OrbStack        https://orbstack.dev" \
				"  Colima          brew install colima docker docker-compose && colima start"
		else
			die "Docker isn't installed." \
				"Install it (the official script works on most distributions):" \
				"" \
				"  curl -fsSL https://get.docker.com | sudo sh" \
				"" \
				"Then run this again. Docs: https://docs.docker.com/engine/install/"
		fi
	fi
	local err
	if ! err=$(docker info 2>&1 >/dev/null); then
		case $(lower "$err") in
		*"permission denied"*)
			die "Docker is installed, but $(id -un) can't use it." \
				"Run this script with sudo:" \
				"" \
				"  curl -fsSL $SCRIPT_URL | sudo bash" \
				"" \
				"or add yourself to the docker group, log out and back in:" \
				"  sudo usermod -aG docker $(id -un)"
			;;
		*)
			if [ "$OS" = Darwin ]; then
				die "Docker is installed but not running." \
					"Start Docker Desktop, OrbStack or Colima (colima start), then run this again."
			else
				local sudo="sudo " start
				[ "$(id -u)" != 0 ] || sudo=""
				# No systemd in WSL and some containers and minimal VMs.
				if [ ! -d /run/systemd/system ] && command -v service >/dev/null 2>&1; then
					start="${sudo}service docker start"
				else
					start="${sudo}systemctl enable --now docker"
				fi
				die "Docker is installed but not running." \
					"Start it, then run this again:" \
					"" \
					"  $start"
			fi
			;;
		esac
	fi
	ok "Docker $(docker version -f '{{.Server.Version}}' 2>/dev/null || echo "") is running"
	if ! docker compose version >/dev/null 2>&1; then
		die "Docker Compose isn't installed." \
			"Install the Compose plugin, then run this again:" \
			"  Debian/Ubuntu  sudo apt-get install docker-compose-plugin" \
			"  Fedora/RHEL    sudo dnf install docker-compose-plugin" \
			"  Docs           https://docs.docker.com/compose/install/"
	fi
	ok "Docker Compose $(docker compose version --short 2>/dev/null) is installed"
}

# ------------------------------------------------------------ existing install

# An install this script made: a folder with our .env in it.
is_install() { [ -f "$1/.env" ] && grep -q '^LOCALLEADS_MODE=' "$1/.env"; }

find_install() {
	if [ -n "$DIR" ]; then return; fi
	# A running install, wherever it is.
	local id dir
	id=$(docker ps -q --filter "label=com.docker.compose.project=$PROJECT" --filter "label=com.docker.compose.service=localleads" 2>/dev/null | head -n 1) || true
	if [ -n "$id" ]; then
		dir=$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project.working_dir"}}' "$id" 2>/dev/null) || true
		if [ -n "$dir" ] && is_install "$dir"; then
			DIR=$dir
			return
		fi
	fi
	if [ "$(id -u)" = 0 ]; then DIR=/opt/localleads; else DIR="$HOME/localleads"; fi
}

load_install() {
	local env="$DIR/.env"
	[ -n "$MODE" ] || MODE=$(env_get LOCALLEADS_MODE "$env")
	[ -n "$DOMAIN" ] || DOMAIN=$(env_get DOMAIN "$env")
	[ -n "$SERVER_IP" ] || SERVER_IP=$(env_get SERVER_IP "$env")
	[ -n "$PORT" ] || PORT=$(env_get PORT "$env")
	[ -n "$HTTP_PORT" ] || HTTP_PORT=$(env_get HTTP_PORT "$env")
	[ -n "$HTTPS_PORT" ] || HTTPS_PORT=$(env_get HTTPS_PORT "$env")
	[ -n "$IMAGE" ] || IMAGE=$(env_get LOCALLEADS_IMAGE "$env")
	if [ -z "$PROXY_URL" ] && [ "$MODE" = proxy ]; then PROXY_URL=$(env_get PUBLIC_URL "$env"); fi
	SETUP_CODE=$(env_get LOCALLEADS_SETUP_CODE "$env")
	case $(env_get LOCALLEADS_MODE "$env") in
	domain | ip) HELD_PORTS=" $(env_get HTTP_PORT "$env") $(env_get HTTPS_PORT "$env") " ;;
	*) HELD_PORTS=" $(env_get PORT "$env") " ;;
	esac
}

running() {
	[ -n "$(dc ps -q --status running localleads 2>/dev/null)" ]
}

# The host ports the existing install publishes, like " 80 443 ".
HELD_PORTS=" "

# In use by something other than the running install, which is restarted
# in place and so frees its own ports.
port_taken() {
	port_busy "$1" || return 1
	case $HELD_PORTS in *" $1 "*) running && return 1 ;; esac
	return 0
}

existing_menu() {
	step "LocalLeads is already installed in $DIR"
	if running; then ok "It's running"; else note "It's stopped"; fi
	if [ -n "$ACTION" ]; then return; fi
	if [ -n "$MODE_FROM_FLAGS" ]; then
		ACTION=reconfigure
		return
	fi
	choose 1 "Update to the latest version (keeps your data and settings)" \
		"Change settings (address, domain, ports)" \
		"Uninstall" \
		"Quit"
	case $ANSWER in
	1) ACTION=update ;;
	2) ACTION=reconfigure ;;
	3) ACTION=uninstall ;;
	*) exit 0 ;;
	esac
}

uninstall() {
	step "Uninstalling LocalLeads from $DIR"
	local delete=$DELETE_DATA
	if [ "$INTERACTIVE" = 1 ] && [ "$delete" != 1 ]; then
		say "  Your leads, proxies and account are kept in a Docker volume unless you"
		say "  delete them now."
		if confirm "Delete all LocalLeads data too? This can't be undone" n; then
			ask "Type delete to confirm"
			if [ "$ANSWER" = delete ]; then delete=1; else note "Keeping the data."; fi
		fi
	fi
	if [ "$delete" = 1 ]; then
		dc down --volumes --remove-orphans
		# Also Caddy's volumes from an earlier domain or IP setup, which the
		# current compose.yaml may no longer list.
		delete_volumes
		ok "Stopped LocalLeads and deleted its data"
	else
		dc down --remove-orphans
		ok "Stopped LocalLeads; the data is kept in the ${PROJECT}_localleads-data volume"
		note "Run this script again to start it with the same data,"
		note "or delete the data later with: docker volume rm ${PROJECT}_localleads-data"
	fi
	rm -f "$DIR/compose.yaml" "$DIR/Caddyfile" "$DIR/.env"
	rm -rf "$DIR/src"
	rmdir "$DIR" 2>/dev/null || true
	ok "Removed the config in $DIR"
	say ""
}

# Docker volumes of this compose project, one per line.
project_volumes() {
	docker volume ls -q --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null || true
}

delete_volumes() {
	local volumes
	volumes=$(project_volumes)
	[ -n "$volumes" ] || return 0
	# shellcheck disable=SC2086 # one name per word
	docker volume rm $volumes >/dev/null
}

# --uninstall when the config is gone, e.g. a second run to delete the data
# an earlier uninstall kept.
uninstall_leftovers() {
	local volumes
	volumes=$(project_volumes)
	if [ -z "$volumes" ]; then
		die "LocalLeads isn't installed in $DIR." "Pass --dir if it's somewhere else."
	fi
	step "LocalLeads isn't installed, but its data is still here"
	note "Volumes: $(printf '%s' "$volumes" | tr '\n' ' ')"
	local delete=$DELETE_DATA
	if [ "$INTERACTIVE" = 1 ] && [ "$delete" != 1 ]; then
		if confirm "Delete all LocalLeads data? This can't be undone" n; then
			ask "Type delete to confirm"
			if [ "$ANSWER" = delete ]; then delete=1; else note "Keeping the data."; fi
		fi
	fi
	if [ "$delete" = 1 ]; then
		delete_volumes
		ok "Deleted the LocalLeads data"
	else
		note "Kept. Delete it with --uninstall --delete-data, or install again to use it."
	fi
	say ""
}

# ------------------------------------------------------------ wizard

MODE_FROM_FLAGS=""

configure() {
	if [ -z "$MODE" ]; then
		step "Where will you open LocalLeads?"
		local default=1
		[ "$OS" = Darwin ] && default=3
		choose "$default" \
			"From anywhere, at this server's IP address ${DIM}(HTTPS with a self-signed certificate; quickest)${RESET}" \
			"From anywhere, at my own domain ${DIM}(HTTPS from Let's Encrypt; needs a DNS record)${RESET}" \
			"Only on this computer ${DIM}(http://localhost)${RESET}" \
			"Behind my own reverse proxy ${DIM}(nginx, Traefik, Cloudflare Tunnel, ...)${RESET}"
		case $ANSWER in
		1) MODE=ip ;;
		2) MODE=domain ;;
		3) MODE=local ;;
		4) MODE=proxy ;;
		esac
	fi
	case $MODE in
	domain) configure_domain ;;
	ip) configure_ip ;;
	local) configure_local ;;
	proxy) configure_proxy ;;
	*) die "Unknown mode \"$MODE\" in $DIR/.env." "Run with --local, --ip, --domain or --proxy to choose again." ;;
	esac
}

configure_domain() {
	while ! is_domain "$DOMAIN" || [ "$DOMAIN" = localhost ]; do
		if [ "$INTERACTIVE" != 1 ]; then
			[ "$DOMAIN" != localhost ] || die "Let's Encrypt can't issue a certificate for localhost." "Use --local to run it on this computer only."
			die "--domain is needed (for example --domain leads.example.com)."
		fi
		[ -z "$DOMAIN" ] || say "  \"$DOMAIN\" doesn't look like a domain name, like leads.example.com."
		ask "Domain name (like leads.example.com)" "$DOMAIN"
		DOMAIN=$(bare_host "$ANSWER")
	done
	[ -n "$HTTP_PORT" ] || HTTP_PORT=80
	[ -n "$HTTPS_PORT" ] || HTTPS_PORT=443
	check_dns
}

check_dns() {
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

configure_local() {
	local default
	default=${PORT:-7878}
	if [ "$ACTION" != update ] && port_taken "$default"; then
		local free
		free=$(free_port_from "$default")
		note "Port $default is in use, so it'll use $free."
		default=$free
	fi
	if [ "$INTERACTIVE" = 1 ]; then
		while :; do
			ask "Port" "$default"
			if is_port "$ANSWER"; then PORT=$ANSWER; break; fi
			say "  Type a port number."
		done
	else
		PORT=$default
	fi
}

configure_proxy() {
	while :; do
		if [ -z "$PROXY_URL" ]; then
			[ "$INTERACTIVE" = 1 ] || die "--proxy needs the address people open, like https://leads.example.com."
			ask "Address people open (like https://leads.example.com)"
			PROXY_URL=$ANSWER
		fi
		case $PROXY_URL in
		http://* | https://*) PROXY_URL=${PROXY_URL%/}; break ;;
		*.*) PROXY_URL="https://${PROXY_URL%/}"; break ;;
		esac
		[ "$INTERACTIVE" = 1 ] || die "--proxy needs a URL like https://leads.example.com."
		say "  That should look like https://leads.example.com."
		PROXY_URL=""
	done
	configure_local
}

public_url() {
	case $MODE in
	domain)
		if [ "$HTTPS_PORT" = 443 ]; then printf 'https://%s' "$DOMAIN"; else printf 'https://%s:%s' "$DOMAIN" "$HTTPS_PORT"; fi
		;;
	ip)
		if [ "$HTTPS_PORT" = 443 ]; then printf 'https://%s' "$SERVER_IP"; else printf 'https://%s:%s' "$SERVER_IP" "$HTTPS_PORT"; fi
		;;
	local) printf 'http://localhost:%s' "$PORT" ;;
	proxy) printf '%s' "$PROXY_URL" ;;
	esac
}

uses_caddy() { [ "$MODE" = domain ] || [ "$MODE" = ip ]; }

check_ports() {
	local port busy=""
	if uses_caddy; then
		for port in "$HTTP_PORT" "$HTTPS_PORT"; do
			if port_taken "$port"; then busy="$busy $port"; fi
		done
		if [ -n "$busy" ]; then
			die "Port(s)$busy are already in use on this machine." \
				"Caddy needs them to serve HTTPS. Stop what's using them (often nginx" \
				"or Apache: sudo systemctl disable --now nginx), or choose" \
				"\"Behind my own reverse proxy\" to keep your web server in front."
		fi
	elif port_taken "$PORT"; then
		die "Port $PORT is already in use." "Choose another with --port."
	fi
}

summary() {
	step "Ready to install"
	printf '  %-10s %s\n' "Address" "$(public_url)"
	case $MODE in
	domain) printf '  %-10s %s\n' "HTTPS" "Let's Encrypt certificate, renewed by Caddy" ;;
	ip) printf '  %-10s %s\n' "HTTPS" "Self-signed certificate (your browser warns once)" ;;
	local) printf '  %-10s %s\n' "Reachable" "Only from this computer" ;;
	proxy) printf '  %-10s %s\n' "Proxy to" "http://127.0.0.1:$PORT" ;;
	esac
	printf '  %-10s %s\n' "Folder" "$DIR"
	if [ -n "$SOURCE" ]; then
		printf '  %-10s %s\n' "Image" "Built from $SOURCE"
	else
		printf '  %-10s %s\n' "Image" "${IMAGE:-$REGISTRY_IMAGE:latest}"
	fi
	say ""
	confirm "Install now?" y || exit 0
}

# ------------------------------------------------------------ image

get_image() {
	step "Getting LocalLeads"
	if [ -n "$SOURCE" ]; then
		build_image "$SOURCE"
		return
	fi
	if [ -n "$IMAGE" ] && [ "$IMAGE" != "$REGISTRY_IMAGE:latest" ] && [ "$IMAGE" != "$REGISTRY_IMAGE:edge" ]; then
		if docker pull "$IMAGE" 2>/dev/null || docker image inspect "$IMAGE" >/dev/null 2>&1; then
			ok "Using $IMAGE"
			return
		fi
		die "Couldn't find the image $IMAGE."
	fi
	local ref err
	# Progress goes to the terminal; the error is kept to print one short line,
	# since a missing tag is expected and shouldn't look like a failure.
	exec 4>&1
	for ref in "$REGISTRY_IMAGE:latest" "$REGISTRY_IMAGE:edge"; do
		if err=$(docker pull "$ref" 2>&1 >&4); then
			exec 4>&-
			IMAGE=$ref
			ok "Downloaded $ref"
			return
		fi
		note "$ref isn't available ($(printf '%s\n' "$err" | sed '/^[[:space:]]*$/d' | tail -n 1))"
	done
	exec 4>&-
	warn "No prebuilt image to download, so it'll be built from source."
	local src="$DIR/src"
	rm -rf "$src"
	mkdir -p "$src"
	curl -fsSL "$SOURCE_URL" | tar -xz -C "$src" --strip-components 1 ||
		die "Couldn't download the source from github.com/$REPO."
	build_image "$src"
}

build_image() {
	[ -f "$1/Dockerfile" ] || die "$1 has no Dockerfile."
	local memory
	memory=$(memory_mb)
	if [ "$memory" -gt 0 ] && [ "$memory" -lt 3000 ]; then
		warn "This machine has ${memory} MB of memory; compiling needs about 3 GB."
		note "If the build gets killed, add swap or use a prebuilt image."
	fi
	note "Compiling takes 5-20 minutes, depending on the machine."
	docker build -t localleads:local "$1" || die "The build failed." "The error is above."
	IMAGE=localleads:local
	ok "Built localleads:local"
}

# ------------------------------------------------------------ files

write_files() {
	step "Writing the config"
	mkdir -p "$DIR"
	[ -n "$SETUP_CODE" ] || SETUP_CODE=$(new_setup_code)

	local health=""
	local version
	version=$(docker version -f '{{.Server.Version}}' 2>/dev/null | cut -d. -f1) || version=0
	case $version in *[!0-9]* | "") version=0 ;; esac
	if [ "$version" -ge 25 ]; then
		# Checks every 2s while starting, so it's up in seconds, not 30.
		health="    healthcheck:
      start_period: 60s
      start_interval: 2s
"
	fi

	local umask_was
	umask_was=$(umask)
	umask 077
	cat >"$DIR/.env" <<EOF
# Written by setup.sh. Run it again to change these:
#   curl -fsSL $SCRIPT_URL | bash
LOCALLEADS_MODE=$MODE
LOCALLEADS_IMAGE=$IMAGE
PUBLIC_URL=$(public_url)
DOMAIN=$DOMAIN
SERVER_IP=$SERVER_IP
PORT=$PORT
HTTP_PORT=$HTTP_PORT
HTTPS_PORT=$HTTPS_PORT
# Needed once, to create the account. Unused after that.
LOCALLEADS_SETUP_CODE=$SETUP_CODE
EOF
	umask "$umask_was"

	local behind_proxy=true listen=""
	if [ "$MODE" = local ]; then behind_proxy=false; fi
	if ! uses_caddy; then
		listen="    ports:
      - \"127.0.0.1:\${PORT}:7878\"
"
	fi

	{
		cat <<EOF
# Written by setup.sh; it's rewritten when you run setup.sh again.
name: $PROJECT

services:
  localleads:
    image: \${LOCALLEADS_IMAGE}
    restart: unless-stopped
    environment:
      LOCALLEADS_PUBLIC_URL: \${PUBLIC_URL}
      LOCALLEADS_BEHIND_PROXY: "$behind_proxy"
      LOCALLEADS_SETUP_CODE: \${LOCALLEADS_SETUP_CODE:-}
    volumes:
      - localleads-data:/data
${listen}${health}    ulimits:
      nofile:
        soft: 65536
        hard: 65536
    # Lets running searches stop cleanly; they resume as "interrupted".
    stop_grace_period: 30s
    logging:
      driver: json-file
      options:
        max-size: 10m
        max-file: "5"
EOF
		if uses_caddy; then
			cat <<'EOF'

  caddy:
    image: caddy:2.10-alpine
    restart: unless-stopped
    depends_on:
      localleads:
        condition: service_healthy
    ports:
      - "${HTTP_PORT}:80"
      - "${HTTPS_PORT}:443"
      - "${HTTPS_PORT}:443/udp"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config
    logging:
      driver: json-file
      options:
        max-size: 10m
        max-file: "5"
EOF
		fi
		cat <<'EOF'

volumes:
  localleads-data:
EOF
		if uses_caddy; then
			cat <<'EOF'
  caddy-data:
  caddy-config:
EOF
		fi
	} >"$DIR/compose.yaml"

	if uses_caddy; then
		write_caddyfile
	else
		rm -f "$DIR/Caddyfile"
	fi
	ok "Wrote $DIR/compose.yaml"
}

write_caddyfile() {
	local site global="" tls=""
	if [ "$MODE" = domain ]; then
		site=$DOMAIN
	else
		site="https://$SERVER_IP"
		# Browsers send no server name for an IP address, so tell Caddy
		# which certificate to answer with; it signs it with its own CA.
		global="{
	default_sni $SERVER_IP
}

"
		tls="	tls internal

"
	fi
	cat >"$DIR/Caddyfile" <<EOF
# Written by setup.sh; it's rewritten when you run setup.sh again.
${global}${site} {
${tls}	# Live updates (/events) and MCP (/mcp) stream; don't compress them.
	@compress not path /events /mcp
	encode @compress zstd gzip

	reverse_proxy localleads:7878

	# The app sends HSTS and its other security headers itself.
	header -Server

	log {
		output stdout
		format console
	}
}
EOF
}

# ------------------------------------------------------------ start

start() {
	step "Starting LocalLeads"
	if ! dc up -d --remove-orphans; then
		say ""
		dc logs --tail 40 localleads >&2 || true
		die "LocalLeads didn't start." "The log is above. Fix the problem and run this again."
	fi
	wait_healthy
	if uses_caddy; then wait_https; fi
}

wait_healthy() {
	local id status i
	id=$(dc ps -q localleads)
	for i in $(seq 1 90); do
		status=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$id" 2>/dev/null) || status=gone
		case $status in
		healthy)
			ok "LocalLeads is up"
			return
			;;
		unhealthy | exited | dead | gone) break ;;
		esac
		sleep 2
	done
	say ""
	dc logs --tail 40 localleads >&2 || true
	die "LocalLeads isn't answering (status: $status)." "The log is above."
}

wait_https() {
	local host=$DOMAIN check
	[ "$MODE" = ip ] && host=$SERVER_IP
	note "Waiting for the HTTPS certificate..."
	local i
	for i in $(seq 1 45); do
		if [ "$MODE" = ip ]; then
			check=$(curl -ks -o /dev/null -w '%{http_code}' --max-time 5 -H "Host: $SERVER_IP" "https://127.0.0.1:$HTTPS_PORT/healthz") || check=""
		else
			check=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 --resolve "$host:$HTTPS_PORT:127.0.0.1" "https://$host:$HTTPS_PORT/healthz") || check=""
		fi
		if [ "$check" = 200 ]; then
			ok "HTTPS works"
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
	note "  cd $DIR && docker compose logs caddy"
}

finish() {
	local url
	url=$(public_url)
	say ""
	say "${GREEN}${BOLD}  LocalLeads is running.${RESET}"
	say ""
	say "  Open      ${BOLD}$url${RESET}"
	if [ "$ACTION" != update ] || [ -n "$SETUP_CODE" ]; then
		say "  Setup code ${BOLD}$SETUP_CODE${RESET}  ${DIM}(to create your account, the first time)${RESET}"
	fi
	if [ "$MODE" = ip ]; then
		say ""
		note "Your browser warns about the certificate because it's self-signed."
		note "Choose Advanced → Proceed. For a trusted certificate, point a domain"
		note "at $SERVER_IP and run this again."
	fi
	if uses_caddy; then
		say ""
		note "If it doesn't load, open ports 80 and 443 (TCP) in your cloud provider's"
		note "firewall or security group."
	fi
	if [ "$MODE" = proxy ]; then
		say ""
		note "Point your proxy at http://127.0.0.1:$PORT and have it pass"
		note "X-Forwarded-For and X-Forwarded-Proto. Don't buffer /events or /mcp."
	fi
	say ""
	say "  ${BOLD}Manage it${RESET}  ${DIM}(from $DIR)${RESET}"
	say "    docker compose logs -f localleads    ${DIM}# logs${RESET}"
	say "    docker compose restart               ${DIM}# restart${RESET}"
	say "    docker compose down                  ${DIM}# stop${RESET}"
	say "    curl -fsSL $SCRIPT_URL | bash"
	say "                                         ${DIM}# update, change settings, uninstall${RESET}"
	say ""
	say "  Connect AI agents over MCP from the app: MCP, in the sidebar."
	say ""
}

# ------------------------------------------------------------ main

main() {
	parse_args "$@"
	MODE_FROM_FLAGS=$MODE
	open_tty
	SETUP_CODE=""

	say ""
	say "${BOLD}  LocalLeads setup${RESET}"
	note "Google Maps lead finder with email enrichment, proxies and MCP."

	check_system
	check_docker
	find_install

	if is_install "$DIR"; then
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
			# Keep the image and setup code; ask everything else again.
			local keep_image keep_code
			keep_image=$IMAGE
			load_install
			keep_code=$SETUP_CODE
			if [ -z "$MODE_FROM_FLAGS" ]; then
				MODE="" DOMAIN="" PROXY_URL=""
			fi
			IMAGE=${keep_image:-$IMAGE}
			SETUP_CODE=$keep_code
			;;
		esac
		# Updates always move to the newest registry image.
		if [ "$ACTION" = update ] && [ -z "${LOCALLEADS_IMAGE:-}" ] && [ -z "$SOURCE" ]; then
			# A from-source build is rebuilt from the latest source too.
			case $IMAGE in "$REGISTRY_IMAGE":* | localleads:local | "") IMAGE="" ;; esac
		fi
	else
		case $ACTION in
		update) die "LocalLeads isn't installed in $DIR." "Pass --dir if it's somewhere else." ;;
		uninstall)
			uninstall_leftovers
			return
			;;
		esac
		ACTION=install
	fi

	if [ "$ACTION" = update ]; then
		case $MODE in
		domain | ip | local | proxy) ;;
		*) die "$DIR/.env is missing its settings." "Run this again without --update to set it up." ;;
		esac
	else
		configure
		check_ports
		summary
	fi

	get_image
	write_files
	start
	finish
}

main "$@"
