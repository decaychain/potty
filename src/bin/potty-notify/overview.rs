use potty::{
    agent::{self, Briefing, Message},
    notify::{self, Tool},
};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
#[path = "briefing_relay.rs"]
mod briefing_relay;

const SKILL: &str = include_str!("../../../assets/skills/potty-briefing/SKILL.md");
// Only this exact unmodified prototype version may be upgraded without an installation baseline.
const LEGACY_SKILL: &str = include_str!("../../../assets/skills/legacy/potty-briefing-v1.md");
const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

pub fn run() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|s| s == flag)
            .and_then(|i| args.get(i + 1))
    };
    let relay_enabled = args.iter().any(|s| s == "--briefing-relay");
    if args.iter().any(|s| s == "--install-overview") && value("--install-overview").is_none() {
        eprintln!("Use --install-overview codex|claude");
        std::process::exit(2);
    }
    if let Some(target) = value("--install-overview") {
        if relay_enabled && target != "codex" {
            eprintln!("--briefing-relay is supported only for Codex overview hooks");
            std::process::exit(2);
        }
        if relay_enabled && !cfg!(unix) {
            eprintln!("--briefing-relay requires Unix");
            std::process::exit(2);
        }
        let home = super::home_dir();
        let dir = match target.as_str() {
            "codex" => std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or(home.join(".codex")),
            "claude" => std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or(home.join(".claude")),
            _ => {
                eprintln!("Use --install-overview codex|claude");
                std::process::exit(2);
            }
        };
        if let Err(e) = install(target, &dir, &super::exe_path(), relay_enabled) {
            eprintln!("potty-notify: {e}");
            std::process::exit(1);
        }
        println!(
            "Installed experimental overview hooks and skill in {}. Restart the agent; Codex may require /hooks trust approval.",
            dir.display()
        );
        if target == "codex" {
            println!(
                "Start Codex inside Potty with `codex --no-daemon`: a shared background server may not inherit this pane's POTTY_* environment."
            );
            if relay_enabled {
                println!(
                    "Enabled the data-only briefing file relay: trusted hooks forward validated session briefings to Potty. Sandbox permissions are unchanged. Review the changed hooks in /hooks."
                );
            }
        }
        return true;
    }
    let hook = args.iter().any(|s| s == "--overview-hook");
    let briefing = args.iter().any(|s| s == "--briefing");
    if !hook && !briefing {
        return false;
    }
    let result = (|| -> Result<(), String> {
        let tool = match value("--tool").map(String::as_str) {
            Some("codex") => Tool::Codex,
            Some("claude") => Tool::Claude,
            _ => return Err("--tool codex|claude is required".into()),
        };
        if relay_enabled && (tool != Tool::Codex || !hook) {
            return Err("--briefing-relay requires a Codex overview hook".into());
        }
        if relay_enabled && !cfg!(unix) {
            return Err("briefing relay requires Unix".into());
        }
        if hook && briefing {
            return Err("--briefing and --overview-hook cannot be combined".into());
        }
        let mut payload = String::new();
        std::io::stdin()
            .take(agent::MAX_WIRE as u64 + 1)
            .read_to_string(&mut payload)
            .map_err(|e| e.to_string())?;
        if payload.len() > agent::MAX_WIRE {
            return Err("payload too large".into());
        }
        let v: serde_json::Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
        if args.iter().any(|s| s == "--briefing-file") {
            if !briefing || tool != Tool::Codex {
                return Err("--briefing-file requires a Codex briefing publication".into());
            }
            let path = value("--briefing-file").ok_or("--briefing-file requires a path")?;
            let session = value("--session").ok_or("--session is required")?;
            let briefing: Briefing = serde_json::from_value(v).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            return briefing_relay::Relay::stage(Path::new(path), session, briefing)
                .map_err(|e| format!("cannot queue briefing: {e}"));
            #[cfg(not(unix))]
            return Err("briefing relay requires Unix".into());
        }
        let Some(socket) = std::env::var_os(notify::ENV_SOCK) else {
            return if hook {
                Ok(())
            } else {
                Err("POTTY_NOTIFY is not set".into())
            };
        };
        let origin = socket.to_string_lossy().into_owned();
        let pane = std::env::var(notify::ENV_PANE)
            .ok()
            .and_then(|s| s.parse().ok());
        let record = if briefing {
            let b: Briefing = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            if !b.valid() {
                return Err("invalid briefing fields or size limits".into());
            }
            let session = value("--session").ok_or("--session is required")?;
            let identity = serde_json::json!({
                "session_id": session,
                "hook_event_name": "SessionStart",
                "cwd": std::env::current_dir().ok(),
            });
            let mut record = agent::hook(tool, &identity, origin, super::hostname(), pane)
                .ok_or("invalid session identity")?;
            record.state_at = 0;
            record.background_at = 0;
            record.briefing_at = agent::now();
            record.briefing = Some(b);
            record
        } else {
            let Some(record) = agent::hook(tool, &v, origin, super::hostname(), pane) else {
                return Ok(());
            };
            record
        };
        send(Path::new(&socket), &Message::json(record.clone())).map_err(|e| e.to_string())?;
        let event = v
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let mut command = format!(
            "{} --tool {} --briefing --session {}",
            quote(&super::exe_path()),
            if tool == Tool::Codex {
                "codex"
            } else {
                "claude"
            },
            quote(&record.session)
        );
        #[cfg(unix)]
        if hook
            && relay_enabled
            && matches!(
                event,
                "SessionStart"
                    | "UserPromptSubmit"
                    | "PostToolUse"
                    | "Stop"
                    | "SessionEnd"
                    | "Interrupt"
            )
        {
            let relay = briefing_relay::Relay::prepare(&record).map_err(|e| e.to_string())?;
            if matches!(event, "SessionStart" | "UserPromptSubmit") {
                command.push_str(&format!(
                    " --briefing-file {}",
                    quote(&relay.path().to_string_lossy())
                ));
            } else {
                relay
                    .flush(&record, |update| {
                        send(Path::new(&socket), &Message::json(update))
                    })
                    .map_err(|e| e.to_string())?;
            }
        }
        if hook && matches!(event, "SessionStart" | "UserPromptSubmit") {
            let context = reporting_context(event, &command);
            println!(
                "{}",
                serde_json::json!({"hookSpecificOutput":{"hookEventName":event, "additionalContext":context}})
            );
        }
        Ok(())
    })();
    // Hook errors never block agent progress; explicit reports surface failures.
    if briefing && let Err(e) = result {
        eprintln!("potty-notify: {e}");
        std::process::exit(1);
    }
    true
}

fn reporting_context(event: &str, command: &str) -> String {
    let instructions = if event == "SessionStart" {
        SKILL
    } else {
        "Before every final response in this configured Potty session, use potty-briefing to publish one concise \
         task briefing with the supplied command, including on discussion-only and unchanged turns. \
         Maintain the Potty task briefing across turns, not a recap of this reply. \
         Keep the overall goal and any known ticket in title; summary is accumulated progress and current state; \
         next is the next action, external wait, or user decision. Preserve the goal on follow-ups unless it changes. \
         Keep it scannable: short title, one short sentence of progress, one short next action or wait; \
         aim for at most 60 words total across these three fields. No paragraphs, lists, or repeated context in them. \
         Milestones are optional: at most 3 short phrases, omitting detail already in the summary. \
         Send one JSON object on stdin: {\"title\":\"...\",\"summary\":\"...\",\"next\":\"...\",\"milestones\":[\"...\"]}. \
         Preserve any --briefing-file argument: it queues the report for the installed hook without opening a socket. \
         A successful queue operation is not a delivery acknowledgement. \
         Limits: title 120, summary 1200, next 600 UTF-8 bytes; up to 5 milestones of 300 bytes each. \
         Use only known facts and no secrets. Quote JSON safely; do not interpolate it into shell commands. \
         If publication fails or requires additional permission, skip it and answer normally. \
         Do not retry, investigate, or call another model solely to report. \
         Do not announce routine briefing publication in the final response. \
         Subagents must not publish for the parent."
    };
    format!("Potty reporting command for THIS session: {command}\n{instructions}")
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn send(path: &Path, json: &str) -> std::io::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_write_timeout(Some(std::time::Duration::from_millis(250)))?;
    stream.write_all(format!("{json}\n").as_bytes())
}
#[cfg(not(unix))]
fn send(_path: &Path, _json: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("Unix sockets required"))
}

fn install(tool: &str, dir: &Path, exe: &str, relay: bool) -> std::io::Result<()> {
    let direct = format!("{} --tool {tool} --overview-hook", quote(exe));
    let relayed = format!("{direct} --briefing-relay");
    let command = if relay { &relayed } else { &direct };
    let mut events = EVENTS.to_vec();
    events.push(if tool == "codex" {
        "Interrupt"
    } else {
        "StopFailure"
    });
    let path = dir.join(if tool == "codex" {
        "config.toml"
    } else {
        "settings.json"
    });
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = if tool == "codex" {
        let mut doc: toml_edit::DocumentMut = existing.parse().map_err(std::io::Error::other)?;
        for event in events {
            if let Some(groups) = doc
                .as_table_mut()
                .get_mut("hooks")
                .and_then(toml_edit::Item::as_table_mut)
                .and_then(|h| h.get_mut(event))
                .and_then(toml_edit::Item::as_array_of_tables_mut)
            {
                for group in groups.iter_mut() {
                    if let Some(handlers) = group
                        .get_mut("hooks")
                        .and_then(toml_edit::Item::as_array_of_tables_mut)
                    {
                        for handler in handlers.iter_mut() {
                            let old = if relay { &direct } else { &relayed };
                            if handler.get("command").and_then(toml_edit::Item::as_str)
                                == Some(old.as_str())
                            {
                                handler["command"] = toml_edit::value(command.as_str());
                            }
                        }
                    }
                }
            }
            // Existing notification hooks remain separate and keep their own semantics.
            add_codex_hook(&mut doc, event, command)?;
        }
        doc.to_string()
    } else {
        let mut root: serde_json::Value = if existing.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&existing)?
        };
        let obj = root
            .as_object_mut()
            .ok_or_else(|| std::io::Error::other("settings must be an object"))?;
        let hooks = obj
            .entry("hooks")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| std::io::Error::other("hooks must be an object"))?;
        for event in events {
            let groups = hooks
                .entry(event)
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .ok_or_else(|| std::io::Error::other("hook groups must be arrays"))?;
            if !groups.iter().any(|g| {
                g.get("hooks").and_then(|v| v.as_array()).is_some_and(|hs| {
                    hs.iter().any(|h| {
                        h.get("command").and_then(|v| v.as_str()) == Some(command.as_str())
                    })
                })
            }) {
                groups.push(serde_json::json!({"hooks":[{"type":"command", "command":command, "timeout":2}]}));
            }
        }
        format!("{}\n", serde_json::to_string_pretty(&root)?)
    };
    let skill = dir.join("skills/potty-briefing/SKILL.md");
    let baseline = dir.join("skills/potty-briefing/.potty-installed-skill");
    // Do not overwrite a user-customized skill.
    if skill.exists() {
        let installed = std::fs::read_to_string(&skill)?;
        let previous = match std::fs::read_to_string(&baseline) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if installed != SKILL
            && installed != LEGACY_SKILL
            && previous.as_deref() != Some(&installed)
        {
            return Err(std::io::Error::other(
                "existing potty-briefing skill is customized; left configuration unchanged",
            ));
        }
    }
    std::fs::create_dir_all(skill.parent().unwrap())?;
    std::fs::write(&skill, SKILL)?;
    std::fs::write(&baseline, SKILL)?;
    if existing != updated {
        std::fs::write(path, updated)?;
    }
    Ok(())
}

fn add_codex_hook(
    doc: &mut toml_edit::DocumentMut,
    event: &str,
    command: &str,
) -> std::io::Result<()> {
    use toml_edit::{ArrayOfTables, Item, Table, value};
    let hooks = doc
        .entry("hooks")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| std::io::Error::other("hooks must be a table"))?;
    let groups = hooks
        .entry(event)
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| std::io::Error::other("hook groups must be tables"))?;
    if groups.iter().any(|g| {
        g.get("hooks")
            .and_then(Item::as_array_of_tables)
            .is_some_and(|hs| {
                hs.iter()
                    .any(|h| h.get("command").and_then(Item::as_str) == Some(command))
            })
    }) {
        return Ok(());
    }
    let mut group = Table::new();
    let mut handlers = ArrayOfTables::new();
    let mut handler = Table::new();
    handler["type"] = value("command");
    handler["command"] = value(command);
    handler["timeout"] = value(2);
    handlers.push(handler);
    group["hooks"] = Item::ArrayOfTables(handlers);
    groups.push(group);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skill_upgrades_preserve_customizations() {
        for tool in ["codex", "claude"] {
            let dir = std::env::temp_dir().join(format!("potty-skill-upgrade-{}", agent::now()));
            let skill = dir.join("skills/potty-briefing/SKILL.md");
            std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
            std::fs::write(&skill, LEGACY_SKILL).unwrap();
            install(tool, &dir, "/tmp/potty-notify", false).unwrap();
            assert_eq!(std::fs::read_to_string(&skill).unwrap(), SKILL);
            let baseline = skill.with_file_name(".potty-installed-skill");
            let previous = "Previous unmodified bundled skill\n";
            std::fs::write(&skill, previous).unwrap();
            std::fs::write(&baseline, previous).unwrap();
            install(tool, &dir, "/tmp/potty-notify", false).unwrap();
            assert_eq!(std::fs::read_to_string(&skill).unwrap(), SKILL);
            assert_eq!(std::fs::read_to_string(&baseline).unwrap(), SKILL);
            let config = dir.join(if tool == "codex" {
                "config.toml"
            } else {
                "settings.json"
            });
            let before = std::fs::read_to_string(&config).unwrap();
            let customized = format!("{SKILL}\nUser customization\n");
            std::fs::write(&skill, &customized).unwrap();
            assert!(install(tool, &dir, "/tmp/new/potty-notify", false).is_err());
            assert_eq!(std::fs::read_to_string(&skill).unwrap(), customized);
            assert_eq!(std::fs::read_to_string(config).unwrap(), before);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn installers_preserve_other_hooks_and_are_idempotent() {
        for tool in ["codex", "claude"] {
            let temp =
                std::env::temp_dir().join(format!("potty-installer-{}-{tool}", agent::now()));
            std::fs::create_dir_all(&temp).unwrap();
            let path = temp.join(if tool == "codex" {
                "config.toml"
            } else {
                "settings.json"
            });
            let original = if tool == "codex" {
                "# preserve me\nmodel = 'example'\n[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype='command'\ncommand='other-hook'\n"
            } else {
                r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-hook"}]}]},"theme":"dark"}"#
            };
            std::fs::write(&path, original).unwrap();
            install(tool, &temp, "/tmp/a 'b/potty-notify", false).unwrap();
            let first = std::fs::read_to_string(&path).unwrap();
            assert!(first.contains("other-hook"));
            install(tool, &temp, "/tmp/a 'b/potty-notify", false).unwrap();
            assert_eq!(first, std::fs::read_to_string(&path).unwrap());
            std::fs::remove_dir_all(temp).unwrap();
        }
    }

    #[test]
    fn codex_relay_opt_in_updates_existing_hooks_without_duplicates_or_permission_changes() {
        let dir = std::env::temp_dir().join(format!("potty-relay-install-{}", agent::now()));
        std::fs::create_dir(&dir).unwrap();
        let config = dir.join("config.toml");
        let original =
            "sandbox_mode = 'workspace-write'\n[sandbox_workspace_write]\nnetwork_access = false\n";
        std::fs::write(&config, original).unwrap();
        install("codex", &dir, "/tmp/potty-notify", false).unwrap();
        for relay in [true, true, false] {
            install("codex", &dir, "/tmp/potty-notify", relay).unwrap();
            let text = std::fs::read_to_string(&config).unwrap();
            assert!(text.starts_with(original));
            let doc: toml_edit::DocumentMut = text.parse().unwrap();
            for event in EVENTS.iter().copied().chain(["Interrupt"]) {
                let groups = doc["hooks"][event].as_array_of_tables().unwrap();
                assert_eq!(groups.len(), 1);
                let handlers = groups.get(0).unwrap()["hooks"]
                    .as_array_of_tables()
                    .unwrap();
                assert_eq!(handlers.len(), 1);
                assert_eq!(
                    handlers.get(0).unwrap()["command"]
                        .as_str()
                        .unwrap()
                        .ends_with("--briefing-relay"),
                    relay
                );
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn notification_installer_does_not_mistake_overview_hooks_for_notifications() {
        let mut doc = toml_edit::DocumentMut::new();
        add_codex_hook(
            &mut doc,
            "PermissionRequest",
            "potty-notify --tool codex --overview-hook",
        )
        .unwrap();
        let hook = super::super::CodexHook {
            event: "PermissionRequest",
            matcher: Some("*"),
            command: "potty-notify --tool codex".into(),
        };
        assert!(super::super::ensure_codex_command_hook(&mut doc, &hook).unwrap());
        assert!(!super::super::ensure_codex_command_hook(&mut doc, &hook).unwrap());
        assert_eq!(
            doc["hooks"]["PermissionRequest"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            2
        );
    }
}
