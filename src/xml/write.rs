// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `{DAV:}multistatus` serialisation.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use std::io::Write;

pub const NS_DAV: &str = "DAV:";
pub const NS_CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const NS_CALENDARSERVER: &str = "http://calendarserver.org/ns/";
pub const NS_SABREDAV: &str = "http://sabredav.org/ns";
pub const NS_OWNCLOUD: &str = "http://owncloud.org/ns";
pub const NS_NEXTCLOUD: &str = "http://nextcloud.com/ns";
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
            NS_CALENDARSERVER => Some("cs"),
            NS_SABREDAV => Some("s"),
            NS_OWNCLOUD => Some("oc"),
            NS_NEXTCLOUD | NS_NEXTCLOUD_FILES => Some("nc"),
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
    /// `ocs`), matching a Nextcloud files PROPFIND. Nextcloud registers `ocs`
    /// only client-side (it is serialised ad-hoc by Sabre); the sidecar declares
    /// it up front, which is namespace-equivalent on the wire.
    pub const FILES_NAMESPACES: &'static [(&'static str, &'static str)] = &[
        ("d", NS_DAV),
        ("s", NS_SABREDAV),
        ("oc", NS_OWNCLOUD),
        ("nc", NS_NEXTCLOUD_FILES),
        ("ocs", NS_OCS),
    ];

    pub fn to_xml(&self) -> String {
        self.to_xml_with(Self::CARDDAV_NAMESPACES)
    }

    pub fn to_xml_files(&self) -> String {
        self.to_xml_with(Self::FILES_NAMESPACES)
    }

    pub fn to_xml_with(&self, namespaces: &[(&str, &str)]) -> String {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        self.write_to(&mut writer, namespaces)
            .expect("writing XML to a Vec cannot fail");
        // The writer only ever emits UTF-8 (all inputs are Rust `String`s).
        String::from_utf8(writer.into_inner()).expect("quick-xml writes UTF-8")
    }

    fn write_to<W: Write>(
        &self,
        writer: &mut Writer<W>,
        namespaces: &[(&str, &str)],
    ) -> std::io::Result<()> {
        writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))?;
        let mut root = BytesStart::new("d:multistatus");
        for (prefix, uri) in namespaces {
            root.push_attribute((format!("xmlns:{prefix}").as_str(), *uri));
        }
        writer.write_event(Event::Start(root.borrow()))?;
        for response in &self.responses {
            self.write_response(writer, response)?;
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
                write_prop(writer, qname, value)?;
            }
            writer.write_event(Event::End(BytesEnd::new("d:prop")))?;
            write_text_element(writer, "d:status", &status_line(propstat.status))?;
            writer.write_event(Event::End(BytesEnd::new("d:propstat")))?;
        }

        if !wrote_propstat {
            match response.status {
                Some(status) => {
                    write_text_element(writer, "d:status", &status_line(status))?;
                }
                None => {
                    // WebDAV requires at least one propstat when there is no
                    // status; Sabre emits an empty 418 propstat.
                    writer.write_event(Event::Start(BytesStart::new("d:propstat")))?;
                    writer.write_event(Event::Empty(BytesStart::new("d:prop")))?;
                    write_text_element(writer, "d:status", &status_line(418))?;
                    writer.write_event(Event::End(BytesEnd::new("d:propstat")))?;
                }
            }
        } else if let Some(status) = response.status {
            write_text_element(writer, "d:status", &status_line(status))?;
        }

        writer.write_event(Event::End(BytesEnd::new("d:response")))?;
        Ok(())
    }
}

fn write_prop<W: Write>(
    writer: &mut Writer<W>,
    qname: &PropQName,
    value: &PropValue,
) -> std::io::Result<()> {
    let (name, unknown_ns) = match qname.prefix() {
        Some(prefix) => (format!("{prefix}:{}", qname.local), None),
        None => (format!("x:{}", qname.local), Some(qname.ns.as_str())),
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
            writer.write_event(Event::Start(start.borrow()))?;
            writer.write_event(Event::Text(BytesText::new(text)))?;
            writer.write_event(Event::End(start.to_end()))?;
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
