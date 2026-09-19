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

## WebDAV files `PROPFIND` (v1)

The sidecar also serves `PROPFIND` for `/remote.php/dav/files/<uid>/<path>`
(Depth 0/1) directly from `oc_filecache`. The scope and the delegation rules are
in `src/files.rs`; the recon is `recon/files-propfind-model.md`.

| id | area | status | one-line difference |
|---|---|---|---|
| `files-property-gate-501` | propfind | intentional | an explicit request for any unimplemented qname → 501, never 404 |
| `files-mount-delegation` | delegation | intentional | any mount at/under the path, or below a collection → 501 |
| `files-non-propfind-501` | delegation | intentional | every non-PROPFIND method (including OPTIONS) → 501 |
| `files-has-preview-static` | propfind | accepted | `nc:has-preview` uses a static mimetype list, not the live provider registry |
| `files-shareapi-exclude-groups-delegated` | delegation | intentional | `core/shareapi_exclude_groups` configured → 501 |
| `files-quota-disk-free-approximation` | propfind | accepted | finite quota without a readable datadirectory ignores disk free |
| `files-lock-props-delegated` | delegation | intentional | `nc:lock*` (only requested when `files_lock` is enabled) → 501 |
| `files-downloadurl-objectstore-delegated` | delegation | intentional | primary object store configured → `oc:downloadURL` → 501 |
| `files-is-encrypted-e2ee-delegated` | delegation | intentional | `end_to_end_encryption` enabled → `nc:is-encrypted` → 501 (else 404, like PHP) |
| `files-sharees-ldap-display-name` | propfind | intentional | `nc:sharees` display-name is joined from `oc_users`/`oc_groups`; an LDAP/circle sharee falls back to the id |
| `discovery-property-gate-501` | propfind | intentional | an explicit request for any unimplemented discovery qname → 501, never 404 |
| `discovery-own-principal-only` | delegation | intentional | another user's principal → 501 |
| `discovery-collection-listings-delegated` | delegation | intentional | `/principals/` and `/principals/users/` listings (and other principal children) → 501 |
| `discovery-non-propfind-501` | delegation | intentional | every non-PROPFIND discovery method (including OPTIONS) → 501 |
| `discovery-group-membership-backends` | propfind | intentional | `group-membership` expands database groups only; LDAP/circles invisible |
| `discovery-language-request-fallback` | propfind | intentional | `nc:language` delegates when no `force_language`/`core/lang` is set |

**The property gate** is the safety rule that makes the whole thing honest: an
explicit property list is only served when *every* requested qname is in the
implemented set. That set is the exact union of the web UI's and desktop
client's real requests:

- constants / joined columns: `d:getetag`, `d:getlastmodified`,
  `d:resourcetype`, `d:getcontentlength`, `d:getcontenttype`, `d:displayname`,
  `d:quota-available-bytes`, `d:quota-used-bytes`, `d:creationdate`
  (`oc_filecache_extended.creation_time`), `oc:size`, `oc:fileid`, `oc:id`,
  `oc:permissions`, `oc:owner-id`, `oc:owner-display-name`, `oc:favorite`,
  `oc:comments-unread`, `oc:checksums`, `oc:downloadURL` (empty for local
  storage; files only), `oc:data-fingerprint` (the `data-fingerprint` config),
  `nc:has-preview`, `nc:mount-type`, `nc:is-mount-root`, `nc:hidden`,
  `nc:metadata-<key>` (`oc_files_metadata.json`), `ocs:share-permissions`;
- one bulk `oc_share` query per collection: `oc:share-types`, `nc:sharees`;
- PHP 404s that are still served natively: `nc:is-encrypted` (no handler in
  Nextcloud 33/36, nor in the encryption app), `oc:dDC` (disallowed by
  `CustomPropertiesBackend`), `nc:note` / `nc:hide-download` (null for a
  non-shared storage).

Anything else — `oc:tags`, `nc:system-tags`, `nc:lock*`, `d:owner`, … —
delegates with 501, because a 404 would tell the client a property PHP serves
does not exist. `allprop` and `propname` use Sabre's fixed 7-property list
(`PropFind::ALLPROPS`), so they are always servable.

**Mounts.** A received share, a groupfolder or an external storage is an
`oc_mounts` row, not a child row in the home storage's `oc_filecache`. Serving a
listing from `oc_filecache` alone would silently drop it, and a directory's
`oc:size`/`getetag` include its submounts (`View::getFileInfo()`), so any path
that has a mount at, under, or (as a collection) below it is delegated.

**`nc:has-preview`.** `PreviewManager::isAvailable()` depends on the enabled
apps, the loaded imagick/ffmpeg/libreoffice binaries and third-party providers,
none of which a database reader can see. The static list matches the
always-registered core providers plus the common imagick/office/video formats.

**Quota.** `d:quota-used-bytes` is the directory's own raw size and
`d:quota-available-bytes` is `-3` for an unlimited quota, or
`min(disk_free, max(quota - used_root, 0))` for a finite one. `disk_free` is read
with `statvfs(datadirectory)`; when that is unavailable the sidecar reports
`max(quota - used_root, 0)`.

## DAV discovery `PROPFIND` (v1)

The sidecar also serves the two requests every client session performs before
it lists anything:

- `PROPFIND` **Depth 0** on the DAV root `/remote.php/dav/`;
- `PROPFIND` **Depth 0** on the caller's own principal
  `/remote.php/dav/principals/users/<uid>/`.

Everything else stays on PHP. The scope and delegation rules live in
`src/discovery.rs`; nginx routes only the two anchored paths
(`^/remote\.php/dav/$`, `^/remote\.php/dav/principals/users/[^/]+/?$`).

**The property gate.** As for files, an explicit property list is served only
when *every* qname is in the implemented set; anything else answers **501**
(delegated), never 404. The implemented sets are the exact live 200 responses
(captured 2026-09-19):

- **root**: `d:resourcetype`, `d:current-user-principal`,
  `d:principal-collection-set`, `d:supported-report-set`,
  `d:current-user-privilege-set`;
- **principal**: the root set plus `d:principal-URL`, `d:displayname`,
  `d:owner`, `d:alternate-URI-set`, `d:group-membership`,
  `card:addressbook-home-set`, `cal:calendar-home-set`,
  `cal:calendar-user-address-set`, `cal:calendar-user-type`, `nc:language`,
  `s:email-address`.

`allprop`/`propname` reproduce PHP's result for these nodes: only
`{DAV:}resourcetype`. The values come from `oc_users` (display name),
`oc_group_user`/`oc_groups` (group hrefs), `oc_preferences` (`settings/email`,
`core/lang`) and `oc_accounts.data` (`additional_mail`). The
`current-user-privilege-set` is the fixed Sabre ACL result for the root
(`{DAV:}authenticated` → `{DAV:}all`) and for a principal
(`{DAV:}owner` → `{DAV:}all`); it is verified user-independent.

**Own principal only.** The principal response mixes the *target* uid (href,
displayname, homes) with *caller-scoped* properties (`current-user-principal`,
`current-user-privilege-set`), so another user's principal answers 501 rather
than inventing the caller-scoped half.

**Delegated listings.** The `/principals/` and `/principals/users/` collection
listings (and group principals, calendar resources/rooms, calendar-proxy
children) answer 501. The nginx location is anchored to the own-principal path,
so these never reach the sidecar in production; the 501 is a safety net.

**Delegated OPTIONS.** Every non-`PROPFIND` method, OPTIONS included, answers
501 before authentication. PHP's `DAV:`/`Allow` headers are long
(`dav: 1, 3, extended-mkcol, access-control, …`), and the sidecar's OPTIONS
advertises addressbook capabilities, which is wrong here.

**Group membership.** `{DAV:}group-membership` is expanded from
`oc_group_user`/`oc_groups` only; LDAP/circle membership and
`hideFromCollaboration()` are not visible in the schema (the same limitation as
`shared-books-group-backends`).

**Language.** `nc:language` is served from `$CONFIG['force_language']` or the
user's `core/lang`. When neither is set, PHP falls back to the `forceLanguage`
request param, the request's `Accept-Language` and `default_language`, which the
sidecar does not reproduce, so the request is delegated.

**The fallback probe.** The sidecar's own authentication fallback is a
credentialed `PROPFIND /remote.php/dav/` through the public URL. Once the root
is served natively that would recurse, so the probe carries
`X-Nextcloud-Dav-Fallback: 1`; the discovery handler answers 501 for it and
nginx replays it to PHP. A client that sends the header is simply delegated.
