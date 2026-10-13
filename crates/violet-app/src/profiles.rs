//! Saved connections and their passwords, kept consistent.

use std::sync::Arc;

use violet_core::ports::{ProfileRepository, SecretStore};
use violet_core::{ConnectionId, ConnectionProfile, Result, Secret};

/// Loads, saves and deletes profiles, and keeps each password in the
/// secret store in step with its profile's `save_password`.
pub struct ProfileService {
    repository: Arc<dyn ProfileRepository>,
    secrets: Arc<dyn SecretStore>,
    profiles: Vec<ConnectionProfile>,
}

impl ProfileService {
    /// Reads the saved profiles. A file that cannot be read is reported and
    /// left alone, and the list starts empty.
    pub fn load(
        repository: Arc<dyn ProfileRepository>,
        secrets: Arc<dyn SecretStore>,
    ) -> (Self, Option<violet_core::DbError>) {
        let (profiles, error) = match repository.load() {
            Ok(profiles) => (profiles, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        (
            Self {
                repository,
                secrets,
                profiles,
            },
            error,
        )
    }

    #[must_use]
    pub fn all(&self) -> &[ConnectionProfile] {
        &self.profiles
    }

    #[must_use]
    pub fn get(&self, id: ConnectionId) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    #[must_use]
    pub fn secrets(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secrets)
    }

    /// Adds or replaces a profile. `password` is `None` when the form left
    /// the stored one untouched.
    pub fn upsert(&mut self, profile: ConnectionProfile, password: Option<&Secret>) -> Result<()> {
        if profile.save_password {
            if let Some(password) = password {
                self.secrets.set(profile.id, password)?;
            }
        } else {
            self.secrets.delete(profile.id)?;
        }

        let mut next = self.profiles.clone();
        match next.iter_mut().find(|existing| existing.id == profile.id) {
            Some(existing) => *existing = profile,
            None => next.push(profile),
        }
        self.repository.save(&next)?;
        self.profiles = next;
        Ok(())
    }

    /// Deletes a profile and its password.
    pub fn remove(&mut self, id: ConnectionId) -> Result<()> {
        let next: Vec<_> = self
            .profiles
            .iter()
            .filter(|profile| profile.id != id)
            .cloned()
            .collect();
        self.repository.save(&next)?;
        self.profiles = next;
        // The profile is gone either way; a stale keychain entry is harmless.
        if let Err(error) = self.secrets.delete(id) {
            log::warn!("could not delete a password from the keychain: {error}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use violet_core::DriverId;

    use super::*;

    #[derive(Default)]
    struct Memory(Mutex<Vec<ConnectionProfile>>);

    impl ProfileRepository for Memory {
        fn load(&self) -> Result<Vec<ConnectionProfile>> {
            Ok(self.0.lock().expect("lock").clone())
        }
        fn save(&self, profiles: &[ConnectionProfile]) -> Result<()> {
            *self.0.lock().expect("lock") = profiles.to_vec();
            Ok(())
        }
    }

    #[derive(Default)]
    struct Secrets(Mutex<std::collections::HashMap<ConnectionId, Secret>>);

    impl SecretStore for Secrets {
        fn get(&self, id: ConnectionId) -> Result<Option<Secret>> {
            Ok(self.0.lock().expect("lock").get(&id).cloned())
        }
        fn set(&self, id: ConnectionId, secret: &Secret) -> Result<()> {
            self.0.lock().expect("lock").insert(id, secret.clone());
            Ok(())
        }
        fn delete(&self, id: ConnectionId) -> Result<()> {
            self.0.lock().expect("lock").remove(&id);
            Ok(())
        }
    }

    fn service() -> (ProfileService, Arc<Secrets>) {
        let secrets = Arc::new(Secrets::default());
        let (service, error) = ProfileService::load(Arc::new(Memory::default()), secrets.clone());
        assert!(error.is_none());
        (service, secrets)
    }

    #[test]
    fn unticking_save_password_forgets_it() {
        let (mut service, secrets) = service();
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        service
            .upsert(profile.clone(), Some(&Secret::new("pw")))
            .expect("save");
        assert!(secrets.get(profile.id).expect("get").is_some());

        profile.save_password = false;
        service.upsert(profile.clone(), None).expect("save");
        assert!(secrets.get(profile.id).expect("get").is_none());
        assert_eq!(service.all().len(), 1);
    }

    #[test]
    fn removing_a_profile_removes_its_password() {
        let (mut service, secrets) = service();
        let profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        service
            .upsert(profile.clone(), Some(&Secret::new("pw")))
            .expect("save");
        service.remove(profile.id).expect("remove");
        assert!(service.all().is_empty());
        assert!(secrets.get(profile.id).expect("get").is_none());
    }
}
