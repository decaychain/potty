//! Wire protocol for remote sessions — spoken between the `potty` client and the `potty-session`
//! server over a single byte stream (the russh channel in production; a stdio pipe in tests).
//!
//! Two frame kinds (`docs/remote-sessions.md`): compact JSON **control** messages, and raw binary
//! **data** (terminal bytes) so high-volume output carries no encoding overhead.
//!
//! ```text
//! frame   = [u32 len big-endian][payload]
//! payload = [u8 tag][...]
//!   tag 1 (Control) = JSON of `Control`
//!   tag 2 (Data)    = [u64 pane little-endian][raw bytes]
//! ```

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;

pub type PaneId = u64;

const TAG_CONTROL: u8 = 1;
const TAG_DATA: u8 = 2;
/// Reject absurd frame lengths so a desync/garbage stream can't make us allocate the world.
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Control messages (both directions), JSON-encoded. Terminal bytes travel as `Frame::Data`, not
/// here, so this stays small and human-readable on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
pub enum Control {
    /// C→S: first frame; negotiate version.
    Hello { version: u32 },
    /// S→C: acknowledge the connection. `client` is this client's daemon-assigned id (0 from a
    /// v1 daemon, which also never sends `Focus` — so `owner == client == 0` and the client
    /// correctly behaves as the sole controller).
    Welcome {
        version: u32,
        #[serde(default)]
        client: u64,
    },
    /// S→C: which attached client currently holds focus (drives layout and pane sizes). Broadcast
    /// whenever it changes; `owner == 0` means nobody (e.g. the focused client detached) until the
    /// next input claims it.
    Focus { owner: u64 },
    /// C→S: open a new pane running a shell of the given size. `cwd_from`, when present, asks the
    /// remote daemon to start the new shell in the current directory of an existing pane.
    Open {
        pane: PaneId,
        cols: u16,
        rows: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd_from: Option<PaneId>,
    },
    /// S→C: the pane is live.
    Opened { pane: PaneId },
    /// C→S (focused client): resize a pane. S→C (to the others): conform your grid — the pane has
    /// one PTY size, and the focused client's geometry is the one that wins.
    Resize { pane: PaneId, cols: u16, rows: u16 },
    /// C→S: close a pane (kill its shell).
    Close { pane: PaneId },
    /// S→C: a pane's shell exited.
    Exited { pane: PaneId },
    /// S→C: a pane that exists in the daemon but that this client doesn't know yet; the client
    /// should adopt it. Its current screen follows as `Data` frames (replay). Sent during the
    /// attach restore burst, and live when another attached client opens a pane (placement then
    /// comes with the next `LayoutTree`).
    Restore { pane: PaneId },
    /// S→C (on attach): the end of the restore burst. If nothing was restored, the client opens a
    /// fresh pane; otherwise it has adopted the daemon's panes.
    Ready,
    /// The client's tab/pane tree for this session, so the daemon can replay it on reattach and
    /// mirror it to other attached clients. C→S from the focused client whenever the layout
    /// changes; S→C during the attach restore burst (before `Ready`) and live whenever the stored
    /// layout changes. Carries the tree as JSON (`Layout`) — the daemon stores it opaquely.
    LayoutTree { json: String },
    /// S→C: an attention-feed note captured by the remote `potty-session` daemon. C→S: a feed
    /// update from the GUI, currently used to clear a daemon-persisted pending note after the user
    /// dismisses it locally. The payload is a `notify::Note` JSON object; keeping it opaque here
    /// avoids coupling the terminal protocol to the attention-feed schema.
    Notify { json: String },
}

/// A serializable snapshot of the client's tab/pane tree for one session, with daemon pane ids at
/// the leaves. The daemon stores it opaquely and replays it on reattach so the client can rebuild
/// the original splits/tabs rather than one-tab-per-pane.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Layout {
    pub tabs: Vec<LayoutTab>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LayoutTab {
    pub root: LayoutNode,
    /// The focused pane's daemon id, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<PaneId>,
}

/// A node in the layout tree: a pane leaf (by daemon pane id) or a split of two children.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum LayoutNode {
    Leaf {
        pane: PaneId,
    },
    /// `cols` = side-by-side (vertical divider); otherwise stacked. `ratio` is the first child's share.
    Split {
        cols: bool,
        ratio: f32,
        a: Box<LayoutNode>,
        b: Box<LayoutNode>,
    },
}

/// A decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Control(Control),
    /// Raw terminal bytes for a pane. S→C = output, C→S = input.
    Data {
        pane: PaneId,
        bytes: Vec<u8>,
    },
}

impl Frame {
    /// Encode and write one length-prefixed frame, flushing it.
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        let mut payload = Vec::new();
        match self {
            Frame::Control(c) => {
                payload.push(TAG_CONTROL);
                serde_json::to_writer(&mut payload, c).map_err(io::Error::other)?;
            }
            Frame::Data { pane, bytes } => {
                payload.push(TAG_DATA);
                payload.extend_from_slice(&pane.to_le_bytes());
                payload.extend_from_slice(bytes);
            }
        }
        w.write_all(&(payload.len() as u32).to_be_bytes())?;
        w.write_all(&payload)?;
        w.flush()
    }

    /// Read one frame from a blocking stream. `Ok(None)` is a clean EOF at a frame boundary.
    pub fn read(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut len = [0u8; 4];
        if !read_full(r, &mut len)? {
            return Ok(None);
        }
        let len = checked_len(len)?;
        let mut payload = vec![0u8; len];
        if !read_full(r, &mut payload)? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(Some(decode_payload(&payload)?))
    }

    /// Try to parse one frame from the front of `buf` (a growing async-read buffer). Returns the
    /// frame and how many bytes it consumed, or `None` if `buf` doesn't yet hold a whole frame.
    pub fn try_parse(buf: &[u8]) -> io::Result<Option<(Frame, usize)>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let len = checked_len(buf[0..4].try_into().unwrap())?;
        if buf.len() < 4 + len {
            return Ok(None);
        }
        Ok(Some((decode_payload(&buf[4..4 + len])?, 4 + len)))
    }
}

fn checked_len(bytes: [u8; 4]) -> io::Result<usize> {
    let len = u32::from_be_bytes(bytes) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::other(format!("bad frame length {len}")));
    }
    Ok(len)
}

fn decode_payload(payload: &[u8]) -> io::Result<Frame> {
    match payload.first().copied() {
        Some(TAG_CONTROL) => Ok(Frame::Control(
            serde_json::from_slice(&payload[1..]).map_err(io::Error::other)?,
        )),
        Some(TAG_DATA) if payload.len() >= 9 => {
            let pane = u64::from_le_bytes(payload[1..9].try_into().unwrap());
            Ok(Frame::Data {
                pane,
                bytes: payload[9..].to_vec(),
            })
        }
        Some(TAG_DATA) => Err(io::Error::other("short data frame")),
        Some(other) => Err(io::Error::other(format!("unknown frame tag {other}"))),
        None => Err(io::Error::other("empty frame")),
    }
}

/// Fill `buf` completely. `Ok(false)` iff EOF arrived before *any* byte (a clean boundary).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => {
                return if n == 0 {
                    Ok(false)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: &Frame) -> Frame {
        let mut buf = Vec::new();
        f.write(&mut buf).unwrap();
        Frame::read(&mut buf.as_slice()).unwrap().unwrap()
    }

    #[test]
    fn control_roundtrips() {
        let f = Frame::Control(Control::Open {
            pane: 7,
            cols: 80,
            rows: 24,
            cwd_from: Some(3),
        });
        assert_eq!(roundtrip(&f), f);

        let c: Control =
            serde_json::from_str(r#"{"t":"Open","pane":7,"cols":80,"rows":24}"#).unwrap();
        assert_eq!(
            c,
            Control::Open {
                pane: 7,
                cols: 80,
                rows: 24,
                cwd_from: None,
            }
        );

        let f = Frame::Control(Control::Notify {
            json: r#"{"v":1,"session":"abc"}"#.into(),
        });
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn welcome_from_v1_daemon_defaults_client_to_zero() {
        let c: Control = serde_json::from_str(r#"{"t":"Welcome","version":1}"#).unwrap();
        assert_eq!(
            c,
            Control::Welcome {
                version: 1,
                client: 0,
            }
        );
    }

    #[test]
    fn data_roundtrips_raw_bytes() {
        let f = Frame::Data {
            pane: 3,
            bytes: vec![0, 27, 255, b'x', b'\n'],
        };
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn clean_eof_is_none() {
        let mut empty: &[u8] = &[];
        assert!(Frame::read(&mut empty).unwrap().is_none());
    }
}
