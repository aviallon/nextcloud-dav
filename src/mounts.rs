// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The mount model for `/remote.php/dav/files/<uid>/**` `PROPFIND`.
//!
//! A received share, a groupfolder or an external storage is an `oc_mounts`
//! row, **not** a child row in the parent storage's `oc_filecache`. Serving a
//! listing from the cache alone silently drops those entries, and a directory's
//! `oc:size`/`getetag` include its submounts (`FileInfo::addSubEntry`). This
//! module reproduces the mount merge, the provider permission masks, the
//! synthetic parent etag/size/mtime, and path resolution inside a mount.
//!
//! The model is documented in `docs/recon/files-mounts-model.md`. Anything the
//! model cannot reproduce exactly (groupfolder ACLs, non-local externals,
//! circles, external `filesystem_check_changes`) marks the mount **unservable**
//! so the caller answers `501` (delegate to PHP) instead of risking a `404` or a
//! wrong listing.
//!
//! The per-user mount map is cached in-process with a short TTL (the user asked
//! for this explicitly): a newly created mount must not be reported as missing,
//! so a stale/unavailable map delegates rather than 404s.

use crate::db::{md5_hex, Db};
use crate::error::Result;
use crate::files::normalize_rel;
use crate::model::FileCacheRow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const PERMISSION_READ: i64 = 1;
const PERMISSION_UPDATE: i64 = 2;
const PERMISSION_DELETE: i64 = 8;
const PERMISSION_ALL: i64 = 31;

/// `OC\Files\Mount\LocalHomeMountProvider`.
pub const PROVIDER_HOME: &str = "OC\\Files\\Mount\\LocalHomeMountProvider";
/// `OCA\GroupFolders\Mount\MountProvider`.
pub const PROVIDER_GROUPFOLDER: &str = "OCA\\GroupFolders\\Mount\\MountProvider";
/// `OCA\Files_Sharing\MountProvider`.
pub const PROVIDER_SHARE: &str = "OCA\\Files_Sharing\\MountProvider";
/// `OCA\Files_External\Config\ConfigAdapter`.
pub const PROVIDER_EXTERNAL: &str = "OCA\\Files_External\\Config\\ConfigAdapter";

/// How long a loaded per-user mount map is trusted before it is reloaded.
pub const MOUNT_CACHE_TTL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Raw rows (loaded by `Db::*` and turned into `MountInfo` here)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MountBaseRow {
    pub mount_point: String,
    pub provider_class: String,
    pub mount_id: Option<i64>,
    pub storage_id: i64,
    pub root_id: i64,
    pub storage_string: String,
    pub root_path: Option<String>,
    pub root_name: Option<String>,
    pub root_size: Option<i64>,
    pub root_mtime: Option<i64>,
    pub root_etag: Option<String>,
    pub root_permissions: Option<i64>,
    pub root_encrypted: Option<i64>,
    pub root_unencrypted_size: Option<i64>,
    pub root_mimetype: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GroupFolderGroupRow {
    pub folder_id: i64,
    pub permissions: i64,
    pub group_id: Option<String>,
    pub circle_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GroupFolderMetaRow {
    pub folder_id: i64,
    pub acl: i64,
    pub quota: i64,
    pub storage_id: i64,
    pub acl_default_no_permission: bool,
}

/// A row of `oc_group_folders_acl` joined to its `oc_filecache` path.
#[derive(Debug, Clone)]
pub struct GroupFolderAclRow {
    pub storage_id: i64,
    pub path: String,
    pub mapping_type: String,
    pub mapping_id: String,
    pub mask: i64,
    pub permissions: i64,
}

/// A row of `oc_group_folders_manage`.
#[derive(Debug, Clone)]
pub struct GroupFolderManageRow {
    pub folder_id: i64,
    pub mapping_type: String,
    pub mapping_id: String,
}

/// One `Rule` (`lib/ACL/Rule.php`): `permissions` is already masked by `mask`
/// in the constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclRule {
    pub path: String,
    pub mapping_type: String,
    pub mapping_id: String,
    pub mask: i64,
    pub permissions: i64,
}

/// The groupfolder ACL model for one folder (`lib/ACL/ACLManager.php`).
#[derive(Debug, Clone)]
pub struct GroupFolderAcl {
    pub folder_id: i64,
    pub storage_id: i64,
    /// `oc_group_folders.acl_default_no_permission` -> `getBasePermission()`.
    pub base_permission: i64,
    /// Rules for this folder's storage, filtered to the user's user/group
    /// mappings.
    pub rules: Vec<AclRule>,
    /// A rule targets a circle: membership is not resolvable from the core
    /// schema, so the mount delegates.
    pub has_circle_rule: bool,
}

impl GroupFolderAcl {
    /// `ACLManager::getACLPermissionsForPath()` plus the `ACLCacheWrapper`
    /// READ gate (`inShare` is false for the user's own mount).
    pub fn permissions_for_path(&self, path: &str) -> i64 {
        let relevant = relevant_paths(path);
        let mut permissions = self.base_permission;
        for candidate in &relevant {
            let rules: Vec<&AclRule> = self
                .rules
                .iter()
                .filter(|rule| &rule.path == candidate)
                .collect();
            if rules.is_empty() {
                continue;
            }
            let (mask, allow) = merge_rules(&rules);
            permissions = apply_permissions(mask, allow, permissions);
        }
        if permissions & PERMISSION_READ == PERMISSION_READ {
            permissions
        } else {
            0
        }
    }
}

/// `ACLManager::getRelevantPaths()`: the path and every parent, parent-first.
fn relevant_paths(path: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current = path.trim_matches('/').to_string();
    while !current.is_empty() {
        paths.push(current.clone());
        current = match current.rfind('/') {
            Some(index) => current[..index].to_string(),
            None => String::new(),
        };
    }
    // `ksort`/`uksort(strlen)`: a parent is always a prefix, so length order is
    // parent-first.
    paths.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    paths
}

/// `Rule::mergeRules()`: OR the masks and the (already masked) permissions.
fn merge_rules(rules: &[&AclRule]) -> (i64, i64) {
    let mut mask = 0;
    let mut permissions = 0;
    for rule in rules {
        mask |= rule.mask;
        permissions |= rule.permissions;
    }
    (mask, permissions)
}

/// `Rule::applyPermissions()`.
fn apply_permissions(mask: i64, permissions: i64, current: i64) -> i64 {
    let deny_mask = !mask | permissions;
    (current & deny_mask) | (mask & permissions)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountShareRow {
    pub file_source: i64,
    pub id: i64,
    pub share_type: i64,
    pub share_with: Option<String>,
    pub permissions: i64,
    pub note: Option<String>,
    pub hide_download: i64,
    pub attributes: Option<String>,
    pub uid_owner: String,
    pub accepted: i64,
    pub stime: i64,
    /// `oc_users.displayname` for the sharee (type 0/2).
    pub user_displayname: Option<String>,
    /// `oc_groups.displayname` for the sharee (type 1).
    pub group_displayname: Option<String>,
    /// `oc_users.displayname` of the share owner.
    pub owner_displayname: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExternalMountRow {
    pub mount_id: i64,
    pub storage_backend: String,
    pub auth_backend: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExternalOptionRow {
    pub mount_id: i64,
    pub key: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// The mount model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    Home,
    GroupFolder,
    Share,
    External,
    ExternalSession,
    Unknown,
}

impl MountKind {
    fn from_provider(class: &str) -> MountKind {
        match class {
            PROVIDER_HOME => MountKind::Home,
            PROVIDER_GROUPFOLDER => MountKind::GroupFolder,
            PROVIDER_SHARE => MountKind::Share,
            PROVIDER_EXTERNAL => MountKind::External,
            _ => MountKind::Unknown,
        }
    }

    /// `MountPoint::getMountType()`.
    pub fn mount_type(self) -> &'static str {
        match self {
            MountKind::Home => "",
            MountKind::GroupFolder => "group",
            MountKind::Share => "shared",
            MountKind::External => "external",
            MountKind::ExternalSession => "external-session",
            MountKind::Unknown => "",
        }
    }
}

/// A received share's "super-share": the grouped shares for one node, OR-ed the
/// way `MountProvider::buildSuperShares()` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareInfo {
    pub id: i64,
    pub share_type: i64,
    pub owner: String,
    pub owner_display_name: Option<String>,
    /// OR of the grouped shares' permissions.
    pub permissions: i64,
    /// `implode("\n", notes)` of the grouped shares (usually empty).
    pub note: String,
    pub hide_download: bool,
    /// The merged share attributes, serialised as JSON (`[]` when none).
    pub attributes: String,
    /// The grouped shares, for `oc:share-types` / `nc:sharees` on a Depth-0
    /// mount root.
    pub grouped: Vec<MountShareRow>,
}

/// One `oc_mounts` row for the user, resolved to its root `oc_filecache` row.
#[derive(Debug, Clone)]
pub struct MountInfo {
    /// Absolute mount point, e.g. `/alice/files/Documents Sélène/`.
    pub mount_point: String,
    /// Normalised path relative to `files/` (empty for the home mount).
    pub rel: String,
    pub kind: MountKind,
    pub mount_id: Option<i64>,
    pub storage_id: i64,
    pub storage_string: String,
    pub root_id: i64,
    pub root_path: String,
    pub root_etag: String,
    pub root_mtime: i64,
    pub root_size: i64,
    pub root_encrypted: i64,
    pub root_unencrypted_size: Option<i64>,
    pub root_mimetype: String,
    /// `oc_filecache.permissions` of the root row.
    pub raw_permissions: i64,
    /// The root permissions after the provider's storage mask.
    pub masked_permissions: i64,
    /// `true` for `IMovableMount` (received shares): the listing injects
    /// `|UPDATE|DELETE` on the mount root.
    pub movable: bool,
    /// The mount's `readonly` option (external mounts); `getSharePermissions`
    /// skips the `|UPDATE|DELETE` branch for these.
    pub readonly: bool,
    pub share: Option<Arc<ShareInfo>>,
    pub owner: Option<String>,
    /// Groupfolder quota (`oc_group_folders.quota`); `-3` = unlimited.
    pub groupfolder_quota: Option<i64>,
    /// The provider's permission mask applied to every row (share permissions,
    /// group permissions, external readonly/sharing).
    pub mask: i64,
    /// The groupfolder ACL model, when the folder has one.
    pub acl: Option<Arc<GroupFolderAcl>>,
    /// `oc_group_folders` group permissions (the folder's base mask).
    pub folder_perms: i64,
    /// `false` when the mount's root `oc_filecache` row could not be resolved:
    /// the mount cannot even be described as an entry in a containing listing.
    pub describable: bool,
    /// `false` when the model cannot reproduce the mount's **contents** (a
    /// non-local external backend, `filesystem_check_changes`, a circle ACL, a
    /// share type other than 0/1/2, …). A request at/under such a mount
    /// delegates, but a listing that merely *contains* it is still served and
    /// describes the entry from `oc_mounts` plus the root row.
    pub servable: bool,
}

impl MountInfo {
    /// The name the mount reaches the wire with: the last path component of the
    /// mount point (`View::getDirectoryContent` overwrites the row name).
    pub fn wire_name(&self) -> String {
        self.mount_point
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// `FileInfo::getSize()`'s encryption substitution on the root row.
    pub fn effective_size(&self) -> i64 {
        if self.root_encrypted != 0 {
            if let Some(size) = self.root_unencrypted_size {
                return size;
            }
        }
        self.root_size
    }

    pub fn is_dir(&self) -> bool {
        self.root_mimetype == "httpd/unix-directory"
    }
}

/// The result of matching a DAV path against the user's mounts.
#[derive(Debug, Clone, Default)]
pub struct MountResolution {
    /// The longest mount whose path is a prefix of (or equal to) the requested
    /// path. `None` means the path lives on the home storage.
    pub mount: Option<Arc<MountInfo>>,
    /// Every mount strictly below the requested path, ascending by mount point
    /// (direct children and nested). The requested node's synthetic
    /// etag/size/mtime are computed from these.
    pub below: Vec<Arc<MountInfo>>,
    /// The direct mount children of a listing at the requested path (relative
    /// path with no further `/`).
    pub direct: Vec<Arc<MountInfo>>,
    /// A mount at/under the path is not contents-servable, or a mount below the
    /// listing is not even describable: the request must delegate.
    pub delegate: bool,
}

// ---------------------------------------------------------------------------
// The per-user cache
// ---------------------------------------------------------------------------

struct CacheEntry {
    loaded_at: Instant,
    mounts: Arc<Vec<Arc<MountInfo>>>,
    failed: bool,
}

/// A short-TTL, per-user mount map. Loaded lazily on first use (and after the
/// TTL), never queried per request when warm. A failed/stale load returns
/// `None`, which the caller turns into a `501` (delegate), never a `404`.
#[derive(Default)]
pub struct MountCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
    ttl: Duration,
}

impl MountCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// The user's mounts, or `None` when the map could not be loaded.
    pub async fn get(&self, db: &Db, uid: &str) -> Option<Arc<Vec<Arc<MountInfo>>>> {
        {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(uid) {
                if !entry.failed && entry.loaded_at.elapsed() < self.ttl {
                    return Some(entry.mounts.clone());
                }
            }
        }
        // Reload under the write lock. Re-check first so a concurrent reload
        // that just completed is reused.
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get(uid) {
            if !entry.failed && entry.loaded_at.elapsed() < self.ttl {
                return Some(entry.mounts.clone());
            }
        }
        match load_user_mounts(db, uid).await {
            Ok(mounts) => {
                let mounts = Arc::new(mounts);
                entries.insert(
                    uid.to_string(),
                    CacheEntry {
                        loaded_at: Instant::now(),
                        mounts: mounts.clone(),
                        failed: false,
                    },
                );
                Some(mounts)
            }
            Err(error) => {
                log::error!("failed to load the mount map for {uid}: {error}");
                entries.insert(
                    uid.to_string(),
                    CacheEntry {
                        loaded_at: Instant::now(),
                        mounts: Arc::new(Vec::new()),
                        failed: true,
                    },
                );
                None
            }
        }
    }

    /// Drop the cached map for a user (mount-change hook / tests).
    pub async fn invalidate(&self, uid: &str) {
        self.entries.write().await.remove(uid);
    }
}

// ---------------------------------------------------------------------------
// Loading and mask computation
// ---------------------------------------------------------------------------

/// Loads and masks every `oc_mounts` row for `uid`.
pub async fn load_user_mounts(db: &Db, uid: &str) -> Result<Vec<Arc<MountInfo>>> {
    let bases = db.mount_base_rows(uid).await?;
    if bases.is_empty() {
        return Ok(Vec::new());
    }

    let has_groupfolder = bases.iter().any(|base| {
        MountKind::from_provider(&base.provider_class) == MountKind::GroupFolder
    });
    let has_external = bases.iter().any(|base| {
        matches!(
            MountKind::from_provider(&base.provider_class),
            MountKind::External | MountKind::ExternalSession
        )
    });

    // Groupfolders: per-user permission, ACL presence, circles. The app tables
    // are optional; when they are missing the affected mounts are unservable
    // (delegated), never served with a guessed mask.
    let group_rows = if has_groupfolder && db.table_exists("group_folders_groups").await? {
        db.group_folder_group_rows().await?
    } else {
        Vec::new()
    };
    let group_meta = if has_groupfolder && db.table_exists("group_folders").await? {
        db.group_folder_meta_rows().await?
    } else {
        Vec::new()
    };
    let acl_rows = if has_groupfolder && db.table_exists("group_folders_acl").await? {
        db.group_folder_acl_rows().await?
    } else {
        Vec::new()
    };
    let manage_rows = if has_groupfolder && db.table_exists("group_folders_manage").await? {
        db.group_folder_manage_rows().await?
    } else {
        Vec::new()
    };
    let authorized_groups = if has_groupfolder && db.table_exists("authorized_groups").await? {
        db.group_folder_authorized_groups().await?
    } else {
        Vec::new()
    };
    let user_groups = db.user_group_ids(uid).await?;
    let user_groups: std::collections::HashSet<String> = user_groups.into_iter().collect();
    // `ACLManagerFactory`: a non-default, global merge mode. When it is enabled
    // the rule merge differs, so ACL groupfolders delegate; folders without an
    // ACL are unaffected.
    let inherit_per_user = db
        .appconfig_value("groupfolders", "acl-inherit-per-user")
        .await?
        .map(|value| value == "true")
        .unwrap_or(false);

    // Received shares: grouped into super-shares by `file_source`.
    let share_rows = db.mount_share_rows(uid).await?;
    let mut shares_by_source: HashMap<i64, Vec<MountShareRow>> = HashMap::new();
    for row in share_rows {
        shares_by_source.entry(row.file_source).or_default().push(row);
    }

    // External storage options (files_external is optional).
    let external_mounts = if has_external && db.table_exists("external_mounts").await? {
        db.external_mount_rows().await?
    } else {
        Vec::new()
    };
    let mut external_by_id: HashMap<i64, ExternalMountRow> = HashMap::new();
    for row in external_mounts {
        external_by_id.insert(row.mount_id, row);
    }
    let mut options_by_mount: HashMap<i64, HashMap<String, String>> = HashMap::new();
    if has_external && db.table_exists("external_options").await? {
        for row in db.external_option_rows().await? {
            options_by_mount
                .entry(row.mount_id)
                .or_default()
                .insert(row.key, row.value);
        }
    }

    let group_meta_by_id: HashMap<i64, GroupFolderMetaRow> =
        group_meta.into_iter().map(|row| (row.folder_id, row)).collect();
    let mut acl_rows_by_storage: HashMap<i64, Vec<GroupFolderAclRow>> = HashMap::new();
    for row in acl_rows {
        acl_rows_by_storage
            .entry(row.storage_id)
            .or_default()
            .push(row);
    }

    let mut mounts = Vec::with_capacity(bases.len());
    for base in bases {
        let kind = MountKind::from_provider(&base.provider_class);
        if kind == MountKind::Home {
            // The home mount is not a child under `files/`; the existing home
            // path handles it.
            continue;
        }
        let rel = mount_rel(&base.mount_point, uid);
        let root_path = base.root_path.clone().unwrap_or_default();
        let root_exists = base.root_path.is_some();
        let raw_permissions = base.root_permissions.unwrap_or(0);
        let root_encrypted = base.root_encrypted.unwrap_or(0);

        let mut mount = MountInfo {
            mount_point: base.mount_point.clone(),
            rel: rel.clone(),
            kind,
            mount_id: base.mount_id,
            storage_id: base.storage_id,
            storage_string: base.storage_string.clone(),
            root_id: base.root_id,
            root_path,
            root_etag: base.root_etag.clone().unwrap_or_default(),
            root_mtime: base.root_mtime.unwrap_or(0),
            root_size: base.root_size.unwrap_or(0),
            root_encrypted,
            root_unencrypted_size: base.root_unencrypted_size,
            root_mimetype: base.root_mimetype.clone().unwrap_or_default(),
            raw_permissions,
            masked_permissions: raw_permissions,
            movable: false,
            readonly: false,
            share: None,
            owner: Some(uid.to_string()),
            groupfolder_quota: None,
            mask: 0,
            acl: None,
            folder_perms: 0,
            describable: root_exists,
            servable: root_exists,
        };

        match kind {
            MountKind::GroupFolder => {
                apply_groupfolder_mask(
                    &mut mount,
                    uid,
                    &group_meta_by_id,
                    &group_rows,
                    &user_groups,
                    &acl_rows_by_storage,
                    &manage_rows,
                    &authorized_groups,
                    inherit_per_user,
                );
            }
            MountKind::Share => {
                apply_share_mask(&mut mount, shares_by_source.get(&base.root_id));
            }
            MountKind::External | MountKind::ExternalSession => {
                apply_external_mask(&mut mount, &external_by_id, &options_by_mount);
            }
            MountKind::Home => {}
            MountKind::Unknown => {
                mount.servable = false;
            }
        }

        log::debug!(
            "mount {} kind={:?} describable={} servable={} mount_id={:?} storage={} mask={}",
            mount.mount_point,
            mount.kind,
            mount.describable,
            mount.servable,
            mount.mount_id,
            mount.storage_id,
            mount.mask
        );
        mounts.push(Arc::new(mount));
    }

    mounts.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    Ok(mounts)
}

/// The normalised path relative to `files/`, or the raw mount point when it is
/// not under `files/`.
fn mount_rel(mount_point: &str, uid: &str) -> String {
    let prefix = format!("/{uid}/files/");
    match mount_point.strip_prefix(&prefix) {
        Some(rest) => normalize_rel(rest),
        None => {
            // Home mount or a malformed row: keep it out of path matching.
            String::new()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_groupfolder_mask(
    mount: &mut MountInfo,
    uid: &str,
    meta_by_id: &HashMap<i64, GroupFolderMetaRow>,
    group_rows: &[GroupFolderGroupRow],
    user_groups: &std::collections::HashSet<String>,
    acl_rows_by_storage: &HashMap<i64, Vec<GroupFolderAclRow>>,
    manage_rows: &[GroupFolderManageRow],
    authorized_groups: &[String],
    inherit_per_user: bool,
) {
    // Find the groupfolder row by storage/root (the `folder_id` is not on
    // `oc_mounts`).
    let Some((folder_id, meta)) = meta_by_id
        .iter()
        .find(|(_, meta)| meta.storage_id == mount.storage_id)
    else {
        // No metadata row: the group mask is unknown, so the contents cannot be
        // reproduced. The entry is still describable if its root row exists.
        mount.servable = false;
        return;
    };
    let folder_id = *folder_id;

    let mut perms: Option<i64> = None;
    let mut has_circle = false;
    for row in group_rows.iter().filter(|row| row.folder_id == folder_id) {
        if row.circle_id.as_deref().is_some_and(|id| !id.is_empty()) {
            has_circle = true;
            continue;
        }
        if let Some(group) = &row.group_id {
            if user_groups.contains(group) {
                perms = Some(perms.unwrap_or(0) | row.permissions);
            }
        }
    }
    // Apply the group permission mask we *can* resolve before any early return,
    // so a listing that merely contains this mount can describe its entry.
    if let Some(perms) = perms {
        mount.folder_perms = perms;
        mount.mask = perms;
        mount.groupfolder_quota = Some(meta.quota);
        mount.masked_permissions = mount.raw_permissions & perms;
    }
    if has_circle {
        // Circle membership is not visible in the database group tables.
        mount.servable = false;
        return;
    }
    let Some(perms) = perms else {
        // No matching group row: the user's membership is not visible.
        mount.servable = false;
        return;
    };

    // The ACL engine (`ACLManager`) only applies when the folder has ACLs or
    // `acl_default_no_permission`. Without either, the `PermissionsMask` alone
    // is the effective permission.
    if meta.acl == 0 && !meta.acl_default_no_permission {
        mount.masked_permissions = mount.raw_permissions & perms;
        return;
    }
    if inherit_per_user {
        // The alternative `acl-inherit-per-user` merge is not ported; delegate
        // only the ACL folders, not the whole app. The group mask above still
        // describes the entry.
        mount.servable = false;
        return;
    }

    // Build the per-user rule set. A rule is kept only when its mapping is one
    // of the caller's user/group mappings (`UserMappingManager::getMappingsForUser`).
    let user_mappings: std::collections::HashSet<(String, String)> = std::iter::once((
        "user".to_string(),
        uid.to_string(),
    ))
    .chain(
        user_groups
            .iter()
            .map(|group| ("group".to_string(), group.clone())),
    )
    .collect();

    let mut has_circle_rule = false;
    let mut rules: Vec<AclRule> = Vec::new();
    if let Some(rows) = acl_rows_by_storage.get(&mount.storage_id) {
        for row in rows {
            if row.mapping_type == "circle" {
                has_circle_rule = true;
                continue;
            }
            if user_mappings.contains(&(row.mapping_type.clone(), row.mapping_id.clone())) {
                rules.push(AclRule {
                    path: row.path.clone(),
                    mapping_type: row.mapping_type.clone(),
                    mapping_id: row.mapping_id.clone(),
                    mask: row.mask,
                    permissions: row.permissions & row.mask,
                });
            }
        }
    }
    if has_circle_rule {
        // A circle rule exists and circle membership is not in the core schema:
        // delegate rather than apply a wrong mask.
        mount.servable = false;
        return;
    }

    let can_manage = can_manage_acl(
        folder_id,
        uid,
        user_groups,
        manage_rows,
        authorized_groups,
    );
    let base_permission = if meta.acl_default_no_permission {
        if can_manage {
            PERMISSION_READ
        } else {
            0
        }
    } else {
        PERMISSION_ALL
    };

    let acl = GroupFolderAcl {
        folder_id,
        storage_id: mount.storage_id,
        base_permission,
        rules,
        has_circle_rule,
    };
    let root_acl = acl.permissions_for_path(&mount.root_path);
    mount.masked_permissions = mount.raw_permissions & perms & root_acl;
    mount.acl = Some(Arc::new(acl));
}

/// `FolderManager::canManageACL()`: an admin, a `oc_group_folders_manage`
/// mapping, or a group authorized in `oc_authorized_groups` for the app's
/// admin settings.
fn can_manage_acl(
    folder_id: i64,
    uid: &str,
    user_groups: &std::collections::HashSet<String>,
    manage_rows: &[GroupFolderManageRow],
    authorized_groups: &[String],
) -> bool {
    if user_groups.contains("admin") {
        return true;
    }
    if authorized_groups
        .iter()
        .any(|group| user_groups.contains(group))
    {
        return true;
    }
    manage_rows.iter().any(|row| {
        row.folder_id == folder_id
            && match row.mapping_type.as_str() {
                "user" => row.mapping_id == uid,
                "group" => user_groups.contains(&row.mapping_id),
                _ => false,
            }
    })
}

fn apply_share_mask(mount: &mut MountInfo, rows: Option<&Vec<MountShareRow>>) {
    let Some(rows) = rows else {
        mount.servable = false;
        return;
    };
    // Only user/group/usergroup shares can be resolved from `oc_share` alone.
    if rows
        .iter()
        .any(|row| !matches!(row.share_type, 0 | 1 | 2))
    {
        mount.servable = false;
        return;
    }
    let user = mount.owner.clone().unwrap_or_default();
    let mut relevant: Vec<MountShareRow> = rows
        .iter()
        .filter(|row| {
            row.permissions > 0
                && row.uid_owner != user
                && (match row.share_type {
                    0 | 2 => row.share_with.as_deref() == Some(user.as_str()),
                    1 => true, // group membership filtered by the caller
                    _ => false,
                })
                && (row.share_type == 1 || row.accepted == 1)
        })
        .cloned()
        .collect();
    // Group shares: the caller resolved the user's groups before loading, so a
    // type-1 row is only relevant when its `share_with` is one of them. That
    // resolution happens in `Db::mount_share_rows` (filtered by `user_id`).
    if relevant.is_empty() {
        mount.servable = false;
        return;
    }
    relevant.sort_by(|a, b| (a.stime, a.id).cmp(&(b.stime, b.id)));
    let super_share = &relevant[0];

    let mut permissions = 0;
    let mut notes: Vec<String> = Vec::new();
    let mut attributes = String::from("[]");
    for row in &relevant {
        permissions |= row.permissions;
        if let Some(note) = &row.note {
            notes.push(note.clone());
        }
        if let Some(raw) = &row.attributes {
            attributes = merge_attributes(&attributes, raw);
        }
    }

    mount.mask = permissions;
    mount.masked_permissions = mount.raw_permissions & permissions;
    mount.movable = true;
    mount.owner = Some(super_share.uid_owner.clone());
    mount.share = Some(Arc::new(ShareInfo {
        id: super_share.id,
        share_type: super_share.share_type,
        owner: super_share.uid_owner.clone(),
        owner_display_name: super_share.owner_displayname.clone(),
        permissions,
        note: notes.join("\n"),
        // `buildSuperShares()` never copies `hideDownload` onto the super
        // share, so it stays at the default `false`.
        hide_download: false,
        attributes,
        grouped: relevant,
    }));
}

fn apply_external_mask(
    mount: &mut MountInfo,
    external_by_id: &HashMap<i64, ExternalMountRow>,
    options_by_mount: &HashMap<i64, HashMap<String, String>>,
) {
    let Some(mount_id) = mount.mount_id else {
        log::debug!("external mount {} has no mount_id", mount.mount_point);
        mount.servable = false;
        return;
    };
    let Some(external) = external_by_id.get(&mount_id) else {
        log::debug!(
            "external mount {} mount_id={mount_id} not in {} rows",
            mount.mount_point,
            external_by_id.len()
        );
        mount.servable = false;
        return;
    };
    // `oc_external_options.value` is JSON-encoded (`"0"`, `"1"`, `""`); PHP
    // decodes it in `DBConfigService`.
    let options = options_by_mount.get(&mount_id);
    let readonly = option_is_true(options, "readonly");
    let enable_sharing = !options
        .and_then(|options| options.get("enable_sharing"))
        .is_some_and(|value| !option_value_true(&decode_option(value)));
    // `SetupManager`: readonly -> `ALL & ~(UPDATE|CREATE|DELETE)` (17),
    // sharing disabled -> `ALL - SHARE` (15). The mask is applied before the
    // contents-servability check so a listing that merely contains this mount
    // describes the entry with the right permissions.
    let mut mask = if readonly { 17 } else { PERMISSION_ALL };
    if !enable_sharing {
        mask &= 15;
    }
    mount.mask = mask;
    mount.masked_permissions = mount.raw_permissions & mask;
    mount.readonly = readonly;

    // Only `local` backends have a usable `oc_filecache`; everything else needs
    // the remote. `filesystem_check_changes != 0` (CHECK_ALWAYS) means PHP would
    // stat and re-scan before answering, so the cached row may be stale.
    if external.storage_backend != "local" {
        log::debug!(
            "external mount {} backend={:?}",
            mount.mount_point,
            external.storage_backend
        );
        mount.servable = false;
        return;
    }
    let check_changes = options
        .and_then(|options| options.get("filesystem_check_changes"))
        .map(|value| {
            let decoded = decode_option(value);
            decoded.trim() != "0" && !decoded.is_empty()
        })
        .unwrap_or(false);
    if check_changes {
        log::debug!("external mount {} has filesystem_check_changes", mount.mount_point);
        mount.servable = false;
    }
}

fn option_is_true(options: Option<&HashMap<String, String>>, key: &str) -> bool {
    options
        .and_then(|options| options.get(key))
        .is_some_and(|value| option_value_true(&decode_option(value)))
}

/// `DBConfigService` JSON-decodes an external option value before PHP reads it.
fn decode_option(value: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(value) {
        Ok(serde_json::Value::String(text)) => text,
        Ok(serde_json::Value::Bool(flag)) => flag.to_string(),
        Ok(serde_json::Value::Number(number)) => number.to_string(),
        Ok(serde_json::Value::Null) => String::new(),
        _ => value.to_string(),
    }
}

fn option_value_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `MountProvider::mergeAttributes()`: a later value overwrites an existing
/// non-`true` one, and `true` sticks.
fn merge_attributes(existing: &str, incoming: &str) -> String {
    use serde_json::Value;
    let mut merged: Vec<(String, String, Value)> = Vec::new();
    for raw in [existing, incoming] {
        let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        for item in items {
            let scope = item
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let key = item
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let value = item.get("value").cloned().unwrap_or(Value::Bool(false));
            if let Some(entry) = merged
                .iter_mut()
                .find(|(s, k, _)| *s == scope && *k == key)
            {
                if entry.2 == Value::Bool(true) {
                    continue;
                }
                entry.2 = value;
            } else {
                merged.push((scope, key, value));
            }
        }
    }
    // Preserve PHP's `['scope' => …, 'key' => …, 'value' => …]` key order.
    let items: Vec<String> = merged
        .into_iter()
        .map(|(scope, key, value)| {
            format!(
                "{{\"scope\":{},\"key\":{},\"value\":{}}}",
                serde_json::to_string(&scope).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&key).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Matches `rel_norm` (the normalised DAV path relative to `files/`) against
/// the user's mount map.
pub fn resolve(mounts: &[Arc<MountInfo>], rel_norm: &str) -> MountResolution {
    let mut resolution = MountResolution::default();
    let prefix = if rel_norm.is_empty() {
        String::new()
    } else {
        format!("{rel_norm}/")
    };

    // Delegation is driven by the *requested* path, never by a mount that merely
    // appears in the listing:
    //  - a mount at/under the path whose **contents** the model cannot reproduce
    //    (`!servable`) needs that mount's storage, so delegate;
    //  - a mount that is not even describable (its root `oc_filecache` row is
    //    gone) cannot be merged into a listing, so delegate.
    // A mount that is merely *below* the path and is describable does **not**
    // suppress the containing listing: its entry is built from `oc_mounts` plus
    // the root row.
    let mut delegate = false;
    for mount in mounts {
        if mount.rel.is_empty() {
            continue;
        }
        let at_or_under = mount.rel == rel_norm
            || rel_norm.starts_with(&format!("{}/", mount.rel));
        let below = if rel_norm.is_empty() {
            true
        } else {
            mount.rel.starts_with(&prefix)
        };
        if (at_or_under || below) && !mount.describable {
            delegate = true;
        } else if at_or_under && !mount.servable {
            delegate = true;
        }
    }

    // Longest servable, visible mount that is a prefix of the requested path. A
    // mount whose root has no READ permission is hidden by PHP
    // (`View::getDirectoryContent` skips it), so it is ignored entirely.
    let mut best: Option<&Arc<MountInfo>> = None;
    for mount in mounts {
        if mount.rel.is_empty()
            || !mount.servable
            || mount.masked_permissions & PERMISSION_READ == 0
        {
            continue;
        }
        if mount.rel == rel_norm || rel_norm.starts_with(&format!("{}/", mount.rel)) {
            if best.map(|b| mount.rel.len() > b.rel.len()).unwrap_or(true) {
                best = Some(mount);
            }
        }
    }
    resolution.mount = best.cloned();

    // The submount aggregate and the direct mount entries use every mount that
    // is *describable*, whether or not its contents are servable.
    for mount in mounts {
        if mount.rel.is_empty() || !mount.describable {
            continue;
        }
        // Strictly below the requested path.
        let below = if rel_norm.is_empty() {
            true
        } else {
            mount.rel.starts_with(&prefix)
        };
        if below {
            let remainder = if rel_norm.is_empty() {
                mount.rel.as_str()
            } else {
                &mount.rel[prefix.len()..]
            };
            if !remainder.contains('/') {
                resolution.direct.push(mount.clone());
            }
            resolution.below.push(mount.clone());
        }
    }
    resolution.below.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    resolution.direct.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));

    resolution.delegate = delegate;
    resolution
}

/// The internal path of `rel_norm` inside `mount`, relative to the storage
/// root (`root_path` + the path below the mount point).
pub fn internal_path_in_mount(mount: &MountInfo, rel_norm: &str) -> String {
    let remainder = if mount.rel.is_empty() {
        rel_norm.to_string()
    } else if rel_norm == mount.rel {
        String::new()
    } else {
        rel_norm
            .strip_prefix(&format!("{}/", mount.rel))
            .unwrap_or_default()
            .to_string()
    };
    if mount.root_path.is_empty() {
        remainder
    } else if remainder.is_empty() {
        mount.root_path.clone()
    } else {
        format!("{}/{}", mount.root_path, remainder)
    }
}

/// Re-reads a mount's root `oc_filecache` row and recomputes its effective
/// permissions. The mount map caches the *structure* (mount points, providers,
/// masks, ACL rules) but the root etag/size/mtime and raw permissions are
/// mutable (the `Propagator` updates the root etag on every write below it), so
/// the synthetic parent etag must never come from the cached copy.
pub async fn refresh_mount(db: &Db, mount: &MountInfo) -> Result<Arc<MountInfo>> {
    let hash = md5_hex(mount.root_path.as_bytes());
    let Some(row) = db.resolve_storage_file(mount.storage_id, &hash).await? else {
        let mut stale = mount.clone();
        stale.describable = false;
        stale.servable = false;
        return Ok(Arc::new(stale));
    };
    let mut fresh = mount.clone();
    fresh.describable = true;
    fresh.root_etag = row.etag.clone();
    fresh.root_mtime = row.mtime;
    fresh.root_size = row.size;
    fresh.root_encrypted = row.encrypted;
    fresh.root_unencrypted_size = row.unencrypted_size;
    fresh.root_mimetype = row.mimetype.clone();
    fresh.raw_permissions = row.permissions;
    let mut masked = row.permissions & fresh.mask;
    if let Some(acl) = &fresh.acl {
        masked &= acl.permissions_for_path(&fresh.root_path);
    }
    fresh.masked_permissions = masked;
    Ok(Arc::new(fresh))
}

/// Refreshes every mount the request touches (the requested mount and the
/// submounts that feed the synthetic parent etag / the listing entries).
pub async fn refresh_resolution(
    db: &Db,
    mut resolution: MountResolution,
) -> Result<MountResolution> {
    let mut cache: HashMap<i64, Arc<MountInfo>> = HashMap::new();
    if let Some(mount) = &resolution.mount {
        let fresh = refresh_one(db, mount, &mut cache).await?;
        // The requested node lives in this mount's storage; if its contents are
        // not servable, the request delegates.
        if !fresh.servable {
            resolution.delegate = true;
        }
        resolution.mount = Some(fresh);
    }
    for mount in resolution.below.iter_mut() {
        let fresh = refresh_one(db, mount, &mut cache).await?;
        // A submount that is no longer describable (its root row vanished)
        // cannot be merged into the listing.
        if !fresh.describable {
            resolution.delegate = true;
        }
        *mount = fresh;
    }
    for mount in resolution.direct.iter_mut() {
        let fresh = refresh_one(db, mount, &mut cache).await?;
        if !fresh.describable {
            resolution.delegate = true;
        }
        *mount = fresh;
    }
    Ok(resolution)
}

async fn refresh_one(
    db: &Db,
    mount: &Arc<MountInfo>,
    cache: &mut HashMap<i64, Arc<MountInfo>>,
) -> Result<Arc<MountInfo>> {
    if let Some(fresh) = cache.get(&mount.root_id) {
        return Ok(fresh.clone());
    }
    let fresh = refresh_mount(db, mount).await?;
    cache.insert(mount.root_id, fresh.clone());
    Ok(fresh)
}

/// Resolves `rel_norm` to a full `oc_filecache` row inside `mount`.
pub async fn resolve_mount_file(
    db: &Db,
    mount: &MountInfo,
    rel_norm: &str,
) -> Result<Option<FileCacheRow>> {
    let internal = internal_path_in_mount(mount, rel_norm);
    let hash = md5_hex(internal.as_bytes());
    db.resolve_storage_file(mount.storage_id, &hash).await
}

/// `FileInfo::addSubEntry()` for every mount below a node: the synthetic etag,
/// size and mtime the parent reports.
///
/// `parent_abs_path` is the absolute view path of the node **without** a
/// trailing slash (`/uid/files` for the home root, `/uid/files/X` otherwise).
/// `relPath = substr(mountPoint, len(parentAbsPath))` keeps both slashes, so
/// each child etag contains a double slash (`/X//<etag><perms>`), and the
/// permissions are appended as a **decimal** integer. The order is ascending by
/// mount point.
pub fn sub_mount_aggregate(
    below: &[Arc<MountInfo>],
    parent_etag: &str,
    parent_size: i64,
    parent_mtime: i64,
    parent_abs_path: &str,
    listing: bool,
) -> (String, i64, i64) {
    if below.is_empty() {
        return (parent_etag.to_string(), parent_size, parent_mtime);
    }
    let mut child_etags: Vec<String> = Vec::with_capacity(below.len());
    let mut size = parent_size;
    let mut mtime = parent_mtime;
    for mount in below {
        let rel_path = mount
            .mount_point
            .strip_prefix(parent_abs_path)
            .unwrap_or(&mount.mount_point);
        child_etags.push(format!(
            "{}/{}{}",
            rel_path,
            mount.root_etag,
            child_etag_permissions(mount, listing)
        ));
        size += mount.effective_size();
        mtime = mtime.max(mount.root_mtime);
    }
    let combined = format!("{}::{}", parent_etag, child_etags.join("::"));
    (md5_hex(combined.as_bytes()), size, mtime)
}

/// The permission value PHP appends to a child etag.
///
/// `FileInfo::addSubEntry()` reads the mount root cache entry. In a Depth-1
/// listing PHP's `View::getDirectoryContent()` has already mutated that entry
/// with the movable/non-movable branch (`|UPDATE|DELETE` / `& ~(UPDATE|DELETE)`)
/// before the parent etag is evaluated, so a Depth-1 parent uses the listing
/// permissions; Depth 0 uses the plain masked permissions. The local harness
/// parity diff is the proof (the recon's DB-derived example used the plain
/// value).
fn child_etag_permissions(mount: &MountInfo, listing: bool) -> i64 {
    if !listing {
        return mount.masked_permissions;
    }
    if mount.movable {
        mount.masked_permissions | PERMISSION_UPDATE | PERMISSION_DELETE
    } else {
        mount.masked_permissions & !(PERMISSION_UPDATE | PERMISSION_DELETE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(mount_point: &str, rel: &str, etag: &str, perms: i64, size: i64, mtime: i64) -> Arc<MountInfo> {
        Arc::new(MountInfo {
            mount_point: mount_point.to_string(),
            rel: rel.to_string(),
            kind: MountKind::GroupFolder,
            mount_id: None,
            storage_id: 1,
            storage_string: "local::/data/".to_string(),
            root_id: 1,
            root_path: String::new(),
            root_etag: etag.to_string(),
            root_mtime: mtime,
            root_size: size,
            root_encrypted: 0,
            root_unencrypted_size: None,
            root_mimetype: "httpd/unix-directory".to_string(),
            raw_permissions: perms,
            masked_permissions: perms,
            movable: false,
            readonly: false,
            share: None,
            owner: Some("alice".to_string()),
            groupfolder_quota: None,
            mask: perms,
            acl: None,
            folder_perms: perms,
            describable: true,
            servable: true,
        })
    }

    #[test]
    fn mount_rel_normalises() {
        assert_eq!(mount_rel("/alice/files/Partage familial/", "alice"), "Partage familial");
        assert_eq!(mount_rel("/alice/files/A/B/", "alice"), "A/B");
        assert_eq!(mount_rel("/alice/", "alice"), "");
    }

    #[test]
    fn resolves_longest_prefix_and_below() {
        let mounts = vec![
            mount("/alice/files/A/", "A", "eA", 31, 10, 1),
            mount("/alice/files/A/B/", "A/B", "eB", 31, 20, 2),
            mount("/alice/files/C/", "C", "eC", 31, 30, 3),
        ];
        // The home root contains all three; A and C are direct, A/B is nested.
        let r = resolve(&mounts, "");
        assert!(r.mount.is_none());
        assert_eq!(r.below.len(), 3);
        let direct: Vec<&str> = r.direct.iter().map(|m| m.rel.as_str()).collect();
        assert_eq!(direct, vec!["A", "C"]);

        // Inside A: the mount is A, and A/B is nested below.
        let r = resolve(&mounts, "A");
        assert_eq!(r.mount.as_ref().unwrap().rel, "A");
        assert_eq!(r.below.len(), 1);
        assert_eq!(r.below[0].rel, "A/B");

        // Inside A/B: longest prefix wins.
        let r = resolve(&mounts, "A/B");
        assert_eq!(r.mount.as_ref().unwrap().rel, "A/B");
        assert!(r.below.is_empty());

        // A path inside A/B resolves against A/B.
        let r = resolve(&mounts, "A/B/child");
        assert_eq!(r.mount.as_ref().unwrap().rel, "A/B");
        assert_eq!(internal_path_in_mount(&r.mount.clone().unwrap(), "A/B/child"), "child");
    }

    #[test]
    fn unservable_contents_do_not_suppress_the_listing() {
        // A mount whose *contents* cannot be reproduced is still describable:
        // a listing that merely contains it must be served, and its entry must
        // be merged into the aggregate/listing.
        let mut bad = mount("/alice/files/Bad/", "Bad", "e", 31, 1, 1);
        Arc::get_mut(&mut bad).unwrap().servable = false;
        let mounts = vec![mount("/alice/files/Ok/", "Ok", "e", 31, 1, 1), bad];
        let home = resolve(&mounts, "");
        assert!(!home.delegate, "a containing listing must be served");
        assert_eq!(home.direct.len(), 2, "the unservable mount is still an entry");
        assert_eq!(home.below.len(), 2);
        // Inside the unservable mount, delegate.
        assert!(resolve(&mounts, "Bad").delegate);
        assert!(resolve(&mounts, "Bad/sub").delegate);
        // A sibling of the bad mount is still fine.
        assert!(!resolve(&mounts, "Ok").delegate);
    }

    #[test]
    fn undescribable_mount_suppresses_the_listing() {
        // A mount whose root row is gone cannot be described at all: the
        // containing listing must delegate rather than silently drop it.
        let mut ghost = mount("/alice/files/Ghost/", "Ghost", "e", 31, 1, 1);
        {
            let inner = Arc::get_mut(&mut ghost).unwrap();
            inner.describable = false;
            inner.servable = false;
        }
        let mounts = vec![ghost];
        assert!(resolve(&mounts, "").delegate);
        assert!(resolve(&mounts, "Ghost").delegate);
    }

    #[test]
    fn internal_path_inside_mount() {
        let m = mount("/alice/files/Partage familial/", "Partage familial", "e", 31, 1, 1);
        // root_path is empty here; a groupfolder with `separate-storage` has one.
        assert_eq!(internal_path_in_mount(&m, "Partage familial"), "");
        assert_eq!(internal_path_in_mount(&m, "Partage familial/a/b"), "a/b");

        let mut jails = mount("/alice/files/Hytrix/", "Hytrix", "e", 31, 1, 1);
        Arc::get_mut(&mut jails).unwrap().root_path = "files".to_string();
        assert_eq!(internal_path_in_mount(&jails, "Hytrix"), "files");
        assert_eq!(internal_path_in_mount(&jails, "Hytrix/a"), "files/a");
    }

    /// The recon's DB-verified example (`docs/recon/files-mounts-model.md` §3c).
    #[test]
    fn parent_synthetic_etag_matches_the_recon() {
        // Ascending by mount_point, masked permissions as documented.
        let below = vec![
            mount("/aviallon/files/Administratif commun/", "Administratif commun", "6aa904c996633", 31, 8135461910, 0),
            mount("/aviallon/files/Dashcam (ceph)/", "Dashcam (ceph)", "69ba093c7d12a", 17, 1195662625280, 0),
            mount("/aviallon/files/Documents Sélène/", "Documents Sélène", "6aa2c083517bd", 31, 67468224845, 0),
            mount("/aviallon/files/Hytrix/", "Hytrix", "6a68d77ca7a1c", 31, 14612945, 0),
            mount("/aviallon/files/Mariage/", "Mariage", "69cef6e63b305", 31, 50779087, 0),
            mount("/aviallon/files/MyCloud Photos Backup/", "MyCloud Photos Backup", "66c9bb3195abf", 1, 502625530869, 0),
            mount("/aviallon/files/Partage familial/", "Partage familial", "69eef3c7d9cda", 31, 13207951013, 0),
        ];
        let (etag, size, _mtime) = sub_mount_aggregate(
            &below,
            "6aadc32bd443f",
            1011016452017,
            0,
            "/aviallon/files",
            false,
        );
        assert_eq!(etag, "bb151705b7603d35fb3b10b5c2f3cee2");
        assert_eq!(size, 2798181637966);
    }

    #[test]
    fn acl_relevant_paths_are_parent_first() {
        assert_eq!(
            relevant_paths("a/b/c"),
            vec!["a".to_string(), "a/b".to_string(), "a/b/c".to_string()]
        );
        assert_eq!(relevant_paths("x"), vec!["x".to_string()]);
        assert_eq!(relevant_paths(""), Vec::<String>::new());
    }

    #[test]
    fn acl_apply_permissions_matches_php() {
        // Allow read: base 31 stays 31.
        assert_eq!(apply_permissions(1, 1, 31), 31);
        // Deny read: 31 & ~1 = 30.
        assert_eq!(apply_permissions(1, 0, 31), 30);
        // Deny delete: 31 & ~8 = 23.
        assert_eq!(apply_permissions(8, 0, 31), 23);
        // Allow delete on top of a deny-all base 0.
        assert_eq!(apply_permissions(8, 8, 0), 8);
    }

    #[test]
    fn acl_permissions_for_path_parent_first_and_read_gate() {
        let acl = GroupFolderAcl {
            folder_id: 1,
            storage_id: 1,
            base_permission: 31,
            rules: vec![
                AclRule {
                    path: "a".into(),
                    mapping_type: "group".into(),
                    mapping_id: "team".into(),
                    mask: 8,
                    permissions: 0,
                },
                AclRule {
                    path: "a/b".into(),
                    mapping_type: "group".into(),
                    mapping_id: "team".into(),
                    mask: 1,
                    permissions: 0,
                },
            ],
            has_circle_rule: false,
        };
        // `a`: delete denied -> 23 (read still present).
        assert_eq!(acl.permissions_for_path("a"), 23);
        // `a/b`: delete denied then read denied -> no read -> 0.
        assert_eq!(acl.permissions_for_path("a/b"), 0);
        // A sibling with no rule keeps the base.
        assert_eq!(acl.permissions_for_path("c"), 31);
    }

    #[test]
    fn acl_default_no_permission_base() {
        let acl = GroupFolderAcl {
            folder_id: 1,
            storage_id: 1,
            base_permission: 0,
            rules: Vec::new(),
            has_circle_rule: false,
        };
        assert_eq!(acl.permissions_for_path("a"), 0);
        let acl = GroupFolderAcl {
            base_permission: 1,
            ..acl
        };
        assert_eq!(acl.permissions_for_path("a"), 1);
    }

    #[test]
    fn merge_attributes_true_sticks() {
        let a = r#"[{"scope":"permissions","key":"download","value":false}]"#;
        let b = r#"[{"scope":"permissions","key":"download","value":true}]"#;
        assert_eq!(
            merge_attributes(a, b),
            r#"[{"scope":"permissions","key":"download","value":true}]"#
        );
        assert_eq!(
            merge_attributes(b, a),
            r#"[{"scope":"permissions","key":"download","value":true}]"#
        );
        assert_eq!(merge_attributes("[]", "[]"), "[]");
    }
}
