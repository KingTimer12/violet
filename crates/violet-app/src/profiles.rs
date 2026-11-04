//! Saved connections and their passwords, kept consistent.

use std::sync::Arc;

use violet_core::ports::{ProfileRepository, SavedProfiles, SavedSecret, SecretStore};
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
    ///
    /// A saved password is bound to the profile's server and user. Edited
    /// here, a profile that moves keeps its password, bound to the new
    /// target; moved any other way (the file edited by hand, say), it does
    /// not get the password.
    pub fn upsert(&mut self, profile: ConnectionProfile, password: Option<&Secret>) -> Result<()> {
        let target = profile.secret_target();
        if profile.save_password {
            if let Some(password) = password {
                self.secrets.set(profile.id, &target, password)?;
            } else if let Some(before) = self.get(profile.id).map(ConnectionProfile::secret_target)
                && before != target
                && let SavedSecret::Found(kept) = self.secrets.get(profile.id, &before)?
            {
                self.secrets.set(profile.id, &target, &kept)?;
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

    /// The profiles and the folders made by hand, as saved.
    #[must_use]
    pub fn saved(&self) -> SavedProfiles {
        SavedProfiles {
            connections: self.profiles.clone(),
            folders: self.folders.clone(),
        }
    }

    /// Merges connections from a backup: each replaces the one with its id
    /// or is added, and the folders join the ones here. A password the
    /// backup carries is saved for its connection's target. Returns how
    /// many connections were added or replaced.
    ///
    /// A connection the backup carries no password for keeps whatever is
    /// saved here; bound to the old target, it is not sent to a new one.
    pub fn import(
        &mut self,
        saved: &SavedProfiles,
        passwords: &[(ConnectionId, Secret)],
    ) -> Result<usize> {
        for profile in &saved.connections {
            let password = passwords
                .iter()
                .find(|(id, _)| *id == profile.id)
                .map(|(_, secret)| secret);
            match password {
                Some(password) if profile.save_password => {
                    self.secrets
                        .set(profile.id, &profile.secret_target(), password)?;
                }
                _ if !profile.save_password => self.secrets.delete(profile.id)?,
                _ => {}
            }
        }
        let mut next = self.profiles.clone();
        for profile in &saved.connections {
            match next.iter_mut().find(|existing| existing.id == profile.id) {
                Some(existing) => *existing = profile.clone(),
                None => next.push(profile.clone()),
            }
        }
        let mut folders = self.folders.clone();
        for folder in &saved.folders {
            if !folders.contains(folder) {
                folders.push(folder.clone());
            }
        }
        self.store(next, folders)?;
        Ok(saved.connections.len())
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

/// Where `folder` lands when dropped in `parent`; `None` when it would go
/// into itself or stay put.
#[must_use]
pub fn moved_folder(folder: &str, parent: &str) -> Option<String> {
    if within(parent, folder).is_some() || parent_folder(folder) == parent {
        return None;
    }
    let name = folder.rsplit('/').next().unwrap_or(folder);
    Some(join(parent, name))
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

    type Secrets = violet_storage::MemorySecretStore;

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
        let target = profile.secret_target();
        assert!(
            secrets
                .get(profile.id, &target)
                .expect("get")
                .found()
                .is_some()
        );

        profile.save_password = false;
        service.upsert(profile.clone(), None).expect("save");
        assert_eq!(
            secrets.get(profile.id, &target).expect("get"),
            SavedSecret::None
        );
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
        assert_eq!(
            secrets
                .get(profile.id, &profile.secret_target())
                .expect("get"),
            SavedSecret::None
        );
    }

    #[test]
    fn editing_the_server_here_moves_the_saved_password_along() {
        let (mut service, secrets) = service();
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        profile.username = "app".to_owned();
        service
            .upsert(profile.clone(), Some(&Secret::new("pw")))
            .expect("save");

        profile.host = "db.example.com".to_owned();
        service.upsert(profile.clone(), None).expect("save");
        assert_eq!(
            secrets
                .get(profile.id, &profile.secret_target())
                .expect("get"),
            SavedSecret::Found(Secret::new("pw"))
        );
    }

    #[test]
    fn a_profile_moved_behind_violets_back_does_not_get_the_password() {
        let repository = Arc::new(Memory::default());
        let secrets = Arc::new(Secrets::default());
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        profile.host = "prod.example.com".to_owned();
        let (mut service, _) = ProfileService::load(repository.clone(), secrets.clone());
        service
            .upsert(profile.clone(), Some(&Secret::new("pw")))
            .expect("save");

        // The profiles file is rewritten to send the connection elsewhere.
        let mut tampered = profile.clone();
        tampered.host = "evil.example.com".to_owned();
        repository
            .save(&SavedProfiles {
                connections: vec![tampered.clone()],
                folders: Vec::new(),
            })
            .expect("tamper");
        let (mut service, _) = ProfileService::load(repository, secrets.clone());
        assert_eq!(
            secrets
                .get(tampered.id, &tampered.secret_target())
                .expect("get"),
            SavedSecret::Elsewhere(profile.secret_target())
        );

        // Saving it from the form, untouched, does not carry it over either.
        tampered.name = "Renamed".to_owned();
        service.upsert(tampered.clone(), None).expect("save");
        assert!(
            secrets
                .get(tampered.id, &tampered.secret_target())
                .expect("get")
                .found()
                .is_none()
        );
    }

    #[test]
    fn importing_merges_by_id_and_saves_the_carried_passwords() {
        let (mut service, secrets) = service();
        let mut kept = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        kept.name = "Here".to_owned();
        service
            .upsert(kept.clone(), Some(&Secret::new("here")))
            .expect("save");

        let mut replaced = kept.clone();
        replaced.name = "From backup".to_owned();
        let added = ConnectionProfile::new(DriverId::new("mysql"), 3306);
        let backup = SavedProfiles {
            connections: vec![replaced.clone(), added.clone()],
            folders: vec!["Imported".to_owned()],
        };
        let count = service
            .import(&backup, &[(added.id, Secret::new("there"))])
            .expect("import");
        assert_eq!(count, 2);
        assert_eq!(service.all(), &[replaced.clone(), added.clone()]);
        assert!(service.folders().contains(&"Imported".to_owned()));
        assert_eq!(
            secrets.get(added.id, &added.secret_target()).expect("get"),
            SavedSecret::Found(Secret::new("there"))
        );
        // Same target, no password in the backup: the one here stays.
        assert_eq!(
            secrets
                .get(replaced.id, &replaced.secret_target())
                .expect("get"),
            SavedSecret::Found(Secret::new("here"))
        );
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

    #[test]
    fn a_dropped_folder_keeps_its_name_under_the_new_parent() {
        assert_eq!(moved_folder("A/B", "C").as_deref(), Some("C/B"));
        assert_eq!(moved_folder("A/B", "").as_deref(), Some("B"));
        assert_eq!(moved_folder("A", "C/D").as_deref(), Some("C/D/A"));
        assert_eq!(moved_folder("A", "AB").as_deref(), Some("AB/A"));
        assert_eq!(moved_folder("A", "A"), None);
        assert_eq!(moved_folder("A", "A/B"), None);
        assert_eq!(moved_folder("A/B", "A"), None);
        assert_eq!(moved_folder("A", ""), None);
    }
}
