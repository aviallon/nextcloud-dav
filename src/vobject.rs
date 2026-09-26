// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A faithful port of the slices of **Sabre VObject 4.5.6** (the copy vendored
//! by Nextcloud 33.0.5) that the `address-data` negotiation needs: the MimeDir
//! reader/writer, `VCardConverter`, jCard output and Sabre's media-type
//! negotiation. The behavioural contract, with `file:line` citations into the
//! reference sources, is `research/address-data-negotiation-spec.md`.
//!
//! Where Sabre merely violates a standard the port follows the standard
//! instead (declared in `tests/deviations.toml`,
//! `vcard-version-negotiation-missing`):
//!
//! - the `<card:prop>` filter is applied in **both** reports (Sabre passes the
//!   filter to `convertVCard()` from `addressbook-query` only);
//! - filter names match **case-insensitively** (Sabre's `array_diff()` is
//!   case-sensitive against the upper-cased parsed names);
//! - a malformed `content-type` attribute degrades to the vCard 3 target
//!   (Sabre's `parseMimeType()` `var_dump()`s and `exit`s the process).

use std::fmt;

/// `VObject\Version::VERSION` (`vobject-lib/Version.php:17`); echoed into the
/// `PRODID` of converted cards (`VCardConverter.php:45-49`,
/// `Component/VCard.php:438-445`).
pub const VOBJECT_VERSION: &str = "4.5.6";

/// A parse failure — the REPORT path turns this into the same HTTP 500 with
/// `s:exception` = `Sabre\VObject\ParseException` that PHP produces
/// (`DAV/Server.php:254-309`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocVersion {
    V21,
    V30,
    V40,
    Unknown,
}

impl DocVersion {
    fn from_version_prop(value: &str) -> DocVersion {
        match value {
            "2.1" => DocVersion::V21,
            "3.0" => DocVersion::V30,
            "4.0" => DocVersion::V40,
            _ => DocVersion::Unknown,
        }
    }
}

/// Value storage, mirroring the vobject property classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `Property\Text` and subclasses (`FlatText`, `Unknown`, `TimeStamp`,
    /// `PhoneNumber`, `LanguageTag`): components decoded by
    /// `MimeDir::unescapeValue` (`Parser/MimeDir.php:548-589`).
    Text(Vec<String>),
    /// `Property\Uri`: one decoded string (`Property/Uri.php:71-114`).
    Uri(String),
    /// `Property\Binary`: raw bytes (`Property/Binary.php:55-71`).
    Binary(Vec<u8>),
    /// `Property\VCard\DateAndOrTime`: the raw mime-dir value, never escaped
    /// (spec B4).
    Raw(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// Upper-case name. Empty when a 2.1 nameless parameter could not be
    /// guessed (`Parameter::guessParameterNameByValue`, `Parameter.php:120-164`).
    pub name: String,
    pub values: Vec<String>,
    /// A vCard 2.1 nameless parameter (`Property::add(null, …)`,
    /// `Property.php:~155-172`).
    pub no_name: bool,
}

impl Param {
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prop {
    pub group: Option<String>,
    /// Upper-case, group stripped (`Document::createProperty`,
    /// `Document.php:200-205`).
    pub name: String,
    /// First-insertion order of distinct names; same-name parameters merged
    /// (`Property::add`, duplicate values collapsed — `Parser/MimeDir.php:385-398`).
    pub params: Vec<Param>,
    pub value: Value,
    /// The vobject class the value was parsed/created as.
    pub class: Class,
}

impl Prop {
    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }

    /// `Property::getValue()` — joined component text for `Text`, the raw
    /// string otherwise.
    pub fn string_value(&self) -> String {
        match &self.value {
            Value::Text(parts) => parts.join(","),
            Value::Uri(s) => s.clone(),
            Value::Raw(s) => s.clone(),
            Value::Binary(_) => String::new(),
        }
    }

    fn text_parts(&self) -> Vec<String> {
        match &self.value {
            Value::Text(parts) => parts.clone(),
            Value::Uri(s) | Value::Raw(s) => vec![s.clone()],
            Value::Binary(b) => vec![String::from_utf8_lossy(b).into_owned()],
        }
    }
    #[allow(dead_code)]
    fn unused_marker(&self) {}
}

/// One parsed vCard document (the `VCard` component's children).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    pub props: Vec<Prop>,
}

impl Doc {
    /// `Component\VCard::getDocumentType()` (`Component/VCard.php:146-166`):
    /// the `VERSION` property's exact string, nothing else.
    pub fn version(&self) -> DocVersion {
        self.props
            .iter()
            .find(|p| p.name == "VERSION")
            .map(|p| DocVersion::from_version_prop(&p.string_value()))
            .unwrap_or(DocVersion::Unknown)
    }

    /// `Component::children()` (`Component.php:119-124, 174-182`): per-name
    /// buckets, buckets in first-appearance order, insertion order within.
    fn children(&self) -> Vec<&Prop> {
        let mut order: Vec<String> = Vec::new();
        let mut buckets: Vec<(String, Vec<&Prop>)> = Vec::new();
        for prop in &self.props {
            match buckets.iter_mut().find(|(name, _)| name == &prop.name) {
                Some((_, bucket)) => bucket.push(prop),
                None => {
                    order.push(prop.name.clone());
                    buckets.push((prop.name.clone(), vec![prop]));
                }
            }
        }
        let _ = order;
        buckets.into_iter().flat_map(|(_, b)| b).collect()
    }

    /// `Component::select($prefix)` — properties whose `GROUP.NAME` starts
    /// with the prefix (`Component.php:219-…`).
    #[allow(dead_code)]
    fn select_prefix(&self, prefix: &str) -> Vec<&Prop> {
        self.props
            .iter()
            .filter(|p| {
                let full = match &p.group {
                    Some(g) => format!("{}.{}", g, p.name),
                    None => p.name.clone(),
                };
                full.starts_with(prefix)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Reader — `Parser/MimeDir.php`
// ---------------------------------------------------------------------------

/// `MimeDir::unescapeValue` (`Parser/MimeDir.php:548-589`): split into
/// components on the unescaped delimiter and decode `\\`, `\N`, `\n`, `\;`,
/// `\,`. With an empty delimiter the value is a plain string.
fn unescape_value(input: &str, delimiter: char) -> Vec<String> {
    if delimiter == '\0' {
        return vec![unescape_flat(input)];
    }
    let bytes: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                '\\' => cur.push('\\'),
                'N' | 'n' => cur.push('\n'),
                ';' => cur.push(';'),
                ',' => cur.push(','),
                other => {
                    // Only the five sequences are escapes; anything else keeps
                    // both characters (the regex only splits on the five).
                    cur.push('\\');
                    cur.push(other);
                }
            }
            i += 2;
        } else if c == delimiter {
            out.push(std::mem::take(&mut cur));
            i += 1;
        } else {
            cur.push(c);
            i += 1;
        }
    }
    out.push(cur);
    out
}

/// Unescape for delimiter-less (`''`) text values: vobject's split with no
/// delimiter alternative still consumes the escape tokens and decodes them.
fn unescape_flat(input: &str) -> String {
    let bytes: Vec<char> = input.chars().collect();
    let mut cur = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                '\\' => cur.push('\\'),
                'N' | 'n' => cur.push('\n'),
                ';' => cur.push(';'),
                ',' => cur.push(','),
                other => {
                    cur.push('\\');
                    cur.push(other);
                }
            }
            i += 2;
        } else {
            cur.push(bytes[i]);
            i += 1;
        }
    }
    cur
}

/// `MimeDir::unescapeParam` (RFC 6868, `Parser/MimeDir.php:623-652`).
fn unescape_param(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '^' {
            match chars.next() {
                Some('^') => out.push('^'),
                Some('n') => out.push('\n'),
                Some('\'') => out.push('"'),
                Some(other) => {
                    out.push('^');
                    out.push(other);
                }
                None => out.push('^'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The property class decision of `Document::createProperty`
/// (`Document.php:212-236`) plus `Component\VCard::getClassNameForPropertyName`
/// (`Component/VCard.php:528-541`: Binary becomes Uri in vCard 4 documents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Text { structured: bool },
    Flat,
    Uri,
    Binary,
    DateAndOrTime,
    Unknown,
    /// `Property\VCard\PhoneNumber` (only reachable via `VALUE=phone-number`):
    /// FlatText behaviour with its own value type.
    PhoneNumber,
}

const STRUCTURED: &[&str] = &["N", "ADR", "ORG", "GENDER", "CLIENTPIDMAP", "REQUEST-STATUS"];
const MINIMUM_VALUES: &[(&str, usize)] = &[("N", 5), ("ADR", 7)];

fn value_type_class(value_type: &str) -> Option<Class> {
    // `Component\VCard::$valueMap` (`Component/VCard.php:25-68`).
    match value_type.to_ascii_uppercase().as_str() {
        "TEXT" => Some(Class::Text { structured: false }),
        "URI" | "URL" | "CAL-ADDRESS" => Some(Class::Uri),
        "BINARY" | "OCTET" => Some(Class::Binary),
        "PHONE-NUMBER" => Some(Class::PhoneNumber),
        "DATE-AND-OR-TIME" | "DATE-TIME" | "DATE" | "TIME" | "TIMESTAMP" => {
            Some(Class::DateAndOrTime)
        }
        _ => None,
    }
}

fn name_class(name: &str) -> Class {
    // `Component\VCard::$propertyMap` (`Component/VCard.php:76-136`).
    match name {
        "N" | "ADR" | "ORG" | "GENDER" | "CLIENTPIDMAP" | "TZ" | "CATEGORIES" | "NICKNAME" => {
            Class::Text {
                structured: STRUCTURED.contains(&name),
            }
        }
        "PHOTO" | "LOGO" => Class::Binary,
        "SOUND" | "KEY" => Class::Binary,
        "BDAY" | "ANNIVERSARY" | "DEATHDATE" => Class::DateAndOrTime,
        "URL" | "SOURCE" | "FBURL" | "CAPURI" | "CALURI" | "CALADRURI" | "IMPP" | "MEMBER"
        | "RELATED" => Class::Uri,
        "REV" | "TIMESTAMP" => Class::DateAndOrTime,
        // FN, TEL, EMAIL, LABEL, MAILER, GEO, TITLE, ROLE, NOTE, UID, VERSION,
        // SORT-STRING, PRODID, CLASS, KIND, XML, BIRTHPLACE, DEATHPLACE,
        // EXPERTISE, HOBBY, INTEREST, ORG-DIRECTORY, NAME …
        name if is_known_flat(name) => Class::Flat,
        _ => Class::Unknown,
    }
}

fn is_known_flat(name: &str) -> bool {
    matches!(
        name,
        "FN"
            | "NAME"
            | "TEL"
            | "EMAIL"
            | "LABEL"
            | "MAILER"
            | "GEO"
            | "TITLE"
            | "ROLE"
            | "NOTE"
            | "UID"
            | "VERSION"
            | "KEY"
            | "SORT-STRING"
            | "PRODID"
            | "CLASS"
            | "KIND"
            | "XML"
            | "BIRTHPLACE"
            | "DEATHPLACE"
            | "EXPERTISE"
            | "HOBBY"
            | "INTEREST"
            | "ORG-DIRECTORY"
            | "AGENT"
            | "LANG"
    )
}

fn class_of(name: &str, value_param: Option<&str>, doc_version: DocVersion) -> Result<Class, ParseError> {
    // `Document::createProperty`: the explicit value type wins, then the VALUE
    // parameter, then the name map. An unknown VALUE parameter is an error
    // (`Document.php:228-231`).
    if let Some(vt) = value_param {
        match value_type_class(vt) {
            Some(class) => return Ok(class),
            None => {
                return Err(ParseError(format!(
                    "Unsupported VALUE parameter for {name} property. You supplied \"{vt}\""
                )))
            }
        }
    }
    let class = name_class(name);
    // In vCard 4 documents binary-typed properties parse as Uri
    // (`Component/VCard.php:528-541`).
    if class == Class::Binary && doc_version == DocVersion::V40 {
        return Ok(Class::Uri);
    }
    Ok(class)
}

fn delimiter_for(name: &str, class: Class) -> char {
    // `Property\Text::__construct` (`Property/Text.php:81-90`) keys the
    // delimiter off the *name*, whatever class created the property.
    if STRUCTURED.contains(&name) {
        return ';';
    }
    match class {
        Class::Text { .. } | Class::Flat | Class::Unknown | Class::PhoneNumber => ',',
        // `Property::delimiter = ''` for `Uri`, `Binary`, `DateAndOrTime`.
        _ => '\0',
    }
}

/// `quoted_printable_decode()` semantics: `=XX` hex bytes and `=\r\n`/`=\n`
/// soft line breaks.
fn quoted_printable_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'=' => {
                let rest = &bytes[i + 1..];
                if rest.first() == Some(&b'\r') && rest.get(1) == Some(&b'\n') {
                    i += 3;
                } else if rest.first() == Some(&b'\n') {
                    i += 2;
                } else if rest.len() >= 2 {
                    let hex = (char::from(rest[0]).to_digit(16), char::from(rest[1]).to_digit(16));
                    if let (Some(hi), Some(lo)) = hex {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    } else {
                        out.push(b'=');
                        i += 1;
                    }
                } else {
                    out.push(b'=');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses one unfolded logical line into a property.
fn read_property(
    line: &str,
    raw_folds: &str,
    doc_version: DocVersion,
) -> Result<Prop, ParseError> {
    // Tokenising per `Parser/MimeDir.php:330-…` (`readProperty`): the name up
    // to the first `;`/`:`, parameters up to the first `:` outside quotes,
    // value = the rest.
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut name = String::new();
    while i < chars.len() && chars[i] != ';' && chars[i] != ':' {
        name.push(chars[i]);
        i += 1;
    }
    if i == chars.len() || name.is_empty() {
        return Err(ParseError(
            "Invalid Mimedir file. Line did not follow iCalendar/vCard conventions".into(),
        ));
    }
    // The name token must match `[A-Za-z0-9\-.]+` (strict mode).
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(ParseError(
            "Invalid Mimedir file. Line did not follow iCalendar/vCard conventions".into(),
        ));
    }

    // Parameters.
    let mut params: Vec<Param> = Vec::new();
    while chars[i] == ';' {
        i += 1;
        let mut pname = String::new();
        while i < chars.len() && chars[i] != '=' && chars[i] != ';' && chars[i] != ':' {
            pname.push(chars[i]);
            i += 1;
        }
        if pname.is_empty()
            || !pname
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(ParseError(
                "Invalid Mimedir file. Line did not follow iCalendar/vCard conventions".into(),
            ));
        }
        let mut values: Vec<String> = Vec::new();
        let mut valueless = true;
        if chars.get(i) == Some(&'=') {
            valueless = false;
            loop {
                i += 1; // consume '=' or ','
                let mut raw = String::new();
                let quoted = chars.get(i) == Some(&'"');
                if quoted {
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        raw.push(chars[i]);
                        i += 1;
                    }
                    if chars.get(i) != Some(&'"') {
                        return Err(ParseError(
                            "Invalid Mimedir file. Line did not follow iCalendar/vCard conventions"
                                .into(),
                        ));
                    }
                    i += 1;
                } else {
                    while i < chars.len() && !matches!(chars[i], ';' | ':' | ',') {
                        raw.push(chars[i]);
                        i += 1;
                    }
                }
                let value = unescape_param(&raw);
                // `Parser/MimeDir.php:385-398`: merge, collapse duplicates.
                if !values.contains(&value) {
                    values.push(value);
                }
                if chars.get(i) != Some(&',') {
                    break;
                }
            }
        }
        let (final_name, values, no_name) = if valueless {
            // 2.1 nameless parameter: `Property::add(null, …)` names it from
            // its value (`Parameter::guessParameterNameByValue`), keeping the
            // token as the value.
            (guess_parameter_name(&pname), vec![pname.clone()], true)
        } else {
            (pname.clone(), values, false)
        };
        match params.iter_mut().find(|p| p.name == final_name) {
            Some(existing) => {
                for v in values {
                    if !existing.values.contains(&v) {
                        existing.values.push(v);
                    }
                }
            }
            None => {
                params.push(Param {
                    name: final_name,
                    values,
                    no_name,
                });
            }
        }
    }
    if chars.get(i) != Some(&':') {
        return Err(ParseError(
            "Invalid Mimedir file. Line did not follow iCalendar/vCard conventions".into(),
        ));
    }
    i += 1;
    let value_raw: String = chars[i..].iter().collect();

        // Group split at the first `.` (`Document::createProperty`,
        // `Document.php:200-205`).
        let (group, name) = match name.find('.') {
            Some(pos) => (
                Some(name[..pos].to_ascii_uppercase()),
                name[pos + 1..].to_ascii_uppercase(),
            ),
            None => (None, name.to_ascii_uppercase()),
        };
        let _ = &name;

    let value_param = params.iter().find(|p| p.name == "VALUE").map(|p| p.values.join(","));
    let class = class_of(&name, value_param.as_deref(), doc_version)?;

    // `Parser/MimeDir.php:455-480`: QUOTED-PRINTABLE first, then charset, then
    // the class's `setRawMimeDirValue`.
    let encoding_qp = params.iter().any(|p| {
        p.name == "ENCODING" && p.values.iter().any(|v| v.eq_ignore_ascii_case("QUOTED-PRINTABLE"))
    });
    let value = if encoding_qp {
        // `extractQuotedPrintableValue` (`Parser/MimeDir.php:…`): the value of
        // the *raw* line (folds joined with `"\n "`), one whitespace char
        // removed after each newline, then QP-decoded and split on unescaped
        // `;` (`Property/Text.php:105-118`). Deliberately more standard-
        // conformant than PHP here: the `\;` escapes are decoded rather than
        // kept and double-escaped on write.
        let mut raw = raw_folds.to_string();
        raw = raw.replace("\n ", "\n");
        let decoded = quoted_printable_decode(&raw);
        Value::Text(
            decoded
                .split(';')
                .map(|s| s.replace("\\;", ";"))
                .collect::<Vec<_>>(),
        )
    } else {
        match class {
            Class::Text { .. } | Class::Flat | Class::Unknown | Class::PhoneNumber => {
                Value::Text(unescape_value(&value_raw, delimiter_for(&name, class)))
            }
            Class::Uri => {
                // `Property/Uri.php:71-114`.
                let decoded = if name == "URL" {
                    value_raw.replace("\\:", ":")
                } else {
                    value_raw.replace("\\,", ",")
                };
                Value::Uri(decoded)
            }
            Class::Binary => {
                Value::Binary(base64_decode_lenient(&value_raw))
            }
            Class::DateAndOrTime => Value::Raw(value_raw),
        }
    };

    Ok(Prop {
        group,
        name,
        params,
        value,
        class,
    })
}

/// `Parameter::guessParameterNameByValue` (`Parameter.php:120-164`).
fn guess_parameter_name(value: &str) -> String {
    let v = value.to_ascii_uppercase();
    let name = match v.as_str() {
        "7-BIT" | "QUOTED-PRINTABLE" | "BASE64" => "ENCODING",
        "WORK" | "HOME" | "PREF" | "DOM" | "INTL" | "POSTAL" | "PARCEL" | "VOICE" | "FAX"
        | "MSG" | "CELL" | "PAGER" | "BBS" | "MODEM" | "CAR" | "ISDN" | "VIDEO" | "AOL"
        | "APPLELINK" | "ATTMAIL" | "CIS" | "EWORLD" | "INTERNET" | "IBMMAIL" | "MCIMAIL"
        | "POWERSHARE" | "PRODIGY" | "TLX" | "X400" | "GIF" | "CGM" | "WMF" | "BMP" | "TIFF"
        | "PDF" | "PS" | "JPEG" | "PNG" | "MPEG" | "MPEG2" | "AVI" | "QTIME" | "WAVE" | "PCM"
        | "AIFF" => "TYPE",
        "INLINE" | "URL" | "CONTENT-ID" | "CID" => "VALUE",
        _ => "",
    };
    name.to_string()
}

fn base64_decode_lenient(input: &str) -> Vec<u8> {
    use base64::Engine;
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .unwrap_or_default()
}

/// `VObject\Reader::read()` + `Parser\MimeDir` for a vCard document.
pub fn parse(data: &[u8]) -> Result<Doc, ParseError> {
    // UTF-8 BOM stripped (`Parser/MimeDir.php:151-156`).
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(data);
    let text = String::from_utf8_lossy(data);

    // Physical lines (`readLine`, `Parser/MimeDir.php:282-323`): `rtrim` of
    // CR/LF, empty lines skipped, continuations joined after dropping exactly
    // one leading whitespace char. The QP path needs the `" \n "`-joined raw
    // form, so both are kept.
    let mut logical: Vec<(String, String)> = Vec::new();
    let mut iter = text.split('\n').map(|l| l.trim_end_matches('\r'));
    let mut pending: Option<(String, String)> = None;
    while let Some((mut joined, mut raw)) = pending
        .take()
        .or_else(|| iter.next().map(|line| (line.to_string(), line.to_string())))
    {
        if joined.is_empty() {
            continue;
        }
        // Folded continuations.
        loop {
            match iter.next() {
                Some(next) if next.is_empty() => break,
                Some(next) if next.starts_with(' ') || next.starts_with('\t') => {
                    joined.push_str(&next[1..]);
                    raw.push_str("\n ");
                    raw.push_str(&next[1..]);
                }
                Some(next) => {
                    pending = Some((next.to_string(), next.to_string()));
                    break;
                }
                None => break,
            }
        }
        logical.push((joined, raw));
    }

    let mut doc = Doc::default();
    let mut inside = false;
    for (line, raw) in logical {
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("BEGIN:") {
            let name = upper[6..].trim().to_string();
            if name != "VCARD" {
                return Err(ParseError(format!("Unexpected component {name}")));
            }
            if inside {
                return Err(ParseError("Nested BEGIN".into()));
            }
            inside = true;
            continue;
        }
        if upper.starts_with("END:") {
            inside = false;
            continue;
        }
        if !inside {
            return Err(ParseError(
                "We found an invalid VCard object. Make sure the object contains BEGIN:VCARD and END:VCARD".into(),
            ));
        }
        // `Component\VCard::getDocumentType()` is evaluated against the
        // already-seen properties, so a VERSION line changes class selection
        // for the lines after it.
        let doc_version = doc.version();
        doc.props.push(read_property(&line, &raw, doc_version)?);
    }
    if doc.props.is_empty() {
        return Err(ParseError(
            "We found an invalid VCard object. Make sure the object contains BEGIN:VCARD and END:VCARD".into(),
        ));
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// Writer — `Property::serialize()` / `Component::serialize()`
// ---------------------------------------------------------------------------

/// `Text::getRawMimeDirValue` (`Property/Text.php:125-155`): pad, escape,
/// join.
fn escape_text(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

fn property_value(prop: &Prop) -> String {
    match &prop.value {
        Value::Text(parts) => {
            let mut parts = parts.clone();
            if let Some((_, min)) = MINIMUM_VALUES.iter().find(|(n, _)| *n == prop.name) {
                while parts.len() < *min {
                    parts.push(String::new());
                }
            }
            let delimiter = delimiter_for(&prop.name, prop.class);
            let joined = if delimiter == '\0' {
                parts.into_iter().map(|p| escape_text(&p)).collect::<String>()
            } else {
                parts.into_iter()
                    .map(|p| escape_text(&p))
                    .collect::<Vec<_>>()
                    .join(&delimiter.to_string())
            };
            joined
        }
        Value::Uri(value) => value.replace(',', "\\,"),
        Value::Binary(bytes) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
        Value::Raw(value) => value.clone(),
    }
}

/// `Parameter::serialize` (`Parameter.php:264-311`).
fn serialize_param(param: &Param, doc_version: DocVersion) -> String {
    if param.values.is_empty() {
        return format!("{}=", param.name);
    }
    if doc_version == DocVersion::V21 && param.no_name {
        return param.values.join(";");
    }
    let values: Vec<String> = param
        .values
        .iter()
        .map(|value| {
            let special = value
                .chars()
                .any(|c| matches!(c, '\n' | '"' | ':' | ';' | '^' | ',' | '+'));
            if special {
                let escaped = value
                    .replace('^', "^^")
                    .replace('\n', "^n")
                    .replace('"', "^'");
                format!("\"{escaped}\"")
            } else {
                value.clone()
            }
        })
        .collect();
    format!("{}={}", param.name, values.join(","))
}

/// `Property::serialize` (`Property.php:242-266`): the line is folded at 75
/// **bytes** (first line 75, continuations `" "` + 74), never splitting a
/// UTF-8 character; each property ends with `\r\n`.
fn fold(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 16);
    let mut start = 0;
    let mut first = true;
    loop {
        let remaining = bytes.len() - start;
        if remaining == 0 {
            break;
        }
        // `(?:^.)? .{1,74}`: up to 75 bytes on the first line, 74 after.
        let mut width = if first { 75 } else { 74 };
        if width > remaining {
            width = remaining;
        }
        // `(?![\x80-\xbf])`: never end before a UTF-8 continuation byte.
        while width > 1 && start + width < bytes.len() && (bytes[start + width] & 0xC0) == 0x80 {
            width -= 1;
        }
        if !first {
            out.extend_from_slice(b"\r\n ");
        }
        out.extend_from_slice(&bytes[start..start + width]);
        start += width;
        first = false;
    }
    out.extend_from_slice(b"\r\n");
    String::from_utf8_lossy(&out).into_owned()
}

fn serialize_property(prop: &Prop, doc_version: DocVersion) -> String {
    let mut params = prop.params.clone();
    // `Property\Uri::parameters` (`Property/Uri.php:45-61`): URL/PHOTO gain a
    // synthetic trailing `VALUE=URI`.
    if matches!(prop.value, Value::Uri(_))
        && matches!(prop.name.as_str(), "URL" | "PHOTO")
        && !prop.params.iter().any(|p| p.name == "VALUE")
    {
        params.push(Param {
            name: "VALUE".into(),
            values: vec!["URI".into()],
            no_name: false,
        });
    }
    let mut line = match &prop.group {
        Some(group) => format!("{}.{}", group, prop.name),
        None => prop.name.clone(),
    };
    for param in &params {
        line.push(';');
        line.push_str(&serialize_param(param, doc_version));
    }
    line.push(':');
    line.push_str(&property_value(prop));
    fold(&line)
}

/// `Component::serialize` (`Component.php:266-334`): `VERSION` first
/// (`$sortScore` 100000000), everything else in `children()` order.
pub fn serialize(doc: &Doc) -> String {
    let doc_version = doc.version();
    let children = doc.children();
    let mut sorted: Vec<&Prop> = children
        .iter()
        .copied()
        .filter(|p| p.name != "VERSION")
        .collect();
    if let Some(version) = children.iter().find(|p| p.name == "VERSION") {
        sorted.insert(0, version);
    }
    let mut out = String::from("BEGIN:VCARD\r\n");
    for prop in sorted {
        out.push_str(&serialize_property(prop, doc_version));
    }
    out.push_str("END:VCARD\r\n");
    out
}

// ---------------------------------------------------------------------------
// jCard — `json_encode($vcard4)` (RFC 7095 shape)
// ---------------------------------------------------------------------------

fn json_string(value: &str) -> String {
    // `json_encode` default flags: compact, `/` escaped, non-ASCII as `\uXXXX`
    // (serde_json matches on the short escapes and `\u` form; `/`, U+2028 and
    // U+2029 are PHP-specific escapes applied here).
    let mut encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into());
    encoded = encoded.replace('/', "\\/");
    encoded = encoded.replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029");
    encoded
}

/// `VCard::jsonSerialize` (`Component/VCard.php:454-467`) +
/// `Property::jsonSerialize` (`Property.php:297-321`).
pub fn json_serialize(doc: &Doc) -> String {
    let mut props = Vec::new();
    for prop in doc.children() {
        let mut entry = String::from("[");
        entry.push_str(&json_string(&prop.name.to_lowercase()));
        entry.push(',');
        // Parameter object: keys lower-cased, VALUE omitted, `group` added
        // (`Property.php:302-311`).
        let mut fields: Vec<String> = Vec::new();
        for param in &prop.params {
            if param.name == "VALUE" {
                continue;
            }
            fields.push(format!(
                "{}:{}",
                json_string(&param.name.to_lowercase()),
                json_param_value(param)
            ));
        }
        if let Some(group) = &prop.group {
            fields.push(format!("{}:{}", json_string("group"), json_string(group)));
        }
        entry.push('{');
        entry.push_str(&fields.join(","));
        entry.push('}');
        entry.push(',');
        entry.push_str(&json_string(&value_type(prop).to_lowercase()));
        for item in json_values(prop) {
            entry.push(',');
            entry.push_str(&item);
        }
        entry.push(']');
        props.push(entry);
    }
    format!("[\"vcard\",[{}]]", props.join(","))
}

fn json_param_value(param: &Param) -> String {
    // `Parameter::jsonSerialize` (`Parameter.php:322-329`): null / string /
    // array of strings.
    match param.values.len() {
        0 => "null".to_string(),
        1 => json_string(&param.values[0]),
        _ => {
            let items: Vec<String> = param.values.iter().map(|v| json_string(v)).collect();
            format!("[{}]", items.join(","))
        }
    }
}

fn value_type(prop: &Prop) -> &'static str {
    // `Property::getValueType` is class-based (`Text`/`FlatText` → "text",
    // `Uri` → "uri", `Binary` → "binary", `DateAndOrTime` →
    // "date-and-or-time", `Unknown` → "unknown"); an explicit VALUE parameter
    // only selects the class at creation time.
    class_value_type(prop.class)
}

fn class_value_type(class: Class) -> &'static str {
    match class {
        Class::Text { .. } | Class::Flat => "text",
        Class::Uri => "uri",
        Class::Binary => "binary",
        Class::DateAndOrTime => "date-and-or-time",
        Class::Unknown => "unknown",
        Class::PhoneNumber => "phone-number",
    }
}

fn json_values(prop: &Prop) -> Vec<String> {
    match &prop.value {
        Value::Text(parts) => {
            if STRUCTURED.contains(&prop.name.as_str()) {
                // Structured text: one item containing the parts array
                // (`Property/Text.php:165-175`).
                vec![format!(
                    "[{}]",
                    parts.iter().map(|p| json_string(p)).collect::<Vec<_>>().join(",")
                )]
            } else if prop.class == Class::Unknown {
                // `Property\Unknown::getJsonValue`: the re-escaped mime-dir
                // value as one item.
                vec![json_string(&escape_text(&parts.join(",")))]
            } else {
                parts.iter().map(|p| json_string(p)).collect()
            }
        }
        Value::Uri(value) | Value::Raw(value) => vec![json_string(value)],
        Value::Binary(bytes) => {
            use base64::Engine;
            vec![json_string(
                &base64::engine::general_purpose::STANDARD.encode(bytes),
            )]
        }
    }
}

// ---------------------------------------------------------------------------
// `VCardConverter` (`VCardConverter.php:31-420`)
// ---------------------------------------------------------------------------

/// Recreates a property like `Component::createProperty` (value type first,
/// then the VALUE parameter, then the name map).
fn create_prop(name: &str, value: Value, value_type: Option<&str>) -> Prop {
    let (group, base) = match name.find('.') {
        Some(pos) => (
            Some(name[..pos].to_ascii_uppercase()),
            name[pos + 1..].to_ascii_uppercase(),
        ),
        None => (None, name.to_ascii_uppercase()),
    };
    let class = match value_type.and_then(value_type_class) {
        Some(class) => class,
        None => name_class(&base),
    };
    Prop {
        group,
        name: base,
        params: Vec::new(),
        value,
        class,
    }
}

fn prop_get_parts(prop: &Prop) -> Value {
    prop.value.clone()
}

/// Converts a parsed card to the target version. `Jcard` converts to 4.0
/// first, exactly like `convertVCard` (`Plugin.php:840-843`).
pub fn convert(doc: &Doc, target: Target) -> Result<Doc, ParseError> {
    let input_version = doc.version();
    let target_version = match target {
        Target::Vcard3 => DocVersion::V30,
        Target::Vcard4 | Target::Jcard => DocVersion::V40,
    };
    if input_version == target_version {
        return Ok(doc.clone());
    }
    if !matches!(input_version, DocVersion::V21 | DocVersion::V30 | DocVersion::V40) {
        return Err(ParseError(
            "Only vCard 2.1, 3.0 and 4.0 are supported for the input data".into(),
        ));
    }

    // `new Component\VCard(['VERSION' => …])` + `getDefaults()` + the
    // generated UID removed (`VCardConverter.php:45-52`,
    // `Component/VCard.php:438-445`).
    let mut output = Doc::default();
    output.props.push(Prop {
        group: None,
        name: "VERSION".into(),
        params: Vec::new(),
        value: Value::Text(vec![
            if target_version == DocVersion::V40 {
                "4.0".into()
            } else {
                "3.0".into()
            },
        ]),
        class: Class::Flat,
    });
    output.props.push(Prop {
        group: None,
        name: "PRODID".into(),
        params: Vec::new(),
        value: Value::Text(vec![format!(
            "-//Sabre//Sabre VObject {VOBJECT_VERSION}//EN"
        )]),
        class: Class::Flat,
    });

    for prop in doc.children() {
        convert_property(doc, &mut output, prop, target_version)?;
    }
    Ok(output)
}

fn convert_property(
    input: &Doc,
    output: &mut Doc,
    prop: &Prop,
    target: DocVersion,
) -> Result<(), ParseError> {
    // VERSION/PRODID are automatic (`VCardConverter.php:68-71`).
    if prop.name == "VERSION" || prop.name == "PRODID" {
        return Ok(());
    }

    // Leftover parameters; the VALUE parameter becomes the value type
    // (`VCardConverter.php:73-81`).
    let mut parameters: Vec<Param> = prop.params.clone();
    let value_param = parameters
        .iter()
        .find(|p| p.name == "VALUE")
        .and_then(|p| p.values.first().cloned());
    parameters.retain(|p| p.name != "VALUE");
    let mut value_type = value_param.unwrap_or_else(|| class_value_type(prop.class).to_string());
    if target != DocVersion::V30 && value_type == "PHONE-NUMBER" {
        value_type = class_value_type(prop.class).to_string();
    }

    let mut new_prop = create_prop(&prop.name, prop_get_parts(prop), Some(&value_type));

    if target == DocVersion::V30 {
        if prop.class == Class::Uri && matches!(prop.name.as_str(), "PHOTO" | "LOGO" | "SOUND") {
            new_prop = convert_uri_to_binary(new_prop);
        } else if prop.class == Class::DateAndOrTime && prop.name != "REV" && prop.name != "TIMESTAMP" {
            // vCard 4 allows year-less values; vCard 3 does not
            // (`VCardConverter.php:95-108`).
            let (year, month, date) = parse_vcard_date(&prop.string_value());
            if year.is_none() {
                new_prop.value = Value::Text(vec![format!("1604-{month}-{date}")]);
                new_prop.params.push(Param {
                    name: "X-APPLE-OMIT-YEAR".into(),
                    values: vec!["1604".into()],
                    no_name: false,
                });
            }
            if new_prop.name == "ANNIVERSARY" {
                // Microsoft non-standard anniversary
                // (`VCardConverter.php:110-124`).
                new_prop.name = "X-ANNIVERSARY".into();
                // First `ITEM<n>` group not already present in the output.
                let mut x = 1;
                while output.props.iter().any(|p| {
                    let full = format!("{}.{}", p.group.clone().unwrap_or_default(), p.name);
                    full.starts_with(&format!("ITEM{x}."))
                }) {
                    x += 1;
                }
                let value = new_prop.value.clone();
                output.props.push(Prop {
                    group: Some(format!("ITEM{x}")),
                    name: "X-ABDATE".into(),
                    params: vec![Param {
                        name: "VALUE".into(),
                        values: vec!["DATE-AND-OR-TIME".into()],
                        no_name: false,
                    }],
                    value,
                    class: Class::Unknown,
                });
                output.props.push(Prop {
                    group: Some(format!("ITEM{x}")),
                    name: "X-ABLABEL".into(),
                    params: Vec::new(),
                    value: Value::Text(vec!["_$!<Anniversary>!$_".into()]),
                    class: Class::Unknown,
                });
            }
        } else if prop.name == "KIND" {
            // `VCardConverter.php:126-141`.
            match prop.string_value().to_lowercase().as_str() {
                "org" => new_prop = create_prop("X-ABSHOWAS", Value::Text(vec!["COMPANY".into()]), None),
                "individual" => return Ok(()),
                "group" => {
                    new_prop = create_prop(
                        "X-ADDRESSBOOKSERVER-KIND",
                        Value::Text(vec!["GROUP".into()]),
                        None,
                    )
                }
                _ => {}
            }
        } else if prop.name == "MEMBER" {
            new_prop = create_prop(
                "X-ADDRESSBOOKSERVER-MEMBER",
                prop_get_parts(prop),
                None,
            );
        }
    } else {
        // Properties removed in vCard 4 (`VCardConverter.php:148-150`).
        if matches!(prop.name.as_str(), "NAME" | "MAILER" | "LABEL" | "CLASS") {
            return Ok(());
        }
        if prop.class == Class::Binary {
            new_prop = convert_binary_to_uri(new_prop, &mut parameters);
        } else if prop.class == Class::DateAndOrTime
            && parameters.iter().any(|p| p.name == "X-APPLE-OMIT-YEAR")
        {
            let omit = parameters
                .iter()
                .find(|p| p.name == "X-APPLE-OMIT-YEAR")
                .and_then(|p| p.values.first().cloned())
                .unwrap_or_default();
            let (year, month, date) = parse_vcard_date(&prop.string_value());
            if year.map(|y| y.to_string()) == Some(omit) {
                new_prop.value = Value::Text(vec![format!("--{month}-{date}")]);
            }
            parameters.retain(|p| p.name != "X-APPLE-OMIT-YEAR");
        }
        // Apple's property renames (`VCardConverter.php:166-218`).
        match prop.name.as_str() {
            "X-ABSHOWAS" => {
                if prop.string_value().eq_ignore_ascii_case("COMPANY") {
                    new_prop = create_prop("KIND", Value::Text(vec!["ORG".into()]), None);
                }
            }
            "X-ADDRESSBOOKSERVER-KIND" => {
                if prop.string_value().eq_ignore_ascii_case("GROUP") {
                    new_prop = create_prop("KIND", Value::Text(vec!["GROUP".into()]), None);
                }
            }
            "X-ADDRESSBOOKSERVER-MEMBER" => {
                new_prop = create_prop("MEMBER", prop_get_parts(prop), None);
            }
            "X-ANNIVERSARY" => {
                new_prop.name = "ANNIVERSARY".into();
                if output.props.iter().any(|a| {
                    a.name == "ANNIVERSARY" && a.string_value() == new_prop.string_value()
                }) {
                    return Ok(());
                }
            }
            "X-ABDATE" => {
                let group = match &prop.group {
                    Some(group) => group.clone(),
                    None => {
                        finish_property(output, new_prop, parameters, prop, target);
                        return Ok(());
                    }
                };
                let label = input
                    .props
                    .iter()
                    .find(|p| p.group.as_deref() == Some(group.as_str()) && p.name == "X-ABLABEL")
                    .map(|p| p.string_value())
                    .unwrap_or_default();
                if label != "_$!<Anniversary>!$_" {
                    finish_property(output, new_prop, parameters, prop, target);
                    return Ok(());
                }
                if output.props.iter().any(|a| {
                    a.name == "ANNIVERSARY" && a.string_value() == new_prop.string_value()
                }) {
                    return Ok(());
                }
                new_prop.name = "ANNIVERSARY".into();
            }
            "X-ABLABEL" => {
                if new_prop.string_value() == "_$!<Anniversary>!$_" {
                    return Ok(());
                }
            }
            _ => {}
        }
    }

    new_prop.group = prop.group.clone();
    finish_property(output, new_prop, parameters, prop, target);
    Ok(())
}

/// The tail of `convertProperty` (`VCardConverter.php:223-241`): parameters,
/// then the VALUE parameter re-add check.
fn finish_property(
    output: &mut Doc,
    mut new_prop: Prop,
    parameters: Vec<Param>,
    original: &Prop,
    target: DocVersion,
) {
    let _ = original;
    if target == DocVersion::V40 {
        convert_parameters_40(&mut new_prop, parameters);
    } else {
        convert_parameters_30(&mut new_prop, parameters);
    }
    // A `VALUE` parameter is kept only when the effective type differs from
    // the name's default in the target document (`VCardConverter.php:232-239`).
    let temp = create_prop(&new_prop.name, Value::Text(Vec::new()), None);
    let temp_class = match target {
        DocVersion::V40 if temp.class == Class::Binary => Class::Uri,
        _ => temp.class,
    };
    if class_value_type(temp_class) != class_value_type(new_prop.class) {
        new_prop.params.push(Param {
            name: "VALUE".into(),
            values: vec![class_value_type(new_prop.class).to_uppercase()],
            no_name: false,
        });
    }
    output.props.push(new_prop);
}

/// `VCardConverter::convertParameters40` (`VCardConverter.php:350-381`).
fn convert_parameters_40(new_prop: &mut Prop, parameters: Vec<Param>) {
    for mut param in parameters {
        param.no_name = false;
        match param.name.as_str() {
            "TYPE" => {
                for part in &param.values {
                    if part.eq_ignore_ascii_case("PREF") {
                        add_param(new_prop, "PREF", "1");
                    } else {
                        add_param(new_prop, "TYPE", part);
                    }
                }
            }
            "ENCODING" | "CHARSET" => {}
            _ => {
                for part in &param.values {
                    add_param(new_prop, &param.name, part);
                }
            }
        }
    }
}

/// `VCardConverter::convertParameters30` (`VCardConverter.php:386-420`).
fn convert_parameters_30(new_prop: &mut Prop, parameters: Vec<Param>) {
    for mut param in parameters {
        param.no_name = false;
        match param.name.as_str() {
            "ENCODING" => {
                if !param.values.iter().any(|v| v.eq_ignore_ascii_case("QUOTED-PRINTABLE")) {
                    for part in &param.values {
                        add_param(new_prop, "ENCODING", part);
                    }
                }
            }
            "PREF" => {
                if param.values.first().map(String::as_str) == Some("1") {
                    add_param(new_prop, "TYPE", "PREF");
                }
            }
            _ => {
                for part in &param.values {
                    add_param(new_prop, &param.name, part);
                }
            }
        }
    }
}

fn add_param(prop: &mut Prop, name: &str, value: &str) {
    let name = name.to_ascii_uppercase();
    match prop.params.iter_mut().find(|p| p.name == name) {
        Some(existing) => {
            if !existing.values.iter().any(|v| v == value) {
                existing.values.push(value.to_string());
            }
        }
        None => prop.params.push(Param {
            name,
            values: vec![value.to_string()],
            no_name: false,
        }),
    }
}

/// `VCardConverter::convertBinaryToUri` (`VCardConverter.php:255-293`).
fn convert_binary_to_uri(mut new_prop: Prop, parameters: &mut Vec<Param>) -> Prop {
    let bytes = match &new_prop.value {
        Value::Binary(bytes) => bytes.clone(),
        Value::Text(parts) => parts.join(",").into_bytes(),
        Value::Uri(s) | Value::Raw(s) => s.clone().into_bytes(),
    };
    let mut mime = "application/octet-stream".to_string();
    if let Some(param) = parameters.iter_mut().find(|p| p.name == "TYPE") {
        let mut kept: Vec<String> = Vec::new();
        for part in &param.values {
            if ["JPEG", "PNG", "GIF"].iter().any(|t| part.eq_ignore_ascii_case(t)) {
                mime = format!("image/{}", part.to_lowercase());
            } else {
                kept.push(part.clone());
            }
        }
        if !kept.is_empty() {
            param.values = kept;
        } else {
            parameters.retain(|p| p.name != "TYPE");
        }
    }
    use base64::Engine;
    new_prop.class = Class::Uri;
    new_prop.value = Value::Uri(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ));
    new_prop
}

/// `VCardConverter::convertUriToBinary` (`VCardConverter.php:306-345`).
fn convert_uri_to_binary(mut new_prop: Prop) -> Prop {
    let value = match &new_prop.value {
        Value::Uri(value) => value.clone(),
        Value::Text(parts) => parts.join(","),
        Value::Raw(value) => value.clone(),
        Value::Binary(_) => return new_prop,
    };
    if !value.starts_with("data:") {
        return new_prop;
    }
    let (meta, payload) = value[5..].split_once(',').unwrap_or(("", ""));
    let mime = meta.split(';').next().unwrap_or("").to_string();
    let bytes = if meta.contains(';') {
        base64_decode_lenient(payload)
    } else {
        payload.as_bytes().to_vec()
    };
    new_prop.class = Class::Binary;
    new_prop.value = Value::Binary(bytes);
    add_param(&mut new_prop, "ENCODING", "b");
    match mime.as_str() {
        "image/jpeg" => add_param(&mut new_prop, "TYPE", "JPEG"),
        "image/png" => add_param(&mut new_prop, "TYPE", "PNG"),
        "image/gif" => add_param(&mut new_prop, "TYPE", "GIF"),
        _ => {}
    }
    new_prop
}

/// `DateTimeParser::parseVCardDateTime` (`DateTimeParser.php`) reduced to the
/// date parts the converter needs: `(year, month, date)` with the raw strings
/// used for the rewrite.
fn parse_vcard_date(value: &str) -> (Option<i64>, String, String) {
    // Compact: `[0-9]{4}(-)?[0-9]{2}?[0-9]{2}?`, `--[0-9]{2}?[0-9]{2}?`, `---[0-9]{2}`.
    // Extended: `YYYY-MM-DD` / `--MM-DD`.
    let digits: String = value.chars().take(10).collect();
    let _ = digits;
    let bytes = value.as_bytes();
    let four = |s: &str| s.parse::<i64>().ok();
    if value.starts_with("--") {
        // Year-less: `--MMDD` or `--MM-DD`.
        let rest = &value[2..];
        let month = rest.get(..2).unwrap_or("").to_string();
        let date = rest.get(3..5).or_else(|| rest.get(2..4)).unwrap_or("").to_string();
        return (None, month, date);
    }
    if bytes.len() >= 4 && bytes[..4].iter().all(|b| b.is_ascii_digit()) {
        let year = four(&value[..4]);
        let rest = &value[4..];
        let rest = rest.strip_prefix('-').unwrap_or(rest);
        let month = rest.get(..2).unwrap_or("").to_string();
        let date = rest.get(2..4).unwrap_or("").to_string();
        return (year, month, date);
    }
    (None, String::new(), String::new())
}

// ---------------------------------------------------------------------------
// Negotiation — `Plugin::negotiateVCard` + `Sabre\HTTP\negotiateContentType`
// ---------------------------------------------------------------------------

/// The `convertVCard` output mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Vcard3,
    Vcard4,
    Jcard,
}

#[derive(Debug, Clone, PartialEq)]
struct MimeType {
    type_name: String,
    sub_type: String,
    /// `parseMimeType` stores the whole `name=value` token as the value
    /// (`http-lib/functions.php:351`); duplicates keep the last
    /// (`functions.php:343-351`).
    parameters: Vec<(String, String)>,
    quality: f64,
}

/// `Sabre\HTTP\parseMimeType` (`http-lib/functions.php:312-356`). A
/// syntactically invalid value (`var_dump(); exit;` in PHP — declared
/// divergence) returns `None` here.
fn parse_mime_type(input: &str) -> Option<MimeType> {
    let mut parts = input.split(';');
    let head = parts.next()?.trim();
    let (type_name, sub_type) = head.split_once('/')?;
    let mut parameters = Vec::new();
    let mut quality = 1.0;
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let name = part.split('=').next().unwrap_or("").trim().to_string();
        if name == "q" {
            if let Some((_, value)) = part.split_once('=') {
                quality = value.trim().parse().unwrap_or(1.0);
                continue;
            }
        }
        parameters.retain(|(existing, _): &(String, String)| existing != &name);
        parameters.push((name, part.to_string()));
    }
    Some(MimeType {
        type_name: type_name.to_string(),
        sub_type: sub_type.to_string(),
        parameters,
        quality,
    })
}

/// `Sabre\HTTP\negotiateContentType` (`http-lib/functions.php:104-184`):
/// every option parameter must appear on the proposal with the identical
/// `name=value` token; the winner is the highest quality, then specificity
/// (20 type + 10 subtype + option parameters), then the lowest option index.
fn negotiate_content_type(proposal: &MimeType, options: &[&str]) -> Option<usize> {
    let mut best: Option<(i64, usize)> = None; // (score, -index) via tuple ordering below
    let mut best_index = 0usize;
    let mut best_score = -1i64;
    for (index, option) in options.iter().enumerate() {
        let option = parse_mime_type(option)?;
        if proposal.quality < 0.0 {
            continue;
        }
        if option.type_name != "*" && option.type_name != proposal.type_name {
            continue;
        }
        if option.sub_type != "*" && option.sub_type != proposal.sub_type {
            continue;
        }
        let mut matched = true;
        for (name, value) in &option.parameters {
            match proposal.parameters.iter().find(|(n, _)| n == name) {
                Some((_, proposal_value)) if proposal_value == value => {}
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if !matched {
            continue;
        }
        let mut score = option.parameters.len() as i64;
        if option.type_name != "*" {
            score += 20;
        }
        if option.sub_type != "*" {
            score += 10;
        }
        if score > best_score {
            best_score = score;
            best_index = index;
            best = Some((score, index));
        }
    }
    let _ = best;
    (best_score >= 0).then_some(best_index)
}

const OPTIONS: &[&str] = &[
    "text/x-vcard",
    "text/vcard",
    "text/vcard; version=4.0",
    "text/vcard; version=3.0",
    "application/vcard+json",
];

/// `Plugin::negotiateVCard` (`Plugin.php:756-789`). The proposal is the
/// report's `content-type` attribute (default `text/vcard`) with
/// `; version=<version>` (default `3.0`) appended; an unmatched proposal
/// falls through to `vcard3`.
pub fn negotiate(content_type: &str, version: &str) -> Target {
    let proposal = format!("{content_type}; version={version}");
    let winner = parse_mime_type(&proposal)
        .and_then(|p| negotiate_content_type(&p, OPTIONS))
        .and_then(|index| OPTIONS.get(index).copied());
    match winner {
        Some("text/vcard; version=4.0") => Target::Vcard4,
        Some("application/vcard+json") => Target::Jcard,
        _ => Target::Vcard3,
    }
}

// ---------------------------------------------------------------------------
// `Plugin::convertVCard` (`Plugin.php:803-855`)
// ---------------------------------------------------------------------------

/// Renders the `address-data` value for a REPORT.
///
/// `filter` is the `<card:prop>` name list. Per RFC 6352 `UID`, `VERSION` and
/// `FN` are always retained, and names match case-insensitively (Sabre is
/// case-sensitive — declared divergence). `apply_filter=false` reproduces the
/// `addressbook-multiget` call site... which the divergence deliberately
/// removes: both reports apply the filter here.
pub fn convert_vcard(
    data: &[u8],
    content_type: Option<&str>,
    version: Option<&str>,
    filter: &[String],
) -> Result<String, ParseError> {
    // `convertVCard` parses unconditionally (Plugin.php:808) — an unparseable
    // stored card is a 500 in the REPORT path.
    let mut doc = parse(data)?;

    let mut rendered: Option<String> = None;
    if !filter.is_empty() {
        // `Plugin.php:809-818`: keep UID/VERSION/FN + the requested names,
        // drop every other name (all occurrences).
        let mut keep: Vec<String> = vec!["UID".into(), "VERSION".into(), "FN".into()];
        keep.extend(filter.iter().map(|n| n.to_ascii_uppercase()));
        doc.props
            .retain(|p| keep.iter().any(|k| k == &p.name));
        rendered = Some(serialize(&doc));
    }

    let target = negotiate(content_type.unwrap_or("text/vcard"), version.unwrap_or("3.0"));
    let doc_version = doc.version();
    match target {
        Target::Vcard3 => {
            if doc_version == DocVersion::V30 {
                return Ok(rendered.unwrap_or_else(|| String::from_utf8_lossy(data).into_owned()));
            }
            Ok(serialize(&convert(&doc, Target::Vcard3)?))
        }
        Target::Vcard4 => {
            if doc_version == DocVersion::V40 {
                return Ok(rendered.unwrap_or_else(|| String::from_utf8_lossy(data).into_owned()));
            }
            Ok(serialize(&convert(&doc, Target::Vcard4)?))
        }
        Target::Jcard => Ok(json_serialize(&convert(&doc, Target::Jcard)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD3: &[u8] = b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Jane Doe\r\nN:Doe;Jane;;;\r\nEMAIL;TYPE=WORK:jane@example.com\r\nX-ABSHOWAS:COMPANY\r\nEND:VCARD\r\n";

    #[test]
    fn plain_request_is_byte_verbatim() {
        let out = convert_vcard(CARD3, None, None, &[]).unwrap();
        assert_eq!(out.as_bytes(), CARD3);
    }

    #[test]
    fn filter_keeps_uid_version_fn_and_requested_and_reorders() {
        // `EMAIL` kept by request, `UID`/`VERSION`/`FN` kept by rule, `N` and
        // `X-ABSHOWAS` dropped; VERSION is hoisted first (`Component.php:299-305`).
        let out = convert_vcard(CARD3, None, None, &["email".to_string()]).unwrap();
        assert_eq!(
            out,
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Jane Doe\r\nEMAIL;TYPE=WORK:jane@example.com\r\nEND:VCARD\r\n"
        );
    }

    #[test]
    fn filter_names_match_case_insensitively() {
        // Sabre is case-sensitive (declared divergence); `name="email"` must
        // keep `EMAIL` here.
        let out = convert_vcard(CARD3, None, None, &["EmAiL".to_string()]).unwrap();
        assert!(out.contains("EMAIL;TYPE=WORK:jane@example.com"), "{out}");
    }

    #[test]
    fn version_40_conversion_applies_the_vobject_rules() {
        let out = convert_vcard(CARD3, None, Some("4.0"), &[]).unwrap();
        assert!(out.starts_with("BEGIN:VCARD\r\nVERSION:4.0\r\n"), "{out}");
        // Fresh PRODID from `VCard::getDefaults()` (`Component/VCard.php:438-445`).
        assert!(
            out.contains(&format!("PRODID:-//Sabre//Sabre VObject {VOBJECT_VERSION}//EN")),
            "{out}"
        );
        // NAME/MAILER/LABEL/CLASS dropped in 4.0; X-ABSHOWAS:COMPANY → KIND:ORG.
        assert!(out.contains("KIND:ORG"), "{out}");
        assert!(!out.contains("X-ABSHOWAS"), "{out}");
    }

    #[test]
    fn version_30_conversion_from_v4_synthesises_the_apple_pair() {
        let card = b"BEGIN:VCARD\r\n\
VERSION:4.0\r\n\
FN:Jane Doe\r\n\
BDAY:--04-12\r\nANNIVERSARY:--06-01\r\nKIND:individual\r\nEND:VCARD\r\n";
        let out = convert_vcard(card, None, Some("3.0"), &[]).unwrap();
        // Year-less values become 1604 with the Apple marker
        // (`VCardConverter.php:95-108`); the marker parameter precedes the
        // value on the line.
        assert!(out.contains("BDAY;X-APPLE-OMIT-YEAR=1604:1604-04-12"), "{out}");
        // ANNIVERSARY → X-ANNIVERSARY + ITEM1.X-ABDATE/X-ABLABEL pair. The
        // converted property keeps DATE-AND-OR-TIME, so VALUE= is re-added
        // (`VCardConverter.php:232-239`).
        assert!(
            out.contains("X-ANNIVERSARY;X-APPLE-OMIT-YEAR=1604;VALUE=DATE-AND-OR-TIME:1604-06-01"),
            "{out}"
        );
        assert!(out.contains("ITEM1.X-ABDATE;VALUE=DATE-AND-OR-TIME:1604-06-01"), "{out}");
        assert!(out.contains("ITEM1.X-ABLABEL:_$!<Anniversary>!$_"), "{out}");
        // KIND:individual is implicit in 3.0 and dropped.
        assert!(!out.contains("KIND"), "{out}");
    }

    #[test]
    fn x_abdate_without_anniversary_label_stays_put() {
        let card = b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:X\r\nitem1.X-ABDATE:2020-01-02\r\nitem1.X-ABLABEL:_$!<Other>!$_\r\nEND:VCARD\r\n";
        let out = convert_vcard(card, None, Some("3.0"), &[]).unwrap();
        assert!(out.contains("ITEM1.X-ABDATE:2020-01-02"), "{out}");
        assert!(!out.contains("ANNIVERSARY"), "{out}");
    }

    #[test]
    fn binary_photo_becomes_a_data_uri_in_v4() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nPHOTO;ENCODING=b;TYPE=JPEG:QUFBQQ==\r\nEND:VCARD\r\n";
        let out = convert_vcard(card, None, Some("4.0"), &[]).unwrap();
        // The comma of a data URI is escaped on write (Uri::getRawMimeDirValue)
        // and the synthetic VALUE=URI parameter is appended for URL/PHOTO.
        assert!(
            out.contains("PHOTO;VALUE=URI:data:image/jpeg;base64\\,QUFBQQ=="),
            "{out}"
        );
    }

    #[test]
    fn jcard_matches_the_rfc7095_shape() {
        let out = convert_vcard(CARD3, Some("application/vcard+json"), None, &[]).unwrap();
        assert!(out.starts_with("[\"vcard\",[["), "{out}");
        // Lower-cased property names, lower-cased param keys, the VALUE
        // parameter omitted, structured N as one parts array.
        assert!(out.contains("[\"email\",{"), "{out}");
        // Parameter values keep their stored case (only keys are lower-cased).
        assert!(out.contains("\"type\":\"WORK\""), "{out}");
        assert!(out.contains("[\"n\",{},\"text\",[\"Doe\",\"Jane\",\"\",\"\",\"\"]]"), "{out}");
        // json_encode escapes `/` by default.
        assert!(out.contains("jane@example.com"), "{out}");
    }

    #[test]
    fn jcard_escaping_matches_php_json_encode() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:path/and\\, comma\r\nEND:VCARD\r\n";
        let out = convert_vcard(card, Some("application/vcard+json"), None, &[]).unwrap();
        // `\,` decodes to `,`; json_encode escapes `/` as `\/`.
        assert!(out.contains("path\\/and, comma"), "{out}");
    }

    #[test]
    fn negotiation_matrix() {
        assert_eq!(negotiate("text/vcard", "3.0"), Target::Vcard3);
        assert_eq!(negotiate("text/vcard", "4.0"), Target::Vcard4);
        assert_eq!(negotiate("text/x-vcard", "4.0"), Target::Vcard3); // x-vcard option has no params
        assert_eq!(negotiate("text/vcard", "2.1"), Target::Vcard3); // 2.1 is never a target
        assert_eq!(negotiate("text/vcard", "junk"), Target::Vcard3);
        assert_eq!(negotiate("application/vcard+json", "3.0"), Target::Jcard);
        // A malformed content-type degrades to vcard3 (declared divergence:
        // PHP `var_dump()`s and exits).
        assert_eq!(negotiate("garbage", "3.0"), Target::Vcard3);
    }

    #[test]
    fn unparseable_cards_are_errors() {
        assert!(convert_vcard(b"not a vcard", None, None, &[]).is_err());
        assert!(convert_vcard(b"", None, None, &[]).is_err());
    }

    #[test]
    fn serialization_round_trip_normalises_like_vobject() {
        // Unescaped `;` becomes escaped, `\N` becomes `\n`, names and param
        // names are upper-cased, same-name properties coalesce at the first
        // occurrence, N pads to five components (spec B6).
        let card = b"BEGIN:VCARD\r\nFN:Hello;World\r\nNOTE:line1\\Nline2\r\nemail:a@b\r\nN:x\r\nemail:c@d\r\nVERSION:3.0\r\nEND:VCARD\r\n";
        let out = convert_vcard(card, None, None, &["NOTE".into(), "EMAIL".into(), "N".into()])
            .unwrap();
        assert_eq!(
            out,
            "BEGIN:VCARD\r\n\
VERSION:3.0\r\n\
FN:Hello\\;World\r\n\
NOTE:line1\\nline2\r\n\
EMAIL:a@b\r\n\
EMAIL:c@d\r\n\
N:x;;;;\r\n\
END:VCARD\r\n"
        );
    }

    #[test]
    fn folding_is_75_bytes_and_never_splits_utf8() {
        let value = "é".repeat(100);
        let card = format!("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{value}\r\nEND:VCARD\r\n");
        let out = convert_vcard(card.as_bytes(), None, None, &["FN".into()]).unwrap();
        for line in out.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(line.len() <= 75, "line over 75 bytes: {line:?}");
            // No orphan continuation bytes at the start of a continuation.
            if let Some(cont) = line.strip_prefix(' ') {
                assert!(
                    !cont.as_bytes().first().map(|b| b & 0xC0 == 0x80).unwrap_or(false),
                    "fold split a UTF-8 char: {line:?}"
                );
            }
        }
    }

    #[test]
    fn empty_prop_filter_means_no_filter() {
        // `<card:prop/>` with no children is `!empty()`-false in PHP: the full
        // card comes back (`Plugin.php:809`).
        let out = convert_vcard(CARD3, None, None, &[]).unwrap();
        assert_eq!(out.as_bytes(), CARD3);
    }

    #[test]
    fn group_names_survive_the_filter() {
        let card = b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nitem1.EMAIL:x@y\r\nEND:VCARD\r\n";
        // Grouped properties match by base name (`Plugin.php:811-814`).
        let out = convert_vcard(card, None, None, &["EMAIL".into()]).unwrap();
        assert!(out.contains("ITEM1.EMAIL:x@y"), "{out}");
    }
}
