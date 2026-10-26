//! Passwords in the operating system's keychain.

use std::collections::HashMap;
use std::sync::Mutex;

use violet_core::ports::SecretStore;
use violet_core::{ConnectionId, DbError, ErrorKind, Result, Secret};

/// The keychain service entries are filed under.
const SERVICE: &str = "dev.violet.connections";

/// Secrets in the platform keychain, one entry per connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringSecretStore;

impl KeyringSecretStore {
    fn entry(id: ConnectionId) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, &id.to_string()).map_err(|error| keychain_error(&error))
    }
}

fn keychain_error(error: &keyring::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("Keychain: {error}"))
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, id: ConnectionId) -> Result<Option<Secret>> {
        match Self::entry(id)?.get_password() {
            Ok(password) => Ok(Some(Secret::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keychain_error(&error)),
        }
    }

    fn set(&self, id: ConnectionId, secret: &Secret) -> Result<()> {
        Self::entry(id)?
            .set_password(secret.expose())
            .map_err(|error| keychain_error(&error))
    }

    fn delete(&self, id: ConnectionId) -> Result<()> {
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keychain_error(&error)),
        }
    }
}

/// The keychain service API keys are filed under, apart from connections.
const API_KEYS: &str = "dev.violet.ai";

/// API keys of the AI providers, in the platform keychain, one entry per
/// provider name.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringApiKeys;

impl KeyringApiKeys {
    fn entry(provider: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(API_KEYS, provider).map_err(|error| keychain_error(&error))
    }

    pub fn get(self, provider: &str) -> Result<Option<Secret>> {
        match Self::entry(provider)?.get_password() {
            Ok(key) => Ok(Some(Secret::new(key))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keychain_error(&error)),
        }
    }

    pub fn set(self, provider: &str, key: &Secret) -> Result<()> {
        Self::entry(provider)?
            .set_password(key.expose())
            .map_err(|error| keychain_error(&error))
    }

    pub fn delete(self, provider: &str) -> Result<()> {
        match Self::entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keychain_error(&error)),
        }
    }
}

/// Secrets held for this run only.
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    secrets: Mutex<HashMap<ConnectionId, Secret>>,
}

impl MemorySecretStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ConnectionId, Secret>> {
        // A panic while holding the lock leaves a map that is still whole.
        self.secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, id: ConnectionId) -> Result<Option<Secret>> {
        Ok(self.lock().get(&id).cloned())
    }

    fn set(&self, id: ConnectionId, secret: &Secret) -> Result<()> {
        self.lock().insert(id, secret.clone());
        Ok(())
    }

    fn delete(&self, id: ConnectionId) -> Result<()> {
        self.lock().remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_sets_gets_and_deletes() {
        let store = MemorySecretStore::default();
        let id = ConnectionId::new();
        assert_eq!(store.get(id).expect("get"), None);
        store.set(id, &Secret::new("pw")).expect("set");
        assert_eq!(store.get(id).expect("get"), Some(Secret::new("pw")));
        store.delete(id).expect("delete");
        assert_eq!(store.get(id).expect("get"), None);
    }
}
