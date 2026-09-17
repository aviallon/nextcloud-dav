// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Row types mirroring the Nextcloud `oc_*` schema (see `dav-bench/CARDDAV_DESIGN.md` §1).

/// A row of `oc_addressbooks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBook {
    pub id: i64,
    pub uri: String,
    pub displayname: Option<String>,
    pub principaluri: String,
    pub description: Option<String>,
    pub synctoken: i64,
}

/// A row of `oc_cards`, after `readBlob()` filtering has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub id: i64,
    /// The URL segment, typically `<UID>.vcf`.
    pub uri: String,
    /// Unquoted `md5(carddata)` as stored in the database.
    pub etag: String,
    /// Byte length of the served body (`carddata` after filtering).
    pub size: i64,
    pub lastmodified: Option<i64>,
    /// The raw stored bytes, after `readBlob()` filtering.
    pub carddata: Vec<u8>,
}

impl Card {
    /// The ETag as sent on the wire (quoted), matching `CardDavBackend::getCard()`.
    pub fn quoted_etag(&self) -> String {
        format!("\"{}\"", self.etag)
    }
}

/// A row of `oc_authtoken` relevant to the fast path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthToken {
    pub uid: String,
    pub login_name: String,
    pub token_type: i64,
    pub expires: Option<i64>,
    pub password_invalid: bool,
    pub last_check: i64,
    pub last_activity: i64,
}

/// A row of `oc_addressbookchanges`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    pub uri: String,
    pub operation: i64,
    pub synctoken: i64,
}

/// `(id, uri)` pair for `oc_cards`, used by the initial-sync paging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardIdUri {
    pub id: i64,
    pub uri: String,
}
