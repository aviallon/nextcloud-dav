// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The PHP fallback: a credentialed `PROPFIND /remote.php/dav/` Depth 0.
//!
//! This is the same code path clients already exercise, so it needs no server
//! modifications. The DAV root is now served natively, so the probe carries
//! [`FALLBACK_HEADER`]: the discovery handler answers `501`, nginx replays the
//! request to PHP (`error_page`), and PHP returns the principal. That is what
//! keeps the fallback from recursing through the root route.

use crate::error::{Error, Result};
use crate::util::percent_decode;
use crate::xml::parse::parse_document;
use crate::xml::write::NS_DAV;
use std::time::Duration;

const PROPFIND_BODY: &str = "<?xml version=\"1.0\"?>\
<d:propfind xmlns:d=\"DAV:\"><d:prop><d:current-user-principal/></d:prop></d:propfind>";

/// Marks a request as the sidecar's own authentication probe.
///
/// The probe is a credentialed `PROPFIND /remote.php/dav/` through the public
/// URL, so once the DAV root is routed to the sidecar it would otherwise recurse
/// (`authenticate` -> `php_fallback` -> `authenticate` -> ...). The discovery
/// handler sees this header and answers `501`, which nginx replays to PHP via
/// `error_page`; PHP then produces the principal.
pub const FALLBACK_HEADER: &str = "x-nextcloud-dav-fallback";

/// The outcome of a fallback authentication attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhpAuth {
    Authenticated(String),
    Invalid,
    Throttled,
    Maintenance,
    Other(u16),
}

pub struct PhpClient {
    http: reqwest::Client,
    base_url: reqwest::Url,
}

impl PhpClient {
    pub fn new(base_url: &str, timeout: Duration, allow_self_signed: bool) -> Result<Self> {
        let base_url =
            reqwest::Url::parse(base_url).map_err(|e| Error::Php(format!("invalid URL: {e}")))?;
        let mut builder = reqwest::Client::builder().timeout(timeout);
        if allow_self_signed {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let http = builder
            .build()
            .map_err(|e| Error::Php(format!("failed to build HTTP client: {e}")))?;
        Ok(Self { http, base_url })
    }

    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
        forwarded_for: Option<&str>,
    ) -> PhpAuth {
        match self.propfind(username, password, forwarded_for).await {
            Ok(response) => {
                let status = response.status();
                match status.as_u16() {
                    207 => match self.parse_principal(response).await {
                        Ok(uid) => PhpAuth::Authenticated(uid),
                        Err(_) => PhpAuth::Invalid,
                    },
                    401 => PhpAuth::Invalid,
                    429 => PhpAuth::Throttled,
                    503 => PhpAuth::Maintenance,
                    other => PhpAuth::Other(other),
                }
            }
            Err(e) => {
                log::warn!("php fallback request failed: {e}");
                PhpAuth::Other(502)
            }
        }
    }

    async fn propfind(
        &self,
        username: &str,
        password: &str,
        forwarded_for: Option<&str>,
    ) -> std::result::Result<reqwest::Response, Error> {
        let url = self
            .base_url
            .join("remote.php/dav/")
            .map_err(|e| Error::Php(e.to_string()))?;
        let mut request = self
            .http
            .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
            .header("Depth", "0")
            .header("Content-Type", "application/xml; charset=utf-8")
            .header(FALLBACK_HEADER, "1")
            .basic_auth(username, Some(password))
            .body(PROPFIND_BODY);
        if let Some(ip) = forwarded_for {
            request = request.header("X-Forwarded-For", ip);
        }
        request.send().await.map_err(|e| Error::Php(e.to_string()))
    }

    async fn parse_principal(&self, response: reqwest::Response) -> Result<String> {
        let body = response
            .bytes()
            .await
            .map_err(|e| Error::Php(e.to_string()))?;
        let document = parse_document(&body)?;
        let principal = document
            .child(NS_DAV, "response")
            .and_then(|response| response.child(NS_DAV, "propstat"))
            .and_then(|propstat| propstat.child(NS_DAV, "prop"))
            .and_then(|prop| prop.child(NS_DAV, "current-user-principal"))
            .ok_or(Error::NoPrincipal)?;

        if principal.child(NS_DAV, "unauthenticated").is_some() {
            return Err(Error::NoPrincipal);
        }
        let href = principal
            .child(NS_DAV, "href")
            .map(|href| href.text.trim())
            .filter(|href| !href.is_empty())
            .ok_or(Error::NoPrincipal)?;
        uid_from_principal_href(href).ok_or(Error::NoPrincipal)
    }
}

/// Extracts the uid from `.../principals/users/<uid>/`.
pub fn uid_from_principal_href(href: &str) -> Option<String> {
    let trimmed = href.trim_end_matches('/');
    let mut segments = trimmed.rsplit('/');
    let uid = segments.next()?;
    let kind = segments.next()?;
    if kind == "users" && !uid.is_empty() {
        Some(percent_decode(uid))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_uid_from_principal_href() {
        assert_eq!(
            uid_from_principal_href("/remote.php/dav/principals/users/alice/"),
            Some("alice".to_string())
        );
        assert_eq!(
            uid_from_principal_href(
                "https://cloud.example.com/remote.php/dav/principals/users/a%20b/"
            ),
            Some("a b".to_string())
        );
        assert_eq!(uid_from_principal_href("/principals/system/system"), None);
        assert_eq!(uid_from_principal_href(""), None);
    }
}
