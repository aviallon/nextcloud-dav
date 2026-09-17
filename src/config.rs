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

/// Nextcloud's default `{urn:ietf:params:xml:ns:carddav}max-resource-size`.
pub const MAX_RESOURCE_SIZE: u64 = 5_242_880;

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
    pub max_connections: u32,
    pub php_timeout: Duration,
    /// Accept invalid TLS certificates on the PHP fallback.
    pub allow_self_signed: bool,
    pub config_path: PathBuf,
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

        Ok(Config {
            database,
            database_prefix: nc.database_prefix,
            secret,
            listen,
            nextcloud_url,
            log_level: opt.log_level.unwrap_or_else(|| "info".to_string()),
            bruteforce,
            max_connections: opt.max_connections.unwrap_or(16),
            php_timeout,
            allow_self_signed,
            config_path,
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
}
