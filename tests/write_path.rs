// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native `PUT`/`DELETE` write path: HTTP semantics, the `oc_cards` +
//! `oc_addressbookchanges` + `oc_cards_properties` transaction, and the
//! event-outbox row.
//!
//! The router is the production one (native writes enabled, outbox table
//! present) and the database is a throwaway PostgreSQL cluster. Tests skip when
//! PostgreSQL is unavailable.

mod common;

use axum::body::Body;
use axum::http::{header, Request};
use common::{call, md5_hex, Resp, TestEnv};
use sqlx::Row;

const USER: &str = "alice";
const PASSWORD: &str = "app-password";
const BOOK: &str = "/remote.php/dav/addressbooks/users/alice/contacts";

const CARD_JANE: &[u8] =
    b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:jane-1\r\nFN:Jane Doe\r\nEND:VCARD\r\n";
const CARD_JANE_V2: &[u8] =
    b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:jane-1\r\nFN:Jane Updated\r\nEND:VCARD\r\n";

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
    env.seed_card(book, "jane.vcf", CARD_JANE).await;
    env.seed_token(USER, USER, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    Some((env, book, app))
}

fn card_path(uri: &str) -> String {
    format!("{BOOK}/{uri}")
}

/// PUT with a body and optional extra headers.
async fn put(app: &axum::Router, path: &str, body: &[u8], extra: &[(&str, &str)]) -> Resp {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(path)
        .header(header::AUTHORIZATION, common::basic(USER, PASSWORD))
        .header(header::CONTENT_TYPE, "text/vcard; charset=utf-8");
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    call(app, builder.body(Body::from(body.to_vec())).unwrap()).await
}

async fn delete(app: &axum::Router, path: &str, extra: &[(&str, &str)]) -> Resp {
    let mut builder = Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, common::basic(USER, PASSWORD));
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    call(app, builder.body(Body::empty()).unwrap()).await
}

/// One `oc_cards` row: `(carddata, etag, size, uid, lastmodified)`.
async fn card_row(
    env: &TestEnv,
    book: i64,
    uri: &str,
) -> Option<(Vec<u8>, String, i64, String, i64)> {
    let sql = format!(
        "SELECT carddata, etag, size, uid, lastmodified FROM {}cards \
         WHERE addressbookid = ? AND uri = ? LIMIT 1",
        env.prefix
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(book)
        .bind(uri)
        .fetch_optional(env.pool())
        .await
        .unwrap()?;
    Some((
        row.try_get("carddata").unwrap(),
        row.try_get("etag").unwrap(),
        row.try_get("size").unwrap(),
        row.try_get::<Option<String>, _>("uid")
            .unwrap()
            .unwrap_or_default(),
        row.try_get("lastmodified").unwrap(),
    ))
}

async fn synctoken(env: &TestEnv, book: i64) -> i64 {
    let sql = format!(
        "SELECT synctoken FROM {}addressbooks WHERE id = ?",
        env.prefix
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(book)
        .fetch_one(env.pool())
        .await
        .unwrap()
        .try_get("synctoken")
        .unwrap()
}

/// The most recent change row for a book: `(uri, operation, synctoken)`.
async fn last_change(env: &TestEnv, book: i64) -> (String, i64, i64) {
    let sql = format!(
        "SELECT uri, operation, synctoken FROM {}addressbookchanges \
         WHERE addressbookid = ? ORDER BY id DESC LIMIT 1",
        env.prefix
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(book)
        .fetch_one(env.pool())
        .await
        .unwrap();
    (
        row.try_get::<Option<String>, _>("uri")
            .unwrap()
            .unwrap_or_default(),
        row.try_get("operation").unwrap(),
        row.try_get("synctoken").unwrap(),
    )
}

/// The most recent outbox row:
/// `(event_type, card_uri, card_row_json, card_data, effects)`.
async fn last_outbox(env: &TestEnv) -> (i64, String, String, Vec<u8>, String) {
    let sql = format!(
        "SELECT event_type, card_uri, card_row, card_data, effects FROM {}dav_event_outbox \
         ORDER BY seq DESC LIMIT 1",
        env.prefix
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(env.pool())
        .await
        .unwrap();
    (
        row.try_get("event_type").unwrap(),
        row.try_get("card_uri").unwrap(),
        row.try_get("card_row").unwrap(),
        row.try_get("card_data").unwrap(),
        row.try_get("effects").unwrap(),
    )
}

async fn outbox_count(env: &TestEnv) -> i64 {
    env.count("dav_event_outbox").await
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_card_201_etag_rows_properties_and_outbox() {
    let (env, book, app) = setup!();
    let before_token = synctoken(&env, book).await;

    let long_note = "€".repeat(200);
    let body = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:pref-1\r\nFN:Pref Person\r\n\
         EMAIL;TYPE=PREF:pref@example.com\r\nEMAIL;TYPE=WORK:work@example.com\r\n\
         NOTE:{long_note}\r\nEND:VCARD\r\n"
    );
    let path = card_path("pref.vcf");
    let resp = put(&app, &path, body.as_bytes(), &[]).await;
    assert_eq!(resp.status, 201, "{}", resp.text());
    let etag = resp.header("etag").expect("ETag header");
    assert_eq!(etag, format!("\"{}\"", md5_hex(body.as_bytes())));

    let (carddata, stored_etag, size, uid, lastmodified) =
        card_row(&env, book, "pref.vcf").await.expect("card row");
    assert_eq!(carddata, body.as_bytes());
    assert_eq!(stored_etag, md5_hex(body.as_bytes()));
    assert_eq!(size, body.len() as i64);
    assert_eq!(uid, "pref-1");
    assert!(lastmodified > 0);

    // addChange(1): the change row carries the pre-increment token.
    let (uri, operation, token) = last_change(&env, book).await;
    assert_eq!(uri, "pref.vcf");
    assert_eq!(operation, 1);
    assert_eq!(token, before_token);
    assert_eq!(synctoken(&env, book).await, before_token + 1);

    // Indexed properties: TYPE=PREF and mb_strcut(…, 254).
    let sql = format!(
        "SELECT name, value, preferred FROM {}cards_properties \
         WHERE cardid = (SELECT id FROM {}cards WHERE addressbookid = ? AND uri = ?) \
         ORDER BY id",
        env.prefix, env.prefix
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(book)
        .bind("pref.vcf")
        .fetch_all(env.pool())
        .await
        .unwrap();
    let mut pref = None;
    let mut note = None;
    for row in &rows {
        let name: String = row.try_get("name").unwrap();
        let value: String = row.try_get("value").unwrap();
        let preferred: i32 = row.try_get("preferred").unwrap();
        match name.as_str() {
            "EMAIL" if value == "pref@example.com" => pref = Some(preferred),
            "EMAIL" if value == "work@example.com" => assert_eq!(preferred, 0),
            "NOTE" => note = Some(value),
            _ => {}
        }
    }
    assert_eq!(pref, Some(1), "TYPE=PREF must set preferred = 1");
    let note = note.expect("NOTE indexed");
    // 200 × 3-byte '€' truncates to 84 code points / 252 bytes.
    assert_eq!(note.len(), 252);
    assert!(note.chars().all(|c| c == '€'));

    // The outbox row is present and carries the filtered snapshot.
    let (event_type, out_uri, card_row, card_data, effects) = last_outbox(&env).await;
    assert_eq!(event_type, 1);
    assert_eq!(out_uri, "pref.vcf");
    let row: serde_json::Value = serde_json::from_str(&card_row).unwrap();
    assert_eq!(row["uid"], "pref-1");
    assert_eq!(row["etag"], etag);
    assert_eq!(card_data, body.as_bytes());
    assert_eq!(
        effects,
        // A create only triggers the listeners registered for
        // CardCreatedEvent: photo_cache is update/delete-only and
        // redis_cloud_id is update-only.
        r#"{"php":["activity_stream","activity_mail","notification_push","birthday_calendar","calendar_reminders"],"rust":[]}"#
    );
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_card_204_changes_etag_and_logs_operation_2() {
    let (env, book, app) = setup!();
    let old_etag = card_row(&env, book, "jane.vcf").await.unwrap().1;
    let before_token = synctoken(&env, book).await;

    let path = card_path("jane.vcf");
    let resp = put(&app, &path, CARD_JANE_V2, &[]).await;
    assert_eq!(resp.status, 204, "{}", resp.text());
    let new_etag = resp.header("etag").unwrap();
    assert_ne!(new_etag, format!("\"{old_etag}\""));

    let (carddata, stored_etag, _, _, _) = card_row(&env, book, "jane.vcf").await.unwrap();
    assert_eq!(carddata, CARD_JANE_V2);
    assert_eq!(stored_etag, md5_hex(CARD_JANE_V2));
    assert_eq!(new_etag, format!("\"{}\"", md5_hex(CARD_JANE_V2)));

    let (uri, operation, token) = last_change(&env, book).await;
    assert_eq!(uri, "jane.vcf");
    assert_eq!(operation, 2);
    assert_eq!(token, before_token);
    assert_eq!(synctoken(&env, book).await, before_token + 1);

    let (event_type, out_uri, _, card_data, _) = last_outbox(&env).await;
    assert_eq!(event_type, 2);
    assert_eq!(out_uri, "jane.vcf");
    assert_eq!(card_data, CARD_JANE_V2);
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_card_204_purges_properties_and_queues_pre_delete_snapshot() {
    let (env, book, app) = setup!();
    let card_id: i64 = {
        let sql = format!(
            "SELECT id FROM {}cards WHERE addressbookid = ? AND uri = 'jane.vcf'",
            env.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(book)
            .fetch_one(env.pool())
            .await
            .unwrap()
            .try_get("id")
            .unwrap()
    };
    // Give the card an indexed property so the purge is observable.
    env.seed_property(book, card_id, "FN", "Jane Doe").await;
    let before_token = synctoken(&env, book).await;

    let path = card_path("jane.vcf");
    let resp = delete(&app, &path, &[]).await;
    assert_eq!(resp.status, 204, "{}", resp.text());

    assert!(card_row(&env, book, "jane.vcf").await.is_none());
    let props: i64 = {
        let sql = format!(
            "SELECT COUNT(*) AS c FROM {}cards_properties WHERE cardid = ?",
            env.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(card_id)
            .fetch_one(env.pool())
            .await
            .unwrap()
            .try_get("c")
            .unwrap()
    };
    assert_eq!(props, 0, "properties must be purged on delete");

    let (uri, operation, token) = last_change(&env, book).await;
    assert_eq!(uri, "jane.vcf");
    assert_eq!(operation, 3);
    assert_eq!(token, before_token);
    assert_eq!(synctoken(&env, book).await, before_token + 1);

    let (event_type, out_uri, card_row, card_data, _) = last_outbox(&env).await;
    assert_eq!(event_type, 3);
    assert_eq!(out_uri, "jane.vcf");
    let row: serde_json::Value = serde_json::from_str(&card_row).unwrap();
    assert_eq!(row["etag"], format!("\"{}\"", md5_hex(CARD_JANE)));
    assert_eq!(card_data, CARD_JANE);
}

// ---------------------------------------------------------------------------
// Preconditions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn if_match_mismatch_is_412_and_match_succeeds() {
    let (_env, _book, app) = setup!();
    let path = card_path("jane.vcf");

    let mismatch = put(&app, &path, CARD_JANE_V2, &[("If-Match", "\"nope\"")]).await;
    assert_eq!(mismatch.status, 412, "{}", mismatch.text());

    let etag = format!("\"{}\"", md5_hex(CARD_JANE));
    let matched = put(&app, &path, CARD_JANE_V2, &[("If-Match", &etag)]).await;
    assert_eq!(matched.status, 204, "{}", matched.text());

    // The unquoted stored ETag also matches.
    let unquoted = md5_hex(CARD_JANE_V2);
    let matched = put(&app, &path, CARD_JANE, &[("If-Match", &unquoted)]).await;
    assert_eq!(matched.status, 204, "{}", matched.text());
}

#[tokio::test]
async fn if_none_match_star_blocks_existing_and_allows_create() {
    let (_env, _book, app) = setup!();
    let existing = put(
        &app,
        &card_path("jane.vcf"),
        CARD_JANE,
        &[("If-None-Match", "*")],
    )
    .await;
    assert_eq!(existing.status, 412, "{}", existing.text());

    let created = put(
        &app,
        &card_path("fresh.vcf"),
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:fresh-1\r\nFN:Fresh\r\nEND:VCARD\r\n",
        &[("If-None-Match", "*")],
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.text());
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_uid_is_409_naming_the_conflicting_href() {
    let (env, _book, app) = setup!();
    let before = outbox_count(&env).await;
    let resp = put(
        &app,
        &card_path("dup.vcf"),
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:jane-1\r\nFN:Duplicate\r\nEND:VCARD\r\n",
        &[],
    )
    .await;
    assert_eq!(resp.status, 409, "{}", resp.text());
    assert!(resp.text().contains("no-uid-conflict"), "{}", resp.text());
    assert!(resp.text().contains("jane.vcf"), "{}", resp.text());
    assert_eq!(
        outbox_count(&env).await,
        before,
        "409 must not enqueue an event"
    );
}

/// PHP parity: `CardDavBackend::updateCard()` does **not** run the
/// no-uid-conflict check (only `createCard()` does, via Sabre's
/// `AddressBook::createFile()`), so an update that introduces a duplicate UID
/// must succeed here too. Enforcing the check on update would reject a write a
/// real Nextcloud accepts.
#[tokio::test]
async fn updating_a_card_to_a_duplicate_uid_is_allowed_like_php() {
    let (_env, _book, app) = setup!();
    let created = put(
        &app,
        &card_path("other.vcf"),
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:other-1\r\nFN:Other\r\nEND:VCARD\r\n",
        &[],
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.text());

    let resp = put(
        &app,
        &card_path("jane.vcf"),
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:other-1\r\nFN:Jane Now Clashes\r\nEND:VCARD\r\n",
        &[],
    )
    .await;
    assert_eq!(resp.status, 204, "{}", resp.text());
}

#[tokio::test]
async fn missing_uid_is_400() {
    let (_env, _book, app) = setup!();
    let resp = put(
        &app,
        &card_path("nouid.vcf"),
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:No Uid\r\nEND:VCARD\r\n",
        &[],
    )
    .await;
    assert_eq!(resp.status, 400, "{}", resp.text());
}

#[tokio::test]
async fn bad_version_is_415() {
    let (_env, _book, app) = setup!();
    let resp = put(
        &app,
        &card_path("badver.vcf"),
        b"BEGIN:VCARD\r\nVERSION:2.1\r\nUID:old\r\nFN:Old\r\nEND:VCARD\r\n",
        &[],
    )
    .await;
    assert_eq!(resp.status, 415, "{}", resp.text());
}

#[tokio::test]
async fn oversized_body_is_403() {
    let (_env, _book, app) = setup!();
    let mut body = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:big-1\r\nFN:Big\r\nNOTE:".to_vec();
    body.resize(5_242_881, b'a');
    body.extend_from_slice(b"\r\nEND:VCARD\r\n");
    let resp = put(&app, &card_path("big.vcf"), &body, &[]).await;
    assert_eq!(resp.status, 403, "{}", resp.text());
}

#[tokio::test]
async fn empty_body_is_plain_text_415() {
    let (_env, _book, app) = setup!();
    let resp = put(&app, &card_path("empty.vcf"), b"", &[]).await;
    assert_eq!(resp.status, 415, "{}", resp.text());
    assert!(
        !resp.text().starts_with("<?xml"),
        "empty body must use a plain-text 415: {}",
        resp.text()
    );
}

#[tokio::test]
async fn latin1_body_is_transcoded_to_utf8() {
    let (env, book, app) = setup!();
    let body = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:latin-1\r\nFN:Caf\xe9\r\nEND:VCARD\r\n";
    let resp = put(&app, &card_path("latin.vcf"), body, &[]).await;
    assert_eq!(resp.status, 201, "{}", resp.text());
    let (carddata, _, _, _, _) = card_row(&env, book, "latin.vcf").await.unwrap();
    assert!(
        carddata.windows(4).any(|w| w == b"Caf\xc3\xa9"),
        "stored bytes are not UTF-8: {:?}",
        String::from_utf8_lossy(&carddata)
    );
}

// ---------------------------------------------------------------------------
// Atomicity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_write_rolls_back_both_the_card_and_the_outbox_row() {
    let (env, book, app) = setup!();
    // Force a failure inside the transaction, after the card INSERT and the
    // change log, by making the indexed-property insert collide.
    let ddl = format!(
        "CREATE UNIQUE INDEX test_unique_property ON {}cards_properties (cardid, name)",
        env.prefix
    );
    sqlx::query(sqlx::AssertSqlSafe(ddl))
        .execute(env.pool())
        .await
        .unwrap();

    let before_cards = env.count("cards").await;
    let before_outbox = outbox_count(&env).await;

    // Two EMAIL properties both pass validation but violate the test index.
    let resp = put(
        &app,
        &card_path("collide.vcf"),
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:collide-1\r\nFN:Collide\r\n\
          EMAIL:a@example.com\r\nEMAIL:b@example.com\r\nEND:VCARD\r\n",
        &[],
    )
    .await;
    assert_eq!(resp.status, 500, "{}", resp.text());

    assert_eq!(
        env.count("cards").await,
        before_cards,
        "card insert was not rolled back"
    );
    assert_eq!(
        outbox_count(&env).await,
        before_outbox,
        "outbox row was not rolled back"
    );
    assert!(card_row(&env, book, "collide.vcf").await.is_none());
}
