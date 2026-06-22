//! Persistence test (step 4a): a shell started through `potty-session` survives the client
//! disconnecting, because the daemon keeps it alive. Unix-only; skips if `pgrep` is unavailable.
#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use potty::proto::{Control, Frame};

fn have(bin: &str) -> bool {
    Command::new(bin).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

/// Does any running process's command line contain `needle`?
fn pgrep(needle: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", needle])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn shell_survives_client_disconnect() {
    if !have("pgrep") {
        eprintln!("skipping: pgrep unavailable");
        return;
    }

    // A unique socket + a unique marker. The marker is the sleep's duration so it lands in the
    // process's argv (a shell comment would be stripped before exec).
    let tag = std::process::id();
    let sock = std::env::temp_dir().join(format!("potty-persist-{tag}.sock"));
    let marker = format!("8{tag}"); // e.g. "8132462" → `sleep 8132462`, grep-able and unique
    let _ = std::fs::remove_file(&sock);

    // Attach: this spawns + detaches the daemon, then relays our stdin/stdout to it.
    let mut attach = Command::new(env!("CARGO_BIN_EXE_potty-session"))
        .env("POTTY_SESSION_SOCK", &sock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn potty-session attach");
    let mut stdin = attach.stdin.take().unwrap();

    // Open a pane and start a long-lived marker process in it.
    Frame::Control(Control::Hello { version: 1 }).write(&mut stdin).unwrap();
    Frame::Control(Control::Open { pane: 1, cols: 80, rows: 24 }).write(&mut stdin).unwrap();
    let cmd = format!("sleep {marker}\r"); // long sleep whose duration is our unique marker
    Frame::Data { pane: 1, bytes: cmd.into_bytes() }.write(&mut stdin).unwrap();

    // Wait until the marker process is actually running.
    let started = wait_until(Duration::from_secs(10), || pgrep(&marker));
    assert!(started, "marker process never started");

    // Disconnect the client: close stdin and kill the attach relay (simulates the SSH channel
    // closing). The daemon — and the shell's sleep — must keep running.
    drop(stdin);
    let _ = attach.kill();
    let _ = attach.wait();

    // The marker must still be alive a moment after the client is gone.
    std::thread::sleep(Duration::from_millis(500));
    let survived = pgrep(&marker);

    // Cleanup: kill the marker and the daemon, remove the socket. Match the daemon by its socket
    // path (a leading "--daemon" would be parsed by pkill as an option).
    let _ = Command::new("pkill").args(["-f", &marker]).status();
    let _ = Command::new("pkill").args(["-f", &sock.display().to_string()]).status();
    let _ = std::fs::remove_file(&sock);

    assert!(survived, "the shell's process did not survive the client disconnect");
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
