// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared test harness: a throwaway PostgreSQL cluster + schema, seeded rows,
//! and an in-process axum `Router` speaking the real request path.
//!
//! The cluster is started once per test binary (via `OnceLock`) using the
//! `initdb`/`pg_ctl` binaries from `$PATH`. When they are absent the DB-backed
//! tests **skip** (they print a clear message and return) rather than fail, so
//! `cargo test` still works on a machine without PostgreSQL.
//!
//! Override with `NEXTCLOUD_DAV_TEST_DATABASE_URL` to point at an existing
//! server; the harness then creates isolated `ncdav_test_*` databases on it.

#![allow(dead_code)]

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use nextcloud_dav::auth::token::hash_token;
use nextcloud_dav::auth::Authenticator;
use nextcloud_dav::config::BruteforceConfig;
use nextcloud_dav::db::Db;
use nextcloud_dav::php::PhpClient;
use nextcloud_dav::routes::{router, AppState};
use sqlx::any::AnyConnectOptions;
use sqlx::{AnyPool, Row};
use std::net::TcpListener;
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The app-password secret used by the DB-backed tests.
pub const TEST_SECRET: &str = "unit-test-secret";

/// Counts started clusters/databases so tests do not collide.
static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct PgCluster {
    data_dir: std::path::PathBuf,
    port: u16,
}

impl Drop for PgCluster {
    fn drop(&mut self) {
        let _ = Command::new("pg_ctl")
            .args([
                "-D",
                self.data_dir.to_str().unwrap_or_default(),
                "-m",
                "immediate",
                "-w",
                "stop",
            ])
            .output();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "{:?} failed: {}{}",
            cmd,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn cluster() -> Option<&'static PgCluster> {
    static CLUSTER: OnceLock<Option<PgCluster>> = OnceLock::new();
    CLUSTER
        .get_or_init(|| {
            if !command_exists("initdb") || !command_exists("pg_ctl") {
                return None;
            }
            // A fixed path so leftovers from a crashed previous run are removed.
            let base = std::env::temp_dir().join("nextcloud-dav-pgtest");
            let _ = std::fs::remove_dir_all(&base);
            if std::fs::create_dir_all(&base).is_err() {
                return None;
            }
            let data_dir = base.join("data");
            run(Command::new("initdb").args([
                "-D",
                data_dir.to_str().unwrap(),
                "-U",
                "postgres",
                "-A",
                "trust",
                "--no-locale",
                "-E",
                "UTF8",
            ]))
            .ok()?;

            // Pick a free loopback port.
            let listener = TcpListener::bind("127.0.0.1:0").ok()?;
            let port = listener.local_addr().ok()?.port();
            drop(listener);

            run(Command::new("pg_ctl").args([
                "-D",
                data_dir.to_str().unwrap(),
                "-o",
                &format!(
                    "-p {port} -c listen_addresses=127.0.0.1 -c max_connections=500 -F"
                ),
                "-w",
                "start",
            ]))
            .ok()?;

            Some(PgCluster { data_dir, port })
        })
        .as_ref()
}

/// A freshly created, isolated database with the Nextcloud `oc_*` schema.
pub struct TestEnv {
    pub db: Arc<Db>,
    pub prefix: String,
    pub secret: String,
    url: String,
}

impl TestEnv {
    /// Creates a new isolated database. Returns `None` when PostgreSQL is not
    /// available, so callers can skip.
    pub async fn new() -> Option<TestEnv> {
        let cluster_url = if let Ok(url) = std::env::var("NEXTCLOUD_DAV_TEST_DATABASE_URL") {
            url
        } else {
            let cluster = cluster()?;
            format!("postgres://postgres@127.0.0.1:{}/postgres", cluster.port)
        };
        let prefix = "oc_";

        let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let db_name = format!("ncdav_test_{}_{}", std::process::id(), n);

        // Connect to the maintenance database and create ours.
        let admin_opts = AnyConnectOptions::from_str(&cluster_url).ok()?;
        sqlx::any::install_default_drivers();
        let admin = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect_with(admin_opts)
            .await
            .ok()?;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {db_name}"
        )))
        .execute(&admin)
        .await;
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db_name}")))
            .execute(&admin)
            .await
            .ok()?;
        admin.close().await;

        // Rewrite the URL to point at the new database.
        let test_url = replace_database(&cluster_url, &db_name);
        let options = AnyConnectOptions::from_str(&test_url).ok()?;
        let db = Db::new(options, prefix.to_string(), 5).await.ok()?;
        create_schema(db.pool()).await.ok()?;

        Some(TestEnv {
            db: Arc::new(db),
            prefix: prefix.to_string(),
            secret: TEST_SECRET.to_string(),
            url: test_url,
        })
    }

    pub fn pool(&self) -> &AnyPool {
        self.db.pool()
    }

    /// The number of rows in a table (unquoted name).
    pub async fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) AS c FROM {}{}", self.prefix, table);
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get::<i64, _>("c")
            .unwrap()
    }

    // ------------------------------------------------------------------
    // Seeding
    // ------------------------------------------------------------------

    pub async fn seed_user(&self, uid: &str, displayname: Option<&str>) {
        let sql = format!(
            "INSERT INTO {}users (uid, displayname) VALUES (?, ?)",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .bind(displayname)
            .execute(self.pool())
            .await
            .unwrap();
    }

    pub async fn disable_user(&self, uid: &str) {
        let sql = format!(
            "INSERT INTO {}preferences (userid, appid, configkey, configvalue) \
             VALUES (?, 'core', 'enabled', 'false')",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .execute(self.pool())
            .await
            .unwrap();
    }

    pub async fn seed_addressbook(
        &self,
        principaluri: &str,
        uri: &str,
        displayname: Option<&str>,
        description: Option<&str>,
        synctoken: i64,
    ) -> i64 {
        let sql = format!(
            "INSERT INTO {}addressbooks (principaluri, displayname, uri, description, synctoken) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(principaluri)
            .bind(displayname)
            .bind(uri)
            .bind(description)
            .bind(synctoken)
            .fetch_one(self.pool())
            .await
            .unwrap()
            .get("id")
    }

    /// Inserts a card exactly as PHP's `createCard()` would (md5 etag,
    /// byte length), then logs the add-change and bumps the sync token.
    pub async fn seed_card(&self, addressbook_id: i64, uri: &str, carddata: &[u8]) -> i64 {
        let etag = md5_hex(carddata);
        let sql = format!(
            "INSERT INTO {}cards (addressbookid, carddata, uri, lastmodified, etag, size, uid) \
             VALUES (?, ?, ?, 1_700_000_000, ?, ?, ?) RETURNING id",
            self.prefix
        );
        let id: i64 = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(addressbook_id)
            .bind(carddata.to_vec())
            .bind(uri)
            .bind(&etag)
            .bind(carddata.len() as i64)
            .bind(uid_of(carddata))
            .fetch_one(self.pool())
            .await
            .unwrap()
            .get("id");
        self.add_change(addressbook_id, uri, 1).await;
        id
    }

    /// Inserts an `oc_cards` row without a change log (for sync-token tests
    /// that need exact control over the change table).
    pub async fn insert_card_raw(&self, addressbook_id: i64, uri: &str, carddata: &[u8]) -> i64 {
        let etag = md5_hex(carddata);
        let sql = format!(
            "INSERT INTO {}cards (addressbookid, carddata, uri, lastmodified, etag, size, uid) \
             VALUES (?, ?, ?, 1_700_000_000, ?, ?, ?) RETURNING id",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(addressbook_id)
            .bind(carddata.to_vec())
            .bind(uri)
            .bind(&etag)
            .bind(carddata.len() as i64)
            .bind(uid_of(carddata))
            .fetch_one(self.pool())
            .await
            .unwrap()
            .get("id")
    }

    /// Mirrors `CardDavBackend::addChange()`: insert a change row carrying the
    /// *pre-increment* token, then bump the book's synctoken by one.
    pub async fn add_change(&self, addressbook_id: i64, uri: &str, operation: i64) {
        let select = format!(
            "SELECT synctoken FROM {}addressbooks WHERE id = ?",
            self.prefix
        );
        let token: i32 = sqlx::query(sqlx::AssertSqlSafe(select))
            .bind(addressbook_id)
            .fetch_one(self.pool())
            .await
            .unwrap()
            .get("synctoken");
        let insert = format!(
            "INSERT INTO {}addressbookchanges (uri, synctoken, addressbookid, operation, created_at) \
             VALUES (?, ?, ?, ?, 1_700_000_000)",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(insert))
            .bind(uri)
            .bind(token)
            .bind(addressbook_id)
            .bind(operation as i16)
            .execute(self.pool())
            .await
            .unwrap();
        let update = format!(
            "UPDATE {}addressbooks SET synctoken = ? WHERE id = ?",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(update))
            .bind(token + 1)
            .bind(addressbook_id)
            .execute(self.pool())
            .await
            .unwrap();
    }

    pub async fn set_synctoken(&self, addressbook_id: i64, token: i64) {
        let sql = format!(
            "UPDATE {}addressbooks SET synctoken = ? WHERE id = ?",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(token)
            .bind(addressbook_id)
            .execute(self.pool())
            .await
            .unwrap();
    }

    pub async fn seed_property(&self, addressbook_id: i64, card_id: i64, name: &str, value: &str) {
        let sql = format!(
            "INSERT INTO {}cards_properties (addressbookid, cardid, name, value, preferred) \
             VALUES (?, ?, ?, ?, 0)",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(addressbook_id)
            .bind(card_id)
            .bind(name)
            .bind(value)
            .execute(self.pool())
            .await
            .unwrap();
    }

    /// Inserts a valid app-password token row for `uid`/`password`.
    pub async fn seed_token(
        &self,
        uid: &str,
        login_name: &str,
        password: &str,
        token_type: i64,
        version: i64,
    ) {
        self.seed_token_full(uid, login_name, password, token_type, version, None, false, now())
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn seed_token_full(
        &self,
        uid: &str,
        login_name: &str,
        password: &str,
        token_type: i64,
        version: i64,
        expires: Option<i64>,
        password_invalid: bool,
        last_check: i64,
    ) {
        let hash = hash_token(password, &self.secret);
        let sql = format!(
            "INSERT INTO {}authtoken \
             (uid, login_name, name, token, type, remember, last_activity, last_check, scope, expires, version, password_invalid) \
             VALUES (?, ?, '', ?, ?, 0, ?, ?, NULL, ?, ?, ?)",
            self.prefix
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uid)
            .bind(login_name)
            .bind(&hash)
            .bind(token_type as i16)
            .bind(last_check)
            .bind(last_check)
            .bind(expires)
            .bind(version as i16)
            .bind(password_invalid)
            .execute(self.pool())
            .await
            .unwrap();
    }

    // ------------------------------------------------------------------
    // App / requests
    // ------------------------------------------------------------------

    /// Builds the production router with an unreachable PHP fallback (auth
    /// fast path only) and brute-force recording off.
    pub async fn app(&self) -> Router {
        self.app_with_php_base("http://127.0.0.1:1/").await
    }

    pub async fn app_with_php_base(&self, base: &str) -> Router {
        let options = AnyConnectOptions::from_str(&self.url).unwrap();
        let db = Arc::new(Db::new(options, self.prefix.clone(), 5).await.unwrap());
        let php = PhpClient::new(base, Duration::from_millis(500), false).unwrap();
        let auth = Authenticator::new(db.clone(), php, self.secret.clone(), BruteforceConfig::default());
        let config = nextcloud_dav::config::Config {
            database: AnyConnectOptions::from_str(&self.url).unwrap(),
            database_prefix: self.prefix.clone(),
            secret: self.secret.clone(),
            listen: "127.0.0.1:0".parse().unwrap(),
            nextcloud_url: base.to_string(),
            log_level: "error".to_string(),
            bruteforce: BruteforceConfig::default(),
            max_connections: 5,
            php_timeout: Duration::from_millis(500),
            allow_self_signed: false,
            config_path: std::path::PathBuf::from("/dev/null"),
        };
        router(Arc::new(AppState { db, auth, config }))
    }

    /// A router that shares this env's pool instead of opening a new one.
    pub fn app_shared(&self) -> Router {
        let php = PhpClient::new("http://127.0.0.1:1/", Duration::from_millis(500), false).unwrap();
        let auth = Authenticator::new(
            self.db.clone(),
            php,
            self.secret.clone(),
            BruteforceConfig::default(),
        );
        let config = nextcloud_dav::config::Config {
            database: AnyConnectOptions::from_str(&self.url).unwrap(),
            database_prefix: self.prefix.clone(),
            secret: self.secret.clone(),
            listen: "127.0.0.1:0".parse().unwrap(),
            nextcloud_url: "http://127.0.0.1:1/".to_string(),
            log_level: "error".to_string(),
            bruteforce: BruteforceConfig::default(),
            max_connections: 5,
            php_timeout: Duration::from_millis(500),
            allow_self_signed: false,
            config_path: std::path::PathBuf::from("/dev/null"),
        };
        router(Arc::new(AppState { db: self.db.clone(), auth, config }))
    }
}

/// Basic auth header value for `user:password`.
pub fn basic(user: &str, password: &str) -> HeaderValue {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    HeaderValue::from_str(&format!("Basic {encoded}")).unwrap()
}

/// A convenience request builder with Basic auth.
pub fn request(method: &str, path: &str, user: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, basic(user, password))
        .body(Body::empty())
        .unwrap()
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }
}

/// Sends a request through the router and collects the response.
pub async fn call(app: &Router, request: Request<Body>) -> Resp {
    use tower::ServiceExt;
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Resp {
        status,
        headers,
        body,
    }
}

/// PROPFIND helper with a body and optional Depth header.
pub async fn propfind(
    app: &Router,
    path: &str,
    user: &str,
    password: &str,
    depth: &str,
    body: &str,
) -> Resp {
    let mut builder = Request::builder()
        .method("PROPFIND")
        .uri(path)
        .header(header::AUTHORIZATION, basic(user, password))
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8");
    if !depth.is_empty() {
        builder = builder.header("Depth", depth);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    call(app, request).await
}

/// REPORT helper.
pub async fn report(
    app: &Router,
    path: &str,
    user: &str,
    password: &str,
    body: &str,
) -> Resp {
    let request = Request::builder()
        .method("REPORT")
        .uri(path)
        .header(header::AUTHORIZATION, basic(user, password))
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap();
    call(app, request).await
}

/// GET helper.
pub async fn get(app: &Router, path: &str, user: &str, password: &str) -> Resp {
    call(app, request("GET", path, user, password)).await
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub fn md5_hex(data: &[u8]) -> String {
    // Nextcloud stores `md5($cardData)`.
    Md5::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Minimal MD5 implementation (RFC 1321) for seeding faithful ETags.
struct Md5;

impl Md5 {
    fn digest(input: &[u8]) -> [u8; 16] {
        const S: [u32; 64] = [
            7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9,
            14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4,
            11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
        ];
        const K: [u32; 64] = [
            0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
            0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
            0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
            0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
            0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
            0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
            0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
            0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
            0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
            0xeb86d391,
        ];
        let mut msg = input.to_vec();
        let bit_len = (input.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_le_bytes());

        let mut a0: u32 = 0x67452301;
        let mut b0: u32 = 0xefcdab89;
        let mut c0: u32 = 0x98badcfe;
        let mut d0: u32 = 0x10325476;

        for chunk in msg.chunks(64) {
            let mut m = [0u32; 16];
            for (i, word) in chunk.chunks(4).enumerate() {
                m[i] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            }
            let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
            for i in 0..64 {
                let (f, g) = match i {
                    0..=15 => ((b & c) | (!b & d), i),
                    16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let f = f
                    .wrapping_add(a)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(f.rotate_left(S[i]));
            }
            a0 = a0.wrapping_add(a);
            b0 = b0.wrapping_add(b);
            c0 = c0.wrapping_add(c);
            d0 = d0.wrapping_add(d);
        }

        let mut out = [0u8; 16];
        for (i, v) in [a0, b0, c0, d0].iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// Best-effort UID extraction, mirroring the stored `uid` column.
pub fn uid_of(carddata: &[u8]) -> String {
    let text = String::from_utf8_lossy(carddata);
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("UID:") {
            return rest.trim().to_string();
        }
    }
    String::new()
}

/// Replaces the database name in a postgres URL (last path segment).
fn replace_database(url: &str, db_name: &str) -> String {
    match url.rfind('/') {
        Some(idx) => {
            let (base, _) = url.split_at(idx);
            format!("{base}/{db_name}")
        }
        None => format!("{url}/{db_name}"),
    }
}

/// Creates the subset of the Nextcloud schema the sidecar reads.
async fn create_schema(pool: &AnyPool) -> Result<(), sqlx::Error> {
    let ddl = r#"
CREATE TABLE oc_users (
    uid varchar(64) NOT NULL PRIMARY KEY,
    displayname varchar(255) NULL
);
CREATE TABLE oc_preferences (
    userid varchar(64) NOT NULL,
    appid varchar(32) NOT NULL,
    configkey varchar(64) NOT NULL,
    configvalue text NULL
);
CREATE TABLE oc_addressbooks (
    id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    principaluri varchar(255) NULL,
    displayname varchar(255) NULL,
    uri varchar(255) NULL,
    description varchar(255) NULL,
    synctoken integer NOT NULL DEFAULT 1
);
CREATE UNIQUE INDEX addressbook_index ON oc_addressbooks (principaluri, uri);
CREATE TABLE oc_cards (
    id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    addressbookid integer NOT NULL DEFAULT 0,
    carddata bytea NULL,
    uri varchar(255) NULL,
    lastmodified bigint NULL,
    etag varchar(32) NULL,
    size bigint NOT NULL DEFAULT 0,
    uid varchar(255) NULL
);
CREATE INDEX cards_abiduri ON oc_cards (addressbookid, uri);
CREATE TABLE oc_addressbookchanges (
    id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    uri varchar(255) NULL,
    synctoken integer NOT NULL DEFAULT 1,
    addressbookid integer NOT NULL,
    operation smallint NOT NULL,
    created_at integer NOT NULL DEFAULT 0
);
CREATE TABLE oc_cards_properties (
    id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    addressbookid bigint NOT NULL DEFAULT 0,
    cardid bigint NOT NULL DEFAULT 0,
    name varchar(64) NULL,
    value varchar(255) NULL,
    preferred integer NOT NULL DEFAULT 1
);
CREATE TABLE oc_dav_shares (
    id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    principaluri varchar(255) NULL,
    type varchar(255) NULL,
    access smallint NULL,
    resourceid integer NULL,
    publicuri varchar(255) NULL
);
CREATE TABLE oc_authtoken (
    id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    uid varchar(64) NOT NULL DEFAULT '',
    login_name varchar(64) NOT NULL DEFAULT '',
    password text NULL,
    name text NOT NULL DEFAULT '',
    token varchar(200) NOT NULL DEFAULT '',
    type smallint NULL DEFAULT 0,
    remember smallint NULL DEFAULT 0,
    last_activity integer NULL DEFAULT 0,
    last_check integer NULL DEFAULT 0,
    scope text NULL,
    expires bigint NULL,
    version smallint NOT NULL DEFAULT 0,
    password_invalid boolean NOT NULL DEFAULT false
);
CREATE UNIQUE INDEX authtoken_token_index ON oc_authtoken (token);
CREATE TABLE oc_bruteforce_attempts (
    id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    ip varchar(255) NULL,
    subnet varchar(255) NULL,
    occurred bigint NULL,
    action varchar(255) NULL,
    metadata text NULL
);
"#;
    for statement in ddl.split(';') {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        sqlx::query(sqlx::AssertSqlSafe(statement.to_string()))
            .execute(pool)
            .await?;
    }
    Ok(())
}
