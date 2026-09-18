# `nextcloud-dav` test suite

Three layers, all runnable from this directory:

| layer | files | needs PostgreSQL? | needs a live Nextcloud? |
|---|---|---|---|
| Pure protocol/wire | `protocol_wire.rs`, `auth_gates.rs` | no | no |
| DB-backed read path | `db_read_path.rs`, `http_read.rs`, `files_read_path.rs`, `deviations.rs` | yes (auto-started) | no |
| Differential conformance | `conformance/conformance.py` | no | yes (sidecar + PHP) |

## Running

Everything uses the project's Nix toolchain (no global installs):

```sh
cd nextcloud-dav
nice -n 19 nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config \
    nixpkgs#cmake nixpkgs#openssl -c cargo test
```

The `nice -n 19` is mandatory on the dev machine: a bare `cargo` build makes the
box unusable.

### PostgreSQL

`db_read_path.rs`, `http_read.rs` and `deviations.rs` need a PostgreSQL server.
They start a **throwaway cluster automatically** with `initdb`/`pg_ctl` from
`$PATH`, create an isolated `ncdav_test_*` database per test, and drop it. To
include the binaries:

```sh
nice -n 19 nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config \
    nixpkgs#cmake nixpkgs#openssl nixpkgs#postgresql -c cargo test
```

If `initdb`/`pg_ctl` are not on `$PATH`, every DB-backed test prints
`SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH` and returns — the
suite still passes, but that layer did not run. **Do not read a green run as
"the DB layer passed" unless you saw no `SKIP` lines.**

Point the harness at an existing server instead with
`NEXTCLOUD_DAV_TEST_DATABASE_URL=postgres://user@host:5432/postgres`
(isolated `ncdav_test_*` databases are still created on it).

Run a single layer:

```sh
cargo test --test protocol_wire      # pure
cargo test --test auth_gates         # pure
cargo test --test db_read_path       # DB layer
cargo test --test http_read          # router / protocol
cargo test --test deviations         # declared deviations
```

## What is covered

### `protocol_wire.rs` — pure parity with Nextcloud/Sabre

- `readBlob()`: non-image `PHOTO:data:` stripping (including folded lines), the
  `PHOTO:data:`-at-start micro-optimisation, image data kept, size semantics.
- `HasPhotoPlugin` `has-photo` semantics.
- vCard unfolding / unescaping / group-prefix stripping, and UID extraction
  (the read path serves the stored `oc_cards.uid`; Nextcloud computes it on
  write via `getUID()`, which the sidecar does not do — see open questions in
  `docs/DEVIATIONS.md`).
- RFC 6352 §10.5 filters: existence, `is-not-defined`, text-match
  (contains/equals/starts-with/ends-with), the three collations, negation,
  `anyof`/`allof`, param filters, empty filter, rejected collation/match-type.
- Sync state machine: token parsing, initial sync, `init_<id>_<tok>` paging,
  incremental dedup (last operation wins), truncation.
- `{DAV:}multistatus` serialisation: namespaces, escaping, 200/404 propstats,
  the empty-418 propstat, response-level statuses, structured values.
- PROPFIND/REPORT body parsing, namespace resolution.
- Path parsing (`parse_path`), Sabre path-segment encoding, percent decoding,
  Basic-auth parsing, HTTP-date formatting.

### `auth_gates.rs` — app-password gates

`classify_token_state` for every gate: valid permanent/onetime, wipe (2),
unknown type, expiry, `password_invalid`, case-insensitive `login_name`,
empty uid, non-native user, disabled user, the 300 s `last_check` boundary.
Also `sha512(token||secret)` hashes, legacy empty-secret hash, brute-force
backoff arithmetic and subnet normalisation.

### `db_read_path.rs` — the SQL result shapes

Address books scoped by principal and ordered by id; `getCard` quoting the
stored md5 ETag; `readBlob` size recomputation only when something was filtered;
`cards`/`cards_by_uris` scoping; `contact_groups` from `oc_cards_properties`
`CATEGORIES`; `sync_initial_cards` paging; `sync_changes` range/order;
display name + `core/enabled`; the `version = 2` authtoken filter; brute-force
counting.

### `http_read.rs` — the router and the wire protocol

- PROPFIND Depth 0/1 on book and card, full property completeness
  (`getctag`, both `sync-token` forms, `supported-report-set`,
  `supported-address-data`, `supported-collation-set`, `owner`,
  `current-user-privilege-set`, `oc:groups`, `nc:owner-displayname`,
  `getetag`/`getcontentlength`/`getlastmodified`/`getcontenttype`/`address-data`/
  `has-photo`), trailing-slash collection hrefs, 404 propstats.
- Home Depth 1 listing (only DB books).
- GET/HEAD: raw body bytes, strong ETag, `Last-Modified`, `Content-Type`;
  GET ETag == PROPFIND ETag; non-image `PHOTO:data:` stripping on read.
- `addressbook-multiget`: hit + miss 404 propstat, href order.
- `addressbook-query`: FN/EMAIL/UID, param filter, `limit`.
- `sync-collection`: initial, `init_` paging with 507 truncation, incremental
  add/modify/delete, malformed token.
- Auth gates over HTTP: wrong password, login-name mismatch, disabled,
  `password_invalid`, v1 token / stale `last_check` / expired / non-native /
  temporary type → PHP fallback (502 with the unreachable test backend), wipe
  token, brute-force recording off.
- Access control: other users' and missing resources are 404, not 403.
- Collection writes (`MKCOL`/`PROPPATCH`/`MOVE`/`COPY`/`POST`, and PUT/DELETE on
  a collection) are 501; `?photo`/`?export` 501.
- OPTIONS discovery headers; unauthenticated 401.

### `write_path.rs` — native `PUT`/`DELETE`

The production router with native writes enabled and the outbox table present:

- Create → 201 + quoted ETag; the `oc_cards` row (carddata/etag/size/uid/
  lastmodified), `oc_addressbookchanges` operation 1 with the pre-increment
  token, `oc_addressbooks.synctoken` +1, and `oc_cards_properties` including
  `TYPE=PREF` → `preferred = 1` and the `mb_strcut(…, 254)` truncation on a
  multibyte value.
- Update → 204 + a changed ETag and operation 2.
- Delete → 204, properties purged, operation 3, and a delete outbox row whose
  `card_data` is the pre-delete (post-`readBlob`) snapshot.
- `If-Match` mismatch 412 / quoted or unquoted match 204; `If-None-Match: *`
  412 on an existing card and 201 on a missing one.
- Validation: duplicate UID 409 (with the conflicting href), missing UID 400,
  bad `VERSION` 415, oversized body 403, ISO-8859-1 → UTF-8 conversion.
- Atomicity: the outbox row is present on success and absent when the
  transaction fails (a forced mid-transaction error rolls back the card too).

### `files_read_path.rs` — the native files `PROPFIND`

The router and `src/files.rs` against a seeded `oc_filecache`/`oc_storages`:
Depth 0/1 shapes, the fixed `allprop`/`propname` list, `Prefer: return=minimal`,
the property gate returning 501, mount delegation in both directions, every
non-PROPFIND method returning 501, NFC/decomposed path resolution, a >5000-child
listing, the `oc:comments-unread` and quota bulk queries, and the
fast-path-only requirement.

### `deviations.rs` — the declared exceptions

Reads `deviations.toml`, asserts the TOML and the Rust list agree, and runs one
assertion per declared deviation (see below).

### `conformance/conformance.py` — live differential harness

Compares the sidecar and PHP for the same account, read-only:

1. `PROPFIND Depth 1` — href sets and ETags (weak ETags normalised).
2. `GET` a sample of cards — status, byte-exact body, ETag, Content-Type.
3. `addressbook-multiget` — hrefs and `address-data` bodies.
4. `sync-collection` initial — sync-token and href set.
5. `addressbook-query` — matched href set.

```sh
cd nextcloud-dav/tests/conformance
python3 conformance.py \
    --base https://cloud.example \
    --user alice --password "$APP_PASSWORD" \
    --book contacts --limit 25 --json report.json
```

Useful flags: `--sidecar-url` (e.g. a `kubectl port-forward` to the pod's
`7868`) and `--php-url` when the sidecar is reached directly; `--insecure`;
`--verbose`; `--json -` for stdout. Exit status is 0 only when every check
passes. It never writes and is safe against production read-only. PHP is
reached with the double-slash `users//` bypass documented in
`.pi/skills/nextcloud-dav-parity/SKILL.md`.

## Declared deviations

`docs/DEVIATIONS.md` explains each one; `tests/deviations.toml` is the
machine-readable list; `tests/deviations.rs` asserts each. Adding a deviation
requires a TOML entry **and** a matching `DECLARED_IDS` entry **and** an
assertion in `assert_deviation()`; otherwise `deviations_toml_ids_match` or
`every_declared_deviation_holds` fails.

Current `resolved` entries (formerly `likely-wrong`, now fixed and pinned by a
test): `max-resource-size-wrong`, `supported-address-data-missing-json`,
`supported-collation-element-name`, `sync-invalid-token-400`,
`query-depth0-on-collection`, `writes-501`.

Write-path deviations added with the native write support:
`no-event-dispatch`, `events-queued-not-dispatched`, `jcard-rejected`,
`vcard-2.1-rejected`, `effect-ownership-registry`.

## What is NOT covered

- **Live PHP parity in CI.** The differential harness needs a running Nextcloud
  + sidecar; it cannot run in this environment. It was validated against a
  local mock that emulates both backends (including a deliberate body
  difference), not against PHP.
- **PHP's own test suite.** We mined `apps/dav/tests/unit/CardDAV/*` and
  `3rdparty/sabre/dav` for expectations and fixtures, but did not run PHPUnit.
- **Shared / group / system address books**, `contactsinteraction`, `?photo`
  and `?export` internals — declared PHP-only.
- **Writes** — card `PUT`/`DELETE` are exercised in `write_path.rs`;
  collection management, `MOVE`/`COPY` and `?photo`/`?export` remain PHP-only
  (declared).
- **The PHP outbox worker** — the sidecar only queues events; draining the
  `oc_dav_event_outbox` queue is the companion app's job and is not tested here.
- **vCard version negotiation, conditional GET, `allprop` completeness** —
  declared deviations.
- **MySQL / SQLite.** The sidecar supports them via `sqlx::Any`; the tests only
  exercise PostgreSQL.
- **Brute-force throttling over HTTP** — the delay/block arithmetic is unit
  tested, but no test waits 0.1·2ⁿ seconds; the recording-off behaviour is
  asserted.
- **`current-user-privilege-set` exact contents, `OPTIONS` header set, home
  `resourcetype`** — see the open questions in `docs/DEVIATIONS.md`.

## Cases that could not be run here

- The differential harness against a real PHP backend (no local Nextcloud PHP
  instance). Run it against the deployed instance read-only.
- Anything requiring collection management (`MKCOL`, `MOVE`, `COPY`, …), which
  is still 501 and served by PHP.
- `max-resource-size` and `supported-address-data` against a deployed NC 33 to
  confirm the vendored Sabre version behaves like the NC 36 checkout.
