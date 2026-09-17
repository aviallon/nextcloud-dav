// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! vCard validation for the write path.
//!
//! Mirrors the *rejecting* half of Sabre's CardDAV validation without ever
//! re-serialising a clean card. Sabre runs `VObject\Reader::read()` /
//! `readJson()` in `CardDAV\Plugin::validateVCard()` and then
//! `VObject\Node::validate(PROFILE_CARDDAV)`. We reproduce the rules that apply
//! to a single-card `PUT`:
//!
//! * jCard (a body starting with `[`) is **not** implemented; it is rejected
//!   with 415 rather than silently re-serialised to vCard.
//! * `VERSION` must occur exactly once and be `3.0` or `4.0`. vCard `2.1` is
//!   forbidden under `PROFILE_CARDDAV`; anything else is rejected (415).
//! * `UID` is mandatory; `CardDavBackend::getUID()` throws `BadRequest` (400)
//!   when it is missing.
//! * `FN` must occur exactly once (415).
//! * A property name must match `^[A-Z0-9-]+$` after group-prefix removal (415).
//! * Exactly one `BEGIN:VCARD`/`END:VCARD` pair (415).
//! * Control characters `[\x00-\x08\x0B-\x0C\x0E-\x1F\x7F]` are rejected (415).
//!
//! Resource limits (none of which a vCard library enforces) are applied to the
//! *raw content lines* before any parsing library sees the bytes, so a hostile
//! upload cannot blow up memory:
//!
//! * logical line length after unfolding: ≤ 256 KiB;
//! * property count: ≤ 10 000;
//! * parameters per property: ≤ 64;
//! * parameter value length: ≤ 4 KiB.
//!
//! `DAV\StringUtil::ensureUTF8()` is reproduced first: bytes that are not valid
//! UTF-8 are transcoded from ISO-8859-1. The stored bytes are the normalised
//! request bytes **verbatim** — never a re-serialisation.

use crate::vcard;

/// 256 KiB maximum logical (unfolded) content-line length.
pub const MAX_LOGICAL_LINE_LENGTH: usize = 256 * 1024;
/// Maximum number of properties in one card.
pub const MAX_PROPERTY_COUNT: usize = 10_000;
/// Maximum parameters on a single property.
pub const MAX_PARAMETERS_PER_PROPERTY: usize = 64;
/// Maximum byte length of a single parameter value.
pub const MAX_PARAMETER_VALUE_LENGTH: usize = 4 * 1024;

/// Why a card was rejected, and the HTTP status Sabre would use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    /// Sabre maps parse errors and validation failures to 415.
    UnsupportedMediaType(String),
    /// `getUID()` throws `BadRequest` for a missing UID.
    BadRequest(String),
}

impl Reject {
    pub fn message(&self) -> &str {
        match self {
            Reject::UnsupportedMediaType(message) | Reject::BadRequest(message) => message,
        }
    }
}

/// A card that passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCard {
    /// The request bytes after `ensureUTF8`, stored verbatim.
    pub data: Vec<u8>,
    /// The first `UID` value (Sabre's `$vCard->UID->getValue()`).
    pub uid: String,
}

/// `DAV\StringUtil::ensureUTF8()`: pass valid UTF-8 through, transcode
/// ISO-8859-1 otherwise.
pub fn ensure_utf8(raw: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(raw).is_ok() {
        raw.to_vec()
    } else {
        raw.iter()
            .map(|byte| *byte as char)
            .collect::<String>()
            .into_bytes()
    }
}

/// One unfolded content line, as needed by the validator.
struct ContentLine {
    /// Upper-cased property name, group prefix removed.
    name: String,
    value: String,
}

/// `(property name, parameters)` returned by [`parse_head`].
type PropertyHead = (String, Vec<(String, Vec<String>)>);

fn is_forbidden_control(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F)
}

/// Splits into unfolded logical lines, preserving raw line length. This is the
/// same unfolding `Sabre\VObject\Parser\MimeDir` performs (a leading space or
/// tab continues the previous line).
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for physical in text.split('\n') {
        let physical = physical.strip_suffix('\r').unwrap_or(physical);
        if let Some(first) = physical.chars().next() {
            if (first == ' ' || first == '\t') && !lines.is_empty() {
                lines
                    .last_mut()
                    .expect("non-empty")
                    .push_str(&physical[first.len_utf8()..]);
                continue;
            }
        }
        lines.push(physical.to_string());
    }
    lines
}

fn scan_lines(text: &str) -> Result<Vec<ContentLine>, Reject> {
    let logical = unfold(text);
    if logical.len() > MAX_PROPERTY_COUNT * 4 {
        // Cheap guard before allocating per-line structures.
        return Err(Reject::UnsupportedMediaType(
            "Validation error in vCard: too many lines".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(logical.len());
    for line in &logical {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LOGICAL_LINE_LENGTH {
            return Err(Reject::UnsupportedMediaType(
                "Validation error in vCard: content line exceeds the maximum length".to_string(),
            ));
        }
        let Some(colon) = vcard::find_unquoted_colon(line) else {
            return Err(Reject::UnsupportedMediaType(
                "Validation error in vCard: property has no value separator".to_string(),
            ));
        };
        let head = &line[..colon];
        let value = &line[colon + 1..];
        let (name, _params) = parse_head(head)?;
        out.push(ContentLine {
            name,
            value: value.to_string(),
        });
    }
    if out.len() > MAX_PROPERTY_COUNT {
        return Err(Reject::UnsupportedMediaType(
            "Validation error in vCard: too many properties".to_string(),
        ));
    }
    Ok(out)
}

/// Parses `[group.]NAME[;PARAM=VALUE;...]`, uppercasing the name and enforcing
/// the property-name charset and the per-property parameter limits.
fn parse_head(head: &str) -> Result<PropertyHead, Reject> {
    let mut parts = vcard::split_unquoted(head, ';').into_iter();
    let raw_name = parts.next().unwrap_or_default();
    // A group prefix is everything up to the last `.`; the property name follows.
    let name = raw_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_uppercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(Reject::UnsupportedMediaType(format!(
            "Validation error in vCard: invalid property name {raw_name:?}"
        )));
    }
    let mut params = Vec::new();
    for part in parts {
        if params.len() >= MAX_PARAMETERS_PER_PROPERTY {
            return Err(Reject::UnsupportedMediaType(
                "Validation error in vCard: too many parameters on a property".to_string(),
            ));
        }
        let (key, raw_value) = match part.split_once('=') {
            Some((key, value)) => (key.trim().to_uppercase(), value),
            None => (part.trim().to_uppercase(), ""),
        };
        if raw_value.trim().len() > MAX_PARAMETER_VALUE_LENGTH {
            return Err(Reject::UnsupportedMediaType(
                "Validation error in vCard: parameter value exceeds the maximum length".to_string(),
            ));
        }
        let values = vcard::split_unquoted(raw_value, ',')
            .into_iter()
            .map(|value| vcard::unquote(value.trim()).to_string())
            .collect();
        params.push((key, values));
    }
    Ok((name, params))
}

/// Validates a PUT body and returns the bytes to store plus the parsed UID.
pub fn validate(raw: &[u8]) -> Result<ValidatedCard, Reject> {
    // jCard is a distinct media type (`^\[`); we do not implement it and reject
    // rather than re-serialise to vCard (which would break byte stability).
    if raw.first() == Some(&b'[') {
        return Err(Reject::UnsupportedMediaType(
            "This resource only supports valid vCard data; jCard is not implemented".to_string(),
        ));
    }
    if raw.is_empty() {
        return Err(Reject::UnsupportedMediaType(
            "This resource only supports valid vCard or jCard data. Parse error: empty body"
                .to_string(),
        ));
    }

    let data = ensure_utf8(raw);

    // Control characters are rejected before parsing, on the normalised bytes.
    if data.iter().copied().any(is_forbidden_control) {
        return Err(Reject::UnsupportedMediaType(
            "Validation error in vCard: value contains control characters".to_string(),
        ));
    }

    let text = String::from_utf8(data.clone()).expect("ensure_utf8 emits valid UTF-8");
    let lines = scan_lines(&text)?;

    // Exactly one BEGIN:VCARD / END:VCARD pair.
    let begins = lines
        .iter()
        .filter(|line| line.name == "BEGIN" && line.value.trim().eq_ignore_ascii_case("VCARD"))
        .count();
    let ends = lines
        .iter()
        .filter(|line| line.name == "END" && line.value.trim().eq_ignore_ascii_case("VCARD"))
        .count();
    if begins != 1 || ends != 1 {
        return Err(Reject::UnsupportedMediaType(
            "This resource only supports a single vCard object".to_string(),
        ));
    }

    // VERSION: exactly one, 3.0 or 4.0.
    let versions: Vec<&ContentLine> = lines.iter().filter(|line| line.name == "VERSION").collect();
    if versions.len() != 1 {
        return Err(Reject::UnsupportedMediaType(format!(
            "Validation error in vCard: VERSION must appear exactly once (found {})",
            versions.len()
        )));
    }
    let version = versions[0].value.trim();
    if version != "3.0" && version != "4.0" {
        return Err(Reject::UnsupportedMediaType(format!(
            "Validation error in vCard: unsupported VERSION {version:?}"
        )));
    }

    // UID: mandatory. `CardDavBackend::getUID()` raises BadRequest (400).
    let uid = lines
        .iter()
        .find(|line| line.name == "UID")
        .map(|line| line.value.trim().to_string())
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| {
            Reject::BadRequest("vCards on CardDAV servers MUST have a UID property".to_string())
        })?;

    // FN: exactly once.
    let fn_count = lines.iter().filter(|line| line.name == "FN").count();
    if fn_count != 1 {
        return Err(Reject::UnsupportedMediaType(format!(
            "Validation error in vCard: FN must appear exactly once (found {fn_count})"
        )));
    }

    // Library parse gate. calcard is deliberately lenient, so this only
    // rejects genuinely unreadable input; the rule layer above is the real
    // enforcement.
    if let Err(error) = calcard::vcard::VCard::parse(&text) {
        return Err(Reject::UnsupportedMediaType(format!(
            "This resource only supports valid vCard or jCard data. Parse error: {error:?}"
        )));
    }

    Ok(ValidatedCard { data, uid })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN: &[u8] = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:1234\r\nFN:Jane Doe\r\nEND:VCARD\r\n";

    #[test]
    fn accepts_a_clean_card_and_stores_bytes_verbatim() {
        let card = validate(CLEAN).unwrap();
        assert_eq!(card.data, CLEAN);
        assert_eq!(card.uid, "1234");
    }

    #[test]
    fn accepts_vcard_4() {
        let card =
            validate(b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:x\r\nFN:X\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(card.uid, "x");
    }

    #[test]
    fn rejects_jcard() {
        assert!(matches!(
            validate(b"[\"vcard\",[]]"),
            Err(Reject::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn rejects_v21() {
        let card = b"BEGIN:VCARD\r\nVERSION:2.1\r\nUID:x\r\nFN:X\r\nEND:VCARD\r\n";
        assert!(matches!(
            validate(card),
            Err(Reject::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn missing_uid_is_bad_request() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nEND:VCARD\r\n";
        assert!(matches!(validate(card), Err(Reject::BadRequest(_))));
    }

    #[test]
    fn duplicate_fn_is_rejected() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:A\r\nFN:B\r\nEND:VCARD\r\n";
        assert!(matches!(
            validate(card),
            Err(Reject::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn lower_case_names_are_uppercased_before_the_charset_check() {
        let card = b"BEGIN:VCARD\r\nversion:3.0\r\nuid:x\r\nfn:X\r\nEND:VCARD\r\n";
        assert!(validate(card).is_ok());
    }

    #[test]
    fn invalid_property_name_is_rejected() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:X\r\nBAD_NAME:a\r\nEND:VCARD\r\n";
        assert!(matches!(
            validate(card),
            Err(Reject::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn control_character_is_rejected() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:A\x07B\r\nEND:VCARD\r\n";
        assert!(matches!(
            validate(card),
            Err(Reject::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn many_vcards_are_rejected() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:X\r\nEND:VCARD\r\n\
                     BEGIN:VCARD\r\nVERSION:3.0\r\nUID:y\r\nFN:Y\r\nEND:VCARD\r\n";
        assert!(matches!(
            validate(card),
            Err(Reject::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn latin1_is_transcoded() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:Caf\xe9\r\nEND:VCARD\r\n";
        let validated = validate(card).unwrap();
        assert!(String::from_utf8(validated.data).unwrap().contains("Café"));
    }

    #[test]
    fn oversized_logical_line_is_rejected() {
        let mut card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:".to_vec();
        card.extend(std::iter::repeat_n(b'a', MAX_LOGICAL_LINE_LENGTH + 1));
        card.extend_from_slice(b"\r\nEND:VCARD\r\n");
        assert!(matches!(
            validate(&card),
            Err(Reject::UnsupportedMediaType(_))
        ));
    }
}
