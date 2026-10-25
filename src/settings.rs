//! Configuration shared by the GUI and the headless session daemon.
use serde::Deserialize;
use std::{
    io,
    path::{Path, PathBuf},
};

/// Config file location: `%APPDATA%\potty\potty.toml` on Windows, otherwise XDG config.
pub fn config_path() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("potty").join("potty.toml")
}

#[derive(Deserialize)]
#[serde(default)]
struct HistorySettings {
    saved_contexts: bool,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            saved_contexts: true,
        }
    }
}

/// Read only the daemon's settings, ignoring GUI-specific options.
pub fn saved_contexts_enabled(path: &Path) -> io::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err),
    };
    let settings: HistorySettings = toml::from_str(&text).map_err(io::Error::other)?;
    Ok(settings.saved_contexts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_defaults_on_and_ignores_gui_settings() {
        for (text, enabled) in [
            ("", true),
            ("font_size = 14\n[colors]\nbackground = '#000000'", true),
            ("saved_contexts = false\nfont_size = 14", false),
            ("saved_contexts = true", true),
        ] {
            assert_eq!(
                toml::from_str::<HistorySettings>(text)
                    .unwrap()
                    .saved_contexts,
                enabled
            );
        }
        assert!(toml::from_str::<HistorySettings>("saved_contexts = 'false'").is_err());
    }
}
