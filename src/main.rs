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

mod clip;
mod config;
mod gridr;
mod workspace;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use config::Config;
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
use winit::window::Window;

use gridr::GridRenderer;
use workspace::{PaneId, Split, Workspace, GAP};

const FONT_PX: f32 = 15.0;
const LINE_PX: f32 = 18.0;
/// Top-bar height reserve (logical px) for the initial PTY sizing.
const TOPBAR: f32 = 34.0;

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
    let mut tc = TermConfig::default();
    tc.osc52 = match cfg.osc52.as_str() {
        "disabled" => Osc52::Disabled,
        "paste" => Osc52::OnlyPaste,
        "copy-paste" => Osc52::CopyPaste,
        _ => Osc52::OnlyCopy,
    };
    tc
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

fn dims_for(width_px: f32, height_px: f32, cell_w: f32, cell_h: f32) -> Dims {
    Dims {
        cols: ((width_px / cell_w).floor() as usize).max(1),
        rows: ((height_px / cell_h).floor() as usize).max(1),
    }
}

/// One live terminal: its grid model + the PTY master (for writes and resize) and current size.
/// The reader thread that feeds the grid is detached; it ends when the PTY closes.
struct Terminal {
    term: SharedTerm,
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
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
    Split(Split),
    ClosePane,
    CloseTab(usize),
    Focus(PaneId),
    SetRatio(u64, f32),
    SetFontFamily(Option<String>),
    SetFontSize(f32),
    ShowFontSettings,
}

/// Apply a workspace (tab/pane) action. Font actions are routed separately (they touch App).
fn apply(ws: &mut Workspace, action: Action) {
    match action {
        Action::SelectTab(i) => ws.active = i.min(ws.tabs.len() - 1),
        Action::NewTab => ws.new_tab(),
        Action::Split(s) => ws.split(s),
        Action::ClosePane => ws.close_focused(),
        Action::CloseTab(i) => ws.close_tab(i),
        Action::Focus(id) => ws.focus(id),
        Action::SetRatio(id, ratio) => ws.set_ratio(id, ratio),
        Action::SetFontFamily(_) | Action::SetFontSize(_) | Action::ShowFontSettings => {}
    }
}

// ---------------------------------------------------------------------------
// egui chrome
// ---------------------------------------------------------------------------

/// Shorten a title for a tab label, with an ellipsis when it overflows.
fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The one pane/tab menu, used by both the ☰ button (`for_pane` = None → acts on the focused
/// pane) and a pane's right-click context menu (`for_pane` = that pane). Being the single menu
/// means hiding the tab bar still gives full access via right-click. Font controls live in a
/// separate window (opened from here) rather than cluttering the menu.
///
/// NOTE: egui 0.34 is mid-migration to `ui.close`; `ui.close_menu` is deprecated-but-working.
#[allow(deprecated)]
fn full_menu(ui: &mut egui::Ui, actions: &mut Vec<Action>, for_pane: Option<PaneId>) {
    let focus = |actions: &mut Vec<Action>| {
        if let Some(id) = for_pane {
            actions.push(Action::Focus(id));
        }
    };
    if ui.button("Split right").clicked() {
        focus(actions);
        actions.push(Action::Split(Split::Cols));
        ui.close_menu();
    }
    if ui.button("Split down").clicked() {
        focus(actions);
        actions.push(Action::Split(Split::Rows));
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Close pane").clicked() {
        focus(actions);
        actions.push(Action::ClosePane);
        ui.close_menu();
    }
    if ui.button("New tab").clicked() {
        actions.push(Action::NewTab);
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Font settings…").clicked() {
        actions.push(Action::ShowFontSettings);
        ui.close_menu();
    }
}

/// The floating Font settings window: terminal font size + family picker. Visibility is tied to
/// `open` (its close button flips it off).
fn font_settings_window(
    ctx: &egui::Context,
    open: &mut bool,
    families: &[String],
    cur_family: Option<&str>,
    cur_size: f32,
    actions: &mut Vec<Action>,
) {
    egui::Window::new("Font settings")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Size");
                if ui.button("−").clicked() {
                    actions.push(Action::SetFontSize(cur_size - 1.0));
                }
                ui.label(format!("{cur_size:.0} px"));
                if ui.button("+").clicked() {
                    actions.push(Action::SetFontSize(cur_size + 1.0));
                }
            });
            ui.separator();
            ui.label("Font family");
            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                if ui.selectable_label(cur_family.is_none(), "(default monospace)").clicked() {
                    actions.push(Action::SetFontFamily(None));
                }
                for fam in families {
                    if ui.selectable_label(cur_family == Some(fam.as_str()), fam).clicked() {
                        actions.push(Action::SetFontFamily(Some(fam.clone())));
                    }
                }
            });
        });
}

#[allow(deprecated)]
fn build_ui(
    ctx: &egui::Context,
    ws: &Workspace,
    families: &[String],
    cur_family: Option<&str>,
    cur_size: f32,
    tab_titles: &[String],
    show_font_settings: &mut bool,
    actions: &mut Vec<Action>,
    leaves: &mut Vec<(PaneId, egui::Rect)>,
    dividers: &mut Vec<egui::Rect>,
) {
    // The tab bar only earns its space with more than one tab; otherwise the menu lives on the
    // pane's right-click (shift-right-click when an app has grabbed the mouse).
    if ws.tabs.len() > 1 {
        egui::TopBottomPanel::top("tabbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (i, _tab) in ws.tabs.iter().enumerate() {
                    // Label + close grouped tightly so they read as one tab.
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 3.0;
                        let title = elide(&tab_titles[i], 24);
                        if ui.selectable_label(i == ws.active, title).clicked() {
                            actions.push(Action::SelectTab(i));
                        }
                        let close = egui::Button::new(egui::RichText::new("×").weak()).frame(false);
                        if ui.add(close).on_hover_text("Close tab").clicked() {
                            actions.push(Action::CloseTab(i));
                        }
                    });
                }
                if ui.button("+").on_hover_text("New tab").clicked() {
                    actions.push(Action::NewTab);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("☰", |ui| full_menu(ui, actions, None));
                });
            });
        });
    }

    if *show_font_settings {
        font_settings_window(ctx, show_font_settings, families, cur_family, cur_size, actions);
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
            for (id, rect) in ws.leaf_rects(area) {
                ui.interact(rect, egui::Id::new(("pane", ws.active, id)), egui::Sense::click())
                    .context_menu(|ui| full_menu(ui, actions, Some(id)));
                let stroke = if id == focus {
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 160, 255))
                } else {
                    egui::Stroke::new(1.0, egui::Color32::from_gray(60))
                };
                ui.painter().rect_stroke(
                    rect,
                    egui::CornerRadius::same(4),
                    stroke,
                    egui::StrokeKind::Inside,
                );
                leaves.push((id, rect));
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
                    ui.painter().rect_filled(
                        d.rect,
                        egui::CornerRadius::ZERO,
                        egui::Color32::from_rgb(120, 160, 255),
                    );
                }
                if resp.dragged() {
                    if let Some(p) = resp.interact_pointer_pos() {
                        let ratio = match d.split {
                            Split::Cols => (p.x - d.area.min.x) / (d.area.width() - GAP),
                            Split::Rows => (p.y - d.area.min.y) / (d.area.height() - GAP),
                        };
                        actions.push(Action::SetRatio(d.id, ratio));
                    }
                }
            }
        });
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

const BG_CLEAR: wgpu::Color = wgpu::Color { r: 0.02, g: 0.02, b: 0.025, a: 1.0 };

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

        Self { device, queue, surface, surface_config, instance, grid, window }
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
    ) {
        self.grid.prepare(&self.device, &self.queue, pane, term, origin, screen);
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
        let Some(frame) = self.acquire() else { return };
        let view = frame.texture.create_view(&TextureViewDescriptor::default());

        // Pass 1: one submit per pane, each drawing from its cached buffers (already prepared
        // for dirty panes). The first clears the whole surface — including inter-pane gaps —
        // then each draw is scissored to its rect.
        let mut first = true;
        for (rect, pane) in panes {
            let mut encoder = self
                .device
                .create_command_encoder(&CommandEncoderDescriptor { label: Some("terminal") });
            {
                let load = if first { LoadOp::Clear(BG_CLEAR) } else { LoadOp::Load };
                let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                    label: Some("terminal"),
                    color_attachments: &[Some(RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: Operations { load, store: wgpu::StoreOp::Store },
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
                .create_command_encoder(&CommandEncoderDescriptor { label: Some("clear") });
            encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(BG_CLEAR), store: wgpu::StoreOp::Store },
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
            .create_command_encoder(&CommandEncoderDescriptor { label: Some("egui") });
        for (id, delta) in &textures_delta.set {
            egui_renderer.update_texture(&self.device, &self.queue, *id, delta);
        }
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
                    ops: Operations { load: LoadOp::Load, store: wgpu::StoreOp::Store },
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

        self.queue.submit(egui_cmds.into_iter().chain(std::iter::once(encoder.finish())));
        frame.present();
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

struct App {
    proxy: EventLoopProxy<UserEvent>,
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
    /// Whether the floating Font settings window is shown.
    show_font_settings: bool,

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
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
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
            show_font_settings: false,
            dirty: std::collections::HashSet::new(),
            visible: std::collections::HashSet::new(),
            last_rect: HashMap::new(),
            divider_px: Vec::new(),
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

    /// Spawn a PTY + Term + reader thread for a pane, sized at `dims`.
    fn spawn_terminal(&mut self, id: PaneId, dims: Dims) {
        let term: SharedTerm = Arc::new(Mutex::new(Term::new(
            term_config(&self.config),
            &dims,
            EventProxy { proxy: self.proxy.clone(), pane: id },
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
        let _child = pair.slave.spawn_command(cmd).unwrap();
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
            let _ = proxy.send_event(UserEvent::PaneExited(id));
        });

        self.terms.insert(
            id,
            Terminal {
                term,
                writer,
                master: pair.master,
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
            if !self.terms.contains_key(id) {
                self.spawn_terminal(*id, self.last_dims);
            }
        }
        let removed: Vec<PaneId> =
            self.terms.keys().copied().filter(|id| !live.contains(id)).collect();
        for id in removed {
            self.terms.remove(&id);
            self.dirty.remove(&id);
            self.last_rect.remove(&id);
            if let Some(state) = self.state.as_mut() {
                state.grid.forget_pane(id);
            }
        }
    }

    /// Apply a (possibly new) config: repaint the palette always; rebuild the font only when
    /// family/size changed (and then force a refit of every terminal, since the cell box moved).
    fn apply_config(&mut self, new: Config) {
        let font_changed =
            new.font_family != self.config.font_family || new.font_size != self.config.font_size;
        self.config = new;
        // Hot-reload the OSC 52 clipboard policy on every live terminal.
        for t in self.terms.values() {
            t.term.lock().unwrap().set_options(term_config(&self.config));
        }
        // Palette / font changes affect every pane's render.
        self.dirty.extend(self.terms.keys().copied());
        let (family, size, scale) = (self.config.font_family.clone(), self.config.font_size, self.scale);
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

    /// Write raw bytes to a pane's PTY.
    fn to_pty(&mut self, id: PaneId, bytes: &[u8]) {
        if let Some(t) = self.terms.get_mut(&id) {
            let _ = t.writer.write_all(bytes);
            let _ = t.writer.flush();
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
            (m.contains(TermMode::ALT_SCREEN), m.contains(TermMode::ALTERNATE_SCROLL))
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
                let seq = [0x1b, if self.app_cursor(id) { b'O' } else { b'[' }, final_byte];
                for _ in 0..lines.unsigned_abs() {
                    self.to_pty(id, &seq);
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
        let vis_row = ((rely / self.cell_h) as i32).min(dims.rows as i32 - 1).max(0);
        let line = vis_row - self.display_offset(id);
        let side = if (relx / self.cell_w).fract() > 0.5 { Side::Right } else { Side::Left };
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
        Some(((relx / self.cell_w) as i64 + 1, (rely / self.cell_h) as i64 + 1))
    }

    /// Encode a mouse event and write it to a pane's PTY (SGR-1006 when negotiated, else X10).
    fn report_mouse(&mut self, id: PaneId, cb: u8, pressed: bool, col: i64, row: i64, sgr: bool) {
        let bytes = if sgr {
            format!("\x1b[<{};{};{}{}", cb, col, row, if pressed { 'M' } else { 'm' }).into_bytes()
        } else {
            // X10: button+32, coords clamped to 223 and offset by 32; release is button 3.
            let b = if pressed { cb } else { 3 };
            vec![0x1b, b'[', b'M', 32 + b, (col.min(223) + 32) as u8, (row.min(223) + 32) as u8]
        };
        self.to_pty(id, &bytes);
    }

    /// Report motion for the held button (or 3 = no button) in pane `id`, deduped to cell changes.
    fn report_motion(&mut self, id: PaneId, cb: u8, sgr: bool) {
        let Some((col, row)) = self.cell_vp(id, self.mouse_px.0, self.mouse_px.1) else { return };
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
        let Some((point, side)) = self.point_at(id, px, py) else { return };

        let now = Instant::now();
        let recent = self
            .last_click
            .is_some_and(|(t, p)| now.duration_since(t) < Duration::from_millis(350) && p == point);
        self.click_count = if recent { (self.click_count % 3) + 1 } else { 1 };
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
        let Some((point, side)) = self.point_at(id, px, py) else { return };
        if let Some(term) = self.arc(id) {
            if let Some(sel) = term.lock().unwrap().selection.as_mut() {
                sel.update(point, side);
            }
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
        if let (Some(cb), Some(s)) = (&self.clipboard, selected) {
            if !s.is_empty() {
                cb.store_primary(s);
            }
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
        self.to_pty(id, &out);
    }

    fn paste(&mut self) {
        let text = self.clipboard.as_ref().and_then(|cb| cb.load());
        if let Some(t) = text {
            self.paste_text(self.focus(), &t);
        }
    }

    fn on_key(&mut self, ev: &KeyEvent) {
        if ev.state != ElementState::Pressed || self.terms.is_empty() {
            return;
        }
        let focus = self.focus();
        // On Windows AltGr is reported as Ctrl+Alt; excluding Alt keeps AltGr symbols
        // (`@ { [ ] } \\ | ~ €` on the German layout) out of the Ctrl-shortcut / control-code path.
        let ctrl = self.mods.state().control_key() && !self.mods.state().alt_key();
        let shift = self.mods.state().shift_key();

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
        // Cursor keys: `ESC O x` in application mode, else `ESC [ x`. mc (ncurses) relies on
        // this; vim is lenient and accepts CSI either way — which is why it "worked".
        let cur = |b: u8| -> Vec<u8> { vec![0x1b, if self.app_cursor(focus) { b'O' } else { b'[' }, b] };

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

            // Editing/paging keys (CSI ~ form, independent of DECCKM).
            Key::Named(NamedKey::Insert) => out.extend_from_slice(b"\x1b[2~"),
            Key::Named(NamedKey::Delete) => out.extend_from_slice(b"\x1b[3~"),
            Key::Named(NamedKey::PageUp) => out.extend_from_slice(b"\x1b[5~"),
            Key::Named(NamedKey::PageDown) => out.extend_from_slice(b"\x1b[6~"),

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
            self.to_pty(focus, &out);
        }
    }

    /// Resize a pane's terminal + PTY to `dims` (no-op if unchanged).
    fn fit_terminal(&mut self, id: PaneId, dims: Dims) {
        if let Some(t) = self.terms.get_mut(&id) {
            if t.dims == dims {
                return;
            }
            t.dims = dims;
            self.last_dims = dims;
            t.term.lock().unwrap().resize(dims);
            let _ = t.master.resize(portable_pty::PtySize {
                rows: dims.rows as u16,
                cols: dims.cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
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
                self.terms
                    .get(&t.focus)
                    .map(|x| x.title.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| t.title.clone())
            })
            .collect();

        // Apply the configurable chrome font size to egui's text styles.
        let ui_size = self.config.ui_font_size;
        self.egui_ctx.style_mut(|style| {
            for st in [egui::TextStyle::Body, egui::TextStyle::Button] {
                if let Some(f) = style.text_styles.get_mut(&st) {
                    f.size = ui_size;
                }
            }
        });

        let raw = self.egui_state.as_mut().unwrap().take_egui_input(&window);
        let mut actions = Vec::new();
        let mut leaves: Vec<(PaneId, egui::Rect)> = Vec::new();
        let mut dividers: Vec<egui::Rect> = Vec::new();
        let mut show_font = self.show_font_settings;
        let full = {
            let ws = &self.workspace;
            let families = &self.font_families;
            let cur_family = self.config.font_family.as_deref();
            let cur_size = self.config.font_size;
            self.egui_ctx.run(raw, |ctx| {
                build_ui(
                    ctx, ws, families, cur_family, cur_size, &tab_titles, &mut show_font,
                    &mut actions, &mut leaves, &mut dividers,
                )
            })
        };
        self.show_font_settings = show_font;
        // Remember whether a popup/menu is open so the next frame's clicks don't leak through
        // the menu into the terminal underneath.
        self.menu_open = self.egui_ctx.memory(|m| m.any_popup_open());
        for a in actions {
            match a {
                Action::SetFontFamily(f) => self.set_font_family(f),
                Action::SetFontSize(s) => self.set_font_size(s),
                Action::ShowFontSettings => self.show_font_settings = true,
                a => apply(&mut self.workspace, a),
            }
        }
        // Actions may have created/destroyed panes — keep a terminal per leaf. (The `leaves`
        // rects reflect the pre-action layout for this frame; egui requests a repaint after the
        // interaction, so the new layout lands next frame.)
        self.reconcile_terms();

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
            .map(|(id, r)| (*id, r.min.x * ppp, r.min.y * ppp, r.width() * ppp, r.height() * ppp))
            .collect();
        // A changed visible set means a split/close/tab-switch rearranged panes — their rects
        // may have moved without a dims change, so rebuild all of them this frame.
        let new_visible: std::collections::HashSet<PaneId> = leaves.iter().map(|(id, _)| *id).collect();
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
            .map(|r| (r.min.x * ppp, r.min.y * ppp, r.width() * ppp, r.height() * ppp))
            .collect();

        let prims = self.egui_ctx.tessellate(shapes, ppp);

        // Damage tracking: re-prepare only the visible panes flagged dirty; the rest render from
        // their cached buffers. We lock each dirty pane just long enough to rebuild it.
        let (sw, sh) = {
            let s = self.state.as_ref().unwrap();
            (s.surface_config.width as f32, s.surface_config.height as f32)
        };
        for (id, r) in &leaves {
            if self.dirty.remove(id) {
                if let Some(term) = self.arc(*id) {
                    let origin = (r.min.x * ppp, r.min.y * ppp);
                    let guard = term.lock().unwrap();
                    self.state.as_mut().unwrap().prepare_pane(*id as u64, &guard, origin, (sw, sh));
                }
            }
        }

        let panes: Vec<(egui::Rect, u64)> = leaves.iter().map(|(id, r)| (*r, *id as u64)).collect();
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

        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("potty")
                        .with_inner_size(LogicalSize::new(960.0, 600.0)),
                )
                .unwrap(),
        );

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
            if let Ok(mut w) = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if res.is_ok() {
                    let _ = proxy.send_event(UserEvent::ReloadConfig);
                }
            }) {
                if w.watch(&dir, notify::RecursiveMode::NonRecursive).is_ok() {
                    self._watcher = Some(w);
                }
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
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Drop the clipboard before the Wayland connection is torn down — its worker thread
        // holds the wl_display, and using it after teardown segfaults.
        self.clipboard = None;
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
                    self.to_pty(pane, response.as_bytes());
                }
            }
            // Terminal query response (DSR, DA, cursor position, …).
            UserEvent::PtyWrite(pane, text) => self.to_pty(pane, text.as_bytes()),
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
            UserEvent::PaneExited(id) => {
                self.terms.remove(&id);
                self.dirty.remove(&id);
                if let Some(state) = self.state.as_mut() {
                    state.grid.forget_pane(id as u64);
                }
                self.workspace.remove_pane(id);
                if self.terms.is_empty() {
                    event_loop.exit();
                } else {
                    self.request_redraw();
                }
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
        let withhold_from_egui = match &event {
            WindowEvent::KeyboardInput { .. } => true,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                !self.mods.state().shift_key()
                    && self
                        .pane_at(self.mouse_px.0, self.mouse_px.1)
                        .is_some_and(|id| self.mouse_modes(id).0)
            }
            _ => false,
        };

        if let Some(window) = self.state.as_ref().map(|s| s.window.clone()) {
            if let Some(es) = self.egui_state.as_mut() {
                if !withhold_from_egui {
                    let resp = es.on_window_event(&window, &event);
                    if resp.repaint {
                        window.request_redraw();
                    }
                }
            }
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
                // A menu/popup, the font window, or a divider grab — let egui own this click.
                if self.menu_open
                    || self.show_font_settings
                    || self.on_divider(self.mouse_px.0, self.mouse_px.1)
                {
                    return;
                }
                let shift = self.mods.state().shift_key();
                let Some(id) = self.pane_at(self.mouse_px.0, self.mouse_px.1) else { return };
                let (report, sgr, ..) = self.mouse_modes(id);
                let pressed = state == ElementState::Pressed;
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
                    if let Some(cb) = cb {
                        if let Some((col, row)) = self.cell_vp(id, self.mouse_px.0, self.mouse_px.1) {
                            self.mouse_held = if pressed { Some(cb) } else { None };
                            self.mouse_pane = if pressed { Some(id) } else { None };
                            self.last_report_cell = Some((col, row));
                            self.report_mouse(id, cb, pressed, col, row, sgr);
                        }
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
            WindowEvent::Ime(Ime::Commit(text)) if !self.terms.is_empty() => {
                let focus = self.focus();
                self.to_pty(focus, text.as_bytes());
            }
            WindowEvent::MouseWheel { delta, .. } if !self.menu_open && !self.show_font_settings => {
                // Positive = up / into history. 3 lines per wheel notch; touchpad by pixels.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y.round() as i32) * 3,
                    MouseScrollDelta::PixelDelta(p) => (p.y / self.cell_h.max(1.0) as f64) as i32,
                };
                if lines != 0 {
                    let Some(id) = self.pane_at(self.mouse_px.0, self.mouse_px.1) else { return };
                    let (report, sgr, ..) = self.mouse_modes(id);
                    if report && !self.mods.state().shift_key() {
                        // Forward as wheel buttons (64 = up, 65 = down) so the app scrolls.
                        let cb = if lines > 0 { 64 } else { 65 };
                        if let Some((col, row)) = self.cell_vp(id, self.mouse_px.0, self.mouse_px.1) {
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
                    state.surface.configure(&state.device, &state.surface_config);
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

fn main() {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).unwrap();
}
