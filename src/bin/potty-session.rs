//! `potty-session` — the remote multiplexer. It has two roles:
//!
//!  * **attach** (the default; what the potty client execs over SSH): a dumb byte relay between
//!    this process's stdin/stdout (the SSH channel) and the persistent daemon's Unix socket. It
//!    starts the daemon if it isn't already running, and exits when the client disconnects —
//!    leaving the daemon (and its shells) alive.
//!  * **daemon** (`--daemon <sock>`, forked + detached): owns the PTYs and speaks the wire
//!    protocol over the socket, surviving client disconnects so remote programs keep running. One
//!    per user (per host, implicitly). See `docs/remote-sessions.md`, step 4.
//!
//! `POTTY_SESSION_NODAEMON=1` runs the multiplexer inline over stdin/stdout (no daemon, no
//! persistence) — used by the protocol/transport tests.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use potty::proto::{Control, Frame, PaneId, PROTOCOL_VERSION};

/// The daemon's socket, remembered so threads can remove it on exit.
static SOCK: OnceLock<PathBuf> = OnceLock::new();

/// How much recent PTY output to keep per pane for replay on reattach. Enough for the current
/// screen plus some scrollback; bounded so a chatty pane can't grow without limit.
const REPLAY_CAP: usize = 256 * 1024;

struct Pane {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    /// Kills the shell on `Close`. The output-reader thread holds a cloned master fd, so just
    /// dropping `master` won't reliably hang the shell up — we signal the process directly.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// Recent raw output (capped at `REPLAY_CAP`), replayed when a client (re)attaches.
    buffer: Arc<Mutex<Vec<u8>>>,
}

/// Shared session state. `panes` outlive any single client connection (that's the persistence);
/// `client` is the currently-attached client's write half, or `None` while detached.
struct Session {
    panes: Mutex<HashMap<PaneId, Pane>>,
    client: Mutex<Option<Box<dyn Write + Send>>>,
    /// The client's last-pushed tab/pane tree (opaque JSON), replayed on reattach.
    layout: Mutex<Option<String>>,
}

fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// Write a frame to the attached client, if any (dropped while detached). Frames must go out
/// atomically, so the whole write happens under the lock.
fn send_frame(session: &Session, frame: Frame) {
    let mut guard = session.client.lock().unwrap();
    if let Some(client) = guard.as_mut() {
        let _ = frame.write(client);
    }
}

/// Spawn a shell in a fresh PTY: output → `Data` frames to whoever's attached; exit → `Exited`
/// (and, if nothing's left and nobody's attached, the daemon exits).
fn open_pane(session: &Arc<Session>, pane: PaneId, cols: u16, rows: u16) {
    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
    let pair = match portable_pty::native_pty_system().openpty(size) {
        Ok(p) => p,
        Err(_) => return send_frame(session, Frame::Control(Control::Exited { pane })),
    };
    let mut cmd = CommandBuilder::new(shell());
    cmd.env("TERM", "xterm-256color");
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(_) => return send_frame(session, Frame::Control(Control::Exited { pane })),
    };
    let killer = child.clone_killer();
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return send_frame(session, Frame::Control(Control::Exited { pane })),
    };
    let writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return send_frame(session, Frame::Control(Control::Exited { pane })),
    };

    let buffer = Arc::new(Mutex::new(Vec::new()));

    // PTY output → the replay buffer and the attached client (if any). Keeps reading (and
    // buffering) while detached, so the shell never blocks on a full PTY and the screen can be
    // replayed on reattach.
    let out = session.clone();
    let out_buf = buffer.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            {
                let mut b = out_buf.lock().unwrap();
                b.extend_from_slice(&buf[..n]);
                if b.len() > REPLAY_CAP {
                    let excess = b.len() - REPLAY_CAP;
                    b.drain(..excess);
                }
            }
            send_frame(&out, Frame::Data { pane, bytes: buf[..n].to_vec() });
        }
    });

    // Shell exit → Exited, drop the pane, and shut the daemon down if it's now idle.
    let wait = session.clone();
    thread::spawn(move || {
        let _ = child.wait();
        send_frame(&wait, Frame::Control(Control::Exited { pane }));
        wait.panes.lock().unwrap().remove(&pane);
        let idle = wait.panes.lock().unwrap().is_empty() && wait.client.lock().unwrap().is_none();
        if idle {
            cleanup_and_exit();
        }
    });

    session.panes.lock().unwrap().insert(pane, Pane { writer, master: pair.master, killer, buffer });
    send_frame(session, Frame::Control(Control::Opened { pane }));
}

/// Serve one client connection: apply its frames to the session until it disconnects (EOF).
fn serve(session: &Arc<Session>, reader: impl Read) {
    let mut reader = BufReader::new(reader);
    while let Ok(Some(frame)) = Frame::read(&mut reader) {
        match frame {
            // Attach handshake: greet, replay every existing pane's current screen, then Ready.
            Frame::Control(Control::Hello { .. }) => {
                send_frame(session, Frame::Control(Control::Welcome { version: PROTOCOL_VERSION }));
                let restores: Vec<(PaneId, Vec<u8>)> = {
                    let panes = session.panes.lock().unwrap();
                    panes.iter().map(|(id, p)| (*id, p.buffer.lock().unwrap().clone())).collect()
                };
                for (id, buf) in restores {
                    send_frame(session, Frame::Control(Control::Restore { pane: id }));
                    if !buf.is_empty() {
                        send_frame(session, Frame::Data { pane: id, bytes: buf });
                    }
                }
                // Replay the stored tab/pane tree (if any) so the client rebuilds the layout.
                if let Some(json) = session.layout.lock().unwrap().clone() {
                    send_frame(session, Frame::Control(Control::LayoutTree { json }));
                }
                send_frame(session, Frame::Control(Control::Ready));
            }
            // The client pushed its current layout — store it for the next reattach.
            Frame::Control(Control::LayoutTree { json }) => {
                *session.layout.lock().unwrap() = Some(json);
            }
            // Ignore an Open for a pane that already exists (e.g. a restored one).
            Frame::Control(Control::Open { pane, cols, rows }) => {
                if !session.panes.lock().unwrap().contains_key(&pane) {
                    open_pane(session, pane, cols, rows);
                }
            }
            Frame::Control(Control::Resize { pane, cols, rows }) => {
                let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
                if let Some(p) = session.panes.lock().unwrap().get(&pane) {
                    let _ = p.master.resize(size);
                }
            }
            Frame::Control(Control::Close { pane }) => {
                if let Some(mut p) = session.panes.lock().unwrap().remove(&pane) {
                    let _ = p.killer.kill();
                }
            }
            Frame::Data { pane, bytes } => {
                if let Some(p) = session.panes.lock().unwrap().get_mut(&pane) {
                    let _ = p.writer.write_all(&bytes);
                    let _ = p.writer.flush();
                }
            }
            // Server→client controls (Welcome/Opened/Exited) never arrive from the client.
            Frame::Control(_) => {}
        }
    }
}

fn cleanup_and_exit() -> ! {
    if let Some(sock) = SOCK.get() {
        let _ = std::fs::remove_file(sock);
    }
    std::process::exit(0);
}

/// `$POTTY_SESSION_SOCK`, else `$XDG_RUNTIME_DIR/potty-session.sock`, else a per-user temp path.
fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("POTTY_SESSION_SOCK") {
        return PathBuf::from(p);
    }
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("potty-session.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    std::env::temp_dir().join(format!("potty-session-{user}.sock"))
}

/// Connect to the daemon, starting (and detaching) one if it isn't running.
fn ensure_daemon(sock: &Path) -> Option<UnixStream> {
    if let Ok(s) = UnixStream::connect(sock) {
        return Some(s);
    }
    if let Some(dir) = sock.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(exe) = std::env::current_exe() {
        // A new process group detaches it from the SSH session, so it survives the channel closing.
        let _ = std::process::Command::new(exe)
            .arg("--daemon")
            .arg(sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
    }
    for _ in 0..300 {
        if let Ok(s) = UnixStream::connect(sock) {
            return Some(s);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Attach role: relay stdin/stdout (the SSH channel) to/from the daemon socket. Exits on client
/// disconnect, leaving the daemon.
fn attach(sock: PathBuf) {
    let Some(stream) = ensure_daemon(&sock) else {
        eprintln!("potty-session: could not reach the session daemon");
        std::process::exit(1);
    };
    let mut to_daemon = stream.try_clone().expect("clone socket");
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let _ = std::io::copy(&mut stdin, &mut to_daemon);
        // The SSH channel closed → exit, dropping the socket so the daemon detaches but lives on.
        std::process::exit(0);
    });
    // Relay daemon → stdout, flushing every chunk. We can't use `io::copy` into the stdout lock:
    // it's a LineWriter, and our binary frames rarely contain '\n', so small control frames
    // (Welcome/Ready) would sit buffered and never reach the client.
    let mut from_daemon = stream;
    let stdout = std::io::stdout();
    let mut buf = [0u8; 8192];
    loop {
        match from_daemon.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut lock = stdout.lock();
                if lock.write_all(&buf[..n]).is_err() || lock.flush().is_err() {
                    break;
                }
            }
        }
    }
}

/// Daemon role: own the PTYs, serve one attached client at a time, persist across detaches.
fn run_daemon(sock: PathBuf) {
    let _ = std::fs::remove_file(&sock);
    let Ok(listener) = UnixListener::bind(&sock) else {
        return; // another daemon won the race to bind
    };
    let _ = SOCK.set(sock);
    let session = Arc::new(Session {
        panes: Mutex::new(HashMap::new()),
        client: Mutex::new(None),
        layout: Mutex::new(None),
    });
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let Ok(write) = conn.try_clone() else { continue };
        *session.client.lock().unwrap() = Some(Box::new(write));
        serve(&session, conn);
        *session.client.lock().unwrap() = None;
        // Detached. If nothing's left to persist, shut down.
        if session.panes.lock().unwrap().is_empty() {
            cleanup_and_exit();
        }
    }
}

/// Inline role (tests): multiplex directly over stdin/stdout, no daemon.
fn run_inline() {
    let session = Arc::new(Session {
        panes: Mutex::new(HashMap::new()),
        client: Mutex::new(Some(Box::new(std::io::stdout()))),
        layout: Mutex::new(None),
    });
    serve(&session, std::io::stdin().lock());
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--daemon") => run_daemon(args.next().map(PathBuf::from).unwrap_or_else(socket_path)),
        _ if std::env::var_os("POTTY_SESSION_NODAEMON").is_some() => run_inline(),
        _ => attach(socket_path()),
    }
}
