// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP routing and the CardDAV protocol surface (read-only).
//!
//! Only `/remote.php/dav/addressbooks/users/<user>/...` is handled. Writes are
//! answered with `501 Not Implemented` so nginx can fall back to PHP (design doc
//! §5.2, §5.3).

use crate::auth::{AuthError, Authenticator};
use crate::config::{Config, DEFAULT_SYNC_LIMIT, MAX_RESOURCE_SIZE};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::model::{AddressBook, Card};
use crate::sync::{self, SYNCTOKEN_PREFIX};
use crate::util::{encode_path_segment, http_date, parse_basic_auth, percent_decode};
use crate::vcard;
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

pub struct AppState {
    pub db: Arc<Db>,
    pub auth: Authenticator,
    pub config: Config,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/remote.php/dav/addressbooks", any(dispatch))
        .route("/remote.php/dav/addressbooks/{*rest}", any(dispatch))
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

async fn handle(state: Arc<AppState>, request: Request) -> Result<Response> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    let headers = request.headers().clone();
    let parsed = parse_path(&path);

    // Authenticate every request, including OPTIONS. Unauthenticated OPTIONS
    // still needs the discovery headers (DAV/Allow) before it is refused.
    let Some((username, password)) = parse_basic_auth(headers.get(header::AUTHORIZATION)) else {
        if method == Method::OPTIONS {
            return options_response(&parsed, false);
        }
        return Ok(unauthorized());
    };
    let client_ip = client_ip(&headers);
    let user = match state
        .auth
        .authenticate(&username, &password, client_ip)
        .await
    {
        Ok(user) => user,
        Err(error) => return Ok(auth_error_response(error)),
    };

    if method == Method::OPTIONS {
        return options_response(&parsed, true);
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
            let target_user = target_user.clone();
            let book_uri = book_uri.clone();
            let card_uri = card_uri.clone();
            let href = href.clone();
            match method.as_str() {
                "GET" | "HEAD" => {
                    get_card(&state, &target_user, &book_uri, &card_uri, &method).await
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
        "PUT" => Some("PUT"),
        "DELETE" => Some("DELETE"),
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

fn unauthorized() -> Response {
    Error::Unauthorized.into_response()
}

fn not_implemented() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        "This CardDAV sidecar is read-only; this method is served by Nextcloud PHP.\n",
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

fn options_response(parsed: &ParsedPath, authenticated: bool) -> Result<Response> {
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
    if !authenticated {
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Nextcloud\", charset=\"UTF-8\""),
        );
    }
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

    let mut responses = Vec::new();
    match &parsed.target {
        DavTarget::Home { href, .. } => {
            let ctx = PropContext {
                principal_href: principal_href.clone(),
                owner_displayname: owner_displayname.clone(),
                groups: Vec::new(),
            };
            responses.push(build_response(href, NodeData::Home, &ctx, &request.props));
            if depth >= 1 {
                let books = state.db.address_books_for_user(&principal(user)).await?;
                for book in &books {
                    let book_href = format!("{href}/{}", encode_path_segment(&book.uri));
                    let groups = requested_groups(&request.props, book, &state.db).await?;
                    let ctx = PropContext {
                        principal_href: principal_href.clone(),
                        owner_displayname: owner_displayname.clone(),
                        groups,
                    };
                    responses.push(build_response(
                        &book_href,
                        NodeData::Book(book),
                        &ctx,
                        &request.props,
                    ));
                }
            }
        }
        DavTarget::Book { href, book_uri, .. } => {
            let Some(book) = state
                .db
                .address_book_by_uri(&principal(user), book_uri)
                .await?
            else {
                return Ok(Error::NotFound.into_response());
            };
            let groups = requested_groups(&request.props, &book, &state.db).await?;
            let ctx = PropContext {
                principal_href: principal_href.clone(),
                owner_displayname: owner_displayname.clone(),
                groups,
            };
            responses.push(build_response(
                href,
                NodeData::Book(&book),
                &ctx,
                &request.props,
            ));
            if depth >= 1 {
                let cards = state.db.cards(book.id).await?;
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
            let Some(book) = state
                .db
                .address_book_by_uri(&principal(user), book_uri)
                .await?
            else {
                return Ok(Error::NotFound.into_response());
            };
            let Some(card) = state.db.card(book.id, card_uri).await? else {
                return Ok(Error::NotFound.into_response());
            };
            let ctx = PropContext {
                principal_href: principal_href.clone(),
                owner_displayname,
                groups: Vec::new(),
            };
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

fn build_response(
    href: &str,
    node: NodeData<'_>,
    ctx: &PropContext,
    props: &PropList,
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
        match resolve_property(qname, &node, ctx) {
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
    Book(&'a AddressBook),
    Card(&'a Card),
}

struct PropContext {
    principal_href: String,
    owner_displayname: String,
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
                book.displayname.clone().unwrap_or_else(|| book.uri.clone()),
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
                book.synctoken
            ))),
            ("owner", _) => Some(PropValue::Elements(vec![
                XmlElement::new("d:href").text(ctx.principal_href.clone())
            ])),
            ("current-user-privilege-set", NodeData::Home) => Some(privilege_set(false)),
            ("current-user-privilege-set", NodeData::Book(_)) => Some(privilege_set(true)),
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
                book.description.clone().map(PropValue::Text)
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
            ])),
            ("supported-collation-set", NodeData::Book(_)) => Some(PropValue::Elements(vec![
                XmlElement::new("card:collation").text("i;ascii-casemap"),
                XmlElement::new("card:collation").text("i;octet"),
                XmlElement::new("card:collation").text("i;unicode-casemap"),
            ])),
            ("address-data", NodeData::Card(card)) => Some(PropValue::Text(
                String::from_utf8_lossy(&card.carddata).into_owned(),
            )),
            _ => None,
        },
        NS_CALENDARSERVER => match (local, node) {
            ("getctag", NodeData::Book(book)) => Some(PropValue::Text(book.synctoken.to_string())),
            _ => None,
        },
        NS_SABREDAV => match (local, node) {
            ("sync-token", NodeData::Book(book)) => {
                Some(PropValue::Text(book.synctoken.to_string()))
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
            // Owned books do not carry `oc:owner-principal`; it is only set for
            // shared books and the system book (design doc §2.2).
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

// ---------------------------------------------------------------------------
// GET / HEAD
// ---------------------------------------------------------------------------

async fn get_card(
    state: &AppState,
    user: &str,
    book_uri: &str,
    card_uri: &str,
    method: &Method,
) -> Result<Response> {
    let Some(book) = state
        .db
        .address_book_by_uri(&principal(user), book_uri)
        .await?
    else {
        return Ok(Error::NotFound.into_response());
    };
    let Some(card) = state.db.card(book.id, card_uri).await? else {
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
    let Some(book) = state
        .db
        .address_book_by_uri(&principal(user), book_uri)
        .await?
    else {
        return Ok(Error::NotFound.into_response());
    };
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

    if body.is_empty() {
        return Ok(Error::bad_request("empty REPORT body").into_response());
    }
    let document = parse::parse_document(body)?;
    match (document.ns.as_str(), document.local.as_str()) {
        (NS_CARDDAV, "addressbook-multiget") => {
            let request = parse::parse_multiget(body)?;
            let cards = state.db.cards(book.id).await?;
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
                        let ctx = PropContext {
                            principal_href: principal_href.clone(),
                            owner_displayname: owner_displayname.clone(),
                            groups: Vec::new(),
                        };
                        responses.push(build_response(
                            &response_href,
                            NodeData::Card(card),
                            &ctx,
                            &PropList::Props(request.props.clone()),
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
            let candidates: Vec<Card> = if depth == 0 {
                let card_uri = percent_decode(last_segment(href));
                match state.db.card(book.id, &card_uri).await? {
                    Some(card) => vec![card],
                    None => Vec::new(),
                }
            } else {
                state.db.cards(book.id).await?
            };

            let ctx = PropContext {
                principal_href: principal_href.clone(),
                owner_displayname: owner_displayname.clone(),
                groups: Vec::new(),
            };
            let mut responses = Vec::new();
            for card in &candidates {
                if let Some(filter) = &request.filter {
                    if !filter::evaluate(&vcard::parse(&card.carddata), filter) {
                        continue;
                    }
                }
                let response_href = if depth == 0 {
                    href.to_string()
                } else {
                    format!("{href}/{}", encode_path_segment(&card.uri))
                };
                responses.push(build_response(
                    &response_href,
                    NodeData::Card(card),
                    &ctx,
                    &PropList::Props(request.props.clone()),
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
            handle_sync_collection(
                state,
                &book,
                href,
                &principal_href,
                &owner_displayname,
                user,
                &request,
            )
            .await
        }
        (ns, local) => {
            log::debug!("unsupported report {{{ns}}}{local}");
            Ok(not_implemented())
        }
    }
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
    let Some(book) = state
        .db
        .address_book_by_uri(&principal(user), book_uri)
        .await?
    else {
        return Ok(Error::NotFound.into_response());
    };
    let Some(card) = state.db.card(book.id, card_uri).await? else {
        return Ok(Error::NotFound.into_response());
    };
    let document = parse::parse_document(body)?;
    if document.ns == NS_CARDDAV && document.local == "addressbook-query" {
        let request = parse::parse_query(body)?;
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
        let ctx = PropContext {
            principal_href: principal_href.clone(),
            owner_displayname,
            groups: Vec::new(),
        };
        let matches = request
            .filter
            .as_ref()
            .map(|filter| filter::evaluate(&vcard::parse(&card.carddata), filter))
            .unwrap_or(true);
        let responses = if matches {
            vec![build_response(
                href,
                NodeData::Card(&card),
                &ctx,
                &PropList::Props(request.props),
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
    principal_href: &str,
    owner_displayname: &str,
    _user: &str,
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
    let ctx = PropContext {
        principal_href: principal_href.to_string(),
        owner_displayname: owner_displayname.to_string(),
        groups: Vec::new(),
    };
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
                    &ctx,
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
    href.split('/')
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or("")
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

    fn test_book() -> AddressBook {
        AddressBook {
            id: 1,
            uri: "contacts".into(),
            displayname: Some("Contacts".into()),
            principaluri: "principals/users/alice".into(),
            description: None,
            synctoken: 5,
        }
    }

    fn test_ctx() -> PropContext {
        PropContext {
            principal_href: "/remote.php/dav/principals/users/alice/".into(),
            owner_displayname: "Alice".into(),
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
}
