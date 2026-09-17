// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Error types shared across the sidecar.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("xml error: {0}")]
    Xml(String),
    #[error("php fallback error: {0}")]
    Php(String),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("no principal found in the fallback response")]
    NoPrincipal,
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("invalid or unknown sync token")]
    InvalidSyncToken,
    #[error("{1}")]
    Status(StatusCode, String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn internal(msg: impl Into<String>) -> Self {
        Error::Internal(msg.into())
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Error::BadRequest(msg.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Error::BadRequest(_) | Error::Xml(_) => StatusCode::BAD_REQUEST,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::NotFound | Error::NoPrincipal => StatusCode::NOT_FOUND,
            Error::InvalidSyncToken => StatusCode::FORBIDDEN,
            Error::Status(status, _) => *status,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // The sync-token precondition carries a Sabre XML body, not our plain
        // text; render it before the generic path.
        if matches!(self, Error::InvalidSyncToken) {
            return crate::dav_error::invalid_sync_token();
        }
        let status = self.status();
        let body = self.to_string();
        let mut response = (status, body).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"Nextcloud\", charset=\"UTF-8\""),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_request_maps_to_400() {
        assert_eq!(Error::bad_request("x").status(), StatusCode::BAD_REQUEST);
        assert_eq!(Error::Xml("x".into()).status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unauthorized_maps_to_401() {
        assert_eq!(Error::Unauthorized.status(), StatusCode::UNAUTHORIZED);
    }
}
