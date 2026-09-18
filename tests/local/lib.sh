#!/usr/bin/env bash
# Shared helpers for the local nextcloud-dav end-to-end harness.
# Source this file; do not execute it directly.
#
# Everything lives under the compose project `ncdav-e2e`. Secrets are generated
# once into state/env (mode 0600) and never printed.
# shellcheck shell=bash

set -euo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$LOCAL_DIR/../.." && pwd)"
STATE_DIR="$LOCAL_DIR/state"
ENV_FILE="$STATE_DIR/env"
CONFIG_DIR="$STATE_DIR/config"
EVIDENCE_DIR="$STATE_DIR/evidence"

NC=ncdav-e2e-nc
PG=ncdav-e2e-db
NET=ncdav-e2e

SIDECAR_BIN="$REPO_DIR/target/release/nextcloud-dav"
SIDECAR_PORT="${SIDECAR_PORT:-17870}"
SIDECAR_URL="http://127.0.0.1:$SIDECAR_PORT"
NC_PORT="${NC_PORT:-18081}"
NC_URL="http://127.0.0.1:$NC_PORT"
PG_PORT="${PG_PORT:-55433}"

compose() { docker compose -p ncdav-e2e -f "$LOCAL_DIR/docker-compose.yml" "$@"; }

# occ inside the container as www-data (the image's config owner).
occ() { docker exec -u www-data -w /var/www/html "$NC" php occ "$@"; }

# psql against the published Postgres port. $DB_PASSWORD comes from state/env.
#
# Run one SQL statement against the given database (default nextcloud) and
# print the result with `|` separators, unaligned, no header.
q() {
	local sql="$1" database="${2:-nextcloud}"
	PGPASSWORD="$DB_PASSWORD" psql -h 127.0.0.1 -p "$PG_PORT" -U nextcloud \
		-d "$database" -v ON_ERROR_STOP=1 -tA -F '|' -c "$sql"
}

# Run SQL and fail the script on error (still prints the result).
q_strict() {
	local sql="$1" database="${2:-nextcloud}"
	PGPASSWORD="$DB_PASSWORD" psql -h 127.0.0.1 -p "$PG_PORT" -U nextcloud \
		-d "$database" -v ON_ERROR_STOP=1 -tA -F '|' -c "$sql"
}
rand_hex() {
	local bytes="$1"
	if command -v openssl >/dev/null 2>&1; then
		openssl rand -hex "$bytes"
	else
		head -c "$bytes" /dev/urandom | od -An -tx1 | tr -d ' \n'
	fi
}

load_env() {
	if [ ! -f "$ENV_FILE" ]; then
		echo "state/env missing; run setup.sh first" >&2
		exit 1
	fi
	# shellcheck disable=SC1090
	set -a; . "$ENV_FILE"; set +a
}

# Wait until the Nextcloud container reports an installed instance.
wait_for_nextcloud() {
	local i
	for i in $(seq 1 120); do
		if docker exec -u www-data -w /var/www/html "$NC" php occ status 2>/dev/null | grep -q 'installed: true'; then
			return 0
		fi
		sleep 2
	done
	echo "Nextcloud did not finish installing within 240s" >&2
	docker logs --tail 40 "$NC" >&2 || true
	return 1
}

# Wait until the Nextcloud HTTP endpoint answers (the container installs, then
# starts Apache a moment later; occ is usable before Apache is).
wait_for_http() {
	local i
	for i in $(seq 1 90); do
		if curl -fsS "$NC_URL/status.php" >/dev/null 2>&1; then
			return 0
		fi
		sleep 1
	done
	echo "Nextcloud HTTP endpoint did not become ready at $NC_URL" >&2
	return 1
}

# Wait for the sidecar health endpoint.
wait_for_sidecar() {
	local i
	for i in $(seq 1 60); do
		if curl -fsS "$SIDECAR_URL/healthz" >/dev/null 2>&1; then
			return 0
		fi
		sleep 1
	done
	echo "sidecar did not become healthy at $SIDECAR_URL" >&2
	return 1
}

# Start the release sidecar on the host against the copied config + the
# containerised Postgres. The DB URL is read from the copied config.php with
# dbhost/dbport overridden by local-db.config.php (state/config/), so no
# secret is ever placed in argv. `--glob-config` loads that sibling file.
start_sidecar() {
	stop_sidecar 2>/dev/null || true
	nohup "$SIDECAR_BIN" \
		--config "$CONFIG_DIR/config.php" \
		--glob-config \
		--listen "127.0.0.1:$SIDECAR_PORT" \
		--log-level debug \
		>"$STATE_DIR/sidecar.log" 2>&1 &
	echo $! >"$STATE_DIR/sidecar.pid"
	wait_for_sidecar
}

# Same, but pointed at an explicit database (used by the missing-outbox test
# with a scratch database).
start_sidecar_db() {
	local url="$1"
	stop_sidecar 2>/dev/null || true
	nohup "$SIDECAR_BIN" \
		--config "$CONFIG_DIR/config.php" \
		--glob-config \
		--database-url "$url" \
		--listen "127.0.0.1:$SIDECAR_PORT" \
		--log-level debug \
		>"$STATE_DIR/sidecar.log" 2>&1 &
	echo $! >"$STATE_DIR/sidecar.pid"
	wait_for_sidecar
}

stop_sidecar() {
	if [ -f "$STATE_DIR/sidecar.pid" ]; then
		kill "$(cat "$STATE_DIR/sidecar.pid")" 2>/dev/null || true
		rm -f "$STATE_DIR/sidecar.pid"
	fi
}

# Authenticated curl config (never passed on the command line).
curl_auth() { curl -K "$STATE_DIR/curlrc" "$@"; }
