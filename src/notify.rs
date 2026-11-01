//! Shared wire contract for the **attention feed** — the cross-machine list of agentic-CLI
//! sessions waiting for you. This module is the one thing the `potty` binary (the listener) and
//! the `potty-notify` helper (the sender) must agree on: the JSON `Note`, the socket they speak
//! over, and the env vars potty injects into its child shells.
//!
//! Design: `docs/attention-feed.md`. The transport is out-of-band (a Unix-domain socket, or a
//! named pipe on Windows), so a note never rides the terminal byte stream — that's what lets a background pane, or (Phase 2)
//! a session over SSH, still reach the UI.

use serde::{Deserialize, Serialize};

/// Wire schema version. Bump on an incompatible change; the listener drops notes it can't read.
pub const SCHEMA_VERSION: u32 = 1;

/// Env var naming the listener socket. potty sets it for child shells; the helper connects to it.
pub const ENV_SOCK: &str = "POTTY_NOTIFY";
/// Env var naming the potty pane a child shell lives in. Lets a *local* note self-correlate to a
/// pane for exact jump-to-focus. Absent across SSH (env isn't forwarded by default).
pub const ENV_PANE: &str = "POTTY_PANE";
/// Env var naming the potty instance that owns the child shell. Pane ids are only process-local,
/// so broadcast notes need this to route click-to-focus back to the right window.
pub const ENV_INSTANCE: &str = "POTTY_INSTANCE";

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
    /// The potty GUI instance that owns `pane`, if known. Optional for compatibility with older
    /// helpers and remote daemons; the receiving instance fills it in for first-hop local notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zellij: Option<ZellijLoc>,
    /// Unix seconds, stamped by the helper.
    #[serde(default)]
    pub ts: u64,
}

/// The default listener socket: `$XDG_RUNTIME_DIR/potty/notify.sock`, falling back to the temp
/// dir. potty binds it and injects its path as `$POTTY_NOTIFY`; the helper honours that env var
/// (so SSH remote-forwarding to a different path keeps working) and only falls back to this.
#[cfg(unix)]
pub fn default_socket_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("potty").join("notify.sock")
}

/// Internal messages exchanged between live potty GUI instances. The public helper still sends a
/// raw `Note` when `$POTTY_NOTIFY` points at a specific owner socket; this wrapper is for fan-out
/// and cross-window activation inside the same user session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstanceMessage {
    Note {
        note: Note,
    },
    Focus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<String>,
        host: String,
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<u64>,
    },
    Dismiss {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<String>,
        host: String,
        session: String,
    },
}

/// On Windows the endpoints are named pipes, addressed by `\\.\pipe\…` paths.
#[cfg(windows)]
pub fn default_socket_path() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{PIPE_DIR}potty-notify"))
}

#[cfg(windows)]
const PIPE_DIR: &str = r"\\.\pipe\";
#[cfg(windows)]
const INSTANCE_PIPE: &str = "potty-notify-";

#[cfg(windows)]
pub fn instance_socket_path(instance: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{PIPE_DIR}{INSTANCE_PIPE}{instance}"))
}

/// Live instances' pipes. The pipe namespace lists like a directory.
#[cfg(windows)]
pub fn instance_socket_paths() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(PIPE_DIR) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(INSTANCE_PIPE))
        .map(|e| std::path::PathBuf::from(format!("{PIPE_DIR}{}", e.file_name().to_string_lossy())))
        .collect()
}

/// Connect to a notify endpoint (a Unix socket path, or a `\\.\pipe\…` path on Windows).
pub fn connect(path: &std::path::Path) -> std::io::Result<interprocess::local_socket::Stream> {
    use interprocess::local_socket::{GenericFilePath, Stream, prelude::*};
    Stream::connect(path.to_fs_name::<GenericFilePath>()?)
}

/// Bind a notify endpoint. On Unix the caller prepares the directory and clears stale sockets.
pub fn listen(path: &std::path::Path) -> std::io::Result<interprocess::local_socket::Listener> {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    ListenerOptions::new()
        .name(path.to_fs_name::<GenericFilePath>()?)
        // Removing the socket is the owner's job (on exit, or when it finds it stale).
        .reclaim_name(false)
        .create_sync()
}

/// Send one JSON line to a notify endpoint.
pub fn send_line(path: &std::path::Path, json: &str) -> std::io::Result<()> {
    use interprocess::local_socket::traits::Stream as _;
    use std::io::Write;
    let mut stream = connect(path)?;
    let _ = stream.set_send_timeout(Some(std::time::Duration::from_millis(250)));
    stream.write_all(format!("{json}\n").as_bytes())?;
    stream.flush()
}

/// Per-instance sockets live under the same private runtime directory as the compatibility socket.
#[cfg(unix)]
pub fn instance_socket_dir() -> std::path::PathBuf {
    default_socket_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join("instances")
}

#[cfg(unix)]
pub fn instance_socket_path(instance: &str) -> std::path::PathBuf {
    instance_socket_dir().join(format!("{instance}.sock"))
}

#[cfg(unix)]
pub fn instance_socket_paths() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(instance_socket_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_carry_one_line_and_instances_are_found() {
        use interprocess::local_socket::prelude::*;
        use std::io::{BufRead, BufReader};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // A real instance endpoint on Windows (to check discovery); a private temp socket on
        // Unix, where discovery is a plain directory listing.
        #[cfg(windows)]
        let path = instance_socket_path(&format!("test-{nanos}"));
        #[cfg(unix)]
        let path = std::env::temp_dir().join(format!("potty-notify-test-{nanos}.sock"));
        let listener = listen(&path).unwrap();
        #[cfg(windows)]
        assert!(
            instance_socket_paths().contains(&path),
            "{path:?} not listed"
        );
        let reader = std::thread::spawn(move || {
            let conn = listener.incoming().next().unwrap().unwrap();
            let mut line = String::new();
            BufReader::new(conn).read_line(&mut line).unwrap();
            line
        });
        send_line(&path, r#"{"v":1}"#).unwrap();
        assert_eq!(reader.join().unwrap(), "{\"v\":1}\n");
        #[cfg(unix)]
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_notes_without_instance_still_deserialize() {
        let json = r#"{
            "v": 1,
            "tool": "codex",
            "kind": "raise",
            "session": "abc",
            "message": "waiting",
            "cwd": "/tmp",
            "host": "host",
            "pid": 42,
            "pane": 7,
            "ts": 1
        }"#;

        let note: Note = serde_json::from_str(json).unwrap();
        assert_eq!(note.instance, None);
        assert_eq!(note.pane, Some(7));
    }

    #[test]
    fn instance_messages_round_trip_focus_requests() {
        let msg = InstanceMessage::Focus {
            instance: Some("123-456".to_string()),
            host: "host".to_string(),
            session: "abc".to_string(),
            pane: Some(7),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let decoded: InstanceMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            InstanceMessage::Focus {
                instance,
                host,
                session,
                pane,
            } => {
                assert_eq!(instance.as_deref(), Some("123-456"));
                assert_eq!(host, "host");
                assert_eq!(session, "abc");
                assert_eq!(pane, Some(7));
            }
            _ => panic!("decoded wrong instance message"),
        }
    }
}
