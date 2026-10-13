//! TLS for connections that ask for it.

use std::sync::{Arc, OnceLock};

use rustls::ClientConfig;
use tokio_postgres_rustls::MakeRustlsConnect;
use violet_core::{DbError, ErrorKind, Result};

/// A connector that verifies the server against the web PKI roots. Built
/// once: loading the roots costs a few milliseconds.
pub(crate) fn connector() -> Result<MakeRustlsConnect> {
    static CONFIG: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map(|builder| Arc::new(builder.with_root_certificates(roots).with_no_client_auth()))
            .map_err(|error| error.to_string())
    });
    match config {
        Ok(config) => Ok(MakeRustlsConnect::new((**config).clone())),
        Err(message) => Err(DbError::new(ErrorKind::Tls, message.clone())),
    }
}
