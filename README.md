# nextcloud-dav

A **DAV sidecar** for Nextcloud. It serves CardDAV
(`/remote.php/dav/addressbooks/users/<user>/...`), a CalDAV subset
(`/remote.php/dav/calendars/<user>/...`), WebDAV file listings
(`/remote.php/dav/files/<user>/...`) and the two discovery `PROPFIND`s (the DAV
root and the caller's own principal) straight from the Nextcloud database,
bypassing the PHP stack for the steady-state sync traffic — reads *and* card
writes.

It implements `../dav-bench/CARDDAV_DESIGN.md`: personal address books, with the
app-password fast path, the PHP fallback and brute-force throttling parity.
Reads are served from the Nextcloud database (PostgreSQL or MySQL). Card
`PUT`/`DELETE` are native when `nextcloud_dav.event_dispatch` is enabled and the
companion app's outbox table exists (otherwise they answer `501` and nginx
hands them to PHP), and the PHP event side effects they trigger (activity,
birthday calendar, photo cache, push) are dispatched asynchronously from a
transactional outbox by the companion [`nextcloud_dav` app](app/nextcloud_dav).

> **Status: reads + card writes.** `MKCOL`, `PROPPATCH`, `MOVE`, `COPY`, `POST`,
> `?photo`/`?export`, and any write to a collection answer `501 Not Implemented`
> on purpose so nginx can hand them back to PHP. Owned, user-shared and
> database-group-shared `oc_dav_shares` books are served; the system book, the
> app-generated `contactsinteraction` book and the address-book home listing
> stay on PHP (`home-listing-php`, `contactsinteraction-php`).

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the as-built design:
deployment topology, the nginx split and its failure mode, the app-password
fast path, the sync-token scheme, the data model, and the operational traps.

## What works

| Capability | Notes |
|---|---|
| `PROPFIND` Depth 0/1 | address book and card nodes (the home listing stays on PHP — `home-listing-php`; the sidecar's own home handler is a misroute safety net listing owned books only) |
| `PROPFIND` discovery | DAV root and the caller's own principal, `Depth 0`, fixed property sets; an unknown requested property answers `501`, never `404` |
| `PROPFIND` files | `/remote.php/dav/files/<uid>/**` `Depth 0`/`1` from `oc_filecache` + `oc_mounts`: mount entries, listings inside local/shared/groupfolder mounts (groupfolder ACL engine included), the synthetic parent etag/size/mtime, quota, `Prefer: minimal` |
| Shared / group books | `oc_dav_shares` user + database-group books served as `<uri>_shared_by_<owner>` with the sharing properties (`owner-principal`, `read-only`, owner's `{DAV:}owner`); read-only shares are `404` on write (never `403`), read-write writes land in the owner's book |
| `GET` / `HEAD` a card | `text/vcard; charset=utf-8`, quoted ETag, `Last-Modified`; conditional `If-None-Match`/`If-Modified-Since` → `304` (RFC 7232 semantics); `Accept`-negotiated body (vCard 4 / jCard) like Sabre's `httpAfterGet` |
| `REPORT addressbook-multiget` | hrefs resolved, missing hrefs get a 404 propstat |
| `address-data` negotiation | vCard 3↔4 conversion, jCard (`content-type="application/vcard+json"`) and the child `<card:prop>` filter (honoured in **both** reports), via a faithful VObject port; plain same-version requests stay byte-verbatim; `GET` negotiates via the `Accept` header like `httpAfterGet` |
| `REPORT addressbook-query` | RFC 6352 §10.5 filters evaluated in Rust, `limit` honoured |
| `REPORT sync-collection` | exact `oc_addressbooks.synctoken` / `http://sabre.io/ns/sync/<n>` scheme, `init_<lastID>_<tok>` paging, `507` on truncation |
| CalDAV `PROPFIND` | the calendar home (`Depth 0`/`1`), one owned calendar (`Depth 0`) and calendar subscriptions from `oc_calendars` + `oc_calendarsubscriptions` + `oc_dav_shares` + the `oc_properties` override layer; the web UI's `{cs}publish-url` and the `{oc}`/`{DAV:}invite` share lists are reproduced |
| CalDAV `REPORT calendar-multiget` | 100-URI chunks (`getMultipleCalendarObjects`), stored `calendar-data` (every `\r` stripped), missing hrefs dropped |
| CalDAV `REPORT sync-collection` | `MAX(operation)` per URI, no `init_` paging, `507` on an initial sync with a limit, `403` + `valid-sync-token` on a bad token |
| `OPTIONS` | `DAV: 1, 2, 3, addressbook` + `Allow` |
| `PUT` a card | create/update, `If-Match`/`If-None-Match`, `409` on a duplicate UID, `403` past `card_size_limit`, quoted `ETag`; native only when `event_dispatch.enabled` and the outbox table exist, else `501` → PHP |
| `DELETE` a card | `204`, change logged, search columns purged |
| Event outbox | one `oc_dav_event_outbox` row + `pg_notify` in the card transaction; the companion app's worker dispatches the real `Card*Event`s |
| Auth fast path | `hex(sha512(app_password + secret))` against `oc_authtoken` (`version = 2`) |
| Auth session | the web UI's cookie-only DAV requests: `oc<instanceid>` + `oc_sessionPassphrase` → the Redis session store → Nextcloud's `Crypto` envelope; anything unprovable is delegated to PHP |
| Auth fallback | credentialed `PROPFIND /remote.php/dav/` Depth 0 → `current-user-principal`; the probe carries `X-Nextcloud-Dav-Fallback` so it never recurses through the root route |
| Brute force | `sleepDelayOrThrowOnMax` parity (`0.1·2^n` s, 25 s cap, 12 h/30 min hard block) |

Properties served include `{DAV:}resourcetype`,
`{DAV:}displayname`, `{carddav}addressbook-description`,
`{calendarserver.org/ns}getctag`, `{sabredav.org/ns}sync-token`,
`{DAV:}sync-token`, `{DAV:}supported-report-set`,
`{carddav}max-resource-size`, `{carddav}supported-address-data`,
`{carddav}supported-collation-set`, `{DAV:}owner`,
`{DAV:}current-user-privilege-set`, `{oc}groups`, `{oc}owner-principal`,
`{oc}read-only`, `{nc}owner-displayname`, `{nc}has-photo`, and on cards
`{DAV:}getetag`, `{DAV:}getcontentlength`, `{DAV:}getlastmodified`,
`{DAV:}getcontenttype`, `{carddav}address-data`.

## Build and run

```sh
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config nixpkgs#cmake nixpkgs#openssl \
  -c cargo build --release

./target/release/nextcloud-dav --config /path/to/nextcloud/config/config.php
```

Options:

```
nextcloud-dav [--config <PATH>] [--glob-config] [--database-url <URL>]
              [--listen <ADDR>] [--log-level <LEVEL>] [--max-connections <N>]
              [CONFIG_FILE]
```

`--glob-config` also merges sibling `*.config.php` files, like
`notify_push`. The health endpoint is `GET /healthz` and is meant to stay on
loopback.

### `config.php` additions

```php
'nextcloud_dav' => [
    // Loopback only; nginx proxies to it. Default: 127.0.0.1:7868.
    'listen' => '127.0.0.1:7868',
    // Base URL of the PHP front end, used by the auth fallback.
    // Defaults to overwrite.cli.url.
    'fallback_base_url' => 'https://cloud.example.com',
    // Accept invalid TLS certificates for the fallback (default: false).
    'allow_self_signed' => false,
    // Timeout for the fallback PROPFIND, seconds (default: 30).
    'php_timeout_secs' => 30,
    // Session store for the web UI's session-cookie auth — the same value PHP
    // has in session.save_path. Defaults to a URL derived from the top-level
    // 'redis' block; with neither, session-cookie requests delegate to PHP.
    'session_redis_url' => 'tcp://127.0.0.1:6379?auth=...',
    // Session lookup timeout, ms (default: 500).
    'session_redis_timeout_ms' => 500,
    // Native card PUT/DELETE: each write enqueues its event side effects in
    // oc_dav_event_outbox for the companion app's worker. When 'enabled' is
    // false (or the outbox table is missing) writes answer 501 and nginx
    // replays them to PHP. Default: true.
    'event_dispatch' => [
        'enabled' => true,
        // pg_notify channel the companion worker LISTENs on.
        'notify_channel' => 'oc_dav_event_outbox',
    ],
    // Override the card write limit in bytes (default: the 'dav' app's
    // card_size_limit app-config value, else 5 MiB).
    'card_size_limit' => 5242880,
    // Record failed logins to oc_bruteforce_attempts (default: false). The
    // delay/block checks always run; only the recording is opt-in. Set to true
    // for full throttling parity with PHP.
    'record_bruteforce_attempts' => false,
],
```

The database connection, `dbtableprefix` and `overwrite.cli.url` are read with
[`nextcloud-config-parser`](https://crates.io/crates/nextcloud-config-parser)
exactly like `notify_push`. `secret` and the `nextcloud_dav` block above are read
from the same merged `$CONFIG` array (the parser does not expose them).

## nginx routing

Route the subtrees the sidecar serves to it and leave everything else on PHP.
The split is an optimisation, not a correctness boundary: every path, method or
property the sidecar cannot reproduce answers `501 Not Implemented`, and
`proxy_intercept_errors` + `error_page` replay the request to PHP.

Three things are load-bearing:

- **Regex locations, listed before the `\.php` location.** nginx picks the first
  matching regex, and every `/remote.php/dav/...` path also matches
  `\.php(?:$|/)`. Never `^~`: it would shadow the PHP regex location and swallow
  paths that must stay on PHP — for CardDAV that includes the home listing
  (deviation `home-listing-php`).
- **`proxy_request_buffering on` (the default).** The `error_page` fallback is
  an internal redirect; with streaming (`proxy_request_buffering off`) the
  request body is gone by then and a replayed `PUT` would reach PHP with an
  empty body. Card bodies are small; keep buffering on.
- **`error_page 501 502 504 = @nextcloud_dav_php`.** `501` is the "not mine"
  answer; `502`/`504` make a dead sidecar degrade to PHP instead of failing the
  subtree.

```nginx
# CardDAV: everything strictly below a user's address-book home. The trailing
# `/.` requires at least one character after `users/<u>/`, so the home listing
# `/remote.php/dav/addressbooks/users/<u>/` falls through to PHP (deviation
# `home-listing-php`: PHP also advertises the app-generated collections).
location ~ ^/remote\.php/dav/addressbooks/users/[^/]+/. {
    proxy_pass http://127.0.0.1:7868;
    proxy_http_version 1.1;
    proxy_set_header Host              $host;
    proxy_set_header X-Real-IP         $remote_addr;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header Authorization     $http_authorization;  # fastcgi drops this by default
    proxy_request_buffering on;        # the error_page replay needs the body
    client_max_body_size 10m;
    proxy_read_timeout 300s;

    # Anything not native (MKCOL/PROPPATCH/MOVE/COPY/POST, ?photo / ?export,
    # PUT/DELETE when native writes are off) answers 501 and is re-dispatched
    # to the regular PHP front controller.
    proxy_intercept_errors on;
    error_page 501 502 504 = @nextcloud_dav_php;
}

location @nextcloud_dav_php {
    include fastcgi_params;
    fastcgi_param SCRIPT_FILENAME    $document_root/remote.php;
    fastcgi_param SCRIPT_NAME        /remote.php;
    fastcgi_param REQUEST_URI        $request_uri;
    fastcgi_param HTTP_AUTHORIZATION $http_authorization;
    fastcgi_pass php-fpm;            # your existing PHP upstream/socket
}

# CalDAV: the calendar home `/calendars/<u>/` and everything below it
# (per-calendar PROPFIND, the `sync-collection`/`calendar-multiget` REPORTs on
# an owned calendar). The path shape has no `users/` segment. `/calendars/<u>`
# without the trailing slash stays on PHP. The 501 fallback replays objects,
# `calendar-query`, writes and the trees the sidecar does not model (trashbin,
# federated/app-generated calendars).
location ~ ^/remote\.php/dav/calendars/[^/]+/ {
    proxy_pass http://127.0.0.1:7868;
    proxy_http_version 1.1;
    proxy_set_header Host              $host;
    proxy_set_header X-Real-IP         $remote_addr;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header Authorization     $http_authorization;
    client_max_body_size 10m;
    proxy_read_timeout 300s;
    proxy_intercept_errors on;
    error_page 501 502 504 = @nextcloud_dav_php;
}

# WebDAV files PROPFIND (Depth 0/1 listings; every other method is 501).
location ~ ^/remote\.php/dav/files/ {
    ...same proxy_* block as above...
}

# Discovery: the DAV root and the caller's own principal (Depth 0 PROPFIND).
# The sidecar's own auth-fallback probe arrives here too, marked with
# `X-Nextcloud-Dav-Fallback`; the sidecar answers 501 for it and nginx replays
# it to PHP, so the fallback never recurses.
location ~ ^/remote\.php/dav/?$ {
    ...same proxy_* block as above...
}
location ~ ^/remote\.php/dav/principals/users/[^/]+/?$ {
    ...same proxy_* block as above...
}

# Everything else — /.well-known/carddav, the system and app-generated books,
# any other /remote.php/dav/ path — stays on PHP. This regex location must come
# AFTER the sidecar locations above.
location ~ \.php(?:$|/) {
    ...
    fastcgi_param HTTP_AUTHORIZATION $http_authorization;
    ...
}
```

Keep `/.well-known/carddav → /remote.php/dav/` exactly as today.

Caddy sketch (nginx is the tested path — Caddy has no `error_page`, so
replaying the sidecar's `501`s to PHP needs a `handle_response` block per
route; the matcher shape is the same idea):

```caddyfile
@carddav path_regexp carddav '^/remote\.php/dav/addressbooks/users/[^/]+/.'
handle @carddav {
    reverse_proxy 127.0.0.1:7868 {
        header_up X-Forwarded-For {remote_host}
    }
}
handle {
    php_server
}
```

If the sidecar is down, the `502`/`504` is replayed to PHP and the subtree
degrades to the regular stack; roll back by deleting the `location` blocks.

## Database access

Reads span the DAV, calendar, files and account tables: `oc_addressbooks`,
`oc_cards`, `oc_addressbookchanges`, `oc_cards_properties`, `oc_dav_shares`,
`oc_calendars`, `oc_calendarobjects`, `oc_calendarchanges`,
`oc_calendarsubscriptions`, `oc_properties`, `oc_authtoken`, `oc_users`,
`oc_groups`, `oc_group_user`, `oc_accounts`, `oc_preferences`, `oc_appconfig`,
`oc_filecache`, `oc_filecache_extended`, `oc_files_metadata`, `oc_mounts`,
`oc_storages`, `oc_mimetypes`, `oc_share`, `oc_vcategory*`, `oc_comments*`,
the external-storage and groupfolders side tables, and `oc_bruteforce_attempts`
(`src/db.rs` is the authoritative list).

Writes are confined to the card path: `INSERT`/`UPDATE`/`DELETE` on `oc_cards`,
`oc_cards_properties` and `oc_addressbookchanges`, `UPDATE` of
`oc_addressbooks.synctoken`, and one `INSERT` into `oc_dav_event_outbox` (plus
`pg_notify` on PostgreSQL) — all in one transaction. The only other write is
the opt-in `INSERT` into `oc_bruteforce_attempts`. A dedicated read-only DB
user is therefore sufficient only when native writes are refused
(`nextcloud_dav.event_dispatch.enabled = false`) and
`record_bruteforce_attempts` is off.

The `sqlx::Any` driver does not translate placeholders, so queries are written
with `?` and rewritten to `$n` for PostgreSQL; it also cannot map PostgreSQL
`boolean` / MySQL `TINYINT` values uniformly, so `oc_authtoken.password_invalid`
is selected through a portable `CASE WHEN ... THEN '1' ELSE '0' END`.

## Auth model

1. Brute-force check first (`sleepDelayOrThrowOnMax`, Basic credentials only):
   compute the `/32` (IPv4, or unwrapped IPv4-mapped IPv6) or `/56` subnet,
   count recent `login` attempts, sleep the `0.1·2^n` s backoff (25 s cap), or
   return `429` past the 12 h/30 min limits.
2. Fast path (Basic with an app password): `hex(sha512(app_password + secret))`,
   then `oc_authtoken WHERE token = ? AND version = 2` (retrying
   `sha512(app_password)` for legacy instances with an empty `secret`), then
   the `type ∈ {1,3}`, `expires`, `password_invalid`, case-insensitive
   `login_name`, `uid ∈ oc_users`, disabled-user and token-scope gates.
3. Session path (no `Authorization` header at all, the web UI's case): the
   `oc<instanceid>` / `oc_sessionPassphrase` cookies resolve the session in the
   Redis session store (configured with `session_redis_url`, or derived from
   the `redis` block), its payload is decrypted with Nextcloud's own `Crypto`
   recipe, and the session's token, 2FA and CSRF state are reproduced. A
   browser form-login session is left to PHP.
4. `last_check` older than 300 s, non-native users, expired tokens and
   non-token credentials go to the PHP fallback, which also refreshes
   `last_check` — the same 5-minute window Nextcloud uses itself.

A request with **no** `Authorization` header that the session path cannot
evaluate is never refused with `401`: the sidecar answers `501` and nginx
replays it to PHP, which owns OAuth `Bearer` and anything else the sidecar
cannot see. A `401` with `WWW-Authenticate: Basic` here would make a browser
pop up a Basic auth prompt for requests PHP serves happily.

The sidecar holds `config.php`, so it is as sensitive as the web server: run it
as an unprivileged user, keep its config unreadable, and bind it to loopback.

## Known limitations / stubs

- **Collection writes are `501`, by design.** `MKCOL`, `PROPPATCH`, `MOVE`,
  `COPY`, `POST` and writes to a collection are replayed to PHP; only card
  `PUT`/`DELETE` are native — and only when `nextcloud_dav.event_dispatch` is
  enabled and the companion app's outbox table exists.
- **The address-book home listing stays on PHP** (`home-listing-php`): PHP also
  advertises the app-generated collections (`z-server-generated--system`,
  `z-app-generated--contactsinteraction--recent`), which the sidecar does not
  model. The sidecar's own home handler (reachable only on a misroute) lists
  just the `oc_addressbooks` rows.
- **Shared books are database-group only.** LDAP/circles group membership and
  `hideFromCollaboration()` are not visible in the schema, so those shares fall
  back to PHP. Tombstone exclusion follows the CalDAV `resourceid` semantics
  rather than the CardDAV `s.id` bug, and a shared write's activity is
  attributed to the owner (the outbox has no actor column). See
  `tests/deviations.toml`.
- **No vCard version negotiation for `PROPFIND`.** A `PROPFIND` `address-data`
  is always the stored bytes (PHP ignores the attributes there too); the
  REPORT paths negotiate fully.
- **`2.1` is never a negotiation target** (matching PHP): a `version="2.1"`
  request converts to 3.0; stored 2.1 cards are upgraded to 3.0.
- **`allprop`** returns a curated property set rather than every live property.
- **`?photo` / `?export`** return `501` and are delegated to PHP.
- **Brute-force recording is opt-in** (`record_bruteforce_attempts`, default
  `false`); the delay/block checks always run.
- **Session-cookie auth needs the session store.** Without `session_redis_url`
  (and no `redis` block to derive it from), the web UI's cookie-only requests
  are delegated to PHP.

Every known behavioural divergence from Nextcloud/SabreDAV is declared in
[`docs/DEVIATIONS.md`](docs/DEVIATIONS.md) and pinned by the test suite.

## Tests

```sh
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config nixpkgs#cmake nixpkgs#openssl \
  -c cargo test
```

Unit and integration tests cover the sync-token state machine (including
`init_` paging and truncation), `{DAV:}multistatus` serialisation/parsing,
RFC 6352 filter evaluation, the token hash and the session-cookie crypto,
brute-force backoff and subnet normalisation, path parsing and property
resolution, the mount/groupfolder-ACL model, and the declared-deviation suite
(`tests/deviations.rs`), which fails if `tests/deviations.toml` and the
behaviour drift apart.

Beyond `cargo test`: `tests/conformance/conformance.py` is the behavioural
conformance suite, and `tests/local/` is a disposable end-to-end harness
(Nextcloud + PostgreSQL in docker) with sidecar-vs-PHP parity scripts for
files, discovery, CalDAV and session-cookie auth — see
[`tests/local/README.md`](tests/local/README.md).
