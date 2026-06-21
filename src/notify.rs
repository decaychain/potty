//! Shared wire contract for the **attention feed** — the cross-machine list of agentic-CLI
//! sessions waiting for you. This module is the one thing the `potty` binary (the listener) and
//! the `potty-notify` helper (the sender) must agree on: the JSON `Note`, the socket they speak
//! over, and the env vars potty injects into its child shells.
//!
//! Design: `docs/attention-feed.md`. The transport is out-of-band (a Unix-domain socket), so a
//! note never rides the terminal byte stream — that's what lets a background pane, or (Phase 2)
//! a session over SSH, still reach the UI.

use serde::{Deserialize, Serialize};

/// Wire schema version. Bump on an incompatible change; the listener drops notes it can't read.
pub const SCHEMA_VERSION: u32 = 1;

/// Env var naming the listener socket. potty sets it for child shells; the helper connects to it.
pub const ENV_SOCK: &str = "POTTY_NOTIFY";
/// Env var naming the potty pane a child shell lives in. Lets a *local* note self-correlate to a
/// pane for exact jump-to-focus. Absent across SSH (env isn't forwarded by default).
pub const ENV_PANE: &str = "POTTY_PANE";

/// Which agentic tool raised the note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
    Other,
}

/// A note either *raises* attention (the session now wants you) or *clears* it (no longer does).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Raise,
    Clear,
}

/// Where a session sits inside Zellij, when it does — read from `ZELLIJ_*` env by the helper.
/// Shown so you know the last hop a remote/muxed session lives behind (potty can't switch it).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZellijLoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

/// One attention note. The full wire contract: one JSON object per socket connection,
/// newline-terminated. Unknown fields are ignored; missing optional fields default.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    /// Schema version (`SCHEMA_VERSION`).
    pub v: u32,
    pub tool: Tool,
    pub kind: Kind,
    /// The tool's session id — the identity/dedup key, paired with `host`.
    pub session: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The potty pane this session lives in (local only; from `$POTTY_PANE`). `Some` → exact jump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zellij: Option<ZellijLoc>,
    /// Unix seconds, stamped by the helper.
    #[serde(default)]
    pub ts: u64,
}

/// The default listener socket: `$XDG_RUNTIME_DIR/potty/notify.sock`, falling back to the temp
/// dir. potty binds it and injects its path as `$POTTY_NOTIFY`; the helper honours that env var
/// (so SSH remote-forwarding to a different path keeps working) and only falls back to this.
pub fn default_socket_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("potty").join("notify.sock")
}
