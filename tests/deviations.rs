// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Asserts every declared deviation in `tests/deviations.toml`.
//!
//! `deviations_toml_ids_match` keeps the TOML and the Rust dispatch list in
//! sync. `every_declared_deviation_holds` runs one assertion per id and reports
//! *all* failures at once, so a silent behaviour change fails this suite and
//! forces the declaration (and its `status`) to be revisited.

mod common;

use common::{call, get, propfind, report, request, TestEnv};
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{NS_CARDDAV, NS_DAV, NS_OWNCLOUD};

const USER: &str = "alice";
const PASSWORD: &str = "app-password";
const BOOK_PATH: &str = "/remote.php/dav/addressbooks/users/alice/contacts";

const CARD_JANE: &[u8] = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:jane-1\r\nFN:Jane Doe\r\nEND:VCARD\r\n";

/// The ids asserted below. Kept in lock-step with `deviations.toml`.
const DECLARED_IDS: &[&str] = &[
    "weak-etag-get",
    "photo-delegated",
    "export-delegated",
    "home-listing-php",
    "writes-501",
    "shared-books-php",
    "contactsinteraction-php",
    "bruteforce-recording-off",
    "no-event-dispatch",
    "allprop-curated",
    "vcard-version-negotiation-missing",
    "conditional-get-missing",
    "max-resource-size-wrong",
    "supported-address-data-missing-json",
    "supported-collation-element-name",
    "sync-invalid-token-400",
    "groups-sorted",
    "query-depth0-on-collection",
    "authtoken-v2-only",
    "error-body-501",
];

fn toml_ids() -> Vec<String> {
    include_str!("deviations.toml")
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("id = ")
                .map(|rest| rest.trim().trim_matches('"').to_string())
        })
        .collect()
}

#[test]
fn deviations_toml_ids_match() {
    let mut toml = toml_ids();
    toml.sort();
    let mut declared: Vec<String> = DECLARED_IDS.iter().map(|s| s.to_string()).collect();
    declared.sort();
    assert_eq!(
        toml, declared,
        "tests/deviations.toml and DECLARED_IDS in tests/deviations.rs are out of sync"
    );
    assert_eq!(toml.len(), toml.iter().collect::<std::collections::HashSet<_>>().len());
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    env: TestEnv,
    #[allow(dead_code)]
    book: i64,
    app: axum::Router,
}

async fn fixture() -> Option<Fixture> {
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
    // Insert Work before Friends: the sidecar sorts, PHP does not.
    env.seed_property(book, jane, "CATEGORIES", "Work").await;
    env.seed_property(book, jane, "CATEGORIES", "Friends").await;
    env.seed_card(book, "john.vcf", b"BEGIN:VCARD\r\nUID:john-1\r\nFN:John Smith\r\nEND:VCARD\r\n")
        .await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    Some(Fixture { env, book, app })
}

macro_rules! ensure {
    ($cond:expr, $($msg:tt)*) => {
        if !$cond {
            return Err(format!($($msg)*));
        }
    };
}

// ---------------------------------------------------------------------------
// The assertions
// ---------------------------------------------------------------------------

async fn assert_deviation(id: &str, f: &Fixture) -> Result<(), String> {
    let card_path = "/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf";
    match id {
        "weak-etag-get" => {
            let get_resp = get(&f.app, card_path, USER, PASSWORD).await;
            let etag = get_resp.header("etag").unwrap_or_default();
            ensure!(!etag.starts_with("W/"), "GET etag is weak: {etag}");
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:propfind>"#;
            let prop = propfind(&f.app, card_path, USER, PASSWORD, "0", body).await;
            let d = doc(&prop.body);
            let prop_etag = prop_text(response(&d, card_path).unwrap(), NS_DAV, "getetag")
                .unwrap_or_default();
            ensure!(etag == prop_etag, "GET etag {etag} != PROPFIND etag {prop_etag}");
        }
        "photo-delegated" => {
            let resp = get(&f.app, &format!("{card_path}?photo"), USER, PASSWORD).await;
            ensure!(resp.status == 501, "?photo returned {}", resp.status);
        }
        "export-delegated" => {
            let resp = get(&f.app, &format!("{card_path}?export"), USER, PASSWORD).await;
            ensure!(resp.status == 501, "?export returned {}", resp.status);
        }
        "home-listing-php" => {
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            let resp = propfind(
                &f.app,
                "/remote.php/dav/addressbooks/users/alice",
                USER,
                PASSWORD,
                "1",
                body,
            )
            .await;
            let d = doc(&resp.body);
            let hrefs: Vec<String> = responses(&d)
                .iter()
                .filter_map(|c| c.child(NS_DAV, "href").map(|h| h.text.clone()))
                .collect();
            ensure!(
                !hrefs.iter().any(|h| h.contains("z-server-generated") || h.contains("contactsinteraction")),
                "home listing advertises app-generated collections: {hrefs:?}"
            );
        }
        "writes-501" => {
            for method in ["PUT", "DELETE", "MKCOL", "PROPPATCH", "MOVE", "COPY", "POST"] {
                let resp = call(&f.app, request(method, card_path, USER, PASSWORD)).await;
                ensure!(resp.status == 501, "{method} returned {}", resp.status);
            }
        }
        "shared-books-php" => {
            // Bob shares his book with alice; the sidecar must ignore it.
            let bob_book = f
                .env
                .seed_addressbook("principals/users/bob", "shared", Some("Bob"), None, 1)
                .await;
            let sql = format!(
                "INSERT INTO {}dav_shares (principaluri, type, access, resourceid) \
                 VALUES ('principals/users/alice', 'addressbook', 3, ?)",
                f.env.prefix
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(bob_book as i32)
                .execute(f.env.pool())
                .await
                .unwrap();
            let books = f
                .env
                .db
                .address_books_for_user("principals/users/alice")
                .await
                .unwrap();
            ensure!(books.len() == 1, "shared book leaked into the listing: {}", books.len());
            let resp = get(
                &f.app,
                "/remote.php/dav/addressbooks/users/alice/shared",
                USER,
                PASSWORD,
            )
            .await;
            ensure!(resp.status == 404, "shared book returned {}", resp.status);
        }
        "contactsinteraction-php" => {
            let resp = get(
                &f.app,
                "/remote.php/dav/addressbooks/users/alice/z-app-generated--contactsinteraction--recent",
                USER,
                PASSWORD,
            )
            .await;
            ensure!(resp.status == 404, "contactsinteraction returned {}", resp.status);
        }
        "bruteforce-recording-off" => {
            let resp = get(&f.app, card_path, USER, "wrong-password").await;
            ensure!(resp.status == 401, "wrong password returned {}", resp.status);
            ensure!(
                f.env.count("bruteforce_attempts").await == 0,
                "a bruteforce attempt was recorded although recording is off"
            );
        }
        "no-event-dispatch" => {
            let before_cards = f.env.count("cards").await;
            let before_changes = f.env.count("addressbookchanges").await;
            let resp = call(&f.app, request("PUT", card_path, USER, PASSWORD)).await;
            ensure!(resp.status == 501, "PUT returned {}", resp.status);
            ensure!(
                f.env.count("cards").await == before_cards,
                "PUT changed the card count"
            );
            ensure!(
                f.env.count("addressbookchanges").await == before_changes,
                "PUT changed the change log"
            );
        }
        "allprop-curated" => {
            // Empty body == allprop.
            let resp = propfind(&f.app, BOOK_PATH, USER, PASSWORD, "0", "").await;
            let d = doc(&resp.body);
            let r = response(&d, "/remote.php/dav/addressbooks/users/alice/contacts/").unwrap();
            ensure!(
                prop_of(r, NS_DAV, "displayname").is_some(),
                "allprop did not include displayname"
            );
            ensure!(
                prop_of(r, NS_DAV, "getetag").is_none(),
                "allprop unexpectedly included a card-only property on a book"
            );
            ensure!(
                prop_of(r, NS_CARDDAV, "address-data").is_none(),
                "allprop unexpectedly included address-data on a book"
            );
        }
        "vcard-version-negotiation-missing" => {
            let body = r#"<?xml version="1.0"?>
<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop><card:address-data content-type="text/vcard" version="4.0"/></d:prop>
  <d:href>/remote.php/dav/addressbooks/users/alice/contacts/jane.vcf</d:href>
</card:addressbook-multiget>"#;
            let resp = report(&f.app, BOOK_PATH, USER, PASSWORD, body).await;
            let d = doc(&resp.body);
            let r = response(&d, card_path).unwrap();
            let data = prop_text(r, NS_CARDDAV, "address-data").unwrap_or_default();
            ensure!(
                data.contains("VERSION:3.0"),
                "address-data was not returned as stored: {data}"
            );
        }
        "conditional-get-missing" => {
            let etag = get(&f.app, card_path, USER, PASSWORD)
                .await
                .header("etag")
                .unwrap();
            let req = axum::http::Request::builder()
                .method("GET")
                .uri(card_path)
                .header(axum::http::header::AUTHORIZATION, common::basic(USER, PASSWORD))
                .header("if-none-match", etag)
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = call(&f.app, req).await;
            ensure!(
                resp.status == 200,
                "conditional GET was evaluated (status {})",
                resp.status
            );
        }
        "max-resource-size-wrong" => {
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><card:max-resource-size/></d:prop></d:propfind>"#;
            let resp = propfind(&f.app, BOOK_PATH, USER, PASSWORD, "0", body).await;
            let d = doc(&resp.body);
            let r = response(&d, "/remote.php/dav/addressbooks/users/alice/contacts/").unwrap();
            let size = prop_text(r, NS_CARDDAV, "max-resource-size").unwrap();
            ensure!(
                size == "5242880",
                "max-resource-size is {size}; Sabre advertises 10000000"
            );
        }
        "supported-address-data-missing-json" => {
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><card:supported-address-data/></d:prop></d:propfind>"#;
            let resp = propfind(&f.app, BOOK_PATH, USER, PASSWORD, "0", body).await;
            let d = doc(&resp.body);
            let r = response(&d, "/remote.php/dav/addressbooks/users/alice/contacts/").unwrap();
            let types = prop_of(r, NS_CARDDAV, "supported-address-data").unwrap();
            let count = types
                .children
                .iter()
                .filter(|c| c.ns == NS_CARDDAV && c.local == "address-data-type")
                .count();
            ensure!(count == 2, "expected 2 address-data types, got {count}");
            ensure!(
                !types.children.iter().any(|c| c
                    .attr("content-type")
                    .map(|t| t == "application/vcard+json")
                    .unwrap_or(false)),
                "the jCard type is now advertised"
            );
        }
        "supported-collation-element-name" => {
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><card:supported-collation-set/></d:prop></d:propfind>"#;
            let resp = propfind(&f.app, BOOK_PATH, USER, PASSWORD, "0", body).await;
            let d = doc(&resp.body);
            let r = response(&d, "/remote.php/dav/addressbooks/users/alice/contacts/").unwrap();
            let set = prop_of(r, NS_CARDDAV, "supported-collation-set").unwrap();
            ensure!(
                set.children
                    .iter()
                    .all(|c| c.ns == NS_CARDDAV && c.local == "collation"),
                "supported-collation-set children are {:?}; Sabre uses supported-collation",
                set.children.iter().map(|c| c.local.clone()).collect::<Vec<_>>()
            );
        }
        "sync-invalid-token-400" => {
            let body = r#"<?xml version="1.0"?><d:sync-collection xmlns:d="DAV:"><d:sync-token>42</d:sync-token><d:prop><d:getetag/></d:prop></d:sync-collection>"#;
            let resp = report(&f.app, BOOK_PATH, USER, PASSWORD, body).await;
            ensure!(
                resp.status == 400,
                "malformed sync token returned {}; Sabre returns 403",
                resp.status
            );
        }
        "groups-sorted" => {
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:groups/></d:prop></d:propfind>"#;
            let resp = propfind(&f.app, BOOK_PATH, USER, PASSWORD, "0", body).await;
            let d = doc(&resp.body);
            let r = response(&d, "/remote.php/dav/addressbooks/users/alice/contacts/").unwrap();
            let groups: Vec<String> = prop_of(r, NS_OWNCLOUD, "groups")
                .unwrap()
                .children
                .iter()
                .map(|c| c.text.clone())
                .collect();
            ensure!(
                groups == vec!["Friends", "Work"],
                "groups are {groups:?}; expected sorted [Friends, Work]"
            );
        }
        "query-depth0-on-collection" => {
            // No Depth header: Sabre's default for this report is 0.
            let body = r#"<?xml version="1.0"?><card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/></d:prop></card:addressbook-query>"#;
            let resp = report(&f.app, BOOK_PATH, USER, PASSWORD, body).await;
            ensure!(
                resp.status == 207,
                "depth-0 query on a collection returned {}; Sabre returns 415",
                resp.status
            );
            let d = doc(&resp.body);
            ensure!(
                responses(&d).is_empty(),
                "depth-0 query on a collection returned responses"
            );
        }
        "authtoken-v2-only" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, None).await;
            env.seed_token(USER, USER, PASSWORD, 1, 1).await;
            let app = env.app_shared();
            let resp = get(&app, card_path, USER, PASSWORD).await;
            ensure!(
                resp.status == 502,
                "v1 token did not fall back to PHP (status {})",
                resp.status
            );
        }
        "error-body-501" => {
            let resp = call(&f.app, request("PUT", card_path, USER, PASSWORD)).await;
            ensure!(
                resp.text().contains("read-only"),
                "501 body is not the declared sidecar text: {}",
                resp.text()
            );
        }
        other => return Err(format!("no assertion implemented for declared id {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn every_declared_deviation_holds() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL (initdb/pg_ctl) not available on $PATH");
        return;
    };
    let mut failures = Vec::new();
    for id in DECLARED_IDS {
        if let Err(error) = assert_deviation(id, &f).await {
            failures.push(format!("{id}: {error}"));
        }
    }
    assert!(
        failures.is_empty(),
        "declared deviations no longer hold:\n  {}",
        failures.join("\n  ")
    );
}
