// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native WebDAV **files** `PROPFIND` (Depth 0/1).
//!
//! Scope (v1), from `docs/recon/files-propfind-model.md` §9: only
//! `/remote.php/dav/files/<uid>/<path>` for the caller's **own home storage**
//! (`oc_storages.id = 'home::<uid>'`, internal path `files/<rel>`), when the
//! request is a `PROPFIND` with Depth 0 or 1, the caller authenticated through
//! the app-password **fast path**, the path is neither at/under a mount point
//! nor a collection with a mount below it, and every requested property is in
//! the implemented set below. Everything else answers **501** so nginx replays
//! the request to PHP.
//!
//! The property gate is the load-bearing safety rule: an explicit request for a
//! qname we do not implement is answered **501**, never 404, because a 404 would
//! claim a property PHP serves is absent.

use crate::auth::AuthMethod;
use crate::config::Config;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::model::{FileCacheRow, ShareRow};
use crate::util::{encode_path_segment, http_date, percent_decode};
use crate::xml::parse::PropList;
use crate::xml::write::{
    DavResponse, MultiStatus, PropQName, PropStat, PropValue, XmlElement, NS_DAV,
    NS_NEXTCLOUD_FILES, NS_OCS, NS_OWNCLOUD,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// `ITags::TAG_FAVORITE`.
pub const TAG_FAVORITE: &str = "_$!<Favorite>!$_";

/// `OCP\Constants`.
const PERMISSION_READ: i64 = 1;
const PERMISSION_UPDATE: i64 = 2;
const PERMISSION_CREATE: i64 = 4;
const PERMISSION_DELETE: i64 = 8;
const PERMISSION_SHARE: i64 = 16;

/// `OCP\Files\FileInfo::SPACE_UNLIMITED`.
pub const SPACE_UNLIMITED: i64 = -3;

/// `httpd/unix-directory`.
#[allow(dead_code)]
const DIRECTORY_MIMETYPE: &str = "httpd/unix-directory";

// ---------------------------------------------------------------------------
// Path parsing / normalisation
// ---------------------------------------------------------------------------

/// A parsed `/remote.php/dav/files/<uid>/<rel>` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesPath {
    /// `/remote.php/dav/files` (webroot included), used to build hrefs.
    pub href_prefix: String,
    /// The percent-decoded user id (the DAV principal name).
    pub uid: String,
    /// The decoded, *un-normalised* relative segments (Sabre's tree path).
    pub rel_segments: Vec<String>,
    /// The decoded, un-normalised relative path (`rel_segments.join("/")`).
    pub rel_raw: String,
    /// The re-encoded href without a trailing slash (Sabre's `encodePath`).
    pub href: String,
}

/// Parses a DAV files path. `None` when the path is not under
/// `/remote.php/dav/files/` or has no user segment.
pub fn parse_files_path(path: &str) -> Option<FilesPath> {
    let marker = "/remote.php/dav/files";
    let index = path.find(marker)?;
    let base = &path[..index];
    let href_prefix = format!("{base}{marker}");
    let raw_rest = path[index + marker.len()..].trim_start_matches('/');
    // `Server::calculateUri()` collapses `/+` *before* decoding.
    let collapsed = collapse_slashes(raw_rest);
    // `HTTP\decodePath()` decodes the whole path, so `%2f` becomes a separator.
    let decoded = percent_decode(&collapsed);
    let segments: Vec<String> = decoded
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect();
    let (uid, rel_segments) = segments.split_first()?;
    if uid.is_empty() {
        return None;
    }
    let rel_raw = rel_segments.join("/");
    let mut href = format!("{href_prefix}/{}", encode_path_segment(uid));
    for segment in rel_segments {
        href.push('/');
        href.push_str(&encode_path_segment(segment));
    }
    Some(FilesPath {
        href_prefix,
        uid: uid.clone(),
        rel_segments: rel_segments.to_vec(),
        rel_raw,
        href,
    })
}

/// `Filesystem::normalizePath()` + `OC_Util::normalizeUnicode()` on the relative
/// path, followed by `Cache::normalize()`'s `trim(..., '/')`.
pub fn normalize_rel(rel: &str) -> String {
    let nfc: String = rel.nfc().collect();
    if nfc.is_empty() {
        return String::new();
    }
    let mut path = format!("/{nfc}");
    loop {
        let before = path.clone();
        path = path.replace('\\', "/");
        while path.contains("/./") {
            path = path.replace("/./", "/");
        }
        path = collapse_slashes(&path);
        if path.ends_with("/.") {
            path.truncate(path.len() - 1);
        }
        if path == before {
            break;
        }
    }
    if path.len() > 1 {
        path = path.trim_end_matches('/').to_string();
    }
    path.trim_start_matches('/').to_string()
}

/// The internal path inside the home storage (`files/<normalized rel>`).
pub fn internal_path(rel_norm: &str) -> String {
    if rel_norm.is_empty() {
        "files".to_string()
    } else {
        format!("files/{rel_norm}")
    }
}

fn collapse_slashes(path: &str) -> String {
    let mut out = path.to_string();
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    out
}

/// Whether any `oc_mounts` row for this user makes the path unservable.
///
/// Two directions matter, and both are delegated:
/// - the path is **at or under** a mount (`View::find()` resolves to the mount's
///   storage, not the home storage's `oc_filecache`);
/// - a mount is **below** the path (`View::getDirectoryContent()` merges mounts
///   into the listing, and `View::getFileInfo()` adds their sizes to the folder
///   and synthesises its etag).
///
/// The home mount (`/<uid>/`) is not a row under `files/`, so it is ignored.
pub fn mounts_delegate(mount_points: &[String], uid: &str, rel_norm: &str) -> bool {
    let prefix = format!("/{uid}/files/");
    for mount_point in mount_points {
        let Some(rest) = mount_point.strip_prefix(&prefix) else {
            continue;
        };
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            continue;
        }
        let mount_rel = normalize_rel(rest);
        if rel_norm.is_empty()
            || rel_norm == mount_rel
            || rel_norm.starts_with(&format!("{mount_rel}/"))
            || mount_rel.starts_with(&format!("{rel_norm}/"))
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// The property gate
// ---------------------------------------------------------------------------

/// The implemented property set. An explicit request for anything outside it is
/// answered 501 (delegated), never 404.
pub fn is_implemented(ns: &str, local: &str) -> bool {
    matches!(
        (ns, local),
        (NS_DAV, "getetag")
            | (NS_DAV, "getlastmodified")
            | (NS_DAV, "resourcetype")
            | (NS_DAV, "getcontentlength")
            | (NS_DAV, "getcontenttype")
            | (NS_DAV, "displayname")
            | (NS_DAV, "quota-available-bytes")
            | (NS_DAV, "quota-used-bytes")
            | (NS_DAV, "creationdate")
            | (NS_OWNCLOUD, "size")
            | (NS_OWNCLOUD, "fileid")
            | (NS_OWNCLOUD, "id")
            | (NS_OWNCLOUD, "permissions")
            | (NS_OWNCLOUD, "owner-id")
            | (NS_OWNCLOUD, "owner-display-name")
            | (NS_OWNCLOUD, "favorite")
            | (NS_OWNCLOUD, "comments-unread")
            | (NS_OWNCLOUD, "checksums")
            | (NS_OWNCLOUD, "share-types")
            | (NS_OWNCLOUD, "downloadURL")
            | (NS_OWNCLOUD, "dDC")
            | (NS_OWNCLOUD, "data-fingerprint")
            | (NS_NEXTCLOUD_FILES, "has-preview")
            | (NS_NEXTCLOUD_FILES, "mount-type")
            | (NS_NEXTCLOUD_FILES, "is-mount-root")
            | (NS_NEXTCLOUD_FILES, "is-encrypted")
            | (NS_NEXTCLOUD_FILES, "hidden")
            | (NS_NEXTCLOUD_FILES, "note")
            | (NS_NEXTCLOUD_FILES, "hide-download")
            | (NS_NEXTCLOUD_FILES, "share-attributes")
            | (NS_NEXTCLOUD_FILES, "sharees")
            | (NS_OCS, "share-permissions")
    ) || (ns == NS_NEXTCLOUD_FILES && local.starts_with("metadata-"))
}

/// Sabre's `allprop` fixed list (`Sabre\DAV\PropFind::ALLPROPS`). `propname` is
/// treated as `allprop`, exactly like Sabre's request parser.
pub fn allprop_list() -> Vec<PropQName> {
    vec![
        PropQName::dav("getlastmodified"),
        PropQName::dav("getcontentlength"),
        PropQName::dav("resourcetype"),
        PropQName::dav("quota-used-bytes"),
        PropQName::dav("quota-available-bytes"),
        PropQName::dav("getetag"),
        PropQName::dav("getcontenttype"),
    ]
}

/// Which per-collection bulk queries the request needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    pub favorite: bool,
    pub unread: bool,
    pub quota: bool,
    pub owner_display_name: bool,
    pub shares: bool,
}

impl Needs {
    pub fn for_props(props: &PropList) -> Needs {
        let requested: Vec<PropQName> = match props {
            PropList::AllProp | PropList::PropName => allprop_list(),
            PropList::Props(props) => props.clone(),
        };
        let has = |ns: &str, local: &str| {
            requested
                .iter()
                .any(|q| q.ns == ns && q.local == local)
        };
        Needs {
            favorite: has(NS_OWNCLOUD, "favorite"),
            unread: has(NS_OWNCLOUD, "comments-unread"),
            quota: has(NS_DAV, "quota-available-bytes") || has(NS_DAV, "quota-used-bytes"),
            owner_display_name: has(NS_OWNCLOUD, "owner-display-name"),
            shares: has(NS_OWNCLOUD, "share-types") || has(NS_NEXTCLOUD_FILES, "sharees"),
        }
    }
}

// ---------------------------------------------------------------------------
// The node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FilesNode {
    pub fileid: i64,
    pub name: String,
    pub displayname: String,
    pub size: i64,
    pub mtime: i64,
    pub etag: String,
    pub permissions: i64,
    pub is_dir: bool,
    pub mimetype: String,
    pub checksum: Option<String>,
    pub href: String,
    pub parent_permissions: i64,
    /// `IHomeStorage && internalPath === 'files'` (the `files` row itself).
    pub is_home_root: bool,
    /// `oc_filecache_extended.creation_time` (0 when absent).
    pub creation_time: i64,
    /// `oc_files_metadata.json` reduced to `key -> value`.
    pub metadata: HashMap<String, serde_json::Value>,
}

impl FilesNode {
    pub fn from_row(
        row: &FileCacheRow,
        href: String,
        displayname: String,
        parent_permissions: i64,
        is_home_root: bool,
    ) -> Self {
        FilesNode {
            fileid: row.fileid,
            name: row.display_name(),
            displayname,
            size: row.effective_size(),
            mtime: row.mtime,
            etag: row.etag.clone(),
            permissions: row.permissions,
            is_dir: row.is_directory(),
            mimetype: row.mimetype.clone(),
            checksum: row.checksum.clone(),
            href,
            parent_permissions,
            is_home_root,
            creation_time: row.creation_time,
            metadata: row.metadata.clone(),
        }
    }

    /// `DavUtil::getDavPermissions()`. `S`/`M` are always absent: a servable
    /// node lives on the caller's home mount (never `ISharedMountPoint`, never a
    /// non-home mount), and mount roots are delegated.
    pub fn dav_permissions(&self) -> String {
        let p = self.permissions;
        let mut out = String::new();
        if p & PERMISSION_SHARE != 0 {
            out.push('R');
        }
        if p & PERMISSION_READ != 0 {
            out.push('G');
        }
        if p & PERMISSION_DELETE != 0 {
            out.push('D');
        }
        if self.can_rename() {
            out.push('N');
        }
        if p & PERMISSION_UPDATE != 0 {
            out.push('V');
        }
        if self.is_dir {
            if p & PERMISSION_CREATE != 0 {
                out.push_str("CK");
            }
        } else if p & PERMISSION_UPDATE != 0 {
            // `$isWritable`: internalPath is never '' for a servable node.
            out.push('W');
        }
        out
    }

    /// `DavUtil::canRename()`.
    fn can_rename(&self) -> bool {
        if self.permissions & PERMISSION_UPDATE != 0 {
            return true;
        }
        // The user's home (`files`) cannot be renamed when not updateable.
        if self.is_home_root {
            return false;
        }
        self.permissions & PERMISSION_DELETE != 0
            && self.parent_permissions & PERMISSION_CREATE != 0
    }
}

// ---------------------------------------------------------------------------
// Property resolution
// ---------------------------------------------------------------------------

pub struct FilesContext {
    pub uid: String,
    pub instance_id: String,
    pub owner_display_name: String,
    pub quota_available: i64,
    pub previews_enabled: bool,
    pub data_fingerprint: String,
    pub favorites: HashSet<i64>,
    pub unread: HashMap<i64, i64>,
    /// Shares keyed by `file_source`, from the per-collection bulk queries.
    pub shares: HashMap<i64, Vec<ShareRow>>,
}

/// Builds one `<d:response>`.
///
/// `allprop` (and `propname`) strip the 404 propstat, matching
/// `PropFind::getResultForMultiStatus()`. `Prefer: return=minimal` strips it for
/// explicit requests too (`Server::generateMultiStatus($props, true)`).
pub fn build_response(
    node: &FilesNode,
    ctx: &FilesContext,
    props: &PropList,
    minimal: bool,
) -> DavResponse {
    let (requested, allprop) = match props {
        PropList::AllProp | PropList::PropName => (allprop_list(), true),
        PropList::Props(props) => (props.clone(), false),
    };

    let mut found: Vec<(PropQName, PropValue)> = Vec::new();
    let mut missing: Vec<PropQName> = Vec::new();
    for qname in &requested {
        match resolve_property(qname, node, ctx) {
            Some(value) => found.push((qname.clone(), value)),
            None => missing.push(qname.clone()),
        }
    }

    let mut propstats = Vec::new();
    if !found.is_empty() {
        propstats.push(PropStat::ok(found));
    }
    if !missing.is_empty() && !(allprop || minimal) {
        propstats.push(PropStat::not_found(missing));
    }
    DavResponse::props(node.href.clone(), propstats)
}

fn resolve_property(
    qname: &PropQName,
    node: &FilesNode,
    ctx: &FilesContext,
) -> Option<PropValue> {
    match (qname.ns.as_str(), qname.local.as_str()) {
        (NS_DAV, "getetag") => Some(PropValue::Text(format!("\"{}\"", node.etag))),
        (NS_DAV, "getlastmodified") => Some(PropValue::Text(http_date(node.mtime))),
        (NS_DAV, "resourcetype") => Some(if node.is_dir {
            PropValue::Elements(vec![XmlElement::new("d:collection")])
        } else {
            PropValue::Empty
        }),
        // `CorePlugin::propFind` handles these only for `IFile`.
        (NS_DAV, "getcontentlength") => {
            (!node.is_dir).then(|| PropValue::Text(node.size.to_string()))
        }
        (NS_DAV, "getcontenttype") => {
            (!node.is_dir).then(|| PropValue::Text(node.mimetype.clone()))
        }
        (NS_DAV, "displayname") => Some(PropValue::Text(node.displayname.clone())),
        // `FilesPlugin` -> `DateTimeImmutable::setTimestamp(getCreationTime())`
        // -> `DateTimeInterface::ATOM`.
        (NS_DAV, "creationdate") => Some(PropValue::Text(atom_date(node.creation_time))),
        // `CorePlugin::propFind` handles these only for `IQuota` (`Directory`).
        (NS_DAV, "quota-available-bytes") => {
            node.is_dir.then(|| PropValue::Text(ctx.quota_available.to_string()))
        }
        (NS_DAV, "quota-used-bytes") => {
            node.is_dir.then(|| PropValue::Text(node.size.to_string()))
        }
        (NS_OWNCLOUD, "size") => Some(PropValue::Text(node.size.to_string())),
        (NS_OWNCLOUD, "fileid") => Some(PropValue::Text(node.fileid.to_string())),
        (NS_OWNCLOUD, "id") => Some(PropValue::Text(format!(
            "{:08}{}",
            node.fileid, ctx.instance_id
        ))),
        (NS_OWNCLOUD, "permissions") => Some(PropValue::Text(node.dav_permissions())),
        (NS_OWNCLOUD, "owner-id") => Some(PropValue::Text(ctx.uid.clone())),
        (NS_OWNCLOUD, "owner-display-name") => {
            Some(PropValue::Text(ctx.owner_display_name.clone()))
        }
        (NS_OWNCLOUD, "favorite") => Some(PropValue::Text(
            if ctx.favorites.contains(&node.fileid) {
                "1"
            } else {
                "0"
            }
            .to_string(),
        )),
        (NS_OWNCLOUD, "comments-unread") => Some(PropValue::Text(
            ctx.unread.get(&node.fileid).copied().unwrap_or(0).to_string(),
        )),
        // `FilesPlugin` handles this only for `File`, and only when the stored
        // checksum is non-empty.
        (NS_OWNCLOUD, "checksums") => {
            if node.is_dir {
                return None;
            }
            let checksum = node.checksum.as_deref().unwrap_or("");
            if checksum.is_empty() {
                return None;
            }
            Some(PropValue::Elements(
                checksum
                    .split(' ')
                    .map(|value| XmlElement::new("oc:checksum").text(value))
                    .collect(),
            ))
        }
        (NS_NEXTCLOUD_FILES, "has-preview") => Some(PropValue::Text(
            if has_preview(&node.mimetype, ctx.previews_enabled) {
                "true"
            } else {
                "false"
            }
            .to_string(),
        )),
        // `MountPoint::getMountType()` returns '' for the home mount.
        (NS_NEXTCLOUD_FILES, "mount-type") => Some(PropValue::Empty),
        // `FilesPlugin` -> `getInternalPath() === '' ? 'true' : 'false'`. A
        // servable node is never a mount root (mount roots are delegated).
        (NS_NEXTCLOUD_FILES, "is-mount-root") => Some(PropValue::Text("false".to_string())),
        // `FilesPlugin` -> `isset($metadata['files-live-photo']) && mimetype
        // === 'video/quicktime'`.
        (NS_NEXTCLOUD_FILES, "hidden") => Some(PropValue::Text(
            if is_hidden(node) { "true" } else { "false" }.to_string(),
        )),
        // `FilesPlugin` -> `getShareAttributes()`: `[]` for a node whose storage
        // is not an `ISharedStorage` (every servable own-home node).
        (NS_NEXTCLOUD_FILES, "share-attributes") => {
            Some(PropValue::Text("[]".to_string()))
        }
        // `getNoteFromShare()` / `getHideDownload()` return null for a
        // non-shared storage, so Sabre leaves the property at 404.
        (NS_NEXTCLOUD_FILES, "note") | (NS_NEXTCLOUD_FILES, "hide-download") => None,
        // No server handler exists for `nc:is-encrypted` in Nextcloud 33/36 (nor
        // in the encryption app), so PHP answers 404. Match it; deriving from
        // `oc_filecache.encrypted` would be a deviation.
        (NS_NEXTCLOUD_FILES, "is-encrypted") => None,
        (NS_NEXTCLOUD_FILES, "sharees") => Some(PropValue::Elements(sharee_elements(node, ctx))),
        // `SharesPlugin` -> `ShareTypeList`.
        (NS_OWNCLOUD, "share-types") => {
            Some(PropValue::Elements(share_type_elements(node, ctx)))
        }
        // `FilesPlugin` -> `File::getDirectDownload()`: `false` for a local
        // home storage (a file only; the directory branch never runs).
        (NS_OWNCLOUD, "downloadURL") => (!node.is_dir).then(|| PropValue::Text(String::new())),
        // `{oc}dDC` is a custom property the desktop client reads. Nextcloud
        // disallows it in `CustomPropertiesBackend` (`isPropertyAllowed`), so
        // PHP always answers 404.
        (NS_OWNCLOUD, "dDC") => None,
        // `FilesPlugin` -> `getSystemValue('data-fingerprint', '')`, for every
        // node (not only the root).
        (NS_OWNCLOUD, "data-fingerprint") => {
            Some(PropValue::Text(ctx.data_fingerprint.clone()))
        }
        // `Node::getSharePermissions()`: the stored permissions, with CREATE and
        // DELETE stripped for files. A servable node is never on shared storage
        // nor a (non-movable) mount root, so the wrapper branches do not apply.
        (NS_OCS, "share-permissions") => {
            Some(PropValue::Text(share_permissions(node).to_string()))
        }
        // `FilesPlugin` loops `FileInfo::getMetadata()` and handles
        // `{nc}metadata-<key>` for each.
        (NS_NEXTCLOUD_FILES, local) if local.starts_with("metadata-") => {
            let key = &local["metadata-".len()..];
            node.metadata.get(key).and_then(json_to_propvalue)
        }
        _ => None,
    }
}

/// `FilesPlugin::HIDDEN_PROPERTYNAME`: a live photo is a `.mov` that carries
/// the `files-live-photo` metadata.
fn is_hidden(node: &FilesNode) -> bool {
    node.metadata.contains_key("files-live-photo") && node.mimetype == "video/quicktime"
}

/// `Node::getSharePermissions()` for a mount-free own-home node.
fn share_permissions(node: &FilesNode) -> i64 {
    let mut permissions = node.permissions;
    if !node.is_dir {
        permissions &= !(PERMISSION_CREATE | PERMISSION_DELETE);
    }
    permissions
}

/// `SharesPlugin`'s `ShareTypeList`: one `<oc:share-type>` per distinct type.
fn share_type_elements(node: &FilesNode, ctx: &FilesContext) -> Vec<XmlElement> {
    let Some(shares) = ctx.shares.get(&node.fileid) else {
        return Vec::new();
    };
    let mut types: Vec<i64> = shares.iter().map(|share| share.share_type).collect();
    types.sort_unstable();
    types.dedup();
    types
        .into_iter()
        .map(|share_type| XmlElement::new("oc:share-type").text(share_type.to_string()))
        .collect()
}

/// `SharesPlugin`'s `ShareeList`: `<nc:sharee>` with id, display-name and type.
/// The id/display-name come from `IShare::getSharedWith()` /
/// `getSharedWithDisplayName()`, which are only populated for user and group
/// shares (the `DefaultShareProvider::createShare()` branches).
fn sharee_elements(node: &FilesNode, ctx: &FilesContext) -> Vec<XmlElement> {
    let Some(shares) = ctx.shares.get(&node.fileid) else {
        return Vec::new();
    };
    shares
        .iter()
        .map(|share| {
            let (id, display_name) = match share.share_type {
                0 => (
                    share.share_with.clone().unwrap_or_default(),
                    share
                        .user_displayname
                        .clone()
                        .or_else(|| share.share_with.clone())
                        .unwrap_or_default(),
                ),
                1 => (
                    share.share_with.clone().unwrap_or_default(),
                    share
                        .group_displayname
                        .clone()
                        .or_else(|| share.share_with.clone())
                        .unwrap_or_default(),
                ),
                _ => (String::new(), String::new()),
            };
            XmlElement::new("nc:sharee")
                .child(XmlElement::new("nc:id").text(id))
                .child(XmlElement::new("nc:display-name").text(display_name))
                .child(XmlElement::new("nc:type").text(share.share_type.to_string()))
        })
        .collect()
}

/// Serialises a metadata JSON value the way Sabre's `standardSerializer` does:
/// scalars as text, objects as nested (unqualified) elements, lists as the
/// concatenation of their scalar items. `null` is absent.
fn json_to_propvalue(value: &serde_json::Value) -> Option<PropValue> {
    use serde_json::Value;
    match value {
        Value::Null => None,
        Value::Bool(true) => Some(PropValue::Text("1".to_string())),
        Value::Bool(false) => Some(PropValue::Text(String::new())),
        Value::Number(number) => Some(PropValue::Text(number.to_string())),
        Value::String(text) => Some(PropValue::Text(text.clone())),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                if let Some(PropValue::Text(text)) = json_to_propvalue(item) {
                    out.push_str(&text);
                }
            }
            Some(PropValue::Text(out))
        }
        Value::Object(map) => Some(PropValue::Elements(
            map.iter()
                .map(|(key, item)| {
                    let element = XmlElement::new(key.clone());
                    match json_to_propvalue(item) {
                        Some(PropValue::Text(text)) => element.text(text),
                        Some(PropValue::Elements(children)) => element.children(children),
                        Some(PropValue::Empty) | None => element,
                    }
                })
                .collect(),
        )),
    }
}

/// `DateTimeInterface::ATOM` (`Y-m-d\TH:i:sP`) in UTC, the timezone Nextcloud
/// pins (`OC_Util::setupFS()` -> `date_default_timezone_set('UTC')`).
fn atom_date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = remainder / 3_600;
    let minute = (remainder % 3_600) / 60;
    let second = remainder % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00"
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 -> y/m/d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// A best-effort `IPreview::isAvailable()`.
///
/// The exact answer depends on the enabled apps, the loaded imagick/ffmpeg
/// binaries and the configured office previews, none of which a database reader
/// can see. This matches the core providers that are always registered (see
/// `PreviewManager::registerCoreProviders()`); binary-dependent formats are
/// declared as the `files-has-preview-static` deviation.
fn has_preview(mimetype: &str, enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    let mime = mimetype.to_ascii_lowercase();
    mime == "text/plain"
        || mime.starts_with("text/markdown")
        || mime.starts_with("text/x-markdown")
        || mime.starts_with("image/png")
        || mime.starts_with("image/jpeg")
        || mime.starts_with("image/gif")
        || mime.starts_with("image/bmp")
        || mime.starts_with("image/x-xbitmap")
        || mime.starts_with("image/webp")
        || mime.starts_with("application/x-krita")
        || mime == "audio/mpeg"
        || mime.starts_with("application/vnd.oasis.opendocument")
        // Imagick-dependent formats are also previewed on a stock image.
        || mime.starts_with("image/svg+xml")
        || mime.starts_with("image/tiff")
        || mime.starts_with("application/pdf")
        || mime.starts_with("application/illustrator")
        || mime.starts_with("application/x-photoshop")
        || mime.starts_with("application/postscript")
        || mime.starts_with("application/font-sfnt")
        || mime.starts_with("application/x-font")
        || mime.starts_with("image/hei")
        || mime.starts_with("image/x-hei")
        || mime.starts_with("image/tga")
        || mime.starts_with("image/x-tga")
        || mime.starts_with("image/targa")
        || mime.starts_with("image/x-targa")
        || mime.starts_with("image/sgi")
        || mime.starts_with("image/x-sgi")
        || mime.starts_with("application/msword")
        || mime.starts_with("application/vnd.ms-")
        || mime.starts_with("application/vnd.openxmlformats-officedocument")
        || mime.starts_with("application/vnd.sun.xml")
        || mime.starts_with("image/emf")
        || mime.starts_with("video/")
}

// ---------------------------------------------------------------------------
// Quota
// ---------------------------------------------------------------------------

/// `OCP\Util::computerFileSize()`.
pub fn computer_file_size(input: &str) -> Option<i64> {
    let value = input.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    if let Ok(bytes) = value.parse::<i64>() {
        return Some(bytes);
    }
    // `([kmgtp]?b?)$`, longest suffix first.
    const UNITS: &[(&str, i64)] = &[
        ("pb", 1024 * 1024 * 1024 * 1024 * 1024),
        ("tb", 1024 * 1024 * 1024 * 1024),
        ("gb", 1024 * 1024 * 1024),
        ("mb", 1024 * 1024),
        ("kb", 1024),
        ("p", 1024 * 1024 * 1024 * 1024 * 1024),
        ("t", 1024 * 1024 * 1024 * 1024),
        ("g", 1024 * 1024 * 1024),
        ("m", 1024 * 1024),
        ("k", 1024),
        ("b", 1),
    ];
    let (number, multiplier) = UNITS
        .iter()
        .find_map(|(suffix, multiplier)| {
            value
                .strip_suffix(suffix)
                .map(|number| (number, *multiplier))
        })
        .unwrap_or((value.as_str(), 1));
    let number = leading_float(number.trim())?;
    Some((number * multiplier as f64).round() as i64)
}

/// PHP's `(float)` cast: the longest leading numeric prefix.
fn leading_float(input: &str) -> Option<f64> {
    let input = input.trim_start();
    let mut end = 0;
    let bytes = input.as_bytes();
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_exp = false;
    while end < bytes.len() {
        let byte = bytes[end];
        match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                end += 1;
            }
            b'+' | b'-' if end == 0 => end += 1,
            b'.' if !seen_dot && !seen_exp => {
                seen_dot = true;
                end += 1;
            }
            b'e' | b'E' if seen_digit && !seen_exp => {
                seen_exp = true;
                end += 1;
            }
            _ => break,
        }
    }
    if !seen_digit {
        return None;
    }
    input[..end].parse::<f64>().ok()
}

/// `Local::free_space()`'s `disk_free_space()`.
#[cfg(unix)]
pub fn disk_free_space(path: &Path) -> Option<i64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let free = (stat.f_bavail as u64).checked_mul(stat.f_frsize as u64)?;
    Some(free as i64)
}

#[cfg(not(unix))]
pub fn disk_free_space(_path: &Path) -> Option<i64> {
    None
}

/// `Directory::getQuotaInfo()` → `OC_Helper::getStorageInfo($path, $info, false)`
/// for a node on the caller's home storage.
///
/// `used` is the node's own raw size (served as `quota-used-bytes`), and `free`
/// is the home `Quota` wrapper's `free_space()`: unlimited quota reports
/// `SPACE_UNLIMITED` (-3), a finite quota reports
/// `min(disk_free, max(quota - used_root, 0))`.
pub async fn quota_available(db: &Db, uid: &str, config: &Config) -> Result<i64> {
    let mut quota = db
        .user_preference(uid, "files", "quota")
        .await?
        .unwrap_or_default();
    if quota.is_empty() || quota == "default" {
        let mut default = db
            .appconfig_value("files", "default_quota")
            .await?
            .unwrap_or_default();
        if default.is_empty() {
            default = "none".to_string();
        }
        let allow_unlimited = db
            .appconfig_value("files", "allow_unlimited_quota")
            .await?
            .map(|value| value == "1")
            .unwrap_or(true);
        if !allow_unlimited && default == "none" {
            let presets = db
                .appconfig_value("files", "quota_preset")
                .await?
                .unwrap_or_default();
            if let Some(preset) = presets
                .split(',')
                .map(str::trim)
                .find(|preset| !preset.is_empty() && *preset != "default" && *preset != "none")
            {
                default = preset.to_string();
            }
        }
        quota = default;
    }
    if quota == "none" {
        return Ok(SPACE_UNLIMITED);
    }
    let Some(quota_bytes) = computer_file_size(&quota) else {
        return Ok(SPACE_UNLIMITED);
    };

    // `Quota::free_space()` computes against `sizeRoot = 'files'`.
    let used_root = match db
        .resolve_home_file(uid, &crate::db::md5_hex(b"files"))
        .await?
    {
        Some(row) => row.effective_size(),
        None => 0,
    };
    let quota_free = (quota_bytes - used_root).max(0);
    let free = match config.datadirectory.as_deref().and_then(disk_free_space) {
        Some(disk_free) if disk_free >= 0 => disk_free.min(quota_free),
        _ => quota_free,
    };
    Ok(free)
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

/// Parses the `Depth` header the way Sabre's `getHTTPDepth(1)` does for
/// PROPFIND: absent or non-numeric defaults to 1, any non-zero numeric value is
/// clamped to 1 (depth infinity is not enabled).
pub fn parse_depth(headers: &axum::http::HeaderMap) -> i64 {
    match headers.get("depth").and_then(|value| value.to_str().ok()) {
        None => 1,
        Some(raw) => {
            let raw = raw.trim();
            if raw.parse::<i64>().map(|n| n == 0).unwrap_or(false) {
                0
            } else {
                1
            }
        }
    }
}

/// `Prefer: return=minimal` (or `Brief: t`) strips the 404 propstat.
pub fn prefer_minimal(headers: &axum::http::HeaderMap) -> bool {
    if let Some(prefer) = headers.get("prefer").and_then(|value| value.to_str().ok()) {
        return prefer
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("return=minimal"));
    }
    headers
        .get("brief")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == "t")
        .unwrap_or(false)
}

/// Serves a files `PROPFIND` or returns 501 (delegate to PHP).
pub async fn handle_propfind(
    db: &Db,
    config: &Config,
    user: &crate::auth::AuthenticatedUser,
    parsed: &FilesPath,
    depth: i64,
    minimal: bool,
    body: &[u8],
) -> Result<Option<MultiStatus>> {
    // Condition 1: only the app-password fast path may be served natively.
    if user.method != AuthMethod::FastPath {
        return Ok(None);
    }
    if config.instance_id.is_empty() || config.sharing_exclude_groups {
        return Ok(None);
    }

    // Parse the request body first: a malformed body is a 400 in PHP too.
    let request = crate::xml::parse::parse_propfind(body)?;

    // Condition 3: no mount at/under the path, and no mount below it.
    let rel_norm = normalize_rel(&parsed.rel_raw);
    let mounts = db.user_mount_points(&parsed.uid).await?;
    if mounts_delegate(&mounts, &parsed.uid, &rel_norm) {
        return Ok(None);
    }

    // Condition 2: resolve inside the caller's own home storage.
    let internal = internal_path(&rel_norm);
    let path_hash = crate::db::md5_hex(internal.as_bytes());
    let Some(row) = db.resolve_home_file(&parsed.uid, &path_hash).await? else {
        return Err(Error::NotFound);
    };

    // Condition 4: every explicit property must be implemented.
    if let PropList::Props(props) = &request.props {
        if props
            .iter()
            .any(|qname| !is_implemented(&qname.ns, &qname.local))
        {
            return Ok(None);
        }
    }

    // `oc:downloadURL` is a presigned URL on a primary object store
    // (`ObjectStoreStorage::getDirectDownloadById`), which the sidecar cannot
    // derive. Delegate rather than answer a wrong URL.
    let wants_download_url = match &request.props {
        PropList::AllProp | PropList::PropName => false,
        PropList::Props(props) => props
            .iter()
            .any(|qname| qname.ns == NS_OWNCLOUD && qname.local == "downloadURL"),
    };
    if config.objectstore && wants_download_url {
        return Ok(None);
    }

    // `nc:is-encrypted` is the end-to-end encryption app's property; with that
    // app enabled the sidecar's 404 would be wrong, so delegate it.
    let wants_is_encrypted = match &request.props {
        PropList::AllProp | PropList::PropName => false,
        PropList::Props(props) => props
            .iter()
            .any(|qname| qname.ns == NS_NEXTCLOUD_FILES && qname.local == "is-encrypted"),
    };
    if config.e2e_encryption && wants_is_encrypted {
        return Ok(None);
    }

    let needs = Needs::for_props(&request.props);
    let owner_display_name = if needs.owner_display_name {
        db.user_display_name(&parsed.uid)
            .await?
            .unwrap_or_else(|| parsed.uid.clone())
    } else {
        parsed.uid.clone()
    };
    let quota_available = if needs.quota {
        quota_available(db, &parsed.uid, config).await?
    } else {
        SPACE_UNLIMITED
    };

    let is_dir = row.is_directory();
    let is_home_root = rel_norm.is_empty();

    // Depth 1 children, in PHP's (unordered) database order.
    let children = if depth >= 1 && is_dir {
        db.file_children(row.storage, row.fileid).await?
    } else {
        Vec::new()
    };

    let mut ids: Vec<i64> = Vec::with_capacity(children.len() + 1);
    ids.push(row.fileid);
    ids.extend(children.iter().map(|child| child.fileid));
    let favorites = if needs.favorite {
        db.favorite_fileids(&parsed.uid, &ids).await?
    } else {
        HashSet::new()
    };
    let unread = if needs.unread {
        db.unread_comment_counts(&parsed.uid, &ids).await?
    } else {
        HashMap::new()
    };

    // One bulk share query per collection (the target node and, for a Depth 1
    // directory, its direct children), exactly like `SharesPlugin` preloads it.
    let mut shares: HashMap<i64, Vec<ShareRow>> = HashMap::new();
    if needs.shares {
        for share in db.node_share_rows(&parsed.uid, row.fileid).await? {
            shares.entry(row.fileid).or_default().push(share);
        }
        if depth >= 1 && is_dir {
            for (fileid, rows) in db.folder_share_rows(&parsed.uid, row.fileid).await? {
                shares.entry(fileid).or_default().extend(rows);
            }
        }
    }

    let ctx = FilesContext {
        uid: parsed.uid.clone(),
        instance_id: config.instance_id.clone(),
        owner_display_name,
        quota_available,
        previews_enabled: config.previews_enabled,
        data_fingerprint: config.data_fingerprint.clone(),
        favorites,
        unread,
        shares,
    };

    let parent_permissions = db.file_permissions(row.parent).await?.unwrap_or(0);
    let node_href = if is_dir {
        format!("{}/", parsed.href)
    } else {
        parsed.href.clone()
    };
    let displayname = if is_home_root {
        parsed.uid.clone()
    } else {
        row.display_name()
    };
    let node = FilesNode::from_row(
        &row,
        node_href.clone(),
        displayname,
        parent_permissions,
        is_home_root,
    );

    let mut responses = vec![build_response(&node, &ctx, &request.props, minimal)];
    for child in &children {
        // Sabre appends a trailing slash to a collection's href.
        let child_href = format!(
            "{node_href}{}{}",
            encode_path_segment(&child.display_name()),
            if child.is_directory() { "/" } else { "" }
        );
        let child_node = FilesNode::from_row(
            child,
            child_href,
            child.display_name(),
            row.permissions,
            false,
        );
        responses.push(build_response(
            &child_node,
            &ctx,
            &request.props,
            minimal,
        ));
    }

    Ok(Some(MultiStatus {
        responses,
        sync_token: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_files_paths() {
        let parsed = parse_files_path("/remote.php/dav/files/alice").unwrap();
        assert_eq!(parsed.uid, "alice");
        assert!(parsed.rel_segments.is_empty());
        assert_eq!(parsed.href, "/remote.php/dav/files/alice");

        let parsed = parse_files_path("/remote.php/dav/files/alice/a/b.txt").unwrap();
        assert_eq!(parsed.rel_segments, vec!["a", "b.txt"]);
        assert_eq!(parsed.rel_raw, "a/b.txt");
        assert_eq!(parsed.href, "/remote.php/dav/files/alice/a/b.txt");

        // Percent-decoding happens before splitting, so `%2f` is a separator.
        let parsed = parse_files_path("/remote.php/dav/files/al%20ice/a%2fb").unwrap();
        assert_eq!(parsed.uid, "al ice");
        assert_eq!(parsed.rel_segments, vec!["a", "b"]);
        // Sabre re-encodes with lower-case hex.
        assert_eq!(parsed.href, "/remote.php/dav/files/al%20ice/a/b");

        assert!(parse_files_path("/remote.php/dav/files/").is_none());
        assert!(parse_files_path("/remote.php/dav/addressbooks/users/a/b").is_none());
    }

    #[test]
    fn normalizes_nfc_and_collapses_paths() {
        // `e` + U+0301 (combining acute) normalises to U+00E9.
        assert_eq!(normalize_rel("cafe\u{301}"), "café");
        assert_eq!(normalize_rel("a//b"), "a/b");
        assert_eq!(normalize_rel("./a/./b/"), "a/b");
        assert_eq!(normalize_rel("a\\b"), "a/b");
        assert_eq!(normalize_rel(""), "");
        assert_eq!(normalize_rel("/"), "");
    }

    #[test]
    fn builds_internal_paths() {
        assert_eq!(internal_path(""), "files");
        assert_eq!(internal_path("a/b"), "files/a/b");
        // `path_hash` is `md5(<internal>)`.
        assert_eq!(
            crate::db::md5_hex(internal_path("a/b").as_bytes()),
            "3fc5bce0add1349da7dacd52db0033f1"
        );
    }

    #[test]
    fn mount_delegation_both_directions() {
        let mounts = vec![
            "/alice/".to_string(),
            "/alice/files/Share/".to_string(),
            "/alice/files/A/B/".to_string(),
        ];
        // Home mount alone is ignored.
        assert!(!mounts_delegate(&["/alice/".to_string()], "alice", "docs"));
        // At a mount.
        assert!(mounts_delegate(&mounts, "alice", "Share"));
        // Under a mount.
        assert!(mounts_delegate(&mounts, "alice", "Share/sub"));
        // A mount below the collection.
        assert!(mounts_delegate(&mounts, "alice", "A"));
        // Any mount at all makes the home root unservable.
        assert!(mounts_delegate(&mounts, "alice", ""));
        // Sibling of a mount is fine.
        assert!(!mounts_delegate(&mounts, "alice", "Other"));
        assert!(!mounts_delegate(&mounts, "alice", "A/C"));
    }

    #[test]
    fn property_gate_is_exact() {
        assert!(is_implemented(NS_DAV, "getetag"));
        assert!(is_implemented(NS_OWNCLOUD, "permissions"));
        assert!(is_implemented(NS_NEXTCLOUD_FILES, "mount-type"));
        assert!(is_implemented(NS_OWNCLOUD, "share-types"));
        assert!(is_implemented(NS_NEXTCLOUD_FILES, "metadata-blurhash"));
        assert!(is_implemented(
            NS_NEXTCLOUD_FILES,
            "metadata-files-live-photo"
        ));
        assert!(is_implemented(NS_DAV, "creationdate"));
        assert!(is_implemented(NS_OCS, "share-permissions"));
        assert!(is_implemented(NS_NEXTCLOUD_FILES, "is-mount-root"));
        assert!(is_implemented(NS_NEXTCLOUD_FILES, "hidden"));
        assert!(is_implemented(NS_NEXTCLOUD_FILES, "share-attributes"));
        assert!(is_implemented(NS_OWNCLOUD, "downloadURL"));
        assert!(is_implemented(NS_OWNCLOUD, "dDC"));
        assert!(is_implemented(NS_OWNCLOUD, "data-fingerprint"));
        // Still delegated (501, never 404).
        assert!(!is_implemented(NS_OWNCLOUD, "tags"));
        assert!(!is_implemented(NS_NEXTCLOUD_FILES, "system-tags"));
        assert!(!is_implemented(NS_NEXTCLOUD_FILES, "lock"));
        assert!(!is_implemented(NS_OWNCLOUD, "comments-count"));
        assert!(!is_implemented(NS_DAV, "owner"));
        // `metadata_etag` (underscore) is not a `metadata-<key>` property.
        assert!(!is_implemented(NS_NEXTCLOUD_FILES, "metadata_etag"));
    }

    #[test]
    fn atom_dates_match_php() {
        assert_eq!(atom_date(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(atom_date(1_700_000_000), "2023-11-14T22:13:20+00:00");
        assert_eq!(atom_date(1_700_000_100), "2023-11-14T22:15:00+00:00");
        assert_eq!(atom_date(1_700_000_300), "2023-11-14T22:18:20+00:00");
    }

    #[test]
    fn share_permissions_match_php() {
        let node = |permissions, is_dir| FilesNode {
            fileid: 1,
            name: "x".into(),
            displayname: "x".into(),
            size: 0,
            mtime: 0,
            etag: "e".into(),
            permissions,
            is_dir,
            mimetype: if is_dir {
                DIRECTORY_MIMETYPE.into()
            } else {
                "text/plain".into()
            },
            checksum: None,
            href: "/x".into(),
            parent_permissions: 31,
            is_home_root: false,
            creation_time: 0,
            metadata: HashMap::new(),
        };
        // A directory keeps its permissions (live PHP: 31).
        assert_eq!(share_permissions(&node(31, true)), 31);
        // A file strips CREATE|DELETE (live PHP: 27 -> 19).
        assert_eq!(share_permissions(&node(27, false)), 19);
    }

    #[test]
    fn metadata_values_serialise_like_sabre() {
        use serde_json::json;
        assert_eq!(
            json_to_propvalue(&json!("L0TSUAWBWB")),
            Some(PropValue::Text("L0TSUAWBWB".to_string()))
        );
        assert_eq!(
            json_to_propvalue(&json!(1608472028)),
            Some(PropValue::Text("1608472028".to_string()))
        );
        assert_eq!(json_to_propvalue(&json!(null)), None);
        let object = json_to_propvalue(&json!({"width": 905, "height": 958})).unwrap();
        let PropValue::Elements(children) = object else {
            panic!("expected nested elements")
        };
        assert_eq!(children.len(), 2);
        assert!(children.iter().any(|c| c.name == "width" && c.text.as_deref() == Some("905")));
        assert!(children
            .iter()
            .any(|c| c.name == "height" && c.text.as_deref() == Some("958")));
    }

    #[test]
    fn hidden_only_for_live_photo_mov() {
        let mut node = FilesNode {
            fileid: 1,
            name: "x.mov".into(),
            displayname: "x.mov".into(),
            size: 0,
            mtime: 0,
            etag: "e".into(),
            permissions: 27,
            is_dir: false,
            mimetype: "video/quicktime".into(),
            checksum: None,
            href: "/x.mov".into(),
            parent_permissions: 31,
            is_home_root: false,
            creation_time: 0,
            metadata: HashMap::new(),
        };
        assert!(!is_hidden(&node));
        node.metadata
            .insert("files-live-photo".into(), serde_json::json!("42"));
        assert!(is_hidden(&node));
        node.mimetype = "image/jpeg".into();
        assert!(!is_hidden(&node));
    }

    #[test]
    fn permissions_letters_match_php() {
        let node = |permissions, is_dir| FilesNode {
            fileid: 1,
            name: "x".into(),
            displayname: "x".into(),
            size: 0,
            mtime: 0,
            etag: "e".into(),
            permissions,
            is_dir,
            mimetype: if is_dir {
                DIRECTORY_MIMETYPE.into()
            } else {
                "text/plain".into()
            },
            checksum: None,
            href: "/x".into(),
            parent_permissions: 31,
            is_home_root: false,
            creation_time: 0,
            metadata: HashMap::new(),
        };
        // A plain home dir is RGDNVCK, a plain file RGDNVW.
        assert_eq!(node(31, true).dav_permissions(), "RGDNVCK");
        assert_eq!(node(27, false).dav_permissions(), "RGDNVW");
        // A read-only file: no UPDATE/DELETE, so no N/V/W.
        assert_eq!(node(17, false).dav_permissions(), "RG");
        // The home root is still renamable while updateable.
        let mut home = node(31, true);
        home.is_home_root = true;
        assert_eq!(home.dav_permissions(), "RGDNVCK");
        // A non-updateable but deletable node needs the parent's CREATE for N.
        let mut deletable = node(25, false);
        deletable.parent_permissions = 0;
        assert_eq!(deletable.dav_permissions(), "RGD");
        deletable.parent_permissions = 4;
        assert_eq!(deletable.dav_permissions(), "RGDN");
    }

    #[test]
    fn allprop_is_sabres_fixed_list() {
        let list: Vec<(String, String)> = allprop_list()
            .into_iter()
            .map(|q| (q.ns, q.local))
            .collect();
        assert_eq!(
            list,
            vec![
                (NS_DAV.to_string(), "getlastmodified".to_string()),
                (NS_DAV.to_string(), "getcontentlength".to_string()),
                (NS_DAV.to_string(), "resourcetype".to_string()),
                (NS_DAV.to_string(), "quota-used-bytes".to_string()),
                (NS_DAV.to_string(), "quota-available-bytes".to_string()),
                (NS_DAV.to_string(), "getetag".to_string()),
                (NS_DAV.to_string(), "getcontenttype".to_string()),
            ]
        );
    }

    #[test]
    fn computer_file_size_matches_php() {
        assert_eq!(computer_file_size("1024"), Some(1024));
        assert_eq!(computer_file_size("1 KB"), Some(1024));
        assert_eq!(computer_file_size("5 GB"), Some(5 * 1024 * 1024 * 1024));
        assert_eq!(computer_file_size("1.5TB"), Some(1024 * 1024 * 1024 * 1024 * 3 / 2));
        assert_eq!(computer_file_size("2m"), Some(2 * 1024 * 1024));
        assert_eq!(computer_file_size("none"), None);
        assert_eq!(computer_file_size("garbage"), None);
    }

    #[test]
    fn depth_defaults_to_one() {
        use axum::http::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        assert_eq!(parse_depth(&headers), 1);
        headers.insert("depth", HeaderValue::from_static("0"));
        assert_eq!(parse_depth(&headers), 0);
        headers.insert("depth", HeaderValue::from_static("1"));
        assert_eq!(parse_depth(&headers), 1);
        headers.insert("depth", HeaderValue::from_static("infinity"));
        assert_eq!(parse_depth(&headers), 1);
        headers.insert("depth", HeaderValue::from_static("bogus"));
        assert_eq!(parse_depth(&headers), 1);
    }

    #[test]
    fn minimal_preference() {
        use axum::http::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        assert!(!prefer_minimal(&headers));
        headers.insert("prefer", HeaderValue::from_static("return=minimal"));
        assert!(prefer_minimal(&headers));
        headers.insert("prefer", HeaderValue::from_static("return=representation"));
        assert!(!prefer_minimal(&headers));
        headers.remove("prefer");
        headers.insert("brief", HeaderValue::from_static("t"));
        assert!(prefer_minimal(&headers));
    }
}
