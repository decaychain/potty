//! End-to-end tests of the russh client over a throwaway localhost sshd:
//!   - publickey round trip → `potty-session` (the protocol survives real SSH),
//!   - host-key rejection aborts the connect,
//!   - ssh-agent authentication.
//! Unix-only, and each test skips (not fails) when the tools it needs aren't installed.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use potty::proto::{Control, Frame};
use potty::remote::{connect_and_exec, Authenticator, HostKeyStatus, RemoteSession, SshConfig};
use tokio::sync::mpsc::{Receiver, Sender};

fn which(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// An `Authenticator` that accepts (or rejects) host keys; no secrets needed for these tests.
struct AcceptHost(bool);
impl Authenticator for AcceptHost {
    fn accept_host_key(&self, _host: &str, _fp: &str, _status: HostKeyStatus) -> bool {
        self.0
    }
}

/// A throwaway sshd on localhost with its own host + client keys. Cleaned up on drop.
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

fn keygen(keygen: &Path, out: &Path) -> bool {
    Command::new(keygen)
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `None` → skip the test (tools unavailable / sshd wouldn't start).
fn start_sshd() -> Option<Sshd> {
    let sshd = which(&["/usr/sbin/sshd", "/usr/bin/sshd"])?;
    let keygen_bin = which(&["/usr/bin/ssh-keygen", "/bin/ssh-keygen"])?;

    let dir = std::env::temp_dir().join(format!("potty-sshtest-{}-{}", std::process::id(), free_port()));
    std::fs::create_dir_all(&dir).ok()?;
    let hostkey = dir.join("hostkey");
    let client_key = dir.join("clientkey");
    let authorized = dir.join("authorized_keys");
    let config = dir.join("sshd_config");

    if !keygen(&keygen_bin, &hostkey) || !keygen(&keygen_bin, &client_key) {
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

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(Sshd { child, dir, port, client_key });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn user() -> Option<String> {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok()
}

/// The remote command: run potty-session inline (no daemon) so these transport tests don't leave
/// a persistent daemon behind. The remote shell applies the env prefix.
fn session_cmd() -> String {
    format!("POTTY_SESSION_NODAEMON=1 {}", env!("CARGO_BIN_EXE_potty-session"))
}

fn config(sshd: &Sshd, user: &str, keys: Vec<PathBuf>, use_agent: bool, agent_sock: Option<PathBuf>) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".into(),
        port: sshd.port,
        user: user.into(),
        keys,
        known_hosts: Some(sshd.dir.join("known_hosts")),
        use_agent,
        agent_sock,
    }
}

/// Open a pane, run a marker echo in it, and assert the output round-trips back as frames. `session`
/// is kept alive (it owns the SSH handle) for the duration.
async fn assert_echo_round_trip(session: RemoteSession, outbound: Sender<Frame>, mut rx: Receiver<Frame>) {
    for f in [
        Frame::Control(Control::Hello { version: 1 }),
        Frame::Control(Control::Open { pane: 1, cols: 80, rows: 24 }),
        Frame::Data { pane: 1, bytes: b"echo SSH_ROUNDTRIP_OK\r".to_vec() },
    ] {
        outbound.send(f).await.expect("send frame");
    }
    let _keep = session;

    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Frame::Data { pane: 1, bytes })) => out.extend(bytes),
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
        if contains(&out, b"SSH_ROUNDTRIP_OK") {
            return;
        }
    }
    panic!("echo did not round-trip over SSH");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publickey_round_trip_to_potty_session() {
    let Some(sshd) = start_sshd() else {
        eprintln!("skipping: sshd/ssh-keygen unavailable");
        return;
    };
    let Some(user) = user() else { return };

    let cfg = config(&sshd, &user, vec![sshd.client_key.clone()], false, None);
    let (session, outbound, rx) =
        connect_and_exec(&cfg, std::sync::Arc::new(AcceptHost(true)), &session_cmd())
            .await
            .expect("connect/auth/exec over ssh");
    assert_echo_round_trip(session, outbound, rx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_host_key_aborts_connect() {
    let Some(sshd) = start_sshd() else { return };
    let Some(user) = user() else { return };

    // The host key is unknown (fresh known_hosts) and the authenticator refuses it.
    let cfg = config(&sshd, &user, vec![sshd.client_key.clone()], false, None);
    let result =
        connect_and_exec(&cfg, std::sync::Arc::new(AcceptHost(false)), &session_cmd()).await;
    assert!(result.is_err(), "connect should fail when the host key is rejected");
}

/// A host without `potty-session`: the SSH exec is accepted, but the shell's "command not found"
/// closes the channel before any protocol frame. The connection must end without a `Welcome`, and
/// the captured stderr must be non-empty so the UI can explain the failure (vs. a silent dead tab).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_remote_command_is_reported() {
    let Some(sshd) = start_sshd() else { return };
    let Some(user) = user() else { return };

    let cfg = config(&sshd, &user, vec![sshd.client_key.clone()], false, None);
    let (session, outbound, mut rx) =
        connect_and_exec(&cfg, std::sync::Arc::new(AcceptHost(true)), "potty-session-DEFINITELY-NOT-INSTALLED")
            .await
            .expect("exec request is accepted even though the command is missing");

    // Mimic the client: greet, then drain until the channel closes.
    outbound.send(Frame::Control(Control::Hello { version: 1 })).await.ok();
    let mut got_welcome = false;
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        if matches!(frame, Frame::Control(Control::Welcome { .. })) {
            got_welcome = true;
        }
    }

    assert!(!got_welcome, "a host without potty-session should never send Welcome");
    assert!(!session.stderr().is_empty(), "the remote's error output should have been captured");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_auth_round_trip() {
    let Some(sshd) = start_sshd() else { return };
    let Some(user) = user() else { return };
    let (Some(ssh_agent), Some(ssh_add)) =
        (which(&["/usr/bin/ssh-agent"]), which(&["/usr/bin/ssh-add"]))
    else {
        eprintln!("skipping: ssh-agent/ssh-add unavailable");
        return;
    };

    // Start an agent on its own socket and load the (unencrypted) client key into it.
    let sock = sshd.dir.join("agent.sock");
    let agent = Command::new(&ssh_agent)
        .args(["-D", "-a"])
        .arg(&sock)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn ssh-agent");
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    // Wait for the agent socket, then add the key.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !sock.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let added = Command::new(&ssh_add)
        .arg(&sshd.client_key)
        .env("SSH_AUTH_SOCK", &sock)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _guard = Kill(agent);
    if !added {
        eprintln!("skipping: ssh-add failed");
        return;
    }

    // Authenticate via the agent only — no key files.
    let cfg = config(&sshd, &user, vec![], true, Some(sock));
    let (session, outbound, rx) =
        connect_and_exec(&cfg, std::sync::Arc::new(AcceptHost(true)), &session_cmd())
            .await
            .expect("agent auth over ssh");
    assert_echo_round_trip(session, outbound, rx).await;
    // `_guard` keeps the agent process alive until here.
}
