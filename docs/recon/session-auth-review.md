# Security review: session-cookie authentication (nextcloud-dav)

**Date:** 2026-09-21. Reviewer: code review agent (adversarial pass).
**Code reviewed at commit:** `0269537` ("feat(auth): derive the session store URL
from the existing redis config"), working tree clean. The running harness
sidecar binary (`target/release/nextcloud-dav`, 2026-09-20 23:06) is newer than
every source file, so live tests below exercise exactly the code under review.

**Scope:** `src/auth/session.rs`, `src/auth/session_redis.rs`,
`authenticate_session` + `classify_session_token_state` in `src/auth/mod.rs`,
the four call sites in `src/routes.rs` (`handle_calendars`, `handle_discovery`,
`handle_files`, `handle`), plus the DB lookups they use
(`authtoken_by_hash`, `native_user_exists`, `user_is_disabled` in `src/db.rs`).

**PHP reference:** `apps/dav/lib/Connector/Sabre/Auth.php`,
`lib/private/User/Session.php` (`getUser`, `validateSession`, `validateToken`,
`checkTokenCredentials`, `isLoggedIn`, `completeLogin`, `logClientIn`),
`lib/private/Authentication/TwoFactorAuth/Manager.php` (`needsSecondFactor`),
`lib/private/AppFramework/Http/Request.php` (`passesCSRFCheck`,
`passesStrictCookieCheck`, `passesLaxCookieCheck`, `cookieCheckRequired`,
`getProtectedCookieName`), `lib/private/Security/Crypto.php`,
`lib/private/Security/CSRF/{CsrfToken,CsrfTokenManager}.php`,
`lib/private/Security/CSRF/TokenStorage/SessionStorage.php`,
`lib/private/Authentication/Token/PublicKeyTokenProvider.php`,
`lib/private/Session/{CryptoWrapper,CryptoSessionData}.php`,
`lib/private/Lockdown/LockdownManager.php`, `lib/private/User/User.php`
(`isEnabled`), `lib/private/Config/UserConfig.php` (`getValueBool`).

## Findings

### F1 — medium — `app_password` type confusion silently swaps the validated token
* **Where:** `src/auth/session.rs:64-86` (`SessionPayload::parse`) and
  `src/auth/mod.rs:418-425` (`authenticate_session` step 6).
* **What:** `has_app_password` is true whenever the key is present and non-null
  (any JSON type), but `app_password` is `Some` only when it is a string. When
  the value is a JSON number/array/bool, the 2FA gate is skipped (step 7 tests
  `has_app_password`) while the token to revalidate silently becomes
  **`session_id`** instead of the `app_password` value
  (`payload.app_password.as_deref()` is `None`, so the `None => session_id`
  arm runs).
* **PHP behaviour:** `validateSession()` (`User/Session.php:203`) reads
  `app_password`; a non-null non-string makes `$token` an int/array →
  `validateToken()` → `getToken()` throws `InvalidTokenException` (short/coerced
  value) → **`logout()`, 401**; a JSON array is a PHP `TypeError` (500). PHP
  never authenticates such a session.
* **Why it diverges:** for a browser form-login session (whose `oc_authtoken`
  row *is* keyed by the session id, type `TEMPORARY_TOKEN`, passwordless), the
  sidecar would revalidate the session-id token, skip 2FA, and serve the
  request — an accept on a path where PHP logs the user out.
* **Triggering input:** a session payload containing
  `"app_password": 12345678901234567890123456789` (number) in an otherwise
  valid form-login session.
* **Reachability today:** PHP writes `app_password` only as a string
  (`Session.php:459,880` — `$password` from the request), so this needs a
  session-content write primitive (direct Redis write / a future PHP bug). It
  is a defence-in-depth violation of the stated rule "any case that cannot be
  evaluated exactly must delegate", not a directly exploitable bypass.
* **Minimal fix:** in `SessionPayload::parse`, treat "present, non-null, but
  not a string" as a parse failure (return `None`), so the request delegates.
* **Status: CONFIRMED EMPIRICALLY** (harness test T1 below: PHP 401, sidecar 207).

### F2 — medium — token scope / lockdown is ignored
* **Where:** `src/auth/mod.rs:151-197` (`classify_session_token_state`) and
  `src/db.rs:1105-1123` (`authtoken_by_hash` — the SELECT never reads
  `oc_authtoken.scope`); `src/auth/session.rs` (`SessionPayload` ignores the
  session's `token_scope` key).
* **What:** PHP's `validateToken()` ends with
  `$this->lockdownManager->setToken($dbToken)` (`User/Session.php:800`), which
  stores the token scope in the session and enables the lockdown;
  `LockdownManager::canAccessFilesystem()` (`Lockdown/LockdownManager.php:63`)
  gates `SetupManager` (`Files/SetupManager.php:335,414,830`) — a token whose
  scope has `filesystem: false` does not get its filesystem mounted, so PHP
  will not serve files-DAV listings for it. The sidecar never looks at scope,
  so such a session (or Basic credential) gets files PROPFIND metadata
  (names, sizes, etags, fileids) served straight from the DB.
* **Triggering input:** Basic-login to DAV with a scoped app password
  (filesystem=false), keep the cookie jar, then issue cookie-only files
  PROPFINDs.
* **Why exploitable/divergent:** it is the one clear class where the sidecar
  serves data the reference refuses; impact is file *metadata* only (content
  GETs are still delegated). Note the same blind spot exists in the Basic fast
  path (`classify_token_state`), so it predates session auth — but session auth
  adds a second route to it.
* **Minimal fix:** include `scope` in `authtoken_by_hash`, and delegate
  (Fallback) whenever the scope is non-empty for the files/calendars trees, or
  reproduce `canAccessFilesystem()`.
* **Status: code-level divergence confirmed; not reproduced live** (no scoped
  token in the disposable harness; see "could not verify").

### F3 — low — requesttoken source precedence differs from PHP (header wins over query param)
* **Where:** `src/auth/mod.rs:453-457` → `RequestFacts.requesttoken_header.or(requesttoken_param)`;
  `src/auth/session.rs:280-303`.
* **PHP:** `Request::passesCSRFCheck()` (`Request.php:440-447`) prefers the
  **GET param**, then POST param, then the `requesttoken` header. With a valid
  header token **and** a bogus `?requesttoken=…` param, PHP's check fails
  (param wins) → 401; the sidecar accepts via the header.
* **Triggering input:** non-exempt method (PROPFIND), branch-1 session, strict
  + lax cookies, `requesttoken: <valid>` header plus
  `?requesttoken=attacker-junk`.
* **Security impact:** marginal — the requester must already possess the valid
  obfuscated token (proof of CSRF knowledge), so this is a parity bug, not a
  bypass of the CSRF *goal*; but it is a literal "accepts what PHP rejects".
* **Minimal fix:** evaluate the query param first (then header), like PHP —
  or simply delegate when both are present and only one is valid.
* **Status: CONFIRMED EMPIRICALLY** (harness test T2 below: PHP 401, sidecar 207).

### F4 — low — `user_is_disabled` is a blacklist; PHP's `isEnabled` is a whitelist
* **Where:** `src/db.rs:1138-1151` vs `User/User.php:472-486` +
  `Config/UserConfig.php:723-727`.
* **What:** the sidecar treats only `oc_preferences core/enabled = 'false'` as
  disabled. PHP `getValueBool` returns enabled **only** for values in
  `{'1','true','yes','on'}` (case-insensitive); anything else present
  (`'0'`, `'off'`, `''`, garbage) is *disabled*.
* **Triggering input:** a row `(alice, core, enabled, '0')` (not produced by
  `setEnabled`, which writes 'true'/'false' — requires manual SQL / a foreign
  app writing the preference). Then a session for that account is served by
  the sidecar and rejected by PHP.
* **Minimal fix:** invert the check — disabled unless the stored value is one
  of `1/true/yes/on` (absent row = enabled).
* **Status: CONFIRMED EMPIRICALLY** (harness test T3 below: PHP 401, sidecar 207).

### F5 — low — same-site cookie name accepted in both plain and `__Host-` form
* **Where:** `src/auth/session.rs:239-252` (`same_site_cookie`) vs
  `Request.php:474-487` (`getProtectedCookieName` picks exactly one name from
  the session cookie params).
* **What:** when the instance is HTTPS (so PHP looks only for
  `__Host-nc_sameSiteCookiestrict`), a plain `nc_sameSiteCookiestrict=true`
  cookie — which a sibling subdomain can set — satisfies the sidecar but not
  PHP.
* **Mitigations:** the requesttoken must still be valid (attacker-unknowable),
  and the session cookie itself is `SameSite=Lax` + httponly, which blocks
  cross-site non-exempt-method requests anyway. Marginal exploitability.
* **Minimal fix:** pick the name the same way PHP does (from
  `session_get_cookie_params`-equivalent config: `__Host-` iff secure && path=/),
  and accept only that one.

### F6 — low — a huge RESP bulk length can abort the process
* **Where:** `src/auth/session_redis.rs:241-255` (`read_reply`: `vec![0u8; len+2]`).
* **What:** a `$<huge>` reply header from the session Redis makes the sidecar
  attempt a giant allocation → `handle_alloc_error` → process abort. Only a
  hostile/compromised Redis (trusted infrastructure) can trigger it; DoS, not
  bypass.
* **Minimal fix:** reject lengths above a sane cap (sessions are a few KB;
  16 MB is generous) → `Err` → delegate.

### F7 — low — an unparseable `?database=` silently selects DB 0
* **Where:** `src/auth/session_redis.rs:108-113`: `database = value.parse::<u32>().ok()`
  — a failed parse leaves `database = None`, i.e. **no `SELECT`**, reading DB 0,
  whereas PHP/php-redis errors out on a broken save_path. Operator-controlled
  input only (config, not attacker), but it violates the file's own
  "fail closed" comment.
* **Minimal fix:** `return None` when a `database`/`db` value is present but
  unparseable (or overflows u32).

### F8 — nit — duplicate cookie names: sidecar takes the first, PHP takes the last
* **Where:** `src/auth/session.rs:224-237` (`cookie_value` returns the first
  match) vs PHP's `$_COOKIE` (later duplicate overrides).
* Only observable with duplicate cookie names (subdomain cookie shadowing);
  it selects which *valid* session is used, never grants anything beyond the
  session's own user. No fix needed for security; note for parity.

### F9 — nit — PHP-lenient CSRF token decoding is (safely) not reproduced
* **Where:** `src/auth/session.rs:117-146` (`decrypt_requesttoken`) vs
  `CsrfToken::getDecryptedValue()` (`CsrfToken.php:37-46`).
* PHP's `base64_decode` is non-strict (invalid chars silently dropped), the
  string XOR tolerates unequal lengths, and there is no UTF-8 validation. The
  Rust version is strictly narrower (STANDARD base64, equal lengths, UTF-8).
  Every input PHP accepts and Rust rejects delegates — fail-closed — so some
  exotic-but-valid web-UI tokens would 501. `explode(':')`-vs-`split(':')`
  semantics were compared case by case and match (`a:b:` → 3 parts → invalid
  in both; `:b` → decodes to a value that cannot equal a non-empty session
  token in both).

### F10 — info — PHP-only leniencies the sidecar does not reproduce (all fail-closed: delegate)
For completeness, these make PHP *more* permissive than the sidecar; each one
means a 501 + nginx replay to PHP, never an accept:
* `OCS-APIRequest` header short-circuits `passesCSRFCheck()` to true
  (`Request.php:434-436`) once strict cookies pass — no requesttoken needed.
* Official desktop/android/iOS user agents skip `requiresCSRFCheck()`
  (`Auth.php:127-134`).
* `session['app_api'] === true` also skips 2FA (`Manager.php:272`) — the
  sidecar only knows `app_password`.
* PHP's v2 ciphertext format and `ctype_xdigit` (uppercase) hex in
  `Crypto::decryptWithoutSecret`; the sidecar only matches lowercase v3
  envelopes.
* Non-native (LDAP/SSO) users: PHP authenticates their sessions; the sidecar
  requires an `oc_users` row (`db.rs:1126-1136`) and delegates otherwise.
* PHP accepts branch 1/branch 2 even with an `Authorization` header present
  (branch 1 has no `empty(Authorization)` clause; branch 2 requires it);
  the sidecar delegates whenever the header exists (`routes.rs:473-475`).
* A PHP form-encoded body `requesttoken` for non-GET/POST methods
  (`Request.php:404-411`) — the sidecar only reads header/query and delegates.

### F11 — info — operational notes (not auth-bypass)
* **Per-request Redis connection** (`session_redis.rs:158-…`): every
  session-auth request opens a fresh TCP connection + `AUTH` + `SELECT` +
  `GET`. An unauthenticated client can force this by sending a cookie named
  like the instance id, so it is also a cheap amplification surface. Consider
  a small connection pool and a cheap pre-filter.
* **Session TTL:** the sidecar only reads `PHPREDIS_SESSION:<id>`; it never
  refreshes the key's TTL or `last_activity`/`LAST_ACTIVITY`. If phpredis does
  not refresh TTL on read, a workload served exclusively by the sidecar could
  let live sessions expire at `session.gc_maxlifetime`. Availability only
  (expiry fails closed). Not verified live.

## Verified correct

Method note: the crypto and decision gates were compared line-by-line against
the PHP sources above; `cargo test --lib` (203 tests) passes including the
PHP-produced blob vector; the pre-existing harness matrix
(`tests/local/session_auth.sh`, evidence in `state/evidence/session-auth.txt`)
passed 54/54 — real Basic session, real form-login session, tampered,
wrong-passphrase, truncated, missing-key, wrong-user, wrong-path, no-2FA,
revoked-token, wrong/missing requesttoken, missing strict cookie, Redis down:
all 501 with no sidecar header; PHP parity of bodies for the accept cases.
Adversarial follow-ups T1-T4 were run for this review (below).

1. **Crypto exactness** (`session.rs:157-220` vs `Crypto.php`):
   * HKDF-SHA512, no salt/info, 64-byte OKM — matches `hash_hkdf('sha512', $password)`
     (empty salt ≡ zero-block salt).
   * HMAC-SHA512 with key = ASCII-hex of `sha512(macKey || 'a')` (128 chars =
     SHA-512 block size, used as-is), message = `ct_hex || iv_hex` — matches
     `Crypto::calculateHMAC`. Compare is `verify_slice` (constant-time).
   * AES-128-CBC, key = PBKDF2-SHA1(encKey32, "phpseclib", 1000, 16) — matches
     phpseclib `setPassword(..., 'pbkdf2', 'sha1', 'phpseclib')` on `AES('cbc')`.
   * Verified against a real `OC\Security\Crypto::encrypt()` blob produced in
     the harness (`decrypts_a_php_produced_blob`, and the 54-check e2e).
2. **HMAC before plaintext on every path**: `decrypt_fields` verifies the MAC
   before AES; `extract_and_decrypt` only returns MAC-verified plaintext;
   `SessionPayload::parse` runs only on decrypted bytes. Empty, short,
   odd-length, non-multiple-of-16, oversized inputs all fail closed
   (unit tests `truncated_blob_is_rejected`, `non_hex_garbage_is_rejected`,
   `tampered_ciphertext_is_rejected`, `wrong_passphrase_is_rejected`).
3. **No reachable panic**: `expect` in `hkdf_sha512` is unreachable (64 ≤ 255);
   `new_from_slice` lengths are fixed; all slice indices in
   `extract_and_decrypt` are bounds-checked (`raw.get(...)` guards); RESP
   `line[1..]` slicing only happens after an ASCII first byte was matched.
   `read_padded_vec_mut` returns `Result` (bad PKCS#7 → `None`).
4. **Envelope scan**: zero verified candidates → delegate; **two** verified →
   delegate (ambiguity refuses, stricter than PHP's first-match); candidates
   are HMAC-gated so attacker-chosen bytes cannot verify without the
   passphrase; a blob from another session cannot replay because the HMAC key
   is derived from the (per-browser, random, httponly) passphrase — and the
   scan only sees the one session's own Redis value. Verified live: T4
   (duplicate envelope: PHP 207, sidecar 501) and harness D2-D5.
5. **The ordered decision matches PHP** (`decision_after_token` vs
   `Auth::auth()` + `Request::passesCSRFCheck`):
   * 2FA before the branches, like PHP; `two_factor_auth_passed === uid`
     matches `SESSION_UID_DONE === getUID()`; app-password skip matches
     `exists('app_password')` — and `!v.is_null()` reproduces PHP `isset`
     semantics exactly (a JSON null is "absent" for both).
   * Branch 2 (`DAV_AUTHENTICATED === uid`) accepted without CSRF — matches
     PHP `requiresCSRFCheck()` returning false when `isDavAuthenticated`.
     The `empty(Authorization)` clause is enforced upstream
     (`routes.rs:473-475`).
   * Branch 1 (`DAV_AUTHENTICATED` null/absent): GET/HEAD/OPTIONS exempt;
     otherwise strict+lax same-site cookies **and** requesttoken equality —
     matches `passesCSRFCheck` (`passesStrictCookieCheck` requires strict==='true'
     && lax==='true', then the token). Constant-time compare matches
     `hash_equals` (length mismatch → false in both).
   * `AUTHENTICATED_TO_DAV_BACKEND` naming a different uid → never accept
     (harness D6/D7 + per-branch `target_user != user.uid` 404/501 checks).
   * The requesttoken de-obfuscation (`base64(v XOR s) : base64(s)`, exactly
     two parts) matches `CsrfToken::getDecryptedValue` on the strict paths.
6. **`classify_session_token_state` vs `validateSession`/`checkTokenCredentials`/`checkToken`**
   (`mod.rs:151-197`):
   * token choice: `app_password` if present (string), else session id — same
     as `validateSession` (the empty-string case delegates, matching PHP's
     `getToken('')` → InvalidTokenException → logout).
   * WIPE (type 2) rejected; expiry delegates (PHP throws → logout — same
     client-visible outcome, audit trail kept in PHP); `password_invalid`
     rejected — all match `PublicKeyTokenProvider::checkToken()`.
   * `expires` NULL **and** 0 are "no expiry" (`db.rs:2572-2574` filters `!= 0`),
     matching PHP's `(int)getExpires() !== 0`.
   * `last_check` NULL → 0 (`db.rs:2577-2579`), matching PHP's `getLastCheck() ?: 0`;
     window is strictly `> now - 300` on both sides; the fresh-check window is
     evaluated *before* the passwordless rule, exactly like PHP.
   * passwordless (`password IS NULL`) accepted even with a stale `last_check`
     — matches `PasswordlessTokenException` → `return true`.
   * password-bearing + stale → delegate (PHP runs `checkPassword()`, which the
     sidecar cannot reproduce). Disabled user → reject (both).
   * The sidecar additionally requires `row.uid == session user_id`, which PHP
     does not (it passes `$user = null` to `validateToken`); stricter, safe.
   * Login-name checking is correctly *absent*: PHP's session revalidation
     does not run `validateTokenLoginName` (only the Basic path does, and the
     fast path reproduces it via `login_name_matches`).
   * Stale/revoked tokens: harness D9 (password_invalid → 501) and the unit
     tests (`session_delegates_a_stale_password_bearing_token`,
     `session_rejects_a_different_uid_and_wipe_token`,
     `session_accepts_a_passwordless_temporary_token`) pin the semantics.
7. **Fail-closed plumbing**: every DB/Redis error in `authenticate_session`
   uses `.ok()?` / `Err → None` (Redis unreachable → delegate: harness D13);
   missing config (`session_redis` None, empty instance id) → delegate;
   unparsable JSON / missing-or-empty `user_id` → delegate. No path returns
   `Some` without passing every gate. The four call sites in `routes.rs` all
   go through the single `authenticate_dav`, and every one applies the
   uid-vs-path rule (files additionally 404s a foreign home, matching PHP).
8. **Cookie handling**: both cookies are read with PHP's `$_COOKIE` decoding
   semantics (`urldecode` with `+`→space — `util.rs:30-32`), the session
   cookie name is the instanceid verbatim (verified: `OC_Util::getInstanceId`
   already carries the `oc` prefix and is used directly as `session_name`),
   the passphrase cookie is `oc_sessionPassphrase` (`CryptoWrapper::COOKIE_NAME`).
9. **Secrets**: the session id, passphrase, payload, Redis password and app
   passwords are never logged (`RedisError` is opaque, `Debug for RedisConfig`
   and `Debug for Authenticator` redact; logs only carry uid / "delegating").
   The `oc_sessionPassphrase` cookie itself is `httponly` in PHP
   (`CryptoWrapper.php:47-59`), so it is not script-readable.
10. **`app_password` semantics** (correct half): present-and-string →
    app-password revalidation (the harness's Basic session, PERMANENT token);
    absent/null → session-id revalidation (form-login session, TEMPORARY
    passwordless token — served natively by the sidecar, F1.3 in the harness
    evidence).

## Could not verify

* **F2 (token scope) end-to-end:** no scoped app password could be created in
  the disposable harness without writing a token row by hand; the divergence is
  established from code reading on both sides (`LockdownManager::canAccessFilesystem`
  gating `SetupManager`, which the sidecar never consults). Whether PHP returns
  an error or an empty listing for a filesystem-scoped files PROPFIND was not
  observed.
* **phpredis TTL-on-read behaviour** (F11): whether serving a session only
  through the sidecar stops the Redis TTL from being refreshed. Would need a
  time-accelerated harness.
* **PHP's unequal-length string XOR** in `CsrfToken::getDecryptedValue`
  (padding vs truncation semantics): only makes PHP *more* lenient than the
  Rust implementation, which rejects unequal lengths, so it cannot create an
  accept-where-PHP-rejects; noted, not tested.
* **`IProvideEnabledStateBackend`-provided enabled state** for non-Database
  backends (User.php:480-484): out of scope because those users are
  non-native and always delegate anyway.
* **Whether any PHP code path can write a non-string `app_password` or
  non-string `AUTHENTICATED_TO_DAV_BACKEND` into a session** (the reachability
  of F1 and the `dav_authenticated`-type note): none was found — all writers
  are `set(..., <string>)` — but a complete audit of every `ISession::set`
  call site in the app ecosystem was not performed.
* **A `dav_authenticated` non-string divergence** (same class as F1, lower
  severity): PHP treats a non-null non-string `AUTHENTICATED_TO_DAV_BACKEND`
  as "not branch 1, not branch 2" (→ Basic → 401), while the sidecar's
  `Option<String>` maps it to branch 1 (absent). Same reachability caveat as
  F1. Not tested live.

## Adversarial harness runs (this review)

Disposable `ncdav-e2e-*` stack (Nextcloud 33.0.5 + redis + postgres), sidecar
on 127.0.0.1:17870, NC on 127.0.0.1:18081. No secrets printed; full transcripts
in `tests/local/state/evidence/session-auth-review.txt`.

* **T1 — `app_password` type confusion (F1):** form-login session, payload
  rewritten in place with `"app_password": 1234567890123456789012345678`
  (JSON number), same passphrase, requesttoken supplied.
  PHP: **401** (session logged out). Sidecar: **207, x-nextcloud-dav: sidecar**.
  → CONFIRMED accept-where-PHP-rejects.
* **T2 — requesttoken precedence (F3):** form-login session, PROPFIND with a
  valid `requesttoken` header and `?requesttoken=not-the-token`.
  PHP: **401** (the GET param wins and is invalid). Sidecar: **207 sidecar**.
  → CONFIRMED.
* **T3 — enabled='0' (F4):** `oc_preferences(alice, core, enabled, '0')`,
  valid session. PHP: **401**. Sidecar: **207 sidecar**. Restored afterwards.
  → CONFIRMED.
* **T4 — duplicate envelope:** session value containing the same verified
  envelope twice. PHP: **207** (first match). Sidecar: **501** (no sidecar
  header) — the ambiguity rule refuses. Fail-closed, as designed.

(Transcripts appended below when run; results summarised here.)

## Summary

No outright authentication bypass was found: the crypto is a faithful,
constant-time, verify-before-decrypt reproduction of `OC\Security\Crypto`; the
envelope scan is HMAC-gated and ambiguity-refusing; the ordered decision
(CSRF → 2FA → the two branches), the token revalidation and every error path
match PHP or delegate. The real divergences are narrower: a type-confusion
hole where a non-string `app_password` silently swaps the revalidated token
and skips 2FA (F1, confirmed live), the complete absence of token-scope /
lockdown enforcement (F2, code-confirmed), two confirmed low-severity decision
mismatches (F3 requesttoken precedence, F4 enabled-state blacklist), plus
several fail-closed parity gaps and hardening nits. F1 and F2 deserve fixes;
the rest are defence-in-depth and parity work.
