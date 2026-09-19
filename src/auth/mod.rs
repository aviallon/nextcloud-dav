// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Authentication: the app-password fast path, the PHP fallback and the
//! brute-force throttle in front of both.

pub mod session;
pub mod session_redis;
pub mod throttle;
pub mod token;

use crate::config::{BruteforceConfig, TOKEN_RECHECK_INTERVAL};
use crate::db::Db;
use crate::model::AuthToken;
use crate::php::{PhpAuth, PhpClient};
use axum::http::HeaderMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How a request was authenticated, for logging/metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Validated against `oc_authtoken` without PHP.
    FastPath,
    /// Validated from a Nextcloud session cookie and its Redis payload.
    Session,
    /// Delegated to `PROPFIND /remote.php/dav/`.
    PhpFallback,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub uid: String,
    pub method: AuthMethod,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    Invalid,
    #[error("too many failed login attempts")]
    Throttled { retry_after_secs: u64 },
    #[error("instance is in maintenance mode")]
    Maintenance,
    #[error("authentication backend error: {0}")]
    Upstream(String),
}

/// Current unix timestamp.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The pure part of the fast-path decision, factored out so the gates can be
/// unit-tested without a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenDecision {
    Accept(String),
    Reject,
    Fallback,
}

/// Evaluates the `checkToken()` / `validateTokenLoginName()` / native-backend
/// gates of design doc §3.2.
pub fn classify_token_state(
    row: &AuthToken,
    presented_user: &str,
    now: i64,
    native_user_exists: bool,
    user_disabled: bool,
) -> TokenDecision {
    // `IToken::WIPE_TOKEN` is a revocation marker and never authenticates.
    if row.token_type == 2 {
        return TokenDecision::Reject;
    }
    // Only PERMANENT (1) and ONETIME (3) tokens are accepted; anything else is
    // left to PHP, which owns the exact rejection semantics.
    if row.token_type != 1 && row.token_type != 3 {
        return TokenDecision::Fallback;
    }
    // A non-zero `expires` in the past: PHP rejects, but the design mandates
    // delegating so the audit trail stays in one place.
    if let Some(expires) = row.expires {
        if expires < now {
            return TokenDecision::Fallback;
        }
    }
    if row.password_invalid {
        return TokenDecision::Reject;
    }
    if row.uid.is_empty() || !token::login_name_matches(&row.login_name, presented_user) {
        return TokenDecision::Reject;
    }
    // LDAP/SSO users are not in `oc_users`; their password checker lives in
    // PHP, so they cannot use the fast path.
    if !native_user_exists {
        return TokenDecision::Fallback;
    }
    if user_disabled {
        return TokenDecision::Reject;
    }
    // Periodic credential re-check (`checkTokenCredentials()`), every 5 min.
    if row.last_check < now - TOKEN_RECHECK_INTERVAL {
        return TokenDecision::Fallback;
    }
    TokenDecision::Accept(row.uid.clone())
}

/// Evaluates the token gate for the **session-cookie** path, which follows
/// `OC\User\Session::checkTokenCredentials()` rather than the app-password fast
/// path.
///
/// The session token is `session['app_password']` when present, else the
/// session id. `OC\Authentication\Token\PublicKeyTokenProvider::getToken()`
/// rejects an expired token and a `WIPE_TOKEN` (type 2), so those delegate.
/// Every other type, including a browser session's `TEMPORARY_TOKEN` (0), is
/// valid - type is not a rejection criterion here. Then
/// `checkTokenCredentials()` accepts when the token was checked within the last
/// 300 s, or when it is passwordless (`password IS NULL`). An app-password
/// token that carries a password and a stale `last_check` needs
/// `checkPassword()`, which the sidecar cannot reproduce, so it delegates.
/// The enabled-user check is already enforced by the caller (`user_disabled`).
pub fn classify_session_token_state(
    row: &AuthToken,
    session_uid: &str,
    now: i64,
    user_disabled: bool,
) -> TokenDecision {
    // `getToken()` throws `WipeTokenException`; never authenticate a marker.
    if row.token_type == 2 {
        return TokenDecision::Reject;
    }
    // `getToken()` throws `ExpiredTokenException`; delegate for the same audit
    // trail as the fast path (a non-zero `expires` in the past).
    if let Some(expires) = row.expires {
        if expires < now {
            return TokenDecision::Fallback;
        }
    }
    if row.password_invalid {
        return TokenDecision::Reject;
    }
    if row.uid.is_empty() || row.uid != session_uid {
        return TokenDecision::Reject;
    }
    if user_disabled {
        return TokenDecision::Reject;
    }
    // `checkTokenCredentials()`: `$lastCheck > ($now - 300)` returns true.
    if row.last_check > now - TOKEN_RECHECK_INTERVAL {
        return TokenDecision::Accept(row.uid.clone());
    }
    // `getPassword()` throws `PasswordlessTokenException`: valid with no
    // password, so `checkTokenCredentials()` returns true without re-checking
    // the login credentials.
    if row.password_is_null {
        return TokenDecision::Accept(row.uid.clone());
    }
    // Password-bearing token with a stale check: the sidecar cannot run
    // `checkPassword()`, so delegate.
    TokenDecision::Fallback
}

pub struct Authenticator {
    db: Arc<Db>,
    php: PhpClient,
    secret: String,
    bruteforce: BruteforceConfig,
    /// `session.save_path` for the Redis session store. `None` keeps session
    /// auth delegated to PHP.
    session_redis: Option<session_redis::RedisConfig>,
    /// `$CONFIG['instanceid']`, part of the session cookie name.
    instance_id: String,
}

impl Authenticator {
    pub fn new(
        db: Arc<Db>,
        php: PhpClient,
        secret: String,
        bruteforce: BruteforceConfig,
        session_redis: Option<session_redis::RedisConfig>,
        instance_id: String,
    ) -> Self {
        Self {
            db,
            php,
            secret,
            bruteforce,
            session_redis,
            instance_id,
        }
    }

    /// Authenticates a Basic credential pair.
    ///
    /// This mirrors, in order:
    /// 1. `sleepDelayOrThrowOnMax()` — throttle before touching credentials;
    /// 2. `PublicKeyTokenProvider::getToken()` + `checkToken()` + the native
    ///    user and enabled gates;
    /// 3. a credentialed PROPFIND fallback that also refreshes `last_check`.
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
        ip: IpAddr,
    ) -> std::result::Result<AuthenticatedUser, AuthError> {
        let now = now_unix();

        if self.bruteforce.enabled {
            let subnet = throttle::normalized_subnet(ip, self.bruteforce.ipv6_subnet_size);
            let attempts_12h = self
                .db
                .bruteforce_attempts(&subnet, "login", now - 43_200)
                .await
                .map_err(|e| AuthError::Upstream(e.to_string()))?;
            if attempts_12h > self.bruteforce.max_attempts {
                let attempts_30m = self
                    .db
                    .bruteforce_attempts(&subnet, "login", now - 1_800)
                    .await
                    .map_err(|e| AuthError::Upstream(e.to_string()))?;
                if attempts_30m > self.bruteforce.max_attempts {
                    log::info!(
                        "blocking {ip}: {attempts_30m} failed logins in the last 30 minutes"
                    );
                    return Err(AuthError::Throttled {
                        retry_after_secs: 30,
                    });
                }
            }
            let delay = throttle::calculate_delay(attempts_12h, self.bruteforce.max_attempts);
            if delay > 0 {
                log::debug!("throttling {ip} for {delay} ms before credential check");
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
            }
        }

        match self.fast_path(username, password, now).await {
            Ok(Some(user)) => return Ok(user),
            Ok(None) => {}
            Err(AuthError::Invalid) => {
                self.record_failure(ip, username, now).await;
                return Err(AuthError::Invalid);
            }
            Err(other) => return Err(other),
        }

        self.php_fallback(username, password, ip).await
    }

    /// Attempts the Nextcloud **session-cookie** path. Returns `None` on any
    /// uncertainty; the caller must then delegate (501).
    ///
    /// The order is design doc §6: cookies -> Redis -> crypto -> JSON ->
    /// user_id -> user exists/enabled -> token revalidation -> 2FA -> the two
    /// acceptance branches with PHP's CSRF rule.
    pub async fn authenticate_session(
        &self,
        headers: &HeaderMap,
        method: &str,
        query: Option<&str>,
    ) -> Option<AuthenticatedUser> {
        let redis = self.session_redis.as_ref()?;
        if self.instance_id.is_empty() {
            return None;
        }

        // Step 1: both cookies must be present.
        let session_id =
            session::cookie_value(headers, &session::session_cookie_name(&self.instance_id))?;
        let passphrase = session::cookie_value(headers, "oc_sessionPassphrase")?;
        if session_id.is_empty() || passphrase.is_empty() {
            return None;
        }

        // Step 2: the session must exist in Redis.
        let key = format!("PHPREDIS_SESSION:{session_id}");
        let raw = match session_redis::get(redis, &key).await {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                log::debug!("session cookie references a missing session");
                return None;
            }
            Err(error) => {
                log::warn!("session store unavailable; delegating DAV request: {error}");
                return None;
            }
        };

        // Step 3: extract, verify and decrypt the envelope.
        let json = session::extract_and_decrypt(&raw, &passphrase)?;
        let payload = session::SessionPayload::parse(&json)?;

        // Step 4: user_id.
        let uid = payload.user_id.clone();

        // Step 5: the user must be a native, enabled account.
        let native = self.db.native_user_exists(&uid).await.ok()?;
        if !native {
            return None;
        }
        let disabled = self.db.user_is_disabled(&uid).await.ok()?;
        if disabled {
            return None;
        }

        // Step 6: token revalidation (`Session::validateSession()`). An
        // app-password session validates the app password; a browser session
        // validates its session-id token. A browser session's token is a
        // passwordless `TEMPORARY_TOKEN` (type 0), so the app-password type
        // gate must not apply here - `classify_session_token_state` follows
        // `Session::checkTokenCredentials()` instead. Anything it cannot prove
        // is a `Fallback`/`Reject`, which delegates.
        let token = match payload.app_password.as_deref() {
            Some(app_password) => app_password,
            None => session_id.as_str(),
        };
        if token.is_empty() {
            return None;
        }
        let hash = token::hash_token(token, &self.secret);
        let row = self.db.authtoken_by_hash(&hash).await.ok()??;
        match classify_session_token_state(&row, &uid, now_unix(), disabled) {
            TokenDecision::Accept(token_uid) if token_uid == uid => {}
            _ => {
                log::debug!("session token failed revalidation; delegating");
                return None;
            }
        }

        // Steps 7-9: 2FA and the acceptance branches, including PHP's CSRF rule.
        let requesttoken_param = session::query_param(query, "requesttoken");
        let facts = session::RequestFacts {
            method,
            requesttoken_header: headers.get("requesttoken").and_then(|v| v.to_str().ok()),
            requesttoken_param: requesttoken_param.as_deref(),
            strict_cookie: session::same_site_cookie(headers, "nc_sameSiteCookiestrict"),
            lax_cookie: session::same_site_cookie(headers, "nc_sameSiteCookielax"),
        };
        let uid = session::decision_after_token(&payload, &facts)?;
        log::debug!("authenticated {uid} via Nextcloud session cookie");
        Some(AuthenticatedUser {
            uid,
            method: AuthMethod::Session,
        })
    }

    /// Returns `Ok(Some(user))` on a fast-path success, `Ok(None)` to delegate
    /// to PHP, and `Err(Invalid)` for a definitive rejection.
    async fn fast_path(
        &self,
        username: &str,
        password: &str,
        now: i64,
    ) -> std::result::Result<Option<AuthenticatedUser>, AuthError> {
        let hash = token::hash_token(password, &self.secret);
        let mut row = self.db.authtoken_by_hash(&hash).await.map_err(upstream)?;
        if row.is_none() && !self.secret.is_empty() {
            // Legacy instances whose `secret` was empty stored `sha512(token)`.
            let legacy = token::hash_token_without_secret(password);
            row = self.db.authtoken_by_hash(&legacy).await.map_err(upstream)?;
        }

        let Some(row) = row else {
            return Ok(None);
        };

        // The native-backend probe and the enabled flag are cheap indexed
        // lookups; doing them unconditionally keeps the gate order below
        // single-pass and correct.
        let native = self
            .db
            .native_user_exists(&row.uid)
            .await
            .map_err(upstream)?;
        let disabled = if native {
            self.db.user_is_disabled(&row.uid).await.map_err(upstream)?
        } else {
            false
        };

        match classify_token_state(&row, username, now, native, disabled) {
            TokenDecision::Accept(uid) => Ok(Some(AuthenticatedUser {
                uid,
                method: AuthMethod::FastPath,
            })),
            TokenDecision::Reject => Err(AuthError::Invalid),
            TokenDecision::Fallback => Ok(None),
        }
    }

    async fn php_fallback(
        &self,
        username: &str,
        password: &str,
        ip: IpAddr,
    ) -> std::result::Result<AuthenticatedUser, AuthError> {
        let forwarded_for = ip.to_string();
        match self
            .php
            .authenticate(username, password, Some(&forwarded_for))
            .await
        {
            PhpAuth::Authenticated(uid) => Ok(AuthenticatedUser {
                uid,
                method: AuthMethod::PhpFallback,
            }),
            PhpAuth::Invalid => Err(AuthError::Invalid),
            PhpAuth::Throttled => Err(AuthError::Throttled {
                retry_after_secs: 30,
            }),
            PhpAuth::Maintenance => Err(AuthError::Maintenance),
            PhpAuth::Other(status) => Err(AuthError::Upstream(format!(
                "php fallback returned HTTP {status}"
            ))),
        }
    }

    async fn record_failure(&self, ip: IpAddr, username: &str, now: i64) {
        if !self.bruteforce.enabled || !self.bruteforce.record_attempts {
            return;
        }
        let subnet = throttle::normalized_subnet(ip, self.bruteforce.ipv6_subnet_size);
        let metadata = serde_json::json!({ "user": username }).to_string();
        let metadata = trim_metadata(&metadata);
        if let Err(e) = self
            .db
            .register_bruteforce_attempt(&ip.to_string(), &subnet, now, "login", &metadata)
            .await
        {
            log::warn!("failed to record bruteforce attempt: {e}");
        }
    }
}

fn upstream(error: crate::error::Error) -> AuthError {
    AuthError::Upstream(error.to_string())
}

/// Byte-truncates the metadata JSON the way PHP's `substr(..., 0, 254)` does,
/// keeping the result within the 254-byte budget including the ellipsis so the
/// INSERT cannot fail on a narrower column.
fn trim_metadata(metadata: &str) -> String {
    const BUDGET: usize = 254;
    if metadata.len() <= BUDGET {
        return metadata.to_string();
    }
    let mut end = BUDGET - '…'.len_utf8();
    while end > 0 && !metadata.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = metadata[..end].to_string();
    out.push('…');
    out
}

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("secret", &"<redacted>")
            .field("bruteforce", &self.bruteforce)
            .finish()
    }
}

// `Result` is used in the module's public signatures.
#[cfg(test)]
mod tests {
    use super::*;

    fn token_row(token_type: i64, uid: &str, login_name: &str, last_check: i64) -> AuthToken {
        AuthToken {
            uid: uid.to_string(),
            login_name: login_name.to_string(),
            token_type,
            expires: None,
            password_invalid: false,
            password_is_null: false,
            last_check,
            last_activity: last_check,
        }
    }

    #[test]
    fn session_accepts_a_passwordless_temporary_token() {
        // A plain browser session: TEMPORARY_TOKEN (0), no password. Within the
        // 300 s window and beyond it, both are valid (`getPassword` throws
        // PasswordlessTokenException -> no password re-check).
        let mut row = token_row(0, "alice", "alice", 1_000);
        row.password_is_null = true;
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_100, false),
            TokenDecision::Accept("alice".to_string())
        );
        // Stale last_check: still accepted because it is passwordless.
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_301, false),
            TokenDecision::Accept("alice".to_string())
        );
        // Exactly at the boundary: the fresh check is false, but passwordless
        // makes it valid anyway.
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_300, false),
            TokenDecision::Accept("alice".to_string())
        );
        // One second before the boundary the fresh check alone accepts.
        assert!(matches!(
            classify_session_token_state(&row, "alice", 1_299, false),
            TokenDecision::Accept(_)
        ));
    }

    #[test]
    fn session_delegates_a_stale_password_bearing_token() {
        // An app-password session whose `last_check` is stale: the sidecar
        // cannot re-check the password it does not have, so it must delegate.
        let row = token_row(1, "alice", "alice", 1_000);
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_100, false),
            TokenDecision::Accept("alice".to_string())
        );
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_301, false),
            TokenDecision::Fallback
        );
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_300, false),
            TokenDecision::Fallback
        );
    }

    #[test]
    fn session_rejects_a_different_uid_and_wipe_token() {
        let mut row = token_row(2, "bob", "bob", 1_000);
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_100, false),
            TokenDecision::Reject
        );
        row.token_type = 0;
        row.password_is_null = true;
        assert_eq!(
            classify_session_token_state(&row, "alice", 1_100, false),
            TokenDecision::Reject
        );
    }

    #[test]
    fn accepts_a_valid_permanent_token() {
        let row = token_row(1, "alice", "alice@example.com", 1_000);
        assert_eq!(
            classify_token_state(&row, "alice@example.com", 1_100, true, false),
            TokenDecision::Accept("alice".to_string())
        );
    }

    #[test]
    fn rejects_wipe_token() {
        let row = token_row(2, "alice", "alice", 1_000);
        assert_eq!(
            classify_token_state(&row, "alice", 1_100, true, false),
            TokenDecision::Reject
        );
    }

    #[test]
    fn delegates_temporary_tokens() {
        let row = token_row(0, "alice", "alice", 1_000);
        assert_eq!(
            classify_token_state(&row, "alice", 1_100, true, false),
            TokenDecision::Fallback
        );
    }

    #[test]
    fn rejects_wrong_login_name() {
        let row = token_row(1, "alice", "alice@example.com", 1_000);
        assert_eq!(
            classify_token_state(&row, "bob@example.com", 1_100, true, false),
            TokenDecision::Reject
        );
        // Case-insensitive match is fine.
        assert!(matches!(
            classify_token_state(&row, "ALICE@EXAMPLE.COM", 1_100, true, false),
            TokenDecision::Accept(_)
        ));
    }

    #[test]
    fn rejects_password_invalid() {
        let mut row = token_row(1, "alice", "alice", 1_000);
        row.password_invalid = true;
        assert_eq!(
            classify_token_state(&row, "alice", 1_100, true, false),
            TokenDecision::Reject
        );
    }

    #[test]
    fn delegates_expired_token() {
        let mut row = token_row(1, "alice", "alice", 1_000);
        row.expires = Some(1_050);
        assert_eq!(
            classify_token_state(&row, "alice", 1_100, true, false),
            TokenDecision::Fallback
        );
        // A future expiry is still valid.
        row.expires = Some(2_000);
        assert!(matches!(
            classify_token_state(&row, "alice", 1_100, true, false),
            TokenDecision::Accept(_)
        ));
    }

    #[test]
    fn delegates_non_native_users() {
        let row = token_row(1, "ldapuser", "ldapuser", 1_000);
        assert_eq!(
            classify_token_state(&row, "ldapuser", 1_100, false, false),
            TokenDecision::Fallback
        );
    }

    #[test]
    fn rejects_disabled_users() {
        let row = token_row(1, "alice", "alice", 1_000);
        assert_eq!(
            classify_token_state(&row, "alice", 1_100, true, true),
            TokenDecision::Reject
        );
    }

    #[test]
    fn delegates_stale_last_check() {
        let row = token_row(1, "alice", "alice", 1_000);
        // last_check is older than 300 s.
        assert_eq!(
            classify_token_state(&row, "alice", 1_301, true, false),
            TokenDecision::Fallback
        );
        // Exactly at the boundary is still accepted.
        assert!(matches!(
            classify_token_state(&row, "alice", 1_300, true, false),
            TokenDecision::Accept(_)
        ));
    }

    #[test]
    fn metadata_truncation_is_byte_bounded() {
        let long = format!("{{\"user\":\"{}\"}}", "a".repeat(400));
        let trimmed = trim_metadata(&long);
        assert!(trimmed.len() <= 254);
        assert!(trimmed.ends_with('…'));
        assert_eq!(trim_metadata("{\"user\":\"a\"}"), "{\"user\":\"a\"}");
    }

    #[test]
    fn now_is_reasonable() {
        assert!(now_unix() > 1_600_000_000);
    }
}
