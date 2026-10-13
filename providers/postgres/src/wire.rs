//! The PostgreSQL wire protocol over Violet's socket: framing, the
//! startup handshake and authentication.

use bytes::BytesMut;
use fallible_iterator::FallibleIterator as _;
use postgres_protocol::authentication::{md5_hash, sasl};
use postgres_protocol::message::backend::{ErrorResponseBody, Message};
use postgres_protocol::message::frontend;
use violet_sdk::net::Connection;
use violet_sdk::{ConnectParams, DbError, ErrorKind, Result, SslMode};

/// The backend's key, which cancels its running statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BackendKey {
    pub process_id: i32,
    pub secret: i32,
}

impl BackendKey {
    pub(crate) fn to_ticket(self) -> Vec<u8> {
        let mut ticket = self.process_id.to_be_bytes().to_vec();
        ticket.extend_from_slice(&self.secret.to_be_bytes());
        ticket
    }

    pub(crate) fn from_ticket(ticket: &[u8]) -> Option<Self> {
        let (pid, secret) = ticket.split_first_chunk::<4>()?;
        let secret: [u8; 4] = secret.try_into().ok()?;
        Some(Self {
            process_id: i32::from_be_bytes(*pid),
            secret: i32::from_be_bytes(secret),
        })
    }
}

/// Framed messages to and from the server.
pub(crate) struct Wire {
    connection: Connection,
    incoming: BytesMut,
    outgoing: BytesMut,
}

impl Wire {
    /// Opens the socket and settles TLS as `params.ssl` says.
    pub(crate) fn open(params: &ConnectParams) -> Result<Self> {
        // A host that is a directory means PostgreSQL's Unix socket there,
        // where TLS does not apply.
        let local = params.host.starts_with('/');
        let mut connection = if local {
            Connection::open_local(&format!(".s.PGSQL.{}", params.port))?
        } else {
            Connection::open()?
        };
        if !local && params.ssl != SslMode::Disable {
            let mut request = BytesMut::new();
            frontend::ssl_request(&mut request);
            connection.send(&request)?;
            let mut answer = [0];
            connection.receive_exact(&mut answer)?;
            match answer[0] {
                b'S' => connection.start_tls()?,
                b'N' if params.ssl == SslMode::Prefer => {}
                b'N' => {
                    return Err(DbError::tls(
                        "The server does not support TLS, which this connection requires",
                    ));
                }
                _ => return Err(DbError::protocol("Unexpected answer to the TLS request")),
            }
        }
        Ok(Self {
            connection,
            incoming: BytesMut::with_capacity(64 * 1024),
            outgoing: BytesMut::with_capacity(4 * 1024),
        })
    }

    /// Opens a session: startup, authentication, and the server's
    /// greeting. Returns the backend key and the server version.
    pub(crate) fn startup(
        &mut self,
        params: &ConnectParams,
    ) -> Result<(Option<BackendKey>, String)> {
        let mut startup = vec![
            ("user", params.username.as_str()),
            ("application_name", "Violet"),
            ("client_encoding", "UTF8"),
        ];
        if !params.database.trim().is_empty() {
            startup.push(("database", params.database.as_str()));
        }
        frontend::startup_message(startup, &mut self.outgoing).map_err(protocol)?;
        self.flush()?;
        self.authenticate(params)?;

        let mut key = None;
        let mut version = String::new();
        loop {
            match self.next()? {
                Message::BackendKeyData(body) => {
                    key = Some(BackendKey {
                        process_id: body.process_id(),
                        secret: body.secret_key(),
                    });
                }
                Message::ParameterStatus(body) => {
                    if body.name().map_err(protocol)? == "server_version" {
                        version = body.value().map_err(protocol)?.to_owned();
                    }
                }
                Message::ReadyForQuery(_) => return Ok((key, version)),
                Message::ErrorResponse(body) => return Err(server_error(&body)),
                _ => {}
            }
        }
    }

    fn authenticate(&mut self, params: &ConnectParams) -> Result<()> {
        let password = || {
            params
                .password
                .as_deref()
                .ok_or_else(|| DbError::authentication("The server asks for a password"))
        };
        let mut scram: Option<sasl::ScramSha256> = None;
        let mut verified = false;
        loop {
            match self.next()? {
                // A server that skipped proving it knows the password too
                // is not trusted.
                Message::AuthenticationOk if scram.is_some() && !verified => {
                    return Err(DbError::authentication(
                        "The server did not complete SCRAM authentication",
                    ));
                }
                Message::AuthenticationOk => return Ok(()),
                Message::AuthenticationCleartextPassword => {
                    frontend::password_message(password()?.as_bytes(), &mut self.outgoing)
                        .map_err(protocol)?;
                }
                Message::AuthenticationMd5Password(body) => {
                    let hash = md5_hash(
                        params.username.as_bytes(),
                        password()?.as_bytes(),
                        body.salt(),
                    );
                    frontend::password_message(hash.as_bytes(), &mut self.outgoing)
                        .map_err(protocol)?;
                }
                Message::AuthenticationSasl(body) => {
                    let mut mechanisms = body.mechanisms();
                    let mut offered = false;
                    while let Some(mechanism) = mechanisms.next().map_err(protocol)? {
                        offered |= mechanism == sasl::SCRAM_SHA_256;
                    }
                    if !offered {
                        return Err(DbError::unsupported(
                            "The server offers no authentication method Violet supports",
                        ));
                    }
                    // Channel binding is not offered: claiming support
                    // without it would read as a downgrade to the server.
                    let state = sasl::ScramSha256::new(
                        password()?.as_bytes(),
                        sasl::ChannelBinding::unsupported(),
                    );
                    frontend::sasl_initial_response(
                        sasl::SCRAM_SHA_256,
                        state.message(),
                        &mut self.outgoing,
                    )
                    .map_err(protocol)?;
                    scram = Some(state);
                }
                Message::AuthenticationSaslContinue(body) => {
                    let state = scram
                        .as_mut()
                        .ok_or_else(|| DbError::protocol("SCRAM out of order"))?;
                    state.update(body.data()).map_err(authentication)?;
                    frontend::sasl_response(state.message(), &mut self.outgoing)
                        .map_err(protocol)?;
                }
                Message::AuthenticationSaslFinal(body) => {
                    // Checks the server's signature: it knew the password
                    // too, so it is not an impostor.
                    scram
                        .as_mut()
                        .ok_or_else(|| DbError::protocol("SCRAM out of order"))?
                        .finish(body.data())
                        .map_err(authentication)?;
                    verified = true;
                    continue;
                }
                Message::ErrorResponse(body) => return Err(server_error(&body)),
                Message::AuthenticationKerberosV5
                | Message::AuthenticationScmCredential
                | Message::AuthenticationGss
                | Message::AuthenticationSspi => {
                    return Err(DbError::unsupported(
                        "The server asks for an authentication method Violet does not support",
                    ));
                }
                _ => return Err(DbError::protocol("Unexpected message while authenticating")),
            }
            self.flush()?;
        }
    }

    /// Queues a message built by `write`.
    pub(crate) fn queue(
        &mut self,
        write: impl FnOnce(&mut BytesMut) -> std::io::Result<()>,
    ) -> Result<()> {
        write(&mut self.outgoing).map_err(protocol)
    }

    /// Sends everything queued.
    pub(crate) fn flush(&mut self) -> Result<()> {
        if self.outgoing.is_empty() {
            return Ok(());
        }
        let result = self.connection.send(&self.outgoing);
        self.outgoing.clear();
        result
    }

    /// The next message from the server.
    pub(crate) fn next(&mut self) -> Result<Message> {
        loop {
            if let Some(message) = Message::parse(&mut self.incoming).map_err(protocol)? {
                return Ok(message);
            }
            let chunk = self.connection.fill()?;
            if chunk.is_empty() {
                return Err(DbError::connection("The server closed the connection"));
            }
            let n = chunk.len();
            self.incoming.extend_from_slice(chunk);
            self.connection.consume(n);
        }
    }

    /// Says goodbye, best effort.
    pub(crate) fn terminate(&mut self) {
        frontend::terminate(&mut self.outgoing);
        let _ = self.flush();
    }

    /// Sends a cancel request for `key` on this fresh connection.
    pub(crate) fn cancel(mut self, key: BackendKey) -> Result<()> {
        frontend::cancel_request(key.process_id, key.secret, &mut self.outgoing);
        self.flush()
    }
}

pub(crate) fn protocol(error: std::io::Error) -> DbError {
    DbError::protocol(format!("Protocol error: {error}"))
}

fn authentication(error: std::io::Error) -> DbError {
    DbError::authentication(format!("Authentication failed: {error}"))
}

/// An `ErrorResponse` in Violet's shape.
pub(crate) fn server_error(body: &ErrorResponseBody) -> DbError {
    let mut error = DbError::query(String::new());
    let mut fields = body.fields();
    while let Ok(Some(field)) = fields.next() {
        let value = String::from_utf8_lossy(field.value_bytes()).into_owned();
        match field.type_() {
            b'C' => error.code = Some(value),
            b'M' => error.message = value,
            b'D' => error.detail = Some(value),
            b'H' => error.hint = Some(value),
            b'P' => error.position = value.parse().ok(),
            _ => {}
        }
    }
    error.kind = match error.code.as_deref() {
        // query_canceled
        Some("57014") => ErrorKind::Cancelled,
        // invalid_password, invalid_authorization_specification
        Some("28P01" | "28000") => ErrorKind::Authentication,
        _ => ErrorKind::Query,
    };
    if error.message.is_empty() {
        error.message = "The server reported an error".into();
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_round_trip() {
        let key = BackendKey {
            process_id: 4242,
            secret: -7,
        };
        assert_eq!(BackendKey::from_ticket(&key.to_ticket()), Some(key));
        assert_eq!(BackendKey::from_ticket(&[1, 2, 3]), None);
        assert_eq!(BackendKey::from_ticket(&[0; 9]), None);
    }
}
