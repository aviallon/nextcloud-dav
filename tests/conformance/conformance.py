#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Differential CardDAV conformance harness: nextcloud-dav vs Nextcloud PHP.

Issues the *same* request to the sidecar and to the PHP/SabreDAV backend and
compares status codes, hrefs, ETags and card bodies, emitting a machine-readable
JSON report.

How PHP is reached while the sidecar owns the route
---------------------------------------------------
nginx routes every address-book sub-path to the sidecar. Two documented
bypasses force the request through PHP (see
`.pi/skills/nextcloud-dav-parity/SKILL.md`):

  * double slash after ``users/``: ``.../users//alice/contacts/x.vcf`` — PHP
    normalises it. Used for per-resource requests.
  * ``?export=1`` — the sidecar answers 501 and nginx falls back to PHP. Used
    for collection-level requests.

The harness only ever issues reads. It is safe to point at production
read-only; it never PUTs, DELETEs or creates anything.

Usage
-----
    python3 conformance.py --base https://cloud.example \
        --user alice --password '<app-password>' [--book contacts] \
        [--limit 25] [--json report.json] [--insecure] [--verbose]

Exit status is 0 when every check passes, 1 otherwise. ``--json -`` writes the
report to stdout.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import ssl
import sys
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET

DAV = "DAV:"
CARDDAV = "urn:ietf:params:xml:ns:carddav"
SYNC_PREFIX = "http://sabre.io/ns/sync/"

PROPFIND_PROPS = (
    '<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">'
    "<d:prop><d:getetag/><card:address-data/></d:prop></d:propfind>"
)

MULTIGET_TEMPLATE = (
    '<?xml version="1.0"?>'
    '<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">'
    "<d:prop><d:getetag/><card:address-data/></d:prop>{hrefs}"
    "</card:addressbook-multiget>"
)

SYNC_BODY = (
    '<?xml version="1.0"?>'
    '<d:sync-collection xmlns:d="DAV:">'
    "<d:sync-token/>"
    "<d:prop><d:getetag/></d:prop>"
    "</d:sync-collection>"
)

QUERY_BODY = (
    '<?xml version="1.0"?>'
    '<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">'
    "<d:prop><d:getetag/></d:prop>"
    '<card:filter><card:prop-filter name="FN">'
    "<card:text-match>a</card:text-match>"
    "</card:prop-filter></card:filter>"
    "</card:addressbook-query>"
)


class HttpResult:
    def __init__(self, status: int, headers: dict, body: bytes):
        self.status = status
        self.headers = headers
        self.body = body


def _opener(insecure: bool) -> urllib.request.OpenerDirector:
    handlers = []
    if insecure:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        handlers.append(urllib.request.HTTPSHandler(context=ctx))
    return urllib.request.build_opener(*handlers)


def request(
    opener: urllib.request.OpenerDirector,
    url: str,
    method: str,
    auth: str,
    body: bytes | None = None,
    depth: str | None = None,
    extra_headers: dict | None = None,
) -> HttpResult:
    headers = {"Authorization": auth, "User-Agent": "nextcloud-dav-conformance/1"}
    if body is not None:
        headers["Content-Type"] = "application/xml; charset=utf-8"
    if depth is not None:
        headers["Depth"] = depth
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with opener.open(req, timeout=60) as resp:
            return HttpResult(resp.status, dict(resp.headers), resp.read())
    except urllib.error.HTTPError as err:
        return HttpResult(err.code, dict(err.headers), err.read())


def parse_multistatus(body: bytes) -> ET.Element:
    return ET.fromstring(body)


def responses_by_href(root: ET.Element) -> dict:
    """href -> {'etag': str|None, 'address_data': bytes|None}."""
    out = {}
    for response in root.findall(f"{{{DAV}}}response"):
        href_el = response.find(f"{{{DAV}}}href")
        if href_el is None or href_el.text is None:
            continue
        href = href_el.text
        etag = None
        address_data = None
        for propstat in response.findall(f"{{{DAV}}}propstat"):
            status = propstat.find(f"{{{DAV}}}status")
            if status is None or " 200 " not in (status.text or ""):
                continue
            prop = propstat.find(f"{{{DAV}}}prop")
            if prop is None:
                continue
            e = prop.find(f"{{{DAV}}}getetag")
            if e is not None:
                etag = (e.text or "").strip()
            a = prop.find(f"{{{CARDDAV}}}address-data")
            if a is not None:
                address_data = (a.text or "").encode("utf-8")
        out[href] = {"etag": etag, "address_data": address_data}
    return out


def sync_token(root: ET.Element) -> str | None:
    el = root.find(f"{{{DAV}}}sync-token")
    return el.text if el is not None else None


def strip_weak(etag: str | None) -> str | None:
    """Drop a `W/` prefix (the accepted GET-ETag deviation)."""
    if etag is None:
        return None
    return etag[2:] if etag.startswith("W/") else etag


def sha(data: bytes | None) -> str | None:
    return None if data is None else hashlib.sha256(data).hexdigest()


class Check:
    def __init__(self, name: str):
        self.name = name
        self.ok = True
        self.notes: list[str] = []
        self.detail: dict = {}

    def fail(self, note: str) -> None:
        self.ok = False
        self.notes.append(note)

    def to_json(self) -> dict:
        return {"name": self.name, "ok": self.ok, "notes": self.notes, "detail": self.detail}


def php_path(path: str) -> str:
    """Force a per-resource request through PHP with the double-slash bypass."""
    return path.replace("/users/", "/users//", 1)


def join(base: str, path: str) -> str:
    return base.rstrip("/") + path


def compare_href_sets(check: Check, sidecar: dict, php: dict) -> None:
    only_side = sorted(set(sidecar) - set(php))
    only_php = sorted(set(php) - set(sidecar))
    if only_side:
        check.fail(f"{len(only_side)} href(s) only on the sidecar: {only_side[:5]}")
    if only_php:
        check.fail(f"{len(only_php)} href(s) only on PHP: {only_php[:5]}")
    check.detail["hrefs_sidecar"] = len(sidecar)
    check.detail["hrefs_php"] = len(php)
    # ETag comparison on the intersection, normalising the weak-ETag deviation.
    differing = []
    for href in sorted(set(sidecar) & set(php)):
        a = strip_weak(sidecar[href]["etag"])
        b = strip_weak(php[href]["etag"])
        if a != b:
            differing.append({"href": href, "sidecar": a, "php": b})
    if differing:
        check.fail(f"{len(differing)} ETag(s) differ: {differing[:3]}")
    check.detail["etags_differing"] = len(differing)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", required=True, help="Public base URL, e.g. https://cloud.example")
    parser.add_argument("--sidecar-url", default=None, help="Override the sidecar base (e.g. a port-forward)")
    parser.add_argument("--php-url", default=None, help="Override the PHP base (defaults to --base)")
    parser.add_argument("--user", default=os.environ.get("NEXTCLOUD_DAV_USER"))
    parser.add_argument(
        "--password",
        default=os.environ.get("NEXTCLOUD_DAV_PASSWORD"),
        help="App password (or NEXTCLOUD_DAV_PASSWORD). Never logged.",
    )
    parser.add_argument("--book", default="contacts", help="Address-book URI (default: contacts)")
    parser.add_argument("--limit", type=int, default=25, help="Cards to GET individually (default: 25)")
    parser.add_argument("--json", default=None, help="Write the JSON report here ('-' for stdout)")
    parser.add_argument("--insecure", action="store_true", help="Do not verify TLS certificates")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args(argv)

    if not args.user or not args.password:
        parser.error("--user and --password (or the NEXTCLOUD_DAV_* env vars) are required")

    sidecar_base = (args.sidecar_url or args.base).rstrip("/")
    php_base = (args.php_url or args.base).rstrip("/")
    auth = "Basic " + base64.b64encode(f"{args.user}:{args.password}".encode()).decode()
    opener = _opener(args.insecure)

    collection_path = f"/remote.php/dav/addressbooks/users/{args.user}/{args.book}"
    checks: list[Check] = []
    report: dict = {
        "base": args.base,
        "sidecar_url": sidecar_base,
        "php_url": php_base,
        "book": args.book,
        "checks": [],
    }

    def log(msg: str) -> None:
        if args.verbose:
            print(msg, file=sys.stderr)

    # 1) PROPFIND Depth 1: hrefs + ETags (+ address-data hashes).
    check = Check("propfind-depth1")
    side = request(opener, join(sidecar_base, collection_path + "/"), "PROPFIND", auth, PROPFIND_PROPS.encode(), "1")
    php = request(opener, join(php_base, php_path(collection_path + "/")), "PROPFIND", auth, PROPFIND_PROPS.encode(), "1")
    check.detail["status_sidecar"] = side.status
    check.detail["status_php"] = php.status
    if side.status != 207 or php.status != 207:
        check.fail(f"expected 207 from both, got sidecar={side.status} php={php.status}")
    else:
        side_map = responses_by_href(parse_multistatus(side.body))
        php_map = responses_by_href(parse_multistatus(php.body))
        compare_href_sets(check, side_map, php_map)
        # Address-data bodies must be byte-identical (declared deviation:
        # only the ETag strength may differ).
        body_diffs = []
        for href in sorted(set(side_map) & set(php_map)):
            a = side_map[href]["address_data"]
            b = php_map[href]["address_data"]
            if a is not None and b is not None and a != b:
                body_diffs.append(href)
        if body_diffs:
            check.fail(f"{len(body_diffs)} address-data bodies differ: {body_diffs[:3]}")
        report["card_hrefs"] = sorted(h for h in side_map if h.endswith(".vcf"))
    checks.append(check)
    log(f"propfind-depth1: {check.notes or 'ok'}")

    # 2) GET a sample of cards: bytes, ETag, Last-Modified, Content-Type.
    check = Check("get-card-bodies")
    hrefs = report.get("card_hrefs", [])[: max(0, args.limit)]
    check.detail["sampled"] = len(hrefs)
    mismatches = []
    for href in hrefs:
        s = request(opener, join(sidecar_base, href), "GET", auth)
        p = request(opener, join(php_base, php_path(href)), "GET", auth)
        entry = {"href": href, "status_sidecar": s.status, "status_php": p.status}
        if s.status != p.status:
            mismatches.append({**entry, "why": "status"})
            continue
        if s.body != p.body:
            mismatches.append(
                {
                    **entry,
                    "why": "body",
                    "sha_sidecar": sha(s.body),
                    "sha_php": sha(p.body),
                    "len_sidecar": len(s.body),
                    "len_php": len(p.body),
                }
            )
            continue
        se = strip_weak(s.headers.get("ETag"))
        pe = strip_weak(p.headers.get("ETag"))
        if se != pe:
            mismatches.append({**entry, "why": "etag", "sidecar": se, "php": pe})
    if mismatches:
        check.fail(f"{len(mismatches)} GET mismatch(es)")
        check.detail["mismatches"] = mismatches[:10]
    checks.append(check)
    log(f"get-card-bodies: {len(hrefs)} sampled, {len(mismatches)} mismatches")

    # 3) addressbook-multiget over all hrefs.
    check = Check("addressbook-multiget")
    if hrefs:
        href_xml = "".join(f"<d:href>{h}</d:href>" for h in hrefs)
        body = MULTIGET_TEMPLATE.format(hrefs=href_xml).encode()
        s = request(opener, join(sidecar_base, collection_path + "/"), "REPORT", auth, body, "1")
        p = request(opener, join(php_base, php_path(collection_path + "/")), "REPORT", auth, body, "1")
        check.detail["status_sidecar"] = s.status
        check.detail["status_php"] = p.status
        if s.status != 207 or p.status != 207:
            check.fail(f"expected 207 from both, got sidecar={s.status} php={p.status}")
        else:
            sm = responses_by_href(parse_multistatus(s.body))
            pm = responses_by_href(parse_multistatus(p.body))
            compare_href_sets(check, sm, pm)
            diffs = [
                h
                for h in sorted(set(sm) & set(pm))
                if sm[h]["address_data"] is not None
                and pm[h]["address_data"] is not None
                and sm[h]["address_data"] != pm[h]["address_data"]
            ]
            if diffs:
                check.fail(f"{len(diffs)} multiget bodies differ: {diffs[:3]}")
    else:
        check.detail["skipped"] = "no cards found"
    checks.append(check)

    # 4) sync-collection initial sync.
    check = Check("sync-collection-initial")
    s = request(opener, join(sidecar_base, collection_path + "/"), "REPORT", auth, SYNC_BODY.encode(), "1")
    p = request(opener, join(php_base, php_path(collection_path + "/")), "REPORT", auth, SYNC_BODY.encode(), "1")
    check.detail["status_sidecar"] = s.status
    check.detail["status_php"] = p.status
    if s.status != 207 or p.status != 207:
        check.fail(f"expected 207 from both, got sidecar={s.status} php={p.status}")
    else:
        sd = parse_multistatus(s.body)
        pd = parse_multistatus(p.body)
        st, pt = sync_token(sd), sync_token(pd)
        check.detail["sync_token_sidecar"] = st
        check.detail["sync_token_php"] = pt
        if st != pt:
            check.fail(f"sync-token differs: sidecar={st} php={pt}")
        sm = responses_by_href(sd)
        pm = responses_by_href(pd)
        only_side = sorted(set(sm) - set(pm))
        only_php = sorted(set(pm) - set(sm))
        if only_side or only_php:
            check.fail(f"sync href sets differ (only sidecar={only_side[:3]}, only php={only_php[:3]})")
    checks.append(check)

    # 5) addressbook-query (FN contains "a").
    check = Check("addressbook-query")
    s = request(opener, join(sidecar_base, collection_path + "/"), "REPORT", auth, QUERY_BODY.encode(), "1")
    p = request(opener, join(php_base, php_path(collection_path + "/")), "REPORT", auth, QUERY_BODY.encode(), "1")
    check.detail["status_sidecar"] = s.status
    check.detail["status_php"] = p.status
    if s.status != 207 or p.status != 207:
        check.fail(f"expected 207 from both, got sidecar={s.status} php={p.status}")
    else:
        sm = responses_by_href(parse_multistatus(s.body))
        pm = responses_by_href(parse_multistatus(p.body))
        only_side = sorted(set(sm) - set(pm))
        only_php = sorted(set(pm) - set(sm))
        if only_side or only_php:
            check.fail(f"query href sets differ (only sidecar={only_side[:3]}, only php={only_php[:3]})")
        check.detail["matched_sidecar"] = len(sm)
        check.detail["matched_php"] = len(pm)
    checks.append(check)

    report["checks"] = [c.to_json() for c in checks]
    report["ok"] = all(c.ok for c in checks)

    if args.json:
        payload = json.dumps(report, indent=2, sort_keys=True)
        if args.json == "-":
            print(payload)
        else:
            with open(args.json, "w", encoding="utf-8") as fh:
                fh.write(payload + "\n")

    # Human summary.
    for check in checks:
        mark = "PASS" if check.ok else "FAIL"
        print(f"[{mark}] {check.name}")
        for note in check.notes:
            print(f"       {note}")
    print(f"\n{'OK' if report['ok'] else 'DIFFERENCES FOUND'}: "
          f"{sum(1 for c in checks if c.ok)}/{len(checks)} checks passed")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
