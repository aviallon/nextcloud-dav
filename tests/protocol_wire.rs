// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pure (no-database) parity tests for the layers the sidecar reimplements:
//! vCard `readBlob`, RFC 6352 filters, the sync-token state machine, XML wire
//! format, path parsing and small HTTP helpers.
//!
//! These encode the behaviour of Nextcloud `CardDavBackend::readBlob()`,
//! `Sabre\CardDAV\Plugin::validateFilters()` and
//! `Sabre\DAV\Sync\Plugin`, which are the source of truth.

use nextcloud_dav::routes::{parse_path, DavTarget};
use nextcloud_dav::sync::{
    self, parse_sync_token, SyncToken, SYNCTOKEN_PREFIX,
};
use nextcloud_dav::util::{encode_path_segment, http_date, parse_basic_auth, percent_decode};
use nextcloud_dav::vcard::{self, filter_read_blob};
use nextcloud_dav::xml::filter::{self, Collation, MatchType, Test};
use nextcloud_dav::xml::parse::{parse_document, parse_propfind, parse_query, PropList};
use nextcloud_dav::xml::write::{
    DavResponse, MultiStatus, PropQName, PropStat, PropValue, XmlElement,
};

// ---------------------------------------------------------------------------
// vCard / readBlob
// ---------------------------------------------------------------------------

#[test]
fn read_blob_strips_non_image_photo_with_folded_lines() {
    // Exactly the PHP loop: drop `PHOTO:data:<non-image>` and any following
    // folded (leading space) lines.
    let data = b"BEGIN:VCARD\r\nPHOTO:data:text/plain;base64,AAAA\r\n AAAA\r\nFN:X\r\nEND:VCARD\r\n";
    let (out, modified) = filter_read_blob(data);
    assert!(modified, "a non-image PHOTO must set modified=true");
    assert_eq!(out, b"BEGIN:VCARD\r\nFN:X\r\nEND:VCARD\r\n".to_vec());
}

#[test]
fn read_blob_keeps_image_data_verbatim() {
    let data = b"BEGIN:VCARD\r\nPHOTO:data:image/jpeg;base64,AAAA\r\nEND:VCARD\r\n";
    let (out, modified) = filter_read_blob(data);
    assert!(!modified);
    assert_eq!(out, data.to_vec());
}

#[test]
fn read_blob_photo_at_start_is_returned_verbatim_even_if_non_image() {
    // PHP's micro-optimisation: `str_starts_with($cardData, 'PHOTO:data:')`.
    let data = b"PHOTO:data:text/plain;base64,AAAA\r\nFN:X\r\n";
    let (out, modified) = filter_read_blob(data);
    assert!(!modified);
    assert_eq!(out, data.to_vec());
}

#[test]
fn read_blob_is_idempotent_when_nothing_to_strip() {
    // PHP splits and rejoins on \r\n unconditionally; for a well-formed card
    // the result must be byte-identical.
    let data = b"BEGIN:VCARD\r\nUID:1\r\nFN:A\r\nEND:VCARD\r\n";
    let (out, modified) = filter_read_blob(data);
    assert!(!modified);
    assert_eq!(out, data.to_vec());
}

#[test]
fn has_photo_is_false_for_non_image_data_uri() {
    // HasPhotoPlugin::propFind(): a PHOTO whose value starts `data:` but not
    // `data:image/` is *not* a photo.
    assert!(!vcard::has_photo(b"BEGIN:VCARD\r\nPHOTO:data:text/plain;base64,AA\r\nEND:VCARD\r\n"));
    assert!(vcard::has_photo(b"BEGIN:VCARD\r\nPHOTO:data:image/png;base64,AA\r\nEND:VCARD\r\n"));
    assert!(vcard::has_photo(b"BEGIN:VCARD\r\nPHOTO;VALUE=uri:https://x/a.jpg\r\nEND:VCARD\r\n"));
    assert!(!vcard::has_photo(b"BEGIN:VCARD\r\nFN:X\r\nEND:VCARD\r\n"));
}

#[test]
fn vcard_unfolds_and_unescapes_like_sabre() {
    let card = vcard::parse(
        b"BEGIN:VCARD\r\nUID:1\r\nNOTE:one\\ntwo,three\\;four\\,five\r\n item1.X-ABLabel:x\r\nEND:VCARD\r\n",
    );
    let note = card.select("NOTE").next().unwrap();
    // The leading-space line is an RFC 2426 continuation, so it is appended.
    assert_eq!(note.value, "one\ntwo,three;four,fiveitem1.X-ABLabel:x");
}

#[test]
fn vcard_strips_group_prefix() {
    let card = vcard::parse(b"BEGIN:VCARD\r\nitem1.EMAIL:jane@example.com\r\nEND:VCARD\r\n");
    assert_eq!(card.select("EMAIL").next().unwrap().value, "jane@example.com");
}

#[test]
fn vcard_uid_is_extractable() {
    // The read path uses the stored `oc_cards.uid`, but the vCard parser (used
    // for addressbook-query and as the basis for a future write path) must
    // extract the same UID PHP's `getUID()` would.
    let card = vcard::parse(b"BEGIN:VCARD\r\nUID:jane-1\r\nFN:Jane\r\nEND:VCARD\r\n");
    assert_eq!(card.select("UID").next().unwrap().value, "jane-1");
    let card = vcard::parse(b"BEGIN:VCARD\r\nFN:No Uid\r\nEND:VCARD\r\n");
    assert!(!card.is_defined("UID"));
}

// ---------------------------------------------------------------------------
// RFC 6352 filters
// ---------------------------------------------------------------------------

const FILTER_CARD: &[u8] = b"BEGIN:VCARD\r\n\
VERSION:3.0\r\n\
UID:1234\r\n\
FN:Jane Doe\r\n\
N:Doe;Jane;;;\r\n\
EMAIL;TYPE=WORK,INTERNET:jane@example.com\r\n\
EMAIL;TYPE=HOME:jane@home.example\r\n\
TEL:+1 555 0100\r\n\
CATEGORIES:Friends,Work\r\n\
END:VCARD\r\n";

fn parse_filter_xml(xml: &str) -> filter::AddressBookFilter {
    let doc = parse_document(xml.as_bytes()).unwrap();
    filter::parse_filter(&doc).unwrap()
}

fn matches(xml: &str) -> bool {
    filter::evaluate(&vcard::parse(FILTER_CARD), &parse_filter_xml(xml))
}

#[test]
fn filter_existence_and_is_not_defined() {
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="EMAIL"/></card:filter>"#
    ));
    assert!(!matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="BDAY"/></card:filter>"#
    ));
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="BDAY"><card:is-not-defined/></card:prop-filter></card:filter>"#
    ));
}

#[test]
fn filter_text_match_semantics() {
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match>doe</card:text-match></card:prop-filter></card:filter>"#
    ));
    // default collation is i;unicode-casemap, default match-type is contains
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match match-type="starts-with">JANE</card:text-match></card:prop-filter></card:filter>"#
    ));
    assert!(!matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match match-type="ends-with">Jane</card:text-match></card:prop-filter></card:filter>"#
    ));
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match match-type="equals">jane doe</card:text-match></card:prop-filter></card:filter>"#
    ));
}

#[test]
fn filter_collations_differ() {
    // i;octet is case-sensitive, i;ascii-casemap folds ASCII only.
    assert!(!matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match collation="i;octet">jane</card:text-match></card:prop-filter></card:filter>"#
    ));
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match collation="i;ascii-casemap">JANE</card:text-match></card:prop-filter></card:filter>"#
    ));
}

#[test]
fn filter_param_filter_on_type() {
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="EMAIL"><card:param-filter name="TYPE"><card:text-match match-type="equals">WORK</card:text-match></card:param-filter></card:prop-filter></card:filter>"#
    ));
    assert!(!matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="EMAIL"><card:param-filter name="TYPE"><card:text-match match-type="equals">FAX</card:text-match></card:param-filter></card:prop-filter></card:filter>"#
    ));
}

#[test]
fn filter_anyof_allof() {
    let anyof = r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav" test="anyof"><card:prop-filter name="FN"><card:text-match>nope</card:text-match></card:prop-filter><card:prop-filter name="EMAIL"><card:text-match>jane@example.com</card:text-match></card:prop-filter></card:filter>"#;
    let allof = r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav" test="allof"><card:prop-filter name="FN"><card:text-match>nope</card:text-match></card:prop-filter><card:prop-filter name="EMAIL"><card:text-match>jane@example.com</card:text-match></card:prop-filter></card:filter>"#;
    assert!(matches(anyof));
    assert!(!matches(allof));
}

#[test]
fn filter_negate_condition() {
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match negate-condition="yes">smith</card:text-match></card:prop-filter></card:filter>"#
    ));
    assert!(!matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match negate-condition="yes">doe</card:text-match></card:prop-filter></card:filter>"#
    ));
}

#[test]
fn filter_empty_filter_matches_every_card() {
    assert!(matches(
        r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"/>"#
    ));
}

#[test]
fn filter_rejects_unknown_collation_and_match_type() {
    let bad_collation = r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match collation="i;bogus">x</card:text-match></card:prop-filter></card:filter>"#;
    let doc = parse_document(bad_collation.as_bytes()).unwrap();
    assert!(filter::parse_filter(&doc).is_err());

    let bad_match = r#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match match-type="bogus">x</card:text-match></card:prop-filter></card:filter>"#;
    let doc = parse_document(bad_match.as_bytes()).unwrap();
    assert!(filter::parse_filter(&doc).is_err());
}

#[test]
fn text_match_unicode_casemap_is_not_ascii_only() {
    assert!(filter::text_match(
        "Strässer",
        "STRÄSSER",
        Collation::UnicodeCasemap,
        MatchType::Equals
    ));
    assert!(!filter::text_match(
        "Strässer",
        "STRÄSSER",
        Collation::AsciiCasemap,
        MatchType::Equals
    ));
}

#[test]
fn test_attr_defaults_to_anyof() {
    assert_eq!(Test::from_attr(None).unwrap(), Test::AnyOf);
    assert_eq!(Test::from_attr(Some("allof")).unwrap(), Test::AllOf);
    assert!(Test::from_attr(Some("nope")).is_err());
}

// ---------------------------------------------------------------------------
// Sync tokens
// ---------------------------------------------------------------------------

#[test]
fn sync_token_parsing() {
    assert_eq!(parse_sync_token(None).unwrap(), SyncToken::Initial);
    // An *empty* <sync-token> element is normalised to `None` by
    // `parse_sync_collection`, so the initial-sync path is the one that runs.
    let req = nextcloud_dav::xml::parse::parse_sync_collection(
        br#"<d:sync-collection xmlns:d="DAV:"><d:sync-token/><d:prop><d:getetag/></d:prop></d:sync-collection>"#,
    )
    .unwrap();
    assert_eq!(req.sync_token, None);
    assert_eq!(
        parse_sync_token(Some(&format!("{SYNCTOKEN_PREFIX}"))).unwrap(),
        SyncToken::Initial
    );
    assert_eq!(
        parse_sync_token(Some(&format!("{SYNCTOKEN_PREFIX}42"))).unwrap(),
        SyncToken::Changes(42)
    );
    assert_eq!(
        parse_sync_token(Some(&format!("{SYNCTOKEN_PREFIX}init_7_42"))).unwrap(),
        SyncToken::InitialPaging {
            last_id: 7,
            token: 42
        }
    );
    // A token without the Sabre prefix is InvalidSyncToken -> 400.
    assert!(parse_sync_token(Some("42")).is_err());
    assert!(parse_sync_token(Some("http://other/ns/sync/42")).is_err());
    assert!(parse_sync_token(Some(&format!("{SYNCTOKEN_PREFIX}init_x_y"))).is_err());
}

fn card(id: i64, uri: &str) -> nextcloud_dav::model::CardIdUri {
    nextcloud_dav::model::CardIdUri {
        id,
        uri: uri.to_string(),
    }
}

fn change(uri: &str, operation: i64, token: i64) -> nextcloud_dav::model::ChangeRow {
    nextcloud_dav::model::ChangeRow {
        uri: uri.to_string(),
        operation,
        synctoken: token,
    }
}

#[test]
fn sync_initial_below_limit_returns_current_token() {
    let page = sync::initial_sync(&[card(1, "a.vcf"), card(2, "b.vcf")], 3, 2500);
    assert_eq!(page.added, vec!["a.vcf", "b.vcf"]);
    assert_eq!(page.sync_token, "3");
    assert!(!page.truncated);
}

#[test]
fn sync_initial_at_limit_pages_with_init_prefix() {
    let page = sync::initial_sync(&[card(10, "a.vcf"), card(11, "b.vcf")], 4, 2);
    assert_eq!(page.sync_token, "init_11_4");
    assert!(page.truncated);
}

#[test]
fn sync_initial_empty_returns_current_token() {
    let page = sync::initial_sync(&[], 7, 2500);
    assert_eq!(page.sync_token, "7");
    assert!(!page.truncated);
}

#[test]
fn sync_initial_continue_exhausted_drops_prefix() {
    let page = sync::initial_sync_continue(&[], 4, 2);
    assert_eq!(page.sync_token, "4");
    assert!(!page.truncated);
}

#[test]
fn sync_incremental_dedups_last_operation_wins() {
    let rows = [
        change("a.vcf", 1, 5),
        change("b.vcf", 1, 6),
        change("a.vcf", 2, 7),
    ];
    let page = sync::changes_sync(&rows, 9, 2500);
    assert_eq!(page.added, vec!["b.vcf"]);
    assert_eq!(page.modified, vec!["a.vcf"]);
    assert_eq!(page.sync_token, "9");
    assert!(!page.truncated);
}

#[test]
fn sync_incremental_deletes_are_reported() {
    let rows = [change("a.vcf", 3, 5), change("b.vcf", 1, 6)];
    let page = sync::changes_sync(&rows, 9, 2500);
    assert_eq!(page.deleted, vec!["a.vcf"]);
    assert_eq!(page.added, vec!["b.vcf"]);
}

#[test]
fn sync_incremental_truncation_returns_highest_token() {
    let rows = [change("a.vcf", 1, 5), change("b.vcf", 1, 6)];
    let page = sync::changes_sync(&rows, 9, 2);
    assert_eq!(page.sync_token, "6");
    assert!(page.truncated);
}

#[test]
fn sync_incremental_at_limit_highest_is_current_is_not_truncated() {
    // PHP: `$highestSyncToken < $currentToken` must hold to truncate.
    let rows = [change("a.vcf", 1, 9), change("b.vcf", 1, 9)];
    let page = sync::changes_sync(&rows, 9, 2);
    assert_eq!(page.sync_token, "9");
    assert!(!page.truncated);
}

// ---------------------------------------------------------------------------
// XML wire format
// ---------------------------------------------------------------------------

#[test]
fn multistatus_has_expected_namespaces_and_shape() {
    let ms = MultiStatus {
        responses: vec![DavResponse::props(
            "/remote.php/dav/addressbooks/users/alice/contacts/",
            vec![PropStat::ok(vec![(
                PropQName::dav("displayname"),
                PropValue::Text("Contacts".to_string()),
            )])],
        )],
        sync_token: Some(format!("{SYNCTOKEN_PREFIX}5")),
    };
    let xml = ms.to_xml();
    assert!(xml.contains("xmlns:d=\"DAV:\""));
    assert!(xml.contains("xmlns:card=\"urn:ietf:params:xml:ns:carddav\""));
    assert!(xml.contains("xmlns:cs=\"http://calendarserver.org/ns/\""));
    assert!(xml.contains("xmlns:s=\"http://sabredav.org/ns\""));
    assert!(xml.contains("xmlns:oc=\"http://owncloud.org/ns\""));
    assert!(xml.contains("xmlns:nc=\"http://nextcloud.com/ns\""));
    assert!(xml.contains("HTTP/1.1 200 OK"));
    let doc = parse_document(xml.as_bytes()).unwrap();
    assert_eq!(doc.local, "multistatus");
    assert_eq!(doc.children.len(), 2); // response + sync-token
}

#[test]
fn multistatus_escapes_property_text() {
    let ms = MultiStatus {
        responses: vec![DavResponse::props(
            "/x",
            vec![PropStat::ok(vec![(
                PropQName::dav("displayname"),
                PropValue::Text("A & B <c>".to_string()),
            )])],
        )],
        sync_token: None,
    };
    let xml = ms.to_xml();
    assert!(xml.contains("A &amp; B &lt;c&gt;"));
    // Round-trips through quick-xml.
    let doc = parse_document(xml.as_bytes()).unwrap();
    let prop = doc.children[0]
        .child("DAV:", "propstat")
        .unwrap()
        .child("DAV:", "prop")
        .unwrap();
    assert_eq!(prop.children[0].text, "A & B <c>");
}

#[test]
fn empty_response_without_status_gets_teapot_propstat() {
    // Sabre emits an empty 418 propstat when a response has neither props nor
    // a response-level status; WebDAV requires at least one of the two.
    let ms = MultiStatus {
        responses: vec![DavResponse::props("/x", Vec::new())],
        sync_token: None,
    };
    let xml = ms.to_xml();
    assert!(
        xml.contains("418") && xml.contains("teapot"),
        "expected a teapot propstat in {xml}"
    );
}

#[test]
fn status_response_renders_response_level_status() {
    let ms = MultiStatus {
        responses: vec![DavResponse::status("/x/gone.vcf", 404)],
        sync_token: None,
    };
    let xml = ms.to_xml();
    assert!(xml.contains("HTTP/1.1 404 Not Found"));
}

#[test]
fn structured_property_renders_nested_elements() {
    let ms = MultiStatus {
        responses: vec![DavResponse::props(
            "/x",
            vec![PropStat::ok(vec![(
                PropQName::carddav("supported-address-data"),
                PropValue::Elements(vec![
                    XmlElement::new("card:address-data-type")
                        .attr("content-type", "text/vcard")
                        .attr("version", "3.0"),
                    XmlElement::new("card:address-data-type")
                        .attr("content-type", "text/vcard")
                        .attr("version", "4.0"),
                ]),
            )])],
        )],
        sync_token: None,
    };
    let xml = ms.to_xml();
    assert!(xml.contains("version=\"3.0\""));
    assert!(xml.contains("version=\"4.0\""));
}

#[test]
fn propfind_allprop_defaults_and_empty_body() {
    let req = parse_propfind(b"").unwrap();
    assert_eq!(req.props, PropList::AllProp);
    let req = parse_propfind(
        br#"<d:propfind xmlns:d="DAV:"><d:allprop/></d:propfind>"#,
    )
    .unwrap();
    assert_eq!(req.props, PropList::AllProp);
    let req = parse_propfind(
        br#"<d:propfind xmlns:d="DAV:"><d:propname/></d:propfind>"#,
    )
    .unwrap();
    assert_eq!(req.props, PropList::PropName);
}

#[test]
fn propfind_namespaces_are_resolved_not_prefix_matched() {
    let req = parse_propfind(
        br#"<x:propfind xmlns:x="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><x:prop><card:address-data/><x:getetag/></x:prop></x:propfind>"#,
    )
    .unwrap();
    match req.props {
        PropList::Props(props) => assert_eq!(
            props,
            vec![
                PropQName::carddav("address-data"),
                PropQName::dav("getetag"),
            ]
        ),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn query_parses_limit_and_filter_namespace() {
    let req = parse_query(
        br#"<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/></d:prop><card:filter><card:prop-filter name="FN"><card:text-match>Jane</card:text-match></card:prop-filter></card:filter><d:limit><d:nresults>5</d:nresults></d:limit></card:addressbook-query>"#,
    )
    .unwrap();
    assert_eq!(req.limit, Some(5));
    assert!(req.filter.is_some());
}

// ---------------------------------------------------------------------------
// Path parsing and helpers
// ---------------------------------------------------------------------------

#[test]
fn parse_path_home_book_card() {
    assert_eq!(
        parse_path("/remote.php/dav/addressbooks/users/alice").target,
        DavTarget::Home {
            user: "alice".into(),
            href: "/remote.php/dav/addressbooks/users/alice".into()
        }
    );
    assert_eq!(
        parse_path("/remote.php/dav/addressbooks/users/alice/contacts/").target,
        DavTarget::Book {
            user: "alice".into(),
            book_uri: "contacts".into(),
            href: "/remote.php/dav/addressbooks/users/alice/contacts".into()
        }
    );
    assert_eq!(
        parse_path("/remote.php/dav/addressbooks/users/alice/contacts/1.vcf").target,
        DavTarget::Card {
            user: "alice".into(),
            book_uri: "contacts".into(),
            card_uri: "1.vcf".into(),
            href: "/remote.php/dav/addressbooks/users/alice/contacts/1.vcf".into()
        }
    );
}

#[test]
fn parse_path_rejects_system_and_unknown_shapes() {
    // The system address book is explicitly not modelled.
    assert_eq!(
        parse_path("/remote.php/dav/addressbooks/system/system/system").target,
        DavTarget::NotFound
    );
    assert_eq!(
        parse_path("/remote.php/dav/calendars/users/a").target,
        DavTarget::NotFound
    );
    assert_eq!(
        parse_path("/remote.php/dav/addressbooks/users/alice/c/x/y").target,
        DavTarget::NotFound
    );
}

#[test]
fn parse_path_decodes_percent_encoding_and_webroot() {
    let parsed = parse_path("/nextcloud/remote.php/dav/addressbooks/users/al%20ice/contacts");
    assert_eq!(parsed.context, "/nextcloud/remote.php/dav/addressbooks");
    match parsed.target {
        DavTarget::Book { user, .. } => assert_eq!(user, "al ice"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn path_segment_encoding_matches_sabre_allowlist() {
    assert_eq!(encode_path_segment("contacts"), "contacts");
    assert_eq!(encode_path_segment("a b.vcf"), "a%20b.vcf");
    assert_eq!(encode_path_segment("a/b"), "a%2fb");
    assert_eq!(encode_path_segment("user@example.com"), "user@example.com");
    assert_eq!(encode_path_segment("x(y)"), "x(y)");
    assert_eq!(percent_decode("al%20ice"), "al ice");
    assert_eq!(percent_decode("%zz"), "%zz");
}

#[test]
fn http_date_matches_rfc1123() {
    assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    assert_eq!(http_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
}

#[test]
fn basic_auth_parsing_splits_on_first_colon_only() {
    use base64::Engine;
    let value = axum::http::HeaderValue::from_str(&format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("alice:s3cret:with:colons")
    ))
    .unwrap();
    assert_eq!(
        parse_basic_auth(Some(&value)),
        Some(("alice".to_string(), "s3cret:with:colons".to_string()))
    );
    assert_eq!(
        parse_basic_auth(Some(&axum::http::HeaderValue::from_static("Bearer x"))),
        None
    );
}
