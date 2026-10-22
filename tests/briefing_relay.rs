#![cfg(unix)]

use potty::agent::{Message, Record, State};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    dir: PathBuf,
    socket: PathBuf,
    listener: UnixListener,
}

fn run(mut command: Command, payload: Value) -> Output {
    let mut child = command
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

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("potty-relay-test-{}", potty::agent::now()));
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("notify.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            dir,
            socket,
            listener,
        }
    }
    fn helper(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_potty-notify"));
        cmd.env("TMPDIR", &self.dir)
            .env("POTTY_NOTIFY", &self.socket)
            .env("POTTY_PANE", "17");
        cmd.args(["--tool", "codex"]);
        cmd
    }
    fn hook(&self, event: &str, session: &str) -> Output {
        let mut cmd = self.helper();
        cmd.args(["--overview-hook", "--briefing-relay"]);
        let output = run(
            cmd,
            json!({"hook_event_name":event, "session_id":session, "cwd":"/work", "last_assistant_message":"Done"}),
        );
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        output
    }
    fn start(&self, session: &str) -> String {
        let output = self.hook("SessionStart", session);
        assert_eq!(self.read().session, session);
        let output: Value = serde_json::from_slice(&output.stdout).unwrap();
        output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .strip_prefix("Potty reporting command for THIS session: ")
            .unwrap()
            .to_owned()
    }
    fn stage(&self, command: &str, sandbox: bool) -> Output {
        let mut cmd = if sandbox {
            let mut cmd = Command::new("codex");
            cmd.args(["sandbox", "-P", ":workspace", "--", "/bin/sh", "-c"]);
            cmd.env("CODEX_HOME", &self.dir);
            cmd
        } else {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c");
            cmd
        };
        cmd.arg(command)
            .env_remove("POTTY_NOTIFY")
            .env_remove("POTTY_PANE");
        run(
            cmd,
            json!({"title":"Relay test", "summary":"A sandboxed report was queued.", "next":"Review"}),
        )
    }
    fn read(&self) -> Record {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("missing relay message: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut json = String::new();
        stream.read_to_string(&mut json).unwrap();
        Message::parse(json.trim()).unwrap()
    }
    fn empty(&self) {
        assert!(self.listener.accept().is_err());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn queued_reports_are_session_scoped_and_stop_flushes_without_feedback() {
    let h = Fixture::new();
    let command = h.start("one ' quoted session");
    assert!(command.contains("--briefing-file"));
    h.start("two");
    let staged = h.stage(&command, false);
    assert!(
        staged.status.success(),
        "{}",
        String::from_utf8_lossy(&staged.stderr)
    );
    assert!(staged.stdout.is_empty());
    h.empty();
    assert!(h.hook("PostToolUse", "two").stdout.is_empty());
    assert_eq!(h.read().session, "two");
    h.empty();
    assert!(h.hook("Stop", "one ' quoted session").stdout.is_empty());
    let status = h.read();
    assert_eq!(status.state, State::ResponseReady);
    let report = h.read();
    assert_eq!(report.key(), status.key());
    assert_eq!(report.pane, Some(17));
    assert_eq!(report.state_at, 0);
    assert_eq!(report.briefing.unwrap().title, "Relay test");
    h.empty();
    assert!(h.hook("Stop", "one ' quoted session").stdout.is_empty());
    h.read();
    h.empty();
}

#[test]
fn post_tool_use_delivers_and_legacy_hooks_do_not_consume_the_outbox() {
    let h = Fixture::new();
    let command = h.start("one");
    assert!(h.stage(&command, false).status.success());
    let mut direct = h.helper();
    direct.arg("--overview-hook");
    assert!(
        run(
            direct,
            json!({"hook_event_name":"Stop", "session_id":"one"})
        )
        .stdout
        .is_empty()
    );
    h.read();
    h.empty();
    assert!(h.hook("PostToolUse", "one").stdout.is_empty());
    h.read();
    assert!(h.read().briefing.is_some());
    h.empty();
}

#[test]
fn subagents_and_invalid_publications_never_relay() {
    let h = Fixture::new();
    let command = h.start("one");
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &command]);
    assert!(
        !run(
            cmd,
            json!({"title":"x", "summary":"x", "socket":"elsewhere"})
        )
        .status
        .success()
    );
    assert!(h.hook("Stop", "one").stdout.is_empty());
    h.read();
    h.empty();
    assert!(h.stage(&command, false).status.success());
    let mut cmd = h.helper();
    cmd.args(["--overview-hook", "--briefing-relay"]);
    let output = run(
        cmd,
        json!({"hook_event_name":"Stop", "session_id":"one", "agent_id":"subagent"}),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    h.empty();
    h.hook("Stop", "one");
    h.read();
    assert!(h.read().briefing.is_some());
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "requires installed Codex CLI and permission to start its Linux sandbox"]
fn codex_workspace_sandbox_blocks_direct_socket_but_allows_file_relay() {
    let h = Fixture::new();
    let command = h.start("sandbox-test");
    let mut direct = Command::new("codex");
    direct
        .args([
            "sandbox",
            "-P",
            ":workspace",
            "--",
            env!("CARGO_BIN_EXE_potty-notify"),
            "--tool",
            "codex",
            "--briefing",
            "--session",
            "sandbox-test",
        ])
        .env("CODEX_HOME", &h.dir)
        .env("POTTY_NOTIFY", &h.socket);
    let denied = run(
        direct,
        json!({"title":"Direct", "summary":"Must be blocked"}),
    );
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("Operation not permitted"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    h.empty();
    let staged = h.stage(&command, true);
    assert!(
        staged.status.success(),
        "{}",
        String::from_utf8_lossy(&staged.stderr)
    );
    assert!(staged.stdout.is_empty());
    h.empty();
    assert!(h.hook("PostToolUse", "sandbox-test").stdout.is_empty());
    h.read();
    assert_eq!(h.read().briefing.unwrap().title, "Relay test");
    h.empty();
}
