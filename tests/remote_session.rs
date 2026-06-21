//! End-to-end test of the `potty-session` server: drive it over a stdio pipe with the real wire
//! protocol and confirm it spawns shells, multiplexes panes over one stream, and reports
//! lifecycle correctly. Unix-only — it runs real shells (remotes are Unix-scoped).
#![cfg(unix)]

use std::collections::HashMap;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use potty::proto::{Control, Frame, PaneId};

#[derive(Default)]
struct Collected {
    out: HashMap<PaneId, Vec<u8>>,
    ctrl: Vec<Control>,
}

impl Collected {
    fn output(&self, pane: PaneId) -> Vec<u8> {
        self.out.get(&pane).cloned().unwrap_or_default()
    }
    fn has(&self, pred: impl Fn(&Control) -> bool) -> bool {
        self.ctrl.iter().any(pred)
    }
}

/// A running `potty-session` plus a background thread demuxing its output stream.
struct Session {
    child: Child,
    stdin: ChildStdin,
    collected: Arc<Mutex<Collected>>,
}

impl Session {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_potty-session"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn potty-session");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let collected = Arc::new(Mutex::new(Collected::default()));
        let sink = collected.clone();
        thread::spawn(move || {
            while let Ok(Some(frame)) = Frame::read(&mut stdout) {
                let mut g = sink.lock().unwrap();
                match frame {
                    Frame::Data { pane, bytes } => g.out.entry(pane).or_default().extend(bytes),
                    Frame::Control(c) => g.ctrl.push(c),
                }
            }
        });
        Session { child, stdin, collected }
    }

    fn send(&mut self, f: Frame) {
        f.write(&mut self.stdin).expect("write frame");
    }

    /// Poll the collected state until `pred` holds or we time out.
    fn wait_until(&self, pred: impl Fn(&Collected) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if pred(&self.collected.lock().unwrap()) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn multiplexes_panes_over_one_stream() {
    let mut s = Session::start();

    s.send(Frame::Control(Control::Hello { version: 1 }));
    assert!(s.wait_until(|c| c.has(|m| matches!(m, Control::Welcome { .. }))), "no Welcome");

    // Two panes share one stream.
    s.send(Frame::Control(Control::Open { pane: 1, cols: 80, rows: 24 }));
    s.send(Frame::Control(Control::Open { pane: 2, cols: 80, rows: 24 }));
    assert!(
        s.wait_until(|c| c.has(|m| matches!(m, Control::Opened { pane: 1 }))
            && c.has(|m| matches!(m, Control::Opened { pane: 2 }))),
        "panes did not open",
    );

    // Input reaches the right shell; output comes back tagged to the right pane.
    s.send(Frame::Data { pane: 1, bytes: b"echo PANE_ONE_OK\r".to_vec() });
    s.send(Frame::Data { pane: 2, bytes: b"echo PANE_TWO_OK\r".to_vec() });
    assert!(
        s.wait_until(|c| contains(&c.output(1), b"PANE_ONE_OK")
            && contains(&c.output(2), b"PANE_TWO_OK")),
        "echo output missing",
    );

    // Isolation: a pane's output never leaks into the other's stream.
    {
        let c = s.collected.lock().unwrap();
        assert!(!contains(&c.output(1), b"PANE_TWO_OK"), "pane 2 leaked into pane 1");
        assert!(!contains(&c.output(2), b"PANE_ONE_OK"), "pane 1 leaked into pane 2");
    }

    // Resize must not disturb the stream.
    s.send(Frame::Control(Control::Resize { pane: 1, cols: 120, rows: 40 }));
    s.send(Frame::Data { pane: 1, bytes: b"echo AFTER_RESIZE\r".to_vec() });
    assert!(s.wait_until(|c| contains(&c.output(1), b"AFTER_RESIZE")), "stream broke after resize");

    // Closing a pane SIGHUPs its shell.
    s.send(Frame::Control(Control::Close { pane: 1 }));
    assert!(s.wait_until(|c| c.has(|m| matches!(m, Control::Exited { pane: 1 }))), "no Exited");

    // Client detach (EOF) → clean shutdown.
    drop(s.stdin);
    let status = s.child.wait().expect("wait for session");
    assert!(status.success(), "session exited non-zero: {status:?}");
}
