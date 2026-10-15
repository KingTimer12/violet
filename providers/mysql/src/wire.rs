//! The MySQL client/server protocol over Violet's socket: packet framing,
//! the handshake, TLS and authentication.

use violet_sdk::net::Connection;
use violet_sdk::{ConnectParams, DbError, ErrorKind, Result, SslMode};

use crate::auth;
use crate::packet::{Reader, put_lenenc, put_lenenc_bytes};

/// A payload this long continues in the next packet.
pub(crate) const MAX_PAYLOAD: usize = 0xFF_FFFF;

/// Capability flags.
pub(crate) mod caps {
    pub const LONG_PASSWORD: u32 = 1;
    pub const LONG_FLAG: u32 = 1 << 2;
    pub const CONNECT_WITH_DB: u32 = 1 << 3;
    pub const PROTOCOL_41: u32 = 1 << 9;
    pub const SSL: u32 = 1 << 11;
    pub const TRANSACTIONS: u32 = 1 << 13;
    pub const SECURE_CONNECTION: u32 = 1 << 15;
    pub const MULTI_STATEMENTS: u32 = 1 << 16;
    pub const MULTI_RESULTS: u32 = 1 << 17;
    pub const PS_MULTI_RESULTS: u32 = 1 << 18;
    pub const PLUGIN_AUTH: u32 = 1 << 19;
    pub const CONNECT_ATTRS: u32 = 1 << 20;
    pub const PLUGIN_AUTH_LENENC_DATA: u32 = 1 << 21;
    pub const DEPRECATE_EOF: u32 = 1 << 24;
}

/// `SERVER_MORE_RESULTS_EXISTS`: another result follows this one.
pub(crate) const MORE_RESULTS: u16 = 1 << 3;

/// `utf8mb4_general_ci`, which every server since 5.5.3 knows.
const UTF8MB4: u8 = 45;

const MAX_PACKET: u32 = 1 << 30;

/// Commands.
pub(crate) const COM_QUIT: u8 = 0x01;
pub(crate) const COM_QUERY: u8 = 0x03;

/// Socket files a local server usually listens on, inside the profile's
/// directory: Debian's and Homebrew's names.
const SOCKETS: &[&str] = &["mysqld.sock", "mysql.sock"];

/// What the server says first.
#[derive(Debug)]
pub(crate) struct Greeting {
    pub version: String,
    pub connection_id: u32,
    pub capabilities: u32,
    pub nonce: Vec<u8>,
    pub plugin: String,
}

/// Packets to and from the server.
pub(crate) struct Wire {
    connection: Connection,
    sequence: u8,
    /// What both sides agreed on.
    capabilities: u32,
    /// TLS or a local socket: the password may travel as is.
    secure: bool,
}

impl Wire {
    /// Connects, settles TLS and authenticates. With `Prefer`, a server
    /// whose TLS fails is tried again in plain text.
    pub(crate) fn connect(params: &ConnectParams) -> Result<(Self, Greeting)> {
        match Self::attempt(params, params.ssl) {
            Err(error) if params.ssl == SslMode::Prefer && error.kind == ErrorKind::Tls => {
                log::warn!("TLS failed ({}); connecting in plain text", error.message);
                Self::attempt(params, SslMode::Disable)
            }
            other => other,
        }
    }

    fn attempt(params: &ConnectParams, ssl: SslMode) -> Result<(Self, Greeting)> {
        let local = params.host.starts_with('/');
        let connection = if local {
            open_local()?
        } else {
            Connection::open()?
        };
        let mut wire = Self {
            connection,
            sequence: 0,
            capabilities: 0,
            secure: local,
        };
        let greeting = wire.greeting()?;
        wire.handshake(
            params,
            &greeting,
            if local { SslMode::Disable } else { ssl },
        )?;
        Ok((wire, greeting))
    }

    fn greeting(&mut self) -> Result<Greeting> {
        let packet = self.read_packet()?;
        if packet.first() == Some(&0xFF) {
            return Err(server_error(&packet));
        }
        let mut r = Reader::new(&packet);
        if r.u8()? != 10 {
            return Err(DbError::unsupported(
                "The server speaks a protocol older than MySQL 4.1",
            ));
        }
        let version = r.nul_text();
        let connection_id = r.u32()?;
        let mut nonce = r.bytes(8)?.to_vec();
        r.skip(1)?;
        let mut capabilities = u32::from(r.u16()?);
        let mut plugin = String::new();
        if !r.is_empty() {
            let _charset = r.u8()?;
            let _status = r.u16()?;
            capabilities |= u32::from(r.u16()?) << 16;
            let nonce_len = usize::from(r.u8()?);
            r.skip(10)?;
            if capabilities & caps::SECURE_CONNECTION != 0 {
                let n = nonce_len.saturating_sub(8).max(13);
                let part = r.bytes(n.min(packet.len() - r.position()))?;
                nonce.extend_from_slice(part);
                // The second part ends with a NUL that is not nonce.
                while nonce.last() == Some(&0) {
                    nonce.pop();
                }
            }
            if capabilities & caps::PLUGIN_AUTH != 0 {
                plugin = r.nul_text();
            }
        }
        if capabilities & caps::PROTOCOL_41 == 0 || capabilities & caps::SECURE_CONNECTION == 0 {
            return Err(DbError::unsupported(
                "The server is older than MySQL 4.1, which Violet does not support",
            ));
        }
        if plugin.is_empty() {
            plugin = auth::NATIVE.to_owned();
        }
        Ok(Greeting {
            version,
            connection_id,
            capabilities,
            nonce,
            plugin,
        })
    }

    fn handshake(
        &mut self,
        params: &ConnectParams,
        greeting: &Greeting,
        ssl: SslMode,
    ) -> Result<()> {
        let mut wanted = caps::LONG_PASSWORD
            | caps::LONG_FLAG
            | caps::PROTOCOL_41
            | caps::TRANSACTIONS
            | caps::SECURE_CONNECTION
            | caps::MULTI_STATEMENTS
            | caps::MULTI_RESULTS
            | caps::PS_MULTI_RESULTS
            | caps::PLUGIN_AUTH
            | caps::CONNECT_ATTRS
            | caps::PLUGIN_AUTH_LENENC_DATA
            | caps::DEPRECATE_EOF;
        let database = params.database.trim();
        if !database.is_empty() {
            wanted |= caps::CONNECT_WITH_DB;
        }
        let tls = match ssl {
            SslMode::Disable => false,
            _ if greeting.capabilities & caps::SSL != 0 => true,
            SslMode::Prefer => false,
            SslMode::Require => {
                return Err(DbError::tls(
                    "The server does not support TLS, which this connection requires",
                ));
            }
        };
        if tls {
            wanted |= caps::SSL;
        }
        self.capabilities = wanted & greeting.capabilities;

        if tls {
            // SSLRequest: the start of the handshake response, 32 bytes.
            let mut request = Vec::with_capacity(32);
            self.put_header(&mut request);
            self.write_packet(&request)?;
            self.connection.start_tls()?;
            self.secure = true;
        }

        let response = auth::answer(
            &greeting.plugin,
            params.password.as_deref(),
            &greeting.nonce,
            self.secure,
        )?;
        let mut packet = Vec::with_capacity(128);
        self.put_header(&mut packet);
        packet.extend_from_slice(params.username.as_bytes());
        packet.push(0);
        if self.capabilities & caps::PLUGIN_AUTH_LENENC_DATA != 0 {
            put_lenenc_bytes(&mut packet, &response);
        } else {
            let n = u8::try_from(response.len())
                .map_err(|_| DbError::protocol("The authentication answer is too long"))?;
            packet.push(n);
            packet.extend_from_slice(&response);
        }
        if self.capabilities & caps::CONNECT_WITH_DB != 0 {
            packet.extend_from_slice(database.as_bytes());
            packet.push(0);
        }
        if self.capabilities & caps::PLUGIN_AUTH != 0 {
            packet.extend_from_slice(greeting.plugin.as_bytes());
            packet.push(0);
        }
        if self.capabilities & caps::CONNECT_ATTRS != 0 {
            let mut attributes = Vec::new();
            for (key, value) in [
                ("_client_name", "violet"),
                ("_client_version", env!("CARGO_PKG_VERSION")),
                ("program_name", "Violet"),
            ] {
                put_lenenc_bytes(&mut attributes, key.as_bytes());
                put_lenenc_bytes(&mut attributes, value.as_bytes());
            }
            put_lenenc(&mut packet, attributes.len() as u64);
            packet.extend_from_slice(&attributes);
        }
        self.write_packet(&packet)?;
        self.authenticate(params, greeting)
    }

    /// Capabilities, maximum packet, character set and 23 zeros.
    fn put_header(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.capabilities.to_le_bytes());
        out.extend_from_slice(&MAX_PACKET.to_le_bytes());
        out.push(UTF8MB4);
        out.extend_from_slice(&[0; 23]);
    }

    /// Follows the server through method switches and extra rounds until
    /// it accepts or refuses.
    fn authenticate(&mut self, params: &ConnectParams, greeting: &Greeting) -> Result<()> {
        let password = params.password.as_deref().unwrap_or_default();
        let mut plugin = greeting.plugin.clone();
        let mut nonce = greeting.nonce.clone();
        // Rounds are few; a server that never settles is not waited on.
        for _ in 0..8 {
            let packet = self.read_packet()?;
            match packet.first() {
                Some(0x00) => return Ok(()),
                Some(0xFF) => return Err(server_error(&packet)),
                Some(0xFE) => {
                    if packet.len() == 1 {
                        return Err(DbError::unsupported(
                            "The server asks for the pre-4.1 password method, \
                             which Violet does not support",
                        ));
                    }
                    let mut r = Reader::new(&packet[1..]);
                    plugin = r.nul_text();
                    nonce = r.rest().to_vec();
                    while nonce.last() == Some(&0) {
                        nonce.pop();
                    }
                    let answer = auth::answer(&plugin, Some(password), &nonce, self.secure)?;
                    self.write_packet(&answer)?;
                }
                Some(0x01) => {
                    let data = &packet[1..];
                    match (plugin.as_str(), data) {
                        (auth::CACHING_SHA2, [auth::FAST_AUTH_OK]) => {}
                        (auth::CACHING_SHA2, [auth::FULL_AUTH]) if self.secure => {
                            self.write_packet(&auth::cleartext(password))?;
                        }
                        (auth::CACHING_SHA2, [auth::FULL_AUTH]) => {
                            self.write_packet(&[auth::REQUEST_CACHING_SHA2_KEY])?;
                        }
                        // The public key Violet asked for.
                        (auth::CACHING_SHA2 | auth::SHA256, pem) if !self.secure => {
                            let encrypted = auth::encrypt(password, &nonce, pem)?;
                            self.write_packet(&encrypted)?;
                        }
                        _ => {
                            return Err(DbError::protocol(
                                "Unexpected message while authenticating",
                            ));
                        }
                    }
                }
                _ => return Err(DbError::protocol("Unexpected message while authenticating")),
            }
        }
        Err(DbError::protocol("Authentication did not finish"))
    }

    /// Whether result sets end with an OK packet instead of an EOF one.
    pub(crate) fn deprecate_eof(&self) -> bool {
        self.capabilities & caps::DEPRECATE_EOF != 0
    }

    /// Starts a command: a new sequence, then the command byte and `body`.
    pub(crate) fn command(&mut self, command: u8, body: &[u8]) -> Result<()> {
        self.sequence = 0;
        let mut payload = Vec::with_capacity(body.len() + 1);
        payload.push(command);
        payload.extend_from_slice(body);
        self.write_packet(&payload)
    }

    /// Sends `payload`, split into packets of at most 16 MiB.
    pub(crate) fn write_packet(&mut self, payload: &[u8]) -> Result<()> {
        let mut out = Vec::with_capacity(payload.len() + 4 * (payload.len() / MAX_PAYLOAD + 1));
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_PAYLOAD);
            out.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
            out.push(self.sequence);
            self.sequence = self.sequence.wrapping_add(1);
            out.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            // A payload of exactly 16 MiB ends with an empty packet.
            if n < MAX_PAYLOAD {
                break;
            }
        }
        self.connection.send(&out)
    }

    /// The next payload, joined from as many packets as it spans.
    pub(crate) fn read_packet(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.connection.receive_exact(&mut header)?;
            let n = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            self.sequence = header[3].wrapping_add(1);
            let start = payload.len();
            payload.resize(start + n, 0);
            self.connection.receive_exact(&mut payload[start..])?;
            if n < MAX_PAYLOAD {
                return Ok(payload);
            }
        }
    }

    /// Says goodbye, best effort.
    pub(crate) fn quit(&mut self) {
        let _ = self.command(COM_QUIT, &[]);
    }
}

fn open_local() -> Result<Connection> {
    let mut failure = None;
    for name in SOCKETS {
        match Connection::open_local(name) {
            Ok(connection) => return Ok(connection),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| DbError::connection("No MySQL socket in that folder")))
}

/// An ERR packet in Violet's shape.
pub(crate) fn server_error(packet: &[u8]) -> DbError {
    let mut r = Reader::new(packet.get(1..).unwrap_or_default());
    let Ok(code) = r.u16() else {
        return DbError::query("The server reported an error");
    };
    let mut state = None;
    if r.peek() == Some(b'#')
        && let Ok(bytes) = r.bytes(6)
    {
        state = Some(String::from_utf8_lossy(&bytes[1..]).into_owned());
    }
    let mut message = String::from_utf8_lossy(r.rest()).into_owned();
    if message.is_empty() {
        message = "The server reported an error".into();
    }
    let kind = match code {
        // ER_ACCESS_DENIED_ERROR, ER_ACCESS_DENIED_NO_PASSWORD_ERROR,
        // ER_NOT_SUPPORTED_AUTH_MODE, ER_MUST_CHANGE_PASSWORD_LOGIN
        1045 | 1698 | 1251 | 1862 => ErrorKind::Authentication,
        // ER_QUERY_INTERRUPTED
        1317 => ErrorKind::Cancelled,
        _ => ErrorKind::Query,
    };
    let mut error = DbError::new(kind, message).with_code(code.to_string());
    error.detail = state.map(|state| format!("SQLSTATE {state}"));
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_carry_code_and_state() {
        let mut packet = vec![0xFF];
        packet.extend_from_slice(&1045u16.to_le_bytes());
        packet.extend_from_slice(b"#28000Access denied for user 'x'");
        let error = server_error(&packet);
        assert_eq!(error.kind, ErrorKind::Authentication);
        assert_eq!(error.code.as_deref(), Some("1045"));
        assert_eq!(error.detail.as_deref(), Some("SQLSTATE 28000"));
        assert_eq!(error.message, "Access denied for user 'x'");

        // Before the handshake, errors have no state.
        let mut packet = vec![0xFF];
        packet.extend_from_slice(&1130u16.to_le_bytes());
        packet.extend_from_slice(b"Host is not allowed");
        let error = server_error(&packet);
        assert_eq!(error.kind, ErrorKind::Query);
        assert_eq!(error.detail, None);
        assert_eq!(error.message, "Host is not allowed");
    }
}
