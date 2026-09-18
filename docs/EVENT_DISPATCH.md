# nextcloud-dav event dispatch (sidecar outbox)

Companion Nextcloud app `nextcloud_dav` + resident worker that turn the
sidecar's native CardDAV writes into real Nextcloud `Card*Event` dispatches.

- Repo path of the app: `nextcloud-dav/app/nextcloud_dav/`
- App id: `nextcloud_dav`
- Namespace: `OCA\NextcloudDav\`
- Runtime dependency: the shipped `dav` app (enforced at worker start-up; see
  [Why the dav dependency is not in info.xml](#why-the-dav-dependency-is-not-in-infoxml))
- Deployed app path: `<nextcloud>/custom_apps/nextcloud_dav/`

Design background: [`recon/event-dispatch.md`](./recon/event-dispatch.md) and
[`recon/card-events.md`](./recon/card-events.md). This document is the
operator-facing counterpart.

---

## 1. Why the outbox exists

When the sidecar performs a native `PUT`/`DELETE`, PHP never runs, so the
listeners that normally fire inside `CardDavBackend::createCard/updateCard/
deleteCard` are skipped. The sidecar therefore writes a row into the
companion app's outbox **in the same database transaction as the card write**:

```
BEGIN
  INSERT/UPDATE/DELETE oc_cards ...
  INSERT oc_addressbookchanges ...
  UPDATE oc_addressbooks.synctoken ...
  INSERT INTO oc_dav_event_outbox (...)   -- the event payload
  SELECT pg_notify('oc_dav_event_outbox', <seq>)
COMMIT                                  -- NOTIFY is delivered on commit
```

Because the card write and the queue row share one transaction there is no
window in which a card exists without its event (or vice versa). `pg_notify`
is only a latency hint; the table is the source of truth.

The worker (`occ dav:event-dispatch`) claims rows with
`SELECT ... WHERE state = 0 ... FOR UPDATE SKIP LOCKED` and then dispatches the
event **and marks the rows done in one transaction**. That pairing is what
makes at-least-once transport into exactly-once effects.

---

## 2. The outbox table

Created by the app migration `Version1000Date20260918000000`. The schema is a
frozen interface with the Rust writer in `nextcloud-dav/src/`; do not rename
columns or change types. The table and index use the configured db prefix
(never a hardcoded `oc_`):

```sql
CREATE TABLE <prefix>dav_event_outbox (
  seq              bigserial   PRIMARY KEY,
  created_at       bigint      NOT NULL,   -- unix seconds (writer clock)
  event_type       smallint    NOT NULL,   -- 1=create 2=update 3=delete
  addressbookid    bigint      NOT NULL,
  card_uri         varchar(255) NOT NULL,
  card_row         text        NOT NULL,   -- JSON {id,uri,lastmodified,etag,size,uid}
  card_data        bytea       NOT NULL,   -- readBlob()-filtered carddata
  effects          text        NOT NULL,   -- JSON {"php":[...],"rust":[...]}
  state            smallint    NOT NULL DEFAULT 0,  -- 0 pending 1 claimed 2 done 3 dead
  attempts         smallint    NOT NULL DEFAULT 0,
  next_attempt_at  bigint      NOT NULL DEFAULT 0,
  reserved_by      varchar(64) NULL,
  reserved_at      bigint      NULL,
  processed_at     bigint      NULL,
  last_error       text        NULL
);
CREATE INDEX <prefix>dav_event_outbox_pending_idx
  ON <prefix>dav_event_outbox (state, next_attempt_at, seq);
```

`card_data` is `bytea`, not `text`: cards are valid up to `max-resource-size`
(5 MB) and there is no 32 000-char limit as in `oc_jobs.argument`. This is one
of the reasons a dedicated outbox is used instead of core's job list.

`effects` is the per-row ownership map. An effect id appears in exactly one of
`php` / `rust`. The worker dispatches the row when `php` is non-empty; when
`php` is empty the row belongs entirely to a native handler and is marked done
without dispatching. Phase 1 rows carry all built-in effects under `php`.

---

## 3. Install and enable

Copy the app into the Nextcloud `custom_apps` directory (the sidecar's PVC
already holds `custom_apps/`):

```sh
# from the sidecar repo
cp -a nextcloud-dav/app/nextcloud_dav /path/to/nextcloud/custom_apps/
# inside the Nextcloud container / install root:
php occ app:enable nextcloud_dav
# Optional: confirm the migration ran. `occ migrations:status` is not present in
# every supported release, so check the table itself (prefix-aware):
#   psql -c '\d oc_dav_event_outbox'
```

`occ app:enable` runs the migration, creating `<prefix>dav_event_outbox`. The
sidecar validates the table at startup and refuses native writes without it.

Upgrades are the normal app upgrade path (`occ upgrade`) and run later
`Version*` migrations.

**Uninstall** (`occ app:remove nextcloud_dav`) runs the `DropOutbox` repair
step, dropping `<prefix>dav_event_outbox` and reporting how many undelivered
rows were discarded.

---

## 4. Run the worker

```sh
php occ dav:event-dispatch \
  --batch=128 --idle-poll-ms=250 --stop-after=3600 --max-requests=10000
```

| option | default | meaning |
|---|---|---|
| `--batch` | 128 | rows per claim |
| `--idle-poll-ms` | 250 | safety-net poll; `LISTEN`/`NOTIFY` is the fast path |
| `--stop-after` | 3600 | exit after N seconds (bounded lifetime; supervisor restarts) |
| `--max-requests` | 10000 | exit after dispatching N rows |
| `--once` | off | claim + process a single batch, then exit (interpreter-per-batch fallback) |

Both `--batch`/`--idle-poll-ms` and `--stop-after`/`--max-requests` fall back
to `config.php` when not passed on the command line (see §6).

Start-up does once per process: `OC::boot()`/`initForRequest()` (already done
by `occ`), loads the `dav` app (registers `CardListener`, `BirthdayListener`,
`ClearPhotoCacheListener`), and instantiates `OC\Federation\CloudIdManager`
(its constructor registers the `CardUpdatedEvent` listener that performs the
Redis `cloud_id_` `DEL`). Skipping the last one silently disables that effect.

On start the worker prints either:

```
LISTEN oc_dav_event_outbox active; wake-on-commit with a 250 ms safety-net poll.
```

or, explicitly, when the fast path cannot be used:

```
LISTEN unavailable (<reason>); falling back to plain polling every 250 ms.
```

The reason is also logged at warning level. Fallback never changes
correctness — only latency.

Every batch then prints one status line:

```
pending=12 in_flight=0 dead=0 oldest_pending=3s processed_this_run=345
```

`SIGTERM`/`SIGINT` stop the worker **after the current batch** (the flag is
checked at the top of the loop); a deploy therefore never abandons a
half-dispatched batch.

---

## 5. Supervision in the pod

PHP lives in the `nextcloud` container; the **sidecar container is a static
musl binary on `alpine` and has no PHP**, so it cannot `exec` the worker.
Run the worker as a second container in the Nextcloud pod, using the same
Nextcloud image (and therefore the same config, database credentials and
mounted `custom_apps`/`config`):

```yaml
- name: nextcloud-dav-dispatch
  image: <same image as the nextcloud container>
  command: ["php", "occ", "dav:event-dispatch",
            "--batch=128", "--idle-poll-ms=250",
            "--stop-after=3600", "--max-requests=10000"]
  workingDir: /var/www/html
  env:
    - name: NEXTCLOUD_CONFIG_DIR
      value: /var/www/html/config
  # The same PVC mounts as the nextcloud container.
```

Notes:

- The container must run as the same uid as `config/config.php` (the `occ`
  bootstrap checks this) and see the same `config/` and `custom_apps/`.
- Do **not** schedule it from cron: `--stop-after`/`--max-requests` make the
  process bounded, and the container restart policy is the supervisor. Several
  replicas are safe: claiming uses `FOR UPDATE SKIP LOCKED`.
- Alert on `pending` growth / `oldest_pending` age and on `dead > 0` (the
  status line, the admin panel in §7, and SQL in §8 all expose them).
- Interpreted-per-batch alternative: run `occ dav:event-dispatch --once` from
  the sidecar's supervisor instead of a resident process. Correct, but it pays
  the Nextcloud bootstrap per batch — keep `--batch` ≥ 64.

---

## 6. Configuration knobs

All under `config.php` (shared with the Rust sidecar, which parses the same
keys):

```php
'nextcloud_dav' => [
  'event_dispatch' => [
    'enabled'                => true,
    'batch_size'             => 128,
    'idle_poll_ms'           => 250,
    'max_attempts'           => 8,
    'backoff_ms'             => [100, 500, 2000, 10000, 30000, 60000, 120000],
    'claim_timeout_s'        => 300,    // re-claim a dead worker's state=1 rows
    'dead_letter_keep_days'  => 30,     // janitor: delete done rows older than this
    'notify_channel'         => 'oc_dav_event_outbox',
    'php_generic_dispatch'   => true,   // phase 1; generic dispatchTyped(Event)
    'handlers'               => ['redis_cloud_id' => 'php'], // phase 2: 'rust'
    'php_worker' => [
      'enabled'       => true,
      'max_requests'  => 10000,
      'stop_after_s'  => 3600,
    ],
  ],
],
```

`notify_channel` defaults to the literal `oc_dav_event_outbox` on both sides
(the Rust producer's `DEFAULT_EVENT_NOTIFY_CHANNEL` is literal too, so it is
independent of the table prefix). It can be overridden in this block; both
sides must then be given the same value. The table itself is always addressed
through `dbtableprefix`, so it follows a custom prefix.

A background job `OCA\NextcloudDav\Cron\OutboxJanitor` (every 10 minutes) does
retention and re-arms stale reservations:

- delete `state = 2` rows with `processed_at < now - dead_letter_keep_days·86400`;
- `state = 1 AND reserved_at < now - claim_timeout_s` → `state = 0`, clearing
  the reservation. `attempts` is deliberately **not** incremented here: a
  killed worker is an infrastructure event, not a poison event, and bumping it
  would let a crash-looping pod dead-letter healthy rows.

---

## 7. Observability

- Worker status line (§4).
- Admin panel **Settings → Administration → Groupware → "Nextcloud DAV event
  outbox"**: pending, in-flight, dead-letter counts and the oldest pending age.
- SQL below.

---

## 8. Inspecting and requeuing dead letters

Dead letters are `state = 3` (retried `max_attempts` times). Inspect:

```sql
SELECT seq, to_timestamp(created_at) AS created, event_type, addressbookid,
       card_uri, attempts, to_timestamp(processed_at) AS died, last_error
FROM   oc_dav_event_outbox
WHERE  state = 3
ORDER  BY seq;
```

Requeue one row (or all of them) after fixing the cause:

```sql
-- one row
UPDATE oc_dav_event_outbox
SET    state = 0, next_attempt_at = 0, attempts = 0, last_error = NULL,
       reserved_by = NULL, reserved_at = NULL, processed_at = NULL
WHERE  seq = <seq> AND state = 3;

-- everything dead
UPDATE oc_dav_event_outbox
SET    state = 0, next_attempt_at = 0, attempts = 0, last_error = NULL,
       reserved_by = NULL, reserved_at = NULL, processed_at = NULL
WHERE  state = 3;
```

The next claim picks the rows up (or the worker is woken by the next write).
A row that dead-letters again had a genuine poison payload; the `last_error`
column carries the exception message.

General backlog / health queries:

```sql
SELECT state, count(*), min(to_timestamp(created_at)) AS oldest
FROM   oc_dav_event_outbox GROUP BY state ORDER BY state;

-- a stuck worker: state=1 reservations older than the claim timeout
SELECT seq, reserved_by, to_timestamp(reserved_at) AS reserved_at
FROM   oc_dav_event_outbox
WHERE  state = 1 AND reserved_at < extract(epoch FROM now()) - 300;
```

Replace `oc_` with the configured `dbtableprefix`.

---

## 9. The exactly-once argument

The outbox gives **at-least-once delivery** plus **idempotent effects**, which
together are exactly-once for every database effect:

1. **Nothing is lost.** The card write and the outbox row commit atomically.
   Killing the sidecar between `COMMIT` and `pg_notify` loses only the wake-up
   hint; the row is still there and the 250 ms poll (or the next write) picks
   it up.
2. **No duplicate effects.** A claimed batch is dispatched and marked done in
   the **same transaction**. If the worker dies mid-batch, the transaction
   rolls back: the card effects are not committed and the rows remain claimed;
   after `claim_timeout_s` they are re-armed and replayed. The replay can only
   produce the effects that the failed attempt had already produced *if* those
   effects were non-transactional — the Redis `cloud_id_` `DEL` and the photo
   cache delete. Both are idempotent by construction (a second `DEL` is a
   no-op; the photo-cache delete swallows `NotFoundException`). The activity
   rows, birthday calendar rows and reminders are written inside the same
   transaction as the `state = 2` update, so they appear exactly once.
3. **No double dispatch between backends.** Ownership is frozen at write time
   in `effects`: an effect id is in exactly one of `php` / `rust`. A row with
   an empty `php` list is marked done without dispatching, so the PHP worker
   and a native handler never both apply the same effect.
4. **A poison row cannot block the queue.** After `max_attempts` the row moves
   to `state = 3` and is excluded from claims.

Ordering: rows are claimed in `seq` order, preserving per-card order; no
global ordering is promised across independent claims.

---

## 10. Acceptance tests

Run these against a disposable instance.

**T1 — kill the worker mid-batch, nothing lost, no duplicates.**
Seed a batch with a card that produces a non-idempotent effect (activity
stream) and one that produces a transactional effect (birthday calendar):
1. Start the worker; while it is dispatching, `kill -9` it.
2. Observe rows in `state = 1` with a stale `reserved_at`.
3. Assert the *effects* of the interrupted batch are absent
   (`SELECT count(*) FROM oc_activity WHERE object_type='addressbook' AND
   object_id=<ab>` unchanged) — the transaction rolled back.
4. Re-claim: either wait for the janitor/claim timeout, or `UPDATE ... SET
   state=0 WHERE state=1`. Restart the worker.
5. Assert the effects now exist exactly once and the rows are `state=2`.

**T2 — kill the sidecar between commit and notify, nothing lost.**
1. Make the sidecar commit a card write but drop before `pg_notify` (stop the
   container right after the write returns, or point `notify_channel` at a
   channel the worker does not listen on).
2. Assert the outbox row exists with `state = 0`.
3. With no further writes, the worker's poll (≤ `idle_poll_ms`) must dispatch
   it and mark it `state = 2`. The effects exist exactly once.

**T3 — crash between effects and completion.**
1. Force the batch to fail after some effects have run (e.g. a listener
   throws). The transaction rolls back.
2. Assert no new `oc_activity` rows for that card and that the row went back to
   `state = 0` with `attempts = 1` and `next_attempt_at` in the future.
3. Assert the retry eventually dispatches the effect exactly once; a payload
   that always fails reaches `state = 3` after `max_attempts`.

**T4 — concurrent workers.**
Start two workers. Total dispatched effects must equal the number of outbox
rows; no row is processed by both (verified through `reserved_by` /
`processed_at` and the effect counts). `FOR UPDATE SKIP LOCKED` is what makes
this hold.

**T5 — retention.**
Insert a `state = 2` row with an old `processed_at`, run the janitor
(`occ background-job:execute ... ` or wait for cron), assert it is deleted;
insert a stale `state = 1` row and assert it is re-armed to `state = 0` with
`attempts` unchanged.

---

## 11. Why the dav dependency is not in info.xml

Nextcloud's `info.xml` has no app-to-app dependency element (verified against
`resources/app-info.xsd` and the appstore schema), so the hard dependency on
the shipped `dav` app is enforced at runtime: `EventDispatch` loads `dav`,
fails with a clear error if it is missing or too old, and checks that
`OCA\DAV\Events\CardCreatedEvent` exists. `info.xml` carries a comment saying
the same.

---

## 12. What the worker does not do

- It does not write the card or the outbox row; that is the Rust sidecar's job.
- `php_generic_dispatch = false` (selective PHP invocation / mixed ownership)
  is designed in `recon/event-dispatch.md` §3.4 but not implemented here. The
  worker still dispatches generically; phase 1 rows never have native owners,
  so this is not yet observable.
- It does not re-check the address book snapshot: `getAddressBookById()` and
  `getShares()` are read at dispatch time (the frozen schema carries no
  snapshot). A change to the address book between the write and dispatch is
  therefore reflected.