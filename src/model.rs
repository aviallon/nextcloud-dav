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

/// A book a user can see: an owned book, or a book shared with them through
/// `oc_dav_shares` (`CardDavBackend::getAddressBooksForUser()`).
///
/// `book` is always the owner's `oc_addressbooks` row; the wire-facing name and
/// the sharing facts live next to it. An owned book has
/// `owner_principal == None`, `read_only == false` and `wire_uri == book.uri`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleBook {
    /// The owner's `oc_addressbooks` row (id, stored uri/displayname, token).
    pub book: AddressBook,
    /// The name the book is served under: the owned `uri`, or
    /// `<uri>_shared_by_<owner-name>` for a shared book.
    pub wire_uri: String,
    /// The `{DAV:}displayname` sent on the wire. Shared books carry
    /// `<displayname> (<owner display name>)`.
    pub wire_displayname: Option<String>,
    /// `Some(owner principal)` for a shared book; `None` for an owned one.
    /// Also the switch for `{oc}owner-principal` / `{oc}read-only`.
    pub owner_principal: Option<String>,
    /// `true` for a read-only share (`oc_dav_shares.access == 3`).
    pub read_only: bool,
}

impl VisibleBook {
    /// An owned book, with the wire fields derived from the row.
    pub fn owned(book: AddressBook) -> Self {
        Self {
            wire_uri: book.uri.clone(),
            wire_displayname: book.displayname.clone(),
            owner_principal: None,
            read_only: false,
            book,
        }
    }
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
