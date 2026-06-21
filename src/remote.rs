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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::known_hosts::learn_known_hosts;
use russh::keys::{
    check_known_hosts, check_known_hosts_path, load_secret_key, HashAlg, PrivateKeyWithHashAlg,
    PublicKey,
};
use russh::ChannelMsg;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::proto::Frame;

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
    fn answer(&self, _name: &str, _instructions: &str, _prompts: &[PromptInfo]) -> Option<Vec<String>> {
        None
    }
    /// Password for the plain `password` method (None → skip).
    fn password(&self, _user: &str, _host: &str) -> Option<String> {
        None
    }
}

/// Where and how to connect.
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Private key files to try, in order, after the agent.
    pub keys: Vec<PathBuf>,
    /// known_hosts file; `None` → the default (`~/.ssh/known_hosts`).
    pub known_hosts: Option<PathBuf>,
    /// Try the ssh-agent first.
    pub use_agent: bool,
    /// Explicit agent socket; `None` → `$SSH_AUTH_SOCK` (Unix) / Pageant (Windows).
    pub agent_sock: Option<PathBuf>,
}

/// A live remote session. Send frames on `outbound`; receive the remote's frames from the channel
/// returned by [`connect_and_exec`]. Keep this alive — dropping it tears the SSH session down.
pub struct RemoteSession {
    _session: Handle<ClientHandler>,
    pub outbound: mpsc::Sender<Frame>,
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
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
            Ok(true) => return Ok(true), // recognised and matches
            Ok(false) => HostKeyStatus::Unknown,
            Err(russh::keys::Error::KeyChanged { .. }) => HostKeyStatus::Changed,
            Err(_) => HostKeyStatus::Unknown, // missing/unreadable known_hosts — let the user decide
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        if self.auth.accept_host_key(&self.host, &fingerprint, status) {
            // Record a newly-accepted key so we don't ask again. Best-effort.
            let _ = match &self.known_hosts {
                Some(p) => russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, p),
                None => learn_known_hosts(&self.host, self.port, key),
            };
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Connect, authenticate, exec `command` (e.g. `"potty-session"`), and bridge its stdio to wire
/// frames. Returns the session and a receiver of frames the remote sends.
pub async fn connect_and_exec(
    cfg: &SshConfig,
    auth: Arc<dyn Authenticator>,
    command: &str,
) -> std::io::Result<(RemoteSession, mpsc::Receiver<Frame>)> {
    let handler = ClientHandler {
        host: cfg.host.clone(),
        port: cfg.port,
        known_hosts: cfg.known_hosts.clone(),
        auth: auth.clone(),
    };
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
        .await
        .map_err(io_err)?;

    authenticate(&mut session, cfg, &auth).await?;

    let channel = session.channel_open_session().await.map_err(io_err)?;
    channel.exec(true, command).await.map_err(io_err)?;
    let (mut read_half, write_half) = channel.split();

    let (in_tx, in_rx) = mpsc::channel::<Frame>(256); // remote → us
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256); // us → remote

    // Reader: reassemble channel data into frames (Data chunks don't respect frame boundaries).
    tokio::spawn(async move {
        let mut buf = Vec::new();
        while let Some(msg) = read_half.wait().await {
            let ChannelMsg::Data { data } = msg else {
                continue; // the loop ends when wait() returns None
            };
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
    });

    // Writer: encode outbound frames onto the channel.
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let mut bytes = Vec::new();
            if frame.write(&mut bytes).is_err() || write_half.data(&bytes[..]).await.is_err() {
                break;
            }
        }
    });

    Ok((RemoteSession { _session: session, outbound: out_tx }, in_rx))
}

/// Run the auth ladder, returning once a method succeeds.
async fn authenticate(
    session: &mut Handle<ClientHandler>,
    cfg: &SshConfig,
    auth: &Arc<dyn Authenticator>,
) -> std::io::Result<()> {
    let rsa_hash = session.best_supported_rsa_hash().await.map_err(io_err)?.flatten();

    // 1. ssh-agent.
    if cfg.use_agent {
        #[cfg(unix)]
        if let Some(agent) = connect_agent_unix(cfg).await
            && agent_auth(session, &cfg.user, agent, rsa_hash).await
        {
            return Ok(());
        }
        #[cfg(windows)]
        if let Some(agent) = connect_agent_windows().await
            && agent_auth(session, &cfg.user, agent, rsa_hash).await
        {
            return Ok(());
        }
    }

    // 2. Key files.
    for key_path in &cfg.keys {
        if try_key(session, &cfg.user, key_path, auth, rsa_hash).await {
            return Ok(());
        }
    }

    // 3. Keyboard-interactive (PAM passwords, OTP, SSSD fallback).
    if try_keyboard_interactive(session, &cfg.user, auth).await? {
        return Ok(());
    }

    // 4. Plain password.
    if let Some(pw) = auth.password(&cfg.user, &cfg.host)
        && session.authenticate_password(&cfg.user, pw).await.map_err(io_err)?.success()
    {
        return Ok(());
    }

    Err(std::io::Error::other("all authentication methods failed"))
}

#[cfg(unix)]
async fn connect_agent_unix(cfg: &SshConfig) -> Option<AgentClient<tokio::net::UnixStream>> {
    match &cfg.agent_sock {
        Some(p) => AgentClient::connect_uds(p).await.ok(),
        None => AgentClient::connect_env().await.ok(),
    }
}

#[cfg(windows)]
async fn connect_agent_windows() -> Option<AgentClient<russh::keys::agent::client::pageant::PageantStream>> {
    AgentClient::connect_pageant().await.ok()
}

/// Try every identity the agent holds, letting the agent do the signing.
async fn agent_auth<R>(
    session: &mut Handle<ClientHandler>,
    user: &str,
    mut agent: AgentClient<R>,
    rsa_hash: Option<HashAlg>,
) -> bool
where
    R: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Ok(identities) = agent.request_identities().await else {
        return false;
    };
    for id in identities {
        // Certificates are skipped for now; plain keys cover the common case.
        if let AgentIdentity::PublicKey { key, .. } = id
            && let Ok(result) = session.authenticate_publickey_with(user, key, rsa_hash, &mut agent).await
            && result.success()
        {
            return true;
        }
    }
    false
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
    let mut response = session
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await
        .map_err(io_err)?;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                let infos: Vec<PromptInfo> = prompts
                    .into_iter()
                    .map(|p| PromptInfo { prompt: p.prompt, echo: p.echo })
                    .collect();
                let Some(answers) = auth.answer(&name, &instructions, &infos) else {
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
