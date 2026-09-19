#!/usr/bin/env bash
# Stand up a disposable Nextcloud 33.0.5 + PostgreSQL 18, install the
# nextcloud_dav companion app, create a user/app-password/address book, copy the
# container's config.php to the host, build (if needed) and start the sidecar.
#
# Idempotent: safe to re-run. Recreates from scratch with teardown.sh first.
#
#   ./setup.sh            # bring everything up
#   ./setup.sh --rebuild  # force a sidecar rebuild
#
# Never prints secrets.
set -euo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"

REBUILD=0
[ "${1:-}" = "--rebuild" ] && REBUILD=1

mkdir -p "$STATE_DIR" "$CONFIG_DIR" "$EVIDENCE_DIR"
chmod 700 "$STATE_DIR"

# --- secrets -----------------------------------------------------------------
if [ ! -f "$ENV_FILE" ]; then
	{
		echo "DB_PASSWORD=$(rand_hex 24)"
		echo "ADMIN_PASSWORD=$(rand_hex 16)"
		echo "ALICE_PASSWORD=$(rand_hex 16)"
	} >"$ENV_FILE"
	chmod 600 "$ENV_FILE"
	echo "generated state/env (secrets not shown)"
fi
load_env

# --- containers --------------------------------------------------------------
echo "==> starting containers"
compose up -d

echo "==> waiting for Nextcloud install"
wait_for_nextcloud
wait_for_http
occ status | sed -n 's/^  - installed: /installed: /p; s/^  - version: /version: /p'

# --- extra config ------------------------------------------------------------
# Nextcloud loads config/*.config.php automatically; the sidecar loads the
# same file from its host-side copy via --glob-config.
write_extra_config() {
	local php="$1"
	docker exec -i -u www-data "$NC" sh -c \
		'cat > /var/www/html/config/nextcloud_dav.config.php' <<<"$php"
}
DISPATCH_CONFIG='<?php
$CONFIG = [
    "nextcloud_dav" => [
        "event_dispatch" => [
            "enabled" => true,
            "max_attempts" => 3,
            "backoff_ms" => [200, 200, 200],
            "claim_timeout_s" => 5,
            "idle_poll_ms" => 100,
        ],
    ],
];
'
write_extra_config "$DISPATCH_CONFIG"

# --- companion app -----------------------------------------------------------
# NOTE: this harness used to install a test-only OCP\Files\ISetupManager shim,
# because the app type-hinted that interface and Nextcloud 33.0.5 does not have
# it (only the private OC\Files\SetupManager). The app now resolves the setup
# manager by name at runtime, so the shim is gone and this harness exercises the
# real code path on the production version.
echo "==> installing nextcloud_dav app"
docker exec "$NC" rm -rf /var/www/html/custom_apps/nextcloud_dav
docker cp "$REPO_DIR/app/nextcloud_dav" "$NC:/var/www/html/custom_apps/nextcloud_dav"
docker exec "$NC" chown -R www-data:www-data /var/www/html/custom_apps/nextcloud_dav
occ app:enable nextcloud_dav
occ app:list 2>/dev/null | grep nextcloud_dav || true

echo "==> outbox table columns + types"
q "SELECT string_agg(column_name||' '||data_type, ',' ORDER BY ordinal_position)
     FROM information_schema.columns
    WHERE table_name = 'oc_dav_event_outbox'"
echo "expected (src/outbox.rs::OUTBOX_COLUMNS):"
printf '%s\n' "seq,created_at,event_type,addressbookid,card_uri,card_row,card_data,effects,state,attempts,next_attempt_at,reserved_by,reserved_at,processed_at,last_error"

# --- user + app password -----------------------------------------------------
echo "==> creating user alice"
if ! occ user:info alice >/dev/null 2>&1; then
	OC_PASS="$ALICE_PASSWORD" docker exec -e OC_PASS -u www-data -w /var/www/html "$NC" \
		php occ user:add --password-from-env --display-name "Alice E2E" alice >/dev/null
fi

echo "==> creating app password"
occ user:add-app-password alice >"$STATE_DIR/apppw.raw" 2>&1 || true
# Extract the token: the last whitespace-delimited field of the last non-empty
# line that looks like an opaque token.
awk '
  NF>0 { last=$0 }
  END {
    n=split(last, f, /[ \t]+/);
    print f[n];
  }' "$STATE_DIR/apppw.raw" >"$STATE_DIR/app_password"
chmod 600 "$STATE_DIR/app_password"
if [ ! -s "$STATE_DIR/app_password" ] || [ "$(wc -c <"$STATE_DIR/app_password")" -lt 20 ]; then
	echo "ERROR: could not parse an app password (occ output had $(wc -l <"$STATE_DIR/apppw.raw") lines)" >&2
	# Show the shape without the token.
	sed -E 's/[A-Za-z0-9_-]{20,}/<REDACTED>/g' "$STATE_DIR/apppw.raw" >&2 || true
	exit 1
fi
rm -f "$STATE_DIR/apppw.raw"
printf 'user = "alice:%s"\n' "$(cat "$STATE_DIR/app_password")" >"$STATE_DIR/curlrc"
chmod 600 "$STATE_DIR/curlrc"
echo "app password stored (not shown)"

# --- address book (MKCOL through Apache) -------------------------------------
echo "==> creating address book via MKCOL"
MKCOL_BODY='<?xml version="1.0" encoding="utf-8"?>
<mkcol xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <set><prop>
    <resourcetype><collection/><card:addressbook/></resourcetype>
    <displayname>E2E contacts</displayname>
  </prop></set>
</mkcol>'
code=$(curl -s -o /dev/null -w '%{http_code}' -K "$STATE_DIR/curlrc" \
	-X MKCOL -H 'Content-Type: application/xml; charset=utf-8' \
	--data-binary "$MKCOL_BODY" \
	"$NC_URL/remote.php/dav/addressbooks/users/alice/contacts/")
echo "MKCOL status: $code"
if [ "$code" != "201" ] && [ "$code" != "405" ]; then
	echo "MKCOL failed" >&2
	exit 1
fi

# --- shared address books (bob -> alice) --------------------------------------
# bob owns two books, one shared read-only and one read-write. The shares are
# seeded with SQL; the sidecar reads oc_dav_shares directly, exactly like PHP.
# The harness never needs bob's password (only alice writes to bob's books).
echo "==> creating bob + shared address books"
if ! occ user:info bob >/dev/null 2>&1; then
	BOB_PASSWORD=$(rand_hex 16)
	printf '%s' "$BOB_PASSWORD" >"$STATE_DIR/bob_password"
	chmod 600 "$STATE_DIR/bob_password"
	OC_PASS="$BOB_PASSWORD" docker exec -e OC_PASS -u www-data -w /var/www/html "$NC" \
		php occ user:add --password-from-env --display-name "Bob Shared" bob >/dev/null
fi
# Idempotent: make sure the display name the checks assert is in place.
q "UPDATE oc_users SET displayname='Bob Shared' WHERE uid='bob'" >/dev/null
for book in bobro bobrw; do
	q "INSERT INTO oc_addressbooks (principaluri, displayname, uri, description, synctoken)
	   SELECT 'principals/users/bob', 'Bob $book', '$book', NULL, 1
	   WHERE NOT EXISTS (SELECT 1 FROM oc_addressbooks
	                     WHERE principaluri='principals/users/bob' AND uri='$book')" >/dev/null
done
# access 3 = read-only, access 2 = read-write.
while read -r book access; do
	q "INSERT INTO oc_dav_shares (principaluri, type, access, resourceid)
	   SELECT 'principals/users/alice', 'addressbook', $access, a.id
	   FROM oc_addressbooks a
	   WHERE a.principaluri='principals/users/bob' AND a.uri='$book'
	     AND NOT EXISTS (SELECT 1 FROM oc_dav_shares s
	                     WHERE s.resourceid=a.id AND s.principaluri='principals/users/alice'
	                       AND s.type='addressbook')" >/dev/null
done <<'SHARES'
bobro 3
bobrw 2
SHARES
q "SELECT string_agg(uri||'->'||access, ',' ORDER BY uri)
   FROM oc_addressbooks a JOIN oc_dav_shares s ON s.resourceid=a.id
   WHERE a.principaluri='principals/users/bob' AND s.principaluri='principals/users/alice'" |
	sed 's/^/shares seeded: /'

# --- files: mount fixtures (share, groupfolder with ACL, local external) ------
# The parity run needs a received share, a groupfolder with ACLs and a local
# external mount, so the sidecar's mount model is diffed against real PHP.
echo "==> installing groupfolders + files_external"
if ! occ app:list 2>/dev/null | grep -q '"groupfolders"'; then
	GF_VERSION=21.0.15
	curl -fsSL -o /tmp/groupfolders.tar.gz \
		"https://github.com/nextcloud/groupfolders/archive/refs/tags/v${GF_VERSION}.tar.gz"
	tar -xzf /tmp/groupfolders.tar.gz -C /tmp
	docker exec "$NC" rm -rf /var/www/html/custom_apps/groupfolders
	docker cp "/tmp/groupfolders-${GF_VERSION}" "$NC:/var/www/html/custom_apps/groupfolders"
	docker exec "$NC" chown -R www-data:www-data /var/www/html/custom_apps/groupfolders
fi
occ app:enable groupfolders >/dev/null 2>&1 || true
occ app:enable files_external >/dev/null 2>&1 || true
occ app:list 2>/dev/null | grep -E 'groupfolders|files_external' || true

# A group both users share, so the groupfolder has a recipient.
occ group:add parity-team >/dev/null 2>&1 || true
occ group:adduser parity-team alice >/dev/null 2>&1 || true

# A received share: bob owns /ParityShare, shared read-write with alice.
echo "==> creating bob's shared folder"
docker exec -u www-data "$NC" mkdir -p /var/www/html/data/bob/files/ParityShare
docker exec -u www-data "$NC" sh -c 'echo "from bob" > /var/www/html/data/bob/files/ParityShare/from-bob.txt'
occ files:scan bob >/dev/null 2>&1 || true
BOB_STORAGE=$(q "SELECT numeric_id FROM oc_storages WHERE id='home::bob'")
SHARE_ROOT=$(q "SELECT fileid FROM oc_filecache WHERE storage=$BOB_STORAGE AND path='files/ParityShare'")
q "INSERT INTO oc_share
     (share_type, share_with, uid_owner, uid_initiator, item_type, file_source,
      file_target, permissions, accepted, stime)
   SELECT 0, 'alice', 'bob', 'bob', 'folder', $SHARE_ROOT, '/ParityShare', 31, 1,
          extract(epoch from now())::bigint
   WHERE NOT EXISTS (SELECT 1 FROM oc_share
                     WHERE file_source=$SHARE_ROOT AND share_with='alice')" >/dev/null

# A groupfolder with ACLs: one denied child and one denied permission.
echo "==> creating the ACL groupfolder"
occ groupfolders:create ParityTeam >/dev/null 2>&1 || true
GF_ID=$(q "SELECT folder_id FROM oc_group_folders WHERE mount_point='ParityTeam'")
q "INSERT INTO oc_group_folders_groups (folder_id, group_id, permissions)
   SELECT $GF_ID, 'parity-team', 31
   WHERE NOT EXISTS (SELECT 1 FROM oc_group_folders_groups
                     WHERE folder_id=$GF_ID AND group_id='parity-team')" >/dev/null
occ groupfolders:permissions "$GF_ID" --enable >/dev/null 2>&1 || true
# `groupfolders:create` uses a separate storage: the mount root is `files/`
# under `<datadirectory>/__groupfolders/<id>/`.
GF_DIR="/var/www/html/data/__groupfolders/$GF_ID/files"
docker exec -u www-data "$NC" mkdir -p "$GF_DIR/sub"
docker exec -u www-data "$NC" sh -c "echo a > $GF_DIR/visible.txt"
docker exec -u www-data "$NC" sh -c "echo b > $GF_DIR/hidden.txt"
docker exec -u www-data "$NC" sh -c "echo c > $GF_DIR/sub/deep.txt"
occ groupfolders:scan "$GF_ID" >/dev/null 2>&1 || true
# Deny READ on hidden.txt (the row must disappear) and DELETE on sub/.
# `--` keeps Symfony from parsing the `-read`/`-delete` permission tokens as
# options.
occ groupfolders:permissions "$GF_ID" hidden.txt -g parity-team -- -read >/dev/null 2>&1 || true
occ groupfolders:permissions "$GF_ID" sub -g parity-team -- -delete >/dev/null 2>&1 || true

# A read-only groupfolder (group permission = READ only).
echo "==> creating the read-only groupfolder"
occ groupfolders:create ParityRO >/dev/null 2>&1 || true
RO_ID=$(q "SELECT folder_id FROM oc_group_folders WHERE mount_point='ParityRO'")
q "INSERT INTO oc_group_folders_groups (folder_id, group_id, permissions)
   SELECT $RO_ID, 'parity-team', 1
   WHERE NOT EXISTS (SELECT 1 FROM oc_group_folders_groups
                     WHERE folder_id=$RO_ID AND group_id='parity-team')" >/dev/null
RO_DIR="/var/www/html/data/__groupfolders/$RO_ID/files"
docker exec -u www-data "$NC" mkdir -p "$RO_DIR"
docker exec -u www-data "$NC" sh -c "echo ro > $RO_DIR/ro.txt"
occ groupfolders:scan "$RO_ID" >/dev/null 2>&1 || true

# A read-only local external mount for alice.
echo "==> creating the local external mount"
docker exec -u www-data "$NC" mkdir -p /var/www/html/data/external-parity
EXT_ID=$(q "SELECT mount_id FROM oc_external_mounts WHERE mount_point='/ParityExt' ORDER BY mount_id DESC LIMIT 1")
if [ -z "$EXT_ID" ]; then
	EXT_ID=$(occ files_external:create /ParityExt local null::null \
		-c datadir=/var/www/html/data/external-parity 2>/dev/null |
		grep -oE 'id [0-9]+' | grep -oE '[0-9]+' | tail -1 || true)
fi
if [ -n "$EXT_ID" ]; then
	occ files_external:option "$EXT_ID" readonly 1 >/dev/null 2>&1 || true
	occ files_external:option "$EXT_ID" filesystem_check_changes 0 >/dev/null 2>&1 || true
	occ files_external:applicable --add-user alice "$EXT_ID" >/dev/null 2>&1 || true
fi
docker exec -u www-data "$NC" sh -c 'echo ext > /var/www/html/data/external-parity/ext.txt'

# Warm PHP once so `oc_mounts` is materialised before the sidecar starts.
echo "==> warming oc_mounts via PHP"
curl -s -o /dev/null -K "$STATE_DIR/curlrc" -X PROPFIND -H 'Depth: 1' \
	-H 'Content-Type: application/xml; charset=utf-8' \
	--data-binary '<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:propfind>' \
	"$NC_URL/remote.php/dav/files/alice/" || true
q "SELECT count(*) FROM oc_mounts WHERE user_id='alice'" | sed 's/^/oc_mounts rows for alice: /'

# --- copy config to the host sidecar -----------------------------------------
# The sidecar gets a *copy* of the container's config.php plus two sibling
# config files: the shared dispatch block and a dbhost/dbport override that
# points at the published Postgres port. Nextcloud never sees the override.
echo "==> copying container config.php to host"
docker cp "$NC:/var/www/html/config/config.php" "$CONFIG_DIR/config.php"
chmod 600 "$CONFIG_DIR/config.php"
# The sidecar resolves the `dav` app's l10n files from the server root derived
# from config_path (`<root>/apps/dav/l10n`). Copy the real tree so the
# localized calendar displaynames can be reproduced on the host.
echo "==> copying dav l10n tree to host"
rm -rf "$STATE_DIR/apps/dav/l10n"
mkdir -p "$STATE_DIR/apps/dav"
docker cp "$NC:/var/www/html/apps/dav/l10n" "$STATE_DIR/apps/dav/" >/dev/null
printf '%s\n' "$DISPATCH_CONFIG" >"$CONFIG_DIR/nextcloud_dav.config.php"
printf '<?php\n$CONFIG = ["dbhost" => "127.0.0.1", "dbport" => %s];\n' "$PG_PORT" \
	>"$CONFIG_DIR/sidecar-db.config.php"

# --- build sidecar -----------------------------------------------------------
if [ "$REBUILD" = 1 ] || [ ! -x "$SIDECAR_BIN" ]; then
	echo "==> building sidecar (release)"
	(cd "$REPO_DIR" && CARGO_BUILD_JOBS=6 nice -n 19 \
		nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config nixpkgs#cmake nixpkgs#openssl \
		-c cargo build --release --locked)
fi

# --- start sidecar -----------------------------------------------------------
echo "==> starting sidecar on $SIDECAR_URL"
start_sidecar
echo "==> sidecar log:"
grep -E 'starting nextcloud-dav|event outbox|listening on' "$STATE_DIR/sidecar.log" || true

echo
echo "setup complete."
echo "  sidecar : $SIDECAR_URL"
echo "  nextcloud: $NC_URL"
echo "  evidence: run ./e2e.sh"
