#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Session-cookie authentication matrix for the nextcloud-dav sidecar.
#
# Proves, against the disposable Nextcloud 33.0.5 + redis harness, that a
# cookie-only DAV request is served natively when it can be evaluated exactly,
# and delegated (501, no sidecar header) in every case that cannot. The accept
# cases are also canonicalised against PHP's answer for the same request.
#
# Never prints secrets (the redis password, the passphrase, session contents).
#
# Run ./setup.sh first. Evidence is written to
# state/evidence/session-auth.txt.
set -uo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"
load_env

mkdir -p "$EVIDENCE_DIR"
EVIDENCE_FILE="$EVIDENCE_DIR/session-auth.txt"
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
sec() { printf '\n===== %s =====\n' "$*"; }

SIDE="$SIDECAR_URL"
IID=$(occ config:system:get instanceid)
SESSION_JAR="$STATE_DIR/session.jar"
TMP="$STATE_DIR/session-tmp"
mkdir -p "$TMP"
PF_BODY='<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop><d:displayname/><d:getetag/><d:resourcetype/><oc:fileid/></d:prop></d:propfind>'
# Depth 1 without `getetag`: the home root's etag is derived from the mount map
# and the sidecar caches it, so it can lag a PHP recomputation after the parity
# scripts mutate the tree. The auth path is what is under test here.
PF_BODY_STRUCT='<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><d:displayname/><d:resourcetype/><oc:fileid/></d:prop></d:propfind>'
REPORT_BODY='<?xml version="1.0"?><c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/></d:prop></c:addressbook-query>'
ALICE_FILES="$SIDE/remote.php/dav/files/alice/"
ALICE_BOOK="$SIDE/remote.php/dav/addressbooks/users/alice/contacts/"
BOB_FILES="$SIDE/remote.php/dav/files/bob/"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Cookie-only request to the sidecar. Prints the status code; headers go to
# $TMP/<name>.hdr and the body to $TMP/<name>.xml.
side_request() { # name method url [extra curl args...]
	local name="$1" method="$2" url="$3"
	shift 3
	curl -s -o "$TMP/$name.xml" -D "$TMP/$name.hdr" -w '%{http_code}' \
		-b "$SESSION_JAR" -X "$method" "$@" "$url"
}

has_sidecar_header() { # name
	grep -qi '^x-nextcloud-dav:[[:space:]]*sidecar' "$TMP/$1.hdr" 2>/dev/null && echo yes || echo no
}
has_any_sidecar_header() { # name
	grep -qi '^x-nextcloud-dav:' "$TMP/$1.hdr" 2>/dev/null && echo yes || echo no
}

# A cookie jar whose session id / passphrase cookies are rewritten. Never
# printed.
rewrite_jar() { # out new_sid new_passphrase
	local out="$1" sid="$2" pass="$3"
	awk -F'\t' -v OFS='\t' -v iid="$IID" -v sid="$sid" -v pass="$pass" '
		NF==7 && $6==iid { $7=sid }
		NF==7 && $6=="oc_sessionPassphrase" { $7=pass }
		{ print }
	' "$SESSION_JAR" >"$out"
	chmod 600 "$out"
}

# Craft a session-store fixture. Modes: copy|tamper|truncate|set_user_id|
# strip_dav_flag|strip_app_password|set_number_app_password|set_number_dav_flag.
# Writes PHPREDIS_SESSION:<out_id>.
craft_session() { # mode out_id [value]
	local mode="$1" out_id="$2" value="${3:-}"
	docker exec -i -e REDIS_PASSWORD -e SESSION_PASSPHRASE \
		-u www-data -w /var/www/html "$NC" \
		php /tmp/session_craft.php "$mode" "PHPREDIS_SESSION:$SID" \
		"PHPREDIS_SESSION:$out_id" "$value" >/dev/null
}

new_sid() { rand_hex 16; }

# Perform a real browser **form** login as alice (the app-password session in
# section 1 is a different beast). Writes the cookie jar to $1 and prints the
# session's CSRF requesttoken, which the web UI sends on every DAV request.
# The password is never printed. Returns non-zero if the login did not land on
# the dashboard.
form_login() { # jar
	local jar="$1"
	rm -f "$jar"
	curl -s -c "$jar" "$NC_URL/login" -o "$TMP/login.html"
	local rt
	rt=$(grep -o 'data-requesttoken="[^"]*"' "$TMP/login.html" | head -1 | sed 's/data-requesttoken="//; s/"$//')
	[ -n "$rt" ] || return 1
	# The login controller rejects an empty/foreign Origin, so the trusted
	# origin of the harness is sent explicitly.
	curl -s -o /dev/null -D "$TMP/login.hdr" -b "$jar" -c "$jar" \
		-H "Origin: $NC_URL" \
		--data-urlencode "user=alice" --data-urlencode "password=$ALICE_PASSWORD" \
		--data-urlencode "requesttoken=$rt" --data-urlencode 'timezone=UTC' \
		"$NC_URL/login"
	grep -qi '^location:.*apps/dashboard' "$TMP/login.hdr" || return 1
	curl -s -b "$jar" -c "$jar" "$NC_URL/apps/dashboard/" -o "$TMP/dashboard.html"
	grep -o 'data-requesttoken="[^"]*"' "$TMP/dashboard.html" | head -1 | sed 's/data-requesttoken="//; s/"$//'
}

# ---------------------------------------------------------------------------
sec "0. preconditions"
wait_for_nextcloud >/dev/null
wait_for_sidecar
echo "sidecar: $(curl -s "$SIDE/healthz" | head -c 120)"
check P0 "instanceid present" "yes" "$([ -n "$IID" ] && echo yes || echo no)"

# ---------------------------------------------------------------------------
sec "1. obtain a real session (Basic DAV login, design doc §8)"
# The Basic request creates the session and sets AUTHENTICATED_TO_DAV_BACKEND,
# and the session stores `app_password` (a PERMANENT token), which is the only
# kind the sidecar can revalidate without PHP.
rm -f "$SESSION_JAR"
code=$(curl -s -o /dev/null -w '%{http_code}' -c "$SESSION_JAR" -K "$STATE_DIR/curlrc" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$NC_URL/remote.php/dav/files/alice/")
check S0 "Basic DAV login creates a session" "207" "$code"
SID=$(awk -F'\t' -v n="$IID" '$6==n {print $7}' "$SESSION_JAR")
ENC_PASS=$(awk -F'\t' '$6=="oc_sessionPassphrase"{print $7}' "$SESSION_JAR")
export SESSION_PASSPHRASE="$ENC_PASS"
check S1 "session id cookie present" "yes" "$([ -n "$SID" ] && echo yes || echo no)"
check S2 "passphrase cookie present" "yes" "$([ -n "$ENC_PASS" ] && echo yes || echo no)"
# The token must be fresh, or the sidecar (correctly) delegates.
q "UPDATE oc_authtoken SET last_check=extract(epoch from now())::bigint WHERE uid='alice'" >/dev/null
q "UPDATE oc_authtoken SET password_invalid=false WHERE uid='alice'" >/dev/null

# Copy the crafting helper into the container.
docker cp "$LOCAL_DIR/session_craft.php" "$NC:/tmp/session_craft.php" >/dev/null
docker exec "$NC" chmod 644 /tmp/session_craft.php

# ---------------------------------------------------------------------------
sec "2. real browser form login (the web UI's session)"

# A form login creates a TEMPORARY_TOKEN (type 0) whose token is the session
# id, and the session carries no `app_password`. Revalidation therefore takes
# the session path, not the app-password type gate.
FORM_JAR="$STATE_DIR/form.jar"
FORM_RT=$(form_login "$FORM_JAR")
check F0 "real form login succeeds" "yes" "$([ -n "$FORM_RT" ] && echo yes || echo no)"

# The form-login session has no AUTHENTICATED_TO_DAV_BACKEND, so PHP's DAV auth
# takes branch 1 and runs its CSRF check: the requesttoken header the web UI
# sends is required. Prove PHP accepts the session first (207), otherwise the
# sidecar assertion below would be vacuous.
php_code=$(curl -s -o "$TMP/php-f1.xml" -D "$TMP/php-f1.hdr" -w '%{http_code}' \
	-b "$FORM_JAR" -H "requesttoken: $FORM_RT" -X PROPFIND -H 'Depth: 0' \
	-H 'Content-Type: application/xml' --data-binary "$PF_BODY" \
	"$NC_URL/remote.php/dav/files/alice/")
check F1.1 "PHP serves the real form-login session (no Authorization)" "207" "$php_code"
check F1.2 "PHP response has no sidecar header" "no" "$(has_any_sidecar_header php-f1)"

st=$(curl -s -o "$TMP/f1.xml" -D "$TMP/f1.hdr" -w '%{http_code}' \
	-b "$FORM_JAR" -H "requesttoken: $FORM_RT" -X PROPFIND -H 'Depth: 0' \
	-H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check F1.3 "sidecar serves the real form-login session" "207" "$st"
check F1.4 "sidecar attribution on the form-login session" "yes" "$(has_sidecar_header f1)"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/php-f1.xml" 2>/dev/null >"$TMP/php-f1.canon"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/f1.xml" 2>/dev/null >"$TMP/f1.canon"
if diff -u "$TMP/php-f1.canon" "$TMP/f1.canon" >"$TMP/f1.diff"; then
	check F1.5 "form-login canonical body identical to PHP" "identical" "identical"
else
	check F1.5 "form-login canonical body identical to PHP" "identical" "different ($(wc -l <"$TMP/f1.diff") diff lines)"
fi

# Literal cookie-only (no requesttoken): PHP's CSRF check answers 401, so the
# sidecar must delegate. This documents why the web UI's requesttoken header
# is part of a real web-UI request.
php_code=$(curl -s -o /dev/null -w '%{http_code}' -b "$FORM_JAR" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$NC_URL/remote.php/dav/files/alice/")
check F2.1 "PHP rejects a no-requesttoken form session (CSRF)" "401" "$php_code"
st=$(curl -s -o /dev/null -D "$TMP/f2.hdr" -w '%{http_code}' -b "$FORM_JAR" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$ALICE_FILES")
check F2.2 "no-requesttoken form session -> 501" "501" "$st"
check F2.3 "no-requesttoken form session -> no sidecar header" "no" "$(has_any_sidecar_header f2)"

# Reverse: a form-login session for alice requesting bob's path must delegate.
st=$(curl -s -o /dev/null -D "$TMP/f3.hdr" -w '%{http_code}' -b "$FORM_JAR" \
	-H "requesttoken: $FORM_RT" -X PROPFIND -H 'Depth: 0' \
	-H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$BOB_FILES")
check F3.1 "form session on another user's path -> 501" "501" "$st"
check F3.2 "another user's path -> no sidecar header" "no" "$(has_any_sidecar_header f3)"

# ---------------------------------------------------------------------------
sec "3. accept cases (cookie-only, sidecar attribution + PHP parity)"

# A1: cookie-only PROPFIND on the owner's own path.
st=$(side_request a1 PROPFIND "$ALICE_FILES" -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY")
check A1.1 "cookie-only PROPFIND served natively" "207" "$st"
check A1.2 "sidecar attribution" "yes" "$(has_sidecar_header a1)"

# PHP's answer for the same cookie-only request, canonicalised and diffed.
php_code=$(curl -s -o "$TMP/php-a1.xml" -D "$TMP/php-a1.hdr" -w '%{http_code}' \
	-b "$SESSION_JAR" -X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$NC_URL/remote.php/dav/files/alice/")
check A1.3 "PHP answers the cookie-only request" "207" "$php_code"
check A1.4 "PHP response has no sidecar header" "no" "$(has_any_sidecar_header php-a1)"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/php-a1.xml" 2>"$TMP/php-a1.count" >"$TMP/php-a1.canon"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/a1.xml" 2>"$TMP/a1.count" >"$TMP/a1.canon"
if diff -u "$TMP/php-a1.canon" "$TMP/a1.canon" >"$TMP/a1.diff"; then
	check A1.5 "canonical body identical to PHP" "identical" "identical"
else
	check A1.5 "canonical body identical to PHP" "identical" "different ($(wc -l <"$TMP/a1.diff") diff lines)"
fi

# A2: a second request on the same session.
st=$(side_request a2 PROPFIND "$ALICE_FILES" -H 'Depth: 1' -H 'Content-Type: application/xml' --data-binary "$PF_BODY_STRUCT")
check A2.1 "second request on the same session" "207" "$st"
check A2.2 "sidecar attribution" "yes" "$(has_sidecar_header a2)"
php_code=$(curl -s -o "$TMP/php-a2.xml" -w '%{http_code}' -b "$SESSION_JAR" \
	-X PROPFIND -H 'Depth: 1' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY_STRUCT" "$NC_URL/remote.php/dav/files/alice/")
check A2.3 "PHP answers Depth 1" "207" "$php_code"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/php-a2.xml" 2>/dev/null >"$TMP/php-a2.canon"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/a2.xml" 2>/dev/null >"$TMP/a2.canon"
if diff -u "$TMP/php-a2.canon" "$TMP/a2.canon" >"$TMP/a2.diff"; then
	check A2.4 "Depth 1 canonical body identical to PHP" "identical" "identical"
else
	check A2.4 "Depth 1 canonical body identical to PHP" "identical" "different ($(wc -l <"$TMP/a2.diff") diff lines)"
fi

# A3: a REPORT.
st=$(side_request a3 REPORT "$ALICE_BOOK" -H 'Depth: 1' -H 'Content-Type: application/xml' --data-binary "$REPORT_BODY")
check A3.1 "cookie-only REPORT served natively" "207" "$st"
check A3.2 "sidecar attribution" "yes" "$(has_sidecar_header a3)"
php_code=$(curl -s -o "$TMP/php-a3.xml" -w '%{http_code}' -b "$SESSION_JAR" \
	-X REPORT -H 'Depth: 1' -H 'Content-Type: application/xml' \
	--data-binary "$REPORT_BODY" "$NC_URL/remote.php/dav/addressbooks/users/alice/contacts/")
check A3.3 "PHP answers the REPORT" "207" "$php_code"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/php-a3.xml" 2>/dev/null >"$TMP/php-a3.canon"
python3 "$LOCAL_DIR/canonicalize_propfind.py" "$TMP/a3.xml" 2>/dev/null >"$TMP/a3.canon"
if diff -u "$TMP/php-a3.canon" "$TMP/a3.canon" >"$TMP/a3.diff"; then
	check A3.4 "REPORT canonical body identical to PHP" "identical" "identical"
else
	check A3.4 "REPORT canonical body identical to PHP" "identical" "different ($(wc -l <"$TMP/a3.diff") diff lines)"
fi

# ---------------------------------------------------------------------------
sec "5. review follow-ups (F3 app_password type, F4 scope, F5 requesttoken)"

# T1/F3: a present, non-string app_password is a parse failure in PHP and must
# now be one in the sidecar too (previously: 2FA skipped, session id revalidated).
NUMAPP_ID=$(new_sid)
craft_session set_number_app_password "$NUMAPP_ID"
rewrite_jar "$TMP/numapp.jar" "$NUMAPP_ID" "$ENC_PASS"
php_code=$(curl -s -o /dev/null -w '%{http_code}' -b "$TMP/numapp.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$NC_URL/remote.php/dav/files/alice/")
check T1.1 "PHP rejects a numeric app_password" "401" "$php_code"
st=$(curl -s -o /dev/null -D "$TMP/t1.hdr" -w '%{http_code}' -b "$TMP/numapp.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$ALICE_FILES")
check T1.2 "numeric app_password -> 501" "501" "$st"
check T1.3 "numeric app_password -> no sidecar header" "no" "$(has_any_sidecar_header t1)"

# T2/F4: mint a real filesystem-scoped app password (occ), flip its scope, and
# prove the files tree now delegates instead of serving DB metadata.
SC_F2_PW=$(occ user:auth-tokens:add alice --name scoped-f2 --no-interaction 2>/dev/null | tail -n 1)
check T2.0 "scoped app password minted" "yes" "$([ ${#SC_F2_PW} -ge 32 ] && echo yes || echo no)"
q "UPDATE oc_authtoken SET scope='{\"filesystem\":false}', last_check=extract(epoch from now())::bigint, last_activity=extract(epoch from now())::bigint WHERE name='scoped-f2'" >/dev/null
st=$(curl -s -o /dev/null -D "$TMP/t2.hdr" -w '%{http_code}' \
	-u "alice:$SC_F2_PW" -X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$ALICE_FILES")
check T2.1 "filesystem-scoped token -> 501" "501" "$st"
check T2.2 "filesystem-scoped token -> no sidecar header" "no" "$(has_any_sidecar_header t2)"
# PHP's own answer for the same credential: lockdown replaces the home with a
# NullStorage, so PHP still returns 207 but with no children (Depth 1).
php_code=$(curl -s -o "$TMP/t2-php.xml" -w '%{http_code}' -u "alice:$SC_F2_PW" \
	-X PROPFIND -H 'Depth: 1' -H 'Content-Type: application/xml' \
	--data-binary "$PF_BODY" "$NC_URL/remote.php/dav/files/alice/")
php_entries=$(grep -o '<d:response>' "$TMP/t2-php.xml" | wc -l)
check T2.3 "PHP serves an empty scoped listing" "1" "$php_entries"
q "DELETE FROM oc_authtoken WHERE name='scoped-f2'" >/dev/null
unset SC_F2_PW

# T3/F5: PHP prefers the GET param over the requesttoken header. A valid header
# plus a bogus query param fails PHP's CSRF check, so the sidecar must delegate.
php_code=$(curl -s -o /dev/null -w '%{http_code}' -b "$FORM_JAR" \
	-H "requesttoken: $FORM_RT" -X PROPFIND -H 'Depth: 0' \
	-H 'Content-Type: application/xml' --data-binary "$PF_BODY" \
	"$NC_URL/remote.php/dav/files/alice/?requesttoken=not-the-token")
check T3.1 "PHP: query param wins over header" "401" "$php_code"
st=$(curl -s -o /dev/null -D "$TMP/t3.hdr" -w '%{http_code}' -b "$FORM_JAR" \
	-H "requesttoken: $FORM_RT" -X PROPFIND -H 'Depth: 0' \
	-H 'Content-Type: application/xml' --data-binary "$PF_BODY" \
	"$ALICE_FILES?requesttoken=not-the-token")
check T3.2 "query param wins over header -> 501" "501" "$st"
check T3.3 "query param wins over header -> no sidecar header" "no" "$(has_any_sidecar_header t3)"

# ---------------------------------------------------------------------------
sec "4. delegate cases (501, no sidecar header)"

# D1: no cookie at all.
st=$(curl -s -o /dev/null -D "$TMP/d1.hdr" -w '%{http_code}' -X PROPFIND \
	-H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D1.1 "no cookie -> 501" "501" "$st"
check D1.2 "no cookie -> no sidecar header" "no" "$(has_any_sidecar_header d1)"

# D2: forged/tampered ciphertext.
TAMPER_ID=$(new_sid)
craft_session tamper "$TAMPER_ID"
rewrite_jar "$TMP/tamper.jar" "$TAMPER_ID" "$ENC_PASS"
st=$(curl -s -o /dev/null -D "$TMP/d2.hdr" -w '%{http_code}' -b "$TMP/tamper.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D2.1 "tampered ciphertext -> 501" "501" "$st"
check D2.2 "tampered ciphertext -> no sidecar header" "no" "$(has_any_sidecar_header d2)"

# D3: wrong passphrase (real session value, wrong cookie).
rewrite_jar "$TMP/wrongpass.jar" "$SID" "wrong-passphrase-value"
st=$(curl -s -o /dev/null -D "$TMP/d3.hdr" -w '%{http_code}' -b "$TMP/wrongpass.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D3.1 "wrong passphrase -> 501" "501" "$st"
check D3.2 "wrong passphrase -> no sidecar header" "no" "$(has_any_sidecar_header d3)"

# D4: truncated blob.
TRUNC_ID=$(new_sid)
craft_session truncate "$TRUNC_ID"
rewrite_jar "$TMP/trunc.jar" "$TRUNC_ID" "$ENC_PASS"
st=$(curl -s -o /dev/null -D "$TMP/d4.hdr" -w '%{http_code}' -b "$TMP/trunc.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D4.1 "truncated blob -> 501" "501" "$st"
check D4.2 "truncated blob -> no sidecar header" "no" "$(has_any_sidecar_header d4)"

# D5: absent Redis key (logout / expired).
MISSING_ID=$(new_sid)
rewrite_jar "$TMP/missing.jar" "$MISSING_ID" "$ENC_PASS"
st=$(curl -s -o /dev/null -D "$TMP/d5.hdr" -w '%{http_code}' -b "$TMP/missing.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D5.1 "absent Redis key -> 501" "501" "$st"
check D5.2 "absent Redis key -> no sidecar header" "no" "$(has_any_sidecar_header d5)"

# D6: a session whose user_id is another user.
OTHER_ID=$(new_sid)
craft_session set_user_id "$OTHER_ID" "bob"
rewrite_jar "$TMP/otheruser.jar" "$OTHER_ID" "$ENC_PASS"
st=$(curl -s -o /dev/null -D "$TMP/d6.hdr" -w '%{http_code}' -b "$TMP/otheruser.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D6.1 "session user_id=other -> 501" "501" "$st"
check D6.2 "session user_id=other -> no sidecar header" "no" "$(has_any_sidecar_header d6)"

# D7: a path belonging to another user (real alice session, bob's path).
st=$(curl -s -o /dev/null -D "$TMP/d7.hdr" -w '%{http_code}' -b "$SESSION_JAR" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$BOB_FILES")
check D7.1 "path owned by another user -> 501" "501" "$st"
check D7.2 "path owned by another user -> no sidecar header" "no" "$(has_any_sidecar_header d7)"

# D8: 2FA not proven (app_password stripped).
NO2FA_ID=$(new_sid)
craft_session strip_app_password "$NO2FA_ID"
rewrite_jar "$TMP/no2fa.jar" "$NO2FA_ID" "$ENC_PASS"
st=$(curl -s -o /dev/null -D "$TMP/d8.hdr" -w '%{http_code}' -b "$TMP/no2fa.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D8.1 "2FA not proven -> 501" "501" "$st"
check D8.2 "2FA not proven -> no sidecar header" "no" "$(has_any_sidecar_header d8)"

# D9: revoked/invalid token (password_invalid), restored afterwards.
q "UPDATE oc_authtoken SET password_invalid=true WHERE uid='alice'" >/dev/null
st=$(curl -s -o /dev/null -D "$TMP/d9.hdr" -w '%{http_code}' -b "$SESSION_JAR" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
q "UPDATE oc_authtoken SET password_invalid=false WHERE uid='alice'" >/dev/null
check D9.1 "revoked token -> 501" "501" "$st"
check D9.2 "revoked token -> no sidecar header" "no" "$(has_any_sidecar_header d9)"

# D10: branch 1 (no DAV flag) on a non-exempt method with a wrong requesttoken.
NODAV_ID=$(new_sid)
craft_session strip_dav_flag "$NODAV_ID"
rewrite_jar "$TMP/nodav.jar" "$NODAV_ID" "$ENC_PASS"
st=$(curl -s -o /dev/null -D "$TMP/d10.hdr" -w '%{http_code}' -b "$TMP/nodav.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	-H 'requesttoken: definitely-not-the-token' --data-binary "$PF_BODY" "$ALICE_FILES")
check D10.1 "wrong requesttoken (non-exempt) -> 501" "501" "$st"
check D10.2 "wrong requesttoken -> no sidecar header" "no" "$(has_any_sidecar_header d10)"

# D11: branch 1 with no requesttoken at all.
st=$(curl -s -o /dev/null -D "$TMP/d11.hdr" -w '%{http_code}' -b "$TMP/nodav.jar" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
check D11.1 "missing requesttoken (non-exempt) -> 501" "501" "$st"
check D11.2 "missing requesttoken -> no sidecar header" "no" "$(has_any_sidecar_header d11)"

# D12: branch 1 with a wrong token and no strict cookie.
st=$(curl -s -o /dev/null -D "$TMP/d12.hdr" -w '%{http_code}' \
	-b "$IID=$NODAV_ID; oc_sessionPassphrase=$ENC_PASS" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
	-H 'requesttoken: definitely-not-the-token' --data-binary "$PF_BODY" "$ALICE_FILES")
check D12.1 "missing strict cookie -> 501" "501" "$st"
check D12.2 "missing strict cookie -> no sidecar header" "no" "$(has_any_sidecar_header d12)"

# D13: Redis unreachable.
docker stop "$(docker ps --filter name=ncdav-e2e-redis -q)" >/dev/null 2>&1 || true
st=$(curl -s -o /dev/null -D "$TMP/d13.hdr" -w '%{http_code}' -b "$SESSION_JAR" \
	-X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' --data-binary "$PF_BODY" "$ALICE_FILES")
docker start "$(docker ps -a --filter name=ncdav-e2e-redis -q)" >/dev/null 2>&1 || true
check D13.1 "Redis unreachable -> 501" "501" "$st"
check D13.2 "Redis unreachable -> no sidecar header" "no" "$(has_any_sidecar_header d13)"

# ---------------------------------------------------------------------------
sec "SUMMARY"
echo "PASS=$PASS FAIL=$FAIL"
printf 'TOTAL|PASS=%d|FAIL=%d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
