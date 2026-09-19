// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The DAV **discovery** surface: `PROPFIND` Depth 0 on the DAV root
//! (`/remote.php/dav/`) and on the caller's own principal
//! (`/remote.php/dav/principals/users/<uid>/`).
//!
//! Every client session performs this pair before it lists anything, and each
//! one otherwise pays a full PHP bootstrap to return a small, fixed response.
//! Only Depth 0 is served; the collection listings (`/principals/`,
//! `/principals/users/`) and every non-`PROPFIND` method (OPTIONS included) are
//! delegated with a `501` so nginx replays them to PHP.
//!
//! ## The property gate
//!
//! An explicit request whose property list contains any qname outside the
//! implemented set is answered **501** (delegated), never `404`: a `404` would
//! claim a property PHP serves is absent. `allprop`/`propname` reproduce PHP's
//! output for these nodes (just `{DAV:}resourcetype`).
//!
//! ## Own principal only
//!
//! Only the authenticated user's own principal is served. Another user's
//! principal answers **501** so PHP produces the response (which carries the
//! *caller's* `current-user-principal`, among other things, so it cannot be
//! invented from the target uid).

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::util::{encode_path_segment, percent_decode};
use crate::xml::parse::{self, PropList};
use crate::xml::write::{
    DavResponse, MultiStatus, PropQName, PropStat, PropValue, XmlElement, NS_CALDAV, NS_CARDDAV,
    NS_DAV, NS_NEXTCLOUD, NS_SABREDAV,
};

/// The parsed discovery request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryPath {
    /// `<webroot>/remote.php/dav` (no trailing slash), used to build hrefs.
    pub context: String,
    pub target: DiscoveryTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryTarget {
    /// `PROPFIND /remote.php/dav/`.
    Root { href: String },
    /// `PROPFIND /remote.php/dav/principals/users/<uid>/`.
    Principal { uid: String, href: String },
    /// A path in the principal tree the sidecar deliberately does not serve:
    /// the `/principals/` and `/principals/users/` collection listings, group
    /// principals, calendar resources/rooms and the calendar-proxy children.
    /// Delegated to PHP (501).
    Delegated,
    NotFound,
}

/// Parses `<webroot>/remote.php/dav[/principals/users/<uid>]`.
pub fn parse_discovery_path(path: &str) -> DiscoveryPath {
    let marker = "/remote.php/dav";
    let Some(index) = path.find(marker) else {
        return DiscoveryPath {
            context: String::new(),
            target: DiscoveryTarget::NotFound,
        };
    };
    let context = path[..index + marker.len()].to_string();
    let rest = &path[index + marker.len()..];
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

    match segments.as_slice() {
        [] => DiscoveryPath {
            context: context.clone(),
            target: DiscoveryTarget::Root {
                href: format!("{context}/"),
            },
        },
        ["principals", "users", uid] if !uid.is_empty() => {
            let uid = percent_decode(uid);
            DiscoveryPath {
                context: context.clone(),
                target: DiscoveryTarget::Principal {
                    href: principal_href(&context, &uid),
                    uid,
                },
            }
        }
        ["principals", ..] => DiscoveryPath {
            context,
            target: DiscoveryTarget::Delegated,
        },
        _ => DiscoveryPath {
            context,
            target: DiscoveryTarget::NotFound,
        },
    }
}

fn principal_href(context: &str, uid: &str) -> String {
    format!("{context}/principals/users/{}/", encode_path_segment(uid))
}

/// The implemented property set for a target. An explicit request for anything
/// outside it is answered 501 (delegated), never 404.
pub fn is_implemented(target: &DiscoveryTarget, ns: &str, local: &str) -> bool {
    match target {
        DiscoveryTarget::Root { .. } => matches!(
            (ns, local),
            (NS_DAV, "resourcetype")
                | (NS_DAV, "current-user-principal")
                | (NS_DAV, "principal-collection-set")
                | (NS_DAV, "supported-report-set")
                | (NS_DAV, "current-user-privilege-set")
        ),
        DiscoveryTarget::Principal { .. } => matches!(
            (ns, local),
            (NS_DAV, "resourcetype")
                | (NS_DAV, "principal-URL")
                | (NS_DAV, "current-user-principal")
                | (NS_DAV, "displayname")
                | (NS_DAV, "principal-collection-set")
                | (NS_DAV, "supported-report-set")
                | (NS_DAV, "current-user-privilege-set")
                | (NS_DAV, "owner")
                | (NS_DAV, "alternate-URI-set")
                | (NS_DAV, "group-membership")
                | (NS_CARDDAV, "addressbook-home-set")
                | (NS_CALDAV, "calendar-home-set")
                | (NS_CALDAV, "calendar-user-address-set")
                | (NS_CALDAV, "calendar-user-type")
                | (NS_NEXTCLOUD, "language")
                | (NS_SABREDAV, "email-address")
        ),
        DiscoveryTarget::NotFound => false,
        DiscoveryTarget::Delegated => false,
    }
}

/// Sabre's `allprop` result for these nodes: only `{DAV:}resourcetype`
/// (the DAVACL/CalDAV/CardDAV handlers only run for explicit requests).
pub fn default_props() -> Vec<PropQName> {
    vec![PropQName::dav("resourcetype")]
}

struct DiscoveryContext {
    context: String,
    uid: String,
    displayname: String,
    language: Option<String>,
    email: Option<String>,
    additional_mails: Vec<String>,
    /// `principals/groups/<urlencoded gid>` (as PHP's `getGroupMembership`).
    groups: Vec<String>,
}

/// Serves a discovery `PROPFIND` or returns `None` (delegate to PHP with 501).
pub async fn handle_propfind(
    db: &Db,
    config: &Config,
    uid: &str,
    parsed: &DiscoveryPath,
    body: &[u8],
) -> Result<Option<MultiStatus>> {
    // Parse the body first: a malformed body is a 400 in PHP too.
    let request = parse::parse_propfind(body)?;

    // Condition 1: every explicit property must be implemented.
    if let PropList::Props(props) = &request.props {
        if props
            .iter()
            .any(|qname| !is_implemented(&parsed.target, &qname.ns, &qname.local))
        {
            return Ok(None);
        }
    }

    let ctx = match &parsed.target {
        DiscoveryTarget::Root { .. } => DiscoveryContext {
            context: parsed.context.clone(),
            uid: uid.to_string(),
            displayname: String::new(),
            language: None,
            email: None,
            additional_mails: Vec::new(),
            groups: Vec::new(),
        },
        DiscoveryTarget::Principal { uid: target_uid, .. } => {
            // Only the caller's own principal is served; another user's is a
            // 501 so PHP answers (its response is caller-scoped).
            if target_uid != uid {
                return Ok(None);
            }
            let wants_language = match &request.props {
                PropList::AllProp | PropList::PropName => false,
                PropList::Props(props) => props
                    .iter()
                    .any(|q| q.ns == NS_NEXTCLOUD && q.local == "language"),
            };
            let language = config
                .force_language
                .clone()
                .or(db.user_preference(uid, "core", "lang").await?);
            // `L10N\Factory::getUserLanguage()` falls back to the request's
            // `Accept-Language` before `default_language`, which the sidecar
            // does not reproduce. With no `force_language`/`core/lang` value the
            // property is not derivable, so the request is delegated.
            if wants_language && language.is_none() {
                return Ok(None);
            }
            let displayname = db
                .user_display_name(uid)
                .await?
                .unwrap_or_else(|| uid.to_string());
            let email = db
                .user_preference(uid, "settings", "email")
                .await?
                .map(|value| value.trim().to_lowercase())
                .filter(|value| !value.is_empty());
            let additional_mails = match db.account_data(uid).await {
                Ok(data) => data
                    .as_deref()
                    .map(additional_mails_from_json)
                    .unwrap_or_default(),
                Err(error) => {
                    // `oc_accounts` is a core table; if it is unavailable the
                    // account is treated as having no extra addresses rather
                    // than failing the whole discovery.
                    log::warn!("oc_accounts lookup failed, ignoring additional emails: {error}");
                    Vec::new()
                }
            };
            let groups = db.group_principals(uid).await?;
            DiscoveryContext {
                context: parsed.context.clone(),
                uid: uid.to_string(),
                displayname,
                language,
                email,
                additional_mails,
                groups,
            }
        }
        DiscoveryTarget::Delegated => return Ok(None),
        DiscoveryTarget::NotFound => return Ok(None),
    };

    let href = match &parsed.target {
        DiscoveryTarget::Root { href } | DiscoveryTarget::Principal { href, .. } => href.clone(),
        DiscoveryTarget::Delegated | DiscoveryTarget::NotFound => return Ok(None),
    };
    let response = build_response(&href, &parsed.target, &ctx, &request.props);
    Ok(Some(MultiStatus {
        responses: vec![response],
        sync_token: None,
    }))
}

fn build_response(
    href: &str,
    target: &DiscoveryTarget,
    ctx: &DiscoveryContext,
    props: &PropList,
) -> DavResponse {
    let requested: Vec<PropQName> = match props {
        PropList::AllProp | PropList::PropName => default_props(),
        PropList::Props(props) => props.clone(),
    };
    let propname_only = matches!(props, PropList::PropName);

    let mut found: Vec<(PropQName, PropValue)> = Vec::new();
    let mut missing: Vec<PropQName> = Vec::new();
    for qname in &requested {
        match resolve_property(qname, target, ctx) {
            Some(value) => {
                if propname_only {
                    found.push((qname.clone(), PropValue::Empty));
                } else {
                    found.push((qname.clone(), value));
                }
            }
            None => missing.push(qname.clone()),
        }
    }

    let mut propstats = Vec::new();
    if !found.is_empty() {
        propstats.push(PropStat::ok(found));
    }
    // `allprop`/`propname` strip the 404 propstat, matching Sabre's
    // `PropFind::getResultForMultiStatus()`.
    if !missing.is_empty() && matches!(props, PropList::Props(_)) {
        propstats.push(PropStat::not_found(missing));
    }
    DavResponse::props(href.to_string(), propstats)
}

fn resolve_property(
    qname: &PropQName,
    target: &DiscoveryTarget,
    ctx: &DiscoveryContext,
) -> Option<PropValue> {
    let ns = qname.ns.as_str();
    let local = qname.local.as_str();
    let principal = principal_href(&ctx.context, &ctx.uid);

    match target {
        DiscoveryTarget::Root { .. } => match (ns, local) {
            (NS_DAV, "resourcetype") => {
                Some(PropValue::Elements(vec![XmlElement::new("d:collection")]))
            }
            (NS_DAV, "current-user-principal") => href_prop(&principal),
            (NS_DAV, "principal-collection-set") => principal_collection_set(&ctx.context),
            (NS_DAV, "supported-report-set") => supported_report_set(),
            (NS_DAV, "current-user-privilege-set") => privilege_set(&[
                "d:all",
                "d:read",
                "d:write",
                "d:write-properties",
                "d:write-content",
                "d:unlock",
                "d:bind",
                "d:unbind",
                "d:read-acl",
                "d:read-current-user-privilege-set",
            ]),
            _ => None,
        },
        DiscoveryTarget::Principal { .. } => match (ns, local) {
            (NS_DAV, "resourcetype") => Some(PropValue::Elements(vec![
                XmlElement::new("d:collection"),
                XmlElement::new("d:principal"),
            ])),
            (NS_DAV, "principal-URL") => href_prop(&principal),
            (NS_DAV, "current-user-principal") => href_prop(&principal),
            (NS_DAV, "displayname") => Some(PropValue::Text(ctx.displayname.clone())),
            (NS_DAV, "principal-collection-set") => principal_collection_set(&ctx.context),
            (NS_DAV, "supported-report-set") => supported_report_set(),
            (NS_DAV, "current-user-privilege-set") => privilege_set(&[
                "d:read",
                "d:read-acl",
                "d:read-current-user-privilege-set",
                "d:all",
                "d:write",
                "d:write-properties",
                "d:write-content",
                "d:unlock",
                "d:bind",
                "d:unbind",
                "d:write-acl",
            ]),
            (NS_DAV, "owner") => href_prop(&principal),
            (NS_DAV, "alternate-URI-set") => Some(href_list(&alternate_uri_set(ctx))),
            (NS_DAV, "group-membership") => Some(href_list(
                &ctx.groups
                    .iter()
                    .map(|group| format!("{}/{group}/", ctx.context))
                    .collect::<Vec<_>>(),
            )),
            (NS_CARDDAV, "addressbook-home-set") => href_prop(&format!(
                "{}/addressbooks/users/{}/",
                ctx.context,
                encode_path_segment(&ctx.uid)
            )),
            (NS_CALDAV, "calendar-home-set") => href_prop(&format!(
                "{}/calendars/{}/",
                ctx.context,
                encode_path_segment(&ctx.uid)
            )),
            (NS_CALDAV, "calendar-user-address-set") => {
                let mut addresses = alternate_uri_set(ctx);
                addresses.push(principal.clone());
                Some(href_list(&addresses))
            }
            (NS_CALDAV, "calendar-user-type") => {
                Some(PropValue::Text("INDIVIDUAL".to_string()))
            }
            (NS_NEXTCLOUD, "language") => ctx.language.clone().map(PropValue::Text),
            (NS_SABREDAV, "email-address") => ctx.email.clone().map(PropValue::Text),
            _ => None,
        },
        DiscoveryTarget::NotFound => None,
        DiscoveryTarget::Delegated => None,
    }
}

/// `{DAV:}principal-collection-set`: the four static principal collections.
fn principal_collection_set(context: &str) -> Option<PropValue> {
    Some(href_list(&[
        format!("{context}/principals/users/"),
        format!("{context}/principals/groups/"),
        format!("{context}/principals/calendar-resources/"),
        format!("{context}/principals/calendar-rooms/"),
    ]))
}

/// The report set Sabre advertises for the DAV root and a principal (live
/// capture, 2026-09-19).
fn supported_report_set() -> Option<PropValue> {
    Some(PropValue::Elements(
        [
            "d:expand-property",
            "d:principal-match",
            "d:principal-property-search",
            "d:principal-search-property-set",
            "oc:filter-comments",
            "oc:filter-files",
        ]
        .iter()
        .map(|name| {
            XmlElement::new("d:supported-report")
                .child(XmlElement::new("d:report").child(XmlElement::new(*name)))
        })
        .collect(),
    ))
}

fn privilege_set(privileges: &[&str]) -> Option<PropValue> {
    Some(PropValue::Elements(
        privileges
            .iter()
            .map(|name| XmlElement::new("d:privilege").child(XmlElement::new(*name)))
            .collect(),
    ))
}

fn href_prop(href: &str) -> Option<PropValue> {
    Some(href_list(std::slice::from_ref(&href.to_string())))
}

fn href_list(hrefs: &[String]) -> PropValue {
    PropValue::Elements(
        hrefs
            .iter()
            .map(|href| XmlElement::new("d:href").text(href.clone()))
            .collect(),
    )
}

/// `{DAV:}alternate-URI-set` / `{CalDAV}calendar-user-address-set` prefix: the
/// account's `additional_mail` addresses plus the primary email, as `mailto:`.
fn alternate_uri_set(ctx: &DiscoveryContext) -> Vec<String> {
    let mut addresses: Vec<String> = ctx
        .additional_mails
        .iter()
        .map(|mail| format!("mailto:{mail}"))
        .collect();
    if let Some(email) = &ctx.email {
        addresses.push(format!("mailto:{email}"));
    }
    let mut seen = std::collections::HashSet::new();
    addresses.retain(|address| seen.insert(address.clone()));
    addresses
}

/// Extracts the `additional_mail` property collection from `oc_accounts.data`.
///
/// The account manager stores collections as an array of objects with a
/// `value` key (`AccountManager::prepareJson`).
pub fn additional_mails_from_json(data: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return Vec::new();
    };
    let Some(entry) = value.get("additional_mail") else {
        return Vec::new();
    };
    let items: Vec<&serde_json::Value> = match entry {
        serde_json::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    items
        .into_iter()
        .filter_map(|item| item.get("value").and_then(|value| value.as_str()))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_root_and_principal() {
        assert_eq!(
            parse_discovery_path("/remote.php/dav/").target,
            DiscoveryTarget::Root {
                href: "/remote.php/dav/".into()
            }
        );
        assert_eq!(
            parse_discovery_path("/remote.php/dav").target,
            DiscoveryTarget::Root {
                href: "/remote.php/dav/".into()
            }
        );
        assert_eq!(
            parse_discovery_path("/remote.php/dav/principals/users/alice/").target,
            DiscoveryTarget::Principal {
                uid: "alice".into(),
                href: "/remote.php/dav/principals/users/alice/".into()
            }
        );
        assert_eq!(
            parse_discovery_path("/remote.php/dav/principals/users/alice").target,
            DiscoveryTarget::Principal {
                uid: "alice".into(),
                href: "/remote.php/dav/principals/users/alice/".into()
            }
        );
    }

    #[test]
    fn parses_with_webroot_and_encoding() {
        let parsed = parse_discovery_path("/nextcloud/remote.php/dav/principals/users/al%20ice");
        assert_eq!(parsed.context, "/nextcloud/remote.php/dav");
        match parsed.target {
            DiscoveryTarget::Principal { uid, href } => {
                assert_eq!(uid, "al ice");
                assert_eq!(href, "/nextcloud/remote.php/dav/principals/users/al%20ice/");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_listings_and_other_trees() {
        // Paths in the principal tree are delegated, not served.
        for path in [
            "/remote.php/dav/principals/",
            "/remote.php/dav/principals",
            "/remote.php/dav/principals/users/",
            "/remote.php/dav/principals/users/alice/contacts",
            "/remote.php/dav/principals/groups/admin/",
        ] {
            assert_eq!(
                parse_discovery_path(path).target,
                DiscoveryTarget::Delegated,
                "{path}"
            );
        }
        // Trees the discovery router does not own are 404.
        for path in [
            "/remote.php/dav/addressbooks/users/alice/",
            "/remote.php/dav/files/alice/",
        ] {
            assert_eq!(
                parse_discovery_path(path).target,
                DiscoveryTarget::NotFound,
                "{path}"
            );
        }
    }

    #[test]
    fn gate_is_target_specific() {
        let root = DiscoveryTarget::Root {
            href: "/remote.php/dav/".into(),
        };
        assert!(is_implemented(&root, NS_DAV, "current-user-principal"));
        assert!(!is_implemented(&root, NS_DAV, "displayname"));
        assert!(!is_implemented(&root, NS_NEXTCLOUD, "language"));

        let principal = DiscoveryTarget::Principal {
            uid: "alice".into(),
            href: "/remote.php/dav/principals/users/alice/".into(),
        };
        assert!(is_implemented(&principal, NS_NEXTCLOUD, "language"));
        assert!(is_implemented(&principal, NS_CALDAV, "calendar-home-set"));
        assert!(!is_implemented(&principal, NS_DAV, "getetag"));
        assert!(!is_implemented(&principal, NS_DAV, "acl"));
    }

    fn ctx() -> DiscoveryContext {
        DiscoveryContext {
            context: "/remote.php/dav".into(),
            uid: "alice".into(),
            displayname: "Alice A".into(),
            language: Some("fr".into()),
            email: Some("alice@example.com".into()),
            additional_mails: vec!["alt@example.com".into()],
            groups: vec!["principals/groups/admin".into()],
        }
    }

    #[test]
    fn root_properties_match_php() {
        let root = DiscoveryTarget::Root {
            href: "/remote.php/dav/".into(),
        };
        let c = ctx();
        assert_eq!(
            resolve_property(&PropQName::dav("resourcetype"), &root, &c),
            Some(PropValue::Elements(vec![XmlElement::new("d:collection")]))
        );
        assert_eq!(
            resolve_property(&PropQName::dav("current-user-principal"), &root, &c),
            Some(PropValue::Elements(vec![XmlElement::new("d:href")
                .text("/remote.php/dav/principals/users/alice/")]))
        );
        // The root does not implement the principal-only properties.
        assert_eq!(resolve_property(&PropQName::dav("owner"), &root, &c), None);
        assert_eq!(resolve_property(&PropQName::dav("displayname"), &root, &c), None);
    }

    #[test]
    fn principal_properties_match_php() {
        let principal = DiscoveryTarget::Principal {
            uid: "alice".into(),
            href: "/remote.php/dav/principals/users/alice/".into(),
        };
        let c = ctx();
        assert_eq!(
            resolve_property(&PropQName::dav("resourcetype"), &principal, &c),
            Some(PropValue::Elements(vec![
                XmlElement::new("d:collection"),
                XmlElement::new("d:principal"),
            ]))
        );
        assert_eq!(
            resolve_property(&PropQName::dav("displayname"), &principal, &c),
            Some(PropValue::Text("Alice A".into()))
        );
        assert_eq!(
            resolve_property(&PropQName::new(NS_CARDDAV, "addressbook-home-set"), &principal, &c),
            Some(PropValue::Elements(vec![XmlElement::new("d:href")
                .text("/remote.php/dav/addressbooks/users/alice/")]))
        );
        assert_eq!(
            resolve_property(&PropQName::new(NS_CALDAV, "calendar-home-set"), &principal, &c),
            Some(PropValue::Elements(vec![XmlElement::new("d:href")
                .text("/remote.php/dav/calendars/alice/")]))
        );
        assert_eq!(
            resolve_property(&PropQName::new(NS_CALDAV, "calendar-user-type"), &principal, &c),
            Some(PropValue::Text("INDIVIDUAL".into()))
        );
        assert_eq!(
            resolve_property(&PropQName::new(NS_NEXTCLOUD, "language"), &principal, &c),
            Some(PropValue::Text("fr".into()))
        );
        assert_eq!(
            resolve_property(&PropQName::new(NS_SABREDAV, "email-address"), &principal, &c),
            Some(PropValue::Text("alice@example.com".into()))
        );
        // group-membership is prefixed and slash-terminated like PHP.
        assert_eq!(
            resolve_property(&PropQName::dav("group-membership"), &principal, &c),
            Some(PropValue::Elements(vec![XmlElement::new("d:href")
                .text("/remote.php/dav/principals/groups/admin/")]))
        );
    }

    #[test]
    fn alternate_uri_set_includes_additional_and_primary() {
        let c = ctx();
        let addresses = alternate_uri_set(&c);
        assert_eq!(
            addresses,
            vec![
                "mailto:alt@example.com".to_string(),
                "mailto:alice@example.com".to_string()
            ]
        );
    }

    #[test]
    fn parses_additional_mail_collection() {
        let data = r#"{"email":{"value":"a@b.c","scope":"private"},"additional_mail":[{"value":"x@y.z","scope":"private"},{"value":"","scope":"private"}]}"#;
        assert_eq!(additional_mails_from_json(data), vec!["x@y.z".to_string()]);
        assert!(additional_mails_from_json("{}").is_empty());
        assert!(additional_mails_from_json("not json").is_empty());
    }

    #[test]
    fn allprop_is_resourcetype_only() {
        assert_eq!(default_props(), vec![PropQName::dav("resourcetype")]);
    }
}
