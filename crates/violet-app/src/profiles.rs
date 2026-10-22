//! Saved connections and their passwords, kept consistent.

use std::sync::Arc;

use violet_core::ports::{ProfileRepository, SavedProfiles, SecretStore};
use violet_core::{ConnectionId, ConnectionProfile, Result, Secret};

/// Loads, saves and deletes profiles, and keeps each password in the
/// secret store in step with its profile's `save_password`.
pub struct ProfileService {
    repository: Arc<dyn ProfileRepository>,
    secrets: Arc<dyn SecretStore>,
    profiles: Vec<ConnectionProfile>,
    /// Folders made by hand, kept so they show while empty.
    folders: Vec<String>,
}

impl ProfileService {
    /// Reads the saved profiles. A file that cannot be read is reported and
    /// left alone, and the list starts empty.
    pub fn load(
        repository: Arc<dyn ProfileRepository>,
        secrets: Arc<dyn SecretStore>,
    ) -> (Self, Option<violet_core::DbError>) {
        let (saved, error) = match repository.load() {
            Ok(saved) => (saved, None),
            Err(error) => (SavedProfiles::default(), Some(error)),
        };
        (
            Self {
                repository,
                secrets,
                profiles: saved.connections,
                folders: saved.folders,
            },
            error,
        )
    }

    #[must_use]
    pub fn all(&self) -> &[ConnectionProfile] {
        &self.profiles
    }

    /// Every folder, made by hand or holding a connection, with the ones
    /// above them, sorted ignoring case.
    #[must_use]
    pub fn folders(&self) -> Vec<String> {
        let mut all = std::collections::BTreeSet::new();
        let filed = self.profiles.iter().map(|profile| profile.folder.as_str());
        for folder in self.folders.iter().map(String::as_str).chain(filed) {
            let mut path = folder;
            while !path.is_empty() {
                all.insert(path.to_owned());
                path = parent_folder(path);
            }
        }
        let mut all: Vec<String> = all.into_iter().collect();
        all.sort_by_key(|folder| folder.to_lowercase());
        all
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
        self.store(next, self.folders.clone())
    }

    /// Deletes a profile and its password.
    pub fn remove(&mut self, id: ConnectionId) -> Result<()> {
        let next: Vec<_> = self
            .profiles
            .iter()
            .filter(|profile| profile.id != id)
            .cloned()
            .collect();
        self.store(next, self.folders.clone())?;
        // The profile is gone either way; a stale keychain entry is harmless.
        if let Err(error) = self.secrets.delete(id) {
            log::warn!("could not delete a password from the keychain: {error}");
        }
        Ok(())
    }

    /// Adds an empty folder.
    pub fn add_folder(&mut self, folder: &str) -> Result<()> {
        let mut folders = self.folders.clone();
        if !folders.iter().any(|existing| existing == folder) {
            folders.push(folder.to_owned());
        }
        self.store(self.profiles.clone(), folders)
    }

    /// Files a connection in `folder`; empty for the top level.
    pub fn move_to_folder(&mut self, id: ConnectionId, folder: &str) -> Result<()> {
        let mut next = self.profiles.clone();
        for profile in next.iter_mut().filter(|profile| profile.id == id) {
            folder.clone_into(&mut profile.folder);
        }
        self.store(next, self.folders.clone())
    }

    /// Renames or moves a folder, with everything in it.
    pub fn rename_folder(&mut self, from: &str, to: &str) -> Result<()> {
        self.refile(|path| within(path, from).map(|rest| join(to, rest)))
    }

    /// Deletes a folder. What it holds moves up a level: no connection is
    /// ever deleted with it.
    pub fn delete_folder(&mut self, folder: &str) -> Result<()> {
        let up = parent_folder(folder);
        self.refile(|path| within(path, folder).map(|rest| join(up, rest)))
    }

    /// Gives every folder path `change` has a new one for, in connections
    /// and hand-made folders alike, that new one.
    fn refile(&mut self, change: impl Fn(&str) -> Option<String>) -> Result<()> {
        let mut next = self.profiles.clone();
        for profile in &mut next {
            if let Some(folder) = change(&profile.folder) {
                profile.folder = folder;
            }
        }
        let mut folders: Vec<String> = Vec::new();
        for folder in &self.folders {
            let folder = change(folder).unwrap_or_else(|| folder.clone());
            if !folder.is_empty() && !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        self.store(next, folders)
    }

    fn store(&mut self, profiles: Vec<ConnectionProfile>, folders: Vec<String>) -> Result<()> {
        let saved = SavedProfiles {
            connections: profiles,
            folders,
        };
        self.repository.save(&saved)?;
        self.profiles = saved.connections;
        self.folders = saved.folders;
        Ok(())
    }
}

/// The folder above `folder`; empty at the top level.
#[must_use]
pub fn parent_folder(folder: &str) -> &str {
    folder.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// What is left of `path` below `folder` (empty for `folder` itself), or
/// `None` when `path` is not in it.
fn within<'a>(path: &'a str, folder: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(folder)?;
    if rest.is_empty() {
        Some(rest)
    } else {
        rest.strip_prefix('/')
    }
}

fn join(folder: &str, rest: &str) -> String {
    match (folder.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_owned(),
        (false, true) => folder.to_owned(),
        (false, false) => format!("{folder}/{rest}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use violet_core::DriverId;

    use super::*;

    #[derive(Default)]
    struct Memory(Mutex<SavedProfiles>);

    impl ProfileRepository for Memory {
        fn load(&self) -> Result<SavedProfiles> {
            Ok(self.0.lock().expect("lock").clone())
        }
        fn save(&self, profiles: &SavedProfiles) -> Result<()> {
            *self.0.lock().expect("lock") = profiles.clone();
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

    #[test]
    fn deleting_a_folder_moves_its_contents_up() {
        let (mut service, _) = service();
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        profile.folder = "Prod/Clients/Big".to_owned();
        service.upsert(profile.clone(), None).expect("save");
        service.add_folder("Prod/Clients/Empty").expect("add");
        assert_eq!(
            service.folders(),
            [
                "Prod",
                "Prod/Clients",
                "Prod/Clients/Big",
                "Prod/Clients/Empty"
            ]
        );

        service.delete_folder("Prod/Clients").expect("delete");
        assert_eq!(service.all().len(), 1);
        assert_eq!(service.all()[0].folder, "Prod/Big");
        assert_eq!(service.folders(), ["Prod", "Prod/Big", "Prod/Empty"]);

        service.rename_folder("Prod", "Live").expect("rename");
        assert_eq!(service.all()[0].folder, "Live/Big");
        service.move_to_folder(profile.id, "").expect("move");
        assert_eq!(service.all()[0].folder, "");
        assert_eq!(service.folders(), ["Live", "Live/Empty"]);
    }
}
