//! `potty-notify` — the helper an agentic CLI invokes from a notification hook. It reads the
//! tool's event, stamps it with identity from its *own* environment, and writes one JSON note to
//! potty's socket (`$POTTY_NOTIFY`). Out-of-band: it never touches the terminal byte stream, so
//! it works from a background pane and (Phase 2) over an SSH-forwarded socket.
//!
//! Usage:
//!   potty-notify --tool claude           # raise; reads Claude's hook JSON on stdin
//!   potty-notify --tool claude --clear   # clear; wire to Claude's UserPromptSubmit hook
//!   potty-notify --tool codex            # Codex `notify`; reads its JSON from argv
//!   potty-notify --install-hook claude   # wire the hooks into ~/.claude/settings.json
//!   potty-notify --install-hook codex    # wire `notify` into ~/.codex/config.toml
//!
//! It always exits 0 and never blocks the tool: with no socket (a shell outside potty) it
//! silently does nothing, so a session behaves exactly as it would without the hook installed.

use std::io::Read;
use std::path::PathBuf;

use lexopt::prelude::{Long, Short, Value};
use potty::notify::{
    default_socket_path, Kind, Note, Tool, ZellijLoc, ENV_PANE, ENV_SOCK, SCHEMA_VERSION,
};

fn main() {
    let mut tool = Tool::Other;
    let mut kind = Kind::Raise;
    let mut positional: Option<String> = None;
    let mut install: Option<String> = None;
    let mut print_ssh = false;
    let mut parser = lexopt::Parser::from_env();
    loop {
        match parser.next() {
            Ok(Some(Long("tool"))) => {
                tool = match parser.value().ok().and_then(|v| v.into_string().ok()).as_deref() {
                    Some("claude") => Tool::Claude,
                    Some("codex") => Tool::Codex,
                    _ => Tool::Other,
                };
            }
            Ok(Some(Long("clear"))) => kind = Kind::Clear,
            Ok(Some(Long("install-hook"))) => {
                install = parser.value().ok().and_then(|v| v.into_string().ok());
            }
            Ok(Some(Long("print-ssh-config"))) => print_ssh = true,
            Ok(Some(Long("help") | Short('h'))) => {
                print_help();
                return;
            }
            // A bare positional is Codex's JSON payload, or the host for --print-ssh-config.
            Ok(Some(Value(val))) => positional = val.into_string().ok(),
            // Ignore unknown flags and stop on a parse error — a hook helper must never abort
            // the tool that invoked it.
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }

    // Print a ready-to-paste ssh config block for forwarding the feed over SSH, then exit.
    if print_ssh {
        print_ssh_config(positional.as_deref());
        return;
    }

    // Installer mode: edit the tool's config so it calls this helper, then exit. Interactive
    // (prints what it did) — distinct from the silent hook path below.
    if let Some(target) = install {
        let res = match target.as_str() {
            "claude" => install_claude(),
            "codex" => install_codex(),
            other => {
                eprintln!("potty-notify: unknown --install-hook target '{other}' (use claude|codex)");
                std::process::exit(2);
            }
        };
        if let Err(e) = res {
            eprintln!("potty-notify: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Claude feeds the hook JSON on stdin; Codex passes it as the positional arg.
    let payload = positional.unwrap_or_else(|| {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        s
    });
    let v: serde_json::Value =
        serde_json::from_str(payload.trim()).unwrap_or(serde_json::Value::Null);
    let get = |k: &str| v.get(k).and_then(|x| x.as_str());

    let host = hostname();
    let pane = std::env::var(ENV_PANE).ok().and_then(|s| s.parse::<u64>().ok());

    // Field names vary by tool/event; fall back gracefully so a note is always well-formed.
    let session = get("session_id")
        .or_else(|| get("session"))
        .or_else(|| get("id"))
        .map(String::from)
        .filter(|s| !s.is_empty())
        // No session id (some Codex events) → synthesize a stable key so notes still group.
        .unwrap_or_else(|| match pane {
            Some(p) => format!("pane-{p}"),
            None => format!("{host}-unknown"),
        });
    let message = get("message")
        .or_else(|| get("title"))
        .or_else(|| get("notification_type"))
        .or_else(|| get("type"))
        .unwrap_or("waiting for you")
        .to_string();
    let cwd = get("cwd")
        .map(String::from)
        .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()))
        .unwrap_or_default();

    let note = Note {
        v: SCHEMA_VERSION,
        tool,
        kind,
        session,
        message,
        cwd,
        host,
        pid: Some(std::process::id()),
        pane,
        zellij: zellij_loc(),
        ts: unix_secs(),
    };

    // Best-effort: any failure (no socket, refused, closed) → exit quietly.
    let _ = send(&note);
}

#[cfg(unix)]
fn send(note: &Note) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let path = std::env::var_os(ENV_SOCK)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path);
    let mut stream = UnixStream::connect(path)?;
    let mut line = serde_json::to_string(note).unwrap_or_default();
    line.push('\n');
    stream.write_all(line.as_bytes())
}

#[cfg(not(unix))]
fn send(_note: &Note) -> std::io::Result<()> {
    // Phase 1 transport is a Unix-domain socket; no-op elsewhere.
    Ok(())
}

/// Best-effort hostname. `$HOSTNAME` isn't always exported, so fall back to `/etc/hostname`.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// Zellij coordinates, when running inside it (`ZELLIJ` is set even for background panes).
fn zellij_loc() -> Option<ZellijLoc> {
    std::env::var_os("ZELLIJ")?;
    Some(ZellijLoc {
        session: std::env::var("ZELLIJ_SESSION_NAME").ok().filter(|s| !s.is_empty()),
        pane: std::env::var("ZELLIJ_PANE_ID").ok().filter(|s| !s.is_empty()),
    })
}

fn unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn print_help() {
    print!(
        "\
potty-notify — attention-feed helper for potty

Tells a running potty when an agentic CLI is waiting for you. Normally invoked from a
tool's notification hook (not by hand); also wires up those hooks and prints SSH config.

USAGE:
  potty-notify --tool <claude|codex> [--clear]   Send a note. Reads the event JSON on
                                                  stdin (claude) or as an argument (codex).
                                                  --clear retracts a note (Claude UserPromptSubmit).
  potty-notify --install-hook <claude|codex>     Wire the hook into the tool's config (idempotent).
  potty-notify --print-ssh-config [host]         Print an ~/.ssh/config block to forward over SSH.
  potty-notify --help                            Show this help.

Sending is best-effort and silent: with no potty socket ($POTTY_NOTIFY) it exits 0 and does
nothing, so a shell outside potty is unaffected.

ENV (potty sets these per pane; you don't):
  POTTY_NOTIFY   socket to send notes to
  POTTY_PANE     pane id, for click-to-jump correlation

See docs/attention-feed.md for the design and the SSH/Zellij setup.
"
    );
}

/// Print an `~/.ssh/config` block that forwards the feed socket over SSH and propagates the env,
/// so notes from a remote session (even inside a background Zellij tab) reach the local potty.
/// `SendEnv POTTY_PANE` is what makes a remote note jump-correlatable to the pane that ran ssh.
fn print_ssh_config(host: Option<&str>) {
    let local = default_socket_path();
    let local = local.display();
    let remote = "/tmp/potty-notify.sock";
    let host = host.unwrap_or("your-remote-host");
    println!("# ~/.ssh/config — forward potty's attention feed over SSH");
    println!("Host {host}");
    println!("    RemoteForward {remote} {local}");
    println!("    SetEnv POTTY_NOTIFY={remote}");
    println!("    SendEnv POTTY_PANE");
    println!();
    println!("# On the REMOTE host, allow those through in sshd_config (or a drop-in), then");
    println!("# restart sshd:");
    println!("#     AcceptEnv POTTY_NOTIFY POTTY_PANE");
    println!("#     StreamLocalBindUnlink yes");
    println!("#");
    println!("# Without AcceptEnv POTTY_PANE a remote session still shows in the feed — it just");
    println!("# can't be clicked-to-jump. If you can't edit sshd_config, drop the SetEnv line and");
    println!("# instead add to the remote shell rc:");
    println!("#     [ -S {remote} ] && export POTTY_NOTIFY={remote}");
}

// --------------------------------------------------------------------------------------------
// `--install-hook` — wire this helper into a tool's config. Idempotent and non-destructive.
// --------------------------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Absolute path to this binary, so the installed hook resolves regardless of the caller's PATH.
/// Falls back to the bare name if the exe path can't be determined.
fn exe_path() -> String {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "potty-notify".into())
}

/// Wire the `Notification` (raise) and `UserPromptSubmit` (clear) hooks into Claude's settings,
/// preserving any existing hooks. Skips an event that already has a potty-notify command.
fn install_claude() -> std::io::Result<()> {
    use serde_json::{json, Value};

    let path = home_dir().join(".claude").join("settings.json");
    let exe = exe_path();

    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| {
            std::io::Error::other(format!("{} is not valid JSON: {e}", path.display()))
        })?,
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other(format!("{} is not a JSON object", path.display())))?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("`hooks` in settings.json is not an object"))?;

    let mut changed = false;
    for (event, command) in [
        ("Notification", format!("{exe} --tool claude")),
        ("UserPromptSubmit", format!("{exe} --tool claude --clear")),
    ] {
        let arr = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| std::io::Error::other(format!("`hooks.{event}` is not an array")))?;
        let present = arr.iter().any(|m| {
            m.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
                hs.iter().any(|h| {
                    h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("potty-notify"))
                })
            })
        });
        if present {
            println!("  {event}: already wired — left as is");
        } else {
            arr.push(json!({ "hooks": [{ "type": "command", "command": command }] }));
            changed = true;
            println!("  {event}: added");
        }
    }

    if changed {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut s = serde_json::to_string_pretty(&root).map_err(std::io::Error::other)?;
        s.push('\n');
        std::fs::write(&path, s)?;
        println!("Updated {}", path.display());
    } else {
        println!("Nothing to do — {} already wired", path.display());
    }
    Ok(())
}

/// Set Codex's `notify` to this helper, preserving the rest of config.toml (comments, layout).
/// Won't clobber an existing `notify` that points elsewhere — it prints what to set instead.
fn install_codex() -> std::io::Result<()> {
    use toml_edit::{value, Array, DocumentMut};

    let path = home_dir().join(".codex").join("config.toml");
    let exe = exe_path();

    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| std::io::Error::other(format!("{} is not valid TOML: {e}", path.display())))?;

    if let Some(existing) = doc.get("notify") {
        let rendered = existing.to_string();
        if rendered.contains("potty-notify") {
            println!("Nothing to do — {} already wired", path.display());
            return Ok(());
        }
        println!("⚠ Codex `notify` is already set to:{rendered}");
        println!("  Not overwriting. To use potty's feed instead, set:");
        println!("  notify = [\"{exe}\", \"--tool\", \"codex\"]");
        return Ok(());
    }

    let mut arr = Array::new();
    arr.push(exe.as_str());
    arr.push("--tool");
    arr.push("codex");
    doc["notify"] = value(arr);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, doc.to_string())?;
    println!("Updated {} — notify = [\"{exe}\", \"--tool\", \"codex\"]", path.display());
    Ok(())
}
