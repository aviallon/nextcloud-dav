// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! App-password hashing.
//!
//! Nextcloud stores app passwords as an opaque, fast hash of
//! `token || config.secret`:
//! `PublicKeyTokenProvider::hashToken()` uses `hash('sha512', $token . $secret)`.
//! A legacy instance whose `secret` was empty hashes `token` alone, hence
//! [`hash_token_without_secret`].

use sha2::{Digest, Sha512};

/// `hex(sha512(password || secret))`.
pub fn hash_token(password: &str, secret: &str) -> String {
    let mut hasher = Sha512::new();
    hasher.update(password.as_bytes());
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// `hex(sha512(password))`, the fallback for instances with an empty `secret`.
pub fn hash_token_without_secret(password: &str) -> String {
    hash_token(password, "")
}

/// Lower-cases a string the way `mb_strtolower()` is used by
/// `Session::validateTokenLoginName()`. Rust's Unicode-aware lowering is close
/// enough for user ids and email addresses; the comparison is only used to
/// reject, never to grant access beyond what the stored `uid` grants.
pub fn login_name_matches(stored: &str, presented: &str) -> bool {
    stored.to_lowercase() == presented.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_matches_php() {
        // sha512("pw" + "secret")
        assert_eq!(
            hash_token("pw", "secret"),
            "74d017fab24191fe922a24f5ef1abd0ea50038d8c7123a74317e59be46f7129c\
             42fd9d402a276540e8acf3e91dfbe168ad934b73961db2fcf2615d66bfc40f37"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn token_hash_without_secret_matches_php() {
        // sha512("abc")
        assert_eq!(
            hash_token_without_secret("abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                .replace(char::is_whitespace, "")
        );
        // sha512("") for an empty secret and empty token.
        assert_eq!(
            hash_token_without_secret(""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn login_name_is_case_insensitive() {
        assert!(login_name_matches("Alice@Example.com", "alice@example.COM"));
        assert!(!login_name_matches("alice", "bob"));
    }
}
