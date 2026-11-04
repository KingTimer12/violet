//! Passwords in the operating system's keychain.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use violet_core::ports::{SavedSecret, SecretStore};
use violet_core::{ConnectionId, DbError, ErrorKind, Result, Secret, SecretTarget};
use zeroize::Zeroizing;

/// The keychain service entries are filed under.
const SERVICE: &str = "dev.violet.connections";

/// A keychain entry: the password and the target it was saved for.
#[derive(Serialize, Deserialize)]
struct Envelope {
    v: u32,
    target: SecretTarget,
    password: Zeroizing<String>,
}

/// The envelope format written today.
const VERSION: u32 = 1;

/// Secrets in the platform keychain, one entry per connection, each bound
/// to its [`SecretTarget`].
///
/// Entries written before targets existed hold the bare password. The
/// first read takes it as saved for the target asked about and writes it
/// back bound to it, so the binding holds from then on.
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

/// The keychain text for `secret` saved for `target`.
fn seal(target: &SecretTarget, secret: &Secret) -> Result<Zeroizing<String>> {
    let envelope = Envelope {
        v: VERSION,
        target: target.clone(),
        password: Zeroizing::new(secret.expose().to_owned()),
    };
    serde_json::to_string(&envelope)
        .map(Zeroizing::new)
        .map_err(|error| DbError::new(ErrorKind::Storage, format!("Keychain: {error}")))
}

/// Just the version of an envelope.
#[derive(Deserialize)]
struct Versioned {
    v: u32,
}

/// What a keychain text holds for `target`; `None` for a bare password
/// from before envelopes. An envelope from a later version of Violet is
/// left alone and reads as nothing saved.
fn open(stored: &str, target: &SecretTarget) -> Option<SavedSecret> {
    // serde reads a struct from an array too; envelopes are objects.
    if !stored.starts_with('{') {
        return None;
    }
    let version: Versioned = serde_json::from_str(stored).ok()?;
    if version.v != VERSION {
        return Some(SavedSecret::None);
    }
    let Ok(envelope) = serde_json::from_str::<Envelope>(stored) else {
        return Some(SavedSecret::None);
    };
    Some(if envelope.target == *target {
        SavedSecret::Found(Secret::new(envelope.password.as_str()))
    } else {
        SavedSecret::Elsewhere(envelope.target)
    })
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, id: ConnectionId, target: &SecretTarget) -> Result<SavedSecret> {
        let entry = Self::entry(id)?;
        let stored = match entry.get_password() {
            Ok(stored) => Zeroizing::new(stored),
            Err(keyring::Error::NoEntry) => return Ok(SavedSecret::None),
            Err(error) => return Err(keychain_error(&error)),
        };
        if let Some(saved) = open(&stored, target) {
            return Ok(saved);
        }
        let secret = Secret::new(stored.as_str());
        if let Err(error) = entry.set_password(&seal(target, &secret)?) {
            log::warn!("could not bind a saved password to its server: {error}");
        }
        Ok(SavedSecret::Found(secret))
    }

    fn set(&self, id: ConnectionId, target: &SecretTarget, secret: &Secret) -> Result<()> {
        Self::entry(id)?
            .set_password(&seal(target, secret)?)
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

/// Secrets held for this run only, bound to their targets like the
/// keychain's.
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    secrets: Mutex<HashMap<ConnectionId, (SecretTarget, Secret)>>,
}

impl MemorySecretStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ConnectionId, (SecretTarget, Secret)>> {
        // A panic while holding the lock leaves a map that is still whole.
        self.secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, id: ConnectionId, target: &SecretTarget) -> Result<SavedSecret> {
        Ok(match self.lock().get(&id) {
            None => SavedSecret::None,
            Some((saved, secret)) if saved == target => SavedSecret::Found(secret.clone()),
            Some((saved, _)) => SavedSecret::Elsewhere(saved.clone()),
        })
    }

    fn set(&self, id: ConnectionId, target: &SecretTarget, secret: &Secret) -> Result<()> {
        self.lock().insert(id, (target.clone(), secret.clone()));
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

    fn target(host: &str) -> SecretTarget {
        SecretTarget {
            driver: "postgres".to_owned(),
            host: host.to_owned(),
            port: 5432,
            user: "app".to_owned(),
            ssl: violet_core::SslMode::Require,
        }
    }

    #[test]
    fn memory_store_sets_gets_and_deletes() {
        let store = MemorySecretStore::default();
        let id = ConnectionId::new();
        let prod = target("prod");
        assert_eq!(store.get(id, &prod).expect("get"), SavedSecret::None);
        store.set(id, &prod, &Secret::new("pw")).expect("set");
        assert_eq!(
            store.get(id, &prod).expect("get"),
            SavedSecret::Found(Secret::new("pw"))
        );
        store.delete(id).expect("delete");
        assert_eq!(store.get(id, &prod).expect("get"), SavedSecret::None);
    }

    #[test]
    fn a_password_is_not_handed_out_for_another_target() {
        let store = MemorySecretStore::default();
        let id = ConnectionId::new();
        store
            .set(id, &target("prod"), &Secret::new("pw"))
            .expect("set");
        assert_eq!(
            store.get(id, &target("evil")).expect("get"),
            SavedSecret::Elsewhere(target("prod"))
        );
    }

    #[test]
    fn envelopes_open_for_their_target_only() {
        let sealed = seal(&target("prod"), &Secret::new("p\"w")).expect("seal");
        assert!(!sealed.contains("evil"));
        assert_eq!(
            open(&sealed, &target("prod")),
            Some(SavedSecret::Found(Secret::new("p\"w")))
        );
        assert_eq!(
            open(&sealed, &target("evil")),
            Some(SavedSecret::Elsewhere(target("prod")))
        );
    }

    #[test]
    fn a_bare_password_is_not_an_envelope() {
        for stored in ["hunter2", "{}", "", "null", "[1]"] {
            assert_eq!(open(stored, &target("prod")), None, "{stored}");
        }
        // An envelope from a later version is neither used nor taken for a
        // bare password (which would overwrite it).
        let later = r#"{"v":2,"target":{"driver":"postgres","host":"prod","port":5432,"user":"app","ssl":"require"},"password":"pw"}"#;
        assert_eq!(open(later, &target("prod")), Some(SavedSecret::None));
        assert_eq!(open(r#"{"v":1}"#, &target("prod")), Some(SavedSecret::None));
    }
}
