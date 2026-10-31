use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// Optimize new images as they are imported, before they are stored.
    pub optimize_on_import: bool,
    /// Optional explicit library root; `None` uses the default under app data.
    pub library_root: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            optimize_on_import: true,
            library_root: None,
        }
    }
}

impl Settings {
    /// `%LOCALAPPDATA%\Forbidden Library`. The installer and the app share this.
    /// `FORBIDDEN_LIBRARY_DATA_DIR` overrides it for a portable install, testing and
    /// recovery without touching the real profile.
    pub fn data_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("FORBIDDEN_LIBRARY_DATA_DIR") {
            return PathBuf::from(dir);
        }
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Forbidden Library")
    }

    pub fn file() -> PathBuf {
        Self::data_dir().join("settings.json")
    }

    pub fn load() -> Self {
        let path = Self::file();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn library_root(&self) -> PathBuf {
        self.library_root
            .clone()
            .unwrap_or_else(|| Self::data_dir().join("library"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_saved_before_lossy_rewrites_were_removed_still_load() {
        // `allow_lossy` is gone, but files written with it must keep their
        // other choices rather than falling back to every default.
        let saved = r#"{"theme":"Light","optimize_on_import":false,"allow_lossy":true}"#;
        let settings: Settings = serde_json::from_str(saved).unwrap();
        assert_eq!(settings.theme, ThemeChoice::Light);
        assert!(!settings.optimize_on_import);
    }
}
