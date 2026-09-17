# Recon: asynchronous side-effect dispatch for `nextcloud-dav` native writes

Status: design recon, 2026-09-17. Companion to
[`card-events.md`](./card-events.md) (what the listeners do) and
[`../ARCHITECTURE.md`](../ARCHITECTURE.md) (how the sidecar is deployed).

All `file:line` references are relative to `nextcloud-server` (Nextcloud
36-dev, HEAD `d077e686132`) unless marked **external**. Sidecar references are
relative to `../src/`.

---

## 0. The problem in invariants

Today the side effects of a card write are produced by PHP **inside the write
transaction** ([`card-events.md`](./card-events.md) §0):

- `CardDavBackend::createCard/updateCard/deleteCard` dispatch
  `Card{Created,Updated,Deleted}Event` (`apps/dav/lib/CardDAV/CardDavBackend.php:689`,
  `:752`, `:836`) from inside `atomic()`.
- `BirthdayListener` and `ClearPhotoCacheListener` have **no** `try/catch`
  (`apps/dav/lib/Listener/BirthdayListener.php:33-38`,
  `apps/dav/lib/Listener/ClearPhotoCacheListener.php:29-31`), so a failure in
  either rolls back the card write and fails the client request. `CardListener`
  swallows `Throwable` (`apps/dav/lib/Listener/CardListener.php:34,52,70`).

For a sidecar that performs `PUT`/`DELETE` itself, "still produce the side
effects" must hold these invariants:

- **I1 — the card write is never blocked or rolled back by a side effect.**
- **I2 — every committed card write eventually produces its effects** (no lost
  events, including a crash between the card `COMMIT` and any enqueue).
- **I3 — every effect is produced at most once** (no duplicates; no double
  dispatch when a native handler and the PHP path are both configured).
- **I4 — effects are batched and owned by the sidecar, not by Nextcloud cron.**
- **I5 — sub-second-to-low-seconds freshness** after the `PUT`/`DELETE` response.

I2 + I3 together are "exactly-once from the writer's point of view" and are the
reason the outbox is not optional (see §4).

---

## 1. Recommended architecture

**Transactional outbox in PostgreSQL + a modular dispatcher with pluggable
backends; phase 1 delegates every effect to a single resident PHP CLI worker.**

```
                                   one DB transaction
Rust sidecar (native PUT/DELETE) ─┬─ oc_cards (+ oc_cards_properties)
  producer                        ├─ oc_addressbookchanges
                                  ├─ oc_addressbooks.synctoken
                                  └─ oc_dav_event_outbox  (1 row)
                                         │  commit  ──► HTTP 201/204 to client
                                         │  pg_notify('oc_dav_event_outbox', seq)  (same txn)
                                         ▼
                       ┌───────────────────────────────────────────────┐
                       │ dispatcher: claim batch FOR UPDATE SKIP LOCKED │
                       │   per row → effects owned by each backend      │
                       │   effects + "done" update in ONE transaction   │
                       └───────────────┬───────────────┬───────────────┘
                                       │               │
                          backend "php" (phase 1)   backend "rust" (phase 2+)
                          resident CLI daemon       in-process handlers
                          in the nextcloud pod      (redis_cloud_id, …)
                                       │
                     ┌─────────────────┴──────────────────┐
                     │ generic: dispatchTyped(Card*Event)  │  phase 1
                     │ or selective: call owned services   │  mixed mode
                     └────────────────────────────────────┘
```

### 1.1 The cross-container constraint (why the sidecar cannot "exec PHP")

The sidecar ships as a static musl binary executed from an **`alpine`**
container; PHP lives in the `nextcloud` container
(`../docs/ARCHITECTURE.md` §2, §12). The sidecar therefore **cannot** `exec`
the PHP worker directly. "Owned and handled by the sidecar" must be read as:

- the sidecar **owns the queue**: it is the sole producer, it defines the
  handler registry/ownership map, and it runs native handlers in-process;
- the PHP-only effects are executed by a **dedicated resident PHP process in
  the `nextcloud` container**, started and supervised once (not by cron, not
  per cron tick). The sidecar wakes it transactionally with `pg_notify`.

If the deployment later switches the PHP runtime to FrankenPHP, the same
outbox can be drained by a FrankenPHP worker thread instead of a CLI daemon
(§6 row 2), without changing the producer or the schema.

### 1.2 Producer (Rust) — what changes

In the single `BEGIN … COMMIT` that already writes `oc_cards`,
`oc_addressbookchanges`, `oc_addressbooks.synctoken` and
`oc_cards_properties`, add:

1. Build the event payload exactly as PHP would have handed it to the
   listeners. This matters: `CardDavBackend::getCard()`
   (`apps/dav/lib/CardDAV/CardDavBackend.php:525`) applies `readBlob()` before
   dispatch, which strips non-image `PHOTO:data:` and may rewrite
   `size`, and it **quotes** `etag`. The outbox payload must be that
   post-`readBlob`, quoted row, not the raw table row.
2. `INSERT INTO oc_dav_event_outbox (…)` with the payload plus the
   per-effect ownership map (§3.4), computed from the static handler registry.
3. `SELECT pg_notify('oc_dav_event_outbox', <seq>)` in the same transaction
   (delivered only on commit).

The listeners **must not** run in this transaction. That directly fixes the
recon §0 hazard: a missing mid-transaction failure can no longer roll back the
card.

### 1.3 Dispatcher

A single logical consumer per instance:

1. **Claim** a batch with
   `SELECT … WHERE state = 0 AND next_attempt_at <= now() ORDER BY seq
   LIMIT $batch FOR UPDATE SKIP LOCKED`, then set `state = 1, reserved_at = now(),
   attempts = attempts + 1` for the claimed ids; commit. `SKIP LOCKED` makes
   several workers safe.
2. **Process** each row by invoking the effects listed under this backend in
   the row's ownership map, then set `state = 2, processed_at = now()` for the
   batch. **The effect writes and the completion update are in one
   transaction** — this is what turns "at-least-once delivery" into
   exactly-once for every DB effect.
3. **Retry**: on exception, roll the batch back and set `state = 0,
   next_attempt_at = now + backoff[attempts], last_error = …`; after
   `max_attempts`, `state = 3` (dead-letter). Dead rows are excluded from
   claims, so a poison row cannot block the queue.
4. **Wake** on `LISTEN oc_dav_event_outbox` and, as a safety net, poll every
   `idle_poll_ms`.

### 1.4 Fallback architecture (if a resident PHP process cannot be deployed)

If operations refuse a supervised long-lived PHP process, use
**interpreter-per-batch**: the sidecar (or a tiny supervisor) invokes a PHP CLI
entrypoint that bootstraps Nextcloud, drains exactly one batch, and exits; the
sidecar continues to own the queue. Costs are quantified in §6 row 4 and §5.
This is strictly worse in fixed cost but needs no new supervisor and no
resident process to monitor. It keeps I1–I3 and I5 (with a larger constant).

A second fallback, if the instance already runs FrankenPHP worker mode, is an
internal HTTP dispatch endpoint served by a dedicated `num 1` worker; the
sidecar POSTs the batch after commit. This trades CLI bootstrap for a
FrankenPHP request and avoids a new OS process.

---

## 2. Non-goals / explicit scope

- This document does **not** specify the Rust write path itself
  (`oc_cards`/`oc_addressbookchanges`/`oc_cards_properties`/`synctoken`); it
  assumes that exists and is transactional.
- It does not specify the birthday VEVENT/reminder algorithm; that stays in
  PHP (`card-events.md` §6.2).
- It does not cover CalDAV events; the same pattern applies but is out of
  scope.

---

## 3. Concrete interface

### 3.1 Outbox schema (DDL, PostgreSQL)

Table name is `<prefix>dav_event_outbox` (the sidecar's `database_prefix`).
Owned by a companion Nextcloud app `nextcloud_dav` migration, so `occ
db:add-missing-indices`/upgrades manage it; the sidecar validates its presence
and shape at startup and refuses to take native writes without it.

```sql
CREATE TABLE oc_dav_event_outbox (
  seq              bigserial   PRIMARY KEY,          -- global dispatch order
  created_at       bigint      NOT NULL,             -- unix seconds (writer clock)
  event_type       smallint    NOT NULL,             -- 1=create 2=update 3=delete
  addressbookid    bigint      NOT NULL,
  card_uri         varchar(255) NOT NULL,
  card_uid         varchar(255) NULL,
  -- Event payload, byte-exact as PHP would pass it to the listeners.
  card_data        bytea       NOT NULL,             -- post-readBlob carddata
  card_row         jsonb       NOT NULL,             -- {id,addressbookid,uri,lastmodified,etag,size,uid}
  addressbook_data jsonb       NOT NULL,             -- CardDavBackend::getAddressBookById()
  shares           jsonb       NOT NULL,             -- CardDavBackend::getShares()
  -- Ownership: an effect id appears in exactly one backend list (see §3.4).
  effects          jsonb       NOT NULL,
  -- Lifecycle.
  state            smallint    NOT NULL DEFAULT 0,   -- 0 pending 1 claimed 2 done 3 dead
  attempts         smallint    NOT NULL DEFAULT 0,
  next_attempt_at  bigint      NOT NULL DEFAULT 0,
  reserved_by      varchar(64) NULL,
  reserved_at      bigint      NULL,
  processed_at     bigint      NULL,
  last_error       text        NULL
);

CREATE INDEX dav_event_outbox_pending_idx
  ON oc_dav_event_outbox (state, next_attempt_at, seq);
```

Notes:

- `card_data` is `bytea`, not `text`: unlike `oc_jobs.argument` there is no
  32 000-char limit (`lib/private/BackgroundJob/JobList.php:33,58-60`) and
  cards are valid up to `max-resource-size` (5 MB,
  `src/config.rs:MAX_RESOURCE_SIZE`).
- `addressbook_data` / `shares` are snapshots taken at write time (matching
  PHP, which reads them inside the same transaction at
  `CardDavBackend.php:686-688`, `:748-750`, `:816-818`), not re-fetched at
  dispatch time. This keeps the effect deterministic and independent of later
  address-book edits.
- For a delete, `card_data`/`card_row` are the pre-delete values, matching PHP's
  `getCard()` before `DELETE` (`CardDavBackend.php:816-818`).
- `pg_notify` payload is just `seq`; it is a latency hint, never the source of
  truth.

### 3.2 Producer pseudocode (Rust)

```rust
// inside the existing card-write transaction
let card_row   = read_blob_filtered_row(&mut tx, ab_id, uri).await?; // matches getCard()
let ab_data    = address_book_by_id(&mut tx, ab_id).await?;
let shares     = get_shares(&mut tx, ab_id).await?;
let effects    = registry.effects_for(event_type); // {"php":[...],"rust":[...]}

sqlx::query("INSERT INTO oc_dav_event_outbox
   (created_at,event_type,addressbookid,card_uri,card_uid,card_data,
    card_row,addressbook_data,shares,effects)
   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING seq")
  .bind(now_unix()).bind(op as i16).bind(ab_id).bind(uri).bind(uid)
  .bind(&card_data).bind(&card_row_json).bind(&ab_json).bind(&shares_json)
  .bind(&effects_json)
  .fetch_one(&mut *tx).await?;   // capture seq

sqlx::query("SELECT pg_notify('oc_dav_event_outbox', $1)")
  .bind(seq.to_string()).execute(&mut *tx).await?; // transactional; fires on commit

tx.commit().await?;            // then the HTTP response
```

### 3.3 Worker loop (PHP, resident CLI daemon)

New `occ` command plus a `worker` mode, modelled on the existing resident
worker `core/Command/Background/JobWorker.php` (which loops forever,
`usleep(50000)` between checks, and does `setupManager->tearDown()` +
`tempManager->clean()` after each job, `JobWorker.php:105-125`).

```php
// occ dav:event-dispatch --batch=128 --idle-poll-ms=250 --stop-after=3600
\OC::boot();
\OC::initForRequest();                 // once per process, not per row
$appManager = Server::get(IAppManager::class);
$appManager->loadApp('dav');           // registers CardListener/Birthday/ClearPhotoCache
Server::get(CloudIdManager::class);    // registers the card-updated listener (see §3.4)

while (!$stopping) {
    $rows = claim_batch($batch);       // FOR UPDATE SKIP LOCKED + state=1
    if (!$rows) { listen_or_sleep($idlePollMs); continue; }

    try {
        Server::get(IDBConnection::class)->beginTransaction();
        foreach ($rows as $row) {
            dispatch_effects($row);    // §3.4: generic or selective
        }
        mark_done(array_column($rows, 'seq'));   // same transaction
        Server::get(IDBConnection::class)->commit();
    } catch (Throwable $e) {
        Server::get(IDBConnection::class)->rollBack();
        schedule_retry($rows, $e);     // backoff, or state=3 after max_attempts
    }
    // per-batch hygiene, cf. JobWorker.php:117-121
    Server::get(ISetupManager::class)->tearDown();
    Server::get(ITempManager::class)->clean();
    gc_collect_cycles();
    if (++$handled >= $maxRequests) break;   // bounded lifetime; supervisor restarts
}
```

Why bootstrap once and not per row: `OC::initForRequest()` starts with
`resetStaticProperties()` and builds a fresh `\OC\Server`
(`lib/OC.php`, `initForRequest`), which is cheap (dav app boot ≈ **1.3 ms**,
`dav-bench/PROFILING.md:19`; `bootstrap:register_apps` ≈ **22 ms**,
`PROFILING.md:17`; `connect:db` ≈ **169 ms**, `PROFILING.md:48`) but not free.
Per row it would dominate for cheap rows. `OC::initForRequest()` also
accumulates `register_shutdown_function` callbacks, so it must **not** be
called per row.

### 3.4 Modular handler registry — exactly-one-owner

An **effect id** is the unit of ownership. Owners are declared once, in the
sidecar's config, and frozen into each outbox row's `effects` map at write
time. An effect id may appear in **exactly one** backend list, so double
dispatch is structurally impossible for effects declared in the registry.

| effect id | phase 1 owner | phase 2+ owner | what it is |
|---|---|---|---|
| `activity_stream` | php | php (native later) | `oc_activity` rows (`CardListener`) |
| `activity_mail` | php | php | `oc_activity_mq` rows (Activity app consumer, external) |
| `notification_push` | php | php | notifications app + `notify_push` (external) |
| `birthday_calendar` | php | php | `BirthdayListener` → `BirthdayService` |
| `calendar_reminders` | php | php | cascaded from birthday (`ReminderService`) |
| `photo_cache` | php | php | `ClearPhotoCacheListener` → `PhotoCache::delete` |
| `redis_cloud_id` | php | **rust** (phase 2) | `CloudIdManager::handleCardEvent` DELs |

Phase 1 row: `{"php":["activity_stream","activity_mail","notification_push",
"birthday_calendar","calendar_reminders","photo_cache","redis_cloud_id"],
"rust":[]}`.
Phase 2 row (example): `{"php":[…minus redis_cloud_id…],"rust":["redis_cloud_id"]}`.

Two dispatch modes in the PHP backend:

- **generic** (`effects.php` = all built-ins): reconstruct the real event and
  `Server::get(IEventDispatcher::class)->dispatchTyped(new CardCreatedEvent(
  $abId, $abData, $shares, $cardRow))`. All registered listeners run, including
  third-party ones, exactly as in a request. This is phase 1.
- **selective** (mixed ownership): call only the PHP-owned dav services
  directly — `Activity\Backend::triggerCardActivity()`,
  `BirthdayService::onCardChanged()/onCardDeleted()`, `PhotoCache::delete()` —
  and do **not** generic-dispatch. This is the only mode that guarantees
  `redis_cloud_id` is not DEL'd twice. The cost is that third-party
  `Card*Event` listeners are not invoked (§7).

The registry must include `CloudIdManager::handleCardEvent` as a known
listener: it is registered in `CloudIdManager::__construct()`
(`lib/private/Federation/CloudIdManager.php:42`), so in generic mode the
service must be instantiated once in the worker, otherwise the Redis DEL
silently does not happen.

### 3.5 Config knobs

In `config.php` under `nextcloud_dav` (parsed by the sidecar, `src/config.rs`),
with env/CLI overrides:

```php
'nextcloud_dav' => [
  'event_dispatch' => [
    'enabled'             => true,
    'batch_size'          => 128,
    'idle_poll_ms'        => 250,     // safety-net poll; NOTIFY is the fast path
    'max_attempts'        => 8,
    'backoff_ms'          => [100, 500, 2000, 10000, 30000, 60000, 120000],
    'claim_timeout_s'     => 300,     // a crashed worker's rows are re-claimable
    'dead_letter_keep_days' => 30,
    'notify_channel'      => 'oc_dav_event_outbox',
    'php_generic_dispatch'=> true,    // phase 1; false once native claims exist
    'handlers'            => [        // effect id => backend; single owner
        'redis_cloud_id' => 'php',    // phase 2: 'rust'
    ],
    'php_worker' => [
      'enabled'      => true,
      'max_requests' => 10000,        // bounded lifetime, supervisor restarts
      'stop_after_s' => 3600,
    ],
  ],
  'redis' => [ /* inherited from config.php 'redis'/'memcache.distributed' */ ],
],
```

---

## 4. Duplication avoidance and crash-consistency

### 4.1 Crash between commit and enqueue — solved by the outbox

The classic failure is: card transaction commits, then the producer crashes
before writing the queue entry, so the effect is lost forever. Writing the
outbox row **inside the card transaction** makes the two atomic: either both
the card and the queue row exist, or neither does. There is no window. This is
the single decisive reason to use a table in the *same database* as the card
write, and it rules out Redis/in-process/file transports as the durable queue
(§6 row 5).

A second crash window — after a batch's effects are applied but before the
"done" update — is closed by putting the effect writes and the completion
update in one transaction (§1.3): rollback replays the whole batch, commit
loses nothing. Non-transactional effects (Redis `DEL`, appdata file unlink)
are idempotent, so a replay is harmless.

### 4.2 Preventing double dispatch (native + PHP both enabled)

- **Static ownership.** The producer writes one ownership list per effect;
  each effect id is in exactly one backend's list. The PHP worker's selective
  mode calls only its own effects; the Rust backend runs only its own. An
  effect cannot be produced by both.
- **Frozen at write time.** Rows carry their own `effects` map, so changing
  the registry does not retroactively re-route in-flight rows.
- **Idempotent-by-construction effects.** `redis_cloud_id` (DEL) and
  `photo_cache` (recursive delete, `NotFoundException` swallowed,
  `PhotoCache.php:267-274`) are naturally idempotent, so even a buggy
  double-run is invisible. `activity_stream` is the one non-idempotent effect,
  which is why it is **not** native-claimed until phase 3 and never runs in
  both backends at once.
- **Phase 3 hardening** (only needed if a native handler and PHP must coexist
  for a non-idempotent effect): an `oc_dav_effect_log(seq, effect_id,
  applied_at)` row written inside the same transaction before the effect; both
  backends check-and-insert, so the loser no-ops.

---

## 5. Batching and latency

Producer cost: one `INSERT` + one `pg_notify` per card write, both inside the
existing transaction — well under 1 ms, invisible next to the card write.

Worker cost, per the measured profile
(`dav-bench/PRODUCTION.md:28`, `RESULTS.md:50`, `PROFILING.md:17-19,48`):

| stage | measured / estimated | source |
|---|---|---|
| PHP interpreter bootstrap only | ~10–15 ms | `RESULTS.md:54` |
| `status.php` boot (nothing loaded) | 0.08–0.10 s | `PRODUCTION.md:28` |
| web boot with 85 apps | ~0.50 s | `PRODUCTION.md:28` |
| `occ status` CLI bootstrap (all commands) | ~1.1 s | `PRODUCTION.md:28` |
| `connect:db` | ~169 ms | `PROFILING.md:48` |
| `bootstrap:register_apps` | ~22 ms | `PROFILING.md:17` |
| `boot_app:dav` | ~1.3 ms | `PROFILING.md:19` |

Proposed defaults and resulting latency:

- `batch_size = 128`, `idle_poll_ms = 250`.
- Typical row (no `BDAY`, no photo): reconstruct + dispatch ≈ 1–5 ms.
- Heavy row (birthday VEVENT + reminder index + cascaded activity): tens of ms.
- p50 side-effect latency ≈ poll/notify + small drain ≈ **0.2–0.4 s**.
- p99 ≈ 250 ms + drain of a batch with ~10 % heavy rows ≈ **~1 s**. This is the
  target to hold.
- Under load: the worker never idles; latency = backlog ÷ throughput. Add
  workers (N daemons) relying on `FOR UPDATE SKIP LOCKED`; per-card ordering is
  preserved (one row per event), cross-card ordering is not required.
- Worker down: card writes still succeed (I1 holds, better than today); the
  outbox grows, bounded only by disk. Alert on
  `max(now - created_at) WHERE state = 0` and on `count(state = 3)`.
  On restart the backlog drains; poison rows dead-letter after `max_attempts`.

The `LISTEN`/`NOTIFY` fast path is transactional (`NOTIFY` is delivered only on
commit), needs no Redis, and is already available since the sidecar holds a
Postgres pool. With it, the 250 ms poll is only a safety net.

---

## 6. Decision table

| # | option | pros | cons | evidence | verdict |
|---|---|---|---|---|---|
| 1 | **Resident PHP CLI worker** (boot once, dispatch many) | Real precedent: `background-job:worker` is an infinite loop with per-job teardown (`core/Command/Background/JobWorker.php:88-133`); `OC::initForRequest()` is explicitly written to be called repeatedly in a worker ("Called before each request served if the same worker serves several request") and resets statics (`lib/OC.php`); bootstrap amortised across a batch; a plain `occ` command, easy to supervisor. | State can leak between rows (upstream still tracks this: `nextcloud/server#58807` "Remove static variables"); memory growth bounded only by `memory_limit`/`MAX_REQUESTS`; long-lived DB connection needs reconnect handling; a new process to supervise; `initForRequest()` per row would accumulate shutdown handlers, so it must be per-process/per-batch. | `JobWorker.php:88-133`; `lib/OC.php` `handleRequests`/`initForRequest`/`resetStaticProperties`; web-boot ~0.50 s vs CLI ~1.1 s (`PRODUCTION.md:28`) | **Recommended PHP backend** |
| 2 | **FrankenPHP worker mode** | Upstream Nextcloud **does** ship it: an experimental `Caddyfile` (SPDX "Nextcloud GmbH", commits `689196b1d37`…`ea512eabb0b`, PR #58541 merged `cc48ec44206`, PR #61115 `aab07d36936`) and `OC::handleRequests()` looping on `frankenphp_handle_request()` (`lib/OC.php:1383-1398`, called from `index.php:25`, `remote.php:102`, `ocs/v1.php:35`); opcache/app warm; graceful restarts via admin API/watch. | Request-driven: the documented API is `frankenphp_handle_request($handler)`, so a *background consumer* needs either an HTTP dispatch endpoint or a worker script that never calls it (undocumented, blocks the thread); upstream still lists "Remove static variables" and "Detect open DB transactions at the end of a request" as **critical** prerequisites (`nextcloud/server#58807`); measured on this instance it removes only ~10–15 ms of a ~200 ms fixed cost, and worker mode was **neutral-to-worse** on listings (possible incomplete static reset, `RESULTS.md:50-56`). | `lib/OC.php:1383-1398`; `Caddyfile`; commits above; `nextcloud/server#58807`; `RESULTS.md:50-56`; frankenphp.dev/docs/worker/ | **Viable as a request-driven dispatch endpoint; not the primitive for a background queue.** Keep as phase-3 option, not phase 1. |
| 3 | **Embedded libphp in the Rust binary** (`ext-php-rs` / `php-embed` / C FFI) | Would remove cross-container and IPC entirely; effects could run in-process. | `ext-php-rs` is a *PHP-extension* toolkit — "Bindings and abstractions for the Zend API to **build PHP extensions** natively in Rust", installed with `cargo php install` into a PHP SAPI; its module list has **no `embed` module** (only `php_eval`, "Execute embedded PHP code **within a running PHP extension**") — i.e. Rust-inside-PHP, not PHP-inside-Rust. The embed SAPI is NTS/single-threaded, so every PHP call would have to serialise on one OS thread in a multi-threaded tokio service (PHP internals list: "Thread safeness is not a problem for a single thread… tsrm_ls … global vars … are not protected"); loaded PHP code can never be unloaded, so a long-lived process grows monotonically; a static libphp for musl with Nextcloud's full extension set (`pdo_pgsql`, `intl`, `gd`, `redis`, `ldap`, `zip`, `mbstring`, …) is not a supported build; and the sidecar's alpine container has no libphp to link against. Nextcloud-in-worker-mode is *still* experimental even under a real, maintained SAPI. No evidence found of anyone embedding Nextcloud in a Rust host. | docs.rs `ext_php_rs` module list (no `embed`); README "build PHP extensions", `cargo php install`; PHP internals thread on embed NTS/tsrm; `nextcloud/server#58807` | **Dead end** for the full Nextcloud app |
| 4 | **Interpreter-per-batch** (spawn a PHP CLI entrypoint that drains N rows and exits) | Simplest to deploy: no resident process, no supervisor, crash-isolated by process exit, trivially gets the latest code/config; still satisfies I1–I4 (the sidecar owns the queue). | Pays a full Nextcloud bootstrap per batch. Measured bounds: floor `status.php` = 0.08–0.10 s (nothing loaded), ceiling `occ status` = ~1.1 s (full console/command load); a purpose-built entrypoint avoiding `console.php`'s command loading should land ~0.1–0.5 s. At batch = 128 that is ~0.8–4 ms/row fixed — acceptable; at batch = 8 it is 12–60 ms/row — bad. Latency adds the bootstrap to every drain, so small/frequent batches are the worst case. | `PRODUCTION.md:28`; `INFRA.md:16` (CLI bootstrap 121 s → 1.1 s); `RESULTS.md:50`; `console.php:1-100` | **Fallback**, not primary. Use if a resident process is unacceptable; keep `batch_size ≥ 64`. |
| 5 | **Queue transport: Redis** | Already present in the deployment (used for locking/cache; `PRODUCTION.md` reports Redis at 11 mCPU / 6 MiB); `notify_push` already integrates PHP↔Rust through it (`nextcloud/notify_push` README: "requires a redis server", config from `config.php`, systemd daemon); sub-ms `LPUSH`/`BRPOPLPUSH`; easy wakeups/pubsub. | **Cannot be enlisted in the card-write transaction** — a crash between Postgres `COMMIT` and `LPUSH` loses the event (violates I2), and a crash between `LPUSH` and `COMMIT` dispatches a phantom event; needs its own durability/idempotency story (streams + consumer groups/`XAUTOCLAIM`, or a dedup set). The sidecar currently **does not connect to Redis** (`../docs/ARCHITECTURE.md` §2: "optional: reach PHP"; no Redis client in `Cargo.toml`), so this adds a second stateful dependency and credentials. | `../docs/ARCHITECTURE.md` §2; `PRODUCTION.md` (Redis usage); `nextcloud/notify_push` README; `lib/private/Federation/CloudIdManager.php:39` | **Not the durable queue.** Use optionally as a latency hint only (phase 3). |
| 5a | **Queue transport: in-process channel** | Zero infrastructure, lowest latency, trivial code. | Ephemeral: lost on sidecar restart and on pod restart (violates I2). Cannot be atomic with the DB commit. No cross-process coordination, no retry/dead-letter, no observability after restart. | `../docs/ARCHITECTURE.md` §12 (binary swap + container restart is routine) | **Rejected as the queue.** Acceptable only as an in-memory hand-off between the producer and a *native* handler when the durable outbox already exists. |
| 5b | **Queue transport: file/journal on the PVC** | Simple, survives process restart, no new service. | Not transactional with Postgres (same crash window as Redis); single-writer fsync semantics are fiddly; no atomic claim across workers, no `SKIP LOCKED`, no SQL observability; the PVC is shared with the mostly-read-only Nextcloud data, making journal compaction/pruning a new operational concern. | `../docs/ARCHITECTURE.md` §2, §12 | **Rejected.** |
| 5c | **Queue transport: `oc_jobs` (core table)** | Already exists, already has a claim/reservation protocol (`JobList.php:175-255`). | Owned by core and **polled by cron**, which the task forbids; `JobList::add()` silently **dedups on (class, argument_hash)** (`JobList.php:63-83`), which would merge two identical card events; argument capped at **32 000 chars** (`JobList.php:33,58-60`), smaller than a card; mixing our events into cron's queue couples latency and failure modes to core. | `lib/private/BackgroundJob/JobList.php:33,58-83,175-255` | **Rejected.** |
| 5d | **Queue transport: dedicated Postgres outbox table** | The **only** transport that can be written in the same transaction as the card (`INSERT … ; SELECT pg_notify(…)` — delivered on commit), which is exactly what I2 needs; no new service, no new credentials (the sidecar already holds a DB pool, `src/main.rs`); `FOR UPDATE SKIP LOCKED` gives safe multi-worker claiming; `state`/`attempts`/`next_attempt_at`/`last_error`/`seq` give retry, dead-letter, ordering and SQL observability for free; `bytea` holds full cards; `pg_notify` + `LISTEN` gives Redis-free sub-poll wakeups. | Adds a table and a claim protocol; polling fallback needed if `LISTEN` misses; table growth needs retention; Postgres becomes the hot path for both writes and dispatch (already true for the card write). | `lib/private/BackgroundJob/JobList.php` (claim protocol to emulate); `src/main.rs`/`src/db.rs` (existing pool); Postgres `SKIP LOCKED`/`LISTEN/NOTIFY` semantics | **Recommended durable queue.** |

---

## 7. What we would give up

1. **Synchronous side effects.** Today a `PUT` returns only after the activity
   row, birthday event and photo-cache delete are done or have aborted the
   request. With the outbox, the `201` can precede the effects by a few hundred
   ms to ~1 s. Clients do not read these effects in the write response
   (`card-events.md` §6.4), so this is safe, but anything that assumed
   read-your-writes on `oc_activity` must wait for dispatch.
2. **Abort-on-side-effect-failure.** Today a birthday failure fails the `PUT`.
   That is a bug we are deliberately removing, but it means a corrupt BDAY can
   now leave the card stored and the birthday calendar stale (dead-lettered)
   instead of rejecting the write.
3. **Third-party `Card*Event` listeners in selective mode.** Generic dispatch
   preserves them in phase 1; once native claims force selective invocation, an
   app that is not in the registry stops seeing card events. Mitigation:
   keep the registry explicit and default to generic until a native claim
   actually requires selective mode.
4. **Native birthday/reminders/photo/push.** The VObject 4.0 serialization,
   byte-exact ICS (ETag = `md5` of it), recurrence expansion for reminders,
   cascaded activity, the notifications app and the appdata/object-store
   deletion all stay in PHP (`card-events.md` §6.2–6.3). Only `redis_cloud_id`
   (and, cautiously, `activity_stream`) are genuinely portable.
5. **Transactional non-DB effects.** Redis `DEL` and appdata unlinks are only
   idempotent, not atomic with the DB. A replay after a crash redoes them.
6. **`oc_activity` schema ownership.** If `activity_stream` is ever made
   native, we couple to the Activity app's private schema, which is **not in
   this checkout** and may differ in production (`card-events.md` §7). That
   unknown must be resolved before any native activity write.
7. **No coalescing in phase 1.** Three rapid edits to the same card produce
   three events and three activity rows, exactly as PHP does today. Coalescing
   (final-state only for birthday/photo) is a phase-3 optimisation.
8. **`oc_jobs` integration.** We do not reuse cron's job list, so we also do
   not inherit its dedup, `last_checked` scheduling or its admin UI. The outbox
   needs its own status command and alerts.
9. **A resident process to operate.** The PHP daemon must be supervised,
   restarted on deploy, and watched (oldest-pending age, dead-letter count).
   The interpreter-per-batch fallback trades that for a per-batch bootstrap.

---

## 8. Phased implementation plan

**Phase 0 — preconditions (no code)**
- Confirm the deployed Activity app version and the real
  `oc_activity`/`oc_activity_mq` schema (`card-events.md` §7).
- Decide where the PHP worker runs (supervisor in the `nextcloud` container vs
  the cron container vs a new `FROM nextcloud` container) and how it is
  restarted on deploy.
- Confirm the sidecar is allowed to connect to Postgres for writes and
  `LISTEN` (it already has the pool; `LISTEN` needs a dedicated connection).

**Phase 1 — simplest correct thing (all-PHP, outbox, no cron)**
- Companion app `nextcloud_dav` migration creates `oc_dav_event_outbox`; the
  sidecar validates it and refuses native writes without it.
- Producer writes the outbox row + `pg_notify` in the card transaction.
- Resident `occ dav:event-dispatch` worker: claim `FOR UPDATE SKIP LOCKED`,
  **generic** `dispatchTyped(Card*Event)`, effects + done in one transaction,
  backoff + dead-letter, `LISTEN` + 250 ms poll, `batch = 128`.
- Metrics/status: pending count, oldest-pending age, dead count, per-effect
  latency. Alert on oldest-pending age.
- Acceptance: kill the worker mid-batch and mid-process, verify no lost and no
  duplicated activity/birthday/photo effects; kill the sidecar between commit
  and notify, verify the row is still dispatched.

**Phase 2 — modularity + trivially-safe native effect**
- Add the `effects` ownership map and selective PHP invocation.
- Implement `redis_cloud_id` natively in Rust (add a Redis client; read
  `config.php` `redis`/`memcache.distributed`), and remove it from the PHP
  list. Because `DEL` is idempotent, verify by counting `cloud_id_` keys and
  `DEBUG`-level Redis command accounting that it happens once.
- Emit the metrics above from both backends.

**Phase 3 — hardening and scale**
- (Only if native `activity_stream` is wanted) add the
  `oc_dav_effect_log` idempotency table and confirm the Activity schema.
- `LISTEN`-only mode (drop polling) + multiple worker processes.
- Per-`(addressbookid, card_uri)` coalescing for `birthday_calendar` and
  `photo_cache` (final state only), keeping one row per activity action.
- Retention/compaction of done rows; dead-letter inspection command.
- Optional: replace the CLI daemon with a FrankenPHP `num 1` dispatch worker
  and compare; make the PHP backend pluggable (CLI / HTTP / FrankenPHP).

**Phase 4 — optional further natives**
- `activity_stream` / `activity_mail` in Rust once the schema is confirmed and
  the effect log exists.
- `photo_cache` native via `oc_filecache` + object store (still needs the
  filecache delete to be transactional; appdata unlink stays best-effort).

---

## 9. Must-verify before implementation

- The exact `oc_dav_event_outbox` creation path: app migration vs sidecar
  `CREATE TABLE IF NOT EXISTS`. Recommendation: migration (schema ownership,
  upgrade path), sidecar fail-fast.
- Deployed Activity app schema (blocks any native activity claim).
- Whether the sidecar's Postgres role may `LISTEN` and whether the connection
  pool can reserve a dedicated listener connection.
- Sampled real per-row dispatch cost for birthday-heavy address books, to tune
  `batch_size`/`idle_poll_ms` against the ~1 s p99 latency target.
- Whether production runs FrankenPHP (changes the phase-3 option, not phase 1).

---

## 10. Evidence index

**Local — `nextcloud-server` (HEAD `d077e686132`)**

- `apps/dav/lib/CardDAV/CardDavBackend.php:525-553` (`getCard`, `readBlob`,
  quoted etag), `:640-689` (create), `:720-752` (update), `:810-836` (delete).
- `apps/dav/lib/Events/CardCreatedEvent.php`, `CardUpdatedEvent.php`,
  `CardDeletedEvent.php` (ctor: `addressBookId, addressBookData, shares, cardData`).
- `apps/dav/lib/Listener/CardListener.php:28-95`,
  `BirthdayListener.php:29-39`, `ClearPhotoCacheListener.php:27-32`.
- `apps/dav/lib/CardDAV/Activity/Backend.php:375-439` (`triggerCardActivity`,
  `getCardNameAndId`).
- `apps/dav/lib/AppInfo/Application.php:188-196` (registrations).
- `lib/private/Federation/CloudIdManager.php:37-67` (listener + Redis DEL).
- `lib/private/EventDispatcher/EventDispatcher.php:72-74`; no try/catch.
- `lib/public/AppFramework/Db/TTransactional.php:44`;
  `lib/private/DB/Connection.php:191` (`setNestTransactionsWithSavepoints(true)`).
- `lib/OC.php`: `boot()`, `initForRequest()` (starts with
  `resetStaticProperties()`), `resetStaticProperties()`,
  `handleRequests()` (`:1383-1398`).
- `index.php:25`, `remote.php:102`, `ocs/v1.php:35` (FrankenPHP callers).
- `lib/base.php:1-12` (`OC::boot(); OC::initForRequest();`).
- `console.php:1-100`, `occ:1-30` (CLI bootstrap shape).
- `core/Command/Background/JobWorker.php:88-133` (resident worker loop,
  `usleep`, `setupManager->tearDown()`, `tempManager->clean()`,
  `memory_reset_peak_usage()`).
- `lib/private/BackgroundJob/JobList.php:33,58-83` (dedup + 32 KB arg limit),
  `:175-255` (claim/reservation protocol).
- `lib/public/Files/ISetupManager.php:26-60`; `lib/private/Files/SetupManager.php:750`.
- `lib/private/legacy/OC_App.php:44-46` (`reset`); `lib/private/App/AppManager.php:87`.
- `Caddyfile` (experimental FrankenPHP worker config).
- Git: `689196b1d37` `6fc03681bd8` `64d222486f3` `27f45934ca3` `b964dfc53e6`
  `ea512eabb0b`; merges `cc48ec44206` (PR #58541) and `aab07d36936`
  (PR #61115, "extend frankenphp support"); `9c92cc16175` "Suppress last known
  impure static properties".
- `apps/` has no `activity` directory (Activity app is external).

**Local — `dav-bench`**

- `RESULTS.md:14-56` — fixed per-request ~200 ms, `status.php` 0.0227 s,
  FrankenPHP removes ~10–15 ms, worker-mode state accumulation, upstream
  issue #58807 reference.
- `PRODUCTION.md:20-71` — `status.php` 0.08–0.10 s, web boot ~0.50 s,
  `occ status` ~1.1 s, Redis at 11 mCPU/6 MiB, FrankenPHP ≈ 2 % of a 4.6 s
  request.
- `INFRA.md:16` — CLI bootstrap 121 s → 1.1 s.
- `PROFILING.md:17-19,45-54,77` — `bootstrap:register_apps` 22 ms,
  `boot_app:dav` 1.3 ms, `connect:db` 169 ms, per-request static reset
  incomplete.

**External**

- <https://frankenphp.dev/docs/worker/> — worker script shape,
  `frankenphp_handle_request()`, `MAX_REQUESTS`, `max_consecutive_failures`,
  persistent statics warning, manual/graceful restart.
- <https://github.com/nextcloud/server/issues/58807> — "Support FrankenPHP",
  critical to-dos: "Remove static variables", "Detect open DB transactions at
  the end of a request"; PR #58541 merged; worker mode still experimental.
- <https://github.com/nextcloud/notify_push> — precedent for a separate Rust
  daemon integrated with Nextcloud: requires Redis, config from `config.php`,
  systemd/OpenRC service, `notify_push_redis` for PHP↔Rust traffic.
- <https://docs.rs/ext-php-rs/latest/ext_php_rs/> — "build PHP extensions
  natively in Rust"; module list contains no `embed`; `php_eval` is "Execute
  embedded PHP code within a running PHP extension"; install via
  `cargo php install`.
- PHP internals, "How to embed PHP5 into a multi-threaded C app?"
  (<https://externals.io/message/15228>) — embed SAPI is not thread-safe
  without ZTS; `tsrm_ls`/globals not protected across threads.
