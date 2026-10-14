//! Ports for persistence, implemented by `violet-storage`.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::driver::{Driver, DriverInfo};
use crate::error::Result;
use crate::profile::{ConnectionId, ConnectionProfile, DriverId};
use crate::query::QueryOutput;
use crate::secret::Secret;

/// One query run from an editor, as the history keeps it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    /// When it finished, in milliseconds since the Unix epoch.
    pub at: u64,
    pub connection: Option<ConnectionId>,
    /// The connection's name at the time, so entries outlive the profile.
    pub source: String,
    pub sql: String,
    pub elapsed_ms: u64,
    /// Rows returned or affected.
    #[serde(default)]
    pub rows: Option<u64>,
    /// Why it failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// Every query run from an editor, kept on this machine.
///
/// Queries can hold anything the user typed, so implementations keep them
/// readable by the user only, and the interface leaves literals out of
/// statements that set passwords.
pub trait QueryHistory: Send + Sync + 'static {
    /// The entries kept, oldest first.
    fn load(&self) -> Result<Vec<HistoryEntry>>;
    fn append(&self, entry: &HistoryEntry) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

/// Where saved connections live.
pub trait ProfileRepository: Send + Sync + 'static {
    fn load(&self) -> Result<Vec<ConnectionProfile>>;
    fn save(&self, profiles: &[ConnectionProfile]) -> Result<()>;
}

/// Where passwords live. Never the profiles file.
pub trait SecretStore: Send + Sync + 'static {
    fn get(&self, id: ConnectionId) -> Result<Option<Secret>>;
    fn set(&self, id: ConnectionId, secret: &Secret) -> Result<()>;
    fn delete(&self, id: ConnectionId) -> Result<()>;
}

/// What a cached result is filed under: the connection and the exact text
/// sent, so different pages and filters never share an entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub connection: ConnectionId,
    pub sql: String,
}

/// A result kept for reuse.
#[derive(Clone, Debug)]
pub struct CachedOutput {
    pub output: Arc<QueryOutput>,
    /// The session's change token when the query ran; a different token
    /// now means the data may have moved on.
    pub token: Option<String>,
    pub stored: Instant,
}

/// A key-value store for read-only query results.
///
/// Results can hold anything the user can read, so implementations should
/// keep them in memory, bounded, and never write them to disk unencrypted.
pub trait ResultCache: Send + Sync + 'static {
    /// The entry for `key`, unless it is missing or too old.
    fn get(&self, key: &CacheKey) -> Option<CachedOutput>;
    fn put(&self, key: CacheKey, value: CachedOutput);
    /// Forgets everything for `connection`, as after a write.
    fn invalidate(&self, connection: ConnectionId);
}

/// A database provider Violet can use, installed or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderListing {
    pub info: DriverInfo,
    /// The newest version on offer.
    pub version: String,
    pub description: String,
    /// Download size in bytes.
    pub size: u64,
    /// The version installed, if any.
    pub installed: Option<String>,
}

impl ProviderListing {
    /// An update is on offer.
    #[must_use]
    pub fn outdated(&self) -> bool {
        self.installed
            .as_ref()
            .is_some_and(|installed| *installed != self.version)
    }
}

/// Where database drivers come from.
///
/// Violet ships none: each database is a provider installed on demand, the
/// first time a connection needs it, and kept for next time.
#[async_trait]
pub trait ProviderStore: Send + Sync + 'static {
    /// The providers installed, ready to register. Cheap: nothing is
    /// compiled until a connection opens.
    fn installed(&self) -> Result<Vec<Arc<dyn Driver>>>;

    /// Every provider on offer, with what is installed of each.
    async fn available(&self) -> Result<Vec<ProviderListing>>;

    /// Downloads, verifies and installs the newest version of `id`,
    /// replacing any installed one.
    async fn install(&self, id: &DriverId) -> Result<Arc<dyn Driver>>;

    async fn remove(&self, id: &DriverId) -> Result<()>;
}
