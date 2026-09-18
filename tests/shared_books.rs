// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared / group address books (`oc_dav_shares`): listing, wire names, the
//! sharing properties and the write ACL, through the DB layer and the real
//! axum router.
//!
//! Tests **skip** when PostgreSQL is unavailable.

mod common;

use common::{call, get, propfind, request, TestEnv};
use nextcloud_dav::model::VisibleBook;
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{NS_DAV, NS_NEXTCLOUD, NS_OWNCLOUD};

const ALICE: &str = "alice";
const PASSWORD: &str = "app-password";

const CARD_BOB: &[u8] =
    b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:bob-1\r\nFN:Bob's Friend\r\nEND:VCARD\r\n";

const ALL_SHARED_PROPS: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns">
  <d:prop>
    <d:displayname/><d:owner/><oc:owner-principal/><oc:read-only/>
    <nc:owner-displayname/><d:current-user-privilege-set/>
  </d:prop>
</d:propfind>"#;

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

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn doc(body: &[u8]) -> XNode {
    parse_document(body)
        .unwrap_or_else(|e| panic!("not XML: {e}: {}", String::from_utf8_lossy(body)))
}

fn response<'a>(doc: &'a XNode, href: &str) -> Option<&'a XNode> {
    doc.children.iter().find(|c| {
        c.ns == NS_DAV
            && c.local == "response"
            && c.child(NS_DAV, "href")
                .map(|h| h.text == href)
                .unwrap_or(false)
    })
}

fn prop_of<'a>(resp: &'a XNode, ns: &str, local: &str) -> Option<&'a XNode> {
    resp.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "propstat")
        .filter(|ps| {
            ps.child(NS_DAV, "status")
                .map(|s| s.text.contains("200"))
                .unwrap_or(false)
        })
        .filter_map(|ps| ps.child(NS_DAV, "prop"))
        .flat_map(|prop| prop.children.iter())
        .find(|c| c.ns == ns && c.local == local)
}

fn prop_text(resp: &XNode, ns: &str, local: &str) -> String {
    prop_of(resp, ns, local)
        .map(|n| n.text.clone())
        .unwrap_or_else(|| panic!("missing prop {{{ns}}}{local}"))
}

fn privilege_names(resp: &XNode) -> Vec<String> {
    prop_of(resp, NS_DAV, "current-user-privilege-set")
        .expect("current-user-privilege-set")
        .children
        .iter()
        .filter_map(|child| {
            child
                .children
                .first()
                .filter(|p| p.ns == NS_DAV)
                .map(|p| p.local.clone())
        })
        .collect()
}

/// Sends a PUT with a raw body and Basic auth.
async fn put_body(app: &axum::Router, path: &str, body: &[u8]) -> common::Resp {
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(path)
        .header(
            axum::http::header::AUTHORIZATION,
            common::basic(ALICE, PASSWORD),
        )
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/vcard; charset=utf-8",
        )
        .body(axum::body::Body::from(body.to_vec()))
        .unwrap();
    call(app, request).await
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    env: TestEnv,
    app: axum::Router,
    alice_book: i64,
    bob_book: i64,
    carol_book: i64,
    tombstoned_book: i64,
    other_group_book: i64,
}

async fn seed_book(env: &TestEnv, uid: &str, uri: &str, displayname: &str) -> i64 {
    env.seed_addressbook(
        &format!("principals/users/{uid}"),
        uri,
        Some(displayname),
        None,
        1,
    )
    .await
}

async fn fixture() -> Option<Fixture> {
    let env = TestEnv::new().await?;
    env.seed_user(ALICE, Some("Alice A")).await;
    env.seed_user("bob", Some("Bob Builder")).await;
    env.seed_user("carol", Some("Carol C")).await;
    env.seed_user("dave", Some("Dave D")).await;
    env.seed_user("erin", Some("Erin E")).await;
    env.seed_user("frank", Some("Frank F")).await;

    let alice_book = seed_book(&env, ALICE, "contacts", "Contacts").await;
    let bob_book = seed_book(&env, "bob", "bobcontacts", "Bob Contacts").await;
    let carol_book = seed_book(&env, "carol", "carolcontacts", "Carol Contacts").await;
    let dave_book = seed_book(&env, "dave", "davecontacts", "Dave Contacts").await;
    let erin_book = seed_book(&env, "erin", "erincontacts", "Erin Contacts").await;
    let frank_book = seed_book(&env, "frank", "frankcontacts", "Frank Contacts").await;
    let tombstoned_book = seed_book(&env, "bob", "bobtomb", "Bob Tombstoned").await;
    let other_group_book = seed_book(&env, "carol", "othergroup", "Other Group").await;

    // alice is in `team`; she is not in `outsiders`.
    env.seed_group("team").await;
    env.seed_group_member("team", ALICE).await;
    env.seed_group("outsiders").await;
    env.seed_group_member("outsiders", "dave").await;

    // bob -> alice, read-only.
    env.seed_share("principals/users/alice", 3, bob_book).await;
    // carol -> alice, read-write.
    env.seed_share("principals/users/alice", 2, carol_book)
        .await;
    // dave -> group team, read-write.
    env.seed_share("principals/groups/team", 2, dave_book).await;
    // erin -> alice read-only AND group team read-write: read-write wins.
    env.seed_share("principals/users/alice", 3, erin_book).await;
    env.seed_share("principals/groups/team", 2, erin_book).await;
    // frank -> alice read-write AND group team read-only: read-write wins too.
    env.seed_share("principals/users/alice", 2, frank_book)
        .await;
    env.seed_share("principals/groups/team", 3, frank_book)
        .await;
    // A group share plus a tombstone for alice: hidden (resourceid semantics).
    env.seed_share("principals/groups/team", 2, tombstoned_book)
        .await;
    env.seed_share("principals/users/alice", 5, tombstoned_book)
        .await;
    // Shared with a group alice is not a member of: invisible.
    env.seed_share("principals/groups/outsiders", 2, other_group_book)
        .await;
    // A share of alice's own book: skipped (already owned).
    env.seed_share("principals/users/alice", 3, alice_book)
        .await;

    // A card inside bob's book, readable through the read-only share.
    env.seed_card(bob_book, "bobfriend.vcf", CARD_BOB).await;
    env.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    Some(Fixture {
        env,
        app,
        alice_book,
        bob_book,
        carol_book,
        tombstoned_book,
        other_group_book,
    })
}

// ---------------------------------------------------------------------------
// Listing (DB layer)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn group_principals_use_php_urlencode() {
    let env = env_or_skip!();
    env.seed_user(ALICE, None).await;
    env.seed_group("team").await;
    env.seed_group_member("team", ALICE).await;
    // A gid with a space, a plus and a tilde: PHP urlencode, not RFC 3986.
    env.seed_group("a b+c~d").await;
    env.seed_group_member("a b+c~d", ALICE).await;
    env.seed_group("zzz").await;
    env.seed_group_member("zzz", "someone-else").await;

    let groups = env.db.group_principals(ALICE).await.unwrap();
    assert_eq!(
        groups,
        vec![
            "principals/groups/a+b%2Bc%7Ed".to_string(),
            "principals/groups/team".to_string(),
        ]
    );
}

#[tokio::test]
async fn visible_books_lists_owned_then_shared_in_id_order() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let groups = f.env.db.group_principals(ALICE).await.unwrap();
    assert_eq!(groups, vec!["principals/groups/team".to_string()]);
    let books = f
        .env
        .db
        .visible_books("principals/users/alice", &groups)
        .await
        .unwrap();
    let names: Vec<&str> = books.iter().map(|b| b.wire_uri.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "contacts",
            "bobcontacts_shared_by_bob",
            "carolcontacts_shared_by_carol",
            "davecontacts_shared_by_dave",
            "erincontacts_shared_by_erin",
            "frankcontacts_shared_by_frank",
        ]
    );
    // A share of the caller's own book is skipped, not duplicated.
    assert_eq!(names.iter().filter(|name| **name == "contacts").count(), 1);
    // The tombstoned group share and the outsider group share are hidden.
    assert!(!names.contains(&"bobtomb_shared_by_bob"));
    assert!(!names.contains(&"othergroup_shared_by_carol"));
}

#[tokio::test]
async fn shared_books_carry_owner_and_read_only_facts() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let groups = f.env.db.group_principals(ALICE).await.unwrap();
    let books = f
        .env
        .db
        .visible_books("principals/users/alice", &groups)
        .await
        .unwrap();
    let by_name = |name: &str| -> &VisibleBook {
        books
            .iter()
            .find(|b| b.wire_uri == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    };

    // Owned book: no sharing facts, wire name == stored uri.
    let owned = by_name("contacts");
    assert_eq!(owned.book.id, f.alice_book);
    assert_eq!(owned.owner_principal, None);
    assert!(!owned.read_only);
    assert_eq!(owned.wire_displayname.as_deref(), Some("Contacts"));

    // Read-only user share.
    let bob = by_name("bobcontacts_shared_by_bob");
    assert_eq!(bob.book.id, f.bob_book);
    assert_eq!(bob.owner_principal.as_deref(), Some("principals/users/bob"));
    assert!(bob.read_only);
    assert_eq!(
        bob.wire_displayname.as_deref(),
        Some("Bob Contacts (Bob Builder)")
    );

    // Read-write user share.
    let carol = by_name("carolcontacts_shared_by_carol");
    assert!(!carol.read_only);
    assert_eq!(
        carol.wire_displayname.as_deref(),
        Some("Carol Contacts (Carol C)")
    );

    // Read-write group share.
    let dave = by_name("davecontacts_shared_by_dave");
    assert!(!dave.read_only);
    assert_eq!(
        dave.owner_principal.as_deref(),
        Some("principals/users/dave")
    );

    // Read-write wins over read-only, in both row orders.
    assert!(!by_name("erincontacts_shared_by_erin").read_only);
    assert!(!by_name("frankcontacts_shared_by_frank").read_only);
}

#[tokio::test]
async fn unshare_tombstone_excludes_by_resourceid() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    // The tombstone row carries the sharee principal and the *resourceid*; the
    // surviving group share is the row PHP's literal `s.id NOT IN` would keep.
    let groups = f.env.db.group_principals(ALICE).await.unwrap();
    let books = f
        .env
        .db
        .visible_books("principals/users/alice", &groups)
        .await
        .unwrap();
    assert!(!books.iter().any(|b| b.book.id == f.tombstoned_book));
    assert!(!books.iter().any(|b| b.book.id == f.other_group_book));
}

// ---------------------------------------------------------------------------
// HTTP: PROPFIND, GET, PUT/DELETE ACL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn propfind_on_a_read_only_share_is_owner_decorated() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let path = "/remote.php/dav/addressbooks/users/alice/bobcontacts_shared_by_bob";
    let resp = propfind(&f.app, path, ALICE, PASSWORD, "0", ALL_SHARED_PROPS).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let d = doc(&resp.body);
    let r = response(&d, &format!("{path}/")).expect("book response");

    assert_eq!(
        prop_text(r, NS_DAV, "displayname"),
        "Bob Contacts (Bob Builder)"
    );
    assert_eq!(
        prop_text(r, NS_OWNCLOUD, "owner-principal"),
        "principals/users/bob"
    );
    assert_eq!(prop_text(r, NS_OWNCLOUD, "read-only"), "1");
    assert_eq!(
        prop_text(r, NS_NEXTCLOUD, "owner-displayname"),
        "Bob Builder"
    );
    assert_eq!(
        prop_of(r, NS_DAV, "owner")
            .and_then(|o| o.child(NS_DAV, "href"))
            .map(|h| h.text.clone())
            .as_deref(),
        Some("/remote.php/dav/principals/users/bob/")
    );
    assert_eq!(
        privilege_names(r),
        vec![
            "read",
            "read-acl",
            "read-current-user-privilege-set",
            "write-properties",
        ]
    );
}

#[tokio::test]
async fn propfind_on_a_read_write_share_keeps_the_full_privilege_set() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let path = "/remote.php/dav/addressbooks/users/alice/carolcontacts_shared_by_carol";
    let resp = propfind(&f.app, path, ALICE, PASSWORD, "0", ALL_SHARED_PROPS).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let d = doc(&resp.body);
    let r = response(&d, &format!("{path}/")).expect("book response");
    // PHP serialises `false` as the empty string.
    assert_eq!(prop_text(r, NS_OWNCLOUD, "read-only"), "");
    let privileges = privilege_names(r);
    assert!(privileges.contains(&"write".to_string()));
    assert!(privileges.contains(&"bind".to_string()));
    assert!(privileges.contains(&"read".to_string()));
}

#[tokio::test]
async fn owned_book_has_no_owner_principal_or_read_only() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let path = "/remote.php/dav/addressbooks/users/alice/contacts";
    let resp = propfind(&f.app, path, ALICE, PASSWORD, "0", ALL_SHARED_PROPS).await;
    assert_eq!(resp.status, 207);
    let d = doc(&resp.body);
    let r = response(&d, &format!("{path}/")).expect("book response");
    // 404 propstat: the property element is absent from the 200 propstat.
    assert!(prop_of(r, NS_OWNCLOUD, "owner-principal").is_none());
    assert!(prop_of(r, NS_OWNCLOUD, "read-only").is_none());
    assert_eq!(prop_text(r, NS_NEXTCLOUD, "owner-displayname"), "Alice A");
    assert_eq!(
        prop_of(r, NS_DAV, "owner")
            .and_then(|o| o.child(NS_DAV, "href"))
            .map(|h| h.text.clone())
            .as_deref(),
        Some("/remote.php/dav/principals/users/alice/")
    );
}

#[tokio::test]
async fn get_through_a_read_only_share_is_allowed() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let path = "/remote.php/dav/addressbooks/users/alice/bobcontacts_shared_by_bob/bobfriend.vcf";
    let resp = get(&f.app, path, ALICE, PASSWORD).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, CARD_BOB);
}

#[tokio::test]
async fn put_and_delete_on_a_read_only_share_are_404_with_no_write() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let before_cards = f.env.count("cards").await;
    let before_outbox = f.env.count("dav_event_outbox").await;
    let path = "/remote.php/dav/addressbooks/users/alice/bobcontacts_shared_by_bob/new.vcf";
    let body = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:new-1\r\nFN:New\r\nEND:VCARD\r\n";

    let put = put_body(&f.app, path, body).await;
    assert_eq!(put.status, 404, "read-only PUT must be 404, not 403");
    let delete = call(&f.app, request("DELETE", path, ALICE, PASSWORD)).await;
    assert_eq!(delete.status, 404, "read-only DELETE must be 404");

    assert_eq!(
        f.env.count("cards").await,
        before_cards,
        "a card was written"
    );
    assert_eq!(
        f.env.count("dav_event_outbox").await,
        before_outbox,
        "an outbox row was queued"
    );
}

#[tokio::test]
async fn put_on_a_read_write_share_lands_in_the_owners_book() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    let path = "/remote.php/dav/addressbooks/users/alice/carolcontacts_shared_by_carol/new.vcf";
    let body =
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:carol-new-1\r\nFN:New Carol Card\r\nEND:VCARD\r\n";
    let put = put_body(&f.app, path, body).await;
    assert_eq!(put.status, 201, "{}", put.text());

    use sqlx::Row;
    let sql = format!(
        "SELECT addressbookid FROM {}cards WHERE uri = 'new.vcf'",
        f.env.prefix
    );
    let row = sqlx::query(common::safe(sql))
        .fetch_one(f.env.pool())
        .await
        .unwrap();
    let addressbookid: i64 = row.try_get("addressbookid").unwrap();
    assert_eq!(
        addressbookid, f.carol_book,
        "the card must land under the owner's address book id"
    );

    let sql = format!(
        "SELECT addressbookid FROM {}dav_event_outbox WHERE card_uri = 'new.vcf'",
        f.env.prefix
    );
    let row = sqlx::query(common::safe(sql))
        .fetch_one(f.env.pool())
        .await
        .unwrap();
    assert_eq!(
        row.try_get::<i64, _>("addressbookid").unwrap(),
        f.carol_book
    );

    // DELETE goes through the same resolved (owner) book id.
    let delete = call(&f.app, request("DELETE", path, ALICE, PASSWORD)).await;
    assert_eq!(delete.status, 204, "{}", delete.text());
    let sql = format!(
        "SELECT COUNT(*) AS c FROM {}cards WHERE addressbookid = ?",
        f.env.prefix
    );
    let row = sqlx::query(common::safe(sql))
        .bind(f.carol_book)
        .fetch_one(f.env.pool())
        .await
        .unwrap();
    assert_eq!(row.try_get::<i64, _>("c").unwrap(), 0);
}

#[tokio::test]
async fn tombstoned_group_share_is_not_served() {
    let Some(f) = fixture().await else {
        eprintln!("SKIP: PostgreSQL not available");
        return;
    };
    for path in [
        "/remote.php/dav/addressbooks/users/alice/bobtomb_shared_by_bob",
        "/remote.php/dav/addressbooks/users/alice/othergroup_shared_by_carol",
    ] {
        let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
        let resp = propfind(&f.app, path, ALICE, PASSWORD, "0", body).await;
        assert_eq!(resp.status, 404, "PROPFIND {path}");
    }
}
