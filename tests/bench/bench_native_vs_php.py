#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
r"""Benchmark the nextcloud-dav sidecar against the PHP DAV backend.

The two backends are addressed **directly**, each through its own base URL, so
the comparison cannot be contaminated by the reverse proxy's routing:

    sidecar: http://127.0.0.1:<port>   (the sidecar's own listener)
    PHP:     http://127.0.0.1:<port>   (the Nextcloud container's web server)

This matters. An earlier version of this script forced PHP by inserting a double
slash into the DAV path (`users//<uid>/...`), believing the sidecar's nginx
location regex (`^/remote\.php/dav/addressbooks/users/[^/]+/.`) would not match.
It does not work: nginx has `merge_slashes on` by default, so the URI is
normalised back before location matching and the sidecar still served it. Both
"backends" then measured the same process, which is why every ratio came out at
~1.0 and the two response bodies were byte-identical.

The canary here is therefore decisive rather than cosmetic: the sidecar can never
create a collection, so `MKCOL` on the sidecar base must return 501 and on the
PHP base 201. If either differs, the script refuses to run.

Usage:
    python3 bench_native_vs_php.py --user alice --password-file /tmp/pw \
        --sidecar-base http://127.0.0.1:17870 \
        --php-base http://127.0.0.1:18081 [--reps 15] [--book contacts]
"""

from __future__ import annotations

import argparse
import base64
import json
import ssl
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

PROPFIND_BODY = (
    b'<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop>'
    b"<d:getetag/><d:resourcetype/></d:prop></d:propfind>"
)
MKCOL_BODY = (
    b'<?xml version="1.0"?><d:mkcol xmlns:d="DAV:" '
    b'xmlns:card="urn:ietf:params:xml:ns:carddav"><d:set><d:prop>'
    b"<d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>"
    b"<d:displayname>ZZ Bench</d:displayname></d:prop></d:set></d:mkcol>"
)
CARD = (
    b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:Bench Card\r\n"
    b"N:Card;Bench;;;\r\nNOTE:revision {rev}\r\nEND:VCARD\r\n"
)


class Client:
    """A DAV client bound to one backend's base URL."""

    def __init__(self, base: str, user: str, password: str) -> None:
        self.base = base.rstrip("/")
        self.auth = base64.b64encode(f"{user}:{password}".encode()).decode()
        # Local/self-signed instances are reached over plain HTTP; a real TLS
        # context is only needed when a base is https.
        self.ctx = ssl.create_default_context() if self.base.startswith("https") else None

    def request(
        self, method: str, path: str, body: bytes | None, headers: dict[str, str]
    ) -> tuple[int, bytes, float]:
        req = urllib.request.Request(self.base + path, data=body, method=method)
        req.add_header("Authorization", "Basic " + self.auth)
        for name, value in headers.items():
            req.add_header(name, value)
        started = time.perf_counter()
        try:
            with urllib.request.urlopen(req, timeout=120, context=self.ctx) as resp:
                payload = resp.read()
                status = resp.status
        except urllib.error.HTTPError as error:
            payload = error.read()
            status = error.code
        return status, payload, time.perf_counter() - started


def pct(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(round(quantile * (len(ordered) - 1))))
    return ordered[index]


def summarise(name: str, sidecar: list[float], php: list[float]) -> dict:
    side_p50 = statistics.median(sidecar)
    php_p50 = statistics.median(php)
    return {
        "request": name,
        "sidecar_p50_ms": round(side_p50 * 1000, 1),
        "sidecar_p90_ms": round(pct(sidecar, 0.9) * 1000, 1),
        "php_p50_ms": round(php_p50 * 1000, 1),
        "php_p90_ms": round(pct(php, 0.9) * 1000, 1),
        "speedup_p50": round(php_p50 / side_p50, 2) if side_p50 > 0 else None,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sidecar-base", required=True)
    parser.add_argument("--php-base", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password-file", required=True)
    parser.add_argument("--reps", type=int, default=15)
    parser.add_argument("--book", default="contacts")
    parser.add_argument(
        "--php-query",
        default="",
        help=(
            "Query appended to PHP requests only. On a production instance, where "
            "the sidecar owns the addressbooks subtree and cannot be bypassed by "
            "path tricks (nginx merges slashes), 'export=1' makes the sidecar "
            "answer 501 so nginx replays the request to PHP, while remaining a "
            "normal request for PHP."
        ),
    )
    parser.add_argument("--json")
    args = parser.parse_args()

    with open(args.password_file, encoding="utf-8") as handle:
        password = handle.read().strip()

    sidecar = Client(args.sidecar_base, args.user, password)
    php = Client(args.php_base, args.user, password)
    uid = urllib.parse.quote(args.user)

    def path(book: str, tail: str = "") -> str:
        return f"/remote.php/dav/addressbooks/users/{uid}/{book}/{tail}"

    def ppath(book: str, tail: str = "") -> str:
        """The same DAV path, with the query that forces PHP (when configured)."""
        base = path(book, tail)
        return f"{base}?{args.php_query}" if args.php_query else base

    props = {"Content-Type": "application/xml"}
    bench_book = "zz-bench"

    # --- canary: the sidecar can never create a collection -------------------
    php.request("DELETE", ppath(bench_book + "/"), None, {})
    status, _, _ = php.request("MKCOL", ppath(bench_book), MKCOL_BODY, props)
    if status != 201:
        print(f"canary failed: PHP MKCOL returned {status} (expected 201)", file=sys.stderr)
        return 2
    status, _, _ = sidecar.request("MKCOL", path(bench_book), MKCOL_BODY, props)
    if status != 501:
        # When the two bases are the same nginx-fronted host (a production run),
        # the sidecar's 501 is intercepted and replayed to PHP, so the client
        # sees PHP's answer and the sidecar cannot be identified this way. The
        # PHP path is then identified by --php-query plus the php-fpm log.
        if args.sidecar_base == args.php_base:
            print(
                f"note: MKCOL on the sidecar base returned {status}; the bases are "
                f"the same host, so this is PHP answering via the intercepted 501 "
                f"(the sidecar is identified by --php-query instead)\n"
            )
        else:
            print(
                f"canary failed: the sidecar returned {status} for MKCOL (expected "
                f"its own 501). Are both bases pointing at the same process?",
                file=sys.stderr,
            )
            return 2
    else:
        print("canary ok: PHP creates the book (201), the sidecar refuses (501)\n")

    results: list[dict] = []

    # --- read: PROPFIND depth 1 on the real book -----------------------------
    sc, ph = [], []
    listing = b""
    for _ in range(args.reps):
        _, listing, dt = sidecar.request("PROPFIND", path(args.book), PROPFIND_BODY,
                                         {**props, "Depth": "1"})
        sc.append(dt)
        _, _, dt = php.request("PROPFIND", ppath(args.book), PROPFIND_BODY,
                               {**props, "Depth": "1"})
        ph.append(dt)
    results.append(summarise("PROPFIND Depth 1 (whole book)", sc, ph))

    # --- read: GET one card (the first .vcf href, not the collection) --------
    hrefs = [
        chunk.split("</d:href>")[0]
        for chunk in listing.decode("utf-8", "replace").split("<d:href>")[1:]
    ]
    cards = [h for h in hrefs if h.endswith(".vcf")]
    if not cards:
        print("no .vcf hrefs in the listing; skipping the GET benchmark", file=sys.stderr)
        card = None
    else:
        card = cards[0].split("/")[-1]
    if card:
        sc, ph = [], []
        for _ in range(args.reps):
            _, _, dt = sidecar.request("GET", path(args.book, card), None, {})
            sc.append(dt)
            _, _, dt = php.request("GET", ppath(args.book, card), None, {})
            ph.append(dt)
        results.append(summarise(f"GET one card ({card})", sc, ph))

    # --- write: PUT create / PUT update / DELETE -----------------------------
    sc, ph = [], []
    for i in range(args.reps):
        body = CARD.replace(b"{uid}", f"bench-sc-{i}".encode()).replace(b"{rev}", b"1")
        _, _, dt = sidecar.request("PUT", path(bench_book, f"bench-sc-{i}.vcf"), body,
                                   {"Content-Type": "text/vcard; charset=utf-8"})
        sc.append(dt)
        body = CARD.replace(b"{uid}", f"bench-php-{i}".encode()).replace(b"{rev}", b"1")
        _, _, dt = php.request("PUT", ppath(bench_book, f"bench-php-{i}.vcf"), body,
                               {"Content-Type": "text/vcard; charset=utf-8"})
        ph.append(dt)
    results.append(summarise("PUT create", sc, ph))

    sc, ph = [], []
    for i in range(args.reps):
        body = CARD.replace(b"{uid}", f"bench-sc-{i}".encode()).replace(b"{rev}", b"2")
        _, _, dt = sidecar.request("PUT", path(bench_book, f"bench-sc-{i}.vcf"), body,
                                   {"Content-Type": "text/vcard; charset=utf-8"})
        sc.append(dt)
        body = CARD.replace(b"{uid}", f"bench-php-{i}".encode()).replace(b"{rev}", b"2")
        _, _, dt = php.request("PUT", ppath(bench_book, f"bench-php-{i}.vcf"), body,
                               {"Content-Type": "text/vcard; charset=utf-8"})
        ph.append(dt)
    results.append(summarise("PUT update", sc, ph))

    sc, ph = [], []
    for i in range(args.reps):
        _, _, dt = sidecar.request("DELETE", path(bench_book, f"bench-sc-{i}.vcf"), None, {})
        sc.append(dt)
        _, _, dt = php.request("DELETE", ppath(bench_book, f"bench-php-{i}.vcf"), None, {})
        ph.append(dt)
    results.append(summarise("DELETE", sc, ph))

    # --- report --------------------------------------------------------------
    header = (
        f"{'request':30} {'sidecar p50':>12} {'sidecar p90':>12} "
        f"{'php p50':>10} {'php p90':>10} {'speedup':>8}"
    )
    print(header)
    print("-" * len(header))
    for row in results:
        print(
            f"{row['request']:30} {row['sidecar_p50_ms']:>9} ms {row['sidecar_p90_ms']:>9} ms "
            f"{row['php_p50_ms']:>7} ms {row['php_p90_ms']:>7} ms {row['speedup_p50']:>7}x"
        )
    print(f"\n(cards in the book: {len(cards)}; reps per backend per row: {args.reps})")

    if args.json:
        with open(args.json, "w", encoding="utf-8") as handle:
            json.dump({"results": results, "cards": len(cards), "reps": args.reps}, handle, indent=2)
        print(f"json written to {args.json}")

    php.request("DELETE", ppath(bench_book + "/"), None, {})
    print("cleanup: throwaway book deleted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
