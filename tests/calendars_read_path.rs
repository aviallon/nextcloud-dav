// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CalDAV read path, part 1: the calendar home (`Depth: 0`/`1`) and the
//! per-calendar `PROPFIND` (`Depth: 0`), through the real axum router.
//!
//! Tests **skip** when PostgreSQL is unavailable.

mod common;

use common::{call, propfind, request, TestEnv};
use nextcloud_dav::xml::parse::{parse_document, XNode};
use nextcloud_dav::xml::write::{
    NS_CALDAV, NS_CALENDARSERVER, NS_DAV, NS_NEXTCLOUD, NS_OWNCLOUD, NS_SABREDAV,
};

const ALICE: &str = "alice";
const PASSWORD: &str = "app-password";
const APPLE: &str = "http://apple.com/ns/ical/";

const CAL_PROPS: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns" xmlns:apple="http://apple.com/ns/ical/" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:resourcetype/><d:displayname/><d:owner/><d:current-user-principal/>
    <d:current-user-privilege-set/><d:acl/><d:supported-report-set/><d:supported-method-set/>
    <d:sync-token/><cs:getctag/><cs:allowed-sharing-modes/>
    <cal:supported-calendar-component-set/><cal:schedule-calendar-transp/>
    <cal:calendar-description/><cal:calendar-timezone/><cal:max-resource-size/>
    <cal:supported-calendar-data/><cal:supported-collation-set/>
    <oc:owner-principal/><oc:read-only/><oc:invite/><oc:calendar-enabled/>
    <nc:owner-displayname/><nc:disable-alarm-notifications/>
    <apple:calendar-color/><apple:calendar-order/><s:sync-token/>
  </d:prop>
</d:propfind>"#;

/// The home Depth 1 child set, without `acl`/`current-user-privilege-set`
/// (those are delegated when the special children are present).
const HOME_LIST_PROPS: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.com/ns" xmlns:apple="http://apple.com/ns/ical/" xmlns:s="http://sabredav.org/ns">
  <d:prop>
    <d:resourcetype/><d:displayname/><d:owner/><d:supported-report-set/><d:supported-method-set/>
    <cs:getctag/><cal:supported-calendar-component-set/><cal:schedule-calendar-transp/>
    <oc:owner-principal/><oc:read-only/><nc:owner-displayname/><nc:trash-bin-retention-duration/>
    <apple:calendar-color/><apple:calendar-order/><s:sync-token/>
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

fn parse_xml(body: &[u8]) -> XNode {
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

fn is_404(resp: &XNode, ns: &str, local: &str) -> bool {
    propstat(resp, "404")
        .and_then(|ps| ps.child(NS_DAV, "prop"))
        .map(|prop| prop.children.iter().any(|c| c.ns == ns && c.local == local))
        .unwrap_or(false)
}

fn prop_text(resp: &XNode, ns: &str, local: &str) -> String {
    prop_of(resp, ns, local)
        .map(|n| n.text.clone())
        .unwrap_or_else(|| panic!("missing prop {{{ns}}}{local}"))
}

fn hrefs(doc: &XNode) -> Vec<String> {
    doc.children
        .iter()
        .filter(|c| c.ns == NS_DAV && c.local == "response")
        .filter_map(|r| r.child(NS_DAV, "href").map(|h| h.text.clone()))
        .collect()
}

async fn fixture() -> Option<(TestEnv, axum::Router, i64, i64)> {
    let env = TestEnv::new().await?;
    env.seed_user(ALICE, Some("Alice A")).await;
    env.seed_user("bob", Some("Bob Builder")).await;
    // Owned calendars: two, ordered by `calendarorder`.
    let work = env
        .seed_calendar(
            "principals/users/alice",
            "work",
            Some("Work"),
            2,
            Some("#111111"),
            Some("VEVENT"),
            false,
            7,
        )
        .await;
    let _home = env
        .seed_calendar(
            "principals/users/alice",
            "home",
            Some("Home"),
            1,
            None,
            Some("VEVENT,VTODO"),
            true,
            1,
        )
        .await;
    // bob's calendar shared read-only with alice.
    let bob_cal = env
        .seed_calendar(
            "principals/users/bob",
            "bobcal",
            Some("Bob Cal"),
            5,
            Some("#ff0000"),
            Some("VEVENT"),
            false,
            1,
        )
        .await;
    env.seed_calendar_share("principals/users/alice", 3, bob_cal)
        .await;
    // A sharee override on the shared calendar.
    env.seed_calendar_property(
        ALICE,
        "calendars/alice/bobcal_shared_by_bob",
        "{DAV:}displayname",
        "My Bob Cal",
    )
    .await;
    env.seed_calendar_property(
        ALICE,
        "calendars/alice/bobcal_shared_by_bob",
        "{http://apple.com/ns/ical/}calendar-color",
        "#123456",
    )
    .await;
    env.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    Some((env, app, work, bob_cal))
}

fn attributed(resp: &common::Resp) -> bool {
    resp.header("x-nextcloud-dav").as_deref() == Some("sidecar")
}

// ---------------------------------------------------------------------------
// Home
// ---------------------------------------------------------------------------

#[tokio::test]
async fn home_depth0_property_set() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/",
        ALICE,
        PASSWORD,
        "0",
        CAL_PROPS,
    )
    .await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    assert!(attributed(&resp), "the home must be served by the sidecar");
    let doc = parse_xml(&resp.body);
    let home = response(&doc, "/remote.php/dav/calendars/alice/").expect("home response");
    for (ns, local) in [
        (NS_DAV, "resourcetype"),
        (NS_DAV, "owner"),
        (NS_DAV, "current-user-principal"),
        (NS_DAV, "current-user-privilege-set"),
        (NS_DAV, "acl"),
        (NS_DAV, "supported-report-set"),
        (NS_DAV, "supported-method-set"),
    ] {
        assert!(prop_of(home, ns, local).is_some(), "{{{ns}}}{local} must be 200");
    }
    assert!(is_404(home, NS_DAV, "displayname"));
    let owner_href = prop_of(home, NS_DAV, "owner")
        .and_then(|n| n.child(NS_DAV, "href"))
        .map(|h| h.text.clone())
        .unwrap_or_default();
    assert_eq!(owner_href, "/remote.php/dav/principals/users/alice/");
}

#[tokio::test]
async fn home_depth1_child_set_and_order() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/",
        ALICE,
        PASSWORD,
        "1",
        HOME_LIST_PROPS,
    )
    .await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    assert!(attributed(&resp));
    let doc = parse_xml(&resp.body);
    let got = hrefs(&doc);
    // home, owned calendars in calendarorder (home=1 before work=2), then the
    // shared calendar, then the special children.
    assert_eq!(
        got,
        vec![
            "/remote.php/dav/calendars/alice/",
            "/remote.php/dav/calendars/alice/home/",
            "/remote.php/dav/calendars/alice/work/",
            "/remote.php/dav/calendars/alice/bobcal_shared_by_bob/",
            "/remote.php/dav/calendars/alice/inbox/",
            "/remote.php/dav/calendars/alice/outbox/",
            "/remote.php/dav/calendars/alice/trashbin/",
        ]
    );
    let inbox = response(&doc, "/remote.php/dav/calendars/alice/inbox/").unwrap();
    assert_eq!(prop_text(inbox, NS_DAV, "resourcetype"), "");
    let rt = prop_of(inbox, NS_DAV, "resourcetype").unwrap();
    assert!(rt.children.iter().any(|c| c.ns == NS_CALDAV && c.local == "schedule-inbox"));
    let trash = response(&doc, "/remote.php/dav/calendars/alice/trashbin/").unwrap();
    assert_eq!(
        prop_text(trash, NS_NEXTCLOUD, "trash-bin-retention-duration"),
        "2592000"
    );
}

#[tokio::test]
async fn home_depth1_with_acl_delegates() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    // The special children carry acl/privilege values the sidecar does not
    // model, so the whole listing delegates.
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:acl/></d:prop></d:propfind>"#;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/",
        ALICE,
        PASSWORD,
        "1",
        body,
    )
    .await;
    assert_eq!(resp.status, 501);
    assert!(!attributed(&resp));
}

// ---------------------------------------------------------------------------
// Per-calendar
// ---------------------------------------------------------------------------

#[tokio::test]
async fn calendar_depth0_ctag_and_sync_token_quirks() {
    let Some((_env, app, work, _bob)) = fixture().await else {
        return;
    };
    let _ = work;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/work/",
        ALICE,
        PASSWORD,
        "0",
        CAL_PROPS,
    )
    .await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    assert!(attributed(&resp));
    let doc = parse_xml(&resp.body);
    let cal = response(&doc, "/remote.php/dav/calendars/alice/work/").unwrap();
    // The CalDAV quirk: getctag is the sabre sync URL, sabredav is the raw
    // token, DAV is the prefixed token.
    assert_eq!(
        prop_text(cal, NS_CALENDARSERVER, "getctag"),
        "http://sabre.io/ns/sync/7"
    );
    assert_eq!(prop_text(cal, NS_SABREDAV, "sync-token"), "7");
    assert_eq!(
        prop_text(cal, NS_DAV, "sync-token"),
        "http://sabre.io/ns/sync/7"
    );
    // `getCalendarByUri()` (owned Depth 0) does not set owner-principal.
    assert!(is_404(cal, NS_OWNCLOUD, "owner-principal"));
    assert!(is_404(cal, NS_OWNCLOUD, "read-only"));
    assert_eq!(prop_text(cal, NS_NEXTCLOUD, "owner-displayname"), "Alice A");
    assert_eq!(prop_text(cal, APPLE, "calendar-order"), "2");
    assert_eq!(prop_text(cal, APPLE, "calendar-color"), "#111111");
}

#[tokio::test]
async fn shared_calendar_properties_and_overrides() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/bobcal_shared_by_bob/",
        ALICE,
        PASSWORD,
        "0",
        CAL_PROPS,
    )
    .await;
    assert_eq!(resp.status, 207, "{}", resp.text());
    assert!(attributed(&resp));
    let doc = parse_xml(&resp.body);
    let cal = response(&doc, "/remote.php/dav/calendars/alice/bobcal_shared_by_bob/").unwrap();
    // The oc_properties override layer replaces the backend displayname/color.
    assert_eq!(prop_text(cal, NS_DAV, "displayname"), "My Bob Cal");
    assert_eq!(prop_text(cal, APPLE, "calendar-color"), "#123456");
    assert_eq!(
        prop_text(cal, NS_OWNCLOUD, "owner-principal"),
        "principals/users/bob"
    );
    assert_eq!(prop_text(cal, NS_OWNCLOUD, "read-only"), "1");
    assert_eq!(prop_text(cal, NS_NEXTCLOUD, "owner-displayname"), "Bob Builder");
    let transp = prop_of(cal, NS_CALDAV, "schedule-calendar-transp").unwrap();
    assert!(transp.children.iter().any(|c| c.local == "transparent"));
    let modes = prop_of(cal, NS_CALENDARSERVER, "allowed-sharing-modes").unwrap();
    assert!(modes.children.is_empty(), "read-only share: no sharing modes");
}

#[tokio::test]
async fn read_only_shared_privilege_set() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/bobcal_shared_by_bob/",
        ALICE,
        PASSWORD,
        "0",
        CAL_PROPS,
    )
    .await;
    let doc = parse_xml(&resp.body);
    let cal = response(&doc, "/remote.php/dav/calendars/alice/bobcal_shared_by_bob/").unwrap();
    let privileges: Vec<String> = prop_of(cal, NS_DAV, "current-user-privilege-set")
        .unwrap()
        .children
        .iter()
        .filter_map(|p| p.children.first().map(|c| format!("{{{}}}{}", c.ns, c.local)))
        .collect();
    assert_eq!(
        privileges,
        vec![
            "{DAV:}write-properties",
            "{DAV:}read",
            "{DAV:}read-acl",
            "{DAV:}read-current-user-privilege-set",
            "{urn:ietf:params:xml:ns:caldav}read-free-busy",
        ]
    );
}

#[tokio::test]
async fn calendar_enabled_override_and_default() {
    let Some((env, app, work, _bob)) = fixture().await else {
        return;
    };
    let _ = work;
    // No row: 404, like PHP.
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:calendar-enabled/></d:prop></d:propfind>"#;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/work/",
        ALICE,
        PASSWORD,
        "0",
        body,
    )
    .await;
    let doc = parse_xml(&resp.body);
    let cal = response(&doc, "/remote.php/dav/calendars/alice/work/").unwrap();
    assert!(is_404(cal, NS_OWNCLOUD, "calendar-enabled"));
    // With a row: the stored value wins.
    env.seed_calendar_property(
        ALICE,
        "calendars/alice/work",
        "{http://owncloud.org/ns}calendar-enabled",
        "0",
    )
    .await;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/work/",
        ALICE,
        PASSWORD,
        "0",
        body,
    )
    .await;
    let doc2 = parse_xml(&resp.body);
    let cal = response(&doc2, "/remote.php/dav/calendars/alice/work/").unwrap();
    assert_eq!(prop_text(cal, NS_OWNCLOUD, "calendar-enabled"), "0");
}

// ---------------------------------------------------------------------------
// Gate / delegation / guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn property_gate_501_and_404_split() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    // A property PHP serves but the sidecar does not model -> 501.
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:publish-url/></d:prop></d:propfind>"#;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/work/",
        ALICE,
        PASSWORD,
        "0",
        body,
    )
    .await;
    assert_eq!(resp.status, 501);
    assert!(!attributed(&resp));

    // A property PHP 404s -> a 404 propstat, not a 501.
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:quota-used-bytes/><d:getlastmodified/><d:share-access/></d:prop></d:propfind>"#;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/work/",
        ALICE,
        PASSWORD,
        "0",
        body,
    )
    .await;
    assert_eq!(resp.status, 207);
    assert!(attributed(&resp));
    let doc = parse_xml(&resp.body);
    let cal = response(&doc, "/remote.php/dav/calendars/alice/work/").unwrap();
    assert!(is_404(cal, NS_DAV, "quota-used-bytes"));
    assert!(is_404(cal, NS_DAV, "getlastmodified"));
    assert!(is_404(cal, NS_DAV, "share-access"));
}

#[tokio::test]
async fn objects_trashbin_and_empty_report_delegate() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
    for path in [
        "/remote.php/dav/calendars/alice/work/x.ics",
        "/remote.php/dav/calendars/alice/trashbin/",
        "/remote.php/dav/calendars/alice/inbox/",
        "/remote.php/dav/calendars/alice/outbox/",
    ] {
        let resp = propfind(&app, path, ALICE, PASSWORD, "0", body).await;
        assert_eq!(resp.status, 501, "PROPFIND {path}");
        assert!(!attributed(&resp), "{path} must be delegated");
    }
    // A REPORT with no body is neither of the two modelled REPORT shapes.
    let resp = call(
        &app,
        request(
            "REPORT",
            "/remote.php/dav/calendars/alice/work/",
            ALICE,
            PASSWORD,
        ),
    )
    .await;
    assert_eq!(resp.status, 501);
}

#[tokio::test]
async fn other_principal_delegates() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/bob/",
        ALICE,
        PASSWORD,
        "1",
        body,
    )
    .await;
    assert_eq!(resp.status, 501);
    assert!(!attributed(&resp));
}

#[tokio::test]
async fn trashed_calendar_subscription_and_federated_delegate() {
    // Trashed calendar.
    let env = env_or_skip!();
    env.seed_user(ALICE, Some("Alice A")).await;
    let cal = env
        .seed_calendar("principals/users/alice", "personal", Some("Personal"), 0, None, Some("VEVENT"), false, 1)
        .await;
    env.trash_calendar(cal, 1_700_000_000).await;
    env.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
    let resp = propfind(&app, "/remote.php/dav/calendars/alice/", ALICE, PASSWORD, "1", body).await;
    assert_eq!(resp.status, 501, "trashed calendar must delegate");
    assert!(!attributed(&resp));

    // Subscription.
    let env = env_or_skip!();
    env.seed_user(ALICE, Some("Alice A")).await;
    env.seed_calendar_subscription("principals/users/alice", "webcal").await;
    env.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    let resp = propfind(&app, "/remote.php/dav/calendars/alice/", ALICE, PASSWORD, "1", body).await;
    assert_eq!(resp.status, 501, "subscription must delegate");

    // Federated calendar.
    let env = env_or_skip!();
    env.seed_user(ALICE, Some("Alice A")).await;
    env.seed_federated_calendar("principals/users/alice", "fed").await;
    env.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    let app = env.app_shared();
    let resp = propfind(&app, "/remote.php/dav/calendars/alice/", ALICE, PASSWORD, "1", body).await;
    assert_eq!(resp.status, 501, "federated calendar must delegate");
}

#[tokio::test]
async fn missing_calendar_is_501_not_404() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/nope/",
        ALICE,
        PASSWORD,
        "0",
        body,
    )
    .await;
    assert_eq!(resp.status, 501);
}

#[tokio::test]
async fn unauthenticated_is_401() {
    let Some((_env, app, _work, _bob)) = fixture().await else {
        return;
    };
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
    let request = axum::http::Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/calendars/alice/")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let resp = call(&app, request).await;
    assert_eq!(resp.status, 401);
}

// ---------------------------------------------------------------------------
// Localized calendar displaynames: the l10n fail-safe
// ---------------------------------------------------------------------------

/// The display-only PROPFIND body; `<d:displayname/>` is inside the property
/// gate, so the fail-safe, not the gate, is what decides each case.
const DISPLAY_PROPS: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>"#;

/// A unique throwaway `dav` l10n directory holding one `fr.json`.
fn write_l10n_dir(tag: &str, fr_json: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ncdav-l10n-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fr.json"), fr_json).unwrap();
    dir
}

/// A home with `personal`, `contact_birthdays` and an ordinary `work` calendar,
/// plus `alice`'s language preference set to `fr`.
async fn l10n_env() -> Option<TestEnv> {
    let env = TestEnv::new().await?;
    env.seed_user(ALICE, Some("Alice A")).await;
    env.seed_calendar(
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
    env.seed_calendar(
        "principals/users/alice",
        "contact_birthdays",
        Some("Contact birthdays"),
        1,
        None,
        Some("VEVENT"),
        false,
        1,
    )
    .await;
    env.seed_calendar(
        "principals/users/alice",
        "work",
        Some("Work"),
        2,
        None,
        Some("VEVENT"),
        false,
        1,
    )
    .await;
    env.seed_preference(ALICE, "core", "lang", "fr").await;
    env.seed_token(ALICE, ALICE, PASSWORD, 1, 2).await;
    Some(env)
}

#[tokio::test]
async fn l10n_unavailable_special_displayname_delegates() {
    let Some(env) = l10n_env().await else {
        return;
    };
    // An l10n_dir that does not exist: the source is unavailable.
    let missing = std::env::temp_dir().join(format!(
        "ncdav-l10n-{}-missing-failsafe",
        std::process::id()
    ));
    let app = env.app_shared_l10n_dir(missing);

    // The home listing carries a special calendar, so the whole listing is
    // delegated rather than served with the untranslated English names.
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/",
        ALICE,
        PASSWORD,
        "1",
        DISPLAY_PROPS,
    )
    .await;
    assert_eq!(resp.status, 501, "home listing must delegate");
    assert!(!attributed(&resp), "a delegated 501 must carry no sidecar header");

    // Both special calendars delegate individually too.
    for path in [
        "/remote.php/dav/calendars/alice/personal/",
        "/remote.php/dav/calendars/alice/contact_birthdays/",
    ] {
        let resp = propfind(&app, path, ALICE, PASSWORD, "0", DISPLAY_PROPS).await;
        assert_eq!(resp.status, 501, "PROPFIND {path} must delegate");
        assert!(!attributed(&resp), "{path} must carry no sidecar header");
    }
}

#[tokio::test]
async fn l10n_unavailable_ordinary_displayname_is_served() {
    let Some(env) = l10n_env().await else {
        return;
    };
    let missing = std::env::temp_dir().join(format!(
        "ncdav-l10n-{}-missing-ordinary",
        std::process::id()
    ));
    let app = env.app_shared_l10n_dir(missing);

    // `work` is not a special case, so it is served even with no l10n tree.
    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/work/",
        ALICE,
        PASSWORD,
        "0",
        DISPLAY_PROPS,
    )
    .await;
    assert_eq!(resp.status, 207, "an ordinary displayname must still be served");
    assert!(attributed(&resp), "the sidecar must have served it");
    let doc = parse_xml(&resp.body);
    let cal = response(&doc, "/remote.php/dav/calendars/alice/work/").unwrap();
    assert_eq!(prop_text(cal, NS_DAV, "displayname"), "Work");
}

#[tokio::test]
async fn l10n_malformed_file_delegates_special_displayname() {
    let Some(env) = l10n_env().await else {
        return;
    };
    // `fr.json` exists (so `fr` is an available language) but cannot be
    // parsed: the source is unavailable for this request.
    let dir = write_l10n_dir("malformed-fr", "{not json");
    let app = env.app_shared_l10n_dir(dir.clone());

    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/personal/",
        ALICE,
        PASSWORD,
        "0",
        DISPLAY_PROPS,
    )
    .await;
    assert_eq!(resp.status, 501, "a malformed l10n file must delegate");
    assert!(!attributed(&resp));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn l10n_available_without_translation_serves_the_identity() {
    let Some(env) = l10n_env().await else {
        return;
    };
    // The file is read; it simply has no translation for the key. English is
    // then the correct answer, so it is served rather than delegated.
    let dir = write_l10n_dir("empty-fr", r#"{"translations":{}}"#);
    let app = env.app_shared_l10n_dir(dir.clone());

    let resp = propfind(
        &app,
        "/remote.php/dav/calendars/alice/personal/",
        ALICE,
        PASSWORD,
        "0",
        DISPLAY_PROPS,
    )
    .await;
    assert_eq!(resp.status, 207, "an available-but-empty translation is served");
    assert!(attributed(&resp));
    let doc = parse_xml(&resp.body);
    let cal = response(&doc, "/remote.php/dav/calendars/alice/personal/").unwrap();
    assert_eq!(prop_text(cal, NS_DAV, "displayname"), "Personal");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn l10n_available_french_translates_the_displayname() {
    let Some(env) = l10n_env().await else {
        return;
    };
    let dir = write_l10n_dir(
        "full-fr",
        r#"{"translations":{"Personal":"Personnel","Contact birthdays":"Anniversaires des contacts"}}"#,
    );
    let app = env.app_shared_l10n_dir(dir.clone());

    for (path, expected) in [
        ("/remote.php/dav/calendars/alice/personal/", "Personnel"),
        (
            "/remote.php/dav/calendars/alice/contact_birthdays/",
            "Anniversaires des contacts",
        ),
    ] {
        let resp = propfind(&app, path, ALICE, PASSWORD, "0", DISPLAY_PROPS).await;
        assert_eq!(resp.status, 207, "PROPFIND {path}");
        assert!(attributed(&resp), "{path} must be served by the sidecar");
        let doc = parse_xml(&resp.body);
        let cal = response(&doc, path).unwrap();
        assert_eq!(prop_text(cal, NS_DAV, "displayname"), expected, "{path}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
