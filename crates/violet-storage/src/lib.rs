//! Where Violet keeps things between runs.
//!
//! - [`JsonProfileRepository`]: saved connections, as JSON in the user's
//!   config directory, written atomically and readable only by the user.
//! - [`KeyringSecretStore`]: passwords, in the operating system's keychain
//!   (Keychain on macOS, Credential Manager on Windows, Secret Service on
//!   Linux). They never touch the profiles file.
//! - [`KeyringApiKeys`]: the AI providers' API keys, in the same keychain.
//! - [`MemorySecretStore`]: for tests and for machines without a keychain.
//! - [`JsonLinesHistory`]: the query history, one JSON line per query,
//!   appended as queries finish and trimmed to the newest on load.
//! - [`ScriptStore`]: SQL scripts saved for a connection's database, as
//!   `.sql` files in folders named after it.
//! - [`VaultSecretStore`]: saved passwords sealed in `vault.json`, its key
//!   in the keychain or behind a master password.
//! - [`MemoryResultCache`]: read-only query results, in a bounded in-memory
//!   key-value store ([`MemoryKv`]) that never touches the disk.

mod cache;
mod dirs;
mod history;
mod json;
mod revisions;
pub mod scripts;
mod secrets;
mod vault;

pub use cache::{MemoryKv, MemoryResultCache};
pub use dirs::{AppDirs, ProjectPaths};
pub use history::JsonLinesHistory;
pub use json::{JsonFile, JsonProfileRepository, create_private_dir, open_private};
pub use revisions::{
    KEPT as REVISIONS_KEPT, Performance, PlanSummary, Revision, RevisionNote, RunSummary,
};
pub use scripts::{SavedScript, ScriptFolder, ScriptStore};
pub use secrets::{KeyringApiKeys, KeyringSecretStore, MemorySecretStore};
pub use vault::{KeySlot, MIN_MASTER_PASSWORD, VaultSecretStore};
