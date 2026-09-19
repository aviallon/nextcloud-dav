# CalDAV traffic & client property sets — recon for a native CalDAV read path

Scope: what CalDAV traffic the production instance actually generates, what the
real clients ask for, and which read paths a Rust sidecar should implement.
Read-only investigation; no deploy, no credentials printed.

Instance: `nextcloud-prod-5f5b5746d9-xfld7`, Nextcloud 33.0.5 / SabreDAV 4.7.
Window: **72 h** (`kubectl logs <pod> -c nextcloud-nginx --since=72h`,
2026-09-16 ~09:50 → 2026-09-19 ~09:50 UTC), 21,700 log lines of which 20,279 are
access-log lines and 1,421 are nginx error-log lines (startup + proxy-buffer
warnings). Raw copy analysed at `/tmp/nginx72h.log`.

Companion docs: `ARCHITECTURE.md` (as-built sidecar), `dav-bench/PRODUCTION.md`
(measured PHP cost), `dav-bench/RESULTS.md`, `files-propfind-model.md`,
`card-events.md`.

---

## 1. The log format, and what it cannot answer

`/etc/nginx/nginx.conf` in the nginx container, lines 18–20:

```
log_format  main  '$remote_addr - $remote_user [$time_local] "$request" '
                  '$status $body_bytes_sent "$http_referer" '
                  '"$http_user_agent" "$http_x_forwarded_for"';
```

php-fpm's own access log (the `nextcloud` container's stdout) is even poorer:
`"GET /status.php" 200` — request + status only.

**Therefore, from the access log alone we cannot determine:**

| not available | consequence |
|---|---|
| `$request_time` / `$upstream_response_time` | **no per-request duration.** §3 gives measured anchors instead of a fabricated distribution. |
| `$request_body` | **REPORT type is not directly observable.** A 272-byte 207 on a calendar collection could be `sync-collection` (no changes), `calendar-query` (no matches) or a `calendar-multiget` for one small event. Type must be inferred from client + response size + path shape. |
| `Depth` header | Depth 0 vs Depth 1 PROPFIND is not visible. |
| request id / correlation id | a PROPFIND cannot be paired with the REPORT/multiget it triggers. |
| HTTP/2 vs HTTP/1.1, auth scheme | not visible. |
| `$upstream_addr` (only in error lines) | whether a 207 came from Rust or PHP is **not** in the access log. Routing is known from the nginx config instead (§2). |

Also: `$body_bytes_sent` is the *compressed* body if gzip applied (gzip is on for
`text/xml`/`application/xml` in this config), so sizes are a lower bound on the
serialised multistatus.

---

## 2. What nginx routes today (from the live config)

`kubectl exec -c nextcloud-nginx -- grep location /etc/nginx/conf.d/default.conf`:

| location | upstream |
|---|---|
| `~ ^/remote\.php/dav/$` | **sidecar** (Rust) — root discovery |
| `~ ^/remote\.php/dav/principals/users/[^/]+/?$` | **sidecar** — own principal |
| `~ ^/remote\.php/dav/addressbooks/users/[^/]+/.` | **sidecar** |
| `~ ^/remote\.php/dav/files/` | **sidecar** |
| everything else, incl. `/remote.php/dav/calendars/…` | php-fpm |

So in the tables below, **principal PROPFIND and root-discovery PROPFIND are
already served by Rust**; every `/calendars/…` request currently reaches PHP.
(`error_page 501 502 504 = @nextcloud_dav_php` replays anything the sidecar
declines.)

---

## 3. CalDAV traffic (72 h)

### 3.1 Calendars + principals, method × path shape × status

`<user>`, `<cal>`, `<object>` collapsed. p50/p90/max are **`$body_bytes_sent`**.
Duration is unavailable (§1) — see §3.4 for the measured anchor.

| method | shape | status | n | p50 B | p90 B | max B | Σ bytes |
|---|---|---|---:|---:|---:|---:|---:|
| PROPFIND | per-calendar `<user>/<cal>/` | 207 | **278** | 452 | 509 | 596 | 115,418 |
| REPORT | per-calendar `<user>/<cal>/` | 207 | **272** | 325 | 327 | 197,636 | 1,358,653 |
| PROPFIND | principal `principals/users/<u>/` | 207 | **264** | 297 | 664 | 3,367 | 125,579 |
| PUT | object `<user>/<cal>/<obj>.ics` | 403 | 126 | 180 | 180 | 180 | 22,680 |
| PROPFIND | calendar-home `<user>/` | 207 | **93** | 1338 | 1338 | 1338 | 124,423 |
| PROPFIND | principal (unauthenticated retry) | 401 | 59 | 12 | 12 | 615 | 3,120 |
| OPTIONS | calendar-home | 200 | 6 | 0 | 0 | 0 | 0 |
| PROPFIND | per-calendar | 401 | 5 | 615 | 615 | 615 | 3,075 |
| OPTIONS | calendar-home | 401 | 3 | 615 | 615 | 615 | 1,845 |
| PUT | object | 201 | 3 | 0 | 0 | 0 | 0 |
| POST | per-calendar (outbox/free-busy) | 201 | 2 | 314 | 314 | 314 | 628 |
| PUT | object | 401 | 1 | 615 | 615 | 615 | 615 |
| POST | per-calendar | 499 | 1 | 0 | 0 | 0 | 0 |

CalDAV-adjacent, also already native:

| method | path | status | n | p50 B | p90 B | max B |
|---|---|---|---:|---:|---:|---:|
| PROPFIND | `/remote.php/dav/` (root discovery) | 207 | **909** | 243 | 312 | 2,389 |
| PROPFIND | `/remote.php/dav/` (unauth) | 401 | 131 | 12 | 12 | 615 |

No traffic at all on `/remote.php/dav/system-calendars/` or
`/public-calendars/`. **No `GET` of a `.ics` object** (0 in 72 h — every event
read goes through a REPORT). No `DELETE`, no `MKCALENDAR`, no `PROPPATCH` on
calendars.

### 3.2 What the REPORTs are (inferred, since the body is not logged)

REPORT response-size buckets, all 272 on per-calendar collections:

| bucket | n | reading |
|---|---:|---|
| < 400 B | **250** | empty result: `sync-collection` with nothing changed, or `calendar-query` with no match |
| 400 B – 2 kB | 9 | a handful of changed members (etags only) |
| 2 – 20 kB | 4 | small multiget / short time-range query |
| 20 – 100 kB | 4 | partial calendar dump |
| > 100 kB | 5 | full calendar dump |

REPORT by user-agent:

| UA | n |
|---|---:|
| `Mozilla/5.0 (X11; Linux x86_64; rv:154.0) … Thunderbird/154.0` | **254** |
| `DAVx5/4.5.19-ose (at.bitfire.davdroid)` | 7 |
| `KDE DAV groupware client` | 6 |
| `DAVx5/4.5.20-beta.1-gplay (at.bitfire.davdroid)` | 5 |

**No browser (Firefox/Chrome) REPORTs** — the Nextcloud *web* calendar app is
not in this window's traffic. The 5 >100 kB responses are the initial dumps for
`selene-et-antoine` (DAVx5 ×2, KDE ×2) and its shared copy (DAVx5 ×1).

Per-calendar REPORT distribution: `selene-et-antoine` 54, `personal` 44,
`contact_birthdays` 43, `famille` 42, `professional` 42, `rappels` 42,
`selene-et-antoine_shared_by_aviallon` 5.

### 3.3 Other shapes

* **Per-calendar PROPFIND** (278): DAVx5 272 (its Depth-0 capability probe, one
  per calendar per sync cycle), Thunderbird 11 (5 of the 283 were 401).
* **Calendar-home PROPFIND** (93): all KDE, all exactly **1338 B** — a fixed
  Depth 0/1 probe every ~45 min. Depth is not in the log.
* **Principal PROPFIND** (320 incl. 59 × 401): KDE 294, `curl/8.20.0` 18
  (monitoring), Thunderbird 8. The KDE pattern is: 2 × PROPFIND on
  `/remote.php/dav/` (one 401, one 207) + 1 × principal + 1 × calendar-home,
  repeated on a timer.
* **PUT 403 × 126**: Thunderbird repeatedly writing
  `contact_birthdays/contacts-*.vcf.ics` — the server-generated birthday
  calendar is read-only. A client bug, not a read path; it is 127 PUTs/72 h of
  pure waste. All on `contact_birthdays`.

### 3.4 Duration — what is known instead of a distribution

Not in any log (§1). The one directly measured CalDAV number on this instance
is `dav-bench/PRODUCTION.md` (2026-09-17, temporary app password, direct
port-forward):

| request | time |
|---|---|
| `PROPFIND /remote.php/dav/calendars/aviallon/` Depth 0 (PHP) | **1.42–1.52 s** |
| unauthenticated `PROPFIND`/`OPTIONS` → 401 (PHP) | 0.27–0.46 s |
| `/remote.php/dav/` Depth 0 authed (PHP) | 1.46 s |
| `/remote.php/dav/principals/users/aviallon/` Depth 0 (PHP) | 1.54 s |
| `status.php` (no app boot) | 0.08–0.10 s |

The cost is ~1.4 s **fixed** per authenticated PHP request (bootstrap + auth +
session listeners), plus per-child work; `dav_server_exec` dominates. CardDAV
sidecar p90s from `dav-bench/BENCHMARKS.md` for scale: PROPFIND Depth 1 749 ms,
GET 407 ms, PUT 1041 ms, DELETE 736 ms — and those numbers are dominated by
Postgres/Ceph, not by the sidecar.

So the honest duration model used below is **n_req × ~1.45 s** for the PHP
baseline, explicitly flagged as a model, not a measurement.

---

## 4. Ranking — where CalDAV time goes

Authenticated (207) requests only; 907 requests; `est.` = n × 1.45 s.

| rank | method + shape | n | est. PHP time | share | Σ bytes | byte share |
|---:|---|---:|---:|---:|---:|---:|
| 1 | PROPFIND per-calendar | 278 | 403 s | **30.7 %** | 115 kB | 6.7 % |
| 2 | REPORT per-calendar | 272 | 394 s | **30.0 %** | 1,359 kB | 78.8 % |
| — | *PROPFIND principal — already native* | 264 | 383 s | *29.1 %* | 126 kB | 7.3 % |
| 3 | PROPFIND calendar-home | 93 | 135 s | **10.3 %** | 124 kB | 7.2 % |

Plus 909 root-discovery PROPFINDs (already native) — CalDAV discovery is
therefore *already* covered by the sidecar, including
`{urn:ietf:params:xml:ns:caldav}calendar-home-set`
(`src/discovery.rs:130,366`, `is_implemented` list).

**The top 3 unimplemented read paths are, unambiguously: per-calendar PROPFIND,
per-calendar REPORT, calendar-home PROPFIND** — together ~71 % of CalDAV
request time and 93 % of CalDAV bytes, and only ~2.4 KB/req of request surface.

---

## 5. Property sets and REPORT bodies the real clients send

Fetched 2026-09-19 (AGPL/MPL sources, exact revisions recorded):

* Nextcloud calendar app **6.7.0-dev.1** `nextcloud/calendar@14b81b8`
* `@nextcloud/cdav-library` **2.8.0** `nextcloud/cdav-library@a58c14d` — this is
  where the web UI's PROPFIND/REPORT bodies actually live (the calendar app
  only calls `calendar.dav.*`)
* `bitfireAT/dav4jvm@0980f6b` (DAVx5's DAV client library)
* `bitfireAT/davx5-ose@481868a`
* Thunderbird 154 `mozilla/releases-comm-central` `CalDavRequestHandlers.sys.mjs`

### 5.1 Nextcloud web calendar app

| | detail | source |
|---|---|---|
| Collection PROPFIND props | `getcontenttype`, `getetag`, `resourcetype` (`davObject.js:290`) + `displayname`, `owner`, `resourcetype`, `sync-token`, `current-user-privilege-set` (`davCollection.js:459`) + `apple:calendar-order`, `apple:calendar-color`, `cs:getctag`, `caldav:calendar-description`, `caldav:calendar-timezone`, `caldav:supported-calendar-component-set`, `caldav:supported-calendar-data`, `caldav:max-resource-size`, `caldav:min-date-time`, `caldav:max-date-time`, `caldav:max-instances`, `caldav:max-attendees-per-instance`, `caldav:supported-collation-set`, `caldav:calendar-free-busy-set`, `caldav:schedule-calendar-transp`, `caldav:schedule-default-calendar-URL`, `oc:calendar-enabled`, `nc:default-alarm-part-day`, `nc:default-alarm-full-day`, `nc:disable-alarm-notifications`, `nc:owner-displayname`, `nc:trash-bin-retention-duration`, `nc:deleted-at` (`calendar.js:264`) + `cs:publish-url` (`davCollectionPublishable.js`) + `oc:invite`, `cs:allowed-sharing-modes` (`davCollectionShareable.js`) | cdav-library |
| Home Depth 1 props | union of every registered collection factory: the above **plus** `cs:source`, `apple:refreshrate`, `cs:subscribed-strip-*` (subscription), `caldav:calendar-availability` (inbox), and the deleted-calendar props | `calendarHome.js:37-43`, `davCollection.js:320` |
| REPORT props | the same list **plus** `caldav:calendar-data` (registered by the `VObject` object factory) | `vobject.js:34` |
| `calendar-query` body | `<cal:calendar-query><d:prop>…</d:prop><cal:filter><cal:comp-filter name="VCALENDAR"><cal:comp-filter name="VEVENT"><cal:time-range start="…Z" end="…Z"/></cal:comp-filter></cal:comp-filter></cal:filter></cal:calendar-query>`, `Depth: 1`. No `expand`, no `limit-*`. `findByType("VTODO")` sends the same without `<time-range>`. | `calendar.js:99-187` |
| `calendar-multiget` body | `<cal:calendar-multiget><d:prop>…</d:prop><d:href>…</d:href>+</cal:calendar-multiget>`, `Depth: 1` | `calendar.js:193-232` |
| `sync-collection` | **not used.** The app reads `{DAV:}sync-token` as a change *hint* and refetches with `calendar-query` when it moves | `davCollection.js:58`, app `src/store/calendars.js:1044-1088` |
| app-level calls | `findByTypeInTimeRange('VEVENT', from, to)` and `findByType('VTODO')` | app `src/store/calendars.js:769,772` |
| discovery | principal PROPFIND with `calendar-home-set`, `calendar-user-address-set`, `calendar-user-type`, `principal-URL`, `alternate-URI-set`, `sabredav:email-address`, `nc:language`, `schedule-inbox-URL`, `schedule-outbox-URL`, `schedule-default-calendar-URL`, `resource-*`/`room-*` (booking), plus `{DAV:}principal-collection-set` and `{DAV:}supported-report-set` | `principal.js:getPropFindList`, `index.js:117-140` |

### 5.2 DAVx5 / DAVdroid (Android)

| request | body / props | source |
|---|---|---|
| capability PROPFIND `Depth: 0` | `{DAV:}supported-report-set`, `{DAV:}sync-token`, `{cs}getctag`, `{caldav}max-resource-size`, `{carddav}max-resource-size`, `{carddav}supported-address-data` | davx5 `BaseWebDavCollection.kt:67-80` |
| `sync-collection` REPORT | `<D:sync-collection><D:sync-token>…</D:sync-token><D:sync-level>1</D:sync-level><D:prop><D:getetag/><D:resourcetype/></D:prop></D:sync-collection>`, `Depth: 0` | dav4jvm `DavCollection.kt:reportChanges`, davx5 `BaseWebDavCollection.kt:100-106` |
| `calendar-query` REPORT | `<CAL:calendar-query><D:prop><D:getetag/></D:prop><CAL:filter><CAL:comp-filter name="VCALENDAR"><CAL:comp-filter name="VEVENT"><CAL:time-range …/></CAL:comp-filter></CAL:comp-filter></CAL:filter></CAL:calendar-query>`, `Depth: 1`; one REPORT per component | dav4jvm `DavCalendar.kt:calendarQuery`, davx5 `CalDavCollection.kt:39-51` |
| `calendar-multiget` REPORT | `<D:prop><D:getcontenttype/><D:getetag/><CAL:schedule-tag/><CAL:calendar-data/></D:prop>` + hrefs (optional `content-type`/`version` attributes on `calendar-data`) | dav4jvm `DavCalendar.kt:multiget` |
| algorithm choice | `sync-collection` unless the user set `timeRangePastDays` **or** the server does not advertise `{DAV:}sync-collection` | davx5 `CalendarSyncManager.kt:87-91` |

**Consequence for the sidecar:** DAVx5 only takes the cheap `sync-collection`
path if the per-calendar PROPFIND advertises `{DAV:}sync-collection` in
`{DAV:}supported-report-set`. If the sidecar serves that PROPFIND it must
advertise sync-collection truthfully, or DAVx5 falls back to a full
`calendar-query`.

### 5.3 Thunderbird / Lightning 154

| request | body | source |
|---|---|---|
| `sync-collection` REPORT | `<sync-collection xmlns="DAV:"><sync-token/>|<sync-token>…</sync-token><sync-level>1</sync-level><prop><getcontenttype/><getetag/></prop></sync-collection>` **and** `Depth: 1` (belt-and-braces for old servers) | `CalDavRequestHandlers.sys.mjs:445-470` |
| `calendar-multiget` REPORT | `<C:calendar-multiget><D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>…</D:href>×≤100</C:calendar-multiget>`, `Depth: 1`; batch size `calendar.caldav.multigetBatchSize` (default 100) | `CalDavRequestHandlers.sys.mjs:866-895` |
| `calendar-query` | **not used** — Thunderbird 154 does sync-collection (empty token = initial full sync) + multiget | grep of the handler file |

### 5.4 iOS / macOS Calendar

**No Apple client appears in this window's traffic** (UA scan: KDE, DAVx5,
Thunderbird, curl only). The property set below is from the specifications, not
from a fetched source:

| request | expectation | citation |
|---|---|---|
| principal PROPFIND | `calendar-home-set`, `calendar-user-address-set`, `calendar-user-type`, `schedule-inbox-URL`, `schedule-outbox-URL`, `schedule-default-calendar-URL`, `{DAV:}current-user-principal` | RFC 4791 §6.2.1, RFC 6638 §6.2 |
| per-calendar PROPFIND | `getctag`, `calendar-color`/`calendar-order` (Apple namespace `http://apple.com/ns/ical/`), `supported-calendar-component-set`, `calendar-timezone`, `calendar-description`, `{DAV:}resourcetype` = `<C:calendar/>` | RFC 4791 §5.2.3; Apple `calendar-color`/`calendar-order` |
| `calendar-query` REPORT | `comp-filter name="VCALENDAR"` → `comp-filter name="VEVENT"` → `time-range`, often with `<C:calendar-data><C:expand start end/></C:calendar-data>` | RFC 4791 §7.8, §9.6.1 |
| `calendar-multiget` REPORT | `getetag` + `calendar-data`, hrefs | RFC 4791 §7.9 |
| `sync-collection` | supported (RFC 6578); Apple uses ctag + calendar-query by default | RFC 6578 |

Practical note: `<C:expand>` is the one CalDAV feature that forces real
recurrence expansion. It is the reason to keep `calendar-query` on PHP until the
sidecar has an iCalendar expansion engine.

### 5.5 Server-side inventory (what PHP answers with)

| property / feature | value / source |
|---|---|
| calendar props from `oc_calendars` | `propertyMap` = `{DAV:}displayname`, `caldav:calendar-description`, `caldav:calendar-timezone`, `apple:calendar-order`, `apple:calendar-color`, `nc:deleted-at`, `nc:default-alarm-part-day`, `nc:default-alarm-full-day`, `nc:disable-alarm-notifications` — `apps/dav/lib/CalDAV/CalDavBackend.php:147-156` |
| computed calendar props | `{cs}getctag` = `http://sabre.io/ns/sync/<synctoken>`, `{http://sabredav.org/ns}sync-token` = `<synctoken>`, `caldav:supported-calendar-component-set`, `caldav:schedule-calendar-transp`, `oc:owner-principal`, `nc:owner-displayname`, `oc:read-only` for shares — `CalDavBackend.php:357-366`, `:4003-4018` |
| **getctag differs from CardDAV** | calendar `getctag` is the **sabre sync URL**; addressbook `getctag` is the **raw int** (`CardDavBackend.php:124` vs `CalDavBackend.php:363`) |
| `{DAV:}supported-report-set` | on a calendar collection: `calendar-multiget`, `calendar-query`, `free-busy-query`; on the calendar **home** additionally `{DAV:}sync-collection` (Sabre requires it on the home for iCal) — `3rdparty/sabre/dav/lib/CalDAV/Plugin.php:162-180` |
| `{DAV:}sync-token` wire format | `http://sabre.io/ns/sync/<int>` — `3rdparty/sabre/dav/lib/DAV/Sync/Plugin.php:32` |
| per-user custom props (`oc_properties`) | `oc:calendar-enabled` (default `1`), `oc:enabled`, `caldav:calendar-availability`, `caldav:schedule-default-calendar-URL` — `apps/dav/lib/DAV/CustomPropertiesBackend.php:88-104` |
| trash retention | `{http://nextcloud.com/ns}trash-bin-retention-duration` — `apps/dav/lib/CalDAV/Trashbin/Plugin.php:33` |
| calendar home children | calendars + shared calendars (`<uri>_shared_by_<owner>`), `inbox`, `outbox`, `notifications`, `trashbin`, subscriptions, federated calendars, app-generated calendars — `apps/dav/lib/CalDAV/CalendarHome.php:getChildren` |
| `calendar-data` options | `content-type` (`text/calendar` \| `application/calendar+json`) and `version` attributes — `3rdparty/sabre/dav/lib/CalDAV/Xml/Request/CalendarMultiGetReport.php:41-64`; `<cal:expand start end>` — `Plugin.php:438-465` (multiget) and `:505-565` (query) |
| `limit-recurrence-set` / `limit-freebusy-set` | **not implemented anywhere** in Sabre 4.7 or `apps/dav` (grep: 0 hits). Only `expand` exists. |
| `calendar-query` SQL fast path | filters on `componenttype`, `firstoccurence`/`lastoccurence`; post-filters with `CalendarQueryValidator` (parses iCalendar, expands recurrences) — `CalDavBackend.php:1958-2050` |

`oc_calendarobjects` columns the sidecar would read: `id, uri, calendardata,
calendarid, calendartype, lastmodified, etag, size, componenttype,
firstoccurence, lastoccurence, uid, classification, deleted_at`; changes live in
`oc_calendarchanges (uri, synctoken, calendarid, calendartype, operation,
created_at)` — same shape as `oc_addressbookchanges` plus a `calendartype`
predicate (`0` = calendar, `1` = subscription).

---

## 6. Recommendation

### Implement natively (in this order)

**1. Per-calendar PROPFIND Depth 0 — 278 req / 30.7 % of CalDAV time.**
Highest count, and it is the gate for everything else: DAVx5's capability probe
decides between `sync-collection` and a full `calendar-query`, and Thunderbird
uses it to identify the collection. Must return the full calendar property set
(§5.5) including `{DAV:}supported-report-set` with `{DAV:}sync-collection`, and
the `getctag` = sabre-sync-URL quirk.

**2. Per-calendar REPORT: `sync-collection` + `calendar-multiget` — 272 req /
30.0 % of time, 78.8 % of bytes.**
`sync-collection` is byte-for-byte the same request shape the sidecar already
parses, and `calendar-multiget` is the same as `addressbook-multiget` with
`address-data` → `calendar-data`. Together they cover 266/272 REPORTs
(Thunderbird 254 + DAVx5 12). **Delegate `calendar-query` to PHP** in v1: it is
the only piece that needs iCalendar comp-filter/time-range semantics and
`<cal:expand>` recurrence expansion, and in this window it is ~0 browser
requests + DAVx5's fallback only.

**3. Calendar-home PROPFIND Depth 1 — 93 req / 10.3 %.**
Cheap in bytes (1338 B, one fixed prop list) but the child set is large
(shared calendars, inbox/outbox/notifications, trashbin, subscriptions,
federated and app-generated calendars). Either implement a read-only listing of
`oc_calendars` + `oc_calendarsubscriptions` and 501 for the rest, or leave it on
PHP. If the home listing must be complete for KDE, PHP is the safer v1.

### Delegate

| path | why |
|---|---|
| `calendar-query` REPORT (any `comp-filter`/`time-range`/`expand`) | needs recurrence-aware iCalendar evaluation; low traffic today |
| `POST` (outbox / free-busy), `PUT`, `DELETE`, `MKCALENDAR`, `PROPPATCH` | write/scheduling path; CardDAV writes came later too |
| `/system-calendars/`, `/public-calendars/`, `inbox`, `outbox`, `notifications`, `trashbin`, federated calendars | no traffic, complex |
| shared calendars | `oc_dav_shares type='calendar'` + `_shared_by_<owner>` URIs; do it after the own-calendar path works |
| any request for a principal other than the caller | already 501 → PHP (`discovery.rs`) |

### Shared vs new — concretely

**Already in the sidecar and directly reusable:**

* the auth fast path (`oc_authtoken` + SHA-512) — no change;
* **CalDAV discovery is already native**: `/remote.php/dav/` root PROPFIND and
  the own-principal PROPFIND including `calendar-home-set`,
  `calendar-user-address-set`, `calendar-user-type` (`src/discovery.rs:107-140`,
  `:366-378`) — that is 29.1 % of CalDAV time + 909 root PROPFINDs already off
  PHP;
* `xml/parse.rs::parse_propfind`, `parse_multiget`, `parse_sync_collection` —
  identical request shapes;
* `xml/write.rs` `MultiStatus`/`PropStat`/`PropValue` serialisation;
* `sync.rs` in full: `parse_sync_token`, `initial_sync`,
  `initial_sync_continue`, `changes_sync`, truncation/`507` semantics — the
  algorithm is table-agnostic, it only needs `oc_calendars.synctoken` +
  `oc_calendarchanges` instead of the addressbook pair;
* `routes.rs` dispatch/`501 → PHP` pattern and the nginx location+`error_page`
  split.

**New for calendars:**

* **path shape**: `/remote.php/dav/calendars/<user>/<cal>/<obj>` — no `users/`
  segment (addressbooks are `/addressbooks/users/<u>/<book>/`). New
  `parse_path` branch + a new nginx regex location.
* **DB layer**: `oc_calendars`, `oc_calendarobjects` (`calendartype = 0 AND
  deleted_at IS NULL`), `oc_calendarchanges`, `oc_calendarsubscriptions`,
  `oc_dav_shares` (`type='calendar'`), `oc_properties` (`oc:calendar-enabled`).
* **property values**: the calendar `propertyMap`, the sabre-sync `getctag`
  quirk, `{DAV:}sync-token` prefixing, and two **XML-typed** properties the
  CardDAV code never had to emit (`supported-calendar-component-set`,
  `schedule-calendar-transp`).
* **iCalendar handling**: `calendar-data` is the raw blob, but `etag` quoting,
  `component` from `componenttype` and `size` must match Sabre; a new
  `ical.rs` for UID/component extraction is needed only if the sidecar ever
  evaluates `comp-filter`.
* **`calendar-query` filter engine** (`xml/filter.rs` is vCard-only) and
  `<cal:expand>` recurrence expansion — the genuinely new, hard part, which is
  why it stays on PHP.

Net: paths 1 and 2 reuse the request parsing, multistatus writer and sync-token
algorithm unchanged; the new work is the DB queries, the calendar property
resolver and the nginx location.

---

## 7. Open questions / what to measure next

1. **Depth of the 93 calendar-home PROPFINDs** — not in the log. Confirm with a
   packet capture or by temporarily logging `$http_depth` (a config change, so
   it needs a go-ahead).
2. **Exact split of the 272 REPORTs** between `sync-collection`,
   `calendar-query` and `calendar-multiget`. Needs `$request_body` (or a
   debug sidecar that logs the body length + root element) — the current log
   cannot answer it.
3. **Real p50/p90 per shape** — requires either a bounded app-password benchmark
   (the `dav-bench` harness pattern) or `$request_time` in the log format.
4. Whether Thunderbird's multiget ever lands here: 250/272 REPORTs are
   no-change, so the multiget volume is currently invisible; it will grow the
   moment sync-collection returns changes.
