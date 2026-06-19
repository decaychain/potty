//! potty — GPU terminal spike with a visual menu and a real per-cell renderer.
//!
//!   winit 0.30 (Wayland/KWin) → wgpu 29 surface
//!     ├─ gridr : per-cell terminal renderer (atlas + instanced bg/fg quads)  [pass 1]
//!     └─ egui  : tab bar + pane menu                                          [pass 2]
//!   portable-pty → vte parser → alacritty_terminal grid
//!
//! One live terminal still (home_pane in tab 0); it renders into its pane's rect, the
//! others are placeholders. Next: a PTY+Term per pane.

mod config;
mod gridr;
mod workspace;

use std::ffi::c_void;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use config::Config;
use notify::Watcher;
use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

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
use workspace::{PaneId, Split, Workspace};

const FONT_PX: f32 = 15.0;
const LINE_PX: f32 = 18.0;
/// Top-bar height reserve (logical px) for the initial PTY sizing.
const TOPBAR: f32 = 34.0;

type SharedTerm = Arc<Mutex<Term<EventProxy>>>;

/// Events the terminal raises (from the PTY reader thread) that the main loop must service.
enum UserEvent {
    Wake,
    ReloadConfig,
    /// OSC 52 store (app writes the system clipboard). Targets the clipboard selection.
    ClipboardStore(String),
    /// OSC 52 load: read the clipboard, run the formatter, write the result back to the PTY.
    ClipboardLoad(std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>),
    /// Terminal query response (cursor position, device attributes, …) bound for the PTY.
    PtyWrite(String),
}

/// Terminal event listener: forwards the events we care about to the main loop via the proxy.
/// (PTY reader thread → here → `user_event`.) Replaces VoidListener, which dropped everything —
/// notably OSC 52 clipboard writes and terminal query responses.
#[derive(Clone)]
struct EventProxy(EventLoopProxy<UserEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        // `ty` (clipboard vs primary) is ignored — OSC 52 in practice targets the clipboard.
        let forward = match event {
            Event::ClipboardStore(_ty, text) => Some(UserEvent::ClipboardStore(text)),
            Event::ClipboardLoad(_ty, fmt) => Some(UserEvent::ClipboardLoad(fmt)),
            Event::PtyWrite(text) => Some(UserEvent::PtyWrite(text)),
            _ => None,
        };
        if let Some(ev) = forward {
            let _ = self.0.send_event(ev);
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

fn dims_for(width_px: f32, height_px: f32, cell_w: f32, cell_h: f32) -> Dims {
    Dims {
        cols: ((width_px / cell_w).floor() as usize).max(1),
        rows: ((height_px / cell_h).floor() as usize).max(1),
    }
}

enum Action {
    SelectTab(usize),
    NewTab,
    Split(Split),
    ClosePane,
    CloseTab(usize),
    Focus(PaneId),
    SetFontFamily(Option<String>),
    SetFontSize(f32),
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
        Action::SetFontFamily(_) | Action::SetFontSize(_) => {}
    }
}

// ---------------------------------------------------------------------------
// egui chrome
// ---------------------------------------------------------------------------

#[allow(deprecated)] // ui.close_menu → ui.close migration, see build_ui note
fn pane_menu(ui: &mut egui::Ui, actions: &mut Vec<Action>, for_pane: Option<PaneId>) {
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
}

/// NOTE: egui 0.34 is mid-migration to `run_ui`/`show_inside`/`ui.close`; the panel
/// helpers used here are deprecated-but-working. Migrate when the new Panel API settles.
/// Font family/size picker. Family list comes from the renderer's monospace faces.
fn appearance_menu(
    ui: &mut egui::Ui,
    actions: &mut Vec<Action>,
    families: &[String],
    cur_family: Option<&str>,
    cur_size: f32,
) {
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
}

#[allow(deprecated)]
fn build_ui(
    ctx: &egui::Context,
    ws: &Workspace,
    families: &[String],
    cur_family: Option<&str>,
    cur_size: f32,
    actions: &mut Vec<Action>,
    home_pts: &mut Option<egui::Rect>,
) {
    egui::TopBottomPanel::top("tabbar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            for (i, tab) in ws.tabs.iter().enumerate() {
                if ui.selectable_label(i == ws.active, &tab.title).clicked() {
                    actions.push(Action::SelectTab(i));
                }
                if ws.tabs.len() > 1 && ui.small_button("✕").on_hover_text("Close tab").clicked() {
                    actions.push(Action::CloseTab(i));
                }
            }
            if ui.button("+").on_hover_text("New tab").clicked() {
                actions.push(Action::NewTab);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("☰", |ui| pane_menu(ui, actions, None));
                ui.menu_button("Aa", |ui| {
                    appearance_menu(ui, actions, families, cur_family, cur_size)
                })
                .response
                .on_hover_text("Font & size");
            });
        });
    });

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ctx, |ui| {
            let area = ui.max_rect();
            let focus = ws.active_tab().focus;
            for (id, rect) in ws.leaf_rects(area) {
                let is_home = id == ws.home_pane && ws.active == 0;

                // The live pane must NOT capture clicks — they belong to text selection.
                // Placeholder panes stay clickable (focus) and right-clickable (pane menu).
                if !is_home {
                    let resp = ui.interact(
                        rect,
                        egui::Id::new(("pane", ws.active, id)),
                        egui::Sense::click(),
                    );
                    if resp.clicked() {
                        actions.push(Action::Focus(id));
                    }
                    resp.context_menu(|ui| pane_menu(ui, actions, Some(id)));
                }

                let painter = ui.painter();
                if is_home {
                    *home_pts = Some(rect); // transparent — terminal drawn here in pass 1
                } else {
                    painter.rect_filled(rect, egui::CornerRadius::same(4), egui::Color32::from_gray(18));
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "empty pane",
                        egui::FontId::proportional(13.0),
                        egui::Color32::from_gray(110),
                    );
                }
                let stroke = if id == focus {
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 160, 255))
                } else {
                    egui::Stroke::new(1.0, egui::Color32::from_gray(60))
                };
                painter.rect_stroke(rect, egui::CornerRadius::same(4), stroke, egui::StrokeKind::Inside);
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

    fn render(
        &mut self,
        egui_renderer: &mut egui_wgpu::Renderer,
        textures_delta: &egui::TexturesDelta,
        prims: &[egui::ClippedPrimitive],
        ppp: f32,
        term_rect: Option<egui::Rect>,
        term: Option<&Term<EventProxy>>,
    ) {
        let (sw, sh) = (self.surface_config.width, self.surface_config.height);

        let draw_term = match (term_rect, term) {
            (Some(rect), Some(term)) => {
                let origin = (rect.min.x * ppp, rect.min.y * ppp);
                self.grid.prepare(&self.device, &self.queue, term, origin, (sw as f32, sh as f32));
                Some(rect)
            }
            _ => None,
        };

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                self.surface.configure(&self.device, &self.surface_config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone()).unwrap();
                self.surface.configure(&self.device, &self.surface_config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => panic!("surface validation error"),
        };
        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor { label: None });

        for (id, delta) in &textures_delta.set {
            egui_renderer.update_texture(&self.device, &self.queue, *id, delta);
        }
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [sw, sh],
            pixels_per_point: ppp,
        };
        let egui_cmds = egui_renderer.update_buffers(&self.device, &self.queue, &mut encoder, prims, &screen);

        // Pass 1: clear + terminal cells (scissored to the pane).
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("terminal"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color { r: 0.02, g: 0.02, b: 0.025, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(rect) = draw_term {
                let x = (rect.min.x * ppp).max(0.0) as u32;
                let y = (rect.min.y * ppp).max(0.0) as u32;
                let w = (rect.width() * ppp) as u32;
                let h = (rect.height() * ppp) as u32;
                let w = w.min(sw.saturating_sub(x));
                let h = h.min(sh.saturating_sub(y));
                if w > 0 && h > 0 {
                    pass.set_scissor_rect(x, y, w, h);
                    self.grid.render(&mut pass);
                }
            }
        }

        // Pass 2: egui chrome on top.
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
    term: Option<SharedTerm>,
    writer: Option<Box<dyn Write + Send>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    mods: Modifiers,

    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    egui_renderer: Option<egui_wgpu::Renderer>,
    workspace: Workspace,
    cur_dims: Dims,
    cell_w: f32,
    cell_h: f32,

    config: Config,
    config_path: PathBuf,
    font_families: Vec<String>,
    scale: f32,
    _watcher: Option<notify::RecommendedWatcher>,

    /// Live terminal pane rect in physical px (origin x, y, width, height) — for hit-testing.
    term_px: Option<(f32, f32, f32, f32)>,
    mouse_px: (f64, f64),
    selecting: bool,
    last_click: Option<(Instant, Point)>,
    click_count: u8,
    /// When forwarding mouse events to the app: which button is held, and the last cell
    /// reported (to suppress duplicate motion reports).
    mouse_held: Option<u8>,
    last_report_cell: Option<(i64, i64)>,

    /// Wayland clipboard (clipboard + primary selection) via the app's own seat.
    clipboard: Option<smithay_clipboard::Clipboard>,
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            state: None,
            term: None,
            writer: None,
            master: None,
            mods: Modifiers::default(),
            egui_ctx: egui::Context::default(),
            egui_state: None,
            egui_renderer: None,
            workspace: Workspace::new(),
            cur_dims: Dims { cols: 80, rows: 24 },
            cell_w: 9.0,
            cell_h: 18.0,
            config: Config::default(),
            config_path: config::config_path(),
            font_families: Vec::new(),
            scale: 1.0,
            _watcher: None,
            term_px: None,
            mouse_px: (0.0, 0.0),
            selecting: false,
            last_click: None,
            click_count: 0,
            mouse_held: None,
            last_report_cell: None,
            clipboard: None,
        }
    }

    /// Physical line height for a logical point size.
    fn line_px(&self, size: f32) -> f32 {
        size * 1.2 * self.scale
    }

    /// Apply a (possibly new) config: repaint the palette always; rebuild the font only when
    /// family/size changed (and then force a terminal refit, since the cell box moved).
    fn apply_config(&mut self, new: Config) {
        let font_changed =
            new.font_family != self.config.font_family || new.font_size != self.config.font_size;
        self.config = new;
        // Hot-reload the OSC 52 clipboard policy.
        if let Some(term) = &self.term {
            term.lock().unwrap().set_options(term_config(&self.config));
        }
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
                self.cur_dims = Dims { cols: 0, rows: 0 }; // force refit next redraw
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

    /// Write raw bytes to the PTY.
    fn to_pty(&mut self, bytes: &[u8]) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// DECCKM (application cursor keys) state — decides SS3 vs CSI for cursor/Home/End.
    fn app_cursor(&self) -> bool {
        self.term
            .as_ref()
            .is_some_and(|t| t.lock().unwrap().mode().contains(TermMode::APP_CURSOR))
    }

    /// (alternate screen, alternate-scroll requested) — wheel behaves differently in each.
    fn alt_modes(&self) -> (bool, bool) {
        self.term.as_ref().map_or((false, false), |t| {
            let guard = t.lock().unwrap();
            let m = guard.mode();
            (m.contains(TermMode::ALT_SCREEN), m.contains(TermMode::ALTERNATE_SCROLL))
        })
    }

    /// Scroll the history viewport. No-op on the alternate screen (it has no scrollback).
    fn scroll(&mut self, s: Scroll) {
        if let Some(term) = &self.term {
            let mut t = term.lock().unwrap();
            if t.mode().contains(TermMode::ALT_SCREEN) {
                return;
            }
            t.scroll_display(s);
        }
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }

    /// Mouse wheel (lines > 0 = up/into history). Primary screen scrolls scrollback; the
    /// alternate screen emits arrow keys when the app asked for alternate-scroll (less/vim).
    /// Forwarding to mouse-reporting apps comes with the selection work.
    fn on_wheel(&mut self, lines: i32) {
        let (alt, alt_scroll) = self.alt_modes();
        if alt {
            if alt_scroll {
                let final_byte = if lines > 0 { b'A' } else { b'B' };
                let seq = [0x1b, if self.app_cursor() { b'O' } else { b'[' }, final_byte];
                for _ in 0..lines.unsigned_abs() {
                    self.to_pty(&seq);
                }
            }
        } else {
            self.scroll(Scroll::Delta(lines));
        }
    }

    fn request_redraw(&self) {
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }

    fn display_offset(&self) -> i32 {
        self.term
            .as_ref()
            .map_or(0, |t| t.lock().unwrap().grid().display_offset() as i32)
    }

    /// Is a physical-pixel position inside the live terminal pane? (Press gate — egui's
    /// `wants_pointer_input` is over-eager over the whole window, so we test geometry.)
    fn in_term(&self, px: f64, py: f64) -> bool {
        self.term_px.is_some_and(|(ox, oy, w, h)| {
            let (px, py) = (px as f32, py as f32);
            px >= ox && px < ox + w && py >= oy && py < oy + h
        })
    }

    /// Map a physical-pixel position to a grid point (absolute line, incl. scrollback) and
    /// which half of the cell it falls on. None when outside the live terminal pane.
    fn point_at(&self, px: f64, py: f64) -> Option<(Point, Side)> {
        let (ox, oy, w, h) = self.term_px?;
        let relx = (px as f32 - ox).clamp(0.0, (w - 1.0).max(0.0));
        let rely = (py as f32 - oy).clamp(0.0, (h - 1.0).max(0.0));
        let col = ((relx / self.cell_w) as usize).min(self.cur_dims.cols.saturating_sub(1));
        let vis_row = ((rely / self.cell_h) as i32).min(self.cur_dims.rows as i32 - 1).max(0);
        let line = vis_row - self.display_offset();
        let side = if (relx / self.cell_w).fract() > 0.5 { Side::Right } else { Side::Left };
        Some((Point::new(Line(line), Column(col)), side))
    }

    /// Mouse-reporting flags: (any reporting on, SGR encoding, any-motion 1003, button-drag 1002).
    fn mouse_modes(&self) -> (bool, bool, bool, bool) {
        self.term.as_ref().map_or((false, false, false, false), |t| {
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

    /// 1-based viewport cell (column, row) under a physical-pixel position, for mouse reports.
    fn cell_vp(&self, px: f64, py: f64) -> Option<(i64, i64)> {
        let (ox, oy, w, h) = self.term_px?;
        let relx = (px as f32 - ox).clamp(0.0, (w - 1.0).max(0.0));
        let rely = (py as f32 - oy).clamp(0.0, (h - 1.0).max(0.0));
        Some(((relx / self.cell_w) as i64 + 1, (rely / self.cell_h) as i64 + 1))
    }

    /// Encode a mouse event and write it to the PTY (SGR-1006 when negotiated, else X10).
    fn report_mouse(&mut self, cb: u8, pressed: bool, col: i64, row: i64, sgr: bool) {
        let bytes = if sgr {
            format!("\x1b[<{};{};{}{}", cb, col, row, if pressed { 'M' } else { 'm' }).into_bytes()
        } else {
            // X10: button+32, coords clamped to 223 and offset by 32; release is button 3.
            let b = if pressed { cb } else { 3 };
            vec![0x1b, b'[', b'M', 32 + b, (col.min(223) + 32) as u8, (row.min(223) + 32) as u8]
        };
        self.to_pty(&bytes);
    }

    /// Report motion for the held button (or 3 = no button), deduped to cell changes.
    fn report_motion(&mut self, cb: u8, sgr: bool) {
        let Some((col, row)) = self.cell_vp(self.mouse_px.0, self.mouse_px.1) else { return };
        if self.last_report_cell == Some((col, row)) {
            return;
        }
        self.last_report_cell = Some((col, row));
        self.report_mouse(cb + 32, true, col, row, sgr);
    }

    /// Begin a selection at the mouse, choosing simple/word/line by click count.
    fn start_selection(&mut self) {
        let (px, py) = self.mouse_px;
        let Some((point, side)) = self.point_at(px, py) else { return };

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
        if let Some(term) = &self.term {
            term.lock().unwrap().selection = Some(Selection::new(ty, point, side));
        }
        self.selecting = true;
        self.request_redraw();
    }

    /// Extend the in-progress selection to the mouse.
    fn update_selection(&mut self) {
        let (px, py) = self.mouse_px;
        let Some((point, side)) = self.point_at(px, py) else { return };
        if let Some(term) = &self.term {
            if let Some(sel) = term.lock().unwrap().selection.as_mut() {
                sel.update(point, side);
            }
        }
        self.request_redraw();
    }

    /// Finish selecting; a plain click (empty selection) clears any highlight, otherwise the
    /// selection is published to the primary selection (middle-click paste source on Linux).
    fn end_selection(&mut self) {
        self.selecting = false;
        let mut selected = None;
        if let Some(term) = &self.term {
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
        self.request_redraw();
    }

    fn clear_selection(&mut self) {
        if let Some(term) = &self.term {
            term.lock().unwrap().selection = None;
        }
    }

    /// Copy the active selection to the clipboard and clear it. Returns whether anything was copied.
    fn copy(&mut self) -> bool {
        let text = self
            .term
            .as_ref()
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

    /// Write text to the PTY, wrapped in bracketed-paste markers when the app enabled them.
    fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = self
            .term
            .as_ref()
            .is_some_and(|t| t.lock().unwrap().mode().contains(TermMode::BRACKETED_PASTE));
        let mut out = Vec::new();
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(text.as_bytes());
        if bracketed {
            out.extend_from_slice(b"\x1b[201~");
        }
        self.to_pty(&out);
    }

    fn paste(&mut self) {
        let text = self.clipboard.as_ref().and_then(|cb| cb.load().ok());
        if let Some(t) = text {
            self.paste_text(&t);
        }
    }

    fn on_key(&mut self, ev: &KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.state().control_key();
        let shift = self.mods.state().shift_key();

        // Shift+nav scrolls the history viewport (and is not sent to the PTY).
        if shift {
            match &ev.logical_key {
                Key::Named(NamedKey::PageUp) => return self.scroll(Scroll::PageUp),
                Key::Named(NamedKey::PageDown) => return self.scroll(Scroll::PageDown),
                Key::Named(NamedKey::Home) => return self.scroll(Scroll::Top),
                Key::Named(NamedKey::End) => return self.scroll(Scroll::Bottom),
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
        let cur = |b: u8| -> Vec<u8> { vec![0x1b, if self.app_cursor() { b'O' } else { b'[' }, b] };

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
            // Typing clears any selection and returns the viewport to the prompt.
            self.clear_selection();
            self.scroll(Scroll::Bottom);
            self.to_pty(&out);
        }
    }

    fn fit_terminal(&mut self, dims: Dims) {
        if dims == self.cur_dims {
            return;
        }
        self.cur_dims = dims;
        if let Some(term) = &self.term {
            term.lock().unwrap().resize(dims);
        }
        if let Some(master) = &self.master {
            let _ = master.resize(portable_pty::PtySize {
                rows: dims.rows as u16,
                cols: dims.cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    #[allow(deprecated)] // egui_ctx.run → run_ui migration, see build_ui note
    fn redraw(&mut self) {
        if self.state.is_none() {
            return;
        }
        let window = self.state.as_ref().unwrap().window.clone();

        let raw = self.egui_state.as_mut().unwrap().take_egui_input(&window);
        let mut actions = Vec::new();
        let mut home_pts = None;
        let full = {
            let ws = &self.workspace;
            let families = &self.font_families;
            let cur_family = self.config.font_family.as_deref();
            let cur_size = self.config.font_size;
            self.egui_ctx.run(raw, |ctx| {
                build_ui(ctx, ws, families, cur_family, cur_size, &mut actions, &mut home_pts)
            })
        };
        for a in actions {
            match a {
                Action::SetFontFamily(f) => self.set_font_family(f),
                Action::SetFontSize(s) => self.set_font_size(s),
                a => apply(&mut self.workspace, a),
            }
        }

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

        if let Some(r) = home_pts {
            let dims = dims_for(r.width() * pixels_per_point, r.height() * pixels_per_point, self.cell_w, self.cell_h);
            self.fit_terminal(dims);
        }
        // Remember the live pane's pixel rect for mouse hit-testing.
        self.term_px = home_pts.map(|r| {
            let p = pixels_per_point;
            (r.min.x * p, r.min.y * p, r.width() * p, r.height() * p)
        });

        let prims = self.egui_ctx.tessellate(shapes, pixels_per_point);
        let guard = self.term.as_ref().map(|t| t.lock().unwrap());
        let renderer = self.egui_renderer.as_mut().unwrap();
        if let Some(state) = self.state.as_mut() {
            state.render(
                renderer,
                &textures_delta,
                &prims,
                pixels_per_point,
                home_pts,
                guard.as_deref(),
            );
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

        // Initial grid size from the content area (window minus the top bar).
        let dims = dims_for(
            size.width as f32,
            size.height as f32 - TOPBAR * scale,
            self.cell_w,
            self.cell_h,
        );
        self.cur_dims = dims;

        let term: SharedTerm = Arc::new(Mutex::new(Term::new(
            term_config(&self.config),
            &dims,
            EventProxy(self.proxy.clone()),
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
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut cmd = portable_pty::CommandBuilder::new(shell);
        // Declare what we actually emulate so terminfo-driven apps (mc, ncurses) agree
        // with the escape sequences we send (e.g. application cursor keys).
        cmd.env("TERM", "xterm-256color");
        let _child = pair.slave.spawn_command(cmd).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();

        let reader_term = term.clone();
        let proxy = self.proxy.clone();
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
                        let _ = proxy.send_event(UserEvent::Wake);
                    }
                }
            }
        });

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

        // Clipboard via our own wl_display — uses the app's seat, so it works on KWin and any
        // Wayland compositor without XWayland or data-control protocols. None on non-Wayland.
        self.clipboard = match window.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(h)) => {
                Some(unsafe { smithay_clipboard::Clipboard::new(h.display.as_ptr() as *mut c_void) })
            }
            _ => None,
        };
        let egui_renderer = egui_wgpu::Renderer::new(
            &state.device,
            state.surface_config.format,
            egui_wgpu::RendererOptions::default(),
        );

        self.state = Some(state);
        self.term = Some(term);
        self.writer = Some(writer);
        self.master = Some(pair.master);
        self.egui_state = Some(egui_state);
        self.egui_renderer = Some(egui_renderer);
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Drop the clipboard before the Wayland connection is torn down — its worker thread
        // holds the wl_display, and using it after teardown segfaults.
        self.clipboard = None;
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => self.request_redraw(),
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
            // OSC 52: app reads the clipboard; format the response and send it to the PTY.
            UserEvent::ClipboardLoad(fmt) => {
                let text = self.clipboard.as_ref().and_then(|cb| cb.load().ok());
                if let Some(t) = text {
                    let response = fmt(&t);
                    self.to_pty(response.as_bytes());
                }
            }
            // Terminal query response (DSR, DA, cursor position, …).
            UserEvent::PtyWrite(text) => self.to_pty(text.as_bytes()),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if let Some(window) = self.state.as_ref().map(|s| s.window.clone()) {
            if let Some(es) = self.egui_state.as_mut() {
                let resp = es.on_window_event(&window, &event);
                if resp.repaint {
                    window.request_redraw();
                }
            }
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_px = (position.x, position.y);
                let (report, sgr, motion, drag) = self.mouse_modes();
                if report && !self.mods.state().shift_key() {
                    // Forward motion: held-button drag (1002) or any-motion tracking (1003).
                    match self.mouse_held {
                        Some(cb) if drag || motion => self.report_motion(cb, sgr),
                        None if motion => self.report_motion(3, sgr), // 3 = no button
                        _ => {}
                    }
                } else if self.selecting {
                    self.update_selection();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (report, sgr, ..) = self.mouse_modes();
                let shift = self.mods.state().shift_key();
                let in_term = self.in_term(self.mouse_px.0, self.mouse_px.1);
                let cb = match button {
                    MouseButton::Left => Some(0),
                    MouseButton::Middle => Some(1),
                    MouseButton::Right => Some(2),
                    _ => None,
                };
                // App mouse mode (and no Shift) → forward to the app (Zellij/vim/htop select).
                // Shift bypasses reporting and forces our local selection — the standard override.
                if report && !shift && in_term {
                    if let (Some(cb), Some((col, row))) =
                        (cb, self.cell_vp(self.mouse_px.0, self.mouse_px.1))
                    {
                        let pressed = state == ElementState::Pressed;
                        self.mouse_held = if pressed { Some(cb) } else { None };
                        self.last_report_cell = Some((col, row));
                        self.report_mouse(cb, pressed, col, row, sgr);
                    }
                } else {
                    match (button, state) {
                        (MouseButton::Left, ElementState::Pressed) if in_term => {
                            self.start_selection()
                        }
                        (MouseButton::Left, ElementState::Released) if self.selecting => {
                            self.end_selection()
                        }
                        (MouseButton::Middle, ElementState::Pressed) if in_term => {
                            let text = self.clipboard.as_ref().and_then(|cb| cb.load_primary().ok());
                            if let Some(t) = text {
                                self.paste_text(&t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m,
            WindowEvent::KeyboardInput { event, .. } => self.on_key(&event),
            // IME commit (composed text, or text from an active input-method framework).
            WindowEvent::Ime(Ime::Commit(text)) => self.to_pty(text.as_bytes()),
            WindowEvent::MouseWheel { delta, .. } => {
                // Positive = up / into history. 3 lines per wheel notch; touchpad by pixels.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y.round() as i32) * 3,
                    MouseScrollDelta::PixelDelta(p) => (p.y / self.cell_h.max(1.0) as f64) as i32,
                };
                if lines != 0 {
                    let (report, sgr, ..) = self.mouse_modes();
                    if report && !self.mods.state().shift_key() {
                        // Forward as wheel buttons (64 = up, 65 = down) so the app scrolls.
                        let cb = if lines > 0 { 64 } else { 65 };
                        if let Some((col, row)) = self.cell_vp(self.mouse_px.0, self.mouse_px.1) {
                            for _ in 0..lines.unsigned_abs() {
                                self.report_mouse(cb, true, col, row, sgr);
                            }
                        }
                    } else {
                        self.on_wheel(lines);
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
