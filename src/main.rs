//! potty — GPU terminal spike with a visual menu and a real per-cell renderer.
//!
//!   winit 0.30 (Wayland/KWin) → wgpu 29 surface
//!     ├─ gridr : per-cell terminal renderer (atlas + instanced bg/fg quads)  [pass 1]
//!     └─ egui  : tab bar + pane menu                                          [pass 2]
//!   portable-pty → vte parser → alacritty_terminal grid
//!
//! Real multiplexing: every leaf pane owns its own PTY + Term (App::terms, keyed by
//! PaneId). The active tab's panes each render into their rect (one render submit per
//! pane, scissored); background tabs keep running. Keyboard goes to the focused pane;
//! the mouse acts on the pane under the cursor.

// On Windows, use the GUI subsystem so launching doesn't spawn a console window alongside
// our own window. (No effect on other platforms.)
#![cfg_attr(windows, windows_subsystem = "windows")]

mod clip;
mod config;
mod gridr;
mod workspace;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use config::{Config, ConnectionProfile};
use notify::Watcher;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config as TermConfig, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

use wgpu::{
    CommandEncoderDescriptor, CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor,
    LoadOp, Operations, PresentMode, RenderPassColorAttachment, RenderPassDescriptor,
    RequestAdapterOptions, SurfaceConfiguration, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{
    ElementState, Ime, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{UserAttentionType, Window};

use gridr::GridRenderer;
use workspace::{GAP, PaneId, Split, Workspace};
// The attention-feed wire contract lives in the lib crate (shared with the `potty-notify` bin).
// Aliased to avoid clashing with the `notify` file-watcher crate used below.
use potty::notify as feed;
use potty::proto::{self, Control, Frame};
use potty::remote;

const FONT_PX: f32 = 15.0;
const LINE_PX: f32 = 18.0;
/// Top-bar height reserve (logical px) for the initial PTY sizing.
const TOPBAR: f32 = 34.0;
/// Cursor blink half-period (time the cursor stays solid, then hidden). ~1.2 Hz, the classic feel.
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// 64×64 RGBA window icon, embedded raw (no PNG decoder needed). Used by winit's
/// `set_window_icon` — drives the title bar / taskbar / alt-tab on Windows (a no-op on Wayland,
/// where the icon comes from the `.desktop` file matched by `app_id`).
const WINDOW_ICON_RGBA: &[u8] = include_bytes!("../assets/icon-64.rgba");
/// Wayland app_id (and the basename of the installed `.desktop` / icon on Linux).
#[cfg(target_os = "linux")]
const APP_ID: &str = "io.github.decaychain.potty";

type SharedTerm = Arc<Mutex<Term<EventProxy>>>;

/// Events the terminal raises (from a PTY reader thread) that the main loop must service.
/// Variants that write back to a PTY carry the originating `PaneId`, since there is now a
/// terminal per pane.
enum UserEvent {
    /// A pane's reader advanced its grid — mark it dirty (and redraw if it's visible).
    Wake(PaneId),
    ReloadConfig,
    /// OSC 52 store (app writes the system clipboard). Targets the clipboard selection.
    ClipboardStore(String),
    /// OSC 52 load: read the clipboard, run the formatter, write the result back to the pane.
    ClipboardLoad(PaneId, std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>),
    /// Terminal query response (cursor position, device attributes, …) bound for the pane.
    PtyWrite(PaneId, String),
    /// The pane's program set its window title (OSC 0/2).
    Title(PaneId, String),
    /// The pane's program reset its title to the default.
    ResetTitle(PaneId),
    /// A pane's shell exited (reader hit EOF) — close the pane.
    PaneExited(PaneId),
    /// An attention note arrived from this instance's own helper/socket path.
    Notify(feed::Note),
    /// A note forwarded by another live potty instance. Already fanned out; don't rebroadcast it.
    PeerNotify(feed::Note),
    /// Another instance's feed asked this process to activate one of its notes.
    PeerFocus {
        instance: Option<String>,
        host: String,
        session: String,
        pane: Option<PaneId>,
    },
    /// Another instance dismissed a feed row; remove the mirrored copy here too.
    PeerDismiss {
        instance: Option<String>,
        host: String,
        session: String,
    },
    /// A remote SSH session authenticated and exec'd `potty-session`; carries the connection id,
    /// the target (for matching/labels), and the channel to send it input/resize/close frames.
    RemoteConnected {
        conn: ConnId,
        target: RemoteTarget,
        display_name: Option<String>,
        outbound: tokio::sync::mpsc::Sender<potty::proto::Frame>,
        /// True for a plain SSH shell (no daemon) — it can't be detached/reattached.
        ephemeral: bool,
    },
    /// A wire-protocol frame arrived from a remote session (tagged with its connection).
    RemoteFrame(ConnId, potty::proto::Frame),
    /// A remote connection attempt failed (auth, network, host key, …).
    /// `conn` is set once a visible attempt was allocated; the connection may still not be
    /// registered yet, but the progress row can be cleared.
    RemoteError {
        conn: Option<ConnId>,
        msg: String,
    },
    /// A previously-greeted remote stream ended while its connection is still registered.
    RemoteDisconnected {
        conn: ConnId,
        msg: String,
    },
    /// A remote connection's auth ladder needs the user (host-key approval, …).
    Auth(AuthPrompt),
}

/// Terminal event listener for one pane: forwards the events we care about to the main loop,
/// tagging the writes with the pane id so they reach the right PTY. (PTY reader thread → here
/// → `user_event`.) Replaces VoidListener, which dropped everything.
#[derive(Clone)]
struct EventProxy {
    proxy: EventLoopProxy<UserEvent>,
    pane: PaneId,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        // `ty` (clipboard vs primary) is ignored — OSC 52 in practice targets the clipboard.
        let forward = match event {
            Event::ClipboardStore(_ty, text) => Some(UserEvent::ClipboardStore(text)),
            Event::ClipboardLoad(_ty, fmt) => Some(UserEvent::ClipboardLoad(self.pane, fmt)),
            Event::PtyWrite(text) => Some(UserEvent::PtyWrite(self.pane, text)),
            Event::Title(title) => Some(UserEvent::Title(self.pane, title)),
            Event::ResetTitle => Some(UserEvent::ResetTitle(self.pane)),
            _ => None,
        };
        if let Some(ev) = forward {
            let _ = self.proxy.send_event(ev);
        }
    }
}

/// Build alacritty's terminal Config from ours (currently just the OSC 52 policy).
fn term_config(cfg: &Config) -> TermConfig {
    TermConfig {
        osc52: match cfg.osc52.as_str() {
            "disabled" => Osc52::Disabled,
            "paste" => Osc52::OnlyPaste,
            "copy-paste" => Osc52::CopyPaste,
            _ => Osc52::OnlyCopy,
        },
        default_cursor_style: alacritty_terminal::vte::ansi::CursorStyle {
            shape: cfg.cursor_shape(),
            blinking: cfg.cursor_blink,
        },
        ..TermConfig::default()
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The shell to spawn: the configured one, else the platform default ($SHELL on unix, %COMSPEC%
/// — i.e. cmd.exe — on Windows).
fn default_shell(cfg: &Config) -> String {
    if let Some(s) = &cfg.shell {
        return s.clone();
    }
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    cwd.is_dir().then_some(cwd)
}

#[cfg(not(target_os = "linux"))]
fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

fn valid_cwd(path: &Path) -> bool {
    path.is_absolute() && path.is_dir()
}

fn pty_cwd(
    master: &(dyn portable_pty::MasterPty + Send),
    child_pid: Option<u32>,
) -> Option<PathBuf> {
    #[cfg(unix)]
    if let Some(cwd) = master
        .process_group_leader()
        .and_then(|pid| u32::try_from(pid).ok())
        .and_then(process_cwd)
    {
        return Some(cwd);
    }

    child_pid.and_then(process_cwd)
}

fn dims_for(width_px: f32, height_px: f32, cell_w: f32, cell_h: f32) -> Dims {
    Dims {
        cols: ((width_px / cell_w).floor() as usize).max(1),
        rows: ((height_px / cell_h).floor() as usize).max(1),
    }
}

/// One live terminal: its grid model + the PTY master (for writes and resize) and current size.
/// The reader thread that feeds the grid is detached; it ends when the PTY closes.
/// A request from a remote connection's auth ladder that needs the user, carrying a reply channel
/// back to the (blocked) connection thread.
enum AuthPrompt {
    HostKey {
        host: String,
        fingerprint: String,
        status: remote::HostKeyStatus,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// One or more text fields (passphrase, password, keyboard-interactive). `echo` is false for
    /// secrets. The reply is one answer per field, or `None` if cancelled.
    Text {
        title: String,
        fields: Vec<(String, bool)>,
        reply: std::sync::mpsc::Sender<Option<Vec<String>>>,
    },
}

/// `Authenticator` that bridges a connection thread's prompts to the UI thread: it sends an
/// `AuthPrompt` over the event loop and *blocks the connection thread* on a reply channel until the
/// user answers. Safe because each connection runs on its own thread (see `connect_remote`).
struct GuiAuth {
    proxy: EventLoopProxy<UserEvent>,
    /// True while a prompt sent to the UI is awaiting its answer — pauses the connect timeout
    /// (see `Authenticator::prompt_in_flight`) so a dialog left open doesn't abort the connect.
    prompting: std::sync::atomic::AtomicBool,
}

impl remote::Authenticator for GuiAuth {
    fn accept_host_key(
        &self,
        host: &str,
        fingerprint: &str,
        status: remote::HostKeyStatus,
    ) -> bool {
        let (reply, rx) = std::sync::mpsc::channel();
        let ask = AuthPrompt::HostKey {
            host: host.into(),
            fingerprint: fingerprint.into(),
            status,
            reply,
        };
        if self.proxy.send_event(UserEvent::Auth(ask)).is_err() {
            return false;
        }
        self.prompting
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let answer = rx.recv().unwrap_or(false);
        self.prompting
            .store(false, std::sync::atomic::Ordering::Relaxed);
        answer
    }

    fn key_passphrase(&self, path: &str) -> Option<String> {
        self.text_prompt(
            format!("Passphrase for {path}"),
            vec![("Passphrase".into(), false)],
        )
        .map(|mut v| v.pop().unwrap_or_default())
    }

    fn password(&self, user: &str, host: &str) -> Option<String> {
        self.text_prompt(
            format!("Password for {user}@{host}"),
            vec![("Password".into(), false)],
        )
        .map(|mut v| v.pop().unwrap_or_default())
    }

    fn answer(
        &self,
        name: &str,
        instructions: &str,
        prompts: &[remote::PromptInfo],
    ) -> Option<Vec<String>> {
        let title = [name, instructions]
            .iter()
            .find(|s| !s.is_empty())
            .map_or("Authentication", |s| s);
        let fields = prompts.iter().map(|p| (p.prompt.clone(), p.echo)).collect();
        self.text_prompt(title.to_string(), fields)
    }

    fn prompt_in_flight(&self) -> bool {
        self.prompting.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl GuiAuth {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            prompting: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Send a text prompt to the UI and block the connection thread until the user answers.
    fn text_prompt(&self, title: String, fields: Vec<(String, bool)>) -> Option<Vec<String>> {
        let (reply, rx) = std::sync::mpsc::channel();
        let ask = AuthPrompt::Text {
            title,
            fields,
            reply,
        };
        if self.proxy.send_event(UserEvent::Auth(ask)).is_err() {
            return None;
        }
        self.prompting
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let answer = rx.recv().unwrap_or(None);
        self.prompting
            .store(false, std::sync::atomic::Ordering::Relaxed);
        answer
    }
}

/// Identifies one SSH connection (a russh client thread). Frames are tagged with it so the client
/// can route by `(conn, remote_id)` — the daemon's pane ids aren't unique across connections.
type ConnId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteTarget {
    user: String,
    host: String,
    port: u16,
    command: String,
}

impl RemoteTarget {
    fn label(&self) -> String {
        let user = if self.user.is_empty() {
            String::new()
        } else {
            format!("{}@", self.user)
        };
        if self.port == 22 {
            format!("{user}{}", self.host)
        } else {
            format!("{user}{}:{}", self.host, self.port)
        }
    }
}

/// Per-connection client state: how to talk to it, the next daemon pane id to request, and the
/// map from the daemon's pane ids to local `PaneId`s (they diverge after a reattach).
struct Connection {
    target: RemoteTarget,
    display_name: Option<String>,
    outbound: tokio::sync::mpsc::Sender<potty::proto::Frame>,
    next_remote_id: u64,
    routes: HashMap<u64, PaneId>,
    /// A plain SSH shell (no daemon): can't be detached/reattached, and pushes no layout.
    ephemeral: bool,
    /// True once the attach handshake finished (`Ready` seen). We only push layout updates after
    /// this, so a mid-handshake push can't clobber the daemon's stored layout we're restoring from.
    ready: bool,
    /// Our daemon-assigned client id (`Welcome`), and the current focus owner (`Focus`). Both
    /// start at 0, so against a v1 daemon (no id, no `Focus`) we correctly act as the sole
    /// controller. While `client_id != focus_owner` another attached potty is driving: we mirror
    /// its layout and pane sizes instead of pushing our own, until our next input flips focus.
    client_id: u64,
    focus_owner: u64,
    /// Panes adopted live (another client opened them) but not yet placed into a tab — the
    /// owner's next `LayoutTree` places them. Kept out of `reconcile_terms`' reaping until then.
    adopting: std::collections::HashSet<PaneId>,
    /// Panes adopted during the restore burst (remote_id, local), placed into tabs at `Ready`.
    restore_panes: Vec<(u64, PaneId)>,
    /// The daemon's replayed layout, applied at `Ready`.
    restore_layout: Option<proto::Layout>,
    /// Last layout JSON pushed to the daemon, to avoid redundant sends.
    pushed_layout: Option<String>,
    /// True once this connection has been written to `profiles` after a successful handshake.
    remembered: bool,
}

/// A pane's I/O backend: a local PTY, or a pane on a remote `potty-session` reached over the wire
/// protocol. The renderer and the `Term` grid don't care which — only input and resize differ.
enum Backend {
    Local {
        writer: Box<dyn Write + Send>,
        master: Box<dyn portable_pty::MasterPty + Send>,
        child_pid: Option<u32>,
    },
    Remote {
        /// Which connection this pane belongs to (frames are tagged with it for routing).
        conn: ConnId,
        /// The pane's id in the *daemon's* namespace — used on the wire. Differs from the local
        /// `PaneId` after a reattach, since the daemon owns ids across reconnects.
        remote_id: u64,
        /// Profile display name or host — prefixed onto the tab label so remote tabs are
        /// distinguishable even after the remote shell sets its own title.
        label: String,
        /// Input/resize/close frames go here (to the russh writer task).
        outbound: tokio::sync::mpsc::Sender<potty::proto::Frame>,
        /// Per-pane vte parse state — for a local pane this lives in its reader thread; a remote
        /// pane is fed from the main loop, so it owns its parser here. Boxed: the parser dwarfs
        /// every other field, and `Backend` lives inside each `Terminal`.
        parser: Box<Processor<StdSyncHandler>>,
    },
}

struct Terminal {
    term: SharedTerm,
    backend: Backend,
    dims: Dims,
    /// Current title (OSC 0/2), shown in the tab label and (when focused) the window title.
    title: String,
    /// What `title` resets to (the shell's basename) on OSC ResetTitle.
    default_title: String,
    /// Coalesces wake-ups: the reader sets it and only sends a `Wake` when it was unset, so a
    /// flood of PTY reads can't spam the main loop with one event each. Cleared when handled.
    wake_pending: Arc<AtomicBool>,
}

enum Action {
    SelectTab(usize),
    NewTab,
    /// New tab that is always a local shell, regardless of the focused pane (the tab bar's "+").
    NewLocalTab,
    Split(Split),
    ClosePane,
    CloseTab(usize),
    Focus(PaneId),
    SetRatio(u64, f32),
    SetFontFamily(Option<String>),
    SetFontSize(f32),
    /// Replace the whole color scheme (a Settings preset, or the defaults).
    SetColors(config::Colors),
    ShowSettings,
    CaptureShortcut(MenuAction),
    ResetShortcut(MenuAction),
    /// Activate an attention-feed entry: focus its pane/window if possible, otherwise clear it.
    ActivateNote(FeedKey, Option<PaneId>),
    /// Dismiss an attention-feed entry.
    DismissNote(FeedKey),
    /// Show or hide the attention-feed overlay (bell toggle / overlay close button).
    ShowFeed(bool),
    /// Dismiss the feed entirely: un-latch the tab bar (so it disappears again with a lone tab).
    /// Triggered by clicking the bell when nothing is waiting.
    DismissFeed,
    /// Answer the current SSH host-key prompt (accept/reject).
    AuthAnswer(bool),
    /// Submit (true) or cancel (false) the current text auth prompt.
    AuthText(bool),
    /// Open / close the "Connect to host…" dialog.
    OpenConnect,
    CloseConnect,
    /// Connect to the given `[user@]host[:port]`.
    Connect(String, String),
    /// Fill the connect dialog from a saved profile/recent by index in `Config::profiles`.
    UseProfile(usize),
    /// Dismiss the connection-error dialog.
    DismissError,
    /// Detach the focused pane's remote session: drop its local tabs/panes and disconnect, but
    /// leave the daemon's shells running so it can be reattached later.
    DetachSession,
}

/// Display data for the active auth prompt, handed to the chrome (the reply channel stays in
/// `App::auth_prompts`; text answers come back via `App::auth_inputs`).
enum AuthView {
    HostKey {
        host: String,
        fingerprint: String,
        status: remote::HostKeyStatus,
    },
    Text {
        title: String,
        fields: Vec<(String, bool)>,
    },
}

/// Internal attention-feed key. `instance` matters because pane ids and synthesized fallback
/// session ids are process-local.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FeedKey {
    instance: Option<String>,
    host: String,
    session: String,
}

impl FeedKey {
    fn from_note(note: &feed::Note) -> Self {
        Self {
            instance: note.instance.clone(),
            host: note.host.clone(),
            session: note.session.clone(),
        }
    }
}

/// A session currently waiting for the user, as tracked by the attention feed. Keyed in `App`
/// by `FeedKey`; a `raise` note inserts/refreshes it, a `clear` (or the user landing on its pane)
/// removes it.
struct Pending {
    tool: feed::Tool,
    message: String,
    host: String,
    cwd: String,
    /// The owning potty pane, for exact jump-to-focus (local sessions only).
    pane: Option<PaneId>,
    zellij: Option<feed::ZellijLoc>,
    /// When it was last raised — drives newest-first ordering and the age label.
    since: Instant,
}

/// A flattened, display-ready attention-feed row handed to the chrome.
struct FeedItem {
    key: FeedKey,
    tool: feed::Tool,
    /// Pre-formatted "where": host (if remote) + cwd basename + Zellij hint.
    label: String,
    message: String,
    /// Pre-formatted age, e.g. "12s", "4m", "2h".
    age: String,
    pane: Option<PaneId>,
    can_activate: bool,
}

struct ConnectProfileView {
    index: usize,
    label: String,
    detail: String,
    use_potty_session: bool,
}

struct ConnectProgress {
    target: RemoteTarget,
    label: String,
    detail: String,
    started: Instant,
}

struct ConnectProgressView {
    label: String,
    detail: String,
    elapsed: Duration,
}

/// Apply a workspace (tab/pane) action. Font actions are routed separately (they touch App).
fn apply(ws: &mut Workspace, action: Action) {
    match action {
        Action::SelectTab(i) => ws.active = i.min(ws.tabs.len() - 1),
        Action::NewTab | Action::NewLocalTab => ws.new_tab(),
        Action::Split(s) => ws.split(s),
        Action::ClosePane => ws.close_focused(),
        Action::CloseTab(i) => ws.close_tab(i),
        Action::Focus(id) => ws.focus(id),
        Action::SetRatio(id, ratio) => ws.set_ratio(id, ratio),
        // Handled in the redraw action loop (they touch App, not just the workspace).
        Action::SetFontFamily(_)
        | Action::SetFontSize(_)
        | Action::SetColors(_)
        | Action::ShowSettings
        | Action::CaptureShortcut(_)
        | Action::ResetShortcut(_)
        | Action::ActivateNote(..)
        | Action::DismissNote(_)
        | Action::ShowFeed(_)
        | Action::DismissFeed
        | Action::AuthAnswer(_)
        | Action::AuthText(_)
        | Action::OpenConnect
        | Action::CloseConnect
        | Action::Connect(..)
        | Action::UseProfile(_)
        | Action::DismissError
        | Action::DetachSession => {}
    }
}

/// The xterm modifier parameter for a key event: `1 + Shift(1) + Alt(2) + Ctrl(4)`. `1` means no
/// modifiers; anything `> 1` selects the modified key-encoding form.
fn xterm_modifier(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl)
}

fn macos_control_click(button: MouseButton, control: bool) -> bool {
    cfg!(target_os = "macos") && matches!(button, MouseButton::Left) && control
}

/// macOS pane context clicks use Potty's deferred popup path. They must be withheld from egui's
/// native context-menu handling, otherwise one physical right-click opens the same popup through
/// both paths in the same frame.
fn macos_pane_context_click(button: MouseButton, control: bool) -> bool {
    cfg!(target_os = "macos")
        && (matches!(button, MouseButton::Right) || macos_control_click(button, control))
}

fn terminal_mouse_claims_context_click(button: MouseButton, report: bool, shift: bool) -> bool {
    matches!(button, MouseButton::Right) && report && !shift
}

/// Encode a cursor key (`final_byte` = `A`/`B`/`C`/`D`/`H`/`F`). With a modifier held, xterm always
/// uses the CSI form `ESC [ 1 ; <mod> <final>` regardless of DECCKM; unmodified it honours
/// application-cursor mode (`ESC O <final>` vs `ESC [ <final>`).
fn cursor_key(final_byte: u8, modifier: u8, app_cursor: bool) -> Vec<u8> {
    if modifier > 1 {
        format!("\x1b[1;{modifier}{}", final_byte as char).into_bytes()
    } else if app_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// Encode a CSI-tilde editing key (`ESC [ <code> ~`, or `ESC [ <code> ; <mod> ~` when modified) —
/// Insert/Delete/PageUp/PageDown.
fn csi_tilde(code: u8, modifier: u8) -> Vec<u8> {
    if modifier > 1 {
        format!("\x1b[{code};{modifier}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

// ---------------------------------------------------------------------------
// egui chrome
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    NewTab,
    SplitRight,
    SplitDown,
    ClosePane,
    Connect,
    DetachSession,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    Font,
    Colors,
    Hotkeys,
}

#[derive(Clone, PartialEq, Eq)]
struct ShortcutBinding {
    key: String,
    shift: bool,
}

struct ShortcutDef {
    action: MenuAction,
    label: &'static str,
    default_key: &'static str,
    default_shift: bool,
}

// On macOS the modifier is Cmd, which the shell doesn't see, so the defaults follow iTerm2
// (Cmd+D / Cmd+Shift+D splits, Cmd+W close). Everywhere else the modifier is Ctrl, and every
// plain Ctrl+letter shadows a control character the shell owns (Ctrl+D = EOF, Ctrl+W =
// delete-word, Ctrl+K = kill-line, Ctrl+S = XOFF), so — like GNOME Terminal, Konsole and
// Kitty — every letter shortcut takes Shift.
const MAC: bool = cfg!(target_os = "macos");

const SHORTCUTS: &[ShortcutDef] = &[
    ShortcutDef {
        action: MenuAction::NewTab,
        label: "New tab",
        default_key: "T",
        default_shift: !MAC,
    },
    ShortcutDef {
        action: MenuAction::SplitRight,
        label: "Split right",
        default_key: if MAC { "D" } else { "R" },
        default_shift: !MAC,
    },
    ShortcutDef {
        action: MenuAction::SplitDown,
        label: "Split down",
        default_key: "D",
        default_shift: true,
    },
    ShortcutDef {
        action: MenuAction::ClosePane,
        label: "Close pane",
        default_key: "W",
        default_shift: !MAC,
    },
    ShortcutDef {
        action: MenuAction::Connect,
        label: "Connect to host",
        default_key: "K",
        default_shift: !MAC,
    },
    ShortcutDef {
        action: MenuAction::DetachSession,
        label: "Detach session",
        // Cmd+Shift+W on macOS; X elsewhere because Ctrl+Shift+W is taken by Close pane.
        default_key: if MAC { "W" } else { "X" },
        default_shift: true,
    },
    ShortcutDef {
        action: MenuAction::Settings,
        label: "Settings",
        default_key: ",",
        default_shift: false,
    },
];

fn modkey_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    }
}

fn shortcut_for(action: MenuAction) -> &'static ShortcutDef {
    SHORTCUTS
        .iter()
        .find(|s| s.action == action)
        .expect("menu action has a shortcut")
}

fn action_id(action: MenuAction) -> &'static str {
    match action {
        MenuAction::NewTab => "new_tab",
        MenuAction::SplitRight => "split_right",
        MenuAction::SplitDown => "split_down",
        MenuAction::ClosePane => "close_pane",
        MenuAction::Connect => "connect",
        MenuAction::DetachSession => "detach_session",
        MenuAction::Settings => "settings",
    }
}

fn default_binding(action: MenuAction) -> ShortcutBinding {
    let shortcut = shortcut_for(action);
    ShortcutBinding {
        key: shortcut.default_key.to_string(),
        shift: shortcut.default_shift,
    }
}

fn parse_binding(s: &str) -> Option<ShortcutBinding> {
    let mut has_mod = false;
    let mut shift = false;
    let mut key = None;
    for part in s.split('+') {
        let part = part.trim();
        match part.to_ascii_lowercase().as_str() {
            "mod" | "cmd" | "command" | "ctrl" | "control" => has_mod = true,
            "shift" => shift = true,
            "" => {}
            _ => key = Some(part.to_ascii_uppercase()),
        }
    }
    let key = key?;
    (has_mod && !key.is_empty()).then_some(ShortcutBinding { key, shift })
}

fn binding_from_config(cfg: &Config, action: MenuAction) -> ShortcutBinding {
    cfg.hotkeys
        .get(action_id(action))
        .and_then(|s| parse_binding(s))
        .unwrap_or_else(|| default_binding(action))
}

fn shortcut_hint(binding: &ShortcutBinding) -> String {
    if binding.shift {
        format!("{}+Shift+{}", modkey_name(), binding.key)
    } else {
        format!("{}+{}", modkey_name(), binding.key)
    }
}

fn char_key(ev: &KeyEvent) -> Option<String> {
    match &ev.logical_key {
        Key::Character(s) => Some(s.to_lowercase()),
        _ => None,
    }
}

/// Shorten a title for a tab label, with an ellipsis when it overflows.
fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// "Where" label for a feed row: host (only when remote) + cwd basename + Zellij session hint.
fn feed_label(host: &str, cwd: &str, zellij: Option<&feed::ZellijLoc>) -> String {
    let dir = cwd
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd);
    let mut s = if host.is_empty() || host == "localhost" {
        dir.to_string()
    } else {
        format!("{host}:{dir}")
    };
    if let Some(sess) = zellij
        .and_then(|z| z.session.as_deref())
        .filter(|s| !s.is_empty())
    {
        s.push_str(&format!(" · zellij:{sess}"));
    }
    s
}

/// Parse a connect-dialog target `[user@]host[:port]` into (user, host, port). Missing user
/// defaults to `$USER`; missing port to 22.
fn parse_host(input: &str) -> (String, String, u16) {
    let input = input.trim();
    let (user, rest) = match input.split_once('@') {
        Some((u, r)) => (u.to_string(), r),
        None => (default_user(), input),
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (rest.to_string(), 22),
    };
    (user, host, port)
}

fn profile_target(profile: &ConnectionProfile) -> String {
    let user = if profile.user.is_empty() {
        String::new()
    } else {
        format!("{}@", profile.user)
    };
    if profile.port == 22 {
        format!("{user}{}", profile.host)
    } else {
        format!("{user}{}:{}", profile.host, profile.port)
    }
}

fn clean_profile_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn terminal_should_receive_ime_commit(
    has_terms: bool,
    text_input_active: bool,
    egui_consumed: bool,
) -> bool {
    has_terms && !text_input_active && !egui_consumed
}

/// Some Wayland compositors send ordinary key text as a bare `Ime::Commit` once text input is
/// enabled, without a preceding `Ime::Preedit`. `egui-winit` deliberately does not enable its
/// Linux composition state for `Ime::Enabled` alone, so its stored IME cursor stops advancing and
/// a `TextEdit` discards every commit after the first. Feed bare Linux commits to egui as ordinary
/// text; commits belonging to a real composition keep the normal IME path so the preedit text is
/// replaced rather than duplicated.
fn ime_commit_needs_text_fallback(text_input_active: bool, had_preedit: bool) -> bool {
    cfg!(target_os = "linux") && text_input_active && !had_preedit
}

/// Terminals that belong to neither the current workspace nor an in-progress restore can be
/// removed. Restored panes are deliberately wired before `Ready` and only placed into tabs when
/// the restore burst finishes, so treating them as stale in that interval would close them on the
/// daemon during an unrelated redraw (the connection-progress spinner is enough to trigger one).
fn stale_terminal_ids(
    terminal_ids: impl IntoIterator<Item = PaneId>,
    workspace_live: &[PaneId],
    restoring: &std::collections::HashSet<PaneId>,
) -> Vec<PaneId> {
    terminal_ids
        .into_iter()
        .filter(|id| !workspace_live.contains(id) && !restoring.contains(id))
        .collect()
}

fn unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

/// The common identity files under `~/.ssh` that exist — tried after the agent.
fn default_keys() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return Vec::new();
    };
    let ssh = PathBuf::from(home).join(".ssh");
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|n| ssh.join(n))
        .filter(|p| p.exists())
        .collect()
}

/// Compact age label: seconds under a minute, then minutes, then hours.
fn human_age(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn rgb(r: u8, g: u8, b: u8) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn chrome_bg() -> egui::Color32 {
    rgb(18, 20, 24)
}

fn chrome_surface() -> egui::Color32 {
    rgb(27, 30, 36)
}

fn chrome_surface_2() -> egui::Color32 {
    rgb(37, 41, 49)
}

fn chrome_hover() -> egui::Color32 {
    rgb(45, 52, 62)
}

fn chrome_stroke() -> egui::Stroke {
    egui::Stroke::new(1.0, rgba(142, 155, 176, 90))
}

fn chrome_text() -> egui::Color32 {
    rgb(226, 232, 240)
}

fn chrome_muted() -> egui::Color32 {
    rgb(148, 163, 184)
}

fn chrome_accent() -> egui::Color32 {
    rgb(34, 244, 255)
}

fn chrome_magenta() -> egui::Color32 {
    rgb(214, 93, 255)
}

fn chrome_blue() -> egui::Color32 {
    rgb(125, 167, 255)
}

fn chrome_warn() -> egui::Color32 {
    rgb(255, 184, 108)
}

fn chrome_error() -> egui::Color32 {
    rgb(255, 104, 120)
}

fn chrome_shadow() -> egui::Shadow {
    egui::Shadow {
        offset: [0, 12],
        blur: 28,
        spread: 0,
        color: egui::Color32::from_black_alpha(140),
    }
}

fn popup_frame() -> egui::Frame {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(12, 10))
        .fill(chrome_surface())
        .stroke(chrome_stroke())
        .corner_radius(egui::CornerRadius::same(8))
        .shadow(chrome_shadow())
}

fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(16, 14))
        .fill(chrome_surface())
        .stroke(chrome_stroke())
        .corner_radius(egui::CornerRadius::same(8))
        .shadow(chrome_shadow())
}

fn tabbar_frame() -> egui::Frame {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 4))
        .fill(rgba(16, 18, 22, 245))
        .stroke(egui::Stroke::new(1.0, rgba(142, 155, 176, 40)))
}

fn inset_frame() -> egui::Frame {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(9, 7))
        .fill(chrome_bg())
        .stroke(egui::Stroke::new(1.0, rgba(142, 155, 176, 55)))
        .corner_radius(egui::CornerRadius::same(6))
}

fn warning_frame() -> egui::Frame {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(9, 7))
        .fill(rgba(255, 104, 120, 26))
        .stroke(egui::Stroke::new(1.0, rgba(255, 104, 120, 120)))
        .corner_radius(egui::CornerRadius::same(6))
}

fn apply_chrome_style(ctx: &egui::Context, ui_size: f32) {
    ctx.global_style_mut(|style| {
        for st in [egui::TextStyle::Body, egui::TextStyle::Button] {
            if let Some(f) = style.text_styles.get_mut(&st) {
                f.size = ui_size;
            }
        }
        if let Some(f) = style.text_styles.get_mut(&egui::TextStyle::Heading) {
            f.size = (ui_size + 2.0).max(14.0);
        }

        style.spacing.window_margin = egui::Margin::symmetric(16, 14);
        style.spacing.menu_margin = egui::Margin::symmetric(10, 8);
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(9.0, 4.0);
        style.spacing.interact_size = egui::vec2(44.0, 24.0);
        style.spacing.text_edit_width = 260.0;
        style.spacing.menu_width = 240.0;

        let visuals = &mut style.visuals;
        visuals.dark_mode = true;
        visuals.override_text_color = None;
        visuals.weak_text_alpha = 0.72;
        visuals.weak_text_color = Some(chrome_muted());
        visuals.hyperlink_color = chrome_blue();
        visuals.faint_bg_color = rgba(125, 167, 255, 18);
        visuals.extreme_bg_color = chrome_bg();
        visuals.text_edit_bg_color = Some(chrome_bg());
        visuals.code_bg_color = rgb(15, 18, 22);
        visuals.warn_fg_color = chrome_warn();
        visuals.error_fg_color = chrome_error();
        visuals.window_corner_radius = egui::CornerRadius::same(8);
        visuals.window_shadow = chrome_shadow();
        visuals.window_fill = chrome_surface();
        visuals.window_stroke = chrome_stroke();
        visuals.menu_corner_radius = egui::CornerRadius::same(8);
        visuals.panel_fill = rgb(13, 15, 18);
        visuals.popup_shadow = chrome_shadow();
        visuals.selection.bg_fill = rgba(34, 244, 255, 72);
        visuals.selection.stroke = egui::Stroke::new(1.0, chrome_text());
        visuals.text_cursor.stroke = egui::Stroke::new(1.4, chrome_accent());
        visuals.button_frame = true;

        visuals.widgets.noninteractive.bg_fill = chrome_surface();
        visuals.widgets.noninteractive.weak_bg_fill = chrome_surface_2();
        visuals.widgets.noninteractive.bg_stroke = chrome_stroke();
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, chrome_text());
        visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);

        visuals.widgets.inactive.bg_fill = chrome_surface_2();
        visuals.widgets.inactive.weak_bg_fill = rgba(125, 167, 255, 18);
        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, rgba(142, 155, 176, 70));
        visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, chrome_text());
        visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);

        visuals.widgets.hovered.bg_fill = chrome_hover();
        visuals.widgets.hovered.weak_bg_fill = rgba(34, 244, 255, 34);
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, rgba(34, 244, 255, 150));
        visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, chrome_text());
        visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);

        visuals.widgets.active.bg_fill = rgba(34, 244, 255, 58);
        visuals.widgets.active.weak_bg_fill = rgba(214, 93, 255, 42);
        visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, chrome_accent());
        visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, chrome_text());
        visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);

        visuals.widgets.open = visuals.widgets.hovered;
    });
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(3.0);
    ui.label(
        egui::RichText::new(text)
            .small()
            .strong()
            .color(chrome_muted()),
    );
}

fn dialog_caption(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(chrome_muted()));
}

fn dialog_header(ui: &mut egui::Ui, title: &str, close_hover: Option<&str>) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(title)
                .strong()
                .color(chrome_text())
                .size((ui.text_style_height(&egui::TextStyle::Body) + 1.0).max(14.0)),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(hover) = close_hover {
                close = icon_button(ui, "x", hover).clicked();
            }
        });
    });
    ui.add_space(2.0);
    close
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).strong().color(rgb(7, 22, 19)))
            .fill(chrome_accent())
            .stroke(egui::Stroke::new(1.0, rgba(207, 250, 244, 120)))
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(84.0, 28.0)),
    )
}

fn secondary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(text)
            .fill(chrome_surface_2())
            .stroke(chrome_stroke())
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(78.0, 28.0)),
    )
}

fn stepper_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(text)
            .fill(chrome_bg())
            .stroke(chrome_stroke())
            .corner_radius(egui::CornerRadius::same(5))
            .min_size(egui::vec2(30.0, 26.0)),
    )
}

fn icon_button(ui: &mut egui::Ui, text: &str, hover: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).strong())
            .fill(egui::Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(24.0, 24.0)),
    )
    .on_hover_text(hover)
}

fn chrome_text_edit<'a>(text: &'a mut String, hint: &'static str) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(text)
        .hint_text(hint)
        .desired_width(f32::INFINITY)
        .margin(egui::Margin::symmetric(8, 5))
}

fn menu_item(ui: &mut egui::Ui, icon: &str, label: &str, hint: Option<&str>) -> egui::Response {
    let width = ui.available_width().clamp(220.0, 240.0);
    let height = 28.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() {
            chrome_hover()
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(6), fill);
        ui.painter().text(
            rect.left_center() + egui::vec2(9.0, 0.0),
            egui::Align2::LEFT_CENTER,
            icon,
            egui::FontId::monospace(12.0),
            if response.hovered() {
                chrome_magenta()
            } else {
                chrome_accent()
            },
        );
        ui.painter().text(
            rect.left_center() + egui::vec2(42.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Button) - 1.0),
            chrome_text(),
        );
        if let Some(hint) = hint {
            ui.painter().text(
                rect.right_center() - egui::vec2(10.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                hint,
                egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Small)),
                chrome_muted(),
            );
        }
    }

    response
}

fn profile_row(ui: &mut egui::Ui, profile: &ConnectProfileView) -> egui::Response {
    let width = ui.available_width().max(300.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 48.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() {
            chrome_hover()
        } else {
            chrome_bg()
        };
        ui.painter().rect(
            rect,
            egui::CornerRadius::same(7),
            fill,
            egui::Stroke::new(1.0, rgba(142, 155, 176, 60)),
            egui::StrokeKind::Inside,
        );
        let mode = if profile.use_potty_session {
            "session"
        } else {
            "plain"
        };
        ui.painter().text(
            rect.left_top() + egui::vec2(10.0, 9.0),
            egui::Align2::LEFT_TOP,
            elide(&profile.label, 30),
            egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Button)),
            chrome_text(),
        );
        ui.painter().text(
            rect.right_top() + egui::vec2(-10.0, 9.0),
            egui::Align2::RIGHT_TOP,
            mode,
            egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Small)),
            if profile.use_potty_session {
                chrome_accent()
            } else {
                chrome_muted()
            },
        );
        ui.painter().text(
            rect.left_bottom() + egui::vec2(10.0, -9.0),
            egui::Align2::LEFT_BOTTOM,
            elide(&profile.detail, 48),
            egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Small)),
            chrome_muted(),
        );
    }
    response
}

/// A full-width attention-feed card. The card itself jumps to the pane; the dismiss target is a
/// separate, later interaction so its click cannot leak through and activate the card beneath it.
fn feed_row(ui: &mut egui::Ui, item: &FeedItem) -> (bool, bool) {
    let can_jump = item.can_activate;
    let width = ui.available_width().max(300.0);
    let height = if item.message.is_empty() { 44.0 } else { 62.0 };
    let (rect, card) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());

    let dismiss_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 18.0, rect.center().y),
        egui::vec2(26.0, 26.0),
    );
    let dismiss = ui
        .interact(
            dismiss_rect,
            egui::Id::new(("feed-dismiss", &item.key)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Dismiss");
    let card = if can_jump {
        card.on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Jump to this pane")
    } else {
        card
    };

    if ui.is_rect_visible(rect) {
        let hovered = can_jump && card.hovered();
        ui.painter().rect(
            rect,
            egui::CornerRadius::same(7),
            if hovered { chrome_hover() } else { chrome_bg() },
            if hovered {
                egui::Stroke::new(1.0, rgba(34, 244, 255, 145))
            } else {
                egui::Stroke::new(1.0, rgba(142, 155, 176, 60))
            },
            egui::StrokeKind::Inside,
        );
        if dismiss.hovered() {
            ui.painter().rect_filled(
                dismiss_rect,
                egui::CornerRadius::same(6),
                rgba(255, 104, 120, 34),
            );
        }
        ui.painter().text(
            dismiss_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Button)),
            if dismiss.hovered() {
                chrome_error()
            } else {
                chrome_muted()
            },
        );

        let tool = match item.tool {
            feed::Tool::Claude => "claude",
            feed::Tool::Codex => "codex",
            feed::Tool::Other => "tool",
        };
        let head = format!("{tool} · {} · {}", item.label, item.age);
        let text_left = rect.left() + 38.0;
        ui.painter().text(
            egui::pos2(text_left, rect.top() + 10.0),
            egui::Align2::LEFT_TOP,
            elide(&head, 48),
            egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Button)),
            chrome_text(),
        );
        if !item.message.is_empty() {
            ui.painter().text(
                egui::pos2(text_left, rect.bottom() - 10.0),
                egui::Align2::LEFT_BOTTOM,
                elide(&item.message, 56),
                egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Small)),
                chrome_muted(),
            );
        }
    }

    let dismiss_clicked = dismiss.clicked();
    (
        can_jump && card.clicked() && !dismiss_clicked,
        dismiss_clicked,
    )
}

/// The one pane/tab menu, used by both the ☰ button (`for_pane` = None → acts on the focused
/// pane) and a pane's right-click context menu (`for_pane` = that pane). Being the single menu
/// means hiding the tab bar still gives full access via right-click. Font controls live in a
/// separate window (opened from here) rather than cluttering the menu.
///
/// NOTE: egui 0.34 is mid-migration to `ui.close`; `ui.close_menu` is deprecated-but-working.
#[allow(deprecated)]
fn full_menu(
    ui: &mut egui::Ui,
    actions: &mut Vec<Action>,
    cfg: &Config,
    for_pane: Option<PaneId>,
    can_detach: bool,
) {
    ui.set_min_width(228.0);
    ui.style_mut().interaction.selectable_labels = false;
    let focus = |actions: &mut Vec<Action>| {
        if let Some(id) = for_pane {
            actions.push(Action::Focus(id));
        }
    };
    let new_tab = shortcut_hint(&binding_from_config(cfg, MenuAction::NewTab));
    if menu_item(ui, "+", "New tab", Some(&new_tab)).clicked() {
        actions.push(Action::NewTab);
        ui.close_menu();
    }
    let split_right = shortcut_hint(&binding_from_config(cfg, MenuAction::SplitRight));
    if menu_item(ui, ">", "Split right", Some(&split_right)).clicked() {
        focus(actions);
        actions.push(Action::Split(Split::Cols));
        ui.close_menu();
    }
    let split_down = shortcut_hint(&binding_from_config(cfg, MenuAction::SplitDown));
    if menu_item(ui, "v", "Split down", Some(&split_down)).clicked() {
        focus(actions);
        actions.push(Action::Split(Split::Rows));
        ui.close_menu();
    }
    let close_pane = shortcut_hint(&binding_from_config(cfg, MenuAction::ClosePane));
    if menu_item(ui, "x", "Close pane", Some(&close_pane)).clicked() {
        focus(actions);
        actions.push(Action::ClosePane);
        ui.close_menu();
    }
    ui.separator();
    section_label(ui, "SSH");
    let connect = shortcut_hint(&binding_from_config(cfg, MenuAction::Connect));
    if menu_item(ui, "ssh", "Connect to host...", Some(&connect)).clicked() {
        actions.push(Action::OpenConnect);
        ui.close_menu();
    }
    // Only for a persistent (potty-session) remote pane: leave the session running and disconnect.
    let detach = shortcut_hint(&binding_from_config(cfg, MenuAction::DetachSession));
    if can_detach
        && menu_item(ui, "dt", "Detach session", Some(&detach))
            .on_hover_text("Disconnect but keep the remote shells running, to reattach later")
            .clicked()
    {
        focus(actions);
        actions.push(Action::DetachSession);
        ui.close_menu();
    }
    ui.separator();
    section_label(ui, "View");
    let settings = shortcut_hint(&binding_from_config(cfg, MenuAction::Settings));
    if menu_item(ui, "Aa", "Settings...", Some(&settings)).clicked() {
        actions.push(Action::ShowSettings);
        ui.close_menu();
    }
}

fn pane_context_menu(
    response: &egui::Response,
    force_open: bool,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let popup = egui::Popup::context_menu(response);
    let popup = if force_open {
        popup.open_memory(Some(egui::SetOpenCommand::Bool(true)))
    } else {
        popup
    };
    popup.show(add_contents);
}

/// One row of the color-scheme list: a swatch strip (background, foreground, then the six
/// colorful normal ANSI slots) and the scheme name; clicking the name applies the scheme.
fn scheme_row(
    ui: &mut egui::Ui,
    name: &str,
    scheme: config::Colors,
    current: &config::Colors,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for hex in [&scheme.background, &scheme.foreground]
            .into_iter()
            .chain(scheme.ansi.iter().skip(1).take(6))
        {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
            if let Some(c) = config::parse_hex(hex) {
                ui.painter()
                    .rect_filled(rect, 2, egui::Color32::from_rgb(c.r, c.g, c.b));
            }
        }
        ui.add_space(4.0);
        let selected = *current == scheme;
        if ui.selectable_label(selected, name).clicked() && !selected {
            actions.push(Action::SetColors(scheme));
        }
    });
}

/// The floating Settings window: terminal font size/family plus app hotkeys. Visibility is tied to
/// `open` (its close button flips it off).
// too_many_arguments: like `build_ui`, the parameters are the dialog's view model.
#[allow(clippy::too_many_arguments)]
fn settings_window(
    ctx: &egui::Context,
    cfg: &Config,
    open: &mut bool,
    page: &mut SettingsPage,
    shortcut_capture: Option<MenuAction>,
    families: &[String],
    cur_family: Option<&str>,
    cur_size: f32,
    actions: &mut Vec<Action>,
) {
    let mut request_close = false;
    egui::Window::new("Settings")
        .open(open)
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(dialog_frame())
        .show(ctx, |ui| {
            ui.set_min_width(560.0);
            request_close |= dialog_header(ui, "Settings", Some("Close"));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(132.0);
                    ui.style_mut().interaction.selectable_labels = false;
                    if ui
                        .selectable_label(*page == SettingsPage::Font, "Font")
                        .clicked()
                    {
                        *page = SettingsPage::Font;
                    }
                    if ui
                        .selectable_label(*page == SettingsPage::Colors, "Colors")
                        .clicked()
                    {
                        *page = SettingsPage::Colors;
                    }
                    if ui
                        .selectable_label(*page == SettingsPage::Hotkeys, "Hotkeys")
                        .clicked()
                    {
                        *page = SettingsPage::Hotkeys;
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(390.0);
                    // Both pages lay out in the same fixed-height area so the window keeps one
                    // height across tabs. Without this the auto-sized window gets stuck small:
                    // the font list shrinks to the height remembered from the shorter Hotkeys
                    // page, and the window then re-sizes to the shrunk list.
                    ui.set_min_height(380.0);
                    ui.set_max_height(380.0);
                    match *page {
                        SettingsPage::Font => {
                            dialog_caption(ui, "Terminal renderer");
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                ui.label(egui::RichText::new("Size").color(chrome_muted()));
                                if stepper_button(ui, "-").clicked() {
                                    actions.push(Action::SetFontSize(cur_size - 1.0));
                                }
                                ui.label(
                                    egui::RichText::new(format!("{cur_size:.0} px"))
                                        .monospace()
                                        .color(chrome_text()),
                                );
                                if stepper_button(ui, "+").clicked() {
                                    actions.push(Action::SetFontSize(cur_size + 1.0));
                                }
                            });
                            ui.separator();
                            section_label(ui, "Font family");
                            let list_width = ui.available_width();
                            inset_frame().show(ui, |ui| {
                                ui.set_min_width((list_width - 20.0).max(260.0));
                                egui::ScrollArea::vertical()
                                    .auto_shrink([true, false])
                                    .show(ui, |ui| {
                                        if ui
                                            .selectable_label(
                                                cur_family.is_none(),
                                                "(default monospace)",
                                            )
                                            .clicked()
                                        {
                                            actions.push(Action::SetFontFamily(None));
                                        }
                                        for fam in families {
                                            if ui
                                                .selectable_label(
                                                    cur_family == Some(fam.as_str()),
                                                    fam,
                                                )
                                                .clicked()
                                            {
                                                actions
                                                    .push(Action::SetFontFamily(Some(fam.clone())));
                                            }
                                        }
                                    });
                            });
                        }
                        SettingsPage::Colors => {
                            dialog_caption(ui, "Color scheme");
                            ui.add_space(2.0);
                            let list_width = ui.available_width();
                            inset_frame().show(ui, |ui| {
                                ui.set_min_width((list_width - 20.0).max(260.0));
                                scheme_row(
                                    ui,
                                    "(default)",
                                    config::Colors::default(),
                                    &cfg.colors,
                                    actions,
                                );
                                for preset in config::COLOR_PRESETS {
                                    scheme_row(
                                        ui,
                                        preset.name,
                                        preset.colors(),
                                        &cfg.colors,
                                        actions,
                                    );
                                }
                            });
                        }
                        SettingsPage::Hotkeys => {
                            dialog_caption(ui, "Keyboard shortcuts");
                            ui.add_space(2.0);
                            inset_frame().show(ui, |ui| {
                                ui.set_min_width(360.0);
                                for shortcut in SHORTCUTS {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(shortcut.label)
                                                .color(chrome_text()),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if secondary_button(ui, "Reset").clicked() {
                                                    actions.push(Action::ResetShortcut(
                                                        shortcut.action,
                                                    ));
                                                }
                                                let binding =
                                                    binding_from_config(cfg, shortcut.action);
                                                let waiting =
                                                    shortcut_capture == Some(shortcut.action);
                                                let label = if waiting {
                                                    "Press shortcut...".to_string()
                                                } else {
                                                    shortcut_hint(&binding)
                                                };
                                                if ui
                                                    .button(
                                                        egui::RichText::new(label)
                                                            .monospace()
                                                            .color(if waiting {
                                                                chrome_accent()
                                                            } else {
                                                                chrome_text()
                                                            }),
                                                    )
                                                    .clicked()
                                                {
                                                    actions.push(Action::CaptureShortcut(
                                                        shortcut.action,
                                                    ));
                                                }
                                            },
                                        );
                                    });
                                }
                            });
                        }
                    }
                });
            });
        });
    if request_close {
        *open = false;
    }
}

// too_many_arguments: this is the once-per-frame chrome entry point; its parameters *are* the
// view model. Bundling them into a struct would just move the list, not shrink it.
#[allow(deprecated, clippy::too_many_arguments)]
fn build_ui(
    ctx: &egui::Context,
    cfg: &Config,
    ws: &Workspace,
    families: &[String],
    cur_family: Option<&str>,
    cur_size: f32,
    tab_titles: &[String],
    border_color: egui::Color32,
    pane_padding: f32,
    show_settings: &mut bool,
    settings_page: &mut SettingsPage,
    shortcut_capture: Option<MenuAction>,
    actions: &mut Vec<Action>,
    leaves: &mut Vec<(PaneId, egui::Rect)>,
    dividers: &mut Vec<egui::Rect>,
    pending: &[FeedItem],
    feed_active: &mut bool,
    chrome_latched: bool,
    feed_open: bool,
    auth: Option<&AuthView>,
    auth_inputs: &mut Vec<String>,
    show_connect: bool,
    connect_host: &mut String,
    connect_name: &mut String,
    connect_use_session: &mut bool,
    connect_profiles: &[ConnectProfileView],
    connect_progress: &[ConnectProgressView],
    connect_progress_active: &mut bool,
    error: Option<&str>,
    detachable_panes: &std::collections::HashSet<PaneId>,
    context_menu_pane: Option<PaneId>,
) {
    // The tab bar earns its space with more than one tab, or once the attention feed has latched
    // it on (it hosts the bell). Otherwise the menu lives on the pane's right-click
    // (shift-right-click when an app has grabbed the mouse).
    let show_chrome = ws.tabs.len() > 1 || chrome_latched;
    if show_chrome {
        egui::TopBottomPanel::top("tabbar")
            .frame(tabbar_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, _tab) in ws.tabs.iter().enumerate() {
                        // Label + close grouped tightly so they read as one tab.
                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.x = 3.0;
                            let title = elide(&tab_titles[i], 24);
                            if ui
                                .add(
                                    egui::Button::selectable(i == ws.active, title)
                                        .corner_radius(egui::CornerRadius::same(6)),
                                )
                                .clicked()
                            {
                                actions.push(Action::SelectTab(i));
                            }
                            let close =
                                egui::Button::new(egui::RichText::new("×").weak()).frame(false);
                            if ui.add(close).on_hover_text("Close tab").clicked() {
                                actions.push(Action::CloseTab(i));
                            }
                        });
                    }
                    if icon_button(ui, "+", "New tab").clicked() {
                        actions.push(Action::NewLocalTab);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let focus_detachable = detachable_panes.contains(&ws.active_tab().focus);
                        ui.menu_button("☰", |ui| {
                            full_menu(ui, actions, cfg, None, focus_detachable)
                        });
                        // Attention-feed bell — present once the feed has latched the bar on. Shows
                        // the waiting count; toggles the overlay.
                        if chrome_latched {
                            let empty = pending.is_empty();
                            let label = if empty {
                                "\u{1F514}".to_string() // 🔔
                            } else {
                                format!("\u{1F514} {}", pending.len())
                            };
                            let hover = if empty {
                                "Nothing waiting — click to dismiss"
                            } else {
                                "Sessions waiting for you"
                            };
                            if ui
                                .add(
                                    egui::Button::selectable(feed_open, label)
                                        .corner_radius(egui::CornerRadius::same(6)),
                                )
                                .on_hover_text(hover)
                                .clicked()
                            {
                                // With nothing waiting, the bell is a dismiss: un-latch the bar.
                                // Otherwise it toggles the overlay.
                                actions.push(if empty {
                                    Action::DismissFeed
                                } else {
                                    Action::ShowFeed(!feed_open)
                                });
                            }
                        }
                    });
                });
            });
    }

    if *show_settings {
        settings_window(
            ctx,
            cfg,
            show_settings,
            settings_page,
            shortcut_capture,
            families,
            cur_family,
            cur_size,
            actions,
        );
    }

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ctx, |ui| {
            let area = ui.max_rect();
            let focus = ws.active_tab().focus;
            // Each leaf is a live terminal drawn underneath (pass 1) — keep the fill
            // transparent. We claim the rect so right-click opens the context menu; left-clicks
            // fall through to our own handler (which gates geometrically, not on egui
            // consumption), so selection/focus still work. Plain right-click in a mouse-mode
            // app is withheld from egui upstream so it reaches the app instead.
            //
            // A lone pane draws no border (it'd just be noise) and fills its area. With several
            // panes the border helps, and the terminal is inset by `pane_padding` so the cells
            // don't touch it.
            let rects = ws.leaf_rects(area);
            let single = rects.len() == 1;
            for (id, rect) in rects {
                let response = ui.interact(
                    rect,
                    egui::Id::new(("pane", ws.active, id)),
                    egui::Sense::click(),
                );
                pane_context_menu(&response, context_menu_pane == Some(id), |ui| {
                    full_menu(ui, actions, cfg, Some(id), detachable_panes.contains(&id))
                });
                if single {
                    leaves.push((id, rect));
                    continue;
                }
                let stroke = if id == focus {
                    egui::Stroke::new(1.5, border_color)
                } else {
                    egui::Stroke::new(1.0, egui::Color32::from_gray(60))
                };
                ui.painter().rect_stroke(
                    rect,
                    egui::CornerRadius::same(4),
                    stroke,
                    egui::StrokeKind::Inside,
                );
                leaves.push((id, rect.shrink(pane_padding)));
            }

            // Draggable split dividers. The grab region is widened beyond the thin visual gap;
            // we publish it (`dividers`) so our own mouse handler yields these pixels to egui.
            for d in ws.dividers(area) {
                let grab = match d.split {
                    Split::Cols => d.rect.expand2(egui::vec2(4.0, 0.0)),
                    Split::Rows => d.rect.expand2(egui::vec2(0.0, 4.0)),
                };
                dividers.push(grab);
                let resp = ui.interact(
                    grab,
                    egui::Id::new(("divider", ws.active, d.id)),
                    egui::Sense::drag(),
                );
                let active = resp.hovered() || resp.dragged();
                if active {
                    let cursor = match d.split {
                        Split::Cols => egui::CursorIcon::ResizeHorizontal,
                        Split::Rows => egui::CursorIcon::ResizeVertical,
                    };
                    ui.ctx().set_cursor_icon(cursor);
                    ui.painter()
                        .rect_filled(d.rect, egui::CornerRadius::ZERO, border_color);
                }
                if resp.dragged()
                    && let Some(p) = resp.interact_pointer_pos()
                {
                    let ratio = match d.split {
                        Split::Cols => (p.x - d.area.min.x) / (d.area.width() - GAP),
                        Split::Rows => (p.y - d.area.min.y) / (d.area.height() - GAP),
                    };
                    actions.push(Action::SetRatio(d.id, ratio));
                }
            }
        });

    // Attention feed — a floating list of sessions waiting for you, drawn as an Area so it
    // overlays the terminal without affecting pane sizing. Shown when the user has it open
    // (auto-opened on a fresh wave; toggled by the bell; hidden by its × button).
    *feed_active = false;
    if feed_open {
        let top = if show_chrome { TOPBAR + 6.0 } else { 8.0 };
        let r = egui::Area::new(egui::Id::new("attention-feed"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, top))
            .show(ctx, |ui| {
                popup_frame().show(ui, |ui| {
                    // egui labels are drag-selectable by default; that just produces stray text
                    // highlights in a read-only overlay, so turn it off here.
                    ui.style_mut().interaction.selectable_labels = false;
                    ui.set_min_width(360.0);
                    ui.set_max_width(360.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} waiting", pending.len())).strong(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if icon_button(ui, "×", "Hide (reopen from the bell)").clicked() {
                                actions.push(Action::ShowFeed(false));
                            }
                        });
                    });
                    for item in pending {
                        ui.add_space(3.0);
                        let (jump, dismiss) = feed_row(ui, item);
                        if dismiss {
                            actions.push(Action::DismissNote(item.key.clone()));
                        } else if jump {
                            actions.push(Action::ActivateNote(item.key.clone(), item.pane));
                        }
                    }
                });
            });
        // Suppress our terminal mouse handling while the pointer is over the feed (it gates on
        // geometry, not egui consumption — see the CentralPanel comment).
        *feed_active = r.response.contains_pointer();
    }

    // SSH auth prompt, centered. The connection thread is blocked waiting on the answer.
    match auth {
        Some(AuthView::HostKey {
            host,
            fingerprint,
            status,
        }) => {
            egui::Window::new("SSH host key")
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_max_width(440.0);
                    dialog_header(ui, "SSH host key", None);
                    let what = match status {
                        remote::HostKeyStatus::Unknown => "is not in your known_hosts yet",
                        remote::HostKeyStatus::Changed => "HAS CHANGED since you last connected",
                    };
                    dialog_caption(ui, format!("The host key for {host} {what}:").as_str());
                    ui.add_space(4.0);
                    inset_frame().show(ui, |ui| {
                        ui.monospace(fingerprint);
                    });
                    if *status == remote::HostKeyStatus::Changed {
                        ui.add_space(4.0);
                        warning_frame().show(ui, |ui| {
                            ui.colored_label(
                                chrome_error(),
                                "If you didn't change it, this could be a man-in-the-middle attack.",
                            );
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if primary_button(ui, "Connect").clicked() {
                            actions.push(Action::AuthAnswer(true));
                        }
                        if secondary_button(ui, "Cancel").clicked() {
                            actions.push(Action::AuthAnswer(false));
                        }
                    });
                });
        }
        Some(AuthView::Text { title, fields }) => {
            auth_inputs.resize(fields.len(), String::new());
            egui::Window::new(title.as_str())
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_max_width(440.0);
                    dialog_header(ui, title.as_str(), None);
                    // Capture Enter before the fields — a singleline TextEdit consumes it.
                    let mut submit = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    for (i, (label, echo)) in fields.iter().enumerate() {
                        section_label(ui, label.as_str());
                        let edit = chrome_text_edit(&mut auth_inputs[i], "").password(!echo);
                        let resp = ui.add(edit);
                        // Auto-focus the first field on open (only while nothing else is focused, so
                        // it doesn't fight the user tabbing between fields).
                        if i == 0 && ui.memory(|m| m.focused().is_none()) {
                            resp.request_focus();
                        }
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        submit |= primary_button(ui, "OK").clicked();
                        if secondary_button(ui, "Cancel").clicked() {
                            actions.push(Action::AuthText(false));
                        }
                    });
                    if submit {
                        actions.push(Action::AuthText(true));
                    }
                });
        }
        None => {}
    }

    // "Connect to host…" dialog.
    if show_connect {
        egui::Window::new("Connect to host")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.set_max_width(420.0);
                dialog_header(ui, "Connect to host", None);
                section_label(ui, "Host");
                // Capture keys before the field — a singleline TextEdit consumes them.
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
                let resp = ui.add(chrome_text_edit(connect_host, "user@host"));
                if ui.memory(|m| m.focused().is_none()) {
                    resp.request_focus();
                }
                ui.add_space(4.0);
                section_label(ui, "Name");
                ui.add(chrome_text_edit(connect_name, "optional"));
                ui.add_space(4.0);
                ui.checkbox(
                    connect_use_session,
                    "Use potty-session (persistent multiplexing)",
                )
                .on_hover_text(
                    "Run potty-session on the host for split panes that survive disconnects \
                         (it must be installed there). Off = a plain SSH shell.",
                );
                if !connect_profiles.is_empty() {
                    ui.separator();
                    section_label(ui, "Recent");
                    egui::ScrollArea::vertical()
                        .max_height(190.0)
                        .show(ui, |ui| {
                            for profile in connect_profiles {
                                let resp = profile_row(ui, profile);
                                if resp.on_hover_text(profile.detail.as_str()).clicked() {
                                    actions.push(Action::UseProfile(profile.index));
                                }
                                ui.add_space(4.0);
                            }
                        });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if (primary_button(ui, "Connect").clicked() || enter)
                        && !connect_host.trim().is_empty()
                    {
                        actions.push(Action::Connect(connect_host.clone(), connect_name.clone()));
                    }
                    if secondary_button(ui, "Cancel").clicked() || escape {
                        actions.push(Action::CloseConnect);
                    }
                });
            });
    }

    // SSH connection progress. This is intentionally a compact, non-modal overlay: users can keep
    // working in an existing pane while a slow SSH handshake, auth ladder, or session restore runs.
    *connect_progress_active = false;
    if !connect_progress.is_empty() {
        ctx.request_repaint_after(Duration::from_millis(100));
        let r = egui::Area::new(egui::Id::new("connection-progress"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-8.0, -8.0))
            .show(ctx, |ui| {
                popup_frame().show(ui, |ui| {
                    ui.style_mut().interaction.selectable_labels = false;
                    ui.set_max_width(340.0);
                    ui.label(egui::RichText::new("Connecting").strong());
                    for item in connect_progress {
                        ui.add_space(4.0);
                        inset_frame().show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.vertical(|ui| {
                                    ui.label(elide(&item.label, 36));
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} - {}s",
                                            item.detail,
                                            item.elapsed.as_secs()
                                        ))
                                        .weak(),
                                    );
                                });
                            });
                        });
                    }
                });
            });
        *connect_progress_active = r.response.contains_pointer();
    }

    // Connection error, shown instead of printing to stderr.
    if let Some(msg) = error {
        egui::Window::new("Connection failed")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                dialog_header(ui, "Connection failed", None);
                warning_frame().show(ui, |ui| {
                    ui.colored_label(chrome_error(), msg);
                });
                ui.separator();
                if primary_button(ui, "OK").clicked() {
                    actions.push(Action::DismissError);
                }
            });
    }
}

// ---------------------------------------------------------------------------
// wgpu window state
// ---------------------------------------------------------------------------

struct WindowState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: SurfaceConfiguration,
    instance: wgpu::Instance,
    grid: GridRenderer,
    window: Arc<Window>,
}

const BG_CLEAR: wgpu::Color = wgpu::Color {
    r: 0.02,
    g: 0.02,
    b: 0.025,
    a: 1.0,
};

impl WindowState {
    async fn new(window: Arc<Window>, event_loop: &ActiveEventLoop) -> Self {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        let instance = Instance::new(InstanceDescriptor::new_with_display_handle(Box::new(
            event_loop.owned_display_handle(),
        )));
        let adapter = instance
            .request_adapter(&RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&DeviceDescriptor::default())
            .await
            .unwrap();

        let surface = instance.create_surface(window.clone()).expect("surface");
        let format = TextureFormat::Bgra8UnormSrgb;
        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width,
            height: size.height,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let grid = GridRenderer::new(&device, &queue, format, FONT_PX * scale, LINE_PX * scale);

        Self {
            device,
            queue,
            surface,
            surface_config,
            instance,
            grid,
            window,
        }
    }

    /// Acquire the surface texture, mapping the recoverable error cases to a redraw request.
    fn acquire(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => Some(f),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.window.request_redraw();
                None
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                self.surface.configure(&self.device, &self.surface_config);
                self.window.request_redraw();
                None
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone()).unwrap();
                self.surface.configure(&self.device, &self.surface_config);
                self.window.request_redraw();
                None
            }
            wgpu::CurrentSurfaceTexture::Validation => panic!("surface validation error"),
        }
    }

    /// Rebuild one pane's cached instance buffers (only the App's dirty panes call this).
    fn prepare_pane(
        &mut self,
        pane: u64,
        term: &Term<EventProxy>,
        origin: (f32, f32),
        screen: (f32, f32),
        show_cursor: bool,
        cursor_thickness: f32,
    ) {
        self.grid.prepare(
            &self.device,
            &self.queue,
            pane,
            term,
            origin,
            screen,
            show_cursor,
            cursor_thickness,
        );
    }

    fn render(
        &mut self,
        egui_renderer: &mut egui_wgpu::Renderer,
        textures_delta: &egui::TexturesDelta,
        prims: &[egui::ClippedPrimitive],
        ppp: f32,
        panes: &[(egui::Rect, u64)],
    ) {
        let (sw, sh) = (self.surface_config.width, self.surface_config.height);
        // Texture deltas are stateful: the first font-atlas delta allocates the texture, and later
        // frames usually contain only partial updates. Apply them even when the surface is
        // temporarily unavailable (common while macOS is occluding/configuring a window), or an
        // early return would discard the allocation and the next partial update would panic in
        // egui-wgpu with "texture has not been allocated yet".
        for (id, delta) in &textures_delta.set {
            egui_renderer.update_texture(&self.device, &self.queue, *id, delta);
        }
        let Some(frame) = self.acquire() else {
            // This frame was not painted, so its no-longer-needed textures can be released now.
            for id in &textures_delta.free {
                egui_renderer.free_texture(id);
            }
            return;
        };
        let view = frame.texture.create_view(&TextureViewDescriptor::default());

        // Pass 1: one submit per pane, each drawing from its cached buffers (already prepared
        // for dirty panes). The first clears the whole surface — including inter-pane gaps —
        // then each draw is scissored to its rect.
        let mut first = true;
        for (rect, pane) in panes {
            let mut encoder = self
                .device
                .create_command_encoder(&CommandEncoderDescriptor {
                    label: Some("terminal"),
                });
            {
                let load = if first {
                    LoadOp::Clear(BG_CLEAR)
                } else {
                    LoadOp::Load
                };
                let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                    label: Some("terminal"),
                    color_attachments: &[Some(RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: Operations {
                            load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                let x = (rect.min.x * ppp).max(0.0) as u32;
                let y = (rect.min.y * ppp).max(0.0) as u32;
                let w = ((rect.width() * ppp) as u32).min(sw.saturating_sub(x));
                let h = ((rect.height() * ppp) as u32).min(sh.saturating_sub(y));
                if w > 0 && h > 0 {
                    pass.set_scissor_rect(x, y, w, h);
                    self.grid.render(&mut pass, *pane);
                }
            }
            self.queue.submit(std::iter::once(encoder.finish()));
            first = false;
        }
        if first {
            // No panes (shouldn't happen) — still clear the surface so egui has a backdrop.
            let mut encoder = self
                .device
                .create_command_encoder(&CommandEncoderDescriptor {
                    label: Some("clear"),
                });
            encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(BG_CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.queue.submit(std::iter::once(encoder.finish()));
        }

        // Pass 2: egui chrome on top.
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("egui"),
            });
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [sw, sh],
            pixels_per_point: ppp,
        };
        let egui_cmds =
            egui_renderer.update_buffers(&self.device, &self.queue, &mut encoder, prims, &screen);
        {
            let pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            egui_renderer.render(&mut pass, prims, &screen);
        }
        for id in &textures_delta.free {
            egui_renderer.free_texture(id);
        }

        self.queue.submit(
            egui_cmds
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        frame.present();
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

struct App {
    proxy: EventLoopProxy<UserEvent>,
    /// Unique id for this GUI process. Used to route broadcast feed clicks back to the owner.
    instance_id: String,
    /// The per-instance socket exported to child shells as `$POTTY_NOTIFY`.
    notify_sock: PathBuf,
    /// Socket paths this process owns, removed on exit where possible.
    notify_paths: Vec<PathBuf>,
    state: Option<WindowState>,
    /// One live terminal per leaf pane, keyed by PaneId (across all tabs).
    terms: HashMap<PaneId, Terminal>,
    mods: Modifiers,

    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    egui_renderer: Option<egui_wgpu::Renderer>,
    workspace: Workspace,
    /// Size newly spawned panes start at (the most recent fit); they're refitted next redraw.
    last_dims: Dims,
    cell_w: f32,
    cell_h: f32,

    config: Config,
    config_path: PathBuf,
    font_families: Vec<String>,
    scale: f32,
    _watcher: Option<notify::RecommendedWatcher>,

    /// Active-tab pane rects in physical px: (id, origin x, origin y, width, height).
    pane_px: Vec<(PaneId, f32, f32, f32, f32)>,
    mouse_px: (f64, f64),
    /// The pane the in-progress press/drag (selection or mouse-report) belongs to.
    mouse_pane: Option<PaneId>,
    selecting: bool,
    last_click: Option<(Instant, Point)>,
    click_count: u8,
    /// While forwarding to a mouse-reporting app: which button is held, and the last cell
    /// reported (to suppress duplicate motion reports).
    mouse_held: Option<u8>,
    last_report_cell: Option<(i64, i64)>,

    /// Platform clipboard (+ primary selection on Linux). See `clip`.
    clipboard: Option<clip::Clipboard>,

    /// Last title pushed to the OS window (so we only call set_title on change).
    window_title: String,
    /// Whether an egui popup/menu was open as of the last frame — suppresses our own mouse
    /// handling so clicking a menu item doesn't also hit the terminal underneath.
    menu_open: bool,
    /// Pane whose context menu should be opened on the next egui frame. Used for platform
    /// secondary-click paths that egui/winit don't consistently turn into `secondary_clicked`.
    context_menu_pane: Option<PaneId>,
    /// Whether the floating Settings window is shown.
    show_settings: bool,
    settings_page: SettingsPage,
    shortcut_capture: Option<MenuAction>,

    /// Panes whose cached render is stale and must be re-prepared next frame (damage tracking).
    dirty: std::collections::HashSet<PaneId>,
    /// Pane ids currently on screen (the active tab's leaves) — a background pane's output
    /// marks it dirty but doesn't force a redraw.
    visible: std::collections::HashSet<PaneId>,
    /// Each visible pane's last-prepared pixel rect — a change (drag-resize, window resize)
    /// means its cached buffers are positioned wrong and must be rebuilt.
    last_rect: HashMap<PaneId, (f32, f32, f32, f32)>,
    /// Divider grab regions in physical px — a press here is a resize drag (egui-owned), so our
    /// terminal mouse handling skips it.
    divider_px: Vec<(f32, f32, f32, f32)>,

    /// Cursor-blink state. `blink_on` is the current visible phase (always true unless the focused
    /// cursor is actively blinking); `blink_next` is when to next toggle. The timer only runs
    /// while the focused pane's cursor blinks and is idle — see `about_to_wait`.
    blink_on: bool,
    blink_next: Option<Instant>,

    /// Attention feed: sessions waiting for the user. Fed by the notify sockets and mirrored
    /// between live potty instances; rendered as a floating overlay in the chrome.
    pending: HashMap<FeedKey, Pending>,

    /// Panes whose backend is a remote session — `reconcile_terms` must not spawn a local PTY for
    /// them, and closing them sends a `Close` frame rather than dropping a PTY.
    remote_panes: std::collections::HashSet<PaneId>,
    /// One-shot startup directories for local panes created from an existing pane.
    pending_cwds: HashMap<PaneId, PathBuf>,
    /// Live SSH connections, keyed by `ConnId`. Holds the per-connection id map and counter.
    connections: HashMap<ConnId, Connection>,
    /// SSH attempts that have been started but have not reached the remote protocol's `Ready`.
    connect_progress: HashMap<ConnId, ConnectProgress>,
    /// Allocates `ConnId`s.
    next_conn_id: ConnId,
    /// Pending auth prompts from remote connections, awaiting the user (host-key approval, …).
    /// Each carries a reply channel back to the blocked connection thread.
    auth_prompts: Vec<AuthPrompt>,
    /// Text-field buffers for the active `AuthPrompt::Text` dialog (one per prompt field).
    auth_inputs: Vec<String>,
    /// Whether the current IME composition emitted a preedit event. Wayland can also emit bare
    /// commits for ordinary text; those need a different egui input path (see `window_event`).
    ime_had_preedit: bool,
    /// Whether the "Connect to host…" dialog is open, and its host-field buffer.
    show_connect: bool,
    connect_host: String,
    connect_name: String,
    /// Connect-dialog checkbox: use `potty-session` (persistent multiplexing) vs. a plain SSH shell.
    /// Off by default — most hosts don't run `potty-session`.
    connect_use_session: bool,
    /// A connection error to show in a dialog (instead of stderr).
    error_msg: Option<String>,
    /// Whether the feed overlay is currently shown. Auto-opens on a fresh wave of notes, toggled
    /// by the tab-bar bell, hidden by the overlay's close button.
    feed_open: bool,
    /// Once a note has ever arrived, keep the tab bar visible (it hosts the bell) for the rest of
    /// the session — so the content doesn't jump as notifications come and go.
    chrome_latched: bool,
}

impl App {
    fn new(
        proxy: EventLoopProxy<UserEvent>,
        instance_id: String,
        notify_sock: PathBuf,
        notify_paths: Vec<PathBuf>,
    ) -> Self {
        Self {
            proxy,
            instance_id,
            notify_sock,
            notify_paths,
            state: None,
            terms: HashMap::new(),
            mods: Modifiers::default(),
            egui_ctx: egui::Context::default(),
            egui_state: None,
            egui_renderer: None,
            workspace: Workspace::new(),
            last_dims: Dims { cols: 80, rows: 24 },
            cell_w: 9.0,
            cell_h: 18.0,
            config: Config::default(),
            config_path: config::config_path(),
            font_families: Vec::new(),
            scale: 1.0,
            _watcher: None,
            pane_px: Vec::new(),
            mouse_px: (0.0, 0.0),
            mouse_pane: None,
            selecting: false,
            last_click: None,
            click_count: 0,
            mouse_held: None,
            last_report_cell: None,
            clipboard: None,
            window_title: String::new(),
            menu_open: false,
            context_menu_pane: None,
            show_settings: false,
            settings_page: SettingsPage::Font,
            shortcut_capture: None,
            dirty: std::collections::HashSet::new(),
            visible: std::collections::HashSet::new(),
            last_rect: HashMap::new(),
            divider_px: Vec::new(),
            blink_on: true,
            blink_next: None,
            pending: HashMap::new(),
            feed_open: false,
            chrome_latched: false,
            remote_panes: std::collections::HashSet::new(),
            pending_cwds: HashMap::new(),
            connections: HashMap::new(),
            connect_progress: HashMap::new(),
            next_conn_id: 0,
            auth_prompts: Vec::new(),
            auth_inputs: Vec::new(),
            ime_had_preedit: false,
            show_connect: false,
            connect_host: String::new(),
            connect_name: String::new(),
            connect_use_session: false,
            error_msg: None,
        }
    }

    /// Whether the focused pane's cursor is currently set to blink (config default or a program's
    /// DECSCUSR), and visible. Drives the blink timer in `about_to_wait`.
    fn focused_cursor_blinks(&self) -> bool {
        self.focused_arc().is_some_and(|t| {
            let g = t.lock().unwrap();
            g.cursor_style().blinking && g.mode().contains(TermMode::SHOW_CURSOR)
        })
    }

    /// Return the cursor to its solid phase and restart the blink cycle — called on focused-pane
    /// activity so the cursor never blinks out mid-keystroke.
    fn reset_blink(&mut self) {
        self.blink_on = true;
        self.blink_next = None;
    }

    /// A first-hop note belongs to this GUI process unless the sender already stamped it. Apply it
    /// locally and mirror it to sibling potty instances.
    fn on_local_note(&mut self, note: feed::Note) {
        let note = self.owned_note(note);
        self.apply_note(note.clone());
        self.broadcast_instance_message(&feed::InstanceMessage::Note { note });
    }

    /// A note forwarded by another potty process: apply it only. Rebroadcasting would loop.
    fn on_peer_note(&mut self, note: feed::Note) {
        if note.kind == feed::Kind::Raise
            && self.pending.keys().any(|key| {
                self.key_is_local(key) && key.host == note.host && key.session == note.session
            })
        {
            return;
        }
        self.apply_note(note);
    }

    fn owned_note(&self, mut note: feed::Note) -> feed::Note {
        if note.instance.is_none() {
            note.instance = Some(self.instance_id.clone());
        }
        note
    }

    /// Fold an attention note into the feed: a `raise` inserts/refreshes the session, a `clear`
    /// removes it. Keyed by `FeedKey` so re-raises update in place rather than pile up.
    fn apply_note(&mut self, note: feed::Note) {
        let key = FeedKey::from_note(&note);
        match note.kind {
            feed::Kind::Raise => {
                // A fresh wave (nothing was waiting) pops the feed open; mid-wave re-raises don't
                // re-pop it if the user has hidden it. Either way the tab bar latches on.
                let was_empty = self.pending.is_empty();
                if self.key_is_local(&key) {
                    let self_id = self.instance_id.as_str();
                    self.pending.retain(|other, _| {
                        other.host != key.host
                            || other.session != key.session
                            || other
                                .instance
                                .as_deref()
                                .is_none_or(|instance| instance == self_id)
                    });
                }
                self.pending.insert(
                    key,
                    Pending {
                        tool: note.tool,
                        message: note.message,
                        host: note.host,
                        cwd: note.cwd,
                        pane: note.pane,
                        zellij: note.zellij,
                        since: Instant::now(),
                    },
                );
                self.chrome_latched = true;
                if was_empty {
                    self.feed_open = true;
                }
            }
            feed::Kind::Clear => {
                self.remove_pending_key(&key);
            }
        }
        self.request_redraw();
    }

    fn key_is_local(&self, key: &FeedKey) -> bool {
        key.instance
            .as_deref()
            .is_none_or(|instance| instance == self.instance_id)
    }

    fn remove_pending_key(&mut self, key: &FeedKey) -> bool {
        let changed = self.pending.remove(key).is_some();
        if changed && self.pending.is_empty() {
            self.feed_open = false;
        }
        changed
    }

    fn dismiss_note(&mut self, key: FeedKey, broadcast: bool) {
        let changed = self.remove_pending_key(&key);
        if broadcast {
            self.broadcast_dismiss(&key);
        }
        if changed {
            self.request_redraw();
        }
    }

    /// Drop any feed entries owned by `pane` (the user landed on it, so it no longer needs
    /// flagging). Returns whether anything was removed.
    fn clear_pending_for_pane(&mut self, pane: PaneId) -> bool {
        let keys: Vec<FeedKey> = self
            .pending
            .iter()
            .filter(|(key, pending)| self.key_is_local(key) && pending.pane == Some(pane))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &keys {
            self.remove_pending_key(key);
            self.broadcast_dismiss(key);
        }
        !keys.is_empty()
    }

    /// Build the display-ready feed rows for the chrome, newest first.
    fn feed_items(&self) -> Vec<FeedItem> {
        let now = Instant::now();
        let mut rows: Vec<(Instant, FeedItem)> = self
            .pending
            .iter()
            .map(|(key, p)| {
                (
                    p.since,
                    FeedItem {
                        key: key.clone(),
                        tool: p.tool,
                        label: feed_label(&p.host, &p.cwd, p.zellij.as_ref()),
                        message: p.message.clone(),
                        age: human_age(now.saturating_duration_since(p.since)),
                        pane: p.pane,
                        can_activate: p.pane.is_some()
                            || key
                                .instance
                                .as_deref()
                                .is_some_and(|instance| instance != self.instance_id),
                    },
                )
            })
            .collect();
        rows.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        rows.into_iter().map(|(_, it)| it).collect()
    }

    fn connect_profile_views(&self) -> Vec<ConnectProfileView> {
        let mut rows: Vec<(u64, ConnectProfileView)> = self
            .config
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.host.trim().is_empty())
            .map(|(index, p)| {
                let detail = profile_target(p);
                let label = p
                    .name
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(detail.as_str())
                    .to_string();
                (
                    p.last_connected.unwrap_or(0),
                    ConnectProfileView {
                        index,
                        label,
                        detail,
                        use_potty_session: p.use_potty_session,
                    },
                )
            })
            .collect();
        rows.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
        rows.into_iter().map(|(_, row)| row).collect()
    }

    fn connect_progress_views(&self) -> Vec<ConnectProgressView> {
        let mut rows: Vec<&ConnectProgress> = self.connect_progress.values().collect();
        rows.sort_by_key(|p| p.started);
        rows.into_iter()
            .map(|p| ConnectProgressView {
                label: p.label.clone(),
                detail: p.detail.clone(),
                elapsed: p.started.elapsed(),
            })
            .collect()
    }

    fn set_connect_progress(&mut self, conn: ConnId, detail: impl Into<String>) {
        if let Some(progress) = self.connect_progress.get_mut(&conn) {
            progress.detail = detail.into();
            self.request_redraw();
        }
    }

    fn use_connect_profile(&mut self, index: usize) {
        let Some(profile) = self.config.profiles.get(index) else {
            return;
        };
        self.connect_host = profile_target(profile);
        self.connect_name = profile.name.clone().unwrap_or_default();
        self.connect_use_session = profile.use_potty_session;
        self.request_redraw();
    }

    fn remember_connection_profile(&mut self, conn: ConnId) {
        let Some(c) = self.connections.get(&conn) else {
            return;
        };
        if c.remembered {
            return;
        }
        let target = c.target.clone();
        let name = c.display_name.clone();
        let use_potty_session = !c.ephemeral;
        let now = unix_secs();

        if let Some(profile) = self
            .config
            .profiles
            .iter_mut()
            .find(|p| p.user == target.user && p.host == target.host && p.port == target.port)
        {
            if name.is_some() {
                profile.name = name;
            }
            profile.use_potty_session = use_potty_session;
            profile.last_connected = Some(now);
        } else {
            self.config.profiles.push(ConnectionProfile {
                name,
                user: target.user,
                host: target.host,
                port: target.port,
                use_potty_session,
                env: Default::default(),
                last_connected: Some(now),
            });
        }
        self.config
            .profiles
            .sort_by_key(|p| std::cmp::Reverse(p.last_connected.unwrap_or(0)));
        self.config.profiles.truncate(32);
        self.config.save(&self.config_path);
        if let Some(c) = self.connections.get_mut(&conn) {
            c.remembered = true;
        }
    }

    fn remote_env_for(&self, target: &RemoteTarget) -> std::collections::BTreeMap<String, String> {
        self.config
            .profiles
            .iter()
            .find(|p| p.user == target.user && p.host == target.host && p.port == target.port)
            .map(|p| p.env.clone())
            .unwrap_or_default()
    }

    /// Activate a feed row clicked in this process. If the owner is another live potty instance,
    /// ask it to focus its own pane/window; if the owner socket is stale, clear the mirrored row.
    fn activate_note(&mut self, key: FeedKey, pane: Option<PaneId>) {
        if let Some(owner) = key.instance.as_deref()
            && owner != self.instance_id
        {
            let sent = self.send_instance_message_to(
                owner,
                &feed::InstanceMessage::Focus {
                    instance: key.instance.clone(),
                    host: key.host.clone(),
                    session: key.session.clone(),
                    pane,
                },
            );
            if !sent {
                self.dismiss_note(key, true);
            }
            return;
        }
        self.activate_local_note(key, pane, true);
    }

    /// Focus a local pane/window for a feed row. If the pane no longer exists, the row is still
    /// cleared; otherwise stale pane ids become impossible to remove by clicking the card.
    fn activate_local_note(&mut self, key: FeedKey, pane: Option<PaneId>, broadcast: bool) {
        if let Some(pane) = pane
            && let Some(i) = self.workspace.tab_of(pane)
        {
            self.workspace.active = i;
            self.workspace.tabs[i].focus = pane;
        }
        self.raise_window();
        self.dismiss_note(key, broadcast);
        self.request_redraw();
    }

    fn on_peer_focus(
        &mut self,
        instance: Option<String>,
        host: String,
        session: String,
        pane: Option<PaneId>,
    ) {
        let key = FeedKey {
            instance,
            host,
            session,
        };
        let pane = pane.or_else(|| self.pending.get(&key).and_then(|pending| pending.pane));
        self.activate_local_note(key, pane, true);
    }

    fn on_peer_dismiss(&mut self, instance: Option<String>, host: String, session: String) {
        self.dismiss_note(
            FeedKey {
                instance,
                host,
                session,
            },
            false,
        );
    }

    fn raise_window(&self) {
        if let Some(state) = self.state.as_ref() {
            state.window.set_minimized(false);
            state.window.focus_window();
            state
                .window
                .request_user_attention(Some(UserAttentionType::Informational));
        }
    }

    fn broadcast_dismiss(&self, key: &FeedKey) {
        self.broadcast_instance_message(&feed::InstanceMessage::Dismiss {
            instance: key.instance.clone(),
            host: key.host.clone(),
            session: key.session.clone(),
        });
    }

    fn broadcast_instance_message(&self, msg: &feed::InstanceMessage) {
        #[cfg(unix)]
        {
            for path in feed::instance_socket_paths() {
                if self.notify_paths.iter().any(|owned| owned == &path) {
                    continue;
                }
                if send_instance_message(&path, msg).is_err() {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = msg;
    }

    fn send_instance_message_to(&self, instance: &str, msg: &feed::InstanceMessage) -> bool {
        #[cfg(unix)]
        {
            let path = feed::instance_socket_path(instance);
            let sent = send_instance_message(&path, msg).is_ok();
            if !sent {
                let _ = std::fs::remove_file(path);
            }
            sent
        }
        #[cfg(not(unix))]
        {
            let _ = (instance, msg);
            false
        }
    }

    /// Flag a pane's render as stale, and ask for a redraw if that pane is on screen.
    fn touch(&mut self, id: PaneId) {
        self.dirty.insert(id);
        if self.visible.contains(&id) {
            self.request_redraw();
        }
    }

    /// Physical line height for a logical point size.
    fn line_px(&self, size: f32) -> f32 {
        size * 1.2 * self.scale
    }

    /// The focused pane id (the keyboard target).
    fn focus(&self) -> PaneId {
        self.workspace.active_tab().focus
    }

    /// A cloned handle to a pane's grid (cloning the Arc, so callers don't borrow `self.terms`).
    fn arc(&self, id: PaneId) -> Option<SharedTerm> {
        self.terms.get(&id).map(|t| t.term.clone())
    }

    fn focused_arc(&self) -> Option<SharedTerm> {
        self.arc(self.focus())
    }

    fn pane_cwd(&self, id: PaneId) -> Option<PathBuf> {
        match self.terms.get(&id).map(|t| &t.backend) {
            Some(Backend::Local {
                master, child_pid, ..
            }) => pty_cwd(master.as_ref(), *child_pid),
            _ => None,
        }
    }

    /// Spawn a PTY + Term + reader thread for a pane, sized at `dims`.
    fn spawn_terminal(&mut self, id: PaneId, dims: Dims) {
        let cwd = self.pending_cwds.remove(&id).filter(|p| valid_cwd(p));
        let term: SharedTerm = Arc::new(Mutex::new(Term::new(
            term_config(&self.config),
            &dims,
            EventProxy {
                proxy: self.proxy.clone(),
                pane: id,
            },
        )));

        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(portable_pty::PtySize {
                rows: dims.rows as u16,
                cols: dims.cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let shell = default_shell(&self.config);
        // Default title until the program sets one: the shell's basename (e.g. "zsh").
        let default_title = std::path::Path::new(&shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("shell")
            .to_string();
        let mut cmd = portable_pty::CommandBuilder::new(shell);
        // Declare what we actually emulate so terminfo-driven apps (mc, ncurses) agree with
        // the escape sequences we send (e.g. application cursor keys).
        cmd.env("TERM", "xterm-256color");
        // Attention feed: tell child tools where to send notes (`potty-notify` connects here) and
        // which pane they live in (for exact jump-to-focus). Unix-only — the listener is a UDS.
        #[cfg(unix)]
        {
            cmd.env(feed::ENV_SOCK, &self.notify_sock);
            cmd.env(feed::ENV_PANE, id.to_string());
            cmd.env(feed::ENV_INSTANCE, &self.instance_id);
        }
        if let Some(cwd) = &cwd {
            cmd.cwd(cwd.as_os_str());
            cmd.env("PWD", cwd.as_os_str());
        }
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        let child_pid = child.process_id();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();

        let reader_term = term.clone();
        let proxy = self.proxy.clone();
        let wake_pending = Arc::new(AtomicBool::new(false));
        let reader_wake = wake_pending.clone();
        thread::spawn(move || {
            let mut parser = Processor::<StdSyncHandler>::new();
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        {
                            let mut t = reader_term.lock().unwrap();
                            parser.advance(&mut *t, &buf[..n]);
                        }
                        // Only wake the main loop if it hasn't an unhandled wake already —
                        // a flooding program (e.g. `yes`) thus can't spam it one event per read.
                        if !reader_wake.swap(true, Ordering::AcqRel) {
                            let _ = proxy.send_event(UserEvent::Wake(id));
                        }
                    }
                }
            }
        });

        // Close the pane when the shell exits. We wait on the child process rather than the
        // reader's EOF: on Windows ConPTY often keeps the output pipe open after the child
        // exits, so the reader never sees EOF — but the process handle still signals. (On unix
        // this fires at the same time as the reader EOF; PaneExited is idempotent.)
        let exit_proxy = self.proxy.clone();
        thread::spawn(move || {
            let _ = child.wait();
            let _ = exit_proxy.send_event(UserEvent::PaneExited(id));
        });

        self.terms.insert(
            id,
            Terminal {
                term,
                backend: Backend::Local {
                    writer,
                    master: pair.master,
                    child_pid,
                },
                dims,
                title: default_title.clone(),
                default_title,
                wake_pending,
            },
        );
        self.dirty.insert(id); // never rendered yet → prepare on first sight
    }

    /// Keep the live terminals in lock-step with the pane tree: spawn one for every leaf that
    /// lacks a terminal, and drop terminals for panes that no longer exist (closing their PTY,
    /// which ends the reader thread). Called after any action that mutates the tree.
    fn reconcile_terms(&mut self) {
        let live = self.workspace.all_leaves();
        for id in &live {
            // Remote panes get their Terminal at connect time — never spawn a local PTY for them.
            if !self.terms.contains_key(id) && !self.remote_panes.contains(id) {
                self.spawn_terminal(*id, self.last_dims);
            }
        }
        // A restore pane has a Terminal and route as soon as `Restore` arrives, but is absent from
        // the workspace until `Ready` — and a live-adopted pane (another client opened it) until
        // the owner's next `LayoutTree` places it. Keep both alive across redraws; otherwise
        // reconciliation sends `Close`, kills the remote process, and drops the whole connection.
        let restoring: std::collections::HashSet<PaneId> = self
            .connections
            .values()
            .flat_map(|c| {
                let pre_ready = (!c.ready).then_some(&c.restore_panes);
                pre_ready
                    .into_iter()
                    .flatten()
                    .map(|(_, local)| *local)
                    .chain(c.adopting.iter().copied())
            })
            .collect();
        let removed = stale_terminal_ids(self.terms.keys().copied(), &live, &restoring);
        for id in removed {
            // A remote pane closed via the UI: tell the remote to kill its shell.
            if let Some(Backend::Remote {
                outbound,
                remote_id,
                ..
            }) = self.terms.get(&id).map(|t| &t.backend)
            {
                let _ = outbound.try_send(Frame::Control(Control::Close { pane: *remote_id }));
            }
            self.drop_remote_route(id);
            self.terms.remove(&id);
            self.remote_panes.remove(&id);
            self.pending_cwds.remove(&id);
            self.dirty.remove(&id);
            self.last_rect.remove(&id);
            if let Some(state) = self.state.as_mut() {
                state.grid.forget_pane(id);
            }
        }
    }

    /// Remove a pane (its local shell exited, its remote pane reported `Exited`, or the UI closed
    /// it). Exits the app once no panes remain.
    fn close_pane(&mut self, event_loop: &ActiveEventLoop, id: PaneId) {
        self.drop_remote_route(id);
        self.terms.remove(&id);
        self.remote_panes.remove(&id);
        self.pending_cwds.remove(&id);
        self.dirty.remove(&id);
        if let Some(state) = self.state.as_mut() {
            state.grid.forget_pane(id);
        }
        self.workspace.remove_pane(id);
        if self.terms.is_empty() {
            event_loop.exit();
        } else {
            self.request_redraw();
        }
    }

    /// Detach the focused pane's remote session, if it is one: drop its tabs/panes locally and
    /// disconnect, but leave the daemon's shells running so the session can be reattached later.
    fn detach_focused_session(&mut self) {
        let focus = self.workspace.active_tab().focus;
        if let Some(Backend::Remote { conn, .. }) = self.terms.get(&focus).map(|t| &t.backend) {
            let conn = *conn;
            self.detach_connection(conn);
        }
    }

    /// Tear down connection `conn` locally *without* killing its remote shells. Unlike closing
    /// panes, we remove each pane's `Terminal` directly (so `reconcile_terms` sends no `Close`) and
    /// drop the `Connection`. With every outbound `Sender` gone, the writer signals channel EOF and
    /// the daemon detaches with its panes intact — ready to reattach. Keeps potty alive with a
    /// fresh local tab if this emptied the workspace.
    fn detach_connection(&mut self, conn: ConnId) {
        let locals: Vec<PaneId> = self
            .connections
            .get(&conn)
            .map(|c| c.routes.values().copied().collect())
            .unwrap_or_default();
        for local in locals {
            self.terms.remove(&local);
            self.remote_panes.remove(&local);
            self.pending_cwds.remove(&local);
            self.dirty.remove(&local);
            self.last_rect.remove(&local);
            if let Some(state) = self.state.as_mut() {
                state.grid.forget_pane(local);
            }
            self.workspace.remove_pane(local);
        }
        self.connections.remove(&conn); // last Sender drops → writer EOFs → daemon keeps its panes
        self.connect_progress.remove(&conn);
        if self.workspace.tabs.is_empty() {
            self.workspace.new_tab();
        }
        self.request_redraw();
    }

    fn focus_connection(&mut self, conn: ConnId) -> bool {
        let locals: Vec<PaneId> = self
            .connections
            .get(&conn)
            .map(|c| c.routes.values().copied().collect())
            .unwrap_or_default();
        for local in locals {
            if let Some(tab) = self.workspace.tab_of(local) {
                self.workspace.active = tab;
                self.workspace.tabs[tab].focus = local;
                self.request_redraw();
                return true;
            }
        }
        false
    }

    fn focus_existing_persistent_target(&mut self, target: &RemoteTarget) -> bool {
        if self
            .connect_progress
            .values()
            .any(|progress| &progress.target == target)
        {
            self.error_msg = Some(format!("Already connecting to {}.", target.label()));
            self.request_redraw();
            return true;
        }
        let conn = self
            .connections
            .iter()
            .find_map(|(conn, c)| (!c.ephemeral && &c.target == target).then_some(*conn));
        let Some(conn) = conn else {
            return false;
        };
        if !self.focus_connection(conn) {
            self.error_msg = Some(format!("Already connecting to {}.", target.label()));
            self.request_redraw();
        }
        true
    }

    /// Spawn the russh client for `cfg` on its own thread (a current-thread tokio runtime). On
    /// success it forwards the remote's frames back to this loop as `UserEvent`s; on failure it
    /// reports `RemoteError`. Per-connection threads mean an auth prompt can block *this*
    /// connection (while the UI keeps rendering the dialog) without stalling anything else.
    /// `use_session` → exec `potty-session` (persistent multiplexing); otherwise a plain SSH shell
    /// (`shell_session`, no persistence). Both speak the same protocol back, so the rest is shared.
    fn connect_remote(
        &mut self,
        cfg: remote::SshConfig,
        auth: Arc<dyn remote::Authenticator>,
        command: String,
        display_name: Option<String>,
        use_session: bool,
    ) {
        let conn = self.next_conn_id;
        self.next_conn_id += 1;
        let target = RemoteTarget {
            user: cfg.user.clone(),
            host: cfg.host.clone(),
            port: cfg.port,
            command: command.clone(),
        };
        let target_label = target.label();
        self.connect_progress.insert(
            conn,
            ConnectProgress {
                target: target.clone(),
                label: display_name.clone().unwrap_or_else(|| target_label.clone()),
                detail: format!("Connecting to {target_label} over SSH"),
                started: Instant::now(),
            },
        );
        self.request_redraw();
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = proxy.send_event(UserEvent::RemoteError {
                        conn: Some(conn),
                        msg: format!("could not start SSH runtime: {e}"),
                    });
                    return;
                }
            };
            rt.block_on(async move {
                remote::sshdbg(format!(
                    "gui: connecting to {}:{} (potty-session: {use_session})",
                    cfg.host, cfg.port
                ));
                let result = if use_session {
                    remote::connect_and_exec(&cfg, auth, &command).await
                } else {
                    remote::shell_session(&cfg, auth).await
                };
                match result {
                    Ok((session, outbound, mut rx)) => {
                        remote::sshdbg("gui: transport ready — handing to UI thread");
                        // Hand the sole outbound `Sender` to the UI thread. Once it (and the per-pane
                        // clones) drop — the connection's last pane closed — the writer signals EOF
                        // and the remote tears down; this loop then ends as the channel closes.
                        let connected = UserEvent::RemoteConnected {
                            conn,
                            target: target.clone(),
                            display_name: display_name.clone(),
                            outbound,
                            ephemeral: !use_session,
                        };
                        if proxy.send_event(connected).is_err() {
                            return;
                        }
                        // Track whether the remote ever greeted us. If the channel closes before a
                        // `Welcome`, the remote command never ran the protocol — almost always
                        // `potty-session` missing on the host — so surface that instead of silently
                        // leaving a dead, paneless connection.
                        let mut greeted = false;
                        while let Some(frame) = rx.recv().await {
                            greeted |= matches!(frame, Frame::Control(Control::Welcome { .. }));
                            if matches!(frame, Frame::Control(_)) {
                                remote::sshdbg(format!("gui: recv {frame:?}"));
                            }
                            if proxy.send_event(UserEvent::RemoteFrame(conn, frame)).is_err() {
                                break;
                            }
                        }
                        remote::sshdbg(format!(
                            "gui: frame stream ended (greeted: {greeted})"
                        ));
                        if !greeted {
                            let detail = session.stderr();
                            let msg = if detail.is_empty() {
                                format!(
                                    "{}: the session ended before it started — is `potty-session` \
                                     installed and on PATH on the host? (set `remote_command` in the \
                                     config to its path if so)",
                                    cfg.host
                                )
                            } else {
                                format!("{}: could not start the remote session — {detail}", cfg.host)
                            };
                            let _ = proxy.send_event(UserEvent::RemoteError { conn: Some(conn), msg });
                        } else {
                            let _ = proxy.send_event(UserEvent::RemoteDisconnected {
                                conn,
                                msg: format!("{}: remote session disconnected", cfg.host),
                            });
                        }
                        drop(session); // keep the SSH session alive until the stream ends
                    }
                    Err(e) => {
                        remote::sshdbg(format!("gui: connect failed: {e}"));
                        let _ = proxy.send_event(UserEvent::RemoteError {
                            conn: Some(conn),
                            msg: e.to_string(),
                        });
                    }
                }
            });
        });
    }

    /// A connection authenticated: register it and greet the daemon. We then wait for its restore
    /// burst — `Ready` decides whether to open a fresh pane (see `on_remote_frame`).
    fn on_remote_connected(
        &mut self,
        conn: ConnId,
        target: RemoteTarget,
        display_name: Option<String>,
        outbound: tokio::sync::mpsc::Sender<Frame>,
        ephemeral: bool,
    ) {
        remote::sshdbg(format!(
            "gui: RemoteConnected (ephemeral: {ephemeral}) — sending Hello"
        ));
        let _ = outbound.try_send(Frame::Control(Control::Hello {
            version: proto::PROTOCOL_VERSION,
        }));
        self.set_connect_progress(
            conn,
            if ephemeral {
                "Opening remote shell"
            } else {
                "Restoring potty-session"
            },
        );
        self.connections.insert(
            conn,
            Connection {
                target,
                display_name,
                outbound,
                next_remote_id: 1,
                routes: HashMap::new(),
                ephemeral,
                ready: false,
                client_id: 0,
                focus_owner: 0,
                adopting: std::collections::HashSet::new(),
                restore_panes: Vec::new(),
                restore_layout: None,
                pushed_layout: None,
                remembered: false,
            },
        );
    }

    /// Wire a local pane `local` to `conn`'s daemon pane `remote_id`. `open` → the daemon doesn't
    /// have it yet (send `Open`); otherwise we're adopting a restored pane.
    fn wire_remote_pane(
        &mut self,
        conn: ConnId,
        remote_id: u64,
        local: PaneId,
        open: bool,
        cwd_from: Option<u64>,
    ) {
        remote::sshdbg(format!(
            "gui: wiring pane local {local} ↔ remote {remote_id} (open: {open})"
        ));
        let (label, outbound) = match self.connections.get_mut(&conn) {
            Some(c) => {
                c.routes.insert(remote_id, local);
                c.next_remote_id = c.next_remote_id.max(remote_id + 1);
                (
                    c.display_name
                        .clone()
                        .unwrap_or_else(|| c.target.host.clone()),
                    c.outbound.clone(),
                )
            }
            None => return,
        };
        let dims = self.last_dims;
        let term: SharedTerm = Arc::new(Mutex::new(Term::new(
            term_config(&self.config),
            &dims,
            EventProxy {
                proxy: self.proxy.clone(),
                pane: local,
            },
        )));
        self.terms.insert(
            local,
            Terminal {
                term,
                backend: Backend::Remote {
                    conn,
                    remote_id,
                    label,
                    outbound: outbound.clone(),
                    parser: Box::new(Processor::new()),
                },
                dims,
                title: "shell".into(),
                default_title: "shell".into(),
                wake_pending: Arc::new(AtomicBool::new(false)),
            },
        );
        self.remote_panes.insert(local);
        if open {
            let _ = outbound.try_send(Frame::Control(Control::Open {
                pane: remote_id,
                cols: dims.cols as u16,
                rows: dims.rows as u16,
                cwd_from,
            }));
        }
        self.dirty.insert(local);
    }

    /// Open a new pane on `conn` (allocating a fresh daemon id) at local pane `local`.
    fn add_remote_pane(&mut self, conn: ConnId, local: PaneId, cwd_from: Option<u64>) {
        let Some(remote_id) = self.connections.get(&conn).map(|c| c.next_remote_id) else {
            return;
        };
        self.wire_remote_pane(conn, remote_id, local, true, cwd_from);
    }

    /// The connection of the focused pane, if it's remote — so a split or new tab reuses it.
    fn focused_conn(&self) -> Option<ConnId> {
        match self.terms.get(&self.focus()).map(|t| &t.backend) {
            Some(Backend::Remote { conn, .. }) => Some(*conn),
            _ => None,
        }
    }

    /// Split the focused pane. From a remote pane, the new pane is another shell on the *same*
    /// connection; from a local pane, a normal local split (reconcile spawns its PTY).
    fn split_pane(&mut self, split: Split) {
        let focus = self.focus();
        let conn = self.focused_conn();
        let cwd = conn.is_none().then(|| self.pane_cwd(focus)).flatten();
        let cwd_from = conn.and_then(|_| self.remote_id_of(focus));
        self.workspace.split(split);
        let new_id = self.workspace.active_tab().focus;
        if let Some(conn) = conn {
            self.add_remote_pane(conn, new_id, cwd_from);
        } else if let Some(cwd) = cwd {
            self.pending_cwds.insert(new_id, cwd);
        }
    }

    /// New tab. From a remote pane, it's a new tab on the same connection — unless `local` forces
    /// a local shell (the tab bar's "+"); otherwise a local tab inheriting the focused pane's cwd.
    fn new_tab(&mut self, local: bool) {
        let focus = self.focus();
        let conn = if local { None } else { self.focused_conn() };
        let cwd = conn.is_none().then(|| self.pane_cwd(focus)).flatten();
        let cwd_from = conn.and_then(|_| self.remote_id_of(focus));
        self.workspace.new_tab();
        let new_id = self.workspace.active_tab().focus;
        if let Some(conn) = conn {
            self.add_remote_pane(conn, new_id, cwd_from);
        } else if let Some(cwd) = cwd {
            self.pending_cwds.insert(new_id, cwd);
        }
    }

    /// Remove a remote pane's route (and the whole connection once it has no panes left, which
    /// drops its `outbound` and lets the daemon detach). No-op for local panes.
    fn drop_remote_route(&mut self, id: PaneId) {
        let Some(Backend::Remote {
            conn, remote_id, ..
        }) = self.terms.get(&id).map(|t| &t.backend)
        else {
            return;
        };
        let (conn, remote_id) = (*conn, *remote_id);
        if let Some(c) = self.connections.get_mut(&conn) {
            c.routes.remove(&remote_id);
            if c.routes.is_empty() {
                self.connections.remove(&conn);
            }
        }
    }

    /// Feed a frame from connection `conn` into the owning pane, or handle the reattach handshake.
    fn on_remote_frame(&mut self, event_loop: &ActiveEventLoop, conn: ConnId, frame: Frame) {
        match frame {
            Frame::Data {
                pane: remote_id,
                bytes,
            } => {
                let local = self
                    .connections
                    .get(&conn)
                    .and_then(|c| c.routes.get(&remote_id))
                    .copied();
                if let Some(local) = local {
                    if let Some(t) = self.terms.get_mut(&local)
                        && let Backend::Remote { parser, .. } = &mut t.backend
                    {
                        let mut term = t.term.lock().unwrap();
                        parser.advance(&mut *term, &bytes);
                    }
                    self.touch(local);
                }
            }
            // Adopt an existing daemon pane (its screen replay follows as Data frames). We create
            // its backend now; placement comes from the layout — at `Ready` during the attach
            // restore burst, or from the owner's next `LayoutTree` push for a live adoption
            // (another attached client opened a pane).
            Frame::Control(Control::Restore { pane: remote_id }) => {
                let ready = self.connections.get(&conn).is_some_and(|c| c.ready);
                let local = self.workspace.alloc_pane();
                self.wire_remote_pane(conn, remote_id, local, false, None);
                if let Some(c) = self.connections.get_mut(&conn) {
                    if ready {
                        c.adopting.insert(local);
                    } else {
                        c.restore_panes.push((remote_id, local));
                    }
                }
            }
            // The daemon's layout — stashed and applied at `Ready` during the restore burst, or
            // (once ready) mirrored immediately: another attached client reshaped the session.
            Frame::Control(Control::LayoutTree { json }) => {
                if self.connections.get(&conn).is_some_and(|c| c.ready) {
                    self.apply_remote_layout(conn, &json);
                } else if let Ok(layout) = serde_json::from_str::<proto::Layout>(&json)
                    && let Some(c) = self.connections.get_mut(&conn)
                {
                    c.restore_layout = Some(layout);
                }
            }
            // Our daemon-assigned client id.
            Frame::Control(Control::Welcome { client, .. }) => {
                if let Some(c) = self.connections.get_mut(&conn) {
                    c.client_id = client;
                }
            }
            // Focus moved (input from some client). If it came to us, the next redraw re-fits our
            // panes to our own geometry; if it left, we stop resizing and follow instead.
            Frame::Control(Control::Focus { owner }) => {
                if let Some(c) = self.connections.get_mut(&conn) {
                    c.focus_owner = owner;
                }
                self.request_redraw();
            }
            // The focused client resized a pane; conform our grid to the PTY (we're a follower —
            // our own `fit_terminal` is suspended while someone else holds focus).
            Frame::Control(Control::Resize {
                pane: remote_id,
                cols,
                rows,
            }) => {
                let local = self
                    .connections
                    .get(&conn)
                    .and_then(|c| c.routes.get(&remote_id))
                    .copied();
                if let Some(local) = local
                    && let Some(t) = self.terms.get_mut(&local)
                {
                    let dims = Dims {
                        cols: (cols as usize).max(1),
                        rows: (rows as usize).max(1),
                    };
                    if t.dims != dims {
                        t.dims = dims;
                        t.term.lock().unwrap().resize(dims);
                        self.dirty.insert(local);
                        self.request_redraw();
                    }
                }
            }
            // Restore burst done: place the restored panes (by layout), or open a fresh pane.
            Frame::Control(Control::Ready) => {
                remote::sshdbg("gui: Ready — placing panes / opening a fresh one");
                self.finish_restore(conn);
                if let Some(c) = self.connections.get_mut(&conn) {
                    c.ready = true;
                }
                self.connect_progress.remove(&conn);
                self.remember_connection_profile(conn);
                self.ensure_remote_connection_has_tab(conn);
                self.request_redraw();
            }
            Frame::Control(Control::Exited { pane: remote_id }) => {
                let local = self
                    .connections
                    .get(&conn)
                    .and_then(|c| c.routes.get(&remote_id))
                    .copied();
                remote::sshdbg(format!(
                    "gui: remote pane {remote_id} exited — closing local pane {local:?}"
                ));
                if let Some(local) = local {
                    self.close_pane(event_loop, local);
                }
            }
            Frame::Control(Control::Notify { json }) => {
                if let Ok(mut note) = serde_json::from_str::<feed::Note>(&json)
                    && note.v == feed::SCHEMA_VERSION
                {
                    if note.host.is_empty()
                        && let Some(host) =
                            self.connections.get(&conn).map(|c| c.target.host.clone())
                    {
                        note.host = host;
                    }
                    note.pane = note.pane.and_then(|remote_id| {
                        self.connections
                            .get(&conn)
                            .and_then(|c| c.routes.get(&remote_id).copied())
                    });
                    note.instance = Some(self.instance_id.clone());
                    if self.conn_focused(conn) {
                        self.on_local_note(note);
                    } else {
                        self.apply_note(note);
                    }
                }
            }
            // Welcome / Opened: nothing to do.
            Frame::Control(_) => {}
        }
    }

    /// Place the panes adopted during the restore burst into tabs: rebuild the original tree from
    /// the replayed layout, then give any leftover (un-laid-out) pane its own tab. A fresh/empty
    /// session (no panes) is handled by `ensure_remote_connection_has_tab` after Ready.
    fn finish_restore(&mut self, conn: ConnId) {
        let (panes, layout) = match self.connections.get_mut(&conn) {
            Some(c) => (
                std::mem::take(&mut c.restore_panes),
                c.restore_layout.take(),
            ),
            None => return,
        };
        if panes.is_empty() {
            return;
        }
        let label = self
            .connections
            .get(&conn)
            .map(|c| {
                c.display_name
                    .clone()
                    .unwrap_or_else(|| c.target.host.clone())
            })
            .unwrap_or_default();
        let map: HashMap<u64, PaneId> = panes.iter().copied().collect();
        let mut placed: std::collections::HashSet<PaneId> = std::collections::HashSet::new();
        if let Some(layout) = layout {
            for ltab in &layout.tabs {
                if let Some(node) = self.build_node(&ltab.root, &map, &mut placed) {
                    let focus = ltab
                        .focus
                        .and_then(|r| map.get(&r).copied())
                        .unwrap_or_else(|| node.first_leaf());
                    self.workspace.push_tab(label.clone(), node, focus);
                }
            }
        }
        // Any restored pane the layout didn't cover (stale/missing layout) → its own tab.
        for (_remote, local) in &panes {
            if !placed.contains(local) {
                self.workspace
                    .push_tab(label.clone(), workspace::Node::Leaf(*local), *local);
            }
        }
    }

    /// Once the handshake reaches Ready, the connection must have at least one local tab. For
    /// persistent remotes this covers a genuinely fresh daemon, an empty daemon after all panes
    /// died while detached, and stale/empty replayed layouts. For ephemeral (plain-shell)
    /// connections it opens the one and only pane — they always arrive at Ready with nothing
    /// restored, so excluding them here (as 97c8847 did) means the connect finishes into thin
    /// air: no pane, no tab, no error.
    fn ensure_remote_connection_has_tab(&mut self, conn: ConnId) {
        let Some(c) = self.connections.get(&conn) else {
            return;
        };
        if !c.ready {
            return;
        }
        let owns_tab = self.workspace.tabs.iter().any(|tab| {
            matches!(
                self.terms.get(&tab.layout.first_leaf()).map(|t| &t.backend),
                Some(Backend::Remote { conn: c, .. }) if *c == conn
            )
        });
        if owns_tab {
            return;
        }
        self.workspace.new_tab();
        let local = self.workspace.active_tab().focus;
        self.add_remote_pane(conn, local, None);
    }

    /// Rebuild a workspace `Node` from a replayed layout node, mapping daemon pane ids to the local
    /// ids created during restore. Missing leaves (a pane that died while detached) collapse to
    /// their surviving sibling. Records which locals it placed.
    fn build_node(
        &mut self,
        ln: &proto::LayoutNode,
        map: &HashMap<u64, PaneId>,
        placed: &mut std::collections::HashSet<PaneId>,
    ) -> Option<workspace::Node> {
        match ln {
            proto::LayoutNode::Leaf { pane } => map.get(pane).map(|&local| {
                placed.insert(local);
                workspace::Node::Leaf(local)
            }),
            proto::LayoutNode::Split { cols, ratio, a, b } => {
                let a = self.build_node(a, map, placed);
                let b = self.build_node(b, map, placed);
                match (a, b) {
                    (Some(a), Some(b)) => Some(workspace::Node::Branch {
                        id: self.workspace.alloc_pane(),
                        split: if *cols { Split::Cols } else { Split::Rows },
                        ratio: *ratio,
                        a: Box::new(a),
                        b: Box::new(b),
                    }),
                    (Some(n), None) | (None, Some(n)) => Some(n), // collapse to the survivor
                    (None, None) => None,
                }
            }
        }
    }

    /// True when we may drive `conn`'s layout and pane sizes: we hold the daemon's focus, or the
    /// daemon never said otherwise (plain shells; v1 daemons — both ids stay 0). Focus follows
    /// input, so typing into the session is what makes this true again.
    fn conn_focused(&self, conn: ConnId) -> bool {
        self.connections
            .get(&conn)
            .is_none_or(|c| c.client_id == c.focus_owner)
    }

    /// Mirror a layout change made by another attached client (the focus owner): rebuild this
    /// connection's tabs in place, placing freshly-adopted panes and locally dropping panes the
    /// layout no longer references. The owner already closed those shells, so nothing is sent
    /// back (a `Close` would also steal the focus).
    fn apply_remote_layout(&mut self, conn: ConnId, json: &str) {
        let Ok(layout) = serde_json::from_str::<proto::Layout>(json) else {
            return;
        };
        // Already in sync (e.g. an `Exited` reshaped us the same way): just record the push so
        // `sync_layouts` won't echo it back once we regain focus.
        let current = serde_json::to_string(&self.layout_for(conn)).unwrap_or_default();
        if current == json {
            if let Some(c) = self.connections.get_mut(&conn) {
                c.pushed_layout = Some(json.to_string());
            }
            return;
        }
        let (map, label) = match self.connections.get(&conn) {
            Some(c) => (
                c.routes.clone(),
                c.display_name
                    .clone()
                    .unwrap_or_else(|| c.target.host.clone()),
            ),
            None => return,
        };
        // Where the user is looking, to put them back after the rebuild.
        let focus_before = self.workspace.active_tab().focus;

        // Take this connection's tabs out, remembering where they started so the rebuilt set
        // lands in the same spot in the tab bar.
        let mine: Vec<usize> = self
            .workspace
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| {
                matches!(
                    self.terms.get(&tab.layout.first_leaf()).map(|t| &t.backend),
                    Some(Backend::Remote { conn: c, .. }) if *c == conn
                )
            })
            .map(|(i, _)| i)
            .collect();
        let insert_at = mine.first().copied().unwrap_or(self.workspace.tabs.len());
        for &i in mine.iter().rev() {
            self.workspace.tabs.remove(i);
        }
        let mut placed: std::collections::HashSet<PaneId> = std::collections::HashSet::new();
        let mut rebuilt: Vec<workspace::Tab> = Vec::new();
        for ltab in &layout.tabs {
            if let Some(node) = self.build_node(&ltab.root, &map, &mut placed) {
                let focus = ltab
                    .focus
                    .and_then(|r| map.get(&r).copied())
                    .unwrap_or_else(|| node.first_leaf());
                rebuilt.push(workspace::Tab {
                    title: label.clone(),
                    layout: node,
                    focus,
                });
            }
        }
        self.workspace.tabs.splice(insert_at..insert_at, rebuilt);

        // Panes the new layout doesn't reference were closed by the owner — drop them locally.
        // Panes still awaiting placement (`adopting`) survive until a layout mentions them.
        let adopting = self
            .connections
            .get(&conn)
            .map(|c| c.adopting.clone())
            .unwrap_or_default();
        for (remote_id, local) in &map {
            if placed.contains(local) || adopting.contains(local) {
                continue;
            }
            self.terms.remove(local);
            self.remote_panes.remove(local);
            self.pending_cwds.remove(local);
            self.dirty.remove(local);
            self.last_rect.remove(local);
            if let Some(state) = self.state.as_mut() {
                state.grid.forget_pane(*local);
            }
            if let Some(c) = self.connections.get_mut(&conn) {
                c.routes.remove(remote_id);
            }
        }
        let mut drop_conn = false;
        if let Some(c) = self.connections.get_mut(&conn) {
            c.adopting.retain(|p| !placed.contains(p));
            c.pushed_layout = Some(json.to_string());
            // The owner closed every pane: the session emptied under us.
            drop_conn = c.routes.is_empty() && c.adopting.is_empty();
        }
        if drop_conn {
            // Like `drop_remote_route`: dropping the last outbound Sender detaches us.
            self.connections.remove(&conn);
            self.connect_progress.remove(&conn);
        }

        if self.workspace.tabs.is_empty() {
            self.workspace.new_tab();
        }
        if let Some(tab) = self.workspace.tab_of(focus_before) {
            self.workspace.active = tab;
            self.workspace.tabs[tab].focus = focus_before;
        } else {
            self.workspace.active = self
                .workspace
                .active
                .min(self.workspace.tabs.len().saturating_sub(1));
        }
        self.request_redraw();
    }

    /// Push each ready connection's current tab/pane tree to its daemon, so a reattach can restore
    /// it. Deduplicated against the last push. Called after structural changes (see the redraw).
    /// A follower (someone else holds the focus) pushes nothing: it mirrors instead.
    fn sync_layouts(&mut self) {
        let conns: Vec<ConnId> = self.connections.keys().copied().collect();
        for conn in conns {
            if !self
                .connections
                .get(&conn)
                .is_some_and(|c| c.ready && !c.ephemeral && c.client_id == c.focus_owner)
            {
                continue; // mid-restore, ephemeral (no daemon), or following another client
            }
            let json = serde_json::to_string(&self.layout_for(conn)).unwrap_or_default();
            if let Some(c) = self.connections.get_mut(&conn)
                && c.pushed_layout.as_deref() != Some(json.as_str())
            {
                c.pushed_layout = Some(json.clone());
                let _ = c
                    .outbound
                    .try_send(Frame::Control(Control::LayoutTree { json }));
            }
        }
    }

    /// The serializable layout of `conn`'s tabs (those whose panes are remote on this connection).
    fn layout_for(&self, conn: ConnId) -> proto::Layout {
        let mut tabs = Vec::new();
        for tab in &self.workspace.tabs {
            // A tab belongs to `conn` if its first leaf is a remote pane on it (all panes in a tab
            // share a connection).
            let on_conn = matches!(
                self.terms.get(&tab.layout.first_leaf()).map(|t| &t.backend),
                Some(Backend::Remote { conn: c, .. }) if *c == conn
            );
            if !on_conn {
                continue;
            }
            if let Some(root) = self.node_to_layout(&tab.layout) {
                let focus = self.remote_id_of(tab.focus);
                tabs.push(proto::LayoutTab { root, focus });
            }
        }
        proto::Layout { tabs }
    }

    /// Convert a workspace `Node` (local pane ids) into a layout node (daemon pane ids). `None` if a
    /// leaf isn't a remote pane (shouldn't happen within a remote tab).
    fn node_to_layout(&self, node: &workspace::Node) -> Option<proto::LayoutNode> {
        match node {
            workspace::Node::Leaf(local) => self
                .remote_id_of(*local)
                .map(|pane| proto::LayoutNode::Leaf { pane }),
            workspace::Node::Branch {
                split, ratio, a, b, ..
            } => Some(proto::LayoutNode::Split {
                cols: matches!(split, Split::Cols),
                ratio: *ratio,
                a: Box::new(self.node_to_layout(a)?),
                b: Box::new(self.node_to_layout(b)?),
            }),
        }
    }

    /// The daemon pane id a local pane maps to, if it's remote.
    fn remote_id_of(&self, local: PaneId) -> Option<u64> {
        match self.terms.get(&local).map(|t| &t.backend) {
            Some(Backend::Remote { remote_id, .. }) => Some(*remote_id),
            _ => None,
        }
    }

    /// Answer the active host-key prompt, unblocking the connection thread waiting on it.
    fn answer_auth(&mut self, accept: bool) {
        if let Some(AuthPrompt::HostKey { .. }) = self.auth_prompts.first() {
            if let AuthPrompt::HostKey { reply, .. } = self.auth_prompts.remove(0) {
                let _ = reply.send(accept);
            }
            self.request_redraw();
        }
    }

    /// Answer the active text prompt (passphrase/keyboard-interactive/password). `submit` sends the
    /// typed fields; otherwise the method is cancelled.
    fn answer_auth_text(&mut self, submit: bool) {
        if let Some(AuthPrompt::Text { .. }) = self.auth_prompts.first() {
            if let AuthPrompt::Text { reply, .. } = self.auth_prompts.remove(0) {
                let answer = submit.then(|| std::mem::take(&mut self.auth_inputs));
                let _ = reply.send(answer);
            }
            self.auth_inputs.clear();
            self.request_redraw();
        }
    }

    /// Start a connection from the "Connect to host…" dialog input (`[user@]host[:port]`).
    fn start_connect(&mut self, input: &str, name: &str) {
        let (user, host, port) = parse_host(input);
        if host.is_empty() {
            return;
        }
        let command = self.config.remote_command.clone();
        let display_name = clean_profile_name(name);
        let target = RemoteTarget {
            user: user.clone(),
            host: host.clone(),
            port,
            command: command.clone(),
        };
        if self.connect_use_session && self.focus_existing_persistent_target(&target) {
            self.show_connect = false;
            self.connect_host.clear();
            self.connect_name.clear();
            return;
        }
        let cfg = remote::SshConfig {
            host,
            port,
            user,
            env: self.remote_env_for(&target),
            keys: default_keys(),
            known_hosts: None,
            use_agent: true,
            agent_sock: None,
            connect_timeout: remote::DEFAULT_CONNECT_TIMEOUT,
        };
        let auth = Arc::new(GuiAuth::new(self.proxy.clone()));
        self.connect_remote(cfg, auth, command, display_name, self.connect_use_session);
        self.show_connect = false;
        self.connect_host.clear();
        self.connect_name.clear();
    }

    /// Whether an egui text field (connect dialog or a text auth prompt) is capturing keyboard —
    /// if so, keys go to egui rather than the focused pane.
    fn text_input_active(&self) -> bool {
        self.show_connect || matches!(self.auth_prompts.first(), Some(AuthPrompt::Text { .. }))
    }

    fn shortcut_action(&self, ev: &KeyEvent) -> Option<MenuAction> {
        if ev.state != ElementState::Pressed {
            return None;
        }
        let mods = self.mods.state();
        let standard_mod_only = if cfg!(target_os = "macos") {
            mods.super_key() && !mods.control_key()
        } else {
            mods.control_key() && !mods.super_key()
        };
        if !standard_mod_only || mods.alt_key() {
            return None;
        }
        let key = char_key(ev)?;
        SHORTCUTS
            .iter()
            .find(|s| {
                let binding = binding_from_config(&self.config, s.action);
                binding.key.eq_ignore_ascii_case(&key) && binding.shift == mods.shift_key()
            })
            .map(|s| s.action)
    }

    fn capture_binding_from_key(&self, ev: &KeyEvent) -> Option<ShortcutBinding> {
        if ev.state != ElementState::Pressed {
            return None;
        }
        let mods = self.mods.state();
        let standard_mod_only = if cfg!(target_os = "macos") {
            mods.super_key() && !mods.control_key()
        } else {
            mods.control_key() && !mods.super_key()
        };
        if !standard_mod_only || mods.alt_key() {
            return None;
        }
        Some(ShortcutBinding {
            key: char_key(ev)?.to_ascii_uppercase(),
            shift: mods.shift_key(),
        })
    }

    fn set_shortcut(&mut self, action: MenuAction, binding: ShortcutBinding) {
        let id = action_id(action).to_string();
        if binding == default_binding(action) {
            self.config.hotkeys.remove(&id);
        } else {
            self.config.hotkeys.insert(id, shortcut_hint(&binding));
        }
        self.shortcut_capture = None;
        self.config.save(&self.config_path);
        self.request_redraw();
    }

    fn reset_shortcut(&mut self, action: MenuAction) {
        self.config.hotkeys.remove(action_id(action));
        if self.shortcut_capture == Some(action) {
            self.shortcut_capture = None;
        }
        self.config.save(&self.config_path);
        self.request_redraw();
    }

    fn run_shortcut(&mut self, action: MenuAction) {
        match action {
            MenuAction::NewTab => self.new_tab(false),
            MenuAction::SplitRight => self.split_pane(Split::Cols),
            MenuAction::SplitDown => self.split_pane(Split::Rows),
            MenuAction::ClosePane => self.workspace.close_focused(),
            MenuAction::Connect => {
                self.show_connect = true;
            }
            MenuAction::DetachSession => self.detach_focused_session(),
            MenuAction::Settings => {
                self.show_settings = true;
            }
        }
        self.reconcile_terms();
        self.sync_layouts();
        self.request_redraw();
    }

    /// SPIKE SCAFFOLDING: auto-connect to a host from `$POTTY_TEST_*` env on startup, to exercise
    /// the remote path before the `+`/menu connect flow exists. To be removed once that lands.
    fn maybe_test_connect(&mut self) {
        let Ok(host) = std::env::var("POTTY_TEST_HOST") else {
            return;
        };
        let cfg = remote::SshConfig {
            host,
            port: std::env::var("POTTY_TEST_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(22),
            user: std::env::var("POTTY_TEST_USER").unwrap_or_default(),
            env: Default::default(),
            keys: std::env::var("POTTY_TEST_KEY")
                .ok()
                .map(std::path::PathBuf::from)
                .into_iter()
                .collect(),
            known_hosts: std::env::var("POTTY_TEST_KNOWN_HOSTS")
                .ok()
                .map(std::path::PathBuf::from),
            // Off by default for the spike so the test doesn't offer the dev's real agent keys to a
            // throwaway sshd (which can exhaust MaxAuthTries before the test key is tried).
            use_agent: std::env::var("POTTY_TEST_AGENT").is_ok(),
            agent_sock: None,
            connect_timeout: remote::DEFAULT_CONNECT_TIMEOUT,
        };
        let command =
            std::env::var("POTTY_TEST_SESSION_BIN").unwrap_or_else(|_| "potty-session".into());
        self.connect_remote(
            cfg,
            Arc::new(GuiAuth::new(self.proxy.clone())),
            command,
            None,
            true,
        );
    }

    /// Apply a (possibly new) config: repaint the palette always; rebuild the font only when
    /// family/size changed (and then force a refit of every terminal, since the cell box moved).
    fn apply_config(&mut self, new: Config) {
        let font_changed =
            new.font_family != self.config.font_family || new.font_size != self.config.font_size;
        self.config = new;
        // Hot-reload the OSC 52 clipboard policy on every live terminal.
        for t in self.terms.values() {
            t.term
                .lock()
                .unwrap()
                .set_options(term_config(&self.config));
        }
        // Palette / font changes affect every pane's render.
        self.dirty.extend(self.terms.keys().copied());
        let (family, size, scale) = (
            self.config.font_family.clone(),
            self.config.font_size,
            self.scale,
        );
        let palette = self.config.palette();
        let line = self.line_px(size);
        if let Some(state) = self.state.as_mut() {
            state.grid.set_palette(palette);
            if font_changed {
                state.grid.set_font(family, size * scale, line);
                let m = state.grid.metrics();
                self.cell_w = m.w;
                self.cell_h = m.h;
                // Invalidate every terminal's size so the next redraw refits it.
                for t in self.terms.values_mut() {
                    t.dims = Dims { cols: 0, rows: 0 };
                }
            }
            state.window.request_redraw();
        }
    }

    fn set_font_family(&mut self, family: Option<String>) {
        let mut c = self.config.clone();
        c.font_family = family;
        c.save(&self.config_path);
        self.apply_config(c);
    }

    fn set_font_size(&mut self, size: f32) {
        let mut c = self.config.clone();
        c.font_size = size.clamp(6.0, 48.0);
        c.save(&self.config_path);
        self.apply_config(c);
    }

    fn set_colors(&mut self, colors: config::Colors) {
        let mut c = self.config.clone();
        c.colors = colors;
        c.save(&self.config_path);
        self.apply_config(c);
    }

    /// Write raw bytes to a pane: its local PTY, or the remote session as an input `Data` frame.
    fn write_pty(&mut self, id: PaneId, bytes: &[u8]) {
        if let Some(t) = self.terms.get_mut(&id) {
            match &mut t.backend {
                Backend::Local { writer, .. } => {
                    let _ = writer.write_all(bytes);
                    let _ = writer.flush();
                }
                Backend::Remote {
                    outbound,
                    remote_id,
                    ..
                } => {
                    let _ = outbound.try_send(Frame::Data {
                        pane: *remote_id,
                        bytes: bytes.to_vec(),
                    });
                }
            }
        }
    }

    /// DECCKM (application cursor keys) state of a pane — decides SS3 vs CSI.
    fn app_cursor(&self, id: PaneId) -> bool {
        self.arc(id)
            .is_some_and(|t| t.lock().unwrap().mode().contains(TermMode::APP_CURSOR))
    }

    /// (alternate screen, alternate-scroll requested) for a pane — wheel behaves differently.
    fn alt_modes(&self, id: PaneId) -> (bool, bool) {
        self.arc(id).map_or((false, false), |t| {
            let guard = t.lock().unwrap();
            let m = guard.mode();
            (
                m.contains(TermMode::ALT_SCREEN),
                m.contains(TermMode::ALTERNATE_SCROLL),
            )
        })
    }

    /// Scroll a pane's history viewport. No-op on the alternate screen (it has no scrollback).
    fn scroll(&mut self, id: PaneId, s: Scroll) {
        if let Some(term) = self.arc(id) {
            let mut t = term.lock().unwrap();
            if t.mode().contains(TermMode::ALT_SCREEN) {
                return;
            }
            t.scroll_display(s);
        }
        self.touch(id);
    }

    /// Mouse wheel over a pane (lines > 0 = up/into history). The primary screen scrolls
    /// scrollback; the alternate screen emits arrow keys when the app asked for alternate-scroll.
    fn on_wheel(&mut self, id: PaneId, lines: i32) {
        let (alt, alt_scroll) = self.alt_modes(id);
        if alt {
            if alt_scroll {
                let final_byte = if lines > 0 { b'A' } else { b'B' };
                let seq = [
                    0x1b,
                    if self.app_cursor(id) { b'O' } else { b'[' },
                    final_byte,
                ];
                for _ in 0..lines.unsigned_abs() {
                    self.write_pty(id, &seq);
                }
            }
        } else {
            self.scroll(id, Scroll::Delta(lines));
        }
    }

    fn request_redraw(&self) {
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }

    fn display_offset(&self, id: PaneId) -> i32 {
        self.arc(id)
            .map_or(0, |t| t.lock().unwrap().grid().display_offset() as i32)
    }

    /// Is a physical-pixel position over a split divider's grab region? (egui owns those drags.)
    fn on_divider(&self, px: f64, py: f64) -> bool {
        let (px, py) = (px as f32, py as f32);
        self.divider_px
            .iter()
            .any(|(ox, oy, w, h)| px >= *ox && px < ox + w && py >= *oy && py < oy + h)
    }

    /// The pane under a physical-pixel position (active tab only), if any.
    fn pane_at(&self, px: f64, py: f64) -> Option<PaneId> {
        let (px, py) = (px as f32, py as f32);
        self.pane_px
            .iter()
            .find(|(_, ox, oy, w, h)| px >= *ox && px < ox + w && py >= *oy && py < oy + h)
            .map(|(id, ..)| *id)
    }

    /// A pane's pixel rect (origin x, y, width, height).
    fn rect_of(&self, id: PaneId) -> Option<(f32, f32, f32, f32)> {
        self.pane_px
            .iter()
            .find(|(p, ..)| *p == id)
            .map(|(_, ox, oy, w, h)| (*ox, *oy, *w, *h))
    }

    /// Map a physical-pixel position to a grid point in pane `id` (absolute line, incl.
    /// scrollback) and which half of the cell it falls on.
    fn point_at(&self, id: PaneId, px: f64, py: f64) -> Option<(Point, Side)> {
        let (ox, oy, w, h) = self.rect_of(id)?;
        let dims = self.terms.get(&id)?.dims;
        let relx = (px as f32 - ox).clamp(0.0, (w - 1.0).max(0.0));
        let rely = (py as f32 - oy).clamp(0.0, (h - 1.0).max(0.0));
        let col = ((relx / self.cell_w) as usize).min(dims.cols.saturating_sub(1));
        let vis_row = ((rely / self.cell_h) as i32)
            .min(dims.rows as i32 - 1)
            .max(0);
        let line = vis_row - self.display_offset(id);
        let side = if (relx / self.cell_w).fract() > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        Some((Point::new(Line(line), Column(col)), side))
    }

    /// Mouse-reporting flags of a pane: (any reporting on, SGR encoding, any-motion 1003,
    /// button-drag 1002).
    fn mouse_modes(&self, id: PaneId) -> (bool, bool, bool, bool) {
        self.arc(id).map_or((false, false, false, false), |t| {
            let guard = t.lock().unwrap();
            let m = guard.mode();
            (
                m.intersects(TermMode::MOUSE_MODE),
                m.contains(TermMode::SGR_MOUSE),
                m.contains(TermMode::MOUSE_MOTION),
                m.contains(TermMode::MOUSE_DRAG),
            )
        })
    }

    /// 1-based viewport cell (column, row) under a position within pane `id`, for mouse reports.
    fn cell_vp(&self, id: PaneId, px: f64, py: f64) -> Option<(i64, i64)> {
        let (ox, oy, w, h) = self.rect_of(id)?;
        let relx = (px as f32 - ox).clamp(0.0, (w - 1.0).max(0.0));
        let rely = (py as f32 - oy).clamp(0.0, (h - 1.0).max(0.0));
        Some((
            (relx / self.cell_w) as i64 + 1,
            (rely / self.cell_h) as i64 + 1,
        ))
    }

    /// Encode a mouse event and write it to a pane's PTY (SGR-1006 when negotiated, else X10).
    fn report_mouse(&mut self, id: PaneId, cb: u8, pressed: bool, col: i64, row: i64, sgr: bool) {
        let bytes = if sgr {
            format!(
                "\x1b[<{};{};{}{}",
                cb,
                col,
                row,
                if pressed { 'M' } else { 'm' }
            )
            .into_bytes()
        } else {
            // X10: button+32, coords clamped to 223 and offset by 32; release is button 3.
            let b = if pressed { cb } else { 3 };
            vec![
                0x1b,
                b'[',
                b'M',
                32 + b,
                (col.min(223) + 32) as u8,
                (row.min(223) + 32) as u8,
            ]
        };
        self.write_pty(id, &bytes);
    }

    /// Report motion for the held button (or 3 = no button) in pane `id`, deduped to cell changes.
    fn report_motion(&mut self, id: PaneId, cb: u8, sgr: bool) {
        let Some((col, row)) = self.cell_vp(id, self.mouse_px.0, self.mouse_px.1) else {
            return;
        };
        if self.last_report_cell == Some((col, row)) {
            return;
        }
        self.last_report_cell = Some((col, row));
        self.report_mouse(id, cb + 32, true, col, row, sgr);
    }

    /// Begin a selection in `self.mouse_pane`, choosing simple/word/line by click count.
    fn start_selection(&mut self) {
        let Some(id) = self.mouse_pane else { return };
        let (px, py) = self.mouse_px;
        let Some((point, side)) = self.point_at(id, px, py) else {
            return;
        };

        let now = Instant::now();
        let recent = self
            .last_click
            .is_some_and(|(t, p)| now.duration_since(t) < Duration::from_millis(350) && p == point);
        self.click_count = if recent {
            (self.click_count % 3) + 1
        } else {
            1
        };
        self.last_click = Some((now, point));

        let ty = match self.click_count {
            2 => SelectionType::Semantic, // word
            3 => SelectionType::Lines,    // whole line
            _ => SelectionType::Simple,
        };
        if let Some(term) = self.arc(id) {
            term.lock().unwrap().selection = Some(Selection::new(ty, point, side));
        }
        self.selecting = true;
        self.touch(id);
    }

    /// Extend the in-progress selection to the mouse.
    fn update_selection(&mut self) {
        let Some(id) = self.mouse_pane else { return };
        let (px, py) = self.mouse_px;
        let Some((point, side)) = self.point_at(id, px, py) else {
            return;
        };
        if let Some(term) = self.arc(id)
            && let Some(sel) = term.lock().unwrap().selection.as_mut()
        {
            sel.update(point, side);
        }
        self.touch(id);
    }

    /// Finish selecting; a plain click (empty selection) clears any highlight, otherwise the
    /// selection is published to the primary selection (middle-click paste source on Linux).
    fn end_selection(&mut self) {
        self.selecting = false;
        let id = self.mouse_pane.take();
        let mut selected = None;
        if let Some(term) = id.and_then(|id| self.arc(id)) {
            let mut t = term.lock().unwrap();
            if t.selection.as_ref().is_some_and(|s| s.is_empty()) {
                t.selection = None;
            } else {
                selected = t.selection_to_string();
            }
        }
        if let (Some(cb), Some(s)) = (&self.clipboard, selected)
            && !s.is_empty()
        {
            cb.store_primary(s);
        }
        if let Some(id) = id {
            self.touch(id);
        }
    }

    /// Clear the focused pane's selection (used when typing into it).
    fn clear_selection(&mut self) {
        if let Some(term) = self.focused_arc() {
            term.lock().unwrap().selection = None;
        }
        let id = self.focus();
        self.touch(id);
    }

    /// Copy the focused pane's selection to the clipboard and clear it. Returns whether anything
    /// was copied.
    fn copy(&mut self) -> bool {
        let text = self
            .focused_arc()
            .and_then(|t| t.lock().unwrap().selection_to_string());
        match text {
            Some(s) if !s.is_empty() => {
                if let Some(cb) = &self.clipboard {
                    cb.store(s);
                }
                self.clear_selection();
                self.request_redraw();
                true
            }
            _ => false,
        }
    }

    /// Write text to a pane's PTY, wrapped in bracketed-paste markers when the app enabled them.
    fn paste_text(&mut self, id: PaneId, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = self
            .arc(id)
            .is_some_and(|t| t.lock().unwrap().mode().contains(TermMode::BRACKETED_PASTE));
        let mut out = Vec::new();
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(text.as_bytes());
        if bracketed {
            out.extend_from_slice(b"\x1b[201~");
        }
        self.write_pty(id, &out);
    }

    fn paste(&mut self) {
        let text = self.clipboard.as_ref().and_then(|cb| cb.load());
        if let Some(t) = text {
            self.paste_text(self.focus(), &t);
        }
    }

    fn on_key(&mut self, ev: &KeyEvent) {
        // A dialog text field owns the keyboard while open — let egui handle it.
        if self.text_input_active() {
            return;
        }
        if let Some(action) = self.shortcut_capture {
            if ev.state != ElementState::Pressed {
                return;
            }
            if matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
                self.shortcut_capture = None;
                self.request_redraw();
                return;
            }
            if let Some(binding) = self.capture_binding_from_key(ev) {
                self.set_shortcut(action, binding);
            }
            return;
        }
        // Esc closes the Settings window (during shortcut capture, the branch above eats it
        // to cancel just the capture). Other keys keep going to the focused pane.
        if self.show_settings
            && ev.state == ElementState::Pressed
            && matches!(ev.logical_key, Key::Named(NamedKey::Escape))
        {
            self.show_settings = false;
            self.request_redraw();
            return;
        }
        if ev.state != ElementState::Pressed || self.terms.is_empty() {
            return;
        }
        if let Some(action) = self.shortcut_action(ev) {
            self.run_shortcut(action);
            return;
        }
        // Typing keeps the cursor solid and restarts the blink cycle.
        self.reset_blink();
        let focus = self.focus();
        // Engaging with a pane (a keystroke) clears any attention note it raised.
        if self.clear_pending_for_pane(focus) {
            self.request_redraw();
        }
        // On Windows AltGr is reported as Ctrl+Alt; excluding Alt keeps AltGr symbols
        // (`@ { [ ] } \\ | ~ €` on the German layout) out of the Ctrl-shortcut / control-code path.
        let ctrl = self.mods.state().control_key() && !self.mods.state().alt_key();
        let shift = self.mods.state().shift_key();
        let alt = self.mods.state().alt_key();
        // xterm modifier for cursor/editing keys, so e.g. Ctrl-Left sends `ESC [ 1 ; 5 D` (word
        // motion) rather than a bare arrow indistinguishable from an unmodified press.
        let modifier = xterm_modifier(shift, alt, ctrl);

        // Shift+nav scrolls the focused pane's history viewport (and is not sent to the PTY).
        if shift {
            match &ev.logical_key {
                Key::Named(NamedKey::PageUp) => return self.scroll(focus, Scroll::PageUp),
                Key::Named(NamedKey::PageDown) => return self.scroll(focus, Scroll::PageDown),
                Key::Named(NamedKey::Home) => return self.scroll(focus, Scroll::Top),
                Key::Named(NamedKey::End) => return self.scroll(focus, Scroll::Bottom),
                _ => {}
            }
        }

        // Clipboard shortcuts. Ctrl-C copies only when a selection exists (else it falls
        // through to ^C / SIGINT); Ctrl-Shift-C always copies; Ctrl-V / Ctrl-Shift-V paste;
        // Ctrl-Insert copies, Shift-Insert pastes.
        match &ev.logical_key {
            Key::Character(s) if ctrl => match s.to_lowercase().as_str() {
                "c" => {
                    if shift {
                        self.copy();
                        return;
                    }
                    if self.copy() {
                        return; // had a selection → copied; otherwise fall through to ^C
                    }
                }
                "v" => return self.paste(),
                _ => {}
            },
            Key::Named(NamedKey::Insert) if ctrl => {
                self.copy();
                return;
            }
            Key::Named(NamedKey::Insert) if shift => return self.paste(),
            _ => {}
        }
        // Cursor keys: `ESC O x` in application mode, else `ESC [ x`; with a modifier held, the CSI
        // `ESC [ 1 ; mod x` form. mc (ncurses) relies on the app-mode form; vim is lenient and
        // accepts CSI either way — which is why unmodified arrows "worked".
        let cur = |b: u8| cursor_key(b, modifier, self.app_cursor(focus));

        let mut out: Vec<u8> = Vec::new();
        match &ev.logical_key {
            Key::Named(NamedKey::Enter) => out.extend_from_slice(b"\r"),
            Key::Named(NamedKey::Backspace) => out.push(0x7f),
            Key::Named(NamedKey::Tab) => out.push(b'\t'),
            Key::Named(NamedKey::Escape) => out.push(0x1b),
            Key::Named(NamedKey::Space) => out.push(b' '),

            Key::Named(NamedKey::ArrowUp) => out = cur(b'A'),
            Key::Named(NamedKey::ArrowDown) => out = cur(b'B'),
            Key::Named(NamedKey::ArrowRight) => out = cur(b'C'),
            Key::Named(NamedKey::ArrowLeft) => out = cur(b'D'),
            Key::Named(NamedKey::Home) => out = cur(b'H'),
            Key::Named(NamedKey::End) => out = cur(b'F'),

            // Editing/paging keys (CSI ~ form, independent of DECCKM; modifier as `code ; mod ~`).
            Key::Named(NamedKey::Insert) => out = csi_tilde(2, modifier),
            Key::Named(NamedKey::Delete) => out = csi_tilde(3, modifier),
            Key::Named(NamedKey::PageUp) => out = csi_tilde(5, modifier),
            Key::Named(NamedKey::PageDown) => out = csi_tilde(6, modifier),

            // Function keys (xterm encoding, matching xterm-256color terminfo).
            Key::Named(NamedKey::F1) => out.extend_from_slice(b"\x1bOP"),
            Key::Named(NamedKey::F2) => out.extend_from_slice(b"\x1bOQ"),
            Key::Named(NamedKey::F3) => out.extend_from_slice(b"\x1bOR"),
            Key::Named(NamedKey::F4) => out.extend_from_slice(b"\x1bOS"),
            Key::Named(NamedKey::F5) => out.extend_from_slice(b"\x1b[15~"),
            Key::Named(NamedKey::F6) => out.extend_from_slice(b"\x1b[17~"),
            Key::Named(NamedKey::F7) => out.extend_from_slice(b"\x1b[18~"),
            Key::Named(NamedKey::F8) => out.extend_from_slice(b"\x1b[19~"),
            Key::Named(NamedKey::F9) => out.extend_from_slice(b"\x1b[20~"),
            Key::Named(NamedKey::F10) => out.extend_from_slice(b"\x1b[21~"),
            Key::Named(NamedKey::F11) => out.extend_from_slice(b"\x1b[23~"),
            Key::Named(NamedKey::F12) => out.extend_from_slice(b"\x1b[24~"),

            _ => {
                if let Some(t) = &ev.text {
                    if ctrl && t.len() == 1 && t.as_bytes()[0].is_ascii_alphabetic() {
                        out.push(t.as_bytes()[0].to_ascii_uppercase() & 0x1f);
                    } else {
                        out.extend_from_slice(t.as_bytes());
                    }
                }
            }
        }
        if !out.is_empty() {
            // Typing clears the focused selection and returns its viewport to the prompt.
            self.clear_selection();
            self.scroll(focus, Scroll::Bottom);
            self.write_pty(focus, &out);
        }
    }

    /// Resize a pane's terminal + PTY to `dims` (no-op if unchanged).
    fn fit_terminal(&mut self, id: PaneId, dims: Dims) {
        // A follower doesn't fit remote panes to its own rects: the focus owner's geometry drives
        // the PTY size, and we conform via the daemon's `Resize` broadcasts instead.
        if let Some(Backend::Remote { conn, .. }) = self.terms.get(&id).map(|t| &t.backend)
            && !self.conn_focused(*conn)
        {
            return;
        }
        if let Some(t) = self.terms.get_mut(&id) {
            if t.dims == dims {
                return;
            }
            t.dims = dims;
            self.last_dims = dims;
            t.term.lock().unwrap().resize(dims);
            match &t.backend {
                Backend::Local { master, .. } => {
                    let _ = master.resize(portable_pty::PtySize {
                        rows: dims.rows as u16,
                        cols: dims.cols as u16,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Backend::Remote {
                    outbound,
                    remote_id,
                    ..
                } => {
                    let _ = outbound.try_send(Frame::Control(Control::Resize {
                        pane: *remote_id,
                        cols: dims.cols as u16,
                        rows: dims.rows as u16,
                    }));
                }
            }
            self.dirty.insert(id);
        }
    }

    #[allow(deprecated)] // egui_ctx.run → run_ui migration, see build_ui note
    fn redraw(&mut self) {
        // Nothing to draw once the last terminal is gone (we're exiting).
        if self.state.is_none() || self.terms.is_empty() {
            return;
        }
        let window = self.state.as_ref().unwrap().window.clone();

        // Window title follows the active (focused) pane's title.
        let active_title = self
            .terms
            .get(&self.focus())
            .map(|t| t.title.clone())
            .unwrap_or_default();
        let desired = if active_title.is_empty() {
            "potty".to_string()
        } else {
            format!("{active_title} — potty")
        };
        if self.window_title != desired {
            self.window_title = desired.clone();
            window.set_title(&desired);
        }

        // Each tab's label is its focused pane's title (falling back to the tab number).
        let tab_titles: Vec<String> = self
            .workspace
            .tabs
            .iter()
            .map(|t| {
                let Some(term) = self.terms.get(&t.focus) else {
                    return t.title.clone();
                };
                let title = if term.title.is_empty() {
                    t.title.clone()
                } else {
                    term.title.clone()
                };
                // Remote tabs carry a host prefix so they're distinguishable at a glance.
                match &term.backend {
                    Backend::Remote { label, .. } => format!("{label}: {title}"),
                    Backend::Local { .. } => title,
                }
            })
            .collect();

        // Apply the configurable chrome font size plus potty's shared menu/dialog style.
        apply_chrome_style(&self.egui_ctx, self.config.ui_font_size);

        let raw = self.egui_state.as_mut().unwrap().take_egui_input(&window);
        let mut actions = Vec::new();
        let mut leaves: Vec<(PaneId, egui::Rect)> = Vec::new();
        let mut dividers: Vec<egui::Rect> = Vec::new();
        let mut show_settings = self.show_settings;
        let mut settings_page = self.settings_page;
        let shortcut_capture = self.shortcut_capture;
        let b = self.config.border();
        let border_color = egui::Color32::from_rgb(b.r, b.g, b.b);
        let pane_padding = self.config.pane_padding;
        let feed_items = self.feed_items();
        let mut feed_active = false;
        let chrome_latched = self.chrome_latched;
        let feed_open = self.feed_open;
        let auth_view = self.auth_prompts.first().map(|p| match p {
            AuthPrompt::HostKey {
                host,
                fingerprint,
                status,
                ..
            } => AuthView::HostKey {
                host: host.clone(),
                fingerprint: fingerprint.clone(),
                status: *status,
            },
            AuthPrompt::Text { title, fields, .. } => AuthView::Text {
                title: title.clone(),
                fields: fields.clone(),
            },
        });
        let show_connect = self.show_connect;
        let error = self.error_msg.clone();
        let connect_profiles = self.connect_profile_views();
        let connect_progress = self.connect_progress_views();
        let context_menu_pane = self.context_menu_pane.take();
        // Panes whose connection is a persistent (potty-session) remote — the only ones that can be
        // detached. Computed fresh (owned) so it doesn't tangle with the borrows below.
        let detachable_panes: std::collections::HashSet<PaneId> = self
            .connections
            .values()
            .filter(|c| !c.ephemeral)
            .flat_map(|c| c.routes.values().copied())
            .collect();
        let auth_inputs = &mut self.auth_inputs;
        let mut connect_progress_active = false;
        let connect_host = &mut self.connect_host;
        let connect_name = &mut self.connect_name;
        let connect_use_session = &mut self.connect_use_session;
        let full = {
            let ws = &self.workspace;
            let families = &self.font_families;
            let cur_family = self.config.font_family.as_deref();
            let cur_size = self.config.font_size;
            self.egui_ctx.run(raw, |ctx| {
                build_ui(
                    ctx,
                    &self.config,
                    ws,
                    families,
                    cur_family,
                    cur_size,
                    &tab_titles,
                    border_color,
                    pane_padding,
                    &mut show_settings,
                    &mut settings_page,
                    shortcut_capture,
                    &mut actions,
                    &mut leaves,
                    &mut dividers,
                    &feed_items,
                    &mut feed_active,
                    chrome_latched,
                    feed_open,
                    auth_view.as_ref(),
                    auth_inputs,
                    show_connect,
                    connect_host,
                    connect_name,
                    connect_use_session,
                    &connect_profiles,
                    &connect_progress,
                    &mut connect_progress_active,
                    error.as_deref(),
                    &detachable_panes,
                    context_menu_pane,
                )
            })
        };
        self.show_settings = show_settings;
        self.settings_page = settings_page;
        if !self.show_settings {
            self.shortcut_capture = None;
        }
        // Remember whether a popup/menu is open so the next frame's clicks don't leak through
        // the menu into the terminal underneath.
        // The feed overlay isn't a popup, so OR in whether the pointer is over it — otherwise a
        // click on the feed would also start a selection in the terminal beneath it. The auth
        // dialog likewise suppresses terminal mouse handling.
        self.menu_open = self.egui_ctx.memory(|m| m.any_popup_open())
            || feed_active
            || connect_progress_active
            || auth_view.is_some()
            || self.show_settings
            || self.show_connect
            || self.error_msg.is_some();
        for a in actions {
            match a {
                Action::SetFontFamily(f) => self.set_font_family(f),
                Action::SetColors(c) => self.set_colors(c),
                Action::SetFontSize(s) => self.set_font_size(s),
                Action::ShowSettings => self.show_settings = true,
                Action::CaptureShortcut(action) => {
                    self.shortcut_capture = Some(action);
                    self.request_redraw();
                }
                Action::ResetShortcut(action) => self.reset_shortcut(action),
                Action::ActivateNote(key, pane) => self.activate_note(key, pane),
                Action::DismissNote(key) => self.dismiss_note(key, true),
                Action::ShowFeed(open) => {
                    self.feed_open = open;
                    self.request_redraw();
                }
                Action::DismissFeed => {
                    self.feed_open = false;
                    self.chrome_latched = false;
                    self.request_redraw();
                }
                Action::AuthAnswer(accept) => self.answer_auth(accept),
                Action::AuthText(submit) => self.answer_auth_text(submit),
                Action::OpenConnect => {
                    self.show_connect = true;
                    self.request_redraw();
                }
                Action::CloseConnect => {
                    self.show_connect = false;
                    self.request_redraw();
                }
                Action::Connect(host, name) => self.start_connect(&host, &name),
                Action::UseProfile(index) => self.use_connect_profile(index),
                Action::DismissError => {
                    self.error_msg = None;
                    self.request_redraw();
                }
                Action::DetachSession => self.detach_focused_session(),
                // Remote-aware: a split/new-tab from a remote pane stays on its connection; the
                // tab bar's "+" always opens a local shell.
                Action::Split(s) => self.split_pane(s),
                Action::NewTab => self.new_tab(false),
                Action::NewLocalTab => self.new_tab(true),
                a => apply(&mut self.workspace, a),
            }
        }
        // Actions may have created/destroyed panes — keep a terminal per leaf. (The `leaves`
        // rects reflect the pre-action layout for this frame; egui requests a repaint after the
        // interaction, so the new layout lands next frame.)
        self.reconcile_terms();
        // Push any changed remote layout to its daemon, so a reattach can restore the tree.
        self.sync_layouts();

        let egui::FullOutput {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            ..
        } = full;
        self.egui_state
            .as_mut()
            .unwrap()
            .handle_platform_output(&window, platform_output);

        let ppp = pixels_per_point;
        // Fit each active-tab terminal to its pane (may mark it dirty), and remember pane rects
        // for hit-testing. Track the visible set so background output doesn't force redraws.
        for (id, r) in &leaves {
            let dims = dims_for(r.width() * ppp, r.height() * ppp, self.cell_w, self.cell_h);
            self.fit_terminal(*id, dims);
        }
        self.pane_px = leaves
            .iter()
            .map(|(id, r)| {
                (
                    *id,
                    r.min.x * ppp,
                    r.min.y * ppp,
                    r.width() * ppp,
                    r.height() * ppp,
                )
            })
            .collect();
        // A changed visible set means a split/close/tab-switch rearranged panes — their rects
        // may have moved without a dims change, so rebuild all of them this frame.
        let new_visible: std::collections::HashSet<PaneId> =
            leaves.iter().map(|(id, _)| *id).collect();
        if new_visible != self.visible {
            self.dirty.extend(new_visible.iter().copied());
        }
        self.visible = new_visible;

        // Geometry damage: any pane whose pixel rect moved/resized (drag-resize a divider, window
        // resize) has its cached buffers positioned for the old rect — rebuild it.
        for (id, ox, oy, w, h) in &self.pane_px {
            let cur = (*ox, *oy, *w, *h);
            if self.last_rect.get(id) != Some(&cur) {
                self.dirty.insert(*id);
                self.last_rect.insert(*id, cur);
            }
        }
        // Divider grab regions → physical px, for the mouse handler to yield to egui.
        self.divider_px = dividers
            .iter()
            .map(|r| {
                (
                    r.min.x * ppp,
                    r.min.y * ppp,
                    r.width() * ppp,
                    r.height() * ppp,
                )
            })
            .collect();

        let prims = self.egui_ctx.tessellate(shapes, ppp);

        // Damage tracking: re-prepare only the visible panes flagged dirty; the rest render from
        // their cached buffers. We lock each dirty pane just long enough to rebuild it.
        let (sw, sh) = {
            let s = self.state.as_ref().unwrap();
            (
                s.surface_config.width as f32,
                s.surface_config.height as f32,
            )
        };
        let focus = self.focus();
        let cursor_thickness = self.config.cursor_thickness;
        for (id, r) in &leaves {
            if self.dirty.remove(id)
                && let Some(term) = self.arc(*id)
            {
                let origin = (r.min.x * ppp, r.min.y * ppp);
                // Only the focused pane's cursor blinks; suppress it during the off phase.
                let show_cursor = *id != focus || self.blink_on;
                let guard = term.lock().unwrap();
                self.state.as_mut().unwrap().prepare_pane(
                    *id,
                    &guard,
                    origin,
                    (sw, sh),
                    show_cursor,
                    cursor_thickness,
                );
            }
        }

        let panes: Vec<(egui::Rect, u64)> = leaves.iter().map(|(id, r)| (*r, (*id))).collect();
        let renderer = self.egui_renderer.as_mut().unwrap();
        if let Some(state) = self.state.as_mut() {
            state.render(renderer, &textures_delta, &prims, ppp, &panes);
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        let icon = winit::window::Icon::from_rgba(WINDOW_ICON_RGBA.to_vec(), 64, 64).ok();
        let attrs = Window::default_attributes()
            .with_title("potty")
            .with_inner_size(LogicalSize::new(960.0, 600.0))
            .with_window_icon(icon);
        // Wayland has no per-window icon protocol — set the app_id so KWin matches the installed
        // `.desktop` (and its Icon=) instead. (Shadow rather than `mut` so Windows stays warning-free.)
        #[cfg(target_os = "linux")]
        let attrs = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs.with_name(APP_ID, "potty")
        };
        let window = Arc::new(event_loop.create_window(attrs).unwrap());

        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        self.scale = scale;

        let mut state = pollster::block_on(WindowState::new(window.clone(), event_loop));
        self.font_families = state.grid.families().to_vec();

        // Load config (writing a default file on first run), then apply it to the renderer.
        if !self.config_path.exists() {
            Config::default().save(&self.config_path);
        }
        self.config = Config::load(&self.config_path);
        state.grid.set_palette(self.config.palette());
        state.grid.set_font(
            self.config.font_family.clone(),
            self.config.font_size * scale,
            self.config.font_size * 1.2 * scale,
        );
        let m = state.grid.metrics();
        self.cell_w = m.w;
        self.cell_h = m.h;

        // Watch the config directory; any change triggers a reload (robust to write-rename).
        if let Some(dir) = self.config_path.parent().map(|p| p.to_path_buf()) {
            let _ = std::fs::create_dir_all(&dir);
            let proxy = self.proxy.clone();
            if let Ok(mut w) =
                notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(event) = res {
                        // Only react to real content changes. Reacting to reads (Access) or
                        // atime/permission churn (Modify::Metadata) creates a feedback loop —
                        // ReloadConfig re-reads the file, which trips the watcher again — and two
                        // instances watching the same dir ping-pong each other's reads to 100% CPU.
                        let ignore = matches!(
                            event.kind,
                            notify::EventKind::Access(_)
                                | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_))
                        );
                        if !ignore {
                            let _ = proxy.send_event(UserEvent::ReloadConfig);
                        }
                    }
                })
                && w.watch(&dir, notify::RecursiveMode::NonRecursive).is_ok()
            {
                self._watcher = Some(w);
            }
        }

        // Initial grid size from the content area (window minus the top bar). New panes spawn
        // at this size until the first redraw fits them to their actual rect.
        self.last_dims = dims_for(
            size.width as f32,
            size.height as f32 - TOPBAR * scale,
            self.cell_w,
            self.cell_h,
        );

        // egui plumbing.
        let egui_state = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(scale),
            Some(winit::window::Theme::Dark),
            Some(state.device.limits().max_texture_dimension_2d as usize),
        );
        // Accept IME commits so an active ibus/fcitx can't swallow input (layout-agnostic safety net).
        window.set_ime_allowed(true);

        // Platform clipboard (Wayland seat on Linux, Win32 on Windows). See `clip`.
        self.clipboard = clip::Clipboard::new(&window);
        let egui_renderer = egui_wgpu::Renderer::new(
            &state.device,
            state.surface_config.format,
            egui_wgpu::RendererOptions::default(),
        );

        self.state = Some(state);
        self.egui_state = Some(egui_state);
        self.egui_renderer = Some(egui_renderer);

        // Spawn the home terminal (and any other leaves, though there's only one at startup).
        self.reconcile_terms();
        // Kick the first frame — `visible` is still empty, so an early Wake wouldn't.
        self.request_redraw();
        // Spike: optionally auto-connect to a remote test host (see `maybe_test_connect`), or open
        // the connect dialog (for testing the UI without clicking the menu).
        self.maybe_test_connect();
        if std::env::var_os("POTTY_TEST_CONNECT").is_some() {
            self.show_connect = true;
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Drop the clipboard before the Wayland connection is torn down — its worker thread
        // holds the wl_display, and using it after teardown segfaults.
        self.clipboard = None;
        for path in &self.notify_paths {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Drive the cursor blink. We otherwise wait idle (everything else is redraw-on-demand), so
    /// the timer — and the CPU it costs — exists only while the focused cursor is actually
    /// blinking. On each toggle we re-prepare just the focused pane and ask for one redraw.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        use winit::event_loop::ControlFlow;
        // During teardown the workspace is empty; `focus()` would index an empty leaf vec and
        // panic — and unwinding back through winit's C frames becomes a segfault.
        if self.state.is_none() || self.terms.is_empty() {
            return;
        }
        if self.focused_cursor_blinks() {
            let now = Instant::now();
            let next = *self.blink_next.get_or_insert(now + BLINK_INTERVAL);
            if now >= next {
                self.blink_on = !self.blink_on;
                self.blink_next = Some(now + BLINK_INTERVAL);
                let focus = self.focus();
                self.dirty.insert(focus);
                self.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                self.blink_next.unwrap_or(now + BLINK_INTERVAL),
            ));
        } else {
            // Not blinking: make sure the cursor is shown, then go fully idle.
            if !self.blink_on {
                self.blink_on = true;
                self.dirty.insert(self.focus());
                self.request_redraw();
            }
            self.blink_next = None;
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            // PTY output: mark the pane dirty; redraw only if it's on screen (a background
            // pane's output is absorbed until its tab is shown). Re-arm the reader's coalescing
            // flag so further output sends a fresh wake.
            UserEvent::Wake(id) => {
                if let Some(t) = self.terms.get(&id) {
                    t.wake_pending.store(false, Ordering::Release);
                }
                // Output in the focused pane keeps its cursor solid (and restarts the blink cycle).
                if !self.terms.is_empty() && id == self.focus() {
                    self.reset_blink();
                }
                self.touch(id);
            }
            UserEvent::ReloadConfig => {
                let cfg = Config::load(&self.config_path);
                self.apply_config(cfg);
            }
            // OSC 52: app wrote the system clipboard.
            UserEvent::ClipboardStore(text) => {
                if let Some(cb) = &self.clipboard {
                    cb.store(text);
                }
            }
            // OSC 52: app reads the clipboard; format the response and send it to that pane.
            UserEvent::ClipboardLoad(pane, fmt) => {
                let text = self.clipboard.as_ref().and_then(|cb| cb.load());
                if let Some(t) = text {
                    let response = fmt(&t);
                    self.write_pty(pane, response.as_bytes());
                }
            }
            // Terminal query response (DSR, DA, cursor position, …).
            UserEvent::PtyWrite(pane, text) => self.write_pty(pane, text.as_bytes()),
            // The pane's program set / reset its title — redraw to refresh tab + window title.
            UserEvent::Title(pane, title) => {
                if let Some(t) = self.terms.get_mut(&pane) {
                    t.title = title;
                }
                self.request_redraw();
            }
            UserEvent::ResetTitle(pane) => {
                if let Some(t) = self.terms.get_mut(&pane) {
                    t.title = t.default_title.clone();
                }
                self.request_redraw();
            }
            // A shell exited — close its pane. Exit the app once no terminals remain.
            UserEvent::PaneExited(id) => self.close_pane(event_loop, id),
            // An agentic CLI (via `potty-notify`) raised/cleared an attention note.
            UserEvent::Notify(note) => self.on_local_note(note),
            UserEvent::PeerNotify(note) => self.on_peer_note(note),
            UserEvent::PeerFocus {
                instance,
                host,
                session,
                pane,
            } => self.on_peer_focus(instance, host, session, pane),
            UserEvent::PeerDismiss {
                instance,
                host,
                session,
            } => self.on_peer_dismiss(instance, host, session),
            // A remote session connected — give it a tab with one shell pane.
            UserEvent::RemoteConnected {
                conn,
                target,
                display_name,
                outbound,
                ephemeral,
            } => self.on_remote_connected(conn, target, display_name, outbound, ephemeral),
            // Output / lifecycle from a remote session.
            UserEvent::RemoteFrame(conn, frame) => self.on_remote_frame(event_loop, conn, frame),
            UserEvent::RemoteDisconnected { conn, msg } => {
                self.connect_progress.remove(&conn);
                if self.connections.contains_key(&conn) {
                    self.detach_connection(conn);
                    self.error_msg = Some(msg);
                    self.request_redraw();
                }
            }
            UserEvent::RemoteError { conn, msg } => {
                remote::sshdbg(format!("gui: showing error dialog: {msg}"));
                // Drop a registered-but-paneless connection (a handshake that never completed), so
                // it doesn't linger in the map.
                if let Some(conn) = conn {
                    self.connect_progress.remove(&conn);
                }
                if let Some(conn) = conn
                    && self
                        .connections
                        .get(&conn)
                        .is_some_and(|c| c.routes.is_empty())
                {
                    self.connections.remove(&conn);
                }
                self.error_msg = Some(msg);
                self.request_redraw();
            }
            // A connection needs the user — queue it and show the dialog.
            UserEvent::Auth(prompt) => {
                self.auth_prompts.push(prompt);
                self.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        // Decide what NOT to hand egui:
        //  - All keyboard input: the chrome is mouse-only by design, so egui must not steal keys
        //    (Tab navigating widgets, Space/Enter activating a focused tab) from the terminal.
        //  - A plain right-click over a mouse-reporting pane: it belongs to the app, not our
        //    context menu (shift, or a non-reporting pane, lets egui open the menu as usual).
        //  - macOS reports Control-click as a primary click; Potty treats it as a menu trigger
        //    instead of letting egui see a normal left click.
        let withhold_from_egui = match &event {
            // Keys normally go to the focused pane, not egui — except while a dialog text field is
            // open, when egui needs them.
            WindowEvent::KeyboardInput { .. } => !self.text_input_active(),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => {
                let mods = self.mods.state();
                let shift = mods.shift_key();
                let control = mods.control_key();
                self.pane_at(self.mouse_px.0, self.mouse_px.1)
                    .is_some_and(|id| {
                        let (report, ..) = self.mouse_modes(id);
                        terminal_mouse_claims_context_click(*button, report, shift)
                            || macos_pane_context_click(*button, control)
                    })
            }
            _ => false,
        };

        let bare_text_commit = matches!(&event, WindowEvent::Ime(Ime::Commit(_)))
            && ime_commit_needs_text_fallback(self.text_input_active(), self.ime_had_preedit);
        let mut egui_consumed = false;
        if let Some(window) = self.state.as_ref().map(|s| s.window.clone())
            && let Some(es) = self.egui_state.as_mut()
        {
            if bare_text_commit {
                // KDE/Wayland can switch from `KeyboardInput.text` to bare IME commits after
                // the first character. A bare commit is plain text, not a composition event.
                if let WindowEvent::Ime(Ime::Commit(text)) = &event {
                    es.egui_input_mut()
                        .events
                        .push(egui::Event::Text(text.clone()));
                }
                egui_consumed = true;
                window.request_redraw();
            } else if !withhold_from_egui {
                let resp = es.on_window_event(&window, &event);
                egui_consumed = resp.consumed;
                if resp.repaint {
                    window.request_redraw();
                }
            }
        }

        // Keep a preedit latched through `Preedit(_, None)`: several IMEs end the visible
        // preedit immediately before sending its Commit. Commit/Disabled finish the composition.
        match &event {
            WindowEvent::Ime(Ime::Preedit(text, Some(_))) => {
                // An empty positioned preedit means a cancelled/cleared composition. A commit
                // that finalizes a composition arrives after `Preedit(_, None)` instead.
                self.ime_had_preedit = !text.is_empty();
            }
            WindowEvent::Ime(Ime::Commit(_)) | WindowEvent::Ime(Ime::Disabled) => {
                self.ime_had_preedit = false;
            }
            _ => {}
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_px = (position.x, position.y);
                let shift = self.mods.state().shift_key();
                if let Some(id) = self.mouse_pane {
                    // An interaction is in progress — it stays with its origin pane.
                    let (report, sgr, motion, drag) = self.mouse_modes(id);
                    if report && !shift {
                        match self.mouse_held {
                            Some(cb) if drag || motion => self.report_motion(id, cb, sgr),
                            _ => {}
                        }
                    } else if self.selecting {
                        self.update_selection();
                    }
                } else if let Some(id) = self.pane_at(position.x, position.y) {
                    // Hover motion (no button): only meaningful for any-motion tracking (1003).
                    let (report, sgr, motion, _) = self.mouse_modes(id);
                    if report && !shift && motion {
                        self.report_motion(id, 3, sgr); // 3 = no button
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // A menu/popup, the settings window, or a divider grab — let egui own this click.
                if self.menu_open
                    || self.show_settings
                    || self.on_divider(self.mouse_px.0, self.mouse_px.1)
                {
                    return;
                }
                let shift = self.mods.state().shift_key();
                let Some(id) = self.pane_at(self.mouse_px.0, self.mouse_px.1) else {
                    return;
                };
                let (report, sgr, ..) = self.mouse_modes(id);
                let pressed = state == ElementState::Pressed;
                let control = self.mods.state().control_key();
                let context_click = macos_pane_context_click(button, control);
                if context_click && !terminal_mouse_claims_context_click(button, report, shift) {
                    if pressed {
                        self.workspace.focus(id);
                        self.context_menu_pane = Some(id);
                        self.request_redraw();
                    }
                    return;
                }
                let cb = match button {
                    MouseButton::Left => Some(0),
                    MouseButton::Middle => Some(1),
                    MouseButton::Right => Some(2),
                    _ => None,
                };
                // App mouse mode (and no Shift) → forward to the app (Zellij/vim/htop select).
                // Shift bypasses reporting and forces our local selection — the standard override.
                if report && !shift {
                    if pressed {
                        self.workspace.focus(id);
                    }
                    if let Some(cb) = cb
                        && let Some((col, row)) = self.cell_vp(id, self.mouse_px.0, self.mouse_px.1)
                    {
                        self.mouse_held = if pressed { Some(cb) } else { None };
                        self.mouse_pane = if pressed { Some(id) } else { None };
                        self.last_report_cell = Some((col, row));
                        self.report_mouse(id, cb, pressed, col, row, sgr);
                    }
                    self.request_redraw();
                } else {
                    match (button, state) {
                        (MouseButton::Left, ElementState::Pressed) => {
                            self.workspace.focus(id);
                            self.mouse_pane = Some(id);
                            self.start_selection();
                        }
                        (MouseButton::Left, ElementState::Released) if self.selecting => {
                            self.end_selection()
                        }
                        (MouseButton::Middle, ElementState::Pressed) => {
                            let text = self.clipboard.as_ref().and_then(|cb| cb.load_primary());
                            if let Some(t) = text {
                                self.paste_text(id, &t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m,
            WindowEvent::KeyboardInput { event, .. } => self.on_key(&event),
            // IME commit (composed text, or text from an active input-method framework).
            WindowEvent::Ime(Ime::Commit(text))
                if terminal_should_receive_ime_commit(
                    !self.terms.is_empty(),
                    self.text_input_active(),
                    egui_consumed,
                ) =>
            {
                let focus = self.focus();
                self.write_pty(focus, text.as_bytes());
            }
            WindowEvent::MouseWheel { delta, .. } if !self.menu_open && !self.show_settings => {
                // Positive = up / into history. 3 lines per wheel notch; touchpad by pixels.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y.round() as i32) * 3,
                    MouseScrollDelta::PixelDelta(p) => (p.y / self.cell_h.max(1.0) as f64) as i32,
                };
                if lines != 0 {
                    let Some(id) = self.pane_at(self.mouse_px.0, self.mouse_px.1) else {
                        return;
                    };
                    let (report, sgr, ..) = self.mouse_modes(id);
                    if report && !self.mods.state().shift_key() {
                        // Forward as wheel buttons (64 = up, 65 = down) so the app scrolls.
                        let cb = if lines > 0 { 64 } else { 65 };
                        if let Some((col, row)) = self.cell_vp(id, self.mouse_px.0, self.mouse_px.1)
                        {
                            for _ in 0..lines.unsigned_abs() {
                                self.report_mouse(id, cb, true, col, row, sgr);
                            }
                        }
                    } else {
                        self.on_wheel(id, lines);
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(state) = &mut self.state {
                    state.surface_config.width = size.width.max(1);
                    state.surface_config.height = size.height.max(1);
                    state
                        .surface
                        .configure(&state.device, &state.surface_config);
                    state.window.request_redraw();
                }
                // Every pane's pixel rect (and the surface uniform) changed — rebuild all.
                self.dirty.extend(self.terms.keys().copied());
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}

/// Listen on this instance's attention-feed sockets and forward each message into the event loop.
/// Raw `Note` JSON is the public helper contract; wrapped `InstanceMessage` JSON is the private
/// fan-out/focus protocol between live GUI instances. Unix-only — the transport is a UDS.
#[cfg(unix)]
fn spawn_notify_listener(proxy: EventLoopProxy<UserEvent>, instance_path: &Path) -> Vec<PathBuf> {
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::net::{UnixListener, UnixStream};

    fn prepare_socket_dir(path: &Path) {
        let Some(dir) = path.parent() else {
            return;
        };
        let _ = std::fs::create_dir_all(dir);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    fn bind_unique(path: &Path) -> Option<UnixListener> {
        prepare_socket_dir(path);
        let _ = std::fs::remove_file(path);
        match UnixListener::bind(path) {
            Ok(listener) => Some(listener),
            Err(err) => {
                eprintln!(
                    "potty: attention feed disabled (socket {}: {err})",
                    path.display()
                );
                None
            }
        }
    }

    fn bind_compat(path: &Path) -> Option<UnixListener> {
        prepare_socket_dir(path);
        match UnixListener::bind(path) {
            Ok(listener) => Some(listener),
            Err(_) if UnixStream::connect(path).is_ok() => None,
            Err(_) => {
                let _ = std::fs::remove_file(path);
                UnixListener::bind(path).ok()
            }
        }
    }

    fn spawn_acceptor(proxy: EventLoopProxy<UserEvent>, listener: UnixListener) {
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                // Cap the read so a rogue client can't make us allocate unbounded.
                let mut line = String::new();
                if BufReader::new(stream.take(64 * 1024))
                    .read_line(&mut line)
                    .is_err()
                {
                    continue;
                }
                handle_notify_line(&proxy, line.trim());
            }
        });
    }

    let mut owned = Vec::new();
    if let Some(listener) = bind_unique(instance_path) {
        owned.push(instance_path.to_path_buf());
        spawn_acceptor(proxy.clone(), listener);
    }

    // Compatibility path for older shells/hooks that still target the fixed default socket. Only
    // bind it if no live process answers there; do not steal it from another running instance.
    let compat = feed::default_socket_path();
    if let Some(listener) = bind_compat(&compat) {
        owned.push(compat);
        spawn_acceptor(proxy, listener);
    }
    owned
}

#[cfg(not(unix))]
fn spawn_notify_listener(_proxy: EventLoopProxy<UserEvent>, _instance_path: &Path) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(unix)]
fn handle_notify_line(proxy: &EventLoopProxy<UserEvent>, line: &str) {
    if let Ok(msg) = serde_json::from_str::<feed::InstanceMessage>(line) {
        let event = match msg {
            feed::InstanceMessage::Note { note } => UserEvent::PeerNotify(note),
            feed::InstanceMessage::Focus {
                instance,
                host,
                session,
                pane,
            } => UserEvent::PeerFocus {
                instance,
                host,
                session,
                pane,
            },
            feed::InstanceMessage::Dismiss {
                instance,
                host,
                session,
            } => UserEvent::PeerDismiss {
                instance,
                host,
                session,
            },
        };
        let _ = proxy.send_event(event);
        return;
    }

    if let Ok(note) = serde_json::from_str::<feed::Note>(line)
        && note.v == feed::SCHEMA_VERSION
    {
        let _ = proxy.send_event(UserEvent::Notify(note));
    }
}

#[cfg(unix)]
fn send_instance_message(path: &Path, msg: &feed::InstanceMessage) -> std::io::Result<()> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(path)?;
    let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

fn new_instance_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn main() {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let proxy = event_loop.create_proxy();
    let instance_id = new_instance_id();
    let notify_sock = feed::instance_socket_path(&instance_id);
    let notify_paths = spawn_notify_listener(proxy.clone(), &notify_sock);
    let mut app = App::new(proxy, instance_id, notify_sock, notify_paths);
    event_loop.run_app(&mut app).unwrap();
}

#[cfg(test)]
mod tests {
    use super::{
        csi_tilde, cursor_key, ime_commit_needs_text_fallback, macos_control_click,
        macos_pane_context_click, stale_terminal_ids, terminal_mouse_claims_context_click,
        terminal_should_receive_ime_commit, xterm_modifier,
    };
    use std::collections::HashSet;
    use winit::event::MouseButton;

    #[test]
    fn modifier_parameter() {
        assert_eq!(xterm_modifier(false, false, false), 1); // none
        assert_eq!(xterm_modifier(true, false, false), 2); // shift
        assert_eq!(xterm_modifier(false, true, false), 3); // alt
        assert_eq!(xterm_modifier(false, false, true), 5); // ctrl
        assert_eq!(xterm_modifier(true, false, true), 6); // ctrl+shift
        assert_eq!(xterm_modifier(true, true, true), 8); // all
    }

    #[test]
    fn macos_pane_context_clicks_include_secondary_and_control_click() {
        assert_eq!(
            macos_pane_context_click(MouseButton::Right, false),
            cfg!(target_os = "macos")
        );
        assert!(!macos_pane_context_click(MouseButton::Left, false));
        assert_eq!(
            macos_control_click(MouseButton::Left, true),
            cfg!(target_os = "macos")
        );
        assert_eq!(
            macos_pane_context_click(MouseButton::Left, true),
            cfg!(target_os = "macos")
        );
    }

    #[test]
    fn terminal_mouse_claims_plain_unshifted_right_click() {
        assert!(terminal_mouse_claims_context_click(
            MouseButton::Right,
            true,
            false
        ));
        assert!(!terminal_mouse_claims_context_click(
            MouseButton::Right,
            true,
            true
        ));
        assert!(!terminal_mouse_claims_context_click(
            MouseButton::Right,
            false,
            false
        ));
        assert!(!terminal_mouse_claims_context_click(
            MouseButton::Left,
            true,
            false
        ));
    }

    #[test]
    fn unmodified_cursor_keys_honour_app_mode() {
        assert_eq!(cursor_key(b'A', 1, false), b"\x1b[A"); // normal
        assert_eq!(cursor_key(b'A', 1, true), b"\x1bOA"); // application-cursor mode
        assert_eq!(cursor_key(b'D', 1, false), b"\x1b[D");
    }

    #[test]
    fn modified_cursor_keys_use_csi_regardless_of_app_mode() {
        assert_eq!(cursor_key(b'D', 5, false), b"\x1b[1;5D"); // Ctrl-Left = word left
        assert_eq!(cursor_key(b'C', 5, true), b"\x1b[1;5C"); // app mode ignored when modified
        assert_eq!(cursor_key(b'A', 2, false), b"\x1b[1;2A"); // Shift-Up
        assert_eq!(cursor_key(b'F', 3, false), b"\x1b[1;3F"); // Alt-End
    }

    #[test]
    fn editing_keys_tilde_form() {
        assert_eq!(csi_tilde(3, 1), b"\x1b[3~"); // Delete
        assert_eq!(csi_tilde(3, 5), b"\x1b[3;5~"); // Ctrl-Delete
        assert_eq!(csi_tilde(5, 6), b"\x1b[5;6~"); // Ctrl-Shift-PageUp
    }

    #[test]
    fn ime_commit_goes_to_terminal_only_when_egui_does_not_own_text() {
        assert!(terminal_should_receive_ime_commit(true, false, false));
        assert!(!terminal_should_receive_ime_commit(true, true, false));
        assert!(!terminal_should_receive_ime_commit(true, false, true));
        assert!(!terminal_should_receive_ime_commit(false, false, false));
    }

    #[test]
    fn bare_ime_commit_is_plain_text_only_for_an_active_editor() {
        assert_eq!(
            ime_commit_needs_text_fallback(true, false),
            cfg!(target_os = "linux")
        );
        assert!(!ime_commit_needs_text_fallback(true, true));
        assert!(!ime_commit_needs_text_fallback(false, false));
    }

    #[test]
    fn reconciliation_keeps_panes_until_restore_reaches_ready() {
        let workspace_live = vec![0];
        let restoring = HashSet::from([2]);

        assert_eq!(
            stale_terminal_ids([0, 2, 3], &workspace_live, &restoring),
            vec![3]
        );
    }
}
