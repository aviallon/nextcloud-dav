# Declared deviations from Nextcloud / SabreDAV

`nextcloud-dav` reimplements a subset of Nextcloud's DAV backends (CardDAV,
CalDAV, WebDAV file listings and discovery). This document lists **every known
behavioural divergence** from Nextcloud (`apps/dav/lib/*`) and SabreDAV
(`3rdparty/sabre/dav`).

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
| `temporary` | expected to change |
| `likely-wrong` | the sidecar probably does not match Nextcloud; not fixed here, pinned by a test so a fix is explicit |

## Summary

| id | area | status | one-line difference |
|---|---|---|---|
| `weak-etag-get` | etag | accepted | PHP GET may be weak; sidecar always strong, matching PROPFIND |
| `unauthenticated-delegates` | auth | intentional | a request with no Basic header is delegated, never refused with 401 (the web UI authenticates DAV by session cookie, which the sidecar cannot evaluate) |
| `photo-delegated` | delegation | accepted | `?photo` → 501 → PHP |
| `export-delegated` | delegation | accepted | `?export` → 501 → PHP |
| `home-listing-php` | routing | accepted | home listing (and app-generated books) served by PHP |
| `writes-501` | writes | resolved | card `PUT`/`DELETE` are native; `MKCOL`/`PROPPATCH`/`MOVE`/`COPY`/`POST` and collection writes → 501 → PHP |
| `shared-books-php` | routing | resolved | owned + user/group-shared `oc_dav_shares` books are served with the sharing properties |
| `shared-unshare-tombstone-semantics` | routing | intentional | tombstones exclude by `resourceid` (CalDAV semantics); PHP CardDAV uses `s.id` and never hides a surviving group share |
| `shared-write-actor` | writes | intentional | the outbox has no actor column, so a shared write is attributed to the owner |
| `shared-books-group-backends` | routing | intentional | group expansion is database-only; LDAP/circles and `hideFromCollaboration()` are invisible |
| `shared-books-listing-order` | routing | intentional | owned books first, then shared rows ordered by id |
| `contactsinteraction-php` | routing | accepted | `z-app-generated--contactsinteraction--recent` is PHP-only |
| `bruteforce-recording-off` | auth | intentional | failed logins are not recorded by default |
| `no-event-dispatch` | writes | intentional | a native write queues the event in `oc_dav_event_outbox`; no listener runs in the sidecar |
| `events-queued-not-dispatched` | writes | intentional | the request returns after one committed outbox row + `pg_notify`; every effect runs in the companion PHP worker |
| `effect-ownership-registry` | writes | intentional | every effect id is claimed by exactly one backend (all `php` in phase 1), frozen into each outbox row |
| `jcard-rejected` | writes | intentional | jCard (`[`-prefixed) `PUT` bodies → `415` instead of being converted to vCard |
| `vcard-2.1-rejected` | writes | intentional | a `VERSION` other than 3.0/4.0 (incl. 2.1) → `415` |
| `allprop-curated` | propfind | intentional | `allprop`/`propname` return a curated property set |
| `vcard-version-negotiation-missing` | report | resolved | `address-data` negotiates version/content-type and applies the `<card:prop>` filter (in **both** reports, case-insensitively — more standards-conformant than Sabre) |
| `conditional-get-missing` | get | resolved | conditional `GET` is implemented (`304`) per RFC 7232, with three deliberate corrections to Sabre's evaluation (`HEAD` → `304`, weak etag comparison, `304` always carries `ETag`) |
| `max-resource-size-wrong` | propfind | resolved | returns `10000000` like Sabre (the `5242880` write limit is enforced separately) |
| `supported-address-data-missing-json` | propfind | resolved | advertises all three types incl. `application/vcard+json` |
| `supported-collation-element-name` | propfind | resolved | emits `<card:supported-collation>`, matching Sabre |
| `sync-invalid-token-400` | sync | resolved | returns `403` + `<d:valid-sync-token/>` like Sabre (`InvalidSyncToken extends Forbidden`) |
| `groups-sorted` | propfind | intentional | `oc:groups` sorted by value; PHP uses database order |
| `query-depth0-on-collection` | report | resolved | returns `415` with a `<d:supported-report/>` body, like Sabre's `ReportNotSupported` |
| `query-report-order` | routing | intentional | `addressbook-query` responses come out in `oc_cards.id` order; PHP has no `ORDER BY` (observed: URI order) |
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

**`writes-501`** — card `PUT`/`DELETE` are native (`201`/`204`: the card row,
change row, sync-token bump, search columns and one outbox row in a single
transaction). `MKCOL`, `PROPPATCH`, `MOVE`, `COPY`, `POST`, and `PUT`/`DELETE`
on a collection, still return `501` on purpose so nginx can hand them to PHP —
as do card writes while native writes are unavailable (`event_dispatch.enabled
= false` or a missing outbox table).

**`no-event-dispatch` / `events-queued-not-dispatched`** —
`CardDavBackend::createCard/updateCard/deleteCard` dispatch
`CardCreatedEvent`/`CardUpdatedEvent`/`CardDeletedEvent` synchronously, and the
listeners (search index, activity, birthday calendar, photo cache, push) run
before the request returns. The sidecar instead commits one
`oc_dav_event_outbox` row (state = 0) and a transactional `pg_notify` with the
card; no listener runs in the sidecar process and the companion PHP worker
(`occ dav:event-dispatch`) drains the queue. A failing listener can therefore
no longer roll back the `PUT`, and every effect still runs exactly once.

**`effect-ownership-registry`** — every effect id (activity, birthday, photo
cache, notifications, Redis `DEL`, …) is claimed by exactly one backend in
`src/outbox.rs::EffectRegistry`; phase 1 is all `php`, frozen into each outbox
row, so an effect can later move to Rust without double dispatch.

**`jcard-rejected` / `vcard-2.1-rejected`** — Sabre parses a `[`-prefixed body
as jCard (RFC 7095) and re-serialises it to vCard, and vobject's REPAIR layer
can upgrade vCard 2.1. The sidecar does neither: jCard and any `VERSION` other
than 3.0/4.0 are rejected with `415`, so the stored bytes stay exactly what was
uploaded.

**`error-body-501`** — the `501` body is a sidecar-specific plain-text message.
nginx intercepts it, so a real client never sees it.

### Properties / reports

**`allprop-curated`** — `allprop` and `propname` return the curated per-node
set in `src/routes.rs::default_props`, not every live property.

**`vcard-version-negotiation-missing` (resolved)** — `address-data` in a
REPORT is now produced by `src/vobject.rs`, a port of VObject 4.5.6's
MimeDir reader/writer, `VCardConverter` and jCard output (the behavioural
contract with source citations is
[`../research/address-data-negotiation-spec.md`](../research/address-data-negotiation-spec.md)).
Same-version requests without a filter stay **byte-verbatim**; `version=` /
`content-type=` negotiate to vCard 3, vCard 4 or jCard
(`application/vcard+json`) and convert through the converter's exact rules
(3.0↔4.0, the Apple `X-ABDATE`/`X-APPLE-OMIT-YEAR` anniversary handling, the
`PRODID:-//Sabre//Sabre VObject 4.5.6//EN` rewrite); a `<card:prop>` filter
re-serialises with `UID`/`VERSION`/`FN` always kept, `VERSION` hoisted first
and vobject's fold/escape normalisation. Unparseable stored cards answer 500
with `s:exception` = `Sabre\VObject\ParseException`, like PHP. Three
deliberate, more-standards-conformant divergences from Sabre are pinned by the
tests: the filter is applied in **both** reports (Sabre's multiget call site
forgets it), filter names match case-insensitively (RFC 6350 names are
case-insensitive; Sabre's `array_diff` is not), and a malformed `content-type`
attribute degrades to the vCard 3 target instead of `var_dump()`-ing and
exiting the PHP process.

**`max-resource-size-wrong` (resolved)** — Sabre's CardDAV plugin sets
`maxResourceSize = 10000000` and Nextcloud does not override the *property*
(the `5242880` value is Nextcloud's separate `card_size_limit` write
validation). The sidecar used to advertise `5242880`; it now returns `10000000`
and enforces the write limit separately on `PUT`.

**`supported-address-data-missing-json` (resolved)** — Sabre advertises
`text/vcard 3.0`, `text/vcard 4.0` **and** `application/vcard+json 4.0`
(`3rdparty/sabre/dav/lib/CardDAV/Xml/Property/SupportedAddressData.php:39`).
The sidecar used to omit the third and now advertises all three.

**`supported-collation-element-name` (resolved)** — Sabre serialises the
collations as `<card:supported-collation>`
(`3rdparty/sabre/dav/lib/CardDAV/Xml/Property/SupportedCollationSet.php:42`);
the sidecar used to emit `<card:collation>` and now matches Sabre.

**`groups-sorted`** — `collectCardProperties()` is a `SELECT DISTINCT value`
with no `ORDER BY`; the sidecar adds `ORDER BY value`. Same set, deterministic
order.

**`query-depth0-on-collection` (resolved)** — Sabre raises
`ReportNotSupported` (HTTP 415) when an `addressbook-query` at Depth 0 targets
a collection (`3rdparty/sabre/dav/lib/CardDAV/Plugin.php:402`); the sidecar
used to return `207` with zero responses and now returns `415` with a
`<d:supported-report/>` body.

### Sync

**`sync-invalid-token-400` (resolved)** — a token that does not start with
`http://sabre.io/ns/sync/` raises `InvalidSyncToken`, which extends `Forbidden`
⇒ **403** with a `<d:valid-sync-token/>` precondition body
(`3rdparty/sabre/dav/lib/DAV/Sync/Plugin.php:116`,
`.../Exception/InvalidSyncToken.php`). The sidecar used to return **400** with
a plain-text body and now returns the same **403** and body as Sabre.

**`conditional-get-missing` (resolved)** — the four conditional headers are
now evaluated for `GET`/`HEAD` on a card, following **RFC 7232** where Sabre's
`Server::checkPreconditions()` diverges from it: `If-None-Match` uses *weak*
comparison (PHP compares raw strings, so `W/"x"` never matched `"x"`), `HEAD`
follows `GET` semantics (PHP answers **412** for `HEAD` + a matching
`If-None-Match`, because it tests `'GET' === $method` literally before
`httpHead()` rewrites the method), and every `304` carries `ETag` +
`Last-Modified` (PHP omits the `ETag` for a bare `If-None-Match: *`). Those
three are deliberate, more-standards-conformant divergences. Kept from Sabre:
`If-Match` is strict (plus its legacy Evolution `\"` workaround) and fails with
412 even for a missing card, `If-Modified-Since` is consulted only when
`If-None-Match` is absent, `If-Unmodified-Since` staleness is 412, and
unparseable dates are silently ignored. The request matrix is pinned by
`tests/http_read.rs::conditional_*`.

### Auth

**`bruteforce-recording-off`** — `nextcloud_dav.record_bruteforce_attempts`
defaults to `false`, so the sidecar's own fast path records no failed login and
the auth path needs no DB write grant. The delay/block *checks* always run, and
the PHP fallback records failures as before. (The sidecar does write on the
card path — see `writes-501`.)

**`authtoken-v2-only`** — the fast path filters `version = 2`
(`PublicKeyToken::VERSION`); any other version is delegated to PHP. A mismatch
can only cause a fallback, never an acceptance.

**`unauthenticated-delegates`** — a request with no `Authorization` header is
first evaluated against the Nextcloud session cookie (when
`nextcloud_dav.session_redis_url` is configured); anything that cannot be
proved exactly is delegated (`501` → PHP) instead of refused, because the web
UI's DAV requests carry only a session cookie and an OAuth `Bearer` token is
PHP's. Credentials that are present but invalid still get `401`, exactly as PHP
does.

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

Three items of the original list are resolved and no longer open: the README's
`version = 1` wording (fixed — the filter is `version = 2`), `oc:owner-principal`
for shared books (implemented: shared books are served with the full sharing
property set), and UID extraction on write (the native write path reproduces
`getUID()` and the no-uid-conflict precondition, pinned by
`tests/write_path.rs`).

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
| `files-mount-delegation` | delegation | intentional | the mount model is served natively; a mount's *contents* may still delegate, its entry never does |
| `files-mount-external-backend-delegated` | delegation | intentional | a non-`local` files_external backend → 501 |
| `files-mount-external-check-changes-delegated` | delegation | intentional | local external with `filesystem_check_changes != 0` → 501 |
| `files-mount-circle-acl-delegated` | delegation | intentional | a groupfolder circle ACL rule or circle group membership → 501 |
| `files-mount-acl-inherit-delegated` | delegation | intentional | groupfolders `acl-inherit-per-user = true` → ACL folders → 501 |
| `files-mount-share-type-delegated` | delegation | intentional | a received share whose type is not user/group/usergroup → 501 |
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
| `calendars-property-gate-501` | propfind | intentional | an explicit calendars PROPFIND for an unimplemented-but-served qname (`cs:publish-url`, ...) → 501, never 404; known-404 qnames stay 404 |
| `calendars-special-children-acl-delegated` | delegation | intentional | home `Depth:1` requesting `{DAV:}acl`/`current-user-privilege-set` → 501 (the special children's ACLs are not modelled) |
| `calendars-trashed-federated-delegated` | delegation | intentional | a caller with a trashed or accepted federated calendar → home listing 501 (a home whose only extra children are subscriptions is served) |
| `calendars-subscriptions-served` | propfind | resolved | subscriptions served as home children and at their own paths with PHP's exact property set; a NULL `source` (PHP 500) delegates |
| `calendars-subscriptions-listing-order` | routing | intentional | subscriptions ordered by `calendarorder ASC, id ASC`; PHP has no id tie-breaker |
| `calendars-webcal-caching-delegated` | delegation | intentional | a webcal-caching client (KDE/Evolution/Windows UA or `X-NC-CalDAV-Webcal-Caching: On`) with subscriptions → 501 (`CachedSubscription` is a different node shape) |
| `calendars-own-home-only` | delegation | intentional | another principal's calendar home/calendar → 501 |
| `calendars-shared-listing-order` | routing | intentional | shared calendars ordered by `a.id`; PHP has no `ORDER BY` |
| `calendars-personal-displayname-localized` | propfind | resolved | `personal`/`contact_birthdays` displayname localized from the `dav` app l10n; a missing l10n source delegates (501) instead of serving English |
| `calendars-sync-nresults-zero` | report | resolved | `<d:nresults>0</d:nresults>` means zero rows (and no initial-sync 507), like `setMaxResults(0)` |
| `calendars-sync-float-token` | report | resolved | a `is_numeric()` token that is not an integer (`1.5`, `1e3`) is incremental and rejected by the database, like PHP |
| `calendars-group-share-acl-delegated` | delegation | intentional | a group-shared calendar's `{DAV:}acl` → 501 |
| `calendars-objects-and-query-delegated` | delegation | intentional | objects, trashbin, inbox/outbox, `calendar-query`, `?export` and all writes → 501 |
| `calendars-report-shared-delegated` | delegation | intentional | a REPORT on a shared or trashed calendar (or a subscription) → 501 |
| `calendars-report-expand-json-delegated` | delegation | intentional | `<cal:expand>` and `application/calendar+json` in a REPORT → 501 |
| `calendars-report-property-gate-501` | report | intentional | an unimplemented REPORT property → 501, never 404; known-404 qnames stay 404 |

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

**Mounts (phase 2).** A received share, a groupfolder or an external storage is
an `oc_mounts` row, not a child row in the home storage's `oc_filecache`. The
sidecar now serves the mount model natively (`src/mounts.rs`):

- a listing that **contains** mounts (the home root, 85 % of files PROPFINDs)
  merges one entry per `oc_mounts` row after the cache children, ascending by
  mount point, with the provider permission masks, `nc:mount-type` and
  `nc:is-mount-root`;
- a listing **inside** a share, groupfolder or local external mount reuses
  `oc_filecache` in the mount's storage and applies the provider mask to every
  row;
- the parent's synthetic `getetag`/`oc:size`/`getlastmodified`
  (`md5(etag.'::'.join('::', relPath.'/'.rootEtag.perms))`, decimal
  permissions, the double slash from the trailing mount-point slash, ascending
  order, `size += Σ`, `mtime = max`);
- the groupfolder **ACL rule engine** (`ACLManager`/`Rule`/`ACLCacheWrapper`):
  per-path rules for the caller's user/group mappings, parent-first
  `mergeRules`/`applyPermissions`, the `acl_default_no_permission` base
  permission and `canManageACL`; a row whose masked permissions are zero is
  dropped from the listing (and is a 404 at Depth 0).

Only genuinely unreproducible inputs stay delegated, each with its own entry:
circle membership (the circles app's own tables), non-local external backends,
external `filesystem_check_changes`, and received-share types other than
user/group/usergroup. Delegation is driven by the **requested** path: a mount
whose *contents* are unreproducible is still **described** as an entry in any
listing that contains it (name, etag, mtime, size, mimetype, permissions,
`nc:mount-type`, `nc:is-mount-root`, and its contribution to the parent's
synthetic etag/size/mtime); only a request **at or under** it answers 501. The
sole case that still suppresses a containing listing is a mount whose root
`oc_filecache` row is gone (a stale `oc_mounts` row), which cannot be described
at all. The share-manager properties for mounts
(`oc:share-types`, `nc:sharees`, `ocs:share-permissions`, `nc:note`,
`nc:hide-download`, `nc:share-attributes`) and `d:quota-*` are served from the
super-share / mount model rather than delegated. The mount map is cached per
user with a 30 s TTL; a stale or failed load answers 501, never 404, so a newly
created mount cannot be reported as missing.

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

## CalDAV `PROPFIND` and REPORTs

The sidecar serves `PROPFIND` on `/remote.php/dav/calendars/<user>/` (Depth 0
and 1) and `/remote.php/dav/calendars/<user>/<cal>/` (Depth 0), from
`oc_calendars` + `oc_dav_shares` + `oc_properties`. The property set, the child
set and the values were captured from a live Nextcloud 33.0.5 and matched
canonically by `tests/local/caldav_parity.sh`.

**The property gate.** An explicit request whose list contains any qname outside
the implemented set or the known-404 set answers 501, never 404. The known-404
set is the exact live 404 set (`{DAV:}quota-*`, `{DAV:}share-access`,
`{DAV:}getlastmodified`, `{cal}min-date-time`, ...). `{cs}publish-url` (200 iff
published) and the `{oc}`/`{DAV:}invite` share lists are modelled
(`calendars-property-gate-501`); `allowed-sharing-modes` under
`limitAddressBookAndCalendarSharingToOwner=yes`, and a share principal the
sidecar cannot resolve (a circle, a federated `principals/remote-users/` share,
or a user/group absent from the local tables), still delegate.

**The override layer.** `displayname`, `calendar-description`,
`calendar-timezone`, `calendar-order`, `calendar-color`,
`schedule-calendar-transp`, `disable-alarm-notifications`, `calendar-enabled`
and `enabled` are read from `oc_properties`, keyed by
`calendars/<requesting-user>/<wire-uri>`, and overwrite the `oc_calendars`
value — this is how a sharee's `PROPPATCH` survives.

**The ctag quirk.** Unlike CardDAV (a raw integer), CalDAV's
`{cs}getctag` is `http://sabre.io/ns/sync/<synctoken ?: '0'>`;
`{sabredav}sync-token` is the raw token and `{DAV:}sync-token` the prefixed one.

**Owned `Depth:0` has no `owner-principal`.** `CalendarHome::getChild()` uses
`getCalendarByUri()`, which does not set `{oc}owner-principal`; a shared
calendar falls back to `getCalendarsForUser()` and does. The sidecar reproduces
both (live-verified).

**Delegated.** Objects (`GET`/`PROPFIND`/`PUT`/`DELETE`), `trashbin/`,
`inbox`/`outbox`, federated and app-generated calendars, `calendar-query`,
`<cal:expand>`, `application/calendar+json`, free-busy, `?export` and every
write answer 501. A home Depth 1 that requests
`{DAV:}acl`/`current-user-privilege-set` delegates because the special
children's ACLs are not modelled; a Depth 0 on a calendar serves both for owned
and direct-user-shared calendars (a group-shared calendar's `{DAV:}acl`
delegates).

**REPORTs (`sync-collection`, `calendar-multiget`).** On an owned, live calendar
the sidecar serves both. `calendar-multiget` fetches the objects by URI in
100-URI chunks (`getMultipleCalendarObjects`) and drops hrefs that do not
resolve, exactly like Sabre's `Tree::getMultipleNodes()`. `calendar-data` is
the stored blob with every `\r` removed (Sabre's `CalDAV\Plugin::propFind()`
does `str_replace("\r", '', $val)`), while `{DAV:}getetag` is the quoted stored
`md5` — over the stored CRLF bytes, not the emitted body. `sync-collection`
uses the pre-increment token and `MAX(operation)` per URI; a non-numeric token
is an initial sync (there is no `init_` paging), an empty token with a limit is
`507` + `<d:number-of-matches-within-limits/>`, and a token missing the prefix
is `403` + `<d:valid-sync-token/>`. A `<d:nresults>0</d:nresults>` limit means
zero rows (and no initial-sync `507`), and a float `is_numeric()` token takes
the incremental path and is rejected by the database — both exactly like PHP
(`calendars-sync-nresults-zero`, `calendars-sync-float-token`). A REPORT on a
**shared** or trashed calendar or a subscription
delegates, because the shared object post-processing (`VALARM` stripping,
`CONFIDENTIAL` masking, size suppression) is a parse-and-re-serialise path the
sidecar does not reproduce. An unimplemented property in a REPORT request
delegates (501), never a 404; the known-404 object set
(`{caldav}schedule-tag`, `{oc}size`, `{DAV:}quota-*`, `{nc}deleted-at`, ...)
stays 404.

**Trashed / federated / webcal-caching.** A caller who can see a trashed calendar
or an accepted federated calendar gets a 501 for the home listing, because
PHP's child set cannot be reproduced by the subset model
(`calendars-trashed-federated-delegated`). Subscriptions, by contrast, are
served — as home children and at their own paths
(`calendars-subscriptions-served`) — except for a webcal-caching client (a
KDE/Evolution/Windows user agent or `X-NC-CalDAV-Webcal-Caching: On`), where
PHP returns a `CachedSubscription` node instead of a `Subscription`
(`calendars-webcal-caching-delegated`).

**Localization.** `Calendar::__construct()` localizes the `personal` and
`contact_birthdays` displayname through the `dav` app's l10n; the sidecar
reproduces both rewrites from `apps/dav/l10n/<lang>.json` and, when the l10n
source cannot be read but a name would be translated, delegates (501) instead
of serving the untranslated English string
(`calendars-personal-displayname-localized`).
