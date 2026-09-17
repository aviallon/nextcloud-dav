// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A small vCard reader used for `addressbook-query` filtering and for
//! reproducing `CardDavBackend::readBlob()`.
//!
//! It is intentionally not a full vCard library: it only needs enough of the
//! mime-dir grammar to evaluate RFC 6352 §10.5 filters. In particular it
//! unfolds continuation lines, strips group prefixes (`item1.EMAIL`), and
//! unescapes text values the way Sabre's VObject reader does.

/// One `NAME;PARAM=VALUE:value` property occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VProperty {
    /// Upper-cased property name, group prefix removed.
    pub name: String,
    /// Upper-cased parameter names with their (unescaped, unquoted) values.
    pub params: Vec<(String, Vec<String>)>,
    /// Unescaped property value; for compound properties `;` separators remain.
    pub value: String,
}

impl VProperty {
    /// First value of the named (case-insensitive) parameter.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, values)| values.first())
            .map(String::as_str)
    }
}

/// A parsed vCard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VCard {
    pub properties: Vec<VProperty>,
}

impl VCard {
    /// All occurrences of a (case-insensitive) property.
    pub fn select(&self, name: &str) -> impl Iterator<Item = &VProperty> {
        let name = name.to_uppercase();
        self.properties.iter().filter(move |p| p.name == name)
    }

    /// Whether a (case-insensitive) property occurs at all.
    pub fn is_defined(&self, name: &str) -> bool {
        self.select(name).next().is_some()
    }
}

/// Parses a vCard. Invalid UTF-8 is replaced rather than rejected: query
/// evaluation must not fail because of one odd byte.
pub fn parse(data: &[u8]) -> VCard {
    let text = String::from_utf8_lossy(data);
    let mut card = VCard::default();
    for line in logical_lines(&text) {
        if line.trim().is_empty() {
            continue;
        }
        let Some(colon) = find_unquoted_colon(&line) else {
            continue;
        };
        let head = &line[..colon];
        let value = &line[colon + 1..];
        let (name, params) = parse_head(head);
        if name.is_empty() {
            continue;
        }
        card.properties.push(VProperty {
            name,
            params,
            value: unescape(value),
        });
    }
    card
}

/// Unfolds RFC 6352/2426 continuation lines.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(first) = raw.chars().next() {
            if (first == ' ' || first == '\t') && !lines.is_empty() {
                let rest = &raw[first.len_utf8()..];
                if let Some(last) = lines.last_mut() {
                    last.push_str(rest);
                }
                continue;
            }
        }
        lines.push(raw.to_string());
    }
    lines
}

fn parse_head(head: &str) -> (String, Vec<(String, Vec<String>)>) {
    let mut parts = split_unquoted(head, ';').into_iter();
    let raw_name = parts.next().unwrap_or_default();
    // Strip a group prefix: `item1.EMAIL` -> `EMAIL`.
    let name = raw_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_uppercase();
    let mut params = Vec::new();
    for part in parts {
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k.trim().to_uppercase(), v),
            None => (part.trim().to_uppercase(), ""),
        };
        if key.is_empty() {
            continue;
        }
        let values = split_unquoted(value, ',')
            .into_iter()
            .map(|v| unescape(unquote(v.trim())))
            .collect();
        params.push((key, values));
    }
    (name, params)
}

fn find_unquoted_colon(s: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (idx, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => return Some(idx),
            _ => {}
        }
    }
    None
}

fn split_unquoted(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            current.push(c);
            if let Some(next) = chars.next() {
                current.push(next);
            }
            continue;
        }
        if c == '"' {
            in_quotes = !in_quotes;
            current.push(c);
        } else if c == sep && !in_quotes {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    parts
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// RFC 6352 / vCard unescaping of a text value.
pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some('r') | Some('R') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(',') => out.push(','),
            Some(';') => out.push(';'),
            Some(':') => out.push(':'),
            Some('"') => out.push('"'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Faithful port of `CardDavBackend::readBlob()`: drop `PHOTO:data:` payloads
/// that are not images, together with their folded continuation lines.
///
/// Returns the (possibly filtered) bytes and whether anything was removed. The
/// caller must recompute `size` when `true`, matching PHP.
pub fn filter_read_blob(carddata: &[u8]) -> (Vec<u8>, bool) {
    // Micro-optimisation from PHP: a blob that starts with a photo is returned
    // verbatim.
    if carddata.starts_with(b"PHOTO:data:") {
        return (carddata.to_vec(), false);
    }

    let lines = split_crlf(carddata);
    let mut filtered: Vec<&[u8]> = Vec::with_capacity(lines.len());
    let mut removing_photo = false;
    let mut modified = false;
    for line in lines {
        if line.starts_with(b"PHOTO:data:") && !line.starts_with(b"PHOTO:data:image/") {
            removing_photo = true;
            modified = true;
            continue;
        }
        if removing_photo {
            if line.starts_with(b" ") {
                continue;
            }
            removing_photo = false;
        }
        filtered.push(line);
    }
    (join_crlf(&filtered), modified)
}

/// Whether a card has a photo that is either a URL or an image data URI,
/// mirroring `HasPhotoPlugin::propFind()`.
pub fn has_photo(carddata: &[u8]) -> bool {
    parse(carddata)
        .select("PHOTO")
        .any(|p| !p.value.starts_with("data:") || p.value.starts_with("data:image/"))
}

fn split_crlf(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == b'\r' && data[i + 1] == b'\n' {
            out.push(&data[start..i]);
            start = i + 2;
            i += 2;
        } else {
            i += 1;
        }
    }
    out.push(&data[start..]);
    out
}

fn join_crlf(lines: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &[u8] = b"BEGIN:VCARD\r\n\
VERSION:3.0\r\n\
UID:1234\r\n\
FN:Jane Doe\r\n\
N:Doe;Jane;;;\r\n\
EMAIL;TYPE=WORK,INTERNET:jane@example.com\r\n\
EMAIL;TYPE=HOME:jane@home.example\r\n\
TEL;TYPE=CELL:+1 555 0100\r\n\
CATEGORIES:Friends,Work\r\n\
NOTE:Line one\\nLine two, with comma\\; and semicolon\r\n\
item1.X-ABLabel:Custom\r\n\
END:VCARD\r\n";

    #[test]
    fn parses_basic_properties() {
        let card = parse(CARD);
        assert!(card.is_defined("FN"));
        assert_eq!(card.select("FN").next().unwrap().value, "Jane Doe");
        assert_eq!(card.select("N").next().unwrap().value, "Doe;Jane;;;");
        assert_eq!(card.select("EMAIL").count(), 2);
        assert_eq!(
            card.select("EMAIL").next().unwrap().param("TYPE"),
            Some("WORK")
        );
    }

    #[test]
    fn unescapes_values() {
        let card = parse(CARD);
        let note = card.select("NOTE").next().unwrap();
        assert_eq!(note.value, "Line one\nLine two, with comma; and semicolon");
    }

    #[test]
    fn strips_group_prefix() {
        let card = parse(CARD);
        assert!(card.is_defined("X-ABLABEL"));
    }

    #[test]
    fn unfolds_continuation_lines() {
        let card = parse(b"BEGIN:VCARD\r\nNOTE:one\r\n two\r\nEND:VCARD\r\n");
        assert_eq!(card.select("NOTE").next().unwrap().value, "onetwo");
    }

    #[test]
    fn read_blob_keeps_image_data() {
        let data = b"BEGIN:VCARD\r\nPHOTO:data:image/jpeg;base64,AAAA\r\nEND:VCARD\r\n";
        let (out, modified) = filter_read_blob(data);
        assert!(!modified);
        assert_eq!(out, data);
    }

    #[test]
    fn read_blob_strips_non_image_photo_and_folds() {
        let data =
            b"BEGIN:VCARD\r\nPHOTO:data:text/plain;base64,AAAA\r\n AAAA\r\nFN:X\r\nEND:VCARD\r\n";
        let (out, modified) = filter_read_blob(data);
        assert!(modified);
        assert_eq!(out, b"BEGIN:VCARD\r\nFN:X\r\nEND:VCARD\r\n".to_vec());
    }

    #[test]
    fn read_blob_rewrites_crlf_identically_when_unmodified() {
        let (out, modified) = filter_read_blob(CARD);
        assert!(!modified);
        assert_eq!(out, CARD);
    }

    #[test]
    fn detects_photo() {
        assert!(has_photo(
            b"BEGIN:VCARD\r\nPHOTO;VALUE=uri:https://example.com/a.jpg\r\nEND:VCARD\r\n"
        ));
        assert!(has_photo(
            b"BEGIN:VCARD\r\nPHOTO:data:image/png;base64,AA\r\nEND:VCARD\r\n"
        ));
        assert!(!has_photo(
            b"BEGIN:VCARD\r\nPHOTO:data:text/plain;base64,AA\r\nEND:VCARD\r\n"
        ));
        assert!(!has_photo(b"BEGIN:VCARD\r\nFN:X\r\nEND:VCARD\r\n"));
    }
}
