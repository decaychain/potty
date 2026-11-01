//! Pane sharing: lend a live pane to an agent. Design: `docs/pane-sharing.md`.
//!
//! This module is everything both sides must agree on (share IDs, endpoint names, the JSON wire
//! format) plus the server itself, which is generic over the terminal's event listener so it can
//! be tested without the GUI. The GUI owns a [`Registry`] and decides which panes are shared and
//! in which [`Mode`]; the server only answers requests for live shares.
//!
//! Transport: one JSON request line and one JSON response line per connection, over a local
//! socket (a Unix socket in a private directory, or a named pipe on Windows).

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Term, TermMode, cell::Flags};
use interprocess::local_socket::{ListenerOptions, Stream, prelude::*};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wire version. Bump on an incompatible change.
pub const PROTOCOL: u32 = 1;
/// Prefix of every share ID, so a pasted ID is recognizable in a prompt.
pub const ID_PREFIX: &str = "psh-";
/// Longest request line accepted (a `send` carries the text to type).
const MAX_REQUEST: u64 = 256 * 1024;
/// Longest text a single `send` may type.
pub const MAX_SEND: usize = 64 * 1024;
/// Default and hard limits for returned text, in characters.
pub const DEFAULT_MAX_CHARS: usize = 20_000;
const HARD_MAX_CHARS: usize = 200_000;
/// `wait` limits.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
const POLL: Duration = Duration::from_millis(40);

// ---------------------------------------------------------------------------
// Share IDs and endpoints
// ---------------------------------------------------------------------------

/// What an agent may do through a share. Potty enforces it per request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    ReadOnly,
    CanType,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read-only",
            Mode::CanType => "can type",
        }
    }
}

/// A parsed share ID: `psh-<instance>-<secret>`. The instance names the endpoint to connect to;
/// the secret (128 random bits, hex) is the capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareId {
    pub instance: String,
    pub secret: String,
}

impl ShareId {
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.trim().strip_prefix(ID_PREFIX)?;
        let (instance, secret) = rest.rsplit_once('-')?;
        // The instance becomes part of a socket path or pipe name: digits and dashes only.
        let instance_ok = !instance.is_empty()
            && instance.len() <= 64
            && instance.bytes().all(|b| b.is_ascii_digit() || b == b'-');
        let secret_ok = secret.len() == 32 && secret.bytes().all(|b| b.is_ascii_hexdigit());
        (instance_ok && secret_ok).then(|| Self {
            instance: instance.to_string(),
            secret: secret.to_ascii_lowercase(),
        })
    }

    pub fn new(instance: &str) -> io::Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let secret = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            instance: instance.to_string(),
            secret,
        })
    }
}

impl std::fmt::Display for ShareId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{ID_PREFIX}{}-{}", self.instance, self.secret)
    }
}

/// Private directory holding the Unix share sockets (next to the notify sockets).
#[cfg(unix)]
pub fn socket_dir() -> std::path::PathBuf {
    crate::notify::instance_socket_dir()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join("share")
}

#[cfg(unix)]
fn endpoint(instance: &str) -> io::Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::GenericFilePath;
    // A filesystem socket, not the Linux abstract namespace: abstract sockets ignore file
    // permissions, so any local user could connect.
    socket_dir()
        .join(format!("{instance}.sock"))
        .to_fs_name::<GenericFilePath>()
        .map(|n| n.into_owned())
}

#[cfg(windows)]
fn endpoint(instance: &str) -> io::Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    // A named pipe. The default DACL denies other users write access, and remote clients are
    // rejected by interprocess.
    format!("potty-share-{instance}")
        .to_ns_name::<GenericNamespaced>()
        .map(|n| n.into_owned())
}

#[cfg(unix)]
fn prepare_endpoint_dir() {
    use std::os::unix::fs::PermissionsExt;
    let dir = socket_dir();
    let _ = std::fs::create_dir_all(&dir);
    let private = std::fs::Permissions::from_mode(0o700);
    if let Some(parent) = dir.parent() {
        let _ = std::fs::set_permissions(parent, private.clone());
    }
    let _ = std::fs::set_permissions(&dir, private);
    // Sockets of crashed instances stay behind; drop the ones nobody listens on.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|e| e == "sock")
                && std::os::unix::net::UnixStream::connect(&path).is_err()
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(windows)]
fn prepare_endpoint_dir() {}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    /// The full share ID as given to the agent.
    pub id: String,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Snapshot,
    Read {
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        max_chars: Option<usize>,
    },
    Send {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        keys: Vec<String>,
        #[serde(default)]
        enter: bool,
    },
    Wait {
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        pattern: Option<String>,
        #[serde(default)]
        regex: bool,
        #[serde(default)]
        quiet_ms: Option<u64>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        max_chars: Option<usize>,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    /// Cursor position on the visible screen, zero-based.
    pub cursor_row: usize,
    pub cursor_col: usize,
    /// A full-screen program (vim, less, htop) is using the alternate screen.
    pub alt_screen: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub text: String,
    /// Pass back as `since` to continue reading after this point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The `since` cursor could not be located; `text` is the newest output instead.
    #[serde(default)]
    pub gap: bool,
    /// Older text was cut to respect `max_chars`.
    #[serde(default)]
    pub truncated: bool,
    /// Why a `wait` returned: `pattern`, `quiet`, `timeout`, or `ended`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<Screen>,
}

impl Response {
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            ..Self::default()
        }
    }
}

/// Client side: send one request to the instance named in its share ID.
pub fn request(req: &Request) -> io::Result<Response> {
    let id = ShareId::parse(&req.id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a Potty share ID (expected psh-…)",
        )
    })?;
    let conn = Stream::connect(endpoint(&id.instance)?).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot reach the Potty window that owns this share ({e}); it may be closed"),
        )
    })?;
    let mut conn = BufReader::new(conn);
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    conn.get_mut().write_all(line.as_bytes())?;
    conn.get_mut().flush()?;
    let mut answer = String::new();
    conn.read_line(&mut answer)?;
    serde_json::from_str(&answer).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ---------------------------------------------------------------------------
// Registry (owned by the GUI, read by the server)
// ---------------------------------------------------------------------------

/// What the server reports back to the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Type these bytes into the pane (already checked against the share's mode).
    Input { pane: u64, bytes: Vec<u8> },
    /// The agent looked at the pane.
    Read { pane: u64 },
}

struct Entry<T> {
    secret: String,
    mode: Mode,
    term: Arc<Mutex<Term<T>>>,
    title: String,
}

/// Live shares by pane. The GUI mutates it; server threads read it on every request, so ending a
/// share takes effect immediately, even for a `wait` in progress.
pub struct Registry<T> {
    instance: String,
    shares: Mutex<HashMap<u64, Entry<T>>>,
}

impl<T> Registry<T> {
    pub fn new(instance: &str) -> Self {
        Self {
            instance: instance.to_string(),
            shares: Mutex::new(HashMap::new()),
        }
    }

    /// Share `pane` (or change the mode of an existing share). Returns the share ID.
    pub fn share(&self, pane: u64, mode: Mode, term: Arc<Mutex<Term<T>>>) -> io::Result<String> {
        let mut shares = self.shares.lock().unwrap();
        if let Some(e) = shares.get_mut(&pane) {
            e.mode = mode;
            return Ok(self.id_of(&e.secret));
        }
        let id = ShareId::new(&self.instance)?;
        shares.insert(
            pane,
            Entry {
                secret: id.secret.clone(),
                mode,
                term,
                title: String::new(),
            },
        );
        Ok(id.to_string())
    }

    pub fn unshare(&self, pane: u64) {
        self.shares.lock().unwrap().remove(&pane);
    }

    pub fn set_mode(&self, pane: u64, mode: Mode) {
        if let Some(e) = self.shares.lock().unwrap().get_mut(&pane) {
            e.mode = mode;
        }
    }

    pub fn set_title(&self, pane: u64, title: &str) {
        if let Some(e) = self.shares.lock().unwrap().get_mut(&pane)
            && e.title != title
        {
            e.title = title.to_string();
        }
    }

    /// (share ID, mode) of every live share.
    pub fn list(&self) -> HashMap<u64, (String, Mode)> {
        self.shares
            .lock()
            .unwrap()
            .iter()
            .map(|(pane, e)| (*pane, (self.id_of(&e.secret), e.mode)))
            .collect()
    }

    /// Drop shares whose pane no longer exists.
    pub fn retain(&self, mut alive: impl FnMut(u64) -> bool) {
        self.shares.lock().unwrap().retain(|pane, _| alive(*pane));
    }

    fn id_of(&self, secret: &str) -> String {
        ShareId {
            instance: self.instance.clone(),
            secret: secret.to_string(),
        }
        .to_string()
    }

    /// Look up a live share by secret.
    fn find(&self, secret: &str) -> Option<Found<T>> {
        let shares = self.shares.lock().unwrap();
        shares
            .iter()
            .find(|(_, e)| constant_time_eq(e.secret.as_bytes(), secret.as_bytes()))
            .map(|(pane, e)| (*pane, e.mode, e.term.clone(), e.title.clone()))
    }
}

/// A live share, as one request sees it: (pane, mode, grid, pane title).
type Found<T> = (u64, Mode, Arc<Mutex<Term<T>>>, String);

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Bind this instance's share endpoint and serve requests on background threads.
pub fn serve<T>(
    registry: Arc<Registry<T>>,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
) -> io::Result<()>
where
    T: EventListener + Send + 'static,
{
    prepare_endpoint_dir();
    let name = endpoint(&registry.instance)?;
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    std::thread::Builder::new()
        .name("potty-share".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                let registry = registry.clone();
                let on_event = on_event.clone();
                let _ = std::thread::Builder::new()
                    .name("potty-share-conn".into())
                    .spawn(move || serve_one(conn, &registry, &*on_event));
            }
        })?;
    Ok(())
}

fn serve_one<T: EventListener>(conn: Stream, registry: &Registry<T>, on_event: &dyn Fn(Event)) {
    let mut conn = BufReader::new(conn);
    let mut line = String::new();
    if (&mut conn).take(MAX_REQUEST).read_line(&mut line).is_err() {
        return;
    }
    let response = match serde_json::from_str::<Request>(&line) {
        Ok(req) => handle(&req, registry, on_event),
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    if let Ok(mut out) = serde_json::to_string(&response) {
        out.push('\n');
        let _ = conn.get_mut().write_all(out.as_bytes());
        let _ = conn.get_mut().flush();
    }
}

const ENDED: &str = "this share has ended (the user stopped sharing or closed the pane); ask the user to share the pane again";

/// Answer one request. Public so tests can drive it without a socket.
pub fn handle<T: EventListener>(
    req: &Request,
    registry: &Registry<T>,
    on_event: &dyn Fn(Event),
) -> Response {
    if req.v != PROTOCOL {
        return Response::err(format!(
            "protocol {} not supported (Potty speaks {PROTOCOL}); update potty-notify or Potty",
            req.v
        ));
    }
    let Some(id) = ShareId::parse(&req.id) else {
        return Response::err("not a Potty share ID (expected psh-…)");
    };
    if id.instance != registry.instance {
        return Response::err(ENDED);
    }
    let Some((pane, mode, term, title)) = registry.find(&id.secret) else {
        return Response::err(ENDED);
    };
    let mut resp = match &req.op {
        Op::Snapshot => {
            on_event(Event::Read { pane });
            let t = term.lock().unwrap();
            Response {
                ok: true,
                text: screen_text(&t),
                cursor: Some(Cursor::at_cursor(&t).encode()),
                screen: Some(screen_info(&t)),
                ..Response::default()
            }
        }
        Op::Read { since, max_chars } => {
            on_event(Event::Read { pane });
            let t = term.lock().unwrap();
            let start = since.as_deref().map(Cursor::decode);
            let out = read_since(&t, start.as_ref(), limit(*max_chars));
            out.into_response(screen_info(&t))
        }
        Op::Send { text, keys, enter } => {
            if mode != Mode::CanType {
                return Response::err(
                    "this share is read-only; ask the user to switch the share badge on the pane to 'can type'",
                );
            }
            let (bytes, cursor) = {
                let t = term.lock().unwrap();
                let app_cursor = t.mode().contains(TermMode::APP_CURSOR);
                match encode_input(text.as_deref(), keys, *enter, app_cursor) {
                    Ok(b) => (b, Cursor::at_cursor(&t)),
                    Err(e) => return Response::err(e),
                }
            };
            if !bytes.is_empty() {
                on_event(Event::Input { pane, bytes });
            }
            Response {
                ok: true,
                // Taken before the input lands: `wait` from here sees the command's echo and output.
                cursor: Some(cursor.encode()),
                ..Response::default()
            }
        }
        Op::Wait {
            since,
            pattern,
            regex,
            quiet_ms,
            timeout_ms,
            max_chars,
        } => {
            on_event(Event::Read { pane });
            let matcher = match pattern.as_deref().filter(|p| !p.is_empty()) {
                None => None,
                Some(p) if *regex => match regex::Regex::new(p) {
                    Ok(r) => Some(Matcher::Regex(r)),
                    Err(e) => return Response::err(format!("bad regex: {e}")),
                },
                Some(p) => Some(Matcher::Plain(p.to_string())),
            };
            let timeout =
                Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS));
            wait(
                registry,
                &id.secret,
                &term,
                since.as_deref(),
                matcher.as_ref(),
                quiet_ms.map(Duration::from_millis),
                timeout,
                limit(*max_chars),
            )
        }
    };
    resp.mode = Some(mode);
    if !title.is_empty() {
        resp.title = Some(title);
    }
    resp
}

fn limit(max: Option<usize>) -> usize {
    max.unwrap_or(DEFAULT_MAX_CHARS).clamp(100, HARD_MAX_CHARS)
}

enum Matcher {
    Plain(String),
    Regex(regex::Regex),
}

impl Matcher {
    fn is_match(&self, text: &str) -> bool {
        match self {
            Matcher::Plain(p) => text.contains(p.as_str()),
            Matcher::Regex(r) => r.is_match(text),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn wait<T: EventListener>(
    registry: &Registry<T>,
    secret: &str,
    term: &Arc<Mutex<Term<T>>>,
    since: Option<&str>,
    matcher: Option<&Matcher>,
    quiet: Option<Duration>,
    timeout: Duration,
    max_chars: usize,
) -> Response {
    let started = Instant::now();
    let mut start = match since {
        Some(s) => Cursor::decode(s),
        None => Cursor::at_cursor(&term.lock().unwrap()),
    };
    let mut last_fingerprint = None;
    let mut changed_at = started;
    loop {
        let (out, screen) = {
            let t = term.lock().unwrap();
            let out = read_since(&t, Some(&start), usize::MAX);
            (out, screen_info(&t))
        };
        // Keep following the original position as rows shift.
        if !out.gap {
            start = out.start.clone();
        }
        let fingerprint = (
            fnv(out.text.as_bytes()),
            screen.cursor_row,
            screen.cursor_col,
        );
        if last_fingerprint != Some(fingerprint) {
            last_fingerprint = Some(fingerprint);
            changed_at = Instant::now();
        }
        let reason = if registry.find(secret).is_none() {
            Some("ended")
        } else if matcher.is_some_and(|m| m.is_match(&out.text)) {
            Some("pattern")
        } else if quiet.is_some_and(|q| changed_at.elapsed() >= q) {
            Some("quiet")
        } else if started.elapsed() >= timeout {
            Some("timeout")
        } else {
            None
        };
        if let Some(reason) = reason {
            let mut out = out;
            out.truncate_to(max_chars);
            let mut resp = out.into_response(screen);
            resp.reason = Some(reason.into());
            return resp;
        }
        std::thread::sleep(POLL);
    }
}

// ---------------------------------------------------------------------------
// Grid text and read cursors
// ---------------------------------------------------------------------------

/// One buffer row as text, and whether it soft-wraps into the next row.
fn row_text<T>(term: &Term<T>, abs: usize) -> (String, bool) {
    let grid = term.grid();
    let line = Line(abs as i32 - grid.history_size() as i32);
    let row = &grid[line];
    let cols = grid.columns();
    let mut s = String::with_capacity(cols);
    for c in 0..cols {
        let cell = &row[Column(c)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        s.push(cell.c);
        if let Some(z) = cell.zerowidth() {
            s.extend(z.iter());
        }
    }
    let wrapped = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    if !wrapped {
        s.truncate(s.trim_end().len());
    }
    (s, wrapped)
}

fn total_rows<T>(term: &Term<T>) -> usize {
    term.grid().history_size() + term.grid().screen_lines()
}

fn cursor_abs<T>(term: &Term<T>) -> usize {
    let grid = term.grid();
    (grid.history_size() as i32 + grid.cursor.point.line.0).max(0) as usize
}

fn screen_info<T>(term: &Term<T>) -> Screen {
    let grid = term.grid();
    Screen {
        cols: grid.columns(),
        rows: grid.screen_lines(),
        cursor_row: grid.cursor.point.line.0.max(0) as usize,
        cursor_col: grid.cursor.point.column.0,
        alt_screen: term.mode().contains(TermMode::ALT_SCREEN),
    }
}

/// The visible screen, one line per row, trailing blank rows dropped.
fn screen_text<T>(term: &Term<T>) -> String {
    let history = term.grid().history_size();
    let rows: Vec<String> = (0..term.grid().screen_lines())
        .map(|r| row_text(term, history + r).0)
        .collect();
    let keep = rows
        .iter()
        .rposition(|r| !r.is_empty())
        .map_or(0, |i| i + 1);
    rows[..keep].join("\n")
}

/// FNV-1a: a stable, dependency-free hash for row fingerprints.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

/// How many rows above a cursor its fingerprint covers.
const SIG_ROWS: usize = 3;

/// A position in the buffer: a row, plus a fingerprint of the rows just above it so the position
/// can be found again after old rows fall off a full scrollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    row: usize,
    sig: u64,
}

impl Cursor {
    fn sig_at<T>(term: &Term<T>, row: usize) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        for r in row.saturating_sub(SIG_ROWS)..row.min(total_rows(term)) {
            h = (h ^ fnv(row_text(term, r).0.as_bytes())).wrapping_mul(0x100000001b3);
        }
        h
    }

    fn at<T>(term: &Term<T>, row: usize) -> Self {
        Self {
            row,
            sig: Self::sig_at(term, row),
        }
    }

    fn at_cursor<T>(term: &Term<T>) -> Self {
        Self::at(term, cursor_abs(term))
    }

    pub fn encode(&self) -> String {
        format!("{}.{:x}", self.row, self.sig)
    }

    /// An unparseable cursor decodes to one that can't be found, which reads as a gap.
    pub fn decode(s: &str) -> Self {
        s.split_once('.')
            .and_then(|(r, h)| {
                Some(Self {
                    row: r.parse().ok()?,
                    sig: u64::from_str_radix(h, 16).ok()?,
                })
            })
            .unwrap_or(Self {
                row: usize::MAX,
                sig: 0,
            })
    }

    /// Where this cursor's row is now, if it can be found. Rows only move up (history drops old
    /// rows), so search from the old row upward.
    fn locate<T>(&self, term: &Term<T>) -> Option<usize> {
        let total = total_rows(term);
        if self.row == usize::MAX {
            return None;
        }
        if self.row <= total && Self::sig_at(term, self.row) == self.sig {
            return Some(self.row);
        }
        // A fingerprint of blank rows matches anywhere; don't guess.
        if self.sig == Self::blank_sig(self.row) {
            return None;
        }
        (1..=self.row.min(total))
            .rev()
            .find(|&r| Self::sig_at(term, r) == self.sig)
    }

    fn blank_sig(row: usize) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        for _ in row.saturating_sub(SIG_ROWS)..row {
            h = (h ^ fnv(b"")).wrapping_mul(0x100000001b3);
        }
        h
    }
}

struct ReadOut {
    text: String,
    /// Where the read started (the located `since`, or the gap fallback).
    start: Cursor,
    next: Cursor,
    gap: bool,
    truncated: bool,
}

impl ReadOut {
    fn truncate_to(&mut self, max_chars: usize) {
        let chars = self.text.chars().count();
        if chars <= max_chars {
            return;
        }
        let skip = chars - max_chars;
        let mut cut = self
            .text
            .char_indices()
            .nth(skip)
            .map_or(self.text.len(), |(i, _)| i);
        // Prefer starting on a line boundary.
        if let Some(nl) = self.text[cut..].find('\n') {
            cut += nl + 1;
        }
        self.text.drain(..cut);
        self.truncated = true;
    }

    fn into_response(self, screen: Screen) -> Response {
        Response {
            ok: true,
            text: self.text,
            cursor: Some(self.next.encode()),
            gap: self.gap,
            truncated: self.truncated,
            screen: Some(screen),
            ..Response::default()
        }
    }
}

/// Text from `since` (or the top of the scrollback) through the cursor's row, with wrapped rows
/// joined into logical lines.
fn read_since<T>(term: &Term<T>, since: Option<&Cursor>, max_chars: usize) -> ReadOut {
    let cursor_row = cursor_abs(term);
    let (start, gap) = match since {
        None => (0, false),
        Some(c) => match c.locate(term) {
            Some(r) => (r, false),
            None => (0, true),
        },
    };
    let mut text = String::new();
    let mut continued = false;
    for r in start..=cursor_row.min(total_rows(term).saturating_sub(1)) {
        if r > start && !continued {
            text.push('\n');
        }
        let (row, wrapped) = row_text(term, r);
        text.push_str(&row);
        continued = wrapped;
    }
    let mut out = ReadOut {
        text,
        start: if gap {
            Cursor::at(term, 0)
        } else {
            Cursor::at(term, start)
        },
        next: Cursor::at(term, cursor_row),
        gap,
        truncated: false,
    };
    out.truncate_to(max_chars);
    out
}

// ---------------------------------------------------------------------------
// Input encoding
// ---------------------------------------------------------------------------

/// Bytes for `text` (newlines typed as Enter), an optional Enter, then named `keys`.
pub fn encode_input(
    text: Option<&str>,
    keys: &[String],
    enter: bool,
    app_cursor: bool,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    if let Some(text) = text {
        if text.len() > MAX_SEND {
            return Err(format!("text too long (max {MAX_SEND} bytes per send)"));
        }
        // A terminal's Enter key sends CR; the tty turns it into a newline for the program.
        out.extend(text.replace("\r\n", "\r").replace('\n', "\r").into_bytes());
    }
    if enter {
        out.push(b'\r');
    }
    for key in keys {
        out.extend(encode_key(key, app_cursor).ok_or_else(|| {
            format!("unknown key '{key}' (try Enter, Tab, Escape, Up, C-c, M-b, F5)")
        })?);
    }
    Ok(out)
}

/// One named key, as a terminal would send it. `C-x` is Ctrl, `M-x` is Alt (ESC prefix).
pub fn encode_key(name: &str, app_cursor: bool) -> Option<Vec<u8>> {
    let name = name.trim();
    for prefix in ["M-", "Alt-", "Alt+", "alt+", "alt-", "m-"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && !rest.is_empty()
        {
            let mut v = vec![0x1b];
            v.extend(encode_key(rest, app_cursor)?);
            return Some(v);
        }
    }
    for prefix in ["C-", "Ctrl-", "Ctrl+", "ctrl+", "ctrl-", "c-"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let mut chars = rest.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return match rest.to_ascii_lowercase().as_str() {
                    "space" => Some(vec![0]),
                    _ => None,
                };
            };
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 & 0x1f,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '/' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            return Some(vec![b]);
        }
    }
    let ss3_or_csi = |c: u8| {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let tilde = |n: &str| format!("\x1b[{n}~").into_bytes();
    let bytes = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" | "cr" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "backtab" | "s-tab" | "shift-tab" | "shift+tab" => b"\x1b[Z".to_vec(),
        "escape" | "esc" => vec![0x1b],
        "backspace" | "bs" => vec![0x7f],
        "space" => vec![b' '],
        "up" => ss3_or_csi(b'A'),
        "down" => ss3_or_csi(b'B'),
        "right" => ss3_or_csi(b'C'),
        "left" => ss3_or_csi(b'D'),
        "home" => ss3_or_csi(b'H'),
        "end" => ss3_or_csi(b'F'),
        "insert" | "ins" => tilde("2"),
        "delete" | "del" => tilde("3"),
        "pageup" | "pgup" => tilde("5"),
        "pagedown" | "pgdn" => tilde("6"),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => tilde("15"),
        "f6" => tilde("17"),
        "f7" => tilde("18"),
        "f8" => tilde("19"),
        "f9" => tilde("20"),
        "f10" => tilde("21"),
        "f11" => tilde("23"),
        "f12" => tilde("24"),
        _ => {
            // A single character stands for itself (useful after M-).
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c.to_string().into_bytes(),
                _ => return None,
            }
        }
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, test::TermSize};
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn term(cols: usize, rows: usize, history: usize) -> Term<VoidListener> {
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        Term::new(config, &TermSize::new(cols, rows), VoidListener)
    }

    fn feed(t: &mut Term<VoidListener>, s: &str) {
        let mut p: Processor<StdSyncHandler> = Processor::new();
        p.advance(t, s.as_bytes());
    }

    #[test]
    fn share_ids_round_trip_and_reject_paths() {
        let id = ShareId::new("123-456").unwrap();
        let s = id.to_string();
        assert!(s.starts_with("psh-123-456-"));
        assert_eq!(ShareId::parse(&s), Some(id));
        assert_eq!(
            ShareId::parse(&format!("  {s}\n")).unwrap().instance,
            "123-456"
        );
        assert!(ShareId::parse("psh-../x-0123456789abcdef0123456789abcdef").is_none());
        assert!(ShareId::parse("psh-1-2-short").is_none());
        assert!(ShareId::parse("1-2-0123456789abcdef0123456789abcdef").is_none());
    }

    #[test]
    fn reads_join_wrapped_rows_and_follow_a_cursor() {
        let mut t = term(10, 4, 100);
        feed(&mut t, "$ echo 0123456789abc\r\n0123456789abc\r\n$ ");
        let out = read_since(&t, None, usize::MAX);
        assert_eq!(out.text, "$ echo 0123456789abc\n0123456789abc\n$");
        let mark = out.next.clone();
        feed(&mut t, "ls\r\nfile\r\n$ ");
        let out = read_since(&t, Some(&mark), usize::MAX);
        assert!(!out.gap);
        assert_eq!(out.text, "$ ls\nfile\n$");
    }

    #[test]
    fn cursor_survives_a_full_scrollback() {
        let mut t = term(20, 5, 10);
        for i in 0..30 {
            feed(&mut t, &format!("line {i}\r\n"));
        }
        let mark = Cursor::at_cursor(&t);
        for i in 30..34 {
            feed(&mut t, &format!("line {i}\r\n"));
        }
        let out = read_since(&t, Some(&mark), usize::MAX);
        assert!(!out.gap, "fingerprint should relocate the cursor");
        assert_eq!(out.text, "line 30\nline 31\nline 32\nline 33\n");
        // Far more output than the history holds: the mark is gone.
        for i in 34..80 {
            feed(&mut t, &format!("line {i}\r\n"));
        }
        let out = read_since(&t, Some(&mark), usize::MAX);
        assert!(out.gap);
        assert!(out.text.contains("line 79"));
    }

    #[test]
    fn truncation_keeps_the_newest_lines() {
        let mut t = term(20, 5, 100);
        for i in 0..50 {
            feed(&mut t, &format!("line {i}\r\n"));
        }
        let out = read_since(&t, None, 30);
        assert!(out.truncated);
        assert!(out.text.ends_with("line 49\n"));
        assert!(out.text.chars().count() <= 30);
    }

    #[test]
    fn keys_encode_like_a_terminal() {
        assert_eq!(encode_key("C-c", false).unwrap(), vec![3]);
        assert_eq!(encode_key("Up", false).unwrap(), b"\x1b[A");
        assert_eq!(encode_key("Up", true).unwrap(), b"\x1bOA");
        assert_eq!(encode_key("M-b", false).unwrap(), b"\x1bb");
        assert_eq!(encode_key("F5", false).unwrap(), b"\x1b[15~");
        assert!(encode_key("Hyper-x", false).is_none());
        assert_eq!(
            encode_input(Some("ls\nl"), &["Enter".into()], false, false).unwrap(),
            b"ls\rl\r"
        );
    }

    #[test]
    fn modes_are_enforced_and_unshare_is_immediate() {
        let registry = Registry::new("1-2");
        let t = Arc::new(Mutex::new(term(20, 5, 100)));
        let id = registry.share(7, Mode::ReadOnly, t.clone()).unwrap();
        let sent = Mutex::new(Vec::new());
        let on_event = |e: Event| sent.lock().unwrap().push(e);
        let send = Request {
            v: PROTOCOL,
            id: id.clone(),
            op: Op::Send {
                text: Some("ls".into()),
                keys: vec![],
                enter: true,
            },
        };
        let r = handle(&send, &registry, &on_event);
        assert!(!r.ok && r.error.unwrap().contains("read-only"));
        registry.set_mode(7, Mode::CanType);
        assert!(handle(&send, &registry, &on_event).ok);
        assert!(sent.lock().unwrap().contains(&Event::Input {
            pane: 7,
            bytes: b"ls\r".to_vec()
        }));
        // Re-sharing keeps the ID; a wrong secret is refused.
        assert_eq!(registry.share(7, Mode::CanType, t).unwrap(), id);
        let forged = Request {
            v: PROTOCOL,
            id: format!("psh-1-2-{}", "0".repeat(32)),
            op: Op::Snapshot,
        };
        assert!(!handle(&forged, &registry, &on_event).ok);
        registry.unshare(7);
        assert!(!handle(&send, &registry, &on_event).ok);
    }

    #[test]
    fn wait_returns_on_pattern_quiet_and_end() {
        let registry = Arc::new(Registry::new("1-2"));
        let t = Arc::new(Mutex::new(term(40, 5, 100)));
        let id = registry.share(1, Mode::CanType, t.clone()).unwrap();
        let mark = Cursor::at_cursor(&t.lock().unwrap()).encode();
        let writer = {
            let t = t.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                feed(
                    &mut t.lock().unwrap(),
                    "building...\r\n__POTTY_DONE_0__\r\n$ ",
                );
            })
        };
        let wait = |since: Option<String>, pattern: Option<&str>, quiet: Option<u64>| Request {
            v: PROTOCOL,
            id: id.clone(),
            op: Op::Wait {
                since,
                pattern: pattern.map(String::from),
                regex: true,
                quiet_ms: quiet,
                timeout_ms: Some(5_000),
                max_chars: None,
            },
        };
        let r = handle(
            &wait(Some(mark), Some(r"__POTTY_DONE_\d+__"), None),
            &registry,
            &|_| {},
        );
        writer.join().unwrap();
        assert_eq!(r.reason.as_deref(), Some("pattern"));
        assert!(r.text.contains("building..."));
        let r = handle(&wait(None, None, Some(100)), &registry, &|_| {});
        assert_eq!(r.reason.as_deref(), Some("quiet"));
        let ender = {
            let registry = registry.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                registry.unshare(1);
            })
        };
        let r = handle(&wait(None, Some("never"), None), &registry, &|_| {});
        ender.join().unwrap();
        assert_eq!(r.reason.as_deref(), Some("ended"));
    }
}
