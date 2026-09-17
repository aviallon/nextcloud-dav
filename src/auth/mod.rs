// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Authentication: the app-password fast path, the PHP fallback and the
//! brute-force throttle in front of both.

pub mod throttle;
pub mod token;

use crate::config::{BruteforceConfig, TOKEN_RECHECK_INTERVAL};
use crate::db::Db;
use crate::model::AuthToken;
use crate::php::{PhpAuth, PhpClient};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How a request was authenticated, for logging/metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Validated against `oc_authtoken` without PHP.
    FastPath,
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

pub struct Authenticator {
    db: Arc<Db>,
    php: PhpClient,
    secret: String,
    bruteforce: BruteforceConfig,
}

impl Authenticator {
    pub fn new(db: Arc<Db>, php: PhpClient, secret: String, bruteforce: BruteforceConfig) -> Self {
        Self {
            db,
            php,
            secret,
            bruteforce,
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
            last_check,
            last_activity: last_check,
        }
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
