//! Session overview. Demo fixtures never enter the live session registry.

use crate::{
    chrome_accent, chrome_bg, chrome_error, chrome_hover, chrome_magenta, chrome_muted,
    chrome_stroke, chrome_text, chrome_warn, icon_button,
};

const MIN_TERMINAL_WIDTH: f32 = 440.0;
const MIN_PANEL_WIDTH: f32 = 340.0;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum View {
    #[default]
    Current,
    Saved,
    Recent,
}

struct Editor {
    owner: String,
    id: String,
    expected: u64,
    annotation: potty::contexts::Annotation,
    links: String,
    pending: bool,
    pending_since: Option<std::time::Instant>,
}

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
    ResponseReady,
    Ended,
    Saved,
}

impl WorkState {
    fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::NeedsYou => "Needs you",
            Self::Finished => "Finished",
            Self::Unknown => "Unknown",
            Self::ResponseReady => "Response ready",
            Self::Ended => "Ended",
            Self::Saved => "Saved context",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Self::Working => chrome_accent(),
            Self::NeedsYou => chrome_warn(),
            Self::Finished | Self::ResponseReady => egui::Color32::from_rgb(126, 216, 161),
            Self::Unknown | Self::Ended | Self::Saved => chrome_muted(),
        }
    }
}

struct SampleSession<'a> {
    id: &'a str,
    title: &'a str,
    tool: &'a str,
    host: &'a str,
    cwd: &'a str,
    state: WorkState,
    availability: Option<&'a str>,
    summary: &'a str,
    newer_prompt: Option<&'a str>,
    next: &'a str,
    age: &'a str,
    milestones: &'a [&'a str],
}

const SAMPLES: &[SampleSession<'static>] = &[
    SampleSession {
        id: "clipboard",
        title: "Ubuntu clipboard support",
        tool: "Codex",
        host: "build-server",
        cwd: "/work/potty",
        state: WorkState::NeedsYou,
        availability: None,
        summary: "X11 backend implemented; selection tests pass.",
        newer_prompt: None,
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
        newer_prompt: None,
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
        newer_prompt: None,
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
        newer_prompt: None,
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
        newer_prompt: None,
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
        newer_prompt: None,
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
    expanded: Option<String>,
    dismissed: std::collections::HashSet<String>,
    demo: bool,
    show_ended: bool,
    focused_pane: Option<u64>,
    reveal_focus: bool,
    was_open: bool,
    pub store: potty::agent::Store,
    pub locations: std::collections::HashMap<String, u64>,
    pub actions: Vec<OverviewAction>,
    view: View,
    search: String,
    search_focused: bool,
    editor: Option<Editor>,
    editor_visible: bool,
    forget: Option<(String, String)>,
    pub context_error: Option<String>,
    pub snapshots: std::collections::BTreeMap<String, potty::contexts::Snapshot>,
    pub context_refs: std::collections::HashMap<String, (String, String)>,
    pub writable_owners: std::collections::HashSet<String>,
    contexts_enabled: bool,
}

pub enum OverviewAction {
    Open(String),
    Dismiss(potty::agent::Record),
    EditContext(potty::contexts::Message),
    OpenContext { owner: String, id: String },
    RefreshContexts,
}

impl Overview {
    pub fn demo() -> Self {
        Self {
            demo: true,
            enabled: true,
            open: true,
            ..Self::default()
        }
    }

    pub fn live() -> Self {
        Self {
            enabled: true,
            contexts_enabled: true,
            ..Self::default()
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open && self.contexts_enabled {
            self.actions.push(OverviewAction::RefreshContexts);
        }
    }

    pub fn set_contexts_enabled(&mut self, enabled: bool) {
        self.contexts_enabled = enabled;
        if !enabled {
            self.view = View::Current;
            self.search.clear();
            self.search_focused = false;
            self.editor = None;
            self.editor_visible = false;
            self.forget = None;
            self.context_error = None;
            self.snapshots.clear();
            self.context_refs.clear();
            self.writable_owners.clear();
            self.actions
                .retain(|a| matches!(a, OverviewAction::Open(_) | OverviewAction::Dismiss(_)));
        }
    }

    pub fn text_input_active(&self) -> bool {
        self.open && (self.editor_visible || self.search_focused)
    }

    pub fn receive_snapshot(&mut self, snapshot: potty::contexts::Snapshot) {
        if !self.contexts_enabled {
            return;
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|e| e.owner == snapshot.owner && !snapshot.contexts.contains_key(&e.id))
        {
            self.editor = None;
        }
        if let Some(editor) = &self.editor
            && editor.pending
            && editor.owner == snapshot.owner
            && snapshot
                .contexts
                .get(&editor.id)
                .is_some_and(|c| c.edited > editor.expected && c.annotation == editor.annotation)
        {
            self.editor = None;
        }
        self.snapshots.insert(snapshot.owner.clone(), snapshot);
    }

    pub fn context_failed(&mut self, error: String) {
        self.context_error = Some(error);
        if let Some(editor) = &mut self.editor {
            editor.pending = false;
            editor.pending_since = None;
        }
    }

    pub fn sync_focus(&mut self, pane: u64) {
        if self.focused_pane != Some(pane) {
            self.focused_pane = Some(pane);
            self.reveal_focus = true;
        }
    }

    pub fn button_visible(&self) -> bool {
        self.enabled
            && (self.open
                || if self.demo {
                    SAMPLES.iter().any(|session| {
                        session.state != WorkState::Ended && !self.dismissed.contains(session.id)
                    })
                } else {
                    self.snapshots
                        .values()
                        .any(|s| s.contexts.values().any(|c| c.annotation.kept))
                        || self.store.records.values().any(|record| {
                            record.visible() && record.state != potty::agent::State::Ended
                        })
                })
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
        self.editor_visible = false;
        self.panel_rect = None;
        if self.open && !self.was_open {
            self.reveal_focus = true;
        }
        self.was_open = self.open;
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
        if self.editor.as_ref().is_some_and(|e| !e.pending)
            && ui.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.editor = None;
        }
        if let Some(started) = self.editor.as_ref().and_then(|e| e.pending_since) {
            let timeout = std::time::Duration::from_secs(15);
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                self.context_failed(
                    "Save confirmation not received. Refresh before retrying.".into(),
                );
            } else {
                ui.ctx().request_repaint_after(timeout - elapsed);
            }
        }
        ui.style_mut().interaction.selectable_labels = false;
        let small_size = (egui::TextStyle::Body.resolve(ui.style()).size - 1.0).max(11.0);
        ui.style_mut().text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::proportional(small_size),
        );
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            let title_width = (ui.available_width() - 60.0).max(80.0);
            ui.allocate_ui_with_layout(
                egui::vec2(title_width, 24.0),
                egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.spacing_mut().button_padding.x = 4.0;
                    ui.label(
                        egui::RichText::new("Sessions")
                            .strong()
                            .color(chrome_text()),
                    );
                    if self.demo {
                        ui.label(
                            egui::RichText::new("Sample data")
                                .small()
                                .color(chrome_magenta()),
                        );
                    } else if self.contexts_enabled {
                        ui.add_space(12.0);
                        for (view, label) in [
                            (View::Current, "Current"),
                            (View::Saved, "Saved"),
                            (View::Recent, "Recent"),
                        ] {
                            if ui
                                .selectable_value(
                                    &mut self.view,
                                    view,
                                    egui::RichText::new(label).small(),
                                )
                                .clicked()
                            {
                                self.actions.push(OverviewAction::RefreshContexts);
                            }
                        }
                    }
                },
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
                    if !self.demo {
                        ui.checkbox(&mut self.show_ended, "Show ended sessions");
                    }
                    if self.demo && ui.button("Reset sample sessions").clicked() {
                        self.dismissed.clear();
                        self.expanded = None;
                        self.filter = Filter::All;
                        ui.close();
                    }
                });
            });
        });
        ui.add_space(6.0);
        if !self.demo {
            if self.view != View::Current {
                self.search_focused = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Search contexts")
                            .desired_width(f32::INFINITY),
                    )
                    .has_focus();
            } else {
                self.search_focused = false;
            }
        }
        if let Some(error) = self.context_error.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(error).small().color(chrome_error()));
                if icon_button(ui, "x", "Dismiss storage error").clicked() {
                    self.context_error = None;
                }
            });
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|e| e.pending && !self.writable_owners.contains(&e.owner))
        {
            self.context_failed("Connection lost before the save was confirmed".into());
        }
        if self.view == View::Current {
            ui.horizontal_wrapped(|ui| {
                for (filter, label) in [
                    (Filter::All, "All"),
                    (Filter::NeedsYou, "Needs you"),
                    (
                        Filter::Finished,
                        if self.demo { "Finished" } else { "Ready" },
                    ),
                ] {
                    let count = if self.demo {
                        SAMPLES.iter().filter(|s| self.matches(s, filter)).count()
                    } else {
                        self.store
                            .records
                            .values()
                            .filter(|r| self.matches_live(r, filter))
                            .count()
                    };
                    ui.selectable_value(&mut self.filter, filter, format!("{label} {count}"));
                }
            });
        }
        ui.add_space(6.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("session-overview-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if !self.demo {
                    if self.view == View::Current {
                        self.live_rows(ui);
                    } else {
                        self.saved_rows(ui);
                    }
                    return;
                }
                if !SAMPLES.iter().any(|s| self.matches(s, self.filter)) {
                    ui.add_space(20.0);
                    ui.label(egui::RichText::new("No sessions").color(chrome_muted()));
                }
                for session in SAMPLES {
                    if self.matches(session, self.filter) {
                        ui.push_id(session.id, |ui| {
                            self.row(ui, session, "Agent briefing", false, None)
                        });
                    }
                }
            });
    }

    fn matches(&self, session: &SampleSession, filter: Filter) -> bool {
        !self.dismissed.contains(session.id)
            && match filter {
                Filter::All => true,
                Filter::NeedsYou => session.state == WorkState::NeedsYou,
                Filter::Finished => matches!(
                    session.state,
                    WorkState::Finished | WorkState::ResponseReady
                ),
            }
    }

    fn saved_rows(&mut self, ui: &mut egui::Ui) {
        let query = self.search.to_lowercase();
        let mut contexts: Vec<_> = self
            .snapshots
            .iter()
            .flat_map(|(owner, s)| s.contexts.values().map(move |c| (owner.clone(), c.clone())))
            .filter(|(_, c)| self.view != View::Saved || c.annotation.kept)
            .filter(|(_, c)| {
                query.is_empty()
                    || format!(
                        "{} {} {} {}",
                        c.title(),
                        c.annotation.notes,
                        c.annotation.links.join(" "),
                        c.sessions
                            .iter()
                            .map(|r| format!(
                                "{} {} {} {}",
                                r.host,
                                r.cwd,
                                r.session,
                                r.briefing.as_ref().map_or("", |b| b.summary.as_str())
                            ))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .collect();
        contexts.sort_by_key(|(_, c)| std::cmp::Reverse(c.updated));
        if contexts.is_empty() {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new(if self.search.is_empty() {
                    if self.view == View::Saved {
                        "No saved contexts"
                    } else {
                        "No recent contexts"
                    }
                } else {
                    "No matching contexts"
                })
                .color(chrome_muted()),
            );
        }
        for (owner, context) in contexts {
            let key = format!("context:{owner}:{}", context.id);
            let live = self.context_refs.iter().find_map(|(key, (o, id))| {
                (o == &owner && id == &context.id)
                    .then(|| self.locations.get(key).copied())
                    .flatten()
            });
            if let Some(pane) = live {
                self.locations.insert(key.clone(), pane);
            }
            self.context_refs
                .insert(key.clone(), (owner.clone(), context.id.clone()));
            let r = context.sessions.last().unwrap();
            let briefing = r.briefing.as_ref();
            let age = format!(
                "{}m ago",
                potty::agent::now().saturating_sub(context.updated) / 60_000_000_000
            );
            let milestones: Vec<_> = briefing
                .map(|b| b.milestones.iter().map(String::as_str).collect())
                .unwrap_or_default();
            let row = SampleSession {
                id: &key,
                title: context.title(),
                tool: match r.tool {
                    potty::notify::Tool::Claude => "Claude",
                    potty::notify::Tool::Codex => "Codex",
                    _ => "Agent",
                },
                host: &r.host,
                cwd: &r.cwd,
                state: WorkState::Saved,
                availability: None,
                summary: briefing.map_or(r.prompt.as_str(), |b| b.summary.as_str()),
                newer_prompt: None,
                next: briefing.map_or("", |b| b.next.as_str()),
                age: &age,
                milestones: &milestones,
            };
            let response = ui.push_id(&key, |ui| {
                self.row(ui, &row, "Last recorded briefing", briefing.is_none(), None)
            });
            if self.reveal_focus
                && self.focused_pane.is_some()
                && self.locations.get(&key).copied() == self.focused_pane
            {
                response.inner.scroll_to_me(None);
                self.reveal_focus = false;
            }
        }
        self.reveal_focus = false;
    }

    fn context_details(&mut self, ui: &mut egui::Ui, key: &str) {
        if !self.contexts_enabled {
            return;
        }
        let Some((owner, id)) = self.context_refs.get(key).cloned() else {
            return;
        };
        let Some(context) = self
            .snapshots
            .get(&owner)
            .and_then(|s| s.contexts.get(&id))
            .cloned()
        else {
            return;
        };
        let writable = self.writable_owners.contains(&owner);
        ui.separator();
        if self
            .editor
            .as_ref()
            .is_some_and(|e| e.owner == owner && e.id == id)
        {
            self.context_editor(ui, key, owner, id);
            return;
        }
        if !context.annotation.notes.is_empty() {
            ui.label(
                egui::RichText::new("Notes")
                    .small()
                    .strong()
                    .color(chrome_muted()),
            );
            egui::ScrollArea::vertical()
                .id_salt(("notes", key))
                .max_height(120.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(&context.annotation.notes).wrap());
                });
        }
        for link in &context.annotation.links {
            // Only HTTP(S) links accepted by the store. No shell commands or file URLs.
            let mut label = egui::text::LayoutJob::simple(
                link.clone(),
                egui::TextStyle::Body.resolve(ui.style()),
                chrome_accent(),
                ui.available_width(),
            );
            label.wrap.max_rows = 1;
            label.wrap.break_anywhere = true;
            ui.add(egui::Hyperlink::from_label_and_url(label, link))
                .on_hover_text(link);
        }
        if let Some(record) = context.sessions.last() {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Agent session")
                    .small()
                    .color(chrome_muted()),
            );
            ui.horizontal(|ui| {
                if ui
                    .small_button("Copy ID")
                    .on_hover_text("Copy agent session ID")
                    .clicked()
                {
                    ui.ctx().copy_text(record.session.clone());
                }
                ui.add(
                    egui::Label::new(format!("{:?}: {}", record.tool, record.session)).truncate(),
                )
                .on_hover_text(&record.session);
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !record.cwd.is_empty(),
                        egui::Button::new("Copy path").small(),
                    )
                    .on_hover_text("Copy recorded working directory")
                    .clicked()
                {
                    ui.ctx().copy_text(record.cwd.clone());
                }
                ui.add(egui::Label::new(&record.cwd).truncate())
                    .on_hover_text(&record.cwd);
            });
            ui.add_space(6.0);
        }
        ui.horizontal_wrapped(|ui| {
            if self.view != View::Current
                && !self.locations.contains_key(key)
                && ui
                    .add_enabled(writable, egui::Button::new("Open shell"))
                    .on_hover_text("Open a new shell in the recorded directory")
                    .clicked()
            {
                self.actions.push(OverviewAction::OpenContext {
                    owner: owner.clone(),
                    id: id.clone(),
                });
            }
            if !context.annotation.kept
                && ui
                    .add_enabled(writable, egui::Button::new("Keep"))
                    .clicked()
            {
                let mut annotation = context.annotation.clone();
                annotation.kept = true;
                self.actions.push(OverviewAction::EditContext(
                    potty::contexts::Message::Edit {
                        owner: owner.clone(),
                        id: id.clone(),
                        expected: context.edited,
                        annotation,
                    },
                ));
            }
            if ui
                .add_enabled(writable, egui::Button::new("Edit notes"))
                .clicked()
            {
                self.editor = Some(Editor {
                    owner: owner.clone(),
                    id: id.clone(),
                    expected: context.edited,
                    annotation: context.annotation.clone(),
                    links: context.annotation.links.join("\n"),
                    pending: false,
                    pending_since: None,
                });
                self.editor_visible = true;
                self.context_error = None;
            }
            if context.annotation.kept {
                ui.label(egui::RichText::new("Kept").small().color(chrome_muted()));
            }
            if !writable {
                ui.label(
                    egui::RichText::new("Disconnected")
                        .small()
                        .color(chrome_muted()),
                );
            }
            if self.view != View::Current
                && ui
                    .add_enabled(writable, egui::Button::new("Forget..."))
                    .clicked()
            {
                self.forget = Some((owner.clone(), id.clone()));
            }
        });
        if self.forget.as_ref() == Some(&(owner.clone(), id.clone())) {
            ui.label(
                egui::RichText::new("Forget this context and its notes?")
                    .small()
                    .color(chrome_warn()),
            );
            ui.horizontal(|ui| {
                if ui.button("Forget").clicked() {
                    self.actions.push(OverviewAction::EditContext(
                        potty::contexts::Message::Forget {
                            owner: owner.clone(),
                            id: id.clone(),
                            expected: context.edited,
                        },
                    ));
                    self.forget = None;
                }
                if ui.button("Cancel").clicked() {
                    self.forget = None;
                }
            });
        }
    }

    fn context_editor(&mut self, ui: &mut egui::Ui, key: &str, owner: String, id: String) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.owner != owner || editor.id != id {
            return;
        }
        self.editor_visible = true;
        ui.add_space(6.0);
        let mut cancel = false;
        ui.add_enabled_ui(!editor.pending, |ui| {
            ui.label(egui::RichText::new("Name").small().color(chrome_muted()));
            ui.add(
                egui::TextEdit::singleline(&mut editor.annotation.name)
                    .desired_width(f32::INFINITY)
                    .char_limit(120),
            );
            ui.label(egui::RichText::new("Notes").small().color(chrome_muted()));
            egui::ScrollArea::vertical()
                .id_salt(("edit-notes", key))
                .max_height(110.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut editor.annotation.notes)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .char_limit(8192),
                    );
                });
            ui.label(egui::RichText::new("Links").small().color(chrome_muted()));
            egui::ScrollArea::vertical()
                .id_salt(("edit-links", key))
                .max_height(65.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut editor.links)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .char_limit(32768),
                    );
                });
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    editor.annotation.links = editor
                        .links
                        .lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    editor.annotation.normalize();
                    editor.annotation.kept = true;
                    if editor.annotation.valid() {
                        editor.pending = true;
                        editor.pending_since = Some(std::time::Instant::now());
                        self.actions.push(OverviewAction::EditContext(
                            potty::contexts::Message::Edit {
                                owner,
                                id,
                                expected: editor.expected,
                                annotation: editor.annotation.clone(),
                            },
                        ));
                    } else {
                        self.context_error = Some(
                            "Use a short name, notes up to 8 KB, and up to 16 HTTP(S) links."
                                .into(),
                        );
                    }
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if editor.pending {
            ui.label(
                egui::RichText::new("Saving...")
                    .small()
                    .color(chrome_muted()),
            );
        }
        if cancel {
            self.editor = None;
        }
    }

    fn sorted_records(&self) -> Vec<potty::agent::Record> {
        let mut records: Vec<_> = self
            .store
            .records
            .values()
            .filter(|r| self.matches_live(r, self.filter))
            .cloned()
            .collect();
        // Resolved panes group local and remote sessions alike. Detached sessions use their
        // original transport identity so equal remote pane IDs never group unrelated hosts.
        records.sort_by_cached_key(|r| {
            let key = r.key();
            match self.locations.get(&key) {
                Some(pane) => (0, *pane, String::new(), String::new(), None, key),
                None => (1, 0, r.origin.clone(), r.host.clone(), r.pane, key),
            }
        });
        records
    }

    fn live_rows(&mut self, ui: &mut egui::Ui) {
        use potty::agent::State;
        let records = self.sorted_records();
        let reveal = self
            .reveal_focus
            .then(|| {
                records.iter().find_map(|r| {
                    let key = r.key();
                    self.locations
                        .get(&key)
                        .copied()
                        .filter(|pane| Some(*pane) == self.focused_pane)
                        .map(|_| key)
                })
            })
            .flatten();
        if records.is_empty() {
            ui.add_space(20.0);
            ui.label(egui::RichText::new("No sessions").color(chrome_muted()));
        }
        for r in records {
            let key = r.key();
            let custom_title = self
                .context_refs
                .get(&key)
                .and_then(|(owner, id)| self.snapshots.get(owner)?.contexts.get(id))
                .map(|c| c.annotation.name.clone())
                .filter(|s| !s.is_empty());
            let has_briefing = r.briefing.is_some();
            let prompt_only =
                !has_briefing && (r.response_at <= r.prompt_at || r.response.is_empty());
            let source = if has_briefing {
                if r.prompt_at > r.briefing_at {
                    "Agent briefing (older than prompt)"
                } else {
                    "Agent briefing"
                }
            } else {
                if r.response_at > r.prompt_at && !r.response.is_empty() {
                    "Latest response (no briefing)"
                } else {
                    "Latest prompt (no briefing)"
                }
            };
            let ts = if has_briefing {
                r.briefing_at
            } else if r.response_at > r.prompt_at && !r.response.is_empty() {
                r.response_at
            } else if !r.prompt.is_empty() {
                r.prompt_at
            } else {
                r.state_at
            };
            let age = format!(
                "{}m ago",
                potty::agent::now().saturating_sub(ts) / 60_000_000_000
            );
            let fallback_title = if r.prompt.is_empty() {
                "Agent session".to_owned()
            } else {
                potty::agent::excerpt(
                    r.prompt.trim().lines().next().unwrap_or("Agent session"),
                    120,
                )
            };
            let summary = r
                .briefing
                .as_ref()
                .map(|b| b.summary.as_str())
                .unwrap_or_else(|| {
                    if r.response_at > r.prompt_at && !r.response.is_empty() {
                        &r.response
                    } else {
                        &r.prompt
                    }
                });
            let next = if r.state == State::NeedsYou {
                "Approval requested in the agent"
            } else {
                r.briefing
                    .as_ref()
                    .map(|b| b.next.as_str())
                    .unwrap_or(match r.state {
                        State::NeedsYou => "Approval requested",
                        State::ResponseReady => "Review the response",
                        State::Ended => "Session ended",
                        _ => "No next step reported",
                    })
            };
            let milestones: Vec<&str> = r
                .briefing
                .as_ref()
                .map(|b| b.milestones.iter().map(String::as_str).collect())
                .unwrap_or_default();
            let row = SampleSession {
                id: &key,
                title: custom_title.as_deref().unwrap_or_else(|| {
                    r.briefing
                        .as_ref()
                        .map(|b| b.title.as_str())
                        .unwrap_or(&fallback_title)
                }),
                tool: match r.tool {
                    potty::notify::Tool::Codex => "Codex",
                    potty::notify::Tool::Claude => "Claude",
                    _ => "Agent",
                },
                host: &r.host,
                cwd: &r.cwd,
                state: match r.state {
                    State::Working => WorkState::Working,
                    State::NeedsYou => WorkState::NeedsYou,
                    State::ResponseReady => WorkState::ResponseReady,
                    State::Ended => WorkState::Ended,
                    State::Unknown => WorkState::Unknown,
                },
                availability: (r.state != State::Ended && !self.locations.contains_key(&key))
                    .then_some("Pane unavailable"),
                summary,
                newer_prompt: (has_briefing && r.prompt_at > r.briefing_at).then_some(&r.prompt),
                next,
                age: &age,
                milestones: &milestones,
            };
            let background = r
                .background
                .as_ref()
                .filter(|b| b.tasks > 0 && r.state != State::Ended)
                .map(|b| (b, r.background_at));
            let response = ui.push_id(&key, |ui| {
                self.row(ui, &row, source, prompt_only, background)
            });
            if reveal.as_deref() == Some(&key) {
                response.inner.scroll_to_me(None);
                self.reveal_focus = false;
            }
        }
        // Do not keep pulling the scroll position toward a filtered-out or missing session.
        self.reveal_focus = false;
    }

    fn matches_live(&self, record: &potty::agent::Record, filter: Filter) -> bool {
        record.visible()
            && (self.show_ended || record.state != potty::agent::State::Ended)
            && live_matches(record.state, filter)
    }

    fn row(
        &mut self,
        ui: &mut egui::Ui,
        session: &SampleSession,
        source: &str,
        prompt_only: bool,
        background_tasks: Option<(&potty::agent::Background, u64)>,
    ) -> egui::Response {
        let expanded = self.expanded.as_deref() == Some(session.id);
        let linked = self.locations.get(session.id).copied();
        let focused = linked.is_some() && linked == self.focused_pane;
        let background = ui.painter().add(egui::Shape::Noop);
        let builder = egui::UiBuilder::new().sense(egui::Sense::click());
        let item = ui.scope_builder(builder, |ui| {
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
                    if prompt_only {
                        prompt_excerpt(ui, session.summary);
                    } else {
                        ui.add(egui::Label::new(session.summary).truncate())
                            .on_hover_text(session.summary);
                    }
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
                    if let Some((background, at)) = background_tasks {
                        let label = format!(
                            "Background: {} {}{}",
                            background.tasks,
                            if background.tasks == 1 { "task" } else { "tasks" },
                            if background.monitors > 0 {
                                format!(" ({} {})", background.monitors,
                                    if background.monitors == 1 { "monitor" } else { "monitors" })
                            } else { String::new() },
                        );
                        ui.label(egui::RichText::new(label).small().color(crate::chrome_accent()))
                            .on_hover_text(format!(
                                "Last reported at turn end, {}m ago. Refreshed on the next turn end; not a live process check.",
                                potty::agent::now().saturating_sub(at) / 60_000_000_000,
                            ));
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{source} | {}", session.age))
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
            if expanded {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(8, 8))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(source)
                                .small()
                                .strong()
                                .color(chrome_muted()),
                        );
                        if !prompt_only {
                            ui.add(egui::Label::new(session.summary).wrap());
                        }
                        ui.add(egui::Label::new(session.next).wrap());
                        ui.add_space(6.0);
                        if !session.milestones.is_empty() { ui.label(
                            egui::RichText::new("Milestones")
                                .small()
                                .strong()
                                .color(chrome_muted()),
                        ); }
                        for milestone in session.milestones {
                            ui.add(egui::Label::new(format!("- {milestone}")).wrap());
                        }
                        if let Some(prompt) = session.newer_prompt {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new("Latest prompt (hook excerpt)")
                                    .small()
                                    .color(chrome_muted()),
                            );
                            prompt_excerpt(ui, prompt);
                        }
                        ui.add_space(6.0);
                        if !self.demo { self.context_details(ui, session.id); }
                        ui.horizontal_wrapped(|ui| {
                            if (self.demo || self.view == View::Current) && ui.button("Dismiss").clicked() {
                                if self.demo {
                                    self.dismissed.insert(session.id.to_owned());
                                } else if let Some(record) = self.store.records.get_mut(session.id) {
                                    record.dismissed = record.revision();
                                    self.actions.push(OverviewAction::Dismiss(record.clone()));
                                }
                                self.expanded = None;
                            }
                        });
                    });
            }
        });
        let response = item
            .response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(if linked.is_some() {
                "Switch to pane"
            } else if expanded {
                "Collapse briefing"
            } else {
                "Expand briefing and milestones"
            });
        if response.clicked() {
            if linked.is_some() {
                self.actions
                    .push(OverviewAction::Open(session.id.to_owned()));
            }
            let editing = self.editor.as_ref().is_some_and(|e| {
                self.context_refs.get(session.id) == Some(&(e.owner.clone(), e.id.clone()))
            });
            self.expanded = if expanded && (focused || linked.is_none()) && !editing {
                None
            } else {
                Some(session.id.to_owned())
            };
        }
        let item_rect = response.rect;
        if response.hovered() || focused {
            ui.painter().set(
                background,
                egui::Shape::rect_filled(item_rect, 0, chrome_hover()),
            );
        }
        if focused {
            ui.painter()
                .rect_stroke(item_rect, 0, active_item_stroke(), egui::StrokeKind::Inside);
        }
        ui.separator();
        response
    }
}

fn active_item_stroke() -> egui::Stroke {
    egui::Stroke::new(1.0, chrome_hover().lerp_to_gamma(chrome_muted(), 0.10))
}

fn prompt_excerpt(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        egui::TextStyle::Body.resolve(ui.style()),
        chrome_text(),
        ui.available_width(),
    );
    job.wrap.max_rows = 4;
    job.wrap.break_anywhere = true;
    ui.add(egui::Label::new(job).wrap())
}

fn live_matches(state: potty::agent::State, filter: Filter) -> bool {
    match filter {
        Filter::All => true,
        Filter::NeedsYou => state == potty::agent::State::NeedsYou,
        Filter::Finished => state == potty::agent::State::ResponseReady,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)]
    fn disabling_contexts_hides_history_without_disabling_live_overview() {
        use potty::contexts::{Annotation, Context, Snapshot};
        let mut overview = Overview::live();
        overview.open = true;
        let record = live_session("active", 1);
        let key = record.key();
        overview.store.merge(record.clone());
        overview.locations.insert(key.clone(), 1);
        overview.expanded = Some(key.clone());
        let mut snapshot = Snapshot::default();
        snapshot.contexts.insert(
            "work".into(),
            Context {
                id: "work".into(),
                annotation: Annotation {
                    kept: true,
                    ..Default::default()
                },
                edited: 1,
                updated: record.revision(),
                sessions: vec![record],
            },
        );
        overview.receive_snapshot(snapshot.clone());
        overview.view = View::Saved;
        overview.editor = Some(Editor {
            owner: snapshot.owner.clone(),
            id: "work".into(),
            expected: 1,
            annotation: Annotation::default(),
            links: String::new(),
            pending: false,
            pending_since: None,
        });
        overview.editor_visible = true;
        overview.search_focused = true;
        overview.actions.push(OverviewAction::RefreshContexts);
        overview.actions.push(OverviewAction::Open(key));
        overview.set_contexts_enabled(false);
        assert!(overview.view == View::Current);
        assert!(!overview.text_input_active());
        assert!(overview.editor.is_none());
        assert!(overview.snapshots.is_empty());
        assert!(matches!(
            overview.actions.as_slice(),
            [OverviewAction::Open(_)]
        ));
        overview.receive_snapshot(snapshot.clone());
        assert!(
            overview.snapshots.is_empty(),
            "late remote snapshots must stay ignored"
        );
        assert!(overview.button_visible());
        let ctx = egui::Context::default();
        crate::apply_chrome_style(&ctx, 13.0);
        let frame = |overview: &mut Overview| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(420.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    overview.show(ctx);
                },
            )
        };
        frame(&mut overview);
        let output = frame(&mut overview);
        let has = |label: &str| {
            output
                .shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.job.text == label))
        };
        assert!(has("Task active"));
        assert!(has("Dismiss"));
        for label in [
            "Current",
            "Saved",
            "Recent",
            "Keep",
            "Edit notes",
            "Copy ID",
            "Copy path",
        ] {
            assert!(!has(label), "history control {label} remained visible");
        }
        overview.set_contexts_enabled(true);
        overview.receive_snapshot(snapshot);
        let output = frame(&mut overview);
        assert!(
            output
                .shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.job.text == "Saved"))
        );
        assert_eq!(overview.store.records.len(), 1);
    }

    #[test]
    #[allow(deprecated)]
    fn saved_context_editor_search_and_actions_fit_narrow_panels() {
        use potty::contexts::{Annotation, Context, Message, Snapshot};
        fn frame(
            ctx: &egui::Context,
            overview: &mut Overview,
            width: f32,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 1000.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    overview.show(ctx);
                },
            )
        }
        fn click(
            ctx: &egui::Context,
            overview: &mut Overview,
            width: f32,
            label: &str,
        ) -> egui::FullOutput {
            let output = frame(ctx, overview, width, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|s| match &s.shape {
                    egui::Shape::Text(t) if t.galley.job.text == label => {
                        Some(t.visual_bounding_rect().center())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("Missing {label}"));
            let mut output = frame(ctx, overview, width, vec![egui::Event::PointerMoved(pos)]);
            for pressed in [true, false] {
                output = frame(
                    ctx,
                    overview,
                    width,
                    vec![egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: Default::default(),
                    }],
                );
            }
            output
        }
        for width in [320.0, 420.0, 900.0, 1280.0] {
            let ctx = egui::Context::default();
            crate::apply_chrome_style(&ctx, 13.0);
            let mut overview = Overview::live();
            overview.open = true;
            overview.view = View::Recent;
            let mut snapshot = Snapshot::default();
            let owner = snapshot.owner.clone();
            let mut record = live_session("saved", 1);
            record.cwd = format!("/projects/{}", "long-path-".repeat(30));
            snapshot.contexts.insert(
                "work".into(),
                Context {
                    id: "work".into(),
                    annotation: Annotation::default(),
                    edited: 0,
                    updated: record.revision(),
                    sessions: vec![record],
                },
            );
            let key = format!("context:{owner}:work");
            overview.receive_snapshot(snapshot.clone());
            overview.writable_owners.insert(owner.clone());
            overview.expanded = Some(key.clone());
            for _ in 0..3 {
                frame(&ctx, &mut overview, width, vec![]);
            }
            let output = frame(&ctx, &mut overview, width, vec![]);
            let heading = |label: &str| {
                output
                    .shapes
                    .iter()
                    .find_map(|s| match &s.shape {
                        egui::Shape::Text(t) if t.galley.job.text == label => {
                            Some(t.visual_bounding_rect())
                        }
                        _ => None,
                    })
                    .unwrap()
            };
            let caption = heading("Sessions");
            for label in ["Current", "Saved", "Recent"] {
                let tab = heading(label);
                assert!(
                    (tab.center().y - caption.center().y).abs() < 4.0,
                    "{label} not beside caption at {width}"
                );
                assert!(tab.left() >= caption.right());
            }
            assert!(heading("Current").left() - caption.right() >= 16.0);
            assert!(heading("Agent session").bottom() < heading("Copy ID").top());
            assert!(heading("Copy path").bottom() < heading("Keep").top());
            assert!(heading("Copy path").bottom() < heading("Edit notes").top());
            let output = click(&ctx, &mut overview, width, "Copy ID");
            assert!(
                output
                    .platform_output
                    .commands
                    .iter()
                    .any(|c| matches!(c, egui::OutputCommand::CopyText(s) if s == "saved"))
            );
            let output = click(&ctx, &mut overview, width, "Copy path");
            let cwd = &snapshot.contexts["work"].sessions[0].cwd;
            assert!(
                output
                    .platform_output
                    .commands
                    .iter()
                    .any(|c| matches!(c, egui::OutputCommand::CopyText(s) if s == cwd))
            );
            assert_eq!(overview.expanded.as_deref(), Some(key.as_str()));
            assert!(
                overview.actions.is_empty(),
                "copying must not navigate or mutate the context"
            );
            click(&ctx, &mut overview, width, "Keep");
            assert!(overview.actions.iter().any(|a| matches!(a, OverviewAction::EditContext(Message::Edit { annotation, .. }) if annotation.kept)));
            overview.actions.clear();
            snapshot.contexts.get_mut("work").unwrap().annotation.kept = true;
            snapshot.contexts.get_mut("work").unwrap().edited = 1;
            overview.receive_snapshot(snapshot.clone());
            let output = frame(&ctx, &mut overview, width, vec![]);
            assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.job.text == "Task saved")), "keeping a context must not remove it from Recent");
            overview.view = View::Saved;
            click(&ctx, &mut overview, width, "Edit notes");
            assert!(overview.text_input_active());
            let editor = overview.editor.as_mut().unwrap();
            editor.annotation.name = "Saved dashboard".into();
            editor.annotation.notes = "Review API first\n".repeat(80);
            editor.links = format!("https://example.com/{}", "long-path-".repeat(30));
            for _ in 0..3 {
                frame(&ctx, &mut overview, width, vec![]);
            }
            let output = frame(&ctx, &mut overview, width, vec![]);
            let bounds = overview.panel_rect.unwrap();
            for s in &output.shapes {
                if let egui::Shape::Text(t) = &s.shape {
                    let rect = t.visual_bounding_rect();
                    if s.clip_rect.intersects(rect) && bounds.intersects(rect) {
                        assert!(
                            rect.right() <= bounds.right() + 2.0,
                            "text overflow at {width}: {}",
                            t.galley.job.text
                        );
                    }
                }
            }
            click(&ctx, &mut overview, width, "Save");
            assert!(overview.editor.as_ref().unwrap().pending);
            let annotation = overview
                .actions
                .iter()
                .find_map(|a| match a {
                    OverviewAction::EditContext(Message::Edit { annotation, .. }) => {
                        Some(annotation.clone())
                    }
                    _ => None,
                })
                .unwrap();
            overview.context_failed("Save rejected".into());
            assert!(!overview.editor.as_ref().unwrap().pending);
            assert_eq!(
                overview.editor.as_ref().unwrap().annotation.notes,
                annotation.notes
            );
            overview.context_error = None;
            click(&ctx, &mut overview, width, "Save");
            let c = snapshot.contexts.get_mut("work").unwrap();
            c.annotation = annotation;
            c.edited = 2;
            overview.receive_snapshot(snapshot.clone());
            assert!(overview.editor.is_none());
            frame(&ctx, &mut overview, width, vec![]);
            assert!(!overview.text_input_active());
            overview.search = "not-present-anywhere".into();
            let output = frame(&ctx, &mut overview, width, vec![]);
            assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.job.text == "No matching contexts")));
            overview.search.clear();
            click(&ctx, &mut overview, width, "Forget...");
            assert!(
                !overview
                    .actions
                    .iter()
                    .any(|a| matches!(a, OverviewAction::EditContext(Message::Forget { .. })))
            );
            click(&ctx, &mut overview, width, "Forget");
            assert!(
                overview
                    .actions
                    .iter()
                    .any(|a| matches!(a, OverviewAction::EditContext(Message::Forget { .. })))
            );
            assert!(!overview.actions.iter().any(|a| matches!(
                a,
                OverviewAction::Open(_) | OverviewAction::OpenContext { .. }
            )));
        }
    }

    fn live_session(session: &str, pane: u64) -> potty::agent::Record {
        potty::agent::hook(
            potty::notify::Tool::Claude,
            &serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":session,
                "prompt":format!("Task {session}")}),
            "local".into(),
            "host".into(),
            Some(pane),
        )
        .unwrap()
    }

    #[test]
    fn sessions_group_by_resolved_pane_without_merging_identities() {
        let mut overview = Overview::live();
        for (session, pane) in [("a", 1), ("b", 2), ("c", 1)] {
            let mut record = live_session(session, pane);
            if session == "c" {
                record.tool = potty::notify::Tool::Codex;
                record.origin = "remote".into();
                record.pane = Some(90);
            }
            overview.locations.insert(record.key(), pane);
            overview.store.merge(record);
        }
        let order = || {
            overview
                .sorted_records()
                .into_iter()
                .map(|r| r.session)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(), ["a", "c", "b"]);
        assert_eq!(overview.store.records.len(), 3);
        overview.locations.clear();
        let records = overview.sorted_records();
        assert_eq!(
            records
                .iter()
                .map(|r| r.session.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    #[allow(deprecated)]
    fn prompt_excerpts_never_exceed_four_wrapped_lines() {
        for width in [120.0, 280.0, 500.0] {
            for font_size in [13.0, 22.0] {
                let ctx = egui::Context::default();
                crate::apply_chrome_style(&ctx, font_size);
                for text in [
                    "long pasted text ".repeat(100),
                    "a\nb\nc\nd\ne\nf".into(),
                    "x".repeat(1600),
                ] {
                    let output = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 900.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                prompt_excerpt(ui, &text);
                            });
                        },
                    );
                    let galley = output
                        .shapes
                        .iter()
                        .find_map(|s| match &s.shape {
                            egui::Shape::Text(t) => Some(&t.galley),
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(galley.rows.len(), 4);
                    assert!(galley.elided);
                    assert!(galley.size().x <= width);
                }
            }
        }
    }

    #[test]
    #[allow(deprecated)]
    fn active_outline_is_subtle_and_contains_expanded_details() {
        let stroke = active_item_stroke();
        for (border, fill) in stroke.color.to_array()[..3]
            .iter()
            .zip(&chrome_hover().to_array()[..3])
        {
            assert!(*border > *fill && *border - *fill <= 12);
        }
        let ctx = egui::Context::default();
        crate::apply_chrome_style(&ctx, 13.0);
        let mut overview = Overview::live();
        overview.open = true;
        let mut record = live_session("active", 1);
        record.briefing = Some(potty::agent::Briefing {
            title: "Active task".into(),
            summary: "Expanded summary".into(),
            next: "Next action".into(),
            milestones: vec!["Completed step".into()],
        });
        record.briefing_at = record.prompt_at - 1;
        let key = record.key();
        overview.locations.insert(key.clone(), 1);
        overview.store.merge(record);
        overview.sync_focus(1);
        let mut collapsed_height = 0.0;
        for expanded in [false, true] {
            overview.expanded = expanded.then(|| key.clone());
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(420.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    overview.show(ctx);
                },
            );
            let outline = output
                .shapes
                .iter()
                .find_map(|s| match &s.shape {
                    egui::Shape::Rect(r) if r.stroke == stroke => Some(r.rect),
                    _ => None,
                })
                .unwrap();
            assert!(output.shapes.iter().any(|s| matches!(&s.shape,
                egui::Shape::Rect(r) if r.rect == outline && r.fill == chrome_hover())));
            if expanded {
                assert!(outline.height() > collapsed_height);
                for label in [
                    "Active task",
                    "Expanded summary",
                    "- Completed step",
                    "Latest prompt (hook excerpt)",
                    "Task active",
                    "Dismiss",
                ] {
                    let text_rect = output
                        .shapes
                        .iter()
                        .find_map(|s| match &s.shape {
                            egui::Shape::Text(t) if t.galley.job.text == label => {
                                Some(t.visual_bounding_rect())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| panic!("missing {label}"));
                    assert!(outline.contains_rect(text_rect), "outline excludes {label}");
                }
            } else {
                collapsed_height = outline.height();
            }
        }
        overview.sync_focus(999);
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 900.0),
                )),
                ..Default::default()
            },
            |ctx| {
                overview.show(ctx);
            },
        );
        assert_eq!(overview.expanded.as_deref(), Some(key.as_str()));
        let title = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.job.text == "Active task" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .unwrap();
        assert!(!output.shapes.iter().any(|s| matches!(&s.shape,
            egui::Shape::Rect(r) if r.stroke == stroke || (r.fill == chrome_hover() && r.rect.contains(title)))),
            "an expanded item must lose its active styling when focus moves to an unrelated pane");
    }

    #[test]
    #[allow(deprecated)]
    fn pane_focus_highlights_and_reveals_rows_without_continuous_scroll_requests() {
        let ctx = egui::Context::default();
        crate::apply_chrome_style(&ctx, 13.0);
        let mut overview = Overview::live();
        overview.open = true;
        for pane in 1..=20 {
            let record = live_session(&format!("{pane:02}"), pane);
            overview.locations.insert(record.key(), pane);
            overview.store.merge(record);
        }
        let mut time = 0.0;
        let mut frame = |overview: &mut Overview| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    overview.show(ctx);
                },
            )
        };
        let visible = |output: &egui::FullOutput, title: &str| {
            output.shapes.iter().any(|s| matches!(&s.shape,
                egui::Shape::Text(t) if t.galley.job.text == title && s.clip_rect.contains(t.visual_bounding_rect().center())))
        };
        overview.sync_focus(1);
        for _ in 0..4 {
            frame(&mut overview);
        }
        assert!(visible(&frame(&mut overview), "Task 01"));
        overview.sync_focus(20);
        assert!(overview.reveal_focus);
        for _ in 0..6 {
            frame(&mut overview);
        }
        let output = frame(&mut overview);
        assert!(visible(&output, "Task 20"));
        assert!(!visible(&output, "Task 01"));
        assert!(output.shapes.iter().any(|s| matches!(&s.shape,
            egui::Shape::Rect(r) if r.stroke == active_item_stroke() && s.clip_rect.intersects(r.rect))));
        overview.sync_focus(20);
        assert!(
            !overview.reveal_focus,
            "ordinary repaint must not reset manual scrolling"
        );
        overview.sync_focus(1);
        for _ in 0..6 {
            frame(&mut overview);
        }
        assert!(visible(&frame(&mut overview), "Task 01"));
        overview.open = false;
        frame(&mut overview);
        overview.sync_focus(20);
        frame(&mut overview);
        assert!(
            overview.reveal_focus,
            "hidden panel must retain the focus request"
        );
        overview.open = true;
        for _ in 0..6 {
            frame(&mut overview);
        }
        assert!(visible(&frame(&mut overview), "Task 20"));
    }

    #[test]
    fn overview_button_tracks_current_sessions_independently_of_filters() {
        use potty::agent::State;

        let mut overview = Overview::live();
        assert!(!overview.button_visible());
        let mut record = potty::agent::hook(
            potty::notify::Tool::Claude,
            &serde_json::json!({"hook_event_name":"SessionStart", "session_id":"current"}),
            "local".into(),
            "host".into(),
            Some(1),
        )
        .unwrap();
        overview.filter = Filter::NeedsYou;
        for state in [
            State::Unknown,
            State::Working,
            State::NeedsYou,
            State::ResponseReady,
        ] {
            record.state = state;
            record.state_at += 1;
            overview.store.merge(record.clone());
            assert!(overview.button_visible());
        }
        record.state = State::Ended;
        record.state_at += 1;
        overview.store.merge(record.clone());
        overview.show_ended = true;
        assert!(
            !overview.button_visible(),
            "history must not keep the button visible"
        );
        overview.open = true;
        assert!(overview.button_visible(), "an open panel keeps its toggle");
        overview.open = false;

        record.state = State::Working;
        record.state_at += 1;
        overview.store.merge(record.clone());
        assert!(overview.button_visible());
        record.dismissed = record.revision();
        overview.store.merge(record.clone());
        assert!(!overview.button_visible());
        record.state_at += 1;
        overview.store.merge(record);
        assert!(
            overview.button_visible(),
            "new activity restores the button"
        );
        overview.enabled = false;
        assert!(!overview.button_visible());
    }

    #[test]
    fn overview_button_remains_available_for_empty_open_panels_and_demo_sessions() {
        let mut overview = Overview::live();
        overview.toggle();
        assert!(overview.button_visible());
        overview.toggle();
        assert!(!overview.button_visible());

        let mut overview = Overview::demo();
        overview.open = false;
        assert!(overview.button_visible());
        overview
            .dismissed
            .extend(SAMPLES.iter().map(|s| s.id.to_owned()));
        assert!(!overview.button_visible());
        overview.toggle();
        assert!(overview.button_visible());
    }

    #[test]
    fn ended_sessions_are_history_not_duplicate_current_panes() {
        let mut overview = Overview::live();
        let active = potty::agent::hook(potty::notify::Tool::Claude,
            &serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":"current", "prompt":"PTT-128"}),
            "local".into(), "host".into(), Some(1)).unwrap();
        let mut ended = active.clone();
        ended.session = "old".into();
        ended.state = potty::agent::State::Ended;
        ended.state_at += 1; // Even a late end event must not displace the current session.
        overview.store.merge(active.clone());
        overview.store.merge(ended.clone());
        assert!(overview.matches_live(&active, Filter::All));
        assert!(!overview.matches_live(&ended, Filter::All));
        overview.show_ended = true;
        assert!(overview.matches_live(&ended, Filter::All));
        overview.show_ended = false;
        overview.store.merge(ended.clone());
        assert!(!overview.matches_live(&overview.store.records[&ended.key()], Filter::All));
        assert_eq!(
            overview.store.records.len(),
            2,
            "history is retained, not merged by pane"
        );

        ended.state = potty::agent::State::Working;
        ended.state_at += 1;
        overview.store.merge(ended.clone());
        assert!(overview.matches_live(&overview.store.records[&ended.key()], Filter::All));
        assert!(
            overview.matches_live(&active, Filter::All),
            "two active sessions in one pane stay distinct"
        );
    }

    #[test]
    #[allow(deprecated)]
    fn overview_controls_expand_filter_and_dismiss_sample_and_live_rows() {
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
        assert_eq!(overview.expanded.as_deref(), Some("clipboard"));
        click(&ctx, &mut overview, "Needs you 2");
        assert!(overview.filter == Filter::NeedsYou);
        click(&ctx, &mut overview, "Dismiss");
        assert!(overview.dismissed.contains("clipboard"));
        assert_eq!(overview.expanded, None);
        click(&ctx, &mut overview, "Finished 1");
        click(&ctx, &mut overview, "Idle CPU regression");
        assert_eq!(overview.expanded.as_deref(), Some("idle"));
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

        let mut overview = Overview::live();
        overview.open = true;
        let record = potty::agent::hook(potty::notify::Tool::Codex,
            &serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":"live", "prompt":"Review transport"}),
            "local".into(), "host".into(), Some(1)).unwrap();
        let key = record.key();
        overview.store.merge(record.clone());
        overview.locations.insert(key.clone(), 1);
        frame(&ctx, &mut overview, vec![]);
        click(&ctx, &mut overview, "Review transport");
        assert_eq!(overview.expanded.as_deref(), Some(key.as_str()));
        assert!(matches!(overview.actions.pop(), Some(OverviewAction::Open(k)) if k == key));

        let mut stopped = record.clone();
        stopped.state = potty::agent::State::ResponseReady;
        stopped.state_at += 1;
        stopped.background = Some(potty::agent::Background {
            tasks: 2,
            monitors: 1,
        });
        stopped.background_at = stopped.state_at;
        overview.store.merge(stopped);
        let output = frame(&ctx, &mut overview, vec![]);
        for label in ["Response ready", "Background: 2 tasks (1 monitor)"] {
            assert!(
                output.shapes.iter().any(|s| matches!(&s.shape,
                egui::Shape::Text(t) if t.galley.job.text == label)),
                "missing {label}"
            );
        }
        assert!(!output.shapes.iter().any(|s| matches!(&s.shape,
            egui::Shape::Text(t) if t.galley.job.text == "Open pane")));

        let mut briefing = record.clone();
        briefing.briefing = Some(potty::agent::Briefing {
            title: "Old task".into(),
            summary: "Earlier work".into(),
            milestones: vec!["Reviewed the transport".into()],
            ..Default::default()
        });
        briefing.briefing_at = record.prompt_at - 1;
        overview.store.merge(briefing);
        let output = frame(&ctx, &mut overview, vec![]);
        assert!(output.shapes.iter().any(|s| matches!(&s.shape,
            egui::Shape::Text(t) if t.galley.job.text.contains("older than prompt"))));
        for label in ["Old task", "Milestones", "Latest prompt (hook excerpt)"] {
            overview.sync_focus(999);
            click(&ctx, &mut overview, label);
            assert!(matches!(overview.actions.pop(), Some(OverviewAction::Open(k)) if k == key));
            assert_eq!(
                overview.expanded.as_deref(),
                Some(key.as_str()),
                "focusing an inactive expanded item must preserve its details"
            );
            overview.sync_focus(1);
            click(&ctx, &mut overview, label);
            assert!(matches!(overview.actions.pop(), Some(OverviewAction::Open(k)) if k == key));
            assert!(overview.actions.is_empty());
            assert!(
                overview.expanded.is_none(),
                "expanded content must toggle just like the header"
            );
            click(&ctx, &mut overview, "Old task");
            assert!(matches!(overview.actions.pop(), Some(OverviewAction::Open(k)) if k == key));
            assert_eq!(overview.expanded.as_deref(), Some(key.as_str()));
        }
        click(&ctx, &mut overview, "Dismiss");
        assert!(matches!(overview.actions.pop(), Some(OverviewAction::Dismiss(r)) if !r.visible()));
        assert!(
            overview.actions.is_empty(),
            "dismiss must not also activate the item"
        );
        assert!(
            overview.expanded.is_none(),
            "dismiss must not re-expand the item"
        );
        overview.store.merge(record);
        assert!(
            !overview.store.records[&key].visible(),
            "old replay must stay hidden"
        );
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
                    overview.expanded = expanded.map(str::to_owned);
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
