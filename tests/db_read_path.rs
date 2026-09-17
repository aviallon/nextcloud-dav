// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! DB-layer parity: the exact SQL result shapes `CardDavBackend` returns.
//!
//! Every test here seeds a throwaway PostgreSQL schema and calls the sidecar's
//! library layer directly. Tests **skip** when PostgreSQL is unavailable.

mod common;

use common::{md5_hex, now, TestEnv};

const CARD: &[u8] = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:1\r\nFN:Jane Doe\r\nEMAIL:jane@example.com\r\nEND:VCARD\r\n";

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

#[tokio::test]
async fn address_books_are_scoped_and_ordered_by_id() {
    let env = env_or_skip!();
    env.seed_user("alice", Some("Alice")).await;
    let b1 = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            3,
        )
        .await;
    let b2 = env
        .seed_addressbook(
            "principals/users/alice",
            "work",
            Some("Work"),
            Some("Work book"),
            9,
        )
        .await;
    let _other = env
        .seed_addressbook("principals/users/bob", "contacts", Some("Bob"), None, 1)
        .await;

    let books = env
        .db
        .address_books_for_user("principals/users/alice")
        .await
        .unwrap();
    assert_eq!(books.len(), 2);
    assert_eq!(books[0].id, b1);
    assert_eq!(books[1].id, b2);
    assert_eq!(books[1].description.as_deref(), Some("Work book"));
    assert_eq!(books[1].synctoken, 9);

    assert!(env
        .db
        .address_book_by_uri("principals/users/alice", "work")
        .await
        .unwrap()
        .is_some());
    // Another user's book is invisible: this is the 404-not-403 rule.
    assert!(env
        .db
        .address_book_by_uri("principals/users/bob", "work")
        .await
        .unwrap()
        .is_none());
    assert!(env
        .db
        .address_book_by_uri("principals/users/alice", "missing")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn card_reads_quote_etag_and_keep_stored_size_unless_filtered() {
    let env = env_or_skip!();
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            1,
        )
        .await;
    env.seed_card(book, "jane.vcf", CARD).await;

    let card = env.db.card(book, "jane.vcf").await.unwrap().unwrap();
    // `oc_cards.etag` is the raw md5; the wire form is quoted.
    assert_eq!(card.etag, md5_hex(CARD));
    assert_eq!(card.quoted_etag(), format!("\"{}\"", md5_hex(CARD)));
    assert_eq!(card.carddata, CARD);
    assert_eq!(card.size, CARD.len() as i64);
}

#[tokio::test]
async fn non_image_photo_is_stripped_and_size_recomputed() {
    let env = env_or_skip!();
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            1,
        )
        .await;
    let raw = b"BEGIN:VCARD\r\nUID:1\r\nPHOTO:data:text/plain;base64,AAAA\r\n AAAA\r\nFN:X\r\nEND:VCARD\r\n";
    // PHP's createCard stores strlen($cardData) (unfiltered).
    env.seed_card(book, "x.vcf", raw).await;

    let card = env.db.card(book, "x.vcf").await.unwrap().unwrap();
    let filtered = b"BEGIN:VCARD\r\nUID:1\r\nFN:X\r\nEND:VCARD\r\n";
    assert_eq!(card.carddata, filtered);
    // `readBlob` sets modified=true, so PHP recomputes size from the filtered
    // bytes; the sidecar does the same.
    assert_eq!(card.size, filtered.len() as i64);
    // ETag stays the stored one (PHP never recomputes it on read).
    assert_eq!(card.etag, md5_hex(raw));
}

#[tokio::test]
async fn stored_size_is_not_recomputed_when_nothing_was_filtered() {
    let env = env_or_skip!();
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            1,
        )
        .await;
    env.insert_card_raw(book, "x.vcf", CARD).await;
    // Simulate a stored size that differs from the body (e.g. legacy row).
    let sql = format!(
        "UPDATE {}cards SET size = 9999 WHERE uri = 'x.vcf'",
        env.prefix
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(env.pool())
        .await
        .unwrap();

    let card = env.db.card(book, "x.vcf").await.unwrap().unwrap();
    assert_eq!(card.size, 9999, "unmodified read must keep the stored size");
}

#[tokio::test]
async fn cards_and_cards_by_uris_are_book_scoped() {
    let env = env_or_skip!();
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            1,
        )
        .await;
    let other = env
        .seed_addressbook("principals/users/alice", "work", Some("Work"), None, 1)
        .await;
    env.seed_card(book, "a.vcf", CARD).await;
    let card2 = b"BEGIN:VCARD\r\nUID:2\r\nFN:Two\r\nEND:VCARD\r\n";
    env.seed_card(book, "b.vcf", card2).await;
    env.seed_card(
        other,
        "a.vcf",
        b"BEGIN:VCARD\r\nUID:9\r\nFN:Other\r\nEND:VCARD\r\n",
    )
    .await;

    let cards = env.db.cards(book).await.unwrap();
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].uri, "a.vcf");

    let by_uri = env
        .db
        .cards_by_uris(book, &["b.vcf".to_string(), "missing.vcf".to_string()])
        .await
        .unwrap();
    assert_eq!(by_uri.len(), 1);
    assert_eq!(by_uri[0].uri, "b.vcf");
}

#[tokio::test]
async fn contact_groups_come_from_cards_properties_categories() {
    let env = env_or_skip!();
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            1,
        )
        .await;
    let card = env.seed_card(book, "a.vcf", CARD).await;
    env.seed_property(book, card, "CATEGORIES", "Friends").await;
    env.seed_property(book, card, "CATEGORIES", "Work").await;
    env.seed_property(book, card, "CATEGORIES", "Friends").await;
    env.seed_property(book, card, "CATEGORIES", "").await;
    env.seed_property(book, card, "FN", "Jane Doe").await;

    let groups = env.db.contact_groups(book).await.unwrap();
    assert_eq!(groups, vec!["Friends", "Work"]);
}

#[tokio::test]
async fn sync_initial_cards_respects_after_id_and_limit() {
    let env = env_or_skip!();
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            1,
        )
        .await;
    let c1 = env.insert_card_raw(book, "a.vcf", CARD).await;
    let c2 = env.insert_card_raw(book, "b.vcf", CARD).await;
    let c3 = env.insert_card_raw(book, "c.vcf", CARD).await;

    let rows = env.db.sync_initial_cards(book, 0, 10).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![c1, c2, c3]
    );
    let rows = env.db.sync_initial_cards(book, c1, 10).await.unwrap();
    assert_eq!(rows[0].id, c2);
    let rows = env.db.sync_initial_cards(book, 0, 2).await.unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn sync_changes_are_ranged_and_ordered() {
    let env = env_or_skip!();
    let book = env
        .seed_addressbook(
            "principals/users/alice",
            "contacts",
            Some("Contacts"),
            None,
            1,
        )
        .await;
    // addChange() logs pre-increment tokens.
    env.add_change(book, "a.vcf", 1).await; // token 1
    env.add_change(book, "b.vcf", 1).await; // token 2
    env.add_change(book, "a.vcf", 2).await; // token 3
    let current = env
        .db
        .address_book_by_uri("principals/users/alice", "contacts")
        .await
        .unwrap()
        .unwrap()
        .synctoken;
    assert_eq!(current, 4);

    let rows = env.db.sync_changes(book, 0, current, 100).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.synctoken).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    // [2,4) excludes token 1.
    let rows = env.db.sync_changes(book, 2, current, 100).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>(),
        vec!["b.vcf", "a.vcf"]
    );
}

#[tokio::test]
async fn user_display_name_and_enabled_flag() {
    let env = env_or_skip!();
    env.seed_user("alice", Some("Alice A")).await;
    env.seed_user("bob", None).await;
    env.seed_user("carol", Some("Carol")).await;
    env.disable_user("carol").await;

    assert_eq!(
        env.db.user_display_name("alice").await.unwrap().as_deref(),
        Some("Alice A")
    );
    assert_eq!(env.db.user_display_name("bob").await.unwrap(), None);
    assert_eq!(env.db.user_display_name("ghost").await.unwrap(), None);
    assert!(env.db.native_user_exists("alice").await.unwrap());
    assert!(!env.db.native_user_exists("ghost").await.unwrap());
    assert!(!env.db.user_is_disabled("alice").await.unwrap());
    assert!(env.db.user_is_disabled("carol").await.unwrap());
}

#[tokio::test]
async fn authtoken_lookup_filters_on_version_2() {
    let env = env_or_skip!();
    let hash = nextcloud_dav::auth::token::hash_token("pw", &env.secret);
    // Insert a v1 row first: a lookup keyed only on the token must not match.
    let sql = format!(
        "INSERT INTO {}authtoken (uid, login_name, name, token, type, last_check, version, password_invalid) \
         VALUES ('alice', 'alice', '', ?, 1, ?, 1, false)",
        env.prefix
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&hash)
        .bind(now())
        .execute(env.pool())
        .await
        .unwrap();
    assert!(env.db.authtoken_by_hash(&hash).await.unwrap().is_none());

    // Then a v2 row with the same token value is found.
    env.seed_token("alice", "alice", "pw", 1, 2).await;
    let row = env.db.authtoken_by_hash(&hash).await.unwrap().unwrap();
    assert_eq!(row.uid, "alice");
    assert_eq!(row.token_type, 1);
    assert!(!row.password_invalid);
}

#[tokio::test]
async fn bruteforce_count_is_scoped_by_subnet_and_window() {
    let env = env_or_skip!();
    let sql = format!(
        "INSERT INTO {}bruteforce_attempts (ip, subnet, occurred, action, metadata) \
         VALUES ('1.2.3.4', '1.2.3.4/32', ?, 'login', '{{}}')",
        env.prefix
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(now())
        .execute(env.pool())
        .await
        .unwrap();

    let count = env
        .db
        .bruteforce_attempts("1.2.3.4/32", "login", now() - 43_200)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let count = env
        .db
        .bruteforce_attempts("9.9.9.9/32", "login", now() - 43_200)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let count = env
        .db
        .bruteforce_attempts("1.2.3.4/32", "login", now() + 10)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
