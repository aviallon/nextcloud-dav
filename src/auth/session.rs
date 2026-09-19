// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Session-cookie authentication: the `OC\Security\Crypto` envelope and the
//! ordered decision of `docs/recon/session-auth.md`.
//!
//! Everything here is **fail closed**: a missing cookie, an unparsable
//! payload, a failed HMAC, a mismatched uid or a check that cannot be
//! evaluated all return `None`, which the caller turns into a `501`
//! (delegate to PHP). The HMAC is verified before the AES plaintext is even
//! looked at, so a forged blob cannot be trusted.

use aes::Aes128;
use axum::http::{header, HeaderMap};
use base64::Engine;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use sha1::Sha1;
use sha2::{Digest, Sha512};

type Aes128CbcDec = Decryptor<Aes128>;
type HmacSha512 = Hmac<Sha512>;

/// `OCA\DAV\Connector\Sabre\Auth::DAV_AUTHENTICATED`.
pub const DAV_AUTHENTICATED: &str = "AUTHENTICATED_TO_DAV_BACKEND";
/// `OC\Authentication\TwoFactorAuth\Manager::SESSION_UID_DONE`.
pub const TWO_FACTOR_PASSED: &str = "two_factor_auth_passed";

/// The session cookie name. Nextcloud uses `OC_Util::getInstanceId()` directly
/// as `session_name()` (`lib/base.php`), and that id already carries the `oc`
/// prefix, so it is **not** prefixed again here.
pub fn session_cookie_name(instance_id: &str) -> String {
    instance_id.to_string()
}

/// The session payload subset the decision needs. Values are `Option` because
/// a key that is absent and one that is `null` behave the same in the checks
/// below (except `app_password`, whose *presence* skips 2FA).
#[derive(Debug, Clone, Default)]
pub struct SessionPayload {
    pub user_id: String,
    pub loginname: Option<String>,
    /// Present (even empty) means the session authenticated with an app
    /// password, which skips the 2FA gate.
    pub app_password: Option<String>,
    pub has_app_password: bool,
    pub dav_authenticated: Option<String>,
    pub two_factor_auth_passed: Option<String>,
    pub requesttoken: Option<String>,
}

impl SessionPayload {
    /// Parses the decrypted JSON. `user_id` must be a non-empty string; every
    /// other key is optional. A payload without `user_id` is rejected so the
    /// caller delegates.
    pub fn parse(json: &[u8]) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_slice(json).ok()?;
        let user_id = value.get("user_id")?.as_str()?.to_string();
        if user_id.is_empty() {
            return None;
        }
        let str_at = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let has_app_password = value
            .get("app_password")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let app_password = if has_app_password {
            value
                .get("app_password")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        } else {
            None
        };
        Some(Self {
            user_id,
            loginname: str_at("loginname"),
            app_password,
            has_app_password,
            dav_authenticated: str_at(DAV_AUTHENTICATED),
            two_factor_auth_passed: str_at(TWO_FACTOR_PASSED),
            requesttoken: str_at("requesttoken"),
        })
    }
}

/// The request properties the ordered decision needs.
#[derive(Debug, Clone, Copy)]
pub struct RequestFacts<'a> {
    pub method: &'a str,
    pub requesttoken_header: Option<&'a str>,
    pub requesttoken_param: Option<&'a str>,
    pub strict_cookie: bool,
    pub lax_cookie: bool,
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Reproduces `OC\Security\CSRF\CsrfToken::getDecryptedValue()`.
///
/// The web UI sends the token as `base64(value XOR secret) : base64(secret)`
/// (a BREACH mitigation), while the session stores the plaintext `value`.
/// PHP splits on `:` (exactly two parts required), base64-decodes both and
/// XORs them back. Returns `None` on any shape/decode failure, which delegates.
pub fn decrypt_requesttoken(presented: &str) -> Option<String> {
    let mut parts = presented.split(':');
    let obfuscated = parts.next()?;
    let secret = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let obfuscated = base64::engine::general_purpose::STANDARD
        .decode(obfuscated)
        .ok()?;
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret)
        .ok()?;
    if obfuscated.len() != secret.len() {
        return None;
    }
    let value: Vec<u8> = obfuscated
        .iter()
        .zip(secret.iter())
        .map(|(obfuscated, secret)| obfuscated ^ secret)
        .collect();
    String::from_utf8(value).ok()
}

fn hkdf_sha512(password: &[u8]) -> [u8; 64] {
    let hkdf = Hkdf::<Sha512>::new(None, password);
    let mut okm = [0u8; 64];
    // 64 bytes is a valid HKDF-SHA512 output length.
    hkdf.expand(&[], &mut okm)
        .expect("64 is a valid HKDF length");
    okm
}

/// Verifies the HMAC and decrypts one `ct|iv|mac|3` envelope. Returns `None`
/// on any structural, MAC or padding failure.
fn decrypt_fields(
    ct_hex: &[u8],
    iv_hex: &[u8],
    mac_hex: &[u8],
    passphrase: &str,
) -> Option<Vec<u8>> {
    let ciphertext = hex::decode(ct_hex).ok()?;
    let iv = hex::decode(iv_hex).ok()?;
    let mac = hex::decode(mac_hex).ok()?;
    if iv.len() != 16 || mac.len() != 64 || ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return None;
    }

    let key_material = hkdf_sha512(passphrase.as_bytes());
    let (enc_key, mac_key) = key_material.split_at(32);

    // `Crypto::calculateHMAC`: the HMAC key is the *ASCII hex* of
    // `sha512(macKey || "a")`. phpseclib treats a key of exactly the SHA-512
    // block size (128 bytes) as-is, which `Hmac` does too.
    let hex_key = hex::encode(Sha512::digest([mac_key, b"a"].concat()));
    let mut hmac = HmacSha512::new_from_slice(hex_key.as_bytes()).ok()?;
    hmac.update(ct_hex);
    hmac.update(iv_hex);
    hmac.verify_slice(&mac).ok()?;

    // phpseclib's `AES('cbc')` defaults to a 128-bit key and derives it with
    // PBKDF2-SHA1(password, "phpseclib", 1000).
    let mut aes_key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(enc_key, b"phpseclib", 1000, &mut aes_key);
    let decryptor = Aes128CbcDec::new_from_slices(&aes_key, &iv).ok()?;
    decryptor.decrypt_padded_vec_mut::<Pkcs7>(&ciphertext).ok()
}

/// Scans an igbinary-framed session value for the authenticated envelope,
/// verifies it and returns the decrypted JSON.
///
/// igbinary is deliberately not parsed: the envelope's shape is unambiguous
/// once authenticated. Candidates are tried at every hex position (a binary
/// length byte immediately before the hex can itself be a hex character), but
/// only a candidate whose HMAC verifies is accepted, and more than one
/// verified candidate is treated as ambiguous and rejected.
pub fn extract_and_decrypt(raw: &[u8], passphrase: &str) -> Option<Vec<u8>> {
    let mut found: Option<Vec<u8>> = None;
    for start in 0..raw.len() {
        if !is_lower_hex(raw[start]) {
            continue;
        }
        let mut end = start;
        while end < raw.len() && is_lower_hex(raw[end]) {
            end += 1;
        }
        let ct_len = end - start;
        if ct_len < 64 || ct_len % 2 != 0 {
            continue;
        }
        if raw.get(end) != Some(&b'|') {
            continue;
        }
        let iv_start = end + 1;
        let iv_end = iv_start + 32;
        if iv_end > raw.len() || !raw[iv_start..iv_end].iter().all(|b| is_lower_hex(*b)) {
            continue;
        }
        if raw.get(iv_end) != Some(&b'|') {
            continue;
        }
        let mac_start = iv_end + 1;
        let mac_end = mac_start + 128;
        if mac_end > raw.len() || !raw[mac_start..mac_end].iter().all(|b| is_lower_hex(*b)) {
            continue;
        }
        if raw.get(mac_end) != Some(&b'|') || raw.get(mac_end + 1) != Some(&b'3') {
            continue;
        }
        if let Some(plaintext) = decrypt_fields(
            &raw[start..end],
            &raw[iv_start..iv_end],
            &raw[mac_start..mac_end],
            passphrase,
        ) {
            if found.is_some() {
                // Two envelopes verified: ambiguous, never accept.
                return None;
            }
            found = Some(plaintext);
        }
    }
    found
}

/// The ordered decision of design doc §6, steps 7-9. Step 6 (token
/// revalidation) has already run and returned `Some`; step 5 (user/path) is
/// handled by the caller.
pub fn decision_after_token(payload: &SessionPayload, facts: &RequestFacts) -> Option<String> {
    let uid = payload.user_id.as_str();

    // Step 7: `TwoFactorManager::needsSecondFactor()`. An app-password session
    // skips 2FA; a browser session must have proven it.
    if !payload.has_app_password && payload.two_factor_auth_passed.as_deref() != Some(uid) {
        return None;
    }

    match payload.dav_authenticated.as_deref() {
        // Step 8, branch 2 (`isDavAuthenticated`): accepted without CSRF.
        Some(dav) if dav == uid => Some(uid.to_string()),
        // Step 9, branch 1 (`DAV_AUTHENTICATED` absent): PHP runs
        // `passesCSRFCheck()`. GET/HEAD/OPTIONS are exempt.
        None => {
            if matches!(facts.method, "GET" | "HEAD" | "OPTIONS") {
                return Some(uid.to_string());
            }
            if !facts.strict_cookie || !facts.lax_cookie {
                return None;
            }
            let session_token = payload.requesttoken.as_deref()?;
            let presented = facts.requesttoken_header.or(facts.requesttoken_param)?;
            let presented = decrypt_requesttoken(presented)?;
            if constant_time_eq(session_token, &presented) {
                Some(uid.to_string())
            } else {
                None
            }
        }
        // The DAV flag names a different user: never accept.
        Some(_) => None,
    }
}

/// Reads a cookie value and URL-decodes it the way PHP's `$_COOKIE` does (the
/// stored `oc_sessionPassphrase` is percent-encoded, and the raw value fails
/// the HMAC). Returns the first match.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            let pair = pair.trim();
            let Some((key, raw)) = pair.split_once('=') else {
                continue;
            };
            if key == name {
                return Some(crate::util::urldecode(raw));
            }
        }
    }
    None
}

/// True when one of the same-site marker cookies is `true`. Both the plain and
/// the `__Host-` prefixed names are accepted: PHP picks based on the session
/// cookie's `secure`/`path`, which the sidecar cannot see.
pub fn same_site_cookie(headers: &HeaderMap, name: &str) -> bool {
    let plain = name;
    let host = format!("__Host-{name}");
    for candidate in [plain, host.as_str()] {
        if cookie_value(headers, candidate).as_deref() == Some("true") {
            return true;
        }
    }
    false
}

/// Reads a query-string parameter and URL-decodes it. The request body is a
/// DAV document, not a form, so only the query string and the header can carry
/// a `requesttoken`; a POST form token is deliberately not consulted (it would
/// require buffering the body, and delegating is the fail-closed answer).
pub fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (pair_key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if pair_key == key {
            return Some(crate::util::urldecode(value));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by the real `OC\Security\Crypto::encrypt()` in the harness with
    /// the passphrase `throwaway-passphrase` and the plaintext
    /// `{"user_id":"alice","n":1}`.
    const PHP_BLOB: &str = "7fc3d4c80eca9a4d3d7a1e21afbe47a08ea39fce892bd187890914b92448a5fe|\
75741d0fb3dff40ba4faaa01515dbafb|\
97d2d34358425c005d4651e9319275125ddceef850742de85f3eee9eb33fac1356bedc48d12d7135c23277e00f093d20d2abf47763d525119c38847c8c57b9ac|3";
    const PHP_PASSPHRASE: &str = "throwaway-passphrase";
    const PHP_PLAINTEXT: &[u8] = b"{\"user_id\":\"alice\",\"n\":1}";

    #[test]
    fn decrypts_a_php_produced_blob() {
        let plain = extract_and_decrypt(PHP_BLOB.as_bytes(), PHP_PASSPHRASE).unwrap();
        assert_eq!(plain, PHP_PLAINTEXT);
    }

    #[test]
    fn decrypts_a_php_blob_inside_igbinary_framing() {
        // `\x00\x00\x00\x02\x14\x01\x11\x16` is igbinary's map header + the
        // "encrypted_session_data" key, then a length byte, then the value.
        let mut framed =
            b"\x00\x00\x00\x02\x14\x01\x11\x16encrypted_session_data\x12\x05\xa4".to_vec();
        framed.extend_from_slice(PHP_BLOB.as_bytes());
        let plain = extract_and_decrypt(&framed, PHP_PASSPHRASE).unwrap();
        assert_eq!(plain, PHP_PLAINTEXT);
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        assert!(extract_and_decrypt(PHP_BLOB.as_bytes(), "not-the-passphrase").is_none());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut tampered = PHP_BLOB.to_string();
        // Flip the first ciphertext hex digit.
        let first = if tampered.starts_with('7') { '8' } else { '7' };
        tampered.replace_range(0..1, &first.to_string());
        assert!(extract_and_decrypt(tampered.as_bytes(), PHP_PASSPHRASE).is_none());
    }

    #[test]
    fn truncated_blob_is_rejected() {
        let truncated = &PHP_BLOB[..PHP_BLOB.len() / 2];
        assert!(extract_and_decrypt(truncated.as_bytes(), PHP_PASSPHRASE).is_none());
    }

    #[test]
    fn non_hex_garbage_is_rejected() {
        assert!(extract_and_decrypt(b"not a session at all", PHP_PASSPHRASE).is_none());
    }

    #[test]
    fn parse_requires_a_non_empty_user_id() {
        assert!(SessionPayload::parse(b"{\"user_id\":\"alice\"}").is_some());
        assert!(SessionPayload::parse(b"{\"user_id\":\"\"}").is_none());
        assert!(SessionPayload::parse(b"{\"loginname\":\"alice\"}").is_none());
        assert!(SessionPayload::parse(b"not json").is_none());
    }

    fn payload(user_id: &str) -> SessionPayload {
        SessionPayload {
            user_id: user_id.to_string(),
            loginname: Some(user_id.to_string()),
            app_password: Some("app-token".to_string()),
            has_app_password: true,
            dav_authenticated: None,
            two_factor_auth_passed: None,
            requesttoken: Some("tok".to_string()),
        }
    }

    /// `OC\Security\CSRF\CsrfToken::getEncryptedValue()`: the on-the-wire token.
    fn obfuscate(value: &str, secret: &[u8]) -> String {
        use base64::Engine;
        let obfuscated: Vec<u8> = value
            .as_bytes()
            .iter()
            .zip(secret.iter())
            .map(|(value, secret)| value ^ secret)
            .collect();
        format!(
            "{}:{}",
            base64::engine::general_purpose::STANDARD.encode(obfuscated),
            base64::engine::general_purpose::STANDARD.encode(secret)
        )
    }

    fn facts(method: &str) -> RequestFacts<'_> {
        RequestFacts {
            method,
            requesttoken_header: None,
            requesttoken_param: None,
            strict_cookie: false,
            lax_cookie: false,
        }
    }

    #[test]
    fn branch_two_accepts_without_token() {
        let mut p = payload("alice");
        p.dav_authenticated = Some("alice".to_string());
        assert_eq!(
            decision_after_token(&p, &facts("PROPFIND")).as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn branch_two_rejects_a_different_dav_uid() {
        let mut p = payload("alice");
        p.dav_authenticated = Some("bob".to_string());
        assert!(decision_after_token(&p, &facts("PROPFIND")).is_none());
    }

    #[test]
    fn branch_one_get_is_exempt() {
        assert_eq!(
            decision_after_token(&payload("alice"), &facts("GET")).as_deref(),
            Some("alice")
        );
        assert_eq!(
            decision_after_token(&payload("alice"), &facts("OPTIONS")).as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn branch_one_non_exempt_needs_token_and_strict_cookie() {
        let presented = obfuscate("tok", b"abc");
        let mut f = facts("PROPFIND");
        assert!(decision_after_token(&payload("alice"), &f).is_none());

        f.strict_cookie = true;
        f.lax_cookie = true;
        assert!(decision_after_token(&payload("alice"), &f).is_none());

        f.requesttoken_header = Some(&presented);
        assert_eq!(
            decision_after_token(&payload("alice"), &f).as_deref(),
            Some("alice")
        );

        // A token that deobfuscates to a different value is rejected.
        let wrong = obfuscate("zzz", b"abc");
        f.requesttoken_header = Some(&wrong);
        assert!(decision_after_token(&payload("alice"), &f).is_none());

        // A raw (unencrypted) token is never valid: PHP's `getDecryptedValue`
        // returns '' when the value is not `base64:base64`.
        f.requesttoken_header = Some("tok");
        assert!(decision_after_token(&payload("alice"), &f).is_none());

        // Missing strict cookie fails even with the right token.
        f.requesttoken_header = Some(&presented);
        f.strict_cookie = false;
        assert!(decision_after_token(&payload("alice"), &f).is_none());
    }

    #[test]
    fn decrypt_requesttoken_matches_php() {
        let presented = obfuscate("the-value", b"nine-byte");
        assert_eq!(decrypt_requesttoken(&presented).as_deref(), Some("the-value"));
        assert!(decrypt_requesttoken("plain").is_none());
        assert!(decrypt_requesttoken("a:b:c").is_none());
        assert!(decrypt_requesttoken("!!!:???").is_none());
    }

    #[test]
    fn requires_2fa_without_an_app_password() {
        let mut p = payload("alice");
        p.app_password = None;
        p.has_app_password = false;
        p.dav_authenticated = Some("alice".to_string());
        assert!(decision_after_token(&p, &facts("GET")).is_none());
        p.two_factor_auth_passed = Some("alice".to_string());
        assert!(decision_after_token(&p, &facts("GET")).is_some());
    }

    #[test]
    fn query_param_decodes() {
        assert_eq!(
            query_param(Some("a=1&requesttoken=x%2By"), "requesttoken").as_deref(),
            Some("x+y")
        );
        assert_eq!(
            query_param(Some("requesttoken="), "requesttoken").as_deref(),
            Some("")
        );
        assert!(query_param(None, "requesttoken").is_none());
    }
}
