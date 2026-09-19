#!/usr/bin/env bash
# Tear down the disposable environment: stop the sidecar, remove the
# containers, the named volumes (database + Nextcloud install) and the network.
# The generated secrets in state/ are removed too.
set -euo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"

# Compose interpolation needs the passwords even when only tearing down.
if [ -f "$ENV_FILE" ]; then
	load_env
else
	export DB_PASSWORD=unused ADMIN_PASSWORD=unused REDIS_PASSWORD=unused
fi

stop_sidecar 2>/dev/null || true
compose down -v --remove-orphans || true
rm -rf "$STATE_DIR"
echo "torn down: containers, volumes, network and state/ removed"
