#![cfg(unix)]

use potty::agent::{Message, State};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    process::{Command, Stdio},
};

fn invoke(
    tool: &str,
    socket: &std::path::Path,
    args: &[&str],
    payload: serde_json::Value,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_potty-notify"))
        .args(["--tool", tool])
        .args(args)
        .env("POTTY_NOTIFY", socket)
        .env("POTTY_PANE", "17")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn both_provider_hooks_and_reports_use_the_same_identity() {
    for tool in ["codex", "claude"] {
        let socket =
            std::env::temp_dir().join(format!("potty-overview-test-{}.sock", potty::agent::now()));
        let listener = UnixListener::bind(&socket).unwrap();
        let read = || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut json = String::new();
            stream.read_to_string(&mut json).unwrap();
            Message::parse(json.trim()).unwrap()
        };
        let result = invoke(
            tool,
            &socket,
            &["--overview-hook"],
            serde_json::json!({"hook_event_name":"SessionStart", "session_id":"root-session", "cwd":"/work"}),
        );
        assert!(result.status.success());
        let start = read();
        assert_eq!(start.pane, Some(17));
        let context: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        let instructions = context["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(instructions.contains("--session 'root-session'"));
        assert!(instructions.contains("Do not include secrets"));

        let result = invoke(
            tool,
            &socket,
            &["--overview-hook"],
            serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":"root-session", "prompt":"Push the update"}),
        );
        assert!(result.status.success());
        assert_eq!(read().key(), start.key());
        let reminder: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            reminder["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert!(
            reminder["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("--session 'root-session'")
        );
        assert!(
            reminder.get("decision").is_none(),
            "reminders must not block the user prompt"
        );
        assert!(reminder.get("continue").is_none());

        let result = invoke(
            tool,
            &socket,
            &["--briefing", "--session", "root-session"],
            serde_json::json!({"title":"SSH restore", "summary":"Reproduced; testing a fix.", "next":"Run regressions", "milestones":["Reproduced"]}),
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stdout.is_empty());
        let report = read();
        assert_eq!(report.key(), start.key());
        assert_eq!(
            report.state_at, 0,
            "briefings must not overwrite hook status"
        );
        assert_eq!(report.briefing.unwrap().title, "SSH restore");

        let result = invoke(
            tool,
            &socket,
            &["--overview-hook"],
            serde_json::json!({"hook_event_name":"Stop", "session_id":"root-session", "last_assistant_message":"Please review"}),
        );
        assert!(result.status.success());
        assert!(result.stdout.is_empty());
        assert_eq!(read().state, State::ResponseReady);

        let result = invoke(
            tool,
            &socket,
            &["--briefing", "--session", "root-session"],
            serde_json::json!({"title":"x", "summary":"x", "unexpected":"bad"}),
        );
        assert!(!result.status.success());
        listener.set_nonblocking(true).unwrap();
        assert!(
            listener.accept().is_err(),
            "invalid reports must not be sent"
        );
        std::fs::remove_file(socket).unwrap();
    }
}

struct ClaudeHooks {
    dir: std::path::PathBuf,
    socket: std::path::PathBuf,
    listener: UnixListener,
}

impl ClaudeHooks {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("potty-stop-test-{}", potty::agent::now()));
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("notify.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        Self {
            dir,
            socket,
            listener,
        }
    }
    fn read(&self) -> potty::agent::Record {
        let (mut stream, _) = self.listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut json = String::new();
        stream.read_to_string(&mut json).unwrap();
        Message::parse(json.trim()).unwrap()
    }
    fn hook(&self, event: &str, mut fields: serde_json::Value) -> std::process::Output {
        fields["session_id"] = "root".into();
        fields["hook_event_name"] = event.into();
        let output = invoke("claude", &self.socket, &["--overview-hook"], fields.clone());
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let record = self.read();
        if event == "Stop" {
            assert_eq!(record.state, State::ResponseReady);
            if let Some(response) = fields["last_assistant_message"].as_str() {
                assert_eq!(record.response, response);
            }
        }
        output
    }
    fn prompt(&self) {
        self.hook(
            "UserPromptSubmit",
            serde_json::json!({"prompt":"Update the dashboard"}),
        );
    }
    fn work(&self) {
        self.hook("PreToolUse", serde_json::json!({"tool_name":"Edit"}));
    }
    fn stop(&self, continuing: bool) -> std::process::Output {
        self.hook(
            "Stop",
            serde_json::json!({"stop_hook_active":continuing, "last_assistant_message":"Updated."}),
        )
    }
    fn report(&self) {
        let output = invoke(
            "claude",
            &self.socket,
            &["--briefing", "--session", "root"],
            serde_json::json!({"title":"PTT-128: Dashboard", "summary":"Overview added to the merge request.", "next":"Waiting for CI."}),
        );
        assert!(output.status.success());
        self.read();
    }
}

impl Drop for ClaudeHooks {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn claude_stop_is_silent_with_missing_fresh_or_stale_briefings() {
    let h = ClaudeHooks::new();
    assert!(
        h.stop(false).stdout.is_empty(),
        "unknown turns must fail open"
    );
    h.prompt();
    assert!(h.stop(false).stdout.is_empty(), "trivial turn");

    h.prompt();
    h.work();
    assert!(
        h.stop(false).stdout.is_empty(),
        "a missing report must not obscure the final response"
    );
    h.work();
    assert!(
        h.stop(false).stdout.is_empty(),
        "duplicate Stop events must also stay silent"
    );
    assert!(h.stop(true).stdout.is_empty());

    h.prompt();
    h.work();
    h.report();
    h.hook("PostToolUse", serde_json::json!({"tool_name":"Bash"}));
    assert!(
        h.stop(false).stdout.is_empty(),
        "a fresh report needs no feedback"
    );

    h.prompt();
    h.report();
    h.work();
    assert!(
        h.stop(false).stdout.is_empty(),
        "a stale report must not cause a continuation"
    );

    h.prompt();
    h.work();
    assert!(
        h.stop(true).stdout.is_empty(),
        "never extend another Stop-hook continuation"
    );
    assert!(h.stop(false).stdout.is_empty());
    h.prompt();
    h.work();
    assert!(
        h.hook("Stop", serde_json::json!({})).stdout.is_empty(),
        "missing provider guard fails open"
    );

    h.prompt();
    let long_answer = "A".repeat(240);
    assert!(
        h.hook(
            "Stop",
            serde_json::json!({"stop_hook_active":false, "last_assistant_message":long_answer})
        )
        .stdout
        .is_empty(),
        "substantive text-only turns must also finish silently"
    );
    assert!(!h.socket.with_extension("overview-hooks").exists());
}

#[test]
fn failed_reports_and_missing_transport_do_not_force_retries() {
    let h = ClaudeHooks::new();
    h.prompt();
    h.work();
    let output = invoke(
        "claude",
        &h.socket,
        &["--briefing", "--session", "root"],
        serde_json::json!({"invalid":"report"}),
    );
    assert!(!output.status.success());
    assert!(
        h.stop(false).stdout.is_empty(),
        "do not ask for another attempt after validation failure"
    );

    std::fs::remove_file(&h.socket).unwrap();
    let output = invoke(
        "claude",
        &h.socket,
        &["--overview-hook"],
        serde_json::json!({"session_id":"root", "hook_event_name":"Stop", "stop_hook_active":false}),
    );
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "missing transport must not request model work"
    );
}

#[test]
fn simultaneous_stop_hooks_never_request_a_continuation() {
    let h = ClaudeHooks::new();
    h.prompt();
    h.work();
    let outputs = std::thread::scope(|scope| {
        let run = || {
            invoke(
                "claude",
                &h.socket,
                &["--overview-hook"],
                serde_json::json!({"session_id":"root", "hook_event_name":"Stop", "stop_hook_active":false}),
            )
        };
        let first = scope.spawn(run);
        let second = scope.spawn(run);
        [first.join().unwrap(), second.join().unwrap()]
    });
    h.read();
    h.read();
    assert!(outputs.iter().all(|o| o.status.success()));
    assert!(
        outputs
            .iter()
            .all(|o| o.stdout.is_empty() && o.stderr.is_empty())
    );
}

#[test]
fn codex_hooks_remain_advisory() {
    let h = ClaudeHooks::new();
    for (event, fields) in [
        (
            "UserPromptSubmit",
            serde_json::json!({"prompt":"Implement a dashboard"}),
        ),
        ("PreToolUse", serde_json::json!({"tool_name":"Bash"})),
        (
            "Stop",
            serde_json::json!({"stop_hook_active":false, "last_assistant_message":"A".repeat(240)}),
        ),
    ] {
        let mut payload = fields;
        payload["session_id"] = "codex-root".into();
        payload["hook_event_name"] = event.into();
        let output = invoke("codex", &h.socket, &["--overview-hook"], payload);
        assert!(output.status.success());
        h.read();
        if event == "Stop" {
            assert!(output.stdout.is_empty());
        }
    }
    assert!(!h.socket.with_extension("overview-hooks").exists());
}

#[test]
fn missing_transport_does_not_fail_hooks_or_inject_context() {
    let result = invoke(
        "codex",
        std::path::Path::new("/nonexistent/potty.sock"),
        &["--overview-hook"],
        serde_json::json!({"hook_event_name":"SessionStart", "session_id":"root"}),
    );
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
}
