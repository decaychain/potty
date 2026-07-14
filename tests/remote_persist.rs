//! Persistence tests (step 4):
//!   - a shell's process survives the client disconnecting (the daemon keeps it alive), and
//!   - reattaching restores the pane and replays its current screen.
//!
//! Unix-only; the first skips if `pgrep` is unavailable.
#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use potty::notify::{Kind, Note, SCHEMA_VERSION, Tool};
use potty::proto::{Control, Frame, Layout, LayoutNode, LayoutTab};

/// What a client's reader thread accumulates: pane output keyed by daemon pane id, plus every
/// control frame in arrival order.
type Collected =
    std::sync::Arc<std::sync::Mutex<(std::collections::HashMap<u64, Vec<u8>>, Vec<Control>)>>;

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn pgrep(needle: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", needle])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn notify_socket(sock: &Path) -> std::path::PathBuf {
    sock.with_extension("notify.sock")
}

fn note(session: &str, kind: Kind) -> Note {
    Note {
        v: SCHEMA_VERSION,
        tool: Tool::Codex,
        kind,
        session: session.to_string(),
        message: "waiting".to_string(),
        cwd: "/tmp".to_string(),
        host: "remote-test".to_string(),
        pid: Some(std::process::id()),
        pane: Some(1),
        instance: None,
        zellij: None,
        ts: 1,
    }
}

fn send_note(sock: &Path, session: &str, kind: Kind) {
    let note = note(session, kind);
    let mut stream = UnixStream::connect(notify_socket(sock)).expect("connect notify socket");
    let mut line = serde_json::to_string(&note).expect("serialize note");
    line.push('\n');
    use std::io::Write;
    stream.write_all(line.as_bytes()).expect("write note");
}

/// Spawn the daemon directly on `sock` (bypassing the attach relay) and wait until it's listening.
fn start_daemon(sock: &Path) -> Child {
    let _ = std::fs::remove_file(sock);
    let child = Command::new(env!("CARGO_BIN_EXE_potty-session"))
        .arg("--daemon")
        .arg(sock)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    assert!(
        wait_until(Duration::from_secs(5), || sock.exists()),
        "daemon never bound its socket"
    );
    assert!(
        wait_until(Duration::from_secs(5), || notify_socket(sock).exists()),
        "daemon never bound its notify socket"
    );
    child
}

/// A direct client connection to the daemon, with a background thread demuxing its output.
struct Client {
    stream: UnixStream,
    collected: Collected,
}

impl Client {
    fn connect(sock: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(sock) {
                Ok(stream) => break stream,
                Err(err) if Instant::now() < deadline => {
                    let _ = err;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("connect to daemon: {err}"),
            }
        };
        let mut read = stream.try_clone().unwrap();
        let collected: Collected = std::sync::Arc::new(std::sync::Mutex::new((
            std::collections::HashMap::new(),
            Vec::new(),
        )));
        let sink = collected.clone();
        std::thread::spawn(move || {
            while let Ok(Some(frame)) = Frame::read(&mut read) {
                let mut g = sink.lock().unwrap();
                match frame {
                    Frame::Data { pane, bytes } => g.0.entry(pane).or_default().extend(bytes),
                    Frame::Control(c) => g.1.push(c),
                }
            }
        });
        Client { stream, collected }
    }

    fn send(&mut self, f: Frame) {
        f.write(&mut self.stream).expect("write frame");
    }

    fn wait(
        &self,
        pred: impl Fn(&std::collections::HashMap<u64, Vec<u8>>, &[Control]) -> bool,
    ) -> bool {
        wait_until(Duration::from_secs(10), || {
            let g = self.collected.lock().unwrap();
            pred(&g.0, &g.1)
        })
    }

    /// Disconnect the way a dropped SSH channel does: shut the socket down fully so the daemon
    /// sees EOF. (A plain drop wouldn't — the reader thread holds a cloned fd of the same socket,
    /// and the real attach relay closes all its fds at once via process exit.)
    fn disconnect(self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// A client connected through the real attach relay (the process SSH execs), not directly to the
/// daemon socket. This covers the production detach path: dropping stdin makes the relay exit and
/// leaves the daemon behind.
struct RelayClient {
    child: Child,
    stdin: Option<ChildStdin>,
    collected: Collected,
}

impl RelayClient {
    fn connect(sock: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_potty-session"))
            .env("POTTY_SESSION_SOCK", sock)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn attach relay");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let collected: Collected = std::sync::Arc::new(std::sync::Mutex::new((
            std::collections::HashMap::new(),
            Vec::new(),
        )));
        let sink = collected.clone();
        std::thread::spawn(move || {
            while let Ok(Some(frame)) = Frame::read(&mut stdout) {
                let mut g = sink.lock().unwrap();
                match frame {
                    Frame::Data { pane, bytes } => g.0.entry(pane).or_default().extend(bytes),
                    Frame::Control(c) => g.1.push(c),
                }
            }
        });
        RelayClient {
            child,
            stdin: Some(stdin),
            collected,
        }
    }

    fn send(&mut self, f: Frame) {
        f.write(self.stdin.as_mut().expect("relay stdin"))
            .expect("write frame");
    }

    fn wait(
        &self,
        pred: impl Fn(&std::collections::HashMap<u64, Vec<u8>>, &[Control]) -> bool,
    ) -> bool {
        wait_until(Duration::from_secs(10), || {
            let g = self.collected.lock().unwrap();
            pred(&g.0, &g.1)
        })
    }

    fn disconnect(mut self) {
        drop(self.stdin.take());
        let exited = wait_until(Duration::from_secs(5), || {
            matches!(self.child.try_wait(), Ok(Some(_)))
        });
        if !exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn shell_survives_client_disconnect() {
    if !have("pgrep") {
        eprintln!("skipping: pgrep unavailable");
        return;
    }
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-persist-{tag}.sock"));
    let marker = format!("8{tag}"); // `sleep 8<pid>` — grep-able, unique, valid
    let daemon = start_daemon(&sock);

    let mut c = Client::connect(&sock);
    c.send(Frame::Control(Control::Hello { version: 1 }));
    c.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    c.send(Frame::Data {
        pane: 1,
        bytes: format!("sleep {marker}\r").into_bytes(),
    });

    assert!(
        wait_until(Duration::from_secs(10), || pgrep(&marker)),
        "marker process never started"
    );
    c.disconnect();

    std::thread::sleep(Duration::from_millis(500));
    let survived = pgrep(&marker);

    let _ = Command::new("pkill").args(["-f", &marker]).status();
    cleanup(daemon, &sock);

    assert!(
        survived,
        "the shell's process did not survive the client disconnect"
    );
}

#[test]
fn relay_detach_keeps_foreground_process_and_reattaches() {
    if !have("pgrep") {
        eprintln!("skipping: pgrep unavailable");
        return;
    }
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-relay-persist-{tag}.sock"));
    let marker = format!("POTTY_RELAY_MARKER_{tag}");
    let _ = std::fs::remove_file(&sock);

    let mut c1 = RelayClient::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 1 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    c1.send(Frame::Data {
        pane: 1,
        bytes: format!("sh -c 'while true; do echo {marker}; sleep 1; done'\r").into_bytes(),
    });
    assert!(
        wait_until(Duration::from_secs(10), || pgrep(&marker)),
        "marker process never started"
    );
    assert!(
        c1.wait(|out, _| out.get(&1).is_some_and(|o| contains(o, marker.as_bytes()))),
        "marker never reached relay client #1"
    );
    c1.disconnect();

    std::thread::sleep(Duration::from_millis(500));
    let survived = pgrep(&marker);

    let mut c2 = RelayClient::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 1 }));
    let restored = c2.wait(|out, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Restore { pane: 1 }))
            && ctrl.iter().any(|m| matches!(m, Control::Ready))
            && out.get(&1).is_some_and(|o| contains(o, marker.as_bytes()))
    });
    c2.send(Frame::Control(Control::Close { pane: 1 }));
    c2.disconnect();
    let _ = Command::new("pkill").args(["-f", &marker]).status();
    let _ = std::fs::remove_file(&sock);

    assert!(
        survived,
        "the foreground process did not survive relay detach"
    );
    assert!(restored, "relay reattach did not restore the running pane");
}

#[test]
fn reattach_restores_and_replays() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-reattach-{tag}.sock"));
    let marker = format!("REATTACH_MARKER_{tag}");
    let daemon = start_daemon(&sock);

    // Client #1: open a pane and leave the marker on its screen, then disconnect.
    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 1 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    c1.send(Frame::Data {
        pane: 1,
        bytes: format!("echo {marker}\r").into_bytes(),
    });
    let echoed = c1.wait(|out, _| out.get(&1).is_some_and(|o| contains(o, marker.as_bytes())));
    assert!(echoed, "marker never echoed on client #1");
    c1.disconnect();
    std::thread::sleep(Duration::from_millis(200));

    // Client #2: a fresh connection. Its Hello should restore pane 1 and replay its screen.
    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 1 }));
    let restored = c2.wait(|out, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Restore { pane: 1 }))
            && ctrl.iter().any(|m| matches!(m, Control::Ready))
            && out.get(&1).is_some_and(|o| contains(o, marker.as_bytes()))
    });
    c2.disconnect();
    cleanup(daemon, &sock);

    assert!(
        restored,
        "reattach did not restore pane 1 with its replayed screen"
    );
}

#[test]
fn second_attach_joins_without_evicting_first() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-multi-{tag}.sock"));
    let marker = format!("MULTI_ATTACH_MARKER_{tag}");
    let marker2 = format!("MULTI_ATTACH_SECOND_{tag}");
    let daemon = start_daemon(&sock);

    // Client #1 is attached and has a live shell.
    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 2 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    c1.send(Frame::Data {
        pane: 1,
        bytes: format!("echo {marker}\r").into_bytes(),
    });
    let echoed = c1.wait(|out, _| out.get(&1).is_some_and(|o| contains(o, marker.as_bytes())));
    assert!(echoed, "marker never echoed on client #1");

    // Client #2 joins alongside: it gets the pane (size + replay) without evicting client #1.
    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 2 }));
    let restored = c2.wait(|out, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Restore { pane: 1 }))
            && ctrl
                .iter()
                .any(|m| matches!(m, Control::Resize { pane: 1, .. }))
            && ctrl.iter().any(|m| matches!(m, Control::Ready))
            && out.get(&1).is_some_and(|o| contains(o, marker.as_bytes()))
    });
    assert!(
        restored,
        "a second attach did not restore the live session alongside the first"
    );

    // Client #1 is still live: its input works, and the output is broadcast to both clients.
    c1.send(Frame::Data {
        pane: 1,
        bytes: format!("echo {marker2}\r").into_bytes(),
    });
    let both_saw = c1.wait(|out, _| out.get(&1).is_some_and(|o| contains(o, marker2.as_bytes())))
        && c2.wait(|out, _| out.get(&1).is_some_and(|o| contains(o, marker2.as_bytes())));

    c1.disconnect();
    c2.disconnect();
    cleanup(daemon, &sock);

    assert!(
        both_saw,
        "output after the second attach did not reach both clients"
    );
}

#[test]
fn focus_follows_input_and_gates_layout_and_resize() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-focus-{tag}.sock"));
    let daemon = start_daemon(&sock);

    // Client #1 attaches first and opens a pane — it holds the focus.
    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 2 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(
        c1.wait(|_, ctrl| ctrl
            .iter()
            .any(|m| matches!(m, Control::Opened { pane: 1 }))),
        "pane did not open"
    );

    // Client #2 joins as a follower and learns who owns the focus (client #1's id).
    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 2 }));
    assert!(
        c2.wait(|_, ctrl| ctrl.iter().any(|m| matches!(m, Control::Ready))),
        "second client never became ready"
    );
    let (my_id, owner) = {
        let g = c2.collected.lock().unwrap();
        let my_id = g.1.iter().find_map(|m| match m {
            Control::Welcome { client, .. } => Some(*client),
            _ => None,
        });
        let owner = g.1.iter().rev().find_map(|m| match m {
            Control::Focus { owner } => Some(*owner),
            _ => None,
        });
        (my_id, owner)
    };
    let my_id = my_id.expect("no Welcome with a client id");
    let owner = owner.expect("no Focus frame during attach");
    assert_ne!(owner, 0, "attach burst did not name a focus owner");
    assert_ne!(
        owner, my_id,
        "the follower must not steal focus by attaching"
    );

    // A layout push from the follower is a stale echo: dropped, and it doesn't flip focus.
    let bad = serde_json::to_string(&Layout {
        tabs: vec![LayoutTab {
            root: LayoutNode::Leaf { pane: 999 },
            focus: None,
        }],
    })
    .unwrap();
    c2.send(Frame::Control(Control::LayoutTree { json: bad.clone() }));

    // Typing flips focus to the follower (both clients hear about it)...
    c2.send(Frame::Data {
        pane: 1,
        bytes: b"true\r".to_vec(),
    });
    assert!(
        c1.wait(|_, ctrl| ctrl
            .iter()
            .any(|m| matches!(m, Control::Focus { owner } if *owner == my_id))),
        "client #1 was not told focus moved to client #2"
    );

    // ...after which the same client's layout pushes and resizes are honored and mirrored.
    let good = serde_json::to_string(&Layout {
        tabs: vec![LayoutTab {
            root: LayoutNode::Leaf { pane: 1 },
            focus: Some(1),
        }],
    })
    .unwrap();
    c2.send(Frame::Control(Control::LayoutTree { json: good.clone() }));
    c2.send(Frame::Control(Control::Resize {
        pane: 1,
        cols: 100,
        rows: 30,
    }));
    assert!(
        c1.wait(|_, ctrl| {
            ctrl.iter()
                .any(|m| matches!(m, Control::LayoutTree { json } if *json == good))
                && ctrl.iter().any(|m| {
                    matches!(
                        m,
                        Control::Resize {
                            pane: 1,
                            cols: 100,
                            rows: 30
                        }
                    )
                })
        }),
        "the new owner's layout/resize were not mirrored to client #1"
    );
    let leaked = {
        let g = c1.collected.lock().unwrap();
        g.1.iter()
            .any(|m| matches!(m, Control::LayoutTree { json } if *json == bad))
    };

    c1.disconnect();
    c2.disconnect();
    cleanup(daemon, &sock);

    assert!(
        !leaked,
        "a follower's layout push must be dropped, not mirrored"
    );
}

#[test]
fn pane_opened_by_one_client_is_announced_to_the_other() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-announce-{tag}.sock"));
    let marker = format!("ANNOUNCE_MARKER_{tag}");
    let daemon = start_daemon(&sock);

    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 2 }));
    assert!(c1.wait(|_, ctrl| ctrl.iter().any(|m| matches!(m, Control::Ready))));
    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 2 }));
    assert!(c2.wait(|_, ctrl| ctrl.iter().any(|m| matches!(m, Control::Ready))));

    // Client #1 opens a pane: it gets Opened; client #2 is told to adopt it (Restore + size),
    // and from then on sees the pane's output.
    c1.send(Frame::Control(Control::Open {
        pane: 7,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(
        c1.wait(|_, ctrl| ctrl
            .iter()
            .any(|m| matches!(m, Control::Opened { pane: 7 }))),
        "opener did not get Opened"
    );
    assert!(
        c2.wait(|_, ctrl| {
            ctrl.iter()
                .any(|m| matches!(m, Control::Restore { pane: 7 }))
                && ctrl
                    .iter()
                    .any(|m| matches!(m, Control::Resize { pane: 7, .. }))
        }),
        "the other client was not told to adopt the new pane"
    );
    c1.send(Frame::Data {
        pane: 7,
        bytes: format!("echo {marker}\r").into_bytes(),
    });
    let mirrored = c2.wait(|out, _| out.get(&7).is_some_and(|o| contains(o, marker.as_bytes())));

    c1.disconnect();
    c2.disconnect();
    cleanup(daemon, &sock);

    assert!(
        mirrored,
        "the new pane's output did not reach the other client"
    );
}

#[test]
fn reattach_replays_layout() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-layout-{tag}.sock"));
    let daemon = start_daemon(&sock);

    // Client #1: open two panes and push a layout that splits them side by side.
    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 1 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    c1.send(Frame::Control(Control::Open {
        pane: 2,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(
        c1.wait(|_, ctrl| ctrl
            .iter()
            .filter(|m| matches!(m, Control::Opened { .. }))
            .count()
            >= 2),
        "panes did not open"
    );
    let layout = Layout {
        tabs: vec![LayoutTab {
            root: LayoutNode::Split {
                cols: true,
                ratio: 0.5,
                a: Box::new(LayoutNode::Leaf { pane: 1 }),
                b: Box::new(LayoutNode::Leaf { pane: 2 }),
            },
            focus: Some(1),
        }],
    };
    let json = serde_json::to_string(&layout).unwrap();
    c1.send(Frame::Control(Control::LayoutTree { json }));
    std::thread::sleep(Duration::from_millis(200)); // let the daemon store it
    c1.disconnect();
    std::thread::sleep(Duration::from_millis(200));

    // Client #2: reattach. The daemon should replay our layout verbatim.
    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 1 }));
    let got_layout = c2.wait(|_, ctrl| {
        ctrl.iter().any(|m| matches!(m, Control::LayoutTree { .. }))
            && ctrl.iter().any(|m| matches!(m, Control::Ready))
    });
    let replayed = {
        let g = c2.collected.lock().unwrap();
        g.1.iter().find_map(|m| match m {
            Control::LayoutTree { json } => serde_json::from_str::<Layout>(json).ok(),
            _ => None,
        })
    };
    c2.disconnect();
    cleanup(daemon, &sock);

    assert!(got_layout, "reattach did not replay a layout + Ready");
    assert_eq!(
        replayed,
        Some(layout),
        "replayed layout did not match what was pushed"
    );
}

#[test]
fn daemon_exits_after_last_pane_closed() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-close-{tag}.sock"));
    let mut daemon = start_daemon(&sock);

    // Open a pane, then close it and disconnect — exactly the wire sequence the client emits when
    // its last remote pane goes away (Close flushed, then channel EOF). The daemon has nothing left
    // to persist and no client, so it must idle-exit (else it lingers and blocks the next connect).
    let mut c = Client::connect(&sock);
    c.send(Frame::Control(Control::Hello { version: 1 }));
    c.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(
        c.wait(|_, ctrl| ctrl
            .iter()
            .any(|m| matches!(m, Control::Opened { pane: 1 }))),
        "pane did not open"
    );
    c.send(Frame::Control(Control::Close { pane: 1 }));
    std::thread::sleep(Duration::from_millis(200)); // let the daemon process Close before EOF
    c.disconnect();

    let exited = wait_until(Duration::from_secs(5), || {
        matches!(daemon.try_wait(), Ok(Some(_)))
    });
    if !exited {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(notify_socket(&sock));
    assert!(
        exited,
        "daemon did not exit after its last pane closed and the client left"
    );
}

#[test]
fn notify_socket_forwards_notes_to_attached_client() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-notify-attached-{tag}.sock"));
    let daemon = start_daemon(&sock);

    let mut c = Client::connect(&sock);
    c.send(Frame::Control(Control::Hello { version: 1 }));
    assert!(c.wait(|_, ctrl| ctrl.iter().any(|m| matches!(m, Control::Ready))));

    send_note(&sock, "native-attached", Kind::Raise);
    let got_note = c.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Notify { json } if json.contains("native-attached")))
    });

    c.disconnect();
    cleanup(daemon, &sock);
    assert!(
        got_note,
        "attached client did not receive native notify note"
    );
}

#[test]
fn notify_socket_replays_pending_notes_on_reattach() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-notify-reattach-{tag}.sock"));
    let daemon = start_daemon(&sock);

    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 1 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Opened { pane: 1 }))
    }));
    c1.disconnect();

    send_note(&sock, "native-detached", Kind::Raise);

    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 1 }));
    let got_note = c2.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Notify { json } if json.contains("native-detached")))
            && ctrl.iter().any(|m| matches!(m, Control::Ready))
    });

    c2.disconnect();
    cleanup(daemon, &sock);
    assert!(
        got_note,
        "reattach did not replay pending native notify note"
    );
}

#[test]
fn notify_socket_replays_detached_clear_on_reattach() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-notify-clear-{tag}.sock"));
    let daemon = start_daemon(&sock);

    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 1 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Opened { pane: 1 }))
    }));

    send_note(&sock, "native-clear", Kind::Raise);
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Notify { json } if json.contains("native-clear")))
    }));
    c1.disconnect();

    send_note(&sock, "native-clear", Kind::Clear);

    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 1 }));
    let got_clear = c2.wait(|_, ctrl| {
        ctrl.iter().any(|m| {
            matches!(
                m,
                Control::Notify { json }
                    if json.contains("native-clear") && json.contains("\"kind\":\"clear\"")
            )
        }) && ctrl.iter().any(|m| matches!(m, Control::Ready))
    });

    c2.disconnect();
    cleanup(daemon, &sock);
    assert!(
        got_clear,
        "reattach did not replay detached native notify clear"
    );
}

#[test]
fn client_notify_clear_removes_pending_note_on_reattach() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-notify-client-clear-{tag}.sock"));
    let daemon = start_daemon(&sock);

    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 2 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Opened { pane: 1 }))
    }));

    send_note(&sock, "client-clear", Kind::Raise);
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Notify { json } if json.contains("client-clear") && json.contains("\"kind\":\"raise\"")))
    }));

    let clear = note("client-clear", Kind::Clear);
    let json = serde_json::to_string(&clear).expect("serialize clear note");
    c1.send(Frame::Control(Control::Notify { json }));
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Notify { json } if json.contains("client-clear") && json.contains("\"kind\":\"clear\"")))
    }));
    c1.disconnect();

    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 2 }));
    assert!(c2.wait(|_, ctrl| ctrl.iter().any(|m| matches!(m, Control::Ready))));
    let replayed = {
        let g = c2.collected.lock().unwrap();
        g.1.iter().any(|m| {
            matches!(
                m,
                Control::Notify { json }
                    if json.contains("client-clear") && json.contains("\"kind\":\"raise\"")
            )
        })
    };

    c2.disconnect();
    cleanup(daemon, &sock);
    assert!(
        !replayed,
        "client-side clear did not remove the daemon's pending note"
    );
}

#[test]
fn pane_close_clears_pending_note_for_that_pane() {
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-notify-pane-close-{tag}.sock"));
    let daemon = start_daemon(&sock);

    let mut c1 = Client::connect(&sock);
    c1.send(Frame::Control(Control::Hello { version: 2 }));
    c1.send(Frame::Control(Control::Open {
        pane: 1,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    c1.send(Frame::Control(Control::Open {
        pane: 2,
        cols: 80,
        rows: 24,
        cwd_from: None,
    }));
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Opened { pane: 1 }))
            && ctrl
                .iter()
                .any(|m| matches!(m, Control::Opened { pane: 2 }))
    }));

    send_note(&sock, "pane-close-clear", Kind::Raise);
    assert!(c1.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Notify { json } if json.contains("pane-close-clear") && json.contains("\"kind\":\"raise\"")))
    }));

    c1.send(Frame::Control(Control::Close { pane: 1 }));
    let cleared = c1.wait(|_, ctrl| {
        ctrl.iter().any(|m| {
            matches!(
                m,
                Control::Notify { json }
                    if json.contains("pane-close-clear")
                        && json.contains("\"kind\":\"clear\"")
            )
        })
    });
    c1.disconnect();

    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 2 }));
    assert!(c2.wait(|_, ctrl| {
        ctrl.iter()
            .any(|m| matches!(m, Control::Restore { pane: 2 }))
            && ctrl.iter().any(|m| matches!(m, Control::Ready))
    }));
    let replayed = {
        let g = c2.collected.lock().unwrap();
        g.1.iter().any(|m| {
            matches!(
                m,
                Control::Notify { json }
                    if json.contains("pane-close-clear") && json.contains("\"kind\":\"raise\"")
            )
        })
    };

    c2.disconnect();
    cleanup(daemon, &sock);
    assert!(cleared, "pane close did not broadcast a clear note");
    assert!(
        !replayed,
        "pane close did not remove the daemon's pending note"
    );
}

/// Kill the daemon and remove its socket.
fn cleanup(mut daemon: Child, sock: &Path) {
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_file(sock);
    let _ = std::fs::remove_file(notify_socket(sock));
}
