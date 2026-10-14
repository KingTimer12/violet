//! Violet's database providers, installed when a connection first needs
//! them.
//!
//! The catalogue (`providers.json`, see [`index`]) lists every provider
//! with the SHA-256 of its component. Installing one downloads it over
//! HTTPS, checks the digest, compiles it and asks it to describe itself,
//! and only then moves it into the `providers` folder (see [`folder`]).
//! Loading an installed provider checks the digest again, so a file
//! changed on disk is refused rather than run.
//!
//! Even a provider that passed all of that runs sandboxed (`violet-wasm`):
//! it can only talk to the server of the profile it serves.

pub mod folder;
pub mod index;
pub mod source;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use violet_core::ports::{ProviderListing, ProviderStore};
use violet_core::{DbError, Driver, DriverId, ErrorKind, Result};
use violet_wasm::{Runtime, WasmDriver};

pub use folder::{Folder, Manifest};
pub use index::{Entry, Index, sha256};
pub use source::Source;

/// Where Violet looks for providers unless told otherwise: the assets of
/// the latest release.
pub const DEFAULT_INDEX: &str =
    "https://github.com/KingTimer12/violet/releases/latest/download/providers.json";

fn error(message: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Provider, message)
}

/// Runs blocking work (disk, HTTP) off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| error(e.to_string()))?
}

/// The providers folder and the catalogue it is filled from.
pub struct Providers {
    folder: Folder,
    runtime: Runtime,
    index: Source,
    /// One install at a time, so two connections asking for the same
    /// provider download it once.
    installing: Mutex<()>,
}

impl Providers {
    pub fn new(root: impl Into<PathBuf>, runtime: Runtime, index: Source) -> Self {
        let folder = Folder::new(root);
        folder.sweep();
        Self {
            folder,
            runtime,
            index,
            installing: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn folder(&self) -> &Folder {
        &self.folder
    }

    fn driver(&self, manifest: Manifest) -> Arc<dyn Driver> {
        let folder = self.folder.clone();
        let info = manifest.info.clone();
        Arc::new(WasmDriver::new(
            self.runtime.clone(),
            info,
            Box::new(move || folder.component(&manifest)),
        ))
    }

    /// The catalogue: fetched fresh, or the copy from last time when it
    /// cannot be reached.
    #[hotpath::measure]
    pub async fn index(&self) -> Result<Index> {
        let source = self.index.clone();
        let fetched = blocking(move || source.read(index::MAX_INDEX_SIZE)).await;
        match fetched.and_then(|bytes| Index::parse(&bytes).map(|index| (index, bytes))) {
            Ok((index, bytes)) => {
                let folder = self.folder.clone();
                if let Err(e) = blocking(move || folder.cache_index(&bytes)).await {
                    log::warn!("could not keep the provider catalogue: {e}");
                }
                Ok(index)
            }
            Err(e) => {
                let cached = self.folder.cached_index();
                match cached.map(|bytes| Index::parse(&bytes)) {
                    Some(Ok(index)) => {
                        log::warn!("provider catalogue unreachable, using the last copy: {e}");
                        Ok(index)
                    }
                    _ => Err(e),
                }
            }
        }
    }

    /// Checks and installs a component that is already at hand: one just
    /// downloaded, or one a developer built (`violet-provider install`).
    #[hotpath::measure]
    pub async fn install_component(
        &self,
        component: Vec<u8>,
        expected_id: Option<&str>,
    ) -> Result<Arc<dyn Driver>> {
        let digest = sha256(&component);
        let description = self.runtime.inspect(component.clone()).await?;
        if let Some(expected) = expected_id
            && description.info.id.as_str() != expected
        {
            return Err(error(format!(
                "The catalogue's \"{expected}\" provider says it is \"{}\"",
                description.info.id
            )));
        }
        let manifest = Manifest {
            version: description.version,
            description: description.description,
            api: violet_wasm::API_VERSION.to_owned(),
            sha256: digest,
            info: description.info,
        };
        let folder = self.folder.clone();
        let installed = manifest.clone();
        blocking(move || folder.install(&installed, &component)).await?;
        log::info!(
            "installed provider {} {} into {}",
            manifest.info.id,
            manifest.version,
            self.folder.root().display()
        );
        Ok(self.driver(manifest))
    }
}

#[async_trait]
impl ProviderStore for Providers {
    fn installed(&self) -> Result<Vec<Arc<dyn Driver>>> {
        Ok(self
            .folder
            .manifests()?
            .into_iter()
            .map(|manifest| self.driver(manifest))
            .collect())
    }

    async fn available(&self) -> Result<Vec<ProviderListing>> {
        let folder = self.folder.clone();
        let installed = blocking(move || folder.manifests()).await?;
        let offered = match self.index().await {
            Ok(index) => index.providers,
            // Offline with nothing cached: what is installed still shows.
            Err(e) if !installed.is_empty() => {
                log::warn!("provider catalogue unavailable: {e}");
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        let mut listings: Vec<ProviderListing> = offered
            .into_iter()
            .map(|entry| {
                let installed = installed
                    .iter()
                    .find(|manifest| manifest.info.id.as_str() == entry.id);
                ProviderListing {
                    installed: installed.map(|manifest| manifest.version.clone()),
                    info: entry.info,
                    version: entry.version,
                    description: entry.description,
                    size: entry.size,
                }
            })
            .collect();
        for manifest in installed {
            if !listings.iter().any(|l| l.info.id == manifest.info.id) {
                listings.push(ProviderListing {
                    info: manifest.info,
                    installed: Some(manifest.version.clone()),
                    version: manifest.version,
                    description: manifest.description,
                    size: 0,
                });
            }
        }
        listings.sort_by(|a, b| a.info.name.cmp(&b.info.name));
        Ok(listings)
    }

    async fn install(&self, id: &DriverId) -> Result<Arc<dyn Driver>> {
        self.download(id).await
    }

    async fn remove(&self, id: &DriverId) -> Result<()> {
        let _one_at_a_time = self.installing.lock().await;
        let folder = self.folder.clone();
        let id = id.as_str().to_owned();
        blocking(move || folder.remove(&id)).await
    }
}

impl Providers {
    /// Installs `id` from the catalogue, unless that version is already in.
    #[hotpath::measure]
    async fn download(&self, id: &DriverId) -> Result<Arc<dyn Driver>> {
        let _one_at_a_time = self.installing.lock().await;
        let index = self.index().await?;
        let entry = index.get(id.as_str()).cloned().ok_or_else(|| {
            error(format!(
                "There is no provider for \"{id}\" in the catalogue ({})",
                self.index
            ))
        })?;

        // Installed meanwhile, by a connection that asked first.
        let folder = self.folder.clone();
        let wanted = id.as_str().to_owned();
        if let Some(manifest) = blocking(move || folder.manifest(&wanted)).await?
            && manifest.sha256 == entry.sha256
        {
            return Ok(self.driver(manifest));
        }

        let location = self.index.join(&entry.url)?;
        log::info!(
            "downloading provider {} {} from {location}",
            entry.id,
            entry.version
        );
        let size = entry.size;
        let component = blocking(move || location.read(size)).await?;
        if component.len() as u64 != entry.size || sha256(&component) != entry.sha256 {
            return Err(error(format!(
                "The downloaded \"{}\" provider does not match the catalogue's digest; \
                 it was not installed",
                entry.id
            )));
        }
        self.install_component(component, Some(&entry.id)).await
    }
}
