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
                if bytes.ends_with(b"\r") {
                    feed(
                        &pane_term,
                        b"\n 10:00 up 3 days\r\n__POTTY_DONE_0__\r\nuser@prod:~$ ",
                    );
                }
            }
        }),
    )
    .unwrap();
    let id = registry.share(3, Mode::ReadOnly, term.clone()).unwrap();

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
        ["pane_snapshot", "pane_read", "pane_send", "pane_wait"]
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

    registry.unshare(3);
    let (msg, err) = mcp.tool("pane_snapshot", json!({"share_id": id}));
    assert!(err && msg.contains("ended"), "{msg}");
}
