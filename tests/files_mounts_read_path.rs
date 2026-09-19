// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! DB-backed integration tests for the mount-bearing files `PROPFIND`
//! (phase 2, `src/mounts.rs` + `src/files.rs`).
//!
//! Covers the home root (a listing that *contains* mounts), listings inside a
//! share/groupfolder/external mount, the provider permission masks, the
//! synthetic parent etag/size/mtime, and the delegation rules (groupfolder ACL,
//! stale rows). When PostgreSQL is unavailable the whole file prints SKIP and
//! returns.

mod common;

use common::{propfind, TestEnv};
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{NS_DAV, NS_NEXTCLOUD_FILES, NS_OCS, NS_OWNCLOUD};

const USER: &str = "alice";
const PASSWORD: &str = "app-password";
const FILES: &str = "/remote.php/dav/files/alice";

const PROVIDER_SHARE: &str = "OCA\\Files_Sharing\\MountProvider";
const PROVIDER_GROUPFOLDER: &str = "OCA\\GroupFolders\\Mount\\MountProvider";

fn prop_body(props: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns" xmlns:ocs="http://open-collaboration-services.org/ns"><d:prop>{props}</d:prop></d:propfind>"#
    )
}

fn responses(doc: &XNode) -> Vec<&XNode> {
    doc.children
        .iter()
        .filter(|child| child.ns == NS_DAV && child.local == "response")
        .collect()
}

fn response<'a>(doc: &'a XNode, href: &str) -> Option<&'a XNode> {
    responses(doc).into_iter().find(|node| {
        node.child(NS_DAV, "href")
            .map(|h| h.text == href)
            .unwrap_or(false)
    })
}

fn prop_of<'a>(node: &'a XNode, ns: &str, local: &str) -> Option<&'a XNode> {
    node.children
        .iter()
        .filter(|child| child.ns == NS_DAV && child.local == "propstat")
        .filter_map(|propstat| propstat.child(NS_DAV, "prop"))
        .flat_map(|prop| prop.children.iter())
        .find(|child| child.ns == ns && child.local == local)
}

fn prop_text(node: &XNode, ns: &str, local: &str) -> Option<String> {
    prop_of(node, ns, local).map(|child| child.text.clone())
}

struct Fixture {
    env: TestEnv,
    app: axum::Router,
    files_root: i64,
    share_root: i64,
    team_root: i64,
}

async fn fixture() -> Option<Fixture> {
    let env = TestEnv::new().await?;
    env.seed_user(USER, Some("Alice A")).await;
    env.seed_user("bob", Some("Bob B")).await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;

    let home = env.seed_storage("home::alice").await;
    let files_root = env
        .seed_file(home, "files", "files", "httpd/unix-directory", 100, 1_700_000_000, "etagfiles", 31, 0, None)
        .await;
    env.seed_file(home, "files/Doc.txt", "Doc.txt", "text/plain", 5, 1_700_000_001, "etagdoc", 27, files_root, None)
        .await;

    // A received read-write and a read-only share from bob.
    let bob = env.seed_storage("home::bob").await;
    let share_root = env
        .seed_file(bob, "files/Shared", "Shared", "httpd/unix-directory", 1000, 1_700_000_100, "etagShared", 31, 0, None)
        .await;
    env.seed_file(bob, "files/Shared/Inner.txt", "Inner.txt", "text/plain", 7, 1_700_000_101, "etagInner", 27, share_root, None)
        .await;
    let share_ro_root = env
        .seed_file(bob, "files/SharedRO", "SharedRO", "httpd/unix-directory", 500, 1_700_000_200, "etagSharedRO", 31, 0, None)
        .await;
    env.seed_incoming_share(0, USER, "bob", share_root, 31, 1).await;
    env.seed_incoming_share(0, USER, "bob", share_ro_root, 1, 1).await;
    env.seed_mount_full(USER, "/alice/files/Shared/", bob, share_root, PROVIDER_SHARE).await;
    env.seed_mount_full(USER, "/alice/files/SharedRO/", bob, share_ro_root, PROVIDER_SHARE).await;

    // A groupfolder mounted from the shared root storage (root-jail).
    let gf = env.seed_storage("local::/data/").await;
    let team_root = env
        .seed_file(gf, "__groupfolders/1", "1", "httpd/unix-directory", 2000, 1_700_000_300, "etagTeam", 31, 0, None)
        .await;
    env.seed_file(gf, "__groupfolders/1/File.txt", "File.txt", "text/plain", 9, 1_700_000_301, "etagTeamFile", 27, team_root, None)
        .await;
    let folder_id = env.seed_group_folder("Team", 0, -3, gf, team_root).await;
    env.seed_group("team").await;
    env.seed_group_member("team", USER).await;
    env.seed_group_folder_group(folder_id, Some("team"), None, 31).await;
    env.seed_mount_full(USER, "/alice/files/Team/", gf, team_root, PROVIDER_GROUPFOLDER).await;

    // A read-only local external mount.
    let ext = env.seed_storage("local::/external/").await;
    let ext_root = env
        .seed_file(ext, "", "", "httpd/unix-directory", 3000, 1_700_000_400, "etagExt", 23, 0, None)
        .await;
    env.seed_file(ext, "ExtFile.txt", "ExtFile.txt", "text/plain", 11, 1_700_000_401, "etagExtFile", 23, ext_root, None)
        .await;
    let mount_id = env.seed_external_mount("local").await;
    env.seed_external_option(mount_id, "readonly", "1").await;
    env.seed_external_option(mount_id, "enable_sharing", "1").await;
    env.seed_external_option(mount_id, "filesystem_check_changes", "0").await;
    env.seed_mount_external(USER, "/alice/files/Ext/", ext, ext_root, mount_id).await;

    let app = env.app_shared();
    Some(Fixture {
        env,
        app,
        files_root,
        share_root,
        team_root,
    })
}

/// The home root: the 85 % case. The listing must contain the cache children
/// *and* the mount entries, and the parent's etag/size/mtime must be synthetic.
#[tokio::test]
async fn home_root_lists_mounts_with_synthetic_parent() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let body = prop_body(
        "<d:getetag/><d:getlastmodified/><oc:size/><oc:fileid/><oc:permissions/>\
         <nc:mount-type/><nc:is-mount-root/><oc:owner-id/><ocs:share-permissions/>",
    );
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();

    let hrefs: Vec<String> = responses(&doc)
        .iter()
        .filter_map(|node| node.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect();
    assert_eq!(
        hrefs,
        vec![
            format!("{FILES}/"),
            format!("{FILES}/Doc.txt"),
            format!("{FILES}/Ext/"),
            format!("{FILES}/Shared/"),
            format!("{FILES}/SharedRO/"),
            format!("{FILES}/Team/"),
        ]
    );

    // The mount entries carry the right type/root/owner.
    let ext = response(&doc, &format!("{FILES}/Ext/")).unwrap();
    assert_eq!(prop_text(ext, NS_NEXTCLOUD_FILES, "mount-type").as_deref(), Some("external"));
    assert_eq!(prop_text(ext, NS_NEXTCLOUD_FILES, "is-mount-root").as_deref(), Some("true"));
    assert_eq!(prop_text(ext, NS_OWNCLOUD, "owner-id").as_deref(), Some("alice"));
    // readonly external: mask 17, non-movable -> 17 & ~(UPDATE|DELETE) = 17.
    assert_eq!(prop_text(ext, NS_OWNCLOUD, "permissions").as_deref(), Some("RMG"));
    assert_eq!(prop_text(ext, NS_OCS, "share-permissions").as_deref(), Some("17"));

    let team = response(&doc, &format!("{FILES}/Team/")).unwrap();
    assert_eq!(prop_text(team, NS_NEXTCLOUD_FILES, "mount-type").as_deref(), Some("group"));
    assert_eq!(prop_text(team, NS_NEXTCLOUD_FILES, "is-mount-root").as_deref(), Some("true"));
    // non-movable: 31 & ~(UPDATE|DELETE) = 21 -> RMGCK.
    assert_eq!(prop_text(team, NS_OWNCLOUD, "permissions").as_deref(), Some("RMGCK"));
    // getSharePermissions re-adds UPDATE|DELETE for a non-readonly mount root.
    assert_eq!(prop_text(team, NS_OCS, "share-permissions").as_deref(), Some("31"));

    let shared = response(&doc, &format!("{FILES}/Shared/")).unwrap();
    assert_eq!(prop_text(shared, NS_NEXTCLOUD_FILES, "mount-type").as_deref(), Some("shared"));
    assert_eq!(prop_text(shared, NS_OWNCLOUD, "owner-id").as_deref(), Some("bob"));
    // movable share root: 31 | UPDATE|DELETE -> SRGDNVCK.
    assert_eq!(prop_text(shared, NS_OWNCLOUD, "permissions").as_deref(), Some("SRGDNVCK"));
    assert_eq!(prop_text(shared, NS_OCS, "share-permissions").as_deref(), Some("31"));

    let shared_ro = response(&doc, &format!("{FILES}/SharedRO/")).unwrap();
    // read-only share root: 1 | UPDATE|DELETE = 11 -> SGDNV.
    assert_eq!(prop_text(shared_ro, NS_OWNCLOUD, "permissions").as_deref(), Some("SGDNV"));
    assert_eq!(prop_text(shared_ro, NS_OCS, "share-permissions").as_deref(), Some("1"));

    // The home root's own permissions are unchanged (it is not a mount).
    let root = response(&doc, &format!("{FILES}/")).unwrap();
    assert_eq!(
        prop_text(root, NS_OWNCLOUD, "fileid").as_deref(),
        Some(f.files_root.to_string().as_str())
    );
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("RGDNVCK"));
    assert_eq!(prop_text(root, NS_NEXTCLOUD_FILES, "is-mount-root").as_deref(), Some("false"));
    assert_eq!(prop_text(root, NS_NEXTCLOUD_FILES, "mount-type").as_deref(), Some(""));

    // Synthetic parent etag/size/mtime: ascending mount points, double slash,
    // decimal permissions. A Depth-1 listing uses the listing permission branch
    // (movable `|UPDATE|DELETE`, non-movable `& ~(UPDATE|DELETE)`): Ext 17,
    // Shared 31, SharedRO 11, Team 21.
    let combined = "etagfiles::/Ext//etagExt17::/Shared//etagShared31::/SharedRO//etagSharedRO11::/Team//etagTeam21";
    assert_eq!(
        prop_text(root, NS_DAV, "getetag").as_deref(),
        Some(format!("\"{}\"", nextcloud_dav::db::md5_hex(combined.as_bytes())).as_str())
    );
    assert_eq!(
        prop_text(root, NS_OWNCLOUD, "size").as_deref(),
        Some((100 + 3000 + 1000 + 500 + 2000).to_string().as_str())
    );
    // mtime = max(root, mounts) = 1700000400 (Ext).
    assert_eq!(
        prop_text(root, NS_DAV, "getlastmodified").as_deref(),
        Some(nextcloud_dav::util::http_date(1_700_000_400).as_str())
    );
}

/// A listing inside each mount kind, with the provider mask applied to every
/// row and `nc:mount-type` / `nc:is-mount-root` set.
/// A mount whose **contents** cannot be reproduced (a non-local external
/// backend) is still describable: a listing that merely contains it must be
/// served, include its entry, and carry the sidecar attribution header. A
/// request *inside* that mount must still delegate.
#[tokio::test]
async fn unservable_mount_does_not_suppress_the_containing_listing() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // A non-local external mount with a real root row: contents delegate, entry
    // is describable.
    let s3 = f.env.seed_storage("amazons3::bucket").await;
    let s3_root = f
        .env
        .seed_file(s3, "", "", "httpd/unix-directory", 4000, 1_700_000_500, "etagS3", 23, 0, None)
        .await;
    let mount_id = f.env.seed_external_mount("amazons3").await;
    f.env
        .seed_external_option(mount_id, "readonly", "1")
        .await;
    f.env
        .seed_mount_external(USER, "/alice/files/S3/", s3, s3_root, mount_id)
        .await;

    let body = prop_body(
        "<d:getetag/><oc:size/><oc:permissions/><oc:fileid/>\
         <nc:mount-type/><nc:is-mount-root/>",
    );
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "the home root must be served: {}", resp.text());
    assert_eq!(
        resp.header(nextcloud_dav::routes::SIDECAR_HEADER).as_deref(),
        Some(nextcloud_dav::routes::SIDECAR_VALUE),
        "the sidecar must identify the response it served"
    );
    let doc = parse_document(&resp.body).unwrap();
    let hrefs: Vec<String> = responses(&doc)
        .iter()
        .filter_map(|node| node.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect();
    assert!(
        hrefs.contains(&format!("{FILES}/S3/")),
        "the unservable mount entry must be merged into the listing: {hrefs:?}"
    );
    let s3_node = response(&doc, &format!("{FILES}/S3/")).unwrap();
    assert_eq!(
        prop_text(s3_node, NS_NEXTCLOUD_FILES, "mount-type").as_deref(),
        Some("external")
    );
    assert_eq!(
        prop_text(s3_node, NS_NEXTCLOUD_FILES, "is-mount-root").as_deref(),
        Some("true")
    );

    // Inside the mount: 501, and no sidecar attribution header.
    let resp = propfind(&f.app, &format!("{FILES}/S3"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 501, "a non-local backend must delegate: {}", resp.text());
    assert!(
        resp.header(nextcloud_dav::routes::SIDECAR_HEADER).is_none(),
        "a delegated 501 must not carry the sidecar header"
    );
}

#[tokio::test]
async fn listings_inside_each_mount_kind() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let body = prop_body(
        "<d:getetag/><oc:size/><oc:permissions/><oc:fileid/>\
         <nc:mount-type/><nc:is-mount-root/>",
    );

    // Share (read-write): the root is movable, the child keeps the share mask.
    let resp = propfind(&f.app, &format!("{FILES}/Shared"), USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/Shared/")).unwrap();
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("SRGDNVCK"));
    assert_eq!(prop_text(root, NS_NEXTCLOUD_FILES, "is-mount-root").as_deref(), Some("true"));
    assert_eq!(
        prop_text(root, NS_OWNCLOUD, "fileid").as_deref(),
        Some(f.share_root.to_string().as_str())
    );
    let child = response(&doc, &format!("{FILES}/Shared/Inner.txt")).unwrap();
    assert_eq!(prop_text(child, NS_OWNCLOUD, "permissions").as_deref(), Some("SRGDNVW"));
    assert_eq!(prop_text(child, NS_NEXTCLOUD_FILES, "mount-type").as_deref(), Some("shared"));
    assert_eq!(prop_text(child, NS_NEXTCLOUD_FILES, "is-mount-root").as_deref(), Some("false"));

    // Read-only share: every row is masked to READ.
    let resp = propfind(&f.app, &format!("{FILES}/SharedRO"), USER, PASSWORD, "1", &body).await;
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/SharedRO/")).unwrap();
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("SGDN"));

    // Groupfolder (root-jail): non-movable root, child keeps group perms.
    let resp = propfind(&f.app, &format!("{FILES}/Team"), USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/Team/")).unwrap();
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("RMGDNVCK"));
    assert_eq!(prop_text(root, NS_NEXTCLOUD_FILES, "mount-type").as_deref(), Some("group"));
    assert_eq!(
        prop_text(root, NS_OWNCLOUD, "fileid").as_deref(),
        Some(f.team_root.to_string().as_str())
    );
    let child = response(&doc, &format!("{FILES}/Team/File.txt")).unwrap();
    assert_eq!(prop_text(child, NS_OWNCLOUD, "permissions").as_deref(), Some("RMGDNVW"));

    // External (read-only): the mask is 17 for every row.
    let resp = propfind(&f.app, &format!("{FILES}/Ext"), USER, PASSWORD, "1", &body).await;
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/Ext/")).unwrap();
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("RMG"));
    let child = response(&doc, &format!("{FILES}/Ext/ExtFile.txt")).unwrap();
    // file: no DELETE/UPDATE -> R M G (READ|SHARE = 17).
    assert_eq!(prop_text(child, NS_OWNCLOUD, "permissions").as_deref(), Some("RMG"));
    assert_eq!(prop_text(child, NS_NEXTCLOUD_FILES, "mount-type").as_deref(), Some("external"));
    assert_eq!(prop_text(child, NS_NEXTCLOUD_FILES, "is-mount-root").as_deref(), Some("false"));
    // The `fileid` is the resolved row in the external storage.
    assert!(prop_text(child, NS_OWNCLOUD, "fileid").is_some());
}

/// A groupfolder with `acl = 1` is served: the per-path ACL permissions mask
/// every row and a row whose masked permissions are zero is dropped.
#[tokio::test]
async fn groupfolder_acl_masks_and_hides() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // A separate storage so the ACL rows cannot poison the servable groupfolder.
    let acl_storage = f.env.seed_storage("local::/data/acl/").await;
    let acl_root = f
        .env
        .seed_file(acl_storage, "__groupfolders/2", "2", "httpd/unix-directory", 30, 1, "etagAcl", 31, 0, None)
        .await;
    f.env
        .seed_file(acl_storage, "__groupfolders/2/Visible.txt", "Visible.txt", "text/plain", 3, 1, "etagVis", 31, acl_root, None)
        .await;
    let hidden = f
        .env
        .seed_file(acl_storage, "__groupfolders/2/Hidden.txt", "Hidden.txt", "text/plain", 4, 1, "etagHid", 31, acl_root, None)
        .await;
    let sub = f
        .env
        .seed_file(acl_storage, "__groupfolders/2/Sub", "Sub", "httpd/unix-directory", 5, 1, "etagSub", 31, acl_root, None)
        .await;
    f.env
        .seed_file(acl_storage, "__groupfolders/2/Sub/Deep.txt", "Deep.txt", "text/plain", 6, 1, "etagDeep", 31, sub, None)
        .await;
    let folder_id = f.env.seed_group_folder("Acl", 1, -3, acl_storage, acl_root).await;
    f.env.seed_group_folder_group(folder_id, Some("team"), None, 31).await;
    f.env
        .seed_mount_full(USER, "/alice/files/Acl/", acl_storage, acl_root, PROVIDER_GROUPFOLDER)
        .await;

    // Root: allow READ (base 31 stays 31). Hidden.txt: deny READ -> 0 -> hidden.
    // Sub: deny DELETE -> 31 & ~8 = 23 (inherited by Deep.txt).
    f.env.seed_acl_rule(acl_root, "group", "team", 1, 1).await;
    f.env.seed_acl_rule(hidden, "group", "team", 1, 0).await;
    f.env.seed_acl_rule(sub, "group", "team", 8, 0).await;

    let body = prop_body("<oc:permissions/><nc:mount-type/><nc:is-mount-root/>");
    // The home listing contains the ACL mount and the visible rows only.
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let hrefs: Vec<String> = responses(&doc)
        .iter()
        .filter_map(|node| node.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect();
    assert!(hrefs.contains(&format!("{FILES}/Acl/")));

    let resp = propfind(&f.app, &format!("{FILES}/Acl"), USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let hrefs: Vec<String> = responses(&doc)
        .iter()
        .filter_map(|node| node.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect();
    assert!(hrefs.contains(&format!("{FILES}/Acl/Visible.txt")));
    assert!(!hrefs.contains(&format!("{FILES}/Acl/Hidden.txt")), "the denied row must be dropped");
    assert!(hrefs.contains(&format!("{FILES}/Acl/Sub/")));

    // The root is served (read allowed) and keeps M; the visible child is 31.
    let root = response(&doc, &format!("{FILES}/Acl/")).unwrap();
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("RMGDNVCK"));
    let visible_node = response(&doc, &format!("{FILES}/Acl/Visible.txt")).unwrap();
    assert_eq!(prop_text(visible_node, NS_OWNCLOUD, "permissions").as_deref(), Some("RMGDNVW"));

    // The denied row is a 404 at Depth 0 (PHP drops it from the cache).
    let resp = propfind(&f.app, &format!("{FILES}/Acl/Hidden.txt"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 404, "a zero-permission row must be not-found");

    // `Sub` denies DELETE: 31 & ~8 = 23, inherited by `Deep.txt`.
    let resp = propfind(&f.app, &format!("{FILES}/Acl/Sub"), USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let sub_node = response(&doc, &format!("{FILES}/Acl/Sub/")).unwrap();
    assert_eq!(prop_text(sub_node, NS_OWNCLOUD, "permissions").as_deref(), Some("RMGNVCK"));
    let deep = response(&doc, &format!("{FILES}/Acl/Sub/Deep.txt")).unwrap();
    assert_eq!(prop_text(deep, NS_OWNCLOUD, "permissions").as_deref(), Some("RMGNVW"));
}

/// `acl_default_no_permission` gives an ACL manager READ, everyone else nothing;
/// a circle rule is the one input the core schema cannot resolve, so it delegates.
#[tokio::test]
async fn groupfolder_acl_default_and_circle() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let storage = f.env.seed_storage("local::/data/acl2/").await;
    let root = f
        .env
        .seed_file(storage, "__groupfolders/4", "4", "httpd/unix-directory", 7, 1, "etagAcl2", 31, 0, None)
        .await;
    let folder_id = f.env.seed_group_folder("AclDefault", 1, -3, storage, root).await;
    f.env.seed_group_folder_group(folder_id, Some("team"), None, 31).await;
    f.env.set_group_folder_acl_default(folder_id, true).await;
    // Alice's group can manage the folder -> base permission is READ.
    f.env.seed_group_folder_manage(folder_id, "group", "team").await;
    f.env
        .seed_mount_full(USER, "/alice/files/AclDefault/", storage, root, PROVIDER_GROUPFOLDER)
        .await;

    let body = prop_body("<oc:permissions/>");
    let resp = propfind(&f.app, &format!("{FILES}/AclDefault"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let node = response(&doc, &format!("{FILES}/AclDefault/")).unwrap();
    // base = READ, no rules -> 31 & 31 & 1 = 1 -> MG (non-movable root).
    assert_eq!(prop_text(node, NS_OWNCLOUD, "permissions").as_deref(), Some("MG"));

    // A circle rule cannot be resolved from the core schema: delegate.
    let circle_storage = f.env.seed_storage("local::/data/acl3/").await;
    let circle_root = f
        .env
        .seed_file(circle_storage, "__groupfolders/5", "5", "httpd/unix-directory", 8, 1, "etagAcl3", 31, 0, None)
        .await;
    let circle_folder = f.env.seed_group_folder("AclCircle", 1, -3, circle_storage, circle_root).await;
    f.env.seed_group_folder_group(circle_folder, Some("team"), None, 31).await;
    f.env.seed_acl_rule(circle_root, "circle", "some-single-id", 1, 1).await;
    f.env
        .seed_mount_full(USER, "/alice/files/AclCircle/", circle_storage, circle_root, PROVIDER_GROUPFOLDER)
        .await;
    let resp = propfind(&f.app, &format!("{FILES}/AclCircle"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 501, "a circle ACL rule must delegate");
}

/// A stale mount row (root no longer resolvable) delegates instead of 404ing,
/// and a genuinely missing path is still a 404.
#[tokio::test]
async fn stale_mount_and_missing_path() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    f.env.seed_mount(USER, "/alice/files/Ghost/").await;
    let body = prop_body("<d:getetag/>");
    // The stale mount root is not in the home cache; never 404.
    let resp = propfind(&f.app, &format!("{FILES}/Ghost"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 501, "a stale mount root must delegate");
    // The home listing contains the stale mount: delegate.
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 501);
    // A path with no mount at all is a real 404.
    let resp = propfind(&f.app, &format!("{FILES}/missing.txt"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 404);
}

/// A groupfolder whose per-user permission is read-only masks every row to
/// READ (no UPDATE/CREATE/DELETE).
#[tokio::test]
async fn read_only_groupfolder_mask() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // Add a second groupfolder on a separate storage, group permission = READ.
    let ro_storage = f.env.seed_storage("local::/data/ro/").await;
    let ro_root = f
        .env
        .seed_file(ro_storage, "__groupfolders/3", "3", "httpd/unix-directory", 20, 1, "etagRo", 31, 0, None)
        .await;
    let folder_id = f.env.seed_group_folder("ReadOnly", 0, -3, ro_storage, ro_root).await;
    f.env.seed_group_folder_group(folder_id, Some("team"), None, 1).await;
    f.env
        .seed_mount_full(USER, "/alice/files/ReadOnly/", ro_storage, ro_root, PROVIDER_GROUPFOLDER)
        .await;

    let body = prop_body("<oc:permissions/>");
    let resp = propfind(&f.app, &format!("{FILES}/ReadOnly"), USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/ReadOnly/")).unwrap();
    // masked to READ, non-movable -> 1 & ~(UPDATE|DELETE) = 1 -> MG.
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("MG"));
}

/// A file inside a received share gets the `S` letter and the share mask; the
/// `W` letter is decided from the unmodified root cache entry.
#[tokio::test]
async fn share_child_permission_letters() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let body = prop_body("<oc:permissions/>");
    let resp = propfind(&f.app, &format!("{FILES}/Shared/Inner.txt"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 207);
    let doc = parse_document(&resp.body).unwrap();
    let node = response(&doc, &format!("{FILES}/Shared/Inner.txt")).unwrap();
    assert_eq!(prop_text(node, NS_OWNCLOUD, "permissions").as_deref(), Some("SRGDNVW"));
    // The read-only share's child is masked to READ only.
    let resp = propfind(&f.app, &format!("{FILES}/SharedRO"), USER, PASSWORD, "1", &body).await;
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/SharedRO/")).unwrap();
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("SGDN"));
}
