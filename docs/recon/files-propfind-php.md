# Where PHP spends time serving a files `PROPFIND` (recon, 2026-09-18)
Instance: **Nextcloud 33.0.5**, pod `nextcloud-prod-5f5b5746d9-xfld7` (php-fpm), PostgreSQL 18 direct, `home::aviallon`, `oc_filecache` 1.6 M rows. Code read: `nextcloud-server` @ 36.0.0-dev; the running 33.0.5 was checked in-pod and already ships the 2025 `preloadCollection`/`PropFindMonitorPlugin` backports, so both agree below. Method: throwaway app password (DB id 30164, deleted), `kubectl port-forward` to the nginx container, one temp `config/zz-recon.config.php`, one temp log and one role-scoped `log_min_duration_statement=0` (all reverted). Props = the 14 in the brief.

## 1. Call path
`remote.php:100-120` -> `apps/dav/appinfo/v2/remote.php:23-25` -> `apps/dav/lib/Server.php:268,304-354` (plugin stack) -> `ObjectTree::getNodeForPath` (`ObjectTree.php:65`) -> `Directory::getChildren` (`Directory.php:253`) -> `Folder::getDirectoryListing` (`Node/Folder.php:88`) -> `View::getDirectoryContent` (`View.php:1509`).
Per child: `Directory.php:259` builds a `File`/`Directory`; its ctor (`Connector/Sabre/Node.php:63-77`) builds an `OC\Files\Node\File|Folder`, and `Node::getParent()` (`Node.php:281`) lazily builds a `LazyFolder`; Sabre then runs one `propFind` per child (`3rdparty/sabre/dav/lib/DAV/Server.php:1050`).

## 2. Measured cost
Depth 0 fixed **0.31 s**; 8-child dir 0.32 s; 8,811-child dir full props **1.43 s p50 / 7.45 MB XML** -> **0.127 ms/child**. Floor with 1 prop (`resourcetype`) 0.65 s -> **0.039 ms/child** is object construction; the remaining ~0.09 ms/child is property-closure invocation + XML serialization. Small-vs-large confirms the fixed ~0.3 s is auth/bootstrap + filesystem setup, not per child.

## 3. SQL per listing = 0 per child
Role-scoped query logging: one full 8,811-child listing = **~26-38 executions, ~115 ms = 0 queries/child** (0.0011 counting tag batches). Per listing: **1** `getFolderContentsById` (`Cache.php:220` <- `View.php:1558`, indexed `fs_parent`, returns all 8,811 rows) + **8** fixed `Cache::get(path)` (`View.php:1394`) + `oc:favorite` prefetch batched by `TagsPlugin::preloadCollection` (`TagsPlugin.php:205`) into **10** `oc_vcategory_to_object` queries (~900 ids each; 1 for a small dir) + `oc:share-types` preload (`SharesPlugin.php:192`, `getSharesInFolder`) **~12** `oc_share` queries, one measured **738 ms**. An idle request already runs ~33 executions, so the children add no per-child SQL.

## 4. Which properties are free / expensive
Free (in-memory off the one cache row): `d:resourcetype getetag getlastmodified getcontenttype getcontentlength`, `oc:id fileid size`, `nc:has-preview` (`PreviewManager.php:224`), `nc:mount-type`, `nc:metadata-*` (`FileInfo.php:419`, already JOINed by the folder query), `oc:checksums` (`File::getChecksum`, cache column).
Per-collection only: `oc:favorite`/`oc:tags` (10 batches), `oc:share-types` (~12), `oc:comments-unread` (`CommentPropertiesPlugin::preloadCollection`), `nc:system-tags` (`SystemTagPlugin::preloadCollection`), `d:quota-*` (`Directory::getQuotaInfo` -> `OC_Helper::getStorageInfo`, once).
Truly per-child DB: `oc:owner-display-name` (resolves `LazyUser` per node) and `oc:share-permissions` (`getShareByToken` per node) - a 3-prop PROPFIND with them measured **5.78 s**; neither is in our set. `oc:downloadURL` (`File::getDirectDownload`) is a per-File storage lookup, also not in our set. `FilesReportPlugin`/`apps/files_sharing` sharees only run on REPORT.

## 5. Wrapper stack for `home::aviallon`
`SetupManager.php:162-246`: `Home`(Local) + `Quota` (HomeMountPoint) + optional `PermissionsMask`; `Availability`/`Encoding` only for external / `encoding_compatibility` mounts; `encryption:status` is **disabled**, so no `Encryption` wrapper. A listing is served from `oc_filecache` and never calls `getMetaData()` (`View::getDirectoryContent` uses the cache), while `Watcher::needsUpdate` is false by default (`filesystem_check_changes=0`): **no wrapper performs per-child DB or filesystem work** for this listing.

## 6. Attributing the 10 s
Not reproducible now: the same request is **1.43 s** (event profiler `request` 1.20 s, `dav_server_exec` 1.12 s, boot 0.08 s; SQL <1 %), 8.5x faster than the quoted 10.06 s on the same code. That measurement was taken under I/O contention (same-day Ceph/Redis incident + CNPG failover); this session saw single `oc_filecache WHERE path_hash=...` lookups of **4.58 s** and **16.30 s**, so the "fixed" part is a random variable. Shape of the 10 s = ~0.4 s fixed + **1.10 ms/child, all PHP-side CPU** (8,811 `FileInfo`/`Node`/`LazyFolder` objects, ~123 k property closures, 7.45 MB XML built in memory); SQL stays ~1 query per collection regardless of child count.

## 7. Memory / the 151 k trash dir
Workers that served these listings peak at **~205-227 MB RSS** (`/proc/<pid>/VmHWM`) vs ~120 MB idle; `memory_limit=512M`, `pm.max_requests=0`, no PHP time limit (`remote.php` `set_time_limit(0)`). The trash dir (151,143 children = 17.2x) needs ~17x the object graph and ~128 MB of XML alone, so it should **exhaust 512 M or OOM the node before finishing - estimate only, do not run it**.

## 8. Biggest lever
Per-child PHP object work + XML serialization, not SQL. Emitting the same properties from one `oc_filecache` query (+10 tag batches) removes ~0.13 ms x N; the ~0.3 s fixed auth/filesystem setup is what a native sidecar also avoids.
