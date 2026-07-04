//! russh-based client for remote sessions: connect to a host's sshd, authenticate, exec
//! `potty-session`, and exchange wire-protocol frames over the channel.
//!
//! **Step 2** (`docs/remote-sessions.md`): the auth ladder — agent → key files (passphrase) →
//! keyboard-interactive → password — plus host-key verification against known_hosts. The
//! interactive bits (passphrase, prompts, host-key approval) go through [`Authenticator`], which
//! the GUI will implement with dialogs (step 3 bridges these sync calls to the UI thread).
//!
//! Note: pulling russh in here means the lib (and thus `potty-session`) compiles it; once the
//! remote-deploy build matters, this module should move behind a `client` feature.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::ChannelMsg;
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::known_hosts::learn_known_hosts;
use russh::keys::{
    HashAlg, PrivateKeyWithHashAlg, PublicKey, check_known_hosts, check_known_hosts_path,
    load_secret_key,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::proto::{Control, Frame, PaneId};

/// A server host key is either unrecognised, or differs from a recorded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyStatus {
    Unknown,
    Changed,
}

/// One keyboard-interactive prompt from the server.
pub struct PromptInfo {
    pub prompt: String,
    /// Whether the typed response should be echoed (false for passwords).
    pub echo: bool,
}

/// Supplies the interactive parts of authentication. The GUI implements this with dialogs; tests
/// with canned answers. Methods are sync for now — the GUI wiring (step 3) bridges them to the UI
/// thread. Defaults decline, so an impl only overrides what it handles.
pub trait Authenticator: Send + Sync {
    /// Trust a server host key that isn't already in known_hosts (or that changed)? Default: no.
    fn accept_host_key(&self, _host: &str, _fingerprint: &str, _status: HostKeyStatus) -> bool {
        false
    }
    /// Passphrase to decrypt an encrypted key file (None → skip it).
    fn key_passphrase(&self, _path: &str) -> Option<String> {
        None
    }
    /// Answer a keyboard-interactive challenge, one response per prompt (None → abandon method).
    fn answer(
        &self,
        _name: &str,
        _instructions: &str,
        _prompts: &[PromptInfo],
    ) -> Option<Vec<String>> {
        None
    }
    /// Password for the plain `password` method (None → skip).
    fn password(&self, _user: &str, _host: &str) -> Option<String> {
        None
    }
    /// Whether a prompt is currently on screen waiting for the user. While true, the connect
    /// timeout clock is paused — a host-key dialog can legitimately stay open for minutes.
    fn prompt_in_flight(&self) -> bool {
        false
    }
}

/// Budget for establishing one SSH connection (TCP + banner + key exchange); see
/// [`SshConfig::connect_timeout`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Where and how to connect.
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Environment variables requested for each remote channel. Persistent `potty-session`
    /// connections also prefix the exec command so arbitrary names work without server `AcceptEnv`.
    pub env: BTreeMap<String, String>,
    /// Private key files to try, in order, after the agent.
    pub keys: Vec<PathBuf>,
    /// known_hosts file; `None` → the default (`~/.ssh/known_hosts`).
    pub known_hosts: Option<PathBuf>,
    /// Try the ssh-agent first.
    pub use_agent: bool,
    /// Explicit agent socket; `None` → `$SSH_AUTH_SOCK` (Unix) / Pageant (Windows).
    pub agent_sock: Option<PathBuf>,
    /// Budget for establishing one SSH connection (TCP + banner + key exchange); time the user
    /// spends in authenticator prompts doesn't count. The auth ladder makes a fresh connection
    /// per method group, and servers that rate-limit after failed logins (fail2ban, sshd
    /// `PerSourcePenalties`) often silently drop the later ones — without a budget, a connect
    /// blocks forever and the GUI shows neither an error nor a tab.
    pub connect_timeout: Duration,
}

/// A live remote session — just the SSH handle. Keep it alive while the session is in use; dropping
/// it tears the SSH session down. The outbound `Sender` and inbound `Receiver` are returned
/// alongside it by [`connect_and_exec`]; when every clone of the outbound `Sender` drops, the
/// writer signals channel EOF (so the remote relay exits and the daemon detaches) — that's how the
/// client closes a connection after its last pane goes away.
pub struct RemoteSession {
    /// The SSH handle for the `potty-session` path (one channel). `None` for the raw-shell path,
    /// where the handle lives in the coordinator task that owns the per-pane channels.
    _session: Option<Handle<ClientHandler>>,
    /// Remote stderr captured from the channel (capped). Lets the caller explain a session that
    /// closed before speaking the protocol — typically a shell's "potty-session: command not found".
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl RemoteSession {
    /// A snapshot of the captured remote stderr, trimmed. Empty if the remote said nothing.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap())
            .trim()
            .to_string()
    }
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// TEMPORARY DIAGNOSTICS for the silent-connect bug: `POTTY_SSH_DEBUG=1 potty` prints
/// timestamped connect/auth/channel steps to stderr (never secrets). Grep for `[potty-ssh]`.
pub fn sshdbg(msg: impl AsRef<str>) {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ON: OnceLock<bool> = OnceLock::new();
    static T0: OnceLock<Instant> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("POTTY_SSH_DEBUG").is_some()) {
        return;
    }
    let t0 = *T0.get_or_init(Instant::now);
    eprintln!(
        "[potty-ssh {:>9.3}s] {}",
        t0.elapsed().as_secs_f64(),
        msg.as_ref()
    );
}

/// Verifies host keys against known_hosts, prompting through the [`Authenticator`] for unknown or
/// changed keys (and recording an accepted unknown key).
struct ClientHandler {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
    auth: Arc<dyn Authenticator>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let known = match &self.known_hosts {
            Some(p) => check_known_hosts_path(&self.host, self.port, key, p),
            None => check_known_hosts(&self.host, self.port, key),
        };
        let status = match known {
            Ok(true) => {
                sshdbg(format!("{}: host key recognised", self.host));
                return Ok(true); // recognised and matches
            }
            Ok(false) => HostKeyStatus::Unknown,
            Err(russh::keys::Error::KeyChanged { .. }) => HostKeyStatus::Changed,
            Err(_) => HostKeyStatus::Unknown, // missing/unreadable known_hosts — let the user decide
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        sshdbg(format!(
            "{}: host key {status:?} ({fingerprint}) — asking user",
            self.host
        ));
        if self.auth.accept_host_key(&self.host, &fingerprint, status) {
            sshdbg(format!("{}: host key accepted by user", self.host));
            // Record a newly-accepted key so we don't ask again. Best-effort.
            let _ = match &self.known_hosts {
                Some(p) => {
                    russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, p)
                }
                None => learn_known_hosts(&self.host, self.port, key),
            };
            Ok(true)
        } else {
            sshdbg(format!("{}: host key REJECTED by user", self.host));
            Ok(false)
        }
    }
}

/// Connect, authenticate, exec `command` (e.g. `"potty-session"`), and bridge its stdio to wire
/// frames. Returns the session handle, an outbound `Sender` (frames to the remote), and a
/// `Receiver` of frames the remote sends. Closing the connection is done by dropping every clone of
/// the outbound `Sender`: the writer then signals channel EOF and the remote side tears down.
pub async fn connect_and_exec(
    cfg: &SshConfig,
    auth: Arc<dyn Authenticator>,
    command: &str,
) -> std::io::Result<(RemoteSession, mpsc::Sender<Frame>, mpsc::Receiver<Frame>)> {
    let session = authenticate(cfg, &auth).await?;

    let channel = session.channel_open_session().await.map_err(io_err)?;
    send_env_requests(&channel, &cfg.env).await;
    let command = command_with_env(command, &cfg.env);
    sshdbg(format!("exec: {command}"));
    channel.exec(true, command).await.map_err(io_err)?;
    let (mut read_half, write_half) = channel.split();

    let (in_tx, in_rx) = mpsc::channel::<Frame>(256); // remote → us
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256); // us → remote
    let stderr = Arc::new(Mutex::new(Vec::new()));

    // Reader: reassemble channel data into frames (Data chunks don't respect frame boundaries), and
    // capture stderr so a session that dies before greeting us (e.g. `potty-session` not installed)
    // can be explained.
    let stderr_w = stderr.clone();
    tokio::spawn(async move {
        const STDERR_CAP: usize = 4096;
        let mut buf = Vec::new();
        while let Some(msg) = read_half.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    buf.extend_from_slice(&data);
                    loop {
                        match Frame::try_parse(&buf) {
                            Ok(Some((frame, used))) => {
                                buf.drain(..used);
                                if in_tx.send(frame).await.is_err() {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => return, // protocol desync
                        }
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    let mut s = stderr_w.lock().unwrap();
                    let room = STDERR_CAP.saturating_sub(s.len());
                    s.extend_from_slice(&data[..data.len().min(room)]);
                }
                _ => {} // the loop ends when wait() returns None
            }
        }
    });

    // Writer: encode outbound frames onto the channel. When every outbound `Sender` has dropped
    // (the client closed this connection — e.g. its last pane went away), `recv` returns `None`; we
    // then signal EOF so the remote relay's stdin closes, it exits, and the daemon detaches (and
    // idle-exits if nothing's left). This happens *after* any queued frames (like `Close`) are sent,
    // so the daemon still processes them first.
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let mut bytes = Vec::new();
            if frame.write(&mut bytes).is_err() || write_half.data(&bytes[..]).await.is_err() {
                return;
            }
        }
        let _ = write_half.eof().await;
        let _ = write_half.close().await;
    });

    Ok((
        RemoteSession {
            _session: Some(session),
            stderr,
        },
        out_tx,
        in_rx,
    ))
}

/// Connect and authenticate, then run a **plain interactive shell** over SSH — no `potty-session`,
/// no persistence. A coordinator task speaks the same wire protocol to the GUI as `connect_and_exec`
/// does, but backs each pane with its own SSH channel (PTY + shell) on the shared session, so the
/// entire GUI (panes, splits, tabs, resize) works unchanged. Closing potty drops the session and the
/// shells with it. Use this for hosts that don't run `potty-session`.
pub async fn shell_session(
    cfg: &SshConfig,
    auth: Arc<dyn Authenticator>,
) -> std::io::Result<(RemoteSession, mpsc::Sender<Frame>, mpsc::Receiver<Frame>)> {
    let handle = authenticate(cfg, &auth).await?;

    let (in_tx, in_rx) = mpsc::channel::<Frame>(256); // shells → us
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256); // us → shells
    let env = cfg.env.clone();

    // Coordinator: owns the SSH handle and one channel per pane, translating proto ↔ raw SSH.
    tokio::spawn(async move {
        // Per-pane channel write halves (input/resize/close); reader tasks own the read halves.
        let mut panes: HashMap<PaneId, russh::ChannelWriteHalf<client::Msg>> = HashMap::new();
        while let Some(frame) = out_rx.recv().await {
            match frame {
                // Greet exactly like the daemon would, so the GUI's connect flow proceeds: with no
                // panes to restore it will open a fresh one (an `Open`). There is exactly one
                // client on a plain shell, so it holds focus by construction.
                Frame::Control(Control::Hello { .. }) => {
                    sshdbg("shell coordinator: Hello received — greeting client");
                    let _ = in_tx
                        .send(Frame::Control(Control::Welcome {
                            version: crate::proto::PROTOCOL_VERSION,
                            client: 1,
                        }))
                        .await;
                    let _ = in_tx
                        .send(Frame::Control(Control::Focus { owner: 1 }))
                        .await;
                    let _ = in_tx.send(Frame::Control(Control::Ready)).await;
                }
                // A new pane → a new channel running a login shell in a PTY of the requested size.
                Frame::Control(Control::Open {
                    pane, cols, rows, ..
                }) => {
                    sshdbg(format!("pane {pane}: opening shell channel ({cols}x{rows})"));
                    match open_shell_channel(&handle, &env, cols, rows).await {
                        Ok(channel) => {
                            sshdbg(format!("pane {pane}: shell channel open"));
                            let (mut read, write) = channel.split();
                            panes.insert(pane, write);
                            let _ = in_tx.send(Frame::Control(Control::Opened { pane })).await;
                            // Reader: raw shell output (incl. stderr) → this pane's Data frames;
                            // channel close → the shell exited.
                            let to_gui = in_tx.clone();
                            tokio::spawn(async move {
                                while let Some(msg) = read.wait().await {
                                    let bytes = match msg {
                                        ChannelMsg::Data { data } => data.to_vec(),
                                        ChannelMsg::ExtendedData { data, .. } => data.to_vec(),
                                        _ => continue,
                                    };
                                    if to_gui.send(Frame::Data { pane, bytes }).await.is_err() {
                                        return;
                                    }
                                }
                                sshdbg(format!("pane {pane}: channel closed — shell exited"));
                                let _ = to_gui.send(Frame::Control(Control::Exited { pane })).await;
                            });
                        }
                        Err(e) => {
                            sshdbg(format!("pane {pane}: OPEN SHELL CHANNEL FAILED: {e}"));
                            let _ = in_tx.send(Frame::Control(Control::Exited { pane })).await;
                        }
                    }
                }
                Frame::Data { pane, bytes } => {
                    if let Some(w) = panes.get(&pane) {
                        let _ = w.data(&bytes[..]).await;
                    }
                }
                Frame::Control(Control::Resize { pane, cols, rows }) => {
                    if let Some(w) = panes.get(&pane) {
                        let _ = w.window_change(cols as u32, rows as u32, 0, 0).await;
                    }
                }
                Frame::Control(Control::Close { pane }) => {
                    if let Some(w) = panes.remove(&pane) {
                        let _ = w.eof().await;
                        let _ = w.close().await;
                    }
                }
                // No persistence: layout pushes and other controls are irrelevant here.
                Frame::Control(_) => {}
            }
        }
        // Every outbound `Sender` dropped (connection closed) → drop the handle, ending the session
        // and all its channels; the reader tasks then finish.
    });

    let stderr = Arc::new(Mutex::new(Vec::new())); // unused here (raw shells always greet), kept for API parity
    Ok((
        RemoteSession {
            _session: None,
            stderr,
        },
        out_tx,
        in_rx,
    ))
}

/// Open one SSH channel with a PTY of `cols`×`rows` running a login shell.
async fn open_shell_channel(
    handle: &Handle<ClientHandler>,
    env: &BTreeMap<String, String>,
    cols: u16,
    rows: u16,
) -> std::io::Result<russh::Channel<client::Msg>> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| io_err(format!("channel open: {e}")))?;
    sshdbg("shell channel: session channel opened, requesting pty");
    send_env_requests(&channel, env).await;
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
        .map_err(|e| io_err(format!("pty request: {e}")))?;
    sshdbg("shell channel: pty requested, starting shell");
    channel
        .request_shell(true)
        .await
        .map_err(|e| io_err(format!("shell request: {e}")))?;
    Ok(channel)
}

async fn send_env_requests(channel: &russh::Channel<client::Msg>, env: &BTreeMap<String, String>) {
    for (name, value) in valid_env(env) {
        let _ = channel.set_env(false, name, value).await;
    }
}

fn command_with_env(command: &str, env: &BTreeMap<String, String>) -> String {
    let assignments: Vec<String> = valid_env(env)
        .map(|(name, value)| format!("{name}={}", shell_quote(value)))
        .collect();
    if assignments.is_empty() {
        command.to_string()
    } else {
        format!("{} {command}", assignments.join(" "))
    }
}

fn valid_env(env: &BTreeMap<String, String>) -> impl Iterator<Item = (&str, &str)> {
    env.iter()
        .filter(|(name, value)| valid_env_name(name) && !value.contains('\0'))
        .map(|(name, value)| (name.as_str(), value.as_str()))
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Open a fresh SSH connection (its own `MaxAuthTries` budget) and verify the host key.
///
/// The whole attempt runs under [`SshConfig::connect_timeout`]: russh's `connect` is a raw
/// `TcpStream::connect` plus an unbounded banner read, so a server that accepts TCP but never
/// speaks (how fail2ban's DROP looks to the client) would otherwise hang the ladder forever.
/// The clock only advances while no authenticator prompt is on screen, so a host-key dialog
/// left open doesn't abort the attempt.
async fn connect_session(
    cfg: &SshConfig,
    auth: &Arc<dyn Authenticator>,
) -> std::io::Result<Handle<ClientHandler>> {
    let handler = ClientHandler {
        host: cfg.host.clone(),
        port: cfg.port,
        known_hosts: cfg.known_hosts.clone(),
        auth: auth.clone(),
    };
    let config = Arc::new(client::Config::default());
    sshdbg(format!(
        "{}:{}: opening connection (timeout {}s)",
        cfg.host,
        cfg.port,
        cfg.connect_timeout.as_secs()
    ));
    let connect = client::connect(config, (cfg.host.as_str(), cfg.port), handler);
    tokio::pin!(connect);
    const TICK: Duration = Duration::from_millis(100);
    let mut left = cfg.connect_timeout;
    loop {
        match tokio::time::timeout(TICK, &mut connect).await {
            Ok(r) => {
                match &r {
                    Ok(_) => sshdbg(format!("{}:{}: connected, handshake done", cfg.host, cfg.port)),
                    Err(e) => sshdbg(format!("{}:{}: connect FAILED: {e}", cfg.host, cfg.port)),
                }
                return r.map_err(io_err);
            }
            Err(_) if auth.prompt_in_flight() => {} // user is reading a dialog — clock paused
            Err(_) => {
                left = left.saturating_sub(TICK);
                if left.is_zero() {
                    sshdbg(format!("{}:{}: connect TIMED OUT", cfg.host, cfg.port));
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "{}:{} did not respond within {}s — after failed authentication \
                             attempts, servers often rate-limit new connections (fail2ban, sshd \
                             penalties); waiting a few minutes usually clears it",
                            cfg.host,
                            cfg.port,
                            cfg.connect_timeout.as_secs()
                        ),
                    ));
                }
            }
        }
    }
}

/// Authenticate, returning the authenticated session. **Each method group runs on its own fresh
/// connection** — a server with a low `MaxAuthTries` (e.g. 6) and an agent holding ≥6 keys would
/// otherwise exhaust the budget on rejected keys and disconnect before the password method is ever
/// reached. A fresh connection resets that budget, so a working method downstream of a key-heavy
/// agent is still reachable.
async fn authenticate(
    cfg: &SshConfig,
    auth: &Arc<dyn Authenticator>,
) -> std::io::Result<Handle<ClientHandler>> {
    // We connect lazily per group so an early success costs only one connection. The first connect
    // also records/learns the host key, so later groups don't re-prompt.

    // 1. ssh-agent (may offer many keys — hence its own connection).
    if cfg.use_agent {
        #[cfg(unix)]
        let agent = connect_agent_unix(cfg).await;
        // Pageant's stream type comes from an external crate russh doesn't re-export, so we can't
        // name it — call the constructor inline and let inference carry the type to `agent_auth`.
        #[cfg(windows)]
        let agent = AgentClient::connect_pageant().await.ok();
        if let Some(agent) = agent {
            sshdbg("auth rung 1: ssh-agent");
            let mut session = connect_session(cfg, auth).await?;
            let rsa_hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(io_err)?
                .flatten();
            // Ok(false)/Err both mean "agent didn't get us in" → reconnect for the next group.
            match agent_auth(&mut session, &cfg.user, agent, rsa_hash).await {
                Ok(true) => {
                    sshdbg("agent: authenticated");
                    return Ok(session);
                }
                Ok(false) => sshdbg("agent: no key accepted"),
                Err(Disconnected) => {
                    sshdbg("agent: server DISCONNECTED mid-auth (attempt cap?)")
                }
            }
        } else {
            sshdbg("auth rung 1: no ssh-agent reachable — skipping");
        }
    }

    // 2. Key files.
    if !cfg.keys.is_empty() {
        sshdbg(format!("auth rung 2: {} key file(s)", cfg.keys.len()));
        let mut session = connect_session(cfg, auth).await?;
        let rsa_hash = session
            .best_supported_rsa_hash()
            .await
            .map_err(io_err)?
            .flatten();
        for key_path in &cfg.keys {
            if try_key(&mut session, &cfg.user, key_path, auth, rsa_hash).await {
                sshdbg(format!("key file {}: authenticated", key_path.display()));
                return Ok(session);
            }
            sshdbg(format!("key file {}: not accepted", key_path.display()));
        }
    }

    // 3. Keyboard-interactive (PAM passwords, OTP, SSSD fallback).
    {
        sshdbg("auth rung 3: keyboard-interactive");
        let mut session = connect_session(cfg, auth).await?;
        if try_keyboard_interactive(&mut session, &cfg.user, auth)
            .await
            .unwrap_or(false)
        {
            sshdbg("keyboard-interactive: authenticated");
            return Ok(session);
        }
        sshdbg("keyboard-interactive: not accepted");
    }

    // 4. Plain password.
    sshdbg("auth rung 4: password — prompting user");
    if let Some(pw) = auth.password(&cfg.user, &cfg.host) {
        sshdbg("password prompt answered — reconnecting");
        let mut session = connect_session(cfg, auth).await?;
        match session.authenticate_password(&cfg.user, pw).await {
            Ok(r) if r.success() => {
                sshdbg("password: authenticated");
                return Ok(session);
            }
            Ok(r) => sshdbg(format!("password: rejected ({r:?})")),
            Err(e) => sshdbg(format!("password: transport error: {e}")),
        }
    } else {
        sshdbg("password prompt cancelled");
    }

    sshdbg("auth: ALL METHODS FAILED");
    Err(std::io::Error::other("all authentication methods failed"))
}

#[cfg(unix)]
async fn connect_agent_unix(cfg: &SshConfig) -> Option<AgentClient<tokio::net::UnixStream>> {
    match &cfg.agent_sock {
        Some(p) => AgentClient::connect_uds(p).await.ok(),
        None => AgentClient::connect_env().await.ok(),
    }
}

/// The SSH session dropped mid-authentication (the server likely caps attempts).
struct Disconnected;

/// Try every identity the agent holds, letting the agent do the signing. `Err(Disconnected)` means
/// the session died (vs `Ok(false)` = all keys rejected) — so the caller stops hammering a server
/// that caps attempts and reports it clearly instead of failing obscurely later.
async fn agent_auth<R>(
    session: &mut Handle<ClientHandler>,
    user: &str,
    mut agent: AgentClient<R>,
    rsa_hash: Option<HashAlg>,
) -> Result<bool, Disconnected>
where
    R: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Ok(identities) = agent.request_identities().await else {
        sshdbg("agent: request_identities failed");
        return Ok(false);
    };
    sshdbg(format!("agent: {} identities", identities.len()));
    for id in identities {
        // Certificates are skipped for now; plain keys cover the common case.
        if let AgentIdentity::PublicKey { key, .. } = id {
            let label = format!(
                "agent key {} ({})",
                key.fingerprint(HashAlg::Sha256),
                key.comment()
            );
            match session
                .authenticate_publickey_with(user, key, rsa_hash, &mut agent)
                .await
            {
                Ok(result) if result.success() => {
                    sshdbg(format!("{label}: ACCEPTED"));
                    return Ok(true);
                }
                Ok(r) => sshdbg(format!("{label}: rejected ({r:?})")), // try the next identity
                Err(e) => {
                    sshdbg(format!("{label}: transport error: {e}"));
                    return Err(Disconnected); // transport/session gone
                }
            }
        }
    }
    Ok(false)
}

/// Try one private-key file, prompting for a passphrase if it's encrypted.
async fn try_key(
    session: &mut Handle<ClientHandler>,
    user: &str,
    path: &Path,
    auth: &Arc<dyn Authenticator>,
    rsa_hash: Option<HashAlg>,
) -> bool {
    if !path.exists() {
        return false;
    }
    let key = match load_secret_key(path, None) {
        Ok(k) => k,
        // Most likely encrypted — ask for the passphrase and retry.
        Err(_) => match auth.key_passphrase(&path.display().to_string()) {
            Some(pw) => match load_secret_key(path, Some(&pw)) {
                Ok(k) => k,
                Err(_) => return false,
            },
            None => return false,
        },
    };
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
    matches!(session.authenticate_publickey(user, key).await, Ok(r) if r.success())
}

/// Drive a keyboard-interactive exchange, answering prompts via the [`Authenticator`].
async fn try_keyboard_interactive(
    session: &mut Handle<ClientHandler>,
    user: &str,
    auth: &Arc<dyn Authenticator>,
) -> std::io::Result<bool> {
    let response = match session
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            sshdbg(format!("keyboard-interactive: start failed: {e}"));
            return Ok(false); // method unavailable or session gone — let the ladder finish cleanly
        }
    };
    let mut response = response;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => {
                sshdbg("keyboard-interactive: server says failure");
                return Ok(false);
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                sshdbg(format!(
                    "keyboard-interactive: info request name={name:?} ({} prompt(s): {:?})",
                    prompts.len(),
                    prompts.iter().map(|p| &p.prompt).collect::<Vec<_>>()
                ));
                let infos: Vec<PromptInfo> = prompts
                    .into_iter()
                    .map(|p| PromptInfo {
                        prompt: p.prompt,
                        echo: p.echo,
                    })
                    .collect();
                let Some(answers) = auth.answer(&name, &instructions, &infos) else {
                    sshdbg("keyboard-interactive: prompt cancelled");
                    return Ok(false);
                };
                response = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(io_err)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::command_with_env;

    #[test]
    fn command_with_env_shell_quotes_values() {
        let mut env = BTreeMap::new();
        env.insert("POTTY_ONE".to_string(), "simple".to_string());
        env.insert("POTTY_TWO".to_string(), "space and 'quote'".to_string());

        assert_eq!(
            command_with_env("potty-session --flag", &env),
            "POTTY_ONE='simple' POTTY_TWO='space and '\\''quote'\\''' potty-session --flag"
        );
    }

    #[test]
    fn command_with_env_skips_invalid_assignments() {
        let mut env = BTreeMap::new();
        env.insert("1BAD".to_string(), "nope".to_string());
        env.insert("ALSO-BAD".to_string(), "nope".to_string());
        env.insert("GOOD_NAME".to_string(), "ok".to_string());
        env.insert("NUL_VALUE".to_string(), "bad\0value".to_string());

        assert_eq!(
            command_with_env("potty-session", &env),
            "GOOD_NAME='ok' potty-session"
        );
    }
}
