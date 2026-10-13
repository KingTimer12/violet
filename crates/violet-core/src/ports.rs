//! Ports for persistence, implemented by `violet-storage`.

use crate::error::Result;
use crate::profile::{ConnectionId, ConnectionProfile};
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
