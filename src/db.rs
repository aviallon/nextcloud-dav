// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Read-only access to the Nextcloud database.
//!
//! Every query here is a `SELECT`, with the single documented exception of
//! [`Db::register_bruteforce_attempt`], which mirrors Nextcloud's
//! `DatabaseBackend::registerAttempt()` and is opt-in via
//! `nextcloud_dav.record_bruteforce_attempts`.
//!
//! The `sqlx::Any` driver does **not** translate placeholders, so queries are
//! written with `?` and rewritten to `$n` for PostgreSQL by [`Db::render`]. It
//! also cannot map PostgreSQL `boolean` / MySQL `TINYINT` values uniformly, so
//! `oc_authtoken.password_invalid` is selected through a portable
//! `CASE WHEN ... THEN '1' ELSE '0' END`.

use crate::error::{Error, Result};
use crate::model::{AddressBook, AuthToken, Card, CardIdUri, ChangeRow, FileCacheRow, ShareRow, VisibleBook};
use crate::vcard;
use md5::{Digest, Md5};
use serde_json::Value;
use sqlx::any::{AnyConnectOptions, AnyPoolOptions, AnyRow};
use sqlx::{Any, AnyPool, Row};
use std::collections::{HashMap, HashSet};

/// `OCA\DAV\DAV\Sharing\Backend` access levels.
const ACCESS_READ: i16 = 3;
const ACCESS_UNSHARED: i16 = 5;

/// `CardDavBackend::INDEXED_PROPERTIES` (`CardDavBackend.php:47-50`). Only these
/// property names are mirrored into `oc_cards_properties` for search.
const INDEXED_PROPERTIES: &[&str] = &[
    "BDAY",
    "UID",
    "N",
    "FN",
    "TITLE",
    "ROLE",
    "NOTE",
    "NICKNAME",
    "ORG",
    "CATEGORIES",
    "EMAIL",
    "TEL",
    "IMPP",
    "ADR",
    "URL",
    "GEO",
    "CLOUD",
    "X-SOCIALPROFILE",
];

pub struct Db {
    pool: AnyPool,
    prefix: String,
    postgres: bool,
}

impl Db {
    pub async fn new(
        options: AnyConnectOptions,
        prefix: String,
        max_connections: u32,
    ) -> Result<Self> {
        // The `Any` driver has no drivers until they are installed.
        static INSTALL_DRIVERS: std::sync::Once = std::sync::Once::new();
        INSTALL_DRIVERS.call_once(sqlx::any::install_default_drivers);
        let postgres = matches!(options.database_url.scheme(), "postgres" | "postgresql");
        let pool = AnyPoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;
        Ok(Self {
            pool,
            prefix,
            postgres,
        })
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").fetch_one(&self.pool).await?;
        Ok(())
    }

    /// Rewrites `?` placeholders to `$n` for PostgreSQL.
    fn render(&self, sql: &str) -> String {
        render_placeholders(sql, self.postgres)
    }

    /// 1-based placeholder for position `index`.
    fn ph(&self, index: usize) -> String {
        placeholder(index, self.postgres)
    }

    // ------------------------------------------------------------------
    // Address books
    // ------------------------------------------------------------------

    pub async fn address_books_for_user(&self, principal: &str) -> Result<Vec<AddressBook>> {
        let sql = self.render(&format!(
            "SELECT id, uri, displayname, principaluri, description, synctoken \
             FROM {}addressbooks WHERE principaluri = ? ORDER BY id",
            self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(principal)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(address_book_from_row).collect()
    }

    pub async fn address_book_by_uri(
        &self,
        principal: &str,
        uri: &str,
    ) -> Result<Option<AddressBook>> {
        let sql = self.render(&format!(
            "SELECT id, uri, displayname, principaluri, description, synctoken \
             FROM {}addressbooks WHERE principaluri = ? AND uri = ? LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(principal)
            .bind(uri)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(address_book_from_row).transpose()
    }

    /// Group principals of a user, as `CardDavBackend::getAddressBooksForUser()`
    /// builds them through `Principal::getGroupMembership()`.
    ///
    /// PHP merges the database group backend with every other registered
    /// backend (LDAP, circles) through `traitGetGroupMembership`, and skips
    /// groups for which `hideFromCollaboration()` is true. Neither is visible
    /// in the schema: LDAP/circle membership is not stored in `oc_group_user`,
    /// and `hideFromCollaboration()` is backend state with no `oc_groups`
    /// column. The sidecar therefore expands database groups only; that
    /// limitation is declared as the `shared-books-group-backends` deviation.
    pub async fn group_principals(&self, uid: &str) -> Result<Vec<String>> {
        let sql = self.render(&format!(
            "SELECT g.gid FROM {}group_user gu JOIN {}groups g ON g.gid = gu.gid \
             WHERE gu.uid = ? ORDER BY g.gid",
            self.prefix, self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_all(&self.pool)
            .await?;
        let mut principals = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(gid) = row.try_get::<Option<String>, _>("gid")? {
                principals.push(format!("principals/groups/{}", php_urlencode(&gid)));
            }
        }
        Ok(principals)
    }

    /// Every book the caller can see: owned books plus the de-duplicated
    /// `oc_dav_shares` books, in PHP's listing order.
    ///
    /// Owned books come first (a wire-name collision must resolve to the owned
    /// book, `Sabre\DAV\Collection::getChild()` returns the first match), then
    /// the shared rows ordered by `a.id` for determinism. PHP adds no `ORDER BY`
    /// (declared as `shared-books-listing-order`).
    pub async fn visible_books(
        &self,
        principal: &str,
        group_principals: &[String],
    ) -> Result<Vec<VisibleBook>> {
        let owned = self.address_books_for_user(principal).await?;
        let mut books: Vec<VisibleBook> = owned.into_iter().map(VisibleBook::owned).collect();
        let mut index: std::collections::HashMap<i64, usize> = books
            .iter()
            .enumerate()
            .map(|(position, book)| (book.book.id, position))
            .collect();

        // The principals are bound twice (share rows + tombstone subquery);
        // the tombstone access level is bound once, ahead of both.
        let mut principals: Vec<String> = Vec::with_capacity(group_principals.len() + 1);
        principals.push(principal.to_string());
        principals.extend(group_principals.iter().cloned());
        let access_ph = self.ph(1);
        let mut in_list = String::new();
        let mut tombstone_list = String::new();
        for i in 0..principals.len() {
            if i > 0 {
                in_list.push_str(", ");
                tombstone_list.push_str(", ");
            }
            in_list.push_str(&self.ph(i + 2));
            tombstone_list.push_str(&self.ph(principals.len() + i + 2));
        }
        // Deliberate deviation from the literal CardDAV query: PHP excludes the
        // tombstone row by `s.id` (`CardDavBackend.php:140-147`), which never
        // hides a resource reached through a surviving group share. We exclude
        // by `resourceid`, which is the intended (CalDAV-aligned) semantics.
        // Declared as `shared-unshare-tombstone-semantics`.
        let sql = format!(
            "SELECT a.id, a.uri, a.displayname, a.principaluri, a.description, a.synctoken, s.access \
             FROM {p}dav_shares s JOIN {p}addressbooks a ON s.resourceid = a.id \
             WHERE s.type = 'addressbook' AND s.principaluri IN ({in_list}) \
               AND NOT EXISTS (SELECT 1 FROM {p}dav_shares d \
                   WHERE d.access = {access_ph} AND d.resourceid = s.resourceid \
                     AND d.principaluri IN ({tombstone_list})) \
             ORDER BY a.id",
            p = self.prefix,
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ACCESS_UNSHARED);
        for principal in &principals {
            query = query.bind(principal.as_str());
        }
        for principal in &principals {
            query = query.bind(principal.as_str());
        }
        let rows = query.fetch_all(&self.pool).await?;

        for row in &rows {
            let owner_principal = row
                .try_get::<Option<String>, _>("principaluri")?
                .unwrap_or_default();
            // A share of the caller's own book: it is already in the owned list.
            if owner_principal == principal {
                continue;
            }
            let id: i64 = row.try_get("id")?;
            let access: i16 = row.try_get("access")?;
            let read_only = access == ACCESS_READ;
            if let Some(&position) = index.get(&id) {
                // Read-write wins: a read-only row never downgrades an existing
                // entry, and an existing read-write entry is never replaced.
                if read_only || !books[position].read_only {
                    continue;
                }
            }

            let uri = row.try_get::<Option<String>, _>("uri")?.unwrap_or_default();
            let displayname: Option<String> = row.try_get("displayname")?;
            let owner_name = owner_principal
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            let owner_displayname = self
                .user_display_name(&owner_name)
                .await?
                .unwrap_or_else(|| owner_name.clone());
            let visible = VisibleBook {
                book: AddressBook {
                    id,
                    uri: uri.clone(),
                    displayname: displayname.clone(),
                    principaluri: owner_principal.clone(),
                    description: row.try_get("description")?,
                    synctoken: row.try_get("synctoken")?,
                },
                wire_uri: format!("{uri}_shared_by_{owner_name}"),
                wire_displayname: Some(format!(
                    "{} ({owner_displayname})",
                    displayname.clone().unwrap_or_default()
                )),
                owner_principal: Some(owner_principal),
                read_only,
            };
            match index.get(&id) {
                Some(&position) => books[position] = visible,
                None => {
                    index.insert(id, books.len());
                    books.push(visible);
                }
            }
        }
        Ok(books)
    }

    /// The visible book served under `wire_uri`, or `None` (the 404 case).
    pub async fn visible_book_by_uri(
        &self,
        principal: &str,
        group_principals: &[String],
        wire_uri: &str,
    ) -> Result<Option<VisibleBook>> {
        Ok(self
            .visible_books(principal, group_principals)
            .await?
            .into_iter()
            .find(|book| book.wire_uri == wire_uri))
    }

    // ------------------------------------------------------------------
    // Cards
    // ------------------------------------------------------------------

    pub async fn cards(&self, address_book_id: i64) -> Result<Vec<Card>> {
        let sql = self.render(&format!(
            "SELECT id, uri, etag, size, lastmodified, carddata \
             FROM {}cards WHERE addressbookid = ? ORDER BY id",
            self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(card_from_row).collect()
    }

    pub async fn card(&self, address_book_id: i64, uri: &str) -> Result<Option<Card>> {
        let sql = self.render(&format!(
            "SELECT id, uri, etag, size, lastmodified, carddata \
             FROM {}cards WHERE addressbookid = ? AND uri = ? LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .bind(uri)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(card_from_row).transpose()
    }

    /// Fetches several cards by URI, chunked to keep the `IN` list small.
    pub async fn cards_by_uris(&self, address_book_id: i64, uris: &[String]) -> Result<Vec<Card>> {
        let mut cards = Vec::new();
        for chunk in uris.chunks(100) {
            let mut sql = format!(
                "SELECT id, uri, etag, size, lastmodified, carddata \
                 FROM {}cards WHERE addressbookid = {} AND uri IN (",
                self.prefix,
                self.ph(1)
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&self.ph(i + 2));
            }
            sql.push(')');
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(address_book_id);
            for uri in chunk {
                query = query.bind(uri.as_str());
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in &rows {
                cards.push(card_from_row(row)?);
            }
        }
        Ok(cards)
    }

    // ------------------------------------------------------------------
    // Sync
    // ------------------------------------------------------------------

    pub async fn sync_initial_cards(
        &self,
        address_book_id: i64,
        after_id: i64,
        limit: i64,
    ) -> Result<Vec<CardIdUri>> {
        let sql = self.render(&format!(
            "SELECT id, uri FROM {}cards WHERE addressbookid = ? AND id > ? \
             ORDER BY id LIMIT ?",
            self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .bind(after_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(CardIdUri {
                    id: row.try_get("id")?,
                    uri: row.try_get::<Option<String>, _>("uri")?.unwrap_or_default(),
                })
            })
            .collect()
    }

    pub async fn sync_changes(
        &self,
        address_book_id: i64,
        from_token: i64,
        current_token: i64,
        limit: i64,
    ) -> Result<Vec<ChangeRow>> {
        let sql = self.render(&format!(
            "SELECT uri, operation, synctoken FROM {}addressbookchanges \
             WHERE synctoken >= ? AND synctoken < ? AND addressbookid = ? \
             ORDER BY synctoken LIMIT ?",
            self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(from_token)
            .bind(current_token)
            .bind(address_book_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(ChangeRow {
                    uri: row.try_get::<Option<String>, _>("uri")?.unwrap_or_default(),
                    operation: row.try_get("operation")?,
                    synctoken: row.try_get("synctoken")?,
                })
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Auth
    // ------------------------------------------------------------------

    pub async fn authtoken_by_hash(&self, hash: &str) -> Result<Option<AuthToken>> {
        // `version` is `PublicKeyToken::VERSION`, which is 2 on both Nextcloud 33
        // and 36 (lib/private/Authentication/Token/PublicKeyToken.php).
        // `PublicKeyTokenMapper` filters on exactly this value, so the sidecar
        // must too; a mismatch only means we fall back to PHP, never that we
        // accept a token Nextcloud would reject.
        let sql = self.render(&format!(
            "SELECT uid, login_name, type, expires, last_check, last_activity, \
                    CASE WHEN password_invalid THEN '1' ELSE '0' END AS password_invalid_flag \
             FROM {}authtoken WHERE token = ? AND version = 2 LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(auth_token_from_row).transpose()
    }

    /// Number of rows in `oc_users` for this uid (0 for LDAP-only users).
    pub async fn native_user_exists(&self, uid: &str) -> Result<bool> {
        let sql = self.render(&format!(
            "SELECT COUNT(*) AS c FROM {}users WHERE uid = ?",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")? > 0)
    }

    /// True when `core/enabled = false` is set for the user.
    pub async fn user_is_disabled(&self, uid: &str) -> Result<bool> {
        let sql = self.render(&format!(
            "SELECT COUNT(*) AS c FROM {}preferences \
             WHERE userid = ? AND appid = 'core' AND configkey = 'enabled' \
               AND configvalue = 'false'",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")? > 0)
    }

    /// The display name from `oc_users` (`NULL`/empty falls back to the uid).
    pub async fn user_display_name(&self, uid: &str) -> Result<Option<String>> {
        let sql = self.render(&format!(
            "SELECT displayname FROM {}users WHERE uid = ? LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(row
                .try_get::<Option<String>, _>("displayname")?
                .filter(|name| !name.is_empty())),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Contacts groups (`oc:groups`)
    // ------------------------------------------------------------------

    pub async fn contact_groups(&self, address_book_id: i64) -> Result<Vec<String>> {
        let sql = self.render(&format!(
            "SELECT DISTINCT value FROM {}cards_properties \
             WHERE addressbookid = ? AND name = 'CATEGORIES' ORDER BY value",
            self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .fetch_all(&self.pool)
            .await?;
        let mut groups = Vec::new();
        for row in &rows {
            if let Some(value) = row.try_get::<Option<String>, _>("value")? {
                if !value.is_empty() && !groups.contains(&value) {
                    groups.push(value);
                }
            }
        }
        Ok(groups)
    }

    // ------------------------------------------------------------------
    // Card writes (native PUT/DELETE) + the event outbox
    // ------------------------------------------------------------------

    /// `oc_appconfig` lookup. `card_size_limit` is read once at startup.
    pub async fn appconfig_value(&self, app: &str, key: &str) -> Result<Option<String>> {
        let sql = self.render(&format!(
            "SELECT configvalue FROM {}appconfig WHERE appid = ? AND configkey = ? LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(app)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(row.try_get::<Option<String>, _>("configvalue")?),
            None => Ok(None),
        }
    }

    /// Id and URI of the card carrying `uid` in an address book, if any.
    /// Mirrors `CardDavBackend::getCardByUid()`.
    pub async fn card_by_uid(
        &self,
        address_book_id: i64,
        uid: &str,
    ) -> Result<Option<(i64, String)>> {
        let sql = self.render(&format!(
            "SELECT id, uri FROM {}cards WHERE addressbookid = ? AND uid = ? LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some((
                row.try_get("id")?,
                row.try_get::<Option<String>, _>("uri")?.unwrap_or_default(),
            ))),
            None => Ok(None),
        }
    }

    /// The single transaction behind a native PUT.
    ///
    /// Mirrors `CardDavBackend::createCard()` (operation 1) and
    /// `updateCard()` (operation 2): write `oc_cards`, append the change row
    /// carrying the pre-increment sync token and bump the book's token, rebuild
    /// `oc_cards_properties`, then enqueue the outbox event and wake the PHP
    /// worker with a transactional `pg_notify`.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_card(
        &self,
        address_book_id: i64,
        uri: &str,
        carddata: &[u8],
        uid: &str,
        existing: bool,
        registry: &crate::outbox::EffectRegistry,
        notify_channel: &str,
    ) -> Result<Card> {
        let etag = md5_hex(carddata);
        let now = now_unix();
        let size = carddata.len() as i64;
        let mut tx = self.pool.begin().await?;

        let card_id = if existing {
            let sql = self.render(&format!(
                "UPDATE {}cards SET carddata = ?, lastmodified = ?, size = ?, etag = ?, uid = ? \
                 WHERE addressbookid = ? AND uri = ?",
                self.prefix
            ));
            let result = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(carddata.to_vec())
                .bind(now)
                .bind(size)
                .bind(&etag)
                .bind(uid)
                .bind(address_book_id)
                .bind(uri)
                .execute(&mut *tx)
                .await?;
            if result.rows_affected() == 0 {
                return Err(Error::NotFound);
            }
            self.card_id(&mut tx, address_book_id, uri).await?
        } else {
            let sql = self.render(&format!(
                "INSERT INTO {}cards (carddata, uri, lastmodified, addressbookid, size, etag, uid) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
                self.prefix
            ));
            let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(carddata.to_vec())
                .bind(uri)
                .bind(now)
                .bind(address_book_id)
                .bind(size)
                .bind(&etag)
                .bind(uid)
                .fetch_one(&mut *tx)
                .await?;
            row.try_get("id")?
        };

        let operation = if existing {
            crate::outbox::EVENT_UPDATE
        } else {
            crate::outbox::EVENT_CREATE
        };
        self.add_change(&mut tx, address_book_id, uri, operation, now)
            .await?;
        self.update_properties(&mut tx, address_book_id, card_id, carddata)
            .await?;

        // Snapshot exactly what PHP's `getCard()` would return: `readBlob()`
        // filtering may shrink `carddata` and `size`.
        let (carddata_filtered, modified) = vcard::filter_read_blob(carddata);
        let snapshot = Card {
            id: card_id,
            uri: uri.to_string(),
            etag: etag.clone(),
            size: if modified {
                carddata_filtered.len() as i64
            } else {
                size
            },
            lastmodified: Some(now),
            carddata: carddata_filtered,
        };
        self.insert_outbox(
            &mut tx,
            operation,
            address_book_id,
            uri,
            &snapshot,
            uid,
            registry,
            notify_channel,
            now,
        )
        .await?;

        tx.commit().await?;
        Ok(snapshot)
    }

    /// The single transaction behind a native DELETE.
    ///
    /// Mirrors `CardDavBackend::deleteCard()`: read the pre-delete row (the
    /// event snapshot), delete it, log operation 3, purge the search columns and
    /// enqueue the delete event. Returns `None` when the card does not exist
    /// (Sabre answers 404).
    pub async fn delete_card(
        &self,
        address_book_id: i64,
        uri: &str,
        registry: &crate::outbox::EffectRegistry,
        notify_channel: &str,
    ) -> Result<Option<Card>> {
        let mut tx = self.pool.begin().await?;
        let Some((card, uid)) = self.fetch_card(&mut tx, address_book_id, uri).await? else {
            return Ok(None);
        };
        let now = now_unix();

        let sql = self.render(&format!(
            "DELETE FROM {}cards WHERE addressbookid = ? AND uri = ?",
            self.prefix
        ));
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .bind(uri)
            .execute(&mut *tx)
            .await?;

        self.add_change(
            &mut tx,
            address_book_id,
            uri,
            crate::outbox::EVENT_DELETE,
            now,
        )
        .await?;
        self.purge_properties(&mut tx, address_book_id, card.id)
            .await?;
        self.insert_outbox(
            &mut tx,
            crate::outbox::EVENT_DELETE,
            address_book_id,
            uri,
            &card,
            &uid,
            registry,
            notify_channel,
            now,
        )
        .await?;

        tx.commit().await?;
        Ok(Some(card))
    }

    /// `CardDavBackend::getCardId()` inside a transaction.
    async fn card_id(
        &self,
        tx: &mut sqlx::Transaction<'_, Any>,
        address_book_id: i64,
        uri: &str,
    ) -> Result<i64> {
        let sql = self.render(&format!(
            "SELECT id FROM {}cards WHERE addressbookid = ? AND uri = ? LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .bind(uri)
            .fetch_optional(&mut **tx)
            .await?;
        match row {
            Some(row) => Ok(row.try_get("id")?),
            None => Err(Error::NotFound),
        }
    }

    /// Reads one card inside a transaction, applying `readBlob()` filtering.
    /// Returns the card plus its stored `uid` (the outbox snapshot needs both).
    async fn fetch_card(
        &self,
        tx: &mut sqlx::Transaction<'_, Any>,
        address_book_id: i64,
        uri: &str,
    ) -> Result<Option<(Card, String)>> {
        let sql = self.render(&format!(
            "SELECT id, uri, etag, size, lastmodified, carddata, uid \
             FROM {}cards WHERE addressbookid = ? AND uri = ? LIMIT 1",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(address_book_id)
            .bind(uri)
            .fetch_optional(&mut **tx)
            .await?;
        match row {
            Some(row) => {
                let uid = row.try_get::<Option<String>, _>("uid")?.unwrap_or_default();
                Ok(Some((card_from_row(&row)?, uid)))
            }
            None => Ok(None),
        }
    }

    /// `CardDavBackend::addChange()`: insert the change row carrying the
    /// *pre-increment* token, then bump the book's synctoken.
    async fn add_change(
        &self,
        tx: &mut sqlx::Transaction<'_, Any>,
        address_book_id: i64,
        uri: &str,
        operation: i16,
        now: i64,
    ) -> Result<()> {
        let select = self.render(&format!(
            "SELECT synctoken FROM {}addressbooks WHERE id = ?",
            self.prefix
        ));
        // `oc_addressbooks.synctoken` and `oc_addressbookchanges.created_at`
        // are PostgreSQL `integer` (int4); bind int4 values so the `Any`
        // driver sends a format the server accepts.
        let token: i32 = sqlx::query(sqlx::AssertSqlSafe(select))
            .bind(address_book_id)
            .fetch_one(&mut **tx)
            .await?
            .try_get("synctoken")?;
        let insert = self.render(&format!(
            "INSERT INTO {}addressbookchanges (uri, synctoken, addressbookid, operation, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            self.prefix
        ));
        sqlx::query(sqlx::AssertSqlSafe(insert))
            .bind(uri)
            .bind(token)
            .bind(address_book_id)
            .bind(operation)
            .bind(now as i32)
            .execute(&mut **tx)
            .await?;
        let update = self.render(&format!(
            "UPDATE {}addressbooks SET synctoken = ? WHERE id = ?",
            self.prefix
        ));
        sqlx::query(sqlx::AssertSqlSafe(update))
            .bind(token + 1)
            .bind(address_book_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// `CardDavBackend::updateProperties()` + `purgeProperties()`: rebuild the
    /// indexed search columns for a card.
    async fn update_properties(
        &self,
        tx: &mut sqlx::Transaction<'_, Any>,
        address_book_id: i64,
        card_id: i64,
        carddata: &[u8],
    ) -> Result<()> {
        self.purge_properties(tx, address_book_id, card_id).await?;
        let card = vcard::parse(carddata);
        for property in &card.properties {
            if !INDEXED_PROPERTIES.contains(&property.name.as_str()) {
                continue;
            }
            // `TYPE=PREF` is case-insensitive on the parameter value.
            let preferred = property.params.iter().any(|(name, values)| {
                name == "TYPE"
                    && values
                        .iter()
                        .any(|value| value.eq_ignore_ascii_case("PREF"))
            });
            // `mb_strcut($value, 0, 254)`: 254 bytes, never splitting a UTF-8
            // code point.
            let value = truncate_utf8(&property.value, 254);
            let sql = self.render(&format!(
                "INSERT INTO {}cards_properties (addressbookid, cardid, name, value, preferred) \
                 VALUES (?, ?, ?, ?, ?)",
                self.prefix
            ));
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(address_book_id)
                .bind(card_id)
                .bind(&property.name)
                .bind(value)
                .bind(i32::from(preferred))
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    async fn purge_properties(
        &self,
        tx: &mut sqlx::Transaction<'_, Any>,
        address_book_id: i64,
        card_id: i64,
    ) -> Result<()> {
        let sql = self.render(&format!(
            "DELETE FROM {}cards_properties WHERE cardid = ? AND addressbookid = ?",
            self.prefix
        ));
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(card_id)
            .bind(address_book_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Inserts the event-outbox row and (on PostgreSQL) `pg_notify`s the
    /// worker. Both happen inside the caller's card transaction, so the event
    /// and the card commit or roll back together.
    #[allow(clippy::too_many_arguments)]
    async fn insert_outbox(
        &self,
        tx: &mut sqlx::Transaction<'_, Any>,
        event_type: i16,
        address_book_id: i64,
        uri: &str,
        card: &Card,
        uid: &str,
        registry: &crate::outbox::EffectRegistry,
        notify_channel: &str,
        now: i64,
    ) -> Result<()> {
        let card_row = crate::outbox::card_row_json(card, uid).to_string();
        let effects = registry.effects_json_for(event_type);
        let sql = self.render(&format!(
            "INSERT INTO {}dav_event_outbox \
             (created_at, event_type, addressbookid, card_uri, card_row, card_data, effects) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING seq",
            self.prefix
        ));
        let seq: i64 = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(now)
            .bind(event_type)
            .bind(address_book_id)
            .bind(uri)
            .bind(card_row)
            .bind(card.carddata.clone())
            .bind(effects)
            .fetch_one(&mut **tx)
            .await?
            .try_get("seq")?;
        if self.postgres {
            let sql = self.render("SELECT pg_notify(?, ?)");
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(notify_channel)
                .bind(seq.to_string())
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Files (WebDAV files PROPFIND)
    // ------------------------------------------------------------------

    /// Resolves `<internal path>` inside `home::<uid>` by `path_hash`, exactly
    /// like `Cache::get()` (`fs_storage_path_hash` is the unique index).
    pub async fn resolve_home_file(
        &self,
        uid: &str,
        path_hash: &str,
    ) -> Result<Option<FileCacheRow>> {
        let sql = self.render(&format!(
            "SELECT f.fileid, f.storage, f.path, f.name, f.size, f.mtime, f.etag, \
                    f.permissions, f.encrypted, f.unencrypted_size, f.checksum, f.parent, \
                    mt.mimetype, fe.creation_time, md.json AS meta_json \
             FROM {p}filecache f \
             JOIN {p}storages s ON s.numeric_id = f.storage \
             LEFT JOIN {p}mimetypes mt ON mt.id = f.mimetype \
             LEFT JOIN {p}filecache_extended fe ON fe.fileid = f.fileid \
             LEFT JOIN {p}files_metadata md ON md.file_id = f.fileid \
             WHERE s.id = ? AND f.path_hash = ? LIMIT 1",
            p = self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(format!("home::{uid}"))
            .bind(path_hash)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(file_cache_row_from_row).transpose()
    }

    /// `Cache::getFolderContentsById()`: all children of `parent` in `storage`.
    ///
    /// The joins mirror `CacheQueryBuilder::selectFileCache()` /
    /// `selectMetadata()` so the (unordered) row order matches PHP's; no
    /// `ORDER BY` is issued, exactly like PHP.
    pub async fn file_children(&self, storage: i64, parent: i64) -> Result<Vec<FileCacheRow>> {
        let sql = self.render(&format!(
            "SELECT f.fileid, f.storage, f.path, f.name, f.size, f.mtime, f.etag, \
                    f.permissions, f.encrypted, f.unencrypted_size, f.checksum, f.parent, \
                    mt.mimetype, fe.creation_time, md.json AS meta_json \
             FROM {p}filecache f \
             LEFT JOIN {p}mimetypes mt ON mt.id = f.mimetype \
             LEFT JOIN {p}filecache_extended fe ON fe.fileid = f.fileid \
             LEFT JOIN {p}files_metadata md ON md.file_id = f.fileid \
             WHERE f.storage = ? AND f.parent = ?",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(storage)
            .bind(parent)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(file_cache_row_from_row).collect()
    }

    /// The permissions of a single filecache row (the parent of a Depth 0 node,
    /// for `DavUtil::canRename()`).
    pub async fn file_permissions(&self, fileid: i64) -> Result<Option<i64>> {
        let sql = self.render(&format!(
            "SELECT permissions FROM {p}filecache WHERE fileid = ? LIMIT 1",
            p = self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(fileid)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some(row.try_get("permissions")?)),
            None => Ok(None),
        }
    }

    /// `oc_mounts.mount_point` for every mount the user has (the home mount
    /// included). These are *not* rows in the home storage's `oc_filecache`.
    pub async fn user_mount_points(&self, uid: &str) -> Result<Vec<String>> {
        let sql = self.render(&format!(
            "SELECT mount_point FROM {p}mounts WHERE user_id = ?",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_all(&self.pool)
            .await?;
        let mut mounts = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(mount) = row.try_get::<Option<String>, _>("mount_point")? {
                mounts.push(mount);
            }
        }
        Ok(mounts)
    }

    /// Resolves `<internal path>` inside an arbitrary storage by `path_hash`,
    /// exactly like `Cache::get()` (used for paths inside a mount).
    pub async fn resolve_storage_file(
        &self,
        storage: i64,
        path_hash: &str,
    ) -> Result<Option<FileCacheRow>> {
        let sql = self.render(&format!(
            "SELECT f.fileid, f.storage, f.path, f.name, f.size, f.mtime, f.etag, \
                    f.permissions, f.encrypted, f.unencrypted_size, f.checksum, f.parent, \
                    mt.mimetype, fe.creation_time, md.json AS meta_json \
             FROM {p}filecache f \
             LEFT JOIN {p}mimetypes mt ON mt.id = f.mimetype \
             LEFT JOIN {p}filecache_extended fe ON fe.fileid = f.fileid \
             LEFT JOIN {p}files_metadata md ON md.file_id = f.fileid \
             WHERE f.storage = ? AND f.path_hash = ? LIMIT 1",
            p = self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(storage)
            .bind(path_hash)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(file_cache_row_from_row).transpose()
    }

    /// Whether an `oc_<table>` exists (the companion apps are optional).
    pub async fn table_exists(&self, table: &str) -> Result<bool> {
        let sql = self.render(&format!(
            "SELECT 1 FROM {}{} LIMIT 1",
            self.prefix, table
        ));
        match sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&self.pool)
            .await
        {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    // ------------------------------------------------------------------
    // Mounts (files PROPFIND phase 2)
    // ------------------------------------------------------------------

    /// The raw `oc_group_user` gids for `uid` (not principal URIs).
    pub async fn user_group_ids(&self, uid: &str) -> Result<Vec<String>> {
        let sql = self.render(&format!(
            "SELECT gid FROM {p}group_user WHERE uid = ?",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(gid) = row.try_get::<Option<String>, _>("gid")? {
                out.push(gid);
            }
        }
        Ok(out)
    }

    /// Every `oc_mounts` row for `uid`, joined to its root `oc_filecache` row.
    pub async fn mount_base_rows(&self, uid: &str) -> Result<Vec<crate::mounts::MountBaseRow>> {
        let sql = self.render(&format!(
            "SELECT m.mount_point, m.mount_provider_class, m.mount_id, m.storage_id, \
                    m.root_id, s.id AS storage_string, \
                    f.path AS root_path, f.name AS root_name, f.size AS root_size, \
                    f.mtime AS root_mtime, f.etag AS root_etag, \
                    f.permissions AS root_permissions, f.encrypted AS root_encrypted, \
                    f.unencrypted_size AS root_unencrypted_size, \
                    mt.mimetype AS root_mimetype \
             FROM {p}mounts m \
             LEFT JOIN {p}storages s ON s.numeric_id = m.storage_id \
             LEFT JOIN {p}filecache f ON f.fileid = m.root_id \
             LEFT JOIN {p}mimetypes mt ON mt.id = f.mimetype \
             WHERE m.user_id = ? ORDER BY m.mount_point",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::MountBaseRow {
                mount_point: row.try_get::<Option<String>, _>("mount_point")?.unwrap_or_default(),
                provider_class: row
                    .try_get::<Option<String>, _>("mount_provider_class")?
                    .unwrap_or_default(),
                mount_id: row.try_get("mount_id")?,
                storage_id: row.try_get("storage_id")?,
                root_id: row.try_get("root_id")?,
                storage_string: row
                    .try_get::<Option<String>, _>("storage_string")?
                    .unwrap_or_default(),
                root_path: row.try_get("root_path")?,
                root_name: row.try_get("root_name")?,
                root_size: row.try_get("root_size")?,
                root_mtime: row.try_get("root_mtime")?,
                root_etag: row.try_get("root_etag")?,
                root_permissions: row.try_get("root_permissions")?,
                root_encrypted: row.try_get("root_encrypted")?,
                root_unencrypted_size: row.try_get("root_unencrypted_size")?,
                root_mimetype: row.try_get("root_mimetype")?,
            });
        }
        Ok(out)
    }

    /// All `oc_group_folders_groups` rows (folder -> group/circle permission).
    pub async fn group_folder_group_rows(
        &self,
    ) -> Result<Vec<crate::mounts::GroupFolderGroupRow>> {
        let sql = self.render(&format!(
            "SELECT folder_id, permissions, group_id, circle_id FROM {p}group_folders_groups",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::GroupFolderGroupRow {
                folder_id: row.try_get("folder_id")?,
                permissions: row.try_get::<Option<i64>, _>("permissions")?.unwrap_or(0),
                group_id: row.try_get("group_id")?,
                circle_id: row.try_get("circle_id")?,
            });
        }
        Ok(out)
    }

    /// All `oc_group_folders` metadata rows.
    pub async fn group_folder_meta_rows(&self) -> Result<Vec<crate::mounts::GroupFolderMetaRow>> {
        // `acl_default_no_permission` is a PostgreSQL `boolean` in production
        // (and a tinyint on MySQL); select it as a portable integer.
        let sql = self.render(&format!(
            "SELECT folder_id, acl, quota, storage_id, \
                    CASE WHEN acl_default_no_permission THEN 1 ELSE 0 END \
                        AS acl_default_no_permission \
             FROM {p}group_folders",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::GroupFolderMetaRow {
                folder_id: row.try_get("folder_id")?,
                acl: row.try_get::<Option<i64>, _>("acl")?.unwrap_or(0),
                quota: row.try_get::<Option<i64>, _>("quota")?.unwrap_or(-3),
                storage_id: row.try_get::<Option<i64>, _>("storage_id")?.unwrap_or(0),
                acl_default_no_permission: row
                    .try_get::<Option<i64>, _>("acl_default_no_permission")?
                    .unwrap_or(0)
                    != 0,
            });
        }
        Ok(out)
    }

    /// Every `oc_group_folders_acl` row joined to its filecache path.
    pub async fn group_folder_acl_rows(&self) -> Result<Vec<crate::mounts::GroupFolderAclRow>> {
        let sql = self.render(&format!(
            "SELECT f.storage AS storage_id, f.path AS path, a.mapping_type, a.mapping_id, \
                    a.mask, a.permissions \
             FROM {p}group_folders_acl a \
             JOIN {p}filecache f ON f.fileid = a.fileid",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::GroupFolderAclRow {
                storage_id: row.try_get("storage_id")?,
                path: row.try_get::<Option<String>, _>("path")?.unwrap_or_default(),
                mapping_type: row
                    .try_get::<Option<String>, _>("mapping_type")?
                    .unwrap_or_default(),
                mapping_id: row
                    .try_get::<Option<String>, _>("mapping_id")?
                    .unwrap_or_default(),
                mask: row.try_get::<Option<i64>, _>("mask")?.unwrap_or(0),
                permissions: row.try_get::<Option<i64>, _>("permissions")?.unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// Every `oc_group_folders_manage` row.
    pub async fn group_folder_manage_rows(
        &self,
    ) -> Result<Vec<crate::mounts::GroupFolderManageRow>> {
        let sql = self.render(&format!(
            "SELECT folder_id, mapping_type, mapping_id FROM {p}group_folders_manage",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::GroupFolderManageRow {
                folder_id: row.try_get("folder_id")?,
                mapping_type: row
                    .try_get::<Option<String>, _>("mapping_type")?
                    .unwrap_or_default(),
                mapping_id: row
                    .try_get::<Option<String>, _>("mapping_id")?
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Group ids authorized to administer the groupfolders app settings
    /// (`oc_authorized_groups`; NC 33 has no `appid` column, the class encodes
    /// the app).
    pub async fn group_folder_authorized_groups(&self) -> Result<Vec<String>> {
        let sql = self.render(&format!(
            "SELECT group_id FROM {p}authorized_groups WHERE class = ?",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind("OCA\\GroupFolders\\Settings\\Admin")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(group) = row.try_get::<Option<String>, _>("group_id")? {
                out.push(group);
            }
        }
        Ok(out)
    }

    /// Incoming shares relevant to `uid` (user, group and usergroup shares,
    /// plus the types we cannot resolve so the mount is marked unservable).
    pub async fn mount_share_rows(&self, uid: &str) -> Result<Vec<crate::mounts::MountShareRow>> {
        // `oc_share.attributes` is a PostgreSQL `json` column; the `Any` driver
        // cannot decode it, so cast it to text on Postgres.
        let attributes = if self.postgres {
            "s.attributes::text"
        } else {
            "s.attributes"
        };
        let sql = self.render(&format!(
            "SELECT s.file_source, s.id, s.share_type, s.share_with, s.permissions, \
                    s.note, s.hide_download, {attributes} AS attributes, s.uid_owner, \
                    s.accepted, s.stime, \
                    u.displayname AS user_displayname, g.displayname AS group_displayname, \
                    uo.displayname AS owner_displayname \
             FROM {p}share s \
             LEFT JOIN {p}users u ON s.share_type IN (0, 2) AND u.uid = s.share_with \
             LEFT JOIN {p}groups g ON s.share_type = 1 AND g.gid = s.share_with \
             LEFT JOIN {p}users uo ON uo.uid = s.uid_owner \
             WHERE s.item_type IN ('file', 'folder') \
               AND s.share_type IN (0, 1, 2, 7, 10, 11, 12) \
               AND s.uid_owner <> ? AND s.uid_initiator <> ? \
               AND ( \
                     (s.share_type IN (0, 2) AND s.share_with = ?) \
                     OR (s.share_type = 1 AND s.share_with IN \
                         (SELECT gid FROM {p}group_user WHERE uid = ?)) \
                     OR s.share_type IN (7, 10, 11, 12) \
                   )",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .bind(uid)
            .bind(uid)
            .bind(uid)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::MountShareRow {
                file_source: row.try_get("file_source")?,
                id: row.try_get("id")?,
                share_type: row.try_get::<Option<i64>, _>("share_type")?.unwrap_or(0),
                share_with: row.try_get("share_with")?,
                permissions: row.try_get::<Option<i64>, _>("permissions")?.unwrap_or(0),
                note: row.try_get("note")?,
                hide_download: row.try_get::<Option<i64>, _>("hide_download")?.unwrap_or(0),
                attributes: row.try_get("attributes")?,
                uid_owner: row.try_get::<Option<String>, _>("uid_owner")?.unwrap_or_default(),
                accepted: row.try_get::<Option<i64>, _>("accepted")?.unwrap_or(0),
                stime: row.try_get::<Option<i64>, _>("stime")?.unwrap_or(0),
                user_displayname: row
                    .try_get::<Option<String>, _>("user_displayname")?
                    .filter(|name| !name.is_empty()),
                group_displayname: row
                    .try_get::<Option<String>, _>("group_displayname")?
                    .filter(|name| !name.is_empty()),
                owner_displayname: row
                    .try_get::<Option<String>, _>("owner_displayname")?
                    .filter(|name| !name.is_empty()),
            });
        }
        Ok(out)
    }

    /// All `oc_external_mounts` rows.
    pub async fn external_mount_rows(&self) -> Result<Vec<crate::mounts::ExternalMountRow>> {
        let sql = self.render(&format!(
            "SELECT mount_id, storage_backend, auth_backend FROM {p}external_mounts",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::ExternalMountRow {
                mount_id: row.try_get("mount_id")?,
                storage_backend: row
                    .try_get::<Option<String>, _>("storage_backend")?
                    .unwrap_or_default(),
                auth_backend: row.try_get("auth_backend")?,
            });
        }
        Ok(out)
    }

    /// All `oc_external_options` rows.
    pub async fn external_option_rows(&self) -> Result<Vec<crate::mounts::ExternalOptionRow>> {
        let sql = self.render(&format!(
            "SELECT mount_id, key, value FROM {p}external_options",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(crate::mounts::ExternalOptionRow {
                mount_id: row.try_get("mount_id")?,
                key: row.try_get::<Option<String>, _>("key")?.unwrap_or_default(),
                value: row.try_get::<Option<String>, _>("value")?.unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Shares in a folder, keyed by `file_source`, exactly like
    /// `SharesPlugin::preloadCollection()` -> `DefaultShareProvider::getSharesInFolder()`:
    /// user/group/link shares the caller owns or initiated, on a direct child of
    /// `parent`. One query for the whole folder (never per child).
    pub async fn folder_share_rows(
        &self,
        uid: &str,
        parent: i64,
    ) -> Result<HashMap<i64, Vec<ShareRow>>> {
        let sql = self.render(&format!(
            "SELECT s.file_source, s.share_type, s.share_with, s.permissions, \
                    u.displayname AS user_displayname, g.displayname AS group_displayname \
             FROM {p}share s \
             JOIN {p}filecache f ON f.fileid = s.file_source \
             LEFT JOIN {p}users u ON s.share_type = 0 AND u.uid = s.share_with \
             LEFT JOIN {p}groups g ON s.share_type = 1 AND g.gid = s.share_with \
             WHERE s.item_type IN ('file', 'folder') \
               AND s.share_type IN (0, 1, 3) \
               AND (s.uid_owner = ? OR s.uid_initiator = ?) \
               AND f.parent = ?",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .bind(uid)
            .bind(parent)
            .fetch_all(&self.pool)
            .await?;
        let mut shares: HashMap<i64, Vec<ShareRow>> = HashMap::new();
        for row in &rows {
            let share = share_row_from_row(row)?;
            shares.entry(share.file_source).or_default().push(share);
        }
        Ok(shares)
    }

    /// Shares on a single node, exactly like `SharesPlugin::getShares()` ->
    /// `getSharesBy(..., reshares=false)`: shares the caller initiated, across
    /// every type the plugin asks for. Received shares are not included: a
    /// received share is a mount, and any listing containing one is delegated.
    pub async fn node_share_rows(&self, uid: &str, fileid: i64) -> Result<Vec<ShareRow>> {
        let sql = self.render(&format!(
            "SELECT s.file_source, s.share_type, s.share_with, s.permissions, \
                    u.displayname AS user_displayname, g.displayname AS group_displayname \
             FROM {p}share s \
             LEFT JOIN {p}users u ON s.share_type = 0 AND u.uid = s.share_with \
             LEFT JOIN {p}groups g ON s.share_type = 1 AND g.gid = s.share_with \
             WHERE s.item_type IN ('file', 'folder') \
               AND s.share_type IN (0, 1, 3, 4, 6, 7, 10, 12) \
               AND s.uid_initiator = ? \
               AND s.file_source = ?",
            p = self.prefix
        ));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .bind(fileid)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(share_row_from_row).collect()
    }

    /// `TagsPlugin`'s favorite prefetch: which of `ids` carry the
    /// `_$!<Favorite>!$_` tag for `uid`. Batched by 900, like
    /// `Tags::getTagsForObjects()`.
    pub async fn favorite_fileids(&self, uid: &str, ids: &[i64]) -> Result<HashSet<i64>> {
        let mut favorites = HashSet::new();
        for chunk in ids.chunks(900) {
            let mut sql = format!(
                "SELECT r.objid FROM {p}vcategory_to_object r \
                 JOIN {p}vcategory t ON t.id = r.categoryid \
                 WHERE t.uid = {} AND r.type = 'files' AND t.category = {} AND r.objid IN (",
                self.ph(1),
                self.ph(2),
                p = self.prefix
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&self.ph(i + 3));
            }
            sql.push(')');
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(uid)
                .bind(crate::files::TAG_FAVORITE);
            for id in chunk {
                query = query.bind(*id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in &rows {
                favorites.insert(row.try_get::<i64, _>("objid")?);
            }
        }
        Ok(favorites)
    }

    /// `CommentPropertiesPlugin`'s unread prefetch
    /// (`Comments\Manager::getNumberOfUnreadCommentsForObjects()`), batched the
    /// same way (1000 ids per query).
    pub async fn unread_comment_counts(
        &self,
        uid: &str,
        ids: &[i64],
    ) -> Result<HashMap<i64, i64>> {
        let mut counts = HashMap::new();
        for chunk in ids.chunks(1000) {
            let mut sql = format!(
                "SELECT c.object_id, count(c.id) AS num_comments \
                 FROM {p}comments c \
                 LEFT JOIN {p}comments_read_markers m \
                   ON m.user_id = {} AND c.object_type = m.object_type AND c.object_id = m.object_id \
                 WHERE c.object_type = {} AND c.object_id IN (",
                self.ph(1),
                self.ph(2),
                p = self.prefix
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&self.ph(i + 3));
            }
            sql.push_str(") AND (c.creation_timestamp > m.marker_datetime OR m.marker_datetime IS NULL) GROUP BY c.object_id");
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(uid)
                .bind("files");
            for id in chunk {
                query = query.bind(id.to_string());
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in &rows {
                let object_id: String = row.try_get("object_id")?;
                let count: i64 = row.try_get("num_comments")?;
                if let Ok(id) = object_id.parse::<i64>() {
                    counts.insert(id, count);
                }
            }
        }
        Ok(counts)
    }

    /// `oc_preferences` lookup (the user quota lives at `files/quota`).
    pub async fn user_preference(&self, uid: &str, app: &str, key: &str) -> Result<Option<String>> {
        let sql = self.render(&format!(
            "SELECT configvalue FROM {p}preferences \
             WHERE userid = ? AND appid = ? AND configkey = ? LIMIT 1",
            p = self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .bind(app)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(row.try_get::<Option<String>, _>("configvalue")?),
            None => Ok(None),
        }
    }

    /// The `oc_accounts.data` JSON for a user, used by the DAV discovery
    /// `{DAV:}alternate-URI-set` (the `additional_mail` property collection).
    ///
    /// `AccountManager::getAccount()` reads this table; when it is unavailable
    /// the caller treats the account as having no extra addresses.
    pub async fn account_data(&self, uid: &str) -> Result<Option<String>> {
        let sql = self.render(&format!(
            "SELECT data FROM {p}accounts WHERE uid = ? LIMIT 1",
            p = self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(row.try_get::<Option<String>, _>("data")?),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Brute force (read + the single opt-in write)
    // ------------------------------------------------------------------

    pub async fn bruteforce_attempts(&self, subnet: &str, action: &str, since: i64) -> Result<i64> {
        let sql = self.render(&format!(
            "SELECT COUNT(*) AS c FROM {}bruteforce_attempts \
             WHERE occurred > ? AND subnet = ? AND action = ?",
            self.prefix
        ));
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(since)
            .bind(subnet)
            .bind(action)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")?)
    }

    /// The only non-`SELECT` statement in the sidecar.
    ///
    /// Mirrors `OC\Security\Bruteforce\Backend\DatabaseBackend::registerAttempt()`.
    /// It is only called when `nextcloud_dav.record_bruteforce_attempts` is
    /// enabled.
    pub async fn register_bruteforce_attempt(
        &self,
        ip: &str,
        subnet: &str,
        occurred: i64,
        action: &str,
        metadata: &str,
    ) -> Result<()> {
        let sql = self.render(&format!(
            "INSERT INTO {}bruteforce_attempts (ip, subnet, occurred, action, metadata) \
             VALUES (?, ?, ?, ?, ?)",
            self.prefix
        ));
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ip)
            .bind(subnet)
            .bind(occurred)
            .bind(action)
            .bind(metadata)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Rewrites `?` placeholders to `$n` for PostgreSQL. Exposed for tests.
pub fn render_placeholders(sql: &str, postgres: bool) -> String {
    if !postgres {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 8);
    let mut index = 0;
    for c in sql.chars() {
        if c == '?' {
            index += 1;
            out.push('$');
            out.push_str(&index.to_string());
        } else {
            out.push(c);
        }
    }
    out
}

/// 1-based placeholder for position `index`. Exposed for tests.
pub fn placeholder(index: usize, postgres: bool) -> String {
    if postgres {
        format!("${index}")
    } else {
        "?".to_string()
    }
}

/// PHP's `urlencode()` (RFC 1738): alphanumerics and `-`, `_`, `.` are left
/// unescaped, a space becomes `+`, and every other byte is `%XX` with
/// upper-case hex. This is what `Principal::getGroupMembership()` applies to a
/// gid before building `principals/groups/<gid>`.
///
/// It differs from RFC 3986 percent-encoding (which leaves `~` and escapes a
/// space as `%20`), and from `rawurlencode()` (which escapes `+` as `%2B`).
pub fn php_urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Unix seconds, as PHP's `time()`.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// `md5($cardData)` as lower-case hex.
pub fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data))
}

/// `mb_strcut($value, 0, 254)`: truncate to at most `max` bytes without
/// splitting a UTF-8 code point.
pub fn truncate_utf8(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn address_book_from_row(row: &AnyRow) -> Result<AddressBook> {
    Ok(AddressBook {
        id: row.try_get("id")?,
        uri: row.try_get::<Option<String>, _>("uri")?.unwrap_or_default(),
        displayname: row.try_get("displayname")?,
        principaluri: row
            .try_get::<Option<String>, _>("principaluri")?
            .unwrap_or_default(),
        description: row.try_get("description")?,
        synctoken: row.try_get("synctoken")?,
    })
}

fn card_from_row(row: &AnyRow) -> Result<Card> {
    let stored_size: i64 = row.try_get("size")?;
    let raw: Option<Vec<u8>> = row.try_get("carddata")?;
    let raw = raw.unwrap_or_default();
    let (carddata, modified) = vcard::filter_read_blob(&raw);
    let size = if modified {
        carddata.len() as i64
    } else {
        stored_size
    };
    Ok(Card {
        id: row.try_get("id")?,
        uri: row.try_get::<Option<String>, _>("uri")?.unwrap_or_default(),
        etag: row
            .try_get::<Option<String>, _>("etag")?
            .unwrap_or_default(),
        size,
        lastmodified: row.try_get("lastmodified")?,
        carddata,
    })
}

fn file_cache_row_from_row(row: &AnyRow) -> Result<FileCacheRow> {
    Ok(FileCacheRow {
        fileid: row.try_get("fileid")?,
        storage: row.try_get("storage")?,
        path: row.try_get::<Option<String>, _>("path")?.unwrap_or_default(),
        name: row.try_get::<Option<String>, _>("name")?.unwrap_or_default(),
        size: row.try_get::<Option<i64>, _>("size")?.unwrap_or_default(),
        mtime: row.try_get::<Option<i64>, _>("mtime")?.unwrap_or_default(),
        etag: row.try_get::<Option<String>, _>("etag")?.unwrap_or_default(),
        permissions: row
            .try_get::<Option<i64>, _>("permissions")?
            .unwrap_or_default(),
        encrypted: row.try_get::<Option<i64>, _>("encrypted")?.unwrap_or_default(),
        unencrypted_size: row.try_get::<Option<i64>, _>("unencrypted_size")?,
        checksum: row.try_get("checksum")?,
        parent: row.try_get::<Option<i64>, _>("parent")?.unwrap_or_default(),
        mimetype: row
            .try_get::<Option<String>, _>("mimetype")?
            .unwrap_or_default(),
        creation_time: row
            .try_get::<Option<i64>, _>("creation_time")?
            .unwrap_or_default(),
        metadata: parse_metadata(row.try_get::<Option<String>, _>("meta_json")?),
    })
}

/// Parses `oc_files_metadata.json` into `key -> value`, exactly like
/// `FileInfo::getMetadata()`: each entry is `{"value": …, "type": …}` and the
/// inner `value` is what `FilesPlugin` serialises. Entries without a value are
/// dropped (PHP's `isset()`/`getValueAny()` semantics).
fn parse_metadata(raw: Option<String>) -> HashMap<String, Value> {
    let Some(raw) = raw else {
        return HashMap::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let mut metadata = HashMap::with_capacity(map.len());
    for (key, entry) in map {
        if let Some(value) = entry.get("value") {
            if !value.is_null() {
                metadata.insert(key, value.clone());
            }
        }
    }
    metadata
}

fn share_row_from_row(row: &AnyRow) -> Result<ShareRow> {
    Ok(ShareRow {
        file_source: row.try_get("file_source")?,
        share_type: row.try_get("share_type")?,
        share_with: row.try_get::<Option<String>, _>("share_with")?,
        permissions: row
            .try_get::<Option<i64>, _>("permissions")?
            .unwrap_or_default(),
        user_displayname: row
            .try_get::<Option<String>, _>("user_displayname")?
            .filter(|name| !name.is_empty()),
        group_displayname: row
            .try_get::<Option<String>, _>("group_displayname")?
            .filter(|name| !name.is_empty()),
    })
}

fn auth_token_from_row(row: &AnyRow) -> Result<AuthToken> {
    let flag: String = row.try_get("password_invalid_flag")?;
    Ok(AuthToken {
        uid: row.try_get::<Option<String>, _>("uid")?.unwrap_or_default(),
        login_name: row
            .try_get::<Option<String>, _>("login_name")?
            .unwrap_or_default(),
        token_type: row.try_get("type")?,
        expires: row
            .try_get::<Option<i64>, _>("expires")?
            .filter(|expires| *expires != 0),
        password_invalid: flag != "0",
        last_check: row
            .try_get::<Option<i64>, _>("last_check")?
            .unwrap_or_default(),
        last_activity: row
            .try_get::<Option<i64>, _>("last_activity")?
            .unwrap_or_default(),
    })
}

// Keep the `Any`/`Error` imports obviously used in case of refactors.
const _: fn() = || {
    let _ = std::marker::PhantomData::<Any>;
    let _ = Error::NotFound;
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_postgres_placeholders() {
        assert_eq!(
            render_placeholders("SELECT * FROM x WHERE a = ? AND b = ?", true),
            "SELECT * FROM x WHERE a = $1 AND b = $2"
        );
        assert_eq!(placeholder(3, true), "$3");
    }

    #[test]
    fn keeps_question_marks_for_mysql_and_sqlite() {
        assert_eq!(
            render_placeholders("SELECT * FROM x WHERE a = ?", false),
            "SELECT * FROM x WHERE a = ?"
        );
        assert_eq!(placeholder(3, false), "?");
    }

    #[test]
    fn php_urlencode_matches_php() {
        // Alphanumerics and `-_.` stay; a space becomes `+`; everything else
        // is upper-case `%XX` (notably `~` -> `%7E`, unlike RFC 3986).
        assert_eq!(php_urlencode("team"), "team");
        assert_eq!(php_urlencode("a-b_c.d"), "a-b_c.d");
        assert_eq!(php_urlencode("a b"), "a+b");
        assert_eq!(php_urlencode("a+b"), "a%2Bb");
        assert_eq!(php_urlencode("a~b"), "a%7Eb");
        assert_eq!(php_urlencode("a/b"), "a%2Fb");
        assert_eq!(php_urlencode("grüppe"), "gr%C3%BCppe");
        assert_eq!(php_urlencode("100%"), "100%25");
    }
}
