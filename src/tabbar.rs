//! Width-budgeted tab headers. Fixed toolbar controls are allocated by the caller first.

use crate::Action;
use crate::workspace::{Node, PaneId, Workspace};
use std::collections::HashSet;

const GAP: f32 = 4.0;

fn covered(node: &Node, panes: &HashSet<PaneId>) -> bool {
    match node {
        Node::Leaf(id) => panes.contains(id),
        Node::Branch { a, b, .. } => covered(a, panes) && covered(b, panes),
    }
}

/// Scale preferred widths together, freezing headers at their minimum as necessary.
/// If even the minima do not fit, the caller scrolls instead of shrinking hit targets.
fn fit_widths(preferred: &[f32], minimum: &[f32], available: f32) -> Vec<f32> {
    let budget = (available - GAP * preferred.len().saturating_sub(1) as f32).max(0.0);
    if preferred.iter().sum::<f32>() <= budget {
        return preferred.to_vec();
    }
    if minimum.iter().sum::<f32>() >= budget {
        return minimum.to_vec();
    }
    let (mut low, mut high) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let scale = (low + high) / 2.0;
        let total: f32 = preferred
            .iter()
            .zip(minimum)
            .map(|(p, m)| (p * scale).max(*m))
            .sum();
        if total > budget {
            high = scale;
        } else {
            low = scale;
        }
    }
    preferred
        .iter()
        .zip(minimum)
        .map(|(p, m)| (p * low).max(*m))
        .collect()
}

#[derive(Clone, PartialEq)]
struct Reveal {
    active: PaneId,
    available: f32,
    widths: Vec<f32>,
}

pub fn show(
    ui: &mut egui::Ui,
    ws: &Workspace,
    titles: &[String],
    represented: &HashSet<PaneId>,
    actions: &mut Vec<Action>,
) {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let height = (ui.text_style_height(&egui::TextStyle::Button)
        + 2.0 * ui.spacing().button_padding.y)
        .max(24.0);
    let compact_width = height.max(28.0);
    let close_width = height;
    let normal_min = (close_width + 40.0).max(72.0);
    let compact: Vec<_> = ws
        .tabs
        .iter()
        .map(|t| covered(&t.layout, represented))
        .collect();
    let minimum: Vec<_> = compact
        .iter()
        .map(|c| if *c { compact_width } else { normal_min })
        .collect();
    let preferred: Vec<_> = titles
        .iter()
        .zip(&compact)
        .map(|(title, compact)| {
            if *compact {
                compact_width
            } else {
                let text = ui.painter().layout_no_wrap(
                    title.clone(),
                    font.clone(),
                    ui.visuals().text_color(),
                );
                (text.size().x + 2.0 * ui.spacing().button_padding.x + close_width + 3.0)
                    .clamp(normal_min, 240.0_f32.max(normal_min))
            }
        })
        .collect();
    let available = ui.available_width().max(0.0);
    let widths = fit_widths(&preferred, &minimum, available);
    let reveal = Reveal {
        active: ws.active_tab().layout.first_leaf(),
        available,
        widths: widths.clone(),
    };
    let id = ui.id().with("tab-strip-reveal");
    let changed = ui.data_mut(|data| {
        let changed = data.get_temp::<Reveal>(id).as_ref() != Some(&reveal);
        data.insert_temp(id, reveal);
        changed
    });
    ui.allocate_ui_with_layout(
        egui::vec2(available, height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal()
                .id_salt("tab-strip")
                .auto_shrink([false, false])
                .max_height(height)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    ui.horizontal(|ui| {
                        for (i, tab) in ws.tabs.iter().enumerate() {
                            let response = ui
                                .push_id(tab.layout.first_leaf(), |ui| {
                                    ui.spacing_mut().item_spacing.x = 3.0;
                                    let label = if compact[i] {
                                        (i + 1).to_string()
                                    } else {
                                        titles[i].clone()
                                    };
                                    let label_width = widths[i]
                                        - if compact[i] { 0.0 } else { close_width + 3.0 };
                                    let response = ui
                                        .add_sized(
                                            egui::vec2(label_width, height),
                                            egui::Button::selectable(i == ws.active, label)
                                                .truncate()
                                                .corner_radius(egui::CornerRadius::same(6)),
                                        )
                                        .on_hover_text(&titles[i]);
                                    if response.clicked() {
                                        actions.push(Action::SelectTab(i));
                                    }
                                    response.context_menu(|ui| {
                                        if ui.button("Close tab").clicked() {
                                            actions.push(Action::CloseTab(i));
                                            ui.close();
                                        }
                                    });
                                    if !compact[i]
                                        && ui
                                            .add_sized(
                                                egui::vec2(close_width, height),
                                                egui::Button::new(
                                                    egui::RichText::new("\u{00d7}").weak(),
                                                )
                                                .frame(false),
                                            )
                                            .on_hover_text("Close tab")
                                            .clicked()
                                    {
                                        actions.push(Action::CloseTab(i));
                                    }
                                })
                                .response;
                            if i == ws.active && changed {
                                response.scroll_to_me_animation(
                                    None,
                                    egui::style::ScrollAnimation::none(),
                                );
                            }
                        }
                    });
                });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_scale_proportionally_then_stop_at_minima() {
        assert_eq!(
            fit_widths(&[100.0, 200.0], &[72.0; 2], 400.0),
            [100.0, 200.0]
        );
        let fit = fit_widths(&[100.0, 200.0], &[72.0; 2], 244.0);
        assert!((fit[0] - 80.0).abs() < 0.01);
        assert!((fit[1] - 160.0).abs() < 0.01);
        let fit = fit_widths(&[28.0, 100.0, 200.0], &[28.0, 72.0, 72.0], 220.0);
        assert_eq!(fit[0], 28.0);
        assert_eq!(fit[1], 72.0);
        assert!((fit[2] - 112.0).abs() < 0.01);
        assert_eq!(
            fit_widths(&[28.0, 200.0], &[28.0, 72.0], 20.0),
            [28.0, 72.0]
        );
        assert!(fit_widths(&[], &[], 0.0).is_empty());
    }

    #[test]
    fn only_fully_represented_tabs_collapse() {
        let mut ws = Workspace::new();
        ws.split(crate::workspace::Split::Cols);
        let panes = ws.all_leaves();
        assert!(!covered(&ws.active_tab().layout, &[panes[0]].into()));
        assert!(covered(
            &ws.active_tab().layout,
            &panes.into_iter().collect()
        ));
        ws.new_tab();
        assert!(!covered(&ws.active_tab().layout, &[0].into()));
    }

    struct Harness {
        ctx: egui::Context,
        ws: Workspace,
        titles: Vec<String>,
        represented: HashSet<PaneId>,
        width: f32,
        time: f64,
    }

    impl Harness {
        fn new(count: usize, width: f32) -> Self {
            let mut ws = Workspace::new();
            for _ in 1..count {
                ws.new_tab();
            }
            let ctx = egui::Context::default();
            crate::apply_chrome_style(&ctx, 13.0);
            Self {
                ctx,
                ws,
                width,
                time: 0.0,
                titles: (0..count)
                    .map(|i| format!("host-{i}: a rather long terminal title"))
                    .collect(),
                represented: HashSet::new(),
            }
        }

        #[allow(deprecated)]
        fn frame(&mut self, events: Vec<egui::Event>) -> (egui::FullOutput, Vec<Action>) {
            self.time += 0.1;
            let mut actions = Vec::new();
            let out = self.ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(self.width, 400.0),
                    )),
                    events,
                    time: Some(self.time),
                    ..Default::default()
                },
                |ctx| {
                    egui::TopBottomPanel::top("test-tabs")
                        .frame(crate::tabbar_frame())
                        .show(ctx, |ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 24.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.add_sized(
                                        egui::vec2(60.0, 24.0),
                                        egui::Button::new("Toolbar"),
                                    );
                                    show(
                                        ui,
                                        &self.ws,
                                        &self.titles,
                                        &self.represented,
                                        &mut actions,
                                    );
                                },
                            );
                        });
                },
            );
            (out, actions)
        }

        fn settle(&mut self) -> egui::FullOutput {
            self.frame(vec![]);
            self.frame(vec![]);
            self.frame(vec![]).0
        }

        fn click(&mut self, pos: egui::Pos2, button: egui::PointerButton) -> Vec<Action> {
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button,
                pressed: true,
                modifiers: Default::default(),
            }]);
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button,
                pressed: false,
                modifiers: Default::default(),
            }])
            .1
        }
    }

    fn text_rect(out: &egui::FullOutput, text: &str) -> Option<egui::Rect> {
        out.shapes.iter().find_map(|s| match &s.shape {
            egui::Shape::Text(t)
                if t.galley.job.text == text
                    && s.clip_rect.contains(t.visual_bounding_rect().center()) =>
            {
                Some(t.visual_bounding_rect())
            }
            _ => None,
        })
    }

    #[test]
    fn compact_tabs_select_close_and_restore_captions() {
        let mut h = Harness::new(3, 600.0);
        h.represented = h.ws.all_leaves().into_iter().collect();
        let out = h.settle();
        assert!(text_rect(&out, &h.titles[0]).is_none());
        assert!(text_rect(&out, "\u{00d7}").is_none());
        let first = text_rect(&out, "1").unwrap().center();
        assert!(
            h.click(first, egui::PointerButton::Primary)
                .iter()
                .any(|a| matches!(a, Action::SelectTab(0)))
        );
        h.click(first, egui::PointerButton::Secondary);
        let out = h.settle();
        let close = text_rect(&out, "Close tab").unwrap().center();
        assert!(
            h.click(close, egui::PointerButton::Primary)
                .iter()
                .any(|a| matches!(a, Action::CloseTab(0)))
        );
        h.represented.clear();
        let out = h.settle();
        assert!(text_rect(&out, &h.titles[0]).is_some());
        assert!(text_rect(&out, "\u{00d7}").is_some());
        assert!(text_rect(&out, "1").is_none());
    }

    #[test]
    fn overflow_is_clipped_and_active_tab_revealed_without_forcing_scroll() {
        let mut h = Harness::new(40, 400.0);
        let out = h.settle();
        assert!(text_rect(&out, &h.titles[39]).is_some());
        assert!(text_rect(&out, &h.titles[0]).is_none());
        let controls = text_rect(&out, "Toolbar").unwrap();
        for shape in &out.shapes {
            if let egui::Shape::Text(text) = &shape.shape
                && text.galley.job.text.starts_with("host-")
            {
                assert!(shape.clip_rect.right() < controls.left());
                assert!(text.visual_bounding_rect().height() < 25.0);
            }
        }
        // Wheel away from the active tab; idle frames must not pull back to it.
        h.frame(vec![
            egui::Event::PointerMoved(egui::pos2(100.0, 16.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                delta: egui::vec2(0.0, 10000.0),
                modifiers: Default::default(),
            },
        ]);
        for _ in 0..15 {
            h.frame(vec![]);
        }
        let out = h.settle();
        assert!(text_rect(&out, &h.titles[0]).is_some());
        assert!(text_rect(&out, &h.titles[39]).is_none());
        // A selection change, resize, or overview toggle reveals the active tab again.
        h.ws.active = 20;
        assert!(text_rect(&h.settle(), &h.titles[20]).is_some());
        h.width = 240.0;
        assert!(text_rect(&h.settle(), &h.titles[20]).is_some());
        h.represented = h.ws.all_leaves().into_iter().collect();
        assert!(text_rect(&h.settle(), "21").is_some());
        h.represented.clear();
        assert!(text_rect(&h.settle(), &h.titles[20]).is_some());
    }

    #[test]
    fn shrinking_preserves_single_row_and_fixed_toolbar_at_different_font_sizes() {
        for width in [240.0, 320.0, 600.0, 1280.0] {
            for font in [13.0, 18.0, 26.0] {
                let mut h = Harness::new(6, width);
                crate::apply_chrome_style(&h.ctx, font);
                // Cover some tabs while leaving others with proportional captions.
                h.represented = [0, 1].into();
                let out = h.settle();
                let toolbar = text_rect(&out, "Toolbar").unwrap();
                assert!(toolbar.right() < width && toolbar.bottom() < 60.0);
                for s in &out.shapes {
                    if let egui::Shape::Text(t) = &s.shape {
                        let rect = t.visual_bounding_rect();
                        if !s.clip_rect.intersects(rect) || t.galley.job.text.is_empty() {
                            continue;
                        }
                        assert!(
                            rect.top() >= 0.0 && rect.bottom() < 60.0,
                            "wrapped header: {rect:?}"
                        );
                        if t.galley.job.text != "Toolbar" {
                            assert!(s.clip_rect.right() < toolbar.left());
                        }
                    }
                }
            }
        }
    }
}
