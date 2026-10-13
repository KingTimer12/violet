//! Ports for persistence, implemented by `violet-storage`.

use std::sync::Arc;
use std::time::Instant;

use crate::error::Result;
use crate::profile::{ConnectionId, ConnectionProfile};
use crate::query::QueryOutput;
use crate::secret::Secret;

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
