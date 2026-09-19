// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration tests for the DAV **discovery** surface: `PROPFIND` Depth 0 on
//! `/remote.php/dav/` and on the caller's own principal.
//!
//! The fixtures assert the exact live PHP shape (captured 2026-09-19) and the
//! delegation rules: another principal, an unimplemented property, a non-zero
//! Depth and every non-`PROPFIND` method (OPTIONS included) answer `501`.

mod common;

use common::{call, propfind, request, Resp, TestEnv};
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{NS_CALDAV, NS_CARDDAV, NS_DAV, NS_NEXTCLOUD, NS_SABREDAV};

const USER: &str = "alice";
const PASSWORD: &str = "app-password";
const ROOT: &str = "/remote.php/dav/";
const PRINCIPAL: &str = "/remote.php/dav/principals/users/alice/";

fn doc(body: &[u8]) -> XNode {
    parse_document(body).unwrap()
}

fn responses(d: &XNode) -> Vec<&XNode> {
    d.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "response")
        .collect()
}

fn response<'a>(d: &'a XNode, href: &str) -> Option<&'a XNode> {
    responses(d).into_iter().find(|c| {
        c.child(NS_DAV, "href")
            .map(|h| h.text == href)
            .unwrap_or(false)
    })
}

fn prop_of<'a>(resp: &'a XNode, ns: &str, local: &str) -> Option<&'a XNode> {
    resp.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "propstat")
        .filter_map(|ps| ps.child(NS_DAV, "prop"))
        .flat_map(|prop| prop.children.iter())
        .find(|c| c.ns == ns && c.local == local)
}

fn prop_text(resp: &XNode, ns: &str, local: &str) -> Option<String> {
    prop_of(resp, ns, local).map(|n| n.text.clone())
}

fn hrefs(node: &XNode) -> Vec<String> {
    node.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "href")
        .map(|c| c.text.clone())
        .collect()
}

fn report_names(resp: &XNode) -> Vec<String> {
    prop_of(resp, NS_DAV, "supported-report-set")
        .map(|set| {
            set.children
                .iter()
                .filter_map(|sr| sr.child(NS_DAV, "report"))
                .filter_map(|report| report.children.first())
                .map(|c| format!("{{{}}}{}", c.ns, c.local))
                .collect()
        })
        .unwrap_or_default()
}

fn privilege_names(resp: &XNode) -> Vec<String> {
    prop_of(resp, NS_DAV, "current-user-privilege-set")
        .map(|set| {
            set.children
                .iter()
                .filter_map(|p| p.children.first())
                .map(|c| format!("{{{}}}{}", c.ns, c.local))
                .collect()
        })
        .unwrap_or_default()
}

/// A user with a language, a display name, a primary email, an additional
/// email, a group membership and an app-password token.
async fn fixture() -> Option<TestEnv> {
    let env = TestEnv::new().await?;
    env.seed_user(USER, Some("Alice A")).await;
    env.seed_user("bob", Some("Bob B")).await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;
    env.seed_preference(USER, "core", "lang", "fr").await;
    // The mixed-case address proves the `getSystemEMailAddress()` lowercasing.
    env.seed_preference(USER, "settings", "email", "Alice@Example.com")
        .await;
    env.seed_account(
        USER,
        r#"{"email":{"value":"alice@example.com","scope":"private"},"additional_mail":[{"value":"alt@example.com","scope":"private"}]}"#,
    )
    .await;
    env.seed_group("admin").await;
    env.seed_group_member("admin", USER).await;
    Some(env)
}

fn root_body() -> String {
    r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:nc="http://nextcloud.com/ns" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:current-user-principal/><d:principal-collection-set/><d:resourcetype/>
    <d:supported-report-set/><d:current-user-privilege-set/>
  </d:prop>
</d:propfind>"#
        .to_string()
}

fn principal_body() -> String {
    r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:nc="http://nextcloud.com/ns" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:principal-URL/><d:displayname/><d:resourcetype/><d:current-user-principal/>
    <d:principal-collection-set/><d:supported-report-set/><d:current-user-privilege-set/>
    <d:owner/><d:alternate-URI-set/><d:group-membership/>
    <card:addressbook-home-set/><cal:calendar-home-set/><cal:calendar-user-address-set/>
    <cal:calendar-user-type/><nc:language/><s:email-address/>
  </d:prop>
</d:propfind>"#
        .to_string()
}

#[tokio::test]
async fn root_depth0_serves_the_discovery_properties() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    let resp = propfind(&app, ROOT, USER, PASSWORD, "0", &root_body()).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let d = doc(&resp.body);
    let r = response(&d, ROOT).expect("root response");
    assert!(
        prop_text(r, NS_DAV, "resourcetype").is_some(),
        "resourcetype must be present"
    );
    assert_eq!(
        hrefs(prop_of(r, NS_DAV, "current-user-principal").unwrap()),
        vec![PRINCIPAL.to_string()]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_DAV, "principal-collection-set").unwrap()),
        vec![
            "/remote.php/dav/principals/users/".to_string(),
            "/remote.php/dav/principals/groups/".to_string(),
            "/remote.php/dav/principals/calendar-resources/".to_string(),
            "/remote.php/dav/principals/calendar-rooms/".to_string(),
        ]
    );
    assert_eq!(
        report_names(r),
        vec![
            "{DAV:}expand-property",
            "{DAV:}principal-match",
            "{DAV:}principal-property-search",
            "{DAV:}principal-search-property-set",
            "{http://owncloud.org/ns}filter-comments",
            "{http://owncloud.org/ns}filter-files",
        ]
    );
    assert_eq!(
        privilege_names(r),
        vec![
            "{DAV:}all",
            "{DAV:}read",
            "{DAV:}write",
            "{DAV:}write-properties",
            "{DAV:}write-content",
            "{DAV:}unlock",
            "{DAV:}bind",
            "{DAV:}unbind",
            "{DAV:}read-acl",
            "{DAV:}read-current-user-privilege-set",
        ]
    );
    // The root does not serve the principal-only properties.
    assert!(prop_of(r, NS_DAV, "displayname").is_none());
    assert!(prop_of(r, NS_NEXTCLOUD, "language").is_none());
}

#[tokio::test]
async fn principal_depth0_serves_the_discovery_properties() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    let resp = propfind(&app, PRINCIPAL, USER, PASSWORD, "0", &principal_body()).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let d = doc(&resp.body);
    let r = response(&d, PRINCIPAL).expect("principal response");

    let resourcetype = prop_of(r, NS_DAV, "resourcetype").unwrap();
    assert_eq!(
        resourcetype
            .children
            .iter()
            .map(|c| c.local.clone())
            .collect::<Vec<_>>(),
        vec!["collection".to_string(), "principal".to_string()]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_DAV, "principal-URL").unwrap()),
        vec![PRINCIPAL.to_string()]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_DAV, "current-user-principal").unwrap()),
        vec![PRINCIPAL.to_string()]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_DAV, "owner").unwrap()),
        vec![PRINCIPAL.to_string()]
    );
    assert_eq!(
        prop_text(r, NS_DAV, "displayname").as_deref(),
        Some("Alice A")
    );
    assert_eq!(
        hrefs(prop_of(r, NS_DAV, "group-membership").unwrap()),
        vec!["/remote.php/dav/principals/groups/admin/".to_string()]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_CARDDAV, "addressbook-home-set").unwrap()),
        vec!["/remote.php/dav/addressbooks/users/alice/".to_string()]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_CALDAV, "calendar-home-set").unwrap()),
        vec!["/remote.php/dav/calendars/alice/".to_string()]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_DAV, "alternate-URI-set").unwrap()),
        vec![
            "mailto:alt@example.com".to_string(),
            "mailto:alice@example.com".to_string(),
        ]
    );
    assert_eq!(
        hrefs(prop_of(r, NS_CALDAV, "calendar-user-address-set").unwrap()),
        vec![
            "mailto:alt@example.com".to_string(),
            "mailto:alice@example.com".to_string(),
            PRINCIPAL.to_string(),
        ]
    );
    assert_eq!(
        prop_text(r, NS_CALDAV, "calendar-user-type").as_deref(),
        Some("INDIVIDUAL")
    );
    assert_eq!(prop_text(r, NS_NEXTCLOUD, "language").as_deref(), Some("fr"));
    assert_eq!(
        prop_text(r, NS_SABREDAV, "email-address").as_deref(),
        Some("alice@example.com")
    );
    assert_eq!(
        privilege_names(r),
        vec![
            "{DAV:}read",
            "{DAV:}read-acl",
            "{DAV:}read-current-user-privilege-set",
            "{DAV:}all",
            "{DAV:}write",
            "{DAV:}write-properties",
            "{DAV:}write-content",
            "{DAV:}unlock",
            "{DAV:}bind",
            "{DAV:}unbind",
            "{DAV:}write-acl",
        ]
    );
    assert_eq!(
        report_names(r),
        vec![
            "{DAV:}expand-property",
            "{DAV:}principal-match",
            "{DAV:}principal-property-search",
            "{DAV:}principal-search-property-set",
            "{http://owncloud.org/ns}filter-comments",
            "{http://owncloud.org/ns}filter-files",
        ]
    );
}

#[tokio::test]
async fn allprop_and_propname_return_resourcetype_only() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    for (path, href) in [(ROOT, ROOT), (PRINCIPAL, PRINCIPAL)] {
        for body in [
            r#"<d:propfind xmlns:d="DAV:"><d:allprop/></d:propfind>"#,
            r#"<d:propfind xmlns:d="DAV:"><d:propname/></d:propfind>"#,
        ] {
            let resp = propfind(&app, path, USER, PASSWORD, "0", body).await;
            assert_eq!(resp.status, 207, "{path} {body}: {}", resp.text());
            let d = doc(&resp.body);
            let r = response(&d, href).unwrap();
            let props: Vec<&str> = r
                .children
                .iter()
                .filter(|c| c.ns == NS_DAV && c.local == "propstat")
                .filter_map(|ps| ps.child(NS_DAV, "prop"))
                .flat_map(|p| p.children.iter())
                .map(|p| p.local.as_str())
                .collect();
            assert_eq!(props, vec!["resourcetype"], "{path} {body}");
        }
    }
}

#[tokio::test]
async fn another_principal_delegates() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    for path in [
        "/remote.php/dav/principals/users/bob/",
        "/remote.php/dav/principals/users/bob",
    ] {
        let resp = propfind(&app, path, USER, PASSWORD, "0", &principal_body()).await;
        assert_eq!(resp.status, 501, "{path}: {}", resp.text());
    }
}

#[tokio::test]
async fn unimplemented_property_delegates() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    let body = |props: &str| {
        format!(
            r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop>{props}</d:prop></d:propfind>"#
        )
    };
    // The root does not implement `displayname`.
    let resp = propfind(&app, ROOT, USER, PASSWORD, "0", &body("<d:displayname/>")).await;
    assert_eq!(resp.status, 501);
    // The principal does not implement `getetag`.
    let resp = propfind(&app, PRINCIPAL, USER, PASSWORD, "0", &body("<d:getetag/>")).await;
    assert_eq!(resp.status, 501);
    // A mixed set delegates as a whole.
    let resp = propfind(
        &app,
        PRINCIPAL,
        USER,
        PASSWORD,
        "0",
        &body("<d:displayname/><d:getetag/>"),
    )
    .await;
    assert_eq!(resp.status, 501);
    // An implemented property is served.
    let resp = propfind(&app, PRINCIPAL, USER, PASSWORD, "0", &body("<d:displayname/>")).await;
    assert_eq!(resp.status, 207);
}

#[tokio::test]
async fn non_propfind_methods_delegate() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    for path in [ROOT, PRINCIPAL] {
        for method in [
            "OPTIONS", "GET", "HEAD", "PUT", "DELETE", "MKCOL", "PROPPATCH", "REPORT", "POST",
            "MOVE", "COPY",
        ] {
            let resp = call(&app, request(method, path, USER, PASSWORD)).await;
            assert_eq!(resp.status, 501, "{method} {path}");
        }
    }
}

#[tokio::test]
async fn depth_one_delegates() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    for path in [ROOT, PRINCIPAL] {
        let resp = propfind(&app, path, USER, PASSWORD, "1", &root_body()).await;
        assert_eq!(resp.status, 501, "{path} Depth 1: {}", resp.text());
    }
}

#[tokio::test]
async fn language_is_delegated_when_not_derivable() {
    let Some(env) = TestEnv::new().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    env.seed_user(USER, Some("Alice A")).await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.com/ns"><d:prop><nc:language/></d:prop></d:propfind>"#;
    let resp = propfind(&app, PRINCIPAL, USER, PASSWORD, "0", body).await;
    assert_eq!(resp.status, 501, "body: {}", resp.text());
}

#[tokio::test]
async fn force_language_is_served() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared_force_language("de");
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.com/ns"><d:prop><nc:language/></d:prop></d:propfind>"#;
    let resp = propfind(&app, PRINCIPAL, USER, PASSWORD, "0", body).await;
    assert_eq!(resp.status, 207, "body: {}", resp.text());
    let d = doc(&resp.body);
    let r = response(&d, PRINCIPAL).unwrap();
    assert_eq!(prop_text(r, NS_NEXTCLOUD, "language").as_deref(), Some("de"));
}

#[tokio::test]
async fn collection_listings_delegate() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    // The `/principals/` and `/principals/users/` listings (and the other
    // principal children) are PHP's; the sidecar answers 501 so nginx replays
    // them. The nginx location only ever routes the own-principal path.
    for path in [
        "/remote.php/dav/principals",
        "/remote.php/dav/principals/",
        "/remote.php/dav/principals/users/",
        "/remote.php/dav/principals/groups/admin/",
        "/remote.php/dav/principals/users/alice/calendar-proxy-read",
    ] {
        let resp = propfind(&app, path, USER, PASSWORD, "0", &root_body()).await;
        assert_eq!(resp.status, 501, "{path}: {}", resp.text());
    }
}

#[tokio::test]
async fn fallback_probe_header_delegates() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    let request = axum::http::Request::builder()
        .method("PROPFIND")
        .uri(ROOT)
        .header(
            axum::http::header::AUTHORIZATION,
            common::basic(USER, PASSWORD),
        )
        .header("Depth", "0")
        .header("x-nextcloud-dav-fallback", "1")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp: Resp = call(&app, request).await;
    assert_eq!(resp.status, 501);
}

#[tokio::test]
async fn missing_credentials_are_unauthorized() {
    let Some(env) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let app = env.app_shared();
    let request = axum::http::Request::builder()
        .method("PROPFIND")
        .uri(ROOT)
        .header("Depth", "0")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp: Resp = call(&app, request).await;
    assert_eq!(resp.status, 401);
}
