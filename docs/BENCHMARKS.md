# Benchmark: nextcloud-dav sidecar vs the PHP DAV backend

Production instance, 2026-09-18, `https://nextcloud.lesviallon.fr`.
Harness: `bench_native_vs_php.py`. 10 reps per backend per row, **interleaved**
(one sidecar, one PHP, …) so a slow window hits both backends rather than only
the second one measured.

## How each backend was addressed

- **sidecar** — the public URL. nginx routes
  `/remote.php/dav/addressbooks/users/<u>/<book>/…` to `127.0.0.1:7868`.
- **PHP** — the same URL with `?export=1`. The sidecar answers `501` for that
  query on purpose and nginx replays the buffered request to php-fpm, where it
  is an ordinary PROPFIND/GET/PUT/DELETE (the export plugin only intercepts
  GET/REPORT). Verified by watching php-fpm's access log gain a line per
  request, and by the two responses differing.

> **Methodology trap, recorded because it cost real time.** The obvious way to
> reach PHP while the sidecar owns the route is to insert a second slash
> (`users//<u>/…`) so the sidecar's location regex
> (`^/remote\.php/dav/addressbooks/users/[^/]+/.`) does not match. **It does not
> work:** nginx has `merge_slashes on` by default, so the URI is normalised
> before location matching and the sidecar still serves it. A first version of
> this benchmark used that trick and produced a tidy table in which both
> backends were within 10 % of each other on every row — because both rows were
> the sidecar measuring itself. The tell-tale signs: every ratio ≈ 1.0, and the
> two response bodies byte-identical. Use `?export=1`, or address the two
> backends on separate ports (which is what the local harness does).

## Results

| request | sidecar p50 | sidecar p90 | PHP p50 | PHP p90 | sidecar is |
|---|---:|---:|---:|---:|---:|
| `PROPFIND` Depth 1 (789-card book) | **642 ms** | 749 ms | 2432 ms | 4180 ms | **3.8× faster** |
| `GET` one card | **400 ms** | 407 ms | 2036 ms | 2154 ms | **5.1× faster** |
| `PUT` create | **829 ms** | 1041 ms | 2751 ms | 3015 ms | **3.3× faster** |
| `PUT` update | **814 ms** | 867 ms | 2741 ms | 3097 ms | **3.4× faster** |
| `DELETE` | **678 ms** | 736 ms | 2852 ms | 3011 ms | **4.2× faster** |

The write rows are new: `PUT`/`DELETE` used to be replayed to PHP, and are now
native (card row + change row + sync token + search columns + one outbox row,
in a single transaction).

## What the absolute numbers actually mean

They are **not** a statement about the sidecar's code. On this instance the
Postgres storage is the bottleneck, and it fluctuates by an order of magnitude
with the health of the Ceph pool behind it. Measured at the same time as the run
above:

```
SELECT 1                                                   1.2 ms
SELECT count(*) FROM oc_cards WHERE addressbookid = 1    528   ms
SELECT count(*) FROM oc_dav_shares                         552   ms   (7 rows!)
SELECT id,uri FROM oc_addressbooks WHERE principaluri=…      6.2 ms
```

A `count(*)` over a **seven-row** table taking 552 ms is not query cost — it is
I/O stall on the Ceph HDD pool (the same pool that was at 83 % and whose slow
OSDs caused the 2026-09-18 outage). The sidecar's slow-statement log during the
run was 12 × `COMMIT` and 1 × card `SELECT`: the writes are waiting on WAL
fsync, the read on the card fetch.

So the honest reading is:

- **The sidecar removes PHP's fixed cost.** PHP pays ~0.4 s just for
  `/status.php` and 2.0–2.9 s for a DAV request on this instance; the sidecar
  pays none of it.
- **What remains is the database work**, which both backends must do. The
  sidecar's time is ≈ the cost of the one query that actually matters (fetching
  the collection's cards); its other round trips (auth, groups, visible books)
  are ~6 ms each and do not move the needle.
- Therefore the **ratio** is the stable, meaningful number (3–5×), and the
  absolute latency tracks storage health. Earlier the same 788-card PROPFIND
  measured **80–94 ms vs PHP's 1.34–1.41 s (~17×)** when the pool was healthy;
  the same request has also been seen at 2.4 s under node memory pressure. The
  sidecar never gets *slower than the DB*; PHP is always slower than the DB by
  its bootstrap.

## Local harness comparison (for contrast, not for the headline)

Same script against the disposable Nextcloud 33.0.5 + PostgreSQL on this
workstation (sidecar on :17870, PHP on :18081, addressed directly):

| request | sidecar p50 | PHP p50 | ratio |
|---|---:|---:|---:|
| PROPFIND Depth 1 | 43.2 ms | 46.0 ms | 1.07× |
| GET one card | 40.3 ms | 43.5 ms | 1.08× |
| PUT create | 49.0 ms | 58.5 ms | 1.19× |
| PUT update | 48.5 ms | 59.3 ms | 1.22× |
| DELETE | 46.4 ms | 55.4 ms | 1.19× |

This looks disappointing and is *expected*: that instance is a minimal install
whose PHP bootstrap costs **13–16 ms** (`status.php`, 54 apps, warm opcache, no
bulk data). There is almost no fixed cost to remove, so the sidecar's advantage
shrinks to the difference in request handling. It is a useful control: it shows
the speedup comes from eliminating PHP's per-request bootstrap, not from the
sidecar doing something clever with the data.

## Reproducing

```sh
# production (PHP forced with the sidecar's own 501 -> nginx replay)
python3 bench_native_vs_php.py --user <u> --password-file /tmp/pw \
  --sidecar-base https://cloud.example --php-base https://cloud.example \
  --php-query export=1 --reps 10 --json bench_prod.json

# local harness (both backends addressed directly)
python3 bench_native_vs_php.py --user alice --password-file <harness>/state/app_password \
  --sidecar-base http://127.0.0.1:17870 --php-base http://127.0.0.1:18081 --reps 15
```

The script creates and deletes a throwaway `zz-bench` address book; it never
writes to a real one.

## Follow-ups this benchmark suggests

1. **The storage is the remaining problem**, not the DAV layer: getting the Ceph
   pool below nearfull and off the Redis/Postgres hot path would improve both
   backends, and the sidecar would benefit most in absolute terms.
2. Per-request round trips could be cut (cache group membership / visible books
   with a short TTL, fold the auth checks into one query). At ~6 ms each on this
   storage they are not currently the bottleneck, so this is only worth doing if
   the DB gets fast enough for them to matter.
