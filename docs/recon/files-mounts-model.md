# Files `PROPFIND` — the mount-bearing model (phase 2 recon)

Recon for serving `/remote.php/dav/files/<uid>/**` `PROPFIND` when the path is a
mount root, is **inside** a mount, or is a collection that **contains** a mount
(the home root: 85 % of files `PROPFIND`s, 2 648 / 3 113). This is the phase-2
counterpart of `docs/recon/files-propfind-model.md` (§2, §9), which delegates
all of that today (`src/files.rs::mounts_delegate`, `src/files.rs:161-177`).

Evidence: read-only `SELECT`s against the production DB (`nextcloud` database,
`nextcloud-pg18-4`, 2026-09-19), the local checkout `nextcloud-server` (36.0.0
dev), and the **running** `groupfolders` 21.0.15 source read out of the
production pod at `/var/www/html/custom_apps/groupfolders` (the app is not in
the checkout). Production runs 33.0.5; table shapes and provider classes below
were verified against the production schema and rows, not only the checkout. No
production write was performed.

---

## Report (summary, ≤30 lines)

**`oc_mounts`** — `id` PK; `storage_id` bigint = `oc_storages.numeric_id`; `root_id` = `oc_filecache.fileid` of the mount root *in that storage*; `user_id`; `mount_point` = `/<uid>/files/<name>/` (home `/<uid>/`); `mount_id` = external id else NULL; `mount_provider_class`; `mount_point_hash` = `xxh128(mount_point)`. Written by `UserMountCache::addToCache` (`.../UserMountCache.php:176-183`).

**List mounts + properties/permissions** (verified):
```sql
SELECT m.mount_point, m.mount_provider_class, m.mount_id, m.storage_id, s.id, m.root_id,
       f.path, f.name, f.mimetype, f.mtime, f.etag, f.size, f.permissions
FROM oc_mounts m JOIN oc_storages s ON s.numeric_id=m.storage_id
JOIN oc_filecache f ON f.fileid=m.root_id WHERE m.user_id=:uid ORDER BY m.mount_point;
```
mask per provider: share `& superShare.perms`; groupfolder `& group perms & ACL`; external `& (readonly?17:31) & (enable_sharing?31:15)`.

**Parent size/etag** (`FileInfo.php:158-167,322-379`): `etag = md5(parent.etag.'::'.implode('::',[relPath.'/'.rootEtag.rootPerms]))`, `size = parent.size + Σ rootSize`, `mtime = max`; `relPath = substr(mountPoint,len(parentAbsPath))` (leading+trailing slash ⇒ **double slash**), perms decimal, order = mount_point ascending. DB-derived for `home::aviallon/files`: `bb151705b7603d35fb3b10b5c2f3cee2`, size `2798181637966`.

**`nc:mount-type`**: home `''`, groupfolder `group`, share `shared`, external `external`/`external-session`; **`nc:is-mount-root`** = `'true'` iff internal path `''`.

**Groupfolder ACL**: reproducible from SQL in principle (`oc_group_folders{,_groups,_acl,_manage}` + `oc_group_user` + rule bit-math) but needs circles, `acl-inherit-per-user`, `canManageACL`, and **drops 0-permission entries**. Production has 0 ACL rows; keep `acl=1` delegated.

**Stay delegated**: writes; `acl=1` groupfolders; non-local external; share-manager props (`oc:share-types`, `ocs:share-permissions`, `nc:note`…); `d:quota-*`; objectstore/encryption.

**Single biggest risk**: the parent's synthetic etag (exact string, decimal perms, double slash, ascending order) — one wrong byte silently re-syncs every client forever.

---

## 1. `oc_mounts`: schema, meaning, and real rows

Verified against the production schema:

```
 id                   bigint   PK
 storage_id           bigint   NOT NULL   -- oc_storages.numeric_id (int, not the string id)
 root_id              bigint   NOT NULL   -- oc_filecache.fileid in that storage
 user_id              varchar(64) NOT NULL
 mount_point          varchar(4000) NOT NULL   -- /<uid>/files/<name>/ (home: /<uid>/)
 mount_id             bigint   NULL       -- oc_external_mounts.mount_id for external, else NULL
 mount_provider_class varchar(128) NULL
 mount_point_hash     varchar(32) NOT NULL -- xxh128(mount_point), hex
 indexes: PK(id), UNIQUE(user_id, root_id, mount_point_hash), (storage_id,user_id),
          (mount_provider_class), (mount_id), (root_id), (storage_id)
```

- **`root_id` is a `fileid` in the mount's own storage**, i.e. the row whose
  `oc_filecache.storage = oc_mounts.storage_id`. It is **not** the storage root
  (`path = ''`) in general: it is the *jail root*. For a groupfolder it is
  `__groupfolders/<id>` (root-jail) or `files` (separate storage); for a share
  it is the shared folder inside the owner's home storage; for external it is
  the storage root (`path = ''`); for home it is the storage root (`path = ''`).
- **`storage_id` is the numeric id**, FK to `oc_storages.numeric_id`; the string
  id lives in `oc_storages.id`. For a **received share** it is the **owner's**
  home storage (`home::selene`), not a `shared::` storage — `SharedMount`
  overrides `getNumericStorageId()` to the source file's storage
  (`apps/files_sharing/lib/SharedMount.php:159-184`).
- **`mount_point`** is produced by `MountPoint::formatPath()`
  (`lib/private/Files/Mount/MountPoint.php:143-150`): leading `/`, trailing `/`
  when longer than `/`, and the user prefix `<uid>/files/`. The home mount is
  `/<uid>/`.
- **`mount_point_hash`** is `hash('xxh128', $mountPoint)` in the checkout
  (`UserMountCache.php:180`); production values are 32 hex chars and match
  `xxh128`, so 33.0.5 used the same.

Real production rows for `aviallon` (8), one per provider, with the root row
resolved in the mount's own storage:

| `mount_point` | provider class | `mount_id` | `storage_id` | storage string | `root_id` | root `path` | root `etag` | root `perms` | root `size` |
|---|---|---|---|---|---|---|---|---|---|
| `/aviallon/` | `OC\Files\Mount\LocalHomeMountProvider` | – | 2 | `home::aviallon` | 6 | `` | `6aadc32bd443f` | 23 | 1098854294968 |
| `/aviallon/files/Partage familial/` | `OCA\GroupFolders\Mount\MountProvider` | – | 11 | `local::/var/www/html/data/` | 366605 | `__groupfolders/1` | `69eef3c7d9cda` | 31 | 13207951013 |
| `/aviallon/files/Administratif commun/` | `OCA\GroupFolders\Mount\MountProvider` | – | 11 | `local::/var/www/html/data/` | 1772168 | `__groupfolders/2` | `6aa904c996633` | 31 | 8135461910 |
| `/aviallon/files/Hytrix/` | `OCA\GroupFolders\Mount\MountProvider` | – | 25 | `local::/var/www/html/data/__groupfolders/3/` | 3981487 | `files` | `6a68d77ca7a1c` | 31 | 14612945 |
| `/aviallon/files/Documents Sélène/` | `OCA\Files_Sharing\MountProvider` | – | 3 | `home::selene` | 483 | `files/Documents Sélène` | `6aa2c083517bd` | 31 | 67468224845 |
| `/aviallon/files/Mariage/` | `OCA\Files_Sharing\MountProvider` | – | 3 | `home::selene` | 602520 | `files/Mariage` | `69cef6e63b305` | 31 | 50779087 |
| `/aviallon/files/MyCloud Photos Backup/` | `OCA\Files_External\Config\ConfigAdapter` | 1 | 18 | `local::/external/mycloud/` | 1599872 | `` | `66c9bb3195abf` | 23 | 502625530869 |
| `/aviallon/files/Dashcam (ceph)/` | `OCA\Files_External\Config\ConfigAdapter` | 2 | 21 | `local::/external/dashcam/` | 3599244 | `` | `69ba093c7d12a` | 23 | 1195662625280 |

The table has 45 rows total; all 45 resolve to an existing storage and root row
(no stale rows today). The home mount is the only row not under `files/`.

## 2. The mount's own metadata, per provider

The mount root's `name`, `mtime`, `etag`, `size`, `permissions`, `mimetype` all
come from the **`oc_filecache` row `root_id`** in storage `storage_id`
(`mimetype = 2 = httpd/unix-directory`), after the provider's storage/cache
wrappers have masked `permissions`. `View::getDirectoryContent` reads it with
`$subCache = $subStorage->getCache(''); $rootEntry = $subCache->get('')`
(`lib/private/Files/View.php:1595-1612`) and `FileInfo::updateEntryFromSubMounts`
does the same (`FileInfo.php:333-339`).

| provider | wrapper applied to the cache entry | effective `permissions` |
|---|---|---|
| home | none | row perms (23 for the storage root; the DAV `files` dir is a different row, 31) |
| groupfolder | `PermissionsMask(folder perms)` + `ACLCacheWrapper` + `CacheRootPermissionsMask` | `row.perms & group perms & aclPerms` |
| received share | `OCA\Files_Sharing\Cache::formatCacheEntry` | `row.perms & superShare.perms` |
| files_external local | `PermissionsMask(readonly)` + `PermissionsMask(enable_sharing)` | `row.perms & (readonly ? 17 : 31) & (enable_sharing ? 31 : 15)` |

Verification query (per mount, returns the root metadata):
```sql
SELECT m.mount_point, f.fileid, f.name, f.mtime, f.etag, f.size, f.permissions,
       mt.mimetype
FROM oc_mounts m
JOIN oc_filecache f ON f.fileid = m.root_id
LEFT JOIN oc_mimetypes mt ON mt.id = f.mimetype
WHERE m.user_id = :uid AND m.mount_point = :mount_point;
```
The `name` that reaches the wire is **not** the row `name` (which is `1`, `2`,
`files`, … for groupfolders): `View::getDirectoryContent` overwrites it with the
mount's last path component — `$rootEntry['name'] = $relativePath`
(`View.php:1650`), where `$relativePath = trim(substr($mountPoint, strlen($dir)), '/')`.
For shares `OCA\Files_Sharing\Cache::formatCacheEntry` also sets
`name = basename(share target)` (`apps/files_sharing/lib/Cache.php:157-159`), but
the `View` assignment wins.

## 3. PHP's exact behaviour for a listing that contains a mount

Call path: `ObjectTree::getNodeForPath` → `View::getFileInfo($path)` (default
`includeMountPoints = true`) for the **node itself**, then
`Directory::getChildren` → `Folder::getDirectoryListing`
(`lib/private/Files/Node/Folder.php:88-107`) → `View::getDirectoryContent`
(`View.php:1509-1666`).

**(a) The mount root is a child entry.** `View::getDirectoryContent` first
issues the ordinary cache query (`getFolderContentsById($folderId)`, no
`ORDER BY`), then merges mounts: `$mounts = Filesystem::getMountManager()->findIn($path)`
and for each mount whose `relativePath` has no `/`:
```php
$rootEntry['name'] = $relativePath;
$rootEntry['type'] = $rootEntry['mimetype'] === 'httpd/unix-directory' ? 'dir' : 'file';
$permissions = $rootEntry['permissions'];
if ($mount instanceof IMovableMount) {
    $rootEntry['permissions'] = $permissions | Constants::PERMISSION_UPDATE | Constants::PERMISSION_DELETE;
} else {
    $rootEntry['permissions'] = $permissions & (Constants::PERMISSION_ALL - (Constants::PERMISSION_UPDATE | Constants::PERMISSION_DELETE));
}
if ($sharingDisabled) { $rootEntry['permissions'] = $rootEntry['permissions'] & ~Constants::PERMISSION_SHARE; }
$files[$rootEntry->getName()] = new FileInfo($path . '/' . $rootEntry['name'], $subStorage, '', $rootEntry, $mount, $owner);
```
(`View.php:1646-1665`). So: **name = last component of the mount point**;
`etag`/`mtime`/`size` = the mount storage root entry's columns (no synthetic
etag here — this `FileInfo` has no `subMounts`); `permissions` = masked row perms
with `|UPDATE|DELETE` for movable mounts (`SharedMount`, `PersonalMount`) or
`& ~(UPDATE|DELETE)` otherwise. `findIn` returns mounts **strictly below** the
path, sorted by mount point string (`lib/private/Files/Mount/Manager.php:118-169`),
and cache children are inserted first, so the response order is *cache rows,
then mount entries*.

**(b) `etag`/`mtime`/`size`/`permissions` of a mount root.** From the cache
entry of `$subStorage->getCache('')->get('')` (the mount root row), masked by
the provider wrappers (§2). `FileInfo::getPermissions()` is just
`(int)$data['permissions']` (`FileInfo.php:215-217`); the extra
`|UPDATE|DELETE` / `& ~(UPDATE|DELETE)` is the `View` branch above.

**(c) The parent's `oc:size` and etag when a submount exists.** The *node* was
built by `View::getFileInfo($path)` with `includeMountPoints = true`, which calls
`addSubMounts` → `setSubMounts(findIn($info->getPath()))`
(`View.php:1487-1498`), i.e. **every** mount below it, direct or nested. Then:
```php
public function getEtag() {
    $this->updateEntryFromSubMounts();
    if (count($this->childEtags) > 0) {
        $combinedEtag = $this->data['etag'] . '::' . implode('::', $this->childEtags);
        return md5($combinedEtag);
    } else { return $this->data['etag']; }
}
public function getSize($includeMounts = true) {
    if ($includeMounts) {
        $this->updateEntryFromSubMounts();
        if ($this->isEncrypted() && isset($this->data['unencrypted_size'])) { return $this->data['unencrypted_size']; }
        else { return isset($this->data['size']) ? 0 + $this->data['size'] : 0; }
    } else { return $this->rawSize; }
}
// updateEntryFromSubMounts(): for each mount -> $rootEntry = $subCache->get(''); $this->addSubEntry($rootEntry, $mount->getMountPoint());
public function addSubEntry($data, $entryPath) {
    if (!$data) { return; }
    $hasUnencryptedSize = !empty($data['encrypted']) && isset($data['unencrypted_size']);
    $subSize = $hasUnencryptedSize ? $data['unencrypted_size'] : ($data['size'] ?: 0);
    $this->data['size'] += $subSize;
    if ($hasUnencryptedSize) { $this->data['unencrypted_size'] += $subSize; }
    if (isset($data['mtime'])) { $this->data['mtime'] = max($this->data['mtime'], $data['mtime']); }
    if (isset($data['etag'])) {
        $relativeEntryPath = substr($entryPath, strlen($this->getPath()));
        $permissions = isset($data['permissions']) ? $data['permissions'] : 0;
        $this->childEtags[] = $relativeEntryPath . '/' . $data['etag'] . $permissions;
    }
}
```
(`FileInfo.php:158-167, 173-185, 322-379`). Notes that must be reproduced
exactly:

- `$entryPath = $mount->getMountPoint()` (`/uid/files/Name/`) and
  `$this->getPath()` is the parent's absolute view path **without** trailing
  slash (`/uid/files`), so `$relativeEntryPath` is `/Name/`; the `'.' . '/'`
  then produces a **double slash**: `childEtag = "/Name//<etag><perms>"`.
- `$permissions` is the **masked** mount-root permission (§2), concatenated as a
  **decimal integer** (e.g. `31`, `1`, `17`).
- Order = `findIn` order = mount points **ascending** (`ksort(..., SORT_STRING)`),
  and it includes **nested** mounts, not only direct children.
- `getMTime()` is also `updateEntryFromSubMounts()`-ed, so the parent's
  `getlastmodified` is the max of the parent and its submount roots.
- The parent's raw `oc_filecache.etag` is used as the prefix (it is itself kept
  current by `Propagator`), and the synthetic etag is **not** written back to the
  DB.
- `getDirectoryContent` additionally calls `addSubEntry` on an intermediate
  folder entry when a mount is nested (`if ($pos = strpos($relativePath, '/'))`,
  `View.php:1622-1644`), creating the parent folder via `mkdir` if missing. This
  affects the intermediate folder's size/etag, not the listed node's.

DB-derived check on `home::aviallon/files` (parent row fileid 7, etag
`6aadc32bd443f`, size `1011016452017`), submounts in ascending order with
masked permissions (shares `31`, groupfolders `31`, external `23&17&15=1` and
`23&17=17`):
```
6aadc32bd443f::/Administratif commun//6aa904c99663331::/Dashcam (ceph)//69ba093c7d12a17::/Documents Sélène//6aa2c083517bd31::/Hytrix//6a68d77ca7a1c31::/Mariage//69cef6e63b30531::/MyCloud Photos Backup//66c9bb3195abf1::/Partage familial//69eef3c7d9cda31
md5 = bb151705b7603d35fb3b10b5c2f3cee2   size = 2798181637966
```
This is *computed from the formula + DB*, not captured from PHP (no production
write was made); the local harness diff is the proof.

**Correction from the local harness (real Nextcloud 33.0.5).** The `childEtags`
value above uses the plain masked root permission (31 for the groupfolders). On
the wire, a **Depth-1** listing uses the `View::getDirectoryContent()` branch
instead: movable mounts get `|UPDATE|DELETE` (so shares stay 31) and
non-movable mounts get `& ~(UPDATE|DELETE)` (so a groupfolder root is **21**, and
a read-only external stays 17). Depth 0 uses the plain value. The sidecar
reproduces both (`src/mounts.rs::child_etag_permissions`), and
`tests/local/files_parity.sh` diffs both depths byte-for-byte against PHP.

**(d) `{nc:}mount-type` and `{nc:}is-mount-root`.** `mount-type` is
`$node->getFileInfo()->getMountPoint()->getMountType()` (`FilesPlugin.php:406-408`):
`''` for the home mount, `group` for groupfolders, `shared` for received shares,
`external` / `external-session` for files_external. `is-mount-root` is
`$node->getNode()->getInternalPath() === '' ? 'true' : 'false'`
(`FilesPlugin.php:416-418`), so it is `true` for a mount root and for the home
mount root, and `false` for the DAV home `files` directory (internal path
`files`).

**(e) `oc:permissions` letters.** `DavUtil::getDavPermissions`
(`lib/public/Files/DavUtil.php:36-81`), exact order: `S` if `isShared()`
(`ISharedMountPoint`), `R` (SHARE bit), `M` if `isMounted()` (`!isHome &&
!isShared`), `G` (READ), `D` (DELETE), `N` if `canRename()`, `V` (UPDATE), then
`W` (files, if writable) or `CK` (dirs, if CREATE). `canRename`
(`DavUtil.php:86-104`) is true for a **movable** mount root, or `isUpdateable()`,
or (`isDeletable() && parent->isCreatable()`) — with the home `files` root
excluded. Note lines 61-69: for a movable mount root it re-reads
`$storage->getCache()->get('')` to decide the `W` letter, because `View` added
`UPDATE` to the mount root.

## 4. files_sharing mounts

`OCA\Files_Sharing\MountProvider` (production
`/var/www/html/apps/files_sharing/lib/MountProvider.php`) groups received shares
by node id into a **super-share** (permissions are the **OR** of the grouped
shares, target/attributes merged, `getMountsFromSuperShares:265-341`). It then
creates a `SharedMount` over `SharedStorage` for each super-share, skipping
`STATUS != ACCEPTED` only for `TYPE_USER`/`TYPE_GROUP`/`TYPE_USERGROUP`
(`:284-291`).

- **storage id**: `SharedStorage::getId() = 'shared::' . mountPoint`
  (`SharedStorage.php:151-153`), but **`oc_mounts.storage_id` is the owner's
  numeric home storage** because `SharedMount::getNumericStorageId()` returns the
  source node's storage (`SharedMount.php:166-184`). Production: `home::selene`
  (3) for both of `aviallon`'s shares.
- **root_id**: `SharedMount::getStorageRootId() = superShare.getNodeId()`
  (`SharedMount.php:159-161`) = `oc_share.file_source` (483, 602520).
- **permissions**: `OCA\Files_Sharing\Cache::formatCacheEntry` masks every entry
  `$entry['permissions'] &= $this->share->getPermissions()`
  (`apps/files_sharing/lib/Cache.php:147-155`), so the mount root is
  `row.perms & superShare.perms`. `SharedStorage::getPermissions` additionally
  `|DELETE` at `path === ''` and strips SHARE when sharing is disabled for the
  user (`SharedStorage.php:244-256`), but the **cache entry** (what `View` reads)
  is only share-masked. A **read-only** share is therefore `permissions = 1`
  (or `17` = READ|SHARE) on every entry, and the mount root gets `|UPDATE|DELETE`
  from `View` because `SharedMount` is `MoveableMount`.
- **`oc_share` columns that matter**: `share_type`, `share_with`, `uid_owner`,
  `uid_initiator`, `parent`, `item_type`, `file_source`, `file_target`,
  `permissions`, `accepted`. Production `oc_share` has **no `hidden` column**
  (that field lives elsewhere); `share_type` values in production are 0, 1, 3,
  4, 10, 11 (`IShare::TYPE_USER=0, TYPE_GROUP=1, TYPE_LINK=3, TYPE_EMAIL=4,
  TYPE_ROOM=10`; 11 is Talk's userroom). `accepted` is enforced only for
  types 0/1/2, so production `selene` has three `accepted = 0` Talk (type 11)
  mounts that **are** real mounts — do not filter them out.
- Real read-only examples: `selene`'s `/selene/files/TIPE/` (`oc_share.id=115`,
  `permissions=1`, root row perms 31 → effective 1) and `ykhelalfa`'s
  `/ykhelalfa/files/Sauvegarde Foncia/` (share perms 1).

SQL for a user's received-share mounts with permissions:
```sql
SELECT m.mount_point, m.storage_id, m.root_id, f.path AS root_internal_path,
       (f.permissions & COALESCE(sh.perms, 0)) AS mount_perms
FROM oc_mounts m
JOIN oc_filecache f ON f.fileid = m.root_id
LEFT JOIN LATERAL (
    SELECT bit_or(s.permissions) AS perms
    FROM oc_share s
    WHERE s.file_source = m.root_id
      AND s.share_with  = m.user_id
      AND s.share_type IN (0, 1, 2, 7)               -- user, group, usergroup, circle
      AND (s.share_type NOT IN (0, 1, 2) OR s.accepted = 1)
) sh ON true
WHERE m.user_id = :uid
  AND m.mount_provider_class = 'OCA\Files_Sharing\MountProvider'
ORDER BY m.mount_point;
```

## 5. groupfolders (21.0.15, read from the production pod)

Source is at `/var/www/html/custom_apps/groupfolders` (not in the checkout).
Tables (verified live):

- **`oc_group_folders`**: `folder_id` PK, `mount_point` varchar(4000) (**bare
  name**, e.g. `Hytrix`, no leading slash), `quota` bigint, `acl` int,
  `root_id` bigint, `storage_id` bigint, `options` text, `acl_default_no_permission`
  bool. Production: 3 rows — `(1, Partage familial, acl 0)`,
  `(2, Administratif commun, acl 0)`,
  `(3, Hytrix, acl 1, options {"separate-storage":true})`.
- **`oc_group_folders_groups`**: `applicable_id` PK, `folder_id`, `permissions`
  int, `group_id`, `circle_id`. Production: one row per folder, all
  `permissions = 31`, groups `famille`, `foyer Sélène et Antoine`, `Hytrix`.
- **`oc_group_folders_acl`**: `acl_id` PK, `fileid` (in the folder's storage),
  `mapping_type` (`user`/`group`/`circle`), `mapping_id`, `mask` smallint,
  `permissions` smallint. Production: **0 rows**.
- **`oc_group_folders_manage`**: `folder_id`, `mapping_type`, `mapping_id`
  (for `canManageACL`). Production: 0 rows.

**Storage/root derivation** (`lib/Mount/FolderStorageManager.php:85-123,
189-255`): with `separate-storage` the base is a `Local` at
`<datadirectory>/__groupfolders/<id>` → storage id
`local::<datadirectory>/__groupfolders/<id>/`, jailed to `files`, so the mount
root row is `path = 'files'` in that storage (production storage 25, root
3981487). Without it, the base is the root storage (`local::<datadirectory>/`,
storage 11) jailed to `__groupfolders/<id>`, so the root row is
`__groupfolders/<id>` (366605, 1772168). In both cases `oc_mounts.storage_id`
and `root_id` match `oc_group_folders.storage_id` / `.root_id`.

**Per-user permission**: `FolderManager::getFoldersForUser` /
`getFoldersForGroups` (`lib/Folder/FolderManager.php:1139-1161, 684-733`) OR the
`oc_group_folders_groups.permissions` of the groups the user is in (and circles);
`MountProvider::getMount` applies it as `PermissionsMask(mask = folder perms)`
(`lib/Mount/MountProvider.php:120-159`), so it masks **every** entry.

**ACL-driven per-file permissions**: when `acl = 1`, `MountProvider` masks the
root by `getPermissionsForPathFromRules`, and `FolderStorageManager` wraps the
storage in `ACLStorageWrapper`, whose cache is `ACLCacheWrapper`
(`lib/ACL/ACLCacheWrapper.php:51-70`):
```php
protected function formatCacheEntry($entry, array $rules = []) {
    if (isset($entry['permissions'])) {
        $entry['scan_permissions'] ??= $entry['permissions'];
        $entry['permissions'] &= $this->getACLPermissionsForPath($entry['path'], $rules);
        if (!$entry['permissions']) { return false; }   // entry dropped from the listing
    }
    return $entry;
}
```
`getACLPermissionsForPath` requires READ (and READ|SHARE when the folder is
reached through a share) or returns 0. The rule engine
(`lib/ACL/ACLManager.php:117-239`, `lib/ACL/Rule.php:42-83, 142-154`) works on
`oc_group_folders_acl` rows filtered to the user's mappings (`user:<uid>`,
`group:<gid>`, `circle:<singleId>`; `UserMappingManager.php:46-58`): collect the
path and all parents, sort **parent first**, per path `mergeRules` = OR of the
`mask` bits and OR of the `permissions` bits, then
`applyPermissions(base)` where
```
denyMask = ~mask | permissions;  p = base & denyMask;  p |= mask & permissions;  return p
```
and `base = 31`, or `0` when `acl_default_no_permission` is set (or `READ` for a
user who `canManageACL`). `acl-inherit-per-user = true` switches to a different
merge (per mapping, then allow-overrides-deny); production uses the default
`false` (`ACLManagerFactory.php:45-50`). `RuleManager::getRulesForFilesByPath`
joins `oc_group_folders_acl a` to `oc_filecache f ON f.fileid = a.fileid` and
filters `f.storage = :storage AND f.path_hash = md5(path)`.

**Can a groupfolder be read-only?** Yes — set the group's `permissions` to e.g.
`1`; the `PermissionsMask` then masks every entry to `READ`, and the root (not
movable) becomes `row.perms & 1 & ~(UPDATE|DELETE) = 1`.

**SQL verdict**: everything the engine needs is in SQL **except** (i) circle
memberships (circles app tables), (ii) `canManageACL` (admin groups,
`oc_group_folders_manage`), and (iii) the `acl-inherit-per-user` switch (an app
config read). The math is fully deterministic and portable. Production has no
ACL rows, so this is currently untested against real rules.

## 6. files_external

Real table names (verified): `oc_external_mounts` (`mount_id` PK, `mount_point`
varchar(128) — **leading** slash, no user prefix, e.g. `/MyCloud Photos Backup`;
`storage_backend`, `auth_backend`, `priority`, `type`), `oc_external_config`
(`config_id`, `mount_id`, `key`, `value` — e.g. `datadir`), `oc_external_options`
(`option_id`, `mount_id`, `key`, `value` — `readonly`, `enable_sharing`,
`encrypt`, `filesystem_check_changes`, `previews`, `encoding_compatibility`),
`oc_external_applicable` (`applicable_id`, `mount_id`, `type`, `value` — type 2
= group). `oc_mounts.mount_id` is the FK for these rows.

- **Storage id**: `local::<datadir>/` (`Local::getId`). Production:
  `local::/external/mycloud/` (18) and `local::/external/dashcam/` (21), both
  `local` backend, `auth_backend` NULL.
- **Read-only**: `SetupManager` wraps a mount with `readonly = true` in
  `PermissionsMask(ALL & ~(UPDATE|CREATE|DELETE)) = 17` (`SetupManager.php:226-237`);
  `enable_sharing = false` additionally applies `PermissionsMask(ALL - SHARE) = 15`
  (`SetupManager.php:184-195`). Both production external mounts are `readonly`
  (17); mount 1 also has `enable_sharing = false` (→ `1`). The
  `FilesPlugin`/`View` root adjustment then applies (`& ~(UPDATE|DELETE)` since
  `SystemMountPoint` is not movable).
- **filecache rows**: yes, for `local` backends the rows are in `oc_filecache`
  under storage 18/21 and a listing can be served from the DB. But both mounts
  set `filesystem_check_changes = 1` (`Watcher::CHECK_ALWAYS`), and
  `View::getCacheEntry` calls `$watcher->needsUpdate()` for the **listed node**
  (`View.php:1394-1432`, `Watcher.php:117-128`, `Common.php:367-368`), so PHP may
  `stat` the local path and re-scan (changing etag/mtime/size) before answering.
  For non-local backends (S3/WebDAV/…) the cache is often empty or stale and PHP
  must touch the remote — delegate those.
- `getMountType()` = `external` (or `external-session` for
  `SessionCredentials`) (`ExternalMountPoint.php:28-31`).

```sql
SELECT m.mount_point, m.storage_id, m.root_id, f.path,
       em.storage_backend, em.auth_backend, em.mount_point AS external_mount_point,
       c.value AS datadir, o.key, o.value
FROM oc_mounts m
JOIN oc_filecache f ON f.fileid = m.root_id
JOIN oc_external_mounts em ON em.mount_id = m.mount_id
LEFT JOIN oc_external_config c ON c.mount_id = em.mount_id AND c.key = 'datadir'
LEFT JOIN oc_external_options o ON o.mount_id = em.mount_id
WHERE m.user_id = :uid
  AND m.mount_provider_class = 'OCA\Files_External\Config\ConfigAdapter';
```

## 7. Path resolution for a path INSIDE a mount

The mount is the **longest** `oc_mounts.mount_point` that is a prefix of the DAV
path; the internal path is `root_internal_path` + the path relative to the
mount point, hashed with `md5` (NFC-normalised, no leading slash). The mount's
`storage_id` + that internal path is the row, **not** the home storage.

```sql
WITH inp(dav) AS (VALUES (:dav)),
m AS (
  SELECT inp.dav, mm.mount_point, mm.storage_id, root.path AS root_path
  FROM inp
  JOIN LATERAL (
    SELECT m.mount_point, m.storage_id, m.root_id
    FROM oc_mounts m
    WHERE m.user_id = :uid AND inp.dav LIKE m.mount_point || '%'
    ORDER BY length(m.mount_point) DESC LIMIT 1
  ) mm ON true
  JOIN oc_filecache root ON root.fileid = mm.root_id
)
SELECT f.fileid, f.path, f.storage
FROM m
JOIN oc_filecache f ON f.storage = m.storage_id
 AND f.path_hash = md5(
       CASE WHEN m.root_path = '' THEN substr(m.dav, length(m.mount_point) + 1)
            ELSE m.root_path || '/' || substr(m.dav, length(m.mount_point) + 1) END);
```

Verified on production for all four provider kinds (returned the expected
fileid):

| DAV path | mount_point | storage | root_path | internal path | fileid |
|---|---|---|---|---|---|
| `/aviallon/files/Hytrix/Comptabilité/Relevés bancaires` | `/aviallon/files/Hytrix/` | 25 | `files` | `files/Comptabilité/Relevés bancaires` | 4036053 |
| `/aviallon/files/Partage familial/Informations.md` | `/aviallon/files/Partage familial/` | 11 | `__groupfolders/1` | `__groupfolders/1/Informations.md` | 366634 |
| `/aviallon/files/Documents Sélène/FROM JULIE NGUYEN` | `/aviallon/files/Documents Sélène/` | 3 | `files/Documents Sélène` | `files/Documents Sélène/FROM JULIE NGUYEN` | 1740888 |
| `/aviallon/files/MyCloud Photos Backup/2018-07 - Coupe du monde de football` | `/aviallon/files/MyCloud Photos Backup/` | 18 | `` | `2018-07 - Coupe du monde de football` | 1600008 |

The same `md5`/NFC/normalisation rules from `docs/recon/files-propfind-model.md`
§1 apply to the relative segment.

## 8. ACL / privacy rules

- **Which mounts a user sees**: `oc_mounts` is already **per-user** — every row
  carries `user_id`, and the mount providers only register a mount for users who
  may see it (external applicability, groupfolder group membership, share
  recipient). Production confirms this: only the two applicable groups'
  members have `OCA\Files_External\...` rows, and each share is expanded to one
  row per recipient. A SQL-only reader must still filter `user_id = :uid` and
  must never serve another user's row (the mount's `storage_id`/`root_id` point
  into the **owner's** storage, so leaking a row leaks the owner's fileids).
- **Another user's mount**: not present in their `oc_mounts`, so the path
  resolves to nothing (404) or to their own home storage. The DAV tree already
  restricts `files/<uid>` to the authenticated principal
  (`RootCollection::getChildForPrincipal`), so the sidecar must keep that check
  and additionally never trust a mount row whose `user_id` differs.
- **Read-only mounts in `oc:permissions`**: a read-only share
  (`oc_share.permissions = 1`) yields `row.perms & 1 = 1` on every entry; a
  read-only external mount yields `row.perms & 17`; a read-only groupfolder
  yields `row.perms & group perms`. On the **mount root** `View` then adds
  `|UPDATE|DELETE` only for movable mounts (shares, personal external mounts),
  and `& ~(UPDATE|DELETE)` for the rest; `DavUtil` turns the bits into letters
  (so a read-only share root is `SGDNV` while its children are `RG`). Clients
  read these bits to decide whether to offer write actions; the storage-level
  masks are what actually reject writes.
- `oc:share-types` / `nc:sharees` / `ocs:share-permissions` need `oc_share`
  (and the share manager for attributes); the sidecar already has
  `node_share_rows`/`folder_share_rows` for the home case, but a mount root's
  share state is the super-share, and `SharedStorage` hides the underlying
  shares.

## 9. Concrete phase-2 plan

### (a) Safe and easy

1. **Serve a listing that contains mounts** (home root first). Keep the existing
   cache-children query, then append one entry per `oc_mounts` row under the
   path using the query in §1, in ascending `mount_point` order, with
   `name` = last path component, `etag`/`mtime`/`size` = root row,
   `mimetype` = root mimetype, `permissions` = masked root perms with the
   `|UPDATE|DELETE` / `& ~(UPDATE|DELETE)` branch. Emit `nc:mount-type` and
   `nc:is-mount-root='true'`. Emit the parent's synthetic etag/size/mtime
   (§3c) — see (b).
2. **Serve listings inside a mount for local/external-local/shared/groupfolder
   storages** from `oc_filecache`: resolve `(storage_id, internal_path)` per §7,
   then reuse `file_children(storage_id, fileid)` verbatim; apply the provider's
   permission mask to every row (`& share perms` / `& group perms` / `& readonly
   and sharing masks`). `mount-type` = the mount's provider, `is-mount-root` =
   `false` for descendants.
3. **Cache the mount map per user** at startup / on `UserMountChangedEvent`,
   not per request (the recon's phase-2 approach). The map is small (8 rows for
   this account) and changes rarely.
4. **NFC + normalise** the relative path before hashing inside a mount, exactly
   as for the home.

### (b) Needs care

1. **Parent size/etag** (§3c). Compute from `oc_mounts` + each root row, apply
   the provider masks to the root permissions first, sort by `mount_point`
   ascending, use `relPath = substr(mountPoint, len(parentAbsPath))` (keeps the
   leading and trailing slash → double slash), append permissions as decimal,
   and `md5(parent.etag . '::' . join('::', childEtags))`. Also
   `mtime = max(...)` and `size += Σ`. Do **not** cache/derive it across
   requests — `Propagator` churns the underlying etags.
2. **Groupfolder ACL**. Port `ACLCacheWrapper` (mask each row, **drop rows whose
   masked permissions are 0**), `ACLManager` (relevant paths parent-first,
   `mergeRules`, `applyPermissions`), the per-user mapping set and the base
   permission. Until then, delegate any `oc_group_folders.acl = 1` folder and any
   folder with rows in `oc_group_folders_acl`.
3. **Read-only permissions**. Apply the share/group/external masks to every row
   and to the parent etag's `childEtags`; apply the movable/non-movable
   `|UPDATE|DELETE` / `& ~(UPDATE|DELETE)` branch on mount roots only.
4. **External `filesystem_check_changes = 1`**. Decide whether to serve the
   cached row or delegate; if served, the etag/mtime may lag a scan PHP would
   have triggered. For non-local backends, delegate.
5. **Shares**: the super-share OR and the accepted-only-for-0/1/2 rule; Talk
   (type 11) pending shares **are** mounted.

### (c) Stay delegated

- Writes (PUT/MKCOL/DELETE/MOVE/COPY) and every non-PROPFIND method.
- Groupfolders with `acl = 1` / any `oc_group_folders_acl` row, until the rule
  engine is ported and differentially tested.
- Non-local external backends and anything with `filesystem_check_changes`
  where the DB may be stale.
- Objectstore / server-side encryption (not enabled here).
- Property sets that need the share manager (`oc:share-types`, `nc:sharees`,
  `ocs:share-permissions`, `nc:note`, `nc:hide-download`, `nc:share-attributes`)
  or `OC_Helper::getStorageInfo` (`d:quota-*`) — unless/until implemented.
- `nc:is-encrypted` when the encryption app is enabled, `oc:downloadURL` under
  objectstore (already the pattern in `src/files.rs:960-1000`).

### Parity tests to write (`nextcloud-dav/tests/local/`, real Nextcloud 33.0.5)

Extend `files_parity.sh` (it already canonicalises and diffs PHP vs sidecar) and
`setup.sh` to create the fixtures. For each, diff the full web + desktop property
sets on **PHP vs sidecar**, then assert the specific value:

1. **Home root with mounts** (the 85 % case): Depth 0 and Depth 1; assert
   `getetag == md5(etag::childEtags)`, `oc:size == parent + Σ submounts`,
   `getlastmodified == max`, and that the mount entries are present with the
   right `name`/`oc:fileid`/`nc:mount-type`/`nc:is-mount-root`/`oc:permissions`.
   Create at least one share, one groupfolder and one local external mount.
2. **Listing inside each mount kind** (share, groupfolder root-jail and
   separate-storage, external local): Depth 1; assert `oc:permissions` masking
   and `nc:mount-type`.
3. **Read-only share and read-only external mount**: assert every child's
   `oc:permissions` and the mount root's `|UPDATE|DELETE` behaviour.
4. **Groupfolder ACL**: add an ACL rule that denies a subfolder, then a rule
   that denies READ; assert the denied entry is **absent** from both listings and
   that surviving entries carry the masked permissions. Also test
   `acl_default_no_permission` and a read-only group permission.
5. **Nested mount**: a mount inside a subfolder of a groupfolder/share; assert
   the intermediate folder's `oc:size`/`getetag` include it and that it appears
   in the parent listing.
6. **Path resolution**: PROPFIND Depth 0 on a deep path inside each mount kind;
   assert `oc:fileid` matches the SQL from §7.
7. **Privacy**: PROPFIND as a second user against another user's mount path;
   assert 404 (never a leaked entry).

`files_external` is shipped with the 33.0.5 image (enable it and add a local
mount via `occ files_external:create`); `groupfolders` must be fetched (appstore
or a copied tarball) into the harness image/`custom_apps`, or the ACL test can
seed `oc_group_folders*` directly and force a re-mount.

## 10. Open questions

- **The parent etag on the wire** was not observed (no production write). The
  formula is from source and the value is DB-derived; the local harness must
  confirm byte-for-byte, including the double slash and the ascending order.
- **`getDirectoryContent` parent folder creation**: when a mount is nested and
  its parent folder is missing from the filecache, PHP calls `mkdir` — a read
  request that writes. Confirm whether the sidecar should mirror that or delegate
  such paths.
- **`accepted` filtering for Talk (type 11)**: production `oc_mounts` contains
  `accepted = 0` type-11 mounts; the exact set of types exempt from the
  accepted check is only partly in `getMountsFromSuperShares`.
- **Super-share target/conflict handling**: `adjustTarget` and the
  `files/<name> (n)` rename in `MountProvider::getMountsForUser` are not fully
  modelled; `oc_mounts.mount_point` is the authoritative post-conflict value, but
  the rename side effect (`userStorage->rename`) is a write.
- **Groupfolder `canManageACL`**: the admin/groupfolders-admin inputs (app config
  + `oc_group_folders_manage`) were not fully enumerated.
- **Circle mappings**: whether circles are enabled in production and how their
  tables map to `oc_group_folders_acl.mapping_id`.
- **`filesystem_check_changes`**: whether serving the cached row for external
  local mounts diverges from PHP on a freshly-changed directory, and how often.
- **Stale `oc_mounts`**: today all 45 rows resolve, but the table is a cache; the
  sidecar should tolerate a row whose `root_id`/`storage_id` no longer exists
  (skip it) and should re-read the map on mount-change events rather than trust
  it forever.
