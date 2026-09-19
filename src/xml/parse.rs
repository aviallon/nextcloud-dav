// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Request body parsing: PROPFIND, `addressbook-multiget`,
//! `addressbook-query` and `sync-collection`.
//!
//! Bodies are parsed into a generic namespace-resolved tree first, then the
//! individual requests are read off it. That keeps the namespace handling in one
//! place and makes the request-specific code easy to test.

use crate::error::{Error, Result};
use crate::xml::filter::{AddressBookFilter, Test};
use crate::xml::write::{PropQName, NS_CALDAV, NS_CARDDAV, NS_DAV};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;

/// A namespace-resolved XML node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XNode {
    pub ns: String,
    pub local: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<XNode>,
}

impl XNode {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(&self, ns: &str, local: &str) -> Option<&XNode> {
        self.children
            .iter()
            .find(|c| c.ns == ns && c.local == local)
    }

    pub fn children_of<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a XNode> {
        self.children
            .iter()
            .filter(move |c| c.ns == ns && c.local == local)
    }

    pub fn qname(&self) -> PropQName {
        PropQName::new(self.ns.clone(), self.local.clone())
    }
}

/// Parses an XML document and returns its root element.
pub fn parse_document(xml: &[u8]) -> Result<XNode> {
    let mut reader = NsReader::from_reader(xml);
    // Whitespace is trimmed by the consumers of text values (`text-match`,
    // `sync-token`, `href`, `nresults`); keeping it here preserves spaces that
    // surround entity references such as `A &amp; B`.
    loop {
        match reader.read_resolved_event() {
            Ok((resolved, event)) => {
                let ns = ns_string(&resolved);
                // The resolver borrows `reader`; release it before recursing.
                drop(resolved);
                match event {
                    Event::Start(start) => return parse_element(&mut reader, ns, &start),
                    Event::Empty(start) => {
                        // A self-closing root element, e.g. `<card:filter/>`.
                        return Ok(XNode {
                            ns,
                            local: start.local_name().into_inner().to_string(),
                            attrs: read_attrs(&start)?,
                            text: String::new(),
                            children: Vec::new(),
                        });
                    }
                    Event::Eof => return Err(Error::Xml("empty XML document".into())),
                    _ => {}
                }
            }
            Err(e) => return Err(Error::Xml(e.to_string())),
        }
    }
}

fn parse_element(reader: &mut NsReader<&[u8]>, ns: String, start: &BytesStart) -> Result<XNode> {
    let mut node = XNode {
        ns,
        local: start.local_name().into_inner().to_string(),
        attrs: read_attrs(start)?,
        text: String::new(),
        children: Vec::new(),
    };
    loop {
        match reader.read_resolved_event() {
            Ok((resolved, Event::Start(child_start))) => {
                let child_ns = ns_string(&resolved);
                drop(resolved);
                let child = parse_element(reader, child_ns, &child_start)?;
                node.children.push(child);
            }
            Ok((resolved, Event::Empty(child_start))) => {
                node.children.push(XNode {
                    ns: ns_string(&resolved),
                    local: child_start.local_name().into_inner().to_string(),
                    attrs: read_attrs(&child_start)?,
                    text: String::new(),
                    children: Vec::new(),
                });
            }
            Ok((_, Event::End(_))) => break,
            Ok((_, Event::Text(text))) => {
                node.text.push_str(&unescape_text(&text));
            }
            Ok((_, Event::GeneralRef(reference))) => {
                node.text.push_str(&resolve_reference(&reference));
            }
            Ok((_, Event::CData(text))) => {
                node.text.push_str(text.into_inner().as_ref());
            }
            Ok((_, Event::Eof)) => {
                return Err(Error::Xml(format!(
                    "unexpected end of document inside <{}>",
                    node.local
                )))
            }
            Ok(_) => {}
            Err(e) => return Err(Error::Xml(e.to_string())),
        }
    }
    Ok(node)
}

fn ns_string(resolved: &ResolveResult) -> String {
    match resolved {
        ResolveResult::Bound(Namespace(ns)) => (*ns).to_string(),
        _ => String::new(),
    }
}

fn read_attrs(start: &BytesStart) -> Result<Vec<(String, String)>> {
    let mut attrs = Vec::new();
    for attr in start.attributes() {
        let attr = attr.map_err(|e| Error::Xml(e.to_string()))?;
        let key = attr.key.0.to_string();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| Error::Xml(e.to_string()))?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(attrs)
}

fn unescape_text(text: &quick_xml::events::BytesText) -> String {
    let raw: &str = text.as_ref();
    quick_xml::escape::unescape(raw)
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// Resolves `&amp;`-style predefined entities and numeric character
/// references, which quick-xml reports as separate `Event::GeneralRef`s.
fn resolve_reference(reference: &quick_xml::events::BytesRef) -> String {
    if let Ok(Some(c)) = reference.resolve_char_ref() {
        return c.to_string();
    }
    match reference.as_ref() {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        _ => String::new(),
    }
}

/// The requested property set of a PROPFIND.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropList {
    AllProp,
    PropName,
    Props(Vec<PropQName>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropFindRequest {
    pub props: PropList,
}

/// `address-data` serialisation request from a REPORT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressDataRequest {
    pub content_type: Option<String>,
    pub version: Option<String>,
    pub properties: Vec<String>,
}

impl AddressDataRequest {
    /// The vCard version requested, if any.
    pub fn requested_version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiGetRequest {
    pub props: Vec<PropQName>,
    pub hrefs: Vec<String>,
    pub address_data: AddressDataRequest,
}

/// The `{urn:ietf:params:xml:ns:caldav}calendar-data` request options of a
/// CalDAV REPORT (`Sabre\CalDAV\Xml\Filter\CalendarData`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarDataRequest {
    pub content_type: Option<String>,
    pub version: Option<String>,
    /// Whether an `{urn:…}expand` child is present. The sidecar delegates
    /// `expand` (and `application/calendar+json`) rather than re-serialising.
    pub expand: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarMultiGetRequest {
    pub props: Vec<PropQName>,
    pub hrefs: Vec<String>,
    pub calendar_data: CalendarDataRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCollectionRequest {
    pub sync_token: Option<String>,
    /// Whether a `{DAV:}sync-token` element was present at all. Sabre's
    /// `SyncCollectionReport` **requires** it (and `{DAV:}prop`); CalDAV
    /// reproduces the 400 when either is missing.
    pub has_sync_token: bool,
    /// Whether a `{DAV:}prop` element was present at all.
    pub has_prop: bool,
    pub limit: Option<i64>,
    pub props: Vec<PropQName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRequest {
    pub props: Vec<PropQName>,
    pub filter: Option<AddressBookFilter>,
    pub test: Test,
    pub limit: Option<i64>,
    pub address_data: AddressDataRequest,
}

/// Parses a PROPFIND body. An empty body is `allprop`.
pub fn parse_propfind(xml: &[u8]) -> Result<PropFindRequest> {
    if xml.is_empty() {
        return Ok(PropFindRequest {
            props: PropList::AllProp,
        });
    }
    let root = parse_document(xml)?;
    let props = if root.child(NS_DAV, "allprop").is_some() {
        PropList::AllProp
    } else if root.child(NS_DAV, "propname").is_some() {
        PropList::PropName
    } else if let Some(prop) = root.child(NS_DAV, "prop") {
        PropList::Props(prop.children.iter().map(XNode::qname).collect())
    } else {
        PropList::AllProp
    };
    Ok(PropFindRequest { props })
}

pub fn parse_multiget(xml: &[u8]) -> Result<MultiGetRequest> {
    let root = parse_document(xml)?;
    let (props, address_data) = prop_and_address_data(&root);
    let hrefs = root
        .children_of(NS_DAV, "href")
        .map(|n| n.text.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    Ok(MultiGetRequest {
        props,
        hrefs,
        address_data,
    })
}

pub fn parse_calendar_multiget(xml: &[u8]) -> Result<CalendarMultiGetRequest> {
    let root = parse_document(xml)?;
    let mut props = Vec::new();
    let mut calendar_data = CalendarDataRequest::default();
    if let Some(prop) = root.child(NS_DAV, "prop") {
        for child in &prop.children {
            if child.ns == NS_CALDAV && child.local == "calendar-data" {
                calendar_data.content_type = child.attr("content-type").map(str::to_string);
                calendar_data.version = child.attr("version").map(str::to_string);
                calendar_data.expand = child.child(NS_CALDAV, "expand").is_some();
            }
            props.push(child.qname());
        }
    }
    let hrefs = root
        .children_of(NS_DAV, "href")
        .map(|n| n.text.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    Ok(CalendarMultiGetRequest {
        props,
        hrefs,
        calendar_data,
    })
}

pub fn parse_sync_collection(xml: &[u8]) -> Result<SyncCollectionRequest> {
    let root = parse_document(xml)?;
    let has_sync_token = root.child(NS_DAV, "sync-token").is_some();
    let sync_token = root
        .child(NS_DAV, "sync-token")
        .map(|n| n.text.trim().to_string())
        .filter(|t| !t.is_empty());
    let limit = root.child(NS_DAV, "limit").and_then(nresults_sync);
    let has_prop = root.child(NS_DAV, "prop").is_some();
    let (props, _) = prop_and_address_data(&root);
    Ok(SyncCollectionRequest {
        sync_token,
        has_sync_token,
        has_prop,
        limit,
        props,
    })
}

pub fn parse_query(xml: &[u8]) -> Result<QueryRequest> {
    let root = parse_document(xml)?;
    let (props, address_data) = prop_and_address_data(&root);
    let filter_node = root.child(NS_CARDDAV, "filter");
    let (filter, test) = match filter_node {
        Some(node) => {
            let test = Test::from_attr(node.attr("test")).map_err(Error::bad_request)?;
            (Some(crate::xml::filter::parse_filter(node)?), test)
        }
        None => (None, Test::AnyOf),
    };
    let limit = root.child(NS_DAV, "limit").and_then(nresults);
    Ok(QueryRequest {
        props,
        filter,
        test,
        limit,
        address_data,
    })
}

fn prop_and_address_data(node: &XNode) -> (Vec<PropQName>, AddressDataRequest) {
    let Some(prop) = node.child(NS_DAV, "prop") else {
        return (Vec::new(), AddressDataRequest::default());
    };
    let mut props = Vec::new();
    let mut address_data = AddressDataRequest::default();
    for child in &prop.children {
        if child.ns == NS_CARDDAV && child.local == "address-data" {
            address_data.content_type = child.attr("content-type").map(str::to_string);
            address_data.version = child.attr("version").map(str::to_string);
            address_data.properties = child
                .children_of(NS_CARDDAV, "prop")
                .filter_map(|p| p.attr("name").map(str::to_string))
                .collect();
        }
        props.push(child.qname());
    }
    (props, address_data)
}

fn nresults(limit: &XNode) -> Option<i64> {
    limit
        .children_of(NS_DAV, "nresults")
        .next()
        .and_then(|n| n.text.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
}

/// `{DAV:}nresults` for `sync-collection`.
///
/// `SyncCollectionReport::xmlDeserialize()` stores `(int) $value` and
/// `getChangesForCalendar()` applies it whenever `is_numeric($limit)`, so a
/// literal `0` means `setMaxResults(0)` — **zero rows** — not "no limit".
/// (For `addressbook-query`, Sabre instead tests `if ($report->limit)`, where
/// `0` is falsy, so the shared [`nresults`] keeps dropping it.)
fn nresults_sync(limit: &XNode) -> Option<i64> {
    limit
        .children_of(NS_DAV, "nresults")
        .next()
        .and_then(|n| n.text.trim().parse::<i64>().ok())
        .filter(|n| *n >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::write::NS_NEXTCLOUD;

    #[test]
    fn propfind_allprop() {
        let req = parse_propfind(b"").unwrap();
        assert_eq!(req.props, PropList::AllProp);
        let req = parse_propfind(
            br#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:allprop/></d:propfind>"#,
        )
        .unwrap();
        assert_eq!(req.props, PropList::AllProp);
    }

    #[test]
    fn propfind_explicit_props_are_namespace_resolved() {
        let req = parse_propfind(
            br#"<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:oc="http://owncloud.org/ns"><d:prop><d:displayname/><card:addressbook-description/><oc:groups/></d:prop></d:propfind>"#,
        )
        .unwrap();
        match req.props {
            PropList::Props(props) => {
                assert_eq!(
                    props,
                    vec![
                        PropQName::dav("displayname"),
                        PropQName::carddav("addressbook-description"),
                        PropQName::owncloud("groups"),
                    ]
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn multiget_reads_hrefs_and_address_data() {
        let req = parse_multiget(
            br#"<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><card:address-data content-type="text/vcard" version="3.0"><card:prop name="FN"/></card:address-data></d:prop><d:href>/remote.php/dav/addressbooks/users/a/b/x.vcf</d:href></card:addressbook-multiget>"#,
        )
        .unwrap();
        assert_eq!(
            req.hrefs,
            vec!["/remote.php/dav/addressbooks/users/a/b/x.vcf"]
        );
        assert_eq!(req.address_data.version.as_deref(), Some("3.0"));
        assert_eq!(req.address_data.properties, vec!["FN"]);
        assert!(req.props.contains(&PropQName::dav("getetag")));
    }

    #[test]
    fn sync_collection_reads_token_and_limit() {
        let req = parse_sync_collection(
            br#"<d:sync-collection xmlns:d="DAV:"><d:sync-token>http://sabre.io/ns/sync/9</d:sync-token><d:limit><d:nresults>100</d:nresults></d:limit><d:prop><d:getetag/></d:prop></d:sync-collection>"#,
        )
        .unwrap();
        assert_eq!(req.sync_token.as_deref(), Some("http://sabre.io/ns/sync/9"));
        assert_eq!(req.limit, Some(100));
        assert_eq!(req.props, vec![PropQName::dav("getetag")]);
    }

    #[test]
    fn unknown_namespace_is_preserved() {
        let req = parse_propfind(
            br#"<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.com/ns"><d:prop><nc:has-photo/></d:prop></d:propfind>"#,
        )
        .unwrap();
        match req.props {
            PropList::Props(props) => {
                assert_eq!(props, vec![PropQName::new(NS_NEXTCLOUD, "has-photo")]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
