//! Persistence tests (step 4):
//!   - a shell's process survives the client disconnecting (the daemon keeps it alive), and
//!   - reattaching restores the pane and replays its current screen.
//! Unix-only; the first skips if `pgrep` is unavailable.
#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use potty::proto::{Control, Frame};

fn have(bin: &str) -> bool {
    Command::new(bin).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
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
    assert!(wait_until(Duration::from_secs(5), || sock.exists()), "daemon never bound its socket");
    child
}

/// A direct client connection to the daemon, with a background thread demuxing its output.
struct Client {
    stream: UnixStream,
    collected: std::sync::Arc<std::sync::Mutex<(std::collections::HashMap<u64, Vec<u8>>, Vec<Control>)>>,
}

impl Client {
    fn connect(sock: &Path) -> Self {
        let stream = UnixStream::connect(sock).expect("connect to daemon");
        let mut read = stream.try_clone().unwrap();
        let collected: std::sync::Arc<std::sync::Mutex<(std::collections::HashMap<u64, Vec<u8>>, Vec<Control>)>> =
            std::sync::Arc::new(std::sync::Mutex::new((std::collections::HashMap::new(), Vec::new())));
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

    fn wait(&self, pred: impl Fn(&std::collections::HashMap<u64, Vec<u8>>, &[Control]) -> bool) -> bool {
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
    c.send(Frame::Control(Control::Open { pane: 1, cols: 80, rows: 24 }));
    c.send(Frame::Data { pane: 1, bytes: format!("sleep {marker}\r").into_bytes() });

    assert!(wait_until(Duration::from_secs(10), || pgrep(&marker)), "marker process never started");
    c.disconnect();

    std::thread::sleep(Duration::from_millis(500));
    let survived = pgrep(&marker);

    let _ = Command::new("pkill").args(["-f", &marker]).status();
    cleanup(daemon, &sock);

    assert!(survived, "the shell's process did not survive the client disconnect");
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
    c1.send(Frame::Control(Control::Open { pane: 1, cols: 80, rows: 24 }));
    c1.send(Frame::Data { pane: 1, bytes: format!("echo {marker}\r").into_bytes() });
    let echoed = c1.wait(|out, _| out.get(&1).is_some_and(|o| contains(o, marker.as_bytes())));
    assert!(echoed, "marker never echoed on client #1");
    c1.disconnect();
    std::thread::sleep(Duration::from_millis(200));

    // Client #2: a fresh connection. Its Hello should restore pane 1 and replay its screen.
    let mut c2 = Client::connect(&sock);
    c2.send(Frame::Control(Control::Hello { version: 1 }));
    let restored = c2.wait(|out, ctrl| {
        ctrl.iter().any(|m| matches!(m, Control::Restore { pane: 1 }))
            && ctrl.iter().any(|m| matches!(m, Control::Ready))
            && out.get(&1).is_some_and(|o| contains(o, marker.as_bytes()))
    });
    c2.disconnect();
    cleanup(daemon, &sock);

    assert!(restored, "reattach did not restore pane 1 with its replayed screen");
}

/// Kill the daemon and remove its socket.
fn cleanup(mut daemon: Child, sock: &Path) {
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_file(sock);
}
