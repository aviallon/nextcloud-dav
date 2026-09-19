// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Sabre-style DAV error responses.
//!
//! A real SabreDAV 4.7 instance renders every thrown `Sabre\DAV\Exception` as
//! an XML `{DAV:}error` document with the PHP class in `{sabredav}exception`,
//! the message in `{sabredav}message` and any precondition element the
//! exception serialises (e.g. `<d:valid-sync-token/>`,
//! `<card:no-uid-conflict><d:href>…`). The sidecar reproduces that shape for
//! the cases where a client is expected to parse it; plain sidecar-specific
//! bodies (the 501 delegation, auth failures) stay as they were.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;

/// Escapes XML text content and attribute values.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Builds a `{DAV:}error` response.
pub fn dav_error(status: StatusCode, exception: &str, message: &str, extra: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\
         <s:exception>{}</s:exception>\
         <s:message>{}</s:message>\
         {}</d:error>",
        escape(exception),
        escape(message),
        extra
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    // Every `dav_error` is a body the sidecar produced itself (never a
    // delegated 501), so it carries the attribution header.
    response.headers_mut().insert(
        crate::routes::SIDECAR_HEADER,
        HeaderValue::from_static(crate::routes::SIDECAR_VALUE),
    );
    response
}

/// `Sabre\DAV\Exception\InvalidSyncToken` (403 + `<d:valid-sync-token/>`).
pub fn invalid_sync_token() -> Response {
    dav_error(
        StatusCode::FORBIDDEN,
        "Sabre\\DAV\\Exception\\InvalidSyncToken",
        "Invalid or unknown sync token",
        "<d:valid-sync-token/>",
    )
}

/// `OCA\DAV\Exception\UnsupportedLimitOnInitialSyncException` (507 +
/// `<d:number-of-matches-within-limits/>`), thrown by `Calendar::getChanges()`
/// when an initial `sync-collection` carries a `<d:limit>`.
pub fn unsupported_limit_on_initial_sync() -> Response {
    dav_error(
        StatusCode::INSUFFICIENT_STORAGE,
        "OCA\\DAV\\Exception\\UnsupportedLimitOnInitialSyncException",
        "",
        "<d:number-of-matches-within-limits/>",
    )
}

/// `Sabre\DAV\Exception\ReportNotSupported` (415 + `<d:supported-report/>`).
pub fn report_not_supported(message: &str) -> Response {
    dav_error(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "Sabre\\DAV\\Exception\\ReportNotSupported",
        message,
        "<d:supported-report/>",
    )
}

/// `OCA\DAV\Exception\UidConflict` (409 + `<card:no-uid-conflict>`).
pub fn uid_conflict(href: &str) -> Response {
    let extra = format!(
        "<card:no-uid-conflict xmlns:card=\"urn:ietf:params:xml:ns:carddav\">\
         <d:href>{}</d:href></card:no-uid-conflict>",
        escape(href)
    );
    dav_error(
        StatusCode::CONFLICT,
        "OCA\\DAV\\Exception\\UidConflict",
        "VCard object with uid already exists in this addressbook collection.",
        &extra,
    )
}

/// `Sabre\DAV\Exception\UnsupportedMediaType` (415).
pub fn unsupported_media_type(message: &str) -> Response {
    dav_error(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "Sabre\\DAV\\Exception\\UnsupportedMediaType",
        message,
        "",
    )
}

/// `Sabre\DAV\Exception\Forbidden` (403).
pub fn forbidden(message: &str) -> Response {
    dav_error(
        StatusCode::FORBIDDEN,
        "Sabre\\DAV\\Exception\\Forbidden",
        message,
        "",
    )
}

/// `Sabre\DAV\Exception\BadRequest` (400).
pub fn bad_request(message: &str) -> Response {
    dav_error(
        StatusCode::BAD_REQUEST,
        "Sabre\\DAV\\Exception\\BadRequest",
        message,
        "",
    )
}

/// `Sabre\DAV\Exception\PreconditionFailed` (412 + `<s:header>`).
pub fn precondition_failed(header_name: &str) -> Response {
    let extra = format!("<s:header>{}</s:header>", escape(header_name));
    dav_error(
        StatusCode::PRECONDITION_FAILED,
        "Sabre\\DAV\\Exception\\PreconditionFailed",
        "A precondition on the request headers failed.",
        &extra,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn invalid_sync_token_body_has_precondition() {
        let response = invalid_sync_token();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("<d:valid-sync-token/>"), "{text}");
    }

    #[tokio::test]
    async fn uid_conflict_names_the_href() {
        let response = uid_conflict("/remote.php/dav/addressbooks/users/a/b/x.vcf");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("no-uid-conflict"), "{text}");
        assert!(
            text.contains("/remote.php/dav/addressbooks/users/a/b/x.vcf"),
            "{text}"
        );
    }
}
