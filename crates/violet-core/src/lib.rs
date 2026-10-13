//! Violet's domain: connection profiles, catalog objects, query results and
//! the ports every database driver and persistence adapter implements.
//!
//! This crate performs no I/O and knows nothing about the interface. Drivers
//! (`violet-postgres`, ...) implement [`driver::Driver`] and
//! [`driver::Session`]; storage adapters implement the traits in [`ports`].
//! The application layer only ever talks to these traits, so a new database
//! is one new crate registered at start-up.

pub mod catalog;
pub mod driver;
pub mod error;
pub mod ports;
pub mod profile;
pub mod query;
pub mod secret;

pub use catalog::{ColumnInfo, DbObject, ObjectKind, ServerInfo};
pub use driver::{Canceller, Capabilities, Dialect, Driver, DriverInfo, Session};
pub use error::{DbError, ErrorKind, Result};
pub use profile::{ConnectionId, ConnectionProfile, Credentials, DriverId, SslMode, is_loopback};
pub use query::{ColumnMeta, ExecLimits, QueryOutput, ResultSet, Row, Rows};
pub use secret::Secret;
