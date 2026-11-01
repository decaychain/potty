//! End to end: a real share endpoint, driven by the real `potty-notify --mcp` over JSON-RPC.
//! Runs on every platform (Unix sockets or Windows named pipes).

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::{Config, Term, test::TermSize};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use potty::share::{Event, Mode, Registry, serve};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

type SharedTerm = Arc<Mutex<Term<VoidListener>>>;

fn feed(term: &SharedTerm, bytes: &[u8]) {
    let mut p: Processor<StdSyncHandler> = Processor::new();
    p.advance(&mut *term.lock().unwrap(), bytes);
}

struct Mcp {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_potty-notify"))
            .arg("--mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            out,
            next: 1,
        }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id);
        v
    }

    fn tool(&mut self, name: &str, args: Value) -> (String, bool) {
        let v = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        let r = &v["result"];
        (
            r["content"][0]["text"].as_str().unwrap().to_string(),
            r["isError"].as_bool().unwrap(),
        )
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cursor_of(text: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix("cursor: "))
        .unwrap()
        .to_string()
}

#[test]
fn agent_reads_types_and_waits_through_the_mcp_server() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let instance = format!("{}-{nanos}", std::process::id());
    let registry = Arc::new(Registry::new(&instance));
    let term: SharedTerm = Arc::new(Mutex::new(Term::new(
        Config::default(),
        &TermSize::new(60, 10),
        VoidListener,
    )));
    feed(&term, b"user@prod:~$ ");
    // Stand-in for the pane: echo what is typed, and answer `uptime` once Enter arrives.
    let pane_term = term.clone();
    serve(
        registry.clone(),
        Arc::new(move |e| {
            if let Event::Input { pane: 3, bytes } = e {
                feed(&pane_term, &bytes);
                let line = String::from_utf8_lossy(&bytes).trim_end().to_string();
                if line.contains("__PTY_") {
                    // A pane_run line: execute it in a real shell, as the remote would.
                    let out = Command::new("sh").arg("-c").arg(&line).output().unwrap();
                    let out = String::from_utf8_lossy(&out.stdout).replace('\n', "\r\n");
                    feed(&pane_term, format!("\r\n{out}user@prod:~$ ").as_bytes());
                } else if bytes.ends_with(b"\r") {
                    feed(
                        &pane_term,
                        b"\n 10:00 up 3 days\r\n__POTTY_DONE_0__\r\nuser@prod:~$ ",
                    );
                }
            }
        }),
    )
    .unwrap();
    let id = registry
        .share(3, Mode::ReadOnly, term.clone(), Default::default())
        .unwrap();

    let mut mcp = Mcp::start();
    let init = mcp.rpc("initialize", json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(init["result"]["serverInfo"]["name"], "potty");
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("untrusted")
    );
    let tools = mcp.rpc("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "pane_snapshot",
            "pane_read",
            "pane_send",
            "pane_run",
            "pane_wait"
        ]
    );

    let (snap, err) = mcp.tool("pane_snapshot", json!({"share_id": id}));
    assert!(!err, "{snap}");
    assert!(snap.contains("mode: read-only"));
    assert!(snap.ends_with("user@prod:~$"));

    // Read-only refuses typing until the user flips the badge.
    let (msg, err) = mcp.tool("pane_send", json!({"share_id": id, "text": "uptime"}));
    assert!(err && msg.contains("read-only"), "{msg}");
    registry.set_mode(3, Mode::CanType);

    let (sent, err) = mcp.tool(
        "pane_send",
        json!({"share_id": id, "text": "uptime; printf '__PO''TTY_DONE_%s__\\n' \"$?\"", "enter": true}),
    );
    assert!(!err, "{sent}");
    let (out, err) = mcp.tool(
        "pane_wait",
        json!({"share_id": id, "since": cursor_of(&sent), "pattern": "__POTTY_DONE_\\d+__", "regex": true, "timeout_ms": 5000}),
    );
    assert!(!err, "{out}");
    assert!(out.contains("returned because: pattern"), "{out}");
    assert!(out.contains("up 3 days"));

    // The cursor from the wait continues where it left off.
    feed(&term, b"\r\nnew line\r\nuser@prod:~$ ");
    let (more, _) = mcp.tool(
        "pane_read",
        json!({"share_id": id, "since": cursor_of(&out)}),
    );
    assert!(
        more.contains("new line") && !more.contains("up 3 days"),
        "{more}"
    );

    let (run, err) = mcp.tool(
        "pane_run",
        json!({"share_id": id, "command": "printf 'a\\nb\\n'; (exit 4)", "timeout_ms": 5000}),
    );
    assert!(!err, "{run}");
    assert!(run.contains("exit status: 4"), "{run}");
    assert!(run.ends_with("--- output ---\na\nb"), "{run}");

    registry.unshare(3);
    let (msg, err) = mcp.tool("pane_snapshot", json!({"share_id": id}));
    assert!(err && msg.contains("ended"), "{msg}");
}

/// pane_run against interactive shells in a real PTY: real echo, prompt, and line editing, in a
/// narrow pane so the typed line wraps. Each shell runs once plain (end marker) and once with
/// OSC 133 integration (no marker typed), including a non-integrated subshell inside it.
#[cfg(unix)]
#[test]
fn pane_run_works_in_interactive_shells() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use potty::osc133::Scanner;
    use potty::share::{Marks, Op, PROTOCOL, Request, Response, feed, handle};
    use std::io::Read;

    let dir = std::env::temp_dir().join(format!("potty-share-shells-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bashrc = dir.join("bashrc");
    std::fs::write(
        &bashrc,
        "PS1='\\[\\e]133;A\\a\\]test$ \\[\\e]133;B\\a\\]'\nPS0='\\e]133;C\\a'\nPROMPT_COMMAND='printf \"\\e]133;D;%s\\a\" \"$?\"'\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".zshrc"),
        "PROMPT=$'%{\\e]133;A\\a%}test$ %{\\e]133;B\\a%}'\npreexec() { print -n '\\e]133;C\\a' }\nprecmd() { local s=$?; print -n \"\\e]133;D;$s\\a\" }\n",
    )
    .unwrap();
    let bashrc = bashrc.to_str().unwrap().to_string();

    struct Case {
        shell: &'static str,
        args: Vec<String>,
        integrated: bool,
    }
    let cases = [
        Case {
            shell: "bash",
            args: vec!["--norc".into(), "--noprofile".into(), "-i".into()],
            integrated: false,
        },
        Case {
            shell: "bash",
            args: vec![
                "--noprofile".into(),
                "--rcfile".into(),
                bashrc.clone(),
                "-i".into(),
            ],
            integrated: true,
        },
        Case {
            shell: "zsh",
            args: vec!["-f".into(), "-i".into()],
            integrated: false,
        },
        Case {
            shell: "zsh",
            args: vec!["-i".into()],
            integrated: true,
        },
    ];
    for case in cases {
        if Command::new(case.shell)
            .arg("-c")
            .arg("true")
            .status()
            .is_err()
        {
            continue;
        }
        let label = format!("{} integrated={}", case.shell, case.integrated);
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 12,
                cols: 40,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(case.shell);
        cmd.args(&case.args);
        cmd.env("PS1", "test$ ");
        cmd.env("PROMPT", "test$ ");
        cmd.env("TERM", "xterm-256color");
        cmd.env("ZDOTDIR", &dir);
        let mut child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);
        let term: SharedTerm = Arc::new(Mutex::new(Term::new(
            Config::default(),
            &TermSize::new(40, 12),
            VoidListener,
        )));
        let marks = Arc::new(Mutex::new(Marks::default()));
        let mut reader = pty.master.try_clone_reader().unwrap();
        let (reader_term, reader_marks) = (term.clone(), marks.clone());
        std::thread::spawn(move || {
            let mut p: Processor<StdSyncHandler> = Processor::new();
            let mut osc = Scanner::default();
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                feed(&mut p, &reader_term, &mut osc, &reader_marks, &buf[..n]);
            }
        });
        let writer = Mutex::new(pty.master.take_writer().unwrap());
        let typed = Mutex::new(Vec::<u8>::new());
        let registry = Registry::new("9-9");
        let id = registry
            .share(1, Mode::CanType, term.clone(), marks)
            .unwrap();
        let on_event = |e: Event| {
            if let Event::Input { bytes, .. } = e {
                typed.lock().unwrap().extend(&bytes);
                let mut w = writer.lock().unwrap();
                w.write_all(&bytes).unwrap();
                w.flush().unwrap();
            }
        };
        let ask = |op: Op| {
            handle(
                &Request {
                    v: PROTOCOL,
                    id: id.clone(),
                    op,
                },
                &registry,
                &on_event,
            )
        };
        let ready = ask(Op::Wait {
            since: None,
            pattern: Some("test$".into()),
            regex: false,
            quiet_ms: None,
            timeout_ms: Some(10_000),
            max_chars: None,
        });
        assert_eq!(
            ready.reason.as_deref(),
            Some("pattern"),
            "{label}: no prompt"
        );
        let run = |command: &str| -> Response {
            ask(Op::Run {
                command: command.into(),
                shell: None,
                timeout_ms: Some(10_000),
                max_chars: None,
            })
        };
        let first = run("echo one; echo two");
        assert_eq!(first.reason.as_deref(), Some("done"), "{label}: {first:?}");
        assert_eq!(first.exit_code, Some(0), "{label}");
        assert_eq!(first.text, "one\ntwo", "{label}");
        // The first run can't trust integration yet (no `C` seen before it): later ones do.
        let r = run("ls /definitely/not/here 2>/dev/null");
        assert_eq!(r.exit_code, Some(2), "{label}: {r:?}");
        assert_eq!(r.text, "", "{label}");
        let expect = if case.integrated { "osc133" } else { "marker" };
        assert_eq!(r.via.as_deref(), Some(expect), "{label}");
        let r = run("printf 'x%.0s' $(seq 1 60); echo");
        assert_eq!(r.text, "x".repeat(60), "{label}: wrapped output");
        if case.integrated {
            let before = typed.lock().unwrap().len();
            let r = run("echo clean");
            assert_eq!(r.text, "clean", "{label}");
            let typed_now = String::from_utf8_lossy(&typed.lock().unwrap()[before..]).to_string();
            assert!(
                !typed_now.contains("__PTY_"),
                "{label}: marker typed: {typed_now:?}"
            );
            // A shell without integration inside (like ssh or sudo -i): the marker takes over.
            ask(Op::Send {
                text: Some("sh".into()),
                keys: vec![],
                enter: true,
            });
            ask(Op::Wait {
                since: None,
                pattern: None,
                regex: false,
                quiet_ms: Some(300),
                timeout_ms: Some(5_000),
                max_chars: None,
            });
            let r = run("echo inside; false");
            assert_eq!(r.via.as_deref(), Some("marker"), "{label}");
            assert_eq!(
                (r.text.as_str(), r.exit_code),
                ("inside", Some(1)),
                "{label}: {r:?}"
            );
        }
        let _ = child.kill();
    }
}
