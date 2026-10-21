//! Opt-in session overview preview. Fixtures never enter the live notification registry.

use crate::{
    chrome_accent, chrome_bg, chrome_error, chrome_hover, chrome_magenta, chrome_muted,
    chrome_stroke, chrome_text, chrome_warn, icon_button,
};

const MIN_TERMINAL_WIDTH: f32 = 440.0;
const MIN_PANEL_WIDTH: f32 = 340.0;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DisplayMode {
    #[default]
    Docked,
    Floating,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum PanelSide {
    Left,
    #[default]
    Right,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    NeedsYou,
    Finished,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkState {
    Working,
    NeedsYou,
    Finished,
    Unknown,
}

impl WorkState {
    fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::NeedsYou => "Needs you",
            Self::Finished => "Finished",
            Self::Unknown => "Unknown",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Self::Working => chrome_accent(),
            Self::NeedsYou => chrome_warn(),
            Self::Finished => egui::Color32::from_rgb(126, 216, 161),
            Self::Unknown => chrome_muted(),
        }
    }
}

struct SampleSession {
    id: &'static str,
    title: &'static str,
    tool: &'static str,
    host: &'static str,
    cwd: &'static str,
    state: WorkState,
    availability: Option<&'static str>,
    summary: &'static str,
    next: &'static str,
    age: &'static str,
    milestones: &'static [&'static str],
}

const SAMPLES: &[SampleSession] = &[
    SampleSession {
        id: "clipboard",
        title: "Ubuntu clipboard support",
        tool: "Codex",
        host: "build-server",
        cwd: "/work/potty",
        state: WorkState::NeedsYou,
        availability: None,
        summary: "X11 backend implemented; selection tests pass.",
        next: "Approve the integration test on Ubuntu.",
        age: "4m ago",
        milestones: &[
            "Found the Wayland-only clipboard assumption.",
            "Added X11 clipboard and primary-selection support.",
            "Local tests passed; Ubuntu validation needs approval.",
        ],
    },
    SampleSession {
        id: "replay",
        title: "Remote session replay",
        tool: "Claude",
        host: "devbox",
        cwd: "/home/alice/projects/potty",
        state: WorkState::Working,
        availability: None,
        summary: "Checking escape-sequence boundaries after reattach.",
        next: "Run repeated detach and replay cycles.",
        age: "1m ago",
        milestones: &[
            "Reproduced stray bytes after restoring a busy pane.",
            "Added a regression case for partial escape sequences.",
            "Testing the replay boundary fix.",
        ],
    },
    SampleSession {
        id: "packaging",
        title: "macOS package signing and distribution",
        tool: "Codex",
        host: "mac-builder",
        cwd: "/Users/alice/work/potty/packaging/macos",
        state: WorkState::NeedsYou,
        availability: None,
        summary: "Unsigned package builds; signing identity is missing.",
        next: "Choose whether to ship an unsigned preview.",
        age: "12m ago",
        milestones: &[
            "Verified the application bundle and installer layout.",
            "Built the arm64 package successfully.",
            "Signing is blocked on a distribution decision.",
        ],
    },
    SampleSession {
        id: "idle",
        title: "Idle CPU regression",
        tool: "Claude",
        host: "localhost",
        cwd: "/work/potty",
        state: WorkState::Finished,
        availability: None,
        summary: "Redraw loop fixed; idle measurements are clean.",
        next: "Review the patch and test results.",
        age: "23m ago",
        milestones: &[
            "Traced idle CPU usage to repeated redraw requests.",
            "Scheduled UI repaints only when needed.",
            "X11 and Wayland regression checks passed.",
        ],
    },
    SampleSession {
        id: "benchmark",
        title: "Renderer benchmark",
        tool: "Codex",
        host: "gpu-lab",
        cwd: "/srv/benchmarks/potty-renderer",
        state: WorkState::Working,
        availability: Some("Disconnected"),
        summary: "Last report: measuring large scrollback workloads.",
        next: "Check results after reconnecting.",
        age: "2h ago",
        milestones: &[
            "Captured baseline frame timings.",
            "Started the large-scrollback benchmark remotely.",
            "Connection lost; current execution state is unverified.",
        ],
    },
    SampleSession {
        id: "migration",
        title: "Connection profile migration",
        tool: "Claude",
        host: "localhost",
        cwd: "/work/potty",
        state: WorkState::Unknown,
        availability: Some("Pane closed"),
        summary: "Drafted migration notes; no completion was reported.",
        next: "Review the saved notes or dismiss this entry.",
        age: "Yesterday",
        milestones: &[
            "Compared existing profile fields with the proposed format.",
            "Drafted migration notes.",
            "The pane closed without a final agent report.",
        ],
    },
];

#[derive(Default)]
pub struct Overview {
    pub enabled: bool,
    pub open: bool,
    /// A narrow window replaces the terminal view; input must not reach the hidden shell.
    pub covers_terminal: bool,
    pub pointer_over: bool,
    mode: DisplayMode,
    side: PanelSide,
    panel_rect: Option<egui::Rect>,
    filter: Filter,
    expanded: Option<&'static str>,
    dismissed: std::collections::HashSet<&'static str>,
}

impl Overview {
    pub fn demo() -> Self {
        Self {
            enabled: true,
            open: true,
            ..Self::default()
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn closes_on_escape(&self) -> bool {
        self.open && (self.covers_terminal || self.mode == DisplayMode::Floating)
    }

    pub fn blocks_pointer(&self, pos: egui::Pos2) -> bool {
        self.panel_rect.is_some_and(|rect| rect.contains(pos))
    }

    /// Returns true when this frame uses the central area instead of showing terminal panes.
    #[allow(deprecated)]
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        self.covers_terminal = false;
        self.pointer_over = false;
        self.panel_rect = None;
        if !self.enabled || !self.open {
            return false;
        }

        let available = ctx.available_rect();
        let width = available.width();
        let frame = egui::Frame::new()
            .fill(chrome_bg())
            .inner_margin(egui::Margin::symmetric(12, 10));
        let full_width = width < MIN_TERMINAL_WIDTH + MIN_PANEL_WIDTH;
        let side = self.side;
        let floating = self.mode == DisplayMode::Floating && !full_width;
        let response = if full_width {
            egui::CentralPanel::default()
                .frame(frame)
                .show(ctx, |ui| self.contents(ui))
                .response
        } else if floating {
            let bounds = available.shrink(8.0);
            let size = egui::vec2(420.0_f32.min(bounds.width()), bounds.height());
            let pos = match side {
                PanelSide::Left => bounds.left_top(),
                PanelSide::Right => bounds.right_top() - egui::vec2(size.x, 0.0),
            };
            egui::Area::new(egui::Id::new("session-overview-floating"))
                .order(egui::Order::Middle)
                .fixed_pos(pos)
                .default_size(size)
                .constrain_to(bounds)
                .show(ctx, |ui| {
                    frame
                        .stroke(chrome_stroke())
                        .corner_radius(6)
                        .shadow(crate::chrome_shadow())
                        .show(ui, |ui| {
                            let inner = size - egui::vec2(26.0, 22.0);
                            ui.set_min_size(inner);
                            ui.set_max_size(inner);
                            self.contents(ui);
                        });
                })
                .response
        } else {
            let panel = match side {
                PanelSide::Left => egui::SidePanel::left("session-overview-left"),
                PanelSide::Right => egui::SidePanel::right("session-overview"),
            };
            panel
                .default_width(420.0)
                .width_range(MIN_PANEL_WIDTH..=(width - MIN_TERMINAL_WIDTH).min(560.0))
                .resizable(true)
                .frame(frame)
                .show(ctx, |ui| self.contents(ui))
                .response
        };
        self.covers_terminal = full_width;
        self.panel_rect = Some(if floating || full_width {
            response.rect
        } else {
            response.rect.expand(5.0)
        });
        self.pointer_over = ctx
            .pointer_interact_pos()
            .is_some_and(|pos| self.blocks_pointer(pos));
        if !floating {
            let edge = match side {
                PanelSide::Left => [response.rect.right_top(), response.rect.right_bottom()],
                PanelSide::Right => [response.rect.left_top(), response.rect.left_bottom()],
            };
            ctx.layer_painter(response.layer_id)
                .line_segment(edge, chrome_stroke());
        }
        full_width
    }

    fn contents(&mut self, ui: &mut egui::Ui) {
        ui.style_mut().interaction.selectable_labels = false;
        let small_size = (egui::TextStyle::Body.resolve(ui.style()).size - 1.0).max(11.0);
        ui.style_mut().text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::proportional(small_size),
        );
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Sessions")
                    .strong()
                    .color(chrome_text()),
            );
            ui.label(
                egui::RichText::new("Sample data")
                    .small()
                    .color(chrome_magenta()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, "x", "Close session overview").clicked() {
                    self.open = false;
                }
                ui.menu_button("...", |ui| {
                    ui.label(egui::RichText::new("Display").small().color(chrome_muted()));
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.mode, DisplayMode::Docked, "Docked");
                        ui.selectable_value(&mut self.mode, DisplayMode::Floating, "Floating");
                    });
                    ui.label(egui::RichText::new("Side").small().color(chrome_muted()));
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.side, PanelSide::Left, "Left");
                        ui.selectable_value(&mut self.side, PanelSide::Right, "Right");
                    });
                    ui.separator();
                    if ui.button("Reset sample sessions").clicked() {
                        self.dismissed.clear();
                        self.expanded = None;
                        self.filter = Filter::All;
                        ui.close();
                    }
                });
            });
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for (filter, label) in [
                (Filter::All, "All"),
                (Filter::NeedsYou, "Needs you"),
                (Filter::Finished, "Finished"),
            ] {
                let count = SAMPLES.iter().filter(|s| self.matches(s, filter)).count();
                ui.selectable_value(&mut self.filter, filter, format!("{label} {count}"));
            }
        });
        ui.add_space(6.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("session-overview-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if !SAMPLES.iter().any(|s| self.matches(s, self.filter)) {
                    ui.add_space(20.0);
                    ui.label(egui::RichText::new("No sessions").color(chrome_muted()));
                }
                for session in SAMPLES {
                    if self.matches(session, self.filter) {
                        ui.push_id(session.id, |ui| self.row(ui, session));
                    }
                }
            });
    }

    fn matches(&self, session: &SampleSession, filter: Filter) -> bool {
        !self.dismissed.contains(session.id)
            && match filter {
                Filter::All => true,
                Filter::NeedsYou => session.state == WorkState::NeedsYou,
                Filter::Finished => session.state == WorkState::Finished,
            }
    }

    fn row(&mut self, ui: &mut egui::Ui, session: &SampleSession) {
        let expanded = self.expanded == Some(session.id);
        let background = ui.painter().add(egui::Shape::Noop);
        let row = egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(8, 9))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.spacing_mut().interact_size.y = ui.text_style_height(&egui::TextStyle::Body);
                ui.horizontal(|ui| {
                    let status = if session.availability.is_some() {
                        format!("Last: {}", session.state.label().to_lowercase())
                    } else {
                        session.state.label().to_owned()
                    };
                    let status_width = ui.fonts_mut(|fonts| {
                        fonts
                            .layout_no_wrap(
                                status.clone(),
                                egui::TextStyle::Small.resolve(ui.style()),
                                session.state.color(),
                            )
                            .size()
                            .x
                    });
                    let title_width = (ui.available_width() - status_width - 30.0).max(30.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(title_width, ui.text_style_height(&egui::TextStyle::Body)),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(session.title).strong())
                                    .truncate(),
                            )
                        },
                    )
                    .inner
                    .on_hover_text(session.title);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(if expanded { "v" } else { ">" });
                        ui.label(
                            egui::RichText::new(status)
                                .small()
                                .color(session.state.color()),
                        );
                    });
                });
                let location = format!("{} | {} | {}", session.tool, session.host, session.cwd);
                ui.add(
                    egui::Label::new(egui::RichText::new(&location).small().color(chrome_muted()))
                        .truncate(),
                )
                .on_hover_text(location);
                ui.add(egui::Label::new(session.summary).truncate())
                    .on_hover_text(session.summary);
                let next = format!(
                    "{}: {}",
                    if session.state == WorkState::NeedsYou {
                        "Needs you"
                    } else {
                        "Next"
                    },
                    session.next
                );
                ui.add(
                    egui::Label::new(egui::RichText::new(&next).color(
                        if session.state == WorkState::NeedsYou {
                            chrome_warn()
                        } else {
                            chrome_muted()
                        },
                    ))
                    .truncate(),
                )
                .on_hover_text(next);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Agent briefing | {}", session.age))
                            .small()
                            .color(chrome_muted()),
                    );
                    if let Some(availability) = session.availability {
                        ui.label(
                            egui::RichText::new(availability)
                                .small()
                                .color(chrome_error()),
                        );
                    } else if session.state == WorkState::Finished {
                        ui.label(
                            egui::RichText::new("Unreviewed")
                                .small()
                                .color(chrome_warn()),
                        );
                    }
                });
            });
        let response = ui
            .interact(
                row.response.rect,
                ui.id().with("expand"),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(if expanded {
                "Collapse briefing"
            } else {
                "Expand briefing and milestones"
            });
        if response.hovered() || expanded {
            ui.painter().set(
                background,
                egui::Shape::rect_filled(row.response.rect, 0, chrome_hover()),
            );
        }
        if session.state == WorkState::NeedsYou {
            let rect = row.response.rect;
            ui.painter().line_segment(
                [
                    rect.left_top() + egui::vec2(0.0, 9.0),
                    rect.left_bottom() - egui::vec2(0.0, 9.0),
                ],
                egui::Stroke::new(2.0, chrome_warn()),
            );
        }
        if response.clicked() {
            self.expanded = if expanded { None } else { Some(session.id) };
        }
        if expanded {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(8, 8))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Briefing")
                            .small()
                            .strong()
                            .color(chrome_muted()),
                    );
                    ui.add(egui::Label::new(session.summary).wrap());
                    ui.add(egui::Label::new(session.next).wrap());
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Milestones")
                            .small()
                            .strong()
                            .color(chrome_muted()),
                    );
                    for milestone in session.milestones {
                        ui.add(egui::Label::new(format!("- {milestone}")).wrap());
                    }
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_enabled(false, egui::Button::new("Open pane"))
                            .on_disabled_hover_text("Sample session has no linked pane");
                        if ui.button("Dismiss").clicked() {
                            self.dismissed.insert(session.id);
                            self.expanded = None;
                        }
                    });
                });
        }
        ui.separator();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)]
    fn overview_controls_expand_filter_and_dismiss_sample_rows() {
        fn frame(
            ctx: &egui::Context,
            overview: &mut Overview,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(420.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    overview.show(ctx);
                },
            )
        }

        fn click(ctx: &egui::Context, overview: &mut Overview, label: &str) {
            let output = frame(ctx, overview, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.job.text == label
                    {
                        Some(text.visual_bounding_rect().center())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("missing control: {label}"));
            frame(ctx, overview, vec![egui::Event::PointerMoved(pos)]);
            for pressed in [true, false] {
                frame(
                    ctx,
                    overview,
                    vec![egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    }],
                );
            }
        }

        let ctx = egui::Context::default();
        crate::apply_chrome_style(&ctx, 13.0);
        let mut overview = Overview::demo();
        frame(&ctx, &mut overview, vec![]);
        click(&ctx, &mut overview, "Ubuntu clipboard support");
        assert_eq!(overview.expanded, Some("clipboard"));
        click(&ctx, &mut overview, "Needs you 2");
        assert!(overview.filter == Filter::NeedsYou);
        click(&ctx, &mut overview, "Dismiss");
        assert!(overview.dismissed.contains("clipboard"));
        assert_eq!(overview.expanded, None);
        click(&ctx, &mut overview, "Finished 1");
        click(&ctx, &mut overview, "Idle CPU regression");
        assert_eq!(overview.expanded, Some("idle"));
        click(&ctx, &mut overview, "Dismiss");
        let output = frame(&ctx, &mut overview, vec![]);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.job.text == "No sessions")));
        click(&ctx, &mut overview, "...");
        click(&ctx, &mut overview, "Floating");
        click(&ctx, &mut overview, "...");
        click(&ctx, &mut overview, "Left");
        assert!(overview.mode == DisplayMode::Floating);
        assert!(overview.side == PanelSide::Left);
        click(&ctx, &mut overview, "...");
        click(&ctx, &mut overview, "Reset sample sessions");
        assert!(overview.dismissed.is_empty());
        assert!(overview.filter == Filter::All);
    }

    #[test]
    #[allow(deprecated)]
    fn overview_layout_preserves_terminal_space_and_fits_narrow_windows() {
        for (width, height, font_size) in [
            (1280.0, 900.0, 13.0),
            (960.0, 600.0, 13.0),
            (420.0, 760.0, 13.0),
            (320.0, 640.0, 13.0),
            (420.0, 760.0, 22.0),
        ] {
            for (mode, side) in [
                (DisplayMode::Docked, PanelSide::Right),
                (DisplayMode::Docked, PanelSide::Left),
                (DisplayMode::Floating, PanelSide::Right),
                (DisplayMode::Floating, PanelSide::Left),
            ] {
                let ctx = egui::Context::default();
                crate::apply_chrome_style(&ctx, font_size);
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
                let mut overview = Overview::demo();
                overview.mode = mode;
                overview.side = side;
                for expanded in [None, Some("packaging")] {
                    overview.expanded = expanded;
                    for _ in 0..3 {
                        let output = ctx.run(
                            egui::RawInput {
                                screen_rect: Some(screen),
                                ..Default::default()
                            },
                            |ctx| {
                                let covered = overview.show(ctx);
                                assert_eq!(covered, width < MIN_TERMINAL_WIDTH + MIN_PANEL_WIDTH);
                                if !covered {
                                    assert!(
                                        ctx.available_rect().width() >= MIN_TERMINAL_WIDTH - 1.0
                                    );
                                    if mode == DisplayMode::Floating {
                                        assert_eq!(
                                            ctx.available_rect(),
                                            screen,
                                            "floating overview must not resize terminal panes"
                                        );
                                    } else {
                                        assert!(ctx.available_rect().width() < width);
                                    }
                                    let panel = overview.panel_rect.unwrap();
                                    assert!(overview.blocks_pointer(panel.center()));
                                    let exposed = match side {
                                        PanelSide::Left => {
                                            screen.right_center() - egui::vec2(20.0, 0.0)
                                        }
                                        PanelSide::Right => {
                                            screen.left_center() + egui::vec2(20.0, 0.0)
                                        }
                                    };
                                    assert!(!overview.blocks_pointer(exposed));
                                    egui::CentralPanel::default().show(ctx, |_| {});
                                }
                                assert!(
                                    ctx.used_rect().right() <= width + 1.0,
                                    "layout overflow at {width}px / {font_size}pt: {:?}",
                                    ctx.used_rect()
                                );
                            },
                        );
                        for shape in &output.shapes {
                            if let egui::Shape::Text(text) = &shape.shape {
                                assert!(
                                    text.visual_bounding_rect().right() <= width + 1.0,
                                    "text overflows at {width}px / {font_size}pt: {}",
                                    text.galley.job.text
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[allow(deprecated)]
    fn overview_idle_frames_settle_and_closing_restores_the_terminal() {
        for width in [1280.0, 420.0] {
            for mode in [DisplayMode::Docked, DisplayMode::Floating] {
                let ctx = egui::Context::default();
                let mut overview = Overview::demo();
                overview.mode = mode;
                let mut settled = false;
                for _ in 0..12 {
                    let output = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 900.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            overview.show(ctx);
                        },
                    );
                    if crate::egui_repaint_deadline(&output, std::time::Instant::now()).is_none() {
                        settled = true;
                        break;
                    }
                }
                assert!(settled, "overview kept requesting idle repaints");
                overview.open = false;
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    assert!(!overview.show(ctx));
                    assert!(!overview.covers_terminal);
                    assert!(!overview.pointer_over);
                    assert!(overview.panel_rect.is_none());
                });
            }
        }
    }
}
