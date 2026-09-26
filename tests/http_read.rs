// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP read-path parity: PROPFIND, GET/HEAD, addressbook-multiget,
//! addressbook-query and sync-collection through the real axum router.
//!
//! The router is the production one; only the PHP fallback points at an
//! unreachable address (so any request that would delegate shows up as a
//! `502`). Tests skip when PostgreSQL is unavailable.

mod common;

use common::{call, get, md5_hex, propfind, report, request, Resp, TestEnv};
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{NS_CALENDARSERVER, NS_CARDDAV, NS_DAV, NS_NEXTCLOUD, NS_SABREDAV};

const CARD_JANE: &[u8] = b"BEGIN:VCARD\r\n\
VERSION:3.0\r\n\
UID:jane-1\r\n\
FN:Jane Doe\r\n\
N:Doe;Jane;;;\r\n\
EMAIL;TYPE=WORK:jane@example.com\r\n\
CATEGORIES:Friends\r\n\
END:VCARD\r\n";

const CARD_JOHN: &[u8] = b"BEGIN:VCARD\r\n\
VERSION:3.0\r\n\
UID:john-1\r\n\
FN:John Smith\r\n\
N:Smith;John;;;\r\n\
EMAIL;TYPE=HOME:john@example.net\r\n\
END:VCARD\r\n";

const CARD_PHOTO: &[u8] = b"BEGIN:VCARD\r\n\
VERSION:3.0\r\n\
UID:photo-1\r\n\
FN:Photo Person\r\n\
PHOTO:data:image/png;base64,AAAA\r\n\
END:VCARD\r\n";

const USER: &str = "alice";
const PASSWORD: &str = "app-password";
const BOOK_PATH: &str = "/remote.php/dav/addressbooks/users/alice/contacts";

macro_rules! setup {
    () => {
        match setup().await {
            Some(value) => value,
            None => {
                eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
                return;
            }
        }
    };
}

macro_rules! env_or_skip {
    () => {
        match TestEnv::new().await {
            Some(env) => env,
            None => {
                eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
                return;
            }
        }
    };
}

async fn setup() -> Option<(TestEnv, i64, axum::Router)> {
    let env = TestEnv::new().await?;
    env.seed_user(USER, Some("Alice A")).await;
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            Some("My contacts"),
            1,
        )
        .await;
    let jane = env.seed_card(book, "jane.vcf", CARD_JANE).await;
    env.seed_property(book, jane, "CATEGORIES", "Friends").await;
    env.seed_card(book, "john.vcf", CARD_JOHN).await;
    env.seed_card(book, "photo.vcf", CARD_PHOTO).await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    Some((env, book, app))
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn doc(body: &[u8]) -> XNode {
    parse_document(body)
        .unwrap_or_else(|e| panic!("not XML: {e}: {}", String::from_utf8_lossy(body)))
}

fn response<'a>(doc: &'a XNode, href: &str) -> &'a XNode {
    doc.children
        .iter()
        .find(|c| {
            c.ns == NS_DAV
                && c.local == "response"
                && c.child(NS_DAV, "href")
                    .map(|h| h.text == href)
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no d:response for href {href}"))
}

fn prop_of<'a>(resp: &'a XNode, ns: &str, local: &str) -> Option<&'a XNode> {
    resp.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "propstat")
        .filter_map(|ps| ps.child(NS_DAV, "prop"))
        .flat_map(|prop| prop.children.iter())
        .find(|c| c.ns == ns && c.local == local)
}

fn prop_text(resp: &XNode, ns: &str, local: &str) -> String {
    prop_of(resp, ns, local)
        .map(|n| n.text.clone())
        .unwrap_or_else(|| panic!("missing prop {{{ns}}}{local}"))
}

fn prop_status(resp: &XNode, ns: &str, local: &str) -> Option<String> {
    resp.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "propstat")
        .find(|ps| {
            ps.child(NS_DAV, "prop")
                .map(|p| p.children.iter().any(|c| c.ns == ns && c.local == local))
                .unwrap_or(false)
        })
        .and_then(|ps| ps.child(NS_DAV, "status"))
        .map(|s| s.text.clone())
}

fn prop_has_element(
    resp: &XNode,
    ns: &str,
    local: &str,
    child_ns: &str,
    child_local: &str,
) -> bool {
    prop_of(resp, ns, local)
        .map(|n| {
            n.children
                .iter()
                .any(|c| c.ns == child_ns && c.local == child_local)
        })
        .unwrap_or(false)
}

fn sync_token_of(doc: &XNode) -> Option<String> {
    doc.children
        .iter()
        .find(|c| c.ns == NS_DAV && c.local == "sync-token")
        .map(|c| c.text.clone())
}

fn response_status(resp: &XNode) -> Option<String> {
    resp.child(NS_DAV, "status").map(|s| s.text.clone())
}

const ALL_BOOK_PROPS: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"
            xmlns:cs="http://calendarserver.org/ns/" xmlns:s="http://sabredav.org/ns"
            xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns">
  <d:prop>
    <d:resourcetype/><d:displayname/><card:addressbook-description/>
    <cs:getctag/><s:sync-token/><d:sync-token/><d:supported-report-set/>
    <card:max-resource-size/><card:supported-address-data/><card:supported-collation-set/>
    <d:owner/><d:current-user-privilege-set/><oc:groups/><nc:owner-displayname/>
  </d:prop>
</d:propfind>"#;

const ALL_CARD_PROPS: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:nc="http://nextcloud.com/ns">
  <d:prop>
    <d:resourcetype/><d:getetag/><d:getcontentlength/><d:getlastmodified/>
    <d:getcontenttype/><card:address-data/><nc:has-photo/>
  </d:prop>
</d:propfind>"#;

// ---------------------------------------------------------------------------
// PROPFIND
// ---------------------------------------------------------------------------

#[tokio::test]
async fn propfind_book_depth0_has_all_expected_properties() {
    let (_env, _book, app) = setup!();
    let resp = propfind(&app, BOOK_PATH, USER, PASSWORD, "0", ALL_BOOK_PROPS).await;
    assert_eq!(resp.status, 207);
    let doc = doc(&resp.body);
    // Collections are emitted with a trailing slash, exactly like Sabre.
    let r = response(&doc, "/remote.php/dav/addressbooks/users/alice/contacts/");

    assert!(prop_has_element(
        r,
        NS_DAV,
        "resourcetype",
        NS_DAV,
        "collection"
    ));
    assert!(prop_has_element(
        r,
        NS_DAV,
        "resourcetype",
        NS_CARDDAV,
        "addressbook"
    ));
    assert_eq!(prop_text(r, NS_DAV, "displayname"), "Contacts");
    assert_eq!(
        prop_text(r, NS_CARDDAV, "addressbook-description"),
        "My contacts"
    );
    assert_eq!(prop_text(r, NS_CALENDARSERVER, "getctag"), "4");
    assert_eq!(prop_text(r, NS_SABREDAV, "sync-token"), "4");
    assert_eq!(
        prop_text(r, NS_DAV, "sync-token"),
        "http://sabre.io/ns/sync/4"
    );
    // Sabre's advertised property is 10 MB; the write limit (card_size_limit)
    // is the separate 5 MiB value.
    assert_eq!(prop_text(r, NS_CARDDAV, "max-resource-size"), "10000000");
    assert_eq!(prop_text(r, NS_NEXTCLOUD, "owner-displayname"), "Alice A");
    // owner href points at the principal, with a trailing slash.
    let owner = prop_of(r, NS_DAV, "owner").unwrap();
    assert_eq!(
        owner.child(NS_DAV, "href").unwrap().text,
        "/remote.php/dav/principals/users/alice/"
    );
    // supported-address-data advertises text/vcard 3.0, text/vcard 4.0 and
    // application/vcard+json 4.0, like Sabre.
    let sad = prop_of(r, NS_CARDDAV, "supported-address-data").unwrap();
    let types: Vec<(String, String)> = sad
        .children
        .iter()
        .filter(|c| c.ns == NS_CARDDAV && c.local == "address-data-type")
        .map(|c| {
            (
                c.attr("content-type").unwrap_or_default().to_string(),
                c.attr("version").unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        types,
        vec![
            ("text/vcard".to_string(), "3.0".to_string()),
            ("text/vcard".to_string(), "4.0".to_string()),
            ("application/vcard+json".to_string(), "4.0".to_string()),
        ]
    );
    // supported-collation-set uses the `card:supported-collation` child name.
    let scs = prop_of(r, NS_CARDDAV, "supported-collation-set").unwrap();
    assert_eq!(
        scs.children
            .iter()
            .filter(|c| c.ns == NS_CARDDAV && c.local == "supported-collation")
            .count(),
        3
    );
    // supported-report-set advertises the three reports.
    let reports: Vec<String> = prop_of(r, NS_DAV, "supported-report-set")
        .unwrap()
        .children
        .iter()
        .filter_map(|sr| sr.child(NS_DAV, "report"))
        .filter_map(|r| r.children.first().map(|c| c.local.clone()))
        .collect();
    assert!(reports.contains(&"addressbook-query".to_string()));
    assert!(reports.contains(&"addressbook-multiget".to_string()));
    assert!(reports.contains(&"sync-collection".to_string()));
    // Privileges contain d:read.
    let privs = prop_of(r, NS_DAV, "current-user-privilege-set").unwrap();
    assert!(privs.children.iter().any(|p| p
        .children
        .iter()
        .any(|c| c.ns == NS_DAV && c.local == "read")));
}

#[tokio::test]
async fn propfind_book_depth1_lists_cards() {
    let (_env, _book, app) = setup!();
    let resp = propfind(&app, BOOK_PATH, USER, PASSWORD, "1", ALL_BOOK_PROPS).await;
    assert_eq!(resp.status, 207);
    let doc = doc(&resp.body);
    // 1 collection + 3 cards.
    assert_eq!(
        doc.children
            .iter()
            .filter(|c| c.ns == NS_DAV && c.local == "response")
            .count(),
        4
    );
    let _ = response(&doc, "/remote.php/dav/addressbooks/users/alice/contacts/");
    response(
        &doc,
        "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf",
    );
}

#[tokio::test]
async fn propfind_card_depth0_returns_address_data_and_metadata() {
    let (_env, _book, app) = setup!();
    let path = "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf";
    let resp = propfind(&app, path, USER, PASSWORD, "0", ALL_CARD_PROPS).await;
    assert_eq!(resp.status, 207);
    let doc = doc(&resp.body);
    let r = response(&doc, path);
    assert_eq!(
        prop_text(r, NS_DAV, "getetag"),
        format!("\"{}\"", md5_hex(CARD_JANE))
    );
    assert_eq!(
        prop_text(r, NS_DAV, "getcontentlength"),
        CARD_JANE.len().to_string()
    );
    assert_eq!(
        prop_text(r, NS_DAV, "getcontenttype"),
        "text/vcard; charset=utf-8"
    );
    assert!(!prop_text(r, NS_DAV, "getlastmodified").is_empty());
    assert_eq!(
        prop_text(r, NS_CARDDAV, "address-data"),
        String::from_utf8_lossy(CARD_JANE)
    );
    assert_eq!(prop_text(r, NS_NEXTCLOUD, "has-photo"), "");
    // A card has no child elements under resourcetype.
    assert!(prop_of(r, NS_DAV, "resourcetype")
        .unwrap()
        .children
        .is_empty());
}

#[tokio::test]
async fn propfind_has_photo_is_1_for_an_image_data_uri() {
    let (_env, _book, app) = setup!();
    let path = "/remote.php/dav/addressbooks/users/alice/contacts/photo.vcf";
    let resp = propfind(&app, path, USER, PASSWORD, "0", ALL_CARD_PROPS).await;
    let doc = doc(&resp.body);
    assert_eq!(
        prop_text(response(&doc, path), NS_NEXTCLOUD, "has-photo"),
        "1"
    );
}

#[tokio::test]
async fn propfind_unknown_property_is_a_404_propstat() {
    let (_env, _book, app) = setup!();
    let body =
        r#"<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/><d:getetag/></d:prop></d:propfind>"#;
    let resp = propfind(&app, BOOK_PATH, USER, PASSWORD, "0", body).await;
    let doc = doc(&resp.body);
    let r = response(&doc, "/remote.php/dav/addressbooks/users/alice/contacts/");
    assert!(prop_status(r, NS_DAV, "displayname")
        .unwrap()
        .contains("200"));
    assert!(prop_status(r, NS_DAV, "getetag").unwrap().contains("404"));
}

#[tokio::test]
async fn propfind_home_depth1_lists_only_database_books() {
    // The sidecar's own home handler only knows `oc_addressbooks`. In
    // production the home is served by PHP, which also advertises the
    // app-generated collections (declared deviation).
    let (_env, _book, app) = setup!();
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/></d:prop></d:propfind>"#;
    let resp = propfind(
        &app,
        "/remote.php/dav/addressbooks/users/alice",
        USER,
        PASSWORD,
        "1",
        body,
    )
    .await;
    assert_eq!(resp.status, 207);
    let doc = doc(&resp.body);
    let hrefs: Vec<String> = doc
        .children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "response")
        .filter_map(|c| c.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect();
    assert!(hrefs.contains(&"/remote.php/dav/addressbooks/users/alice/".to_string()));
    assert!(hrefs.contains(&"/remote.php/dav/addressbooks/users/alice/contacts/".to_string()));
    // No z-server-generated--system / z-app-generated--contactsinteraction.
    assert!(!hrefs.iter().any(|h| h.contains("z-")));
}

// ---------------------------------------------------------------------------
// GET / HEAD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_returns_raw_body_and_strong_etag() {
    let (_env, _book, app) = setup!();
    let path = "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf";
    let resp = get(&app, path, USER, PASSWORD).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, CARD_JANE);
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("text/vcard; charset=utf-8")
    );
    let etag = resp.header("etag").unwrap();
    assert_eq!(etag, format!("\"{}\"", md5_hex(CARD_JANE)));
    // The declared deviation: the sidecar never weakens the ETag.
    assert!(!etag.starts_with("W/"));
    assert!(resp.header("last-modified").is_some());
}

#[tokio::test]
async fn get_and_propfind_agree_on_the_etag() {
    let (_env, _book, app) = setup!();
    let path = "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf";
    let get_resp = get(&app, path, USER, PASSWORD).await;
    let prop_resp = propfind(&app, path, USER, PASSWORD, "0", ALL_CARD_PROPS).await;
    let doc = doc(&prop_resp.body);
    let prop_etag = prop_text(response(&doc, path), NS_DAV, "getetag");
    assert_eq!(get_resp.header("etag").unwrap(), prop_etag);
    assert!(!prop_etag.starts_with("W/"));
}

#[tokio::test]
async fn head_has_headers_but_no_body() {
    let (_env, _book, app) = setup!();
    let path = "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf";
    let resp = call(&app, request("HEAD", path, USER, PASSWORD)).await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
    assert_eq!(
        resp.header("etag").unwrap(),
        format!("\"{}\"", md5_hex(CARD_JANE))
    );
}

// ---------------------------------------------------------------------------
// Conditional GET (Sabre `checkPreconditions` parity, quirks included)
// ---------------------------------------------------------------------------

fn conditional(method: &str, path: &str, headers: &[(&str, &str)]) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(
            axum::http::header::AUTHORIZATION,
            common::basic(USER, PASSWORD),
        );
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

const CARD_PATH: &str = "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf";

#[tokio::test]
async fn conditional_get_if_none_match_match_is_304_with_the_etag() {
    let (_env, _book, app) = setup!();
    let etag = get(&app, CARD_PATH, USER, PASSWORD).await.header("etag").unwrap();
    let resp = call(
        &app,
        conditional("GET", CARD_PATH, &[("if-none-match", &etag)]),
    )
    .await;
    assert_eq!(resp.status, 304);
    // Sabre sets the ETag header before answering 304.
    assert_eq!(resp.header("etag").as_deref(), Some(etag.as_str()));
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn conditional_get_if_none_match_miss_serves_the_body() {
    let (_env, _book, app) = setup!();
    let resp = call(
        &app,
        conditional("GET", CARD_PATH, &[("if-none-match", "\"deadbeef\"")]),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, CARD_JANE);
}

#[tokio::test]
async fn conditional_get_star_is_304_with_the_etag() {
    let (_env, _book, app) = setup!();
    let etag = get(&app, CARD_PATH, USER, PASSWORD).await.header("etag").unwrap();
    // RFC 7232 §4.1: the 304 carries the validators. (PHP would omit the
    // ETag here — declared divergence, `conditional-get-missing`.)
    let resp = call(
        &app,
        conditional("GET", CARD_PATH, &[("if-none-match", "*")]),
    )
    .await;
    assert_eq!(resp.status, 304);
    assert_eq!(resp.header("etag").as_deref(), Some(etag.as_str()));
    assert!(resp.header("last-modified").is_some());
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn conditional_get_weak_compares_etags_per_rfc7232() {
    let (_env, _book, app) = setup!();
    let etag = get(&app, CARD_PATH, USER, PASSWORD).await.header("etag").unwrap();
    let weak = format!("W/{etag}");
    // RFC 7232 §3.2: If-None-Match uses *weak* comparison, so the weak form
    // of the current tag is a match and the body is not re-sent. (PHP compares
    // raw strings and would answer 200 — declared divergence.)
    let resp = call(
        &app,
        conditional("GET", CARD_PATH, &[("if-none-match", &weak)]),
    )
    .await;
    assert_eq!(resp.status, 304);
}

#[tokio::test]
async fn conditional_get_if_modified_since_at_mtime_is_304() {
    let (_env, _book, app) = setup!();
    let get_resp = get(&app, CARD_PATH, USER, PASSWORD).await;
    let last_modified = get_resp.header("last-modified").unwrap();
    let resp = call(
        &app,
        conditional(
            "GET",
            CARD_PATH,
            &[("if-modified-since", &last_modified)],
        ),
    )
    .await;
    assert_eq!(resp.status, 304);
    // Sabre's IMS branch sets Last-Modified (and not ETag) on the 304.
    assert_eq!(
        resp.header("last-modified").as_deref(),
        Some(last_modified.as_str())
    );
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn conditional_get_if_none_match_takes_precedence_over_if_modified_since() {
    let (_env, _book, app) = setup!();
    let get_resp = get(&app, CARD_PATH, USER, PASSWORD).await;
    let last_modified = get_resp.header("last-modified").unwrap();
    // If-None-Match present and not matching: the request is served even
    // though the IMS date says "not modified" (IMS must be ignored).
    let resp = call(
        &app,
        conditional(
            "GET",
            CARD_PATH,
            &[
                ("if-none-match", "\"deadbeef\""),
                ("if-modified-since", &last_modified),
            ],
        ),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, CARD_JANE);
}

#[tokio::test]
async fn conditional_head_with_matching_if_none_match_is_304_per_rfc7232() {
    let (_env, _book, app) = setup!();
    let etag = get(&app, CARD_PATH, USER, PASSWORD).await.header("etag").unwrap();
    // RFC 7232 §6: HEAD follows GET semantics. PHP would answer 412 here
    // (`'GET' === $method` is tested before `httpHead()` rewrites the method)
    // — declared divergence, `conditional-get-missing`.
    let resp = call(
        &app,
        conditional("HEAD", CARD_PATH, &[("if-none-match", &etag)]),
    )
    .await;
    assert_eq!(resp.status, 304);
    assert_eq!(resp.header("etag").as_deref(), Some(etag.as_str()));
}

#[tokio::test]
async fn conditional_get_if_unmodified_since_stale_is_412() {
    let (_env, _book, app) = setup!();
    let resp = call(
        &app,
        conditional(
            "GET",
            CARD_PATH,
            &[("if-unmodified-since", "Sun, 06 Nov 1994 08:49:37 GMT")],
        ),
    )
    .await;
    assert_eq!(resp.status, 412);
    // Sabre sets the ETag only for a failed If-Match, not here.
    assert_eq!(resp.header("etag"), None);
}

#[tokio::test]
async fn conditional_on_a_missing_card_is_412_for_if_match_and_404_otherwise() {
    let (_env, _book, app) = setup!();
    let missing = "/remote.php/dav/addressbooks/users/alice/contacts/missing.vcf";
    // `checkPreconditions()` resolves the node inside the If-Match branch and
    // turns NotFound into a PreconditionFailed before the 404 dispatch.
    let resp = call(&app, conditional("GET", missing, &[("if-match", "*")])).await;
    assert_eq!(resp.status, 412);
    let resp = call(
        &app,
        conditional("GET", missing, &[("if-none-match", "*")]),
    )
    .await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn get_strips_non_image_photo_data() {
    let (env, book, app) = setup!();
    let raw = b"BEGIN:VCARD\r\nUID:1\r\nPHOTO:data:text/plain;base64,AAAA\r\n AAAA\r\nFN:X\r\nEND:VCARD\r\n";
    env.seed_card(book, "nophoto.vcf", raw).await;
    let path = "/remote.php/dav/addressbooks/users/alice/contacts/nophoto.vcf";
    let resp = get(&app, path, USER, PASSWORD).await;
    assert_eq!(
        resp.body,
        b"BEGIN:VCARD\r\nUID:1\r\nFN:X\r\nEND:VCARD\r\n".to_vec()
    );
    // ETag is the stored md5 of the *unfiltered* body, like PHP.
    assert_eq!(
        resp.header("etag").unwrap(),
        format!("\"{}\"", md5_hex(raw))
    );
}

// ---------------------------------------------------------------------------
// address-data negotiation (Sabre `convertVCard` parity + declared divergences)
// ---------------------------------------------------------------------------

fn multiget_body(address_data_attrs: &str, prop_filter: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><card:address-data{address_data_attrs}>{prop_filter}</card:address-data></d:prop>
  <d:href>/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf</d:href>
</card:addressbook-multiget>"#
    )
}

#[tokio::test]
async fn multiget_plain_address_data_is_verbatim() {
    let (_env, _book, app) = setup!();
    let resp = report(
        &app,
        BOOK_PATH,
        USER,
        PASSWORD,
        &multiget_body("", ""),
    )
    .await;
    let d = doc(&resp.body);
    let data = prop_text(response(&d, CARD_PATH), NS_CARDDAV, "address-data");
    assert_eq!(data.as_bytes(), CARD_JANE);
}

#[tokio::test]
async fn multiget_version_40_is_converted() {
    let (_env, _book, app) = setup!();
    let resp = report(
        &app,
        BOOK_PATH,
        USER,
        PASSWORD,
        &multiget_body(r#" version="4.0""#, ""),
    )
    .await;
    let d = doc(&resp.body);
    let data = prop_text(response(&d, CARD_PATH), NS_CARDDAV, "address-data");
    assert!(data.starts_with("BEGIN:VCARD\r\nVERSION:4.0\r\n"), "{data}");
    assert!(
        data.contains("PRODID:-//Sabre//Sabre VObject 4.5.6//EN"),
        "{data}"
    );
}

#[tokio::test]
async fn multiget_applies_the_prop_filter_divergence() {
    let (_env, _book, app) = setup!();
    // Sabre ignores the filter in addressbook-multiget (it calls convertVCard
    // with two arguments); the sidecar applies it in both reports — declared
    // divergence `vcard-version-negotiation-missing`.
    let resp = report(
        &app,
        BOOK_PATH,
        USER,
        PASSWORD,
        &multiget_body("", r#"<card:prop name="EMAIL"/>"#),
    )
    .await;
    let d = doc(&resp.body);
    let data = prop_text(response(&d, CARD_PATH), NS_CARDDAV, "address-data");
    assert!(data.contains("EMAIL;TYPE=WORK:jane@example.com"), "{data}");
    assert!(!data.contains("N:Doe"), "{data}");
    // UID/VERSION/FN always survive the filter.
    assert!(data.contains("FN:Jane Doe"), "{data}");
    assert!(data.contains("UID:jane-1"), "{data}");
}

#[tokio::test]
async fn query_address_data_jcard_is_json() {
    let (_env, _book, app) = setup!();
    let body = r#"<?xml version="1.0"?>
<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop><card:address-data content-type="application/vcard+json"/></d:prop>
</card:addressbook-query>"#;
    // addressbook-query at Depth 0 is a 415 (`query-depth0-on-collection`),
    // so the request carries the Depth every real client sends.
    let request = axum::http::Request::builder()
        .method("REPORT")
        .uri(BOOK_PATH)
        .header(
            axum::http::header::AUTHORIZATION,
            common::basic(USER, PASSWORD),
        )
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )
        .header("depth", "1")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let resp = call(&app, request).await;
    let d = doc(&resp.body);
    let data = prop_text(response(&d, CARD_PATH), NS_CARDDAV, "address-data");
    assert!(data.starts_with("[\"vcard\",[["), "{data}");
    assert!(data.contains("[\"fn\""), "{data}");
}

#[tokio::test]
async fn report_on_an_unparseable_card_is_500() {
    let (env, book, app) = setup!();
    env.seed_card(book, "broken.vcf", b"not a vcard at all").await;
    let body = r#"<?xml version="1.0"?>
<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop><card:address-data/></d:prop>
  <d:href>/remote.php/dav/addressbooks/users/alice/contacts/broken.vcf</d:href>
</card:addressbook-multiget>"#;
    let resp = report(&app, BOOK_PATH, USER, PASSWORD, body).await;
    // PHP lets Sabre\VObject\ParseException escape to Server::start(): HTTP
    // 500 with the exception name in the error body.
    assert_eq!(resp.status, 500);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(text.contains("Sabre\\VObject\\ParseException"), "{text}");
}

// ---------------------------------------------------------------------------
// addressbook-multiget / addressbook-query
// ---------------------------------------------------------------------------

const MULTIGET: &str = r#"<?xml version="1.0"?>
<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><card:address-data/></d:prop>
  <d:href>/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf</d:href>
  <d:href>/remote.php/dav/addressbooks/users/alice/contacts/missing.vcf</d:href>
</card:addressbook-multiget>"#;

#[tokio::test]
async fn multiget_hit_and_miss_propstat() {
    let (_env, _book, app) = setup!();
    let resp = report(&app, BOOK_PATH, USER, PASSWORD, MULTIGET).await;
    assert_eq!(resp.status, 207);
    let doc = doc(&resp.body);
    let hit = response(
        &doc,
        "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf",
    );
    assert_eq!(
        prop_text(hit, NS_CARDDAV, "address-data"),
        String::from_utf8_lossy(CARD_JANE)
    );
    let miss = response(
        &doc,
        "/remote.php/dav/addressbooks/users/alice/contacts/missing.vcf",
    );
    assert!(prop_status(miss, NS_DAV, "getetag")
        .unwrap()
        .contains("404"));
    assert!(response_status(miss).is_none());
}

fn query_body(inner: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><card:address-data/></d:prop>
  {inner}
</card:addressbook-query>"#
    )
}

async fn query(app: &axum::Router, inner: &str) -> Vec<String> {
    // addressbook-query on a collection needs Depth: 1; Depth: 0 is the
    // `query-depth0-on-collection` 415 case and is asserted separately.
    let request = axum::http::Request::builder()
        .method("REPORT")
        .uri(BOOK_PATH)
        .header(
            axum::http::header::AUTHORIZATION,
            common::basic(USER, PASSWORD),
        )
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )
        .header("Depth", "1")
        .body(axum::body::Body::from(query_body(inner)))
        .unwrap();
    let resp = call(app, request).await;
    assert_eq!(resp.status, 207, "query failed: {}", resp.text());
    let doc = doc(&resp.body);
    doc.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "response")
        .filter_map(|c| c.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect()
}

#[tokio::test]
async fn query_filters_by_fn_email_uid() {
    let (_env, _book, app) = setup!();
    let fn_filter = r#"<card:filter><card:prop-filter name="FN"><card:text-match>Jane</card:text-match></card:prop-filter></card:filter>"#;
    assert_eq!(
        query(&app, fn_filter).await,
        vec!["/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf"]
    );

    let email_filter = r#"<card:filter><card:prop-filter name="EMAIL"><card:text-match match-type="ends-with">example.net</card:text-match></card:prop-filter></card:filter>"#;
    assert_eq!(
        query(&app, email_filter).await,
        vec!["/remote.php/dav/addressbooks/users/alice/contacts/john.vcf"]
    );

    let uid_filter = r#"<card:filter><card:prop-filter name="UID"><card:text-match match-type="equals">john-1</card:text-match></card:prop-filter></card:filter>"#;
    assert_eq!(
        query(&app, uid_filter).await,
        vec!["/remote.php/dav/addressbooks/users/alice/contacts/john.vcf"]
    );
}

#[tokio::test]
async fn query_limit_truncates_results() {
    let (_env, _book, app) = setup!();
    let inner = r#"<d:limit><d:nresults>1</d:nresults></d:limit>"#;
    assert_eq!(query(&app, inner).await.len(), 1);
}

#[tokio::test]
async fn query_param_filter_on_email_type() {
    let (_env, _book, app) = setup!();
    let inner = r#"<card:filter><card:prop-filter name="EMAIL"><card:param-filter name="TYPE"><card:text-match match-type="equals">HOME</card:text-match></card:param-filter></card:prop-filter></card:filter>"#;
    assert_eq!(
        query(&app, inner).await,
        vec!["/remote.php/dav/addressbooks/users/alice/contacts/john.vcf"]
    );
}

// ---------------------------------------------------------------------------
// sync-collection
// ---------------------------------------------------------------------------

fn sync_body(token: Option<&str>, limit: Option<i64>) -> String {
    let token = token
        .map(|t| format!("<d:sync-token>{t}</d:sync-token>"))
        .unwrap_or_default();
    let limit = limit
        .map(|n| format!("<d:limit><d:nresults>{n}</d:nresults></d:limit>"))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  {token}{limit}
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#
    )
}

#[tokio::test]
async fn sync_collection_initial_sync_reports_all_cards() {
    let (_env, _book, app) = setup!();
    let resp = report(&app, BOOK_PATH, USER, PASSWORD, &sync_body(None, None)).await;
    assert_eq!(resp.status, 207);
    let doc = doc(&resp.body);
    let hrefs: Vec<String> = doc
        .children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "response")
        .filter_map(|c| c.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect();
    assert_eq!(hrefs.len(), 3);
    assert_eq!(sync_token_of(&doc).unwrap(), "http://sabre.io/ns/sync/4");
}

#[tokio::test]
async fn sync_collection_paging_and_507_truncation() {
    let (_env, _book, app) = setup!();
    let resp = report(&app, BOOK_PATH, USER, PASSWORD, &sync_body(None, Some(1))).await;
    assert_eq!(resp.status, 207);
    let first = doc(&resp.body);
    let token = sync_token_of(&first).unwrap();
    assert!(
        token.starts_with("http://sabre.io/ns/sync/init_"),
        "got {token}"
    );
    // Truncation is signalled as a 507 response on the collection.
    let truncated = first
        .children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "response")
        .find(|c| {
            c.child(NS_DAV, "href")
                .map(|h| h.text == "/remote.php/dav/addressbooks/users/alice/contacts/")
                .unwrap_or(false)
        })
        .expect("a truncation response");
    assert!(response_status(truncated).unwrap().contains("507"));

    // Continue paging from the init token.
    let resp = report(
        &app,
        BOOK_PATH,
        USER,
        PASSWORD,
        &sync_body(Some(&token), Some(1)),
    )
    .await;
    let doc2 = doc(&resp.body);
    let next = sync_token_of(&doc2).unwrap();
    assert_ne!(next, token);
}

#[tokio::test]
async fn sync_collection_incremental_reports_add_modify_delete() {
    let (env, book, app) = setup!();
    let resp = report(&app, BOOK_PATH, USER, PASSWORD, &sync_body(None, None)).await;
    let current = sync_token_of(&doc(&resp.body)).unwrap();

    // New card (add), a modify, and a delete are logged as PHP would.
    env.seed_card(
        book,
        "new.vcf",
        b"BEGIN:VCARD\r\nUID:new\r\nFN:New\r\nEND:VCARD\r\n",
    )
    .await;
    env.add_change(book, "john.vcf", 2).await;
    env.add_change(book, "jane.vcf", 3).await;

    let resp = report(
        &app,
        BOOK_PATH,
        USER,
        PASSWORD,
        &sync_body(Some(&current), None),
    )
    .await;
    assert_eq!(resp.status, 207);
    let doc = doc(&resp.body);
    let add = response(
        &doc,
        "/remote.php/dav/addressbooks/users/alice/contacts/new.vcf",
    );
    assert!(prop_status(add, NS_DAV, "getetag").unwrap().contains("200"));
    let modified = response(
        &doc,
        "/remote.php/dav/addressbooks/users/alice/contacts/john.vcf",
    );
    assert!(prop_status(modified, NS_DAV, "getetag")
        .unwrap()
        .contains("200"));
    let deleted = response(
        &doc,
        "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf",
    );
    assert!(response_status(deleted).unwrap().contains("404"));
}

#[tokio::test]
async fn sync_collection_malformed_token_is_403_with_precondition() {
    // Sabre raises InvalidSyncToken (extends Forbidden) => 403 with a
    // `<d:valid-sync-token/>` body.
    let (_env, _book, app) = setup!();
    let resp = report(
        &app,
        BOOK_PATH,
        USER,
        PASSWORD,
        &sync_body(Some("42"), None),
    )
    .await;
    assert_eq!(resp.status, 403);
    assert!(resp.text().contains("valid-sync-token"), "{}", resp.text());
}

// ---------------------------------------------------------------------------
// Access control, writes, delegation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn other_users_and_missing_resources_are_404_not_403() {
    let (_env, _book, app) = setup!();
    // Cards that do not exist or belong to another user are 404, never 403.
    let card_cases = [
        "/remote.php/dav/addressbooks/users/bob/contacts/x.vcf",
        "/remote.php/dav/addressbooks/users/alice/contacts/missing.vcf",
    ];
    for path in card_cases {
        let resp = get(&app, path, USER, PASSWORD).await;
        assert_eq!(resp.status, 404, "GET {path} should be 404");
    }
    // Collection GETs for another user are hidden (404). A missing book on the
    // caller's own home is a collection read, so it delegates to PHP (501);
    // the sidecar never answers 403.
    for path in [
        "/remote.php/dav/addressbooks/users/bob",
        "/remote.php/dav/addressbooks/users/bob/contacts",
    ] {
        let resp = get(&app, path, USER, PASSWORD).await;
        assert_eq!(resp.status, 404, "GET {path} should be hidden");
    }
    let resp = get(
        &app,
        "/remote.php/dav/addressbooks/users/alice/missing",
        USER,
        PASSWORD,
    )
    .await;
    assert_eq!(
        resp.status, 501,
        "a missing book GET should delegate to PHP"
    );
    // PROPFIND on another user's book is 404 (not 403).
    let resp = propfind(
        &app,
        "/remote.php/dav/addressbooks/users/bob/contacts",
        USER,
        PASSWORD,
        "0",
        ALL_BOOK_PROPS,
    )
    .await;
    assert_eq!(resp.status, 404);
    // PROPFIND on a missing book of the caller is also 404.
    let resp = propfind(
        &app,
        "/remote.php/dav/addressbooks/users/alice/missing",
        USER,
        PASSWORD,
        "0",
        ALL_BOOK_PROPS,
    )
    .await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn collection_write_methods_return_501_so_nginx_can_fall_back_to_php() {
    // PUT/DELETE are native for cards; collection-level writes and the other
    // methods still delegate to PHP via 501.
    let (_env, _book, app) = setup!();
    let paths = [
        BOOK_PATH,
        "/remote.php/dav/addressbooks/users/alice/contacts/new.vcf",
    ];
    let methods = ["MKCOL", "PROPPATCH", "MOVE", "COPY", "POST"];
    for path in paths {
        for method in methods {
            let resp = call(&app, request(method, path, USER, PASSWORD)).await;
            assert_eq!(resp.status, 501, "{method} {path}");
        }
    }
    // A PUT on the collection (not a card) also stays 501.
    let resp = call(&app, request("PUT", BOOK_PATH, USER, PASSWORD)).await;
    assert_eq!(resp.status, 501, "PUT {BOOK_PATH}");
}

#[tokio::test]
async fn photo_and_export_delegate_to_php_with_501() {
    let (_env, _book, app) = setup!();
    let card = "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf";
    for query in ["photo", "photo&size=64", "export"] {
        let resp = get(&app, &format!("{card}?{query}"), USER, PASSWORD).await;
        assert_eq!(resp.status, 501, "?{query}");
    }
}

#[tokio::test]
async fn options_advertises_dav_classes() {
    let (_env, _book, app) = setup!();
    let resp = call(&app, request("OPTIONS", BOOK_PATH, USER, PASSWORD)).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("dav").as_deref(), Some("1, 2, 3, addressbook"));
    assert!(resp.header("allow").unwrap().contains("REPORT"));
    assert!(resp.header("allow").unwrap().contains("PROPFIND"));
    assert_eq!(resp.header("ms-author-via").as_deref(), Some("DAV"));
}

#[tokio::test]
async fn unauthenticated_requests_delegate() {
    let (_env, _book, app) = setup!();
    // No Authorization header: the request may still be authenticated by the
    // Nextcloud session cookie (which is exactly how the web UI talks to DAV)
    // or by an OAuth Bearer token, and the sidecar can evaluate neither.
    // Answering 401 here made the browser show a Basic Auth prompt for requests
    // PHP serves happily, so every such request is delegated instead
    // (501 -> nginx replays it to PHP, which owns the auth stack).
    let resp = call(
        &app,
        axum::http::Request::builder()
            .method("GET")
            .uri("/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status, 501);
    assert!(
        resp.header("www-authenticate").is_none(),
        "a 401 with WWW-Authenticate makes the browser pop up a Basic Auth prompt"
    );

    // OPTIONS is no exception: it delegates too, so a capability probe from a
    // session-authenticated client is not refused either.
    let resp = call(
        &app,
        axum::http::Request::builder()
            .method("OPTIONS")
            .uri(BOOK_PATH)
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status, 501);
}

// ---------------------------------------------------------------------------
// Authentication gates through the router
// ---------------------------------------------------------------------------

async fn status_with(app: &axum::Router, user: &str, password: &str) -> u16 {
    get(
        app,
        "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf",
        user,
        password,
    )
    .await
    .status
    .as_u16()
}

#[tokio::test]
async fn wrong_password_falls_back_to_php() {
    let (_env, _book, app) = setup!();
    // A password the app-password hash cannot match is not the fast path's to
    // judge (it may be the account password, which only PHP can check), so the
    // request is delegated. PHP is unreachable in this harness, hence 502.
    assert_eq!(status_with(&app, USER, "wrong").await, 502);
}

#[tokio::test]
async fn login_name_mismatch_is_401() {
    let (_env, _book, app) = setup!();
    // The token is minted under login_name "alice"; presenting "bob" with the
    // right password must not authenticate.
    assert_eq!(status_with(&app, "bob", PASSWORD).await, 401);
}

#[tokio::test]
async fn disabled_user_is_401() {
    let (env, _book, app) = setup!();
    env.disable_user(USER).await;
    assert_eq!(status_with(&app, USER, PASSWORD).await, 401);
}

#[tokio::test]
async fn password_invalid_token_is_401() {
    let env = env_or_skip!();
    env.seed_user(USER, None).await;
    env.seed_token_full(USER, USER, PASSWORD, 1, 2, None, true, common::now())
        .await;
    let app = env.app_shared();
    assert_eq!(status_with(&app, USER, PASSWORD).await, 401);
}

#[tokio::test]
async fn version_1_token_falls_back_to_php_and_therefore_502() {
    let env = env_or_skip!();
    env.seed_user(USER, None).await;
    // A v1 row: the sidecar must not use it; with PHP unreachable this is 502.
    env.seed_token(USER, USER, PASSWORD, 1, 1).await;
    let app = env.app_shared();
    assert_eq!(status_with(&app, USER, PASSWORD).await, 502);
}

#[tokio::test]
async fn stale_last_check_falls_back_to_php() {
    let env = env_or_skip!();
    env.seed_user(USER, None).await;
    env.seed_token_full(
        USER,
        USER,
        PASSWORD,
        1,
        2,
        None,
        false,
        common::now() - 3600,
    )
    .await;
    let app = env.app_shared();
    assert_eq!(status_with(&app, USER, PASSWORD).await, 502);
}

#[tokio::test]
async fn expired_token_falls_back_to_php() {
    let env = env_or_skip!();
    env.seed_user(USER, None).await;
    env.seed_token_full(
        USER,
        USER,
        PASSWORD,
        1,
        2,
        Some(common::now() - 1),
        false,
        common::now(),
    )
    .await;
    let app = env.app_shared();
    assert_eq!(status_with(&app, USER, PASSWORD).await, 502);
}

#[tokio::test]
async fn non_native_user_falls_back_to_php() {
    let env = env_or_skip!();
    // No oc_users row: LDAP/SSO style.
    env.seed_token("ldapuser", "ldapuser", PASSWORD, 1, 2).await;
    let app = env.app_shared();
    // The token is for uid ldapuser; present that login name.
    let resp = get(
        &app,
        "/remote.php/dav/addressbooks/users/ldapuser/contacts/x.vcf",
        "ldapuser",
        PASSWORD,
    )
    .await;
    assert_eq!(resp.status, 502);
}

#[tokio::test]
async fn wipe_token_type_is_rejected_401() {
    let env = env_or_skip!();
    env.seed_user(USER, None).await;
    env.seed_token(USER, USER, PASSWORD, 2, 2).await;
    let app = env.app_shared();
    assert_eq!(status_with(&app, USER, PASSWORD).await, 401);
}

#[tokio::test]
async fn temporary_token_type_falls_back_to_php() {
    let env = env_or_skip!();
    env.seed_user(USER, None).await;
    env.seed_token(USER, USER, PASSWORD, 0, 2).await;
    let app = env.app_shared();
    assert_eq!(status_with(&app, USER, PASSWORD).await, 502);
}

#[tokio::test]
async fn brute_force_recording_is_off_by_default() {
    let (env, _book, app) = setup!();
    // A hash miss delegates to PHP (unreachable here, hence 502); the point is
    // that no failure is recorded locally.
    assert_eq!(status_with(&app, USER, "wrong").await, 502);
    // `record_bruteforce_attempts` defaults to false: no INSERT happens.
    assert_eq!(env.count("bruteforce_attempts").await, 0);
}

#[tokio::test]
async fn successful_fast_path_does_not_touch_the_database_for_writes() {
    // The sidecar is read-only; a successful request must not add rows.
    let (env, _book, app) = setup!();
    let before = env.count("bruteforce_attempts").await;
    assert_eq!(status_with(&app, USER, PASSWORD).await, 200);
    assert_eq!(env.count("bruteforce_attempts").await, before);
}

// Keep the unused-import warning away for the rarely used helper.
#[allow(dead_code)]
fn _unused(_: &Resp, _: &str) {}
