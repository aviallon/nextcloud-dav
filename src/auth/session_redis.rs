// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A minimal RESP client for the session store.
//!
//! PHP's redis session handler keeps each session at
//! `PHPREDIS_SESSION:<sessionid>`. Reading it needs exactly two commands
//! (`AUTH` when the `save_path` carries one, then `GET`), so a full redis
//! client would be dead weight. This speaks just enough RESP2 to issue them,
//! and every failure is an `Err` the caller turns into a delegation.
//!
//! The connection string comes from the same value PHP puts in
//! `session.save_path` (`tcp://host:port?auth=...&database=...`), so the
//! password is never duplicated into the sidecar's config.

use crate::util::urldecode;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A parsed `session.save_path`. Only the TCP form is supported; anything else
/// leaves session auth delegated to PHP.
#[derive(Clone)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    /// `redis://user:password@host` ACL username (URL-decoded). Not secret.
    pub user: Option<String>,
    /// `?auth=...` / the userinfo password (URL-decoded). Never logged.
    pub auth: Option<String>,
    /// `?database=` / `?db=`; redis `SELECT`.
    pub database: Option<u32>,
    pub timeout: Duration,
}

impl RedisConfig {
    /// Parses `tcp://` / `redis://` URLs. Accepts an optional
    /// `user:password@` userinfo (percent-encoded) and the php-redis query keys
    /// `auth` / `database` (`db`). A value-less `auth` or a `unix://` path is
    /// rejected (fail closed).
    pub fn from_url(url: &str, timeout: Duration) -> Option<Self> {
        let rest = url
            .strip_prefix("tcp://")
            .or_else(|| url.strip_prefix("redis://"))?;
        let (rest, query) = match rest.split_once('?') {
            Some((a, q)) => (a, q),
            None => (rest, ""),
        };
        if rest.is_empty() {
            return None;
        }
        // Optional `user:password@` userinfo. Credentials are percent-encoded by
        // whoever builds the URL, so a literal `@` only appears as the delimiter.
        let (userinfo, authority) = match rest.rsplit_once('@') {
            Some((userinfo, authority)) => (Some(userinfo), authority),
            None => (None, rest),
        };
        let (user, userinfo_password) = match userinfo {
            Some(userinfo) => match userinfo.split_once(':') {
                Some((user, password)) => (Some(urldecode(user)), Some(urldecode(password))),
                None => (Some(urldecode(userinfo)), None),
            },
            None => (None, None),
        };
        let user = user.filter(|u| !u.is_empty());
        // A bracketed IPv6 literal keeps its colons (`redis://[::1]:6379`).
        let (host, port) = if let Some(host) = authority.strip_prefix('[') {
            let (host, rest) = host.split_once(']')?;
            let port = match rest.strip_prefix(':') {
                Some(port) => port.parse::<u16>().ok()?,
                None if rest.is_empty() => 6379,
                None => return None,
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, port.parse::<u16>().ok()?),
                None => (authority, 6379),
            }
        };
        if host.is_empty() {
            return None;
        }
        let mut auth = userinfo_password.filter(|password| !password.is_empty());
        let mut database = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            match key {
                "auth" => {
                    if value.is_empty() {
                        return None;
                    }
                    auth = Some(urldecode(value));
                }
                "database" | "db" => {
                    database = value.parse::<u32>().ok();
                }
                _ => {}
            }
        }
        Some(Self {
            host: host.to_string(),
            port,
            user,
            auth,
            database,
            timeout,
        })
    }
}

impl std::fmt::Debug for RedisConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth.as_ref().map(|_| "<redacted>"))
            .field("database", &self.database)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A redis error. Kept opaque: the message never contains credentials.
#[derive(Debug, thiserror::Error)]
#[error("redis session store error")]
pub struct RedisError;

/// Reads one key. `Ok(None)` means the key does not exist (logged out or
/// expired); `Err` means the store could not be reached or answered an error.
pub async fn get(config: &RedisConfig, key: &str) -> Result<Option<Vec<u8>>, RedisError> {
    tokio::time::timeout(config.timeout, get_inner(config, key))
        .await
        .map_err(|_| RedisError)?
}

async fn get_inner(config: &RedisConfig, key: &str) -> Result<Option<Vec<u8>>, RedisError> {
    let stream = TcpStream::connect((config.host.as_str(), config.port))
        .await
        .map_err(|_| RedisError)?;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    match (&config.user, &config.auth) {
        // ACL auth (`AUTH user password`).
        (Some(user), Some(auth)) => {
            write_command(&mut write, &["AUTH", user.as_str(), auth.as_str()]).await?;
            expect_ok(&mut reader).await?;
        }
        (Some(user), None) => {
            write_command(&mut write, &["AUTH", user.as_str(), ""]).await?;
            expect_ok(&mut reader).await?;
        }
        (None, Some(auth)) => {
            write_command(&mut write, &["AUTH", auth.as_str()]).await?;
            expect_ok(&mut reader).await?;
        }
        (None, None) => {}
    }
    if let Some(database) = config.database {
        let db = database.to_string();
        write_command(&mut write, &["SELECT", db.as_str()]).await?;
        expect_ok(&mut reader).await?;
    }

    write_command(&mut write, &["GET", key]).await?;
    read_reply(&mut reader).await
}

async fn write_command(
    write: &mut (impl AsyncWriteExt + Unpin),
    args: &[&str],
) -> Result<(), RedisError> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    write.write_all(&out).await.map_err(|_| RedisError)?;
    write.flush().await.map_err(|_| RedisError)
}

async fn read_line(
    reader: &mut BufReader<impl AsyncReadExt + Unpin>,
) -> Result<String, RedisError> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.map_err(|_| RedisError)?;
    if n == 0 {
        return Err(RedisError);
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

async fn expect_ok(reader: &mut BufReader<impl AsyncReadExt + Unpin>) -> Result<(), RedisError> {
    let line = read_line(reader).await?;
    match line.as_bytes().first() {
        Some(b'+') => Ok(()),
        _ => Err(RedisError),
    }
}

async fn read_reply(
    reader: &mut BufReader<impl AsyncReadExt + Unpin>,
) -> Result<Option<Vec<u8>>, RedisError> {
    let line = read_line(reader).await?;
    match line.as_bytes().first() {
        Some(b'+') => Ok(Some(line[1..].as_bytes().to_vec())),
        Some(b'$') => {
            let len: i64 = line[1..].parse().map_err(|_| RedisError)?;
            if len < 0 {
                return Ok(None);
            }
            let len = len as usize;
            let mut buf = vec![0u8; len + 2];
            reader.read_exact(&mut buf).await.map_err(|_| RedisError)?;
            buf.truncate(len);
            Ok(Some(buf))
        }
        Some(b':') => Ok(Some(line[1..].as_bytes().to_vec())),
        // `-ERR ...` and arrays (not expected for GET) are failures.
        _ => Err(RedisError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> Option<RedisConfig> {
        RedisConfig::from_url(url, Duration::from_millis(500))
    }

    #[test]
    fn parses_host_port_and_auth() {
        let c = cfg("tcp://nextcloud-redis:6379?auth=s3cret").unwrap();
        assert_eq!(c.host, "nextcloud-redis");
        assert_eq!(c.port, 6379);
        assert_eq!(c.auth.as_deref(), Some("s3cret"));
        assert_eq!(c.database, None);
    }

    #[test]
    fn parses_percent_encoded_auth_and_db() {
        let c = cfg("tcp://127.0.0.1:6380?auth=a%2Bb%2Fc&database=3").unwrap();
        assert_eq!(c.port, 6380);
        assert_eq!(c.auth.as_deref(), Some("a+b/c"));
        assert_eq!(c.database, Some(3));
    }

    #[test]
    fn defaults_to_6379() {
        let c = cfg("tcp://redis").unwrap();
        assert_eq!(c.host, "redis");
        assert_eq!(c.port, 6379);
    }

    #[test]
    fn parses_userinfo_acl_credentials() {
        let c = cfg("redis://alice:s3cret@redis:6380").unwrap();
        assert_eq!(c.host, "redis");
        assert_eq!(c.port, 6380);
        assert_eq!(c.user.as_deref(), Some("alice"));
        assert_eq!(c.auth.as_deref(), Some("s3cret"));
    }

    #[test]
    fn parses_percent_encoded_userinfo_credentials() {
        // Password is `p@ss:w/rd%` and user is `a:b@c` (URL-encoded).
        let c = cfg("redis://a%3Ab%40c:p%40ss%3Aw%2Frd%25@redis:6379").unwrap();
        assert_eq!(c.user.as_deref(), Some("a:b@c"));
        assert_eq!(c.auth.as_deref(), Some("p@ss:w/rd%"));
    }

    #[test]
    fn query_auth_takes_precedence_over_userinfo_password() {
        let c = cfg("redis://alice:userinfo@redis:6379?auth=querysecret").unwrap();
        assert_eq!(c.user.as_deref(), Some("alice"));
        assert_eq!(c.auth.as_deref(), Some("querysecret"));
    }

    #[test]
    fn parses_bracketed_ipv6_host() {
        let c = cfg("redis://[::1]:6380").unwrap();
        assert_eq!(c.host, "::1");
        assert_eq!(c.port, 6380);
    }

    #[test]
    fn rejects_unix_sockets_and_empty_auth() {
        assert!(cfg("unix:///run/redis.sock").is_none());
        assert!(cfg("tcp://redis:6379?auth=").is_none());
        assert!(cfg("tcp://").is_none());
    }

    #[test]
    fn debug_does_not_leak_the_password() {
        let c = cfg("redis://alice:topsecret@redis:6379").unwrap();
        let rendered = format!("{c:?}");
        assert!(
            !rendered.contains("topsecret"),
            "password leaked: {rendered}"
        );
    }
}
