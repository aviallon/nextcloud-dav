// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The CalDAV read surface, part 1: the calendar home listing and the
//! per-calendar `PROPFIND`.
//!
//! Only `/remote.php/dav/calendars/<user>/` (`Depth: 0`/`1`) and
//! `/remote.php/dav/calendars/<user>/<cal>/` (`Depth: 0`) are served. The path
//! shape has **no `users/` segment**, unlike address books.
//!
//! Everything else in the tree — objects, `trashbin/`, subscriptions, federated
//! and app-generated calendars, `calendar-query`, `calendar-multiget`,
//! `sync-collection`, `expand`, free-busy, writes — answers `501` so nginx
//! replays the request to PHP.
//!
//! ## The property gate
//!
//! An explicit request whose property list contains a qname outside the
//! implemented/known-404 set is answered `501` (delegated), never `404`: a
//! `404` would claim a property PHP serves is absent. The implemented set and
//! the known-404 set were both captured from a live Nextcloud 33.0.5 instance
//! (`home-d0`/`home-d1`/`cal-d0`, 2026-09-20).
//!
//! ## The `oc_properties` override layer
//!
//! `CustomPropertiesBackend` runs on every request and overwrites
//! `displayname`, `calendar-description`, `calendar-timezone`,
//! `calendar-order`, `calendar-color`, `schedule-calendar-transp`,
//! `disable-alarm-notifications`, `calendar-enabled` and `enabled` from
//! `oc_properties`, keyed by `calendars/<requesting-user>/<wire-uri>`. This is
//! the mechanism a sharee's `PROPPATCH` survives on a shared calendar, so it is
//! reproduced here.
//!
//! ## The ctag / sync-token quirk
//!
//! Unlike CardDAV (raw integer `getctag`), the CalDAV backend builds
//! `{cs}getctag` as `http://sabre.io/ns/sync/<synctoken ?: '0'>` and
//! `{sabredav}sync-token` as the raw token; Sabre's `Sync\Plugin` prefixes the
//! latter into `{DAV:}sync-token`. Both are reproduced.

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::l10n::{DavL10n, LocalizedDisplayname};
use crate::model::{CalendarObject, VisibleCalendar};
use crate::sync::{self, CalendarSyncToken};
use crate::util::{encode_path_segment, http_date, percent_decode};
use crate::xml::parse::{self, PropList};
use crate::xml::write::{
    DavResponse, MultiStatus, PropQName, PropStat, PropValue, XmlElement, NS_CALDAV,
    NS_CALENDARSERVER, NS_DAV, NS_NEXTCLOUD, NS_OWNCLOUD, NS_SABREDAV,
};
use std::collections::HashMap;

/// `{http://apple.com/ns/ical/}` — the iCal calendar colour/order namespace.
pub const NS_APPLE: &str = "http://apple.com/ns/ical/";

/// `http://sabre.io/ns/sync/` (`Sabre\DAV\Sync\Plugin::SYNCTOKEN_PREFIX`).
const SYNCTOKEN_PREFIX: &str = "http://sabre.io/ns/sync/";

// The `oc_properties` override layer replaces these names (see
// `CustomPropertiesBackend::propFind`): `{DAV:}displayname`,
// `{caldav}calendar-description`, `{caldav}calendar-timezone`,
// `{apple}calendar-order`, `{apple}calendar-color`,
// `{caldav}schedule-calendar-transp`,
// `{nc}disable-alarm-notifications`, `{oc}calendar-enabled`, `{oc}enabled`.
// They are applied in `build_calendar_response`.

/// The `oc_calendars.components` CSV split into component names.
fn components_of(components: &Option<String>) -> Vec<String> {
    components
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| value.split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Path parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarsPath {
    /// `<webroot>/remote.php/dav` (no trailing slash), used to build hrefs.
    pub context: String,
    pub target: CalendarsTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarsTarget {
    Home { user: String, href: String },
    Calendar {
        user: String,
        cal_uri: String,
        href: String,
    },
    /// A path in the calendars tree the sidecar does not serve: an object, the
    /// trashbin, a subscription, or anything deeper. Delegated to PHP (501).
    Delegated,
    NotFound,
}

/// Parses `<webroot>/remote.php/dav/calendars/<u>[/<cal>[/<obj>]]`.
pub fn parse_calendars_path(path: &str) -> CalendarsPath {
    let marker = "/remote.php/dav";
    let Some(index) = path.find(marker) else {
        return CalendarsPath {
            context: String::new(),
            target: CalendarsTarget::NotFound,
        };
    };
    let context = path[..index + marker.len()].to_string();
    let rest = &path[index + marker.len()..];
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

    match segments.as_slice() {
        ["calendars", user] if !user.is_empty() => CalendarsPath {
            context: context.clone(),
            target: CalendarsTarget::Home {
                user: percent_decode(user),
                href: format!("{context}/calendars/{user}"),
            },
        },
        ["calendars", user, cal] if !user.is_empty() && !cal.is_empty() => CalendarsPath {
            context: context.clone(),
            target: CalendarsTarget::Calendar {
                user: percent_decode(user),
                cal_uri: percent_decode(cal),
                href: format!("{context}/calendars/{user}/{cal}"),
            },
        },
        ["calendars", ..] if segments.len() > 1 => CalendarsPath {
            context,
            target: CalendarsTarget::Delegated,
        },
        _ => CalendarsPath {
            context,
            target: CalendarsTarget::NotFound,
        },
    }
}

// ---------------------------------------------------------------------------
// Property gate
// ---------------------------------------------------------------------------

/// The implemented (`200`) properties of a **calendar** node.
fn is_implemented_calendar(ns: &str, local: &str) -> bool {
    matches!(
        (ns, local),
        (NS_DAV, "resourcetype")
            | (NS_DAV, "displayname")
            | (NS_DAV, "owner")
            | (NS_DAV, "current-user-principal")
            | (NS_DAV, "current-user-privilege-set")
            | (NS_DAV, "acl")
            | (NS_DAV, "supported-report-set")
            | (NS_DAV, "supported-method-set")
            | (NS_DAV, "sync-token")
            | (NS_CALDAV, "calendar-description")
            | (NS_CALDAV, "calendar-timezone")
            | (NS_CALDAV, "supported-calendar-component-set")
            | (NS_CALDAV, "schedule-calendar-transp")
            | (NS_CALDAV, "max-resource-size")
            | (NS_CALDAV, "supported-calendar-data")
            | (NS_CALDAV, "supported-collation-set")
            | (NS_CALENDARSERVER, "getctag")
            | (NS_CALENDARSERVER, "allowed-sharing-modes")
            | (NS_SABREDAV, "sync-token")
            | (NS_OWNCLOUD, "owner-principal")
            | (NS_OWNCLOUD, "read-only")
            | (NS_OWNCLOUD, "invite")
            | (NS_OWNCLOUD, "calendar-enabled")
            | (NS_OWNCLOUD, "enabled")
            | (NS_NEXTCLOUD, "owner-displayname")
            | (NS_NEXTCLOUD, "disable-alarm-notifications")
            | (NS_APPLE, "calendar-color")
            | (NS_APPLE, "calendar-order")
    )
}

/// Properties PHP answers with `404` on a calendar (and, for most, on the home
/// and the special children). Keeping them known means the gate delegates
/// neither a property PHP serves nor one it 404s.
fn is_known_404_calendar(ns: &str, local: &str) -> bool {
    matches!(
        (ns, local),
        (NS_DAV, "creationdate")
            | (NS_DAV, "getcontentlength")
            | (NS_DAV, "getcontenttype")
            | (NS_DAV, "getetag")
            | (NS_DAV, "getlastmodified")
            | (NS_DAV, "invite")
            | (NS_DAV, "principal-URL")
            | (NS_DAV, "quota-available-bytes")
            | (NS_DAV, "quota-used-bytes")
            | (NS_DAV, "share-access")
            | (NS_APPLE, "refreshrate")
            | (NS_CALENDARSERVER, "calendar-availability")
            | (NS_CALENDARSERVER, "source")
            | (NS_CALENDARSERVER, "subscribed-strip-alarms")
            | (NS_CALENDARSERVER, "subscribed-strip-attachments")
            | (NS_CALENDARSERVER, "subscribed-strip-todos")
            | (NS_NEXTCLOUD, "calendar-search")
            | (NS_NEXTCLOUD, "default-alarm-full-day")
            | (NS_NEXTCLOUD, "default-alarm-part-day")
            | (NS_NEXTCLOUD, "deleted-at")
            | (NS_NEXTCLOUD, "has-photo")
            | (NS_NEXTCLOUD, "trash-bin-retention-duration")
            | (NS_OWNCLOUD, "groups")
            | (NS_OWNCLOUD, "public")
            | (NS_SABREDAV, "email-address")
            | (NS_CALDAV, "calendar-availability")
            | (NS_CALDAV, "calendar-data")
            | (NS_CALDAV, "calendar-free-busy-set")
            | (NS_CALDAV, "calendar-home-set")
            | (NS_CALDAV, "calendar-user-address-set")
            | (NS_CALDAV, "calendar-user-type")
            | (NS_CALDAV, "max-attendees-per-instance")
            | (NS_CALDAV, "max-date-time")
            | (NS_CALDAV, "max-instances")
            | (NS_CALDAV, "min-date-time")
            | (NS_CALDAV, "schedule-default-calendar-URL")
            | (NS_CALDAV, "schedule-inbox-URL")
            | (NS_CALDAV, "schedule-outbox-URL")
    )
}

/// The implemented (`200`) properties of the **calendar home** node.
fn is_implemented_home(ns: &str, local: &str) -> bool {
    matches!(
        (ns, local),
        (NS_DAV, "resourcetype")
            | (NS_DAV, "owner")
            | (NS_DAV, "current-user-principal")
            | (NS_DAV, "current-user-privilege-set")
            | (NS_DAV, "acl")
            | (NS_DAV, "supported-report-set")
            | (NS_DAV, "supported-method-set")
    )
}

/// The implemented (`200`) properties of the special children (`inbox`,
/// `outbox`, `trashbin`).
fn is_implemented_special(ns: &str, local: &str) -> bool {
    matches!(
        (ns, local),
        (NS_DAV, "resourcetype")
            | (NS_DAV, "owner")
            | (NS_DAV, "current-user-principal")
            | (NS_DAV, "supported-report-set")
            | (NS_DAV, "supported-method-set")
            | (NS_CALDAV, "max-resource-size")
            | (NS_CALDAV, "supported-calendar-data")
            | (NS_CALDAV, "supported-collation-set")
            | (NS_NEXTCLOUD, "trash-bin-retention-duration")
    )
}

/// True when every explicitly requested property is one the sidecar either
/// serves or knows PHP 404s. Anything else is delegated with 501.
pub fn gate_ok(props: &PropList) -> bool {
    match props {
        PropList::AllProp | PropList::PropName => true,
        PropList::Props(props) => props.iter().all(|qname| {
            let ns = qname.ns.as_str();
            let local = qname.local.as_str();
            is_implemented_calendar(ns, local)
                || is_known_404_calendar(ns, local)
                || is_implemented_home(ns, local)
                || is_implemented_special(ns, local)
        }),
    }
}

fn wants(props: &PropList, ns: &str, local: &str) -> bool {
    match props {
        PropList::AllProp | PropList::PropName => false,
        PropList::Props(props) => props.iter().any(|q| q.ns == ns && q.local == local),
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct CalendarCtx {
    /// `<webroot>/remote.php/dav`.
    context: String,
    caller: String,
    caller_principal: String,
    caller_href: String,
    caller_displayname: String,
}

impl CalendarCtx {
    fn principal_href(&self, principal: &str) -> String {
        let name = principal.rsplit('/').next().unwrap_or_default();
        format!(
            "{}/principals/users/{}/",
            self.context,
            encode_path_segment(name)
        )
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Serves a calendars `PROPFIND`, or returns `None` to delegate (501).
pub async fn handle_propfind(
    db: &Db,
    config: &Config,
    caller: &str,
    parsed: &CalendarsPath,
    depth: i64,
    accept_language: Option<&str>,
    body: &[u8],
) -> Result<Option<MultiStatus>> {
    let request = parse::parse_propfind(body)?;
    if !gate_ok(&request.props) {
        return Ok(None);
    }

    let (target_user, href) = match &parsed.target {
        CalendarsTarget::Home { user, href } => (user, href),
        CalendarsTarget::Calendar { user, href, .. } => (user, href),
        CalendarsTarget::Delegated => return Ok(None),
        CalendarsTarget::NotFound => return Ok(None),
    };
    // Only the caller's own home is served; any other principal is PHP's.
    if target_user != caller {
        return Ok(None);
    }

    let caller_principal = format!("principals/users/{caller}");
    let caller_href = format!(
        "{}/principals/users/{}/",
        parsed.context,
        encode_path_segment(caller)
    );
    let caller_displayname = db
        .user_display_name(caller)
        .await?
        .unwrap_or_else(|| caller.to_string());
    let ctx = CalendarCtx {
        context: parsed.context.clone(),
        caller: caller.to_string(),
        caller_principal,
        caller_href,
        caller_displayname,
    };
    let l10n = DavL10n::resolve(db, config, caller, accept_language).await;

    match &parsed.target {
        CalendarsTarget::Home { .. } => {
            handle_home(db, config, &ctx, href, depth, &request.props, &l10n).await
        }
        CalendarsTarget::Calendar { cal_uri, .. } => {
            handle_calendar(db, config, &ctx, href, cal_uri, depth, &request.props, &l10n).await
        }
        CalendarsTarget::Delegated | CalendarsTarget::NotFound => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Home
// ---------------------------------------------------------------------------

async fn handle_home(
    db: &Db,
    _config: &Config,
    ctx: &CalendarCtx,
    href: &str,
    depth: i64,
    props: &PropList,
    l10n: &DavL10n,
) -> Result<Option<MultiStatus>> {
    // Depth: 0 is only the home node; its property set does not depend on the
    // children, so the child guards below are not needed.
    if depth < 1 {
        let response = build_home_response(&collection_href(href), ctx, props);
        return Ok(Some(MultiStatus {
            responses: vec![response],
            sync_token: None,
        }));
    }

    // The trashbin/subscription/federated guards: PHP's listing returns trashed
    // calendars (with a `deleted-calendar` resourcetype) and subscription /
    // federated children the subset model does not reproduce. Prefer a clean
    // 501 over an approximate listing.
    let groups = db.group_principals(&ctx.caller).await?;
    if db
        .has_trashed_calendars(&ctx.caller_principal, &groups)
        .await?
        || db.has_calendar_subscriptions(&ctx.caller_principal).await?
        || db.has_federated_calendars(&ctx.caller_principal).await?
    {
        return Ok(None);
    }

    // The special children carry `acl`/`current-user-privilege-set` values the
    // sidecar does not model; delegate rather than emit a wrong 404.
    if wants(props, NS_DAV, "acl") || wants(props, NS_DAV, "current-user-privilege-set") {
        return Ok(None);
    }

    let mut calendars = db
        .visible_calendars(&ctx.caller_principal, &groups)
        .await?;
    for calendar in &mut calendars {
        match l10n.localize_displayname(&calendar.wire_uri, calendar.wire_displayname.take()) {
            LocalizedDisplayname::Value(name) => calendar.wire_displayname = name,
            // The stored name would be translated but the `dav` l10n tree could
            // not be read; serving the English string would be a silent wrong
            // answer, so delegate the whole listing to PHP.
            LocalizedDisplayname::Unresolved => return Ok(None),
        }
    }

    // One bulk `oc_properties` lookup for every child calendar path, exactly
    // like `CustomPropertiesBackend::cacheCalendars()`.
    let paths: Vec<String> = calendars
        .iter()
        .map(|calendar| property_path(ctx, &calendar.wire_uri))
        .collect();
    let overrides = db
        .user_properties_for_paths(&ctx.caller, &paths)
        .await?;

    // `{oc}invite` is non-empty for an owned calendar with outgoing shares,
    // which the sidecar does not reproduce.
    if wants(props, NS_OWNCLOUD, "invite") && has_outgoing_shares(db, &ctx.caller_principal).await? {
        return Ok(None);
    }
    let mut responses = vec![build_home_response(&collection_href(href), ctx, props)];
    for calendar in &calendars {
        let cal_href = format!("{href}/{}", encode_path_segment(&calendar.wire_uri));
        let empty = HashMap::new();
        let override_props = overrides
            .get(&property_path(ctx, &calendar.wire_uri))
            .unwrap_or(&empty);
        responses.push(build_calendar_response(
            &collection_href(&cal_href),
            ctx,
            calendar,
            override_props,
            props,
            true,
        ));
    }
    responses.push(build_special_response(
        &collection_href(&format!("{href}/inbox")),
        SpecialKind::Inbox,
        ctx,
        props,
    ));
    responses.push(build_special_response(
        &collection_href(&format!("{href}/outbox")),
        SpecialKind::Outbox,
        ctx,
        props,
    ));
    responses.push(build_special_response(
        &collection_href(&format!("{href}/trashbin")),
        SpecialKind::Trashbin,
        ctx,
        props,
    ));

    Ok(Some(MultiStatus {
        responses,
        sync_token: None,
    }))
}

async fn has_outgoing_shares(db: &Db, principal: &str) -> Result<bool> {
    db.calendar_has_outgoing_shares(principal).await
}

// ---------------------------------------------------------------------------
// Calendar (Depth 0)
// ---------------------------------------------------------------------------

async fn handle_calendar(
    db: &Db,
    _config: &Config,
    ctx: &CalendarCtx,
    href: &str,
    cal_uri: &str,
    _depth: i64,
    props: &PropList,
    l10n: &DavL10n,
) -> Result<Option<MultiStatus>> {
    // The special children and anything the sidecar does not model are PHP's.
    if matches!(cal_uri, "inbox" | "outbox" | "trashbin" | "notifications") {
        return Ok(None);
    }
    let groups = db.group_principals(&ctx.caller).await?;
    let Some(mut calendar) = db
        .visible_calendar_by_uri(&ctx.caller_principal, &groups, cal_uri)
        .await?
    else {
        return Ok(None);
    };
    calendar.wire_displayname =
        match l10n.localize_displayname(&calendar.wire_uri, calendar.wire_displayname.take()) {
            LocalizedDisplayname::Value(name) => name,
            // See `handle_home`: an unresolvable translation delegates.
            LocalizedDisplayname::Unresolved => return Ok(None),
        };

    // A trashed calendar carries a `{nc}deleted-calendar` resourcetype the
    // subset model does not reproduce; delegate it.
    if calendar.calendar.deleted_at.is_some() {
        return Ok(None);
    }

    // A calendar reached at Depth 0 through `getCalendarByUri` (owned) does not
    // carry `{oc}owner-principal`; a shared calendar falls back to
    // `getCalendarsForUser` and does. This is the live 33.0.5 behaviour.
    let depth_one = calendar.owner_principal.is_some();

    // The shared ACL is only reproduced for a direct user share; a group or
    // circle share adds ACEs the sidecar does not model.
    if wants(props, NS_DAV, "acl") {
        if let Some(share_principal) = &calendar.share_principal {
            if share_principal != &ctx.caller_principal {
                return Ok(None);
            }
        }
    }

    // `{oc}invite` is only non-empty for an owned calendar with outgoing
    // shares, which the sidecar does not reproduce.
    if wants(props, NS_OWNCLOUD, "invite")
        && calendar.owner_principal.is_none()
        && db
            .calendar_id_has_outgoing_shares(calendar.calendar.id)
            .await?
    {
        return Ok(None);
    }

    let path = property_path(ctx, &calendar.wire_uri);
    let overrides = db
        .user_properties_for_paths(&ctx.caller, std::slice::from_ref(&path))
        .await?;
    let empty = HashMap::new();
    let override_props = overrides.get(&path).unwrap_or(&empty);

    let response = build_calendar_response(
        &collection_href(href),
        ctx,
        &calendar,
        override_props,
        props,
        depth_one,
    );
    Ok(Some(MultiStatus {
        responses: vec![response],
        sync_token: None,
    }))
}

// ---------------------------------------------------------------------------
// REPORT (`sync-collection`, `calendar-multiget`)
// ---------------------------------------------------------------------------

/// The outcome of a CalDAV REPORT.
pub enum ReportOutcome {
    /// A native multistatus the sidecar built.
    Multistatus(MultiStatus),
    /// Hand the request back to PHP (nginx replays it).
    Delegated,
    /// `Sabre\DAV\Exception\InvalidSyncToken` (403 + `valid-sync-token`).
    InvalidSyncToken,
    /// `OCA\DAV\Exception\UnsupportedLimitOnInitialSyncException` (507).
    UnsupportedInitialLimit,
    /// `Sabre\DAV\Exception\BadRequest` (400) with a specific message.
    BadRequest(String),
}

/// The implemented (`200`) properties of a **calendar object**.
fn is_implemented_object(ns: &str, local: &str) -> bool {
    matches!(
        (ns, local),
        (NS_DAV, "getetag")
            | (NS_DAV, "getcontentlength")
            | (NS_DAV, "getcontenttype")
            | (NS_DAV, "getlastmodified")
            | (NS_DAV, "resourcetype")
            | (NS_CALDAV, "calendar-data")
    )
}

/// Properties PHP answers with `404` on a calendar object (captured live from
/// Nextcloud 33.0.5). `{caldav}schedule-tag` matters because DAVx5 asks for it
/// in every `calendar-multiget`.
fn is_known_404_object(ns: &str, local: &str) -> bool {
    matches!(
        (ns, local),
        (NS_DAV, "creationdate")
            | (NS_DAV, "displayname")
            | (NS_DAV, "getcontentlanguage")
            | (NS_DAV, "quota-available-bytes")
            | (NS_DAV, "quota-used-bytes")
            | (NS_DAV, "share-access")
            | (NS_CALENDARSERVER, "getctag")
            | (NS_CALDAV, "schedule-tag")
            | (NS_NEXTCLOUD, "deleted-at")
            | (NS_OWNCLOUD, "size")
    )
}

/// True when every explicitly requested object property is one the sidecar
/// serves or knows PHP 404s. An empty list delegates (PHP would run `allprop`).
fn object_gate_ok(props: &[PropQName]) -> bool {
    !props.is_empty()
        && props.iter().all(|qname| {
            is_implemented_object(&qname.ns, &qname.local)
                || is_known_404_object(&qname.ns, &qname.local)
        })
}

/// Serves a CalDAV REPORT on one owned calendar, or delegates.
///
/// Shared calendars, trashed calendars, subscriptions, the calendar home,
/// objects and every other REPORT type are delegated rather than answered from
/// a subset. `calendar-query` and `free-busy-query` are therefore 501 too.
pub async fn handle_report(
    db: &Db,
    caller: &str,
    parsed: &CalendarsPath,
    body: &[u8],
) -> Result<ReportOutcome> {
    if body.is_empty() {
        return Ok(ReportOutcome::Delegated);
    }
    let Ok(document) = parse::parse_document(body) else {
        return Ok(ReportOutcome::Delegated);
    };
    let is_multiget = document.ns == NS_CALDAV && document.local == "calendar-multiget";
    let is_sync = document.ns == NS_DAV && document.local == "sync-collection";
    if !is_multiget && !is_sync {
        return Ok(ReportOutcome::Delegated);
    }

    let CalendarsTarget::Calendar { user, cal_uri, href } = &parsed.target else {
        return Ok(ReportOutcome::Delegated);
    };
    if user != caller {
        return Ok(ReportOutcome::Delegated);
    }
    let caller_principal = format!("principals/users/{caller}");
    let groups = db.group_principals(caller).await?;
    let Some(calendar) = db
        .visible_calendar_by_uri(&caller_principal, &groups, cal_uri)
        .await?
    else {
        return Ok(ReportOutcome::Delegated);
    };
    // A shared or trashed calendar, or a subscription (which never resolves
    // here), is answered from a subset the sidecar does not model.
    if calendar.owner_principal.is_some() || calendar.calendar.deleted_at.is_some() {
        return Ok(ReportOutcome::Delegated);
    }
    let href = collection_href(href);

    if is_multiget {
        handle_calendar_multiget(db, &calendar, &href, body).await
    } else {
        handle_calendar_sync(db, &calendar, &href, body).await
    }
}

async fn handle_calendar_multiget(
    db: &Db,
    calendar: &VisibleCalendar,
    href: &str,
    body: &[u8],
) -> Result<ReportOutcome> {
    let request = parse::parse_calendar_multiget(body)?;
    if !object_gate_ok(&request.props) {
        return Ok(ReportOutcome::Delegated);
    }
    // `expand` re-serialises (and breaks the ETag); `application/calendar+json`
    // needs `VObject::jsonSerialize()`. Both delegate.
    if request.calendar_data.expand
        || request.calendar_data.content_type.as_deref() == Some("application/calendar+json")
    {
        return Ok(ReportOutcome::Delegated);
    }

    // PHP groups the hrefs by parent and asks that collection's
    // `getMultipleChildren()`; an href outside this calendar would answer from
    // a different collection, so delegate it.
    let collection = href.trim_end_matches('/');
    let mut uris = Vec::with_capacity(request.hrefs.len());
    for raw in &request.hrefs {
        let normalized = normalize_report_href(raw, href);
        let trimmed = normalized.trim_end_matches('/');
        let Some((parent, segment)) = trimmed.rsplit_once('/') else {
            return Ok(ReportOutcome::Delegated);
        };
        if parent != collection {
            return Ok(ReportOutcome::Delegated);
        }
        uris.push(percent_decode(segment));
    }

    let objects = db
        .calendar_objects_by_uris(calendar.calendar.id, &uris)
        .await?;
    let responses = objects
        .iter()
        .map(|object| {
            let object_href = format!("{href}{}", encode_path_segment(&object.uri));
            build_object_response(&object_href, object, &request.props)
        })
        .collect();
    Ok(ReportOutcome::Multistatus(MultiStatus {
        responses,
        sync_token: None,
    }))
}

async fn handle_calendar_sync(
    db: &Db,
    calendar: &VisibleCalendar,
    href: &str,
    body: &[u8],
) -> Result<ReportOutcome> {
    let request = parse::parse_sync_collection(body)?;
    // `Sabre\DAV\Xml\Request\SyncCollectionReport::xmlDeserialize()` requires
    // both elements, in this order.
    if !request.has_sync_token {
        return Ok(ReportOutcome::BadRequest(
            "The {DAV:}sync-token element in the {DAV:}sync-collection report is required"
                .to_string(),
        ));
    }
    if !request.has_prop {
        return Ok(ReportOutcome::BadRequest(
            "The {DAV:}prop element in the {DAV:}sync-collection report is required".to_string(),
        ));
    }
    if !object_gate_ok(&request.props) {
        return Ok(ReportOutcome::Delegated);
    }

    let token = sync::parse_calendar_sync_token(request.sync_token.as_deref())?;
    let current = calendar.calendar.synctoken;
    let mut changed: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    match token {
        CalendarSyncToken::EmptyInitial => {
            // `Calendar::getChanges()` throws only when `$limit` is truthy, so
            // `<d:nresults>0</d:nresults>` is an empty initial sync, not a 507.
            if request.limit.is_some_and(|limit| limit != 0) {
                return Ok(ReportOutcome::UnsupportedInitialLimit);
            }
            let rows = db
                .calendar_objects_for_sync(calendar.calendar.id, request.limit)
                .await?;
            changed = rows.into_iter().map(|row| row.uri).collect();
        }
        CalendarSyncToken::NonNumericInitial => {
            // PHP applies the limit to the initial query in this case.
            let rows = db
                .calendar_objects_for_sync(calendar.calendar.id, request.limit)
                .await?;
            changed = rows.into_iter().map(|row| row.uri).collect();
        }
        CalendarSyncToken::Changes(from) => {
            let rows = db
                .calendar_changes(calendar.calendar.id, from, current, request.limit)
                .await?;
            for row in rows {
                match row.operation {
                    1 | 2 => changed.push(row.uri),
                    3 => deleted.push(row.uri),
                    _ => {}
                }
            }
        }
        CalendarSyncToken::NumericRaw(raw) => {
            // PHP's `is_numeric()` accepted it, so this is an *incremental*
            // query; the raw string is parsed by PostgreSQL, exactly like
            // PHP's own `setMaxResults()`/`synctoken >= ?` query.
            let rows = db
                .calendar_changes_raw(calendar.calendar.id, &raw, current, request.limit)
                .await?;
            for row in rows {
                match row.operation {
                    1 | 2 => changed.push(row.uri),
                    3 => deleted.push(row.uri),
                    _ => {}
                }
            }
        }
    }

    // Added and modified URIs are fetched in one `getMultipleCalendarObjects`
    // call (DB order); URIs that no longer exist are silently dropped, exactly
    // like `Tree::getMultipleNodes()`. Deleted URIs are 404 responses.
    let objects = db
        .calendar_objects_by_uris(calendar.calendar.id, &changed)
        .await?;
    let mut responses: Vec<DavResponse> = objects
        .iter()
        .map(|object| {
            let object_href = format!("{href}{}", encode_path_segment(&object.uri));
            build_object_response(&object_href, object, &request.props)
        })
        .collect();
    for uri in &deleted {
        responses.push(DavResponse::status(
            format!("{href}{}", encode_path_segment(uri)),
            404,
        ));
    }

    Ok(ReportOutcome::Multistatus(MultiStatus {
        responses,
        sync_token: Some(format!("{SYNCTOKEN_PREFIX}{current}")),
    }))
}

/// Resolves a client-provided REPORT href to an absolute path, like
/// `Sabre\DAV\Server::calculateUri()` plus the base-URI prefix.
fn normalize_report_href(raw: &str, collection_href: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        if let Ok(url) = reqwest::Url::parse(raw) {
            return url.path().to_string();
        }
    }
    if raw.starts_with('/') {
        return raw.to_string();
    }
    format!("{collection_href}{}", raw.trim_start_matches('/'))
}

fn object_content_type(object: &CalendarObject) -> String {
    // `Sabre\CalDAV\CalendarObject::getContentType()` appends the component
    // only when it is non-empty (`strtolower(null) === ''`).
    match object.componenttype.as_deref() {
        Some(component) if !component.is_empty() => {
            format!("text/calendar; charset=utf-8; component={}", component.to_lowercase())
        }
        _ => "text/calendar; charset=utf-8".to_string(),
    }
}

fn resolve_object_property(qname: &PropQName, object: &CalendarObject) -> Option<PropValue> {
    match (qname.ns.as_str(), qname.local.as_str()) {
        (NS_DAV, "getetag") => Some(PropValue::Text(object.quoted_etag())),
        (NS_DAV, "getcontentlength") => Some(PropValue::Text(object.size.to_string())),
        (NS_DAV, "getcontenttype") => Some(PropValue::Text(object_content_type(object))),
        (NS_DAV, "getlastmodified") => {
            object.lastmodified.map(|ts| PropValue::Text(http_date(ts)))
        }
        (NS_DAV, "resourcetype") => Some(PropValue::Empty),
        // `Sabre\CalDAV\Plugin::propFind()` strips every `\r` from the
        // calendar-data value ("Taking out \r to not screw up the xml
        // output"), so the wire body is LF-only even though the stored blob and
        // the ETag are over the CRLF bytes.
        (NS_CALDAV, "calendar-data") => Some(PropValue::Text(
            String::from_utf8_lossy(&object.calendardata).replace('\r', ""),
        )),
        _ => None,
    }
}

fn build_object_response(
    href: &str,
    object: &CalendarObject,
    props: &[PropQName],
) -> DavResponse {
    let mut found: Vec<(PropQName, PropValue)> = Vec::new();
    let mut missing: Vec<PropQName> = Vec::new();
    for qname in props {
        match resolve_object_property(qname, object) {
            Some(value) => found.push((qname.clone(), value)),
            None => missing.push(qname.clone()),
        }
    }
    let mut propstats = Vec::new();
    if !found.is_empty() {
        propstats.push(PropStat::ok(found));
    }
    if !missing.is_empty() {
        propstats.push(PropStat::not_found(missing));
    }
    DavResponse::props(href.to_string(), propstats)
}

// ---------------------------------------------------------------------------
// Response building
// ---------------------------------------------------------------------------

fn collection_href(href: &str) -> String {
    if href.ends_with('/') {
        href.to_string()
    } else {
        format!("{href}/")
    }
}

fn property_path(ctx: &CalendarCtx, wire_uri: &str) -> String {
    Db::property_path(&format!("calendars/{}/{}", ctx.caller, wire_uri))
}

fn requested_props(props: &PropList, node: NodeKind) -> Vec<PropQName> {
    match props {
        PropList::AllProp | PropList::PropName => default_props(node),
        PropList::Props(props) => props.clone(),
    }
}

#[derive(Clone, Copy)]
enum NodeKind {
    Home,
    Calendar,
    Special,
}

/// Sabre's `allprop` result for these nodes: only `{DAV:}resourcetype` (the
/// DAVACL/CalDAV handlers only run for explicit requests).
fn default_props(_node: NodeKind) -> Vec<PropQName> {
    vec![PropQName::dav("resourcetype")]
}

fn assemble(
    href: &str,
    node: NodeKind,
    props: &PropList,
    mut resolve: impl FnMut(&PropQName) -> Option<PropValue>,
) -> DavResponse {
    let requested = requested_props(props, node);
    let propname_only = matches!(props, PropList::PropName);
    let mut found: Vec<(PropQName, PropValue)> = Vec::new();
    let mut missing: Vec<PropQName> = Vec::new();
    for qname in &requested {
        match resolve(qname) {
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
    // `allprop`/`propname` strip the 404 propstat, like Sabre.
    if !missing.is_empty() && matches!(props, PropList::Props(_)) {
        propstats.push(PropStat::not_found(missing));
    }
    DavResponse::props(href.to_string(), propstats)
}

fn build_home_response(href: &str, ctx: &CalendarCtx, props: &PropList) -> DavResponse {
    assemble(href, NodeKind::Home, props, |qname| {
        let ns = qname.ns.as_str();
        let local = qname.local.as_str();
        match (ns, local) {
            (NS_DAV, "resourcetype") => {
                Some(PropValue::Elements(vec![XmlElement::new("d:collection")]))
            }
            (NS_DAV, "owner") | (NS_DAV, "current-user-principal") => {
                Some(href_prop(&ctx.caller_href))
            }
            (NS_DAV, "current-user-privilege-set") => Some(privilege_set(&[
                "d:write",
                "d:write-properties",
                "d:write-content",
                "d:unlock",
                "d:bind",
                "d:unbind",
                "d:write-acl",
                "d:read",
                "d:read-acl",
                "d:read-current-user-privilege-set",
            ])),
            (NS_DAV, "acl") => Some(home_acl(&ctx.caller_href)),
            (NS_DAV, "supported-report-set") => Some(report_set(&[
                "d:expand-property",
                "d:principal-match",
                "d:principal-property-search",
                "d:principal-search-property-set",
                "d:sync-collection",
                "oc:filter-comments",
                "nc:calendar-search",
                "oc:filter-files",
            ])),
            (NS_DAV, "supported-method-set") => Some(method_set(false)),
            _ => None,
        }
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpecialKind {
    Inbox,
    Outbox,
    Trashbin,
}

fn build_special_response(
    href: &str,
    kind: SpecialKind,
    ctx: &CalendarCtx,
    props: &PropList,
) -> DavResponse {
    assemble(href, NodeKind::Special, props, |qname| {
        let ns = qname.ns.as_str();
        let local = qname.local.as_str();
        match (ns, local) {
            (NS_DAV, "resourcetype") => Some(PropValue::Elements(match kind {
                SpecialKind::Inbox => vec![
                    XmlElement::new("d:collection"),
                    XmlElement::new("cal:schedule-inbox"),
                ],
                SpecialKind::Outbox => vec![
                    XmlElement::new("d:collection"),
                    XmlElement::new("cal:schedule-outbox"),
                ],
                SpecialKind::Trashbin => vec![
                    XmlElement::new("d:collection"),
                    XmlElement::new("nc:trash-bin"),
                ],
            })),
            (NS_DAV, "owner") | (NS_DAV, "current-user-principal") => {
                Some(href_prop(&ctx.caller_href))
            }
            (NS_DAV, "supported-report-set") => Some(report_set(match kind {
                SpecialKind::Inbox => &[
                    "d:expand-property",
                    "d:principal-match",
                    "d:principal-property-search",
                    "d:principal-search-property-set",
                    "cal:calendar-multiget",
                    "cal:calendar-query",
                    "oc:filter-comments",
                    "oc:filter-files",
                ][..],
                SpecialKind::Outbox | SpecialKind::Trashbin => &[
                    "d:expand-property",
                    "d:principal-match",
                    "d:principal-property-search",
                    "d:principal-search-property-set",
                    "oc:filter-comments",
                    "oc:filter-files",
                ][..],
            })),
            (NS_DAV, "supported-method-set") => {
                Some(method_set(kind == SpecialKind::Outbox))
            }
            // The inbox is an `ICalendarObjectContainer`, so Sabre's CalDAV
            // plugin serves these three; the outbox and trashbin are not.
            (NS_CALDAV, "max-resource-size") if kind == SpecialKind::Inbox => {
                Some(PropValue::Text("10000000".to_string()))
            }
            (NS_CALDAV, "supported-calendar-data") if kind == SpecialKind::Inbox => {
                Some(PropValue::Elements(vec![
                    XmlElement::new("cal:calendar-data")
                        .attr("content-type", "text/calendar")
                        .attr("version", "2.0"),
                    XmlElement::new("cal:calendar-data")
                        .attr("content-type", "application/calendar+json"),
                ]))
            }
            (NS_CALDAV, "supported-collation-set") if kind == SpecialKind::Inbox => {
                Some(PropValue::Elements(vec![
                    XmlElement::new("cal:supported-collation").text("i;ascii-casemap"),
                    XmlElement::new("cal:supported-collation").text("i;octet"),
                    XmlElement::new("cal:supported-collation").text("i;unicode-casemap"),
                ]))
            }
            (NS_NEXTCLOUD, "trash-bin-retention-duration") if kind == SpecialKind::Trashbin => {
                // `RetentionService::getDuration()`: the `dav` app config with a
                // 30-day default.
                Some(PropValue::Text("2592000".to_string()))
            }
            _ => None,
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn build_calendar_response(
    href: &str,
    ctx: &CalendarCtx,
    calendar: &VisibleCalendar,
    overrides: &HashMap<String, String>,
    props: &PropList,
    depth_one: bool,
) -> DavResponse {
    let owner_principal = calendar
        .owner_principal
        .clone()
        .unwrap_or_else(|| ctx.caller_principal.clone());
    let owner_href = ctx.principal_href(&owner_principal);
    let owner_displayname = if calendar.owner_principal.is_some() {
        calendar.owner_displayname.clone()
    } else {
        ctx.caller_displayname.clone()
    };
    let is_birthday = calendar.calendar.uri == "contact_birthdays";
    let read_only = calendar.read_only;
    let can_write = !is_birthday && !read_only;
    let token = calendar.calendar.synctoken;

    assemble(href, NodeKind::Calendar, props, |qname| {
        let ns = qname.ns.as_str();
        let local = qname.local.as_str();
        match (ns, local) {
            (NS_DAV, "resourcetype") => Some(PropValue::Elements(vec![
                XmlElement::new("d:collection"),
                XmlElement::new("cal:calendar"),
            ])),
            (NS_DAV, "displayname") => overrides
                .get("{DAV:}displayname")
                .cloned()
                .map(PropValue::Text)
                .or_else(|| calendar.wire_displayname.clone().map(PropValue::Text)),
            (NS_DAV, "owner") => Some(href_prop(&owner_href)),
            (NS_DAV, "current-user-principal") => Some(href_prop(&ctx.caller_href)),
            (NS_DAV, "current-user-privilege-set") => {
                Some(privilege_set(calendar_privileges(is_birthday, read_only)))
            }
            (NS_DAV, "acl") => Some(calendar_acl(
                &owner_href,
                &ctx.caller_href,
                is_birthday,
                calendar.owner_principal.is_some(),
                read_only,
            )),
            (NS_DAV, "supported-report-set") => Some(report_set(&[
                "d:sync-collection",
                "d:expand-property",
                "d:principal-match",
                "d:principal-property-search",
                "d:principal-search-property-set",
                "cal:calendar-multiget",
                "cal:calendar-query",
                "cal:free-busy-query",
                "oc:filter-comments",
                "oc:filter-files",
            ])),
            (NS_DAV, "supported-method-set") => Some(method_set(false)),
            (NS_DAV, "sync-token") => Some(PropValue::Text(format!("{SYNCTOKEN_PREFIX}{token}"))),
            (NS_CALDAV, "calendar-description") => overrides
                .get("{urn:ietf:params:xml:ns:caldav}calendar-description")
                .cloned()
                .map(PropValue::Text)
                .or_else(|| calendar.calendar.description.clone().map(PropValue::Text)),
            (NS_CALDAV, "calendar-timezone") => overrides
                .get("{urn:ietf:params:xml:ns:caldav}calendar-timezone")
                .cloned()
                .map(PropValue::Text)
                .or_else(|| calendar.calendar.timezone.clone().map(PropValue::Text)),
            (NS_CALDAV, "supported-calendar-component-set") => {
                Some(PropValue::Elements(
                    components_of(&calendar.calendar.components)
                        .into_iter()
                        .map(|comp| XmlElement::new("cal:comp").attr("name", comp))
                        .collect(),
                ))
            }
            (NS_CALDAV, "schedule-calendar-transp") => {
                let transparent = overrides
                    .get("{urn:ietf:params:xml:ns:caldav}schedule-calendar-transp")
                    .map(|value| value.contains("transparent"))
                    .unwrap_or(calendar.transparent);
                Some(PropValue::Elements(vec![XmlElement::new(if transparent {
                    "cal:transparent"
                } else {
                    "cal:opaque"
                })]))
            }
            (NS_CALDAV, "max-resource-size") => Some(PropValue::Text("10000000".to_string())),
            (NS_CALDAV, "supported-calendar-data") => Some(PropValue::Elements(vec![
                XmlElement::new("cal:calendar-data")
                    .attr("content-type", "text/calendar")
                    .attr("version", "2.0"),
                XmlElement::new("cal:calendar-data").attr("content-type", "application/calendar+json"),
            ])),
            (NS_CALDAV, "supported-collation-set") => Some(PropValue::Elements(vec![
                XmlElement::new("cal:supported-collation").text("i;ascii-casemap"),
                XmlElement::new("cal:supported-collation").text("i;octet"),
                XmlElement::new("cal:supported-collation").text("i;unicode-casemap"),
            ])),
            (NS_CALENDARSERVER, "getctag") => {
                Some(PropValue::Text(format!("{SYNCTOKEN_PREFIX}{token}")))
            }
            (NS_CALENDARSERVER, "allowed-sharing-modes") => Some(if can_write {
                PropValue::Elements(vec![
                    XmlElement::new("cs:can-be-shared"),
                    XmlElement::new("cs:can-be-published"),
                ])
            } else {
                PropValue::Empty
            }),
            (NS_SABREDAV, "sync-token") => Some(PropValue::Text(token.to_string())),
            (NS_OWNCLOUD, "owner-principal") => {
                // `getCalendarByUri()` (owned Depth 0) does not set it.
                if depth_one || calendar.owner_principal.is_some() {
                    Some(PropValue::Text(owner_principal.clone()))
                } else {
                    None
                }
            }
            (NS_OWNCLOUD, "read-only") => calendar.owner_principal.as_ref().map(|_| {
                PropValue::Text(if read_only { "1".to_string() } else { String::new() })
            }),
            (NS_OWNCLOUD, "invite") => Some(PropValue::Empty),
            (NS_OWNCLOUD, "calendar-enabled") | (NS_OWNCLOUD, "enabled") => {
                overrides.get(&format!("{{{ns}}}{local}")).cloned().map(PropValue::Text)
            }
            (NS_NEXTCLOUD, "owner-displayname") => Some(PropValue::Text(owner_displayname.clone())),
            (NS_NEXTCLOUD, "disable-alarm-notifications") => overrides
                .get("{http://nextcloud.com/ns}disable-alarm-notifications")
                .cloned()
                .map(PropValue::Text),
            (NS_APPLE, "calendar-color") => overrides
                .get("{http://apple.com/ns/ical/}calendar-color")
                .cloned()
                .map(PropValue::Text)
                .or_else(|| calendar.calendar.calendarcolor.clone().map(PropValue::Text)),
            (NS_APPLE, "calendar-order") => overrides
                .get("{http://apple.com/ns/ical/}calendar-order")
                .cloned()
                .map(PropValue::Text)
                .or_else(|| Some(PropValue::Text(calendar.calendar.calendarorder.to_string()))),
            _ => None,
        }
    })
}

// ---------------------------------------------------------------------------
// Property value helpers
// ---------------------------------------------------------------------------

fn href_prop(href: &str) -> PropValue {
    PropValue::Elements(vec![XmlElement::new("d:href").text(href.to_string())])
}

fn report_set(reports: &[&str]) -> PropValue {
    PropValue::Elements(
        reports
            .iter()
            .map(|name| {
                XmlElement::new("d:supported-report")
                    .child(XmlElement::new("d:report").child(XmlElement::new(*name)))
            })
            .collect(),
    )
}

fn method_set(allow_post: bool) -> PropValue {
    let mut methods = vec![
        "OPTIONS",
        "GET",
        "HEAD",
        "DELETE",
        "PROPFIND",
        "PUT",
        "PROPPATCH",
        "COPY",
        "MOVE",
        "REPORT",
    ];
    if allow_post {
        methods.push("POST");
    }
    PropValue::Elements(
        methods
            .into_iter()
            .map(|name| XmlElement::new("d:supported-method").attr("name", name))
            .collect(),
    )
}

fn privilege_set(privileges: &[&str]) -> PropValue {
    PropValue::Elements(
        privileges
            .iter()
            .map(|name| XmlElement::new("d:privilege").child(XmlElement::new(*name)))
            .collect(),
    )
}

fn calendar_privileges(is_birthday: bool, read_only: bool) -> &'static [&'static str] {
    if is_birthday || read_only {
        &[
            "d:write-properties",
            "d:read",
            "d:read-acl",
            "d:read-current-user-privilege-set",
            "cal:read-free-busy",
        ]
    } else {
        &[
            "d:write",
            "d:write-properties",
            "d:write-content",
            "d:unlock",
            "d:bind",
            "d:unbind",
            "d:write-acl",
            "d:read",
            "d:read-acl",
            "d:read-current-user-privilege-set",
            "cal:read-free-busy",
        ]
    }
}

fn ace(principal_href: &str, privilege: &str) -> XmlElement {
    XmlElement::new("d:ace")
        .child(
            XmlElement::new("d:principal")
                .child(XmlElement::new("d:href").text(principal_href.to_string())),
        )
        .child(
            XmlElement::new("d:grant")
                .child(XmlElement::new("d:privilege").child(XmlElement::new(privilege))),
        )
        .child(XmlElement::new("d:protected"))
}

fn home_acl(caller_href: &str) -> PropValue {
    let proxy_write = format!("{caller_href}calendar-proxy-write/");
    let proxy_read = format!("{caller_href}calendar-proxy-read/");
    PropValue::Elements(vec![
        ace(caller_href, "d:read"),
        ace(caller_href, "d:write"),
        ace(&proxy_write, "d:read"),
        ace(&proxy_write, "d:write"),
        ace(&proxy_read, "d:read"),
    ])
}

/// `Calendar::getACL()` for the owned and direct-user-shared cases (live
/// 33.0.5 capture). Group/circle shares are handled by delegation before this
/// point.
fn calendar_acl(
    owner_href: &str,
    caller_href: &str,
    is_birthday: bool,
    is_shared: bool,
    read_only: bool,
) -> PropValue {
    let owner_write = if is_birthday {
        "d:write-properties"
    } else {
        "d:write"
    };
    let proxy_write = format!("{owner_href}calendar-proxy-write/");
    let proxy_read = format!("{owner_href}calendar-proxy-read/");
    let mut aces = vec![
        ace(owner_href, "d:read"),
        ace(&proxy_write, "d:read"),
        ace(&proxy_read, "d:read"),
        ace(owner_href, owner_write),
        ace(&proxy_write, owner_write),
        ace(&proxy_read, "d:write-properties"),
    ];
    if is_shared {
        // `applyShareAcl()` on top of the base ACL, direct user share.
        let share_privilege = if read_only { "d:write-properties" } else { "d:write" };
        aces.push(ace(caller_href, "d:read"));
        aces.push(ace(caller_href, share_privilege));
        aces.push(ace(caller_href, "d:read"));
        aces.push(ace(caller_href, share_privilege));
    }
    PropValue::Elements(aces)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Calendar;

    fn test_calendar(uri: &str, shared: bool) -> VisibleCalendar {
        let row = Calendar {
            id: 1,
            uri: uri.to_string(),
            displayname: Some("Work".to_string()),
            principaluri: "principals/users/alice".to_string(),
            description: None,
            timezone: None,
            calendarorder: 3,
            calendarcolor: Some("#123456".to_string()),
            components: Some("VEVENT".to_string()),
            transparent: false,
            synctoken: 7,
            deleted_at: None,
        };
        if shared {
            VisibleCalendar {
                calendar: Calendar {
                    principaluri: "principals/users/bob".to_string(),
                    ..row
                },
                wire_uri: format!("{uri}_shared_by_bob"),
                wire_displayname: Some("Work (Bob Builder)".to_string()),
                owner_principal: Some("principals/users/bob".to_string()),
                owner_displayname: "Bob Builder".to_string(),
                share_principal: Some("principals/users/alice".to_string()),
                read_only: true,
                transparent: true,
            }
        } else {
            VisibleCalendar::owned(row)
        }
    }

    fn ctx() -> CalendarCtx {
        CalendarCtx {
            context: "/remote.php/dav".to_string(),
            caller: "alice".to_string(),
            caller_principal: "principals/users/alice".to_string(),
            caller_href: "/remote.php/dav/principals/users/alice/".to_string(),
            caller_displayname: "Alice".to_string(),
        }
    }

    #[test]
    fn parses_home_and_calendar() {
        assert_eq!(
            parse_calendars_path("/remote.php/dav/calendars/alice").target,
            CalendarsTarget::Home {
                user: "alice".into(),
                href: "/remote.php/dav/calendars/alice".into()
            }
        );
        assert_eq!(
            parse_calendars_path("/remote.php/dav/calendars/alice/personal/").target,
            CalendarsTarget::Calendar {
                user: "alice".into(),
                cal_uri: "personal".into(),
                href: "/remote.php/dav/calendars/alice/personal".into()
            }
        );
        assert_eq!(
            parse_calendars_path("/remote.php/dav/calendars/alice/personal/x.ics").target,
            CalendarsTarget::Delegated
        );
        assert_eq!(
            parse_calendars_path("/remote.php/dav/system-calendars/alice/").target,
            CalendarsTarget::NotFound
        );
    }

    #[test]
    fn gate_accepts_live_sets_and_rejects_unknown() {
        let ok = PropList::Props(vec![
            PropQName::dav("displayname"),
            PropQName::new(NS_CALENDARSERVER, "getctag"),
            PropQName::new(NS_DAV, "quota-used-bytes"),
            PropQName::new(NS_CALDAV, "supported-calendar-component-set"),
        ]);
        assert!(gate_ok(&ok));
        let bad = PropList::Props(vec![PropQName::new(NS_CALENDARSERVER, "publish-url")]);
        assert!(!gate_ok(&bad));
    }

    fn props_map(response: &DavResponse) -> HashMap<String, PropValue> {
        response.propstats[0]
            .props
            .iter()
            .map(|(q, v)| (format!("{{{}}}{}", q.ns, q.local), v.clone()))
            .collect()
    }

    #[test]
    fn ctag_and_sync_token_quirk() {
        let cal = test_calendar("work", false);
        let props = PropList::Props(vec![
            PropQName::new(NS_CALENDARSERVER, "getctag"),
            PropQName::new(NS_SABREDAV, "sync-token"),
            PropQName::dav("sync-token"),
        ]);
        let response = build_calendar_response(
            "/remote.php/dav/calendars/alice/work/",
            &ctx(),
            &cal,
            &HashMap::new(),
            &props,
            true,
        );
        let values = props_map(&response);
        assert_eq!(
            values["{http://calendarserver.org/ns/}getctag"],
            PropValue::Text("http://sabre.io/ns/sync/7".into())
        );
        assert_eq!(
            values["{http://sabredav.org/ns}sync-token"],
            PropValue::Text("7".into())
        );
        assert_eq!(
            values["{DAV:}sync-token"],
            PropValue::Text("http://sabre.io/ns/sync/7".into())
        );
    }

    #[test]
    fn overrides_replace_backend_values() {
        let cal = test_calendar("work", false);
        let mut overrides = HashMap::new();
        overrides.insert("{DAV:}displayname".to_string(), "Renamed".to_string());
        overrides.insert(
            "{http://apple.com/ns/ical/}calendar-color".to_string(),
            "#abcdef".to_string(),
        );
        overrides.insert(
            "{http://apple.com/ns/ical/}calendar-order".to_string(),
            "9".to_string(),
        );
        let props = PropList::Props(vec![
            PropQName::dav("displayname"),
            PropQName::new(NS_APPLE, "calendar-color"),
            PropQName::new(NS_APPLE, "calendar-order"),
        ]);
        let response = build_calendar_response(
            "/x/",
            &ctx(),
            &cal,
            &overrides,
            &props,
            true,
        );
        let values = props_map(&response);
        assert_eq!(values["{DAV:}displayname"], PropValue::Text("Renamed".into()));
        assert_eq!(
            values["{http://apple.com/ns/ical/}calendar-color"],
            PropValue::Text("#abcdef".into())
        );
        assert_eq!(
            values["{http://apple.com/ns/ical/}calendar-order"],
            PropValue::Text("9".into())
        );
    }

    #[test]
    fn owned_depth0_has_no_owner_principal() {
        let cal = test_calendar("work", false);
        let props = PropList::Props(vec![PropQName::new(NS_OWNCLOUD, "owner-principal")]);
        let d0 = build_calendar_response("/x/", &ctx(), &cal, &HashMap::new(), &props, false);
        assert_eq!(d0.propstats[0].status, 404);
        let d1 = build_calendar_response("/x/", &ctx(), &cal, &HashMap::new(), &props, true);
        assert_eq!(d1.propstats[0].status, 200);
    }

    #[test]
    fn shared_calendar_is_transparent_and_read_only() {
        let cal = test_calendar("work", true);
        let props = PropList::Props(vec![
            PropQName::new(NS_OWNCLOUD, "read-only"),
            PropQName::new(NS_CALDAV, "schedule-calendar-transp"),
            PropQName::new(NS_NEXTCLOUD, "owner-displayname"),
        ]);
        let response = build_calendar_response("/x/", &ctx(), &cal, &HashMap::new(), &props, true);
        let values = props_map(&response);
        assert_eq!(values["{http://owncloud.org/ns}read-only"], PropValue::Text("1".into()));
        assert_eq!(values["{http://nextcloud.com/ns}owner-displayname"], PropValue::Text("Bob Builder".into()));
        let PropValue::Elements(children) = &values["{urn:ietf:params:xml:ns:caldav}schedule-calendar-transp"] else {
            panic!("expected elements");
        };
        assert_eq!(children[0].name, "cal:transparent");
    }

    #[test]
    fn allprop_is_resourcetype_only() {
        let cal = test_calendar("work", false);
        let response = build_calendar_response(
            "/x/",
            &ctx(),
            &cal,
            &HashMap::new(),
            &PropList::AllProp,
            true,
        );
        assert_eq!(response.propstats.len(), 1);
        assert_eq!(response.propstats[0].props.len(), 1);
        assert_eq!(response.propstats[0].props[0].0, PropQName::dav("resourcetype"));
    }

    fn test_object() -> CalendarObject {
        CalendarObject {
            id: 1,
            uri: "e.ics".to_string(),
            etag: "abc".to_string(),
            size: 20,
            lastmodified: Some(1_700_000_000),
            componenttype: Some("VEVENT".to_string()),
            classification: 0,
            calendardata: b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_vec(),
        }
    }

    #[test]
    fn object_gate_allows_implemented_and_known_404() {
        let ok = vec![
            PropQName::dav("getetag"),
            PropQName::new(NS_CALDAV, "calendar-data"),
            PropQName::new(NS_CALDAV, "schedule-tag"),
            PropQName::new(NS_OWNCLOUD, "size"),
            PropQName::new(NS_NEXTCLOUD, "deleted-at"),
        ];
        assert!(object_gate_ok(&ok));
        assert!(!object_gate_ok(&[]));
        assert!(!object_gate_ok(&[PropQName::new(
            "http://example.com/ns",
            "whatever"
        )]));
    }

    #[test]
    fn object_calendar_data_strips_cr() {
        let object = test_object();
        let props = vec![PropQName::new(NS_CALDAV, "calendar-data")];
        let response = build_object_response("/x/e.ics", &object, &props);
        let value = &response.propstats[0].props[0].1;
        assert_eq!(
            value,
            &PropValue::Text("BEGIN:VCALENDAR\nEND:VCALENDAR\n".to_string())
        );
    }

    #[test]
    fn object_etag_quotes_and_content_type_lowercases() {
        let object = test_object();
        let props = vec![
            PropQName::dav("getetag"),
            PropQName::dav("getcontenttype"),
            PropQName::dav("resourcetype"),
        ];
        let response = build_object_response("/x/e.ics", &object, &props);
        let values: HashMap<PropQName, PropValue> =
            response.propstats[0].props.iter().cloned().collect();
        assert_eq!(
            values[&PropQName::dav("getetag")],
            PropValue::Text("\"abc\"".to_string())
        );
        assert_eq!(
            values[&PropQName::dav("getcontenttype")],
            PropValue::Text("text/calendar; charset=utf-8; component=vevent".to_string())
        );
        assert_eq!(values[&PropQName::dav("resourcetype")], PropValue::Empty);
    }

    #[test]
    fn object_content_type_omits_empty_component() {
        let mut object = test_object();
        object.componenttype = None;
        assert_eq!(object_content_type(&object), "text/calendar; charset=utf-8");
        object.componenttype = Some(String::new());
        assert_eq!(object_content_type(&object), "text/calendar; charset=utf-8");
    }
}
