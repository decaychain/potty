//! `potty-session` — the headless remote half of potty's multiplexer. It owns the PTYs for one
//! host and multiplexes them over a single byte stream: it reads the wire protocol from stdin and
//! writes it to stdout. In production the `potty` client execs this over an SSH channel (russh);
//! for the spike it's driven over a plain stdio pipe.
//!
//! This is the spike surface (`docs/remote-sessions.md`): panes on demand, multiplexed, no
//! persistence and no pane *tree* yet — just enough to prove the protocol and the round trip.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use potty::proto::{Control, Frame, PaneId, PROTOCOL_VERSION};

/// stdout is written from many threads (each pane's reader, plus control replies); a frame must go
/// out atomically, so all writers share this lock.
type Sink = Arc<Mutex<std::io::Stdout>>;

struct Pane {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    /// Kills the shell on `Close`. The output-reader thread holds a cloned master fd, so just
    /// dropping `master` won't reliably hang the shell up — we signal the process directly.
    killer: Box<dyn ChildKiller + Send + Sync>,
}

fn send(sink: &Sink, frame: Frame) {
    let mut out = sink.lock().unwrap();
    let _ = frame.write(&mut *out);
}

fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// Spawn a shell in a fresh PTY and wire it up: PTY output → `Data` frames, child exit → `Exited`.
fn open_pane(pane: PaneId, cols: u16, rows: u16, sink: &Sink) -> std::io::Result<Pane> {
    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
    let pair = portable_pty::native_pty_system().openpty(size).map_err(std::io::Error::other)?;

    let mut cmd = CommandBuilder::new(shell());
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd).map_err(std::io::Error::other)?;
    let killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader().map_err(std::io::Error::other)?;
    let writer = pair.master.take_writer().map_err(std::io::Error::other)?;

    // Pump PTY output to the client.
    let out_sink = sink.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => send(&out_sink, Frame::Data { pane, bytes: buf[..n].to_vec() }),
            }
        }
    });

    // Report the shell exiting (ConPTY can keep the reader open after exit, so wait on the child).
    let exit_sink = sink.clone();
    thread::spawn(move || {
        let _ = child.wait();
        send(&exit_sink, Frame::Control(Control::Exited { pane }));
    });

    Ok(Pane { writer, master: pair.master, killer })
}

fn main() {
    let sink: Sink = Arc::new(Mutex::new(std::io::stdout()));
    let mut panes: HashMap<PaneId, Pane> = HashMap::new();
    let mut stdin = std::io::stdin().lock();

    // Reads until clean EOF (client detached) or a protocol error — either way we're done. (When
    // persistence lands, detach will keep panes alive instead of exiting.)
    while let Ok(Some(frame)) = Frame::read(&mut stdin) {
        match frame {
            Frame::Control(Control::Hello { .. }) => {
                send(&sink, Frame::Control(Control::Welcome { version: PROTOCOL_VERSION }));
            }
            Frame::Control(Control::Open { pane, cols, rows }) => match open_pane(pane, cols, rows, &sink) {
                Ok(p) => {
                    panes.insert(pane, p);
                    send(&sink, Frame::Control(Control::Opened { pane }));
                }
                Err(_) => send(&sink, Frame::Control(Control::Exited { pane })),
            },
            Frame::Control(Control::Resize { pane, cols, rows }) => {
                if let Some(p) = panes.get(&pane) {
                    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
                    let _ = p.master.resize(size);
                }
            }
            // Kill the shell; the wait thread then emits `Exited`. (Dropping the pane alone is
            // unreliable — the reader thread keeps a cloned master fd open.)
            Frame::Control(Control::Close { pane }) => {
                if let Some(mut p) = panes.remove(&pane) {
                    let _ = p.killer.kill();
                }
            }
            Frame::Data { pane, bytes } => {
                if let Some(p) = panes.get_mut(&pane) {
                    let _ = p.writer.write_all(&bytes);
                    let _ = p.writer.flush();
                }
            }
            // Server→client controls (Welcome/Opened/Exited) never arrive from the client.
            Frame::Control(_) => {}
        }
    }
}
