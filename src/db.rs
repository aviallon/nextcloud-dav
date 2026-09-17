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
use crate::model::{AddressBook, AuthToken, Card, CardIdUri, ChangeRow};
use crate::vcard;
use sqlx::any::{AnyConnectOptions, AnyPoolOptions, AnyRow};
use sqlx::{Any, AnyPool, Row};

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
}
