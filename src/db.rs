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
use crate::model::{AddressBook, AuthToken, Card, CardIdUri, ChangeRow, VisibleBook};
use crate::vcard;
use md5::{Digest, Md5};
use sqlx::any::{AnyConnectOptions, AnyPoolOptions, AnyRow};
use sqlx::{Any, AnyPool, Row};

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
