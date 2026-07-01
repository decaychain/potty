//! `potty-session` — the remote multiplexer. It has two roles:
//!
//!  * **attach** (the default; what the potty client execs over SSH): a dumb byte relay between
//!    this process's stdin/stdout (the SSH channel) and the persistent daemon's Unix socket. It
//!    starts the daemon if it isn't already running, and exits when the client disconnects —
//!    leaving the daemon (and its shells) alive.
//!  * **daemon** (`--daemon <sock>`, forked + detached): owns the PTYs and speaks the wire
//!    protocol over the socket, surviving client disconnects so remote programs keep running.
//!    Any number of clients may attach at once; pane output is broadcast to all of them, and
//!    focus follows input — whoever last typed/opened/closed drives the layout and pane sizes
//!    (see `docs/remote-sessions.md`, steps 4–5). One per user (per host, implicitly).
//!
//! `POTTY_SESSION_NODAEMON=1` runs the multiplexer inline over stdin/stdout (no daemon, no
//! persistence) — used by the protocol/transport tests.
//!
//! Unix-only (Unix-domain sockets, process groups, PTYs). On other platforms the implementation is
//! cfg'd out and `main` is a stub that exits with an error, so a full `cargo build` still succeeds
//! on Windows even though potty-session isn't shipped there (potty connects with a plain SSH shell).

#[cfg(not(unix))]
fn main() {
    eprintln!("potty-session runs only on Unix.");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    imp::main();
}

#[cfg(unix)]
mod imp {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;

    use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
    use potty::notify as feed;
    use potty::proto::{Control, Frame, PROTOCOL_VERSION, PaneId};

    /// The daemon's socket, remembered so threads can remove it on exit.
    static SOCK: OnceLock<PathBuf> = OnceLock::new();
    static NOTIFY_SOCK: OnceLock<PathBuf> = OnceLock::new();

    /// How much recent PTY output to keep per pane for replay on reattach. Enough for the current
    /// screen plus some scrollback; bounded so a chatty pane can't grow without limit.
    const REPLAY_CAP: usize = 256 * 1024;

    struct Pane {
        writer: Box<dyn Write + Send>,
        master: Box<dyn MasterPty + Send>,
        /// Kills the shell on `Close`. The output-reader thread holds a cloned master fd, so just
        /// dropping `master` won't reliably hang the shell up — we signal the process directly.
        killer: Box<dyn ChildKiller + Send + Sync>,
        /// Shell PID fallback for cwd inheritance if the PTY foreground process group is unknown.
        child_pid: Option<u32>,
        /// Recent raw output (capped at `REPLAY_CAP`), replayed when a client (re)attaches.
        buffer: Arc<Mutex<Vec<u8>>>,
        /// Current PTY size, replayed to late-joining clients so their grids match the PTY.
        size: (u16, u16), // (cols, rows)
    }

    struct Client {
        id: u64,
        writer: Box<dyn Write + Send>,
        shutdown: Option<UnixStream>,
        /// Panes this client has been told about (`Restore`/`Opened`). `Data` is only broadcast to
        /// clients that know the pane, so replay always precedes live output on their stream.
        announced: std::collections::HashSet<PaneId>,
    }

    /// Shared session state. `panes` outlive any single client connection (that's the persistence);
    /// `clients` are all currently-attached clients — any number may watch, and `focus` (a client
    /// id; 0 = nobody) names the one whose layout pushes and resizes are authoritative. Focus
    /// follows input: it flips to whichever client last typed, opened, or closed a pane.
    struct Session {
        panes: Mutex<HashMap<PaneId, Pane>>,
        clients: Mutex<Vec<Client>>,
        next_client: AtomicU64,
        focus: AtomicU64,
        /// The focused client's last-pushed tab/pane tree (opaque JSON), replayed on reattach and
        /// mirrored live to the other clients.
        layout: Mutex<Option<String>>,
        /// Attention notes raised while detached, replayed on the next attach.
        pending_notes: Mutex<HashMap<(String, String), feed::Note>>,
        notify_sock: PathBuf,
    }

    fn shell() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }

    fn process_cwd(pid: u32) -> Option<PathBuf> {
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        cwd.is_dir().then_some(cwd)
    }

    fn valid_cwd(path: &Path) -> bool {
        path.is_absolute() && path.is_dir()
    }

    fn pane_cwd(pane: &Pane) -> Option<PathBuf> {
        if let Some(cwd) = pane
            .master
            .process_group_leader()
            .and_then(|pid| u32::try_from(pid).ok())
            .and_then(process_cwd)
        {
            return Some(cwd);
        }

        pane.child_pid.and_then(process_cwd)
    }

    fn cwd_from_pane(session: &Session, pane: Option<PaneId>) -> Option<PathBuf> {
        let pane = pane?;
        session.panes.lock().unwrap().get(&pane).and_then(pane_cwd)
    }

    /// Write a frame to one client under the clients lock (frames must go out atomically). A write
    /// error shuts the client's stream down; its serve thread then cleans it up.
    fn write_or_hangup(client: &mut Client, frame: &Frame) {
        if frame.write(&mut client.writer).is_err()
            && let Some(stream) = client.shutdown.as_ref()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Send a frame to one attached client by id (dropped if it detached meanwhile).
    fn send_to(session: &Session, client_id: u64, frame: Frame) {
        let mut clients = session.clients.lock().unwrap();
        if let Some(client) = clients.iter_mut().find(|c| c.id == client_id) {
            write_or_hangup(client, &frame);
        }
    }

    /// Broadcast a frame to every attached client except `except` (the originator, if any).
    fn broadcast(session: &Session, frame: Frame, except: Option<u64>) {
        let mut clients = session.clients.lock().unwrap();
        for client in clients.iter_mut() {
            if Some(client.id) != except {
                write_or_hangup(client, &frame);
            }
        }
    }

    /// Broadcast pane output — only to clients that already know the pane, so a client never sees
    /// `Data` before its `Restore`/`Opened` announcement (which carries the replay).
    fn broadcast_data(session: &Session, pane: PaneId, bytes: Vec<u8>) {
        let mut clients = session.clients.lock().unwrap();
        for client in clients.iter_mut() {
            if client.announced.contains(&pane) {
                write_or_hangup(
                    client,
                    &Frame::Data {
                        pane,
                        bytes: bytes.clone(),
                    },
                );
            }
        }
    }

    /// Make `client_id` the focus owner (it typed / opened / closed something). Broadcasts the
    /// change to everyone, the new owner included.
    fn take_focus(session: &Session, client_id: u64) {
        if session.focus.swap(client_id, Ordering::Relaxed) != client_id {
            broadcast(
                session,
                Frame::Control(Control::Focus { owner: client_id }),
                None,
            );
        }
    }

    fn has_focus(session: &Session, client_id: u64) -> bool {
        session.focus.load(Ordering::Relaxed) == client_id
    }

    fn remove_client(session: &Session, client_id: u64) {
        session
            .clients
            .lock()
            .unwrap()
            .retain(|c| c.id != client_id);
        // The focused client left: nobody owns the layout until the next input claims it.
        if session
            .focus
            .compare_exchange(client_id, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            broadcast(session, Frame::Control(Control::Focus { owner: 0 }), None);
        }
    }

    fn note_key(note: &feed::Note) -> (String, String) {
        (note.host.clone(), note.session.clone())
    }

    fn send_note(session: &Session, note: &feed::Note) {
        if let Ok(json) = serde_json::to_string(note) {
            broadcast(session, Frame::Control(Control::Notify { json }), None);
        }
    }

    fn handle_note(session: &Session, note: feed::Note) {
        match note.kind {
            feed::Kind::Raise => {
                session
                    .pending_notes
                    .lock()
                    .unwrap()
                    .insert(note_key(&note), note.clone());
                send_note(session, &note);
            }
            feed::Kind::Clear => {
                if !session.clients.lock().unwrap().is_empty() {
                    session
                        .pending_notes
                        .lock()
                        .unwrap()
                        .remove(&note_key(&note));
                } else {
                    session
                        .pending_notes
                        .lock()
                        .unwrap()
                        .insert(note_key(&note), note.clone());
                }
                send_note(session, &note);
            }
        }
    }

    /// Replay stored notes to the client that just attached.
    fn replay_pending_notes(session: &Session, client_id: u64) {
        let notes: Vec<feed::Note> = {
            let mut pending = session.pending_notes.lock().unwrap();
            let notes = pending.values().cloned().collect();
            pending.retain(|_, note| note.kind == feed::Kind::Raise);
            notes
        };
        for note in notes {
            if let Ok(json) = serde_json::to_string(&note) {
                send_to(session, client_id, Frame::Control(Control::Notify { json }));
            }
        }
    }

    fn notify_socket_path(sock: &Path) -> PathBuf {
        sock.with_extension("notify.sock")
    }

    fn inline_notify_socket_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "potty-session-inline-{}-notify.sock",
            std::process::id()
        ))
    }

    fn spawn_notify_listener(session: &Arc<Session>) {
        let path = session.notify_sock.clone();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!(
                    "potty-session: attention feed disabled (socket {}: {err})",
                    path.display()
                );
                return;
            }
        };
        let _ = NOTIFY_SOCK.set(path);
        let session = session.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(stream.take(64 * 1024))
                    .read_line(&mut line)
                    .is_err()
                {
                    continue;
                }
                if let Ok(note) = serde_json::from_str::<feed::Note>(line.trim())
                    && note.v == feed::SCHEMA_VERSION
                {
                    handle_note(&session, note);
                }
            }
        });
    }

    /// Spawn a shell in a fresh PTY for `opener`: it gets `Opened`; every other attached client is
    /// told to adopt the pane (`Restore` + `Resize`). Output → `Data` frames broadcast to everyone
    /// who knows the pane; exit → `Exited` (and, if nothing's left and nobody's attached, the
    /// daemon exits).
    fn open_pane(
        session: &Arc<Session>,
        opener: u64,
        pane: PaneId,
        cols: u16,
        rows: u16,
        cwd: Option<PathBuf>,
    ) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let fail = |s: &Session| send_to(s, opener, Frame::Control(Control::Exited { pane }));
        let pair = match portable_pty::native_pty_system().openpty(size) {
            Ok(p) => p,
            Err(_) => return fail(session),
        };
        let mut cmd = CommandBuilder::new(shell());
        cmd.env("TERM", "xterm-256color");
        cmd.env(feed::ENV_SOCK, &session.notify_sock);
        cmd.env(feed::ENV_PANE, pane.to_string());
        if let Some(cwd) = cwd.as_deref().filter(|path| valid_cwd(path)) {
            cmd.cwd(cwd.as_os_str());
            cmd.env("PWD", cwd.as_os_str());
        }
        let mut child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(_) => return fail(session),
        };
        let child_pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(_) => return fail(session),
        };
        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(_) => return fail(session),
        };

        let buffer = Arc::new(Mutex::new(Vec::new()));

        session.panes.lock().unwrap().insert(
            pane,
            Pane {
                writer,
                master: pair.master,
                killer,
                child_pid,
                buffer: buffer.clone(),
                size: (cols, rows),
            },
        );

        // Announce before the output reader starts, so nobody can see `Data` for an unknown pane:
        // the opener gets `Opened`, everyone else adopts it (`Restore` + its size; no replay — the
        // pane is brand new). Placement into tabs follows with the opener's next layout push.
        {
            let mut clients = session.clients.lock().unwrap();
            for client in clients.iter_mut() {
                client.announced.insert(pane);
                if client.id == opener {
                    write_or_hangup(client, &Frame::Control(Control::Opened { pane }));
                } else {
                    write_or_hangup(client, &Frame::Control(Control::Restore { pane }));
                    write_or_hangup(
                        client,
                        &Frame::Control(Control::Resize { pane, cols, rows }),
                    );
                }
            }
        }

        // PTY output → the replay buffer and all attached clients. Keeps reading (and buffering)
        // while detached, so the shell never blocks on a full PTY and the screen can be replayed
        // on reattach.
        let out = session.clone();
        let out_buf = buffer;
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
                broadcast_data(&out, pane, buf[..n].to_vec());
            }
        });

        // Shell exit → Exited to everyone, drop the pane, and shut the daemon down if it's idle.
        let wait = session.clone();
        thread::spawn(move || {
            let _ = child.wait();
            broadcast(&wait, Frame::Control(Control::Exited { pane }), None);
            wait.panes.lock().unwrap().remove(&pane);
            let idle =
                wait.panes.lock().unwrap().is_empty() && wait.clients.lock().unwrap().is_empty();
            if idle {
                cleanup_and_exit();
            }
        });
    }

    /// Serve one client connection: apply its frames to the session until it disconnects (EOF).
    /// Any number of clients may be attached at once; focus follows input (`Data`/`Open`/`Close`
    /// flip it to the sender), and only the focused client's `Resize`/`LayoutTree` are honored.
    fn serve(session: &Arc<Session>, client_id: u64, reader: impl Read) {
        let mut reader = BufReader::new(reader);
        while let Ok(Some(frame)) = Frame::read(&mut reader) {
            match frame {
                // Attach handshake: greet, tell the client who holds focus, replay every existing
                // pane (screen + size), the stored layout, pending notes, then Ready. All of it
                // goes to this client only — attaching neither disturbs nor evicts the others.
                Frame::Control(Control::Hello { .. }) => {
                    send_to(
                        session,
                        client_id,
                        Frame::Control(Control::Welcome {
                            version: PROTOCOL_VERSION,
                            client: client_id,
                        }),
                    );
                    // Nobody focused (fresh daemon, or the owner left) → the newcomer claims it;
                    // otherwise it joins as a follower of whoever is working.
                    let _ = session.focus.compare_exchange(
                        0,
                        client_id,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                    broadcast(
                        session,
                        Frame::Control(Control::Focus {
                            owner: session.focus.load(Ordering::Relaxed),
                        }),
                        None,
                    );
                    let restores: Vec<(PaneId, Vec<u8>, (u16, u16))> = {
                        let panes = session.panes.lock().unwrap();
                        panes
                            .iter()
                            .map(|(id, p)| (*id, p.buffer.lock().unwrap().clone(), p.size))
                            .collect()
                    };
                    for (id, buf, (cols, rows)) in restores {
                        if let Some(client) = session
                            .clients
                            .lock()
                            .unwrap()
                            .iter_mut()
                            .find(|c| c.id == client_id)
                        {
                            client.announced.insert(id);
                        }
                        send_to(
                            session,
                            client_id,
                            Frame::Control(Control::Restore { pane: id }),
                        );
                        send_to(
                            session,
                            client_id,
                            Frame::Control(Control::Resize {
                                pane: id,
                                cols,
                                rows,
                            }),
                        );
                        if !buf.is_empty() {
                            send_to(
                                session,
                                client_id,
                                Frame::Data {
                                    pane: id,
                                    bytes: buf,
                                },
                            );
                        }
                    }
                    // Replay the stored tab/pane tree (if any) so the client rebuilds the layout.
                    if let Some(json) = session.layout.lock().unwrap().clone() {
                        send_to(
                            session,
                            client_id,
                            Frame::Control(Control::LayoutTree { json }),
                        );
                    }
                    replay_pending_notes(session, client_id);
                    send_to(session, client_id, Frame::Control(Control::Ready));
                }
                // The focused client pushed its layout — store it for reattach and mirror it to
                // the other clients. Pushes from followers are stale echoes: dropped, and they do
                // NOT flip focus (they're machine-generated, not user intent).
                Frame::Control(Control::LayoutTree { json }) => {
                    if has_focus(session, client_id) {
                        *session.layout.lock().unwrap() = Some(json.clone());
                        broadcast(
                            session,
                            Frame::Control(Control::LayoutTree { json }),
                            Some(client_id),
                        );
                    }
                }
                // Ignore an Open for a pane that already exists (e.g. a restored one).
                Frame::Control(Control::Open {
                    pane,
                    cols,
                    rows,
                    cwd_from,
                }) => {
                    take_focus(session, client_id);
                    if !session.panes.lock().unwrap().contains_key(&pane) {
                        let cwd = cwd_from_pane(session, cwd_from);
                        open_pane(session, client_id, pane, cols, rows, cwd);
                    }
                }
                // A pane has one PTY size; the focused client's geometry wins, and the others are
                // told to conform. Resizes from followers are dropped without flipping focus —
                // honoring them would start a resize war between differently-sized windows.
                Frame::Control(Control::Resize { pane, cols, rows }) => {
                    if !has_focus(session, client_id) {
                        continue;
                    }
                    let size = PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    };
                    let mut resized = false;
                    if let Some(p) = session.panes.lock().unwrap().get_mut(&pane) {
                        p.size = (cols, rows);
                        resized = p.master.resize(size).is_ok();
                    }
                    if resized {
                        broadcast(
                            session,
                            Frame::Control(Control::Resize { pane, cols, rows }),
                            Some(client_id),
                        );
                    }
                }
                Frame::Control(Control::Close { pane }) => {
                    take_focus(session, client_id);
                    if let Some(mut p) = session.panes.lock().unwrap().remove(&pane) {
                        let _ = p.killer.kill();
                    }
                }
                Frame::Data { pane, bytes } => {
                    take_focus(session, client_id);
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
        if let Some(sock) = NOTIFY_SOCK.get() {
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
            // Start in a new session so the daemon is not tied to the SSH exec process's job-control
            // state. This is stronger than just setpgid(0, 0), which still leaves it in the same
            // session as the short-lived attach relay.
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("--daemon")
                .arg(sock)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: this closure runs in the child process after fork and before exec. It only calls
            // the async-signal-safe setsid(2) and constructs an io::Error if that syscall fails.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            let _ = cmd.spawn();
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

    /// Daemon role: own the PTYs, keep accepting attaches, and persist across detaches.
    fn run_daemon(sock: PathBuf) {
        let _ = std::fs::remove_file(&sock);
        let listener = match UnixListener::bind(&sock) {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("potty-session: could not bind {}: {err}", sock.display());
                return; // another daemon won the race to bind
            }
        };
        let _ = SOCK.set(sock);
        let notify_sock = SOCK
            .get()
            .map(|sock| notify_socket_path(sock))
            .unwrap_or_else(inline_notify_socket_path);
        let session = Arc::new(Session {
            panes: Mutex::new(HashMap::new()),
            clients: Mutex::new(Vec::new()),
            next_client: AtomicU64::new(1),
            focus: AtomicU64::new(0),
            layout: Mutex::new(None),
            pending_notes: Mutex::new(HashMap::new()),
            notify_sock,
        });
        spawn_notify_listener(&session);
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let Ok(write) = conn.try_clone() else {
                continue;
            };
            let shutdown = conn.try_clone().ok();
            let id = session.next_client.fetch_add(1, Ordering::Relaxed);
            session.clients.lock().unwrap().push(Client {
                id,
                writer: Box::new(write),
                shutdown,
                announced: std::collections::HashSet::new(),
            });

            let serving = session.clone();
            thread::spawn(move || {
                serve(&serving, id, conn);
                remove_client(&serving, id);
                // Detached. If nothing's left to persist, shut down.
                let idle = serving.panes.lock().unwrap().is_empty()
                    && serving.clients.lock().unwrap().is_empty();
                if idle {
                    cleanup_and_exit();
                }
            });
        }
    }

    /// Inline role (tests): multiplex directly over stdin/stdout, no daemon, one client.
    fn run_inline() {
        let session = Arc::new(Session {
            panes: Mutex::new(HashMap::new()),
            clients: Mutex::new(vec![Client {
                id: 1,
                writer: Box::new(std::io::stdout()),
                shutdown: None,
                announced: std::collections::HashSet::new(),
            }]),
            next_client: AtomicU64::new(2),
            focus: AtomicU64::new(0),
            layout: Mutex::new(None),
            pending_notes: Mutex::new(HashMap::new()),
            notify_sock: inline_notify_socket_path(),
        });
        serve(&session, 1, std::io::stdin().lock());
    }

    pub fn main() {
        let mut args = std::env::args().skip(1);
        match args.next().as_deref() {
            Some("--daemon") => {
                run_daemon(args.next().map(PathBuf::from).unwrap_or_else(socket_path))
            }
            _ if std::env::var_os("POTTY_SESSION_NODAEMON").is_some() => run_inline(),
            _ => attach(socket_path()),
        }
    }
} // mod imp
