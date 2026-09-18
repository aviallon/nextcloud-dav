# Local end-to-end validation harness

A disposable Nextcloud **33.0.5** (the production version) + **PostgreSQL 18**
on a private docker network, used to exercise the `nextcloud-dav` native
`PUT`/`DELETE` path and the companion `nextcloud_dav` outbox dispatcher
(`occ dav:event-dispatch`) against a real PHP stack.

Everything is local. It never touches the production cluster, its database or
its Redis. Ports are bound to `127.0.0.1` only.

## Recreate and run in one command

```sh
cd nextcloud-dav/tests/local
./setup.sh      # containers, install, app, user, address book, sidecar
./e2e.sh        # the acceptance checks (writes state/evidence/e2e.txt)
./teardown.sh   # remove containers, volumes, network, secrets
```

`setup.sh` is idempotent; `teardown.sh` deletes the named volumes so the next
`setup.sh` starts from a clean Nextcloud install. `./setup.sh --rebuild` forces
a release rebuild of the sidecar.

Requirements: `docker` + `docker compose`, `curl`, `psql`, and the project's Nix
toolchain (the sidecar is built with
`nice -n 19 nix shell nixpkgs#cargo … -c cargo build --release --locked`).

## Files

| file | purpose |
|---|---|
| `docker-compose.yml` | postgres:18-alpine + nextcloud:33.0.5-apache, private network, host ports 55433/18081 |
| `lib.sh` | shared helpers (compose, occ, psql, sidecar lifecycle) |
| `setup.sh` | bring up, install, enable the app, create user/app-password/address book, copy `config.php`, build + start the sidecar |
| `e2e.sh` | the acceptance checks; writes `state/evidence/e2e.txt` |
| `teardown.sh` | `compose down -v` + remove `state/` |
| `state/` (git-ignored) | generated secrets, copied `config.php`, sidecar/worker logs, evidence |

Secrets are generated into `state/env` (mode 0600) and never printed. The app
password is written to `state/app_password` (0600) and used only through a curl
config file (`state/curlrc`), never on a command line.

## How the sidecar reaches the containerised Postgres

`setup.sh` copies the container's `config/config.php` to `state/config/` and
adds two sibling files Nextcloud/`--glob-config` both understand:

- `nextcloud_dav.config.php` — the shared `nextcloud_dav.event_dispatch` block
  (tuned for the tests: `max_attempts=3`, `backoff_ms=[200,200,200]`,
  `claim_timeout_s=5`, `idle_poll_ms=100`);
- `sidecar-db.config.php` — `dbhost=127.0.0.1` / `dbport=55433`, so the
  host-side sidecar reaches the published Postgres port without a password on
  the command line. Nextcloud never sees this file.

The sidecar runs on the host:
`target/release/nextcloud-dav --config state/config/config.php --glob-config`.

## Product bugs found by this harness (both now FIXED)

**B1 — blocking (fixed).** Nextcloud 33.0.5 has **no `OCP\Files\ISetupManager`**
(it only exists from a later major; only the private `OC\Files\SetupManager`
exists). The companion app type-hinted the interface in its `EventDispatch`
constructor, so once the app was enabled **every `occ` invocation failed** while
loading commands:

```
Could not resolve OCP\Files\ISetupManager! Class "OCP\Files\ISetupManager" does not exist
OC\AppFramework\Utility\QueryNotFoundException … SimpleContainer.php:138
```

A constructor parameter whose class does not exist makes the DI container fail
to build the command — and command loading happens for *any* `occ` call, so this
was not limited to `dav:event-dispatch`.

The app now resolves the setup manager **by name at runtime**
(`class_exists(ISetupManager::class) ? … : \OC\Files\SetupManager::class`, wrapped
in `try/catch`) and never type-hints it. The test-only shim that this harness
used to install is therefore gone: `setup.sh` exercises the real code path.

**B2 — minor (fixed).** The outbox row listed the same seven effects for every
operation, but `ClearPhotoCacheListener` is registered for update/delete only and
`CloudIdManager` for update only. The producer now records the effects that the
specific event actually triggers (`src/outbox.rs::effects_for`).

See `REPORT.md` for the full write-up of the run that found them.

## Notes

- The official image ships only `pdo_pgsql`, not the `pgsql` extension, so
  `pg_socket()` is unavailable and the worker reports
  `LISTEN unavailable … falling back to plain polling`. The polling fallback is
  exercised by criterion 6b.
- `e2e.sh` resets only alice's test data (cards, changes, properties, birthday
  calendar, activity, outbox) before it starts, so re-runs are reproducible.
