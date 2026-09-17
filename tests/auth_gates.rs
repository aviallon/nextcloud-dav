// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! App-password gate parity (`PublicKeyTokenMapper` / `checkToken()` /
//! `Session::validateTokenLoginName()`) and brute-force throttling arithmetic.
//!
//! These are pure functions so they run without a database.

use nextcloud_dav::auth::throttle::{calculate_delay, normalized_subnet, MAX_DELAY_MS};
use nextcloud_dav::auth::token::{hash_token, hash_token_without_secret, login_name_matches};
use nextcloud_dav::auth::{classify_token_state, now_unix, TokenDecision};
use nextcloud_dav::model::AuthToken;
use std::net::IpAddr;
use std::str::FromStr;

const NOW: i64 = 1_700_000_000;

fn token(token_type: i64, uid: &str, login_name: &str, last_check: i64) -> AuthToken {
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
fn accepts_valid_permanent_token() {
    let row = token(1, "alice", "alice@example.com", NOW);
    assert_eq!(
        classify_token_state(&row, "alice@example.com", NOW, true, false),
        TokenDecision::Accept("alice".to_string())
    );
}

#[test]
fn accepts_onetime_token() {
    // IToken::ONETIME == 3.
    let row = token(3, "alice", "alice", NOW);
    assert!(matches!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Accept(_)
    ));
}

#[test]
fn wipe_token_is_rejected() {
    // IToken::WIPE_TOKEN == 2 is a revocation marker and must never pass.
    let row = token(2, "alice", "alice", NOW);
    assert_eq!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Reject
    );
}

#[test]
fn unknown_token_type_delegates_to_php() {
    // Temporary (0) and anything unexpected: PHP owns the exact semantics.
    let row = token(0, "alice", "alice", NOW);
    assert_eq!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Fallback
    );
    let row = token(7, "alice", "alice", NOW);
    assert_eq!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Fallback
    );
}

#[test]
fn expired_token_delegates_not_rejects() {
    let mut row = token(1, "alice", "alice", NOW);
    row.expires = Some(NOW - 1);
    assert_eq!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Fallback
    );
    row.expires = Some(NOW + 1);
    assert!(matches!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Accept(_)
    ));
}

#[test]
fn password_invalid_is_rejected() {
    let mut row = token(1, "alice", "alice", NOW);
    row.password_invalid = true;
    assert_eq!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Reject
    );
}

#[test]
fn login_name_must_match_case_insensitively() {
    let row = token(1, "alice", "alice@example.com", NOW);
    assert_eq!(
        classify_token_state(&row, "bob@example.com", NOW, true, false),
        TokenDecision::Reject
    );
    assert!(matches!(
        classify_token_state(&row, "ALICE@EXAMPLE.COM", NOW, true, false),
        TokenDecision::Accept(_)
    ));
    assert!(login_name_matches("Alice@Example.com", "alice@example.COM"));
    assert!(!login_name_matches("alice", "bob"));
}

#[test]
fn empty_stored_uid_is_rejected() {
    let row = token(1, "", "alice", NOW);
    assert_eq!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Reject
    );
}

#[test]
fn non_native_ldap_user_delegates_to_php() {
    let row = token(1, "ldapuser", "ldapuser", NOW);
    assert_eq!(
        classify_token_state(&row, "ldapuser", NOW, false, false),
        TokenDecision::Fallback
    );
}

#[test]
fn disabled_user_is_rejected() {
    let row = token(1, "alice", "alice", NOW);
    assert_eq!(
        classify_token_state(&row, "alice", NOW, true, true),
        TokenDecision::Reject
    );
}

#[test]
fn stale_last_check_delegates_at_300s_boundary() {
    let row = token(1, "alice", "alice", NOW - 300);
    // Exactly at the boundary is still fresh.
    assert!(matches!(
        classify_token_state(&row, "alice", NOW, true, false),
        TokenDecision::Accept(_)
    ));
    let stale = token(1, "alice", "alice", NOW - 301);
    assert_eq!(
        classify_token_state(&stale, "alice", NOW, true, false),
        TokenDecision::Fallback
    );
}

#[test]
fn hash_matches_php_sha512_token_secret() {
    // sha512("pw" + "secret")
    assert_eq!(
        hash_token("pw", "secret"),
        "74d017fab24191fe922a24f5ef1abd0ea50038d8c7123a74317e59be46f7129c\
         42fd9d402a276540e8acf3e91dfbe168ad934b73961db2fcf2615d66bfc40f37"
            .replace(char::is_whitespace, "")
    );
    // sha512("abc") for the legacy empty-secret path.
    assert_eq!(
        hash_token_without_secret("abc"),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
         2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
            .replace(char::is_whitespace, "")
    );
}

#[test]
fn brute_force_backoff_matches_nextcloud() {
    // 0.1 * 2^n seconds, capped at 25 s; attempts > max => 25 s.
    assert_eq!(calculate_delay(0, 10), 0);
    assert_eq!(calculate_delay(1, 10), 200);
    assert_eq!(calculate_delay(4, 10), 1600);
    assert_eq!(calculate_delay(8, 10), 25_000);
    assert_eq!(calculate_delay(11, 10), 25_000);
    assert_eq!(calculate_delay(1000, 1000), MAX_DELAY_MS);
}

#[test]
fn subnet_normalisation() {
    let v4 = IpAddr::from_str("203.0.113.7").unwrap();
    assert_eq!(normalized_subnet(v4, 56), "203.0.113.7/32");
    let mapped = IpAddr::from_str("::ffff:203.0.113.7").unwrap();
    assert_eq!(normalized_subnet(mapped, 56), "203.0.113.7/32");
    let v6 = IpAddr::from_str("2001:db8:abcd:1234:5678:9abc:def0:1234").unwrap();
    assert_eq!(normalized_subnet(v6, 56), "2001:db8:abcd:1200::/56");
}

#[test]
fn now_unix_is_sane() {
    assert!(now_unix() > 1_600_000_000);
}
