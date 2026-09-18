# Declared deviations from Nextcloud / SabreDAV

`nextcloud-dav` reimplements a subset of Nextcloud's CardDAV backend. This
document lists **every known behavioural divergence** from Nextcloud
(`apps/dav/lib/CardDAV/*`) and SabreDAV (`3rdparty/sabre/dav`).

The machine-readable copy is [`../tests/deviations.toml`](../tests/deviations.toml).
Every entry there is asserted by
`tests/deviations.rs::every_declared_deviation_holds`, and
`tests/deviations.rs::deviations_toml_ids_match` fails if the TOML and the Rust
list drift apart. **Changing the sidecar's behaviour without updating the
declaration fails the suite** — that is the point of the exercise.

Status vocabulary:

| status | meaning |
|---|---|
| `intentional` | a deliberate v1 scope decision |
| `accepted` | a divergence that was investigated and accepted |
| `temporary` | expected to change (write support is being added right now) |
| `likely-wrong` | the sidecar probably does not match Nextcloud; not fixed here, pinned by a test so a fix is explicit |

## Summary

| id | area | status | one-line difference |
|---|---|---|---|
| `weak-etag-get` | etag | accepted | PHP GET may be weak; sidecar always strong, matching PROPFIND |
| `photo-delegated` | delegation | accepted | `?photo` → 501 → PHP |
| `export-delegated` | delegation | accepted | `?export` → 501 → PHP |
| `home-listing-php` | routing | accepted | home listing (and app-generated books) served by PHP |
| `writes-501` | writes | temporary | PUT/DELETE/MKCOL/PROPPATCH/MOVE/COPY/POST → 501 → PHP |
| `shared-books-php` | routing | resolved | owned + user/group-shared `oc_dav_shares` books are served with the sharing properties |
| `shared-unshare-tombstone-semantics` | routing | intentional | tombstones exclude by `resourceid` (CalDAV semantics); PHP CardDAV uses `s.id` and never hides a surviving group share |
| `shared-write-actor` | writes | intentional | the outbox has no actor column, so a shared write is attributed to the owner |
| `shared-books-group-backends` | routing | intentional | group expansion is database-only; LDAP/circles and `hideFromCollaboration()` are invisible |
| `shared-books-listing-order` | routing | intentional | owned books first, then shared rows ordered by id |
| `contactsinteraction-php` | routing | accepted | `z-app-generated--contactsinteraction--recent` is PHP-only |
| `bruteforce-recording-off` | auth | intentional | failed logins are not recorded by default |
| `no-event-dispatch` | writes | intentional | no CardCreated/Updated/DeletedEvent, so no search/activity/notification side effects |
| `allprop-curated` | propfind | intentional | `allprop`/`propname` return a curated property set |
| `vcard-version-negotiation-missing` | report | intentional | `address-data` returned as stored; version/prop filters ignored |
| `conditional-get-missing` | get | intentional | `If-None-Match`/`If-Modified-Since` not evaluated |
| `max-resource-size-wrong` | propfind | **likely-wrong** | sidecar 5242880 vs Sabre 10000000 |
| `supported-address-data-missing-json` | propfind | **likely-wrong** | sidecar omits `application/vcard+json` |
| `supported-collation-element-name` | propfind | **likely-wrong** | sidecar emits `<card:collation>`, Sabre emits `<card:supported-collation>` |
| `sync-invalid-token-400` | sync | **likely-wrong** | sidecar 400 vs Sabre 403 (`InvalidSyncToken extends Forbidden`) |
| `groups-sorted` | propfind | intentional | `oc:groups` sorted by value; PHP uses database order |
| `query-depth0-on-collection` | report | **likely-wrong** | sidecar 207/empty vs Sabre 415 `ReportNotSupported` |
| `authtoken-v2-only` | auth | intentional | only `version = 2` tokens use the fast path; others fall back to PHP |
| `error-body-501` | writes | accepted | 501 body is sidecar-specific text (nginx intercepts it) |

## Details

### ETag

**`weak-etag-get`** — For some cards, PHP's `GET` returns a weak ETag
(`W/"…"`) while its own PROPFIND returns the strong `"…"`; the sidecar always
returns the strong form. Bodies are byte-identical. This was investigated and
accepted (`../dav-bench/CARDDAV_LIVE.md`); the mechanism is not in the app
source. The differential harness normalises this by stripping `W/`.

### Delegation to PHP

**`photo-delegated` / `export-delegated`** — `?photo` needs appdata files and
image processing; `?export` re-serialises the collection. Both answer `501`
and are replayed to PHP by nginx (`error_page 501`).

### Routing

**`home-listing-php`** — nginx keeps
`/remote.php/dav/addressbooks/users/<u>` on PHP because PHP also advertises the
app-generated collections. The sidecar's own `Home` handler (only reachable if
the route is misconfigured) lists just `oc_addressbooks`.

**`shared-books-php` / `contactsinteraction-php`** — the sidecar now serves
owned, user-shared and database-group-shared `oc_dav_shares` books through a
single `visible_books()` path, with the owner's sharing properties and the
read-only write ACL. The system book and the `contactsinteraction` book stay on
PHP because they are not backed by `oc_addressbooks`/`oc_cards` rows. The three
shared-book limitations (tombstone semantics, group backends, write actor) are
listed above.

### Writes

**`writes-501`** — `PUT`, `DELETE`, `MKCOL`, `PROPPATCH`, `MOVE`, `COPY`,
`POST` return `501` on purpose so nginx can hand them to PHP. **This is being
changed concurrently**: write support is under implementation. The deviation is
declared `temporary`; the test asserts the current `501` and will fail the
moment a method starts working, forcing this entry to be updated.

**`no-event-dispatch`** — `CardDavBackend::createCard/updateCard/deleteCard`
dispatch `CardCreatedEvent`/`CardUpdatedEvent`/`CardDeletedEvent`, which drive
the search index, activity and notifications. The sidecar never writes, so it
never dispatches; writes continue to go through PHP, so events still fire once.
The test asserts a `PUT` leaves both `oc_cards` and `oc_addressbookchanges`
untouched.

**`error-body-501`** — the `501` body is a sidecar-specific plain-text message.
nginx intercepts it, so a real client never sees it.

### Properties / reports

**`allprop-curated`** — `allprop` and `propname` return the curated per-node
set in `src/routes.rs::default_props`, not every live property.

**`vcard-version-negotiation-missing`** — `address-data` is returned exactly as
stored. `content-type`, `version` and the child `<card:prop>` filter are parsed
but ignored; a client asking for vCard 4.0 gets the stored (usually 3.0) bytes.

**`max-resource-size-wrong` (likely bug)** — Sabre's CardDAV plugin sets
`maxResourceSize = 10000000` and Nextcloud does not override the *property*
(the `5242880` value is Nextcloud's separate `card_size_limit` write
validation). The sidecar advertises `5242880`. Evidence:
`src/config.rs:MAX_RESOURCE_SIZE`,
`3rdparty/sabre/dav/lib/CardDAV/Plugin.php:58`,
`apps/dav/lib/CardDAV/Validation/CardDavValidatePlugin.php:34`.

**`supported-address-data-missing-json` (likely bug)** — Sabre advertises
`text/vcard 3.0`, `text/vcard 4.0` **and** `application/vcard+json 4.0`
(`3rdparty/sabre/dav/lib/CardDAV/Xml/Property/SupportedAddressData.php:39`).
The sidecar advertises only the first two.

**`supported-collation-element-name` (likely bug)** — Sabre serialises the
collations as `<card:supported-collation>`
(`3rdparty/sabre/dav/lib/CardDAV/Xml/Property/SupportedCollationSet.php:42`);
the sidecar emits `<card:collation>` (`src/routes.rs`, `supported-collation-set`).

**`groups-sorted`** — `collectCardProperties()` is a `SELECT DISTINCT value`
with no `ORDER BY`; the sidecar adds `ORDER BY value`. Same set, deterministic
order.

**`query-depth0-on-collection` (likely bug)** — Sabre raises
`ReportNotSupported` (HTTP 415) when an `addressbook-query` at Depth 0 targets
a collection (`3rdparty/sabre/dav/lib/CardDAV/Plugin.php:402`); the sidecar
returns `207` with zero responses.

### Sync

**`sync-invalid-token-400` (likely bug)** — a token that does not start with
`http://sabre.io/ns/sync/` raises `InvalidSyncToken`, which extends `Forbidden`
⇒ **403** with a `<d:valid-sync-token/>` precondition body
(`3rdparty/sabre/dav/lib/DAV/Sync/Plugin.php:116`,
`.../Exception/InvalidSyncToken.php`). The sidecar returns **400** with a
plain-text body (`src/sync.rs::parse_sync_token`). Note the task brief said
"malformed token → 400"; the Sabre source says 403. The test pins the current
400.

### Auth

**`bruteforce-recording-off`** — `nextcloud_dav.record_bruteforce_attempts`
defaults to `false` so the shipped sidecar performs no writes at all. The
delay/block *checks* always run. The PHP fallback records failures as before.

**`authtoken-v2-only`** — the fast path filters `version = 2`
(`PublicKeyToken::VERSION`); any other version is delegated to PHP. A mismatch
can only cause a fallback, never an acceptance.

## Open questions (not asserted, deliberately)

These could not be determined with certainty here and are **not** encoded as
deviations. Each is a candidate for a live differential check.

1. **Exact `{DAV:}current-user-privilege-set`.** The sidecar hardcodes a
   privilege list (`src/routes.rs::privilege_set`). Sabre derives it from ACLs
   (`AddressBook::getACL`, `DavAclPlugin`). The *sets* may differ for shared
   books or read-only shares. Needs a live PHP comparison.
2. **`OPTIONS` `Allow`/`DAV` headers.** The sidecar's `Allow` includes
   `MKCOL` but omits `COPY`/`MOVE`/`POST`, while it answers all of them `501`.
   Sabre's advertised `Allow` was not compared live.
3. **`{DAV:}resourcetype` of the home.** The sidecar emits a bare
   `d:collection`; PHP's `AddressBookHome` may add
   `{carddav}addressbook-home` or similar. Needs a live check.
4. **`Last-Modified` formatting.** The sidecar uses RFC 1123
   (`src/util.rs::http_date`); Sabre serialises the int via its HTTP layer.
   Expected to match, not verified against a live server.
5. **`max-resource-size` on a deployed NC 33.** The `10000000` value was read
   from the Sabre copy in the NC 36 checkout; a deployed NC 33 may vendor a
   different Sabre version.
6. **`README.md` auth section is stale.** It says the fast path filters
   `version = 1`; the code (`src/db.rs`) and `ARCHITECTURE.md` use `version = 2`.
   The README text is wrong, the code is right.
7. **`oc:owner-principal` for shared books.** Not applicable because shared
   books are PHP-only, but if they are ever served the property must be added.
8. **UID extraction on write.** Nextcloud computes `oc_cards.uid` in
   `CardDavBackend::getUID()` (and enforces the no-uid-conflict precondition)
   on create/update. The sidecar is read-only and serves the stored `uid`
   column; its vCard parser can extract a UID but nothing writes it. A future
   write path must reproduce `getUID()` (including the 400 for a missing UID).
   Exercised at the parser level only
   (`tests/protocol_wire.rs::vcard_uid_is_extractable`).

## How the guarantee works

- `tests/deviations.toml` is the machine-readable declaration.
- `tests/deviations.rs::deviations_toml_ids_match` asserts the TOML ids and the
  Rust `DECLARED_IDS` list are identical.
- `tests/deviations.rs::every_declared_deviation_holds` runs one assertion per
  id and reports every failure, so a silent behaviour change fails the suite and
  points at the exact declaration that needs revisiting.
