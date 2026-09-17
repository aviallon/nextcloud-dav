# nextcloud-dav

A read-only **CardDAV sidecar** for Nextcloud. It serves
`/remote.php/dav/addressbooks/users/<user>/...` straight from the Nextcloud
database, bypassing the PHP stack for the steady-state sync traffic.

It is the v1 implementation of `../dav-bench/CARDDAV_DESIGN.md`: personal
address books, read-only, with the app-password fast path, the PHP fallback and
brute-force throttling parity.

> **Status: v1 (read-only).** `PUT`, `DELETE`, `MKCOL`, `PROPPATCH`, `MOVE`,
> `COPY` and `POST` answer `501 Not Implemented` on purpose so nginx can hand
> them back to PHP. Shared/group/system address books are not served and must
> stay routed to PHP.

## What works

| Capability | Notes |
|---|---|
| `PROPFIND` Depth 0/1 | home, address book and card nodes |
| `GET` / `HEAD` a card | `text/vcard; charset=utf-8`, quoted ETag, `Last-Modified` |
| `REPORT addressbook-multiget` | hrefs resolved, missing hrefs get a 404 propstat |
| `REPORT addressbook-query` | RFC 6352 §10.5 filters evaluated in Rust, `limit` honoured |
| `REPORT sync-collection` | exact `oc_addressbooks.synctoken` / `http://sabre.io/ns/sync/<n>` scheme, `init_<lastID>_<tok>` paging, `507` on truncation |
| `OPTIONS` | `DAV: 1, 2, 3, addressbook` + `Allow` |
| Auth fast path | `hex(sha512(app_password + secret))` against `oc_authtoken` |
| Auth fallback | credentialed `PROPFIND /remote.php/dav/` Depth 0 → `current-user-principal` |
| Brute force | `sleepDelayOrThrowOnMax` parity (`0.1·2^n` s, 25 s cap, 12 h/30 min hard block) |

Properties served include `{DAV:}resourcetype`,
`{DAV:}displayname`, `{carddav}addressbook-description`,
`{calendarserver.org/ns}getctag`, `{sabredav.org/ns}sync-token`,
`{DAV:}sync-token`, `{DAV:}supported-report-set`,
`{carddav}max-resource-size`, `{carddav}supported-address-data`,
`{carddav}supported-collation-set`, `{DAV:}owner`,
`{DAV:}current-user-privilege-set`, `{oc}groups`, `{nc}owner-displayname`,
`{nc}has-photo`, and on cards `{DAV:}getetag`, `{DAV:}getcontentlength`,
`{DAV:}getlastmodified`, `{DAV:}getcontenttype`, `{carddav}address-data`.

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
    // Write failures to oc_bruteforce_attempts. This is the *only* write the
    // v1 sidecar can perform; it is off by default so the sidecar is strictly
    // read-only. Set to true for full throttling parity with PHP.
    'record_bruteforce_attempts' => false,
],
```

The database connection, `dbtableprefix` and `overwrite.cli.url` are read with
[`nextcloud-config-parser`](https://crates.io/crates/nextcloud-config-parser)
exactly like `notify_push`. `secret` and the app-config block above are read
from the same merged `$CONFIG` array (the parser does not expose them).

## nginx routing

Move **only** `/remote.php/dav/addressbooks/**` to the sidecar. Keep
`/.well-known/carddav`, the DAV root `/remote.php/dav/` and
`/remote.php/dav/principals/...` on PHP: the root/principal are shared with
CalDAV, and `/remote.php/dav/` is the auth fallback (it must never recurse).

```nginx
# CardDAV fast path: read-only methods.
location ^~ /remote.php/dav/addressbooks/ {
    proxy_pass http://127.0.0.1:7868;
    proxy_http_version 1.1;
    proxy_set_header Host              $host;
    proxy_set_header X-Real-IP         $remote_addr;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header Authorization     $http_authorization;  # fastcgi drops this by default
    proxy_request_buffering off;       # keep PUT streaming if you ever enable writes
    client_max_body_size 10m;
    proxy_read_timeout 300s;

    # PUT/DELETE/MKCOL/PROPPATCH (and ?photo / ?export) answer 501 here and are
    # re-dispatched to the regular PHP front controller.
    proxy_intercept_errors on;
    error_page 501 = @nextcloud_dav_php;
}

location @nextcloud_dav_php {
    include fastcgi_params;
    fastcgi_param SCRIPT_FILENAME    $document_root/remote.php;
    fastcgi_param SCRIPT_NAME        /remote.php;
    fastcgi_param REQUEST_URI        $request_uri;
    fastcgi_param HTTP_AUTHORIZATION $http_authorization;
    fastcgi_pass php-fpm;            # your existing PHP upstream/socket
}

# Discovery, principals, the system book and everything else stay on PHP.
location ~ \.php(?:$|/) {
    ...
    fastcgi_param HTTP_AUTHORIZATION $http_authorization;
    ...
}
```

Keep `/.well-known/carddav → /remote.php/dav/` exactly as today.

Caddy equivalent:

```caddyfile
@carddav path /remote.php/dav/addressbooks/*
handle @carddav {
    reverse_proxy 127.0.0.1:7868 {
        header_up X-Forwarded-For {remote_host}
    }
}
handle { php_server }
```

If the sidecar is down, that subtree returns `502`; roll back by deleting the
`location` block.

## Database access

All queries are `SELECT`s against `oc_addressbooks`, `oc_cards`,
`oc_addressbookchanges`, `oc_authtoken`, `oc_users`, `oc_preferences`,
`oc_cards_properties` and `oc_bruteforce_attempts`. The only possible write is
the opt-in `INSERT` into `oc_bruteforce_attempts`. A dedicated read-only DB user
is therefore sufficient unless `record_bruteforce_attempts` is enabled.

The `sqlx::Any` driver does not translate placeholders, so queries are written
with `?` and rewritten to `$n` for PostgreSQL; it also cannot map PostgreSQL
`boolean` / MySQL `TINYINT` values uniformly, so `oc_authtoken.password_invalid`
is selected through a portable `CASE WHEN ... THEN '1' ELSE '0' END`.

## Auth model

1. Brute-force check first (`sleepDelayOrThrowOnMax`): compute the `/32` (IPv4,
   or unwrapped IPv4-mapped IPv6) or `/56` subnet, count recent `login`
   attempts, sleep the backoff, or return `429` past the 12 h/30 min limits.
2. Fast path: `hex(sha512(app_password + secret))`, then
   `oc_authtoken WHERE token = ? AND version = 1` (retrying
   `sha512(app_password)` for legacy instances with an empty `secret`), then
   the `type ∈ {1,3}`, `expires`, `password_invalid`, case-insensitive
   `login_name`, `uid ∈ oc_users` and disabled-user gates.
3. `last_check` older than 300 s, non-native users, expired tokens and
   non-token credentials go to the PHP fallback, which also refreshes
   `last_check` — the same 5-minute window Nextcloud uses itself.

The sidecar holds `config.php`, so it is as sensitive as the web server: run it
as an unprivileged user, keep its config unreadable, and bind it to loopback.

## Known limitations / stubs

- **Read-only.** Writes are `501`, by design.
- **Personal books only.** Shared books (`oc_dav_shares`), group books and the
  system book (`addressbooks/system/...`, `z-server-generated--system`) are not
  served; keep them on PHP.
- **No vCard version negotiation.** `address-data` is returned as stored
  (typically vCard 3.0); the `version`/`content-type` attributes are parsed but
  ignored, and the `address-data` child `prop` filter is not applied.
- **No conditional GET.** `ETag`/`Last-Modified` are sent, but
  `If-None-Match`/`If-Modified-Since` are not evaluated (clients re-download).
- **`allprop`** returns a curated property set rather than every live property.
- **`?photo` / `?export`** return `501` and are delegated to PHP.
- **Brute-force recording is opt-in** (`record_bruteforce_attempts`, default
  `false`) to keep the sidecar strictly read-only; the delay/block checks always
  run.

## Tests

```sh
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config nixpkgs#cmake nixpkgs#openssl \
  -c cargo test
```

Unit tests cover the sync-token state machine (including `init_` paging and
truncation), `{DAV:}multistatus` serialisation/parsing, RFC 6352 filter
evaluation, the token hash, brute-force backoff and subnet normalisation, path
parsing and property resolution.
