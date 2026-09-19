# Recon: the PHP CalDAV read path (for a Rust sidecar)

Source of truth: `nextcloud-server` (checkout `36.0.0 dev` at
`/home/aviallon/Programing/Opensource/nextcloud/nextcloud-server`). All
`apps/...`, `lib/...`, `3rdparty/...` paths below are relative to that repo;
`src/...` paths are relative to `nextcloud-dav`.

**Version note (important).** Production runs **Nextcloud 33.0.5**
(`kubectl -n nextcloud get pod nextcloud-prod-5f5b5746d9-xfld7`), the checkout is
36-dev. The deployed `apps/dav/lib/CalDAV/CalDavBackend.php` was copied out of a
33.0.5 container and diffed method-by-method against the checkout:

| method | 33.0.5 vs 36-dev |
|---|---|
| `getCalendarsForUser`, `getCalendarObjects`, `getCalendarObject`, `getMultipleCalendarObjects`, `getCalendarObjectByUID`, `getSubscriptionsForUser`, `calendarQuery`, `getChangesForCalendar` | **identical** |
| `getDenormalizedData` (write side) | **materially different** — see §3.5 |
| `Calendar.php` / `CalendarObject.php` / `CalendarHome.php` | only `#[\Override]` + extra calendar-proxy ACL entries |
| `propertyMap` | 33.0.5 has **6** entries; 36-dev adds 3 `default-alarm-*`/`disable-alarm-notifications` entries that **do not exist as columns** in 33.0.5 |

Everything read-path below is therefore true for the deployed 33.0.5 *except*
§3.5 (denormalisation), which is quoted in both versions.

Scale of the production instance (single `-tAc` aggregate, 2026-09-19):
`oc_calendars` **20**, `oc_calendarobjects` **10 114**, `oc_calendarchanges`
**59 433**, `oc_calendarsubscriptions` **4**, `oc_dav_shares type='calendar'`
**7**, `oc_calendar_reminders` **255**, `oc_schedulingobjects` **0**.

Companion recon: `shared-addressbooks.md` (the `oc_dav_shares` row, the
`<uri>_shared_by_<owner>` naming, principals/group membership, sync tokens) and
`card-events.md` (write path, event dispatch). This document does not repeat
that shared machinery; where CalDAV differs from CardDAV it says so explicitly.

---

## 0. Call path

`remote.php` → `apps/dav/appinfo/v2/remote.php` → `apps/dav/lib/Server.php`:
the CalDAV plugin stack is added **only** for the subtrees `calendars`,
`public-calendars`, `system-calendars`, `principals`
(`Server.php:206-227`):

```php
new DAV\Sharing\Plugin(...)          // Nextcloud's, not Sabre's
new \OCA\DAV\CalDAV\Plugin()         // NS_CALDAV, calendar-home-set
new ICSExportPlugin(...)             // ?export
new \OCA\DAV\CalDAV\Schedule\Plugin(...)
\OCP\Server::get(\OCA\DAV\CalDAV\Trashbin\Plugin::class)
new \OCA\DAV\CalDAV\WebcalCaching\Plugin($this->request)
new \Sabre\CalDAV\Subscriptions\Plugin()   // if allow_calendar_link_subscriptions=yes
new \Sabre\CalDAV\Notifications\Plugin()
new PublishPlugin(...)
new RateLimitingPlugin(...); new CalDavValidatePlugin(...)
```

plus, for **every** authenticated request (added in the `beforeMethod:*`
callback, `Server.php:288-355`): `\Sabre\DAV\PropertyStorage\Plugin` backed by
`CustomPropertiesBackend`, `QuotaPlugin`, `FilesPlugin`, `SharesPlugin`,
`TagsPlugin`, `CommentPropertiesPlugin`, `IMipPlugin`, `SearchPlugin`
(`apps/dav/lib/CalDAV/Search/SearchPlugin.php`).

Node tree for a calendar home (`apps/dav/lib/CalDAV/CalendarHome.php:88-140`),
in this order: `getCalendarsForUser()` → `Calendar` nodes, then `Inbox`,
`Outbox`, then `TrashbinHome`, then federated calendars, then
`getSubscriptionsForUser()` → `CachedSubscription` or `Subscription`, then
app-provided `ExternalCalendar`s. **There is no `notifications` child**:
`CalDavBackend` does not implement `NotificationSupport`
(`CalDavBackend.php:112`), even though the Sabre Notifications plugin is
registered.

Backend entry points:

| HTTP | path | handler |
|---|---|---|
| `PROPFIND` | `/remote.php/dav/calendars/<u>/` | `CalendarHome::getChildren` → `getCalendarsForUser` |
| `PROPFIND` | `/…/calendars/<u>/<cal>/` | `getCalendarByUri`, or the `getCalendarsForUser` fallback for shared/`_shared_by_` URIs (`CalendarHome.php:167-175`) |
| `PROPFIND` | `/…/calendars/<u>/<cal>/<obj>.ics` | `getCalendarObject` (`Calendar::getChild`, `Calendar.php:246`) |
| `REPORT` | `calendar-query`, `calendar-multiget` | `Sabre\CalDAV\Plugin::calendarQueryReport` (`3rdparty/sabre/dav/lib/CalDAV/Plugin.php:492`) → `Calendar::calendarQuery` → `CalDavBackend::calendarQuery` |
| `REPORT` | `sync-collection` | `Sabre\DAV\Sync\Plugin::syncCollection` → `Calendar::getChanges` → `getChangesForCalendar` |
| `REPORT` | `{http://nextcloud.com/ns}calendar-search` | `apps/dav/lib/CalDAV/Search/SearchPlugin.php:81` → `calendarSearch` |
| `REPORT` | `free-busy-query` | Sabre CalDAV `freeBusyQueryReport` (calls `calendarQuery` internally) |
| `GET` | `…?export` | `ICSExportPlugin::httpGet` |
| `GET`/`PROPFIND` | `/…/calendars/<u>/trashbin/…` | `TrashbinHome`, `DeletedCalendarObjectsCollection` |

---

## 1. Schema (as deployed, `information_schema` + `pg_indexes` on the prod DB)

Prefix `oc_`. Types are PostgreSQL 18 as reported by `information_schema`.

### `oc_calendars` — one row per calendar (owned; shared rows live in `oc_dav_shares`)

| column | type | null | default | meaning |
|---|---|---|---|---|
| `id` | bigint | NOT NULL | seq | PK; the `calendarid` used everywhere |
| `principaluri` | varchar(255) | null | — | owner, `principals/users/<uid>` |
| `displayname` | varchar(255) | null | — | `{DAV:}displayname` |
| `uri` | varchar(255) | null | — | last path segment; unique per principal |
| `synctoken` | int | NOT NULL | `1` | monotonic; incremented by `addChanges` |
| `description` | varchar(255) | null | — | `{urn:ietf:params:xml:ns:caldav}calendar-description` |
| `calendarorder` | int | NOT NULL | `0` | `{http://apple.com/ns/ical/}calendar-order`; **the listing ORDER BY** |
| `calendarcolor` | varchar(255) | null | — | `{http://apple.com/ns/ical/}calendar-color`, e.g. `#00679e` |
| `timezone` | text | null | — | `{urn:…}calendar-timezone`, a whole VCALENDAR with one VTIMEZONE |
| `components` | varchar(64) | null | — | CSV, e.g. `VEVENT` or `VEVENT,VTODO` → `supported-calendar-component-set` |
| `transparent` | smallint | NOT NULL | `0` | 0=opaque, 1=transparent → `schedule-calendar-transp` |
| `deleted_at` | int | null | — | **trashbin**: unix ts when the calendar was deleted; NULL = live |

Indexes: `calendars_index (principaluri, uri) UNIQUE`, `cals_princ_del_idx
(principaluri, deleted_at)`, PK `id`.

### `oc_calendarobjects`

| column | type | null | default | meaning |
|---|---|---|---|---|
| `id` | bigint | NOT NULL | seq | PK |
| `calendardata` | bytea | null | — | the raw iCalendar blob, **CRLF line endings** |
| `uri` | varchar(255) | null | — | `<name>.ics`, or `<name>-deleted.ics` when trashed |
| `calendarid` | bigint | NOT NULL | — | `oc_calendars.id` **or** `oc_calendarsubscriptions.id` (see `calendartype`) |
| `lastmodified` | int | null | — | unix ts → `{DAV:}getlastmodified` |
| `etag` | varchar(32) | null | — | `md5(calendardata)` **without quotes** |
| `size` | bigint | NOT NULL | — | `strlen(calendardata)` → `{DAV:}getcontentlength` |
| `componenttype` | varchar(8) | null | — | `VEVENT`/`VTODO`/`VJOURNAL` (mixed case in DB; PHP lowercases on read) |
| `firstoccurence` | bigint | null | — | unix ts, start of the **first** occurrence |
| `lastoccurence` | bigint | null | — | unix ts, end of the **last** occurrence; `MAX_DATE` sentinel for infinite RRULE |
| `uid` | varchar(255) (512 in 36-dev) | null | — | iCalendar `UID` |
| `classification` | int | null | `0` | 0 PUBLIC, 1 PRIVATE, 2 CONFIDENTIAL |
| `calendartype` | int | NOT NULL | `0` | 0 calendar, 1 subscription (webcal cache), 2 federated |
| `deleted_at` | int | null | — | **trashbin**: NULL = live |

Indexes: `calobjects_index (calendarid, calendartype, uri) UNIQUE`,
`calobjects_by_uid_index (calendarid, calendartype, uid)`,
`calobj_clssfction_index (classification)`, PK `id`.
Note: there is **no index on `firstoccurence`/`lastoccurence`** — the
`calendar-query` time-range predicate is an unindexed filter inside the
`(calendarid, calendartype)` index range.

### `oc_calendarchanges` — the sync log

| column | type | null | default | meaning |
|---|---|---|---|---|
| `id` | bigint | NOT NULL | seq | PK |
| `uri` | varchar(255) | null | — | object uri as it was at change time |
| `synctoken` | int | NOT NULL | `1` | the **pre-increment** calendar token (see §3.4) |
| `calendarid` | bigint | NOT NULL | — | calendar **or** subscription id |
| `operation` | smallint | NOT NULL | — | 1 add, 2 modify, 3 delete |
| `calendartype` | int | NOT NULL | `0` | mirrors `calendarobjects.calendartype` |
| `created_at` | int | NOT NULL | `0` | unix ts |

Index: `calid_type_synctoken (calendarid, calendartype, synctoken)`.

### `oc_calendarsubscriptions`

`id`, `uri`, `principaluri`, `displayname`, `refreshrate` (ISO8601 duration
string, e.g. `PT4H`), `calendarorder`, `calendarcolor`, `striptodos`,
`stripalarms`, `stripattachments` (smallint), `lastmodified`, `synctoken`
(int NOT NULL default 1), `source` (**text**, the webcal URL; note 33.0.5 still
has a leftover `source_copy` column from migration 1008 — the deployed DB shows
only `source`). Unique `calsub_index (principaluri, uri)`.

### `oc_dav_shares` (calendar rows)

Identical shape to CardDAV (`shared-addressbooks.md` §0): `id`, `principaluri`
(sharee), `type` = `'calendar'`, `access` (`1` owner / `2` read-write /
`3` read-only / `4` **public link** / `5` unshared tombstone —
`apps/dav/lib/DAV/Sharing/Backend.php:24-29`), `resourceid` →
`oc_calendars.id`, `publicuri` (only for `access=4`), `token` (added in
`Version1034Date20250605132605`).

### `oc_schedulingobjects` (the CalDAV `inbox`)

`id`, `principaluri`, `calendardata` (bytea), `uri`, `lastmodified`, `etag`,
`size`. Indexes `principaluri`, `lastmodified`. 0 rows in production.
Read by `getSchedulingObject` / `getSchedulingObjects`
(`CalDavBackend.php:3181`, `:3216`).

### `oc_calendar_reminders` (not on the DAV read path)

`id`, `calendar_id`, `object_id`, `is_recurring`, `uid`, `recurrence_id`,
`is_recurrence_exception`, `event_hash`, `alarm_hash`, `type`, `is_relative`,
`notification_date`, `is_repeat_based`. Written/read only by
`apps/dav/lib/CalDAV/Reminder/Backend.php` (notification jobs), never by a
DAV request. **A sidecar can ignore it entirely.**

### `oc_calendarobjects_props` (the calendar search index)

`id`, `calendarid`, `objectid`, `name`, `parameter`, `value`, `calendartype`.
Still present in 33.0.5 and populated by `updateProperties`
(`CalDavBackend.php:3711`) for `INDEXED_PROPERTIES`
(`CATEGORIES, COMMENT, DESCRIPTION, LOCATION, RESOURCES, STATUS, SUMMARY,
ATTENDEE, CONTACT, ORGANIZER`, `CalDavBackend.php:181`). Used **only** by the
`calendar-search` REPORT / OCP `search()`, not by `calendar-query`.

### `oc_properties` — the per-node property override store (hazard, see §5.4)

`propertypath`, `propertyname`, `propertyvalue`, `valuetype`, `userid`.
Production has **17** rows under `calendars/…`: 11×
`{http://owncloud.org/ns}calendar-enabled`, 4×
`{http://apple.com/ns/ical/}calendar-order`, 1×
`{urn:…}schedule-calendar-transp`, 1× `{urn:…}calendar-availability`, 2×
`default-alarm-*`. **12 of the 17 sit on a path whose user is not the calendar
owner** — i.e. they are live sharee overrides.

---

## 2. Per-method SQL and semantics

All SQL below is the exact shape produced by the Nextcloud query builder
(`?` = named parameter).

### 2.1 `getCalendarsForUser($principalUri)` — `CalDavBackend.php:325-461`

Two queries.

**(a) Owned** (`:333-374`):

```sql
SELECT displayname, description, timezone, calendarorder, calendarcolor, deleted_at,
       id, uri, synctoken, components, principaluri, transparent
FROM oc_calendars
WHERE principaluri = ?
ORDER BY calendarorder ASC
```

The column list is `array_column($propertyMap, 0)` + `id, uri, synctoken,
components, principaluri, transparent`. **No `deleted_at IS NULL` filter**:
trashed calendars are returned here (they are only marked with a different
`{DAV:}resourcetype`, §2.10).

**(b) Shared** (`:381-455`):

```sql
SELECT a.displayname, a.description, a.timezone, a.calendarorder, a.calendarcolor, a.deleted_at,
       a.id, a.uri, a.synctoken, a.components, a.principaluri, a.transparent,
       s.access
FROM oc_dav_shares s
JOIN oc_calendars a ON s.resourceid = a.id
WHERE s.principaluri IN (?, ?, …)      -- group principals + circle principals + own
  AND s.type = 'calendar'
  AND a.id NOT IN (
      SELECT d.resourceid FROM oc_dav_shares d
      WHERE d.access = 5 AND d.principaluri IN (?, ?, …)   -- same list
  )
```

**No `ORDER BY`** on the shared query — order is whatever the DB returns, then
the merge below. `$principals` = `getGroupMembership(…, true)` +
`getCircleMembership(…)` + the user's own converted principal
(`:377-379`).

PHP post-processing, owned branch (`:342-373`):
* `components` CSV → array → `SupportedCalendarComponentSet`.
* `principaluri` re-converted via `convertPrincipal` (`:3987`): the legacy
  v1 form `principals/<uid>` ↔ v2 `principals/users/<uid>`.
* builds the fixed property set (`:360-368`): `getctag` =
  `'http://sabre.io/ns/sync/' . ($synctoken ?: '0')`, `sync-token` =
  raw `$synctoken ?: '0'`, `supported-calendar-component-set`,
  `schedule-calendar-transp` (`transparent` ? transparent : opaque),
  `owner-principal` = the **requesting** principal.
* `rowToCalendar` (`:4042`) adds the `propertyMap` columns with `settype`
  (`int` for calendarorder/deleted_at, `string` otherwise).
* `addOwnerPrincipalToCalendar` (`:4002`) adds
  `{http://nextcloud.com/ns}owner-displayname` by asking the **principal
  backend** for `{DAV:}displayname` of `owner-principal` (one extra lookup per
  calendar — cached by `Principal`).
* `addResourceTypeToCalendar` (`:4020`).
* de-duplication: `if (!isset($calendars[$calendar['id']]))` — first row wins.

PHP post-processing, shared branch (`:417-455`):
* `if ($row['principaluri'] === $principalUri) continue;` — the owner also
  reaching their own calendar through a group share is dropped (the owned entry
  wins).
* read-only resolution: `$readOnly = ((int)$row['access'] === 3)`; if the
  calendar is already present and the new share is read-only, `continue`; if the
  calendar is already present **read-write**, `continue` ("no more permissions
  can be gained").
* `[, $name] = Uri\split($row['principaluri'])`; **`uri = $row['uri'] .
  '_shared_by_' . $name`**; `displayname .= ' (' . <owner display name> . ')'`
  (`:439-440`).
* `principaluri` of the returned entry is the **sharee**; `owner-principal` is
  the real owner; `schedule-calendar-transp` is hard-coded `'transparent'`
  (`:448`); `{http://owncloud.org/ns}read-only` is **always set** on shared
  entries (true or false) (`:450`).
* the result is `array_values($calendars)` — owned calendars first (in
  `calendarorder`), then shared ones in DB order.

### 2.2 `getCalendarsForUserCount` — `:235`

```sql
SELECT COUNT(*) FROM oc_calendars WHERE principaluri = ? AND uri <> 'contact_birthdays'
```
(`excludeBirthday` defaults true; `BIRTHDAY_CALENDAR_URI = 'contact_birthdays'`).
Used only by `Security/RateLimitingPlugin.php:71` and
`Listener/UserEventsListener.php:161` — **never on a DAV read path**.

### 2.3 `getCalendarById($id)` / `getCalendarByUri($principal, $uri)` — `:683` / `:633`

```sql
SELECT <same 12 columns> FROM oc_calendars WHERE id = ? LIMIT 1
SELECT <same 12 columns> FROM oc_calendars WHERE uri = ? AND principaluri = ? LIMIT 1
```

`getCalendarById` returns `sync-token` as `$row['synctoken'] ?? 0` (an int, not
the `?: '0'` string form — a cosmetic upstream inconsistency). `getCalendarByUri`
is the fast path for a **non-shared** calendar lookup in `CalendarHome::getChild`
(`CalendarHome.php:167`); shared URIs (`<uri>_shared_by_<owner>`) miss it and
fall through to the `getCalendarsForUser` scan (`:174-179`) — i.e. **every
request into a shared calendar re-runs the whole two-query listing.**

### 2.4 `getUsersOwnCalendars($principalUri)` — `:469`

Same as 2.1(a) with no `ORDER BY` de-dup guard. Used only by
`Listener/UserEventsListener.php:100` (user deletion) — not a DAV read path.

### 2.5 `getSubscriptionsForUser($principalUri)` — `:2983`

```sql
SELECT displayname, refreshrate, calendarorder, calendarcolor, striptodos,
       stripalarms, stripattachments, id, uri, source, principaluri, lastmodified, synctoken
FROM oc_calendarsubscriptions
WHERE principaluri = ?
ORDER BY calendarorder ASC
```

Per row PHP adds `supported-calendar-component-set` = `['VTODO','VEVENT']`
(**hard-coded**, `:760`) and `sync-token` = `$row['synctoken'] ?: '0'`, then
`rowToSubscription` (`:4065`) adds the `subscriptionPropertyMap` columns.
Note subscriptions have **no `getctag`**, no `owner-principal`, no
`read-only`, and `source` is exposed via
`{http://calendarserver.org/ns/}source` as a `Href`.

### 2.6 `getCalendarObjects($calendarId, $calendarType = 0)` — `:1137`

```sql
SELECT id, uri, lastmodified, etag, calendarid, size, componenttype, classification
FROM oc_calendarobjects
WHERE calendarid = ? AND calendartype = ? AND deleted_at IS NULL
```
No `ORDER BY`. **No `calendardata`** (deliberately: this is the cheap listing).
PHP maps `etag` → `'"' . $row['etag'] . '"'`, `size` → int, `component` →
`strtolower($row['componenttype'])`, `classification` → int.
Callers: `Calendar::getChildren` (Depth:1 PROPFIND) and the Sabre
`AbstractBackend::calendarQuery` fallback (not used, Nextcloud overrides it).
`Calendar::getChildren` (`Calendar.php:265`) **skips
`classification === CLASSIFICATION_PRIVATE` objects when the calendar is
shared** (`isShared()` = `owner-principal !== principaluri`).

### 2.7 `getMultipleCalendarObjects($calendarId, $uris, $calendarType = 0)` — `:1462`

```sql
SELECT id, uri, lastmodified, etag, calendarid, size, calendardata, componenttype, classification
FROM oc_calendarobjects
WHERE calendarid = ? AND uri IN (…) AND calendartype = ? AND deleted_at IS NULL
```
chunked by **100** URIs (`array_chunk($uris, 100)`). Includes `calendardata`.
Same private-object filtering in `Calendar::getMultipleChildren`
(`Calendar.php:284`). This is what `calendar-multiget` uses.

### 2.8 `getCalendarObject($calendarId, $objectUri, $calendarType = 0)` — `:1408`

```sql
SELECT id, uri, uid, lastmodified, etag, calendarid, size, calendardata,
       componenttype, classification, deleted_at
FROM oc_calendarobjects
WHERE calendarid = ? AND uri = ? AND calendartype = ?
```
**No `deleted_at IS NULL`** — the trashed object is still reachable by URI (it
is only hidden because the listing filters it out; `Calendar::getChild` does not
check `deleted_at`). Result is memoised in `$this->cachedObjects[$id.'::'.$uri.'::'.$type]`
(`:1409-1412`) — this is why `calendarQuery` pre-populates the cache
(`:2044-2045`) so that the per-URI `getPropertiesForPath` in
`calendarQueryReport` (`Plugin.php:604`) costs **zero** further queries.

### 2.9 `getCalendarObjectByUID($principalUri, $uid, $calendarUri = null)` — `:2678`

```sql
SELECT c.uri AS calendaruri, co.uri AS objecturi
FROM oc_calendarobjects co
LEFT JOIN oc_calendars c ON co.calendarid = c.id
WHERE c.principaluri = ? AND co.uid = ? AND co.deleted_at IS NULL
  [AND c.uri = ?]
```
**Owned calendars only** — shared and subscribed calendars are ignored, and
there is no `calendartype` filter (a webcal-cached object with the same UID and
a colliding id could match). Returns `"<calendaruri>/<objecturi>"` or null.
First row only (`fetchAssociative`), no `LIMIT`. Used by `Schedule\Plugin` and
the `calendar-object-uid` lookups.

### 2.10 `getChangesForCalendar($calendarId, $syncToken, $syncLevel, $limit, $calendarType)` — `:2882`

Three queries inside one transaction:

```sql
-- 1. current token
SELECT synctoken FROM oc_calendars WHERE id = ?            -- or oc_calendarsubscriptions
```

```sql
-- 2. initial sync ($syncToken is null / non-numeric)
SELECT uri FROM oc_calendarobjects
WHERE calendarid = ? AND calendartype = ? AND deleted_at IS NULL
```

```sql
-- 3. incremental sync
SELECT uri, MAX(operation) FROM oc_calendarchanges
WHERE calendarid = ? AND calendartype = ? AND synctoken >= ? AND synctoken < ?
GROUP BY uri
```

`$syncToken` here is the part **after** the `http://sabre.io/ns/sync/` prefix
(stripped by `Sabre\DAV\Sync\Plugin::syncCollection`, `Sync/Plugin.php:115-120`).
`$currentToken === false` → `null` → the plugin raises
`InvalidSyncToken`. An optional `LIMIT` is applied to the *grouped* result
(`setMaxResults`), and `result_truncated` is **never** set by Nextcloud.
`Calendar::getChanges` (`Calendar.php:432`) throws
`UnsupportedLimitOnInitialSyncException` when `!$syncToken && $limit`.
The `MAX(operation)` means a uri touched add→delete inside one token window is
reported as delete (3 > 1).

### 2.11 `calendarQuery($calendarId, $filters, $calendarType = 0)` — `:1958`

```sql
SELECT id, uri, uid, lastmodified, etag, calendarid, size, calendardata,
       componenttype, classification, deleted_at
FROM oc_calendarobjects
WHERE calendarid = ? AND calendartype = ? AND deleted_at IS NULL
  [AND componenttype = :componentType]        -- only when comp-filters[0] is a real filter
  [AND lastoccurence > :start]                -- only when time-range has a start
  [AND firstoccurence < :end]                 -- only when time-range has an end
```
No `ORDER BY`, no `LIMIT`. Then PHP:
1. `readBlob` the `calendardata` (`:2009`) so the post-filter does not re-fetch.
2. If `$requirePostFilter`, run `validateFilterForObject` (inherited from
   `Sabre\CalDAV\Backend\AbstractBackend`, i.e.
   `VObject\Reader::read()` + `Sabre\CalDAV\CalendarQueryValidator`), skipping
   the row on `ParseException` / `InvalidDataException` /
   `MaxInstancesExceededException` (the latter is logged as a warning and the
   object is silently dropped — >3500 recurrence instances).
3. Collect the matching `uri`s and **populate `$this->cachedObjects`** so the
   subsequent per-object `getPropertiesForPath` is free (`:2044-2045`).

`$requirePostFilter` is computed **only from `comp-filters[0]`** (`:1962-1989`),
which is a real deviation — see §4.4.

### 2.12 `Calendar::calendarQuery` override — `apps/dav/lib/CalDAV/Calendar.php:307`

For a **shared** calendar it additionally filters the URI list through
`childExists()` — which is one `getCalendarObject` per URI (`:307-316`), i.e.
the `classification === PRIVATE` hiding for sharees. For an owned calendar the
list is returned as-is.

### 2.13 Trashbin read path — `:1164`, `:1204-1361`

`getDeletedCalendarObjectsByPrincipal($principalUri)` (`:1204`) runs
`collectDeletedCalendarObjectsForPrincipal` twice: once for the principal, once
per calendar-proxy delegator (with an access overlay). Each pass is two queries:

```sql
-- owned
SELECT co.id, co.uri, co.lastmodified, co.etag, co.calendarid, co.size,
       co.componenttype, co.classification, co.deleted_at,
       c.uri AS calendaruri, c.principaluri AS calendarprincipaluri
FROM oc_calendarobjects co
JOIN oc_calendars c ON c.id = co.calendarid
WHERE c.principaluri = ? AND co.deleted_at IS NOT NULL AND c.deleted_at IS NULL

-- shared (applySharedCalendarFilters, :1376)
SELECT … , s.access AS shareaccess
FROM oc_calendarobjects co
JOIN oc_calendars c ON c.id = co.calendarid
JOIN oc_dav_shares s ON s.resourceid = c.id
WHERE co.deleted_at IS NOT NULL AND c.deleted_at IS NULL
  AND s.principaluri IN (…) AND s.type = 'calendar' AND c.principaluri <> ?
  AND c.id NOT IN (SELECT d.resourceid FROM oc_dav_shares d WHERE d.access = 5 AND d.principaluri IN (…))
```
Deduplicated in PHP by object id, keeping the most permissive access
(`resultHasMorePermissiveEntry`, `:1293`). The sharee-facing URI is
`<calendauri>_shared_by_<owner>`; delegated entries get `_delegated_by_<owner>`
(`:1253`, `:1269`).

### 2.14 Write-side helpers, only for the columns they define

* `createCalendarObject` (`:1550`): inserts
  `calendarid, uri, calendardata, lastmodified=time(), etag, size,
  componenttype, firstoccurence, lastoccurence, classification, uid,
  calendartype`; then `updateProperties` (search index) and
  `addChanges(…, 1)`.
* `updateCalendarObject` (`:1628`): same minus `uid`/`calendartype`,
  `addChanges(…, 2)`.
* `deleteCalendarObject` (`:1769`): with the trashbin, sets
  `deleted_at = time()` and rewrites `uri` to `<base>-deleted.ics`
  (see `restoreChanges` `:3395` which maps `-deleted.ics` → `.ics` for the
  delete change row); `addChanges(…, 3)`.
* `getDenormalizedData` (`:3418`) — see §3.5.

---

## 3. ETag, ctag, sync-token, `oc_calendarchanges`

### 3.1 Per-object ETag

`etag = md5($calendarData)` where `$calendarData` is the **raw stored blob**
(`getDenormalizedData` `:3420-3423` in 36-dev; the 33.0.5 body returns
`'etag' => md5($calendarData)` too). Stored **without** quotes in the column;
PHP adds them on every read path (`'"' . $row['etag'] . '"'` — `:1151`, `:1436`,
`:1477`, `:1488`). So the wire ETag is
`"85fc79b91397bd7c56ec418e56290f27"` (observed).

**Gotcha:** the blob uses CRLF, and XML text normalisation turns CRLF into LF
on the wire, so `md5(<the calendar-data the client receives>) != <the ETag>`.
Verified against production data: DB `md5(calendardata)` =
`85fc79b9…` (ETag), `md5` of the returned XML text = `39994c7e…`. A sidecar
must hash the **stored bytes**, not the bytes it emits.

Sabre's fallback `'"'.md5($this->get()).'"'` (`CalDAV/CalendarObject.php:140`)
is never reached because `objectData['etag']` is always set.

### 3.2 ctag

`'{http://calendarserver.org/ns/}getctag' => 'http://sabre.io/ns/sync/' . ($row['synctoken'] ?: '0')`
— built by the backend in every calendar-returning method (`:363`, `:445`,
`:495`, `:547`, `:612`, `:666`, `:715`). Note the ctag is a **URL-shaped**
string, and `?: '0'` means a literal `0` token renders as `…/sync/0`.

There is a fallback in `Sabre\DAV\CorePlugin::propFindLate`
(`3rdparty/sabre/dav/lib/DAV/CorePlugin.php:817`) that derives `getctag` from
`{http://sabredav.org/ns}sync-token` or `{DAV:}sync-token` if the backend did
not provide one — relevant for calendars reached through a code path that does
not set it.

### 3.3 `{DAV:}sync-token`

Two distinct properties:

* `{http://sabredav.org/ns}sync-token` — the backend's **raw token string**
  (`'2'`), exposed only because `Sabre\CalDAV\Calendar::getProperties()`
  (`3rdparty/sabre/dav/lib/CalDAV/Calendar.php:81-95`) returns *every*
  clark-notation key of `calendarInfo`, and
  `CorePlugin::propFindNode` (`CorePlugin.php:801`) feeds those to the
  `PropFind` for any property still at 404.
* `{DAV:}sync-token` — produced by `Sabre\DAV\Sync\Plugin::propFind`
  (`DAV/Sync/Plugin.php:189-197`), which prefixes:
  `'http://sabre.io/ns/sync/' . $node->getSyncToken()`.
  `Calendar::getSyncToken()` (`CalDAV/Calendar.php:366-383`) returns the
  `{http://sabredav.org/ns}sync-token` value.

Observed on the wire: `<cs:getctag>http://sabre.io/ns/sync/2</cs:getctag>` and
`<d:sync-token>http://sabre.io/ns/sync/2</d:sync-token>`. The sync-collection
response carries the same prefixed token in `<d:sync-token>` at the multistatus
level (`Sync/Plugin.php:176`).

### 3.4 How `oc_calendarchanges` rows are written — `addChanges`, `:3326`

```php
$syncToken = (int) SELECT synctoken FROM oc_calendars WHERE id = :id;   // read
INSERT INTO oc_calendarchanges (uri, synctoken, calendarid, operation, calendartype, created_at)
  VALUES (:uri, :syncToken, :calendarid, :operation, :calendartype, :now);   -- one row per uri
UPDATE oc_calendars SET synctoken = :syncToken + 1 WHERE id = :id;           -- then increment
```

So a change is logged under the **pre-increment** token, and the collection
token afterwards is `T+1`. Verified on production data:
`oc_calendarchanges` rows have `synctoken = 1` while `oc_calendars.synctoken = 2`.
`getChangesForCalendar` therefore selects
`synctoken >= clientToken AND synctoken < currentToken`. `restoreChanges`
(`:3362`) re-marks everything: all live objects as operation **2** and all
trashed ones as **3**.

### 3.5 `getDenormalizedData` — the **33.0.5 vs 36-dev divergence**

`firstoccurence`/`lastoccurence` are what the `calendar-query` SQL filter
compares against, so the writer matters even for a read-only sidecar.

**36-dev** (`CalDavBackend.php:3418-3545`):
* `etag = md5($data)`, `size = strlen($data)`.
* `componentType`/`uid`/`classification` are extracted from **one** component:
  `getBaseComponent()` if its name is in `[VEVENT, VTODO, VJOURNAL]`, otherwise
  the min/max range over all VEVENT/VTODO/VJOURNAL components present; a
  VCALENDAR with none of them throws `BadRequest`.
* start = `DTSTART` (only if it is an iCalendar `DateTime`), end:
  * recurring (`RRULE` or `RDATE`): `RDATE` instances push the end out; an
    `RRULE` is evaluated by `EventReaderRRule` — infinite → `MAX_DATE`
    (`2038-01-01`), else `concludes()`; then the duration of one occurrence is
    added (`DTEND`-`DTSTART` diff, `DURATION`, or `DUE`-`DTSTART` diff, or +1
    day for a date-only VEVENT). Infinite rules keep the `MAX_DATE` sentinel.
  * singleton: `DTEND`, else `DURATION`, else `DUE`, else +1 day for a date-only
    VEVENT, else end = start.
* both timestamps `max(0, …)`; **missing `DTSTART` yields `0`, not NULL**.
* `classification`: `CLASS` absent → PUBLIC; `PUBLIC`→0, `CONFIDENTIAL`→2,
  anything else→1.

**33.0.5 (deployed, `CalDavBackend-33.php:3083-3190`)** differs in ways that
change stored data:
* `componentType`/`uid` come from the **first non-`VTIMEZONE` component** — so a
  VCALENDAR whose first component is e.g. `VFREEBUSY` gets
  `componenttype = 'VFREEBUSY'`, and `uid = (string)$component->UID` (empty
  string if absent).
* `classification` is read from `$component`, which after the loop is the
  **last** non-VTIMEZONE component (or `$vEvents[0]` when a `DTSTART` exists) —
  a multi-component object can get the wrong classification.
* Recurrence range uses `Sabre\VObject\Recur\EventIterator($vEvents)` over all
  VEVENTs (master + exceptions); infinite → `MAX_DATE`; otherwise it iterates
  to the last occurrence and takes `getDtEnd()`. Singleton with a timed
  `DTSTART` and **no `DTEND`/`DURATION`** → `lastOccurence = firstOccurence`
  (a zero-length window).
* **`firstOccurence`/`lastOccurence` are `NULL`** when there is no `DTSTART`
  (the `max(0, …)` is skipped, `:3185-3186`). `NULL` never satisfies
  `lastoccurence > :start` in SQL.

A sidecar that reimplements `calendar-query` must therefore tolerate NULL
occurrences and must not assume `componenttype` is one of the three component
names.

---

## 4. Time-range semantics — the risky part

### 4.1 What SQL does vs what PHP does

The prefilter is a plain interval-overlap test on the precomputed range:

```sql
lastoccurence  > :start      -- only if the filter has a start
firstoccurence < :end        -- only if the filter has an end
```

This is a **correct superset**: if any occurrence of the object overlaps
`[start, end)` then necessarily `firstoccurence ≤ occurrenceStart < end` and
`lastoccurence ≥ occurrenceEnd > start`. So it can produce false positives but
never false negatives, *provided* `firstoccurence`/`lastoccurence` were written
correctly (see §3.5).

The exact answer comes from `Sabre\CalDAV\CalendarQueryValidator`
(`3rdparty/sabre/dav/lib/CalDAV/CalendarQueryValidator.php`, 354 lines) fed by
`Sabre\VObject\Reader::read()`:

* `validate()` (`:30-42`): top-level `$vObject->name !== $filters['name']`
  (i.e. `VCALENDAR`) → false; then AND of `validateCompFilters` and
  `validatePropFilters`.
* `validateCompFilters` (`:54-113`): for each filter, `is-not-defined` →
  `isset($parent->{$name})` must be false; otherwise the component must exist;
  a `time-range` must be satisfied by **at least one** sub-component
  (`continue 2` on the first hit); sub-comp/comp-prop filters must be satisfied
  by at least one sub-component.
* `validatePropFilters` (`:124-186`): symmetric, plus `param-filters` and
  `text-match`.
* `validateParamFilters` (`:196-238`) / `validateTextMatch` (`:250-260`):
  `\Sabre\DAV\StringUtil::textMatch($value, $needle, $collation)`, then
  `negate-condition xor $isMatching`.
* `validateTimeRange` (`:271-354`): missing bounds default to
  `1900-01-01` / `3000-01-01`; `VEVENT`/`VTODO`/`VJOURNAL` →
  `$component->isInTimeRange($start, $end)`; `VALARM` has a special
  recurrence-expanding path (`EventIterator` over the parent VEVENT) and is
  documented in-code as "a hack, and an expensive one too"; `VFREEBUSY` throws
  `NotImplemented`; `COMPLETED/CREATED/DTEND/DTSTAMP/DTSTART/DUE/LAST-MODIFIED`
  are a simple `start <= value <= end`; anything else throws `BadRequest`.
* `VEvent::isInTimeRange` (`3rdparty/sabre/vobject/lib/Component/VEvent.php:30-72`):
  recurring → `new EventIterator(...)`, `fastForward($start)`, then
  `getDTStart() < $end && getDTEnd() > $start`; singleton → `DTEND`, else
  `DURATION`, else +1 day for a date-only `DTSTART`, else end = start, then
  `($start < $effectiveEnd) && ($end > $effectiveStart)`.

Recurring events therefore **do not** rely on `firstoccurence`/`lastoccurence`
being exact — the SQL only narrows, and `EventIterator` decides.

### 4.2 `expand`

`{urn:ietf:params:xml:ns:caldav}expand` is parsed by
`Sabre\CalDAV\Xml\Filter\CalendarData::xmlDeserialize`
(`3rdparty/sabre/dav/lib/CalDAV/Xml/Filter/CalendarData.php:57-71`): both
`start` and `end` are **required**, and `end <= start` is a `BadRequest`. It
lands on `$report->expand` and is applied in
`Sabre\CalDAV\Plugin::calendarQueryReport` (`Plugin.php:505-520`, `:604-616`) /
`calendarMultiGetReport` (`:438-466`):

* the calendar's `{urn:…}calendar-timezone` is read and parsed
  (`$vtimezoneObj->VTIMEZONE->getTimeZone()`), defaulting to UTC;
* `VCalendar::expand($start, $end, $tz)` (`vobject/lib/Component/VCalendar.php:284`)
  strips every `VTIMEZONE`, converts all date-times to UTC, expands recurrences
  into individual `VEVENT`s, and clones non-recurring in-range events;
* the result is **re-serialised**, so the returned `calendar-data` differs
  byte-for-byte from the stored blob (the ETag does *not* change — this is an
  upstream deviation worth recording).

`contentType = application/calendar+json` (via `Content-Type` in
`calendar-data` or the `Accept` header) makes it `json_encode($vObject->jsonSerialize())`.

### 4.3 `component` / `prop-filter` / `param-filter` / `is-not-defined` / `allof`

* The SQL `componenttype = ?` predicate is taken from
  **`$filters['comp-filters'][0]['name']`** only — the first *nested*
  comp-filter (the outer `VCALENDAR` filter is `$filters` itself). Its value is
  the raw client string (`VEVENT`), compared against the stored
  `componenttype` column, which is **case-sensitive in PostgreSQL** and, in
  33.0.5, may be `VFREEBUSY` or similar (§3.5).
* `is-not-defined` on the top-level comp-filter disables the SQL component
  predicate entirely.
* `allof`/`anyof` are **not implemented anywhere in Sabre**
  (`grep -rn 'anyof\|allof' 3rdparty/sabre/dav/lib/CalDAV/` → nothing). A
  `<c:anyof>` element is silently dropped by `PropFilter::xmlDeserialize`, and
  `validatePropFilters` requires *every* entry in the list to pass, i.e. AND
  semantics only. A second `<c:text-match>` inside one `prop-filter`
  overwrites the first (the parser assigns, not appends).
* `param-filter` handles `ATTENDEE;CN=`, `ORGANIZER;CN=` etc. via
  `Property::getParts()` (comma-separated parameter values are split and each
  part is matched).

### 4.4 The `$requirePostFilter` shortcut — a real deviation

`CalDavBackend::calendarQuery` (`:1962-1989`) only inspects
**`comp-filters[0]`** to decide whether the post-filter can be skipped:

```php
if (count($filters['comp-filters']) > 0 && !$filters['comp-filters'][0]['is-not-defined']) {
    $componentType = $filters['comp-filters'][0]['name'];
    if (!$filters['prop-filters'] && !$filters['comp-filters'][0]['comp-filters']
        && !$filters['comp-filters'][0]['time-range'] && !$filters['comp-filters'][0]['prop-filters']) {
        $requirePostFilter = false;
    }
    if ($componentType === 'VEVENT' && isset(...['time-range']) && is_array(...)) {
        $timeRange = ...;
        if (!$filters['prop-filters'] && !$filters['comp-filters'][0]['comp-filters']
            && !$filters['comp-filters'][0]['prop-filters']
            && (!$timeRange['start'] || !$timeRange['end'])) {
            $requirePostFilter = false;
        }
    }
}
```

Consequences a faithful reimplementation must copy:
1. A bare `<comp-filter name="VEVENT"/>` (no sub-filters) is answered **from the
   `componenttype` column alone** — no parsing of the object at all.
2. A `time-range` with **only a `start` or only an `end`** is answered from the
   SQL alone. For a 33.0.5 row with `lastoccurence = firstoccurence` (timed
   VEVENT without `DTEND`), `lastoccurence > start` is then the only test.
3. A **second sibling comp-filter** is ignored by the shortcut *and* never
   reaches `$requirePostFilter` — a request with `VEVENT` (bare) + `VTODO`
   returns all VEVENTs. (Sabre would reject the conjunction as unsatisfiable.)
4. `time-range` on a non-`VEVENT` comp-filter (VTODO/VJOURNAL) is **never**
   pushed into SQL — `$timeRange` is only set when
   `$componentType === 'VEVENT'` — so it always costs a full scan + parse.

### 4.5 Risky vs mechanical — verdict

| case | verdict |
|---|---|
| bare `<comp-filter name="VEVENT"/>`, no time-range, no prop-filters | **mechanical** — pure `componenttype = 'VEVENT'` + `deleted_at IS NULL`, no parsing |
| `comp-filter` + `time-range` with **both** bounds, no other filters | **mechanical** if you also copy the `EventIterator` post-filter; **risky** if you trust SQL alone (false positives for recurring series whose window falls between occurrences) |
| `comp-filter` + `time-range` with **one** bound | **mechanical** (PHP itself trusts SQL) but only reproduces 33.0.5 if `firstoccurence`/`lastoccurence` are trusted, including their NULLs and the zero-length-window rows |
| `prop-filter` with `text-match` (the `calendar-object-uid` / CalDAV `UID` lookup, "find event by summary") | **risky** — needs the full Sabre VObject parse plus `StringUtil::textMatch` collation semantics (`i;ascii-casemap` default, `i;octet`) |
| `prop-filter` with `param-filter` (`ATTENDEE;CN=`) | **risky** — needs `Property::getParts()` splitting |
| `is-not-defined` | **risky** — requires parsing the object; SQL cannot help |
| `VALARM` time-range | **very risky / do not reimplement** — expands the parent recurrence with an ad-hoc early-exit heuristic; upstream calls it a hack |
| `expand` | **risky / delegate** — `VCalendar::expand` re-serialises, needs the calendar timezone, and the ETag no longer matches the body |
| `calendar+json` content type | **mechanical-ish** but the JSON shape must match `VObject::jsonSerialize()` exactly |
| `calendar-multiget` | **mechanical** — `getMultipleCalendarObjects` (100-URI chunks) + the same property set |
| `sync-collection` | **mechanical** — §2.10 is three simple queries with a `MAX(operation)` group-by |

**Recommendation for the time-range path:** reproduce exactly the
"SQL superset + Sabre post-filter" split, *including* the `$requirePostFilter`
shortcuts, and implement the post-filter as: (a) full VObject-equivalent parse
(quick-xml + an iCalendar model), (b) an `EventIterator`-equivalent recurrence
expander with the same `fastForward` semantics, (c) `StringUtil::textMatch`.
That is the single most expensive part of this work; if it is not worth it,
delegate **every** `calendar-query` that is not a bare `comp-filter` or a
two-bounded `VEVENT` time-range, and keep only those two mechanical cases
native.

---

## 5. PROPFIND property matrix

Observed on a live 33.0.5 instance (`PROPFIND` against
`/remote.php/dav/calendars/admin/` Depth:1 and
`/remote.php/dav/calendars/admin/personal/` Depth:1) and cross-checked against
the code. `404` = the server does not provide it on that node type.

### 5.1 `/calendars/<user>/<cal>/` (a `Calendar` node)

| property | status | handler → source of value |
|---|---|---|
| `{DAV:}resourcetype` | 200 | `CorePlugin::propFind` (`CorePlugin.php:746`) → `Server::getResourceTypeForNode` = `{DAV:}collection` + `{urn:…}calendar`; overridden by `CalDavBackend::addResourceTypeToCalendar` (`:4020`) to `{DAV:}collection` + `{http://nextcloud.com/ns}deleted-calendar` when `deleted_at` is set |
| `{DAV:}displayname` | 200 | `CalDavBackend::rowToCalendar` → `oc_calendars.displayname` (sharee suffix for shared: `<name> (<owner display name>)`) |
| `{http://apple.com/ns/ical/}calendar-color` | 200 | `rowToCalendar` → `calendarcolor` |
| `{http://apple.com/ns/ical/}calendar-order` | 200 | `rowToCalendar` → `calendarorder` (int) |
| `{urn:ietf:params:xml:ns:caldav}calendar-description` | 200 iff set | `rowToCalendar` → `description` |
| `{urn:ietf:params:xml:ns:caldav}calendar-timezone` | 200 iff set | `rowToCalendar` → `timezone` |
| `{urn:ietf:params:xml:ns:caldav}supported-calendar-component-set` | 200 | backend, from the `components` CSV (`:365`) — hard-coded `VEVENT` for calendars created by the Calendar app |
| `{urn:ietf:params:xml:ns:caldav}schedule-calendar-transp` | 200 | backend, from `transparent` (owned) / literal `transparent` (shared) |
| `{urn:ietf:params:xml:ns:caldav}max-resource-size` | 200 | `Sabre\CalDAV\Plugin::propFind` (`Plugin.php:328`) — `10000000` |
| `{urn:ietf:params:xml:ns:caldav}supported-calendar-data` | 200 | same (`:329-331`) |
| `{urn:ietf:params:xml:ns:caldav}supported-collation-set` | 200 | same (`:332-334`) |
| `{http://calendarserver.org/ns/}getctag` | 200 | backend string `http://sabre.io/ns/sync/<synctoken>` |
| `{http://sabredav.org/ns}sync-token` | 200 | backend raw token via `Sabre\CalDAV\Calendar::getProperties` + `CorePlugin::propFindNode` |
| `{DAV:}sync-token` | 200 | `Sabre\DAV\Sync\Plugin::propFind` (`Sync/Plugin.php:189`) — prefixes the raw token |
| `{http://owncloud.org/ns}owner-principal` | 200 | backend (`:367` owned / `:449` shared) |
| `{http://nextcloud.com/ns}owner-displayname` | 200 | `addOwnerPrincipalToCalendar` (`:4002`) → principal backend `{DAV:}displayname` |
| `{http://owncloud.org/ns}read-only` | 200 on **shared only**, 404 on owned | backend (`:450`), bool |
| `{http://owncloud.org/ns}public` | 200 on **public** (`access=4`) only | `getPublicCalendars`/`getPublicCalendar` (`:552`, `:617`) |
| `{http://nextcloud.com/ns}deleted-at` | 200 iff trashed | `rowToCalendar` → `deleted_at` (int; `Calendar::__construct` converts to ATOM for the trashbin node) |
| `{DAV:}invite` / `{http://owncloud.org/ns}invite` | 200 (empty when no shares) | `apps/dav/lib/DAV/Sharing/Plugin.php:220` → `Calendar::getShares()` → `CalDavBackend::getShares` |
| `{http://calendarserver.org/ns/}allowed-sharing-modes` | 200 | `apps/dav/lib/CalDAV/Publishing/PublishPlugin.php:125` → `AllowedSharingModes($canShare, $canPublish)`; `can-be-shared`/`can-be-published` are its children. Absent for the birthday calendar (its `canWrite()` is false) |
| `{http://calendarserver.org/ns/}publish-url` | 200 iff published | same plugin (`:115`) → `PublishPlugin`/`CalDavBackend::getPublishStatus` |
| `{DAV:}share-access` | **404** | Sabre's `DAV\Sharing\Plugin::propFind` (`3rdparty/.../DAV/Sharing/Plugin.php:141`) only fires for `ISharedNode`, and Sabre's `CalDAV\SharingPlugin` is **not registered** by Nextcloud's `Server.php` |
| `{DAV:}quota-used-bytes` / `{DAV:}quota-available-bytes` | **404** | `CorePlugin::propFind` only for `IQuota`; the `QuotaPlugin` in `Server.php:332` wraps the *files* view |
| `{DAV:}getlastmodified` | **404** | `Sabre\CalDAV\Calendar::getLastModified()` returns `null` (`CalDAV/Calendar.php:224`) |
| `{DAV:}supported-report-set` | 200 | `CorePlugin::propFind` aggregating every plugin's `getSupportedReportSet` — observed `{DAV:}sync-collection`, `{DAV:}expand-property`, `{DAV:}principal-match`, `{DAV:}principal-property-search`, `{DAV:}principal-search-property-set`, `{urn:…}calendar-multiget`, `{urn:…}calendar-query`, `{urn:…}free-busy-query`, `{http://owncloud.org/ns}filter-comments`, `{http://owncloud.org/ns}filter-files` |
| `{DAV:}supported-method-set` | 200 | `CorePlugin::propFind` — observed `OPTIONS GET HEAD DELETE PROPFIND PUT PROPPATCH COPY MOVE REPORT` |

### 5.2 `/calendars/<user>/<cal>/<obj>.ics` (a `CalendarObject`)

| property | status | handler → source |
|---|---|---|
| `{DAV:}getetag` | 200 | `CorePlugin::propFind` → `Sabre\CalDAV\CalendarObject::getETag` (`CalDAV/CalendarObject.php:140`) → `objectData['etag']` = `"<md5 of stored blob>"` |
| `{DAV:}getcontentlength` | 200 | → `getSize()` → `objectData['size']` (= `oc_calendarobjects.size`; **unset for shared calendars**, `CalendarObject.php:41`, so it then falls back to `strlen(get())`) |
| `{DAV:}getcontenttype` | 200 | → `getContentType()` → `text/calendar; charset=utf-8; component=<lowercased componenttype>` |
| `{DAV:}getlastmodified` | 200 | → `getLastModified()` → `objectData['lastmodified']`, rendered as an HTTP date |
| `{DAV:}resourcetype` | 200, **empty** | `Server::getResourceTypeForNode` → no types |
| `{urn:ietf:params:xml:ns:caldav}calendar-data` | 200 | `Sabre\CalDAV\Plugin::propFind` (`Plugin.php:405-413`) → `$node->get()`; **`OCA\DAV\CalDAV\CalendarObject::get()` post-processes**: for a shared read-only calendar it strips every `VALARM`, and for a shared/public `CONFIDENTIAL` object it masks the event (`createConfidentialObject`, keeping only `CREATED DTSTART RRULE RECURRENCE-ID RDATE DURATION DTEND CLASS EXRULE EXDATE UID` and `SUMMARY` → "Busy"). The stored blob and the wire body therefore **differ** in those cases, while the ETag stays the stored `md5`. |
| `{http://owncloud.org/ns}size`, `{DAV:}quota-*`, `{http://nextcloud.com/ns}deleted-at` | 404 | not provided on calendar objects |

### 5.3 `/calendars/<user>/` (the `CalendarHome`)

Observed 200: `{DAV:}resourcetype` = `{DAV:}collection`,
`{DAV:}current-user-principal`, `{DAV:}supported-report-set`,
`{DAV:}supported-method-set`. Everything else is 404 — in particular
`{urn:…}calendar-home-set`, `{urn:…}schedule-inbox-URL`,
`{urn:…}schedule-outbox-URL`, `{urn:…}schedule-default-calendar-URL`,
`{urn:…}calendar-user-address-set`, `{http://calendarserver.org/ns/}getctag`
and `{DAV:}sync-token` are **404 on the home** (the home is not an
`IPrincipal` and not an `ISyncCollection`). The home has no `getctag`.

### 5.4 The property-override hazard (`CustomPropertiesBackend`)

`apps/dav/lib/DAV/CustomPropertiesBackend.php:179-292` runs on **every**
authenticated request and, for paths under `calendars/` with exactly **two**
slashes (`calendars/<u>/<cal>`), it fetches these names from `oc_properties`
and `PropFind::set()`s them, **overwriting** whatever the calendar backend
returned (`:187-206`):

```
{DAV:}displayname
{urn:ietf:params:xml:ns:caldav}calendar-description
{urn:ietf:params:xml:ns:caldav}calendar-timezone
{http://apple.com/ns/ical/}calendar-order
{http://apple.com/ns/ical/}calendar-color
{urn:ietf:params:xml:ns:caldav}schedule-calendar-transp
{http://nextcloud.com/ns}disable-alarm-notifications
```

plus `{http://owncloud.org/ns}calendar-enabled` / `{http://owncloud.org/ns}enabled`
from `ALLOWED_NC_PROPERTIES` (`:80-83`), and
`{urn:…}calendar-availability` + `{urn:…}schedule-default-calendar-URL` as
*published read-only* properties visible to other users (`:94-97`, `:363`).
Individual `CalendarObject`s explicitly return early — "No custom properties
supported on individual events" (`:249-252`).

This is the mechanism by which a **sharee's** PROPPATCH of a shared calendar
(colour, order, displayname, transparency) survives: `Calendar::propPatch`
(`Calendar.php:238`) deliberately does *not* touch `oc_calendars` when the
calendar is shared, so the write lands in `oc_properties` keyed by
`calendars/<sharee>/<uri>`. **12 of the 17 production calendar-path property
rows are such sharee overrides.** A sidecar that serves calendar PROPFIND from
`oc_calendars` alone will disagree with PHP for those rows.

### 5.5 Principal properties (`/principals/users/<uid>/`)

Observed: `{urn:…}calendar-home-set` → `/remote.php/dav/calendars/<uid>/`
(`apps/dav/lib/CalDAV/Plugin.php:30-45`, `Sabre CalDAV\Plugin::propFind`
`Plugin.php:339`); `{urn:…}schedule-inbox-URL` →
`/…/calendars/<uid>/inbox/`; `{urn:…}schedule-outbox-URL` →
`/…/calendars/<uid>/outbox/`; `{urn:…}schedule-default-calendar-URL` →
`/…/calendars/<uid>/personal/` (from `OCA\DAV\CalDAV\Schedule\Plugin`
`propFindDefaultCalendarUrl`, `Schedule/Plugin.php:387`); `{urn:…}calendar-user-address-set`
→ the principal href (+ any `mailto:` from `getAlternateUriSet`); `{urn:…}calendar-user-type`
→ `INDIVIDUAL`; `{DAV:}displayname`, `{DAV:}principal-URL`,
`{DAV:}current-user-principal`; `{http://calendarserver.org/ns/}calendar-availability`
404 unless stored in `oc_properties`.

---

## 6. Sharing, subscriptions, public calendars, trashbin, home set

* **Shared calendars** appear in the same `CalendarHome` collection as owned
  ones, named `<uri>_shared_by_<owner-uid>` (`CalDavBackend.php:438`), with
  `principaluri` = the sharee, `owner-principal` = the real owner,
  `read-only` set (bool) and `schedule-calendar-transp` forced to
  `transparent`. `CalendarHome::getChild` resolves them through the
  `getCalendarsForUser` scan (`CalendarHome.php:174-179`) — a full re-list per
  request. Group and circle shares are folded in via
  `getGroupMembership`/`getCircleMembership`; unsharing is the `access = 5`
  tombstone (see `shared-addressbooks.md` §0) and the CalDAV query excludes by
  **`resourceid`** (the CardDAV twin excludes by `s.id` — see the note in
  `shared-addressbooks.md`).
* **Subscriptions** (`oc_calendarsubscriptions`) are children of the same home,
  after the calendars, ordered by `calendarorder`. With the WebcalCaching
  plugin enabled for the request (`X-NC-CalDAV-Webcal-Caching: On`, a known
  client UA, or a `?export` GET — `WebcalCaching/Plugin.php:52-67`) they are
  served as `CachedSubscription`, which reads objects from
  `oc_calendarobjects` with `calendartype = 1`; otherwise as plain
  `Subscription` (a read-only node with no children).
* **Public calendars** live under `/remote.php/dav/public-calendars/<token>/`
  (`PublicCalendarRoot`, `getPublicCalendars`/`getPublicCalendar`,
  `:516`/`:574`), selected by `oc_dav_shares.access = 4` and `publicuri`. They
  carry `{http://owncloud.org/ns}public` and `read-only` and are owned by
  `principals/system/public`.
* **Trashbin**: `/calendars/<u>/trashbin/` with children `restore/` and
  `objects/` (`TrashbinHome.php:74-99`). `trashbin/` carries
  `{DAV:}resourcetype` = `{DAV:}collection` + `{http://nextcloud.com/ns}trash-bin`
  and the read-only property `{http://nextcloud.com/ns}retention-duration`
  (`Trashbin/Plugin.php:116-120`). Trashed **calendars** (`oc_calendars.deleted_at`)
  are *not* in the trashbin collection — they stay in the calendar home with the
  `{http://nextcloud.com/ns}deleted-calendar` resourcetype and
  `{http://nextcloud.com/ns}deleted-at`. Trashed **objects** are listed by
  `getDeletedCalendarObjectsByPrincipal` (§2.13) and each carries
  `{http://nextcloud.com/ns}deleted-at` (ATOM), `…calendar-uri`,
  `…calendar-owner-principal-uri` and, for proxy-delegated entries,
  `…delegator` (`Trashbin/Plugin.php:87-121`).
* **`inbox`** is the CalDAV scheduling inbox — children are
  `oc_schedulingobjects` rows for the principal (`getSchedulingObjects`,
  `:3216`), each with `getetag`/`getcontentlength`/`getlastmodified` and
  `calendar-data`. **`outbox`** is a POST-only node (iTIP `REQUEST`/`REPLY`),
  its `resourcetype` is `{DAV:}collection` + `{urn:…}schedule-outbox`. Both are
  read-visible on PROPFIND. `notifications` is **not present** (§0).
* **`calendar-home-set`** is reported only on the **principal** node, as
  `<d:href>/remote.php/dav/calendars/<uid>/</d:href>` — computed by
  `OCA\DAV\CalDAV\Plugin::getCalendarHomeForPrincipal`
  (`apps/dav/lib/CalDAV/Plugin.php:30-45`); `calendar-resources` and
  `calendar-rooms` map to `system-calendars/…` instead.

---

## 7. Sabre plugins that can change a read response

| plugin | read-visible on `/calendars/…`? | effect |
|---|---|---|
| `ICSExportPlugin` (Nextcloud subclass) | **yes**, on `GET …?export` | `httpGet` (`3rdparty/.../CalDAV/ICSExportPlugin.php:80`) intercepts any GET whose query has `export`; requires `resourcetype` to contain `{urn:…}calendar`. Merges every object into one VCALENDAR with `X-WR-CALNAME`, `X-APPLE-CALENDAR-COLOR`, `REFRESH-INTERVAL;VALUE=DURATION` + `X-PUBLISHED-TTL` (Nextcloud default `PT4H`, `ICSExportPlugin/ICSExportPlugin.php:26`, `:36-41`), `Content-Disposition: attachment; filename="<uri>-<date>.ics"`. Supports `start=`, `end=`, `expand=1` (requires both), `componentType=`, `accept=jcal`. **Not a PROPFIND/REPORT path** — a sidecar can answer 501 and let nginx replay. |
| `OCA\DAV\DAV\Sharing\Plugin` (Nextcloud's) | yes | `propFind` (`:220`) adds `{http://owncloud.org/ns}invite`; `preloadCollection` (`:192`) batches `getShares` for a Depth:1 home PROPFIND. Also handles the `POST` share/unshare. |
| `Sabre\CalDAV\SharingPlugin` | **not registered** | would have added `{http://calendarserver.org/ns/}invite` + `allowed-sharing-modes`; Nextcloud uses its own plugin and `PublishPlugin` instead. |
| `Sabre\DAV\Sharing\Plugin` | only for `ISharedNode` | `{DAV:}share-access`, `{DAV:}invite`, `{DAV:}share-resource-uri`. Nextcloud's `Calendar` is **not** an `ISharedNode` → all three are 404 on calendars. |
| `PublishPlugin` (`CalDAV/Publishing`) | yes | `propFind` (`:99-141`) adds `{http://calendarserver.org/ns/}publish-url` (only when published) and `…allowed-sharing-modes`; at Depth:1 on the home it preloads publish statuses (`preloadPublishStatuses`). |
| `Trashbin\Plugin` | yes | `propFind` (`:87-121`) adds `{http://nextcloud.com/ns}deleted-at`, `…calendar-uri`, `…source-calendar-uri`, `…calendar-owner-principal-uri`, `…delegator`, `…retention-duration`; `beforeMethod` (`:56`) can disable the trashbin for a calendar. |
| `Sabre\CalDAV\Subscriptions\Plugin` | yes | enables `MKCALENDAR`-style subscription creation via POST; does not alter PROPFIND output itself (subscriptions come from `getSubscriptionsForUser`). |
| `Sabre\CalDAV\Notifications\Plugin` | yes (collection), but the home has **no** `notifications` child | would serve `{urn:…}notification` collections if the backend implemented `NotificationSupport`; it does not. |
| `Sabre\CalDAV\Schedule\Plugin` | yes, on **principals** | `schedule-outbox-URL`, `schedule-inbox-URL`, `schedule-default-calendar-URL`, `calendar-user-type`, `calendar-availability` (`3rdparty/.../CalDAV/Schedule/Plugin.php:195-296`); `free-busy-query` REPORT. |
| `OCA\DAV\CalDAV\Schedule\Plugin` | yes, on principals + calendar objects | `propFindDefaultCalendarUrl` (`:387`) adds `schedule-default-calendar-URL` (from `oc_properties`, with a fallback to the `personal` calendar); `propFind` adds `calendar-user-type` (`:118`); also `calendar-object-uid` lookup + iTIP broker. |
| `IMipPlugin` | no (write-side) | sends iTIP mail on scheduling; registered only for authenticated requests (`Server.php:354`). |
| `OCA\DAV\CalDAV\Search\SearchPlugin` | yes, on the **home** | `{http://nextcloud.com/ns}calendar-search` REPORT (`:81`), advertised in the home's `supported-report-set`; uses `oc_calendarobjects_props` via `CalDavBackend::calendarSearch`/`search`. |
| `CustomPropertiesBackend` (PropertyStorage) | **yes** | §5.4 — overrides calendar properties from `oc_properties`. |
| `WebcalCaching\Plugin` | indirectly | `beforeMethod` (`:88`) flips subscriptions to their cached form for specific clients / `?export`. |
| `Availability` | **not a plugin here** | `{http://calendarserver.org/ns/}calendar-availability` is handled by `Sabre\CalDAV\Schedule\Plugin::propFind` (`:268`) and stored in `oc_properties`; there is no `Availability` plugin in this tree. |
| `DefaultCalendar` | **not a plugin** | the default-calendar logic lives in `OCA\DAV\CalDAV\Schedule\Plugin::propFindDefaultCalendarUrl` + `CalDAV/DefaultCalendarValidator.php` (validation only). |
| `Proxy` | **not a plugin** | calendar-proxy read/write is expressed purely through ACLs (`Calendar::getACL`, `Calendar.php:112-235`) and `CalDAV/Proxy/ProxyMapper.php`; there is no Sabre proxy plugin. |
| `AppleQuirksPlugin` | yes but only for `REPORT` | `apps/dav/lib/Connector/Sabre/AppleQuirksPlugin.php` rewrites `{DAV:}principal-property-search` for macOS agents. No calendar property aliases exist in this tree — `calendar-color`/`calendar-order` are the real iCal namespace properties, not aliases. |

---

## 8. What a sidecar can reproduce exactly / what it should delegate

### Can be reproduced exactly (byte-identical, low risk)

1. **Calendar-home Depth:1 PROPFIND** (`getCalendarsForUser` + the fixed
   property set). Two SQL statements plus one principal-displayname lookup per
   calendar; no VObject parsing. Requires: `convertPrincipal`, the
   `<uri>_shared_by_<owner>` naming, the read-only/de-dup rules, the `oc_properties`
   override layer (§5.4), and the extra home children
   (`inbox`, `outbox`, `trashbin`) which are cheap to synthesize.
2. **Calendar Depth:0 PROPFIND** (the property set above for one calendar).
3. **Calendar Depth:1 PROPFIND** (`getCalendarObjects` + per-object property
   set) — but note the object's `calendar-data` post-processing for shared
   calendars (VALARM stripping, CONFIDENTIAL masking) and that `size` is
   suppressed for shared calendars.
4. **`sync-collection` REPORT** — three simple queries; the only subtleties are
   the pre-increment token convention, `MAX(operation)` per uri, the
   `sync-token`/`getctag` prefixes, and
   `UnsupportedLimitOnInitialSyncException`.
5. **`calendar-multiget` REPORT** — `getMultipleCalendarObjects` with 100-URI
   chunks and the same property set.
6. **Object GET / PROPFIND `calendar-data`** for **owned** calendars (raw blob
   passthrough; keep the CRLF bytes and the ETag derived from the stored bytes).
7. **Bare `calendar-query`** (`<comp-filter name="VEVENT"/>` with nothing else)
   and **two-bounded `VEVENT` time-range** queries, if you also implement the
   `EventIterator` post-filter.

### Should be delegated (501 → nginx replays to PHP)

* Any `calendar-query` with `prop-filter`, `param-filter`, `is-not-defined`,
  `VALARM` time-range, or a second comp-filter — the `$requirePostFilter`
  shortcuts make the semantics path-dependent and the Sabre validator is the
  only correct oracle.
* Any `calendar-query`/`multiget` with `expand` or `application/calendar+json`.
* `free-busy-query`, `{http://nextcloud.com/ns}calendar-search`.
* `GET …?export`.
* `POST` on `outbox`, `publish`, share/unshare, PROPPATCH of calendar
  properties that must land in `oc_properties`.
* Anything under `public-calendars/`, `system-calendars/`, `remote-calendars/`,
  `trashbin/` for now (each has its own node classes and property sets).

### Top 2–3 read paths by cost, in priority order

1. **Calendar-home Depth:1 PROPFIND** (`/calendars/<user>/`). This is the
   request every CalDAV client issues on every sync, and PHP currently pays
   *two* full calendar queries + N principal lookups + N `oc_properties`
   lookups + the whole Sabre node graph. It is also the cheapest to reproduce
   correctly (pure SQL + string formatting, no VObject). **Do this first.**
2. **`sync-collection` REPORT on a calendar.** It is what a synced client runs
   on every poll; it is three tiny indexed queries and zero parsing, and the
   token arithmetic is fully specified in §2.10/§3.4. High value, low risk.
3. **`calendar-query` restricted to the two mechanical shapes** (bare
   `comp-filter`, and two-bounded `VEVENT` time-range with no other filters).
   This is the actual "give me my events in this window" request. It needs the
   `EventIterator` post-filter to be *correct*; if that is too much for the
   first iteration, ship the bare-`comp-filter` case only (which is a pure
   `componenttype` filter) and delegate the time-range case.

**Cheapest lever overall:** the home listing + sync-collection pair removes the
per-request PHP bootstrap (auth, plugin stack, Sabre tree) that dominates these
small queries; the SQL itself is already trivial.

---

## Appendix — evidence and repro commands

* Method-by-method diff of `CalDavBackend.php` 33.0.5 vs 36-dev:
  `docker cp ncdav-e2e-nc:/var/www/html/apps/dav/lib/CalDAV/CalDavBackend.php`
  (the local e2e stack runs the same `nextcloud:33.0.5-apache` image as
  production), then a brace-matched `function` extraction + `difflib`.
* Live PROPFIND/REPORT captures (local 33.0.5 stack, port 18081, throwaway
  admin credential written to `/tmp/calrecon/admin.curlrc` mode 0600, outside
  the repo): home Depth:1, calendar Depth:0/1, principal Depth:0,
  `calendar-query`, `sync-collection`, `trashbin` Depth:1, `?export`.
  Outputs in `/tmp/calrecon/` (`admin-home.xml`, `cq.xml`, `sync.xml`,
  `cal-d1.xml`, `princ.xml`, `tb.xml`, `exp.ics`).
* Schema and indexes: two `-tAc` queries against the production cluster
  (`information_schema.columns`, `pg_indexes`) returning column metadata only,
  plus one aggregate `count(*)` row.
* ETag/CRLF check: `md5(convert_from(calendardata,'UTF8'))` in the DB vs `md5`
  of the `calendar-data` text extracted from the REPORT response, plus
  `position(E'\r' in convert_from(calendardata,'UTF8'))` = 16.
* `oc_properties` override check: one aggregate query classifying the 17
  `calendars/%` rows by whether the path user owns the matching calendar
  (12 non-owning ⇒ sharee overrides).
* No mutation was performed; no secret was printed; no process listing was run
  inside the `nextcloud-dav` sidecar container.
