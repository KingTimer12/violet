//! The database file of a file-based provider, such as SQLite's.
//!
//! A provider whose [`ProviderInfo::default_port`](crate::ProviderInfo) is
//! 0 opens a file instead of reaching a server: the profile's host is the
//! file's absolute path. [`open`] reaches that file (an empty suffix) and
//! the `-journal` and `-wal` files beside it, nothing else. [`File::lock`]
//! takes SQLite's locks, on the same bytes native SQLite locks, so Violet
//! and other programs using the database take turns.

pub use crate::bindings::violet::provider::files::{File, LockLevel, delete, exists, open};
