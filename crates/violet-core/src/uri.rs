//! Connection strings such as `rediss://user:pass@host:6380/2` or
//! `postgresql://user@host/db?sslmode=require`, split into a profile's
//! fields. Violet parses them itself, so pasting one works before the
//! database's provider is installed.

use crate::profile::SslMode;

/// What a connection string says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedUri {
    /// The provider it is for, such as `redis`.
    pub driver: String,
    pub host: String,
    /// `None` for the provider's default.
    pub port: Option<u16>,
    pub database: String,
    pub username: String,
    pub password: Option<String>,
    pub ssl: Option<SslMode>,
}

/// The provider each scheme is for, and whether the scheme means TLS.
const SCHEMES: &[(&str, &str, bool)] = &[
    ("redis", "redis", false),
    ("rediss", "redis", true),
    ("valkey", "redis", false),
    ("valkeys", "redis", true),
    ("dragonfly", "redis", false),
    ("postgres", "postgres", false),
    ("postgresql", "postgres", false),
    ("mysql", "mysql", false),
    ("mariadb", "mysql", false),
];

fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = text
                .get(at + 1..at + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or_else(|| format!("{text:?} has a broken %-escape"))?;
            out.push(hex);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("{text:?} is not UTF-8 once decoded"))
}

/// Splits a connection string.
///
/// # Errors
///
/// A scheme Violet has no provider for, or a malformed string.
pub fn parse(text: &str) -> Result<ParsedUri, String> {
    let text = text.trim();
    let (scheme, rest) = text
        .split_once("://")
        .ok_or("A connection string starts with a scheme, such as redis://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme.ends_with("+srv") {
        return Err("DNS seed lists (+srv) are not supported yet: use one host".to_owned());
    }
    let &(_, driver, tls) = SCHEMES
        .iter()
        .find(|(name, ..)| *name == scheme)
        .ok_or_else(|| format!("Violet has no provider for {scheme}:// connection strings"))?;
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (credentials, address) = match authority.rsplit_once('@') {
        Some((credentials, address)) => (Some(credentials), address),
        None => (None, authority),
    };
    let mut parsed = ParsedUri {
        driver: driver.to_owned(),
        ..ParsedUri::default()
    };
    if let Some(credentials) = credentials {
        let (user, password) = match credentials.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (credentials, None),
        };
        parsed.username = decode(user)?;
        parsed.password = password.map(decode).transpose()?;
    }
    if address.contains(',') {
        return Err("Several hosts are not supported yet: use one".to_owned());
    }
    let (host, port) = if let Some(inner) = address.strip_prefix('[') {
        let (host, after) = inner
            .split_once(']')
            .ok_or("An IPv6 address needs its closing ]")?;
        (host, after.strip_prefix(':'))
    } else {
        match address.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        }
    };
    parsed.host = decode(host)?;
    if parsed.host.is_empty() {
        "localhost".clone_into(&mut parsed.host);
    }
    parsed.port = match port.filter(|p| !p.is_empty()) {
        Some(port) => Some(
            port.parse()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("{port:?} is not a port"))?,
        ),
        None => None,
    };
    let database = decode(path.trim_end_matches('/'))?;
    parsed.database = if driver == "redis" && !database.is_empty() {
        let index: u32 = database
            .parse()
            .map_err(|_| format!("{database:?} is not a Redis database number"))?;
        format!("db{index}")
    } else {
        database
    };
    if tls {
        parsed.ssl = Some(SslMode::Require);
    }
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode(value)?;
        match key.to_ascii_lowercase().as_str() {
            "sslmode" | "ssl-mode" => {
                parsed.ssl = Some(match value.to_ascii_lowercase().as_str() {
                    "disable" | "disabled" => SslMode::Disable,
                    "prefer" | "preferred" | "allow" => SslMode::Prefer,
                    _ => SslMode::Require,
                });
            }
            "ssl" | "tls" => {
                parsed.ssl = Some(if matches!(value.as_str(), "true" | "1") {
                    SslMode::Require
                } else {
                    SslMode::Disable
                });
            }
            "db" if driver == "redis" => {
                let index: u32 = value
                    .parse()
                    .map_err(|_| format!("{value:?} is not a Redis database number"))?;
                parsed.database = format!("db{index}");
            }
            "user" | "username" if parsed.username.is_empty() => parsed.username = value,
            "password" if parsed.password.is_none() => parsed.password = Some(value),
            _ => {}
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_strings_split_into_fields() {
        let parsed = parse("rediss://default:p%40ss@cache.example.com:6380/2").expect("parsed");
        assert_eq!(
            parsed,
            ParsedUri {
                driver: "redis".into(),
                host: "cache.example.com".into(),
                port: Some(6380),
                database: "db2".into(),
                username: "default".into(),
                password: Some("p@ss".into()),
                ssl: Some(SslMode::Require),
            }
        );
        // A password alone, as Redis's own clients write it.
        let parsed = parse("redis://:secret@localhost").expect("parsed");
        assert_eq!(
            (parsed.username.as_str(), parsed.password.as_deref()),
            ("", Some("secret"))
        );
        assert_eq!(parsed.port, None);
        assert_eq!(parse("redis://[::1]:7000").expect("ipv6").host, "::1");
        assert!(parse("redis://h/x").is_err());
    }

    #[test]
    fn other_schemes_and_refusals() {
        let parsed = parse("postgresql://me@db.local/shop?sslmode=require").expect("pg");
        assert_eq!(parsed.driver, "postgres");
        assert_eq!(parsed.database, "shop");
        assert_eq!(parsed.ssl, Some(SslMode::Require));
        assert!(parse("mongodb+srv://x/y").is_err());
        assert!(parse("ftp://x").is_err());
        assert!(parse("localhost:6379").is_err());
        assert!(parse("redis://a,b/0").is_err());
    }
}
