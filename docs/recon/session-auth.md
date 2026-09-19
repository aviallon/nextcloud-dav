# Session-cookie authentication (verified recon + design)

**Goal.** Let the sidecar serve the Nextcloud **web UI**'s DAV requests, which
authenticate with the session cookie and send **no** `Authorization` header.
Today those requests are delegated to PHP (correct, no popup, but at PHP speed,
~0.33 s vs ~0.05 s).

**Risk.** A mistake here is an **authentication bypass** — the worst class of
bug. Everything below is either quoted from the server source or was **verified
empirically against PHP's own `OC\Security\Crypto`** in the local harness. Any
case that cannot be evaluated exactly must **delegate (501)**, never accept.

## 1. Why the sidecar may not simply refuse

`apps/dav/lib/Connector/Sabre/Auth.php::auth()` accepts a session with no
credentials at all:

```php
//Fix for broken webdav clients
($this->userSession->isLoggedIn() && is_null($this->session->get(self::DAV_AUTHENTICATED)))
//Well behaved clients that only send the cookie are allowed
|| ($this->userSession->isLoggedIn() && $this->session->get(self::DAV_AUTHENTICATED) === $uid && empty($request->getHeader('Authorization')))
```

`DAV_AUTHENTICATED` = `'AUTHENTICATED_TO_DAV_BACKEND'`. A **Basic** DAV login sets
it (`OC\User\Session::tryBasicAuthLogin`), which is why the browser's
cookie-only requests work after it has sent credentials once.

## 2. The cookie pair (verified in the harness)

| cookie | meaning |
|---|---|
| `oc<instanceid>` | the **session id**, in the clear |
| `oc_sessionPassphrase` | a random 128-char passphrase; the session *payload* is encrypted with it (`CryptoSessionData`) |

**The passphrase value must be URL-decoded** before use (PHP decodes `$_COOKIE`;
`curl`'s jar holds the encoded form). Verified: the raw value fails the HMAC,
the decoded one verifies.

## 3. The store

Production and the harness both run `session.save_handler=redis` with
`session.save_path=tcp://nextcloud-redis:6379?auth=…` (never print it).
php-redis stores sessions under `PHPREDIS_SESSION:<sessionid>`.

The stored payload is the session array serialized with **igbinary**
(`session.serialize_handler=igbinary` in both environments), i.e. an array with
a single key `encrypted_session_data` whose value is the ciphertext.

**Do not parse igbinary.** The ciphertext has an unmistakable shape and is
authenticated, so extract it with an anchored pattern and verify the HMAC:

```
([0-9a-f]{64,})\|([0-9a-f]{32})\|([0-9a-f]{128})\|3
```

If it does not match **exactly once**, delegate. Forgery is impossible without
the passphrase because the HMAC is checked before anything is trusted.

## 4. The crypto (verified against `OC\Security\Crypto`)

Blob format: `ciphertext_hex|iv_hex|hmac_hex|3`.

1. `keyMaterial = HKDF-SHA512(passphrase)` — no salt/info, 64 bytes
   (PHP's `hash_hkdf('sha512', $password)`; an empty salt and 64 zero bytes give
   the same result, confirmed).
2. `encKey = keyMaterial[0..32]`, `macKey = keyMaterial[32..64]`.
3. **HMAC-SHA512** — key = the **ASCII hex string** of `SHA-512(macKey || 'a')`
   (128 chars; phpseclib uses a key of exactly the block length as-is), message =
   `ciphertext_hex || iv_hex` (the **hex strings**). Compare **constant-time**
   against `hmac_hex`. *Verified equal to PHP's own `calculateHMAC`.*
4. **AES-128-CBC** — key = `PBKDF2-SHA1(encKey, salt="phpseclib", 1000, dkLen=16)`
   (phpseclib's `new AES('cbc')` defaults to a 128-bit key), IV = `iv_hex`.
   *Verified: decrypts PHP's ciphertext to the exact plaintext.*
5. Plaintext = **JSON** (PKCS#7 padding). Parse it; on any failure → delegate.

## 5. The session keys (from a real, decrypted DAV session)

```
AUTHENTICATED_TO_DAV_BACKEND = 'alice'   user_id = 'alice'   loginname = 'alice'
app_password, login_credentials, token-id, token_scope, requesttoken, LAST_ACTIVITY
```

## 6. The ordered decision (implement literally)

For a request with **no `Authorization` header** and a session cookie:

1. Extract both cookies; no session cookie or no passphrase → **delegate**.
2. `GET PHPREDIS_SESSION:<id>`; missing → **delegate** (logged out/expired).
3. Extract + verify + decrypt + parse as in §3–4; any failure → **delegate**.
4. `uid = session['user_id']` (must be a non-empty string) → else **delegate**.
5. The user must exist and be enabled, and the path must belong to them — the
   existing `uid == target_user` rule stays.
6. **Token revalidation** (`Session::validateSession()`): the session is only
   valid while its token is. `token = session['app_password']` if present, else
   the session id; look it up in `oc_authtoken` (`version = 2`). Reject an
   expired token, a `WIPE_TOKEN` (type 2) or `password_invalid`; otherwise
   follow `checkTokenCredentials()`: a token whose `uid` matches the session's
   `user_id` is valid when `last_check > now - 300` **or** when it is
   passwordless (`password IS NULL`, the plain-browser case). A password-bearing
   token with a stale `last_check` needs `checkPassword()`, which the sidecar
   cannot reproduce → **delegate**. **Token type is not a rejection criterion
   here**: a browser session's `TEMPORARY_TOKEN` (0) is valid. Failing → **delegate**.
7. **2FA** (`TwoFactorManager::needsSecondFactor`): accept only if the session
   has `app_password` (app-password sessions skip 2FA) **or**
   `two_factor_auth_passed === uid`. Otherwise → **delegate**.
8. **Branch 2 (preferred, no CSRF needed)**: `AUTHENTICATED_TO_DAV_BACKEND === uid`
   → **accept**. (PHP's `requiresCSRFCheck()` is false in this case.)
9. **Branch 1**: `AUTHENTICATED_TO_DAV_BACKEND` absent → PHP additionally runs
   `passesCSRFCheck()`, so reproduce it or delegate:
   - `GET/HEAD/OPTIONS` are exempt (`requiresCSRFCheck()` false) → **accept**;
   - otherwise the request must carry a valid `requesttoken`: the `requesttoken`
     header (or GET/POST param) must be the web UI's obfuscated token
     `base64(value XOR secret) : base64(secret)`. PHP's
     `CsrfToken::getDecryptedValue()` splits on `:`, base64-decodes both parts
     and XORs them; the result must equal `session['requesttoken']`
     (`isTokenValid` = `hash_equals`). A raw value without `:` is never valid.
     **and** `nc_sameSiteCookiestrict === 'true'` with the lax cookie present
     (`passesStrictCookieCheck`). Anything short of that → **delegate**.
10. A `WWW-Authenticate`/`Authorization` header present but not Basic → delegate.

## 7. Must delegate (never accept)

Missing/odd cookie, Redis miss, HMAC or AES failure, unparsable JSON, no
`user_id`, disabled or missing user, token revalidation failure, 2FA not proven,
`AUTHENTICATED_TO_DAV_BACKEND` set to a **different** uid, a non-exempt method
without a matching requesttoken, a missing strict cookie, a path not owned by
the session user, any Redis/DB error (fail closed, never fall through to
"accept").

## 8. Harness recipe (how to obtain a real session)

`tests/local/`: the app password in `state/app_password` belongs to **alice**
(the admin form login does not work there — "login name does not match").

```sh
AP=$(cat state/app_password)
J=/tmp/jar
# 1. Basic DAV request: creates the session and sets AUTHENTICATED_TO_DAV_BACKEND
curl -s -c $J -u alice:$AP -X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
  --data '<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>' \
  http://127.0.0.1:18081/remote.php/dav/files/alice/     # -> 207
# 2. cookie-only request: PHP answers 207 (this is what the sidecar must match)
curl -s -b $J -X PROPFIND ... http://127.0.0.1:18081/remote.php/dav/files/alice/
```

The session is at `/tmp/sess_<id>` inside `ncdav-e2e-nc` (files handler there,
igbinary + the same crypto), which is convenient for negative tests.

## 9. Test matrix (all required)

Accept: cookie-only PROPFIND on the owner's path; a second request on the same
session; a REPORT.
Delegate: no cookie; a forged/tampered ciphertext; a wrong passphrase; a
truncated blob; an expired/absent Redis key (logout); a session whose
`user_id` is another user; a path belonging to another user; a session without
`app_password` and without `two_factor_auth_passed`; a session whose token was
revoked; a non-exempt method with a wrong/absent requesttoken; a missing strict
cookie; Redis unreachable.

**Every delegate case must be asserted to return 501 with no sidecar header.**
