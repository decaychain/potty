//! User configuration: font family/size + color scheme, persisted as TOML and hot-reloaded.
//!
//! Font family/size are also editable visually (the "Aa" menu writes them back here, so a
//! visual change survives restart). Colors are file-only. Lives at
//! $XDG_CONFIG_HOME/potty/potty.toml (or ~/.config/potty/potty.toml).

use std::path::PathBuf;

use alacritty_terminal::vte::ansi::Rgb;
use serde::{Deserialize, Serialize};

use crate::gridr::{default_ansi, Palette, BASE16};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// None → generic monospace; otherwise a specific family name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Shell to spawn; None → platform default ($SHELL on unix, %COMSPEC% on Windows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    pub font_size: f32,
    /// Point size for the chrome (tab bar + menus). The terminal grid uses `font_size`.
    pub ui_font_size: f32,
    /// OSC 52 clipboard policy: "copy" (default, safe), "copy-paste", "paste", or "disabled".
    /// "paste"/"copy-paste" let programs READ your clipboard via escape sequence — including
    /// remote hosts over SSH. Enable read deliberately.
    pub osc52: String,
    pub colors: Colors,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Colors {
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    /// The 16 base ANSI colors as `#rrggbb`. Missing/short entries fall back to defaults.
    pub ansi: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: None,
            shell: None,
            font_size: 15.0,
            ui_font_size: 13.0,
            osc52: "copy".into(),
            colors: Colors::default(),
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: "#cccccc".into(),
            background: "#0d0d10".into(),
            cursor: "#cccccc".into(),
            ansi: BASE16.iter().map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}")).collect(),
        }
    }
}

impl Config {
    /// Load from disk, falling back to defaults on missing file or parse error.
    pub fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write to disk (creating the parent dir). Best-effort.
    pub fn save(&self, path: &PathBuf) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }

    /// Resolve the color scheme into the renderer's palette.
    pub fn palette(&self) -> Palette {
        let mut ansi = default_ansi();
        for (slot, hex) in ansi.iter_mut().zip(self.colors.ansi.iter()) {
            if let Some(c) = parse_hex(hex) {
                *slot = c;
            }
        }
        Palette {
            fg: parse_hex(&self.colors.foreground).unwrap_or(Rgb { r: 0xcc, g: 0xcc, b: 0xcc }),
            bg: parse_hex(&self.colors.background).unwrap_or(Rgb { r: 0x0d, g: 0x0d, b: 0x10 }),
            cursor: parse_hex(&self.colors.cursor).unwrap_or(Rgb { r: 0xcc, g: 0xcc, b: 0xcc }),
            ansi,
        }
    }
}

/// Config file location: `%APPDATA%\potty\potty.toml` on Windows, else
/// `$XDG_CONFIG_HOME/potty/potty.toml` falling back to `~/.config/...`.
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

fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    Some(Rgb {
        r: u8::from_str_radix(&s[0..2], 16).ok()?,
        g: u8::from_str_radix(&s[2..4], 16).ok()?,
        b: u8::from_str_radix(&s[4..6], 16).ok()?,
    })
}
