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

pub const PROTOCOL_VERSION: u32 = 1;

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
    /// S→C: acknowledge the connection.
    Welcome { version: u32 },
    /// C→S: open a new pane running a shell of the given size.
    Open { pane: PaneId, cols: u16, rows: u16 },
    /// S→C: the pane is live.
    Opened { pane: PaneId },
    /// C→S: resize a pane.
    Resize { pane: PaneId, cols: u16, rows: u16 },
    /// C→S: close a pane (kill its shell).
    Close { pane: PaneId },
    /// S→C: a pane's shell exited.
    Exited { pane: PaneId },
}

/// A decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Control(Control),
    /// Raw terminal bytes for a pane. S→C = output, C→S = input.
    Data { pane: PaneId, bytes: Vec<u8> },
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

    /// Read one frame. `Ok(None)` is a clean EOF at a frame boundary (peer detached).
    pub fn read(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut len = [0u8; 4];
        if !read_full(r, &mut len)? {
            return Ok(None);
        }
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(io::Error::other(format!("bad frame length {len}")));
        }
        let mut payload = vec![0u8; len];
        if !read_full(r, &mut payload)? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        match payload[0] {
            TAG_CONTROL => {
                let c = serde_json::from_slice(&payload[1..]).map_err(io::Error::other)?;
                Ok(Some(Frame::Control(c)))
            }
            TAG_DATA if payload.len() >= 9 => {
                let pane = u64::from_le_bytes(payload[1..9].try_into().unwrap());
                Ok(Some(Frame::Data { pane, bytes: payload[9..].to_vec() }))
            }
            TAG_DATA => Err(io::Error::other("short data frame")),
            other => Err(io::Error::other(format!("unknown frame tag {other}"))),
        }
    }
}

/// Fill `buf` completely. `Ok(false)` iff EOF arrived before *any* byte (a clean boundary).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => return if n == 0 { Ok(false) } else { Err(io::ErrorKind::UnexpectedEof.into()) },
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
        let f = Frame::Control(Control::Open { pane: 7, cols: 80, rows: 24 });
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn data_roundtrips_raw_bytes() {
        let f = Frame::Data { pane: 3, bytes: vec![0, 27, 255, b'x', b'\n'] };
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn clean_eof_is_none() {
        let mut empty: &[u8] = &[];
        assert!(Frame::read(&mut empty).unwrap().is_none());
    }
}
