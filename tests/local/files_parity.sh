#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Differential check: the sidecar's native files PROPFIND against PHP's, for a
# large directory (>5000 children), on the disposable Nextcloud 33.0.5 harness.
#
# It runs the *real* client property sets, not a synthetic union:
#   web     - @nextcloud/files defaultDavProperties + the properties registered
#             by apps/files_sharing, apps/files and LivePhotos.
#   desktop - LsColJob::defaultProperties (non-root, server >= 10, no files_lock).
#
# Run ./setup.sh first. This never mutates product data outside the throwaway
# harness database.
#
#   ./files_parity.sh            # 8000 children
#   BIG=2000 ./files_parity.sh
#
# Evidence is written to state/evidence/files-parity.txt.
set -uo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"
load_env

mkdir -p "$EVIDENCE_DIR"
EVIDENCE_FILE="$EVIDENCE_DIR/files-parity.txt"
: >"$EVIDENCE_FILE"
exec > >(tee -a "$EVIDENCE_FILE") 2>&1

N="${BIG:-8000}"
DIR=ParityBig
CURLRC="$STATE_DIR/curlrc"
PHP_BASE="$NC_URL/remote.php/dav/files/alice/$DIR/"
SIDE_BASE="$SIDECAR_URL/remote.php/dav/files/alice/$DIR/"

# The exact web-UI property set (see tests/files_read_path.rs WEB_PROPS).
PROPS_WEB='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns" xmlns:ocs="http://open-collaboration-services.org/ns">
  <d:prop>
    <d:getcontentlength/><d:getcontenttype/><d:getetag/><d:getlastmodified/>
    <d:creationdate/><d:displayname/><d:quota-available-bytes/><d:resourcetype/>
    <nc:has-preview/><nc:is-encrypted/><nc:mount-type/><oc:comments-unread/>
    <oc:favorite/><oc:fileid/><oc:owner-display-name/><oc:owner-id/>
    <oc:permissions/><oc:size/><nc:note/><nc:sharees/><nc:hide-download/>
    <nc:share-attributes/><oc:share-types/><ocs:share-permissions/><nc:hidden/>
    <nc:is-mount-root/><nc:metadata-blurhash/><nc:metadata-files-live-photo/>
  </d:prop>
</d:propfind>'

# The exact desktop-client property set (see tests/files_read_path.rs DESKTOP_PROPS).
PROPS_DESKTOP='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
  <d:prop>
    <d:resourcetype/><d:getlastmodified/><d:getcontentlength/><d:getetag/>
    <d:quota-available-bytes/><d:quota-used-bytes/><oc:size/><oc:id/>
    <oc:fileid/><oc:downloadURL/><oc:dDC/><oc:permissions/><oc:checksums/>
    <nc:is-encrypted/><nc:metadata-files-live-photo/><nc:share-attributes/>
    <oc:share-types/><nc:is-mount-root/>
  </d:prop>
</d:propfind>'

sec() { printf '\n===== %s =====\n' "$*"; }

sec "preconditions"
wait_for_nextcloud >/dev/null
wait_for_sidecar
echo "sidecar: $(curl -s "$SIDECAR_URL/healthz" | head -c 200)"

sec "create $DIR ($N children) via MKCOL + SQL"
# A clean slate: drop any previous run's rows (children first).
q "DELETE FROM oc_files_metadata WHERE file_id IN (
     SELECT fileid FROM oc_filecache WHERE path LIKE 'files/$DIR/%')" >/dev/null 2>&1 || true
q "DELETE FROM oc_vcategory_to_object WHERE objid IN (
     SELECT fileid FROM oc_filecache WHERE path LIKE 'files/$DIR/%')" >/dev/null 2>&1 || true
q "DELETE FROM oc_comments WHERE object_id IN (
     SELECT fileid::text FROM oc_filecache WHERE path LIKE 'files/$DIR/%')" >/dev/null 2>&1 || true
q "DELETE FROM oc_share WHERE file_source IN (
     SELECT fileid FROM oc_filecache WHERE path LIKE 'files/$DIR/%')" >/dev/null 2>&1 || true
q "DELETE FROM oc_filecache WHERE path LIKE 'files/$DIR/%'" >/dev/null
q "DELETE FROM oc_filecache WHERE path = 'files/$DIR'" >/dev/null

MKCOL=$(curl -s -o /dev/null -w '%{http_code}' -K "$CURLRC" -X MKCOL \
	"$PHP_BASE") || true
echo "MKCOL status: $MKCOL"

q "INSERT INTO oc_mimetypes (mimetype) SELECT 'text/plain'
   WHERE NOT EXISTS (SELECT 1 FROM oc_mimetypes WHERE mimetype='text/plain')" >/dev/null
q "INSERT INTO oc_mimetypes (mimetype) SELECT 'text'
   WHERE NOT EXISTS (SELECT 1 FROM oc_mimetypes WHERE mimetype='text')" >/dev/null
STORAGE=$(q "SELECT numeric_id FROM oc_storages WHERE id='home::alice'")
DIRID=$(q "SELECT fileid FROM oc_filecache WHERE storage=$STORAGE AND path='files/$DIR'")
TXT=$(q "SELECT id FROM oc_mimetypes WHERE mimetype='text/plain'")
TXTPART=$(q "SELECT id FROM oc_mimetypes WHERE mimetype='text'")
DIRMIME=$(q "SELECT id FROM oc_mimetypes WHERE mimetype='httpd/unix-directory'")
DIRPART=$(q "SELECT id FROM oc_mimetypes WHERE mimetype='httpd'")
echo "storage=$STORAGE dirid=$DIRID"

q "INSERT INTO oc_filecache
     (storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime,
      storage_mtime, encrypted, unencrypted_size, etag, permissions, checksum)
   SELECT $STORAGE, 'files/$DIR/file-' || g, md5('files/$DIR/file-' || g), $DIRID,
          'file-' || g, $TXT, $TXTPART, 10, 1700000000, 1700000000, 0, 0,
          'e' || g, 27, NULL
   FROM generate_series(1, $N) g" >/dev/null

# A sub-directory (collection href trailing slash) and a checksummed file.
q "INSERT INTO oc_filecache
     (storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime,
      storage_mtime, encrypted, unencrypted_size, etag, permissions, checksum)
   SELECT $STORAGE, 'files/$DIR/Sub', md5('files/$DIR/Sub'), $DIRID, 'Sub',
          $DIRMIME, $DIRPART, 0, 1700000000, 1700000000, 0, 0, 'esub', 31, NULL" >/dev/null
SUBID=$(q "SELECT fileid FROM oc_filecache WHERE storage=$STORAGE AND path='files/$DIR/Sub'")
q "INSERT INTO oc_filecache
     (storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime,
      storage_mtime, encrypted, unencrypted_size, etag, permissions, checksum)
   SELECT $STORAGE, 'files/$DIR/Sub/child.txt', md5('files/$DIR/Sub/child.txt'),
          $SUBID, 'child.txt', $TXT, $TXTPART, 12, 1700000001, 1700000001, 0, 0,
          'esubchild', 27, 'SHA1:deadbeefdeadbeef'" >/dev/null
q "INSERT INTO oc_filecache
     (storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime,
      storage_mtime, encrypted, unencrypted_size, etag, permissions, checksum)
   SELECT $STORAGE, 'files/$DIR/Checksummed.bin', md5('files/$DIR/Checksummed.bin'),
          $DIRID, 'Checksummed.bin', $TXT, $TXTPART, 3, 1700000002, 1700000002, 0, 0,
          'echeck', 27, 'SHA1:cafebabecafebabe'" >/dev/null

# A favorite and an unread comment, so the bulk queries are exercised.
q "INSERT INTO oc_vcategory (uid, type, category) SELECT 'alice', 'files', '_\$!<Favorite>!\$_'
   WHERE NOT EXISTS (SELECT 1 FROM oc_vcategory WHERE uid='alice' AND type='files' AND category='_\$!<Favorite>!\$_')" >/dev/null
FAV_FILEID=$(q "SELECT fileid FROM oc_filecache WHERE storage=$STORAGE AND path='files/$DIR/file-7'")
FAV_CATID=$(q "SELECT id FROM oc_vcategory WHERE uid='alice' AND type='files' AND category='_\$!<Favorite>!\$_'")
q "INSERT INTO oc_vcategory_to_object (objid, categoryid, type) VALUES ($FAV_FILEID, $FAV_CATID, 'files')
   ON CONFLICT DO NOTHING" >/dev/null
COMMENT_FILEID=$(q "SELECT fileid FROM oc_filecache WHERE storage=$STORAGE AND path='files/$DIR/file-11'")
q "INSERT INTO oc_comments (actor_type, actor_id, message, verb, object_type, object_id, creation_timestamp)
   VALUES ('users', 'alice', 'note', 'comment', 'files', '$COMMENT_FILEID', now())" >/dev/null

# A metadata row (nc:metadata-blurhash) and a share (oc:share-types/nc:sharees),
# so the joined column and the bulk query are exercised for a real file.
META_FILEID=$(q "SELECT fileid FROM oc_filecache WHERE storage=$STORAGE AND path='files/$DIR/file-13'")
q "INSERT INTO oc_files_metadata (file_id, json, sync_token, last_update)
   VALUES ($META_FILEID, '{\"blurhash\":{\"value\":\"L0TSUAWBWB\",\"type\":\"string\"}}', 'recon', now())
   ON CONFLICT (file_id) DO NOTHING" >/dev/null
SHARE_FILEID=$(q "SELECT fileid FROM oc_filecache WHERE storage=$STORAGE AND path='files/$DIR/file-17'")
q "INSERT INTO oc_share (share_type, share_with, uid_owner, uid_initiator, item_type, file_source, permissions)
   SELECT 0, 'bob', 'alice', 'alice', 'file', $SHARE_FILEID, 19
   WHERE EXISTS (SELECT 1 FROM oc_users WHERE uid='bob')" >/dev/null
echo "seeded $(q "SELECT count(*) FROM oc_filecache WHERE storage=$STORAGE AND path LIKE 'files/$DIR/%'") children"

PARITY_OK=1

# run_set NAME PROPS
run_set() {
	local name="$1" props="$2"
	local php_xml="$STATE_DIR/php-files-$name.xml"
	local side_xml="$STATE_DIR/sidecar-files-$name.xml"

	sec "$name set: PHP PROPFIND"
	printf '%s' "$props" >"$STATE_DIR/pf-files-$name.xml"
	local php_out php_time php_code
	php_out=$(curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary @"$STATE_DIR/pf-files-$name.xml" \
		-o "$php_xml" -w '%{http_code} %{time_total}' "$PHP_BASE")
	php_code=${php_out%% *}
	php_time=${php_out##* }
	echo "php: status=$php_code time=${php_time}s size=$(wc -c <"$php_xml") bytes"

	sec "$name set: sidecar PROPFIND"
	# The sidecar only serves the app-password fast path; the token's
	# `last_check` must be fresh or it delegates to PHP (which this
	# direct-to-sidecar request cannot replay).
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	local side_out side_time side_code
	side_out=$(curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary @"$STATE_DIR/pf-files-$name.xml" \
		-o "$side_xml" -w '%{http_code} %{time_total}' "$SIDE_BASE")
	side_code=${side_out%% *}
	side_time=${side_out##* }
	echo "sidecar: status=$side_code time=${side_time}s size=$(wc -c <"$side_xml") bytes"

	if [ "$side_code" != "207" ]; then
		echo "RESULT|FILES-PARITY-$name|FAIL|sidecar returned $side_code (expected 207: delegated)"
		PARITY_OK=0
		return
	fi

	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$php_xml" \
		2>"$STATE_DIR/php-files-$name.count" >"$STATE_DIR/php-files-$name.canon"
	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$side_xml" \
		2>"$STATE_DIR/sidecar-files-$name.count" >"$STATE_DIR/sidecar-files-$name.canon"
	echo "PHP     $(cat "$STATE_DIR/php-files-$name.count")"
	echo "sidecar $(cat "$STATE_DIR/sidecar-files-$name.count")"

	sec "$name set: diff"
	if diff -u "$STATE_DIR/php-files-$name.canon" "$STATE_DIR/sidecar-files-$name.canon" \
		>"$STATE_DIR/files-$name.diff"; then
		echo "RESULT|FILES-PARITY-$name|PASS|canonical listings are identical"
		echo "PARITY($name): identical canonical listings"
	else
		echo "RESULT|FILES-PARITY-$name|FAIL|canonical listings differ"
		echo "PARITY($name): DIFFERENT ($(wc -l <"$STATE_DIR/files-$name.diff") diff lines)"
		head -40 "$STATE_DIR/files-$name.diff"
		PARITY_OK=0
	fi
	echo "TIMING|$name|php=${php_time}s|sidecar=${side_time}s"
}

run_set web "$PROPS_WEB"
run_set desktop "$PROPS_DESKTOP"

echo
echo "summary: children=$N (see TIMING lines above for per-set timings)"
echo "evidence: $EVIDENCE_FILE"

[ "$PARITY_OK" = 1 ]
