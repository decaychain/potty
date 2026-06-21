//! End-to-end test of the russh client (step 1): stand up a throwaway localhost sshd, then
//! `connect_and_exec` → `potty-session` over real SSH and prove the wire protocol round-trips.
//! Unix-only, and skipped (not failed) when `sshd`/`ssh-keygen` aren't installed.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use potty::proto::{Control, Frame};
use potty::remote::connect_and_exec;

fn which(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A throwaway sshd on localhost, with its own host + client keys. Killed and cleaned on drop.
struct Sshd {
    child: Child,
    dir: PathBuf,
    port: u16,
    client_key: PathBuf,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Returns `None` (→ skip the test) if the tools aren't available.
fn start_sshd() -> Option<Sshd> {
    let sshd = which(&["/usr/sbin/sshd", "/usr/bin/sshd"])?;
    let keygen = which(&["/usr/bin/ssh-keygen", "/bin/ssh-keygen"])?;

    let dir = std::env::temp_dir().join(format!("potty-sshtest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let hostkey = dir.join("hostkey");
    let client_key = dir.join("clientkey");
    let authorized = dir.join("authorized_keys");
    let config = dir.join("sshd_config");

    let keygen_ok = |path: &Path| {
        Command::new(&keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !keygen_ok(&hostkey) || !keygen_ok(&client_key) {
        return None;
    }
    std::fs::copy(client_key.with_extension("pub"), &authorized).ok()?;

    let port = free_port();
    std::fs::write(
        &config,
        format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {hk}\n\
             PidFile {dir}/sshd.pid\n\
             AuthorizedKeysFile {auth}\n\
             PasswordAuthentication no\n\
             PubkeyAuthentication yes\n\
             UsePAM no\n\
             StrictModes no\n",
            hk = hostkey.display(),
            dir = dir.display(),
            auth = authorized.display(),
        ),
    )
    .ok()?;

    let child = Command::new(&sshd)
        .args(["-D", "-f"])
        .arg(&config)
        .arg("-E")
        .arg(dir.join("sshd.log"))
        .stdin(Stdio::null())
        .spawn()
        .ok()?;

    // Wait for it to accept connections.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(Sshd { child, dir, port, client_key });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_round_trip_to_potty_session() {
    let Some(sshd) = start_sshd() else {
        eprintln!("skipping: sshd/ssh-keygen unavailable");
        return;
    };
    let Ok(user) = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")) else {
        eprintln!("skipping: no $USER");
        return;
    };

    let session_bin = env!("CARGO_BIN_EXE_potty-session");
    let (session, mut rx) =
        connect_and_exec(("127.0.0.1", sshd.port), &user, &sshd.client_key, session_bin)
            .await
            .expect("connect/auth/exec over ssh");

    // Drive a pane and run a command in it, all over the SSH channel.
    for f in [
        Frame::Control(Control::Hello { version: 1 }),
        Frame::Control(Control::Open { pane: 1, cols: 80, rows: 24 }),
        Frame::Data { pane: 1, bytes: b"echo SSH_ROUNDTRIP_OK\r".to_vec() },
    ] {
        session.outbound.send(f).await.expect("send frame");
    }

    let mut welcomed = false;
    let mut opened = false;
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Frame::Control(Control::Welcome { .. }))) => welcomed = true,
            Ok(Some(Frame::Control(Control::Opened { pane: 1 }))) => opened = true,
            Ok(Some(Frame::Data { pane: 1, bytes })) => out.extend(bytes),
            Ok(Some(_)) => {}
            Ok(None) => break, // channel closed
            Err(_) => {}       // timeout tick
        }
        if welcomed && opened && contains(&out, b"SSH_ROUNDTRIP_OK") {
            break;
        }
    }

    assert!(welcomed, "no Welcome over SSH");
    assert!(opened, "no Opened over SSH");
    assert!(contains(&out, b"SSH_ROUNDTRIP_OK"), "echo did not round-trip over SSH");
}
