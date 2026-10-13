//! Where the catalogue and the providers are fetched from.

use std::path::{Path, PathBuf};
use std::time::Duration;

use violet_core::{DbError, ErrorKind, Result, is_loopback};

/// Gives up on a stalled download after this long.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A catalogue location: an `https://` URL, an `http://` one on this
/// machine (for development), or a local file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Web(String),
    File(PathBuf),
}

fn error(message: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Provider, message)
}

impl Source {
    /// Parses a location, refusing plain HTTP to anything but loopback:
    /// on a network, someone could swap the catalogue and its digests.
    pub fn parse(location: &str) -> Result<Self> {
        let location = location.trim();
        if let Some(rest) = location.strip_prefix("http://") {
            let host = authority_host(rest);
            return if is_loopback(host) {
                Ok(Self::Web(location.to_owned()))
            } else {
                Err(error(format!(
                    "Refusing to fetch providers over plain HTTP from {host}; use HTTPS"
                )))
            };
        }
        if location.starts_with("https://") {
            return Ok(Self::Web(location.to_owned()));
        }
        if let Some(path) = location.strip_prefix("file://") {
            return Ok(Self::File(PathBuf::from(path)));
        }
        if location.contains("://") {
            return Err(error(format!("Unsupported provider location {location}")));
        }
        Ok(Self::File(PathBuf::from(location)))
    }

    /// `reference` (an entry's `url`) resolved against this location.
    pub fn join(&self, reference: &str) -> Result<Self> {
        if reference.contains("://") {
            let source = Self::parse(reference)?;
            // A web catalogue cannot point into the local disk.
            return match (self, &source) {
                (Self::Web(_), Self::File(_)) => Err(error("A web catalogue named a local file")),
                _ => Ok(source),
            };
        }
        if reference.starts_with('/') || reference.contains("..") || reference.contains('\\') {
            return Err(error(format!("Invalid provider path {reference}")));
        }
        Ok(match self {
            Self::Web(url) => {
                let base = url.split(['?', '#']).next().unwrap_or(url);
                let base = &base[..base.rfind('/').map_or(base.len(), |i| i + 1)];
                Self::Web(format!("{base}{reference}"))
            }
            Self::File(path) => Self::File(
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(reference),
            ),
        })
    }

    /// Reads it whole, refusing more than `limit` bytes. Blocking.
    pub fn read(&self, limit: u64) -> Result<Vec<u8>> {
        match self {
            Self::File(path) => {
                let size = std::fs::metadata(path)
                    .map_err(|e| error(format!("{}: {e}", path.display())))?
                    .len();
                if size > limit {
                    return Err(error(format!("{} is too large", path.display())));
                }
                std::fs::read(path).map_err(|e| error(format!("{}: {e}", path.display())))
            }
            Self::Web(url) => {
                let https = url.starts_with("https://");
                let agent = ureq::Agent::new_with_config(
                    ureq::Agent::config_builder()
                        // Redirects (GitHub sends release assets to its
                        // CDN) must stay on HTTPS.
                        .https_only(https)
                        .timeout_global(Some(TIMEOUT))
                        .user_agent(concat!("Violet/", env!("CARGO_PKG_VERSION")))
                        .build(),
                );
                let mut response = agent
                    .get(url)
                    .call()
                    .map_err(|e| error(format!("Could not download {url}: {e}")))?;
                response
                    .body_mut()
                    .with_config()
                    .limit(limit)
                    .read_to_vec()
                    .map_err(|e| error(format!("Could not download {url}: {e}")))
            }
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Web(url) => f.write_str(url),
            Self::File(path) => path.display().fmt(f),
        }
    }
}

/// The host of `authority/path...`, without user info or port.
fn authority_host(rest: &str) -> &str {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if host.starts_with('[') {
        return host.split(']').next().map_or(host, |h| &host[..=h.len()]);
    }
    host.split(':').next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_only_reaches_this_machine() {
        assert!(Source::parse("https://example.com/providers.json").is_ok());
        assert!(Source::parse("http://localhost:8000/providers.json").is_ok());
        assert!(Source::parse("http://127.0.0.1/p.json").is_ok());
        assert!(Source::parse("http://[::1]:80/p.json").is_ok());
        assert!(Source::parse("http://example.com/p.json").is_err());
        assert!(Source::parse("http://localhost@evil.com/p.json").is_err());
        assert!(Source::parse("http://localhost.evil.com/p.json").is_err());
        assert!(Source::parse("ftp://x/y").is_err());
        assert_eq!(
            Source::parse("/tmp/p.json").ok(),
            Some(Source::File("/tmp/p.json".into()))
        );
    }

    #[test]
    fn entries_resolve_next_to_their_catalogue() {
        let web = Source::parse("https://github.com/o/r/releases/latest/download/providers.json")
            .expect("web");
        assert_eq!(
            web.join("postgres-0.1.0.wasm").ok(),
            Some(Source::Web(
                "https://github.com/o/r/releases/latest/download/postgres-0.1.0.wasm".into()
            ))
        );
        assert!(web.join("https://cdn.example.com/p.wasm").is_ok());
        assert!(web.join("file:///etc/passwd").is_err());
        assert!(web.join("/etc/passwd").is_err());
        assert!(web.join("../x.wasm").is_err());

        let file = Source::parse("/srv/dist/providers.json").expect("file");
        assert_eq!(
            file.join("p.wasm").ok(),
            Some(Source::File("/srv/dist/p.wasm".into()))
        );
    }
}
