# Files `PROPFIND` — data model & parity recon

Recon for serving `/remote.php/dav/files/<uid>/**` `PROPFIND` (Depth 0/1) from
`nextcloud-dav`. Evidence from the local checkout `nextcloud-server` (36.0.0
dev) and from read-only `SELECT`s against the production DB (`nextcloud`
database, `nextcloud-pg18-4`, 2026-09-18). Production runs 33.0.5; table shapes
below were verified against the production schema, not only the checkout.
No production write was performed.

---

## Report (summary, ≤30 lines)

**Resolve a DAV path to a fileid** (`<rel>` = DAV path after `<uid>/`, NFC-normalised, prefixed with `files/`):
```sql
SELECT f.fileid, f.path, f.name, f.size, f.mtime, f.etag, f.permissions,
       f.encrypted, f.unencrypted_size, f.checksum, f.parent,
       mt.mimetype, mp.mimetype AS mimepart
FROM oc_filecache f
JOIN oc_storages s ON s.numeric_id = f.storage
LEFT JOIN oc_mimetypes mt ON mt.id = f.mimetype
LEFT JOIN oc_mimetypes mp ON mp.id = f.mimepart
WHERE s.id = 'home::<uid>' AND f.path_hash = md5('<nfc>files/<rel>');
```
**List children** (no `ORDER BY` — PHP issues none):
```sql
SELECT f.fileid, f.name, f.size, f.mtime, f.etag, f.permissions,
       mt.mimetype, f.checksum, fe.metadata_etag, md.json AS meta_json
FROM oc_filecache f
LEFT JOIN oc_mimetypes mt ON mt.id = f.mimetype
LEFT JOIN oc_filecache_extended fe ON fe.fileid = f.fileid
LEFT JOIN oc_files_metadata   md ON md.file_id = f.fileid
WHERE f.storage = :numeric_id AND f.parent = :fileid;
```
- **Dangerous properties** (per-child work in PHP / non-row source): `oc:size` dir = `size` **+ submount sizes** (`FileInfo::getSize`); dir etag = `md5(etag.'::'.childEtags)` when submounts exist; `oc:favorite`/`oc:tags` → `oc_vcategory_to_object`+`oc_vcategory`; `oc:comments-unread` → `oc_comments`+`oc_comments_read_markers`; `oc:share-types`/`oc:sharees` → `oc_share` (`getSharesInFolder`); `nc:metadata-*` → `oc_files_metadata.json`; `ocs:share-permissions` → share/`SharedStorage`; `d:quota-*` → storage `free_space` + `OC_Helper::getStorageInfo`; `nc:has-preview` → mimetype/provider (no DB); `oc:checksums` → `oc_filecache.checksum`.
- **Minimal client set**: `getetag`, `getlastmodified`, `resourcetype`, `getcontentlength`+`getcontenttype` (files only), `oc:size`, `oc:fileid`, `oc:id`, `oc:permissions`, `oc:owner-id`, `oc:owner-display-name`, `nc:has-preview`, `oc:favorite`, `oc:comments-unread`, `nc:mount-type`, `oc:checksums`, `d:quota-available-bytes`+`d:quota-used-bytes` (dirs only).
- **Objectstore**: no — `objectstore`/`objectstore_multibucket` absent from `config.php`. **Encryption**: no — `occ encryption:status` → `enabled: false`. **But** `files_external`+`groupfolders` are enabled and the account has real external/groupfolder/received-share mounts.
- **v1 scope**: serve Depth 0/1 only for owned-home paths (`home::<uid>`, internal `files/…`) that are neither a mount root nor a collection containing a mount; return 501 → PHP for everything else (home root, mounts, shares, groupfolders, external, trash/versions). Reads only.
- **Top 5 parity risks**: (1) NFC path normalisation before `md5`; (2) mount points are not in the parent storage's `oc_filecache` and must be merged; (3) directory etag with submounts (`md5(etag::childEtags)`) and `Propagator` churn; (4) the `oc:permissions` letter string + movable-mount injection + `oc:id` instance-id suffix; (5) property 404 semantics / `allprop` fixed list / `propname` treated as `allprop` / namespace prefixes / row order.

---

## 1. Path → storage → filecache

The DAV URL is `/remote.php/dav/files/<uid>/<rel>`. Sabre strips its base
(`/remote.php/dav/`) and the tree path becomes `files/<uid>/<rel>`.

- `OCA\DAV\Files\RootCollection::getChildForPrincipal()` returns a `FilesHome`
  **only** when the requested principal equals the authenticated user; otherwise
  an empty `SimpleCollection` (`apps/dav/lib/Files/RootCollection.php:31-46`).
- `FilesHome` is a `Directory` whose `View` is `Filesystem::getView()`
  (`apps/dav/lib/Files/FilesHome.php:28-31`).
- `Filesystem::getView()` builds the default view with fake root
  `/<uid>/files` (`lib/private/Files/Filesystem.php:365-375`).
- `Directory::getChildren()` → `Folder::getDirectoryListing()`
  (`apps/dav/lib/Connector/Sabre/Directory.php:107-120`,
  `lib/private/Files/Node/Folder.php:96-107`) → `View::getDirectoryContent()`.
- `View::getFileInfo()` (`lib/private/Files/View.php:1439-1470`):
  `$path = normalizePath(fakeRoot . '/' . $rel)`; `$mount = find($path)`;
  `$internalPath = $mount->getInternalPath($path)`; `$data = $cache->get($internalPath)`.
- For a home storage the mount point is `/<uid>/`, the storage id is
  `home::<uid>`, and the internal path is `files/<rel>` — the **`files/` prefix
  trick**: the home root itself is the row `path = 'files'`, and `cache/`,
  `files_trashbin/`, `files_versions/`, `uploads/` are siblings in the same
  storage (verified: storage `home::aviallon` has rows `files`,
  `files_trashbin`, `files_versions`, `uploads`, `cache`).
- `Cache::get()` looks the row up by `path_hash`
  (`lib/private/Files/Cache/CacheQueryBuilder.php:85-88`), and `path_hash =
  md5($path)` (`lib/private/Files/Cache/Cache.php:495`). The lookup is a unique
  index scan on `fs_storage_path_hash (storage, path_hash)` (verified in
  `pg_indexes`), which is why PHP does not use `path`.
- `Cache::normalize()` is `trim(OC_Util::normalizeUnicode($path), '/')`
  (`lib/private/Files/Cache/Cache.php:1252-1254`); `normalizeUnicode` applies
  `Normalizer::normalize()` (NFC) (`lib/private/legacy/OC_Util.php:652-664`).
  `Filesystem::normalizePath()` additionally collapses `//`, `/./`, trailing
  `/.` and backslashes (`lib/private/Files/Filesystem.php:619-657`). **The
  sidecar must do the same before hashing, or decomposed-Unicode paths 404.**
- `oc_storages.id` is the string storage id, `numeric_id` the FK stored in
  `oc_filecache.storage`. `oc_filecache.mimetype`/`mimepart` are ids into
  `oc_mimetypes` (verified: `2 = httpd/unix-directory`).

**Exact resolve SQL** (proved against production — returns `fileid = 1066107`,
`path = files/Telecom SP/Stages/Ingénieur/Enioka/www.service-public.fr/particuliers/vosdroits`,
mimetype `httpd/unix-directory`, etag `61de14d15a699`, permissions `31`):

```sql
SELECT f.fileid, f.path, f.name, f.size, f.mtime, f.etag, f.permissions,
       f.encrypted, f.unencrypted_size, f.checksum, f.parent,
       mt.mimetype, mp.mimetype AS mimepart
FROM oc_filecache f
JOIN oc_storages s ON s.numeric_id = f.storage
LEFT JOIN oc_mimetypes mt ON mt.id = f.mimetype
LEFT JOIN oc_mimetypes mp ON mp.id = f.mimepart
WHERE s.id = 'home::<uid>' AND f.path_hash = md5('<nfc>files/<rel>');
```

**Exact children SQL.** PHP's `Cache::getFolderContentsById()`
(`lib/private/Files/Cache/Cache.php:220-248`) selects the columns from
`CacheQueryBuilder::selectFileCache()` (`CacheQueryBuilder.php:51-56`) — a LEFT
JOIN to `oc_filecache_extended` (`metadata_etag`, `creation_time`,
`upload_time`) — and `selectMetadata()` (`CacheQueryBuilder.php:117-121`) LEFT
JOINs `oc_files_metadata`. It filters on `parent` and `storage` only, and issues
**no `ORDER BY`**:

```sql
SELECT f.fileid, f.path, f.name, f.size, f.mtime, f.etag, f.permissions,
       f.encrypted, f.unencrypted_size, f.checksum, f.parent,
       mt.mimetype, mp.mimetype AS mimepart,
       fe.metadata_etag, fe.creation_time, fe.upload_time,
       md.json AS meta_json, md.sync_token AS meta_sync_token
FROM oc_filecache f
LEFT JOIN oc_mimetypes mt ON mt.id = f.mimetype
LEFT JOIN oc_mimetypes mp ON mp.id = f.mimepart
LEFT JOIN oc_filecache_extended fe ON fe.fileid = f.fileid
LEFT JOIN oc_files_metadata   md ON md.file_id = f.fileid
WHERE f.storage = :numeric_id AND f.parent = :fileid;
```

Measured on production: the 8 811-child directory (`fileid = 1066107`) lists in
**33 ms** in Postgres (PHP: ~10 s). `oc_filecache` currently holds 1 595 771
rows; `home::aviallon` alone has 475 021.

## 2. Mounts and shares

`oc_mounts` is the materialised mount table (columns: `id`, `storage_id`,
`root_id`, `user_id`, `mount_point`, `mount_id`, `mount_provider_class`,
`mount_point_hash`; unique index `mounts_user_root_path_index (user_id, root_id,
mount_point_hash)`). On production it has 45 rows and is the authoritative
source for what appears under a user's `files/` tree.

Verified `oc_mounts` rows for `aviallon` (8):

| mount_point | provider | storage | root_id |
|---|---|---|---|
| `/aviallon/` | `OC\Files\Mount\LocalHomeMountProvider` | `home::aviallon` (2) | `files` row |
| `/aviallon/files/Documents Sélène/` | `OCA\Files_Sharing\MountProvider` | `home::selene` (3) | 483 |
| `/aviallon/files/Mariage/` | `OCA\Files_Sharing\MountProvider` | `home::selene` (3) | 602520 |
| `/aviallon/files/MyCloud Photos Backup/` | `OCA\Files_External\Config\ConfigAdapter` | `local::/external/mycloud/` (18) | 1599872 |
| `/aviallon/files/Dashcam (ceph)/` | `OCA\Files_External\Config\ConfigAdapter` | `local::/external/dashcam/` (21) | 3599244 |
| `/aviallon/files/Partage familial/` | `OCA\GroupFolders\Mount\MountProvider` | `local::/var/www/html/data/` (11) | 366605 |
| `/aviallon/files/Administratif commun/` | `OCA\GroupFolders\Mount\MountProvider` | (11) | 1772168 |
| `/aviallon/files/Hytrix/` | `OCA\GroupFolders\Mount\MountProvider` | `local::/var/www/html/data/__groupfolders/3/` (25) | 3981487 |

Key consequences:

- **Mount points are not rows in the parent storage's `oc_filecache`.**
  `View::getDirectoryContent()` merges them from the mount manager after the
  cache query (`lib/private/Files/View.php:1600-1690`). Proof: `home::aviallon`
  has 34 rows with `parent = 7` (the `files` row) but the real home listing has
  41 entries (34 + 7 mounts). A filecache-only Depth 1 listing silently omits
  every share/groupfolder/external mount.
- A **received share** is exposed under `/<sharee>/files/<file_target>/` by an
  `oc_mounts` row whose `storage_id` is the **owner's** home storage and whose
  `root_id` equals `oc_share.file_source`; e.g. share id 43: `share_type = 0`,
  `share_with = 'aviallon'`, `uid_owner = 'selene'`, `file_source = 483`,
  `file_target = '/Documents Sélène'`, `permissions = 31`, `accepted = 1` →
  mount `/aviallon/files/Documents Sélène/`, storage 3, internal
  `files/Documents Sélène`.
- `oc_share` columns that matter for a listing: `share_type`, `share_with`,
  `uid_owner`, `uid_initiator`, `item_type`, `file_source`, `file_target`,
  `permissions`, `accepted`, `parent` (share-type constants:
  `lib/public/Share/IShare.php:30-105`; production has types 0, 1, 3, 4, 10,
  11). `oc:share-types` is produced by `SharesPlugin`, which calls
  `getSharesBy`/`getSharedWith` per child, prefetched per folder by
  `getSharesInFolder` (`apps/dav/lib/Connector/Sabre/SharesPlugin.php:99-125,
  192-217`; provider query at `lib/private/Share20/DefaultShareProvider.php:633-700`).
- **Groupfolders** add `oc_group_folders` (`folder_id`, `mount_point`,
  `quota`, `acl`, `root_id`, `storage_id`, `options`), `oc_group_folders_groups`
  and, when `acl = 1`, `oc_group_folders_acl`. Production groupfolder 3 has
  `acl = 1` and `options = {"separate-storage":true}`; its root is
  `local::/var/www/html/data/__groupfolders/3/`, path `files`, whereas folders
  1/2 live in the shared root storage with path `__groupfolders/<id>`.
- **External storage** mounts come from `oc_external_mounts` (`mount_id`,
  `mount_point`, `storage_backend`, `auth_backend`, `priority`, `type`);
  production has two `local` mounts (`/MyCloud Photos Backup`,
  `/Dashcam (ceph)`).

**Decision (v1): a mount root or any path under a mount must be delegated to
PHP.** `oc_filecache + oc_mounts + oc_share` alone cannot reproduce:
groupfolder ACLs and `separate-storage`; `files_external` backends (S3 keys,
not paths); the `SharedStorage` permission wrapper and `getShareAttributes` /
`hide-download` / `note`; `nc:mount-type`; `oc:share-types` /
`ocs:share-permissions`; the `S`/`M` permission letters; and the
movable-mount `UPDATE|DELETE` injection. A collection that merely *contains* a
mount must also delegate (or the listing drops the mount entry). For the owned
home this is expressed as: if any `oc_mounts.mount_point` for the user equals or
is a prefix of the requested path (excluding the home mount itself), delegate.

## 3. Permissions

`{oc}permissions` is `DavUtil::getDavPermissions($info, $parent)`
(`apps/dav/lib/Connector/Sabre/Node.php:332-334`,
`lib/public/Files/DavUtil.php:36-75`). Letters, in this exact order:

| letter | condition | bit (`lib/public/Constants.php:23-49`) |
|---|---|---|
| `S` | `$info->isShared()` (mount instanceof `ISharedMountPoint`) | — |
| `R` | `permissions & PERMISSION_SHARE` | 16 |
| `M` | `$info->isMounted()` (not home, not shared) | — |
| `G` | `permissions & PERMISSION_READ` | 1 |
| `D` | `permissions & PERMISSION_DELETE` | 8 |
| `N` | `canRename($info,$parent)` | — |
| `V` | `permissions & PERMISSION_UPDATE` | 2 |
| `W` | files only, if writable | — |
| `C` `K` | dirs only, `permissions & PERMISSION_CREATE` | 4 |

`canRename` (`DavUtil.php:78-96`) = movable-mount root, or `isUpdateable()`, or
(`isDeletable() && parent->isCreatable()`), except the home `files` root.
`isShared`/`isMounted` come from the mount object (`lib/private/Files/FileInfo.php:240-260`).
`FileInfo::getPermissions()` is just `oc_filecache.permissions`
(`lib/private/Files/FileInfo.php:196-198`); the extra bits come from
`View::getFileInfo` (`IMovableMount && internalPath === ''` → `|DELETE`,
`View.php:1457-1459`) and `View::getDirectoryContent` (mount roots:
`|UPDATE|DELETE` for movable mounts, else `& ~(UPDATE|DELETE)`,
`View.php:1655-1665`).

`ocs:share-permissions` (`{http://open-collaboration-services.org/ns}share-permissions`)
is `Node::getSharePermissions($uid)` (`Node.php:245-286`): shared storage →
the share's permissions, else the file's permissions; then `|UPDATE|DELETE` for
a non-movable mount root, and files strip `CREATE|DELETE`.

Examples on production: a plain home dir is `RGDNVCK` (31), a file `RGDNVW`
(27), the external mount root 1599872 has `permissions = 23` (`RGDV` + no D).

## 4. The property set

`FilesPlugin` is the main handler (`apps/dav/lib/Connector/Sabre/FilesPlugin.php`).
"row" = already in the `oc_filecache` row; "folder query" = one query per
collection, not per child.

| property | source | serialisation | cost |
|---|---|---|---|
| `{DAV:}getetag` | `Node::getETag()` → `'"'.FileInfo::getEtag().'"'` (`Node.php:189-191`) | quoted string | row; **dir with submounts → `md5(etag.'::'.childEtags)`** (`FileInfo.php:150-160`) |
| `{DAV:}getlastmodified` | `Node::getLastModified()` (`Node.php:170-172`) | RFC 1123 via `GetLastModified` (`3rdparty/.../GetLastModified.php:57-61`) | row (`mtime`) |
| `{DAV:}getcontentlength` | `CorePlugin::propFind` only for `IFile` (`3rdparty/sabre/dav/lib/DAV/CorePlugin.php:756`) | int | **files only; dirs → 404** |
| `{DAV:}getcontenttype` | `File::getContentType()` (`File.php:570-578`); PROPFIND returns the raw `oc_mimetypes` value | string | **files only** |
| `{DAV:}resourcetype` | `Server::getResourceTypeForNode` (`CorePlugin.php:785-787`) | `<d:collection/>` or empty | row |
| `{DAV:}displayname` | `FilesPlugin.php:472-474` → `getName()` | string | row |
| `{oc}size` | `FilesPlugin.php:403-405` (Node) + `523-525` (Directory) → `FileInfo::getSize(true)` | int | row **+ submount sizes** |
| `{oc}fileid` | `FilesPlugin.php:322-324` → `getInternalFileId()` (int) | int | row |
| `{oc}id` | `FilesPlugin.php:318-320` → `DavUtil::getDavFileId()` = `sprintf('%08d',$id).instanceid` (`DavUtil.php:25-30`) | `00001066oc…` | row + config |
| `{oc}owner-id` / `{oc}owner-display-name` | `FilesPlugin.php:360-380` | string | row owner / user manager |
| `{nc}has-preview` | `FilesPlugin.php:400-402` → `json_encode(IPreview::isAvailable())` | `true`/`false` | mimetype/provider (no DB) |
| `{oc}favorite` / `{oc}tags` | `TagsPlugin.php:246-258`, prefetch `:205-233` | `1`/`0`; `<oc:tag>` list | **`oc_vcategory_to_object`+`oc_vcategory`** (`lib/private/Tags.php:41-42,129-160`) |
| `{oc}share-types` / `{nc}sharees` | `SharesPlugin.php:227-238`, prefetch `:192-217` | `<oc:share-type>` list | **`oc_share`** (`getSharesInFolder`) |
| `{oc}comments-unread` / `-count` / `-href` | `CommentPropertiesPlugin.php:129-140`, prefetch `:99-119` | int / href | **`oc_comments` + `oc_comments_read_markers`** (`lib/private/Comments/Manager.php:686-710`) |
| `{nc}mount-type` | `FilesPlugin.php:406-408` → `IMountPoint::getMountType()` | `''`/`shared`/`external`/`external-session` (`MountPoint.php:281`, `SharedMount.php:185`, `ExternalMountPoint.php:29`) | mount object |
| `{nc}is-mount-root` | `FilesPlugin.php:416-418` → `internalPath === ''` | `true`/`false` | row |
| `{oc}checksums` | `FilesPlugin.php:508-515` → `FileInfo::getChecksum()` | `<oc:checksum>TYPE:hash</oc:checksum>` (`ChecksumList.php:50-54`) | row (`oc_filecache.checksum`) |
| `{oc}downloadURL` | `FilesPlugin.php:493-499` → `File::getDirectDownload()` (`File.php:584-600`) | URL or `false` | `Common::getDirectDownloadById` returns `false` (`Common.php:500-502`); S3 returns a presigned URL (`ObjectStoreStorage.php:934-938`) |
| `{DAV:}quota-available-bytes` / `-used-bytes` | `CorePlugin.php:762-775` for `IQuota` = `Directory`; `Directory::getQuotaInfo()` (`Directory.php:338-360`) → `OC_Helper::getStorageInfo` (`OC_Helper.php:160-290`) | int | **dirs only**; `used` = that dir's raw size, `free` = storage `free_space` |
| `{nc}metadata-<key>` | `FilesPlugin.php:456-458` loops `FileInfo::getMetadata()` | the JSON value | **`oc_files_metadata.json`** (LEFT JOIN in the same query) |
| `{nc}metadata_etag` | constant `FilesPlugin.php:69` but **no handler found** in this checkout | — | `oc_filecache_extended.metadata_etag` (open question) |
| `{DAV:}owner` | **not served for files** — `DavAclPlugin::propFind` returns early for `Node` (`DavAclPlugin.php:70-73`) | 404 | — |
| `{DAV:}current-user-privilege-set` | same early return | 404 | — |
| `{DAV:}supported-report-set` | `CorePlugin.php:777-783` iterates plugins | see §6 | static |
| `{ocs}share-permissions` | `FilesPlugin.php:330-337` → `getSharePermissions` | int | share object |

## 5. What real clients actually request

- **Web UI** (`@nextcloud/files` v4 `defaultDavProperties`, extracted from
  `nextcloud-server/dist/*.js.map` → `node_modules/@nextcloud/files/dist/dav.mjs`):
  `d:getcontentlength`, `d:getcontenttype`, `d:getetag`, `d:getlastmodified`,
  `d:creationdate`, `d:displayname`, `d:quota-available-bytes`,
  `d:resourcetype`, `nc:has-preview`, `nc:is-encrypted`, `nc:mount-type`,
  `oc:comments-unread`, `oc:favorite`, `oc:fileid`, `oc:owner-display-name`,
  `oc:owner-id`, `oc:permissions`, `oc:size`.
  `apps/files_sharing/src/init.ts:22-27` additionally registers `nc:note`,
  `nc:sharees`, `nc:hide-download`, `nc:share-attributes`, `oc:share-types`,
  `ocs:share-permissions`; `apps/files/src/init.ts:77-79` registers
  `nc:hidden`, `nc:is-mount-root`, `nc:metadata-blurhash`;
  `apps/files/src/services/LivePhotos.ts:13` registers
  `nc:metadata-files-live-photo`.
- **Desktop client** (`LsColJob::defaultProperties`,
  `nextcloud/desktop src/libsync/networkjobs.cpp`):
  `resourcetype`, `getlastmodified`, `getcontentlength`, `getetag`,
  `quota-available-bytes`, `quota-used-bytes`, `oc:size`, `oc:id`,
  `oc:fileid`, `oc:downloadURL`, `oc:dDC`, `oc:permissions`, `oc:checksums`,
  `nc:is-encrypted`, `nc:metadata-files-live-photo`, `nc:share-attributes`,
  `oc:data-fingerprint` (root only), `oc:share-types` (server ≥ 10),
  `nc:is-mount-root`, plus `nc:lock*` when `files_lock` is available. It reads
  `oc:id` (not `oc:fileid`) as the file id, and `oc:size` for folder size.
- **gvfs** (GNOME, from a captured request): `creationdate`, `displayname`,
  `getcontentlength`, `getcontenttype`, `getetag`, `getlastmodified`,
  `resourcetype` (Depth 0 and 1).
- **rclone** (captured `--dump bodies`): `displayname`, `getlastmodified`,
  `getcontentlength`, `resourcetype`, `getcontenttype`, `oc:checksums`.
- **cadaver/litmus**: RFC 4918 `allprop`/`propname` and explicit prop sets
  (`getetag`, `resourcetype`, `getcontentlength`, `getlastmodified`,
  `displayname`, `getcontenttype`). No Nextcloud extensions.
- **DAVx5/Android** is calendar/contacts first; its WebDAV file provider uses
  `dav4jvm`, whose core set is the RFC 4918 properties above. (Not directly
  verified from source here.)

**Minimal satisfying set** (union of web + desktop + gvfs + rclone, ignoring
lock props): `d:getetag`, `d:getlastmodified`, `d:resourcetype`,
`d:getcontentlength`, `d:getcontenttype`, `d:displayname`, `oc:size`,
`oc:fileid`, `oc:id`, `oc:permissions`, `oc:owner-id`, `oc:owner-display-name`,
`oc:checksums`, `oc:favorite`, `oc:comments-unread`, `nc:has-preview`,
`nc:mount-type`, `nc:is-mount-root`, `d:quota-available-bytes`,
`d:quota-used-bytes`. Cheap to add anyway: `oc:share-types` (needs `oc_share`),
`nc:metadata-*` (already joined), `d:creationdate` (`creation_time`).

## 6. Depth and method semantics

- **Depth 0** = the node; **Depth 1** = node + children; **absent** defaults to
  1 (`CorePlugin::httpPropFind` → `getHTTPDepth(1)`,
  `3rdparty/sabre/dav/lib/DAV/CorePlugin.php:322`).
- **Depth: infinity is NOT refused.** `Server::$enablePropfindDepthInfinity`
  defaults to `false`, and both `httpPropFind` and `getPropertiesIteratorForPath`
  silently coerce any non-zero depth to 1 (`CorePlugin.php:327-331`,
  `3rdparty/sabre/dav/lib/DAV/Server.php:961-963`). Reproduce this (do not
  return 403).
- **`allprop`** (empty body or `<d:allprop/>`) is a **fixed 7-property list**:
  `getlastmodified`, `getcontentlength`, `resourcetype`, `quota-used-bytes`,
  `quota-available-bytes`, `getetag`, `getcontenttype`
  (`3rdparty/sabre/dav/lib/DAV/PropFind.php:53-64`); 404s are stripped from the
  response (`PropFind::getResultForMultiStatus`).
- **`propname`** is ignored by Sabre's request parser, which only understands
  `{DAV:}prop` and `{DAV:}allprop`
  (`3rdparty/sabre/dav/lib/DAV/Xml/Request/PropFind.php:57-72`), so it behaves
  as `allprop`.
- **Explicit `prop`**: a 200 propstat is emitted first, then a 404 propstat, and
  property order within a propstat follows request order
  (`PropFind::getResultForMultiStatus`). A 404 entry looks like
  `<d:propstat><d:prop><x/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>`
  (see the rclone capture where `displayname`/`quota-*` come back 404).
- **Framing**: `<d:multistatus>` with `<d:response><d:href>…</d:href>…`.
  `href = baseUri + encodePath(path)`, trailing `/` for collections
  (`3rdparty/sabre/dav/lib/DAV/Xml/Element/Response.php:120`). The root element
  declares **every** namespace in the writer's `namespaceMap` at once —
  `xmlns:d="DAV:"`, `xmlns:s="http://sabredav.org/ns"`
  (`3rdparty/sabre/dav/lib/DAV/Xml/Service.php:43-46`) plus `xmlns:oc` and
  `xmlns:nc` registered by the plugins (`Writer.php:150-153`). `PropfindCompressionPlugin`
  gzips the body when `Accept-Encoding: gzip`
  (`apps/dav/lib/Connector/Sabre/PropfindCompressionPlugin.php:45-58`).
  `Prefer: return=minimal` strips 404 propstats (`CorePlugin.php:341-345`).
- **`{DAV:}sync-collection` on files**: the Sync plugin is registered globally
  (`apps/dav/lib/Server.php:194`) but files nodes do not implement
  `ISyncCollection`, so `Sync\Plugin::syncCollection` throws `ReportNotSupported`
  → **415** (`3rdparty/sabre/dav/lib/DAV/Sync/Plugin.php:104-106`,
  `Exception/ReportNotSupported.php:18`). It can stay on PHP.
- **`supported-report-set`** for files is assembled from every plugin:
  DAVACL's `expand-property`/`principal-*` (`DAVACL/Plugin.php:166-172`),
  `FilesReportPlugin`'s `{http://owncloud.org/ns}filter-files`
  (`FilesReportPlugin.php:101-103`, returned for *every* URI),
  `CommentsPlugin`'s `{http://owncloud.org/ns}filter-comments`
  (`CommentsPlugin.php:128`), and `sync-collection` only for sync collections.
- **`REPORT`**: `{http://owncloud.org/ns}filter-files` (favorites, fileid
  filters, limits) is `FilesReportPlugin`; `{DAV:}searchrequest` is Sabre's
  Search plugin over `FileSearchBackend`. Both can stay on PHP for v1.

## 7. Etags

- A file's etag is the stored `oc_filecache.etag`, quoted on output
  (`Node::getETag`, `Node.php:189-191`). It is not `md5(content)` at read time;
  it is whatever the scanner/propagator last wrote.
- **Directory etags propagate on child changes.** `Propagator::propagateChange()`
  updates every parent row's `etag` to the *same* `uniqid()` on any write below
  it (`lib/private/Files/Cache/Propagator.php:47-60,127`). A read-only sidecar
  therefore just serves the current column value; it must not recompute it.
- **But a directory with submounts has a synthetic etag.** `FileInfo::getEtag()`
  returns `md5($data['etag'] . '::' . implode('::', $childEtags))` when
  `$subMounts` exist (`lib/private/Files/FileInfo.php:150-160`), where each
  child tag is `relativePath.'/'.$etag.$permissions` (`FileInfo.php:290-300`).
  So a directory's wire etag can differ from `oc_filecache.etag` — another
  reason to delegate mount-bearing directories.
- There is **no unique constraint on `oc_filecache.etag`** (only
  `fs_storage_path_hash (storage, path_hash)` and the usual indexes). The
  `oc_files_metadata` table has its own `metadata_etag`? No — `metadata_etag`
  lives in `oc_filecache_extended` (unique `fileid`), while `oc_files_metadata`
  is unique on `file_id` and holds the JSON metadata.
- **A directory's `{DAV:}getetag` IS served** (`Node::getETag` works for
  `Directory`); only `getcontentlength`/`getcontenttype` are file-only.

## 8. Objectstore / encryption pitfalls

Checked on production `config/config.php` (values not printed):

- `objectstore` — **absent**; `objectstore_multibucket` — **absent**; no S3
  primary storage. (Keys present: `instanceid`, `datadirectory`, `dbtype`,
  `dbname`, `dbhost`, `dbport`, `dbuser`, `dbtableprefix`, `memcache.*`,
  `redis`, `passwordsalt`, `secret`, `dbpassword`, ….)
- `encryption` — **absent** from `config.php`, and `occ encryption:status`
  reports `enabled: false` (app `encryption` 2.21.0 installed but disabled).
  No server-side encryption.
- **However** `files_external` 1.25.1 and `groupfolders` 21.0.15 are enabled,
  and `oc_mounts`/`oc_storages` show real external (`local::/external/mycloud/`,
  `local::/external/dashcam/`), groupfolder (`local::/var/www/html/data/__groupfolders/3/`)
  and received-share mounts. So the "naive `oc_filecache`-only" implementation
  is still wrong here — not because of S3/encryption, but because of mounts.
- If objectstore or encryption were enabled: an S3 object key is not the
  `oc_filecache` path, `oc:downloadURL` becomes a presigned URL
  (`ObjectStoreStorage.php:934-938`), and encrypted rows carry
  `encrypted = 1` / `unencrypted_size` (size is the encrypted size unless
  `getSize()` substitutes `unencrypted_size`, `FileInfo.php:164-185`). A v1
  should detect `encrypted != 0` (or the `encryption` app) and delegate.

## 9. Parity checklist and v1 scope

**v1 scope.** Native `PROPFIND` Depth 0/1 only for the authenticated user's own
home storage (`oc_storages.id = 'home::<uid>'`), internal path `files/<rel>`,
where the requested node is not a mount root and no `oc_mounts` row for the user
is equal to or below the requested path. Return 501 (nginx → PHP) for: the home
root, any mount root/path under a mount, received shares, groupfolders,
external storage, and anything not in `files/` (trash, versions). Reads only;
every other method keeps falling back.

**Must get right for the common case:**

1. NFC-normalise + `normalizePath` the path, prefix `files/`, `md5` it, and look
   up by `(storage, path_hash)`.
2. Join `oc_mimetypes`; map `mimetype = httpd/unix-directory` to a collection.
3. Quote the etag; format `getlastmodified` as RFC 1123 GMT.
4. `getcontentlength`/`getcontenttype` only for files; `oc:size` for both;
   quota only for dirs.
5. The exact `oc:permissions` letter order and the movable-mount
   `UPDATE|DELETE` injection; `oc:id` = 8-digit zero-padded id + instance id.
6. 200 propstat then 404 propstat, request order, `allprop` fixed list,
   `propname` as `allprop`, depth infinity → 1, no `ORDER BY`.
7. Root namespaces `d`, `s`, `oc`, `nc`; trailing `/` on collection hrefs.

**Top 5 ways to be subtly wrong:**

1. **Unicode/path normalisation.** Not NFC-normalising (or not collapsing
   `//`/`/./`) before `md5` makes a valid path 404, and would differ from PHP
   for macOS/NFC-mixed clients.
2. **Mounts.** Serving the home root or a directory that contains a mount from
   `oc_filecache` alone silently drops share/groupfolder/external entries (and
   their `S`/`M` permissions, `nc:mount-type`, `oc:share-types`).
3. **Directory etag.** A dir with submounts has
   `md5(etag::childEtags)`, not `oc_filecache.etag`; also the etag churns via
   `Propagator`, so caching/deriving it is wrong.
4. **Permissions.** Letter order, the mount-root `|UPDATE|DELETE`, the file
   `& ~(CREATE|DELETE)`, and `oc:id`'s instance-id suffix are all easy to get
   wrong; `oc:permissions` drives the web UI's write affordances.
5. **Property semantics.** Emitting `getcontentlength`/`getcontenttype` for
   dirs, omitting the 404 propstat, sorting children, treating `propname`
   correctly (it is *not* a name-only listing), or forgetting the `oc`/`nc`
   namespace declarations all break byte parity.

## Open questions

- `nc:is-encrypted` is requested by the web UI and desktop client, but no
  server-side handler exists in this checkout (`grep` finds it only in
  `apps/files/src`). It is probably provided by the (disabled) encryption app or
  is always 404. Confirm with a live PROPFIND.
- `nc:rich-workspace` / `nc:rich-workspace-file` are not in `nextcloud-server`;
  they come from the Text app (not in this checkout).
- `groupfolders` 21.0.15 is not in this checkout, so its `getMountType()`
  (documented as `group`) and ACL permission computation were not read from
  source.
- `{DAV:}owner` / `{DAV:}current-user-privilege-set` are inferred to be 404 for
  files from `DavAclPlugin.php:70-73`; a live probe should confirm.
- `oc_mounts` completeness for pending shares (`oc_share.accepted = 0`) and for
  LDAP/circle groups was not verified.
- `oc:id` needs the `instanceid` from `config.php`, which `nextcloud-dav`'s
  config parser does not currently read.
- Whether `Prefer: return=minimal` is sent by the `webdav` JS library / desktop
  client was not verified; it changes whether 404 propstats appear.
