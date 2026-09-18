#!/usr/bin/env bash
# End-to-end validation of the nextcloud-dav native write path + the companion
# nextcloud_dav outbox dispatcher, against a disposable Nextcloud 33.0.5.
#
# Run setup.sh first. This script never mutates the product code; it only drives
# the sidecar, the occ worker and SQL on the disposable database.
#
# Evidence is written to state/evidence/e2e.txt.
set -uo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"
load_env

mkdir -p "$EVIDENCE_DIR"
EVIDENCE_FILE="$EVIDENCE_DIR/e2e.txt"
: >"$EVIDENCE_FILE"
exec > >(tee -a "$EVIDENCE_FILE") 2>&1

PASS=0
FAIL=0
check() { # id desc expected actual
	local id="$1" desc="$2" expected="$3" actual="$4"
	if [ "$expected" = "$actual" ]; then
		PASS=$((PASS + 1))
		printf 'RESULT|%s|PASS|%s|expected=%s actual=%s\n' "$id" "$desc" "$expected" "$actual"
	else
		FAIL=$((FAIL + 1))
		printf 'RESULT|%s|FAIL|%s|expected=%s actual=%s\n' "$id" "$desc" "$expected" "$actual"
	fi
}
has() { case "$2" in *"$1"*) echo yes ;; *) echo no ;; esac; }
sec() { printf '\n===== %s =====\n' "$*"; }

SIDE="$SIDECAR_URL"
BOOK=contacts
BOOK_URL="$SIDE/remote.php/dav/addressbooks/users/alice/$BOOK"
APACHE_BOOK="$NC_URL/remote.php/dav/addressbooks/users/alice/$BOOK"
CURLRC="$STATE_DIR/curlrc"
IID=$(occ config:system:get instanceid)

# ---- helpers ----------------------------------------------------------------
put_card() { # file -> prints "status etag"
	local file="$1" url="$2"
	curl -s -K "$CURLRC" -X PUT -H 'Content-Type: text/vcard; charset=utf-8' \
		--data-binary "@$file" -D - -o /dev/null "$url" |
		awk 'NR==1{s=$2} tolower($1)=="etag:"{gsub(/\r/,"");e=$2} END{print s, e}'
}
delete_card() { # url -> status
	local url="$1"
	curl -s -K "$CURLRC" -X DELETE -o /dev/null -w '%{http_code}' "$url"
}
run_worker_once() { occ dav:event-dispatch --once 2>&1 | tail -1; }
outbox_state() { q "SELECT state FROM oc_dav_event_outbox WHERE seq=$1"; }
photo_key() { printf '%s %s' "$1" "$2" | md5sum | awk '{print $1}'; }
photo_filecache_count() { # bookid uri
	local key
	key=$(photo_key "$1" "$2")
	q "SELECT count(*) FROM oc_filecache WHERE path LIKE '%dav-photocache/$key/%'"
}
photo_phys_exists() { # bookid uri -> yes/no
	local key
	key=$(photo_key "$1" "$2")
	if docker exec "$NC" test -e "/var/www/html/data/appdata_$IID/dav-photocache/$key"; then echo yes; else echo no; fi
}
seed_photo_cache() { # uri
	curl -s -o /dev/null -K "$CURLRC" "$APACHE_BOOK/$1?photo"
}
activity_count() { q "SELECT count(*) FROM oc_activity WHERE object_type='addressbook' AND object_id=$1"; }
birthday_vevent_count() { q "SELECT count(*) FROM oc_calendarobjects WHERE uid='$1'"; }

reset_alice_state() {
	sec "reset alice test state"
	local abids
	abids=$(q "SELECT string_agg(id::text, ',') FROM oc_addressbooks WHERE principaluri='principals/users/alice'")
	[ -n "$abids" ] || abids='-1'
	q "DELETE FROM oc_cards_properties WHERE addressbookid IN ($abids)" >/dev/null
	q "DELETE FROM oc_cards WHERE addressbookid IN ($abids)" >/dev/null
	q "DELETE FROM oc_addressbookchanges WHERE addressbookid IN ($abids)" >/dev/null
	q "UPDATE oc_addressbooks SET synctoken=1 WHERE id IN ($abids)" >/dev/null
	q "DELETE FROM oc_calendarobjects WHERE calendarid IN (SELECT id FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='contact_birthdays')" >/dev/null
	q "DELETE FROM oc_calendarchanges WHERE calendarid IN (SELECT id FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='contact_birthdays')" >/dev/null
	q "DELETE FROM oc_calendar_reminders WHERE calendar_id IN (SELECT id FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='contact_birthdays')" >/dev/null
	q "DELETE FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='contact_birthdays'" >/dev/null
	q "DELETE FROM oc_activity WHERE affecteduser='alice'" >/dev/null
	q "DELETE FROM oc_activity_mq WHERE amq_affecteduser='alice'" >/dev/null 2>&1 || true
	q "TRUNCATE oc_dav_event_outbox RESTART IDENTITY" >/dev/null
	echo "reset done (address book ids: $abids)"
}

ensure_book() { # uri
	local code
	code=$(curl -s -o /dev/null -w '%{http_code}' -K "$CURLRC" -X MKCOL \
		-H 'Content-Type: application/xml' --data-binary \
		'<?xml version="1.0"?><mkcol xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><set><prop><resourcetype><collection/><card:addressbook/></resourcetype></prop></set></mkcol>' \
		"$NC_URL/remote.php/dav/addressbooks/users/alice/$1/")
	echo "MKCOL $1 -> $code"
}

# ---------------------------------------------------------------------------
sec "0. preconditions"
wait_for_nextcloud >/dev/null
wait_for_sidecar
echo "sidecar: $(curl -s "$SIDE/healthz" | head -c 200)"
check P0 "activity app enabled" "yes" "$([ "$(occ app:list 2>/dev/null | sed -n '/^Enabled:/,/^Disabled:/p' | grep -c '  - activity:')" -gt 0 ] && echo yes || echo no)"
reset_alice_state
ensure_book crash

BOOK_ID=$(q "SELECT id FROM oc_addressbooks WHERE principaluri='principals/users/alice' AND uri='$BOOK'")
CRASH_ID=$(q "SELECT id FROM oc_addressbooks WHERE principaluri='principals/users/alice' AND uri='crash'")
echo "contacts book id=$BOOK_ID crash book id=$CRASH_ID"

# ---------------------------------------------------------------------------
sec "1. CREATE"
NOTE=$(printf '\u20ac%.0s' $(seq 1 100))
cat >"$STATE_DIR/card-create.vcf" <<EOF
BEGIN:VCARD
VERSION:3.0
UID:e2e-create-1
FN:Alice Example
N:Example;Alice;;;
BDAY:1990-04-12
NOTE:$NOTE
EMAIL;TYPE=PREF:alice@example.com
END:VCARD
EOF
PRE_TOKEN=$(q "SELECT synctoken FROM oc_addressbooks WHERE id=$BOOK_ID")
read -r PUT_STATUS PUT_ETAG <<<"$(put_card "$STATE_DIR/card-create.vcf" "$BOOK_URL/e2e-create.vcf")"
check C1.1 "PUT create status" "201" "$PUT_STATUS"
check C1.2 "PUT create ETag is quoted" "yes" "$(has '"' "$PUT_ETAG")"

DB_ETAG=$(q "SELECT etag FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
CARD_ID=$(q "SELECT id FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
DB_UID=$(q "SELECT uid FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
DB_SIZE=$(q "SELECT size FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
DB_LEN=$(q "SELECT octet_length(carddata) FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
DB_MD5OK=$(q "SELECT md5(carddata)=etag FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
check C1.3 "etag == md5(carddata)" "t" "$DB_MD5OK"
check C1.4 "uid" "e2e-create-1" "$DB_UID"
check C1.5 "size == octet_length(carddata)" "$DB_LEN" "$DB_SIZE"
check C1.6 "PUT ETag == DB etag" "\"$DB_ETAG\"" "$PUT_ETAG"

NEW_TOKEN=$(q "SELECT synctoken FROM oc_addressbooks WHERE id=$BOOK_ID")
CHG=$(q "SELECT operation||','||synctoken FROM oc_addressbookchanges WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
check C1.7 "synctoken incremented by 1" "$((PRE_TOKEN + 1))" "$NEW_TOKEN"
check C1.8 "change row operation,synctoken (pre-increment)" "1,$PRE_TOKEN" "$CHG"

PROPS=$(q "SELECT name||'='||value||'='||preferred FROM oc_cards_properties WHERE addressbookid=$BOOK_ID AND name IN ('FN','UID','BDAY','EMAIL') ORDER BY name")
check C1.9 "FN/UID/BDAY properties" "yes" "$(has 'FN=Alice Example=0' "$PROPS")"
check C1.10 "TYPE=PREF -> preferred=1" "yes" "$(has 'EMAIL=alice@example.com=1' "$PROPS")"
check C1.11 "BDAY property" "yes" "$(has 'BDAY=1990-04-12=0' "$PROPS")"
TRUNC=$(q "SELECT octet_length(value)||','||char_length(value) FROM oc_cards_properties WHERE addressbookid=$BOOK_ID AND name='NOTE'")
check C1.12 "multibyte NOTE truncated to 254-byte boundary" "252,84" "$TRUNC"

OBOX=$(q "SELECT state||','||event_type||','||effects FROM oc_dav_event_outbox WHERE card_uri='e2e-create.vcf'")
OBOX_N=$(q "SELECT count(*) FROM oc_dav_event_outbox WHERE card_uri='e2e-create.vcf'")
check C1.13 "exactly one outbox row" "1" "$OBOX_N"
check C1.14 "outbox state=0 event_type=1" "yes" "$(has '0,1,' "$OBOX")"
EFFECTS=$(q "SELECT effects FROM oc_dav_event_outbox WHERE card_uri='e2e-create.vcf'")
# A create only triggers the listeners registered for CardCreatedEvent:
# photo_cache is update/delete-only and redis_cloud_id is update-only.
for e in activity_stream activity_mail notification_push birthday_calendar calendar_reminders; do
	check "C1.15-$e" "effect $e under php" "yes" "$(has "\"$e\"" "$EFFECTS")"
done
for e in photo_cache redis_cloud_id; do
	check "C1.15-absent-$e" "effect $e absent for a create" "no" "$(has "\"$e\"" "$EFFECTS")"
done
check C1.16 "rust effects empty" "yes" "$(has '"rust":[]' "$EFFECTS")"
CARD_ROW=$(q "SELECT card_row FROM oc_dav_event_outbox WHERE card_uri='e2e-create.vcf'")
for k in id uri lastmodified etag size uid; do
	check "C1.17-$k" "card_row has $k" "yes" "$(has "\"$k\"" "$CARD_ROW")"
done
check C1.18 "card_row etag is quoted" "yes" "$(has "\\\"$DB_ETAG\\\"" "$CARD_ROW")"
check C1.19 "card_row uid matches" "yes" "$(has '"uid":"e2e-create-1"' "$CARD_ROW")"

# ---------------------------------------------------------------------------
sec "2. DISPATCH (create)"
seed_photo_cache e2e-create.vcf
PHOTO_BEFORE=$(photo_filecache_count "$BOOK_ID" e2e-create.vcf)
ACT_BEFORE=$(activity_count "$BOOK_ID")
echo "seeded photo cache rows=$PHOTO_BEFORE activity_before=$ACT_BEFORE"
run_worker_once >/dev/null
check C2.1 "outbox row state=2 after dispatch" "2" "$(outbox_state 1)"
check C2.2 "processed_at set" "t" "$(q "SELECT processed_at IS NOT NULL FROM oc_dav_event_outbox WHERE seq=1")"

BCAL=$(q "SELECT id||','||synctoken FROM oc_calendars WHERE principaluri='principals/users/alice' AND uri='contact_birthdays' ORDER BY id LIMIT 1")
check C2.3 "alice contact_birthdays calendar exists" "yes" "$(has ',' "$BCAL")"
BCAL_ID=${BCAL%%,*}
check C2.4 "birthday VEVENT count for UID" "1" "$(birthday_vevent_count e2e-create-1)"
VEVENT=$(q "SELECT convert_from(calendardata,'UTF8') FROM oc_calendarobjects WHERE uid='e2e-create-1'")
check C2.5 "VEVENT DTSTART from BDAY" "yes" "$(has 'DTSTART;VALUE=DATE:19900412' "$VEVENT")"
check C2.6 "VEVENT SUMMARY" "yes" "$(has 'SUMMARY:🎂 Alice Example (1990)' "$VEVENT")"
check C2.7 "VEVENT VALARM from default PT9H" "yes" "$(has 'TRIGGER;VALUE=DURATION:PT9H' "$VEVENT")"
CCHG=$(q "SELECT operation||','||synctoken FROM oc_calendarchanges WHERE calendarid=$BCAL_ID AND uri='contacts-e2e-create.vcf.ics'")
check C2.8 "calendarchanges op=1 pre-increment token" "1,1" "$CCHG"
check C2.9 "birthday calendar synctoken == 2" "2" "${BCAL##*,}"
check C2.10 "birthday reminders indexed" "yes" "$(q "SELECT CASE WHEN count(*)>0 THEN 'yes' ELSE 'no' END FROM oc_calendar_reminders WHERE calendar_id=$BCAL_ID")"
ACT_AFTER=$(activity_count "$BOOK_ID")
check C2.11 "exactly one new activity row (card_add_self)" "1" "$((ACT_AFTER - ACT_BEFORE))"
check C2.12 "activity subject card_add_self" "1" "$(q "SELECT count(*) FROM oc_activity WHERE object_type='addressbook' AND object_id=$BOOK_ID AND subject='card_add_self'")"
PHOTO_AFTER=$(photo_filecache_count "$BOOK_ID" e2e-create.vcf)
check C2.13 "photo cache NOT removed on create (listener only handles update/delete)" "$PHOTO_BEFORE" "$PHOTO_AFTER"

# ---------------------------------------------------------------------------
sec "3. UPDATE"
sed 's/BDAY:1990-04-12/BDAY:1991-05-20/; s/alice@example.com/alice2@example.com/' \
	"$STATE_DIR/card-create.vcf" >"$STATE_DIR/card-update.vcf"
read -r UPD_STATUS UPD_ETAG <<<"$(put_card "$STATE_DIR/card-update.vcf" "$BOOK_URL/e2e-create.vcf")"
check C3.1 "PUT update status" "204" "$UPD_STATUS"
check C3.2 "ETag changed" "no" "$([ "$UPD_ETAG" = "$PUT_ETAG" ] && echo yes || echo no)"
UPD_DB_ETAG=$(q "SELECT etag FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")
check C3.3 "PUT ETag == new DB etag" "\"$UPD_DB_ETAG\"" "$UPD_ETAG"
UPD_CHG=$(q "SELECT operation||','||synctoken FROM oc_addressbookchanges WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf' ORDER BY id DESC LIMIT 1")
check C3.4 "change row operation=2 pre-increment token" "2,2" "$UPD_CHG"
UPD_TYPE=$(q "SELECT event_type||','||state FROM oc_dav_event_outbox WHERE card_uri='e2e-create.vcf' ORDER BY seq DESC LIMIT 1")
check C3.5 "new outbox row event_type=2 state=0" "2,0" "$UPD_TYPE"
seed_photo_cache e2e-create.vcf
PHOTO_UPD_BEFORE=$(photo_filecache_count "$BOOK_ID" e2e-create.vcf)
echo "photo cache before update dispatch=$PHOTO_UPD_BEFORE"
run_worker_once >/dev/null
check C3.6 "VEVENT count still 1 (updated, not duplicated)" "1" "$(birthday_vevent_count e2e-create-1)"
check C3.7 "VEVENT DTSTART updated to 1991-05-20" "yes" "$(has 'DTSTART;VALUE=DATE:19910520' "$(q "SELECT convert_from(calendardata,'UTF8') FROM oc_calendarobjects WHERE uid='e2e-create-1'")")"
check C3.8 "photo cache removed on update" "0" "$(photo_filecache_count "$BOOK_ID" e2e-create.vcf)"
check C3.9 "photo cache physical dir gone" "no" "$(photo_phys_exists "$BOOK_ID" e2e-create.vcf)"
check C3.10 "update activity row exists" "1" "$(q "SELECT count(*) FROM oc_activity WHERE object_type='addressbook' AND object_id=$BOOK_ID AND subject='card_update_self'")"

# ---------------------------------------------------------------------------
sec "4. DELETE"
seed_photo_cache e2e-create.vcf
run_worker_once >/dev/null # flush any pending first
PRE_DEL_CHG=$(q "SELECT operation||','||synctoken FROM oc_addressbookchanges WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf' ORDER BY id DESC LIMIT 1")
DEL_STATUS=$(delete_card "$BOOK_URL/e2e-create.vcf")
check C4.1 "DELETE status" "204" "$DEL_STATUS"
DEL_CHG=$(q "SELECT operation||','||synctoken FROM oc_addressbookchanges WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf' ORDER BY id DESC LIMIT 1")
check C4.2 "change row operation=3" "yes" "$(has '3,' "$DEL_CHG")"
check C4.3 "card gone" "0" "$(q "SELECT count(*) FROM oc_cards WHERE addressbookid=$BOOK_ID AND uri='e2e-create.vcf'")"
check C4.4 "properties purged" "0" "$(q "SELECT count(*) FROM oc_cards_properties WHERE addressbookid=$BOOK_ID AND cardid=$CARD_ID")"
DEL_ROW=$(q "SELECT event_type||','||state||','||(convert_from(card_data,'UTF8') LIKE '%BDAY:1991-05-20%') FROM oc_dav_event_outbox WHERE card_uri='e2e-create.vcf' AND event_type=3 ORDER BY seq DESC LIMIT 1")
check C4.5 "delete outbox row event_type=3 state=0" "yes" "$(has '3,0,' "$DEL_ROW")"
check C4.6 "delete row card_data is pre-delete (old BDAY)" "yes" "$(has ',t' "$DEL_ROW")"
check C4.7 "delete row card_row is pre-delete ETag" "yes" "$(has "$UPD_DB_ETAG" "$(q "SELECT card_row FROM oc_dav_event_outbox WHERE event_type=3 ORDER BY seq DESC LIMIT 1")")"
run_worker_once >/dev/null
check C4.8 "birthday VEVENT removed after delete dispatch" "0" "$(birthday_vevent_count e2e-create-1)"
check C4.9 "photo cache removed on delete" "0" "$(photo_filecache_count "$BOOK_ID" e2e-create.vcf)"
check C4.10 "delete activity row exists" "1" "$(q "SELECT count(*) FROM oc_activity WHERE object_type='addressbook' AND object_id=$BOOK_ID AND subject='card_delete_self'")"

# ---------------------------------------------------------------------------
sec "5. IDEMPOTENCY"
OUTBOX_SIG_BEFORE=$(q "SELECT string_agg(seq||':'||state||':'||coalesce(processed_at,0)::text, ',' ORDER BY seq) FROM oc_dav_event_outbox")
ACT_BEFORE5=$(q "SELECT count(*) FROM oc_activity")
BCAL_OBJ_BEFORE=$(q "SELECT count(*) FROM oc_calendarobjects")
PHOTO_BEFORE5=$(q "SELECT count(*) FROM oc_filecache WHERE path LIKE '%dav-photocache%'")
run_worker_once >/dev/null
run_worker_once >/dev/null
OUTBOX_SIG_AFTER=$(q "SELECT string_agg(seq||':'||state||':'||coalesce(processed_at,0)::text, ',' ORDER BY seq) FROM oc_dav_event_outbox")
check C5.1 "outbox rows unchanged (no reprocessing)" "$OUTBOX_SIG_BEFORE" "$OUTBOX_SIG_AFTER"
check C5.2 "activity count unchanged" "$ACT_BEFORE5" "$(q "SELECT count(*) FROM oc_activity")"
check C5.3 "calendarobjects count unchanged" "$BCAL_OBJ_BEFORE" "$(q "SELECT count(*) FROM oc_calendarobjects")"
check C5.4 "photo cache count unchanged" "$PHOTO_BEFORE5" "$(q "SELECT count(*) FROM oc_filecache WHERE path LIKE '%dav-photocache%'")"

# ---------------------------------------------------------------------------
sec "6a. CRASH SAFETY - SIGKILL mid-batch"
q "TRUNCATE oc_dav_event_outbox RESTART IDENTITY" >/dev/null
N=40
for i in $(seq 1 $N); do
	cat >"$STATE_DIR/crash-$i.vcf" <<EOF
BEGIN:VCARD
VERSION:3.0
UID:crash-$i
FN:Crash $i
BDAY:1970-01-01
END:VCARD
EOF
	put_card "$STATE_DIR/crash-$i.vcf" "$SIDE/remote.php/dav/addressbooks/users/alice/crash/crash-$i.vcf" >/dev/null
done
CRASH_ROWS=$(q "SELECT count(*) FROM oc_dav_event_outbox WHERE addressbookid=$CRASH_ID")
check C6a.1 "seeded $N outbox rows" "$N" "$CRASH_ROWS"
CRASH_ACT_BEFORE=$(activity_count "$CRASH_ID")

# Start the resident worker in the container; kill -9 it mid-batch.
docker exec -u www-data -w /var/www/html "$NC" sh -c \
	'exec php occ dav:event-dispatch --batch=1000 --idle-poll-ms=100' \
	>"$STATE_DIR/worker-crash.log" 2>&1 &
WORKER_HOST_PID=$!
DISPATCHED=0
for _ in $(seq 1 200); do
	DISPATCHED=$(q "SELECT count(*) FROM oc_dav_event_outbox WHERE state=2")
	[ "$DISPATCHED" -ge 5 ] && break
	sleep 0.05
done
docker exec "$NC" pkill -9 -f 'dav:event-dispatch' >/dev/null 2>&1 || true
wait "$WORKER_HOST_PID" 2>/dev/null || true
echo "killed worker after $DISPATCHED rows reached state=2"
STALE=$(q "SELECT count(*) FROM oc_dav_event_outbox WHERE state=1")
check C6a.2 "worker killed while holding claims (state=1 rows exist)" "yes" "$([ "${STALE:-0}" -gt 0 ] && echo yes || echo no)"
echo "state=1 stale claims: $STALE; state=2 committed: $(q "SELECT count(*) FROM oc_dav_event_outbox WHERE state=2")"

# Wait past claim_timeout_s (5s) so the worker re-claims the stale reservations.
sleep 6
run_worker_once >/dev/null
run_worker_once >/dev/null
NOT_DONE=$(q "SELECT count(*) FROM oc_dav_event_outbox WHERE addressbookid=$CRASH_ID AND state<>2")
check C6a.3 "every row eventually reaches done (nothing lost)" "0" "$NOT_DONE"
DUPES=$(q "SELECT count(*) FROM (SELECT uid, count(*) c FROM oc_calendarobjects WHERE uid LIKE 'crash-%' GROUP BY uid HAVING count(*)>1) x")
check C6a.4 "no duplicate birthday VEVENTs" "0" "$DUPES"
check C6a.4b "one birthday VEVENT per crash card" "$N" "$(q "SELECT count(*) FROM oc_calendarobjects WHERE uid LIKE 'crash-%'")"
CRASH_ACT_AFTER=$(activity_count "$CRASH_ID")
check C6a.5 "exactly one activity row per crash card" "$N" "$((CRASH_ACT_AFTER - CRASH_ACT_BEFORE))"
check C6a.6 "all crash rows state=2" "$N" "$(q "SELECT count(*) FROM oc_dav_event_outbox WHERE addressbookid=$CRASH_ID AND state=2")"

# ---------------------------------------------------------------------------
sec "6b. POLLING FALLBACK (row inserted with SQL, no pg_notify)"
q "INSERT INTO oc_dav_event_outbox (created_at,event_type,addressbookid,card_uri,card_row,card_data,effects,state,attempts,next_attempt_at)
   VALUES (extract(epoch from now())::bigint,1,$BOOK_ID,'e2e-poll.vcf',
     '{\"id\":999999,\"uri\":\"e2e-poll.vcf\",\"lastmodified\":1,\"etag\":\"\\\"deadbeef\\\"\",\"size\":60,\"uid\":\"e2e-poll-1\"}',
     convert_to(E'BEGIN:VCARD\nVERSION:3.0\nUID:e2e-poll-1\nFN:Poll One\nBDAY:1985-06-15\nEND:VCARD\n','UTF8'),
     '{\"php\":[\"activity_stream\",\"activity_mail\",\"notification_push\",\"birthday_calendar\",\"calendar_reminders\",\"photo_cache\",\"redis_cloud_id\"],\"rust\":[]}',0,0,0)" >/dev/null
POLL_SEQ=$(q "SELECT seq FROM oc_dav_event_outbox WHERE card_uri='e2e-poll.vcf'")
docker exec -u www-data -w /var/www/html "$NC" sh -c \
	'exec php occ dav:event-dispatch --batch=128 --idle-poll-ms=100' \
	>"$STATE_DIR/worker-poll.log" 2>&1 &
POLL_PID=$!
for _ in $(seq 1 100); do
	[ "$(outbox_state "$POLL_SEQ")" = "2" ] && break
	sleep 0.1
done
docker exec "$NC" pkill -9 -f 'dav:event-dispatch' >/dev/null 2>&1 || true
wait "$POLL_PID" 2>/dev/null || true
check C6b.1 "non-notified row picked up by polling" "2" "$(outbox_state "$POLL_SEQ")"
check C6b.2 "its birthday effect ran" "1" "$(birthday_vevent_count e2e-poll-1)"

# ---------------------------------------------------------------------------
sec "6c. POISON ROW ISOLATION"
q "TRUNCATE oc_dav_event_outbox RESTART IDENTITY" >/dev/null
insert_healthy() { # seq-order: uri uid
	q "INSERT INTO oc_dav_event_outbox (created_at,event_type,addressbookid,card_uri,card_row,card_data,effects,state,attempts,next_attempt_at)
     VALUES (extract(epoch from now())::bigint,1,$BOOK_ID,'$1',
       '{\"id\":1,\"uri\":\"$1\",\"lastmodified\":1,\"etag\":\"\\\"e\\\"\",\"size\":50,\"uid\":\"$2\"}',
       convert_to(E'BEGIN:VCARD\nVERSION:3.0\nUID:$2\nFN:$2\nBDAY:1975-03-03\nEND:VCARD\n','UTF8'),
       '{\"php\":[\"activity_stream\",\"activity_mail\",\"notification_push\",\"birthday_calendar\",\"calendar_reminders\",\"photo_cache\",\"redis_cloud_id\"],\"rust\":[]}',0,0,0)" >/dev/null
}
insert_healthy e2e-poison-a.vcf e2e-poison-a
q "INSERT INTO oc_dav_event_outbox (created_at,event_type,addressbookid,card_uri,card_row,card_data,effects,state,attempts,next_attempt_at)
   VALUES (extract(epoch from now())::bigint,1,$BOOK_ID,'e2e-poison-bad.vcf','{',
     convert_to('x','UTF8'),'{\"php\":[\"activity_stream\"],\"rust\":[]}',0,0,0)" >/dev/null
q "INSERT INTO oc_dav_event_outbox (created_at,event_type,addressbookid,card_uri,card_row,card_data,effects,state,attempts,next_attempt_at)
   VALUES (extract(epoch from now())::bigint,1,99999999,'e2e-poison-missing-ab.vcf',
     '{\"id\":1,\"uri\":\"e2e-poison-missing-ab.vcf\",\"lastmodified\":1,\"etag\":\"\\\"e\\\"\",\"size\":50,\"uid\":\"x\"}',
     convert_to('x','UTF8'),'{\"php\":[\"activity_stream\"],\"rust\":[]}',0,0,0)" >/dev/null
insert_healthy e2e-poison-b.vcf e2e-poison-b
for _ in 1 2 3 4 5; do
	run_worker_once >/dev/null
	sleep 1.5
done
check C6c.1 "healthy row before poison reaches done" "2" "$(q "SELECT state FROM oc_dav_event_outbox WHERE card_uri='e2e-poison-a.vcf'")"
check C6c.2 "healthy row after poison reaches done" "2" "$(q "SELECT state FROM oc_dav_event_outbox WHERE card_uri='e2e-poison-b.vcf'")"
check C6c.3 "poison row dead-letters (state=3)" "3" "$(q "SELECT state FROM oc_dav_event_outbox WHERE card_uri='e2e-poison-bad.vcf'")"
check C6c.4 "poison row retried max_attempts times" "3" "$(q "SELECT attempts FROM oc_dav_event_outbox WHERE card_uri='e2e-poison-bad.vcf'")"
check C6c.5 "poison last_error recorded" "yes" "$(q "SELECT CASE WHEN length(last_error)>0 THEN 'yes' ELSE 'no' END FROM oc_dav_event_outbox WHERE card_uri='e2e-poison-bad.vcf'")"
check C6c.6 "missing-addressbook row completes (skipped)" "2" "$(q "SELECT state FROM oc_dav_event_outbox WHERE card_uri='e2e-poison-missing-ab.vcf'")"
check C6c.7 "healthy birthday effects present" "2" "$(q "SELECT count(*) FROM oc_calendarobjects WHERE uid IN ('e2e-poison-a','e2e-poison-b')")"

# ---------------------------------------------------------------------------
sec "6d. SIDECAR REFUSES NATIVE WRITES WITHOUT THE OUTBOX TABLE"
docker exec "$PG" sh -c 'dropdb -U nextcloud --if-exists ncdav_scratch >/dev/null 2>&1; createdb -U nextcloud ncdav_scratch && pg_dump -U nextcloud nextcloud | psql -q -U nextcloud -d ncdav_scratch >/dev/null 2>&1' && echo "scratch db created"
q "DROP TABLE oc_dav_event_outbox" ncdav_scratch >/dev/null
check C6d.1 "scratch db has no outbox table" "0" "$(q "SELECT count(*) FROM information_schema.tables WHERE table_name='oc_dav_event_outbox'" ncdav_scratch)"
stop_sidecar
start_sidecar_db "postgres://nextcloud:${DB_PASSWORD}@127.0.0.1:${PG_PORT}/ncdav_scratch"
grep -o 'event outbox table is missing[^"]*' "$STATE_DIR/sidecar.log" | head -1
SCRATCH_PUT=$(put_card "$STATE_DIR/card-create.vcf" "$BOOK_URL/e2e-create.vcf" | awk '{print $1}')
check C6d.2 "PUT returns 501 without the outbox table" "501" "$SCRATCH_PUT"
stop_sidecar
start_sidecar
RESTORE_PUT=$(put_card "$STATE_DIR/card-create.vcf" "$BOOK_URL/e2e-create.vcf" | awk '{print $1}')
check C6d.3 "sidecar restored, native writes work again" "201" "$RESTORE_PUT"
docker exec "$PG" sh -c 'dropdb -U nextcloud --if-exists ncdav_scratch' >/dev/null 2>&1 || true
check C6d.4 "scratch DB dropped" "0" "$(q "SELECT count(*) FROM pg_database WHERE datname='ncdav_scratch'")"

# ---------------------------------------------------------------------------
sec "7. MISSING OPTIONAL APPS"
occ app:disable activity >/dev/null 2>&1 || true
occ app:disable notifications >/dev/null 2>&1 || true
echo "activity enabled=$(occ app:list 2>/dev/null | sed -n '/^Enabled:/,/^Disabled:/p' | grep -c '  - activity:') notifications enabled=$(occ app:list 2>/dev/null | sed -n '/^Enabled:/,/^Disabled:/p' | grep -c '  - notifications:')"
cat >"$STATE_DIR/card-noapp.vcf" <<'EOF'
BEGIN:VCARD
VERSION:3.0
UID:e2e-noapp-1
FN:No App
BDAY:1979-09-09
END:VCARD
EOF
put_card "$STATE_DIR/card-noapp.vcf" "$BOOK_URL/e2e-noapp.vcf" >/dev/null
NOAPP_SEQ=$(q "SELECT seq FROM oc_dav_event_outbox WHERE card_uri='e2e-noapp.vcf' ORDER BY seq DESC LIMIT 1")
WORKER_OUT=$(run_worker_once)
echo "$WORKER_OUT"
check C7.1 "row still reaches done without activity/notifications" "2" "$(outbox_state "$NOAPP_SEQ")"
check C7.2 "birthday effect still ran" "1" "$(birthday_vevent_count e2e-noapp-1)"
check C7.3 "no activity row written (app disabled)" "0" "$(q "SELECT count(*) FROM oc_activity WHERE object_type='addressbook' AND object_id=$BOOK_ID AND subject='card_add_self' AND timestamp > (SELECT created_at FROM oc_dav_event_outbox WHERE seq=$NOAPP_SEQ)" 2>/dev/null || echo 0)"
occ app:enable activity >/dev/null 2>&1 || true
occ app:enable notifications >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
sec "SUMMARY"
echo "PASS=$PASS FAIL=$FAIL"
printf 'TOTAL|PASS=%d|FAIL=%d\n' "$PASS" "$FAIL"
sleep 0.3
