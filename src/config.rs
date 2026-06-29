//! User configuration: font family/size + color scheme, persisted as TOML and hot-reloaded.
//!
//! Font family/size are also editable visually (the "Aa" menu writes them back here, so a
//! visual change survives restart). Colors are file-only. Lives at
//! $XDG_CONFIG_HOME/potty/potty.toml (or ~/.config/potty/potty.toml).

use std::collections::BTreeMap;
use std::path::PathBuf;

use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use serde::{Deserialize, Serialize};

use crate::gridr::{BASE16, Palette, default_ansi};

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
    /// Inner padding (logical px) between a pane's border and its terminal cells. Only applies
    /// when a tab has more than one pane (a lone pane draws no border and fills its area).
    pub pane_padding: f32,
    /// Default cursor shape: "block", "underline" (or "underscore"), or "beam" (or "bar").
    /// Programs may override this at runtime via DECSCUSR (`CSI Ps SP q`).
    pub cursor_shape: String,
    /// Default cursor blinking. Programs may override via DECSCUSR. The blink only consumes CPU
    /// while the *focused* pane's cursor is actually blinking and idle.
    pub cursor_blink: bool,
    /// Thickness of the underline/beam cursor as a fraction of the cell (height for underline,
    /// width for beam). Bump it for a fatter underscore. Ignored for the block cursor.
    pub cursor_thickness: f32,
    /// Command run on a remote host by "Connect to host…" to start the multiplexer backend. Must
    /// be on the remote's PATH, or an absolute path (until bootstrapping installs it for you).
    pub remote_command: String,
    /// Saved SSH connection profiles and recents. The canonical target fields identify the
    /// connection; `name` is only a display label.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ConnectionProfile>,
    pub colors: Colors,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub user: String,
    pub host: String,
    pub port: u16,
    pub use_potty_session: bool,
    /// Environment variables to inject into remote sessions for this target.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Colors {
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    /// Mouse-selection highlight background.
    pub selection: String,
    /// Active (focused) pane border, when a tab has more than one pane.
    pub border: String,
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
            pane_padding: 5.0,
            cursor_shape: "block".into(),
            cursor_blink: false,
            cursor_thickness: 0.15,
            remote_command: "potty-session".into(),
            profiles: Vec::new(),
            colors: Colors::default(),
        }
    }
}

impl Default for ConnectionProfile {
    fn default() -> Self {
        Self {
            name: None,
            user: String::new(),
            host: String::new(),
            port: 22,
            use_potty_session: false,
            env: BTreeMap::new(),
            last_connected: None,
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: "#cccccc".into(),
            background: "#0d0d10".into(),
            cursor: "#cccccc".into(),
            selection: "#334a6b".into(),
            border: "#78a0ff".into(),
            ansi: BASE16
                .iter()
                .map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
                .collect(),
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
            fg: parse_hex(&self.colors.foreground).unwrap_or(Rgb {
                r: 0xcc,
                g: 0xcc,
                b: 0xcc,
            }),
            bg: parse_hex(&self.colors.background).unwrap_or(Rgb {
                r: 0x0d,
                g: 0x0d,
                b: 0x10,
            }),
            cursor: parse_hex(&self.colors.cursor).unwrap_or(Rgb {
                r: 0xcc,
                g: 0xcc,
                b: 0xcc,
            }),
            selection: parse_hex(&self.colors.selection).unwrap_or(Rgb {
                r: 0x33,
                g: 0x4a,
                b: 0x6b,
            }),
            ansi,
        }
    }

    /// The configured default cursor shape (the starting style before any program issues
    /// DECSCUSR). Unknown values fall back to a block.
    pub fn cursor_shape(&self) -> CursorShape {
        match self.cursor_shape.trim().to_ascii_lowercase().as_str() {
            "underline" | "underscore" => CursorShape::Underline,
            "beam" | "bar" => CursorShape::Beam,
            _ => CursorShape::Block,
        }
    }

    /// Active pane border colour for the chrome.
    pub fn border(&self) -> Rgb {
        parse_hex(&self.colors.border).unwrap_or(Rgb {
            r: 0x78,
            g: 0xa0,
            b: 0xff,
        })
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

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn connection_profile_env_deserializes_from_toml() {
        let cfg: Config = toml::from_str(
            r#"
[[profiles]]
name = "work"
user = "alice"
host = "example.test"
port = 2222
use_potty_session = true
env = { POTTY_CONTEXT = "codex", EMPTY_OK = "" }
"#,
        )
        .expect("config parses");

        let profile = &cfg.profiles[0];
        assert_eq!(profile.env["POTTY_CONTEXT"], "codex");
        assert_eq!(profile.env["EMPTY_OK"], "");
    }
}
