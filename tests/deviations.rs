// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Asserts every declared deviation in `tests/deviations.toml`.
//!
//! `deviations_toml_ids_match` keeps the TOML and the Rust dispatch list in
//! sync. `every_declared_deviation_holds` runs one assertion per id and reports
//! *all* failures at once, so a silent behaviour change fails this suite and
//! forces the declaration (and its `status`) to be revisited.

mod common;

use common::{call, get, propfind, propfind_with_headers, report, request, safe, TestEnv};
use nextcloud_dav::outbox::EffectRegistry;
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{
    NS_CALDAV, NS_CALENDARSERVER, NS_CARDDAV, NS_DAV, NS_NEXTCLOUD_FILES, NS_OWNCLOUD,
    NS_SABREDAV,
};

const APPLE: &str = "http://apple.com/ns/ical/";

const USER: &str = "alice";
const PASSWORD: &str = "app-password";
const BOOK_PATH: &str = "/remote.php/dav/addressbooks/users/alice/contacts";

const CARD_JANE: &[u8] =
    b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:jane-1\r\nFN:Jane Doe\r\nEND:VCARD\r\n";

/// The ids asserted below. Kept in lock-step with `deviations.toml`.
const DECLARED_IDS: &[&str] = &[
    "weak-etag-get",
    "photo-delegated",
    "export-delegated",
    "home-listing-php",
    "writes-501",
    "shared-books-php",
    "shared-unshare-tombstone-semantics",
    "shared-write-actor",
    "shared-books-group-backends",
    "shared-books-listing-order",
    "contactsinteraction-php",
    "bruteforce-recording-off",
    "no-event-dispatch",
    "events-queued-not-dispatched",
    "jcard-rejected",
    "vcard-2.1-rejected",
    "effect-ownership-registry",
    "allprop-curated",
    "vcard-version-negotiation-missing",
    "conditional-get-missing",
    "max-resource-size-wrong",
    "supported-address-data-missing-json",
    "supported-collation-element-name",
    "sync-invalid-token-400",
    "unauthenticated-delegates",
    "groups-sorted",
    "query-depth0-on-collection",
    "authtoken-v2-only",
    "error-body-501",
    "files-property-gate-501",
    "files-mount-delegation",
    "files-mount-external-backend-delegated",
    "files-mount-external-check-changes-delegated",
    "files-mount-circle-acl-delegated",
    "files-mount-share-type-delegated",
    "files-mount-acl-inherit-delegated",
    "files-non-propfind-501",
    "files-has-preview-static",
    "files-shareapi-exclude-groups-delegated",
    "files-quota-disk-free-approximation",
    "files-lock-props-delegated",
    "files-downloadurl-objectstore-delegated",
    "files-is-encrypted-e2ee-delegated",
    "files-sharees-ldap-display-name",
    "discovery-property-gate-501",
    "discovery-own-principal-only",
    "discovery-collection-listings-delegated",
    "discovery-non-propfind-501",
    "discovery-group-membership-backends",
    "discovery-language-request-fallback",
    "calendars-property-gate-501",
    "calendars-special-children-acl-delegated",
    "calendars-trashed-federated-delegated",
    "calendars-subscriptions-served",
    "calendars-subscriptions-listing-order",
    "calendars-webcal-caching-delegated",
    "calendars-own-home-only",
    "calendars-shared-listing-order",
    "calendars-personal-displayname-localized",
    "calendars-sync-nresults-zero",
    "calendars-sync-float-token",
    "calendars-group-share-acl-delegated",
    "calendars-objects-and-query-delegated",
    "calendars-report-shared-delegated",
    "calendars-report-expand-json-delegated",
    "calendars-report-property-gate-501",
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
    assert_eq!(
        toml.len(),
        toml.iter().collect::<std::collections::HashSet<_>>().len()
    );
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn doc(body: &[u8]) -> XNode {
    parse_document(body).unwrap()
}

/// A files PROPFIND body with the `d`/`oc`/`nc` prefixes bound.
fn files_prop_body(props: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop>{props}</d:prop></d:propfind>"#
    )
}

/// The text of one property of one response in a files multistatus.
fn files_prop_text(body: &[u8], href: &str, ns: &str, local: &str) -> Option<String> {
    let d = parse_document(body).ok()?;
    let r = response(&d, href)?;
    prop_text(r, ns, local)
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

/// True when `{ns}local` is present in a 404 propstat.
fn propstat_404(resp: &XNode, ns: &str, local: &str) -> bool {
    resp.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "propstat")
        .filter(|ps| {
            ps.child(NS_DAV, "status")
                .map(|s| s.text.contains("404"))
                .unwrap_or(false)
        })
        .filter_map(|ps| ps.child(NS_DAV, "prop"))
        .flat_map(|prop| prop.children.iter())
        .any(|c| c.ns == ns && c.local == local)
}

/// Sends a PUT with a raw body and Basic auth.
async fn put_body(app: &axum::Router, path: &str, body: &[u8]) -> common::Resp {
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(path)
        .header(
            axum::http::header::AUTHORIZATION,
            common::basic(USER, PASSWORD),
        )
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/vcard; charset=utf-8",
        )
        .body(axum::body::Body::from(body.to_vec()))
        .unwrap();
    call(app, request).await
}

/// The `effects` JSON and `state` of the most recent outbox row.
async fn latest_outbox(env: &TestEnv) -> (String, i64) {
    use sqlx::Row;
    let sql = format!(
        "SELECT effects, state FROM {}dav_event_outbox ORDER BY seq DESC LIMIT 1",
        env.prefix
    );
    let row = sqlx::query(common::safe(sql))
        .fetch_one(env.pool())
        .await
        .unwrap();
    (
        row.try_get("effects").unwrap(),
        row.try_get("state").unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    env: TestEnv,
    #[allow(dead_code)]
    book: i64,
    app: axum::Router,
    #[allow(dead_code)]
    pdf: i64,
    #[allow(dead_code)]
    zip: i64,
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
    env.seed_card(
        book,
        "john.vcf",
        b"BEGIN:VCARD\r\nUID:john-1\r\nFN:John Smith\r\nEND:VCARD\r\n",
    )
    .await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;

    // Files home, for the files deviations.
    let storage = env.seed_storage("home::alice").await;
    let root = env
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
    let pdf = env
        .seed_file(
            storage,
            "files/Doc.pdf",
            "Doc.pdf",
            "application/pdf",
            10,
            1_700_000_100,
            "etagpdf",
            27,
            root,
            None,
        )
        .await;
    let zip = env
        .seed_file(
            storage,
            "files/Archive.zip",
            "Archive.zip",
            "application/zip",
            20,
            1_700_000_200,
            "etagzip",
            27,
            root,
            None,
        )
        .await;
    env.seed_file(
        storage,
        "files/Sub",
        "Sub",
        "httpd/unix-directory",
        5,
        1_700_000_300,
        "etagsub",
        31,
        root,
        None,
    )
    .await;

    let app = env.app_shared();
    Some(Fixture {
        env,
        book,
        app,
        pdf,
        zip,
    })
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
            let prop_etag =
                prop_text(response(&d, card_path).unwrap(), NS_DAV, "getetag").unwrap_or_default();
            ensure!(
                etag == prop_etag,
                "GET etag {etag} != PROPFIND etag {prop_etag}"
            );
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
            let body =
                r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
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
                !hrefs
                    .iter()
                    .any(|h| h.contains("z-server-generated") || h.contains("contactsinteraction")),
                "home listing advertises app-generated collections: {hrefs:?}"
            );
        }
        "writes-501" => {
            // PUT/DELETE are native for cards; collection writes and the other
            // methods still delegate to PHP.
            for method in ["MKCOL", "PROPPATCH", "MOVE", "COPY", "POST"] {
                let resp = call(&f.app, request(method, card_path, USER, PASSWORD)).await;
                ensure!(resp.status == 501, "{method} returned {}", resp.status);
            }
            let resp = call(&f.app, request("PUT", BOOK_PATH, USER, PASSWORD)).await;
            ensure!(
                resp.status == 501,
                "PUT on a collection returned {}",
                resp.status
            );
        }
        "shared-books-php" => {
            // Bob shares his book with alice; the sidecar now serves it.
            let bob_book = f
                .env
                .seed_addressbook("principals/users/bob", "shared", Some("Bob"), None, 1)
                .await;
            f.env
                .seed_share("principals/users/alice", 3, bob_book)
                .await;

            let books = f
                .env
                .db
                .visible_books("principals/users/alice", &[])
                .await
                .unwrap();
            let shared = books
                .iter()
                .find(|book| book.wire_uri == "shared_shared_by_bob")
                .ok_or("the shared book is not listed")?;
            ensure!(shared.read_only, "the shared book should be read-only");
            ensure!(
                shared.owner_principal.as_deref() == Some("principals/users/bob"),
                "owner principal is {:?}",
                shared.owner_principal
            );

            let path = "/remote.php/dav/addressbooks/users/alice/shared_shared_by_bob";
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:owner-principal/><oc:read-only/></d:prop></d:propfind>"#;
            let resp = propfind(&f.app, path, USER, PASSWORD, "0", body).await;
            ensure!(
                resp.status == 207,
                "PROPFIND on the shared book returned {}",
                resp.status
            );
            let d = doc(&resp.body);
            let r = response(&d, &format!("{path}/")).ok_or("no book response")?;
            ensure!(
                prop_text(r, NS_OWNCLOUD, "owner-principal").as_deref()
                    == Some("principals/users/bob"),
                "owner-principal is missing"
            );
            ensure!(
                prop_text(r, NS_OWNCLOUD, "read-only").as_deref() == Some("1"),
                "read-only is missing"
            );
        }
        "shared-unshare-tombstone-semantics" => {
            // A group share plus an access=5 tombstone for the caller. PHP's
            // literal `s.id NOT IN (tombstone ids)` would keep it; we hide it.
            let bob_book = f
                .env
                .seed_addressbook("principals/users/bob", "tomb", Some("Tomb"), None, 1)
                .await;
            f.env.seed_group("tombgroup").await;
            f.env.seed_group_member("tombgroup", USER).await;
            f.env
                .seed_share("principals/groups/tombgroup", 2, bob_book)
                .await;
            f.env
                .seed_share("principals/users/alice", 5, bob_book)
                .await;
            let groups = f.env.db.group_principals(USER).await.unwrap();
            let books = f
                .env
                .db
                .visible_books("principals/users/alice", &groups)
                .await
                .unwrap();
            ensure!(
                !books.iter().any(|book| book.book.id == bob_book),
                "a tombstoned group share is still visible"
            );
        }
        "shared-write-actor" => {
            // The outbox has no actor column: a shared write is attributed to
            // the owner by the worker.
            let bob_book = f
                .env
                .seed_addressbook("principals/users/bob", "actor", Some("Actor"), None, 1)
                .await;
            f.env
                .seed_share("principals/users/alice", 2, bob_book)
                .await;
            let path = "/remote.php/dav/addressbooks/users/alice/actor_shared_by_bob/actor.vcf";
            let resp = put_body(
                &f.app,
                path,
                b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:actor-1\r\nFN:Actor\r\nEND:VCARD\r\n",
            )
            .await;
            ensure!(
                resp.status == 201,
                "PUT into the read-write share returned {}",
                resp.status
            );
            let sql = format!(
                "SELECT COUNT(*) AS c FROM information_schema.columns \
                 WHERE table_name = '{}dav_event_outbox' AND column_name = 'actor'",
                f.env.prefix
            );
            let row = sqlx::query(common::safe(sql))
                .fetch_one(f.env.pool())
                .await
                .unwrap();
            use sqlx::Row;
            ensure!(
                row.try_get::<i64, _>("c").unwrap() == 0,
                "the outbox grew an actor column; this deviation is stale"
            );
            let sql = format!(
                "SELECT addressbookid FROM {}dav_event_outbox \
                 WHERE card_uri = 'actor.vcf' ORDER BY seq DESC LIMIT 1",
                f.env.prefix
            );
            let row = sqlx::query(common::safe(sql))
                .fetch_one(f.env.pool())
                .await
                .unwrap();
            ensure!(
                row.try_get::<i64, _>("addressbookid").unwrap() == bob_book,
                "the outbox row does not carry the owner's book id"
            );
        }
        "shared-books-group-backends" => {
            // Group expansion is database-only: one principal per oc_group_user
            // row, and none for a user with no membership.
            let groups = f.env.db.group_principals(USER).await.unwrap();
            let sql = format!(
                "SELECT COUNT(*) AS c FROM {}group_user WHERE uid = ?",
                f.env.prefix
            );
            let row = sqlx::query(common::safe(sql))
                .bind(USER)
                .fetch_one(f.env.pool())
                .await
                .unwrap();
            use sqlx::Row;
            let membership: i64 = row.try_get("c").unwrap();
            ensure!(
                groups.len() as i64 == membership,
                "group expansion is not database-only ({} principals vs {} rows)",
                groups.len(),
                membership
            );
            ensure!(
                groups.iter().all(|g| g.starts_with("principals/groups/")),
                "unexpected group principal shape: {groups:?}"
            );
            ensure!(
                f.env
                    .db
                    .group_principals("nobody")
                    .await
                    .unwrap()
                    .is_empty(),
                "a user with no membership has group principals"
            );
        }
        "shared-books-listing-order" => {
            let groups = f.env.db.group_principals(USER).await.unwrap();
            let books = f
                .env
                .db
                .visible_books("principals/users/alice", &groups)
                .await
                .unwrap();
            let owned_count = books
                .iter()
                .take_while(|book| book.owner_principal.is_none())
                .count();
            let owned: Vec<i64> = books[..owned_count]
                .iter()
                .map(|book| book.book.id)
                .collect();
            let shared: Vec<i64> = books[owned_count..]
                .iter()
                .map(|book| book.book.id)
                .collect();
            ensure!(
                owned.windows(2).all(|w| w[0] < w[1]),
                "owned books are not ordered by id: {owned:?}"
            );
            ensure!(
                shared.windows(2).all(|w| w[0] < w[1]),
                "shared books are not ordered by id: {shared:?}"
            );
            ensure!(
                books
                    .iter()
                    .skip(owned_count)
                    .all(|b| b.owner_principal.is_some()),
                "an owned book appears after a shared one"
            );
        }
        "contactsinteraction-php" => {
            // Plugin-provided books are delegated to PHP (501 -> nginx replay),
            // not denied: the home listing advertises them and PHP can serve
            // them. Both known aliases must behave this way, for the book and
            // for a card under it.
            let body =
                r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            for book in [
                "z-app-generated--contactsinteraction--recent",
                "z-server-generated--system",
            ] {
                let path = format!("/remote.php/dav/addressbooks/users/alice/{book}");
                let resp = propfind(&f.app, &path, USER, PASSWORD, "0", body).await;
                ensure!(
                    resp.status == 501,
                    "{book} returned {} (expected 501 so nginx replays it to PHP)",
                    resp.status
                );
                let card = format!("{path}/someone.vcf");
                let resp = propfind(&f.app, &card, USER, PASSWORD, "0", body).await;
                ensure!(
                    resp.status == 501,
                    "a card under {book} returned {} (expected 501)",
                    resp.status
                );
            }
        }
        "unauthenticated-delegates" => {
            // No Authorization header at all. The request may still carry a
            // Nextcloud session cookie - which is exactly how the web UI talks
            // to DAV - or an OAuth Bearer token, and the sidecar can evaluate
            // neither, so it must delegate rather than refuse. Refusing with 401
            // sends `WWW-Authenticate`, which makes the browser pop up a Basic
            // Auth prompt for a request PHP answers with 200.
            let req = axum::http::Request::builder()
                .method("PROPFIND")
                .uri(card_path)
                .header("Depth", "0")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = call(&f.app, req).await;
            ensure!(
                resp.status == 501,
                "an unauthenticated request returned {} instead of delegating",
                resp.status
            );
            ensure!(
                resp.header("www-authenticate").is_none(),
                "the delegation carried WWW-Authenticate, which triggers a browser prompt"
            );
            // Credentials that ARE present but invalid are still refused, which
            // is what PHP does with them too.
            let bad = get(&f.app, card_path, USER, "wrong-password").await;
            ensure!(
                bad.status == 401 || bad.status == 502,
                "invalid credentials returned {}",
                bad.status
            );
        }
        "bruteforce-recording-off" => {
            let resp = get(&f.app, card_path, USER, "wrong-password").await;
            // A password the token hash cannot match is delegated to PHP (502
            // here, because the test PHP is unreachable); either way it is
            // rejected and nothing is recorded.
            ensure!(
                resp.status == 401 || resp.status == 502,
                "wrong password returned {}",
                resp.status
            );
            ensure!(
                f.env.count("bruteforce_attempts").await == 0,
                "a bruteforce attempt was recorded although recording is off"
            );
        }
        "no-event-dispatch" => {
            // The write commits, but the sidecar runs no listener: the event is
            // queued for the PHP worker instead.
            let before = f.env.count("cards").await;
            let resp = put_body(
                &f.app,
                "/remote.php/dav/addressbooks/users/alice/contacts/deviation.vcf",
                b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:dev-noevent\r\nFN:Deviation\r\nEND:VCARD\r\n",
            )
            .await;
            ensure!(resp.status == 201, "PUT returned {}", resp.status);
            ensure!(
                f.env.count("cards").await == before + 1,
                "PUT did not persist the card"
            );
            let (effects, state) = latest_outbox(&f.env).await;
            ensure!(state == 0, "outbox row is not pending (state {state})");
            let parsed: serde_json::Value = serde_json::from_str(&effects).unwrap();
            ensure!(
                parsed["rust"].as_array().unwrap().is_empty(),
                "the sidecar claims a Rust-owned effect: {effects}"
            );
        }
        "events-queued-not-dispatched" => {
            let before = f.env.count("dav_event_outbox").await;
            let resp = put_body(
                &f.app,
                "/remote.php/dav/addressbooks/users/alice/contacts/queued.vcf",
                b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:dev-queued\r\nFN:Queued\r\nEND:VCARD\r\n",
            )
            .await;
            ensure!(resp.status == 201, "PUT returned {}", resp.status);
            ensure!(
                f.env.count("dav_event_outbox").await == before + 1,
                "no exactly-one outbox row was queued"
            );
            // A create triggers activity + birthday, but not the update/delete
            // only effects (photo_cache, redis_cloud_id).
            let (effects, _state) = latest_outbox(&f.env).await;
            ensure!(
                effects.contains("activity_stream") && effects.contains("birthday_calendar"),
                "outbox effects are incomplete: {effects}"
            );
            ensure!(
                !effects.contains("redis_cloud_id"),
                "a create must not claim the update-only redis effect: {effects}"
            );
        }
        "jcard-rejected" => {
            let resp = put_body(
                &f.app,
                "/remote.php/dav/addressbooks/users/alice/contacts/jcard.vcf",
                b"[\"vcard\",[]]",
            )
            .await;
            ensure!(resp.status == 415, "jCard PUT returned {}", resp.status);
        }
        "vcard-2.1-rejected" => {
            let resp = put_body(
                &f.app,
                "/remote.php/dav/addressbooks/users/alice/contacts/v21.vcf",
                b"BEGIN:VCARD\r\nVERSION:2.1\r\nUID:v21\r\nFN:Old\r\nEND:VCARD\r\n",
            )
            .await;
            ensure!(resp.status == 415, "vCard 2.1 PUT returned {}", resp.status);
        }
        "effect-ownership-registry" => {
            let registry = EffectRegistry::default();
            let create = registry.effects_json_for(1);
            ensure!(
                create
                    == r#"{"php":["activity_stream","activity_mail","notification_push","birthday_calendar","calendar_reminders"],"rust":[]}"#,
                "phase-1 create registry differs: {create}"
            );
            let update = registry.effects_json_for(2);
            ensure!(
                update
                    == r#"{"php":["activity_stream","activity_mail","notification_push","birthday_calendar","calendar_reminders","photo_cache","redis_cloud_id"],"rust":[]}"#,
                "phase-1 update registry differs: {update}"
            );
            ensure!(
                registry.rust_effects().is_empty(),
                "phase 1 must own no effect natively"
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
                .header(
                    axum::http::header::AUTHORIZATION,
                    common::basic(USER, PASSWORD),
                )
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
                size == "10000000",
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
            ensure!(count == 3, "expected 3 address-data types, got {count}");
            ensure!(
                types.children.iter().any(|c| c
                    .attr("content-type")
                    .map(|t| t == "application/vcard+json")
                    .unwrap_or(false)),
                "the jCard type is not advertised"
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
                    .all(|c| c.ns == NS_CARDDAV && c.local == "supported-collation"),
                "supported-collation-set children are {:?}; Sabre uses supported-collation",
                set.children
                    .iter()
                    .map(|c| c.local.clone())
                    .collect::<Vec<_>>()
            );
        }
        "sync-invalid-token-400" => {
            let body = r#"<?xml version="1.0"?><d:sync-collection xmlns:d="DAV:"><d:sync-token>42</d:sync-token><d:prop><d:getetag/></d:prop></d:sync-collection>"#;
            let resp = report(&f.app, BOOK_PATH, USER, PASSWORD, body).await;
            ensure!(
                resp.status == 403,
                "malformed sync token returned {}",
                resp.status
            );
            ensure!(
                resp.text().contains("valid-sync-token"),
                "403 body lacks the valid-sync-token precondition: {}",
                resp.text()
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
                resp.status == 415,
                "depth-0 query on a collection returned {}; Sabre returns 415",
                resp.status
            );
            ensure!(
                resp.text().contains("supported-report"),
                "415 body lacks the supported-report precondition: {}",
                resp.text()
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
            let resp = call(
                &f.app,
                request(
                    "MKCOL",
                    "/remote.php/dav/addressbooks/users/alice/contacts/new",
                    USER,
                    PASSWORD,
                ),
            )
            .await;
            ensure!(
                resp.text().contains("does not implement this method"),
                "501 body is not the declared sidecar text: {}",
                resp.text()
            );
        }
        "files-property-gate-501" => {
            let pdf = "/remote.php/dav/files/alice/Doc.pdf";
            let body = files_prop_body("<d:getetag/>");
            let resp = propfind(&f.app, pdf, USER, PASSWORD, "0", &body).await;
            ensure!(
                resp.status == 207,
                "an implemented property must be served, got {}",
                resp.status
            );
            // A property PHP serves but the sidecar does not implement: 501.
            let body = files_prop_body("<oc:tags/>");
            let resp = propfind(&f.app, pdf, USER, PASSWORD, "0", &body).await;
            ensure!(
                resp.status == 501,
                "an unimplemented property must delegate, got {}",
                resp.status
            );
            let body = files_prop_body("<d:getetag/><oc:tags/>");
            let resp = propfind(&f.app, pdf, USER, PASSWORD, "0", &body).await;
            ensure!(
                resp.status == 501,
                "a mixed property set must delegate, got {}",
                resp.status
            );
        }
        "files-mount-delegation" => {
            let body = files_prop_body(
                "<d:getetag/><nc:mount-type/><nc:is-mount-root/>",
            );
            // A servable local external mount.
            let storage = f.env.seed_storage("local::/external/local/").await;
            let root = f
                .env
                .seed_file(storage, "", "", "httpd/unix-directory", 5, 1, "eLocal", 31, 0, None)
                .await;
            let mount_id = f.env.seed_external_mount("local").await;
            f.env
                .seed_external_option(mount_id, "filesystem_check_changes", "0")
                .await;
            f.env
                .seed_mount_external(USER, "/alice/files/LocalExt/", storage, root, mount_id)
                .await;
            // A non-local external mount: its contents delegate, its entry does
            // not suppress the containing listing.
            let s3 = f.env.seed_storage("amazons3::bucket-home").await;
            let s3_root = f
                .env
                .seed_file(s3, "", "", "httpd/unix-directory", 5, 1, "eS3", 31, 0, None)
                .await;
            let s3_mount_id = f.env.seed_external_mount("amazons3").await;
            f.env
                .seed_mount_external(USER, "/alice/files/S3/", s3, s3_root, s3_mount_id)
                .await;

            // The 85 % case: the home root contains mounts and is served.
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice",
                USER,
                PASSWORD,
                "1",
                &body,
            )
            .await;
            ensure!(
                resp.status == 207,
                "the home root with mounts must be served, got {}",
                resp.status
            );
            ensure!(
                resp.header(nextcloud_dav::routes::SIDECAR_HEADER).as_deref()
                    == Some(nextcloud_dav::routes::SIDECAR_VALUE),
                "a served listing must carry the sidecar header"
            );
            ensure!(
                files_prop_text(
                    &resp.body,
                    "/remote.php/dav/files/alice/S3/",
                    NS_NEXTCLOUD_FILES,
                    "mount-type",
                )
                .as_deref()
                    == Some("external"),
                "an unservable mount entry must be merged into the listing"
            );
            ensure!(
                files_prop_text(
                    &resp.body,
                    "/remote.php/dav/files/alice/LocalExt/",
                    NS_NEXTCLOUD_FILES,
                    "mount-type",
                )
                .as_deref()
                    == Some("external"),
                "a servable mount entry must be merged into the listing"
            );

            // Inside the non-local mount: delegate, and no sidecar header.
            for path in [
                "/remote.php/dav/files/alice/S3",
                "/remote.php/dav/files/alice/S3/sub",
            ] {
                let resp = propfind(&f.app, path, USER, PASSWORD, "0", &body).await;
                ensure!(resp.status == 501, "{path} must delegate, got {}", resp.status);
                ensure!(
                    resp.header(nextcloud_dav::routes::SIDECAR_HEADER).is_none(),
                    "{path} delegated but carried the sidecar header"
                );
            }
            // The servable local external mount is native.
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/LocalExt",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 207,
                "a servable local external mount must be native, got {}",
                resp.status
            );
            // A sibling of the mounts is still native.
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/Doc.pdf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 207,
                "a mount sibling must stay native, got {}",
                resp.status
            );

            // A mount whose root row is gone cannot be described at all: the
            // containing listing delegates rather than silently dropping it.
            f.env.seed_mount(USER, "/alice/files/Ghost/").await;
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/Ghost",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "an undescribable mount must delegate, got {}",
                resp.status
            );
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice",
                USER,
                PASSWORD,
                "1",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "a listing containing an undescribable mount must delegate, got {}",
                resp.status
            );
        }
        "files-mount-external-backend-delegated" => {
            let storage = f.env.seed_storage("amazons3::bucket").await;
            let root = f
                .env
                .seed_file(storage, "", "", "httpd/unix-directory", 5, 1, "eS3", 31, 0, None)
                .await;
            let mount_id = f.env.seed_external_mount("amazons3").await;
            f.env
                .seed_mount_external(USER, "/alice/files/S3/", storage, root, mount_id)
                .await;
            let body = files_prop_body("<d:getetag/>");
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/S3",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "a non-local external backend must delegate, got {}",
                resp.status
            );
        }
        "files-mount-external-check-changes-delegated" => {
            let storage = f.env.seed_storage("local::/external/cc/").await;
            let root = f
                .env
                .seed_file(storage, "", "", "httpd/unix-directory", 5, 1, "eCC", 31, 0, None)
                .await;
            let mount_id = f.env.seed_external_mount("local").await;
            f.env
                .seed_external_option(mount_id, "filesystem_check_changes", "1")
                .await;
            f.env
                .seed_mount_external(USER, "/alice/files/CheckChanges/", storage, root, mount_id)
                .await;
            let body = files_prop_body("<d:getetag/>");
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/CheckChanges",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "filesystem_check_changes must delegate, got {}",
                resp.status
            );
        }
        "files-mount-circle-acl-delegated" => {
            f.env.seed_group("circle-team").await;
            f.env.seed_group_member("circle-team", USER).await;
            let storage = f.env.seed_storage("local::/data/circle/").await;
            let root = f
                .env
                .seed_file(storage, "__groupfolders/9", "9", "httpd/unix-directory", 5, 1, "eCircle", 31, 0, None)
                .await;
            let folder_id = f
                .env
                .seed_group_folder("CircleGf", 1, -3, storage, root)
                .await;
            f.env
                .seed_group_folder_group(folder_id, Some("circle-team"), None, 31)
                .await;
            f.env
                .seed_acl_rule(root, "circle", "circle-single-id", 1, 1)
                .await;
            f.env
                .seed_mount_full(
                    USER,
                    "/alice/files/CircleGf/",
                    storage,
                    root,
                    "OCA\\GroupFolders\\Mount\\MountProvider",
                )
                .await;
            let body = files_prop_body("<d:getetag/>");
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/CircleGf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "a circle ACL rule must delegate, got {}",
                resp.status
            );
        }
        "files-mount-share-type-delegated" => {
            let storage = f.env.seed_storage("home::bob").await;
            let root = f
                .env
                .seed_file(storage, "files/CircleShare", "CircleShare", "httpd/unix-directory", 5, 1, "eCS", 31, 0, None)
                .await;
            f.env.seed_incoming_share(7, "circle-id", "bob", root, 31, 1).await;
            f.env
                .seed_mount_full(
                    USER,
                    "/alice/files/CircleShare/",
                    storage,
                    root,
                    "OCA\\Files_Sharing\\MountProvider",
                )
                .await;
            let body = files_prop_body("<d:getetag/>");
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/CircleShare",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "an unresolvable share type must delegate, got {}",
                resp.status
            );
        }
        "files-mount-acl-inherit-delegated" => {
            f.env.seed_group("inherit-team").await;
            f.env.seed_group_member("inherit-team", USER).await;
            let storage = f.env.seed_storage("local::/data/inherit/").await;
            let root = f
                .env
                .seed_file(storage, "__groupfolders/10", "10", "httpd/unix-directory", 5, 1, "eInherit", 31, 0, None)
                .await;
            let folder_id = f
                .env
                .seed_group_folder("InheritGf", 1, -3, storage, root)
                .await;
            f.env
                .seed_group_folder_group(folder_id, Some("inherit-team"), None, 31)
                .await;
            f.env
                .seed_mount_full(
                    USER,
                    "/alice/files/InheritGf/",
                    storage,
                    root,
                    "OCA\\GroupFolders\\Mount\\MountProvider",
                )
                .await;
            let body = files_prop_body("<d:getetag/>");
            // Default (false): the ACL folder is served.
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/InheritGf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 207,
                "an ACL groupfolder must be served by default, got {}",
                resp.status
            );
            // `acl-inherit-per-user = true`: the alternative merge, delegate.
            f.env
                .seed_appconfig("groupfolders", "acl-inherit-per-user", "true")
                .await;
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/InheritGf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "acl-inherit-per-user must delegate ACL folders, got {}",
                resp.status
            );
            // A groupfolder without ACLs is unaffected: reset the flag.
            let sql = format!(
                "UPDATE {}appconfig SET configvalue = 'false' WHERE appid='groupfolders' AND configkey='acl-inherit-per-user'",
                f.env.prefix
            );
            sqlx::query(safe(sql)).execute(f.env.pool()).await.unwrap();
        }
        "files-non-propfind-501" => {
            for method in [
                "OPTIONS",
                "GET",
                "HEAD",
                "PUT",
                "DELETE",
                "MKCOL",
                "PROPPATCH",
                "REPORT",
            ] {
                let resp = call(
                    &f.app,
                    request(
                        method,
                        "/remote.php/dav/files/alice/Doc.pdf",
                        USER,
                        PASSWORD,
                    ),
                )
                .await;
                ensure!(
                    resp.status == 501,
                    "{method} must delegate, got {}",
                    resp.status
                );
            }
        }
        "files-has-preview-static" => {
            let body = files_prop_body("<nc:has-preview/>");
            let pdf = "/remote.php/dav/files/alice/Doc.pdf";
            let resp = propfind(&f.app, pdf, USER, PASSWORD, "0", &body).await;
            let text = files_prop_text(&resp.body, pdf, NS_NEXTCLOUD_FILES, "has-preview");
            ensure!(
                text.as_deref() == Some("true"),
                "a PDF is assumed previewable by the static list, got {text:?}"
            );
            let zip = "/remote.php/dav/files/alice/Archive.zip";
            let resp = propfind(&f.app, zip, USER, PASSWORD, "0", &body).await;
            let text = files_prop_text(&resp.body, zip, NS_NEXTCLOUD_FILES, "has-preview");
            ensure!(
                text.as_deref() == Some("false"),
                "a zip is not previewable, got {text:?}"
            );
        }
        "files-shareapi-exclude-groups-delegated" => {
            let app = f.env.app_shared_with("testinst", true, None);
            let body = files_prop_body("<d:getetag/>");
            let resp = propfind(
                &app,
                "/remote.php/dav/files/alice/Doc.pdf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "shareapi_exclude_groups must delegate, got {}",
                resp.status
            );
        }
        "files-quota-disk-free-approximation" => {
            f.env.seed_appconfig("files", "default_quota", "1 GB").await;
            let app = f.env.app_shared_with(
                "testinst",
                false,
                Some(std::path::PathBuf::from("/nonexistent-ncdav")),
            );
            let body = files_prop_body("<d:quota-available-bytes/>");
            let resp = propfind(
                &app,
                "/remote.php/dav/files/alice/Sub",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 207,
                "a quota request must be served, got {}",
                resp.status
            );
            let text = files_prop_text(
                &resp.body,
                "/remote.php/dav/files/alice/Sub/",
                NS_DAV,
                "quota-available-bytes",
            );
            let expected = (1024 * 1024 * 1024 - 100).to_string();
            ensure!(
                text.as_deref() == Some(expected.as_str()),
                "quota available is {text:?}, expected {expected}"
            );
        }
        "files-lock-props-delegated" => {
            // `files_lock` is not enabled on the reference instance, so the
            // desktop client does not request these. If it did, they need the
            // lock backend and must delegate (501), never be invented.
            for prop in ["lock", "lock-owner", "lock-token", "lock-time"] {
                let body = files_prop_body(&format!("<nc:{prop}/>"));
                let resp = propfind(
                    &f.app,
                    "/remote.php/dav/files/alice/Doc.pdf",
                    USER,
                    PASSWORD,
                    "0",
                    &body,
                )
                .await;
                ensure!(
                    resp.status == 501,
                    "nc:{prop} must delegate, got {}",
                    resp.status
                );
            }
        }
        "files-downloadurl-objectstore-delegated" => {
            // With a primary object store, PHP's `oc:downloadURL` is a presigned
            // URL the sidecar cannot derive, so it delegates instead of
            // answering the local-storage empty value.
            let app = f.env.app_shared_objectstore();
            let body = files_prop_body("<oc:downloadURL/>");
            let resp = propfind(
                &app,
                "/remote.php/dav/files/alice/Doc.pdf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "objectstore downloadURL must delegate, got {}",
                resp.status
            );
            // Without the objectstore it is served (empty, like PHP's false).
            let body = files_prop_body("<oc:downloadURL/>");
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/Doc.pdf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(resp.status == 207, "downloadURL returned {}", resp.status);
        }
        "files-is-encrypted-e2ee-delegated" => {
            // Without end-to-end encryption, PHP has no `nc:is-encrypted`
            // handler, so the sidecar serves the request natively (the property
            // itself is a 404 propstat, checked in tests/files_read_path.rs).
            let body = files_prop_body("<nc:is-encrypted/>");
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/Doc.pdf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(resp.status == 207, "is-encrypted returned {}", resp.status);
            // With the E2EE app enabled it is delegated instead.
            let app = f.env.app_shared_e2ee();
            let resp = propfind(
                &app,
                "/remote.php/dav/files/alice/Doc.pdf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "E2EE is-encrypted must delegate, got {}",
                resp.status
            );
        }
        "files-sharees-ldap-display-name" => {
            // `nc:sharees` display-name is joined from oc_users/oc_groups. A
            // sharee the database backend does not know (an LDAP/circle user)
            // falls back to the id, where PHP would ask that backend.
            f.env
                .seed_file_share(0, Some("ldapuser"), USER, USER, f.pdf, 19)
                .await;
            let body = files_prop_body("<oc:share-types/><nc:sharees/>");
            let resp = propfind(
                &f.app,
                "/remote.php/dav/files/alice/Doc.pdf",
                USER,
                PASSWORD,
                "0",
                &body,
            )
            .await;
            ensure!(resp.status == 207, "sharees returned {}", resp.status);
            let d = doc(&resp.body);
            let node = response(&d, "/remote.php/dav/files/alice/Doc.pdf").unwrap();
            let sharee = prop_of(node, NS_NEXTCLOUD_FILES, "sharees")
                .and_then(|sharees| sharees.children.first())
                .ok_or("no sharee element")?;
            let display = sharee
                .children
                .iter()
                .find(|c| c.local == "display-name")
                .map(|c| c.text.clone())
                .unwrap_or_default();
            ensure!(
                display == "ldapuser",
                "sharee display-name is {display:?}, expected the id fallback"
            );
        }
        "discovery-property-gate-501" => {
            let body = |props: &str| {
                format!(
                    r#"<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.com/ns"><d:prop>{props}</d:prop></d:propfind>"#
                )
            };
            let root = "/remote.php/dav/";
            let principal = "/remote.php/dav/principals/users/alice/";
            // An implemented property is served.
            let resp = propfind(
                &f.app,
                root,
                USER,
                PASSWORD,
                "0",
                &body("<d:current-user-principal/>"),
            )
            .await;
            ensure!(
                resp.status == 207,
                "an implemented root property must be served, got {}",
                resp.status
            );
            // An unimplemented property on the root delegates.
            let resp = propfind(&f.app, root, USER, PASSWORD, "0", &body("<d:displayname/>")).await;
            ensure!(
                resp.status == 501,
                "an unimplemented root property must delegate, got {}",
                resp.status
            );
            // An unimplemented property on the principal delegates.
            let resp = propfind(
                &f.app,
                principal,
                USER,
                PASSWORD,
                "0",
                &body("<d:getetag/>"),
            )
            .await;
            ensure!(
                resp.status == 501,
                "an unimplemented principal property must delegate, got {}",
                resp.status
            );
            // A mixed set delegates as a whole.
            let resp = propfind(
                &f.app,
                principal,
                USER,
                PASSWORD,
                "0",
                &body("<d:displayname/><d:getetag/>"),
            )
            .await;
            ensure!(
                resp.status == 501,
                "a mixed property set must delegate, got {}",
                resp.status
            );
        }
        "discovery-own-principal-only" => {
            let body =
                r#"<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>"#;
            let resp = propfind(
                &f.app,
                "/remote.php/dav/principals/users/bob/",
                USER,
                PASSWORD,
                "0",
                body,
            )
            .await;
            ensure!(
                resp.status == 501,
                "another user's principal must delegate, got {}",
                resp.status
            );
            let resp = propfind(
                &f.app,
                "/remote.php/dav/principals/users/alice/",
                USER,
                PASSWORD,
                "0",
                body,
            )
            .await;
            ensure!(
                resp.status == 207,
                "the caller's own principal must be served, got {}",
                resp.status
            );
        }
        "discovery-collection-listings-delegated" => {
            let body =
                r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            for path in [
                "/remote.php/dav/principals",
                "/remote.php/dav/principals/",
                "/remote.php/dav/principals/users/",
                "/remote.php/dav/principals/groups/admin/",
            ] {
                let resp = propfind(&f.app, path, USER, PASSWORD, "0", body).await;
                ensure!(resp.status == 501, "{path} must delegate, got {}", resp.status);
            }
        }
        "discovery-non-propfind-501" => {
            for path in ["/remote.php/dav/", "/remote.php/dav/principals/users/alice/"] {
                for method in ["OPTIONS", "GET", "HEAD", "PUT", "MKCOL", "REPORT"] {
                    let resp = call(&f.app, request(method, path, USER, PASSWORD)).await;
                    ensure!(
                        resp.status == 501,
                        "{method} {path} must delegate, got {}",
                        resp.status
                    );
                }
            }
        }
        "discovery-group-membership-backends" => {
            f.env.seed_group("devs").await;
            f.env.seed_group_member("devs", USER).await;
            let body =
                r#"<d:propfind xmlns:d="DAV:"><d:prop><d:group-membership/></d:prop></d:propfind>"#;
            let principal = "/remote.php/dav/principals/users/alice/";
            let resp = propfind(&f.app, principal, USER, PASSWORD, "0", body).await;
            ensure!(resp.status == 207, "group-membership returned {}", resp.status);
            let d = doc(&resp.body);
            let r = response(&d, principal).ok_or("no principal response")?;
            let membership = prop_of(r, NS_DAV, "group-membership")
                .ok_or("no group-membership property")?;
            let hrefs: Vec<String> = membership
                .children
                .iter()
                .filter(|c| c.ns == NS_DAV && c.local == "href")
                .map(|c| c.text.clone())
                .collect();
            ensure!(
                hrefs.contains(&"/remote.php/dav/principals/groups/devs/".to_string()),
                "group-membership is missing the database group: {hrefs:?}"
            );
        }
        "discovery-language-request-fallback" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.com/ns"><d:prop><nc:language/></d:prop></d:propfind>"#;
            let principal = "/remote.php/dav/principals/users/alice/";
            let resp = propfind(&app, principal, USER, PASSWORD, "0", body).await;
            ensure!(
                resp.status == 501,
                "an underivable language must delegate, got {}",
                resp.status
            );
            env.seed_preference(USER, "core", "lang", "fr").await;
            let resp = propfind(&app, principal, USER, PASSWORD, "0", body).await;
            ensure!(
                resp.status == 207,
                "a derivable language must be served, got {}",
                resp.status
            );
            let d = doc(&resp.body);
            let r = response(&d, principal).unwrap();
            ensure!(
                prop_text(r, "http://nextcloud.com/ns", "language").as_deref() == Some("fr"),
                "language is wrong"
            );
        }
        "calendars-property-gate-501" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_calendar(
                "principals/users/alice",
                "work",
                Some("Work"),
                0,
                None,
                Some("VEVENT"),
                false,
                1,
            )
            .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let path = "/remote.php/dav/calendars/alice/work/";
            // A property outside the modelled/known-404 set delegates.
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:x="http://example.com/ns"><d:prop><x:whatever/></d:prop></d:propfind>"#;
            let resp = propfind(&app, path, USER, PASSWORD, "0", body).await;
            ensure!(
                resp.status == 501,
                "an unknown property must delegate, got {}",
                resp.status
            );
            // `{cs}publish-url` is now modelled: unpublished -> a 404 propstat,
            // served (207) rather than delegated.
            let body = r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:publish-url/></d:prop></d:propfind>"#;
            let resp = propfind(&app, path, USER, PASSWORD, "0", body).await;
            ensure!(
                resp.status == 207,
                "publish-url must be served, got {}",
                resp.status
            );
            let d = doc(&resp.body);
            let r = response(&d, path).unwrap();
            ensure!(
                propstat_404(r, "http://calendarserver.org/ns/", "publish-url"),
                "an unpublished publish-url must be a 404 propstat"
            );
            // A property PHP 404s stays a 404 propstat, not a 501.
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:quota-used-bytes/></d:prop></d:propfind>"#;
            let resp = propfind(&app, path, USER, PASSWORD, "0", body).await;
            ensure!(resp.status == 207, "quota-used-bytes got {}", resp.status);
            let d = doc(&resp.body);
            let r = response(&d, path).unwrap();
            ensure!(
                propstat_404(r, NS_DAV, "quota-used-bytes"),
                "quota-used-bytes must be a 404 propstat"
            );
        }
        "calendars-special-children-acl-delegated" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:acl/></d:prop></d:propfind>"#;
            let resp = propfind(&app, "/remote.php/dav/calendars/alice/", USER, PASSWORD, "1", body).await;
            ensure!(
                resp.status == 501,
                "home Depth:1 with acl must delegate, got {}",
                resp.status
            );
        }
        "calendars-trashed-federated-delegated" => {
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            let cal = env
                .seed_calendar("principals/users/alice", "personal", Some("Personal"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.trash_calendar(cal, 1_700_000_000).await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let resp = propfind(&app, "/remote.php/dav/calendars/alice/", USER, PASSWORD, "1", body).await;
            ensure!(
                resp.status == 501,
                "a trashed calendar must delegate the listing, got {}",
                resp.status
            );

            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_federated_calendar("principals/users/alice", "fed").await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let resp = propfind(&app, "/remote.php/dav/calendars/alice/", USER, PASSWORD, "1", body).await;
            ensure!(
                resp.status == 501,
                "a federated calendar must delegate the listing, got {}",
                resp.status
            );
        }
        "calendars-subscriptions-served" => {
            const BODY: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:apple="http://apple.com/ns/ical/" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns"><d:prop><d:resourcetype/><d:displayname/><d:owner/><d:getlastmodified/><d:supported-report-set/><d:supported-method-set/><cs:source/><cs:getctag/><cs:subscribed-strip-todos/><cs:subscribed-strip-alarms/><cs:subscribed-strip-attachments/><cal:supported-calendar-component-set/><cal:max-resource-size/><apple:refreshrate/><apple:calendar-color/><apple:calendar-order/><s:sync-token/><oc:owner-principal/><oc:read-only/></d:prop></d:propfind>"#;
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_calendar("principals/users/alice", "work", Some("Work"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_subscription_full(
                "principals/users/alice",
                "webcal",
                Some("Work Webcal"),
                Some("PT4H"),
                20,
                Some("#ff00ff"),
                1,
                0,
                0,
                Some(1_700_000_000),
                5,
                "https://example.com/work.ics",
            )
            .await;
            env.seed_calendar_subscription_full(
                "principals/users/alice",
                "nullsub",
                None,
                None,
                21,
                None,
                0,
                0,
                0,
                None,
                1,
                "webcal://example.com/null.ics",
            )
            .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();

            let resp = propfind(&app, "/remote.php/dav/calendars/alice/", USER, PASSWORD, "1", BODY).await;
            ensure!(
                resp.status == 207,
                "a home with subscriptions must be served, got {}",
                resp.status
            );
            ensure!(
                resp.header(nextcloud_dav::routes::SIDECAR_HEADER).as_deref() == Some("sidecar"),
                "the home with subscriptions must be served by the sidecar"
            );
            let d = doc(&resp.body);
            let order: Vec<String> = responses(&d)
                .iter()
                .filter_map(|r| r.child(NS_DAV, "href").map(|h| h.text.clone()))
                .collect();
            let webcal = "/remote.php/dav/calendars/alice/webcal/";
            let nullsub = "/remote.php/dav/calendars/alice/nullsub/";
            let trashbin = "/remote.php/dav/calendars/alice/trashbin/";
            let pos = |href: &str| order.iter().position(|h| h == href);
            ensure!(
                pos(trashbin) < pos(webcal) && pos(webcal) < pos(nullsub),
                "subscriptions must come after the special children, in calendarorder: {order:?}"
            );

            let sub = response(&d, webcal).expect("the webcal subscription response");
            let resourcetype = prop_of(sub, NS_DAV, "resourcetype").expect("resourcetype");
            ensure!(
                resourcetype
                    .children
                    .iter()
                    .any(|c| c.ns == NS_CALENDARSERVER && c.local == "subscribed"),
                "a subscription resourcetype must carry {{cs}}subscribed"
            );
            ensure!(
                prop_text(sub, NS_CALENDARSERVER, "source").is_some(),
                "{{cs}}source must be served"
            );
            let source = prop_of(sub, NS_CALENDARSERVER, "source").unwrap();
            ensure!(
                source.child(NS_DAV, "href").map(|h| h.text.as_str()) == Some("https://example.com/work.ics"),
                "{{cs}}source must be the Href of the webcal URL"
            );
            ensure!(
                prop_text(sub, NS_CALENDARSERVER, "getctag").as_deref() == Some("5"),
                "a subscription getctag is the raw sync-token"
            );
            ensure!(
                prop_text(sub, NS_SABREDAV, "sync-token").as_deref() == Some("5"),
                "a subscription {{sabredav}}sync-token is the raw token"
            );
            ensure!(
                prop_text(sub, APPLE, "refreshrate").as_deref() == Some("PT4H"),
                "{{apple}}refreshrate must be served"
            );
            for strip in ["subscribed-strip-todos", "subscribed-strip-alarms", "subscribed-strip-attachments"] {
                ensure!(
                    prop_of(sub, NS_CALENDARSERVER, strip).is_some(),
                    "{{cs}}{strip} must be served empty"
                );
            }
            let comps = prop_of(sub, NS_CALDAV, "supported-calendar-component-set").unwrap();
            let names: Vec<&str> = comps
                .children
                .iter()
                .filter_map(|c| c.attr("name"))
                .collect();
            ensure!(
                names == ["VTODO", "VEVENT"],
                "a subscription component set is hard-coded VTODO,VEVENT, got {names:?}"
            );
            // Properties a subscription must NOT serve.
            for (ns, local) in [
                (NS_OWNCLOUD, "owner-principal"),
                (NS_OWNCLOUD, "read-only"),
                (NS_CALDAV, "max-resource-size"),
            ] {
                ensure!(
                    propstat_404(sub, ns, local),
                    "{{{ns}}}{local} must be a 404 propstat on a subscription"
                );
            }

            let child = propfind(&app, webcal, USER, PASSWORD, "0", BODY).await;
            ensure!(
                child.status == 207
                    && child.header(nextcloud_dav::routes::SIDECAR_HEADER).as_deref() == Some("sidecar"),
                "a Depth:0 subscription PROPFIND must be served, got {}",
                child.status
            );
            let child_doc = doc(&child.body);
            let child_resp = response(&child_doc, webcal).expect("the child response");
            ensure!(
                prop_text(child_resp, NS_CALENDARSERVER, "getctag").as_deref() == Some("5"),
                "the Depth:0 subscription must carry the raw getctag"
            );
        }
        "calendars-subscriptions-listing-order" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            // Equal `calendarorder`; the sidecar must fall back to id order.
            env.seed_calendar_subscription_full(
                "principals/users/alice", "first", Some("First"), None, 7, None, 0, 0, 0, None, 1, "https://e/first.ics",
            )
            .await;
            env.seed_calendar_subscription_full(
                "principals/users/alice", "second", Some("Second"), None, 7, None, 0, 0, 0, None, 1, "https://e/second.ics",
            )
            .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            let resp = propfind(&app, "/remote.php/dav/calendars/alice/", USER, PASSWORD, "1", body).await;
            ensure!(resp.status == 207, "listing must be served, got {}", resp.status);
            let d = doc(&resp.body);
            let order: Vec<String> = responses(&d)
                .iter()
                .filter_map(|r| r.child(NS_DAV, "href").map(|h| h.text.clone()))
                .collect();
            let first = order.iter().position(|h| h.ends_with("/first/"));
            let second = order.iter().position(|h| h.ends_with("/second/"));
            ensure!(
                first.is_some() && second.is_some() && first < second,
                "equal calendarorder must fall back to id order: {order:?}"
            );
        }
        "calendars-webcal-caching-delegated" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_calendar_subscription_full(
                "principals/users/alice", "webcal", Some("Webcal"), None, 1, None, 0, 0, 0, None, 1, "https://e/w.ics",
            )
            .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            // The explicit header turns caching on.
            let resp = propfind_with_headers(
                &app,
                "/remote.php/dav/calendars/alice/",
                USER,
                PASSWORD,
                "1",
                body,
                &[("X-NC-CalDAV-Webcal-Caching", "On")],
            )
            .await;
            ensure!(
                resp.status == 501,
                "the caching header must delegate the subscription listing, got {}",
                resp.status
            );
            // A KDE KIO user agent also turns caching on.
            let resp = propfind_with_headers(
                &app,
                "/remote.php/dav/calendars/alice/",
                USER,
                PASSWORD,
                "1",
                body,
                &[("User-Agent", "Mozilla/5.0 (X11; Linux) KIO/5.0")],
            )
            .await;
            ensure!(
                resp.status == 501,
                "a KIO user agent must delegate the subscription listing, got {}",
                resp.status
            );
            // Without caching the plain subscription listing is served.
            let resp = propfind(&app, "/remote.php/dav/calendars/alice/", USER, PASSWORD, "1", body).await;
            ensure!(
                resp.status == 207,
                "without caching the listing must be served, got {}",
                resp.status
            );
        }
        "calendars-own-home-only" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_user("bob", Some("Bob B")).await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            let resp = propfind(&app, "/remote.php/dav/calendars/bob/", USER, PASSWORD, "1", body).await;
            ensure!(
                resp.status == 501,
                "another principal's home must delegate, got {}",
                resp.status
            );
        }
        "calendars-shared-listing-order" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_user("bob", Some("Bob B")).await;
            // Insert a high-id calendar first and a low-id one second.
            let first = env
                .seed_calendar("principals/users/bob", "first", Some("First"), 0, None, Some("VEVENT"), false, 1)
                .await;
            let second = env
                .seed_calendar("principals/users/bob", "second", Some("Second"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_share("principals/users/alice", 3, first).await;
            env.seed_calendar_share("principals/users/alice", 3, second).await;
            let groups = env.db.group_principals(USER).await.unwrap();
            let calendars = env
                .db
                .visible_calendars("principals/users/alice", &groups)
                .await
                .unwrap();
            let uris: Vec<&str> = calendars.iter().map(|c| c.wire_uri.as_str()).collect();
            ensure!(
                uris == vec!["first_shared_by_bob", "second_shared_by_bob"],
                "shared calendars must be ordered by a.id, got {uris:?}"
            );
        }
        "calendars-personal-displayname-localized" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_calendar("principals/users/alice", "personal", Some("Personal"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar("principals/users/alice", "contact_birthdays", Some("Contact birthdays"), 1, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar("principals/users/alice", "other", Some("Personal"), 2, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_preference(USER, "core", "lang", "fr").await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;

            // A throwaway `dav` l10n tree carrying the real French strings.
            let dir = std::env::temp_dir().join(format!("ncdav-l10n-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("fr.json"),
                r#"{"translations":{"Personal":"Personnel","Contact birthdays":"Anniversaires des contacts"},"pluralForm":"nplurals=2; plural=(n > 1);"}"#,
            )
            .unwrap();
            let app = env.app_shared_l10n_dir(dir);

            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>"#;
            for (path, expected) in [
                (
                    "/remote.php/dav/calendars/alice/personal/",
                    "Personnel",
                ),
                (
                    "/remote.php/dav/calendars/alice/contact_birthdays/",
                    "Anniversaires des contacts",
                ),
                // The rewrite is keyed on the *uri*; another calendar whose
                // stored name happens to be `Personal` is untouched.
                ("/remote.php/dav/calendars/alice/other/", "Personal"),
            ] {
                let resp = propfind(&app, path, USER, PASSWORD, "0", body).await;
                let d = doc(&resp.body);
                let r = response(&d, path).unwrap();
                ensure!(
                    prop_text(r, NS_DAV, "displayname").as_deref() == Some(expected),
                    "{path} must serve {expected:?}, got {:?}",
                    prop_text(r, NS_DAV, "displayname")
                );
            }
        }
        "calendars-group-share-acl-delegated" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_user("bob", Some("Bob B")).await;
            env.seed_group("team").await;
            env.seed_group_member("team", USER).await;
            let cal = env
                .seed_calendar("principals/users/bob", "teamcal", Some("Team"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_share("principals/groups/team", 2, cal).await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let path = "/remote.php/dav/calendars/alice/teamcal_shared_by_bob/";
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:acl/></d:prop></d:propfind>"#;
            let resp = propfind(&app, path, USER, PASSWORD, "0", body).await;
            ensure!(
                resp.status == 501,
                "a group-share ACL must delegate, got {}",
                resp.status
            );
        }
        "calendars-objects-and-query-delegated" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_calendar("principals/users/alice", "work", Some("Work"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
            for path in [
                "/remote.php/dav/calendars/alice/work/x.ics",
                "/remote.php/dav/calendars/alice/trashbin/",
                "/remote.php/dav/calendars/alice/inbox/",
            ] {
                let resp = propfind(&app, path, USER, PASSWORD, "0", body).await;
                ensure!(resp.status == 501, "{path} must delegate, got {}", resp.status);
            }
            // The object GET and `calendar-query` still delegate.
            let resp = get(&app, "/remote.php/dav/calendars/alice/work/x.ics", USER, PASSWORD).await;
            ensure!(resp.status == 501, "object GET must delegate, got {}", resp.status);
            let query = r#"<cal:calendar-query xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/></d:prop><cal:filter><cal:comp-filter name="VCALENDAR"/></cal:filter></cal:calendar-query>"#;
            let resp = report(&app, "/remote.php/dav/calendars/alice/work/", USER, PASSWORD, query).await;
            ensure!(resp.status == 501, "calendar-query must delegate, got {}", resp.status);
        }
        "calendars-report-shared-delegated" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            env.seed_user("bob", Some("Bob B")).await;
            let cal = env
                .seed_calendar("principals/users/bob", "bobcal", Some("Bob Cal"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_share("principals/users/alice", 3, cal).await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let body = r#"<d:sync-collection xmlns:d="DAV:"><d:sync-token/><d:prop><d:getetag/></d:prop></d:sync-collection>"#;
            let resp = report(
                &app,
                "/remote.php/dav/calendars/alice/bobcal_shared_by_bob/",
                USER,
                PASSWORD,
                body,
            )
            .await;
            ensure!(resp.status == 501, "a shared REPORT must delegate, got {}", resp.status);

            // A trashed calendar, too.
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            let cal = env
                .seed_calendar("principals/users/alice", "personal", Some("Personal"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.trash_calendar(cal, 1_700_000_000).await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let resp = report(
                &app,
                "/remote.php/dav/calendars/alice/personal/",
                USER,
                PASSWORD,
                body,
            )
            .await;
            ensure!(resp.status == 501, "a trashed REPORT must delegate, got {}", resp.status);
        }
        "calendars-report-expand-json-delegated" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            let cal = env
                .seed_calendar("principals/users/alice", "work", Some("Work"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_object(cal, "e.ics", b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", "VEVENT", 0)
                .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let path = "/remote.php/dav/calendars/alice/work/";
            let expand = r#"<cal:calendar-multiget xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><cal:calendar-data><cal:expand start="20260101T000000Z" end="20270101T000000Z"/></cal:calendar-data></d:prop><d:href>/remote.php/dav/calendars/alice/work/e.ics</d:href></cal:calendar-multiget>"#;
            let resp = report(&app, path, USER, PASSWORD, expand).await;
            ensure!(resp.status == 501, "expand must delegate, got {}", resp.status);
            let json = r#"<cal:calendar-multiget xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><cal:calendar-data content-type="application/calendar+json"/></d:prop><d:href>/remote.php/dav/calendars/alice/work/e.ics</d:href></cal:calendar-multiget>"#;
            let resp = report(&app, path, USER, PASSWORD, json).await;
            ensure!(resp.status == 501, "calendar+json must delegate, got {}", resp.status);
        }
        "calendars-report-property-gate-501" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            let cal = env
                .seed_calendar("principals/users/alice", "work", Some("Work"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_object(cal, "e.ics", b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", "VEVENT", 0)
                .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let path = "/remote.php/dav/calendars/alice/work/";
            // An unimplemented property delegates.
            let body = r#"<cal:calendar-multiget xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:x="http://example.com/ns"><d:prop><d:getetag/><x:whatever/></d:prop><d:href>/remote.php/dav/calendars/alice/work/e.ics</d:href></cal:calendar-multiget>"#;
            let resp = report(&app, path, USER, PASSWORD, body).await;
            ensure!(resp.status == 501, "an unknown REPORT prop must delegate, got {}", resp.status);
            // A known-404 property stays a 404 propstat, not a 501.
            let body = r#"<cal:calendar-multiget xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><cal:schedule-tag/></d:prop><d:href>/remote.php/dav/calendars/alice/work/e.ics</d:href></cal:calendar-multiget>"#;
            let resp = report(&app, path, USER, PASSWORD, body).await;
            ensure!(resp.status == 207, "schedule-tag must be served, got {}", resp.status);
            let d = doc(&resp.body);
            let r = response(&d, "/remote.php/dav/calendars/alice/work/e.ics").unwrap();
            ensure!(
                propstat_404(r, "urn:ietf:params:xml:ns:caldav", "schedule-tag"),
                "schedule-tag must be a 404 propstat"
            );
        }
        "calendars-sync-nresults-zero" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            let cal = env
                .seed_calendar("principals/users/alice", "work", Some("Work"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_object(cal, "a.ics", b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", "VEVENT", 0)
                .await;
            env.seed_calendar_object(cal, "b.ics", b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", "VEVENT", 0)
                .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let path = "/remote.php/dav/calendars/alice/work/";
            // `setMaxResults(0)` => zero rows, and because `$limit` is falsy the
            // initial-sync 507 must NOT fire.
            for token in ["", "http://sabre.io/ns/sync/1"] {
                let body = format!(
                    r#"<d:sync-collection xmlns:d="DAV:"><d:sync-token>{token}</d:sync-token><d:limit><d:nresults>0</d:nresults></d:limit><d:prop><d:getetag/></d:prop></d:sync-collection>"#
                );
                let resp = report(&app, path, USER, PASSWORD, &body).await;
                ensure!(
                    resp.status == 207,
                    "nresults=0 with token {token:?} must be 207, got {}",
                    resp.status
                );
                let d = doc(&resp.body);
                ensure!(
                    responses(&d).is_empty(),
                    "nresults=0 with token {token:?} must return zero responses, got {}",
                    responses(&d).len()
                );
            }
        }
        "calendars-sync-float-token" => {
            let env = match TestEnv::new().await {
                Some(env) => env,
                None => return Ok(()),
            };
            env.seed_user(USER, Some("Alice A")).await;
            let cal = env
                .seed_calendar("principals/users/alice", "work", Some("Work"), 0, None, Some("VEVENT"), false, 1)
                .await;
            env.seed_calendar_object(cal, "a.ics", b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", "VEVENT", 0)
                .await;
            env.seed_token(USER, USER, PASSWORD, 1, 2).await;
            let app = env.app_shared();
            let path = "/remote.php/dav/calendars/alice/work/";
            // `is_numeric('1.5')`/`is_numeric('1e3')` are true, so PHP treats
            // them as *incremental* and its own query rejects the raw token.
            for token in ["1.5", "1e3"] {
                let body = format!(
                    r#"<d:sync-collection xmlns:d="DAV:"><d:sync-token>http://sabre.io/ns/sync/{token}</d:sync-token><d:prop><d:getetag/></d:prop></d:sync-collection>"#
                );
                let resp = report(&app, path, USER, PASSWORD, &body).await;
                ensure!(
                    resp.status == 500,
                    "token {token:?} must be incremental and fail like PHP (500), got {}",
                    resp.status
                );
            }
        }
        other => {
            return Err(format!(
                "no assertion implemented for declared id {other:?}"
            ))
        }
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
