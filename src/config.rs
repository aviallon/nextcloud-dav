// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Configuration loading.
//!
//! Database connection details and `overwrite.cli.url` come from
//! [`nextcloud_config_parser`], exactly like `notify_push`. The parser does not
//! expose `secret` nor arbitrary app-config keys, so the merged `$CONFIG` array
//! is additionally parsed with `php-literal-parser` to read:
//!
//! ```php
//! 'secret' => '...',
//! 'nextcloud_dav' => [
//!     'listen'              => '127.0.0.1:7868',
//!     'fallback_base_url'   => 'https://cloud.example.com',
//!     'record_bruteforce_attempts' => true,
//!     'php_timeout_secs'    => 30,
//! ],
//! ```

use crate::error::{Error, Result};
use indexmap::IndexMap;
use nextcloud_config_parser::Config as NcConfig;
use php_literal_parser::{Key, Value};
use sqlx::any::AnyConnectOptions;
use std::fs::DirEntry;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// Default listen address. Loopback only: nginx is expected to proxy to it and
/// the health endpoint must never be exposed publicly.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7868";

/// PHP's `checkTokenCredentials()` re-validates at most once every 5 minutes.
pub const TOKEN_RECHECK_INTERVAL: i64 = 300;

/// `carddav_sync_request_truncation` default (`CardDavBackend.php`).
pub const DEFAULT_SYNC_LIMIT: i64 = 2500;

/// Sabre's advertised `{urn:ietf:params:xml:ns:carddav}max-resource-size`.
/// This is *not* Nextcloud's write limit: `CardDAV\Plugin` hardcodes 10 MB and
/// Nextcloud does not override it.
pub const MAX_RESOURCE_SIZE: u64 = 10_000_000;

/// Nextcloud's `card_size_limit` app-config default (`dav` app), enforced by
/// `CardDavValidatePlugin::beforePut()` on `PUT`. It is deliberately smaller
/// than [`MAX_RESOURCE_SIZE`].
pub const DEFAULT_CARD_SIZE_LIMIT: u64 = 5_242_880;

/// Default `pg_notify` channel used to wake the event-dispatch worker.
pub const DEFAULT_EVENT_NOTIFY_CHANNEL: &str = "oc_dav_event_outbox";

/// `\RedisCluster::*` / `\PDO::*` constants appear inside `$CONFIG` and are not
/// valid PHP literals for the parser. Replace them with their integer values,
/// mirroring `nextcloud-config-parser`.
const CONFIG_CONSTANTS: &[(&str, &str)] = &[
    (r"\RedisCluster::FAILOVER_NONE", "0"),
    (r"\RedisCluster::FAILOVER_ERROR", "1"),
    (r"\RedisCluster::DISTRIBUTE", "2"),
    (r"\RedisCluster::FAILOVER_DISTRIBUTE_SLAVES", "3"),
    (r"\PDO::MYSQL_ATTR_SSL_KEY", "1007"),
    (r"\PDO::MYSQL_ATTR_SSL_CERT", "1008"),
    (r"\PDO::MYSQL_ATTR_SSL_CA", "1009"),
    (r"\PDO::MYSQL_ATTR_SSL_VERIFY_SERVER_CERT", "1014"),
];

/// Brute-force throttling parity settings (`dav-bench/CARDDAV_DESIGN.md` §3.5).
#[derive(Debug, Clone)]
pub struct BruteforceConfig {
    /// `auth.bruteforce.protection.enabled` (Nextcloud default: true).
    pub enabled: bool,
    /// `auth.bruteforce.max-attempts` (Nextcloud default: 10).
    pub max_attempts: i64,
    /// `security.ipv6_normalized_subnet_size` (Nextcloud default: 56).
    pub ipv6_subnet_size: u8,
    /// Whether the sidecar may INSERT into `oc_bruteforce_attempts`.
    ///
    /// This is the **only** write the v1 sidecar performs. Set it to `false` to
    /// keep the sidecar strictly read-only (the delay/block checks still run,
    /// but new failures from the sidecar's fast path are not recorded).
    pub record_attempts: bool,
}

impl Default for BruteforceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 10,
            ipv6_subnet_size: 56,
            record_attempts: false,
        }
    }
}

/// The `nextcloud_dav.event_dispatch` config block.
///
/// The crate does not run a dispatcher in phase 1: native writes enqueue an
/// `oc_dav_event_outbox` row and the companion PHP app drains it. What this
/// block controls is *where each effect is owned*, recorded into every outbox
/// row so a later phase can move an effect to `rust` without ambiguity. Each
/// effect id must be claimed by exactly one backend.
#[derive(Debug, Clone)]
pub struct EventDispatchConfig {
    /// Master switch: when false, native writes are refused (501) exactly like
    /// a missing outbox table.
    pub enabled: bool,
    /// `pg_notify` channel the PHP worker `LISTEN`s on.
    pub notify_channel: String,
    /// effect id -> `"php"` | `"rust"`. Defaults to `"php"` for every known
    /// effect. Unknown keys are ignored (forward compatibility).
    pub handlers: IndexMap<String, String>,
}

impl Default for EventDispatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            notify_channel: DEFAULT_EVENT_NOTIFY_CHANNEL.to_string(),
            handlers: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database: AnyConnectOptions,
    pub database_prefix: String,
    /// `$CONFIG['secret']`; empty when unset (legacy instances used
    /// `sha512(token)` only).
    pub secret: String,
    pub listen: SocketAddr,
    /// Base URL of the PHP front end, with a trailing slash. Used for the
    /// credentialed fallback.
    pub nextcloud_url: String,
    pub log_level: String,
    pub bruteforce: BruteforceConfig,
    /// The `nextcloud_dav.event_dispatch` block.
    pub event_dispatch: EventDispatchConfig,
    /// `nextcloud_dav.card_size_limit` override. When `None`, the value is read
    /// from `oc_appconfig` at startup with [`DEFAULT_CARD_SIZE_LIMIT`] as the
    /// fallback.
    pub card_size_limit_override: Option<u64>,
    pub max_connections: u32,
    pub php_timeout: Duration,
    /// Accept invalid TLS certificates on the PHP fallback.
    pub allow_self_signed: bool,
    pub config_path: PathBuf,
    /// `$CONFIG['instanceid']`, needed for the `oc:id` property
    /// (`DavUtil::getDavFileId()` = `sprintf('%08d', $id) . instanceid`).
    pub instance_id: String,
    /// `$CONFIG['datadirectory']`, needed to reproduce `Local::free_space()`
    /// (`disk_free_space`) for a finite user quota.
    pub datadirectory: Option<PathBuf>,
    /// `$CONFIG['enable_previews']` (default true). Gates `nc:has-preview`.
    pub previews_enabled: bool,
    /// `$CONFIG['data-fingerprint']` (default `''`), served as
    /// `oc:data-fingerprint` for every node (`FilesPlugin`).
    pub data_fingerprint: String,
    /// True when a primary object store is configured (`objectstore` or
    /// `objectstore_multibucket`). `oc:downloadURL` is then a presigned URL the
    /// sidecar cannot derive, so a request for it delegates.
    pub objectstore: bool,
    /// True when the `end_to_end_encryption` app is enabled. That app is the
    /// only handler of `nc:is-encrypted`, so when it is on the sidecar must
    /// delegate that property instead of answering PHP's no-handler 404.
    pub e2e_encryption: bool,
    /// True when `core/shareapi_exclude_groups` is configured. The sidecar then
    /// delegates, because it cannot reproduce `ShareDisableChecker` group
    /// expansion for LDAP/circles.
    pub sharing_exclude_groups: bool,
    /// `$CONFIG['force_language']` when set. `L10N\Factory::getUserLanguage()`
    /// returns it before the user's `core/lang` preference, so the discovery
    /// `nc:language` property must too.
    pub force_language: Option<String>,
}

#[derive(Debug, Default)]
pub struct Opt {
    pub config_file: Option<PathBuf>,
    pub glob_config: bool,
    pub database_url: Option<String>,
    pub listen: Option<String>,
    pub log_level: Option<String>,
    pub max_connections: Option<u32>,
    pub help: bool,
    pub version: bool,
}

pub const HELP: &str = "\
nextcloud-dav - read-only CardDAV sidecar for Nextcloud

USAGE:
    nextcloud-dav [OPTIONS] [CONFIG_FILE]

ARGS:
    CONFIG_FILE            Path to config/config.php (default: config/config.php)

OPTIONS:
    --config <PATH>        Same as CONFIG_FILE
    --glob-config          Also load sibling *.config.php files
    --database-url <URL>   Override the database URL parsed from config.php
    --listen <ADDR>        Override the listen address
    --log-level <LEVEL>    Log level (default: info)
    --max-connections <N>  Database pool size (default: 16)
    -h, --help             Print help
    -V, --version          Print version
";

impl Opt {
    /// Minimal argument parser. A dedicated arg crate is not worth the build
    /// cost for five flags.
    pub fn parse() -> Result<Opt> {
        let mut opt = Opt::default();
        let mut args = std::env::args().skip(1).peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => opt.help = true,
                "-V" | "--version" => opt.version = true,
                "--glob-config" => opt.glob_config = true,
                "--config" => {
                    opt.config_file = Some(PathBuf::from(next_arg(&mut args, "--config")?));
                }
                "--database-url" => {
                    opt.database_url = Some(next_arg(&mut args, "--database-url")?);
                }
                "--listen" => opt.listen = Some(next_arg(&mut args, "--listen")?),
                "--log-level" => opt.log_level = Some(next_arg(&mut args, "--log-level")?),
                "--max-connections" => {
                    let raw = next_arg(&mut args, "--max-connections")?;
                    opt.max_connections =
                        Some(raw.parse().map_err(|_| {
                            Error::Config(format!("invalid --max-connections {raw}"))
                        })?);
                }
                other if other.starts_with('-') => {
                    return Err(Error::Config(format!("unknown argument {other}")));
                }
                other => {
                    if opt.config_file.is_some() {
                        return Err(Error::Config(format!("unexpected extra argument {other}")));
                    }
                    opt.config_file = Some(PathBuf::from(other));
                }
            }
        }
        Ok(opt)
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| Error::Config(format!("{flag} requires a value")))
}

impl Config {
    pub fn from_opt(opt: Opt) -> Result<Config> {
        let config_path = opt
            .config_file
            .clone()
            .unwrap_or_else(|| PathBuf::from("config/config.php"));

        let nc: NcConfig = if opt.glob_config {
            nextcloud_config_parser::parse_glob(&config_path)
        } else {
            nextcloud_config_parser::parse(&config_path)
        }
        .map_err(|e| Error::Config(e.to_string()))?;

        let raw = RawConfig::load(&config_path, opt.glob_config)?;
        let app = raw.get("nextcloud_dav").map(AppConfig);

        let database_url = opt
            .database_url
            .clone()
            .unwrap_or_else(|| nc.database.url());
        let database = AnyConnectOptions::from_str(&database_url)
            .map_err(|e| Error::Config(format!("invalid database url: {e}")))?;

        let nextcloud_url = {
            let fallback = app
                .and_then(|a| a.get_str_at("fallback_base_url"))
                .filter(|url| !url.is_empty())
                .or_else(|| raw.get_str("overwrite.cli.url").map(str::to_string))
                .unwrap_or_else(|| nc.nextcloud_url.clone());
            normalize_base_url(&fallback)
        };

        let listen_raw = opt
            .listen
            .clone()
            .or_else(|| app.and_then(|a| a.get_str_at("listen")))
            .unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let listen = SocketAddr::from_str(&listen_raw)
            .map_err(|e| Error::Config(format!("invalid listen address {listen_raw:?}: {e}")))?;

        let secret = raw
            .get_str("secret")
            .map(|s| s.to_string())
            .unwrap_or_default();

        let bruteforce = BruteforceConfig {
            enabled: raw.get_bool("auth.bruteforce.protection.enabled", true),
            max_attempts: raw.get_int("auth.bruteforce.max-attempts", 10),
            ipv6_subnet_size: raw
                .get_int("security.ipv6_normalized_subnet_size", 56)
                .clamp(32, 64) as u8,
            // Strictly read-only by default; enable for full parity with PHP.
            record_attempts: app
                .and_then(|a| a.get_bool_at("record_bruteforce_attempts"))
                .unwrap_or(false),
        };

        let php_timeout = Duration::from_secs(
            app.and_then(|a| a.get_int_at("php_timeout_secs"))
                .unwrap_or(30)
                .clamp(1, 300) as u64,
        );

        let allow_self_signed = app
            .and_then(|a| a.get_bool_at("allow_self_signed"))
            .unwrap_or(false);

        let event_dispatch = {
            let mut dispatch = EventDispatchConfig::default();
            let block = raw
                .get("nextcloud_dav")
                .map(|value| value["event_dispatch"].clone())
                .unwrap_or(Value::Null);
            if let Some(enabled) = AppConfig(&block).get_bool_at("enabled") {
                dispatch.enabled = enabled;
            }
            if let Some(channel) = AppConfig(&block).get_str_at("notify_channel") {
                if !channel.is_empty() {
                    dispatch.notify_channel = channel;
                }
            }
            if let Value::Array(map) = &block["handlers"] {
                for (key, value) in map {
                    if let (Some(key), Some(owner)) = (key.as_str(), value.as_str()) {
                        dispatch.handlers.insert(key.to_string(), owner.to_string());
                    }
                }
            }
            dispatch
        };

        let card_size_limit_override = app
            .and_then(|a| a.get_int_at("card_size_limit"))
            .filter(|limit| *limit > 0)
            .map(|limit| limit as u64);

        Ok(Config {
            database,
            database_prefix: nc.database_prefix,
            secret,
            listen,
            nextcloud_url,
            log_level: opt.log_level.unwrap_or_else(|| "info".to_string()),
            bruteforce,
            event_dispatch,
            card_size_limit_override,
            max_connections: opt.max_connections.unwrap_or(16),
            php_timeout,
            allow_self_signed,
            config_path,
            instance_id: raw.get_str("instanceid").unwrap_or_default().to_string(),
            datadirectory: raw
                .get_str("datadirectory")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
            previews_enabled: raw.get_bool("enable_previews", true),
            data_fingerprint: raw
                .get_str("data-fingerprint")
                .unwrap_or_default()
                .to_string(),
            objectstore: raw.get_str("objectstore").is_some()
                || raw.get("objectstore_multibucket").is_some(),
            // Set from `oc_appconfig` at startup (see `main.rs`).
            e2e_encryption: false,
            sharing_exclude_groups: false,
            force_language: raw
                .get_str("force_language")
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        })
    }

    /// Whether a `secret` was found in `config.php`. The fast path also works
    /// without one (legacy `sha512(token)` rows), so this is informational only.
    pub fn secret_configured(&self) -> bool {
        !self.secret.is_empty()
    }
}

fn normalize_base_url(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

/// The merged `$CONFIG` array, accessed by dotted app-config paths.
struct RawConfig {
    values: IndexMap<Key, Value>,
}

/// A small view over an app-config sub-array.
#[derive(Clone, Copy)]
struct AppConfig<'a>(&'a Value);

impl<'a> AppConfig<'a> {
    fn get_str_at(&self, key: &str) -> Option<String> {
        self.0[key].as_str().map(str::to_string)
    }

    fn get_int_at(&self, key: &str) -> Option<i64> {
        self.0[key].as_int()
    }

    fn get_bool_at(&self, key: &str) -> Option<bool> {
        match &self.0[key] {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

impl RawConfig {
    fn load(path: &Path, glob: bool) -> Result<Self> {
        let mut values: IndexMap<Key, Value> = IndexMap::new();
        for file in config_files(path, glob) {
            let parsed = parse_php(&file)?;
            let map = parsed.into_map().ok_or_else(|| {
                Error::Config(format!("$CONFIG in {} is not an array", file.display()))
            })?;
            for (key, value) in map {
                values.insert(key, value);
            }
        }
        Ok(Self { values })
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    fn get_int(&self, key: &str, default: i64) -> i64 {
        self.get(key).and_then(Value::as_int).unwrap_or(default)
    }

    fn get_bool(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            Some(Value::Bool(b)) => *b,
            _ => default,
        }
    }
}

fn config_files(path: &Path, glob: bool) -> Vec<PathBuf> {
    let mut files = vec![path.to_path_buf()];
    if glob {
        if let Some(parent) = path.parent() {
            if let Ok(dir) = parent.read_dir() {
                let mut extras: Vec<PathBuf> = dir
                    .filter_map(std::result::Result::ok)
                    .map(|entry: DirEntry| entry.path())
                    .filter(|p| {
                        p.to_str()
                            .map(|s| s.ends_with(".config.php"))
                            .unwrap_or(false)
                            && p != path
                    })
                    .collect();
                extras.sort();
                files.extend(extras);
            }
        }
    }
    files
}

/// Mirrors `nextcloud-config-parser`'s extraction: find the last `$CONFIG`
/// assignment and parse the literal after it.
fn parse_php(path: &Path) -> Result<Value> {
    let mut content = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("failed to read {}: {e}", path.display())))?;
    for (search, replace) in CONFIG_CONSTANTS {
        if content.contains(search) {
            content = content.replace(search, replace);
        }
    }
    let php = match content.rfind("$CONFIG") {
        Some(pos) => content[pos + "$CONFIG".len()..]
            .trim()
            .trim_start_matches('='),
        None => {
            return Err(Error::Config(format!(
                "$CONFIG not found in {}",
                path.display()
            )))
        }
    };
    php_literal_parser::from_str(php)
        .map_err(|e| Error::Config(format!("failed to parse {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_base_url() {
        assert_eq!(normalize_base_url("https://x"), "https://x/");
        assert_eq!(normalize_base_url("https://x/"), "https://x/");
    }

    #[test]
    fn parses_config_literal() {
        let value: Value = php_literal_parser::from_str(
            r#"[
                'secret' => 's3cr3t',
                'overwrite.cli.url' => 'https://cloud.example.com',
                'nextcloud_dav' => [
                    'listen' => '127.0.0.1:9999',
                    'record_bruteforce_attempts' => true,
                ],
                'auth.bruteforce.max-attempts' => 7,
            ]"#,
        )
        .unwrap();
        let raw = RawConfig {
            values: value.into_map().unwrap(),
        };
        assert_eq!(raw.get_str("secret"), Some("s3cr3t"));
        assert_eq!(raw.get_int("auth.bruteforce.max-attempts", 10), 7);
        let app = raw.get("nextcloud_dav").unwrap();
        let app = AppConfig(app);
        assert_eq!(app.get_str_at("listen").as_deref(), Some("127.0.0.1:9999"));
        assert_eq!(app.get_bool_at("record_bruteforce_attempts"), Some(true));
    }

    #[test]
    fn parses_event_dispatch_block() {
        let value: Value = php_literal_parser::from_str(
            r#"[
                'nextcloud_dav' => [
                    'card_size_limit' => 1234,
                    'event_dispatch' => [
                        'enabled' => false,
                        'notify_channel' => 'custom_channel',
                        'handlers' => [
                            'redis_cloud_id' => 'rust',
                            'activity_stream' => 'php',
                            'bogus' => 'rust',
                        ],
                    ],
                ],
            ]"#,
        )
        .unwrap();
        let raw = RawConfig {
            values: value.into_map().unwrap(),
        };
        let app = raw.get("nextcloud_dav").unwrap();
        let block = app["event_dispatch"].clone();
        assert_eq!(AppConfig(&block).get_bool_at("enabled"), Some(false));
        assert_eq!(
            AppConfig(&block).get_str_at("notify_channel").as_deref(),
            Some("custom_channel")
        );
        let mut dispatch = EventDispatchConfig::default();
        if let Value::Array(map) = &block["handlers"] {
            for (key, value) in map {
                dispatch.handlers.insert(
                    key.as_str().unwrap().to_string(),
                    value.as_str().unwrap().to_string(),
                );
            }
        }
        let registry = crate::outbox::EffectRegistry::from_config(&dispatch);
        assert_eq!(registry.rust_effects(), vec!["redis_cloud_id"]);
        assert_eq!(AppConfig(app).get_int_at("card_size_limit"), Some(1234));
    }
}
