// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `{DAV:}multistatus` serialisation.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use std::collections::HashSet;
use std::io::Write;

pub const NS_DAV: &str = "DAV:";
pub const NS_CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const NS_CALENDARSERVER: &str = "http://calendarserver.org/ns/";
pub const NS_SABREDAV: &str = "http://sabredav.org/ns";
pub const NS_OWNCLOUD: &str = "http://owncloud.org/ns";
pub const NS_NEXTCLOUD: &str = "http://nextcloud.com/ns";
/// The iCal namespace (`http://apple.com/ns/ical/`), bound to the `apple`
/// prefix in a CalDAV multistatus.
pub const NS_APPLE: &str = "http://apple.com/ns/ical/";
/// The **files** `nc` namespace (`FilesPlugin::NS_NEXTCLOUD`). It differs from
/// the CardDAV/CalDAV sharing one (`NS_NEXTCLOUD`) and is bound to the `nc`
/// prefix in a files multistatus, exactly like Nextcloud.
pub const NS_NEXTCLOUD_FILES: &str = "http://nextcloud.org/ns";
/// The Open Collaboration Services namespace (`ocs`), used by the files
/// property `ocs:share-permissions` (`FilesPlugin::SHARE_PERMISSIONS_PROPERTYNAME`).
pub const NS_OCS: &str = "http://open-collaboration-services.org/ns";

/// A namespace-qualified element name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PropQName {
    pub ns: String,
    pub local: String,
}

impl PropQName {
    pub fn new(ns: impl Into<String>, local: impl Into<String>) -> Self {
        Self {
            ns: ns.into(),
            local: local.into(),
        }
    }

    /// The prefix Sabre/Nextcloud use for a namespace, if known.
    pub fn prefix(&self) -> Option<&'static str> {
        match self.ns.as_str() {
            NS_DAV => Some("d"),
            NS_CARDDAV => Some("card"),
            NS_CALDAV => Some("cal"),
            NS_CALENDARSERVER => Some("cs"),
            NS_SABREDAV => Some("s"),
            NS_OWNCLOUD => Some("oc"),
            NS_NEXTCLOUD | NS_NEXTCLOUD_FILES => Some("nc"),
            NS_APPLE => Some("apple"),
            NS_OCS => Some("ocs"),
            _ => None,
        }
    }

    pub fn dav(local: &str) -> Self {
        Self::new(NS_DAV, local)
    }

    pub fn carddav(local: &str) -> Self {
        Self::new(NS_CARDDAV, local)
    }

    pub fn owncloud(local: &str) -> Self {
        Self::new(NS_OWNCLOUD, local)
    }

    pub fn nextcloud(local: &str) -> Self {
        Self::new(NS_NEXTCLOUD, local)
    }
}

/// A generic XML element used for structured property values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlElement {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub text: Option<String>,
    pub children: Vec<XmlElement>,
}

impl XmlElement {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            attributes: Vec::new(),
            text: None,
            children: Vec::new(),
        }
    }

    pub fn attr(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.push((key.into(), value.into()));
        self
    }

    pub fn text(mut self, value: impl Into<String>) -> Self {
        self.text = Some(value.into());
        self
    }

    pub fn child(mut self, child: XmlElement) -> Self {
        self.children.push(child);
        self
    }

    pub fn children(mut self, children: impl IntoIterator<Item = XmlElement>) -> Self {
        self.children.extend(children);
        self
    }
}

/// The value of a property in a `propstat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropValue {
    Empty,
    Text(String),
    Elements(Vec<XmlElement>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropStat {
    pub status: u16,
    pub props: Vec<(PropQName, PropValue)>,
}

impl PropStat {
    pub fn ok(props: Vec<(PropQName, PropValue)>) -> Self {
        Self { status: 200, props }
    }

    pub fn not_found(props: Vec<PropQName>) -> Self {
        Self {
            status: 404,
            props: props.into_iter().map(|q| (q, PropValue::Empty)).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResponse {
    pub href: String,
    pub propstats: Vec<PropStat>,
    /// A response-level status, used by `sync-collection` for deleted items and
    /// the truncation marker.
    pub status: Option<u16>,
}

impl DavResponse {
    pub fn props(href: impl Into<String>, propstats: Vec<PropStat>) -> Self {
        Self {
            href: href.into(),
            propstats,
            status: None,
        }
    }

    pub fn status(href: impl Into<String>, status: u16) -> Self {
        Self {
            href: href.into(),
            propstats: Vec::new(),
            status: Some(status),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MultiStatus {
    pub responses: Vec<DavResponse>,
    /// Emitted as `<d:sync-token>` after the responses.
    pub sync_token: Option<String>,
}

impl MultiStatus {
    /// CardDAV's namespace map (`d`, `card`, `cs`, `s`, `oc`, `nc`).
    pub const CARDDAV_NAMESPACES: &'static [(&'static str, &'static str)] = &[
        ("d", NS_DAV),
        ("card", NS_CARDDAV),
        ("cs", NS_CALENDARSERVER),
        ("s", NS_SABREDAV),
        ("oc", NS_OWNCLOUD),
        ("nc", NS_NEXTCLOUD),
    ];

    /// The files namespace map (`d`, `s`, `oc`, `nc` = the `.org` namespace,
    /// `ocs`), matching a Nextcloud files PROPFIND. It is a *candidate* set:
    /// `write_to` declares only the prefixes actually used by the emitted
    /// qnames, so a response that does not carry `ocs:share-permissions` no
    /// longer declares `xmlns:ocs` (PHP never did).
    pub const FILES_NAMESPACES: &'static [(&'static str, &'static str)] = &[
        ("d", NS_DAV),
        ("s", NS_SABREDAV),
        ("oc", NS_OWNCLOUD),
        ("nc", NS_NEXTCLOUD_FILES),
        ("ocs", NS_OCS),
    ];

    /// The DAV-root discovery namespace map, matching PHP's
    /// `PROPFIND /remote.php/dav/` response (`d`, `s`, `oc`, `nc`).
    pub const DAV_ROOT_NAMESPACES: &'static [(&'static str, &'static str)] = &[
        ("d", NS_DAV),
        ("s", NS_SABREDAV),
        ("oc", NS_OWNCLOUD),
        ("nc", NS_NEXTCLOUD),
    ];

    /// The principal discovery namespace map, matching PHP's
    /// `PROPFIND /remote.php/dav/principals/users/<uid>/` response
    /// (`d`, `s`, `cal`, `cs`, `card`, `oc`, `nc`).
    pub const PRINCIPAL_NAMESPACES: &'static [(&'static str, &'static str)] = &[
        ("d", NS_DAV),
        ("s", NS_SABREDAV),
        ("cal", NS_CALDAV),
        ("cs", NS_CALENDARSERVER),
        ("card", NS_CARDDAV),
        ("oc", NS_OWNCLOUD),
        ("nc", NS_NEXTCLOUD),
    ];

    /// The CalDAV namespace map (`d`, `s`, `cal`, `cs`, `oc`, `nc`, `apple`),
    /// matching a Nextcloud CalDAV `PROPFIND` response.
    pub const CALDAV_NAMESPACES: &'static [(&'static str, &'static str)] = &[
        ("d", NS_DAV),
        ("s", NS_SABREDAV),
        ("cal", NS_CALDAV),
        ("cs", NS_CALENDARSERVER),
        ("oc", NS_OWNCLOUD),
        ("nc", NS_NEXTCLOUD),
        ("apple", NS_APPLE),
    ];

    pub fn to_xml(&self) -> String {
        self.to_xml_with(Self::CARDDAV_NAMESPACES)
    }

    pub fn to_xml_files(&self) -> String {
        self.to_xml_with(Self::FILES_NAMESPACES)
    }

    pub fn to_xml_caldav(&self) -> String {
        self.to_xml_with(Self::CALDAV_NAMESPACES)
    }

    pub fn to_xml_with(&self, namespaces: &[(&str, &str)]) -> String {
        // Compact, like PHP: no indentation and no newlines between elements.
        let mut writer = Writer::new(Vec::new());
        self.write_to(&mut writer, namespaces)
            .expect("writing XML to a Vec cannot fail");
        // The writer only ever emits UTF-8 (all inputs are Rust `String`s).
        String::from_utf8(writer.into_inner()).expect("quick-xml writes UTF-8")
    }

    /// The subset of `namespaces` whose prefix is actually used by the emitted
    /// document, in the order given. `d` is always kept: the structural
    /// elements (`multistatus`, `response`, `propstat`, `prop`, `href`,
    /// `status`, `sync-token`) are all `d:`-prefixed.
    fn used_namespaces<'a>(
        &'a self,
        namespaces: &'a [(&'a str, &'a str)],
    ) -> Vec<(&'a str, &'a str)> {
        let mut used: HashSet<&'a str> = HashSet::new();
        used.insert("d");
        for response in &self.responses {
            for propstat in &response.propstats {
                for (qname, value) in &propstat.props {
                    if let Some(prefix) = qname.prefix() {
                        used.insert(prefix);
                    }
                    collect_element_prefixes(value, &mut used);
                }
            }
        }
        namespaces
            .iter()
            .filter(|(prefix, _)| used.contains(prefix))
            .copied()
            .collect()
    }

    fn write_to<W: Write>(
        &self,
        writer: &mut Writer<W>,
        namespaces: &[(&str, &str)],
    ) -> std::io::Result<()> {
        // PHP/Sabre emit `<?xml version="1.0"?>` with no encoding pseudo-attribute.
        writer.write_event(Event::Decl(BytesDecl::new("1.0", None, None)))?;
        let mut root = BytesStart::new("d:multistatus");
        for (prefix, uri) in self.used_namespaces(namespaces) {
            root.push_attribute((format!("xmlns:{prefix}").as_str(), uri));
        }
        writer.write_event(Event::Start(root.borrow()))?;
        // Reused across every property of every response so serialising a
        // large listing does not allocate one `String` per prop qname.
        let mut name = String::new();
        for response in &self.responses {
            self.write_response(writer, response, &mut name)?;
        }
        if let Some(token) = &self.sync_token {
            write_text_element(writer, "d:sync-token", token)?;
        }
        writer.write_event(Event::End(BytesEnd::new("d:multistatus")))?;
        Ok(())
    }

    fn write_response<W: Write>(
        &self,
        writer: &mut Writer<W>,
        response: &DavResponse,
        name: &mut String,
    ) -> std::io::Result<()> {
        writer.write_event(Event::Start(BytesStart::new("d:response")))?;
        write_text_element(writer, "d:href", &response.href)?;

        let mut wrote_propstat = false;
        for propstat in &response.propstats {
            if propstat.props.is_empty() {
                continue;
            }
            wrote_propstat = true;
            writer.write_event(Event::Start(BytesStart::new("d:propstat")))?;
            writer.write_event(Event::Start(BytesStart::new("d:prop")))?;
            for (qname, value) in &propstat.props {
                write_prop(writer, qname, value, name)?;
            }
            writer.write_event(Event::End(BytesEnd::new("d:prop")))?;
            write_status(writer, propstat.status)?;
            writer.write_event(Event::End(BytesEnd::new("d:propstat")))?;
        }

        if !wrote_propstat {
            match response.status {
                Some(status) => {
                    write_status(writer, status)?;
                }
                None => {
                    // WebDAV requires at least one propstat when there is no
                    // status; Sabre emits an empty 418 propstat.
                    writer.write_event(Event::Start(BytesStart::new("d:propstat")))?;
                    writer.write_event(Event::Empty(BytesStart::new("d:prop")))?;
                    write_status(writer, 418)?;
                    writer.write_event(Event::End(BytesEnd::new("d:propstat")))?;
                }
            }
        } else if let Some(status) = response.status {
            write_status(writer, status)?;
        }

        writer.write_event(Event::End(BytesEnd::new("d:response")))?;
        Ok(())
    }
}

fn write_prop<W: Write>(
    writer: &mut Writer<W>,
    qname: &PropQName,
    value: &PropValue,
    name: &mut String,
) -> std::io::Result<()> {
    name.clear();
    let unknown_ns = match qname.prefix() {
        Some(prefix) => {
            name.push_str(prefix);
            name.push(':');
            name.push_str(&qname.local);
            None
        }
        None => {
            name.push_str("x:");
            name.push_str(&qname.local);
            Some(qname.ns.as_str())
        }
    };
    let mut start = BytesStart::new(name.as_str());
    if let Some(ns) = unknown_ns {
        start.push_attribute(("xmlns:x", ns));
    }
    match value {
        PropValue::Empty => {
            writer.write_event(Event::Empty(start))?;
        }
        PropValue::Text(text) => {
            if text.is_empty() {
                // An empty value is an empty element, exactly like PHP/Sabre
                // (`<oc:share-types/>`, not `<oc:share-types></oc:share-types>`):
                // the two are XML-equivalent but the pair costs 16 bytes per
                // child on a large listing.
                writer.write_event(Event::Empty(start))?;
            } else {
                writer.write_event(Event::Start(start.borrow()))?;
                writer.write_event(Event::Text(BytesText::new(text)))?;
                writer.write_event(Event::End(start.to_end()))?;
            }
        }
        PropValue::Elements(children) => {
            writer.write_event(Event::Start(start.borrow()))?;
            for child in children {
                write_element(writer, child)?;
            }
            writer.write_event(Event::End(start.to_end()))?;
        }
    }
    Ok(())
}

fn write_element<W: Write>(writer: &mut Writer<W>, element: &XmlElement) -> std::io::Result<()> {
    let mut start = BytesStart::new(element.name.as_str());
    for (key, value) in &element.attributes {
        start.push_attribute((key.as_str(), value.as_str()));
    }
    if element.children.is_empty() {
        if let Some(text) = &element.text {
            if text.is_empty() {
                writer.write_event(Event::Start(start.borrow()))?;
                writer.write_event(Event::End(start.to_end()))?;
            } else {
                writer.write_event(Event::Start(start.borrow()))?;
                writer.write_event(Event::Text(BytesText::new(text)))?;
                writer.write_event(Event::End(start.to_end()))?;
            }
        } else {
            writer.write_event(Event::Empty(start))?;
        }
        return Ok(());
    }
    writer.write_event(Event::Start(start.borrow()))?;
    if let Some(text) = &element.text {
        writer.write_event(Event::Text(BytesText::new(text)))?;
    }
    for child in &element.children {
        write_element(writer, child)?;
    }
    writer.write_event(Event::End(start.to_end()))?;
    Ok(())
}

/// Records the namespace prefixes referenced by a structured property value's
/// nested elements (`d:collection`, `card:address-data-type`, …). Unqualified
/// names (the metadata JSON keys) carry no prefix and are skipped.
fn collect_element_prefixes<'a>(value: &'a PropValue, used: &mut HashSet<&'a str>) {
    if let PropValue::Elements(children) = value {
        for child in children {
            collect_element_prefix(child, used);
        }
    }
}

fn collect_element_prefix<'a>(element: &'a XmlElement, used: &mut HashSet<&'a str>) {
    if let Some((prefix, _)) = element.name.split_once(':') {
        used.insert(prefix);
    }
    for child in &element.children {
        collect_element_prefix(child, used);
    }
}

fn write_text_element<W: Write>(
    writer: &mut Writer<W>,
    name: &str,
    text: &str,
) -> std::io::Result<()> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::Text(BytesText::new(text)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))?;
    Ok(())
}

/// The status line as a `'static` string for the statuses the sidecar actually
/// emits, so serialising a large listing does not allocate one `String` per
/// `propstat`. `None` means the caller must fall back to [`status_line`].
fn status_line_static(status: u16) -> Option<&'static str> {
    Some(match status {
        200 => "HTTP/1.1 200 OK",
        207 => "HTTP/1.1 207 Multi-Status",
        403 => "HTTP/1.1 403 Forbidden",
        404 => "HTTP/1.1 404 Not Found",
        409 => "HTTP/1.1 409 Conflict",
        418 => "HTTP/1.1 418 I'm a teapot",
        507 => "HTTP/1.1 507 Insufficient Storage",
        _ => return None,
    })
}

fn write_status<W: Write>(writer: &mut Writer<W>, status: u16) -> std::io::Result<()> {
    match status_line_static(status) {
        Some(line) => write_text_element(writer, "d:status", line),
        None => write_text_element(writer, "d:status", &status_line(status)),
    }
}

pub fn status_line(status: u16) -> String {
    let reason = match status {
        200 => "OK",
        207 => "Multi-Status",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        418 => "I'm a teapot",
        507 => "Insufficient Storage",
        _ => "Unknown",
    };
    format!("HTTP/1.1 {status} {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::parse::parse_document;

    #[test]
    fn multistatus_roundtrips() {
        let ms = MultiStatus {
            responses: vec![
                DavResponse::props(
                    "/remote.php/dav/addressbooks/users/alice/",
                    vec![PropStat::ok(vec![
                        (
                            PropQName::dav("resourcetype"),
                            PropValue::Elements(vec![XmlElement::new("d:collection")]),
                        ),
                        (
                            PropQName::owncloud("groups"),
                            PropValue::Elements(vec![XmlElement::new("oc:group").text("Friends")]),
                        ),
                    ])],
                ),
                DavResponse::status(
                    "/remote.php/dav/addressbooks/users/alice/contacts/gone.vcf",
                    404,
                ),
            ],
            sync_token: Some("http://sabre.io/ns/sync/5".to_string()),
        };
        let xml = ms.to_xml();
        let doc = parse_document(xml.as_bytes()).unwrap();
        assert_eq!(doc.local, "multistatus");
        assert_eq!(doc.children.len(), 3);
        let sync = &doc.children[2];
        assert_eq!(sync.local, "sync-token");
        assert_eq!(sync.text, "http://sabre.io/ns/sync/5");
        let first = &doc.children[0];
        assert_eq!(first.local, "response");
        assert!(xml.contains("HTTP/1.1 404 Not Found"));
    }

    #[test]
    fn escapes_text() {
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
        let doc = parse_document(xml.as_bytes()).unwrap();
        let prop = &doc.children[0].children[1].children[0].children[0];
        assert_eq!(prop.text, "A & B <c>");
    }
}
