//! `potty-notify` — the helper an agentic CLI invokes from a notification hook. It reads the
//! tool's event, stamps it with identity from its *own* environment, and writes one JSON note to
//! potty's socket (`$POTTY_NOTIFY`). Out-of-band: it never touches the terminal byte stream, so
//! it works from a background pane and (Phase 2) over an SSH-forwarded socket.
//!
//! Usage:
//!   potty-notify --tool claude           # raise; reads Claude's hook JSON on stdin
//!   potty-notify --tool claude --clear   # clear; wire to Claude's UserPromptSubmit hook
//!   potty-notify --tool codex            # Codex `notify`/hooks; reads JSON from argv or stdin
//!   potty-notify --install-hook claude   # wire the hooks into ~/.claude/settings.json
//!   potty-notify --install-hook codex    # wire `notify` + hooks into ~/.codex/config.toml
//!
//! It always exits 0 and never blocks the tool: with no socket (a shell outside potty) it
//! silently does nothing, so a session behaves exactly as it would without the hook installed.

use std::io::Read;
use std::path::PathBuf;

use lexopt::prelude::{Long, Short, Value};
use potty::notify::{
    ENV_PANE, ENV_SOCK, Kind, Note, SCHEMA_VERSION, Tool, ZellijLoc, default_socket_path,
};

fn main() {
    let mut tool = Tool::Other;
    let mut kind = Kind::Raise;
    let mut positional: Option<String> = None;
    let mut install: Option<String> = None;
    let mut print_ssh = false;
    let mut print_wrapper = false;
    let mut parser = lexopt::Parser::from_env();
    loop {
        match parser.next() {
            Ok(Some(Long("tool"))) => {
                tool = match parser
                    .value()
                    .ok()
                    .and_then(|v| v.into_string().ok())
                    .as_deref()
                {
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
            Ok(Some(Long("print-ssh-wrapper"))) => print_wrapper = true,
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
    if print_wrapper {
        print_ssh_wrapper();
        return;
    }

    // Installer mode: edit the tool's config so it calls this helper, then exit. Interactive
    // (prints what it did) — distinct from the silent hook path below.
    if let Some(target) = install {
        let res = match target.as_str() {
            "claude" => install_claude(),
            "codex" => install_codex(),
            other => {
                eprintln!(
                    "potty-notify: unknown --install-hook target '{other}' (use claude|codex)"
                );
                std::process::exit(2);
            }
        };
        if let Err(e) = res {
            eprintln!("potty-notify: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Claude and Codex hooks feed JSON on stdin; Codex's legacy `notify` passes it as argv.
    let payload = positional.unwrap_or_else(|| {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        s
    });
    let v: serde_json::Value =
        serde_json::from_str(payload.trim()).unwrap_or(serde_json::Value::Null);

    let host = hostname();
    let pane = std::env::var(ENV_PANE)
        .ok()
        .and_then(|s| s.parse::<u64>().ok());

    // Field names vary by tool/event; fall back gracefully so a note is always well-formed.
    let session = payload_str(&v, &["session_id", "session", "id"])
        .map(String::from)
        .filter(|s| !s.is_empty())
        // No session id (some Codex events) → synthesize a stable key so notes still group.
        .unwrap_or_else(|| match pane {
            Some(p) => format!("pane-{p}"),
            None => format!("{host}-unknown"),
        });
    let message = message_from_payload(&v);
    let cwd = payload_str(&v, &["cwd"])
        .map(String::from)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
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
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// Zellij coordinates, when running inside it (`ZELLIJ` is set even for background panes).
fn zellij_loc() -> Option<ZellijLoc> {
    std::env::var_os("ZELLIJ")?;
    Some(ZellijLoc {
        session: std::env::var("ZELLIJ_SESSION_NAME")
            .ok()
            .filter(|s| !s.is_empty()),
        pane: std::env::var("ZELLIJ_PANE_ID")
            .ok()
            .filter(|s| !s.is_empty()),
    })
}

fn unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn payload_str<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        v.get(*key)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
    })
}

fn message_from_payload(v: &serde_json::Value) -> String {
    if let Some(message) = payload_str(v, &["message", "title", "notification_type"]) {
        return message.to_string();
    }

    let event = payload_str(
        v,
        &[
            "hook_event_name",
            "hookEventName",
            "event_name",
            "eventName",
            "event",
            "type",
        ],
    );
    if event.is_some_and(|event| event_name_eq(event, "PermissionRequest")) {
        return payload_str(v, &["tool_name", "toolName", "tool"])
            .map(|tool| format!("{tool} approval needed"))
            .unwrap_or_else(|| "approval needed".to_string());
    }
    if event.is_some_and(|event| event_name_eq(event, "UserPromptSubmit")) {
        return "prompt submitted".to_string();
    }

    payload_str(v, &["type"])
        .unwrap_or("waiting for you")
        .to_string()
}

fn event_name_eq(actual: &str, expected: &str) -> bool {
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| *c != '-' && *c != '_')
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    normalize(actual) == normalize(expected)
}

fn print_help() {
    print!(
        "\
potty-notify — attention-feed helper for potty

Tells a running potty when an agentic CLI is waiting for you. Normally invoked from a
tool's notification hook (not by hand); also wires up those hooks and prints SSH config.

USAGE:
  potty-notify --tool <claude|codex> [--clear]   Send a note. Reads the event JSON on
                                                  stdin, or from Codex `notify` argv.
                                                  --clear retracts a note (Claude UserPromptSubmit).
  potty-notify --install-hook <claude|codex>     Wire the hook into the tool's config (idempotent).
  potty-notify --print-ssh-config [host]         Print an ~/.ssh/config block to forward over SSH
                                                  (simple; one session per host at a time).
  potty-notify --print-ssh-wrapper               Print a shell `ssh` wrapper with per-pane sockets
                                                  (handles concurrent sessions to one host).
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
    println!("#");
    println!("# NOTE: this fixed remote path collides if you open two sessions to {host} at once.");
    println!("# For that, use the per-pane wrapper instead:  potty-notify --print-ssh-wrapper");
}

/// Print a shell `ssh` wrapper that forwards the feed on every connection using a *per-pane*
/// remote socket path. Unlike the static `--print-ssh-config` block, this lets concurrent
/// sessions to the same host coexist (each pane gets its own remote socket). No-op outside potty.
fn print_ssh_wrapper() {
    let remote = "/tmp/potty-notify-$POTTY_PANE.sock";
    println!("# potty attention feed over SSH — add to ~/.bashrc or ~/.zshrc.");
    println!("# Forwards the notify socket on every ssh, with a PER-PANE remote path so two");
    println!(
        "# sessions to the same host don't collide. Outside potty (no $POTTY_PANE) it's plain ssh."
    );
    println!("ssh() {{");
    println!("  if [ -n \"$POTTY_PANE\" ] && [ -S \"$POTTY_NOTIFY\" ]; then");
    println!("    command ssh -R \"{remote}:$POTTY_NOTIFY\" \\");
    println!("      -o \"SetEnv POTTY_NOTIFY={remote}\" -o \"SendEnv POTTY_PANE\" \"$@\"");
    println!("  else");
    println!("    command ssh \"$@\"");
    println!("  fi");
    println!("}}");
    println!();
    println!("# On each REMOTE host, in sshd_config (or a drop-in), then restart sshd:");
    println!("#     AcceptEnv POTTY_NOTIFY POTTY_PANE");
    println!("#     StreamLocalBindUnlink yes");
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
    use serde_json::{Value, json};

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
                    h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|c| c.contains("potty-notify"))
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

/// Set Codex's `notify` and lifecycle hooks to this helper, preserving the rest of config.toml
/// (comments, layout). Won't clobber an existing `notify` that points elsewhere — it prints what
/// to set instead, then still installs lifecycle hooks.
fn install_codex() -> std::io::Result<()> {
    use toml_edit::{Array, DocumentMut, value};

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

    let mut changed = false;
    if let Some(existing) = doc.get("notify") {
        let rendered = existing.to_string();
        if rendered.contains("potty-notify") {
            println!("  notify: already wired — left as is");
        } else {
            println!("⚠ Codex `notify` is already set to:{rendered}");
            println!("  Not overwriting. To use potty's feed for turn-complete too, set:");
            println!("  notify = [\"{exe}\", \"--tool\", \"codex\"]");
        }
    } else {
        let mut arr = Array::new();
        arr.push(exe.as_str());
        arr.push("--tool");
        arr.push("codex");
        doc["notify"] = value(arr);
        changed = true;
        println!("  notify: added");
    }

    for hook in [
        CodexHook {
            event: "PermissionRequest",
            matcher: Some("*"),
            command: format!("{exe} --tool codex"),
        },
        CodexHook {
            event: "UserPromptSubmit",
            matcher: None,
            command: format!("{exe} --tool codex --clear"),
        },
    ] {
        if ensure_codex_command_hook(&mut doc, &hook)? {
            changed = true;
            println!("  {}: added", hook.event);
        } else {
            println!("  {}: already wired — left as is", hook.event);
        }
    }

    if changed {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, doc.to_string())?;
        println!("Updated {}", path.display());
    } else {
        println!("Nothing to do — {} already wired", path.display());
    }
    println!("  Codex may ask you to trust command hooks once via /hooks before they run.");
    Ok(())
}

struct CodexHook {
    event: &'static str,
    matcher: Option<&'static str>,
    command: String,
}

fn ensure_codex_command_hook(
    doc: &mut toml_edit::DocumentMut,
    hook: &CodexHook,
) -> std::io::Result<bool> {
    use toml_edit::{ArrayOfTables, Item, Table, value};

    let hooks_item = doc
        .as_table_mut()
        .entry("hooks")
        .or_insert(Item::Table(Table::new()));
    let hooks = hooks_item
        .as_table_mut()
        .ok_or_else(|| std::io::Error::other("`hooks` in config.toml is not a table"))?;
    let groups_item = hooks
        .entry(hook.event)
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    let groups = groups_item.as_array_of_tables_mut().ok_or_else(|| {
        std::io::Error::other(format!(
            "`hooks.{}` in config.toml is not an array of tables",
            hook.event
        ))
    })?;

    if groups.iter().any(table_has_potty_hook) {
        return Ok(false);
    }

    let mut group = Table::new();
    if let Some(matcher) = hook.matcher {
        group.insert("matcher", value(matcher));
    }

    let mut handlers = ArrayOfTables::new();
    let mut handler = Table::new();
    handler.insert("type", value("command"));
    handler.insert("command", value(hook.command.as_str()));
    handler.insert("timeout", value(10));
    handlers.push(handler);
    group.insert("hooks", Item::ArrayOfTables(handlers));

    groups.push(group);
    Ok(true)
}

fn table_has_potty_hook(table: &toml_edit::Table) -> bool {
    table
        .get("hooks")
        .and_then(toml_edit::Item::as_array_of_tables)
        .is_some_and(|hooks| hooks.iter().any(handler_is_potty_hook))
}

fn handler_is_potty_hook(handler: &toml_edit::Table) -> bool {
    handler
        .get("command")
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_str)
        .is_some_and(|command| command.contains("potty-notify"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_permission_request_message_mentions_tool() {
        let payload = serde_json::json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash"
        });

        assert_eq!(message_from_payload(&payload), "Bash approval needed");
    }

    #[test]
    fn codex_permission_request_message_has_generic_fallback() {
        let payload = serde_json::json!({
            "event": "permission-request"
        });

        assert_eq!(message_from_payload(&payload), "approval needed");
    }

    #[test]
    fn codex_hook_install_is_idempotent() {
        let mut doc: toml_edit::DocumentMut = r#"
model = "gpt-5"
"#
        .parse()
        .unwrap();
        let hook = CodexHook {
            event: "PermissionRequest",
            matcher: Some("*"),
            command: "/usr/bin/potty-notify --tool codex".to_string(),
        };

        assert!(ensure_codex_command_hook(&mut doc, &hook).unwrap());
        assert!(!ensure_codex_command_hook(&mut doc, &hook).unwrap());

        let groups = doc["hooks"]["PermissionRequest"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(
            groups
                .iter()
                .filter(|group| table_has_potty_hook(group))
                .count(),
            1
        );

        let rendered = doc.to_string();
        assert!(rendered.contains("[[hooks.PermissionRequest]]"));
        assert!(rendered.contains("matcher = \"*\""));
        assert!(rendered.contains("[[hooks.PermissionRequest.hooks]]"));
        assert!(rendered.contains("command = \"/usr/bin/potty-notify --tool codex\""));
    }
}
