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
# The sidecar is driven directly at :$SIDECAR_PORT, but production reaches it
# through nginx which preserves `Host`. Sending the Nextcloud authority makes
# the absolute `{cs}publish-url` identical to PHP's.
NC_HOST="${NC_URL#*://}"
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
q "DELETE FROM oc_calendarobjects WHERE calendarid IN (SELECT id FROM oc_calendarsubscriptions WHERE principaluri IN ('principals/users/alice','principals/users/bob'))" >/dev/null
q "DELETE FROM oc_calendarsubscriptions WHERE principaluri IN ('principals/users/alice','principals/users/bob')" >/dev/null
q "DELETE FROM oc_calendars WHERE principaluri IN ('principals/users/alice','principals/users/bob')" >/dev/null
q "INSERT INTO oc_calendars (principaluri, uri, displayname, calendarorder, calendarcolor, components, transparent, synctoken)
   VALUES ('principals/users/alice','home','Home',1,NULL,'VEVENT,VTODO',1,1),
          ('principals/users/alice','work','Work',2,'#111111','VEVENT',0,7),
          ('principals/users/alice','personal','Personal',10,NULL,'VEVENT',0,1),
          ('principals/users/alice','contact_birthdays','Contact birthdays',11,NULL,'VEVENT',0,1),
          ('principals/users/bob','bobcal','Bob Cal',5,'#ff0000','VEVENT',0,3)" >/dev/null
BOB_CAL=$(q "SELECT id FROM oc_calendars WHERE principaluri='principals/users/bob' AND uri='bobcal'")
WORK_CAL=$(q "SELECT id FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='work'")
HOME_CAL=$(q "SELECT id FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='home'")
q "INSERT INTO oc_dav_shares (principaluri, type, access, resourceid)
   VALUES ('principals/users/alice','calendar',3,$BOB_CAL)" >/dev/null
# Two published calendars (`access = 4`), mirroring production's two rows for
# the test account, so both `{cs}publish-url` branches are testable against
# real PHP. `work` also carries outgoing shares - a read-write direct user
# share and a read-only group share - which make `{oc}invite` non-empty on an
# owned calendar (PHP's `CalDavBackend::setPublishStatus` writes the owner as
# an `access=4` row, so the published calendars' own invite is non-empty too).
q "INSERT INTO oc_groups (gid, displayname) VALUES ('parity-team','Parity Team')
   ON CONFLICT (gid) DO UPDATE SET displayname='Parity Team'" >/dev/null
q "INSERT INTO oc_dav_shares (principaluri, type, access, resourceid, publicuri)
   VALUES ('principals/users/alice','calendar',4,$WORK_CAL,'pubtoken-work'),
          ('principals/users/alice','calendar',4,$HOME_CAL,'pubtoken-home'),
          ('principals/users/bob','calendar',2,$WORK_CAL,NULL),
          ('principals/groups/parity-team','calendar',3,$WORK_CAL,NULL)" >/dev/null
q "INSERT INTO oc_properties (userid, propertypath, propertyname, propertyvalue, valuetype)
   VALUES ('alice','calendars/alice/bobcal_shared_by_bob','{DAV:}displayname','My Bob Cal',1),
          ('alice','calendars/alice/bobcal_shared_by_bob','{http://apple.com/ns/ical/}calendar-color','#123456',1),
          ('alice','calendars/alice/work','{http://owncloud.org/ns}calendar-enabled','0',1)" >/dev/null
q "UPDATE oc_users SET displayname='Alice E2E' WHERE uid='alice'" >/dev/null
q "UPDATE oc_users SET displayname='Bob Shared' WHERE uid='bob'" >/dev/null

# Three subscriptions (the production test account has three), with the full
# column set `CalDavBackend::getSubscriptionsForUser()` reads. The first carries
# an `oc_properties` override on its own path so the override layer is compared
# against PHP for a subscription, not just a calendar. The third has NULL
# displayname / refreshrate / color (the 200-empty-element branch).
q "INSERT INTO oc_calendarsubscriptions
   (uri, principaluri, displayname, refreshrate, calendarorder, calendarcolor,
    striptodos, stripalarms, stripattachments, lastmodified, synctoken, source)
   VALUES ('webcal-work','principals/users/alice','Work Webcal','PT4H',20,'#00679e',1,0,0,1700000000,5,'https://example.com/work.ics'),
          ('webcal-holidays','principals/users/alice','Holidays','PT12H',21,NULL,0,1,1,1700000001,3,'https://example.com/holidays.ics'),
          ('webcal-null','principals/users/alice',NULL,NULL,22,NULL,0,0,0,NULL,1,'webcal://example.com/null.ics')" >/dev/null
q "INSERT INTO oc_properties (userid, propertypath, propertyname, propertyvalue, valuetype)
   VALUES ('alice','calendars/alice/webcal-work','{DAV:}displayname','Overridden Webcal',1),
          ('alice','calendars/alice/webcal-work','{http://apple.com/ns/ical/}calendar-color','#123456',1)" >/dev/null

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
echo "seeded $(q "SELECT count(*) FROM oc_calendars WHERE principaluri LIKE 'principals/users/%'") calendars, $(q "SELECT count(*) FROM oc_calendarobjects WHERE calendarid=$WORK_CAL") objects, $(q "SELECT count(*) FROM oc_calendarsubscriptions") subscriptions"

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

# The real home Depth:1 set (calendar properties + the subscription-specific
# ones), without `acl`/`current-user-privilege-set` (those delegate the listing
# because the special children's ACLs are not modelled).
HOME_SUBS_PROPS='<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns" xmlns:apple="http://apple.com/ns/ical/" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:resourcetype/><d:displayname/><d:owner/><d:supported-report-set/><d:supported-method-set/>
    <cs:getctag/><cs:source/><cs:subscribed-strip-todos/><cs:subscribed-strip-alarms/>
    <cs:subscribed-strip-attachments/>
    <cal:supported-calendar-component-set/><cal:schedule-calendar-transp/>
    <cal:calendar-description/><cal:calendar-timezone/><cal:max-resource-size/>
    <cal:supported-calendar-data/><cal:supported-collation-set/>
    <oc:owner-principal/><oc:read-only/><oc:calendar-enabled/>
    <nc:owner-displayname/><nc:trash-bin-retention-duration/>
    <apple:calendar-color/><apple:calendar-order/><apple:refreshrate/><s:sync-token/>
  </d:prop>
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
		-H "Host: $NC_HOST" \
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

# --- the REAL client per-calendar property sets ------------------------------
# Extracted from the clients' own source (2026-09-19 revisions in
# docs/recon/caldav-traffic.md) - not a hand-written approximation. Each list is
# the exact union a client's `getPropFindList()` produces for a calendar
# collection. The harness passed 23/23 with a synthetic list while the web UI
# delegated on two of these properties.
#
# Nextcloud web UI - `@nextcloud/cdav-library` 2.8.0 (dist/index.mjs):
#   davObject.js getPropFindList  -> getcontenttype, getetag, resourcetype
#   davCollection.js getPropFindList
#     -> displayname, owner, resourcetype, sync-token, current-user-privilege-set
#   calendar.js getPropFindList
#     -> apple calendar-order/color, cs getctag, caldav calendar-description,
#        calendar-timezone, supported-calendar-component-set,
#        supported-calendar-data, max-resource-size, min-date-time,
#        max-date-time, max-instances, max-attendees-per-instance,
#        supported-collation-set, calendar-free-busy-set,
#        schedule-calendar-transp, schedule-default-calendar-URL,
#        oc calendar-enabled, nc default-alarm-part-day/full-day,
#        disable-alarm-notifications, owner-displayname,
#        trash-bin-retention-duration, deleted-at
#   davCollectionShareable.js -> oc invite, cs allowed-sharing-modes
#   davCollectionPublishable.js -> cs publish-url
WEBUI_SET=(
	'DAV:|getcontenttype' 'DAV:|getetag' 'DAV:|resourcetype' 'DAV:|displayname'
	'DAV:|owner' 'DAV:|sync-token' 'DAV:|current-user-privilege-set'
	'http://apple.com/ns/ical/|calendar-order' 'http://apple.com/ns/ical/|calendar-color'
	'http://calendarserver.org/ns/|getctag' 'http://calendarserver.org/ns/|allowed-sharing-modes'
	'http://calendarserver.org/ns/|publish-url'
	'urn:ietf:params:xml:ns:caldav|calendar-description'
	'urn:ietf:params:xml:ns:caldav|calendar-timezone'
	'urn:ietf:params:xml:ns:caldav|supported-calendar-component-set'
	'urn:ietf:params:xml:ns:caldav|supported-calendar-data'
	'urn:ietf:params:xml:ns:caldav|max-resource-size'
	'urn:ietf:params:xml:ns:caldav|min-date-time'
	'urn:ietf:params:xml:ns:caldav|max-date-time'
	'urn:ietf:params:xml:ns:caldav|max-instances'
	'urn:ietf:params:xml:ns:caldav|max-attendees-per-instance'
	'urn:ietf:params:xml:ns:caldav|supported-collation-set'
	'urn:ietf:params:xml:ns:caldav|calendar-free-busy-set'
	'urn:ietf:params:xml:ns:caldav|schedule-calendar-transp'
	'urn:ietf:params:xml:ns:caldav|schedule-default-calendar-URL'
	'http://owncloud.org/ns|calendar-enabled' 'http://owncloud.org/ns|invite'
	'http://nextcloud.com/ns|default-alarm-part-day'
	'http://nextcloud.com/ns|default-alarm-full-day'
	'http://nextcloud.com/ns|disable-alarm-notifications'
	'http://nextcloud.com/ns|owner-displayname'
	'http://nextcloud.com/ns|trash-bin-retention-duration'
	'http://nextcloud.com/ns|deleted-at'
)

# Thunderbird 154 (`CalDavCalendar.sys.mjs`, renamed from
# `CalDavRequestHandlers.sys.mjs`) `checkDavResourceType()` initial per-calendar
# PROPFIND (Depth: 0).
THUNDERBIRD_SET=(
	'DAV:|resourcetype' 'DAV:|owner' 'DAV:|current-user-principal'
	'DAV:|current-user-privilege-set' 'DAV:|supported-report-set'
	'urn:ietf:params:xml:ns:caldav|supported-calendar-component-set'
	'http://calendarserver.org/ns/|getctag'
)

# DAVx5 4.5.x (`BaseWebDavCollection.queryCapabilities()`, dav4jvm
# `CalDAV.GetCTag` = the calendarserver namespace): the same collection
# capability probe is sent for calendars and address books, so the two CardDAV
# properties appear on a calendar too (PHP answers them 404).
DAVX5_SET=(
	'DAV:|supported-report-set' 'DAV:|sync-token'
	'http://calendarserver.org/ns/|getctag'
	'urn:ietf:params:xml:ns:caldav|max-resource-size'
	'urn:ietf:params:xml:ns:carddav|max-resource-size'
	'urn:ietf:params:xml:ns:carddav|supported-address-data'
)

# A calendar-subscription property set: every property PHP serves on a
# `Sabre\CalDAV\Subscriptions\Subscription` (resourcetype `{cs}subscribed`,
# source, the strip flags, refreshrate, the hard-coded VTODO,VEVENT component
# set, the **raw** `{cs}getctag`/`{sabredav}sync-token`) plus the calendar
# properties a subscription 404s. Captured live from 33.0.5.
SUB_SET=(
	'DAV:|resourcetype' 'DAV:|displayname' 'DAV:|owner' 'DAV:|current-user-principal'
	'DAV:|current-user-privilege-set' 'DAV:|acl' 'DAV:|supported-report-set'
	'DAV:|supported-method-set' 'DAV:|getlastmodified' 'DAV:|sync-token'
	'http://calendarserver.org/ns/|getctag' 'http://calendarserver.org/ns/|source'
	'http://calendarserver.org/ns/|subscribed-strip-todos'
	'http://calendarserver.org/ns/|subscribed-strip-alarms'
	'http://calendarserver.org/ns/|subscribed-strip-attachments'
	'http://calendarserver.org/ns/|allowed-sharing-modes'
	'http://calendarserver.org/ns/|publish-url'
	'urn:ietf:params:xml:ns:caldav|supported-calendar-component-set'
	'urn:ietf:params:xml:ns:caldav|schedule-calendar-transp'
	'urn:ietf:params:xml:ns:caldav|calendar-description'
	'urn:ietf:params:xml:ns:caldav|calendar-timezone'
	'urn:ietf:params:xml:ns:caldav|max-resource-size'
	'urn:ietf:params:xml:ns:caldav|supported-calendar-data'
	'urn:ietf:params:xml:ns:caldav|supported-collation-set'
	'http://owncloud.org/ns|owner-principal' 'http://owncloud.org/ns|read-only'
	'http://owncloud.org/ns|invite' 'http://owncloud.org/ns|calendar-enabled'
	'http://owncloud.org/ns|enabled'
	'http://nextcloud.com/ns|owner-displayname'
	'http://nextcloud.com/ns|disable-alarm-notifications'
	'http://apple.com/ns/ical/|calendar-color' 'http://apple.com/ns/ical/|calendar-order'
	'http://apple.com/ns/ical/|refreshrate'
	'http://sabredav.org/ns|sync-token'
)

prefix_for() {
	case "$1" in
		"DAV:") echo d ;;
		"urn:ietf:params:xml:ns:caldav") echo cal ;;
		"urn:ietf:params:xml:ns:carddav") echo card ;;
		"http://calendarserver.org/ns/") echo cs ;;
		"http://owncloud.org/ns") echo oc ;;
		"http://nextcloud.com/ns") echo nc ;;
		"http://apple.com/ns/ical/") echo apple ;;
		"http://sabredav.org/ns") echo s ;;
		*) echo z ;;
	esac
}

# One `<d:propfind>` from `ns|local` lines on stdin.
propfind_from_set() {
	local out='<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns" xmlns:apple="http://apple.com/ns/ical/" xmlns:s="http://sabredav.org/ns"><d:prop>'
	local ns prop
	while IFS='|' read -r ns prop; do
		[ -z "$ns" ] && continue
		out+="<$(prefix_for "$ns"):$prop/>"
	done
	out+='</d:prop></d:propfind>'
	printf '%s' "$out"
}

# run_property SET REL NS LOCAL: the property ALONE, one per request. Fails
# unless the sidecar served it (attribution header) and the canonical propstat
# matches PHP's (status, and the value for `{cs}publish-url`).
run_property() {
	local set="$1" rel="$2" ns="$3" prop="$4"
	local label="$set-$(prefix_for "$ns")-$prop"
	local name="prop-$label"
	local body; body=$(printf '%s|%s\n' "$ns" "$prop" | propfind_from_set)
	local php_xml="$STATE_DIR/php-caldav-$name.xml"
	local side_xml="$STATE_DIR/sidecar-caldav-$name.xml"
	local php_code side_code
	php_code=$(curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 0' \
		-H 'Content-Type: application/xml; charset=utf-8' \
		--data-binary "$body" -o "$php_xml" -w '%{http_code}' "$PHP_BASE/$rel")
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	side_code=$(curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 0' \
		-H 'Content-Type: application/xml; charset=utf-8' -H "Host: $NC_HOST" \
		--data-binary "$body" -D "$STATE_DIR/sidecar-caldav-$name.headers" \
		-o "$side_xml" -w '%{http_code}' "$SIDE_BASE/$rel")
	if ! grep -qi '^x-nextcloud-dav:[[:space:]]*sidecar' "$STATE_DIR/sidecar-caldav-$name.headers" 2>/dev/null; then
		echo "RESULT|CALDAV-PROP-$label|FAIL|sidecar delegated (no attribution header); sidecar=$side_code"
		PARITY_OK=0
		return
	fi
	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$php_xml" >"$STATE_DIR/php-caldav-$name.canon" 2>/dev/null
	python3 "$LOCAL_DIR/canonicalize_propfind.py" "$side_xml" >"$STATE_DIR/sidecar-caldav-$name.canon" 2>/dev/null
	if [ "$php_code" = "$side_code" ] \
		&& diff -q "$STATE_DIR/php-caldav-$name.canon" "$STATE_DIR/sidecar-caldav-$name.canon" >/dev/null; then
		echo "RESULT|CALDAV-PROP-$label|PASS|sidecar served; canonical propstat matches PHP ($side_code)"
	else
		echo "RESULT|CALDAV-PROP-$label|FAIL|php=$php_code sidecar=$side_code or propstat differs"
		diff -u "$STATE_DIR/php-caldav-$name.canon" "$STATE_DIR/sidecar-caldav-$name.canon" | head -20
		PARITY_OK=0
	fi
}

run_property_set() {
	local set="$1" rel="$2"; shift 2
	local entry ns prop
	for entry in "$@"; do
		ns="${entry%%|*}"
		prop="${entry#*|}"
		run_property "$set" "$rel" "$ns" "$prop"
	done
}

sec "real client sets: per-property attribution"
run_property_set webui "alice/work/" "${WEBUI_SET[@]}"
run_property_set webui-unpub "alice/personal/" \
	'http://calendarserver.org/ns/|publish-url'
run_property_set thunderbird "alice/work/" "${THUNDERBIRD_SET[@]}"
run_property_set davx5 "alice/work/" "${DAVX5_SET[@]}"
# Subscription child: every property alone, one request each.
run_property_set webcal "alice/webcal-work/" "${SUB_SET[@]}"

sec "real client sets: full-set parity"
run_case real-webui-cal 0 "alice/work/" "$(printf '%s\n' "${WEBUI_SET[@]}" | propfind_from_set)"
# The unpublished branch of `{cs}publish-url` (404 propstat) must match too.
run_case real-webui-cal-unpublished 0 "alice/personal/" "$(printf '%s\n' "${WEBUI_SET[@]}" | propfind_from_set)"
run_case real-thunderbird-cal 0 "alice/work/" "$(printf '%s\n' "${THUNDERBIRD_SET[@]}" | propfind_from_set)"
run_case real-davx5-cal 0 "alice/work/" "$(printf '%s\n' "${DAVX5_SET[@]}" | propfind_from_set)"
# The subscription child (a `{cs}subscribed` node, not a calendar) and the home
# listing that now contains subscriptions.
run_case real-webcal-sub-d0 0 "alice/webcal-work/" "$(printf '%s\n' "${SUB_SET[@]}" | propfind_from_set)"
run_case real-webcal-holidays-d0 0 "alice/webcal-holidays/" "$(printf '%s\n' "${SUB_SET[@]}" | propfind_from_set)"
run_case real-webcal-null-d0 0 "alice/webcal-null/" "$(printf '%s\n' "${SUB_SET[@]}" | propfind_from_set)"
run_case real-home-subs-d1 1 "alice/" "$HOME_SUBS_PROPS"

# With webcal caching on, PHP returns a `CachedSubscription` (a `{caldav}calendar`
# node) instead of a plain `Subscription`; the sidecar must delegate, not serve
# the wrong node shape. Both triggers are checked (the magic header and a KDE
# `KIO` user agent, which is what the production home traffic uses).
sec "webcal caching must delegate the subscription listing"
for trigger in header kio; do
	q "UPDATE oc_authtoken SET last_check = extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
	case "$trigger" in
		header) extra=(-H 'X-NC-CalDAV-Webcal-Caching: On') ;;
		kio) extra=(-H 'User-Agent: Mozilla/5.0 (X11; Linux x86_64) KIO/5.116') ;;
	esac
	code=$(curl -s -K "$CURLRC" -X PROPFIND -H 'Depth: 1' \
		-H 'Content-Type: application/xml; charset=utf-8' "${extra[@]}" \
		--data-binary "$HOME_SUBS_PROPS" -D "$STATE_DIR/sidecar-caldav-webcal-$trigger.headers" \
		-o /dev/null -w '%{http_code}' "$SIDE_BASE/alice/")
	if [ "$code" = "501" ] && ! grep -qi '^x-nextcloud-dav:' "$STATE_DIR/sidecar-caldav-webcal-$trigger.headers" 2>/dev/null; then
		echo "RESULT|CALDAV-WEBCAL-CACHING-$trigger|PASS|501 with no sidecar header"
	else
		echo "RESULT|CALDAV-WEBCAL-CACHING-$trigger|FAIL|expected a bare 501, got $code"
		PARITY_OK=0
	fi
done

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
