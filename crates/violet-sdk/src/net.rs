//! The one connection a provider may open: to the profile's server.
//!
//! Violet opens the socket and does TLS itself, so a provider never holds
//! certificates or chooses whom to trust: [`Connection::start_tls`]
//! verifies the server against the web PKI roots for the host the user
//! typed. When TLS is required, Violet will not send more than a short
//! preamble in plain text, so a provider cannot leak a password by
//! forgetting to upgrade.

use std::io;

use crate::DbError;
use crate::bindings::violet::provider::network::{self, Socket};

/// How much one read asks the host for.
const CHUNK: u32 = 64 * 1024;

/// A buffered byte stream to the database server. Implements
/// [`io::Read`] and [`io::Write`]; their errors carry a [`DbError`] that
/// `?` turns back into one.
pub struct Connection {
    socket: Socket,
    buffer: Vec<u8>,
    /// How much of `buffer` was consumed.
    read: usize,
}

impl Connection {
    /// Connects to the profile's host and port over TCP.
    pub fn open() -> Result<Self, DbError> {
        Ok(Self::new(network::connect()?))
    }

    /// Connects to the socket file `name` in the profile's host directory,
    /// when the host is one (an absolute path, such as
    /// `/var/run/postgresql`).
    pub fn open_local(name: &str) -> Result<Self, DbError> {
        Ok(Self::new(network::connect_local(name)?))
    }

    fn new(socket: Socket) -> Self {
        Self {
            socket,
            buffer: Vec::new(),
            read: 0,
        }
    }

    /// Upgrades to TLS.
    ///
    /// Refused if the server already sent bytes past the point of the
    /// upgrade: they arrived in plain text, and taking them as part of the
    /// encrypted session would let anyone on the path inject them
    /// (CVE-2021-23214 and friends).
    pub fn start_tls(&mut self) -> Result<(), DbError> {
        if self.read < self.buffer.len() {
            return Err(DbError::tls(
                "The server sent unexpected data before the TLS handshake",
            ));
        }
        self.buffer.clear();
        self.read = 0;
        self.socket.start_tls()
    }

    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.socket.is_tls()
    }

    /// Writes all of `bytes`.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), DbError> {
        self.socket.write(bytes)
    }

    /// Bytes received and not consumed yet, reading more when there are
    /// none. Empty only at the end of the stream.
    pub fn fill(&mut self) -> Result<&[u8], DbError> {
        if self.read == self.buffer.len() {
            self.buffer = self.socket.read(CHUNK)?;
            self.read = 0;
        }
        Ok(&self.buffer[self.read..])
    }

    /// Marks `n` bytes of what [`Connection::fill`] returned as used.
    pub fn consume(&mut self, n: usize) {
        self.read = (self.read + n).min(self.buffer.len());
    }

    /// Reads exactly `out.len()` bytes.
    pub fn receive_exact(&mut self, out: &mut [u8]) -> Result<(), DbError> {
        let mut filled = 0;
        while filled < out.len() {
            let available = self.fill()?;
            if available.is_empty() {
                return Err(DbError::connection("The server closed the connection"));
            }
            let n = available.len().min(out.len() - filled);
            out[filled..filled + n].copy_from_slice(&available[..n]);
            self.consume(n);
            filled += n;
        }
        Ok(())
    }
}

fn io_error(error: DbError) -> io::Error {
    io::Error::other(error)
}

impl io::Read for Connection {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill().map_err(io_error)?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl io::BufRead for Connection {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.fill().map_err(io_error)
    }

    fn consume(&mut self, n: usize) {
        Connection::consume(self, n);
    }
}

impl io::Write for Connection {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.send(bytes).map_err(io_error)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
