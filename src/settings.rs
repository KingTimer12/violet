//! Interface preferences kept between runs.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
}

impl From<ThemeChoice> for egui::ThemePreference {
    fn from(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::System => Self::System,
            ThemeChoice::Dark => Self::Dark,
            ThemeChoice::Light => Self::Light,
        }
    }
}

impl From<egui::ThemePreference> for ThemeChoice {
    fn from(preference: egui::ThemePreference) -> Self {
        match preference {
            egui::ThemePreference::System => Self::System,
            egui::ThemePreference::Dark => Self::Dark,
            egui::ThemePreference::Light => Self::Light,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub sidebar: bool,
    /// The sidebar's width, in points.
    pub sidebar_width: f32,
    /// How much of an editor tab the results take, from 0 to 1.
    pub results_share: f32,
    /// Providers the user let run their client programs, by id.
    pub trusted_tools: BTreeSet<String>,
    /// The folder with each provider's client programs, when picked.
    pub client_folders: BTreeMap<String, String>,
    /// The settings each backup and restore last ran with, by
    /// `provider/task`.
    pub task_settings: BTreeMap<String, BTreeMap<String, String>>,
    /// Where backups went last, and how they were named.
    pub backup_folder: String,
    pub backup_name: String,
    /// Where a script was last saved, for the file dialogs.
    pub script_folder: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            sidebar: true,
            sidebar_width: 280.0,
            results_share: 0.5,
            trusted_tools: BTreeSet::new(),
            client_folders: BTreeMap::new(),
            task_settings: BTreeMap::new(),
            backup_folder: String::new(),
            backup_name: String::new(),
            script_folder: String::new(),
        }
    }
}
