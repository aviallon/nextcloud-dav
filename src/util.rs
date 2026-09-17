// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Small HTTP/path helpers.

use axum::http::HeaderValue;
use base64::Engine;

/// Percent-decodes a URL path segment, leaving invalid escapes untouched.
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes one path segment the way `Sabre\HTTP\encodePathSegment()`
/// does: `A-Za-z0-9_-~():@` are left alone, everything else is `%xx`
/// (lower-case hex).
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '~' | '(' | ')' | ':' | '@') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02x}"));
        }
    }
    out
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parses an HTTP Basic `Authorization` header.
pub fn parse_basic_auth(header: Option<&HeaderValue>) -> Option<(String, String)> {
    let value = header?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

/// Formats a unix timestamp as an HTTP date (`getlastmodified`).
pub fn http_date(timestamp: i64) -> String {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(timestamp.max(0) as u64);
    httpdate::fmt_http_date(time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("al%20ice"), "al ice");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn path_segment_encoding_matches_sabre() {
        assert_eq!(encode_path_segment("contacts"), "contacts");
        assert_eq!(encode_path_segment("a b.vcf"), "a%20b.vcf");
        assert_eq!(encode_path_segment("a/b"), "a%2fb");
        assert_eq!(encode_path_segment("user@example.com"), "user@example.com");
        assert_eq!(encode_path_segment("x(y)"), "x(y)");
    }

    #[test]
    fn basic_auth_parsing() {
        let value = HeaderValue::from_str(&format!(
            "Basic {}",
            STANDARD.encode("alice:s3cret:with:colons")
        ))
        .unwrap();
        assert_eq!(
            parse_basic_auth(Some(&value)),
            Some(("alice".to_string(), "s3cret:with:colons".to_string()))
        );
        let bearer = HeaderValue::from_static("Bearer abc");
        assert_eq!(parse_basic_auth(Some(&bearer)), None);
        assert_eq!(parse_basic_auth(None), None);
    }

    #[test]
    fn http_date_is_rfc1123() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
    }
}
