#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Differential check: the sidecar's native CalDAV calendar-home and
# per-calendar PROPFIND against PHP's, on the disposable Nextcloud 33.0.5
# harness.
#
# Run ./setup.sh first. This never mutates product data outside the throwaway
# harness database.
#
# The nginx location that routes this subtree to the sidecar in production is:
#
#   location ~ ^/remote\.php/dav/calendars/[^/]+/. {
#       proxy_pass http://127.0.0.1:7868;
#       proxy_http_version 1.1;
#       proxy_set_header Host              $host;
#       proxy_set_header X-Real-IP         $remote_addr;
#       proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
#       proxy_set_header X-Forwarded-Proto $scheme;
#       proxy_set_header Authorization     $http_authorization;
#       client_max_body_size 10m;
#       proxy_read_timeout 300s;
#       proxy_intercept_errors on;
#       error_page 501 502 504 = @nextcloud_dav_php;
#   }
#
# The regex requires at least one segment after `calendars/<u>/`, so the home
# (`/calendars/<u>/`) also reaches the sidecar while `/calendars/<u>` (no
# trailing slash) stays on PHP. The local harness drives the sidecar directly,
# so it exercises the same paths without nginx.
#
# Evidence is written to state/evidence/caldav-parity.txt.
set -uo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"
load_env

mkdir -p "$EVIDENCE_DIR"
EVIDENCE_FILE="$EVIDENCE_DIR/caldav-parity.txt"
: >"$EVIDENCE_FILE"
exec > >(tee -a "$EVIDENCE_FILE") 2>&1

CURLRC="$STATE_DIR/curlrc"
PHP_BASE="$NC_URL/remote.php/dav/calendars"
SIDE_BASE="$SIDECAR_URL/remote.php/dav/calendars"
PARITY_OK=1

sec() { printf '\n===== %s =====\n' "$*"; }

wait_for_nextcloud >/dev/null
wait_for_sidecar

# --- fixture: a clean, known calendar set ------------------------------------
# The disposable DB may carry rows from an earlier manual session; replace them
# with a deterministic set (owned calendars, the two PHP-localized special ones,
# and one read-only share with a sharee override).
sec "fixture"
# The sidecar reads the `dav` app's l10n files from `<config-root>/apps/dav/l10n`.
# Make sure the real tree is present (setup.sh copies it; re-assert here so a
# rerun against an older sidecar start still works).
if [ ! -f "$STATE_DIR/apps/dav/l10n/fr.json" ]; then
	mkdir -p "$STATE_DIR/apps/dav"
	docker cp "$NC:/var/www/html/apps/dav/l10n" "$STATE_DIR/apps/dav/" >/dev/null 2>&1 || true
fi
# French, so the `personal`/`contact_birthdays` displayname rewrite is visible
# (in English it is the identity).
q "DELETE FROM oc_preferences WHERE userid='alice' AND appid='core' AND configkey='lang'" >/dev/null
q "INSERT INTO oc_preferences (userid, appid, configkey, configvalue) VALUES ('alice','core','lang','fr')" >/dev/null
q "DELETE FROM oc_properties WHERE propertypath LIKE 'calendars/%'" >/dev/null
q "DELETE FROM oc_dav_shares WHERE type = 'calendar'" >/dev/null
q "DELETE FROM oc_calendarchanges WHERE calendarid IN (SELECT id FROM oc_calendars WHERE principaluri IN ('principals/users/alice','principals/users/bob'))" >/dev/null
q "DELETE FROM oc_calendarobjects WHERE calendarid IN (SELECT id FROM oc_calendars WHERE principaluri IN ('principals/users/alice','principals/users/bob'))" >/dev/null
q "DELETE FROM oc_calendars WHERE principaluri IN ('principals/users/alice','principals/users/bob')" >/dev/null
q "INSERT INTO oc_calendars (principaluri, uri, displayname, calendarorder, calendarcolor, components, transparent, synctoken)
   VALUES ('principals/users/alice','home','Home',1,NULL,'VEVENT,VTODO',1,1),
          ('principals/users/alice','work','Work',2,'#111111','VEVENT',0,7),
          ('principals/users/alice','personal','Personal',10,NULL,'VEVENT',0,1),
          ('principals/users/alice','contact_birthdays','Contact birthdays',11,NULL,'VEVENT',0,1),
          ('principals/users/bob','bobcal','Bob Cal',5,'#ff0000','VEVENT',0,3)" >/dev/null
BOB_CAL=$(q "SELECT id FROM oc_calendars WHERE principaluri='principals/users/bob' AND uri='bobcal'")
WORK_CAL=$(q "SELECT id FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='work'")
q "INSERT INTO oc_dav_shares (principaluri, type, access, resourceid)
   VALUES ('principals/users/alice','calendar',3,$BOB_CAL)" >/dev/null
q "INSERT INTO oc_properties (userid, propertypath, propertyname, propertyvalue, valuetype)
   VALUES ('alice','calendars/alice/bobcal_shared_by_bob','{DAV:}displayname','My Bob Cal',1),
          ('alice','calendars/alice/bobcal_shared_by_bob','{http://apple.com/ns/ical/}calendar-color','#123456',1),
          ('alice','calendars/alice/work','{http://owncloud.org/ns}calendar-enabled','0',1)" >/dev/null
q "UPDATE oc_users SET displayname='Alice E2E' WHERE uid='alice'" >/dev/null
q "UPDATE oc_users SET displayname='Bob Shared' WHERE uid='bob'" >/dev/null

# Two throwaway calendar objects in `work`, with the pre-increment change
# convention: both logged at token 7, the calendar token becomes 8.
EV1=$'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//e2e//EN\r\nBEGIN:VEVENT\r\nUID:e2e-1\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260101T100000Z\r\nDTEND:20260101T110000Z\r\nSUMMARY:Parity One\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n'
EV2=$'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//e2e//EN\r\nBEGIN:VEVENT\r\nUID:e2e-2\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260201T100000Z\r\nDTEND:20260201T110000Z\r\nSUMMARY:Parity Two\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n'
q "INSERT INTO oc_calendarobjects (calendarid, uri, calendardata, lastmodified, etag, size, componenttype, classification, calendartype)
   VALUES ($WORK_CAL,'p1.ics',convert_to(\$q\$$EV1\$q\$,'UTF8'),1700000000,md5(convert_to(\$q\$$EV1\$q\$,'UTF8')),length(convert_to(\$q\$$EV1\$q\$,'UTF8')),'VEVENT',0,0),
          ($WORK_CAL,'p2.ics',convert_to(\$q\$$EV2\$q\$,'UTF8'),1700000001,md5(convert_to(\$q\$$EV2\$q\$,'UTF8')),length(convert_to(\$q\$$EV2\$q\$,'UTF8')),'VEVENT',0,0)" >/dev/null
q "INSERT INTO oc_calendarchanges (uri, synctoken, calendarid, operation, calendartype, created_at)
   VALUES ('p1.ics',7,$WORK_CAL,1,0,1700000000),('p2.ics',7,$WORK_CAL,1,0,1700000001)" >/dev/null
q "UPDATE oc_calendars SET synctoken=8 WHERE id=$WORK_CAL" >/dev/null
echo "seeded $(q "SELECT count(*) FROM oc_calendars WHERE principaluri LIKE 'principals/users/%'") calendars, $(q "SELECT count(*) FROM oc_calendarobjects WHERE calendarid=$WORK_CAL") objects"

# --- request bodies ----------------------------------------------------------
HOME_PROPS='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns" xmlns:apple="http://apple.com/ns/ical/" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:resourcetype/><d:displayname/><d:owner/><d:current-user-principal/>
    <d:supported-report-set/><d:supported-method-set/>
    <cs:getctag/><cal:supported-calendar-component-set/><cal:schedule-calendar-transp/>
    <cal:calendar-description/><cal:calendar-timezone/><cal:max-resource-size/>
    <cal:supported-calendar-data/><cal:supported-collation-set/>
    <oc:owner-principal/><oc:read-only/><oc:calendar-enabled/><nc:owner-displayname/>
    <nc:trash-bin-retention-duration/><nc:disable-alarm-notifications/>
    <apple:calendar-color/><apple:calendar-order/><s:sync-token/><d:sync-token/>
  </d:prop>
</d:propfind>'

CAL_PROPS='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns" xmlns:apple="http://apple.com/ns/ical/" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:resourcetype/><d:displayname/><d:owner/><d:current-user-principal/>
    <d:current-user-privilege-set/><d:acl/><d:supported-report-set/><d:supported-method-set/>
    <d:sync-token/><cs:getctag/><cs:allowed-sharing-modes/>
    <cal:supported-calendar-component-set/><cal:schedule-calendar-transp/>
    <cal:calendar-description/><cal:calendar-timezone/><cal:max-resource-size/>
    <cal:supported-calendar-data/><cal:supported-collation-set/>
    <oc:owner-principal/><oc:read-only/><oc:invite/><oc:calendar-enabled/>
    <nc:owner-displayname/><nc:disable-alarm-notifications/>
    <apple:calendar-color/><apple:calendar-order/><s:sync-token/>
  </d:prop>
</d:propfind>'

printf '%s' "$HOME_PROPS" >"$STATE_DIR/pf-caldav-home.xml"
printf '%s' "$CAL_PROPS" >"$STATE_DIR/pf-caldav-cal.xml"

# The French-localization case only needs the displayname.
DISPLAY_PROPS='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:displayname/></d:prop>
</d:propfind>'

# assert_attribution NAME KIND: the sidecar stamps `x-nextcloud-dav: sidecar`
# on bodies it produces and never on the 501 it delegates. A case that
# delegated must fail even if the canonical diff would pass (PHP vs PHP).
assert_attribution() {
	local name="$1"
	local side_headers="$STATE_DIR/sidecar-caldav-$name.headers"
	local php_headers="$STATE_DIR/php-caldav-$name.headers"
	if grep -qi '^x-nextcloud-dav:[[:space:]]*sidecar' "$side_headers" 2>/dev/null; then
		echo "ATTRIBUTION|$name|sidecar"
	else
		echo "ATTRIBUTION|$name|php/delegated"
		echo "RESULT|CALDAV-PARITY-$name|FAIL|sidecar did not serve this case (no x-nextcloud-dav: sidecar header)"
		PARITY_OK=0
	fi
	if grep -qi '^x-nextcloud-dav:' "$php_headers" 2>/dev/null; then
		echo "RESULT|CALDAV-PARITY-$name|FAIL|the PHP response carried the sidecar header"
		PARITY_OK=0
	fi
}

# run_case NAME DEPTH RELPATH BODY
run_case() {
	local name="$1" depth="$2" rel="$3" body="$4"
	local php_xml="$STATE_DIR/php-caldav-$name.xml"
	local side_xml="$STATE_DIR/sidecar-caldav-$name.xml"

	sec "$name: PHP PROPFIND (Depth $depth, $rel)"
	local php_out php_time php_code
	php_out=$(curl -s -K "$CURLRC" -X PROPFIND -H "Depth: $depth" \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$body" -D "$STATE_DIR/php-caldav-$name.headers" \
		-o "$php_xml" -w '%{http_code} %{time_total}' "$PHP_BASE/$rel")
	php_code=${php_out%% *}; php_time=${php_out##* }
	echo "php: status=$php_code time=${php_time}s size=$(wc -c <"$php_xml") bytes"

	sec "$name: sidecar PROPFIND"
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	local side_out side_time side_code
	side_out=$(curl -s -K "$CURLRC" -X PROPFIND -H "Depth: $depth" \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$body" -D "$STATE_DIR/sidecar-caldav-$name.headers" \
		-o "$side_xml" -w '%{http_code} %{time_total}' "$SIDE_BASE/$rel")
	side_code=${side_out%% *}; side_time=${side_out##* }
	echo "sidecar: status=$side_code time=${side_time}s size=$(wc -c <"$side_xml") bytes"
	assert_attribution "$name"

	if [ "$php_code" != "207" ] || [ "$side_code" != "207" ]; then
		echo "RESULT|CALDAV-PARITY-$name|FAIL|php=$php_code sidecar=$side_code (expected 207)"
		PARITY_OK=0
		return
	fi

	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$php_xml" \
		2>"$STATE_DIR/php-caldav-$name.count" >"$STATE_DIR/php-caldav-$name.canon"
	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$side_xml" \
		2>"$STATE_DIR/sidecar-caldav-$name.count" >"$STATE_DIR/sidecar-caldav-$name.canon"
	echo "PHP     $(cat "$STATE_DIR/php-caldav-$name.count")"
	echo "sidecar $(cat "$STATE_DIR/sidecar-caldav-$name.count")"

	sec "$name: diff"
	if diff -u "$STATE_DIR/php-caldav-$name.canon" "$STATE_DIR/sidecar-caldav-$name.canon" \
		>"$STATE_DIR/caldav-$name.diff"; then
		echo "RESULT|CALDAV-PARITY-$name|PASS|canonical listings are identical"
		echo "PARITY($name): identical canonical listings"
	else
		echo "RESULT|CALDAV-PARITY-$name|FAIL|canonical listings differ"
		echo "PARITY($name): DIFFERENT ($(wc -l <"$STATE_DIR/caldav-$name.diff") diff lines)"
		head -40 "$STATE_DIR/caldav-$name.diff"
		PARITY_OK=0
	fi
	echo "TIMING|$name|php=${php_time}s|sidecar=${side_time}s"
}

# assert_delegated NAME METHOD RELPATH [BODY]
assert_delegated() {
	local name="$1" method="$2" rel="$3" body="${4:-}"
	sec "$name: sidecar must delegate"
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	local args=(-s -K "$CURLRC" -X "$method" -D "$STATE_DIR/sidecar-caldav-$name.headers" -o /dev/null -w '%{http_code}')
	[ -n "$body" ] && args+=(-H 'Content-Type: application/xml; charset=utf-8' --data-binary "$body")
	local code
	code=$(curl "${args[@]}" "$SIDE_BASE/$rel")
	echo "sidecar $method $rel -> $code"
	if [ "$code" = "501" ] && ! grep -qi '^x-nextcloud-dav:' "$STATE_DIR/sidecar-caldav-$name.headers" 2>/dev/null; then
		echo "RESULT|CALDAV-DELEGATED-$name|PASS|501 with no sidecar header"
	else
		echo "RESULT|CALDAV-DELEGATED-$name|FAIL|expected a bare 501, got $code"
		PARITY_OK=0
	fi
}

run_case home-d0 0 "alice/" "$HOME_PROPS"
run_case home-d1 1 "alice/" "$HOME_PROPS"
run_case cal-owned-d0 0 "alice/work/" "$CAL_PROPS"
run_case cal-owned-home-d0 0 "alice/home/" "$CAL_PROPS"
run_case cal-shared-d0 0 "alice/bobcal_shared_by_bob/" "$CAL_PROPS"

# PHP localizes the `personal` and `contact_birthdays` displaynames
# (`Calendar::__construct`). With core/lang=fr they must be `Personnel` and
# `Anniversaires des contacts` on both backends; the canonical diff plus the
# attribution header prove the sidecar (not a PHP replay) produced them.
run_case home-fr-d1 1 "alice/" "$DISPLAY_PROPS"
run_case cal-personal-fr 0 "alice/personal/" "$DISPLAY_PROPS"
run_case cal-birthday-fr 0 "alice/contact_birthdays/" "$DISPLAY_PROPS"

# A canonical diff alone would also pass if *both* backends returned the English
# identity. Assert the French strings are actually present on each side.
assert_french() {
	local name="$1" side_file="$2" php_file="$3" needle="$4"
	if grep -qF "<d:displayname>$needle</d:displayname>" "$side_file" \
		&& grep -qF "<d:displayname>$needle</d:displayname>" "$php_file"; then
		echo "RESULT|CALDAV-FR-$name|PASS|both backends served '$needle'"
	else
		echo "RESULT|CALDAV-FR-$name|FAIL|expected '$needle' in both responses"
		PARITY_OK=0
	fi
}
assert_french personal-fr "$STATE_DIR/sidecar-caldav-cal-personal-fr.xml" \
	"$STATE_DIR/php-caldav-cal-personal-fr.xml" "Personnel"
assert_french birthday-fr "$STATE_DIR/sidecar-caldav-cal-birthday-fr.xml" \
	"$STATE_DIR/php-caldav-cal-birthday-fr.xml" "Anniversaires des contacts"

# --- CalDAV REPORTs (sync-collection, calendar-multiget) ---------------------
# The request shape and the PHP-side canonical response are compared per case;
# `assert_attribution` fails a case the sidecar delegated (PHP vs PHP).
run_report_case() {
	local name="$1" rel="$2" body="$3"
	local php_xml="$STATE_DIR/php-caldav-$name.xml"
	local side_xml="$STATE_DIR/sidecar-caldav-$name.xml"

	sec "$name: PHP REPORT ($rel)"
	local php_out php_code
	php_out=$(curl -s -K "$CURLRC" -X REPORT -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$body" -D "$STATE_DIR/php-caldav-$name.headers" \
		-o "$php_xml" -w '%{http_code} %{time_total}' "$PHP_BASE/$rel")
	php_code=${php_out%% *}
	echo "php: status=$php_code size=$(wc -c <"$php_xml") bytes"

	sec "$name: sidecar REPORT"
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	local side_out side_code
	side_out=$(curl -s -K "$CURLRC" -X REPORT -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$body" -D "$STATE_DIR/sidecar-caldav-$name.headers" \
		-o "$side_xml" -w '%{http_code} %{time_total}' "$SIDE_BASE/$rel")
	side_code=${side_out%% *}
	echo "sidecar: status=$side_code size=$(wc -c <"$side_xml") bytes"
	assert_attribution "$name"

	if [ "$php_code" != "207" ] || [ "$side_code" != "207" ]; then
		echo "RESULT|CALDAV-REPORT-$name|FAIL|php=$php_code sidecar=$side_code (expected 207)"
		PARITY_OK=0
		return
	fi

	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$php_xml" \
		2>"$STATE_DIR/php-caldav-$name.count" >"$STATE_DIR/php-caldav-$name.canon"
	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$side_xml" \
		2>"$STATE_DIR/sidecar-caldav-$name.count" >"$STATE_DIR/sidecar-caldav-$name.canon"
	echo "PHP     $(cat "$STATE_DIR/php-caldav-$name.count")"
	echo "sidecar $(cat "$STATE_DIR/sidecar-caldav-$name.count")"

	sec "$name: diff"
	if diff -u "$STATE_DIR/php-caldav-$name.canon" "$STATE_DIR/sidecar-caldav-$name.canon" \
		>"$STATE_DIR/caldav-$name.diff"; then
		echo "RESULT|CALDAV-REPORT-$name|PASS|canonical responses are identical"
	else
		echo "RESULT|CALDAV-REPORT-$name|FAIL|canonical responses differ"
		head -40 "$STATE_DIR/caldav-$name.diff"
		PARITY_OK=0
	fi
}

# Compares a Sabre `{DAV:}error` body against PHP's for a REPORT that must
# fail identically (invalid/unknown token, initial+limit, missing elements).
assert_error_case() {
	local name="$1" rel="$2" body="$3" expect="$4"
	local php_xml="$STATE_DIR/php-caldav-$name.xml"
	local side_xml="$STATE_DIR/sidecar-caldav-$name.xml"

	sec "$name: PHP REPORT (expected $expect)"
	local php_code side_code
	php_code=$(curl -s -K "$CURLRC" -X REPORT -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$body" -D "$STATE_DIR/php-caldav-$name.headers" \
		-o "$php_xml" -w '%{http_code}' "$PHP_BASE/$rel")
	echo "php: status=$php_code"

	sec "$name: sidecar REPORT (expected $expect)"
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	side_code=$(curl -s -K "$CURLRC" -X REPORT -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$body" -D "$STATE_DIR/sidecar-caldav-$name.headers" \
		-o "$side_xml" -w '%{http_code}' "$SIDE_BASE/$rel")
	echo "sidecar: status=$side_code"
	assert_attribution "$name"

	if [ "$php_code" != "$expect" ] || [ "$side_code" != "$expect" ]; then
		echo "RESULT|CALDAV-REPORT-$name|FAIL|php=$php_code sidecar=$side_code (expected $expect)"
		PARITY_OK=0
		return
	fi

	python3 -c 'import sys, xml.etree.ElementTree as ET
r = ET.parse(sys.argv[1]).getroot()
for c in sorted(r, key=lambda e: e.tag):
    if c.tag.endswith("}exception") or c.tag.endswith("}message"):
        print(c.tag + "\t" + (c.text or "").strip())
    else:
        print(c.tag)' "$php_xml" >"$STATE_DIR/php-caldav-$name.canon"
	python3 -c 'import sys, xml.etree.ElementTree as ET
r = ET.parse(sys.argv[1]).getroot()
for c in sorted(r, key=lambda e: e.tag):
    if c.tag.endswith("}exception") or c.tag.endswith("}message"):
        print(c.tag + "\t" + (c.text or "").strip())
    else:
        print(c.tag)' "$side_xml" >"$STATE_DIR/sidecar-caldav-$name.canon"

	sec "$name: diff"
	if diff -u "$STATE_DIR/php-caldav-$name.canon" "$STATE_DIR/sidecar-caldav-$name.canon" \
		>"$STATE_DIR/caldav-$name.diff"; then
		echo "RESULT|CALDAV-REPORT-$name|PASS|error bodies are identical"
	else
		echo "RESULT|CALDAV-REPORT-$name|FAIL|error bodies differ"
		cat "$STATE_DIR/caldav-$name.diff"
		PARITY_OK=0
	fi
}

# The multistatus-level `{DAV:}sync-token` of a captured response.
php_token() {
	python3 -c 'import sys, xml.etree.ElementTree as ET
print((ET.parse(sys.argv[1]).getroot().find("{DAV:}sync-token").text or ""))' "$1"
}

sync_body() {
	printf '<d:sync-collection xmlns:d="DAV:"><d:sync-token>%s</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/><d:resourcetype/></d:prop></d:sync-collection>' "$1"
}

SYNC_INITIAL=$(sync_body '')
MULTIGET='<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><d:getcontenttype/><d:getcontentlength/><d:getlastmodified/><d:resourcetype/><c:calendar-data/></d:prop><d:href>/remote.php/dav/calendars/alice/work/p1.ics</d:href><d:href>/remote.php/dav/calendars/alice/work/missing.ics</d:href></c:calendar-multiget>'

run_report_case report-sync-initial "alice/work/" "$SYNC_INITIAL"
run_report_case report-multiget "alice/work/" "$MULTIGET"

# The failure modes must match PHP exactly (status + `<d:error>` body).
assert_error_case report-sync-invalid "alice/work/" \
	'<d:sync-collection xmlns:d="DAV:"><d:sync-token>bogus</d:sync-token><d:prop><d:getetag/></d:prop></d:sync-collection>' 403
assert_error_case report-sync-limit "alice/work/" \
	'<d:sync-collection xmlns:d="DAV:"><d:sync-token/><d:limit><d:nresults>1</d:nresults></d:limit><d:prop><d:getetag/></d:prop></d:sync-collection>' 507
assert_error_case report-sync-missing-token "alice/work/" \
	'<d:sync-collection xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:sync-collection>' 400
assert_error_case report-sync-missing-prop "alice/work/" \
	'<d:sync-collection xmlns:d="DAV:"><d:sync-token/></d:sync-collection>' 400

# --- incremental round trip through PHP --------------------------------------
# Create/modify/delete a throwaway event with PHP, then confirm the sidecar's
# `sync-collection` reports exactly the same changes PHP does at each step.
sec "incremental round trip (create/modify/delete through PHP)"
RT_BASE="$PHP_BASE/alice/work"
RT_EVENT=$'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//e2e//EN\r\nBEGIN:VEVENT\r\nUID:e2e-roundtrip\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260301T100000Z\r\nDTEND:20260301T110000Z\r\nSUMMARY:Round Trip\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n'
RT_EVENT2=$'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//e2e//EN\r\nBEGIN:VEVENT\r\nUID:e2e-roundtrip\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260301T100000Z\r\nDTEND:20260301T110000Z\r\nSUMMARY:Round Trip Edited\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n'

q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
curl -s -K "$CURLRC" -X REPORT -H 'Depth: 1' -H 'Content-Type: application/xml; charset=utf-8' \
	--data-binary "$SYNC_INITIAL" -o "$STATE_DIR/php-caldav-rt0.xml" "$PHP_BASE/alice/work/"
RT_TOKEN=$(php_token "$STATE_DIR/php-caldav-rt0.xml")
echo "token before create: $RT_TOKEN"

sec "round trip: create via PHP PUT"
curl -s -K "$CURLRC" -X PUT -H 'Content-Type: text/calendar; charset=utf-8' \
	--data-binary "$RT_EVENT" -o /dev/null -w 'PUT create -> %{http_code}\n' "$RT_BASE/rt.ics"
run_report_case rt-add "alice/work/" "$(sync_body "$RT_TOKEN")"
RT_TOKEN=$(php_token "$STATE_DIR/php-caldav-rt-add.xml")

sec "round trip: modify via PHP PUT"
curl -s -K "$CURLRC" -X PUT -H 'Content-Type: text/calendar; charset=utf-8' \
	--data-binary "$RT_EVENT2" -o /dev/null -w 'PUT modify -> %{http_code}\n' "$RT_BASE/rt.ics"
run_report_case rt-modify "alice/work/" "$(sync_body "$RT_TOKEN")"
RT_TOKEN=$(php_token "$STATE_DIR/php-caldav-rt-modify.xml")

sec "round trip: delete via PHP DELETE"
curl -s -K "$CURLRC" -X DELETE -o /dev/null -w 'DELETE -> %{http_code}\n' "$RT_BASE/rt.ics"
run_report_case rt-delete "alice/work/" "$(sync_body "$RT_TOKEN")"

# Clean the throwaway data back up.
q "DELETE FROM oc_calendarchanges WHERE calendarid=$WORK_CAL" >/dev/null
q "DELETE FROM oc_calendarobjects WHERE calendarid=$WORK_CAL" >/dev/null

assert_delegated object-propfind PROPFIND "alice/work/event.ics" "$CAL_PROPS"
assert_delegated trashbin-propfind PROPFIND "alice/trashbin/" "$HOME_PROPS"
assert_delegated get-object GET "alice/work/event.ics"
assert_delegated calendar-query REPORT "alice/work/" '<cal:calendar-query xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/></d:prop><cal:filter><cal:comp-filter name="VCALENDAR"/></cal:filter></cal:calendar-query>'

# --- fail-safe: a missing l10n tree must delegate, not serve English --------
# The production sidecar mounts only the PVC's config/custom_apps subpaths, so
# `/var/www/html/apps/dav/l10n` does not exist there. Without the tree the two
# special displaynames cannot be translated; serving the stored English string
# would be a silent wrong answer. The listing must be delegated (501, no
# sidecar header) so nginx replays it to PHP, and the sidecar must warn once.
sec "fail-safe: missing l10n tree delegates the home listing"
stop_sidecar
rm -rf "$STATE_DIR/apps/dav/l10n.failsafe-bak"
mv "$STATE_DIR/apps/dav/l10n" "$STATE_DIR/apps/dav/l10n.failsafe-bak"
start_sidecar

failsafe_probe() {
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$DISPLAY_PROPS" -D "$STATE_DIR/sidecar-caldav-failsafe.headers" \
		-o "$STATE_DIR/sidecar-caldav-failsafe.xml" -w '%{http_code}' "$SIDE_BASE/alice/"
}
FAILSAFE_CODE=$(failsafe_probe)
echo "sidecar PROPFIND alice/ with no l10n tree -> $FAILSAFE_CODE"
if [ "$FAILSAFE_CODE" = "501" ] \
	&& ! grep -qi '^x-nextcloud-dav:' "$STATE_DIR/sidecar-caldav-failsafe.headers" 2>/dev/null; then
	echo "RESULT|CALDAV-FAILSAFE-MISSING-L10N|PASS|501 with no sidecar header"
else
	echo "RESULT|CALDAV-FAILSAFE-MISSING-L10N|FAIL|expected a bare 501, got $FAILSAFE_CODE"
	PARITY_OK=0
fi
# A second request must not add another warning, and the warning must name the
# missing path (so this cannot regress unnoticed).
WARN_AFTER_1=$(grep -c 'dav l10n source unavailable' "$STATE_DIR/sidecar.log" 2>/dev/null || true)
failsafe_probe >/dev/null
WARN_AFTER_2=$(grep -c 'dav l10n source unavailable' "$STATE_DIR/sidecar.log" 2>/dev/null || true)
if [ "${WARN_AFTER_1:-0}" -ge 1 ] && [ "${WARN_AFTER_1:-0}" = "${WARN_AFTER_2:-0}" ] \
	&& grep -q 'apps/dav/l10n' "$STATE_DIR/sidecar.log"; then
	echo "RESULT|CALDAV-FAILSAFE-WARNING|PASS|warned once ($WARN_AFTER_1) naming the missing path"
else
	echo "RESULT|CALDAV-FAILSAFE-WARNING|FAIL|warning count $WARN_AFTER_1 -> $WARN_AFTER_2, or path not named"
	PARITY_OK=0
fi
# Restore the tree and the normal sidecar so the harness ends in its start state.
stop_sidecar
rm -rf "$STATE_DIR/apps/dav/l10n"
mv "$STATE_DIR/apps/dav/l10n.failsafe-bak" "$STATE_DIR/apps/dav/l10n"
start_sidecar

echo
echo "evidence: $EVIDENCE_FILE"
[ "$PARITY_OK" = 1 ]
