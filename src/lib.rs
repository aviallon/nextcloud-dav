// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A minimal, read-only CardDAV sidecar for Nextcloud.
//!
//! This crate exists to serve `/remote.php/dav/addressbooks/users/<user>/...`
//! straight from the Nextcloud database without booting PHP. It is deliberately
//! scoped to read-only personal address books in v1; see `README.md`.

pub mod auth;
pub mod config;
pub mod dav_error;
pub mod db;
pub mod discovery;
pub mod error;
pub mod files;
pub mod model;
pub mod mounts;
pub mod outbox;
pub mod php;
pub mod routes;
pub mod sync;
pub mod util;
pub mod vcard;
pub mod vcard_validate;
pub mod xml;
