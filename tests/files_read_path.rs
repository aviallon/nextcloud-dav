// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! DB-backed integration tests for the native files `PROPFIND`
//! (`src/files.rs`, routed by `src/routes.rs`).
//!
//! These exercise the real request path through the axum router against a
//! throwaway PostgreSQL database. When PostgreSQL is unavailable the whole
//! file prints a SKIP and returns (see `tests/README.md`).

mod common;

use axum::body::Body;
use axum::http::{header, Request};
use common::{call, propfind, request, safe, TestEnv};
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{NS_DAV, NS_NEXTCLOUD_FILES, NS_OCS, NS_OWNCLOUD};

const USER: &str = "alice";
const PASSWORD: &str = "app-password";

const FILES: &str = "/remote.php/dav/files/alice";

fn prop_body(props: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns" xmlns:ocs="http://open-collaboration-services.org/ns"><d:prop>{props}</d:prop></d:propfind>"#
    )
}

/// The exact property set the web UI sends: `@nextcloud/files` v4
/// `defaultDavProperties` plus the properties registered by
/// `apps/files_sharing/src/init.ts`, `apps/files/src/init.ts` and
/// `apps/files/src/services/LivePhotos.ts`.
const WEB_PROPS: &str = "<d:getcontentlength/><d:getcontenttype/><d:getetag/>\
<d:getlastmodified/><d:creationdate/><d:displayname/><d:quota-available-bytes/>\
<d:resourcetype/><nc:has-preview/><nc:is-encrypted/><nc:mount-type/>\
<oc:comments-unread/><oc:favorite/><oc:fileid/><oc:owner-display-name/>\
<oc:owner-id/><oc:permissions/><oc:size/><nc:note/><nc:sharees/>\
<nc:hide-download/><nc:share-attributes/><oc:share-types/>\
<ocs:share-permissions/><nc:hidden/><nc:is-mount-root/><nc:metadata-blurhash/>\
<nc:metadata-files-live-photo/>";

/// The exact property set the desktop client's `LsColJob::defaultProperties`
/// sends for a non-root folder on a server >= 10 with `files_lock` absent.
const DESKTOP_PROPS: &str = "<d:resourcetype/><d:getlastmodified/>\
<d:getcontentlength/><d:getetag/><d:quota-available-bytes/><d:quota-used-bytes/>\
<oc:size/><oc:id/><oc:fileid/><oc:downloadURL/><oc:dDC/><oc:permissions/>\
<oc:checksums/><nc:is-encrypted/><nc:metadata-files-live-photo/>\
<nc:share-attributes/><oc:share-types/><nc:is-mount-root/>";

/// The desktop root set additionally requests `oc:data-fingerprint`.
const DESKTOP_ROOT_PROPS: &str = "<d:resourcetype/><d:getlastmodified/>\
<d:getcontentlength/><d:getetag/><d:quota-available-bytes/><d:quota-used-bytes/>\
<oc:size/><oc:id/><oc:fileid/><oc:downloadURL/><oc:dDC/><oc:permissions/>\
<oc:checksums/><nc:is-encrypted/><nc:metadata-files-live-photo/>\
<nc:share-attributes/><oc:data-fingerprint/><oc:share-types/><nc:is-mount-root/>";

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

fn status_of(node: &XNode, ns: &str, local: &str) -> Option<String> {
    node.children
        .iter()
        .filter(|child| child.ns == NS_DAV && child.local == "propstat")
        .find(|propstat| {
            propstat
                .child(NS_DAV, "prop")
                .map(|prop| {
                    prop.children
                        .iter()
                        .any(|child| child.ns == ns && child.local == local)
                })
                .unwrap_or(false)
        })
        .and_then(|propstat| propstat.child(NS_DAV, "status"))
        .map(|status| status.text.clone())
}

struct Fixture {
    env: TestEnv,
    app: axum::Router,
    storage: i64,
    files_root: i64,
    docs: i64,
    notes: i64,
    notes_child: i64,
}

async fn fixture() -> Option<Fixture> {
    let env = TestEnv::new().await?;
    env.seed_user(USER, Some("Alice A")).await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;
    let storage = env.seed_storage("home::alice").await;
    let files_root = env
        .seed_file(
            storage,
            "files",
            "files",
            "httpd/unix-directory",
            100,
            1_700_000_000,
            "etagfiles",
            31,
            0,
            None,
        )
        .await;
    let docs = env
        .seed_file(
            storage,
            "files/Documents",
            "Documents",
            "httpd/unix-directory",
            50,
            1_700_000_100,
            "etagdocs",
            31,
            files_root,
            None,
        )
        .await;
    let notes = env
        .seed_file(
            storage,
            "files/Notes.txt",
            "Notes.txt",
            "text/plain",
            5,
            1_700_000_200,
            "etagnotes",
            27,
            files_root,
            Some("SHA1:deadbeef"),
        )
        .await;
    let notes_child = env
        .seed_file(
            storage,
            "files/Documents/Child.txt",
            "Child.txt",
            "text/plain",
            7,
            1_700_000_300,
            "etagchild",
            27,
            docs,
            None,
        )
        .await;
    env.seed_favorite(USER, notes).await;
    let app = env.app_shared();
    Some(Fixture {
        env,
        app,
        storage,
        files_root,
        docs,
        notes,
        notes_child,
    })
}

#[tokio::test]
async fn propfind_depth0_file_matches_php_shapes() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };

    let body = prop_body(WEB_PROPS);
    let resp = propfind(&f.app, &format!("{FILES}/Notes.txt"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    assert_eq!(responses(&doc).len(), 1);
    let node = response(&doc, &format!("{FILES}/Notes.txt")).expect("the file response");

    assert_eq!(prop_text(node, NS_DAV, "getetag").as_deref(), Some("\"etagnotes\""));
    assert_eq!(prop_text(node, NS_DAV, "getcontentlength").as_deref(), Some("5"));
    assert_eq!(prop_text(node, NS_DAV, "getcontenttype").as_deref(), Some("text/plain"));
    assert_eq!(prop_text(node, NS_DAV, "displayname").as_deref(), Some("Notes.txt"));
    assert_eq!(prop_text(node, NS_OWNCLOUD, "size").as_deref(), Some("5"));
    assert_eq!(
        prop_text(node, NS_OWNCLOUD, "fileid").as_deref(),
        Some(f.notes.to_string().as_str())
    );
    assert_eq!(prop_text(node, NS_OWNCLOUD, "permissions").as_deref(), Some("RGDNVW"));
    assert_eq!(prop_text(node, NS_OWNCLOUD, "owner-id").as_deref(), Some("alice"));
    assert_eq!(
        prop_text(node, NS_OWNCLOUD, "owner-display-name").as_deref(),
        Some("Alice A")
    );
    assert_eq!(prop_text(node, NS_OWNCLOUD, "favorite").as_deref(), Some("1"));
    assert_eq!(prop_text(node, NS_OWNCLOUD, "comments-unread").as_deref(), Some("0"));
    assert_eq!(prop_text(node, NS_NEXTCLOUD_FILES, "has-preview").as_deref(), Some("true"));
    // The web UI's registered extras: an unshared file has no sharees/types,
    // no note/hide-download and no share attributes; the properties are still
    // served (200 for the empty ones, 404 for the null ones).
    assert!(prop_of(node, NS_OWNCLOUD, "share-types").is_some());
    assert!(prop_of(node, NS_OWNCLOUD, "share-types").unwrap().children.is_empty());
    assert!(prop_of(node, NS_NEXTCLOUD_FILES, "sharees").is_some());
    assert_eq!(
        prop_text(node, NS_NEXTCLOUD_FILES, "share-attributes").as_deref(),
        Some("[]")
    );
    assert_eq!(
        prop_text(node, NS_OCS, "share-permissions").as_deref(),
        Some("19")
    );
    // No server handler exists for `nc:is-encrypted`, so PHP 404s it.
    assert_eq!(
        status_of(node, NS_NEXTCLOUD_FILES, "is-encrypted").as_deref(),
        Some("HTTP/1.1 404 Not Found")
    );
    assert_eq!(
        status_of(node, NS_NEXTCLOUD_FILES, "note").as_deref(),
        Some("HTTP/1.1 404 Not Found")
    );
    assert_eq!(
        status_of(node, NS_NEXTCLOUD_FILES, "hide-download").as_deref(),
        Some("HTTP/1.1 404 Not Found")
    );
    // File-only / directory-only semantics: the quota property lands in the
    // 404 propstat for a file (it is implemented, just not applicable).
    assert_eq!(
        status_of(node, NS_DAV, "quota-available-bytes").as_deref(),
        Some("HTTP/1.1 404 Not Found")
    );
    // `resourcetype` is empty for a file.
    assert!(prop_of(node, NS_DAV, "resourcetype").is_some());
    assert!(prop_of(node, NS_DAV, "resourcetype").unwrap().children.is_empty());
}

/// The desktop client's set must be served natively too: `oc:downloadURL` is
/// empty for a local-storage file, `oc:dDC` is a PHP 404, `oc:checksums`
/// carries the stored checksum, and `oc:data-fingerprint` is the config value.
#[tokio::test]
async fn desktop_client_property_set_is_served_natively() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };

    let body = prop_body(DESKTOP_PROPS);
    let resp = propfind(&f.app, &format!("{FILES}/Notes.txt"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let node = response(&doc, &format!("{FILES}/Notes.txt")).unwrap();
    // `oc:id`, not `oc:fileid`, is the id the desktop client reads.
    assert_eq!(
        prop_text(node, NS_OWNCLOUD, "id").as_deref(),
        Some(format!("{:08}testinst", f.notes).as_str())
    );
    // A local home storage returns `false` for the direct download URL.
    assert_eq!(prop_text(node, NS_OWNCLOUD, "downloadURL").as_deref(), Some(""));
    // `oc:dDC` is disallowed by `CustomPropertiesBackend`; PHP 404s it.
    assert_eq!(
        status_of(node, NS_OWNCLOUD, "dDC").as_deref(),
        Some("HTTP/1.1 404 Not Found")
    );
    // `oc:checksums` emits one `<oc:checksum>` child.
    let checksums = prop_of(node, NS_OWNCLOUD, "checksums").expect("checksums");
    assert_eq!(checksums.children.len(), 1);
    assert_eq!(checksums.children[0].text, "SHA1:deadbeef");
    // The desktop root set adds `oc:data-fingerprint`; PHP serves it for every
    // node (here an empty system-config default).
    let root_body = prop_body(DESKTOP_ROOT_PROPS);
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", &root_body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/")).unwrap();
    assert_eq!(
        prop_text(root, NS_OWNCLOUD, "data-fingerprint").as_deref(),
        Some("")
    );
}

#[tokio::test]
async fn propfind_depth1_lists_parent_and_children() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };

    let body = prop_body(
        "<d:getetag/><d:resourcetype/><oc:permissions/><oc:fileid/><d:displayname/>",
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
            format!("{FILES}/Documents/"),
            format!("{FILES}/Notes.txt"),
        ]
    );

    // The home root's displayname is the principal name (`FilesHome::getName`).
    let root = response(&doc, &format!("{FILES}/")).unwrap();
    assert_eq!(prop_text(root, NS_DAV, "displayname").as_deref(), Some("alice"));
    assert_eq!(prop_text(root, NS_OWNCLOUD, "permissions").as_deref(), Some("RGDNVCK"));

    // A directory is a collection, with the directory letter string.
    let docs = response(&doc, &format!("{FILES}/Documents/")).unwrap();
    assert!(!prop_of(docs, NS_DAV, "resourcetype").unwrap().children.is_empty());
    assert_eq!(prop_text(docs, NS_OWNCLOUD, "permissions").as_deref(), Some("RGDNVCK"));
    assert_eq!(
        prop_text(docs, NS_OWNCLOUD, "fileid").as_deref(),
        Some(f.docs.to_string().as_str())
    );

    // Depth 0 on the same collection lists only itself.
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", &body).await;
    let doc = parse_document(&resp.body).unwrap();
    assert_eq!(responses(&doc).len(), 1);
    assert!(response(&doc, &format!("{FILES}/")).is_some());

    // Depth 1 on a file lists only the file.
    let resp = propfind(
        &f.app,
        &format!("{FILES}/Notes.txt"),
        USER,
        PASSWORD,
        "1",
        &body,
    )
    .await;
    let doc = parse_document(&resp.body).unwrap();
    assert_eq!(responses(&doc).len(), 1);

    // An absent Depth header defaults to 1 (`getHTTPDepth(1)`).
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "", &body).await;
    let doc = parse_document(&resp.body).unwrap();
    assert_eq!(responses(&doc).len(), 3);

    // A child of a subdirectory resolves too.
    let resp = propfind(
        &f.app,
        &format!("{FILES}/Documents"),
        USER,
        PASSWORD,
        "1",
        &body,
    )
    .await;
    let doc = parse_document(&resp.body).unwrap();
    let hrefs: Vec<String> = responses(&doc)
        .iter()
        .filter_map(|node| node.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect();
    assert_eq!(
        hrefs,
        vec![
            format!("{FILES}/Documents/"),
            format!("{FILES}/Documents/Child.txt"),
        ]
    );
}

#[tokio::test]
async fn allprop_and_propname_use_sabres_fixed_list() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };

    // Empty body == allprop; the 404s are stripped.
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", "").await;
    assert_eq!(resp.status, 207);
    let doc = parse_document(&resp.body).unwrap();
    let node = response(&doc, &format!("{FILES}/")).unwrap();
    let propstats: Vec<&XNode> = node
        .children
        .iter()
        .filter(|child| child.ns == NS_DAV && child.local == "propstat")
        .collect();
    assert_eq!(propstats.len(), 1, "allprop strips the 404 propstat");
    for local in [
        "getlastmodified",
        "resourcetype",
        "quota-used-bytes",
        "quota-available-bytes",
        "getetag",
    ] {
        assert!(
            prop_of(node, NS_DAV, local).is_some(),
            "allprop is missing {local}"
        );
    }
    assert!(prop_of(node, NS_DAV, "getcontentlength").is_none());

    // `propname` is treated as `allprop` by Sabre's parser.
    let resp = propfind(
        &f.app,
        FILES,
        USER,
        PASSWORD,
        "0",
        r#"<d:propfind xmlns:d="DAV:"><d:propname/></d:propfind>"#,
    )
    .await;
    assert_eq!(resp.status, 207);
    let doc = parse_document(&resp.body).unwrap();
    let node = response(&doc, &format!("{FILES}/")).unwrap();
    assert!(prop_of(node, NS_DAV, "getetag").is_some());
}

#[tokio::test]
async fn prefer_minimal_strips_the_404_propstat() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let body = prop_body("<d:getetag/><d:getcontentlength/>");
    let req = Request::builder()
        .method("PROPFIND")
        .uri(&format!("{FILES}/Notes.txt"))
        .header(header::AUTHORIZATION, common::basic(USER, PASSWORD))
        .header("Depth", "0")
        .header("Prefer", "return=minimal")
        .body(Body::from(body))
        .unwrap();
    let resp = call(&f.app, req).await;
    assert_eq!(resp.status, 207);
    let doc = parse_document(&resp.body).unwrap();
    let node = response(&doc, &format!("{FILES}/Notes.txt")).unwrap();
    let propstats = node
        .children
        .iter()
        .filter(|child| child.ns == NS_DAV && child.local == "propstat")
        .count();
    assert_eq!(propstats, 1);
}

#[tokio::test]
async fn property_gate_delegates_to_php() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // A property PHP serves but the sidecar does not implement: 501, not 404.
    for prop in [
        "<oc:tags/>",
        "<nc:system-tags/>",
        "<nc:rich-workspace/>",
        "<oc:comments-count/>",
        "<d:owner/>",
    ] {
        let body = prop_body(prop);
        let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", &body).await;
        assert_eq!(resp.status, 501, "{prop} should delegate, got {}", resp.status);
    }
    // A single unimplemented property in an otherwise implemented set delegates
    // the whole request (it is the same propstat for every node).
    let body = prop_body("<d:getetag/><oc:tags/>");
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 501);

    // A malformed body is a 400 before anything else.
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", "<not-xml").await;
    assert_eq!(resp.status, 400);
}

#[tokio::test]
async fn mounts_delegate_in_both_directions() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // A mount below the home root: the home listing would drop it.
    f.env.seed_mount(USER, "/alice/files/Shared/").await;
    let body = prop_body("<d:getetag/>");
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 501, "a mount in the listing must delegate");

    // The mount root itself is not in the home storage's filecache.
    let resp = propfind(
        &f.app,
        &format!("{FILES}/Shared"),
        USER,
        PASSWORD,
        "0",
        &body,
    )
    .await;
    assert_eq!(resp.status, 501, "the mount root must delegate");

    // A path under a mount.
    let resp = propfind(
        &f.app,
        &format!("{FILES}/Shared/sub"),
        USER,
        PASSWORD,
        "0",
        &body,
    )
    .await;
    assert_eq!(resp.status, 501, "a path under a mount must delegate");

    // A mount deeper below the requested collection also delegates: it changes
    // the direct child's size and etag.
    f.env.seed_mount(USER, "/alice/files/Documents/Nested/").await;
    let resp = propfind(
        &f.app,
        &format!("{FILES}/Documents"),
        USER,
        PASSWORD,
        "1",
        &body,
    )
    .await;
    assert_eq!(resp.status, 501, "a nested mount must delegate");

    // A sibling of the mount is still served.
    let resp = propfind(&f.app, &format!("{FILES}/Notes.txt"), USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 207);
}

#[tokio::test]
async fn non_propfind_methods_delegate() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    for method in ["OPTIONS", "GET", "HEAD", "PUT", "DELETE", "MKCOL", "PROPPATCH", "REPORT"] {
        let resp = call(&f.app, request(method, FILES, USER, PASSWORD)).await;
        assert_eq!(resp.status, 501, "{method} should delegate");
    }
    // OPTIONS delegates even unauthenticated (PHP owns the DAV/Allow headers).
    let req = Request::builder()
        .method("OPTIONS")
        .uri(FILES)
        .body(Body::empty())
        .unwrap();
    let resp = call(&f.app, req).await;
    assert_eq!(resp.status, 501);
}

#[tokio::test]
async fn unknown_paths_are_not_found() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let body = prop_body("<d:getetag/>");
    let resp = propfind(
        &f.app,
        &format!("{FILES}/missing.txt"),
        USER,
        PASSWORD,
        "0",
        &body,
    )
    .await;
    assert_eq!(resp.status, 404);

    // Another user's home is a 404 (RootCollection hides it), not a 403.
    let resp = propfind(
        &f.app,
        "/remote.php/dav/files/bob",
        USER,
        PASSWORD,
        "0",
        &body,
    )
    .await;
    assert_eq!(resp.status, 404);

    // Unauthenticated PROPFIND is delegated, never refused with 401: the
    // request may carry a Nextcloud session cookie, which the sidecar cannot
    // evaluate, and a 401 with WWW-Authenticate makes the browser prompt.
    let req = Request::builder()
        .method("PROPFIND")
        .uri(FILES)
        .header("Depth", "0")
        .body(Body::from(body))
        .unwrap();
    let resp = call(&f.app, req).await;
    assert_eq!(resp.status, 501);
}

#[tokio::test]
async fn nfc_and_percent_encoded_paths_resolve() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // The stored path is NFC (`é` = U+00E9); a decomposed request must match it.
    f.env
        .seed_file(
            f.storage,
            "files/Café.txt",
            "Café.txt",
            "text/plain",
            3,
            1_700_000_400,
            "etagcafe",
            27,
            f.files_root,
            None,
        )
        .await;
    let body = prop_body("<d:getetag/><d:displayname/>");
    // `%C3%A9` (NFC, percent-encoded) and `e%CC%81` (decomposed) both resolve.
    for encoded in ["Caf%C3%A9.txt", "Cafe%CC%81.txt"] {
        let resp = propfind(
            &f.app,
            &format!("{FILES}/{encoded}"),
            USER,
            PASSWORD,
            "0",
            &body,
        )
        .await;
        assert_eq!(resp.status, 207, "{encoded} did not resolve");
        let doc = parse_document(&resp.body).unwrap();
        let node = responses(&doc)[0];
        assert_eq!(prop_text(node, NS_DAV, "getetag").as_deref(), Some("\"etagcafe\""));
        // The href echoes the requested encoding (lower-case hex, as Sabre).
        let href = node.child(NS_DAV, "href").unwrap().text.clone();
        assert_eq!(
            href.to_lowercase(),
            format!("{FILES}/{encoded}").to_lowercase()
        );
    }
}

#[tokio::test]
async fn large_listing_is_served_in_one_request() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let big = f
        .env
        .seed_file(
            f.storage,
            "files/big",
            "big",
            "httpd/unix-directory",
            0,
            1_700_000_500,
            "etagbig",
            31,
            f.files_root,
            None,
        )
        .await;
    let file_mime = f.env.seed_mimetype("text/plain").await;
    let file_part = f.env.seed_mimetype("text").await;
    let sql = format!(
        "INSERT INTO {}filecache \
         (storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime, \
          storage_mtime, encrypted, unencrypted_size, etag, permissions, checksum) \
         SELECT ?, 'files/big/file-' || g, md5('files/big/file-' || g), ?, 'file-' || g, \
                ?, ?, 10, 1700000000, 1700000000, 0, 0, 'e' || g, 27, NULL \
         FROM generate_series(1, 5500) g",
        f.env.prefix
    );
    sqlx::query(safe(sql))
        .bind(f.storage)
        .bind(big)
        .bind(file_mime)
        .bind(file_part)
        .execute(f.env.pool())
        .await
        .unwrap();

    let body = prop_body("<d:getetag/><oc:size/><oc:fileid/><d:displayname/>");
    let resp = propfind(&f.app, &format!("{FILES}/big"), USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();
    // The collection itself plus 5500 children.
    assert_eq!(responses(&doc).len(), 5501);
    // Spot-check one child.
    let node = response(&doc, &format!("{FILES}/big/file-5500")).expect("last child");
    assert_eq!(prop_text(node, NS_OWNCLOUD, "size").as_deref(), Some("10"));
}

/// `oc:comments-unread` must count comments newer than the reader's marker.
#[tokio::test]
async fn comments_unread_uses_one_bulk_query() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    f.env.seed_comment(f.notes, "2024-01-01 00:00:00").await;
    f.env.seed_comment(f.notes, "2024-01-02 00:00:00").await;
    f.env
        .seed_comment_marker(USER, f.notes, "2024-01-01 12:00:00")
        .await;
    f.env.seed_comment(f.notes_child, "2024-01-01 00:00:00").await;

    let body = prop_body("<oc:comments-unread/>");
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207);
    let doc = parse_document(&resp.body).unwrap();
    assert_eq!(
        prop_text(
            response(&doc, &format!("{FILES}/Notes.txt")).unwrap(),
            NS_OWNCLOUD,
            "comments-unread"
        )
        .as_deref(),
        Some("1")
    );
    assert_eq!(
        prop_text(
            response(&doc, &format!("{FILES}/")).unwrap(),
            NS_OWNCLOUD,
            "comments-unread"
        )
        .as_deref(),
        Some("0")
    );

    // A comment on a grandchild is only visible in that folder's listing, and
    // no marker means every comment is unread.
    let resp = propfind(
        &f.app,
        &format!("{FILES}/Documents"),
        USER,
        PASSWORD,
        "1",
        &body,
    )
    .await;
    let doc = parse_document(&resp.body).unwrap();
    assert_eq!(
        prop_text(
            response(&doc, &format!("{FILES}/Documents/Child.txt")).unwrap(),
            NS_OWNCLOUD,
            "comments-unread"
        )
        .as_deref(),
        Some("1")
    );
}

/// The quota properties are directories-only and computed once per request.
#[tokio::test]
async fn quota_properties_are_directory_only() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // No quota preference and no `files/default_quota` -> unlimited (-3).
    let body = prop_body("<d:quota-available-bytes/><d:quota-used-bytes/>");
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 207);
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/")).unwrap();
    assert_eq!(
        prop_text(root, NS_DAV, "quota-available-bytes").as_deref(),
        Some("-3")
    );
    assert_eq!(
        prop_text(root, NS_DAV, "quota-used-bytes").as_deref(),
        Some("100")
    );

    // A finite quota: available = quota - used_root.
    f.env
        .seed_appconfig("files", "default_quota", "1 GB")
        .await;
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", &body).await;
    let doc = parse_document(&resp.body).unwrap();
    let root = response(&doc, &format!("{FILES}/")).unwrap();
    assert_eq!(
        prop_text(root, NS_DAV, "quota-available-bytes").as_deref(),
        Some((1024 * 1024 * 1024 - 100).to_string().as_str())
    );

    // Files never get the quota properties.
    let resp = propfind(
        &f.app,
        &format!("{FILES}/Notes.txt"),
        USER,
        PASSWORD,
        "0",
        &body,
    )
    .await;
    let doc = parse_document(&resp.body).unwrap();
    let node = response(&doc, &format!("{FILES}/Notes.txt")).unwrap();
    assert_eq!(
        status_of(node, NS_DAV, "quota-used-bytes").as_deref(),
        Some("HTTP/1.1 404 Not Found")
    );
}

#[tokio::test]
async fn app_password_fast_path_is_required() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    // A stale `last_check` forces the PHP fallback; the PHP backend is
    // unreachable in tests, so the request is a 502 (never served natively).
    let sql = format!(
        "UPDATE {}authtoken SET last_check = 1000",
        f.env.prefix
    );
    sqlx::query(safe(sql)).execute(f.env.pool()).await.unwrap();
    let body = prop_body("<d:getetag/>");
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "0", &body).await;
    assert_eq!(resp.status, 502);
}

/// `d:creationdate`, `nc:metadata-*` and `nc:hidden` come from the
/// `oc_filecache_extended` / `oc_files_metadata` joins, not extra queries.
#[tokio::test]
async fn creationdate_metadata_and_hidden_come_from_the_joins() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    f.env.seed_extended(f.notes, 1_700_000_000).await;
    f.env
        .seed_metadata(
            f.notes,
            r#"{"blurhash":{"value":"L0TSUAWBWB","type":"string"},"files-live-photo":{"value":"42","type":"string"}}"#,
        )
        .await;
    // A `.mov` carrying the live-photo metadata is `nc:hidden=true`.
    let mov = f
        .env
        .seed_file(
            f.storage,
            "files/Live.mov",
            "Live.mov",
            "video/quicktime",
            99,
            1_700_000_100,
            "etagmov",
            27,
            f.files_root,
            None,
        )
        .await;
    f.env
        .seed_metadata(mov, r#"{"files-live-photo":{"value":"42","type":"string"}}"#)
        .await;

    let body = prop_body(WEB_PROPS);
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();

    let notes = response(&doc, &format!("{FILES}/Notes.txt")).unwrap();
    assert_eq!(
        prop_text(notes, NS_DAV, "creationdate").as_deref(),
        Some("2023-11-14T22:13:20+00:00")
    );
    assert_eq!(
        prop_text(notes, NS_NEXTCLOUD_FILES, "metadata-blurhash").as_deref(),
        Some("L0TSUAWBWB")
    );
    assert_eq!(
        prop_text(notes, NS_NEXTCLOUD_FILES, "metadata-files-live-photo").as_deref(),
        Some("42")
    );
    // `nc:hidden` needs both the metadata and the `video/quicktime` mimetype.
    assert_eq!(
        prop_text(notes, NS_NEXTCLOUD_FILES, "hidden").as_deref(),
        Some("false")
    );

    let mov = response(&doc, &format!("{FILES}/Live.mov")).unwrap();
    assert_eq!(
        prop_text(mov, NS_NEXTCLOUD_FILES, "hidden").as_deref(),
        Some("true")
    );
    // A file with no extended/metadata rows still serves the properties:
    // `creation_time = 0`, metadata absent -> 404.
    let docs = response(&doc, &format!("{FILES}/Documents/")).unwrap();
    assert_eq!(
        prop_text(docs, NS_DAV, "creationdate").as_deref(),
        Some("1970-01-01T00:00:00+00:00")
    );
    assert_eq!(
        status_of(docs, NS_NEXTCLOUD_FILES, "metadata-blurhash").as_deref(),
        Some("HTTP/1.1 404 Not Found")
    );
}

/// `oc:share-types` / `nc:sharees` are one bulk `oc_share` query for the whole
/// folder, keyed by fileid, with the sharee display names joined in.
#[tokio::test]
async fn share_properties_come_from_one_bulk_query() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    f.env.seed_user("bob", Some("Bob B")).await;
    f.env.seed_group_with_displayname("team", "The Team").await;
    // The caller shared Notes.txt with a user and a group.
    f.env
        .seed_file_share(0, Some("bob"), USER, USER, f.notes, 19)
        .await;
    f.env
        .seed_file_share(1, Some("team"), USER, USER, f.notes, 19)
        .await;

    let body = prop_body(WEB_PROPS);
    let resp = propfind(&f.app, FILES, USER, PASSWORD, "1", &body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let doc = parse_document(&resp.body).unwrap();

    let notes = response(&doc, &format!("{FILES}/Notes.txt")).unwrap();
    let types: Vec<String> = prop_of(notes, NS_OWNCLOUD, "share-types")
        .unwrap()
        .children
        .iter()
        .map(|c| c.text.clone())
        .collect();
    assert_eq!(types, vec!["0", "1"]);

    let sharees = prop_of(notes, NS_NEXTCLOUD_FILES, "sharees").unwrap();
    let mut ids: Vec<(String, String, String)> = sharees
        .children
        .iter()
        .map(|sharee| {
            (
                sharee
                    .children
                    .iter()
                    .find(|c| c.local == "id")
                    .unwrap()
                    .text
                    .clone(),
                sharee
                    .children
                    .iter()
                    .find(|c| c.local == "display-name")
                    .unwrap()
                    .text
                    .clone(),
                sharee
                    .children
                    .iter()
                    .find(|c| c.local == "type")
                    .unwrap()
                    .text
                    .clone(),
            )
        })
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            ("bob".to_string(), "Bob B".to_string(), "0".to_string()),
            ("team".to_string(), "The Team".to_string(), "1".to_string()),
        ]
    );

    // A sibling with no shares serves the properties empty.
    let sibling = response(&doc, &format!("{FILES}/Documents/")).unwrap();
    assert!(prop_of(sibling, NS_OWNCLOUD, "share-types")
        .unwrap()
        .children
        .is_empty());
    assert!(prop_of(sibling, NS_NEXTCLOUD_FILES, "sharees")
        .unwrap()
        .children
        .is_empty());

    // `ocs:share-permissions` for an unshared directory is its permissions.
    assert_eq!(
        prop_text(sibling, NS_OCS, "share-permissions").as_deref(),
        Some("31")
    );
}
