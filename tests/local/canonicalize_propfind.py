#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Canonicalise a WebDAV files PROPFIND multistatus for diffing.

Reads one XML file (or `-` for stdin) and prints one sorted, canonical line per
`<d:response>`. Whitespace, property order, propstat order and XML namespace
prefixes are normalised away; the href, each propstat status and each property
value are preserved.

Usage: canonicalize_propfind.py FILE
"""

import sys
import xml.etree.ElementTree as ET

DAV = "{DAV:}"


def canonical_value(element) -> str:
    """A stable string for an XML element, ignoring attribute order."""
    attributes = tuple(sorted((key, value) for key, value in element.attrib.items()))
    children = tuple(
        sorted((child.tag, canonical_value(child)) for child in element)
    )
    text = (element.text or "").strip()
    return f"({element.tag}{attributes}{text!r}{children})"


def canonical_response(response) -> str:
    href = ""
    propstats = []
    for child in response:
        if child.tag == DAV + "href":
            href = (child.text or "").strip()
        elif child.tag == DAV + "propstat":
            status = ""
            props = []
            for propstat_child in child:
                if propstat_child.tag == DAV + "status":
                    status = (propstat_child.text or "").strip()
                elif propstat_child.tag == DAV + "prop":
                    for prop in propstat_child:
                        props.append((prop.tag, canonical_value(prop)))
            propstats.append((status, tuple(sorted(props))))
        elif child.tag == DAV + "status":
            propstats.append(((child.text or "").strip(), ()))
    return f"{href}\t{tuple(sorted(propstats))}"


def main() -> int:
    source = sys.argv[1]
    tree = ET.parse(sys.stdin if source == "-" else source)
    lines = []
    response_count = 0
    for child in tree.getroot():
        if child.tag == DAV + "response":
            lines.append(canonical_response(child))
            response_count += 1
        elif child.tag == DAV + "sync-token":
            # The `sync-collection` reply carries the new token at the
            # multistatus level; keep it in the canonical output so a REPORT
            # diff does not silently ignore it.
            lines.append(f"SYNC-TOKEN\t{(child.text or '').strip()}")
    for line in sorted(lines):
        print(line)
    print(f"# responses={response_count}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())