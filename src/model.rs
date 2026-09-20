// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Row types mirroring the Nextcloud `oc_*` schema (see `dav-bench/CARDDAV_DESIGN.md` §1).

use serde_json::Value;
use std::collections::HashMap;

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

/// A row of `oc_calendars` (the owner's row; shared rows live in
/// `oc_dav_shares`).
///
/// The columns mirror `CalDavBackend::getCalendarsForUser()`'s select list
/// (the 33.0.5 `propertyMap` plus the fixed columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    pub id: i64,
    pub uri: String,
    pub displayname: Option<String>,
    pub principaluri: String,
    pub description: Option<String>,
    pub timezone: Option<String>,
    pub calendarorder: i64,
    pub calendarcolor: Option<String>,
    /// CSV from `oc_calendars.components`, e.g. `VEVENT` or `VEVENT,VTODO`.
    pub components: Option<String>,
    /// `oc_calendars.transparent`: 1 = transparent, 0 = opaque.
    pub transparent: bool,
    pub synctoken: i64,
    /// Non-null when the calendar is in the trashbin. A caller that has any of
    /// these is delegated, so a served calendar always has `None`.
    pub deleted_at: Option<i64>,
}

/// A calendar a user can see: an owned calendar, or a calendar shared with
/// them through `oc_dav_shares` (`CalDavBackend::getCalendarsForUser()`).
///
/// `calendar` is always the owner's `oc_calendars` row; the wire-facing name
/// and the sharing facts live next to it. An owned calendar has
/// `owner_principal == None`, `read_only == false` and `wire_uri == uri`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleCalendar {
    /// The owner's `oc_calendars` row.
    pub calendar: Calendar,
    /// The name the calendar is served under: the owned `uri`, or
    /// `<uri>_shared_by_<owner-name>` for a shared calendar.
    pub wire_uri: String,
    /// The `{DAV:}displayname` sent on the wire. Shared calendars carry
    /// `<displayname> (<owner display name>)`.
    pub wire_displayname: Option<String>,
    /// `Some(owner principal)` for a shared calendar; `None` for an owned one.
    /// Also the switch for `{oc}owner-principal` / `{oc}read-only`.
    pub owner_principal: Option<String>,
    /// The owner's display name (`{nc}owner-displayname`).
    pub owner_displayname: String,
    /// The `oc_dav_shares.principaluri` the calendar was shared through. For a
    /// direct user share this is the caller's principal; a group/circle
    /// principal means the sidecar's ACL model does not apply.
    pub share_principal: Option<String>,
    /// `true` for a read-only share (`oc_dav_shares.access == 3`).
    pub read_only: bool,
    /// `true` when `schedule-calendar-transp` must be `transparent`. Shared
    /// calendars force it; owned ones use the stored column.
    pub transparent: bool,
}

impl VisibleCalendar {
    /// An owned calendar, with the wire fields derived from the row.
    pub fn owned(calendar: Calendar) -> Self {
        Self {
            wire_uri: calendar.uri.clone(),
            wire_displayname: calendar.displayname.clone(),
            owner_principal: None,
            owner_displayname: String::new(),
            share_principal: None,
            read_only: false,
            transparent: calendar.transparent,
            calendar,
        }
    }
}

/// One `oc_dav_shares` row for a calendar (`type = 'calendar'`), as
/// `OCA\DAV\DAV\Sharing\Backend::getShares()` reads it. `access` is the
/// `OCA\DAV\DAV\Sharing\Backend` level (2 read-write, 3 read-only, 4 public).
/// `access == 5` (unshared tombstone) is filtered out by the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarShare {
    pub principaluri: String,
    pub access: i16,
}

/// A row of `oc_calendarsubscriptions` (`CalDavBackend::getSubscriptionsForUser()`).
///
/// Subscriptions are children of the calendar home, after the calendars and the
/// special children, in `calendarorder`. They have no `getctag`, `owner-principal`
/// or `read-only`; `source` is exposed through `{calendarserver}source` as a
/// `Href` and the strip flags through `{calendarserver}subscribed-strip-*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarSubscription {
    pub id: i64,
    pub uri: String,
    pub principaluri: String,
    pub displayname: Option<String>,
    /// `oc_calendarsubscriptions.refreshrate`, an ISO8601 duration (`PT4H`).
    pub refreshrate: Option<String>,
    pub calendarorder: i64,
    pub calendarcolor: Option<String>,
    /// The three strip flags. Their value is irrelevant on the wire (`Sabre\CalDAV\Subscriptions\Plugin`
    /// forces the element empty), only that the element is present.
    pub striptodos: Option<i64>,
    pub stripalarms: Option<i64>,
    pub stripattachments: Option<i64>,
    pub lastmodified: Option<i64>,
    pub synctoken: i64,
    /// The webcal URL (`{calendarserver}source`).
    pub source: Option<String>,
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
    /// `oc_authtoken.password IS NULL`. A passwordless token is what a plain
    /// browser session creates; `PublicKeyTokenProvider::getPassword()` throws
    /// `PasswordlessTokenException` for it, which `checkTokenCredentials()`
    /// treats as valid without re-checking the login password.
    pub password_is_null: bool,
    pub last_check: i64,
    pub last_activity: i64,
    /// Raw `oc_authtoken.scope` JSON (or `NULL`). `LockdownManager::setToken()`
    /// reads this on every token validation and uses it to decide whether the
    /// filesystem may be set up at all.
    pub scope: Option<String>,
}

/// A row of `oc_addressbookchanges`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    pub uri: String,
    pub operation: i64,
    pub synctoken: i64,
}

/// A row of `oc_calendarobjects` on the CalDAV read path.
///
/// `calendardata` is the raw stored blob (CRLF); the XML writer normalises the
/// line endings on the wire exactly like Sabre's `XMLWriter`, so the bytes must
/// not be re-serialised here. `etag` is the stored, **unquoted**
/// `md5(calendardata)`; the wire form adds the quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarObject {
    pub id: i64,
    pub uri: String,
    pub etag: String,
    pub size: i64,
    pub lastmodified: Option<i64>,
    /// `oc_calendarobjects.componenttype` (mixed case in the database; PHP
    /// lowercases it for `{DAV:}getcontenttype`).
    pub componenttype: Option<String>,
    /// 0 PUBLIC, 1 PRIVATE, 2 CONFIDENTIAL.
    pub classification: i64,
    /// The raw stored `calendardata` bytes.
    pub calendardata: Vec<u8>,
}

impl CalendarObject {
    /// The ETag as sent on the wire (quoted).
    pub fn quoted_etag(&self) -> String {
        format!("\"{}\"", self.etag)
    }
}

/// One `oc_calendarchanges` entry, already aggregated to `MAX(operation)` per
/// URI (`CalDavBackend::getChangesForCalendar()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarChange {
    pub uri: String,
    pub operation: i64,
}

/// `(id, uri)` pair for `oc_cards`, used by the initial-sync paging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardIdUri {
    pub id: i64,
    pub uri: String,
}

/// A row of `oc_filecache` relevant to a files `PROPFIND`.
///
/// `size` is the stored size; callers must apply [`FileCacheRow::effective_size`]
/// to reproduce `FileInfo::getSize()` (which substitutes `unencrypted_size` for
/// encrypted rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCacheRow {
    pub fileid: i64,
    /// `oc_filecache.storage` (the numeric id, not `oc_storages.id`).
    pub storage: i64,
    pub path: String,
    pub name: String,
    pub size: i64,
    pub mtime: i64,
    pub etag: String,
    pub permissions: i64,
    pub encrypted: i64,
    pub unencrypted_size: Option<i64>,
    pub checksum: Option<String>,
    pub parent: i64,
    /// The resolved `oc_mimetypes.mimetype` (empty when the LEFT JOIN missed).
    pub mimetype: String,
    /// `oc_filecache_extended.creation_time` (0 when the LEFT JOIN missed, like
    /// PHP's `(int) null`).
    pub creation_time: i64,
    /// `oc_files_metadata.json`, already reduced to `key -> value` (the inner
    /// `value` field of each entry, exactly like `FileInfo::getMetadata()`).
    pub metadata: HashMap<String, Value>,
}

impl FileCacheRow {
    /// `httpd/unix-directory` is Nextcloud's folder mimetype.
    pub fn is_directory(&self) -> bool {
        self.mimetype == "httpd/unix-directory"
    }

    /// `FileInfo::getSize()` / `FileInfo::rawSize`: an encrypted row reports its
    /// `unencrypted_size` when present.
    pub fn effective_size(&self) -> i64 {
        if self.encrypted != 0 {
            if let Some(size) = self.unencrypted_size {
                return size;
            }
        }
        self.size
    }

    /// `FileInfo::getName()`: the `name` column, or the last `path` segment when
    /// it is empty.
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        self.path
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or_default()
            .to_string()
    }
}

/// A row of `oc_share` relevant to the files sharing properties
/// (`oc:share-types` / `nc:sharees`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRow {
    pub file_source: i64,
    pub share_type: i64,
    pub share_with: Option<String>,
    pub permissions: i64,
    /// `oc_users.displayname` for a user sharee (type 0), when resolvable.
    pub user_displayname: Option<String>,
    /// `oc_groups.displayname` for a group sharee (type 1), when resolvable.
    pub group_displayname: Option<String>,
}
