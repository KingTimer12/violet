//! Interface preferences kept between runs.

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub sidebar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            sidebar: true,
        }
    }
}
