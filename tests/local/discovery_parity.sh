#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Differential check: the sidecar's native DAV discovery PROPFIND against PHP's,
# on the disposable Nextcloud 33.0.5 harness.
#
# Covers both endpoints the client discovery chain uses:
#   - PROPFIND Depth 0 /remote.php/dav/                    (the DAV root)
#   - PROPFIND Depth 0 /remote.php/dav/principals/users/alice/
#
# and both request styles:
#   - the explicit implemented property set
#   - allprop (PHP returns only {DAV:}resourcetype for these nodes)
#
# Run ./setup.sh first. It mutates only the throwaway harness database.
#
#   ./discovery_parity.sh
#
# Evidence is written to state/evidence/discovery-parity.txt.
set -uo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"
load_env

mkdir -p "$EVIDENCE_DIR"
EVIDENCE_FILE="$EVIDENCE_DIR/discovery-parity.txt"
: >"$EVIDENCE_FILE"
exec > >(tee -a "$EVIDENCE_FILE") 2>&1

CURLRC="$STATE_DIR/curlrc"
ROOT_PATH="/remote.php/dav/"
PRINCIPAL_PATH="/remote.php/dav/principals/users/alice/"

PROPS_ROOT='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:nc="http://nextcloud.com/ns" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:current-user-principal/><d:principal-collection-set/><d:resourcetype/>
    <d:supported-report-set/><d:current-user-privilege-set/>
  </d:prop>
</d:propfind>'

PROPS_PRINCIPAL='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:nc="http://nextcloud.com/ns" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:principal-URL/><d:displayname/><d:resourcetype/><d:current-user-principal/>
    <d:principal-collection-set/><d:supported-report-set/><d:current-user-privilege-set/>
    <d:owner/><d:alternate-URI-set/><d:group-membership/>
    <card:addressbook-home-set/><cal:calendar-home-set/><cal:calendar-user-address-set/>
    <cal:calendar-user-type/><nc:language/><s:email-address/>
  </d:prop>
</d:propfind>'

ALLPROP='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:"><d:allprop/></d:propfind>'

sec() { printf '\n===== %s =====\n' "$*"; }

sec "preconditions"
wait_for_nextcloud >/dev/null
wait_for_sidecar
echo "sidecar: $(curl -s "$SIDECAR_URL/healthz" | head -c 200)"

# The sidecar only serves the app-password fast path; the token's `last_check`
# must be fresh or it delegates to PHP.
sec "seed the discovery-visible account data"
q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
q "DELETE FROM oc_preferences WHERE userid='alice' AND appid='core' AND configkey='lang'" >/dev/null
q "INSERT INTO oc_preferences (userid, appid, configkey, configvalue) VALUES ('alice','core','lang','fr')" >/dev/null
q "DELETE FROM oc_preferences WHERE userid='alice' AND appid='settings' AND configkey='email'" >/dev/null
q "INSERT INTO oc_preferences (userid, appid, configkey, configvalue) VALUES ('alice','settings','email','alice@example.com')" >/dev/null
q "INSERT INTO oc_accounts (uid, data) VALUES ('alice', '{\"email\":{\"value\":\"alice@example.com\",\"scope\":\"private\"},\"additional_mail\":[{\"value\":\"alt@example.com\",\"scope\":\"private\"}]}')
   ON CONFLICT (uid) DO UPDATE SET data = EXCLUDED.data" >/dev/null
echo "seeded core/lang, settings/email and oc_accounts.additional_mail for alice"

PARITY_OK=1

# run_case NAME PATH PROPS
run_case() {
	local name="$1" path="$2" props="$3"
	local php_xml="$STATE_DIR/php-discovery-$name.xml"
	local side_xml="$STATE_DIR/sidecar-discovery-$name.xml"

	sec "$name: PHP PROPFIND"
	printf '%s' "$props" >"$STATE_DIR/pf-discovery-$name.xml"
	local php_out php_code
	php_out=$(curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 0' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary @"$STATE_DIR/pf-discovery-$name.xml" \
		-o "$php_xml" -w '%{http_code} %{time_total}' "$NC_URL$path")
	php_code=${php_out%% *}
	echo "php: status=$php_code time=${php_out##* }s size=$(wc -c <"$php_xml") bytes"

	sec "$name: sidecar PROPFIND"
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	local side_out side_code
	side_out=$(curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 0' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary @"$STATE_DIR/pf-discovery-$name.xml" \
		-o "$side_xml" -w '%{http_code} %{time_total}' "$SIDECAR_URL$path")
	side_code=${side_out%% *}
	echo "sidecar: status=$side_code time=${side_out##* }s size=$(wc -c <"$side_xml") bytes"

	if [ "$side_code" != "207" ]; then
		echo "RESULT|DISCOVERY-PARITY-$name|FAIL|sidecar returned $side_code (expected 207: delegated)"
		PARITY_OK=0
		return
	fi

	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$php_xml" \
		2>"$STATE_DIR/php-discovery-$name.count" >"$STATE_DIR/php-discovery-$name.canon"
	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$side_xml" \
		2>"$STATE_DIR/sidecar-discovery-$name.count" >"$STATE_DIR/sidecar-discovery-$name.canon"
	echo "PHP     $(cat "$STATE_DIR/php-discovery-$name.count")"
	echo "sidecar $(cat "$STATE_DIR/sidecar-discovery-$name.count")"

	sec "$name: diff"
	if diff -u "$STATE_DIR/php-discovery-$name.canon" "$STATE_DIR/sidecar-discovery-$name.canon" \
		>"$STATE_DIR/discovery-$name.diff"; then
		echo "RESULT|DISCOVERY-PARITY-$name|PASS|canonical responses are identical"
		echo "PARITY($name): identical canonical responses"
	else
		echo "RESULT|DISCOVERY-PARITY-$name|FAIL|canonical responses differ"
		echo "PARITY($name): DIFFERENT ($(wc -l <"$STATE_DIR/discovery-$name.diff") diff lines)"
		head -40 "$STATE_DIR/discovery-$name.diff"
		PARITY_OK=0
	fi
}

run_case root-props "$ROOT_PATH" "$PROPS_ROOT"
run_case principal-props "$PRINCIPAL_PATH" "$PROPS_PRINCIPAL"
run_case root-allprop "$ROOT_PATH" "$ALLPROP"
run_case principal-allprop "$PRINCIPAL_PATH" "$ALLPROP"

echo
echo "evidence: $EVIDENCE_FILE"
[ "$PARITY_OK" = 1 ]
