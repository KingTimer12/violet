//! The one connection a provider can open, and the rules around it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::bindings::types::{DbError, ErrorKind};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest read a provider can ask for at once.
const MAX_READ: u32 = 1 << 20;

/// What may be sent in plain text before TLS when TLS is required: enough
/// for a protocol's "let's switch to TLS" message (8 bytes for
/// PostgreSQL, 36 for MySQL), never enough for credentials worth having.
const PLAIN_PREAMBLE: usize = 64;

/// Sockets one instance may hold at once.
pub(crate) const MAX_SOCKETS: usize = 4;

/// Sockets one instance may open in its life, so it cannot hammer the
/// server with reconnects.
pub(crate) const MAX_OPENS: usize = 16;

/// The plain text an instance may still send before TLS, over all its
/// sockets together. Per socket, a provider could open one socket after
/// another and send a secret a preamble at a time.
#[derive(Debug)]
pub(crate) struct PlainBudget(AtomicUsize);

impl PlainBudget {
    pub fn new() -> Self {
        Self(AtomicUsize::new(PLAIN_PREAMBLE))
    }

    /// Takes `n` bytes from the budget; `false` (and nothing taken) when
    /// that is more than is left.
    fn spend(&self, n: usize) -> bool {
        self.0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(n)
            })
            .is_ok()
    }
}

/// Where an instance may connect: the profile it serves.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub host: String,
    pub port: u16,
    /// The profile requires TLS: plain text is limited to a preamble.
    pub tls_required: bool,
}

/// Stops an instance: wakes socket waits and traps running code, so
/// closing a session never waits on a server or a loop.
#[derive(Debug, Default)]
pub(crate) struct Kill {
    fired: AtomicBool,
    notify: Notify,
}

impl Kill {
    pub fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    async fn wait(&self) {
        let mut notified = std::pin::pin!(self.notify.notified());
        // Registered before the check, so a fire in between is not missed.
        notified.as_mut().enable();
        if !self.fired() {
            notified.await;
        }
    }
}

enum Transport {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    /// Between states, or after a failed upgrade.
    Closed,
}

/// A socket a provider holds.
pub struct Socket {
    transport: Transport,
    host: String,
    /// The instance's plain-text budget while TLS is required and not
    /// started; `None` when unlimited.
    plain_budget: Option<Arc<PlainBudget>>,
    kill: Arc<Kill>,
}

pub(crate) fn error(kind: ErrorKind, message: impl Into<String>) -> DbError {
    DbError {
        kind,
        message: message.into(),
        code: None,
        detail: None,
        hint: None,
        position: None,
    }
}

fn io_error(cause: &std::io::Error) -> DbError {
    error(ErrorKind::Connection, cause.to_string())
}

fn killed() -> DbError {
    error(ErrorKind::Cancelled, "The session was closed")
}

impl Socket {
    /// Opens TCP to `target`.
    pub(crate) async fn connect(
        target: &Target,
        kill: Arc<Kill>,
        budget: &Arc<PlainBudget>,
    ) -> Result<Self, DbError> {
        if target.host.starts_with('/') {
            return Err(error(
                ErrorKind::Connection,
                "The host is a socket directory; use connect-local",
            ));
        }
        let host = target
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let connect = TcpStream::connect((host.as_str(), target.port));
        let stream = tokio::select! {
            result = tokio::time::timeout(CONNECT_TIMEOUT, connect) => match result {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    return Err(error(
                        ErrorKind::Connection,
                        format!("Could not connect to {}:{}: {e}", target.host, target.port),
                    ));
                }
                Err(_) => {
                    return Err(error(
                        ErrorKind::Connection,
                        format!("Timed out connecting to {}:{}", target.host, target.port),
                    ));
                }
            },
            () = kill.wait() => return Err(killed()),
        };
        let _ = stream.set_nodelay(true);
        Ok(Self {
            transport: Transport::Tcp(stream),
            host,
            plain_budget: target.tls_required.then(|| Arc::clone(budget)),
            kill,
        })
    }

    /// Opens the socket file `name` inside the target's directory.
    pub(crate) async fn connect_local(
        target: &Target,
        name: &str,
        kill: Arc<Kill>,
    ) -> Result<Self, DbError> {
        let path = local_path(&target.host, name)?;
        #[cfg(unix)]
        {
            let connect = tokio::net::UnixStream::connect(&path);
            let stream = tokio::select! {
                result = tokio::time::timeout(CONNECT_TIMEOUT, connect) => match result {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        return Err(error(
                            ErrorKind::Connection,
                            format!("Could not connect to {}: {e}", path.display()),
                        ));
                    }
                    Err(_) => return Err(error(ErrorKind::Connection, "Timed out connecting")),
                },
                () = kill.wait() => return Err(killed()),
            };
            Ok(Self {
                transport: Transport::Unix(stream),
                host: target.host.clone(),
                // Never leaves the machine.
                plain_budget: None,
                kill,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (path, kill);
            Err(error(
                ErrorKind::Unsupported,
                "Socket files are not supported on this system",
            ))
        }
    }

    pub(crate) async fn read(&mut self, max: u32) -> Result<Vec<u8>, DbError> {
        let mut buffer = vec![0; max.clamp(1, MAX_READ) as usize];
        let kill = Arc::clone(&self.kill);
        let read = async {
            match &mut self.transport {
                Transport::Tcp(stream) => stream.read(&mut buffer).await,
                Transport::Tls(stream) => stream.read(&mut buffer).await,
                #[cfg(unix)]
                Transport::Unix(stream) => stream.read(&mut buffer).await,
                Transport::Closed => Ok(0),
            }
        };
        let n = tokio::select! {
            result = read => result.map_err(|e| io_error(&e))?,
            () = kill.wait() => return Err(killed()),
        };
        buffer.truncate(n);
        Ok(buffer)
    }

    pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<(), DbError> {
        if let Some(budget) = &self.plain_budget
            && !budget.spend(bytes.len())
        {
            self.transport = Transport::Closed;
            return Err(error(
                ErrorKind::Tls,
                "Refused to send data in plain text: this connection requires TLS",
            ));
        }
        let kill = Arc::clone(&self.kill);
        let write = async {
            match &mut self.transport {
                Transport::Tcp(stream) => stream.write_all(bytes).await,
                Transport::Tls(stream) => {
                    stream.write_all(bytes).await?;
                    stream.flush().await
                }
                #[cfg(unix)]
                Transport::Unix(stream) => stream.write_all(bytes).await,
                Transport::Closed => Err(std::io::ErrorKind::NotConnected.into()),
            }
        };
        tokio::select! {
            result = write => result.map_err(|e| io_error(&e)),
            () = kill.wait() => Err(killed()),
        }
    }

    pub(crate) async fn start_tls(&mut self) -> Result<(), DbError> {
        let Transport::Tcp(_) = &self.transport else {
            return Err(error(
                ErrorKind::Tls,
                "TLS can only start once, on a TCP connection",
            ));
        };
        let Transport::Tcp(stream) = std::mem::replace(&mut self.transport, Transport::Closed)
        else {
            unreachable!("checked above");
        };
        let name = ServerName::try_from(self.host.clone())
            .map_err(|e| error(ErrorKind::Tls, format!("Invalid host name for TLS: {e}")))?;
        let connector = TlsConnector::from(tls_config()?);
        let kill = Arc::clone(&self.kill);
        let stream = tokio::select! {
            result = connector.connect(name, stream) => {
                result.map_err(|e| error(ErrorKind::Tls, format!("TLS handshake failed: {e}")))?
            }
            () = kill.wait() => return Err(killed()),
        };
        self.transport = Transport::Tls(Box::new(stream));
        self.plain_budget = None;
        Ok(())
    }

    pub(crate) fn is_tls(&self) -> bool {
        matches!(self.transport, Transport::Tls(_))
    }
}

/// `name` inside the socket directory `dir`, refusing anything that could
/// step out of it.
fn local_path(dir: &str, name: &str) -> Result<PathBuf, DbError> {
    if !dir.starts_with('/') {
        return Err(error(
            ErrorKind::Connection,
            "The host is not a socket directory",
        ));
    }
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && name.len() <= 100;
    if !plain {
        return Err(error(
            ErrorKind::Connection,
            format!("Invalid socket name {name:?}"),
        ));
    }
    Ok(Path::new(dir).join(name))
}

/// Verifies servers against the web PKI roots. Built once.
fn tls_config() -> Result<Arc<ClientConfig>, DbError> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map(|builder| {
                    Arc::new(builder.with_root_certificates(roots).with_no_client_auth())
                })
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|message| error(ErrorKind::Tls, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plain_text_budget_is_shared_and_never_refilled() {
        let budget = PlainBudget::new();
        assert!(budget.spend(8), "an SSLRequest");
        assert!(budget.spend(PLAIN_PREAMBLE - 8));
        // Another socket of the same instance gets nothing more.
        assert!(!budget.spend(1));
        assert!(budget.spend(0));
        assert!(!PlainBudget::new().spend(PLAIN_PREAMBLE + 1));
    }

    #[test]
    fn socket_names_stay_in_their_directory() {
        assert_eq!(
            local_path("/var/run/postgresql", ".s.PGSQL.5432").ok(),
            Some(PathBuf::from("/var/run/postgresql/.s.PGSQL.5432"))
        );
        for name in ["", ".", "..", "../etc/passwd", "a/b", "a\\b", "a\0"] {
            assert!(local_path("/tmp", name).is_err(), "{name:?}");
        }
        assert!(local_path("db.example.com", "x").is_err());
    }
}
