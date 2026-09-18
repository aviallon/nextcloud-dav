# nextcloud-dav end-to-end validation report

Date: 2026-09-18. Environment: disposable `nextcloud:33.0.5-apache` (the
production version) + `postgres:18-alpine`, private docker network, sidecar
built from this checkout and run on the host. Nothing touched production.

**Result: 91/91 acceptance checks PASS — but only after a test-only shim was
applied. As shipped, the companion app cannot run on Nextcloud 33 at all
(blocking bug B1 below), and the dispatch half of the pipeline is therefore
unverifiable on the production version without it.**

**All database-backed effects were verified by querying the database**
(`oc_cards`, `oc_cards_properties`, `oc_addressbookchanges`, `oc_addressbooks`,
`oc_dav_event_outbox`, `oc_calendars`, `oc_calendarobjects`,
`oc_calendarchanges`, `oc_calendar_reminders`, `oc_activity`, `oc_filecache`,
`information_schema`, `pg_database`). Nothing is inferred from exit codes or
from the worker's status line.

## Files created

| file | purpose |
|---|---|
| `tests/local/docker-compose.yml` | postgres:18-alpine + nextcloud:33.0.5-apache, private network, loopback ports 55433/18081 |
| `tests/local/lib.sh` | shared helpers (compose, occ, psql, sidecar lifecycle, readiness waits) |
| `tests/local/setup.sh` | recreate: up, install, shim, app, user, app-password, address book, copy config, build + start sidecar |
| `tests/local/e2e.sh` | the 91 acceptance checks; evidence to `state/evidence/e2e.txt` |
| `tests/local/teardown.sh` | `compose down -v` + remove `state/` |
| `tests/local/README.md` | instructions |
| `tests/local/.gitignore` | ignores generated `state/` |
| `tests/local/EVIDENCE.txt` | full 91-check run log (copied out of `state/` before teardown) |
| `tests/local/bug-isetupmanager.txt` | captured as-shipped failure (full stack trace) |
| `tests/local/REPORT.md` | this file |

## Recreate + run

```sh
cd nextcloud-dav/tests/local
./setup.sh      # ~2–3 min: containers, install, app, user, address book, sidecar
./e2e.sh        # ~6 min: the checks, writes state/evidence/e2e.txt
./teardown.sh   # free everything (containers, volumes, network, secrets)
```

Verified from scratch: `teardown.sh` → `setup.sh` → `e2e.sh` = 91/91 PASS.
The sidecar is built with `nice -n 19 nix shell nixpkgs#cargo … -c cargo build
--release --locked`; `setup.sh` skips the build if the binary exists.

## Acceptance criteria

| # | criterion | result | observed evidence |
|---|---|---|---|
| 1 | **Create** | **PASS** | `PUT` → `201`, `etag: "662f0263699aa8e82daa318d4d856954"`. DB: `md5(carddata)=etag` true, `size=443=octet_length(carddata)`, `uid=e2e-create-1`; `oc_addressbookchanges` `operation=1, synctoken=1` (pre-increment) and `oc_addressbooks.synctoken` 1→2; properties `FN/UID/BDAY`, `EMAIL TYPE=PREF → preferred=1`, 100×`€` NOTE → `octet_length=252, char_length=84` (no split); exactly 1 outbox row `state=0, event_type=1`, `card_row={"etag":"\"662f…\"","id":44,"lastmodified":…,"size":443,"uid":"e2e-create-1","uri":"e2e-create.vcf"}`, `effects={"php":[all 7],"rust":[]}` |
| 2 | **Dispatch** | **PASS** | `occ dav:event-dispatch --once` → row `state=2`, `processed_at` set. `oc_calendars` alice `contact_birthdays` synctoken=2; `oc_calendarobjects` 1 VEVENT `uid=e2e-create-1`, `DTSTART;VALUE=DATE:19900412`, `SUMMARY:🎂 Alice Example (1990)`, `VALARM PT9H`; `oc_calendarchanges op=1, synctoken=1`; `oc_calendar_reminders` non-empty; activity delta = 1 (`card_add_self`). Photo cache: on **create** the seeded `nophoto` entry is **not** removed (see B2) — removal verified on update/delete (C3.8/C3.9/C4.9) |
| 3 | **Update** | **PASS** | `PUT` → `204`, new ETag `"ed3ec51435d6797870c420573d128d91"`; change `operation=2, synctoken=2`; new outbox row `event_type=2, state=0`; after dispatch VEVENT count still **1**, `DTSTART` now `19910520` (updated, not duplicated); photo cache filecache rows 1→0 and physical dir gone; `card_update_self` activity = 1 |
| 4 | **Delete** | **PASS** | `DELETE` → `204`; change `operation=3`; `oc_cards` 0, `oc_cards_properties` for the card 0; delete outbox row `event_type=3, state=0` with `card_data` still containing the **pre-delete** `BDAY:1991-05-20` and `card_row` the pre-delete ETag; after dispatch VEVENT count 0, photo cache 0, `card_delete_self` activity = 1 |
| 5 | **Idempotency** | **PASS** | two further `--once` runs: outbox signature `1:2:…,2:2:…,3:2:…` byte-identical, `oc_activity` 82→82, `oc_calendarobjects` 2→2, photo cache 1→1 |
| 6a | **SIGKILL mid-batch** | **PASS** | 40 rows seeded; `kill -9` after 5 rows reached done → **32 rows left `state=1`** (stale claims), 8 committed. After claim timeout: all 40 `state=2`, **40** birthday VEVENTs (one per card), **40** activity rows, 0 duplicate UIDs |
| 6b | **Polling fallback** | **PASS** | row inserted with raw SQL (no `pg_notify`) while a resident worker was idle → picked up, `state=2`, birthday VEVENT exists. (The image lacks the `pgsql` extension, so the worker was already in polling mode; see N1) |
| 6c | **Poison-row isolation** | **PASS** | `healthy-a`(2) → invalid-JSON row → missing-addressbook row → `healthy-b`(2): both healthy rows `state=2`, missing-addressbook row `state=2` (skipped, not retried), poison row retried 3× with backoff then `state=3`, `last_error` recorded, both healthy birthday VEVENTs present |
| 6d | **Refuse writes w/o outbox** | **PASS** | scratch DB (pg_dump clone) with the table dropped: sidecar logs `event outbox table is missing … refusing native writes`, `PUT` → **501**; restored sidecar `PUT` → **201**; scratch DB dropped |
| 7 | **Missing optional apps** | **PASS** | `activity` + `notifications` disabled → create row still `state=2`, birthday VEVENT created, 0 activity rows. Worker still starts (no `CloudIdManager` error printed) |
| 7b | **CloudIdManager cannot be built** | **NOT TESTED** | could not be forced in isolation: removing `CloudIdManager.php` breaks `OC\Server` construction globally before the worker's `try/catch` runs; a dead Redis does not throw (connection is lazy). See "Could not test" |

### Outbox table schema (criterion preamble)

`information_schema.columns` matches `src/outbox.rs::OUTBOX_COLUMNS` exactly,
including types: `seq bigint` (BIGSERIAL), `event_type smallint`,
`addressbookid bigint`, `card_uri varchar(255)`, `card_row text`,
`card_data bytea`, `effects text`, `state/attempts smallint`,
`next_attempt_at/reserved_at/processed_at bigint`, `reserved_by varchar(64)`,
`last_error text`.

## Bugs and observations

### B1 — BLOCKING: app cannot run on Nextcloud 33 (`OCP\Files\ISetupManager` missing)

- **Where:** `app/nextcloud_dav/lib/Command/EventDispatch.php:80`
  (`private readonly ISetupManager $setupManager`), import at `:24`;
  `appinfo/info.xml` declares `min-version="33"`.
- **Repro:** enable the app, then run any `occ` command, e.g.
  `docker exec -u www-data -w /var/www/html <nc> php occ status`.
- **Observed:**
  `Could not resolve OCP\Files\ISetupManager! Class "OCP\Files\ISetupManager" does not exist`
  (`OC\AppFramework\Utility\QueryNotFoundException … SimpleContainer.php:138`).
  Nextcloud 33.0.5 has only the private `OC\Files\SetupManager` (which *does*
  have `tearDown()`); the public interface was added in a later major.
- **Impact:** the command cannot be instantiated, so **every** `occ` invocation
  fails while the app is enabled — including `dav:event-dispatch`, and even
  unrelated commands (`occ user:add`, `occ app:disable`). The sidecar's writes
  still succeed, but nothing drains the outbox.
- **Expected:** either the app supports NC 33 (use `OC\Files\SetupManager` /
  skip the hygiene call when the interface is absent) or `info.xml` requires the
  major that introduced `ISetupManager`.
- **Harness workaround (test-only):** `setup.sh` installs a no-op concrete class
  `OCP\Files\ISetupManager` in the disposable container so the remaining 91
  checks could run. This is not product code and must not be deployed.

### B2 — Minor: `effects` over-declares `photo_cache` for create events

- **Where:** `src/outbox.rs::KNOWN_EFFECTS` / `EffectRegistry` (static for all
  event types); the dispatcher `EventDispatch::dispatchRow()` dispatches
  generically. `ClearPhotoCacheListener` is registered only for
  `CardUpdatedEvent`/`CardDeletedEvent` (`apps/dav/…/Application.php:194-195`).
- **Observed:** a create row carries
  `{"php":[…,"photo_cache",…],"rust":[]}`, but a seeded `dav-photocache` entry
  survives a `CardCreatedEvent` dispatch (C2.13: 1→1); it is removed on update
  (C3.8: 1→0) and delete (C4.9: 1→0).
- **Impact:** cosmetic — the ownership map is not an exact statement of what a
  create row will do; no wrong behaviour.

### N1 — Deployment note: `LISTEN` fast path is dead on the official image

The `nextcloud:33.0.5-apache` image ships `pdo_pgsql` but **not** the `pgsql`
extension, so `pg_socket()` is `false` and the worker prints
`LISTEN unavailable (the pgsql extension / pg_socket() is not available);
falling back to plain polling every 100 ms.` The fallback is correct and was
exercised by 6b, but if production runs the same image the NOTIFY fast path
never engages and dispatch latency is bounded by `idle_poll_ms`.

### N2 — Doc drift

`docs/EVENT_DISPATCH.md` §3 suggests `occ migrations:status nextcloud_dav`;
that command does not exist in NC 33 (`Command "migrations:status" is not
defined`). `setup.sh` verifies the table via `information_schema` instead.

## Could not test

- **Criterion 7b (CloudIdManager cannot be built):** forcing the failure
  requires breaking global boot. Renaming `lib/private/Federation/
  CloudIdManager.php` makes `OC\Server`'s service registration throw before the
  worker's `try { Server::get(CloudIdManager::class); } catch (Throwable $e)`
  is reached; a dead Redis does not throw because the connection is lazy. The
  guarded code path is present and correct-looking, but was not exercised.
- **`redis_cloud_id` effect:** needs a Redis-backed distributed cache and a
  `CLOUD;` vCard line. No Redis is configured in this disposable instance, so
  the effect was not observed. It is listed in `effects` as required (C1.15).
- **`notification_push` / `activity_mail`:** push/email are off by default for
  `contacts`; no `oc_notifications`/`oc_activity_mq` rows are expected, so only
  the effect *ownership* was verified, not a delivered notification.

## Verification method

Every row of the acceptance table is a `RESULT|id|PASS|…|expected=… actual=…`
line in `tests/local/EVIDENCE.txt` (also regenerated at
`tests/local/state/evidence/e2e.txt` by `./e2e.sh`), produced by direct SQL
against the disposable Postgres and by HTTP status/ETag headers from the
sidecar. The as-shipped failure is captured in
`tests/local/bug-isetupmanager.txt`. The script is deterministic and
re-runnable (`./e2e.sh` resets only alice's test data first).
