//! The per-user directories Violet writes to.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// Config, state and log locations for the current user.
#[derive(Clone, Debug)]
pub struct AppDirs {
    config: PathBuf,
    state: PathBuf,
}

impl AppDirs {
    /// The platform's directories for Violet, or `None` when the home
    /// directory cannot be found.
    #[must_use]
    pub fn discover() -> Option<Self> {
        let dirs = ProjectDirs::from("dev", "violet", "Violet")?;
        let config = dirs.config_dir().to_path_buf();
        let state = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf();
        Some(Self { config, state })
    }

    /// Directories rooted at `root`, for tests and portable installs.
    pub fn at(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config: root.join("config"),
            state: root.join("state"),
        }
    }

    #[must_use]
    pub fn profiles_file(&self) -> PathBuf {
        self.config.join("connections.json")
    }

    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.state.join("violet.log")
    }

    #[must_use]
    pub fn panic_log(&self) -> PathBuf {
        self.state.join("panic.log")
    }

    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state
    }
}
