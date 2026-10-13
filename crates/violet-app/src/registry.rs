//! The drivers available now. Starts with the installed providers and
//! grows as more are installed while the app runs.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use violet_core::{Driver, DriverId, DriverInfo};

/// Drivers by id.
#[derive(Default)]
pub struct DriverRegistry {
    drivers: RwLock<BTreeMap<DriverId, Arc<dyn Driver>>>,
}

impl DriverRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a driver, replacing one with the same id.
    #[must_use]
    pub fn with(self, driver: impl Driver) -> Self {
        self.add(Arc::new(driver));
        self
    }

    /// Adds a driver, replacing one with the same id.
    pub fn add(&self, driver: Arc<dyn Driver>) {
        self.drivers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(driver.info().id, driver);
    }

    pub fn remove(&self, id: &DriverId) {
        self.drivers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    #[must_use]
    pub fn get(&self, id: &DriverId) -> Option<Arc<dyn Driver>> {
        self.drivers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Every driver's description, by id.
    #[must_use]
    pub fn infos(&self) -> Vec<DriverInfo> {
        self.drivers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|driver| driver.info())
            .collect()
    }
}

impl std::fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let drivers = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        f.debug_list().entries(drivers.keys()).finish()
    }
}
