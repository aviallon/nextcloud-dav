// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The `sync-collection` token state machine.
//!
//! This is a faithful port of
//! `apps/dav/lib/CardDAV/CardDavBackend.php::getChangesForAddressBook()` and the
//! `{DAV:}sync-token` handling in
//! `3rdparty/sabre/dav/lib/DAV/Sync/Plugin.php`.
//!
//! Two invariants matter:
//!
//! * `oc_addressbooks.synctoken` is always `max(oc_addressbookchanges.synctoken) + 1`.
//! * A change row carries the *pre-increment* token, so the range
//!   `[old, current)` is exactly the set of changes since `old`.
//!
//! It is kept pure (no database access) so the paging and truncation rules can
//! be unit-tested deterministically.

use crate::error::{Error, Result};
use crate::model::{CardIdUri, ChangeRow};
use std::collections::HashMap;

/// `Sabre\DAV\Sync\Plugin::SYNCTOKEN_PREFIX`.
pub const SYNCTOKEN_PREFIX: &str = "http://sabre.io/ns/sync/";

/// A parsed `{DAV:}sync-token` request value (the prefix already stripped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncToken {
    /// No token supplied: the client wants the initial sync.
    Initial,
    /// `init_<lastID>_<token>`: paging through the initial sync.
    InitialPaging { last_id: i64, token: i64 },
    /// A normal monotonic token.
    Changes(i64),
}

/// Parses a request token. An absent/empty value means "initial sync"; a token
/// without the Sabre prefix is rejected exactly like `InvalidSyncToken`.
pub fn parse_sync_token(raw: Option<&str>) -> Result<SyncToken> {
    let Some(raw) = raw else {
        return Ok(SyncToken::Initial);
    };
    let Some(rest) = raw.strip_prefix(SYNCTOKEN_PREFIX) else {
        return Err(Error::InvalidSyncToken);
    };
    if rest.is_empty() {
        return Ok(SyncToken::Initial);
    }
    if let Some(paging) = rest.strip_prefix("init_") {
        let mut parts = paging.splitn(2, '_');
        let last_id = parts.next().and_then(|p| p.parse::<i64>().ok());
        let token = parts.next().and_then(|p| p.parse::<i64>().ok());
        return match (last_id, token) {
            (Some(last_id), Some(token)) => Ok(SyncToken::InitialPaging { last_id, token }),
            _ => Err(Error::InvalidSyncToken),
        };
    }
    rest.parse::<i64>()
        .map(SyncToken::Changes)
        .map_err(|_| Error::InvalidSyncToken)
}

/// A page of sync results, before it is turned into a `{DAV:}multistatus`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncPage {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    /// The value to put after the `http://sabre.io/ns/sync/` prefix.
    pub sync_token: String,
    pub truncated: bool,
}

/// Initial sync (no token): `CardDavBackend.php` `else` branch.
pub fn initial_sync(rows: &[CardIdUri], current_token: i64, limit: i64) -> SyncPage {
    if rows.is_empty() {
        return SyncPage {
            sync_token: current_token.to_string(),
            ..Default::default()
        };
    }
    let last_id = rows[rows.len() - 1].id;
    let added: Vec<String> = rows.iter().map(|r| r.uri.clone()).collect();
    let truncated = rows.len() as i64 >= limit;
    SyncPage {
        added,
        sync_token: if truncated {
            format!("init_{last_id}_{current_token}")
        } else {
            current_token.to_string()
        },
        modified: Vec::new(),
        deleted: Vec::new(),
        truncated,
    }
}

/// Continue an initial sync that was paged (`init_<lastID>_<token>`).
pub fn initial_sync_continue(rows: &[CardIdUri], initial_token: i64, limit: i64) -> SyncPage {
    if rows.is_empty() {
        return SyncPage {
            sync_token: initial_token.to_string(),
            ..Default::default()
        };
    }
    let last_id = rows[rows.len() - 1].id;
    let added: Vec<String> = rows.iter().map(|r| r.uri.clone()).collect();
    let truncated = rows.len() as i64 >= limit;
    SyncPage {
        added,
        sync_token: if truncated {
            format!("init_{last_id}_{initial_token}")
        } else {
            initial_token.to_string()
        },
        modified: Vec::new(),
        deleted: Vec::new(),
        truncated,
    }
}

/// Incremental sync: `CardDavBackend.php` `elseif ($syncToken)` branch.
///
/// `rows` must already be ordered by `synctoken`. Duplicates are collapsed by
/// URI, keeping the *last* operation and the position of the first occurrence,
/// exactly like PHP's associative-array assignment.
pub fn changes_sync(rows: &[ChangeRow], current_token: i64, limit: i64) -> SyncPage {
    let row_count = rows.len() as i64;
    let mut order: Vec<String> = Vec::with_capacity(rows.len());
    let mut ops: HashMap<String, i64> = HashMap::with_capacity(rows.len());
    let mut highest: i64 = 0;
    for row in rows {
        if !ops.contains_key(&row.uri) {
            order.push(row.uri.clone());
        }
        ops.insert(row.uri.clone(), row.operation);
        highest = row.synctoken;
    }

    let mut page = SyncPage {
        sync_token: current_token.to_string(),
        ..Default::default()
    };
    for uri in order {
        match ops.get(&uri).copied().unwrap_or_default() {
            1 => page.added.push(uri),
            2 => page.modified.push(uri),
            3 => page.deleted.push(uri),
            _ => {}
        }
    }

    // PHP checks `empty($changes)`; since operations are only ever 1/2/3 this
    // is equivalent to "no responses".
    let empty = page.added.is_empty() && page.modified.is_empty() && page.deleted.is_empty();
    if !empty && row_count == limit && highest < current_token {
        page.sync_token = highest.to_string();
        page.truncated = true;
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: i64, uri: &str) -> CardIdUri {
        CardIdUri {
            id,
            uri: uri.to_string(),
        }
    }

    fn change(uri: &str, operation: i64, token: i64) -> ChangeRow {
        ChangeRow {
            uri: uri.to_string(),
            operation,
            synctoken: token,
        }
    }

    #[test]
    fn parse_missing_token_is_initial() {
        assert_eq!(parse_sync_token(None).unwrap(), SyncToken::Initial);
        assert_eq!(
            parse_sync_token(Some("http://sabre.io/ns/sync/")).unwrap(),
            SyncToken::Initial
        );
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        assert!(parse_sync_token(Some("5")).is_err());
        assert!(parse_sync_token(Some("http://other/ns/sync/5")).is_err());
    }

    #[test]
    fn parse_incremental_and_paging() {
        assert_eq!(
            parse_sync_token(Some("http://sabre.io/ns/sync/42")).unwrap(),
            SyncToken::Changes(42)
        );
        assert_eq!(
            parse_sync_token(Some("http://sabre.io/ns/sync/init_99_7")).unwrap(),
            SyncToken::InitialPaging {
                last_id: 99,
                token: 7
            }
        );
    }

    #[test]
    fn initial_sync_empty_returns_current_token() {
        let page = initial_sync(&[], 7, 2500);
        assert_eq!(page.sync_token, "7");
        assert!(!page.truncated);
        assert!(page.added.is_empty());
    }

    #[test]
    fn initial_sync_below_limit_returns_current_token() {
        let rows = [card(1, "a.vcf"), card(2, "b.vcf")];
        let page = initial_sync(&rows, 3, 2500);
        assert_eq!(page.added, vec!["a.vcf", "b.vcf"]);
        assert_eq!(page.sync_token, "3");
        assert!(!page.truncated);
    }

    #[test]
    fn initial_sync_at_limit_pages() {
        let rows = [card(10, "a.vcf"), card(11, "b.vcf")];
        let page = initial_sync(&rows, 4, 2);
        assert_eq!(page.added, vec!["a.vcf", "b.vcf"]);
        assert_eq!(page.sync_token, "init_11_4");
        assert!(page.truncated);
    }

    #[test]
    fn initial_sync_continue_exhausted_drops_prefix() {
        let page = initial_sync_continue(&[], 4, 2);
        assert_eq!(page.sync_token, "4");
        assert!(!page.truncated);
        assert!(page.added.is_empty());
    }

    #[test]
    fn initial_sync_continue_still_pages() {
        let rows = [card(20, "c.vcf"), card(21, "d.vcf")];
        let page = initial_sync_continue(&rows, 4, 2);
        assert_eq!(page.sync_token, "init_21_4");
        assert!(page.truncated);
        assert_eq!(page.added, vec!["c.vcf", "d.vcf"]);
    }

    #[test]
    fn incremental_dedups_keeping_last_operation() {
        // Same URI first added, then modified: only one response, op = modify.
        let rows = [
            change("a.vcf", 1, 5),
            change("b.vcf", 1, 6),
            change("a.vcf", 2, 7),
        ];
        let page = changes_sync(&rows, 9, 2500);
        assert_eq!(page.added, vec!["b.vcf"]);
        assert_eq!(page.modified, vec!["a.vcf"]);
        assert!(page.deleted.is_empty());
        // Not truncated: rowCount (3) != limit (2500).
        assert_eq!(page.sync_token, "9");
        assert!(!page.truncated);
    }

    #[test]
    fn incremental_deletes_and_adds() {
        let rows = [change("a.vcf", 3, 5), change("b.vcf", 1, 6)];
        let page = changes_sync(&rows, 9, 2500);
        assert_eq!(page.deleted, vec!["a.vcf"]);
        assert_eq!(page.added, vec!["b.vcf"]);
        assert_eq!(page.sync_token, "9");
    }

    #[test]
    fn incremental_empty_returns_current_token() {
        let page = changes_sync(&[], 9, 2500);
        assert_eq!(page.sync_token, "9");
        assert!(!page.truncated);
    }

    #[test]
    fn incremental_truncation_returns_highest_token() {
        let rows = [change("a.vcf", 1, 5), change("b.vcf", 1, 6)];
        let page = changes_sync(&rows, 9, 2);
        assert_eq!(page.sync_token, "6");
        assert!(page.truncated);
        assert_eq!(page.added, vec!["a.vcf", "b.vcf"]);
    }

    #[test]
    fn incremental_at_limit_but_highest_is_current_is_not_truncated() {
        // PHP: `$highestSyncToken < $currentToken` must hold to truncate.
        let rows = [change("a.vcf", 1, 9), change("b.vcf", 1, 9)];
        let page = changes_sync(&rows, 9, 2);
        assert_eq!(page.sync_token, "9");
        assert!(!page.truncated);
    }
}
