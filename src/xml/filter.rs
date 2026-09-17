// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RFC 6352 §10.5 `addressbook-query` filter parsing and evaluation.
//!
//! The control flow is a direct port of `Sabre\CardDAV\Plugin::validateFilters()`
//! (`3rdparty/sabre/dav/lib/CardDAV/Plugin.php`), including its early-exit rules,
//! so that Rust and PHP agree on every filter the test suite can express.
//! `oc_cards_properties` is deliberately *not* used: it is a truncated,
//! lower-cased search index, not the source of truth (see design doc §1.3).

use crate::error::{Error, Result};
use crate::vcard::{VCard, VProperty};
use crate::xml::parse::XNode;
use crate::xml::write::NS_CARDDAV;

/// `anyof` (OR) / `allof` (AND).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Test {
    AnyOf,
    AllOf,
}

impl Test {
    pub fn from_attr(attr: Option<&str>) -> std::result::Result<Self, String> {
        match attr {
            None | Some("anyof") => Ok(Test::AnyOf),
            Some("allof") => Ok(Test::AllOf),
            Some(other) => Err(format!(
                "The \"test\" attribute must be one of \"allof\" or \"anyof\", got {other:?}"
            )),
        }
    }
}

/// `collation` attribute of a `text-match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collation {
    AsciiCasemap,
    Octet,
    UnicodeCasemap,
}

impl Collation {
    fn from_attr(attr: Option<&str>) -> Result<Self> {
        match attr.unwrap_or("i;unicode-casemap") {
            "i;ascii-casemap" => Ok(Collation::AsciiCasemap),
            "i;octet" => Ok(Collation::Octet),
            "i;unicode-casemap" => Ok(Collation::UnicodeCasemap),
            other => Err(Error::bad_request(format!(
                "Collation type: {other} is not supported"
            ))),
        }
    }
}

/// `match-type` attribute of a `text-match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchType {
    Contains,
    Equals,
    StartsWith,
    EndsWith,
}

impl MatchType {
    fn from_attr(attr: Option<&str>) -> Result<Self> {
        match attr.unwrap_or("contains") {
            "contains" => Ok(MatchType::Contains),
            "equals" => Ok(MatchType::Equals),
            "starts-with" => Ok(MatchType::StartsWith),
            "ends-with" => Ok(MatchType::EndsWith),
            other => Err(Error::bad_request(format!("Unknown match-type: {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    pub value: String,
    pub collation: Collation,
    pub match_type: MatchType,
    pub negate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamFilter {
    pub name: String,
    pub is_not_defined: bool,
    pub text_match: Option<TextMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropFilter {
    pub name: String,
    pub test: Test,
    pub is_not_defined: bool,
    pub text_matches: Vec<TextMatch>,
    pub param_filters: Vec<ParamFilter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBookFilter {
    pub test: Test,
    pub prop_filters: Vec<PropFilter>,
}

/// Parses a `{urn:ietf:params:xml:ns:carddav}filter` element.
pub fn parse_filter(node: &XNode) -> Result<AddressBookFilter> {
    if node.ns != NS_CARDDAV || node.local != "filter" {
        return Err(Error::bad_request("expected a carddav:filter element"));
    }
    let test = Test::from_attr(node.attr("test")).map_err(Error::bad_request)?;
    let prop_filters = node
        .children_of(NS_CARDDAV, "prop-filter")
        .map(parse_prop_filter)
        .collect::<Result<Vec<_>>>()?;
    Ok(AddressBookFilter { test, prop_filters })
}

fn parse_prop_filter(node: &XNode) -> Result<PropFilter> {
    let name = node
        .attr("name")
        .ok_or_else(|| Error::bad_request("prop-filter requires a name attribute"))?
        .to_uppercase();
    let test = Test::from_attr(node.attr("test")).map_err(Error::bad_request)?;
    let is_not_defined = node.child(NS_CARDDAV, "is-not-defined").is_some();
    let text_matches = node
        .children_of(NS_CARDDAV, "text-match")
        .map(parse_text_match)
        .collect::<Result<Vec<_>>>()?;
    let param_filters = node
        .children_of(NS_CARDDAV, "param-filter")
        .map(parse_param_filter)
        .collect::<Result<Vec<_>>>()?;
    Ok(PropFilter {
        name,
        test,
        is_not_defined,
        text_matches,
        param_filters,
    })
}

fn parse_param_filter(node: &XNode) -> Result<ParamFilter> {
    let name = node
        .attr("name")
        .ok_or_else(|| Error::bad_request("param-filter requires a name attribute"))?
        .to_uppercase();
    let is_not_defined = node.child(NS_CARDDAV, "is-not-defined").is_some();
    let text_match = node
        .children_of(NS_CARDDAV, "text-match")
        .next()
        .map(parse_text_match)
        .transpose()?;
    Ok(ParamFilter {
        name,
        is_not_defined,
        text_match,
    })
}

fn parse_text_match(node: &XNode) -> Result<TextMatch> {
    Ok(TextMatch {
        value: node.text.trim().to_string(),
        collation: Collation::from_attr(node.attr("collation"))?,
        match_type: MatchType::from_attr(node.attr("match-type"))?,
        negate: node.attr("negate-condition") == Some("yes"),
    })
}

/// Evaluates a filter against a parsed vCard.
pub fn evaluate(card: &VCard, filter: &AddressBookFilter) -> bool {
    if filter.prop_filters.is_empty() {
        return true;
    }
    for prop_filter in &filter.prop_filters {
        let success = validate_prop_filter(card, prop_filter);
        if filter.test == Test::AnyOf && success {
            return true;
        }
        if filter.test == Test::AllOf && !success {
            return false;
        }
    }
    filter.test == Test::AllOf
}

fn validate_prop_filter(card: &VCard, filter: &PropFilter) -> bool {
    let properties: Vec<&VProperty> = card.select(&filter.name).collect();
    let is_defined = !properties.is_empty();

    if filter.is_not_defined {
        return !is_defined;
    }
    if (filter.param_filters.is_empty() && filter.text_matches.is_empty()) || !is_defined {
        return is_defined;
    }

    let mut results: Vec<bool> = Vec::with_capacity(2);
    if !filter.param_filters.is_empty() {
        results.push(validate_param_filters(
            &properties,
            &filter.param_filters,
            filter.test,
        ));
    }
    if !filter.text_matches.is_empty() {
        let texts: Vec<&str> = properties.iter().map(|p| p.value.as_str()).collect();
        results.push(validate_text_matches(
            &texts,
            &filter.text_matches,
            filter.test,
        ));
    }

    match results.len() {
        0 => false,
        1 => results[0],
        _ => {
            if filter.test == Test::AnyOf {
                results[0] || results[1]
            } else {
                results[0] && results[1]
            }
        }
    }
}

fn validate_text_matches(texts: &[&str], filters: &[TextMatch], test: Test) -> bool {
    for filter in filters {
        let mut success = false;
        for haystack in texts {
            success = text_match(haystack, &filter.value, filter.collation, filter.match_type);
            if filter.negate {
                success = !success;
            }
            if success {
                break;
            }
        }
        if success && test == Test::AnyOf {
            return true;
        }
        if !success && test == Test::AllOf {
            return false;
        }
    }
    test == Test::AllOf
}

fn validate_param_filters(properties: &[&VProperty], filters: &[ParamFilter], test: Test) -> bool {
    for filter in filters {
        let is_defined = properties
            .iter()
            .any(|p| p.params.iter().any(|(name, _)| name == &filter.name));

        let success = if filter.is_not_defined {
            !is_defined
        } else if filter.text_match.is_none() || !is_defined {
            is_defined
        } else {
            let text_match_filter = filter.text_match.as_ref().unwrap();
            let mut matched = false;
            'outer: for property in properties {
                if let Some((_, values)) = property
                    .params
                    .iter()
                    .find(|(name, _)| name == &filter.name)
                {
                    for value in values {
                        let mut success = text_match(
                            value,
                            &text_match_filter.value,
                            text_match_filter.collation,
                            text_match_filter.match_type,
                        );
                        if text_match_filter.negate {
                            success = !success;
                        }
                        if success {
                            matched = true;
                            break 'outer;
                        }
                    }
                }
            }
            matched
        };

        if success && test == Test::AnyOf {
            return true;
        }
        if !success && test == Test::AllOf {
            return false;
        }
    }
    test == Test::AllOf
}

/// `Sabre\DAV\StringUtil::textMatch()`.
pub fn text_match(
    haystack: &str,
    needle: &str,
    collation: Collation,
    match_type: MatchType,
) -> bool {
    let (haystack, needle) = match collation {
        Collation::Octet => (haystack.to_string(), needle.to_string()),
        Collation::AsciiCasemap => (haystack.to_ascii_uppercase(), needle.to_ascii_uppercase()),
        Collation::UnicodeCasemap => (haystack.to_uppercase(), needle.to_uppercase()),
    };
    match match_type {
        MatchType::Contains => haystack.contains(&needle),
        MatchType::Equals => haystack == needle,
        MatchType::StartsWith => haystack.starts_with(&needle),
        MatchType::EndsWith => haystack.ends_with(&needle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcard;

    const CARD: &[u8] = b"BEGIN:VCARD\r\n\
VERSION:3.0\r\n\
UID:1234\r\n\
FN:Jane Doe\r\n\
N:Doe;Jane;;;\r\n\
EMAIL;TYPE=WORK,INTERNET:jane@example.com\r\n\
EMAIL;TYPE=HOME:jane@home.example\r\n\
TEL:+1 555 0100\r\n\
END:VCARD\r\n";

    fn filter(xml: &[u8]) -> AddressBookFilter {
        let doc = crate::xml::parse::parse_document(xml).unwrap();
        parse_filter(&doc).unwrap()
    }

    #[test]
    fn text_match_semantics() {
        use Collation::*;
        use MatchType::*;
        assert!(text_match("Jane Doe", "jane", UnicodeCasemap, Contains));
        assert!(text_match("Jane Doe", "JANE", AsciiCasemap, Contains));
        assert!(!text_match("Jane Doe", "jane", Octet, Contains));
        assert!(text_match("Jane Doe", "Jane Doe", UnicodeCasemap, Equals));
        assert!(!text_match("Jane Doe", "Jane", UnicodeCasemap, Equals));
        assert!(text_match("Jane Doe", "Jane", UnicodeCasemap, StartsWith));
        assert!(text_match("Jane Doe", "Doe", UnicodeCasemap, EndsWith));
        assert!(!text_match("Jane Doe", "Doe", UnicodeCasemap, StartsWith));
        // Unicode case folding, not just ASCII.
        assert!(text_match("Strässer", "STRÄSSER", UnicodeCasemap, Equals));
    }

    #[test]
    fn existence_only_prop_filter() {
        let card = vcard::parse(CARD);
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="EMAIL"/></card:filter>"#,
        );
        assert!(evaluate(&card, &f));
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="BDAY"/></card:filter>"#,
        );
        assert!(!evaluate(&card, &f));
    }

    #[test]
    fn is_not_defined() {
        let card = vcard::parse(CARD);
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="BDAY"><card:is-not-defined/></card:prop-filter></card:filter>"#,
        );
        assert!(evaluate(&card, &f));
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:is-not-defined/></card:prop-filter></card:filter>"#,
        );
        assert!(!evaluate(&card, &f));
    }

    #[test]
    fn text_match_contains() {
        let card = vcard::parse(CARD);
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match>doe</card:text-match></card:prop-filter></card:filter>"#,
        );
        assert!(evaluate(&card, &f));
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match>smith</card:text-match></card:prop-filter></card:filter>"#,
        );
        assert!(!evaluate(&card, &f));
    }

    #[test]
    fn negate_condition() {
        let card = vcard::parse(CARD);
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match negate-condition="yes">smith</card:text-match></card:prop-filter></card:filter>"#,
        );
        assert!(evaluate(&card, &f));
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match negate-condition="yes">doe</card:text-match></card:prop-filter></card:filter>"#,
        );
        assert!(!evaluate(&card, &f));
    }

    #[test]
    fn param_filter_type() {
        let card = vcard::parse(CARD);
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="EMAIL"><card:param-filter name="TYPE"><card:text-match match-type="equals">WORK</card:text-match></card:param-filter></card:prop-filter></card:filter>"#,
        );
        assert!(evaluate(&card, &f));
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="EMAIL"><card:param-filter name="TYPE"><card:text-match match-type="equals">FAX</card:text-match></card:param-filter></card:prop-filter></card:filter>"#,
        );
        assert!(!evaluate(&card, &f));
    }

    #[test]
    fn param_filter_is_not_defined() {
        let card = vcard::parse(CARD);
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="EMAIL"><card:param-filter name="X-NOPE"><card:is-not-defined/></card:param-filter></card:prop-filter></card:filter>"#,
        );
        assert!(evaluate(&card, &f));
    }

    #[test]
    fn allof_and_anyof() {
        let card = vcard::parse(CARD);
        // anyof: one matching prop-filter is enough.
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav" test="anyof"><card:prop-filter name="FN"><card:text-match>nope</card:text-match></card:prop-filter><card:prop-filter name="EMAIL"><card:text-match>jane@example.com</card:text-match></card:prop-filter></card:filter>"#,
        );
        assert!(evaluate(&card, &f));
        // allof: both must match.
        let f = filter(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav" test="allof"><card:prop-filter name="FN"><card:text-match>nope</card:text-match></card:prop-filter><card:prop-filter name="EMAIL"><card:text-match>jane@example.com</card:text-match></card:prop-filter></card:filter>"#,
        );
        assert!(!evaluate(&card, &f));
    }

    #[test]
    fn empty_filter_matches_everything() {
        let card = vcard::parse(CARD);
        let f = filter(br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"/>"#);
        assert!(evaluate(&card, &f));
    }

    #[test]
    fn rejects_unknown_collation_and_match_type() {
        let doc = crate::xml::parse::parse_document(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match collation="i;bogus">x</card:text-match></card:prop-filter></card:filter>"#,
        )
        .unwrap();
        assert!(parse_filter(&doc).is_err());

        let doc = crate::xml::parse::parse_document(
            br#"<card:filter xmlns:card="urn:ietf:params:xml:ns:carddav"><card:prop-filter name="FN"><card:text-match match-type="bogus">x</card:text-match></card:prop-filter></card:filter>"#,
        )
        .unwrap();
        assert!(parse_filter(&doc).is_err());
    }
}
