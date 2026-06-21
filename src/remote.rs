//! russh-based client for remote sessions: connect to a host's sshd, authenticate, exec
//! `potty-session`, and exchange wire-protocol frames over the channel.
//!
//! This is **step 1** of the client (`docs/remote-sessions.md`): publickey auth only, the host
//! key accepted blindly, no agent/password fallback — just enough to prove the SSH round trip.
//! The auth ladder (agent → key → keyboard-interactive) and host-key verification come next.
//!
//! Note: pulling russh in here means the lib (and thus `potty-session`) compiles it; once the
//! remote-deploy build matters, this module should move behind a `client` feature so the headless
//! server can be built without it.

use std::path::Path;
use std::sync::Arc;

use russh::client::{self, Handle};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use tokio::net::ToSocketAddrs;
use tokio::sync::mpsc;

use crate::proto::Frame;

/// Accepts any server key. TODO(step 2): check known_hosts and prompt on mismatch.
struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// A live remote session. Send frames on `outbound`; receive the remote's frames from the channel
/// returned by [`connect_and_exec`]. Keep this alive — dropping it tears the SSH session down.
pub struct RemoteSession {
    /// Held only to keep the SSH session alive for as long as the caller wants it.
    _session: Handle<ClientHandler>,
    pub outbound: mpsc::Sender<Frame>,
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Connect to `addr` as `user`, authenticate with the private key at `key_path`, exec `command`
/// (e.g. `"potty-session"`), and bridge its stdio to wire-protocol frames. Returns the session and
/// a receiver of frames the remote sends; frames sent on `session.outbound` reach the remote.
pub async fn connect_and_exec(
    addr: impl ToSocketAddrs,
    user: &str,
    key_path: impl AsRef<Path>,
    command: &str,
) -> std::io::Result<(RemoteSession, mpsc::Receiver<Frame>)> {
    let key = load_secret_key(key_path, None).map_err(io_err)?;
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, ClientHandler).await.map_err(io_err)?;

    let rsa_hash = session.best_supported_rsa_hash().await.map_err(io_err)?.flatten();
    let auth = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash))
        .await
        .map_err(io_err)?;
    if !auth.success() {
        return Err(std::io::Error::other("publickey authentication failed"));
    }

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
                continue; // ExitStatus/Eof/etc.; the loop ends when wait() returns None.
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
                    Err(_) => return, // protocol desync — give up on this channel
                }
            }
        }
    });

    // Writer: encode outbound frames onto the channel.
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let mut bytes = Vec::new();
            if frame.write(&mut bytes).is_err() {
                break;
            }
            if write_half.data(&bytes[..]).await.is_err() {
                break;
            }
        }
    });

    Ok((RemoteSession { _session: session, outbound: out_tx }, in_rx))
}
