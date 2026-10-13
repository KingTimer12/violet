//! The drivers this build knows.

use std::collections::BTreeMap;
use std::sync::Arc;

use violet_core::{Driver, DriverId, DriverInfo};

/// Drivers by id. Filled once at start-up by the composition root.
#[derive(Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<DriverId, Arc<dyn Driver>>,
}

impl DriverRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a driver, replacing one with the same id.
    #[must_use]
    pub fn with(mut self, driver: impl Driver) -> Self {
        self.drivers.insert(driver.info().id, Arc::new(driver));
        self
    }

    #[must_use]
    pub fn get(&self, id: &DriverId) -> Option<Arc<dyn Driver>> {
        self.drivers.get(id).cloned()
    }

    /// Every driver's description, by id.
    #[must_use]
    pub fn infos(&self) -> Vec<DriverInfo> {
        self.drivers.values().map(|driver| driver.info()).collect()
    }
}

impl std::fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.drivers.keys()).finish()
    }
}
