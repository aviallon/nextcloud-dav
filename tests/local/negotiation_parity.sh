#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Differential check: `address-data` negotiation (vCard 3<->4 conversion, jCard,
# the `<card:prop>` filter) and conditional GET/HEAD — sidecar vs PHP — on the
# disposable Nextcloud 33.0.5 harness.
#
# Run ./setup.sh first (./setup.sh --rebuild after code changes). This only
# writes to the throwaway harness database.
#
# Three cases are DECLARED divergences (tests/deviations.toml:
# `vcard-version-negotiation-missing`, `conditional-get-missing`) and are
# asserted here in the divergent direction, on BOTH sides:
#
#   - the `<card:prop>` filter is applied in addressbook-multiget by the
#     sidecar (Sabre's call site omits it) and matches case-insensitively
#     (Sabre's array_diff is case-sensitive);
#   - `HEAD` + matching `If-None-Match` is 304 per RFC 7232 (PHP: 412);
#   - `If-None-Match` uses weak comparison (PHP: 200, body re-sent).
#
# Everything else must be byte-identical to PHP's own vobject 4.5.6 output.
# Evidence is written to state/evidence/negotiation-parity.txt.
set -uo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$LOCAL_DIR/lib.sh"
load_env

EVIDENCE_FILE="$EVIDENCE_DIR/negotiation-parity.txt"
CURLRC="$STATE_DIR/curlrc"
: >"$EVIDENCE_FILE"
exec > >(tee -a "$EVIDENCE_FILE") 2>&1

BOOK="/remote.php/dav/addressbooks/users/alice/contacts"
PARITY_OK=1

sec() { printf '\n===== %s =====\n' "$*"; }
pass() { printf 'ok: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*"; PARITY_OK=0; }

# Extracts the (entity-unescaped) address-data payloads from a multistatus —
# ALL responses, so a per-card assertion cannot silently test a different card.
EXTRACT='
import re, sys, html
data = sys.stdin.read()
ms = re.findall(r"<card:address-data[^>]*>(.*?)</card:address-data>", data, re.S)
# No trailing newline is appended: the payload itself is byte-exact and blocks
# stay comparable after sorting (the declared query-report-order deviation).
sys.stdout.write("\n--\n".join(html.unescape(m) for m in ms) if ms else "NOPROP\n")
'

# Torture cards: structured values, TYPE multi-params, binary PHOTO, Apple
# anniversary pair (v3) and year-less dates + KIND (v4). $'...' quoting so the
# CRLFs are real and the vCard-level backslash escapes survive verbatim — on
# ONE line each: a `\` + newline inside $'...' is not a continuation and would
# inject a stray backslash into the card.
CARD1=$'BEGIN:VCARD\r\nVERSION:3.0\r\nUID:parity-1\r\nFN:Parity Te\;st, Jr\r\nN:Doe;Jane;Q.;Jr.;MD\r\nEMAIL;TYPE=WORK,INTERNET:jane@example.com\r\nTEL;TYPE=CELL:+3312345678\r\nBDAY:1985-04-12\r\nPHOTO;ENCODING=b;TYPE=JPEG:QUFBQQ==\r\nX-ABSHOWAS:COMPANY\r\nitem1.X-ABLABEL:_$!<Anniversary>!$_\r\nitem1.X-ABDATE:1990-06-01\r\nNOTE:Line one\\nLine two\r\nCATEGORIES:Friends,Work\r\nEND:VCARD\r\n'
CARD2=$'BEGIN:VCARD\r\nVERSION:4.0\r\nUID:parity-2\r\nFN:Year Less\r\nBDAY:--04-12\r\nANNIVERSARY:--06-01\r\nKIND:group\r\nEND:VCARD\r\n'

put_card() { # uri, body
	printf '%s' "$2" | curl -s -m 30 -o /dev/null -w "%{http_code}" -K "$CURLRC" -X PUT \
		-H 'Content-Type: text/vcard' --data-binary @- "$SIDECAR_URL$BOOK/$1"
}

multiget_body() { # uri, attrs, filter
	printf '<?xml version="1.0"?>\n<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">\n  <d:prop><d:getetag/><card:address-data%s>%s</card:address-data></d:prop>\n  <d:href>%s/%s</d:href>\n</card:addressbook-multiget>' \
		"$2" "$3" "$BOOK" "$1"
}

query_body() { # filter-name-attr
	printf '<?xml version="1.0"?>\n<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">\n  <d:prop><card:address-data><card:prop name="%s"/></card:address-data></d:prop>\n</card:addressbook-query>' \
		"$1"
}

fetch() { # backend-url, body  -> address-data payload on stdout
	printf '%s' "$2" | curl -s -m 30 -K "$CURLRC" -X REPORT -H 'Content-Type: application/xml' \
		--data-binary @- "$1$BOOK/" | python3 -c "$EXTRACT"
}

fetch_query() { # backend-url, body
	printf '%s' "$2" | curl -s -m 30 -K "$CURLRC" -X REPORT -H 'Depth: 1' \
		-H 'Content-Type: application/xml' --data-binary @- "$1$BOOK/" |
		python3 -c "$EXTRACT"
}

case_identical() { # name, body
	local name="$1" body="$2"
	fetch "$SIDECAR_URL" "$body" > /tmp/np-s.txt
	fetch "$NC_URL" "$body" > /tmp/np-p.txt
	compare_payloads "$name"
}

case_identical_query() { # name, body  (addressbook-query needs Depth: 1)
	local name="$1" body="$2"
	fetch_query "$SIDECAR_URL" "$body" > /tmp/np-s.raw
	fetch_query "$NC_URL" "$body" > /tmp/np-p.raw
	# The response ORDER is the declared `query-report-order` deviation
	# (sidecar: oc_cards.id order; PHP: its ORDER-LESS getChildren(), observed
	# as URI order), so the query result is compared as a SET.
	sort_blocks < /tmp/np-s.raw > /tmp/np-s.txt
	sort_blocks < /tmp/np-p.raw > /tmp/np-p.txt
	compare_payloads "$name"
}

sort_blocks() {
	python3 -c 'import sys
blocks = [b for b in sys.stdin.read().split("\n--\n") if b]
sys.stdout.write("\n--\n".join(sorted(blocks)) + "\n")'
}

compare_payloads() { # name
	local name="$1"
	# Guard against a vacuous pass: two missing payloads compare "identical".
	if grep -qx 'NOPROP' /tmp/np-s.txt || grep -qx 'NOPROP' /tmp/np-p.txt; then
		fail "$name: no address-data payload (empty card or bad request)"
		return
	fi
	if cmp -s /tmp/np-s.txt /tmp/np-p.txt; then
		pass "$name: byte-identical to PHP"
	else
		fail "$name: differs from PHP"
		diff /tmp/np-p.txt /tmp/np-s.txt | head -10
	fi
}

sec "preconditions"
wait_for_nextcloud >/dev/null
wait_for_sidecar
echo "sidecar: $(curl -s -m 30 "$SIDECAR_URL/healthz" | head -c 200)"
echo "evidence: $EVIDENCE_FILE"

sec "seed torture cards via the sidecar's native PUT"
# Seed failures are fatal: every later case would silently compare nothing.
case "$(put_card np-parity1.vcf "$CARD1")" in
201 | 204) pass "PUT np-parity1.vcf" ;;
*) echo "FATAL: cannot seed np-parity1.vcf"; exit 1 ;;
esac
case "$(put_card np-parity2.vcf "$CARD2")" in
201 | 204) pass "PUT np-parity2.vcf" ;;
*) echo "FATAL: cannot seed np-parity2.vcf"; exit 1 ;;
esac

sec "address-data negotiation (identical cases)"
case_identical "plain verbatim (v3)" "$(multiget_body np-parity1.vcf '' '')"
case_identical "plain verbatim (v4)" "$(multiget_body np-parity2.vcf '' '')"
case_identical "version=4.0 on v3 (3->4)" "$(multiget_body np-parity1.vcf ' version="4.0"' '')"
case_identical "version=3.0 on v4 (4->3)" "$(multiget_body np-parity2.vcf ' version="3.0"' '')"
case_identical "version=2.1 (target is 3.0)" "$(multiget_body np-parity2.vcf ' version="2.1"' '')"
case_identical "jcard (v3)" "$(multiget_body np-parity1.vcf ' content-type="application/vcard+json"' '')"
case_identical "jcard (v4)" "$(multiget_body np-parity2.vcf ' content-type="application/vcard+json"' '')"
case_identical_query "query filter EMAIL (both apply it)" "$(query_body EMAIL)"

sec "declared divergences (asserted in the divergent direction, both sides)"
# 1. The multiget filter: the sidecar applies it, Sabre's call site does not.
BODY=$(multiget_body np-parity1.vcf '' '<card:prop name="EMAIL"/>')
fetch "$SIDECAR_URL" "$BODY" > /tmp/np-s.txt
fetch "$NC_URL" "$BODY" > /tmp/np-p.txt
if grep -q 'EMAIL' /tmp/np-s.txt && ! grep -q 'N:Doe' /tmp/np-s.txt; then
	pass "multiget filter: sidecar filters (EMAIL kept, N dropped)"
else
	fail "multiget filter: sidecar did not filter"
fi
if grep -q 'N:Doe' /tmp/np-p.txt; then
	pass "multiget filter: PHP keeps the full card (Sabre omits the filter)"
else
	fail "multiget filter: PHP behaviour changed (it now filters?)"
fi

# 2. Filter names: sidecar case-insensitive, Sabre case-sensitive.
BODY=$(query_body email)
fetch_query "$SIDECAR_URL" "$BODY" > /tmp/np-s.txt
fetch_query "$NC_URL" "$BODY" > /tmp/np-p.txt
if grep -q 'EMAIL' /tmp/np-s.txt; then
	pass "lowercase filter name: sidecar keeps EMAIL (case-insensitive)"
else
	fail "lowercase filter name: sidecar dropped EMAIL"
fi
if ! grep -q 'EMAIL' /tmp/np-p.txt; then
	pass "lowercase filter name: PHP drops EMAIL (case-sensitive array_diff)"
else
	fail "lowercase filter name: PHP behaviour changed"
fi

sec "GET Accept negotiation (httpAfterGet parity)"
accept_case() { # name, accept, expected-content-type
	local name="$1" accept="$2" ct="$3"
	local sp pp sct pct
	sp=$(curl -s -m 30 -D /tmp/np-sh -o /tmp/np-sb -w '%{http_code}' -K "$CURLRC" \
		-H "Accept: $accept" "$SIDECAR_URL$BOOK/np-parity1.vcf")
	pp=$(curl -s -m 30 -D /tmp/np-ph -o /tmp/np-pb -w '%{http_code}' -K "$CURLRC" \
		-H "Accept: $accept" "$NC_URL$BOOK/np-parity1.vcf")
	sct=$(tr -d '\r' < /tmp/np-sh | grep -i '^content-type:' | sed 's/^[^:]*: //')
	pct=$(tr -d '\r' < /tmp/np-ph | grep -i '^content-type:' | sed 's/^[^:]*: //')
	if [ "$sp:$sct" = "200:$ct" ] && [ "$pp:$pct" = "200:$ct" ] &&
		cmp -s /tmp/np-sb /tmp/np-pb; then
		pass "$name: identical (ct=$ct, body byte-identical)"
	else
		fail "$name: sidecar=$sp/$sct php=$pp/$pct want=200/$ct body-equal=$(cmp -s /tmp/np-sb /tmp/np-pb && echo yes || echo no)"
	fi
}

accept_case "GET Accept=json" 'application/vcard+json' 'application/vcard+json; charset=utf-8'
accept_case "GET Accept=v4" 'text/vcard; version=4.0' 'text/vcard; version=4.0; charset=utf-8'
accept_case "GET Accept list (q-values)" 'text/vcard;q=0.5, application/vcard+json;q=0.9' 'application/vcard+json; charset=utf-8'
accept_case "GET Accept unknown (vcard3 fallback)" 'application/xml' 'text/vcard; charset=utf-8'

# Declared divergence: PHP's httpAfterGet parses the empty HEAD body and 500s
# (convertVCard('')); RFC 7232 section 6 says HEAD follows GET.
HS=$(curl -s -m 30 -o /dev/null -w '%{http_code}' -K "$CURLRC" --head \
	-H 'Accept: application/vcard+json' "$SIDECAR_URL$BOOK/np-parity1.vcf")
HP=$(curl -s -m 30 -o /dev/null -w '%{http_code}' -K "$CURLRC" --head \
	-H 'Accept: application/vcard+json' "$NC_URL$BOOK/np-parity1.vcf")
if [ "$HS" = 200 ] && [ "$HP" = 500 ]; then
	pass "HEAD + Accept (divergent): sidecar=$HS php=$HP (as declared)"
else
	fail "HEAD + Accept: sidecar=$HS (want 200) php=$HP (want 500)"
fi

sec "conditional GET/HEAD (status codes)"
ETAG=$(curl -s -m 30 -D - -o /dev/null -K "$CURLRC" "$SIDECAR_URL$BOOK/np-parity1.vcf" |
	tr -d '\r' | awk 'tolower($1)=="etag:" {print $2}')
echo "etag: $ETAG"
FUTURE=$(LC_ALL=C date -u -d @$(( $(date +%s) + 60 )) '+%a, %d %b %Y %H:%M:%S GMT')
PAST=$(LC_ALL=C date -u -d @$(( $(date +%s) - 86400 )) '+%a, %d %b %Y %H:%M:%S GMT')

code() { # method, backend-url, header...
	local m="$1" u="$2" h="$3"
	if [ "$m" = HEAD ]; then
		# `curl -X HEAD` hangs (it waits for a body): --head is the HEAD form.
		curl -s -m 30 -o /dev/null -w '%{http_code}' -K "$CURLRC" --head \
			-H "$h" "$u$BOOK/np-parity1.vcf"
	else
		curl -s -m 30 -o /dev/null -w '%{http_code}' -K "$CURLRC" -X "$m" \
			-H "$h" "$u$BOOK/np-parity1.vcf"
	fi
}

check_pair() { # name, header, expect-sidecar, expect-php, method
	local name="$1" h="$2" es="$3" ep="$4" m="${5:-GET}"
	local s p
	s=$(code "$m" "$SIDECAR_URL" "$h")
	p=$(code "$m" "$NC_URL" "$h")
	if [ "$s" = "$es" ] && [ "$p" = "$ep" ]; then
		pass "$name: sidecar=$s php=$p (as declared)"
	else
		fail "$name: sidecar=$s (want $es) php=$p (want $ep)"
	fi
}

check_pair "GET + matching If-None-Match" "If-None-Match: $ETAG" 304 304
check_pair "GET + If-None-Match miss" 'If-None-Match: "deadbeef"' 200 200
check_pair "GET + If-None-Match *" 'If-None-Match: *' 304 304
check_pair "GET + future If-Modified-Since" "If-Modified-Since: $FUTURE" 304 304
check_pair "GET + stale If-Modified-Since" "If-Modified-Since: $PAST" 200 200
check_pair "GET + stale If-Unmodified-Since" "If-Unmodified-Since: $PAST" 412 412
# Declared divergence: RFC 7232 says HEAD follows GET; PHP answers 412
# (checkPreconditions runs before httpHead() rewrites the method).
check_pair "HEAD + matching If-None-Match (divergent)" "If-None-Match: $ETAG" 304 412 HEAD
# Declared divergence: RFC 7232 weak comparison; PHP compares raw strings.
check_pair "GET + weak If-None-Match (divergent)" "If-None-Match: W/$ETAG" 304 200

sec "summary"
if [ "$PARITY_OK" = 1 ]; then
	echo "NEGOTIATION PARITY: PASS"
else
	echo "NEGOTIATION PARITY: FAIL"
fi
echo "evidence: $EVIDENCE_FILE"

[ "$PARITY_OK" = 1 ]
