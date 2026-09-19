// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CalDAV read path, part 2: the `sync-collection` and `calendar-multiget`
//! REPORTs on one owned calendar, through the real axum router.
//!
//! Tests **skip** when PostgreSQL is unavailable.

mod common;

use common::{report, TestEnv};
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{NS_CALDAV, NS_DAV, NS_NEXTCLOUD, NS_OWNCLOUD};

const ALICE: &str = "alice";
const PASSWORD: &str = "app-password";
const CAL_PATH: &str = "/remote.php/dav/calendars/alice/work/";

/// A minimal event with CRLF line endings, exactly as stored.
const EVENT_ONE: &[u8] = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//probe//EN\r\nBEGIN:VEVENT\r\nUID:one\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260101T100000Z\r\nDTEND:20260101T110000Z\r\nSUMMARY:One\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
const EVENT_TWO: &[u8] = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//probe//EN\r\nBEGIN:VEVENT\r\nUID:two\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260201T100000Z\r\nDTEND:20260201T110000Z\r\nSUMMARY:Two\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

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

fn parse_xml(body: &[u8]) -> XNode {
    parse_document(body)
        .unwrap_or_else(|e| panic!("not XML: {e}: {}", String::from_utf8_lossy(body)))
}

fn responses(doc: &XNode) -> Vec<&XNode> {
    doc.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "response")
        .collect()
}

fn hrefs(doc: &XNode) -> Vec<String> {
    responses(doc)
        .iter()
        .filter_map(|r| r.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect()
}

fn response<'a>(doc: &'a XNode, href: &str) -> Option<&'a XNode> {
    responses(doc).into_iter().find(|r| {
        r.child(NS_DAV, "href")
            .map(|h| h.text == href)
            .unwrap_or(false)
    })
}

fn response_status(resp: &XNode) -> Option<u16> {
    resp.children
        .iter()
        .find(|c| c.ns == NS_DAV && c.local == "status")
        .and_then(|s| s.text.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
}

fn propstat<'a>(resp: &'a XNode, status: &str) -> Option<&'a XNode> {
    resp.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "propstat")
        .find(|ps| {
            ps.child(NS_DAV, "status")
                .map(|s| s.text.contains(status))
                .unwrap_or(false)
        })
}

fn prop_of<'a>(resp: &'a XNode, ns: &str, local: &str) -> Option<&'a XNode> {
    propstat(resp, "200")
        .and_then(|ps| ps.child(NS_DAV, "prop"))
        .and_then(|prop| prop.children.iter().find(|c| c.ns == ns && c.local == local))
}

fn prop_text(resp: &XNode, ns: &str, local: &str) -> String {
    prop_of(resp, ns, local)
        .map(|n| n.text.clone())
        .unwrap_or_else(|| panic!("missing prop {{{ns}}}{local}"))
}

fn sync_token(doc: &XNode) -> Option<String> {
    doc.child(NS_DAV, "sync-token").map(|n| n.text.clone())
}

fn attributed(resp: &common::Resp) -> bool {
    resp.header("x-nextcloud-dav").as_deref() == Some("sidecar")
}

async fn fixture() -> Option<(TestEnv, axum::Router, i64)> {
    let env = TestEnv::new().await?;
    env.seed_user(ALICE, Some("Alice A")).await;
    env.seed_user("bob", Some("Bob B")).await;
    let work = env
        .seed_calendar(
            "principals/users/alice",
            "work",
            Some("Work"),
            2,
            Some("#111111"),
            Some("VEVENT"),
            false,
            1,
        )
        .await;
    env.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    Some((env, app, work))
}

fn sync_body(token: &str, props: &str, limit: Option<i64>) -> String {
    let limit_xml = limit
        .map(|n| format!("<d:limit><d:nresults>{n}</d:nresults></d:limit>"))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0"?><d:sync-collection xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns"><d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level>{limit_xml}<d:prop>{props}</d:prop></d:sync-collection>"#
    )
}

fn multiget_body(props: &str, hrefs: &[&str]) -> String {
    let hrefs: String = hrefs
        .iter()
        .map(|h| format!("<d:href>{h}</d:href>"))
        .collect();
    format!(
        r#"<?xml version="1.0"?><cal:calendar-multiget xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns"><d:prop>{props}</d:prop>{hrefs}</cal:calendar-multiget>"#
    )
}

// ---------------------------------------------------------------------------
// sync-collection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_initial_lists_every_live_object() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    env.seed_calendar_object(work, "p2.ics", EVENT_TWO, "VEVENT", 0)
        .await;
    let body = sync_body("", "<d:getetag/><d:resourcetype/>", None);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    assert!(attributed(&resp));
    let doc = parse_xml(&resp.body);
    assert_eq!(
        hrefs(&doc),
        vec![
            format!("{CAL_PATH}p1.ics"),
            format!("{CAL_PATH}p2.ics"),
        ]
    );
    // The token is the calendar's current token, prefixed.
    assert_eq!(
        sync_token(&doc).as_deref(),
        Some("http://sabre.io/ns/sync/3")
    );
    let p1 = response(&doc, &format!("{CAL_PATH}p1.ics")).unwrap();
    assert_eq!(
        prop_text(p1, NS_DAV, "getetag"),
        format!("\"{}\"", common::md5_hex(EVENT_ONE))
    );
    // resourcetype is empty on an object.
    assert_eq!(prop_text(p1, NS_DAV, "resourcetype"), "");
}

#[tokio::test]
async fn sync_incremental_reports_add_modify_delete() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    // p1 is added at token 1 (the calendar token becomes 2).
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    let body = sync_body("http://sabre.io/ns/sync/1", "<d:getetag/>", None);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let doc = parse_xml(&resp.body);
    assert_eq!(hrefs(&doc), vec![format!("{CAL_PATH}p1.ics")]);

    // Modify p1 (op 2) and add p2 (op 1), then delete p1 (op 3). The window
    // [1,5) aggregates p1 to MAX(1,2,3)=3 (delete) and p2 to 1 (add).
    env.insert_calendar_object_raw(work, "p2.ics", EVENT_TWO, "VEVENT", 0)
        .await;
    env.add_calendar_change(work, "p2.ics", 1).await;
    env.add_calendar_change(work, "p1.ics", 2).await;
    env.add_calendar_change(work, "p1.ics", 3).await;
    let body = sync_body("http://sabre.io/ns/sync/1", "<d:getetag/>", None);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let doc = parse_xml(&resp.body);
    let p1 = response(&doc, &format!("{CAL_PATH}p1.ics")).unwrap();
    assert_eq!(response_status(p1), Some(404), "{}", resp.text());
    let p2 = response(&doc, &format!("{CAL_PATH}p2.ics")).unwrap();
    assert_eq!(
        prop_text(p2, NS_DAV, "getetag"),
        format!("\"{}\"", common::md5_hex(EVENT_TWO))
    );
}

#[tokio::test]
async fn sync_requires_token_and_prop() {
    let Some((_env, app, _work)) = fixture().await else {
        return;
    };
    // No sync-token element.
    let body = r#"<d:sync-collection xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:sync-collection>"#;
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, body).await;
    assert_eq!(resp.status, 400, "{}", resp.text());
    assert!(resp.text().contains("sync-token element"), "{}", resp.text());
    assert!(attributed(&resp));

    // No prop element.
    let body = r#"<d:sync-collection xmlns:d="DAV:"><d:sync-token/></d:sync-collection>"#;
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, body).await;
    assert_eq!(resp.status, 400, "{}", resp.text());
    assert!(resp.text().contains("prop element"), "{}", resp.text());
}

#[tokio::test]
async fn sync_initial_with_limit_is_507() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    let body = sync_body("", "<d:getetag/>", Some(1));
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 507, "{}", resp.text());
    assert!(
        resp.text().contains("number-of-matches-within-limits"),
        "{}",
        resp.text()
    );
    assert!(attributed(&resp));
}

#[tokio::test]
async fn sync_non_numeric_token_is_initial_with_limit() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    env.seed_calendar_object(work, "p2.ics", EVENT_TWO, "VEVENT", 0)
        .await;
    // PHP treats a non-numeric remainder as an initial sync and applies the
    // limit instead of raising.
    let body = sync_body(
        "http://sabre.io/ns/sync/abc",
        "<d:getetag/>",
        Some(1),
    );
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let doc = parse_xml(&resp.body);
    assert_eq!(hrefs(&doc).len(), 1);
    assert_eq!(sync_token(&doc).as_deref(), Some("http://sabre.io/ns/sync/3"));
}

#[tokio::test]
async fn sync_invalid_token_is_403() {
    let Some((_env, app, _work)) = fixture().await else {
        return;
    };
    let body = sync_body("bogus", "<d:getetag/>", None);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 403, "{}", resp.text());
    assert!(resp.text().contains("valid-sync-token"), "{}", resp.text());
    assert!(attributed(&resp));
}

#[tokio::test]
async fn sync_token_beyond_current_is_empty() {
    let Some((_env, app, _work)) = fixture().await else {
        return;
    };
    let body = sync_body("http://sabre.io/ns/sync/99", "<d:getetag/>", None);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let doc = parse_xml(&resp.body);
    assert!(hrefs(&doc).is_empty());
    assert_eq!(sync_token(&doc).as_deref(), Some("http://sabre.io/ns/sync/1"));
}

// ---------------------------------------------------------------------------
// calendar-multiget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiget_returns_existing_and_omits_missing() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    let props = "<d:getetag/><d:getcontenttype/><d:getcontentlength/><d:getlastmodified/><d:resourcetype/><cal:calendar-data/>";
    let body = multiget_body(
        props,
        &[
            &format!("{CAL_PATH}p1.ics"),
            &format!("{CAL_PATH}missing.ics"),
        ],
    );
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    assert!(attributed(&resp));
    let doc = parse_xml(&resp.body);
    // PHP's `Tree::getMultipleNodes()` drops the missing href entirely.
    assert_eq!(hrefs(&doc), vec![format!("{CAL_PATH}p1.ics")]);
    let p1 = response(&doc, &format!("{CAL_PATH}p1.ics")).unwrap();
    assert_eq!(
        prop_text(p1, NS_DAV, "getetag"),
        format!("\"{}\"", common::md5_hex(EVENT_ONE))
    );
    assert_eq!(
        prop_text(p1, NS_DAV, "getcontenttype"),
        "text/calendar; charset=utf-8; component=vevent"
    );
    assert_eq!(
        prop_text(p1, NS_DAV, "getcontentlength"),
        EVENT_ONE.len().to_string()
    );
    assert_eq!(
        prop_text(p1, NS_DAV, "getlastmodified"),
        "Tue, 14 Nov 2023 22:13:20 GMT"
    );
    // Sabre's CalDAV plugin strips every `\r` from the value before writing
    // it ("Taking out \r to not screw up the xml output"), so the wire body is
    // LF-only while the ETag stays `md5` of the stored CRLF bytes.
    let data = prop_text(p1, NS_CALDAV, "calendar-data");
    assert_eq!(
        data,
        String::from_utf8_lossy(EVENT_ONE).replace('\r', "")
    );
}

#[tokio::test]
async fn multiget_known_404_props_and_gate() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    // schedule-tag (DAVx5 asks for it), oc:size and nc:deleted-at are 404s.
    let props = "<d:getetag/><cal:schedule-tag/><oc:size/><nc:deleted-at/>";
    let body = multiget_body(props, &[&format!("{CAL_PATH}p1.ics")]);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let doc = parse_xml(&resp.body);
    let p1 = response(&doc, &format!("{CAL_PATH}p1.ics")).unwrap();
    assert!(prop_of(p1, NS_DAV, "getetag").is_some());
    assert!(propstat(p1, "404").is_some());
    for (ns, local) in [
        (NS_CALDAV, "schedule-tag"),
        (NS_OWNCLOUD, "size"),
        (NS_NEXTCLOUD, "deleted-at"),
    ] {
        let missing = propstat(p1, "404")
            .and_then(|ps| ps.child(NS_DAV, "prop"))
            .map(|prop| prop.children.iter().any(|c| c.ns == ns && c.local == local))
            .unwrap_or(false);
        assert!(missing, "{{{ns}}}{local} must be a 404 propstat");
    }

    // An unknown property is delegated, never a 404.
    let props = "<d:getetag/><x:whatever xmlns:x=\"http://example.com/ns\"/>";
    let body = multiget_body(props, &[&format!("{CAL_PATH}p1.ics")]);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 501, "{}", resp.text());
    assert!(!attributed(&resp));
}

#[tokio::test]
async fn multiget_chunks_one_hundred_uris() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    let mut hrefs = Vec::new();
    for i in 0..101 {
        let uri = format!("e{i:03}.ics");
        env.insert_calendar_object_raw(work, &uri, EVENT_ONE, "VEVENT", 0)
            .await;
        hrefs.push(format!("{CAL_PATH}{uri}"));
    }
    let href_refs: Vec<&str> = hrefs.iter().map(String::as_str).collect();
    let body = multiget_body("<d:getetag/>", &href_refs);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    let doc = parse_xml(&resp.body);
    assert_eq!(responses(&doc).len(), 101);
}

#[tokio::test]
async fn multiget_expand_and_json_delegate() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    let expand = r#"<d:getetag/><cal:calendar-data><cal:expand start="20260101T000000Z" end="20270101T000000Z"/></cal:calendar-data>"#;
    let body = multiget_body(expand, &[&format!("{CAL_PATH}p1.ics")]);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 501, "{}", resp.text());
    assert!(!attributed(&resp));

    let json = r#"<d:getetag/><cal:calendar-data content-type="application/calendar+json"/>"#;
    let body = multiget_body(json, &[&format!("{CAL_PATH}p1.ics")]);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 501, "{}", resp.text());
}

#[tokio::test]
async fn multiget_cross_collection_href_delegates() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    let body = multiget_body(
        "<d:getetag/>",
        &["/remote.php/dav/calendars/alice/other/p1.ics"],
    );
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 501, "{}", resp.text());
}

// ---------------------------------------------------------------------------
// Delegation guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reports_on_shared_trashed_and_other_delegate() {
    let Some((env, app, work)) = fixture().await else {
        return;
    };
    env.seed_calendar_object(work, "p1.ics", EVENT_ONE, "VEVENT", 0)
        .await;
    let body = sync_body("", "<d:getetag/>", None);

    // Shared calendar.
    let bob = env
        .seed_calendar(
            "principals/users/bob",
            "bobcal",
            Some("Bob Cal"),
            0,
            None,
            Some("VEVENT"),
            false,
            1,
        )
        .await;
    env.seed_calendar_share("principals/users/alice", 3, bob)
        .await;
    let resp = report(
        &app,
        "/remote.php/dav/calendars/alice/bobcal_shared_by_bob/",
        ALICE,
        PASSWORD,
        &body,
    )
    .await;
    assert_eq!(resp.status, 501, "shared REPORT: {}", resp.text());
    assert!(!attributed(&resp));

    // Trashed calendar.
    let env2 = env_or_skip!();
    env2.seed_user(ALICE, Some("Alice A")).await;
    let trashed = env2
        .seed_calendar(
            "principals/users/alice",
            "personal",
            Some("Personal"),
            0,
            None,
            Some("VEVENT"),
            false,
            1,
        )
        .await;
    env2.trash_calendar(trashed, 1_700_000_000).await;
    env2.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    let app2 = env2.app_shared();
    let resp = report(
        &app2,
        "/remote.php/dav/calendars/alice/personal/",
        ALICE,
        PASSWORD,
        &body,
    )
    .await;
    assert_eq!(resp.status, 501, "trashed REPORT: {}", resp.text());

    // Another principal.
    let resp = report(
        &app,
        "/remote.php/dav/calendars/bob/",
        ALICE,
        PASSWORD,
        &body,
    )
    .await;
    assert_eq!(resp.status, 501, "other principal REPORT: {}", resp.text());

    // calendar-query is not implemented.
    let query = r#"<cal:calendar-query xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/></d:prop><cal:filter><cal:comp-filter name="VCALENDAR"/></cal:filter></cal:calendar-query>"#;
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, query).await;
    assert_eq!(resp.status, 501, "calendar-query: {}", resp.text());
}

#[tokio::test]
async fn multiget_empty_body_and_empty_prop_delegate() {
    let Some((_env, app, _work)) = fixture().await else {
        return;
    };
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, "").await;
    assert_eq!(resp.status, 501);
    // No prop element: PHP would run allprop.
    let body = multiget_body("", &[&format!("{CAL_PATH}p1.ics")]);
    let resp = report(&app, CAL_PATH, ALICE, PASSWORD, &body).await;
    assert_eq!(resp.status, 501);
}
