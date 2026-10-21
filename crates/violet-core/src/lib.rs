//! Violet's domain: connection profiles, catalog objects, query results and
//! the ports every database driver and persistence adapter implements.
//!
//! This crate performs no I/O and knows nothing about the interface.
//! Drivers implement [`driver::Driver`] and [`driver::Session`] (in Violet,
//! one adapter runs every database provider compiled to WebAssembly);
//! storage adapters implement the traits in [`ports`]. The application
//! layer only ever talks to these traits.

pub mod catalog;
pub mod driver;
pub mod error;
pub mod ports;
pub mod profile;
pub mod query;
pub mod secret;
pub mod tools;

pub use catalog::{ColumnInfo, DbObject, ForeignKey, ObjectKind, ServerInfo};
pub use driver::{
    Canceller, Capabilities, Dialect, Driver, DriverInfo, ManualCommit, OrderBy, PagedSql, Paging,
    Session, SqlDialect, string_literal,
};
pub use error::{DbError, ErrorKind, Result};
pub use profile::{ConnectionId, ConnectionProfile, Credentials, DriverId, SslMode, is_loopback};
pub use query::{ColumnMeta, ExecLimits, Page, PageInfo, QueryOutput, ResultSet, Row, Rows};
pub use secret::Secret;
pub use tools::{
    Argument, Binding, Choice, CommandPart, CommandTemplate, Condition, Connection, Download,
    Field, FieldKind, INSPECT_ENTRIES, INSPECT_HEAD, ObjectArguments, ObjectRef, PathKind, Piece,
    Quoting, Target, Task, TaskKind, TaskRequest, TaskSettings, ToolCommand, ValuePiece, Variable,
    split_args,
};
