//! Where Violet keeps things between runs.
//!
//! - [`JsonProfileRepository`]: saved connections, as JSON in the user's
//!   config directory, written atomically and readable only by the user.
//! - [`KeyringSecretStore`]: passwords, in the operating system's keychain
//!   (Keychain on macOS, Credential Manager on Windows, Secret Service on
//!   Linux). They never touch the profiles file.
//! - [`MemorySecretStore`]: for tests and for machines without a keychain.
//! - [`JsonLinesHistory`]: the query history, one JSON line per query,
//!   appended as queries finish and trimmed to the newest on load.
//! - [`MemoryResultCache`]: read-only query results, in a bounded in-memory
//!   key-value store ([`MemoryKv`]) that never touches the disk.

mod cache;
mod dirs;
mod history;
mod json;
mod secrets;

pub use cache::{MemoryKv, MemoryResultCache};
pub use dirs::AppDirs;
pub use history::JsonLinesHistory;
pub use json::{JsonFile, JsonProfileRepository};
pub use secrets::{KeyringSecretStore, MemorySecretStore};
