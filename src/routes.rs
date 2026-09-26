// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP routing and the CardDAV protocol surface (read-only).
//!
//! Only `/remote.php/dav/addressbooks/users/<user>/...` is handled. Writes are
//! answered with `501 Not Implemented` so nginx can fall back to PHP (design doc
//! §5.2, §5.3).

use crate::auth::{AuthError, AuthenticatedUser, Authenticator};
use crate::config::{Config, DEFAULT_SYNC_LIMIT, MAX_RESOURCE_SIZE};
use crate::dav_error;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::model::{AddressBook, Card, VisibleBook};
use crate::outbox::EffectRegistry;
use crate::sync::{self, SYNCTOKEN_PREFIX};
use crate::util::{
    encode_path_segment, http_date, parse_basic_auth, parse_http_date, percent_decode,
};
use crate::vcard;
use crate::vcard_validate::{self, Reject};
use crate::xml::filter;
use crate::xml::parse::{self, PropList};
use crate::xml::write::{
    DavResponse, MultiStatus, PropQName, PropStat, PropValue, XmlElement, NS_CALENDARSERVER,
    NS_CARDDAV, NS_DAV, NS_NEXTCLOUD, NS_OWNCLOUD, NS_SABREDAV,
};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::sync::Arc;

const MAX_BODY: usize = 10 * 1024 * 1024;

/// Marks a response the sidecar served itself. It is deliberately **not** set
/// on the `501` responses that delegate to PHP (nginx replays those), so a
/// differential harness can prove which backend produced a body instead of
/// silently diffing PHP against itself.
pub const SIDECAR_HEADER: &str = "x-nextcloud-dav";
/// The value of [`SIDECAR_HEADER`] on a native response.
pub const SIDECAR_VALUE: &str = "sidecar";

pub struct AppState {
    pub db: Arc<Db>,
    pub auth: Authenticator,
    pub config: Config,
    /// The resolved write-size limit (`oc_appconfig` `dav/card_size_limit`, or
    /// the `nextcloud_dav.card_size_limit` override), cached at startup.
    pub card_size_limit: u64,
    /// Whether native `PUT`/`DELETE` are served in-process. False when the
    /// outbox table is missing or `event_dispatch.enabled` is off; writes then
    /// return 501 so nginx falls back to PHP.
    pub native_writes: bool,
    /// The effect-ownership registry frozen into every outbox row.
    pub registry: Arc<EffectRegistry>,
    /// The short-TTL per-user mount map for the files `PROPFIND`.
    pub mounts: Arc<crate::mounts::MountCache>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/remote.php/dav", any(dispatch_discovery))
        .route("/remote.php/dav/", any(dispatch_discovery))
        .route(
            "/remote.php/dav/principals/users/{uid}",
            any(dispatch_discovery),
        )
        .route(
            "/remote.php/dav/principals/users/{uid}/",
            any(dispatch_discovery),
        )
        .route("/remote.php/dav/principals", any(dispatch_discovery))
        .route("/remote.php/dav/principals/", any(dispatch_discovery))
        .route(
            "/remote.php/dav/principals/{*rest}",
            any(dispatch_discovery),
        )
        .route("/remote.php/dav/addressbooks", any(dispatch))
        .route("/remote.php/dav/addressbooks/{*rest}", any(dispatch))
        .route("/remote.php/dav/calendars", any(dispatch_calendars))
        .route("/remote.php/dav/calendars/{*rest}", any(dispatch_calendars))
        .route("/remote.php/dav/files", any(dispatch_files))
        .route("/remote.php/dav/files/{*rest}", any(dispatch_files))
        .with_state(state)
}

async fn healthz() -> Response {
    (StatusCode::OK, "ok\n").into_response()
}

/// The parsed request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPath {
    /// Everything up to and including `/addressbooks`, used to build hrefs.
    pub context: String,
    pub target: DavTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DavTarget {
    Home {
        user: String,
        href: String,
    },
    Book {
        user: String,
        book_uri: String,
        href: String,
    },
    Card {
        user: String,
        book_uri: String,
        card_uri: String,
        href: String,
    },
    NotFound,
}

impl DavTarget {
    pub fn user(&self) -> Option<&str> {
        match self {
            DavTarget::Home { user, .. }
            | DavTarget::Book { user, .. }
            | DavTarget::Card { user, .. } => Some(user),
            DavTarget::NotFound => None,
        }
    }
}

/// Parses `<webroot>/remote.php/dav/addressbooks/users/<u>[/<book>[/<card>]]`.
pub fn parse_path(path: &str) -> ParsedPath {
    let Some(index) = path.rfind("/addressbooks") else {
        return ParsedPath {
            context: String::new(),
            target: DavTarget::NotFound,
        };
    };
    let context = format!("{}/addressbooks", &path[..index]);
    let rest = &path[index + "/addressbooks".len()..];
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

    match segments.as_slice() {
        ["users", user_raw] if !user_raw.is_empty() => {
            let href = format!("{context}/users/{user_raw}");
            ParsedPath {
                context,
                target: DavTarget::Home {
                    user: percent_decode(user_raw),
                    href,
                },
            }
        }
        ["users", user_raw, book_raw] if !user_raw.is_empty() && !book_raw.is_empty() => {
            let home = format!("{context}/users/{user_raw}");
            ParsedPath {
                context,
                target: DavTarget::Book {
                    user: percent_decode(user_raw),
                    book_uri: percent_decode(book_raw),
                    href: format!("{home}/{book_raw}"),
                },
            }
        }
        ["users", user_raw, book_raw, card_raw]
            if !user_raw.is_empty() && !book_raw.is_empty() && !card_raw.is_empty() =>
        {
            let home = format!("{context}/users/{user_raw}");
            let book = format!("{home}/{book_raw}");
            ParsedPath {
                context,
                target: DavTarget::Card {
                    user: percent_decode(user_raw),
                    book_uri: percent_decode(book_raw),
                    card_uri: percent_decode(card_raw),
                    href: format!("{book}/{card_raw}"),
                },
            }
        }
        _ => ParsedPath {
            context,
            target: DavTarget::NotFound,
        },
    }
}

async fn dispatch(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match handle(state, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

/// WebDAV **files** (`/remote.php/dav/files/**`) dispatch.
///
/// Only a native `PROPFIND` Depth 0/1 for the caller's own, mount-free home
/// storage is served; everything else (including `OPTIONS`, whose DAV/Allow
/// headers the sidecar advertises for address books) answers 501 so nginx
/// replays the request to PHP. See `src/files.rs`.
async fn dispatch_files(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match handle_files(state, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

/// DAV **discovery** dispatch: `PROPFIND` Depth 0 on the DAV root and on the
/// caller's own principal. See `src/discovery.rs`.
async fn dispatch_discovery(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match handle_discovery(state, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

/// CalDAV dispatch: `PROPFIND` on the caller's own calendar home and on one of
/// their calendars, plus the `sync-collection` and `calendar-multiget` REPORTs
/// on one of their calendars. Everything else delegates with 501. See
/// `src/calendars.rs`.
async fn dispatch_calendars(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match handle_calendars(state, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn handle_calendars(state: Arc<AppState>, request: Request) -> Result<Response> {
    // Every other method (OPTIONS included) is delegated: PHP owns the
    // capability headers, the object GETs and the whole write path.
    let method = request.method().as_str().to_string();
    if method != "PROPFIND" && method != "REPORT" {
        return Ok(not_implemented());
    }

    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let headers = request.headers().clone();
    let parsed = crate::calendars::parse_calendars_path(&path);
    match parsed.target {
        crate::calendars::CalendarsTarget::NotFound => return Ok(Error::NotFound.into_response()),
        crate::calendars::CalendarsTarget::Delegated => return Ok(not_implemented()),
        _ => {}
    }

    let target_user = match &parsed.target {
        crate::calendars::CalendarsTarget::Home { user, .. }
        | crate::calendars::CalendarsTarget::Calendar { user, .. } => Some(user.clone()),
        _ => None,
    };
    let user = match authenticate_dav(
        &state,
        &headers,
        &method,
        query.as_deref(),
        target_user.as_deref(),
        false,
    )
    .await
    {
        AuthOutcome::User(user) => user,
        AuthOutcome::Delegate => return Ok(not_implemented()),
        AuthOutcome::Error(error) => return Ok(auth_error_response(error)),
    };

    let body = read_body(request).await?;
    if method == "REPORT" {
        return match crate::calendars::handle_report(&state.db, &user.uid, &parsed, &body).await? {
            crate::calendars::ReportOutcome::Multistatus(multistatus) => Ok(xml_response(
                StatusCode::MULTI_STATUS,
                multistatus.to_xml_caldav(),
            )),
            crate::calendars::ReportOutcome::Delegated => Ok(not_implemented()),
            crate::calendars::ReportOutcome::InvalidSyncToken => {
                Ok(dav_error::invalid_sync_token())
            }
            crate::calendars::ReportOutcome::UnsupportedInitialLimit => {
                Ok(dav_error::unsupported_limit_on_initial_sync())
            }
            crate::calendars::ReportOutcome::BadRequest(message) => {
                Ok(dav_error::bad_request(&message))
            }
        };
    }

    let depth = parse_depth(&headers);
    let accept_language = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok());
    let origin = request_origin(&headers, &state.config.nextcloud_url);
    let cached_subscriptions = webcal_caching_enabled(&headers);
    match crate::calendars::handle_propfind(
        &state.db,
        &state.config,
        &user.uid,
        &parsed,
        depth,
        accept_language,
        &origin,
        cached_subscriptions,
        &body,
    )
    .await?
    {
        Some(multistatus) => Ok(xml_response(
            StatusCode::MULTI_STATUS,
            multistatus.to_xml_caldav(),
        )),
        None => Ok(not_implemented()),
    }
}

async fn handle_discovery(state: Arc<AppState>, request: Request) -> Result<Response> {
    // Every non-PROPFIND method (OPTIONS included) is delegated before touching
    // credentials: PHP owns the DAV/Allow capability headers and the write path.
    if request.method().as_str() != "PROPFIND" {
        return Ok(not_implemented());
    }
    // The sidecar's own PHP-fallback probe must not recurse through the root
    // route; delegate it so nginx replays it to PHP (see `php::FALLBACK_HEADER`).
    if request.headers().contains_key(crate::php::FALLBACK_HEADER) {
        return Ok(not_implemented());
    }

    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let headers = request.headers().clone();
    let parsed = crate::discovery::parse_discovery_path(&path);
    match parsed.target {
        // A tree the discovery router does not own is a plain 404 (its own
        // route, if any, handles it).
        crate::discovery::DiscoveryTarget::NotFound => return Ok(Error::NotFound.into_response()),
        // The principal collection listings and the other principal children
        // are PHP's: delegate so nginx replays the request.
        crate::discovery::DiscoveryTarget::Delegated => return Ok(not_implemented()),
        _ => {}
    }
    // Only Depth 0 is served; Depth 1 on the root lists every collection and on
    // the principal collection lists every user, so both are delegated.
    if parse_depth(&headers) != 0 {
        return Ok(not_implemented());
    }

    let target_user = match &parsed.target {
        crate::discovery::DiscoveryTarget::Principal { uid, .. } => Some(uid.clone()),
        _ => None,
    };
    let user = match authenticate_dav(
        &state,
        &headers,
        "PROPFIND",
        query.as_deref(),
        target_user.as_deref(),
        false,
    )
    .await
    {
        AuthOutcome::User(user) => user,
        AuthOutcome::Delegate => return Ok(not_implemented()),
        AuthOutcome::Error(error) => return Ok(auth_error_response(error)),
    };

    let body = read_body(request).await?;
    match crate::discovery::handle_propfind(&state.db, &state.config, &user.uid, &parsed, &body)
        .await?
    {
        Some(multistatus) => {
            let namespaces = match parsed.target {
                crate::discovery::DiscoveryTarget::Root { .. } => MultiStatus::DAV_ROOT_NAMESPACES,
                _ => MultiStatus::PRINCIPAL_NAMESPACES,
            };
            Ok(xml_response(
                StatusCode::MULTI_STATUS,
                multistatus.to_xml_with(namespaces),
            ))
        }
        None => Ok(not_implemented()),
    }
}

async fn handle_files(state: Arc<AppState>, request: Request) -> Result<Response> {
    // Every non-PROPFIND method is delegated before touching credentials: PHP
    // owns OPTIONS discovery and the whole write path for files.
    if request.method().as_str() != "PROPFIND" {
        return Ok(not_implemented());
    }

    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let headers = request.headers().clone();
    let Some(parsed) = crate::files::parse_files_path(&path) else {
        return Ok(Error::NotFound.into_response());
    };

    let user = match authenticate_dav(
        &state,
        &headers,
        "PROPFIND",
        query.as_deref(),
        Some(parsed.uid.as_str()),
        true,
    )
    .await
    {
        AuthOutcome::User(user) => user,
        AuthOutcome::Delegate => return Ok(not_implemented()),
        AuthOutcome::Error(error) => return Ok(auth_error_response(error)),
    };

    // `Files\RootCollection::getChildForPrincipal()` only serves the caller's
    // own home; a different principal is a 404, not a 403.
    if parsed.uid != user.uid {
        return Ok(Error::NotFound.into_response());
    }

    let body = read_body(request).await?;
    let depth = crate::files::parse_depth(&headers);
    let minimal = crate::files::prefer_minimal(&headers);
    match crate::files::handle_propfind(
        &state.db,
        &state.config,
        &state.mounts,
        &user,
        &parsed,
        depth,
        minimal,
        &body,
    )
    .await?
    {
        Some(multistatus) => Ok(xml_response(
            StatusCode::MULTI_STATUS,
            multistatus.to_xml_files(),
        )),
        None => Ok(not_implemented()),
    }
}

/// Outcome of the Basic-or-session authentication attempt.
enum AuthOutcome {
    User(AuthenticatedUser),
    /// Return `501` so nginx replays the request to PHP.
    Delegate,
    /// A definitive Basic-auth failure (`401`/`429`/`503`/`502`).
    Error(AuthError),
}

/// Authenticates a DAV request.
///
/// A Basic credential pair goes through the app-password fast path (unchanged).
/// A request with **no** `Authorization` header is evaluated against the
/// Nextcloud session cookie (design doc §6); anything uncertain returns
/// [`AuthOutcome::Delegate`], never a user. A non-Basic `Authorization` header
/// (OAuth Bearer) is PHP's and is delegated.
async fn authenticate_dav(
    state: &AppState,
    headers: &HeaderMap,
    method: &str,
    query: Option<&str>,
    target_user: Option<&str>,
    filesystem_required: bool,
) -> AuthOutcome {
    if let Some((username, password)) = parse_basic_auth(headers.get(header::AUTHORIZATION)) {
        let client_ip = client_ip(headers);
        return match state
            .auth
            .authenticate(&username, &password, client_ip, filesystem_required)
            .await
        {
            Ok(user) => AuthOutcome::User(user),
            Err(error) => AuthOutcome::Error(error),
        };
    }
    if headers.get(header::AUTHORIZATION).is_some() {
        return AuthOutcome::Delegate;
    }
    match state
        .auth
        .authenticate_session(headers, method, query, filesystem_required)
        .await
    {
        Some(user) => {
            // The session's user must own the request path; a session for a
            // different user is delegated, never served (design doc §7).
            if target_user.is_some_and(|target| target != user.uid) {
                log::debug!("session user does not own the request path; delegating");
                return AuthOutcome::Delegate;
            }
            AuthOutcome::User(user)
        }
        None => AuthOutcome::Delegate,
    }
}

async fn handle(state: Arc<AppState>, request: Request) -> Result<Response> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    let headers = request.headers().clone();
    let parsed = parse_path(&path);

    // A request with no Basic credentials is NOT refused here, not even for
    // OPTIONS. It may be authenticated by the Nextcloud session cookie (which
    // the sidecar now evaluates, see `Authenticator::authenticate_session`) or
    // by an OAuth Bearer token. Anything it cannot evaluate is delegated:
    // 501 -> nginx replays it to PHP, which owns auth. A 401 from here would
    // make the browser pop up a Basic Auth prompt for requests PHP serves
    // happily; that was a real regression in production, visible as 401s on
    // /remote.php/dav/files/<user>/ interleaved with 207s from the retry.
    let target_user = match &parsed.target {
        DavTarget::Home { user, .. }
        | DavTarget::Book { user, .. }
        | DavTarget::Card { user, .. } => Some(user.clone()),
        DavTarget::NotFound => None,
    };
    let user = match authenticate_dav(
        &state,
        &headers,
        method.as_str(),
        Some(&query),
        target_user.as_deref(),
        false,
    )
    .await
    {
        AuthOutcome::User(user) => user,
        AuthOutcome::Delegate => return Ok(not_implemented()),
        AuthOutcome::Error(error) => return Ok(auth_error_response(error)),
    };

    if method == Method::OPTIONS {
        return options_response(&parsed);
    }

    if let Some(method) = unsupported_method(&method) {
        log::debug!("{method} {path} -> 501 (nginx fallback)");
        return Ok(not_implemented());
    }

    // `?photo` / `?export` need PHP's appdata files and are delegated.
    if query_requires_php(&query) {
        return Ok(not_implemented());
    }

    let target = parsed.target.clone();
    match target {
        DavTarget::NotFound => Ok(Error::NotFound.into_response()),
        DavTarget::Home {
            user: ref target_user,
            ref href,
        } => {
            if target_user != &user.uid {
                return Ok(Error::NotFound.into_response());
            }
            let href = href.clone();
            let target_user = target_user.clone();
            match method.as_str() {
                "PROPFIND" => {
                    let body = read_body(request).await?;
                    let depth = parse_depth(&headers);
                    handle_propfind(&state, &parsed, &target_user, &href, depth, &body).await
                }
                _ => Ok(not_implemented()),
            }
        }
        DavTarget::Book {
            user: ref target_user,
            ref book_uri,
            ref href,
        } => {
            if target_user != &user.uid {
                return Ok(Error::NotFound.into_response());
            }
            if is_delegated_book_uri(book_uri) {
                return Ok(not_implemented());
            }
            let target_user = target_user.clone();
            let book_uri = book_uri.clone();
            let href = href.clone();
            match method.as_str() {
                "PROPFIND" => {
                    let body = read_body(request).await?;
                    let depth = parse_depth(&headers);
                    handle_propfind(&state, &parsed, &target_user, &href, depth, &body).await
                }
                "REPORT" => {
                    let body = read_body(request).await?;
                    handle_report_book(
                        &state,
                        &parsed,
                        &target_user,
                        &book_uri,
                        &href,
                        &headers,
                        &body,
                    )
                    .await
                }
                _ => Ok(not_implemented()),
            }
        }
        DavTarget::Card {
            user: ref target_user,
            ref book_uri,
            ref card_uri,
            ref href,
        } => {
            if target_user != &user.uid {
                return Ok(Error::NotFound.into_response());
            }
            if is_delegated_book_uri(book_uri) {
                return Ok(not_implemented());
            }
            let target_user = target_user.clone();
            let book_uri = book_uri.clone();
            let card_uri = card_uri.clone();
            let href = href.clone();
            match method.as_str() {
                "GET" | "HEAD" => {
                    get_card(&state, &target_user, &book_uri, &card_uri, &method, &headers).await
                }
                "PUT" => {
                    if !state.native_writes {
                        return Ok(not_implemented());
                    }
                    put_card(
                        &state,
                        &target_user,
                        &book_uri,
                        &card_uri,
                        &href,
                        &headers,
                        request,
                    )
                    .await
                }
                "DELETE" => {
                    if !state.native_writes {
                        return Ok(not_implemented());
                    }
                    delete_card(&state, &target_user, &book_uri, &card_uri, &headers).await
                }
                "PROPFIND" => {
                    let body = read_body(request).await?;
                    let depth = parse_depth(&headers);
                    handle_propfind(&state, &parsed, &target_user, &href, depth, &body).await
                }
                "REPORT" => {
                    let body = read_body(request).await?;
                    handle_report_card(
                        &state,
                        &parsed,
                        &target_user,
                        &book_uri,
                        &card_uri,
                        &href,
                        &body,
                    )
                    .await
                }
                _ => Ok(not_implemented()),
            }
        }
    }
}

fn query_requires_php(query: &str) -> bool {
    query
        .split('&')
        .filter_map(|pair| pair.split('=').next())
        .any(|key| key == "photo" || key == "export")
}

fn unsupported_method(method: &Method) -> Option<&'static str> {
    match method.as_str() {
        "MKCOL" => Some("MKCOL"),
        "PROPPATCH" => Some("PROPPATCH"),
        "MOVE" => Some("MOVE"),
        "COPY" => Some("COPY"),
        "POST" => Some("POST"),
        _ => None,
    }
}

async fn read_body(request: Request) -> Result<Vec<u8>> {
    let bytes = axum::body::to_bytes(request.into_body(), MAX_BODY)
        .await
        .map_err(|e| Error::bad_request(format!("failed to read body: {e}")))?;
    Ok(bytes.to_vec())
}

fn parse_depth(headers: &HeaderMap) -> i64 {
    match headers
        .get("depth")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        Some("0") | None => 0,
        Some("1") | Some("infinity") => 1,
        Some(_) => 0,
    }
}

fn client_ip(headers: &HeaderMap) -> IpAddr {
    if let Some(forwarded) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
    {
        if let Some(first) = forwarded.split(',').next() {
            if let Ok(ip) = IpAddr::from_str(first.trim()) {
                return ip;
            }
        }
    }
    if let Some(real) = headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(ip) = IpAddr::from_str(real.trim()) {
            return ip;
        }
    }
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

/// The absolute origin (`scheme://host`) PHP's `URLGenerator::getAbsoluteURL()`
/// sees, used for the CalDAV `{cs}publish-url`. The sidecar sits behind nginx,
/// which preserves `Host` and sets `X-Forwarded-Proto`; the scheme falls back to
/// `http` and the authority to the configured Nextcloud URL.
fn request_origin(headers: &HeaderMap, fallback: &str) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|scheme| !scheme.is_empty())
        .unwrap_or("http");
    match headers.get(header::HOST).and_then(|value| value.to_str().ok()) {
        Some(host) if !host.is_empty() => format!("{scheme}://{host}"),
        _ => fallback.trim_end_matches('/').to_string(),
    }
}

/// `OCA\DAV\CalDAV\WebcalCaching\Plugin`: true when the request asks for cached
/// subscriptions (a known client user agent or the explicit header). With
/// caching on, PHP serves a subscription as a `CachedSubscription`, a node the
/// sidecar does not model, so the CalDAV router delegates such requests.
fn webcal_caching_enabled(headers: &HeaderMap) -> bool {
    if headers
        .get("x-nc-caldav-webcal-caching")
        .and_then(|value| value.to_str().ok())
        == Some("On")
    {
        return true;
    }
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    // `ENABLE_FOR_CLIENTS = ['/^MSFT-WIN-3/', '/Evolution/', '/KIO/']`.
    user_agent.starts_with("MSFT-WIN-3")
        || user_agent.contains("Evolution")
        || user_agent.contains("KIO")
}

fn unauthorized() -> Response {
    Error::Unauthorized.into_response()
}

fn not_implemented() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        "This CardDAV sidecar does not implement this method; it is served by Nextcloud PHP.\n",
    )
        .into_response()
}

fn auth_error_response(error: AuthError) -> Response {
    match error {
        AuthError::Invalid => unauthorized(),
        AuthError::Throttled { retry_after_secs } => {
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many failed login attempts.\n",
            )
                .into_response();
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
        AuthError::Maintenance => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Nextcloud is in maintenance mode.\n",
        )
            .into_response(),
        // The scoped token (or another condition PHP owns) must be replayed by
        // nginx to PHP, never served by the sidecar.
        AuthError::Delegate => not_implemented(),
        AuthError::Upstream(message) => {
            log::warn!("authentication upstream error: {message}");
            (
                StatusCode::BAD_GATEWAY,
                "Authentication backend unavailable.\n",
            )
                .into_response()
        }
    }
}

/// The `OPTIONS` answer, reachable only with credentials: an unauthenticated
/// request is delegated before this point, because the sidecar cannot tell a
/// session-cookie client (the web UI) from an anonymous one.
fn options_response(parsed: &ParsedPath) -> Result<Response> {
    if matches!(parsed.target, DavTarget::NotFound) {
        return Ok(Error::NotFound.into_response());
    }
    let mut response = Response::new(Body::empty());
    response
        .headers_mut()
        .insert("DAV", HeaderValue::from_static("1, 2, 3, addressbook"));
    response.headers_mut().insert(
        header::ALLOW,
        HeaderValue::from_static(
            "OPTIONS, GET, HEAD, PROPFIND, REPORT, PUT, DELETE, MKCOL, PROPPATCH",
        ),
    );
    response
        .headers_mut()
        .insert("MS-Author-Via", HeaderValue::from_static("DAV"));
    Ok(response)
}

// ---------------------------------------------------------------------------
// PROPFIND
// ---------------------------------------------------------------------------

async fn handle_propfind(
    state: &AppState,
    parsed: &ParsedPath,
    user: &str,
    _href: &str,
    depth: i64,
    body: &[u8],
) -> Result<Response> {
    let request = parse::parse_propfind(body)?;

    let mut responses = Vec::new();
    match &parsed.target {
        DavTarget::Home { href, .. } => {
            // The home stays on PHP in production (declared deviation
            // `home-listing-php`); this handler is only reached on a misroute.
            // It lists the caller's own books.
            let ctx = caller_context(state, parsed, user).await?;
            responses.push(build_response(
                &collection_href(href),
                NodeData::Home,
                &ctx,
                &request.props,
            ));
            if depth >= 1 {
                let books = state.db.address_books_for_user(&principal(user)).await?;
                for book in books.into_iter().map(VisibleBook::owned) {
                    let book_href = format!("{href}/{}", encode_path_segment(&book.wire_uri));
                    let mut ctx = caller_context(state, parsed, user).await?;
                    ctx.groups = requested_groups(&request.props, &book.book, &state.db).await?;
                    responses.push(build_response(
                        &collection_href(&book_href),
                        NodeData::Book(&book),
                        &ctx,
                        &request.props,
                    ));
                }
            }
        }
        DavTarget::Book { href, book_uri, .. } => {
            let Some(book) = resolve_book(state, user, book_uri).await? else {
                return Ok(Error::NotFound.into_response());
            };
            let mut ctx = book_context(state, parsed, user, &book).await?;
            ctx.groups = requested_groups(&request.props, &book.book, &state.db).await?;
            responses.push(build_response(
                &collection_href(href),
                NodeData::Book(&book),
                &ctx,
                &request.props,
            ));
            if depth >= 1 {
                let cards = state.db.cards(book.book.id).await?;
                for card in &cards {
                    let card_href = format!("{href}/{}", encode_path_segment(&card.uri));
                    responses.push(build_response(
                        &card_href,
                        NodeData::Card(card),
                        &ctx,
                        &request.props,
                    ));
                }
            }
        }
        DavTarget::Card {
            href,
            book_uri,
            card_uri,
            ..
        } => {
            let Some(book) = resolve_book(state, user, book_uri).await? else {
                return Ok(Error::NotFound.into_response());
            };
            let Some(card) = state.db.card(book.book.id, card_uri).await? else {
                return Ok(Error::NotFound.into_response());
            };
            let ctx = book_context(state, parsed, user, &book).await?;
            responses.push(build_response(
                href,
                NodeData::Card(&card),
                &ctx,
                &request.props,
            ));
        }
        DavTarget::NotFound => return Ok(Error::NotFound.into_response()),
    }

    let multistatus = MultiStatus {
        responses,
        sync_token: None,
    };
    Ok(xml_response(StatusCode::MULTI_STATUS, multistatus.to_xml()))
}

async fn requested_groups(props: &PropList, book: &AddressBook, db: &Db) -> Result<Vec<String>> {
    let wants_groups = match props {
        PropList::AllProp => true,
        PropList::PropName => true,
        PropList::Props(props) => props
            .iter()
            .any(|p| p.ns == NS_OWNCLOUD && p.local == "groups"),
    };
    if wants_groups {
        db.contact_groups(book.id).await
    } else {
        Ok(Vec::new())
    }
}

/// Collection responses must carry a trailing slash, exactly as Sabre emits
/// them (`/remote.php/dav/addressbooks/users/<u>/<book>/`). Children are built
/// by appending to the slash-less href, so only the collection's own response
/// is normalised here.
fn collection_href(href: &str) -> String {
    if href.ends_with('/') {
        href.to_string()
    } else {
        format!("{href}/")
    }
}

fn build_response(
    href: &str,
    node: NodeData<'_>,
    ctx: &PropContext,
    props: &PropList,
) -> DavResponse {
    build_response_with_data(href, node, ctx, props, None)
}

/// Like [`build_response`], but with a pre-rendered `address-data` value (the
/// REPORT paths negotiate it through `vobject::convert_vcard`).
fn build_response_with_data(
    href: &str,
    node: NodeData<'_>,
    ctx: &PropContext,
    props: &PropList,
    address_data: Option<&str>,
) -> DavResponse {
    let requested: Vec<PropQName> = match props {
        PropList::AllProp => default_props(&node),
        PropList::PropName => default_props(&node),
        PropList::Props(props) => props.clone(),
    };
    let propname_only = matches!(props, PropList::PropName);

    let mut found: Vec<(PropQName, PropValue)> = Vec::new();
    let mut missing: Vec<PropQName> = Vec::new();
    for qname in &requested {
        let resolved = match (&node, address_data) {
            (NodeData::Card(_), Some(data))
                if qname.ns == NS_CARDDAV && qname.local == "address-data" =>
            {
                Some(PropValue::Text(data.to_string()))
            }
            _ => resolve_property(qname, &node, ctx),
        };
        match resolved {
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
    if !missing.is_empty() {
        propstats.push(PropStat::not_found(missing));
    }
    DavResponse::props(href.to_string(), propstats)
}

#[derive(Clone, Copy)]
enum NodeData<'a> {
    Home,
    Book(&'a VisibleBook),
    Card(&'a Card),
}

struct PropContext {
    /// `{DAV:}owner` href: the owner's principal for a shared book, the
    /// caller's otherwise.
    principal_href: String,
    /// `{nc}owner-displayname`.
    owner_displayname: String,
    /// `Some(owner principal)` only for a shared book; the switch for
    /// `{oc}owner-principal` / `{oc}read-only`.
    owner_principal: Option<String>,
    /// `{oc}read-only` and the read-only `current-user-privilege-set`.
    read_only: bool,
    groups: Vec<String>,
}

fn context_of(parsed: &ParsedPath) -> String {
    // `context` is `<webroot>/addressbooks`; the DAV context is everything
    // before `/addressbooks`.
    let base = parsed.context.strip_suffix("/addressbooks").unwrap_or("");
    format!("{base}/")
}

fn principal(user: &str) -> String {
    format!("principals/users/{user}")
}

/// Resolves a requested book against the caller's visible set (owned + shared),
/// matching the constructed wire name. `None` is the 404 case.
///
/// A book whose real URI already contains `_shared_by_` can collide with a
/// shared book's wire name; owned books are listed first, so they win, exactly
/// like `Sabre\DAV\Collection::getChild()`.
/// Book URIs that only PHP can serve: the address books contributed by
/// Nextcloud plugins rather than by rows the sidecar can read.
///
/// `z-app-generated` is the reserved prefix for plugin-provided books
/// (`apps/dav/lib/CardDAV/Integration/ExternalAddressBook.php`, and
/// `UserAddressBooks::createExtendedCollection()` refuses to let a user create
/// such a name), and `z-server-generated--system` is the system address book's
/// shared alias (`apps/dav/lib/CardDAV/SystemAddressbook.php`). Neither has an
/// `oc_addressbooks` row the sidecar could resolve, so answering 501 lets nginx
/// replay the request to PHP — which is what the client expects, and what the
/// home listing (served by PHP) has already advertised.
fn is_delegated_book_uri(book_uri: &str) -> bool {
    book_uri == "z-server-generated--system" || book_uri.starts_with("z-app-generated")
}

async fn resolve_book(state: &AppState, user: &str, book_uri: &str) -> Result<Option<VisibleBook>> {
    let caller = principal(user);
    let groups = state.db.group_principals(user).await?;
    state
        .db
        .visible_book_by_uri(&caller, &groups, book_uri)
        .await
}

/// The property context for the caller's own principal (the home node and
/// owned books).
async fn caller_context(state: &AppState, parsed: &ParsedPath, user: &str) -> Result<PropContext> {
    let principal_href = format!(
        "{}principals/users/{}/",
        context_of(parsed),
        encode_path_segment(user)
    );
    let owner_displayname = state
        .db
        .user_display_name(user)
        .await?
        .unwrap_or_else(|| user.to_string());
    Ok(PropContext {
        principal_href,
        owner_displayname,
        owner_principal: None,
        read_only: false,
        groups: Vec::new(),
    })
}

/// The property context for a book: the **owner's** principal and display name
/// for a shared book, the caller's otherwise.
async fn book_context(
    state: &AppState,
    parsed: &ParsedPath,
    user: &str,
    book: &VisibleBook,
) -> Result<PropContext> {
    let base = context_of(parsed);
    let (principal_href, owner_displayname) = match &book.owner_principal {
        Some(owner) => {
            let owner_name = owner.rsplit('/').next().unwrap_or_default();
            let href = format!(
                "{base}principals/users/{}/",
                encode_path_segment(owner_name)
            );
            let displayname = state
                .db
                .user_display_name(owner_name)
                .await?
                .unwrap_or_else(|| owner_name.to_string());
            (href, displayname)
        }
        None => {
            let href = format!("{base}principals/users/{}/", encode_path_segment(user));
            let displayname = state
                .db
                .user_display_name(user)
                .await?
                .unwrap_or_else(|| user.to_string());
            (href, displayname)
        }
    };
    Ok(PropContext {
        principal_href,
        owner_displayname,
        owner_principal: book.owner_principal.clone(),
        read_only: book.read_only,
        groups: Vec::new(),
    })
}

fn default_props(node: &NodeData<'_>) -> Vec<PropQName> {
    match node {
        NodeData::Home => vec![
            PropQName::dav("resourcetype"),
            PropQName::dav("current-user-privilege-set"),
            PropQName::dav("owner"),
            PropQName::dav("supported-report-set"),
        ],
        NodeData::Book(_) => vec![
            PropQName::dav("resourcetype"),
            PropQName::dav("displayname"),
            PropQName::carddav("addressbook-description"),
            PropQName::new(NS_CALENDARSERVER, "getctag"),
            PropQName::new(NS_SABREDAV, "sync-token"),
            PropQName::dav("sync-token"),
            PropQName::dav("supported-report-set"),
            PropQName::carddav("max-resource-size"),
            PropQName::carddav("supported-address-data"),
            PropQName::carddav("supported-collation-set"),
            PropQName::dav("owner"),
            PropQName::dav("current-user-privilege-set"),
            PropQName::owncloud("groups"),
            PropQName::owncloud("owner-principal"),
            PropQName::owncloud("read-only"),
            PropQName::nextcloud("owner-displayname"),
        ],
        NodeData::Card(_) => vec![
            PropQName::dav("resourcetype"),
            PropQName::dav("getetag"),
            PropQName::dav("getcontentlength"),
            PropQName::dav("getlastmodified"),
            PropQName::dav("getcontenttype"),
            PropQName::nextcloud("has-photo"),
        ],
    }
}

fn resolve_property(
    qname: &PropQName,
    node: &NodeData<'_>,
    ctx: &PropContext,
) -> Option<PropValue> {
    let ns = qname.ns.as_str();
    let local = qname.local.as_str();

    match ns {
        NS_DAV => match (local, node) {
            ("resourcetype", NodeData::Home) => {
                Some(PropValue::Elements(vec![XmlElement::new("d:collection")]))
            }
            ("resourcetype", NodeData::Book(_)) => Some(PropValue::Elements(vec![
                XmlElement::new("d:collection"),
                XmlElement::new("card:addressbook"),
            ])),
            ("resourcetype", NodeData::Card(_)) => Some(PropValue::Empty),
            ("displayname", NodeData::Book(book)) => Some(PropValue::Text(
                book.wire_displayname
                    .clone()
                    .unwrap_or_else(|| book.wire_uri.clone()),
            )),
            ("supported-report-set", NodeData::Book(_)) => Some(report_set(&[
                "card:addressbook-query",
                "card:addressbook-multiget",
                "d:sync-collection",
            ])),
            ("supported-report-set", NodeData::Card(_)) => Some(report_set(&[
                "card:addressbook-query",
                "card:addressbook-multiget",
            ])),
            ("supported-report-set", NodeData::Home) => Some(PropValue::Elements(Vec::new())),
            ("sync-token", NodeData::Book(book)) => Some(PropValue::Text(format!(
                "{SYNCTOKEN_PREFIX}{}",
                book.book.synctoken
            ))),
            ("owner", _) => Some(PropValue::Elements(vec![
                XmlElement::new("d:href").text(ctx.principal_href.clone())
            ])),
            ("current-user-privilege-set", NodeData::Home) => Some(privilege_set(false)),
            ("current-user-privilege-set", NodeData::Book(_)) => {
                if ctx.read_only {
                    Some(read_only_privilege_set())
                } else {
                    Some(privilege_set(true))
                }
            }
            ("current-user-privilege-set", NodeData::Card(_)) => Some(privilege_set(false)),
            ("getetag", NodeData::Card(card)) => Some(PropValue::Text(card.quoted_etag())),
            ("getcontentlength", NodeData::Card(card)) => {
                Some(PropValue::Text(card.size.to_string()))
            }
            ("getlastmodified", NodeData::Card(card)) => {
                card.lastmodified.map(|ts| PropValue::Text(http_date(ts)))
            }
            ("getcontenttype", NodeData::Card(_)) => {
                Some(PropValue::Text("text/vcard; charset=utf-8".to_string()))
            }
            _ => None,
        },
        NS_CARDDAV => match (local, node) {
            ("addressbook-description", NodeData::Book(book)) => {
                book.book.description.clone().map(PropValue::Text)
            }
            ("max-resource-size", NodeData::Book(_)) => {
                Some(PropValue::Text(MAX_RESOURCE_SIZE.to_string()))
            }
            ("supported-address-data", NodeData::Book(_)) => Some(PropValue::Elements(vec![
                XmlElement::new("card:address-data-type")
                    .attr("content-type", "text/vcard")
                    .attr("version", "3.0"),
                XmlElement::new("card:address-data-type")
                    .attr("content-type", "text/vcard")
                    .attr("version", "4.0"),
                XmlElement::new("card:address-data-type")
                    .attr("content-type", "application/vcard+json")
                    .attr("version", "4.0"),
            ])),
            ("supported-collation-set", NodeData::Book(_)) => Some(PropValue::Elements(vec![
                XmlElement::new("card:supported-collation").text("i;ascii-casemap"),
                XmlElement::new("card:supported-collation").text("i;octet"),
                XmlElement::new("card:supported-collation").text("i;unicode-casemap"),
            ])),
            ("address-data", NodeData::Card(card)) => Some(PropValue::Text(
                String::from_utf8_lossy(&card.carddata).into_owned(),
            )),
            _ => None,
        },
        NS_CALENDARSERVER => match (local, node) {
            ("getctag", NodeData::Book(book)) => {
                Some(PropValue::Text(book.book.synctoken.to_string()))
            }
            _ => None,
        },
        NS_SABREDAV => match (local, node) {
            ("sync-token", NodeData::Book(book)) => {
                Some(PropValue::Text(book.book.synctoken.to_string()))
            }
            _ => None,
        },
        NS_OWNCLOUD => match (local, node) {
            ("groups", NodeData::Book(_)) => Some(PropValue::Elements(
                ctx.groups
                    .iter()
                    .map(|group| XmlElement::new("oc:group").text(group.clone()))
                    .collect(),
            )),
            // `{oc}owner-principal` / `{oc}read-only` are set by the backend
            // only for shared books; an owned book answers 404 for both.
            ("owner-principal", NodeData::Book(_)) => {
                ctx.owner_principal.clone().map(PropValue::Text)
            }
            ("read-only", NodeData::Book(_)) => ctx.owner_principal.as_ref().map(|_| {
                // PHP serialises the bool: `1` for a read-only share and the
                // empty string for a read-write one (`(string) false === ''`).
                PropValue::Text(if ctx.read_only {
                    "1".to_string()
                } else {
                    String::new()
                })
            }),
            _ => None,
        },
        NS_NEXTCLOUD => match (local, node) {
            ("owner-displayname", NodeData::Book(_)) => {
                Some(PropValue::Text(ctx.owner_displayname.clone()))
            }
            ("has-photo", NodeData::Card(card)) => {
                Some(PropValue::Text(if vcard::has_photo(&card.carddata) {
                    "1".to_string()
                } else {
                    String::new()
                }))
            }
            _ => None,
        },
        _ => None,
    }
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

fn privilege_set(collection: bool) -> PropValue {
    let mut privileges = vec![
        "read",
        "read-acl",
        "read-current-user-privilege-set",
        "write",
        "write-properties",
        "write-content",
        "unlock",
    ];
    if collection {
        privileges.extend(["bind", "unbind", "write-acl"]);
    }
    PropValue::Elements(
        privileges
            .iter()
            .map(|name| XmlElement::new("d:privilege").child(XmlElement::new(format!("d:{name}"))))
            .collect(),
    )
}

/// The `{DAV:}current-user-privilege-set` of a **read-only** shared book.
///
/// `AddressBook::getACL()` grants the sharee `{DAV:}read`, and
/// `Backend::applyShareAcl()` adds `{DAV:}write-properties` for a read-only
/// address book. Sabre aggregates `read-acl` and
/// `read-current-user-privilege-set` from `{DAV:}read`.
fn read_only_privilege_set() -> PropValue {
    let privileges = [
        "read",
        "read-acl",
        "read-current-user-privilege-set",
        "write-properties",
    ];
    PropValue::Elements(
        privileges
            .iter()
            .map(|name| XmlElement::new("d:privilege").child(XmlElement::new(format!("d:{name}"))))
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// GET / HEAD
// ---------------------------------------------------------------------------

/// What the conditional-request evaluation decides for a GET/HEAD.
enum GetPrecondition {
    Pass,
    /// 304 with the `ETag` and `Last-Modified` of the current representation.
    NotModified(Response),
    /// 412 with Sabre's `PreconditionFailed` body (and its `ETag` header
    /// where Sabre sets one).
    Failed(Response),
}

/// Evaluates `If-Match` / `If-None-Match` / `If-Modified-Since` /
/// `If-Unmodified-Since` for a GET/HEAD on a card, following **RFC 7232**
/// where Sabre's `checkPreconditions()`
/// (`3rdparty/sabre/dav/lib/DAV/Server.php:1289`) diverges from it. The
/// divergences are deliberate and declared in `tests/deviations.toml`
/// (`conditional-get-missing`):
///
/// - `If-None-Match` uses RFC 7232 *weak* comparison (Sabre compares entity
///   tags as raw strings, so `W/"x"` never matches `"x"`);
/// - a `HEAD` with a matching `If-None-Match` is a `304` (Sabre tests
///   `'GET' === $method` literally and answers `412`, because
///   `checkPreconditions()` runs before `httpHead()` rewrites the method);
/// - every `304` carries `ETag` and `Last-Modified` (Sabre omits `ETag` for a
///   bare `If-None-Match: *`).
///
/// Keeping Sabre's semantics: strict `If-Match` evaluation (plus its legacy
/// Evolution `\"` workaround), `If-Modified-Since` consulted only when
/// `If-None-Match` is absent, unparseable dates silently ignored, and a
/// missing card under `If-Match` is a 412, not a 404.
fn check_get_preconditions(
    headers: &HeaderMap,
    method: &Method,
    card: Option<&Card>,
) -> GetPrecondition {
    if let Some(raw) = header_str(headers, header::IF_MATCH).filter(|raw| !raw.is_empty()) {
        // A missing node is a 412 here, even for `*` (the PHP lookup happens
        // before the `*` check).
        let Some(card) = card else {
            return GetPrecondition::Failed(dav_error::precondition_failed("If-Match"));
        };
        let quoted = card.quoted_etag();
        if raw != "*" {
            // Strong comparison (RFC 7232 §3.1): a weak validator never
            // matches — plain string equality against the wire ETag gives
            // exactly that. The second arm is Sabre's workaround for Evolution
            // prepending the closing quote with a backslash.
            let matched = raw.split(',').any(|item| {
                let item = item.trim_matches(' ');
                item == quoted || item.replace("\\\"", "\"") == quoted
            });
            if !matched {
                let mut response = dav_error::precondition_failed("If-Match");
                if let Ok(value) = HeaderValue::from_str(&quoted) {
                    response.headers_mut().insert(header::ETAG, value);
                }
                return GetPrecondition::Failed(response);
            }
        }
    }

    let if_none_match = header_str(headers, header::IF_NONE_MATCH).filter(|raw| !raw.is_empty());
    if let Some(raw) = if_none_match {
        let matched = match card {
            // A missing node skips the whole block in PHP (`$nodeExists`).
            None => false,
            Some(card) => {
                if raw == "*" {
                    true
                } else {
                    // RFC 7232 §3.2: weak comparison.
                    let quoted = card.quoted_etag();
                    let current = weak_tag(&quoted);
                    raw.split(',').any(|item| weak_tag(item.trim_matches(' ')) == current)
                }
            }
        };
        if matched {
            // RFC 7232 §6: HEAD follows GET semantics.
            if *method == Method::GET || *method == Method::HEAD {
                return GetPrecondition::NotModified(not_modified_response(card));
            }
            let mut response = dav_error::precondition_failed("If-None-Match");
            if let Some(etag) = card.and_then(|c| HeaderValue::from_str(&c.quoted_etag()).ok()) {
                response.headers_mut().insert(header::ETAG, etag);
            }
            return GetPrecondition::Failed(response);
        }
    }

    if if_none_match.is_none() {
        if let Some(raw) = header_str(headers, header::IF_MODIFIED_SINCE).filter(|raw| !raw.is_empty())
        {
            if let (Some(date), Some(card)) = (parse_http_date(raw), card) {
                if let Some(lastmodified) = card.lastmodified {
                    if lastmodified <= date {
                        return GetPrecondition::NotModified(not_modified_response(Some(card)));
                    }
                }
            }
        }
    }

    if let Some(raw) =
        header_str(headers, header::IF_UNMODIFIED_SINCE).filter(|raw| !raw.is_empty())
    {
        if let (Some(date), Some(card)) = (parse_http_date(raw), card) {
            if let Some(lastmodified) = card.lastmodified {
                if lastmodified > date {
                    return GetPrecondition::Failed(
                        dav_error::precondition_failed("If-Unmodified-Since"),
                    );
                }
            }
        }
    }

    GetPrecondition::Pass
}

/// RFC 7232 §2.3.2 weak comparison: strip the `W/` marker from both sides.
fn weak_tag(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// The `304` response: empty body, with both validators advertised.
fn not_modified_response(card: Option<&Card>) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .body(Body::empty())
        .unwrap();
    if let Some(card) = card {
        if let Ok(value) = HeaderValue::from_str(&card.quoted_etag()) {
            response.headers_mut().insert(header::ETAG, value);
        }
        if let Some(lastmodified) = card.lastmodified {
            if let Ok(value) = HeaderValue::from_str(&http_date(lastmodified)) {
                response.headers_mut().insert(header::LAST_MODIFIED, value);
            }
        }
    }
    response
}

async fn get_card(
    state: &AppState,
    user: &str,
    book_uri: &str,
    card_uri: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Response> {
    let book = resolve_book(state, user, book_uri).await?;
    let card = match &book {
        Some(book) => state.db.card(book.book.id, card_uri).await?,
        None => None,
    };
    // `Server::invokeMethod()` runs `checkPreconditions()` before dispatching
    // to `CorePlugin::httpGet()`, so a missing node under `If-Match` is a 412,
    // not a 404 — resolve first, answer second.
    match check_get_preconditions(headers, method, card.as_ref()) {
        GetPrecondition::Pass => {}
        GetPrecondition::NotModified(response) | GetPrecondition::Failed(response) => {
            return Ok(response)
        }
    }
    let Some(card) = card else {
        return Ok(Error::NotFound.into_response());
    };

    let mut response = Response::new(Body::from(card.carddata.clone()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/vcard; charset=utf-8"),
    );
    if let Ok(etag) = HeaderValue::from_str(&card.quoted_etag()) {
        response.headers_mut().insert(header::ETAG, etag);
    }
    if let Some(lastmodified) = card.lastmodified {
        if let Ok(value) = HeaderValue::from_str(&http_date(lastmodified)) {
            response.headers_mut().insert(header::LAST_MODIFIED, value);
        }
    }
    if *method == Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// PUT / DELETE (native writes)
// ---------------------------------------------------------------------------

/// Evaluates the `If-Match` / `If-None-Match` preconditions against the current
/// card. Returns the 412 response when a precondition fails.
///
/// Mirrors `Sabre\DAV\Server::checkPreconditions()`: `If-Match` is evaluated
/// first (and fails if the resource is absent unless the value is `*`), then
/// `If-None-Match` (fails if the value matches or is `*` on an existing
/// resource). Both compare against the stored unquoted ETag and its quoted
/// wire form.
fn check_preconditions(headers: &HeaderMap, card: Option<&Card>) -> Option<Response> {
    if let Some(raw) = header_str(headers, header::IF_MATCH) {
        let Some(card) = card else {
            return Some(dav_error::precondition_failed("If-Match"));
        };
        if raw.trim() != "*" {
            let matched = raw.split(',').any(|item| {
                let item = item.trim();
                item == card.etag
                    || item == card.quoted_etag()
                    || item.replace("\\\"", "\"") == card.quoted_etag()
            });
            if !matched {
                return Some(dav_error::precondition_failed("If-Match"));
            }
        }
    }

    if let Some(raw) = header_str(headers, header::IF_NONE_MATCH) {
        if let Some(card) = card {
            let raw = raw.trim();
            let matched = raw == "*"
                || raw.split(',').any(|item| {
                    let item = item.trim();
                    item == card.etag || item == card.quoted_etag()
                });
            if matched {
                return Some(dav_error::precondition_failed("If-None-Match"));
            }
        }
    }

    None
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[allow(clippy::too_many_arguments)]
async fn put_card(
    state: &AppState,
    user: &str,
    book_uri: &str,
    card_uri: &str,
    href: &str,
    headers: &HeaderMap,
    request: Request,
) -> Result<Response> {
    let Some(book) = resolve_book(state, user, book_uri).await? else {
        return Ok(Error::NotFound.into_response());
    };

    // A read-only share is invisible to a write (Nextcloud's DavAclPlugin hides
    // existence from non-owners): 404, before reading the body, with no DB
    // write and no outbox row. Reads on a read-only share are allowed.
    if book.read_only {
        return Ok(Error::NotFound.into_response());
    }

    let body = read_body(request).await?;

    // `CardDavValidatePlugin::beforePut()`: 403 once the *actual* bytes read
    // exceed the configured limit (not merely `Content-Length`).
    if body.len() as u64 > state.card_size_limit {
        return Ok(dav_error::forbidden(&format!(
            "VCard object exceeds {} bytes",
            state.card_size_limit
        )));
    }

    let existing = state.db.card(book.book.id, card_uri).await?;
    if let Some(response) = check_preconditions(headers, existing.as_ref()) {
        return Ok(response);
    }

    // Sabre's `Reader::read('')` raises a parse error. The empty body is called
    // out separately and answered with a plain-text 415.
    if body.is_empty() {
        return Ok((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "This resource only supports valid vCard data; the request body is empty.\n",
        )
            .into_response());
    }

    let validated = match vcard_validate::validate(&body) {
        Ok(validated) => validated,
        Err(Reject::UnsupportedMediaType(message)) => {
            return Ok(dav_error::unsupported_media_type(&message));
        }
        Err(Reject::BadRequest(message)) => return Ok(dav_error::bad_request(&message)),
    };

    // RFC 6352 6.3.2.1 no-uid-conflict, on CREATE only. Sabre's
    // `AddressBook::createFile()` calls `createCard()` with the check enabled,
    // but `Card::put()` calls `updateCard()`, which does *not* check
    // (`apps/dav/lib/CardDAV/CardDavBackend.php`). Matching that means an update
    // is allowed to introduce a duplicate UID, exactly as PHP allows. Checking
    // here on update too would reject writes a real Nextcloud accepts.
    if existing.is_none() {
        if let Some((_id, existing_uri)) =
            state.db.card_by_uid(book.book.id, &validated.uid).await?
        {
            let collection = href.rsplit_once('/').map(|(base, _)| base).unwrap_or(href);
            let conflict_href = format!("{collection}/{}", encode_path_segment(&existing_uri));
            return Ok(dav_error::uid_conflict(&conflict_href));
        }
    }

    let created = existing.is_none();
    let snapshot = state
        .db
        .put_card(
            book.book.id,
            card_uri,
            &validated.data,
            &validated.uid,
            !created,
            &state.registry,
            &state.config.event_dispatch.notify_channel,
        )
        .await?;

    let mut response = Response::new(Body::empty());
    *response.status_mut() = if created {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    };
    if let Ok(etag) = HeaderValue::from_str(&snapshot.quoted_etag()) {
        response.headers_mut().insert(header::ETAG, etag);
    }
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    Ok(response)
}

async fn delete_card(
    state: &AppState,
    user: &str,
    book_uri: &str,
    card_uri: &str,
    headers: &HeaderMap,
) -> Result<Response> {
    let Some(book) = resolve_book(state, user, book_uri).await? else {
        return Ok(Error::NotFound.into_response());
    };
    // Read-only share: 404, never 403, and no DB write (see `put_card`).
    if book.read_only {
        return Ok(Error::NotFound.into_response());
    }
    let Some(card) = state.db.card(book.book.id, card_uri).await? else {
        return Ok(Error::NotFound.into_response());
    };
    if let Some(response) = check_preconditions(headers, Some(&card)) {
        return Ok(response);
    }

    let deleted = state
        .db
        .delete_card(
            book.book.id,
            card_uri,
            &state.registry,
            &state.config.event_dispatch.notify_channel,
        )
        .await?;
    if deleted.is_none() {
        return Ok(Error::NotFound.into_response());
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    Ok(response)
}

// ---------------------------------------------------------------------------
// REPORT
// ---------------------------------------------------------------------------

async fn handle_report_book(
    state: &AppState,
    parsed: &ParsedPath,
    user: &str,
    book_uri: &str,
    href: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response> {
    let Some(book) = resolve_book(state, user, book_uri).await? else {
        return Ok(Error::NotFound.into_response());
    };
    let ctx = book_context(state, parsed, user, &book).await?;

    if body.is_empty() {
        return Ok(Error::bad_request("empty REPORT body").into_response());
    }
    let document = parse::parse_document(body)?;
    match (document.ns.as_str(), document.local.as_str()) {
        (NS_CARDDAV, "addressbook-multiget") => {
            let request = parse::parse_multiget(body)?;
            let cards = state.db.cards(book.book.id).await?;
            let mut responses = Vec::new();
            // Sabre resolves hrefs, preserving the client's order and returning
            // a 404 propstat for hrefs that do not resolve.
            let by_uri: std::collections::HashMap<String, &Card> =
                cards.iter().map(|card| (card.uri.clone(), card)).collect();
            for raw_href in &request.hrefs {
                let card_uri = percent_decode(last_segment(raw_href));
                let response_href = normalize_href(raw_href, href);
                match by_uri.get(&card_uri) {
                    Some(card) => {
                        let rendered =
                            match render_address_data(card, &request.props, &request.address_data) {
                                Ok(rendered) => rendered,
                                Err(message) => {
                                    return Ok(dav_error::vobject_parse_error(&message))
                                }
                            };
                        responses.push(build_response_with_data(
                            &response_href,
                            NodeData::Card(card),
                            &ctx,
                            &PropList::Props(request.props.clone()),
                            rendered.as_deref(),
                        ));
                    }
                    None => {
                        let missing: Vec<PropQName> = if request.props.is_empty() {
                            vec![PropQName::dav("getetag")]
                        } else {
                            request.props.clone()
                        };
                        responses.push(DavResponse::props(
                            response_href,
                            vec![PropStat::not_found(missing)],
                        ));
                    }
                }
            }
            Ok(xml_response(
                StatusCode::MULTI_STATUS,
                MultiStatus {
                    responses,
                    sync_token: None,
                }
                .to_xml(),
            ))
        }
        (NS_CARDDAV, "addressbook-query") => {
            let request = parse::parse_query(body)?;
            let depth = parse_depth(headers);
            // `CardDAV\Plugin::addressbookQueryReport()`: Depth: 0 on a
            // collection is only valid when the target itself is an ICard;
            // otherwise Sabre raises ReportNotSupported (415).
            if depth == 0 {
                return Ok(dav_error::report_not_supported(
                    "The addressbook-query report is not supported on this url with Depth: 0",
                ));
            }
            let candidates = state.db.cards(book.book.id).await?;

            let mut responses = Vec::new();
            for card in &candidates {
                if let Some(filter) = &request.filter {
                    if !filter::evaluate(&vcard::parse(&card.carddata), filter) {
                        continue;
                    }
                }
                let response_href = format!("{href}/{}", encode_path_segment(&card.uri));
                let rendered =
                    match render_address_data(card, &request.props, &request.address_data) {
                        Ok(rendered) => rendered,
                        Err(message) => return Ok(dav_error::vobject_parse_error(&message)),
                    };
                responses.push(build_response_with_data(
                    &response_href,
                    NodeData::Card(card),
                    &ctx,
                    &PropList::Props(request.props.clone()),
                    rendered.as_deref(),
                ));
                if let Some(limit) = request.limit {
                    if responses.len() as i64 >= limit {
                        break;
                    }
                }
            }
            Ok(xml_response(
                StatusCode::MULTI_STATUS,
                MultiStatus {
                    responses,
                    sync_token: None,
                }
                .to_xml(),
            ))
        }
        (NS_DAV, "sync-collection") => {
            let request = parse::parse_sync_collection(body)?;
            handle_sync_collection(state, &book.book, href, &ctx, &request).await
        }
        (ns, local) => {
            log::debug!("unsupported report {{{ns}}}{local}");
            Ok(not_implemented())
        }
    }
}

/// Renders the `{urn:ietf:params:xml:ns:carddav}address-data` value of a
/// REPORT response through `CardDAV\Plugin::convertVCard`
/// (`3rdparty/sabre/dav/lib/CardDAV/Plugin.php:803-855`): negotiated version /
/// content-type and the `<card:prop>` filter. `Ok(None)` when the request does
/// not ask for `address-data`; `Err` when the stored card does not parse (an
/// HTTP 500 in PHP, `DAV/Server.php:254-309`).
fn render_address_data(
    card: &Card,
    props: &[PropQName],
    request: &parse::AddressDataRequest,
) -> std::result::Result<Option<String>, String> {
    let wanted = props
        .iter()
        .any(|q| q.ns == NS_CARDDAV && q.local == "address-data");
    if !wanted {
        return Ok(None);
    }
    crate::vobject::convert_vcard(
        &card.carddata,
        request.content_type.as_deref(),
        request.version.as_deref(),
        &request.properties,
    )
    .map(Some)
    .map_err(|error| error.to_string())
}

async fn handle_report_card(
    state: &AppState,
    parsed: &ParsedPath,
    user: &str,
    book_uri: &str,
    card_uri: &str,
    href: &str,
    body: &[u8],
) -> Result<Response> {
    let Some(book) = resolve_book(state, user, book_uri).await? else {
        return Ok(Error::NotFound.into_response());
    };
    let Some(card) = state.db.card(book.book.id, card_uri).await? else {
        return Ok(Error::NotFound.into_response());
    };
    let document = parse::parse_document(body)?;
    if document.ns == NS_CARDDAV && document.local == "addressbook-query" {
        let request = parse::parse_query(body)?;
        let ctx = book_context(state, parsed, user, &book).await?;
        let matches = request
            .filter
            .as_ref()
            .map(|filter| filter::evaluate(&vcard::parse(&card.carddata), filter))
            .unwrap_or(true);
        let responses = if matches {
            let rendered = match render_address_data(&card, &request.props, &request.address_data) {
                Ok(rendered) => rendered,
                Err(message) => return Ok(dav_error::vobject_parse_error(&message)),
            };
            vec![build_response_with_data(
                href,
                NodeData::Card(&card),
                &ctx,
                &PropList::Props(request.props),
                rendered.as_deref(),
            )]
        } else {
            Vec::new()
        };
        return Ok(xml_response(
            StatusCode::MULTI_STATUS,
            MultiStatus {
                responses,
                sync_token: None,
            }
            .to_xml(),
        ));
    }
    Ok(not_implemented())
}

async fn handle_sync_collection(
    state: &AppState,
    book: &AddressBook,
    href: &str,
    ctx: &PropContext,
    request: &parse::SyncCollectionRequest,
) -> Result<Response> {
    let limit = request
        .limit
        .map(|limit| limit.min(DEFAULT_SYNC_LIMIT))
        .unwrap_or(DEFAULT_SYNC_LIMIT)
        .max(1);
    let token = sync::parse_sync_token(request.sync_token.as_deref())?;

    let page = match token {
        sync::SyncToken::Initial => {
            let rows = state.db.sync_initial_cards(book.id, 0, limit).await?;
            sync::initial_sync(&rows, book.synctoken, limit)
        }
        sync::SyncToken::InitialPaging { last_id, token } => {
            let rows = state.db.sync_initial_cards(book.id, last_id, limit).await?;
            sync::initial_sync_continue(&rows, token, limit)
        }
        sync::SyncToken::Changes(from) => {
            let rows = state
                .db
                .sync_changes(book.id, from, book.synctoken, limit)
                .await?;
            sync::changes_sync(&rows, book.synctoken, limit)
        }
    };

    let mut responses = Vec::new();
    let mut changed_uris = page.added.clone();
    changed_uris.extend(page.modified.clone());
    if !changed_uris.is_empty() {
        let cards = state.db.cards_by_uris(book.id, &changed_uris).await?;
        let by_uri: std::collections::HashMap<&str, &Card> =
            cards.iter().map(|card| (card.uri.as_str(), card)).collect();
        for uri in &changed_uris {
            if let Some(card) = by_uri.get(uri.as_str()) {
                let card_href = format!("{href}/{}", encode_path_segment(uri));
                responses.push(build_response(
                    &card_href,
                    NodeData::Card(card),
                    ctx,
                    &PropList::Props(request.props.clone()),
                ));
            }
        }
    }
    for uri in &page.deleted {
        let card_href = format!("{href}/{}", encode_path_segment(uri));
        responses.push(DavResponse::status(card_href, 404));
    }
    if page.truncated {
        responses.push(DavResponse::status(format!("{href}/"), 507));
    }

    Ok(xml_response(
        StatusCode::MULTI_STATUS,
        MultiStatus {
            responses,
            sync_token: Some(format!("{SYNCTOKEN_PREFIX}{}", page.sync_token)),
        }
        .to_xml(),
    ))
}

fn last_segment(href: &str) -> &str {
    href.split('/').rfind(|s| !s.is_empty()).unwrap_or("")
}

/// Turns a client-provided href into an absolute path we can echo back.
fn normalize_href(raw: &str, collection_href: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        if let Ok(url) = reqwest::Url::parse(raw) {
            return url.path().to_string();
        }
    }
    if raw.starts_with('/') {
        return raw.to_string();
    }
    format!("{collection_href}/{}", raw.trim_start_matches('/'))
}

fn xml_response(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    // Every `xml_response` is a body the sidecar produced itself (a native
    // multistatus). The `501` delegation path uses `not_implemented()` and must
    // stay indistinguishable from PHP.
    response
        .headers_mut()
        .insert(SIDECAR_HEADER, HeaderValue::from_static(SIDECAR_VALUE));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_home_book_and_card() {
        let parsed = parse_path("/remote.php/dav/addressbooks/users/alice");
        assert_eq!(
            parsed.target,
            DavTarget::Home {
                user: "alice".into(),
                href: "/remote.php/dav/addressbooks/users/alice".into()
            }
        );
        let parsed = parse_path("/remote.php/dav/addressbooks/users/alice/contacts/");
        assert_eq!(
            parsed.target,
            DavTarget::Book {
                user: "alice".into(),
                book_uri: "contacts".into(),
                href: "/remote.php/dav/addressbooks/users/alice/contacts".into()
            }
        );
        let parsed = parse_path("/remote.php/dav/addressbooks/users/alice/contacts/1234.vcf");
        assert_eq!(
            parsed.target,
            DavTarget::Card {
                user: "alice".into(),
                book_uri: "contacts".into(),
                card_uri: "1234.vcf".into(),
                href: "/remote.php/dav/addressbooks/users/alice/contacts/1234.vcf".into()
            }
        );
    }

    #[test]
    fn parses_with_webroot_and_percent_encoding() {
        let parsed = parse_path("/nextcloud/remote.php/dav/addressbooks/users/al%20ice/contacts");
        assert_eq!(parsed.context, "/nextcloud/remote.php/dav/addressbooks");
        match parsed.target {
            DavTarget::Book { user, .. } => assert_eq!(user, "al ice"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_system_and_unknown_homes() {
        assert_eq!(
            parse_path("/remote.php/dav/addressbooks/system/system/system").target,
            DavTarget::NotFound
        );
        assert_eq!(
            parse_path("/remote.php/dav/calendars/users/a").target,
            DavTarget::NotFound
        );
        assert_eq!(
            parse_path("/remote.php/dav/addressbooks/users/alice/c/x/y").target,
            DavTarget::NotFound
        );
    }

    #[test]
    fn normalizes_hrefs() {
        assert_eq!(
            normalize_href("/remote.php/dav/addressbooks/users/a/b/x.vcf", "/base"),
            "/remote.php/dav/addressbooks/users/a/b/x.vcf"
        );
        assert_eq!(normalize_href("x.vcf", "/a/b"), "/a/b/x.vcf");
        assert_eq!(
            normalize_href("https://cloud.example.com/remote.php/dav/x", "/a"),
            "/remote.php/dav/x"
        );
    }

    #[test]
    fn last_segment_ignores_trailing_slash() {
        assert_eq!(last_segment("/a/b/c.vcf/"), "c.vcf");
        assert_eq!(last_segment("c.vcf"), "c.vcf");
    }

    fn test_book() -> VisibleBook {
        VisibleBook::owned(AddressBook {
            id: 1,
            uri: "contacts".into(),
            displayname: Some("Contacts".into()),
            principaluri: "principals/users/alice".into(),
            description: None,
            synctoken: 5,
        })
    }

    fn test_shared_book(read_only: bool) -> VisibleBook {
        VisibleBook {
            book: AddressBook {
                id: 2,
                uri: "bobcontacts".into(),
                displayname: Some("Bob Contacts".into()),
                principaluri: "principals/users/bob".into(),
                description: None,
                synctoken: 1,
            },
            wire_uri: "bobcontacts_shared_by_bob".into(),
            wire_displayname: Some("Bob Contacts (Bob Builder)".into()),
            owner_principal: Some("principals/users/bob".into()),
            read_only,
        }
    }

    fn test_ctx() -> PropContext {
        PropContext {
            principal_href: "/remote.php/dav/principals/users/alice/".into(),
            owner_displayname: "Alice".into(),
            owner_principal: None,
            read_only: false,
            groups: vec!["Friends".into()],
        }
    }

    #[test]
    fn book_sync_token_properties() {
        let book = test_book();
        let ctx = test_ctx();
        assert_eq!(
            resolve_property(
                &PropQName::new(NS_SABREDAV, "sync-token"),
                &NodeData::Book(&book),
                &ctx
            ),
            Some(PropValue::Text("5".into()))
        );
        assert_eq!(
            resolve_property(&PropQName::dav("sync-token"), &NodeData::Book(&book), &ctx),
            Some(PropValue::Text(format!("{SYNCTOKEN_PREFIX}5")))
        );
        assert_eq!(
            resolve_property(
                &PropQName::new(NS_CALENDARSERVER, "getctag"),
                &NodeData::Book(&book),
                &ctx
            ),
            Some(PropValue::Text("5".into()))
        );
    }

    #[test]
    fn shared_book_properties_are_owner_scoped() {
        let book = test_shared_book(true);
        let mut ctx = test_ctx();
        ctx.principal_href = "/remote.php/dav/principals/users/bob/".into();
        ctx.owner_displayname = "Bob Builder".into();
        ctx.owner_principal = Some("principals/users/bob".into());
        ctx.read_only = true;

        assert_eq!(
            resolve_property(&PropQName::dav("displayname"), &NodeData::Book(&book), &ctx),
            Some(PropValue::Text("Bob Contacts (Bob Builder)".into()))
        );
        assert_eq!(
            resolve_property(&PropQName::dav("owner"), &NodeData::Book(&book), &ctx),
            Some(PropValue::Elements(vec![
                XmlElement::new("d:href").text("/remote.php/dav/principals/users/bob/")
            ]))
        );
        assert_eq!(
            resolve_property(
                &PropQName::nextcloud("owner-displayname"),
                &NodeData::Book(&book),
                &ctx
            ),
            Some(PropValue::Text("Bob Builder".into()))
        );
        assert_eq!(
            resolve_property(
                &PropQName::owncloud("owner-principal"),
                &NodeData::Book(&book),
                &ctx
            ),
            Some(PropValue::Text("principals/users/bob".into()))
        );
        assert_eq!(
            resolve_property(
                &PropQName::owncloud("read-only"),
                &NodeData::Book(&book),
                &ctx
            ),
            Some(PropValue::Text("1".into()))
        );

        let privileges = resolve_property(
            &PropQName::dav("current-user-privilege-set"),
            &NodeData::Book(&book),
            &ctx,
        )
        .unwrap();
        let PropValue::Elements(children) = privileges else {
            panic!("expected element privileges");
        };
        let names: Vec<String> = children
            .iter()
            .filter_map(|child| child.children.first().map(|p| p.name.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                "d:read",
                "d:read-acl",
                "d:read-current-user-privilege-set",
                "d:write-properties",
            ]
        );
    }

    #[test]
    fn read_write_share_serialises_an_empty_read_only() {
        let book = test_shared_book(false);
        let mut ctx = test_ctx();
        ctx.owner_principal = Some("principals/users/bob".into());
        assert_eq!(
            resolve_property(
                &PropQName::owncloud("read-only"),
                &NodeData::Book(&book),
                &ctx
            ),
            Some(PropValue::Text(String::new()))
        );
        assert!(!ctx.read_only);
    }

    #[test]
    fn owned_book_has_no_sharing_properties() {
        let book = test_book();
        let ctx = test_ctx();
        assert_eq!(
            resolve_property(
                &PropQName::owncloud("owner-principal"),
                &NodeData::Book(&book),
                &ctx
            ),
            None
        );
        assert_eq!(
            resolve_property(
                &PropQName::owncloud("read-only"),
                &NodeData::Book(&book),
                &ctx
            ),
            None
        );
    }

    #[test]
    fn unknown_property_is_missing() {
        let book = test_book();
        let ctx = test_ctx();
        assert_eq!(
            resolve_property(
                &PropQName::new("http://example.com/ns", "nope"),
                &NodeData::Book(&book),
                &ctx
            ),
            None
        );
    }

    #[test]
    fn card_etag_is_quoted() {
        let card = Card {
            id: 1,
            uri: "x.vcf".into(),
            etag: "abc".into(),
            size: 3,
            lastmodified: Some(1_700_000_000),
            carddata: b"abc".to_vec(),
        };
        let ctx = test_ctx();
        assert_eq!(
            resolve_property(&PropQName::dav("getetag"), &NodeData::Card(&card), &ctx),
            Some(PropValue::Text("\"abc\"".into()))
        );
        assert_eq!(
            resolve_property(
                &PropQName::new(NS_NEXTCLOUD, "has-photo"),
                &NodeData::Card(&card),
                &ctx
            ),
            Some(PropValue::Text(String::new()))
        );
    }

    #[test]
    fn propfind_response_has_200_and_404_propstats() {
        let book = test_book();
        let ctx = test_ctx();
        let props = PropList::Props(vec![
            PropQName::dav("displayname"),
            PropQName::dav("getetag"),
        ]);
        let response = build_response(
            "/remote.php/dav/addressbooks/users/alice/contacts",
            NodeData::Book(&book),
            &ctx,
            &props,
        );
        assert_eq!(response.propstats.len(), 2);
        assert_eq!(response.propstats[0].status, 200);
        assert_eq!(response.propstats[0].props.len(), 1);
        assert_eq!(response.propstats[1].status, 404);
        assert_eq!(response.propstats[1].props[0].0, PropQName::dav("getetag"));
    }

    #[test]
    fn propname_returns_empty_values() {
        let book = test_book();
        let ctx = test_ctx();
        let props = PropList::PropName;
        let response = build_response(
            "/remote.php/dav/addressbooks/users/alice/contacts",
            NodeData::Book(&book),
            &ctx,
            &props,
        );
        let ok = response
            .propstats
            .iter()
            .find(|p| p.status == 200)
            .expect("a 200 propstat");
        assert!(!ok.props.is_empty());
        assert!(ok.props.iter().all(|(_, value)| *value == PropValue::Empty));
    }

    #[test]
    fn depth_defaults_to_zero_and_clamps_infinity() {
        let mut headers = HeaderMap::new();
        assert_eq!(parse_depth(&headers), 0);
        headers.insert("depth", HeaderValue::from_static("1"));
        assert_eq!(parse_depth(&headers), 1);
        headers.insert("depth", HeaderValue::from_static("infinity"));
        assert_eq!(parse_depth(&headers), 1);
        headers.insert("depth", HeaderValue::from_static("bogus"));
        assert_eq!(parse_depth(&headers), 0);
    }

    #[test]
    fn client_ip_prefers_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 10.0.0.1"),
        );
        assert_eq!(
            client_ip(&headers),
            IpAddr::from_str("203.0.113.9").unwrap()
        );
        headers.remove("x-forwarded-for");
        headers.insert("x-real-ip", HeaderValue::from_static("198.51.100.4"));
        assert_eq!(
            client_ip(&headers),
            IpAddr::from_str("198.51.100.4").unwrap()
        );
        headers.remove("x-real-ip");
        assert_eq!(client_ip(&headers), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn photo_and_export_delegate_to_php() {
        assert!(query_requires_php("photo"));
        assert!(query_requires_php("photo&size=64"));
        assert!(query_requires_php("export"));
        assert!(!query_requires_php(""));
    }

    #[test]
    fn webcal_caching_switch_matches_the_plugin() {
        let mut headers = HeaderMap::new();
        assert!(!webcal_caching_enabled(&headers));
        headers.insert(
            "x-nc-caldav-webcal-caching",
            HeaderValue::from_static("On"),
        );
        assert!(webcal_caching_enabled(&headers));
        headers.remove("x-nc-caldav-webcal-caching");
        for ua in ["KIO/5.0", "Evolution/3.44", "MSFT-WIN-3/10.0"] {
            headers.insert(header::USER_AGENT, HeaderValue::from_str(ua).unwrap());
            assert!(webcal_caching_enabled(&headers), "{ua}");
        }
        headers.insert(header::USER_AGENT, HeaderValue::from_static("Mozilla/5.0"));
        assert!(!webcal_caching_enabled(&headers));
    }
}
